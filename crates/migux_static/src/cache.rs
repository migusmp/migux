//! Cache utilities for static responses.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use migux_config::{HttpConfig, LocationConfig};
use tokio::fs;
use tracing::debug;

/// In-memory cache entry with expiration.
struct CacheEntry {
    response: Vec<u8>,
    expires_at: Instant,
}

impl CacheEntry {
    pub fn new(response: Vec<u8>, expires_at: Instant) -> Self {
        Self {
            response,
            expires_at
        }
    }
}

/// Global in-memory cache map for static responses.
static STATIC_CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

/// Get a reference to the global cache store.
fn cache_store() -> &'static Mutex<HashMap<String, CacheEntry>> {
    STATIC_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetch a cached response from memory, honoring expiration.
pub(crate) fn cache_get(key: &str) -> Option<Vec<u8>> {
    let mut map = cache_store().lock().ok()?;
    if let Some(entry) = map.get(key) {
        if Instant::now() <= entry.expires_at {
            debug!(target: "migux::static_cache", cache_key = %key, layer = "memory", "Cache hit");
            return Some(entry.response.clone());
        }
    }
    map.remove(key);
    None
}

/// Store a response in memory with a TTL.
pub(crate) fn cache_put(key: String, response: Vec<u8>, ttl: Duration) {
    if ttl.as_secs() == 0 {
        return;
    }

    let entry = CacheEntry::new(response, Instant::now() + ttl);
    
    if let Ok(mut map) = cache_store().lock() {
        map.insert(key, entry);
    }
}

/// Read a cached response from disk if present and not expired.
pub(crate) async fn disk_cache_get(cache_dir: &str, key: &str) -> Option<Vec<u8>> {
    let (data_path, meta_path) = cache_paths(cache_dir, key);
    let meta_bytes = fs::read(&meta_path).await.ok()?;
    let meta_str = std::str::from_utf8(&meta_bytes).ok()?.trim();
    let expires_at = meta_str.parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now > expires_at {
        let _ = fs::remove_file(&data_path).await;
        let _ = fs::remove_file(&meta_path).await;
        return None;
    }

    let data = fs::read(&data_path).await.ok()?;
    debug!(
        target: "migux::static_cache",
        cache_key = %key,
        layer = "disk",
        "Cache hit"
    );
    Some(data)
}

/// Persist a cached response and its expiration metadata to disk.
pub(crate) async fn disk_cache_put(cache_dir: &str, key: &str, response: &[u8], ttl: Duration) {
    if ttl.as_secs() == 0 {
        return;
    }

    if fs::create_dir_all(cache_dir).await.is_err() {
        return;
    }

    let (data_path, meta_path) = cache_paths(cache_dir, key);
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let _ = fs::write(&data_path, response).await;
    let _ = fs::write(&meta_path, expires_at.to_string()).await;
}

/// Decide whether caching is enabled for this location and method.
pub(crate) fn cache_enabled(http_cfg: &HttpConfig, location: &LocationConfig, method: &str) -> bool {
    if method != "GET" {
        return false;
    }
    if http_cfg.cache_dir.is_none() {
        return false;
    }
    !matches!(location.cache, Some(false))
}

/// Resolve disk paths for a cache entry and its metadata file.
fn cache_paths(cache_dir: &str, key: &str) -> (PathBuf, PathBuf) {
    let hash = cache_key_hash(key);
    let mut data = PathBuf::from(cache_dir);
    let mut meta = PathBuf::from(cache_dir);
    data.push(format!("{:016x}.cache", hash));
    meta.push(format!("{:016x}.meta", hash));
    (data, meta)
}

/// Hash a cache key to a stable filename.
fn cache_key_hash(key: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}
