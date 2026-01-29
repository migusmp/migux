//! Static file serving and cache utilities.
//!
//! Provides a minimal static file handler with optional in-memory and
//! disk-backed caching, respecting configured TTL and object size limits.

mod cache;
mod fs;
mod response;

use mime_guess::mime;
use std::time::{Duration, UNIX_EPOCH};
use tokio::fs as tokio_fs;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use migux_config::{HttpConfig, LocationConfig, ServerConfig};

use cache::{CachePolicy, DiskCache, MemoryCache};
use fs::PathResolver;
use response::ResponseBuilder;

struct StaticService<'a> {
    server_cfg: &'a ServerConfig,
    location: &'a LocationConfig,
}

impl<'a> StaticService<'a> {
    fn new(server_cfg: &'a ServerConfig, location: &'a LocationConfig) -> Self {
        Self {
            server_cfg,
            location,
        }
    }

    async fn serve<S>(&self, stream: &mut S, req_path: &str, keep_alive: bool) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        let resp = self.serve_bytes(req_path, keep_alive).await?;
        stream.write_all(&resp).await?;
        Ok(())
    }

    async fn serve_cached<S>(
        &self,
        stream: &mut S,
        http_cfg: &HttpConfig,
        method: &str,
        req_path: &str,
        keep_alive: bool,
    ) -> anyhow::Result<()>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        if !CachePolicy::enabled(http_cfg, self.location, method) {
            return self.serve(stream, req_path, keep_alive).await;
        }

        let resp = self.serve_bytes_cached(http_cfg, req_path, keep_alive).await?;
        stream.write_all(&resp).await?;
        Ok(())
    }

    async fn serve_bytes(&self, req_path: &str, keep_alive: bool) -> anyhow::Result<Vec<u8>> {
        let root = self.location.root_or(self.server_cfg.root());
        let index = self.location.index_or(self.server_cfg.index());

        // Resolver path relativo dentro de `root`
        let rel = PathResolver::resolve_relative_path(req_path, &self.location.path, index);

        // Si no matchea realmente esa location → 404
        let Some(rel) = rel else {
            return Ok(ResponseBuilder::not_found(keep_alive));
        };

        let file_path = format!("{}/{}", root, rel);

        match tokio_fs::read(&file_path).await {
            Ok(body) => {
                let mime = mime_guess::from_path(&file_path).first_or_octet_stream();

                let content_type = if mime.type_() == mime::TEXT {
                    format!("{}; charset=utf-8", mime.essence_str())
                } else {
                    mime.essence_str().to_string()
                };

                Ok(ResponseBuilder::build(
                    "200 OK",
                    Some(&content_type),
                    &body,
                    keep_alive,
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(ResponseBuilder::not_found(keep_alive))
            }
            Err(_) => Ok(ResponseBuilder::internal_error(keep_alive)),
        }
    }

    async fn serve_bytes_cached(
        &self,
        http_cfg: &HttpConfig,
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
                return Ok(ResponseBuilder::not_found(keep_alive))
            }
            Err(_) => return Ok(ResponseBuilder::internal_error(keep_alive)),
        };

        if !metadata.is_file() {
            return Ok(ResponseBuilder::not_found(keep_alive));
        }

        let mtime_key = metadata
            .modified()
            .ok()
            .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
            .map(|dur| dur.as_nanos().to_string())
            .unwrap_or_else(|| "0".to_string());

        let key = format!("{file_path}|{mtime_key}");

        if let Some(resp) = MemoryCache::get(&key) {
            return Ok(resp);
        }

        let body = match tokio_fs::read(&file_path).await {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ResponseBuilder::not_found(keep_alive))
            }
            Err(_) => return Ok(ResponseBuilder::internal_error(keep_alive)),
        };

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

        let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
        let content_type = if mime.type_() == mime::TEXT {
            format!("{}; charset=utf-8", mime.essence_str())
        } else {
            mime.essence_str().to_string()
        };

        let resp = ResponseBuilder::build("200 OK", Some(&content_type), &body, keep_alive);

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
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve(stream, req_path, keep_alive)
        .await
}

/// Serve a static file using cache when enabled.
pub async fn serve_static_cached<S>(
    stream: &mut S,
    http_cfg: &HttpConfig,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    method: &str,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    StaticService::new(server_cfg, location)
        .serve_cached(stream, http_cfg, method, req_path, keep_alive)
        .await
}

/// Read a static file and return a full HTTP response.
pub async fn serve_static_bytes(
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<Vec<u8>> {
    StaticService::new(server_cfg, location)
        .serve_bytes(req_path, keep_alive)
        .await
}

// Helpers are defined in the `cache`, `fs`, and `response` modules.
