//! Static file serving and cache utilities.
//!
//! Provides a minimal static file handler with optional in-memory and
//! disk-backed caching, respecting configured TTL and object size limits.

mod cache;
mod etag;
mod fs;
mod response;

use httpdate::fmt_http_date;
use mime_guess::mime;
use std::time::{Duration, SystemTime};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use migux_config::{HttpConfig, LocationConfig, ServerConfig};

use cache::{CachePolicy, DiskCache, MemoryCache};
use fs::PathResolver;
use response::ResponseBuilder;

use crate::etag::{last_modified_header, weak_etag_size_mtime, EtagInfo};

struct StaticService<'a> {
    server_cfg: &'a ServerConfig,
    location: &'a LocationConfig,
}

#[derive(Debug)]
enum IfNoneMatch {
    Any,
    Tags(Vec<String>),
}

struct StaticFileInfo {
    content_length: usize,
    etag: EtagInfo,
    last_modified: Option<String>,
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

fn parse_if_none_match(headers: &str) -> Option<IfNoneMatch> {
    let mut combined = String::new();
    for line in headers.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("if-none-match") {
            if !combined.is_empty() {
                combined.push(',');
            }
            combined.push_str(value.trim());
        }
    }

    if combined.is_empty() {
        None
    } else {
        parse_if_none_match_value(&combined)
    }
}

fn parse_if_none_match_value(value: &str) -> Option<IfNoneMatch> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value == "*" {
        return Some(IfNoneMatch::Any);
    }

    let mut tags = Vec::new();
    for token in value.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token == "*" {
            return Some(IfNoneMatch::Any);
        }
        if let Some(tag) = normalize_etag_token(token) {
            tags.push(tag);
        }
    }

    if tags.is_empty() {
        None
    } else {
        Some(IfNoneMatch::Tags(tags))
    }
}

fn normalize_etag_token(token: &str) -> Option<String> {
    let mut tag = token.trim();
    if tag.is_empty() {
        return None;
    }

    if let Some(stripped) = tag.strip_prefix("W/").or_else(|| tag.strip_prefix("w/")) {
        tag = stripped.trim();
    }

    if (tag.starts_with('"') && tag.ends_with('"')) || (tag.starts_with('\'') && tag.ends_with('\'')) {
        tag = &tag[1..tag.len() - 1];
    }

    let tag = tag.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn if_none_match_satisfied(if_none_match: &IfNoneMatch, etag_value: &str) -> bool {
    match if_none_match {
        IfNoneMatch::Any => true,
        IfNoneMatch::Tags(tags) => tags.iter().any(|tag| tag == etag_value),
    }
}

fn should_return_not_modified(method: &str, headers: &str, etag_value: &str) -> bool {
    if method != "GET" && method != "HEAD" {
        return false;
    }
    let Some(if_none_match) = parse_if_none_match(headers) else {
        return false;
    };
    if_none_match_satisfied(&if_none_match, etag_value)
}

fn build_static_headers<'a>(info: &'a StaticFileInfo) -> Vec<(&'static str, &'a str)> {
    let mut headers = Vec::new();
    headers.push(("ETag", info.etag.header.as_str()));
    if let Some(last_modified) = info.last_modified.as_deref() {
        headers.push(("Last-Modified", last_modified));
    }
    headers
}

fn build_not_modified(info: &StaticFileInfo, keep_alive: bool) -> Vec<u8> {
    let date = fmt_http_date(SystemTime::now());
    let mut headers = build_static_headers(info);
    headers.push(("Date", date.as_str()));
    ResponseBuilder::build_with_headers(
        "304 Not Modified",
        None,
        0,
        keep_alive,
        &headers,
        None,
    )
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
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        let resp = self.serve_bytes(method, headers, req_path, keep_alive).await?;
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
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        if !CachePolicy::enabled(http_cfg, self.location, method) {
            return self.serve(stream, method, headers, req_path, keep_alive).await;
        }

        let resp = self
            .serve_bytes_cached(http_cfg, method, headers, req_path, keep_alive)
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
    ) -> anyhow::Result<Vec<u8>> {
        let root = self.location.root_or(self.server_cfg.root());
        let index = self.location.index_or(self.server_cfg.index());

        // Resolver path relativo dentro de `root`
        let rel = PathResolver::resolve_relative_path(req_path, &self.location.path, index);

        // Si no matchea realmente esa location → 404
        let Some(rel) = rel else {
            return Ok(ResponseBuilder::not_found(keep_alive));
        };

        let file_path = format!("{}/{}", root, rel);

        let metadata = match tokio_fs::metadata(&file_path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ResponseBuilder::not_found(keep_alive));
            }
            Err(_) => return Ok(ResponseBuilder::internal_error(keep_alive)),
        };

        if !metadata.is_file() {
            return Ok(ResponseBuilder::not_found(keep_alive));
        }

        let info = StaticFileInfo::from_metadata(&metadata);

        if should_return_not_modified(method, headers, &info.etag.value) {
            return Ok(build_not_modified(&info, keep_alive));
        }

        let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
        let content_type = if mime.type_() == mime::TEXT {
            format!("{}; charset=utf-8", mime.essence_str())
        } else {
            mime.essence_str().to_string()
        };

        let extra_headers = build_static_headers(&info);

        if method == "HEAD" {
            return Ok(ResponseBuilder::build_with_headers(
                "200 OK",
                Some(&content_type),
                info.content_length,
                keep_alive,
                &extra_headers,
                None,
            ));
        }

        match tokio_fs::read(&file_path).await {
            Ok(body) => Ok(ResponseBuilder::build_with_headers(
                "200 OK",
                Some(&content_type),
                body.len(),
                keep_alive,
                &extra_headers,
                Some(&body),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(ResponseBuilder::not_found(keep_alive))
            }
            Err(_) => Ok(ResponseBuilder::internal_error(keep_alive)),
        }
    }

    async fn serve_bytes_cached(
        &self,
        http_cfg: &HttpConfig,
        method: &str,
        headers: &str,
        req_path: &str,
        keep_alive: bool,
    ) -> anyhow::Result<Vec<u8>> {
        let root = self.location.root_or(self.server_cfg.root());
        let index = self.location.index_or(self.server_cfg.index());

        let rel = PathResolver::resolve_relative_path(req_path, &self.location.path, index);
        let Some(rel) = rel else {
            return Ok(ResponseBuilder::not_found(keep_alive));
        };

        let file_path = format!("{}/{}", root, rel);
        let metadata = match tokio_fs::metadata(&file_path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ResponseBuilder::not_found(keep_alive));
            }
            Err(_) => return Ok(ResponseBuilder::internal_error(keep_alive)),
        };

        if !metadata.is_file() {
            return Ok(ResponseBuilder::not_found(keep_alive));
        }

        let info = StaticFileInfo::from_metadata(&metadata);

        if should_return_not_modified(method, headers, &info.etag.value) {
            return Ok(build_not_modified(&info, keep_alive));
        }

        let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
        let content_type = if mime.type_() == mime::TEXT {
            format!("{}; charset=utf-8", mime.essence_str())
        } else {
            mime.essence_str().to_string()
        };

        let extra_headers = build_static_headers(&info);

        if method == "HEAD" {
            return Ok(ResponseBuilder::build_with_headers(
                "200 OK",
                Some(&content_type),
                info.content_length,
                keep_alive,
                &extra_headers,
                None,
            ));
        }

        let key = format!("{file_path}|{}|{}", metadata.len(), info.etag.mtime_nanos);

        if let Some(resp) = MemoryCache::get(&key) {
            return Ok(resp);
        }

        let max_obj = http_cfg.cache_max_object_bytes().unwrap_or(0);
        let ttl_secs = http_cfg.cache_default_ttl_secs().unwrap_or(0) as u64;
        let ttl = Duration::from_secs(ttl_secs);

        if let Some(cache_dir) = http_cfg.cache_dir() {
            let disk_cache = DiskCache::new(cache_dir);
            if let Some(resp) = disk_cache.get(&key).await {
                if ttl_secs > 0 {
                    MemoryCache::put(key.clone(), resp.clone(), ttl);
                }
                return Ok(resp);
            }
        }

        tracing::debug!(target: "migux::static_cache", cache_key = %key, "Cache miss");
        let body = match tokio_fs::read(&file_path).await {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ResponseBuilder::not_found(keep_alive));
            }
            Err(_) => return Ok(ResponseBuilder::internal_error(keep_alive)),
        };

        let resp = ResponseBuilder::build_with_headers(
            "200 OK",
            Some(&content_type),
            body.len(),
            keep_alive,
            &extra_headers,
            Some(&body),
        );

        if max_obj > 0 && (body.len() as u64) <= max_obj && ttl_secs > 0 {
            MemoryCache::put(key.clone(), resp.clone(), ttl);
            if let Some(cache_dir) = http_cfg.cache_dir() {
                DiskCache::new(cache_dir).put(&key, &resp, ttl).await;
            }
            tracing::debug!(
                target: "migux::static_cache",
                cache_key = %key,
                bytes = body.len(),
                ttl_secs = ttl_secs,
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
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve(stream, method, headers, req_path, keep_alive)
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
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve_cached(stream, http_cfg, method, headers, req_path, keep_alive)
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
) -> anyhow::Result<Vec<u8>> {
    StaticService::new(server_cfg, location)
        .serve_bytes(method, headers, req_path, keep_alive)
        .await
}

// Helpers are defined in the `cache`, `fs`, and `response` modules.

#[cfg(test)]
mod tests {
    use super::{
        if_none_match_satisfied, parse_if_none_match, parse_if_none_match_value, IfNoneMatch,
    };

    #[test]
    fn parse_if_none_match_single() {
        let parsed = parse_if_none_match_value(r#""abc""#).expect("expected tags");
        assert!(if_none_match_satisfied(&parsed, "abc"));
        assert!(!if_none_match_satisfied(&parsed, "nope"));
    }

    #[test]
    fn parse_if_none_match_list() {
        let parsed = parse_if_none_match_value(r#""a", "b", W/"c""#).expect("expected tags");
        assert!(if_none_match_satisfied(&parsed, "b"));
        assert!(if_none_match_satisfied(&parsed, "c"));
        assert!(!if_none_match_satisfied(&parsed, "d"));
        match parsed {
            IfNoneMatch::Tags(tags) => assert_eq!(tags, vec!["a", "b", "c"]),
            IfNoneMatch::Any => panic!("expected tag list"),
        }
    }

    #[test]
    fn parse_if_none_match_wildcard() {
        let parsed = parse_if_none_match_value("*").expect("expected wildcard");
        assert!(matches!(parsed, IfNoneMatch::Any));
        assert!(if_none_match_satisfied(&parsed, "anything"));
    }

    #[test]
    fn parse_if_none_match_header() {
        let headers = "GET / HTTP/1.1\r\nHost: example\r\nIf-None-Match: \"etag\"\r\n\r\n";
        let parsed = parse_if_none_match(headers).expect("expected header value");
        assert!(if_none_match_satisfied(&parsed, "etag"));
    }
}
