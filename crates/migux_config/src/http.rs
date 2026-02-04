use serde::Deserialize;

// =======================================================
// HTTP CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub sendfile: bool,
    pub keepalive_timeout_secs: u64,
    pub access_log: String,

    // Timeouts (seconds)
    pub client_read_timeout_secs: u64,
    pub proxy_connect_timeout_secs: u64,
    pub proxy_read_timeout_secs: u64,
    pub proxy_write_timeout_secs: u64,

    // Upstream pool limits
    pub proxy_pool_max_per_addr: usize,
    pub proxy_pool_idle_timeout_secs: u64,

    // Limits (bytes)
    pub max_request_headers_bytes: u64,
    pub max_request_body_bytes: u64,
    pub max_upstream_response_headers_bytes: u64,
    pub max_upstream_response_body_bytes: u64,

    // Caché control
    /// Directory used for disk-backed static cache (optional).
    pub cache_dir: Option<String>,
    /// Default TTL in seconds for cached static objects (optional).
    pub cache_default_ttl_secs: Option<u32>,
    /// Maximum size in bytes for a cached static object (optional).
    pub cache_max_object_bytes: Option<u64>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            sendfile: true,
            keepalive_timeout_secs: 65,
            access_log: "/var/log/migux/access.log".into(),
            client_read_timeout_secs: 15,
            proxy_connect_timeout_secs: 5,
            proxy_read_timeout_secs: 30,
            proxy_write_timeout_secs: 30,
            proxy_pool_max_per_addr: 32,
            proxy_pool_idle_timeout_secs: 60,
            max_request_headers_bytes: 64 * 1024,
            max_request_body_bytes: 10 * 1024 * 1024,
            max_upstream_response_headers_bytes: 64 * 1024,
            max_upstream_response_body_bytes: 10 * 1024 * 1024,
            cache_dir: None,
            cache_default_ttl_secs: None,
            cache_max_object_bytes: None,
        }
    }
}

impl HttpConfig {
    pub fn sendfile(&self) -> bool {
        self.sendfile
    }

    pub fn keepalive_timeout_secs(&self) -> u64 {
        self.keepalive_timeout_secs
    }

    pub fn access_log(&self) -> &str {
        &self.access_log
    }

    pub fn client_read_timeout_secs(&self) -> u64 {
        self.client_read_timeout_secs
    }

    pub fn proxy_connect_timeout_secs(&self) -> u64 {
        self.proxy_connect_timeout_secs
    }

    pub fn proxy_read_timeout_secs(&self) -> u64 {
        self.proxy_read_timeout_secs
    }

    pub fn proxy_write_timeout_secs(&self) -> u64 {
        self.proxy_write_timeout_secs
    }

    pub fn proxy_pool_max_per_addr(&self) -> usize {
        self.proxy_pool_max_per_addr
    }

    pub fn proxy_pool_idle_timeout_secs(&self) -> u64 {
        self.proxy_pool_idle_timeout_secs
    }

    pub fn max_request_headers_bytes(&self) -> u64 {
        self.max_request_headers_bytes
    }

    pub fn max_request_body_bytes(&self) -> u64 {
        self.max_request_body_bytes
    }

    pub fn max_upstream_response_headers_bytes(&self) -> u64 {
        self.max_upstream_response_headers_bytes
    }

    pub fn max_upstream_response_body_bytes(&self) -> u64 {
        self.max_upstream_response_body_bytes
    }

    pub fn cache_dir(&self) -> Option<&str> {
        self.cache_dir.as_deref()
    }

    pub fn cache_default_ttl_secs(&self) -> Option<u32> {
        self.cache_default_ttl_secs
    }

    pub fn cache_max_object_bytes(&self) -> Option<u64> {
        self.cache_max_object_bytes
    }

    pub(crate) fn apply_cache_defaults(&mut self) {
        if self.cache_dir.is_some() {
            if self.cache_default_ttl_secs.is_none() {
                self.cache_default_ttl_secs = Some(30);
            }
            if self.cache_max_object_bytes.is_none() {
                self.cache_max_object_bytes = Some(1_048_576);
            }
        }
    }

    pub(crate) fn apply_defaults_from(&mut self, defaults: &HttpConfig) {
        if self.access_log.is_empty() {
            self.access_log = defaults.access_log.clone();
        }
        if self.keepalive_timeout_secs == 0 {
            self.keepalive_timeout_secs = defaults.keepalive_timeout_secs;
        }
        if self.client_read_timeout_secs == 0 {
            self.client_read_timeout_secs = defaults.client_read_timeout_secs;
        }
        if self.proxy_connect_timeout_secs == 0 {
            self.proxy_connect_timeout_secs = defaults.proxy_connect_timeout_secs;
        }
        if self.proxy_read_timeout_secs == 0 {
            self.proxy_read_timeout_secs = defaults.proxy_read_timeout_secs;
        }
        if self.proxy_write_timeout_secs == 0 {
            self.proxy_write_timeout_secs = defaults.proxy_write_timeout_secs;
        }
        if self.proxy_pool_max_per_addr == 0 {
            self.proxy_pool_max_per_addr = defaults.proxy_pool_max_per_addr;
        }
        if self.proxy_pool_idle_timeout_secs == 0 {
            self.proxy_pool_idle_timeout_secs = defaults.proxy_pool_idle_timeout_secs;
        }
        if self.max_request_headers_bytes == 0 {
            self.max_request_headers_bytes = defaults.max_request_headers_bytes;
        }
        if self.max_request_body_bytes == 0 {
            self.max_request_body_bytes = defaults.max_request_body_bytes;
        }
        if self.max_upstream_response_headers_bytes == 0 {
            self.max_upstream_response_headers_bytes = defaults.max_upstream_response_headers_bytes;
        }
        if self.max_upstream_response_body_bytes == 0 {
            self.max_upstream_response_body_bytes = defaults.max_upstream_response_body_bytes;
        }
    }
}
