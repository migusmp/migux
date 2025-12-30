use std::collections::HashMap;
use std::sync::RwLock;

use crate::entry::CacheEntry;
use crate::key::CacheKey;

#[derive(Debug)]
pub struct MemoryCacheStore {
    inner: RwLock<HashMap<CacheKey, CacheEntry>>,
}

impl MemoryCacheStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        self.inner.read().ok()?.get(key).cloned()
    }

    pub fn insert(&self, key: CacheKey, entry: CacheEntry) {
        let _ = self.inner.write().map(|mut m| m.insert(key, entry));
    }
}
