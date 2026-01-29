use mime_guess::mime;
use tokio::{fs, io::{AsyncWrite, AsyncWriteExt}};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use migux_config::{HttpConfig, LocationConfig, ServerConfig};

struct CacheEntry {
    response: Vec<u8>,
    expires_at: Instant,
}

static STATIC_CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

fn cache_store() -> &'static Mutex<HashMap<String, CacheEntry>> {
    STATIC_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(key: &str) -> Option<Vec<u8>> {
    let mut map = cache_store().lock().ok()?;
    if let Some(entry) = map.get(key) {
        if Instant::now() <= entry.expires_at {
            tracing::debug!(target: "migux::static_cache", cache_key = %key, layer = "memory", "Cache hit");
            return Some(entry.response.clone());
        }
    }
    map.remove(key);
    None
}

fn cache_put(key: String, response: Vec<u8>, ttl: Duration) {
    if ttl.as_secs() == 0 {
        return;
    }
    let entry = CacheEntry {
        response,
        expires_at: Instant::now() + ttl,
    };
    if let Ok(mut map) = cache_store().lock() {
        map.insert(key, entry);
    }
}

async fn disk_cache_get(cache_dir: &str, key: &str) -> Option<Vec<u8>> {
    let (data_path, meta_path) = cache_paths(cache_dir, key);
    let meta_bytes = tokio::fs::read(&meta_path).await.ok()?;
    let meta_str = std::str::from_utf8(&meta_bytes).ok()?.trim();
    let expires_at = meta_str.parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now > expires_at {
        let _ = tokio::fs::remove_file(&data_path).await;
        let _ = tokio::fs::remove_file(&meta_path).await;
        return None;
    }

    let data = tokio::fs::read(&data_path).await.ok()?;
    tracing::debug!(
        target: "migux::static_cache",
        cache_key = %key,
        layer = "disk",
        "Cache hit"
    );
    Some(data)
}

async fn disk_cache_put(cache_dir: &str, key: &str, response: &[u8], ttl: Duration) {
    if ttl.as_secs() == 0 {
        return;
    }

    if tokio::fs::create_dir_all(cache_dir).await.is_err() {
        return;
    }

    let (data_path, meta_path) = cache_paths(cache_dir, key);
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let _ = tokio::fs::write(&data_path, response).await;
    let _ = tokio::fs::write(&meta_path, expires_at.to_string()).await;
}

fn cache_paths(cache_dir: &str, key: &str) -> (PathBuf, PathBuf) {
    let hash = cache_key_hash(key);
    let mut data = PathBuf::from(cache_dir);
    let mut meta = PathBuf::from(cache_dir);
    data.push(format!("{:016x}.cache", hash));
    meta.push(format!("{:016x}.meta", hash));
    (data, meta)
}

fn cache_key_hash(key: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn build_response_bytes(
    status: &str,
    content_type: Option<&str>,
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let mut headers = String::new();

    headers.push_str(&format!("HTTP/1.1 {}\r\n", status));
    headers.push_str(&format!("Content-Length: {}\r\n", body.len()));

    if let Some(ct) = content_type {
        headers.push_str(&format!("Content-Type: {}\r\n", ct));
    }

    if keep_alive {
        headers.push_str("Connection: keep-alive\r\n");
    } else {
        headers.push_str("Connection: close\r\n");
    }
    headers.push_str("\r\n");

    let mut out = headers.into_bytes();
    out.extend_from_slice(body);
    out
}

fn build_404(keep_alive: bool) -> Vec<u8> {
    let body = b"404 Not Found";
    build_response_bytes("404 Not Found", Some("text/plain; charset=utf-8"), body, keep_alive)
}

fn build_500(keep_alive: bool) -> Vec<u8> {
    let body = b"500 Internal Server Error";
    build_response_bytes(
        "500 Internal Server Error",
        Some("text/plain; charset=utf-8"),
        body,
        keep_alive,
    )
}

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

    match fs::read(&file_path).await {
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

    let metadata = match fs::metadata(&file_path).await {
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

    let body = match fs::read(&file_path).await {
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

/// Resuelve la ruta relativa al root, teniendo en cuenta el index y el prefijo.
fn resolve_relative_path(req_path: &str, location_path: &str, index: &str) -> Option<String> {
    if req_path == "/" && location_path == "/" {
        return Some(index.to_string());
    }

    if req_path == location_path {
        return Some(index.to_string());
    }

    if req_path.starts_with(location_path) {
        let mut tail = &req_path[location_path.len()..];

        if tail.starts_with('/') {
            tail = &tail[1..];
        }

        if tail.is_empty() {
            Some(index.to_string())
        } else {
            Some(tail.to_string())
        }
    } else {
        None
    }
}

fn cache_enabled(http_cfg: &HttpConfig, location: &LocationConfig, method: &str) -> bool {
    if method != "GET" {
        return false;
    }
    if http_cfg.cache_dir.is_none() {
        return false;
    }
    !matches!(location.cache, Some(false))
}
