use dashmap::DashMap;
use migux_config::{LocationConfig, ServerConfig};

#[derive(Debug, Clone)]
pub struct CacheStore {
    pub responses: DashMap<String, Vec<u8>>,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self {
            responses: DashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerRuntime {
    pub name: String,
    pub config: ServerConfig,
    pub locations: Vec<LocationConfig>,
}

impl ServerRuntime {
    pub fn new(name: String, config: ServerConfig, locations: Vec<LocationConfig>) -> Self {
        Self {
            name,
            config,
            locations,
        }
    }
}
