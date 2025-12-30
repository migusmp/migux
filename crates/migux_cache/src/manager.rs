use http::{HeaderMap, Request, Response, StatusCode};

use crate::{entry::CacheEntry, key::CacheKey, policy::CachePolicy, store::MemoryCacheStore};

#[derive(Debug)]
pub struct CacheManager {
    store: MemoryCacheStore,
}

impl CacheManager {
    pub fn new() -> Self {
        Self {
            store: MemoryCacheStore::new(),
        }
    }

    pub fn try_get<B>(&self, req: &Request<B>) -> Option<CacheEntry> {
        if !CachePolicy::is_cacheable(req.method()) {
            return None;
        }

        let key = CacheKey::new(req.method().as_str(), req.uri().path());
        let entry = self.store.get(&key)?;

        if entry.is_expired() {
            None
        } else {
            Some(entry)
        }
    }

    pub fn store<B>(&self, req: &Request<B>, res: &Response<Vec<u8>>) {
        let key = CacheKey::new(req.method().as_str(), req.uri().path());

        let entry = CacheEntry {
            status: res.status(),
            headers: res.headers().clone(),
            body: res.body().clone(),
            created_at: std::time::Instant::now(),
            ttl: CachePolicy::default_ttl(),
        };

        self.store.insert(key, entry);
    }
    pub fn try_get_raw(&self, method: &str, path: &str) -> Option<CacheEntry> {
        // reutilizas la policy existente
        if !CachePolicy::is_cacheable(&method.parse().ok()?) {
            return None;
        }

        let key = CacheKey::new(method, path);
        let entry = self.store.get(&key)?;

        if entry.is_expired() {
            None
        } else {
            Some(entry)
        }
    }

    pub fn store_raw(
        &self,
        method: &str,
        path: &str,
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    ) {
        let key = CacheKey::new(method, path);

        let entry = CacheEntry {
            status,
            headers,
            body,
            created_at: std::time::Instant::now(),
            ttl: CachePolicy::default_ttl(),
        };

        self.store.insert(key, entry);
    }
}
