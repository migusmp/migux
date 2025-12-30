use std::time::{Duration, Instant};

use http::{HeaderMap, StatusCode};

#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub created_at: Instant,
    pub ttl: Duration,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}
