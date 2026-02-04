use httpdate::fmt_http_date;
use mime_guess::mime;
use std::time::{Duration, SystemTime};
use tokio::fs as tokio_fs;
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

use migux_config::{HttpConfig, LocationConfig, ServerConfig};

use crate::cache::{
    build_cache_key, cache_metrics_snapshot, CacheKey, CachePolicy, DiskCache, MemoryCache,
};
use crate::conditional::should_return_not_modified;
use crate::etag::{EtagInfo, last_modified_header, weak_etag_size_mtime};
use crate::fs::PathResolver;
use crate::response::ResponseBuilder;

struct StaticService<'a> {
    server_cfg: &'a ServerConfig,
    location: &'a LocationConfig,
}

struct StaticFileInfo {
    content_length: usize,
    etag: EtagInfo,
    last_modified: Option<String>,
}

struct ResolvedFile {
    path: String,
    len: u64,
    info: StaticFileInfo,
    content_type: String,
}

const DEFAULT_STREAM_THRESHOLD_BYTES: u64 = 1024 * 1024;

enum FileResolution {
    File(ResolvedFile),
    Response(Vec<u8>),
}

impl StaticFileInfo {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let content_length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let etag = weak_etag_size_mtime(metadata);
        let last_modified = last_modified_header(metadata);
        Self {
            content_length,
            etag,
            last_modified,
        }
    }
}

impl ResolvedFile {
    fn static_headers<'a>(&'a self, hsts: Option<&'a str>) -> Vec<(&'static str, &'a str)> {
        let mut headers = Vec::new();
        headers.push(("ETag", self.info.etag.header.as_str()));
        if let Some(last_modified) = self.info.last_modified.as_deref() {
            headers.push(("Last-Modified", last_modified));
        }
        if let Some(hsts_value) = hsts {
            headers.push(("Strict-Transport-Security", hsts_value));
        }
        headers
    }

    fn cache_key(&self, hsts: Option<&str>) -> CacheKey {
        let hsts_flag = hsts.is_some();
        build_cache_key(
            self.path.as_str(),
            self.len,
            self.info.etag.mtime_nanos,
            hsts_flag,
        )
    }
}

fn build_not_modified(info: &StaticFileInfo, keep_alive: bool, hsts: Option<&str>) -> Vec<u8> {
    let date = fmt_http_date(SystemTime::now());
    let mut headers = Vec::new();
    headers.push(("ETag", info.etag.header.as_str()));
    if let Some(last_modified) = info.last_modified.as_deref() {
        headers.push(("Last-Modified", last_modified));
    }
    if let Some(hsts_value) = hsts {
        headers.push(("Strict-Transport-Security", hsts_value));
    }
    headers.push(("Date", date.as_str()));
    ResponseBuilder::build_with_headers("304 Not Modified", None, 0, keep_alive, &headers, None)
}

fn content_type_for_path(path: &str) -> String {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
    {
        if let Some(ct) = fast_content_type(ext) {
            return ct.to_string();
        }
    }

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    if mime.type_() == mime::TEXT {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    }
}

fn fast_content_type(ext: &str) -> Option<&'static str> {
    if ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") {
        return Some("text/html; charset=utf-8");
    }
    if ext.eq_ignore_ascii_case("css") {
        return Some("text/css; charset=utf-8");
    }
    if ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("mjs") {
        return Some("text/javascript; charset=utf-8");
    }
    if ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("map") {
        return Some("application/json; charset=utf-8");
    }
    if ext.eq_ignore_ascii_case("txt") {
        return Some("text/plain; charset=utf-8");
    }
    if ext.eq_ignore_ascii_case("svg") {
        return Some("image/svg+xml");
    }
    if ext.eq_ignore_ascii_case("png") {
        return Some("image/png");
    }
    if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        return Some("image/jpeg");
    }
    if ext.eq_ignore_ascii_case("gif") {
        return Some("image/gif");
    }
    if ext.eq_ignore_ascii_case("webp") {
        return Some("image/webp");
    }
    if ext.eq_ignore_ascii_case("ico") {
        return Some("image/x-icon");
    }
    if ext.eq_ignore_ascii_case("wasm") {
        return Some("application/wasm");
    }
    if ext.eq_ignore_ascii_case("woff") {
        return Some("font/woff");
    }
    if ext.eq_ignore_ascii_case("woff2") {
        return Some("font/woff2");
    }
    None
}

async fn read_body(path: &str, keep_alive: bool) -> Result<Vec<u8>, Vec<u8>> {
    match tokio_fs::read(path).await {
        Ok(body) => Ok(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ResponseBuilder::not_found(keep_alive))
        }
        Err(_) => Err(ResponseBuilder::internal_error(keep_alive)),
    }
}

impl<'a> StaticService<'a> {
    fn new(server_cfg: &'a ServerConfig, location: &'a LocationConfig) -> Self {
        Self {
            server_cfg,
            location,
        }
    }

    async fn serve<S>(
        &self,
        stream: &mut S,
        method: &str,
        headers: &str,
        req_path: &str,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        self.serve_uncached(stream, None, method, headers, req_path, keep_alive, hsts)
            .await
    }

    async fn serve_uncached<S>(
        &self,
        stream: &mut S,
        http_cfg: Option<&HttpConfig>,
        method: &str,
        headers: &str,
        req_path: &str,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        let file = match self.resolve_file(req_path, keep_alive).await? {
            FileResolution::File(file) => file,
            FileResolution::Response(resp) => {
                stream.write_all(&resp).await?;
                return Ok(());
            }
        };

        if let Some(resp) = self.not_modified_response(method, headers, &file, keep_alive, hsts) {
            stream.write_all(&resp).await?;
            return Ok(());
        }

        if method == "HEAD" {
            let resp = self.head_response(&file, keep_alive, hsts);
            stream.write_all(&resp).await?;
            return Ok(());
        }

        let threshold = http_cfg
            .map(stream_threshold_bytes)
            .unwrap_or(DEFAULT_STREAM_THRESHOLD_BYTES);
        if should_stream_file(file.len, threshold) {
            self.stream_file_response(stream, &file, keep_alive, hsts)
                .await?;
            return Ok(());
        }

        let body = match read_body(&file.path, keep_alive).await {
            Ok(body) => body,
            Err(resp) => {
                stream.write_all(&resp).await?;
                return Ok(());
            }
        };
        let resp = self.ok_response(&file, &body, keep_alive, hsts);
        stream.write_all(&resp).await?;
        Ok(())
    }

    async fn serve_cached<S>(
        &self,
        stream: &mut S,
        http_cfg: &HttpConfig,
        method: &str,
        headers: &str,
        req_path: &str,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        if !CachePolicy::enabled(http_cfg, self.location, method) {
            return self
                .serve_uncached(
                    stream,
                    Some(http_cfg),
                    method,
                    headers,
                    req_path,
                    keep_alive,
                    hsts,
                )
                .await;
        }

        let file = match self.resolve_file(req_path, keep_alive).await? {
            FileResolution::File(file) => file,
            FileResolution::Response(resp) => {
                stream.write_all(&resp).await?;
                return Ok(());
            }
        };

        if let Some(resp) = self.not_modified_response(method, headers, &file, keep_alive, hsts) {
            stream.write_all(&resp).await?;
            return Ok(());
        }

        if method == "HEAD" {
            let resp = self.head_response(&file, keep_alive, hsts);
            stream.write_all(&resp).await?;
            return Ok(());
        }

        let stream_threshold = stream_threshold_bytes(http_cfg);
        if should_stream_file(file.len, stream_threshold) {
            self.stream_file_response(stream, &file, keep_alive, hsts)
                .await?;
            return Ok(());
        }

        let resp = self
            .serve_bytes_cached_for_file(http_cfg, method, headers, file, keep_alive, hsts)
            .await?;
        stream.write_all(&resp).await?;
        Ok(())
    }

    async fn serve_bytes(
        &self,
        method: &str,
        headers: &str,
        req_path: &str,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        let file = match self.resolve_file(req_path, keep_alive).await? {
            FileResolution::File(file) => file,
            FileResolution::Response(resp) => return Ok(resp),
        };

        if let Some(resp) = self.not_modified_response(method, headers, &file, keep_alive, hsts) {
            return Ok(resp);
        }

        if method == "HEAD" {
            return Ok(self.head_response(&file, keep_alive, hsts));
        }

        let body = match read_body(&file.path, keep_alive).await {
            Ok(body) => body,
            Err(resp) => return Ok(resp),
        };

        Ok(self.ok_response(&file, &body, keep_alive, hsts))
    }

    async fn serve_bytes_cached_for_file(
        &self,
        http_cfg: &HttpConfig,
        method: &str,
        headers: &str,
        file: ResolvedFile,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        if let Some(resp) = self.not_modified_response(method, headers, &file, keep_alive, hsts) {
            return Ok(resp);
        }

        if method == "HEAD" {
            return Ok(self.head_response(&file, keep_alive, hsts));
        }

        let key = file.cache_key(hsts);

        if let Some(resp) = MemoryCache::get(key) {
            return Ok(resp);
        }

        let max_obj = http_cfg.cache_max_object_bytes().unwrap_or(0);
        let ttl_secs = http_cfg.cache_default_ttl_secs().unwrap_or(0) as u64;
        let ttl = Duration::from_secs(ttl_secs);

        if let Some(cache_dir) = http_cfg.cache_dir() {
            let disk_cache = DiskCache::new(cache_dir);
            if let Some(resp) = disk_cache.get(key).await {
                if ttl_secs > 0 {
                    MemoryCache::put(key, resp.clone(), ttl);
                }
                return Ok(resp);
            }
        }

        tracing::debug!(target: "migux::static_cache", cache_key = %key, "Cache miss");

        let body = match read_body(&file.path, keep_alive).await {
            Ok(body) => body,
            Err(resp) => return Ok(resp),
        };

        let resp = self.ok_response(&file, &body, keep_alive, hsts);

        if max_obj > 0 && (body.len() as u64) <= max_obj && ttl_secs > 0 {
            MemoryCache::put(key, resp.clone(), ttl);
            if let Some(cache_dir) = http_cfg.cache_dir() {
                DiskCache::new(cache_dir).put(key, &resp, ttl).await;
            }
            let metrics = cache_metrics_snapshot();
            tracing::debug!(
                target: "migux::static_cache",
                cache_key = %key,
                bytes = body.len(),
                ttl_secs = ttl_secs,
                memory_hits = metrics.memory_hits,
                memory_misses = metrics.memory_misses,
                disk_hits = metrics.disk_hits,
                disk_misses = metrics.disk_misses,
                "Cached static response"
            );
        } else if max_obj > 0 && ttl_secs > 0 {
            tracing::debug!(
                target: "migux::static_cache",
                cache_key = %key,
                bytes = body.len(),
                max_bytes = max_obj,
                "Cache skip: object too large"
            );
        } else if ttl_secs == 0 {
            tracing::debug!(
                target: "migux::static_cache",
                cache_key = %key,
                "Cache disabled: TTL is 0"
            );
        }
        if max_obj == 0 {
            tracing::debug!(
                target: "migux::static_cache",
                cache_key = %key,
                "Cache disabled: max object bytes is 0"
            );
        }

        Ok(resp)
    }

    async fn resolve_file(
        &self,
        req_path: &str,
        keep_alive: bool,
    ) -> anyhow::Result<FileResolution> {
        let root = self.location.root_or(self.server_cfg.root());
        let index = self.location.index_or(self.server_cfg.index());

        let rel = PathResolver::resolve_relative_path(req_path, &self.location.path, index);
        let Some(rel) = rel else {
            return Ok(FileResolution::Response(ResponseBuilder::not_found(
                keep_alive,
            )));
        };

        let file_path = format!("{}/{}", root, rel);

        let metadata = match tokio_fs::metadata(&file_path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileResolution::Response(ResponseBuilder::not_found(
                    keep_alive,
                )));
            }
            Err(_) => {
                return Ok(FileResolution::Response(ResponseBuilder::internal_error(
                    keep_alive,
                )));
            }
        };

        if !metadata.is_file() {
            return Ok(FileResolution::Response(ResponseBuilder::not_found(
                keep_alive,
            )));
        }

        let info = StaticFileInfo::from_metadata(&metadata);
        let content_type = content_type_for_path(&file_path);
        let len = metadata.len();

        Ok(FileResolution::File(ResolvedFile {
            path: file_path,
            len,
            info,
            content_type,
        }))
    }

    fn not_modified_response(
        &self,
        method: &str,
        headers: &str,
        file: &ResolvedFile,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> Option<Vec<u8>> {
        if should_return_not_modified(method, headers, &file.info.etag.value) {
            Some(build_not_modified(&file.info, keep_alive, hsts))
        } else {
            None
        }
    }

    fn head_response(&self, file: &ResolvedFile, keep_alive: bool, hsts: Option<&str>) -> Vec<u8> {
        let extra_headers = file.static_headers(hsts);
        ResponseBuilder::build_with_headers(
            "200 OK",
            Some(file.content_type.as_str()),
            file.info.content_length,
            keep_alive,
            &extra_headers,
            None,
        )
    }

    fn ok_response(
        &self,
        file: &ResolvedFile,
        body: &[u8],
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> Vec<u8> {
        let extra_headers = file.static_headers(hsts);
        ResponseBuilder::build_with_headers(
            "200 OK",
            Some(file.content_type.as_str()),
            body.len(),
            keep_alive,
            &extra_headers,
            Some(body),
        )
    }

    async fn stream_file_response<S>(
        &self,
        stream: &mut S,
        file: &ResolvedFile,
        keep_alive: bool,
        hsts: Option<&str>,
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        let mut handle = match tokio_fs::File::open(&file.path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let resp = ResponseBuilder::not_found(keep_alive);
                stream.write_all(&resp).await?;
                return Ok(());
            }
            Err(_) => {
                let resp = ResponseBuilder::internal_error(keep_alive);
                stream.write_all(&resp).await?;
                return Ok(());
            }
        };

        let extra_headers = file.static_headers(hsts);
        let head = ResponseBuilder::build_with_headers(
            "200 OK",
            Some(file.content_type.as_str()),
            file.info.content_length,
            keep_alive,
            &extra_headers,
            None,
        );
        stream.write_all(&head).await?;
        io::copy(&mut handle, stream).await?;
        Ok(())
    }
}

fn stream_threshold_bytes(http_cfg: &HttpConfig) -> u64 {
    let max_obj = http_cfg.cache_max_object_bytes().unwrap_or(0);
    let base = if max_obj == 0 {
        DEFAULT_STREAM_THRESHOLD_BYTES
    } else {
        max_obj
    };
    if http_cfg.sendfile() {
        base
    } else {
        base.saturating_mul(4)
    }
}

fn should_stream_file(len: u64, threshold: u64) -> bool {
    len >= threshold
}

/// Serve a static file directly to the client stream.
pub async fn serve_static<S>(
    stream: &mut S,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    method: &str,
    headers: &str,
    req_path: &str,
    keep_alive: bool,
    hsts: Option<&str>,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve(stream, method, headers, req_path, keep_alive, hsts)
        .await
}

/// Serve a static file using cache when enabled.
pub async fn serve_static_cached<S>(
    stream: &mut S,
    http_cfg: &HttpConfig,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    method: &str,
    headers: &str,
    req_path: &str,
    keep_alive: bool,
    hsts: Option<&str>,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve_cached(stream, http_cfg, method, headers, req_path, keep_alive, hsts)
        .await
}

/// Read a static file and return a full HTTP response.
pub async fn serve_static_bytes(
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    method: &str,
    headers: &str,
    req_path: &str,
    keep_alive: bool,
    hsts: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    StaticService::new(server_cfg, location)
        .serve_bytes(method, headers, req_path, keep_alive, hsts)
        .await
}
