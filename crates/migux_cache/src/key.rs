#[derive(Hash, Eq, PartialEq, Debug)]
pub struct CacheKey {
    pub method: String,
    pub uri: String,
}

impl CacheKey {
    pub fn new(method: &str, uri: &str) -> Self {
        Self {
            method: method.to_string(),
            uri: uri.to_string(),
        }
    }
}
