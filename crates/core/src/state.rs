use dashmap::{self, DashMap};
use migux_config::MiguxConfig;

pub struct CacheStore {
    pub responses: DashMap<String, Vec<u8>>,
}

impl CacheStore {
    pub fn new() -> Self {
        Self {
            responses: DashMap::new(),
        }
    }
}

pub struct MiguxApp {
    pub config: MiguxConfig,
    pub cache: CacheStore,
}

impl MiguxApp {
    pub fn init() -> Self {
        Self {
            config: MiguxConfig::default(),
            cache: CacheStore::new(),
        }
    }
}
