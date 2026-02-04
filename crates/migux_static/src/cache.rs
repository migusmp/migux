//! Cache utilities for static responses.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
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
    fn new(response: Vec<u8>, expires_at: Instant) -> Self {
        Self {
            response,
            expires_at,
        }
    }
}

/// Compact hash key for cache entries.
pub(crate) type CacheKey = u64;

/// Cache hit/miss counters for quick tuning.
#[derive(Debug, Clone, Copy)]
pub struct CacheMetrics {
    pub memory_hits: u64,
    pub memory_misses: u64,
    pub disk_hits: u64,
    pub disk_misses: u64,
}

static MEMORY_HITS: AtomicU64 = AtomicU64::new(0);
static MEMORY_MISSES: AtomicU64 = AtomicU64::new(0);
static DISK_HITS: AtomicU64 = AtomicU64::new(0);
static DISK_MISSES: AtomicU64 = AtomicU64::new(0);

/// Global in-memory cache map for static responses.
static STATIC_CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheEntry>>> = OnceLock::new();

/// Build a compact cache key from file attributes.
pub(crate) fn build_cache_key(
    path: &str,
    len: u64,
    mtime_nanos: u128,
    hsts: bool,
) -> CacheKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    len.hash(&mut hasher);
    mtime_nanos.hash(&mut hasher);
    hsts.hash(&mut hasher);
    hasher.finish()
}

/// Snapshot current cache hit/miss counters.
pub fn cache_metrics_snapshot() -> CacheMetrics {
    CacheMetrics {
        memory_hits: MEMORY_HITS.load(Ordering::Relaxed),
        memory_misses: MEMORY_MISSES.load(Ordering::Relaxed),
        disk_hits: DISK_HITS.load(Ordering::Relaxed),
        disk_misses: DISK_MISSES.load(Ordering::Relaxed),
    }
}

pub(crate) struct MemoryCache;

impl MemoryCache {
    /// Get a reference to the global cache store.
    fn store() -> &'static Mutex<HashMap<CacheKey, CacheEntry>> {
        STATIC_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Fetch a cached response from memory, honoring expiration.
    pub(crate) fn get(key: CacheKey) -> Option<Vec<u8>> {
        let mut map = Self::store().lock().ok()?;
        if let Some(entry) = map.get(&key) {
            if Instant::now() <= entry.expires_at {
                MEMORY_HITS.fetch_add(1, Ordering::Relaxed);
                debug!(
                    target: "migux::static_cache",
                    cache_key = %key,
                    layer = "memory",
                    "Cache hit"
                );
                return Some(entry.response.clone());
            }
        }
        map.remove(&key);
        MEMORY_MISSES.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Store a response in memory with a TTL.
    pub(crate) fn put(key: CacheKey, response: Vec<u8>, ttl: Duration) {
        if ttl.as_secs() == 0 {
            return;
        }

        let entry = CacheEntry::new(response, Instant::now() + ttl);

        if let Ok(mut map) = Self::store().lock() {
            map.insert(key, entry);
        }
    }
}

pub(crate) struct DiskCache {
    cache_dir: PathBuf,
}

impl DiskCache {
    pub(crate) fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// Read a cached response from disk if present and not expired.
    pub(crate) async fn get(&self, key: CacheKey) -> Option<Vec<u8>> {
        let (data_path, meta_path) = self.cache_paths(key);
        let meta_bytes = match fs::read(&meta_path).await {
            Ok(bytes) => bytes,
            Err(_) => {
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let meta_str = match std::str::from_utf8(&meta_bytes) {
            Ok(s) => s.trim(),
            Err(_) => {
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let expires_at = match meta_str.parse::<u64>() {
            Ok(val) => val,
            Err(_) => {
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if now > expires_at {
            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
            DISK_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let data = match fs::read(&data_path).await {
            Ok(data) => data,
            Err(_) => {
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        DISK_HITS.fetch_add(1, Ordering::Relaxed);
        debug!(
            target: "migux::static_cache",
            cache_key = %key,
            layer = "disk",
            "Cache hit"
        );
        Some(data)
    }

    /// Persist a cached response and its expiration metadata to disk.
    pub(crate) async fn put(&self, key: CacheKey, response: &[u8], ttl: Duration) {
        if ttl.as_secs() == 0 {
            return;
        }

        if fs::create_dir_all(&self.cache_dir).await.is_err() {
            return;
        }

        let (data_path, meta_path) = self.cache_paths(key);
        let expires_at = SystemTime::now()
            .checked_add(ttl)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let _ = fs::write(&data_path, response).await;
        let _ = fs::write(&meta_path, expires_at.to_string()).await;
    }

    /// Resolve disk paths for a cache entry and its metadata file.
    fn cache_paths(&self, key: CacheKey) -> (PathBuf, PathBuf) {
        let mut data = self.cache_dir.clone();
        let mut meta = self.cache_dir.clone();
        data.push(format!("{:016x}.cache", key));
        meta.push(format!("{:016x}.meta", key));
        (data, meta)
    }
}

pub(crate) struct CachePolicy;

impl CachePolicy {
    /// Decide whether caching is enabled for this location and method.
    pub(crate) fn enabled(http_cfg: &HttpConfig, location: &LocationConfig, method: &str) -> bool {
        if method != "GET" {
            return false;
        }
        if http_cfg.cache_dir().is_none() {
            return false;
        }
        !matches!(location.cache(), Some(false))
    }
}
