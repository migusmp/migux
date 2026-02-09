//! Cache utilities for static responses.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use migux_config::{HttpConfig, LocationConfig};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex as AsyncMutex};
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
    pub disk_evictions: u64,
    pub disk_evicted_bytes: u64,
    pub disk_bytes: u64,
    pub disk_entries: u64,
}

static MEMORY_HITS: AtomicU64 = AtomicU64::new(0);
static MEMORY_MISSES: AtomicU64 = AtomicU64::new(0);
static DISK_HITS: AtomicU64 = AtomicU64::new(0);
static DISK_MISSES: AtomicU64 = AtomicU64::new(0);
static DISK_EVICTIONS: AtomicU64 = AtomicU64::new(0);
static DISK_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Global in-memory cache map for static responses.
static STATIC_CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheEntry>>> = OnceLock::new();
/// Global disk cache index for size/LRU tracking.
static DISK_CACHE_INDEX: OnceLock<AsyncMutex<DiskCacheIndex>> = OnceLock::new();

/// Build a compact cache key from file attributes.
pub(crate) fn build_cache_key(path: &str, len: u64, mtime_nanos: u128, hsts: bool) -> CacheKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    len.hash(&mut hasher);
    mtime_nanos.hash(&mut hasher);
    hsts.hash(&mut hasher);
    hasher.finish()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Snapshot current cache hit/miss counters and disk usage.
pub async fn cache_metrics_snapshot() -> CacheMetrics {
    let (disk_bytes, disk_entries) = match DISK_CACHE_INDEX.get() {
        Some(lock) => {
            let index = lock.lock().await;
            (index.total_bytes, index.entries.len() as u64)
        }
        None => (0, 0),
    };

    CacheMetrics {
        memory_hits: MEMORY_HITS.load(Ordering::Relaxed),
        memory_misses: MEMORY_MISSES.load(Ordering::Relaxed),
        disk_hits: DISK_HITS.load(Ordering::Relaxed),
        disk_misses: DISK_MISSES.load(Ordering::Relaxed),
        disk_evictions: DISK_EVICTIONS.load(Ordering::Relaxed),
        disk_evicted_bytes: DISK_EVICTED_BYTES.load(Ordering::Relaxed),
        disk_bytes,
        disk_entries,
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

struct DiskMetaRecord {
    expires_at: u64,
    last_access: u64,
    size: u64,
}

#[derive(Clone, Debug)]
struct DiskEntryMeta {
    size: u64,
    expires_at: u64,
    last_access: u64,
    prev: Option<CacheKey>,
    next: Option<CacheKey>,
}

#[derive(Clone, Copy)]
struct CacheSettings {
    max_total_bytes: u64,
    max_entries: usize,
    inactive_secs: u64,
}

impl CacheSettings {
    fn from(http_cfg: &HttpConfig) -> Self {
        Self {
            max_total_bytes: http_cfg.cache_max_total_bytes().unwrap_or(0),
            max_entries: http_cfg.cache_max_entries().unwrap_or(0),
            inactive_secs: http_cfg.cache_inactive_secs().unwrap_or(0),
        }
    }

    fn exceeds_limits(&self, total_bytes: u64, entries: usize) -> bool {
        if self.max_total_bytes > 0 && total_bytes > self.max_total_bytes {
            return true;
        }
        if self.max_entries > 0 && entries > self.max_entries {
            return true;
        }
        false
    }
}

struct DiskCacheIndex {
    cache_dir: PathBuf,
    entries: HashMap<CacheKey, DiskEntryMeta>,
    lru_head: Option<CacheKey>,
    lru_tail: Option<CacheKey>,
    total_bytes: u64,
    loaded: bool,
}

fn cache_paths_for(cache_dir: &Path, key: CacheKey) -> (PathBuf, PathBuf) {
    let mut data = cache_dir.to_path_buf();
    let mut meta = cache_dir.to_path_buf();
    data.push(format!("{:016x}.cache", key));
    meta.push(format!("{:016x}.meta", key));
    (data, meta)
}

fn key_from_meta_path(path: &Path) -> Option<CacheKey> {
    let file_name = path.file_name()?.to_str()?;
    let hex = file_name.strip_suffix(".meta")?;
    u64::from_str_radix(hex, 16).ok()
}

fn parse_meta_record(meta_str: &str) -> Option<DiskMetaRecord> {
    let trimmed = meta_str.trim();
    if let Ok(expires_at) = trimmed.parse::<u64>() {
        return Some(DiskMetaRecord {
            expires_at,
            last_access: expires_at,
            size: 0,
        });
    }

    let mut expires_at = None;
    let mut last_access = None;
    let mut size = None;
    for line in meta_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "expires_at" => {
                expires_at = value.parse::<u64>().ok();
            }
            "last_access" => {
                last_access = value.parse::<u64>().ok();
            }
            "size" => {
                size = value.parse::<u64>().ok();
            }
            _ => {}
        }
    }

    let expires_at = expires_at?;
    let size = size.unwrap_or(0);
    let last_access = last_access.unwrap_or(expires_at);
    Some(DiskMetaRecord {
        expires_at,
        last_access,
        size,
    })
}

fn format_meta_record(record: &DiskMetaRecord) -> String {
    format!(
        "expires_at={}\nlast_access={}\nsize={}\n",
        record.expires_at, record.last_access, record.size
    )
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cache.tmp");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!("{file_name}.tmp-{nanos}");
    let mut tmp_path = path.to_path_buf();
    tmp_path.set_file_name(tmp_name);
    tmp_path
}

async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp_path = temp_path_for(path);
    let mut file = fs::File::create(&tmp_path).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}

async fn write_temp_file(path: &Path, data: &[u8]) -> std::io::Result<PathBuf> {
    let tmp_path = temp_path_for(path);
    let mut file = fs::File::create(&tmp_path).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    Ok(tmp_path)
}

impl DiskCacheIndex {
    fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            entries: HashMap::new(),
            lru_head: None,
            lru_tail: None,
            total_bytes: 0,
            loaded: false,
        }
    }

    fn reset(&mut self, cache_dir: PathBuf) {
        self.cache_dir = cache_dir;
        self.entries.clear();
        self.lru_head = None;
        self.lru_tail = None;
        self.total_bytes = 0;
        self.loaded = false;
    }

    async fn ensure_loaded(&mut self, http_cfg: &HttpConfig) {
        if self.loaded {
            return;
        }
        self.load_from_disk(http_cfg).await;
    }

    async fn load_from_disk(&mut self, http_cfg: &HttpConfig) {
        self.loaded = true;
        self.entries.clear();
        self.lru_head = None;
        self.lru_tail = None;
        self.total_bytes = 0;

        let settings = CacheSettings::from(http_cfg);
        let now = now_epoch_secs();
        let mut loaded_entries: Vec<(CacheKey, DiskEntryMeta)> = Vec::new();
        let mut stale_keys: Vec<CacheKey> = Vec::new();

        let Ok(mut dir) = fs::read_dir(&self.cache_dir).await else {
            return;
        };

        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("meta") {
                continue;
            }
            let Some(key) = key_from_meta_path(&path) else {
                continue;
            };
            let meta_bytes = match fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    stale_keys.push(key);
                    continue;
                }
            };
            let meta_str = match std::str::from_utf8(&meta_bytes) {
                Ok(s) => s,
                Err(_) => {
                    stale_keys.push(key);
                    continue;
                }
            };
            let Some(mut record) = parse_meta_record(meta_str) else {
                stale_keys.push(key);
                continue;
            };
            if record.expires_at == 0 || now > record.expires_at {
                stale_keys.push(key);
                continue;
            }
            if settings.inactive_secs > 0
                && now.saturating_sub(record.last_access) > settings.inactive_secs
            {
                stale_keys.push(key);
                continue;
            }

            let (data_path, _) = cache_paths_for(&self.cache_dir, key);
            let meta = match fs::metadata(&data_path).await {
                Ok(meta) => meta,
                Err(_) => {
                    stale_keys.push(key);
                    continue;
                }
            };

            if record.size == 0 {
                record.size = meta.len();
            }
            if settings.max_total_bytes > 0 && record.size > settings.max_total_bytes {
                stale_keys.push(key);
                continue;
            }

            loaded_entries.push((
                key,
                DiskEntryMeta {
                    size: record.size,
                    expires_at: record.expires_at,
                    last_access: record.last_access,
                    prev: None,
                    next: None,
                },
            ));
        }

        loaded_entries.sort_by_key(|(_, meta)| meta.last_access);
        for (key, meta) in loaded_entries {
            self.insert(key, meta);
        }

        let evicted_keys = self.evict_to_limits(settings);
        stale_keys.extend(evicted_keys);

        for key in stale_keys {
            let (data_path, meta_path) = cache_paths_for(&self.cache_dir, key);
            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
        }
    }

    fn insert(&mut self, key: CacheKey, mut meta: DiskEntryMeta) {
        let _ = self.remove(key);
        meta.prev = None;
        meta.next = None;
        self.total_bytes = self.total_bytes.saturating_add(meta.size);
        self.entries.insert(key, meta);
        self.attach_to_tail(key);
    }

    fn touch(&mut self, key: CacheKey, now: u64) -> Option<DiskMetaRecord> {
        {
            let entry = self.entries.get_mut(&key)?;
            entry.last_access = now;
        }
        if self.lru_tail != Some(key) {
            self.detach(key);
            self.attach_to_tail(key);
        }
        let entry = self.entries.get(&key)?;
        Some(DiskMetaRecord {
            expires_at: entry.expires_at,
            last_access: entry.last_access,
            size: entry.size,
        })
    }

    fn remove(&mut self, key: CacheKey) -> Option<DiskEntryMeta> {
        let (prev, next, size) = {
            let entry = self.entries.get(&key)?;
            (entry.prev, entry.next, entry.size)
        };

        if let Some(prev_key) = prev {
            if let Some(prev_entry) = self.entries.get_mut(&prev_key) {
                prev_entry.next = next;
            }
        } else {
            self.lru_head = next;
        }

        if let Some(next_key) = next {
            if let Some(next_entry) = self.entries.get_mut(&next_key) {
                next_entry.prev = prev;
            }
        } else {
            self.lru_tail = prev;
        }

        self.total_bytes = self.total_bytes.saturating_sub(size);
        self.entries.remove(&key)
    }

    fn pop_lru(&mut self) -> Option<(CacheKey, DiskEntryMeta)> {
        let key = self.lru_head?;
        self.remove(key).map(|meta| (key, meta))
    }

    fn evict_to_limits(&mut self, settings: CacheSettings) -> Vec<CacheKey> {
        let mut evicted = Vec::new();
        while settings.exceeds_limits(self.total_bytes, self.entries.len()) {
            let Some((key, meta)) = self.pop_lru() else {
                break;
            };
            DISK_EVICTIONS.fetch_add(1, Ordering::Relaxed);
            DISK_EVICTED_BYTES.fetch_add(meta.size, Ordering::Relaxed);
            evicted.push(key);
        }
        evicted
    }

    fn evict_inactive(&mut self, now: u64, inactive_secs: u64) -> Vec<CacheKey> {
        if inactive_secs == 0 {
            return Vec::new();
        }
        let mut evicted = Vec::new();
        loop {
            let Some(key) = self.lru_head else {
                break;
            };
            let inactive = match self.entries.get(&key) {
                Some(entry) => now.saturating_sub(entry.last_access) > inactive_secs,
                None => false,
            };
            if !inactive {
                break;
            }
            if let Some((_, meta)) = self.pop_lru() {
                DISK_EVICTIONS.fetch_add(1, Ordering::Relaxed);
                DISK_EVICTED_BYTES.fetch_add(meta.size, Ordering::Relaxed);
                evicted.push(key);
            } else {
                break;
            }
        }
        evicted
    }

    fn detach(&mut self, key: CacheKey) {
        let (prev, next) = match self.entries.get(&key) {
            Some(entry) => (entry.prev, entry.next),
            None => return,
        };

        if let Some(prev_key) = prev {
            if let Some(prev_entry) = self.entries.get_mut(&prev_key) {
                prev_entry.next = next;
            }
        } else {
            self.lru_head = next;
        }

        if let Some(next_key) = next {
            if let Some(next_entry) = self.entries.get_mut(&next_key) {
                next_entry.prev = prev;
            }
        } else {
            self.lru_tail = prev;
        }

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.prev = None;
            entry.next = None;
        }
    }

    fn attach_to_tail(&mut self, key: CacheKey) {
        match self.lru_tail {
            Some(tail_key) => {
                if let Some(tail_entry) = self.entries.get_mut(&tail_key) {
                    tail_entry.next = Some(key);
                }
            }
            None => {
                self.lru_head = Some(key);
            }
        }

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.prev = self.lru_tail;
            entry.next = None;
        }
        self.lru_tail = Some(key);
        if self.lru_head.is_none() {
            self.lru_head = Some(key);
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
    pub(crate) async fn get(&self, http_cfg: &HttpConfig, key: CacheKey) -> Option<Vec<u8>> {
        let settings = CacheSettings::from(http_cfg);
        let now = now_epoch_secs();
        let mut meta_record = None;
        let mut expired = false;

        {
            let mut index = self.lock_index(http_cfg).await;
            if let Some(entry) = index.entries.get(&key) {
                if entry.expires_at == 0 || now > entry.expires_at {
                    index.remove(key);
                    expired = true;
                } else if settings.inactive_secs > 0
                    && now.saturating_sub(entry.last_access) > settings.inactive_secs
                {
                    index.remove(key);
                    expired = true;
                } else {
                    meta_record = index.touch(key, now);
                }
            } else {
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        let (data_path, meta_path) = self.cache_paths(key);
        if expired {
            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
            DISK_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let data = match fs::read(&data_path).await {
            Ok(data) => data,
            Err(_) => {
                let mut index = self.lock_index(http_cfg).await;
                let _ = index.remove(key);
                DISK_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };

        if let Some(record) = meta_record {
            let meta_contents = format_meta_record(&record);
            let _ = write_atomic(&meta_path, meta_contents.as_bytes()).await;
        }

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
    pub(crate) async fn put(
        &self,
        http_cfg: &HttpConfig,
        key: CacheKey,
        response: &[u8],
        ttl: Duration,
    ) {
        if ttl.as_secs() == 0 {
            return;
        }

        if fs::create_dir_all(&self.cache_dir).await.is_err() {
            return;
        }

        let settings = CacheSettings::from(http_cfg);
        let size = response.len() as u64;
        if settings.max_total_bytes > 0 && size > settings.max_total_bytes {
            return;
        }

        let now = now_epoch_secs();
        let record = DiskMetaRecord {
            expires_at: now.saturating_add(ttl.as_secs()),
            last_access: now,
            size,
        };

        let (data_path, meta_path) = self.cache_paths(key);
        let data_tmp = match write_temp_file(&data_path, response).await {
            Ok(path) => path,
            Err(_) => return,
        };
        let meta_contents = format_meta_record(&record);
        let meta_tmp = match write_temp_file(&meta_path, meta_contents.as_bytes()).await {
            Ok(path) => path,
            Err(_) => {
                let _ = fs::remove_file(&data_tmp).await;
                return;
            }
        };

        let mut evicted_keys: Vec<CacheKey> = Vec::new();
        {
            let mut index = self.lock_index(http_cfg).await;
            let _ = index.remove(key);
            evicted_keys.extend(index.evict_inactive(now, settings.inactive_secs));

            while settings.exceeds_limits(index.total_bytes + size, index.entries.len() + 1) {
                let Some((evicted_key, meta)) = index.pop_lru() else {
                    break;
                };
                DISK_EVICTIONS.fetch_add(1, Ordering::Relaxed);
                DISK_EVICTED_BYTES.fetch_add(meta.size, Ordering::Relaxed);
                evicted_keys.push(evicted_key);
            }

            if settings.exceeds_limits(index.total_bytes + size, index.entries.len() + 1) {
                let _ = fs::remove_file(&data_tmp).await;
                let _ = fs::remove_file(&meta_tmp).await;
                return;
            }

            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
            if fs::rename(&data_tmp, &data_path).await.is_err() {
                let _ = fs::remove_file(&data_tmp).await;
                let _ = fs::remove_file(&meta_tmp).await;
                return;
            }
            if fs::rename(&meta_tmp, &meta_path).await.is_err() {
                let _ = fs::remove_file(&data_path).await;
                let _ = fs::remove_file(&meta_tmp).await;
                return;
            }

            index.insert(
                key,
                DiskEntryMeta {
                    size: record.size,
                    expires_at: record.expires_at,
                    last_access: record.last_access,
                    prev: None,
                    next: None,
                },
            );
        }

        for evicted_key in evicted_keys {
            let (data_path, meta_path) = self.cache_paths(evicted_key);
            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
        }
    }

    /// Update access time / LRU for an existing cached entry.
    pub(crate) async fn touch(&self, http_cfg: &HttpConfig, key: CacheKey) {
        let settings = CacheSettings::from(http_cfg);
        let now = now_epoch_secs();
        let mut meta_record = None;
        let mut expired = false;

        {
            let mut index = self.lock_index(http_cfg).await;
            if let Some(entry) = index.entries.get(&key) {
                if entry.expires_at == 0 || now > entry.expires_at {
                    index.remove(key);
                    expired = true;
                } else if settings.inactive_secs > 0
                    && now.saturating_sub(entry.last_access) > settings.inactive_secs
                {
                    index.remove(key);
                    expired = true;
                } else {
                    meta_record = index.touch(key, now);
                }
            } else {
                return;
            }
        }

        let (data_path, meta_path) = self.cache_paths(key);
        if expired {
            let _ = fs::remove_file(&data_path).await;
            let _ = fs::remove_file(&meta_path).await;
            return;
        }

        if let Some(record) = meta_record {
            let meta_contents = format_meta_record(&record);
            let _ = write_atomic(&meta_path, meta_contents.as_bytes()).await;
        }
    }

    /// Resolve disk paths for a cache entry and its metadata file.
    fn cache_paths(&self, key: CacheKey) -> (PathBuf, PathBuf) {
        cache_paths_for(&self.cache_dir, key)
    }

    async fn lock_index(
        &self,
        http_cfg: &HttpConfig,
    ) -> tokio::sync::MutexGuard<'_, DiskCacheIndex> {
        let lock = DISK_CACHE_INDEX
            .get_or_init(|| AsyncMutex::new(DiskCacheIndex::new(self.cache_dir.clone())));
        let mut index = lock.lock().await;
        if index.cache_dir != self.cache_dir {
            index.reset(self.cache_dir.clone());
        }
        index.ensure_loaded(http_cfg).await;
        index
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
