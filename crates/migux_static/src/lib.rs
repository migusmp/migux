//! Static file serving and cache utilities.
//!
//! Provides a minimal static file handler with optional in-memory and
//! disk-backed caching, respecting configured TTL and object size limits.

mod cache;
mod fs;
mod response;

use mime_guess::mime;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::fs as tokio_fs;
use std::time::{Duration, UNIX_EPOCH};

use migux_config::{HttpConfig, LocationConfig, ServerConfig};
use cache::{cache_enabled, cache_get, cache_put, disk_cache_get, disk_cache_put};
use fs::resolve_relative_path;
use response::{build_404, build_500, build_response_bytes};

/// Serve a static file directly to the client stream.
pub async fn serve_static(
    stream: &mut (impl AsyncWrite + Unpin),
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<()> {
    let resp = serve_static_bytes(server_cfg, location, req_path, keep_alive).await?;
    stream.write_all(&resp).await?;
    Ok(())
}

/// Serve a static file using cache when enabled.
pub async fn serve_static_cached(
    stream: &mut (impl AsyncWrite + Unpin),
    http_cfg: &HttpConfig,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    method: &str,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<()> {
    if !cache_enabled(http_cfg, location, method) {
        return serve_static(stream, server_cfg, location, req_path, keep_alive).await;
    }

    let resp = serve_static_bytes_cached(http_cfg, server_cfg, location, req_path, keep_alive).await?;
    stream.write_all(&resp).await?;
    Ok(())
}

/// Read a static file and return a full HTTP response.
pub async fn serve_static_bytes(
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<Vec<u8>> {
    let root = location.root.as_deref().unwrap_or(&server_cfg.root);
    let index = location.index.as_deref().unwrap_or(&server_cfg.index);

    // Resolver path relativo dentro de `root`
    let rel = resolve_relative_path(req_path, &location.path, index);

    // Si no matchea realmente esa location → 404
    let Some(rel) = rel else {
        return Ok(build_404(keep_alive));
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

            Ok(build_response_bytes("200 OK", Some(&content_type), &body, keep_alive))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(build_404(keep_alive)),
        Err(_) => Ok(build_500(keep_alive)),
    }
}

/// Read a static file with cache support and return a full HTTP response.
async fn serve_static_bytes_cached(
    http_cfg: &HttpConfig,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
    keep_alive: bool,
) -> anyhow::Result<Vec<u8>> {
    let root = location.root.as_deref().unwrap_or(&server_cfg.root);
    let index = location.index.as_deref().unwrap_or(&server_cfg.index);

    let rel = resolve_relative_path(req_path, &location.path, index);
    let Some(rel) = rel else {
        return Ok(build_404(keep_alive));
    };

    let file_path = format!("{}/{}", root, rel);

    let metadata = match tokio_fs::metadata(&file_path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(build_404(keep_alive)),
        Err(_) => return Ok(build_500(keep_alive)),
    };

    if !metadata.is_file() {
        return Ok(build_404(keep_alive));
    }

    let mtime_key = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|dur| dur.as_nanos().to_string())
        .unwrap_or_else(|| "0".to_string());

    let key = format!("{file_path}|{mtime_key}");

    if let Some(resp) = cache_get(&key) {
        return Ok(resp);
    }

    let body = match tokio_fs::read(&file_path).await {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(build_404(keep_alive)),
        Err(_) => return Ok(build_500(keep_alive)),
    };

    let max_obj = http_cfg.cache_max_object_bytes.unwrap_or(0);
    let ttl_secs = http_cfg.cache_default_ttl_secs.unwrap_or(0) as u64;
    let ttl = Duration::from_secs(ttl_secs);

    if let Some(cache_dir) = http_cfg.cache_dir.as_deref() {
        if let Some(resp) = disk_cache_get(cache_dir, &key).await {
            if ttl_secs > 0 {
                cache_put(key.clone(), resp.clone(), ttl);
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

    let resp = build_response_bytes("200 OK", Some(&content_type), &body, keep_alive);

    if max_obj > 0 && (body.len() as u64) <= max_obj && ttl_secs > 0 {
        cache_put(key.clone(), resp.clone(), ttl);
        if let Some(cache_dir) = http_cfg.cache_dir.as_deref() {
            disk_cache_put(cache_dir, &key, &resp, ttl).await;
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

// Helpers are defined in the `cache` and `fs` modules.
