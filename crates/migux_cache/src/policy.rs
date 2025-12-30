use http::Method;

pub struct CachePolicy;

impl CachePolicy {
    pub fn is_cacheable(method: &Method) -> bool {
        matches!(*method, Method::GET | Method::HEAD)
    }

    pub fn default_ttl() -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
}
