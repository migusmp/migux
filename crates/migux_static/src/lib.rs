//! Static file serving and cache utilities.
//!
//! Provides a minimal static file handler with optional in-memory and
//! disk-backed caching, respecting configured TTL and object size limits.

mod cache;
mod conditional;
mod etag;
mod fs;
mod response;
mod service;

pub use cache::{cache_metrics_snapshot, CacheMetrics};
pub use service::{serve_static, serve_static_bytes, serve_static_cached};
