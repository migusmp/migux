use serde::Deserialize;
use std::collections::HashMap;

// =======================================================
// GLOBAL CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub worker_processes: u8,
    pub worker_connections: u16,
    pub log_level: String,
    pub error_log: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            worker_processes: 1,
            worker_connections: 1024,
            log_level: "info".into(),
            error_log: "/var/log/migux/error.log".into(),
        }
    }
}

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

// =======================================================
// UPSTREAM CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UpstreamServers {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub server: UpstreamServers,
    pub strategy: Option<String>,
    pub health: UpstreamHealthConfig,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            server: UpstreamServers::One(String::new()),
            strategy: Some("round_robin".to_string()),
            health: UpstreamHealthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
/// Health/circuit-breaker configuration for an upstream pool.
pub struct UpstreamHealthConfig {
    /// Failures before marking an upstream down.
    pub fail_threshold: u32,
    /// Cooldown time in seconds before retrying a down node.
    pub cooldown_secs: u64,
    /// Enable active TCP health checks.
    pub active: bool,
    /// Interval for active checks in seconds.
    pub interval_secs: u64,
    /// Timeout for active checks in seconds.
    pub timeout_secs: u64,
}

impl Default for UpstreamHealthConfig {
    fn default() -> Self {
        Self {
            fail_threshold: 1,
            cooldown_secs: 10,
            active: false,
            interval_secs: 10,
            timeout_secs: 1,
        }
    }
}

impl std::fmt::Display for UpstreamServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamServers::One(s) => write!(f, "{s}"),
            UpstreamServers::Many(list) => write!(f, "{:?}", list),
        }
    }
}

// =======================================================
// SERVER CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
    pub server_name: String,
    pub root: String,
    pub index: String,
    pub tls: Option<TlsConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            server_name: "localhost".into(),
            root: "./public".into(),
            index: "index.html".into(),
            tls: None,
        }
    }
}

// =======================================================
// TLS CONFIG
// =======================================================
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
/// TLS listener configuration for a server.
pub struct TlsConfig {
    /// TLS listen address (host:port).
    pub listen: String,
    /// Path to PEM-encoded certificate chain.
    pub cert_path: String,
    /// Path to PEM-encoded private key.
    pub key_path: String,
    /// Redirect HTTP -> HTTPS for this server.
    pub redirect_http: bool,
    /// Enable HTTP/2 via ALPN on this TLS listener.
    pub http2: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8443".into(),
            cert_path: String::new(),
            key_path: String::new(),
            redirect_http: false,
            http2: false,
        }
    }
}

// =======================================================
// LOCATION TYPE (enum tipado)
// =======================================================
#[derive(Debug, Deserialize, Clone, Default)]
pub enum LocationType {
    #[default]
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "proxy")]
    Proxy,
}

// =======================================================
// LOCATION CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LocationConfig {
    pub server: String,
    pub path: String,
    pub r#type: LocationType, // static | proxy
    pub root: Option<String>, // solo para static
    pub index: Option<String>,
    pub upstream: Option<String>,
    pub strip_prefix: Option<String>,
    pub cache: Option<bool>,
}

impl Default for LocationConfig {
    fn default() -> Self {
        Self {
            server: "main".into(),
            path: "/".into(),
            r#type: LocationType::Static,
            root: None,
            index: None,
            upstream: None,
            strip_prefix: None,
            cache: None,
        }
    }
}

// =======================================================
// MIGUX CONFIG — main config
// =======================================================
#[derive(Debug, Deserialize)]
pub struct MiguxConfig {
    #[serde(default)]
    pub global: GlobalConfig,

    #[serde(default)]
    pub http: HttpConfig,

    #[serde(default)]
    pub upstream: HashMap<String, UpstreamConfig>,

    #[serde(default)]
    #[serde(rename = "server")]
    pub servers: HashMap<String, ServerConfig>,

    #[serde(default)]
    pub location: HashMap<String, LocationConfig>,
}

impl Default for MiguxConfig {
    fn default() -> Self {
        let mut cfg = Self {
            global: GlobalConfig::default(),
            http: HttpConfig::default(),
            upstream: HashMap::new(),
            servers: HashMap::new(),
            location: HashMap::new(),
        };
        cfg.apply_defaults();
        cfg
    }
}

impl MiguxConfig {
    pub fn from_file(file_name: &str) -> Result<Self, config::ConfigError> {
        let built = config::Config::builder()
            .add_source(config::File::new(file_name, config::FileFormat::Ini).required(false))
            .build()?;

        let mut cfg: MiguxConfig = built.try_deserialize()?;

        cfg.apply_defaults();
        Ok(cfg)
    }

    pub fn from_file_or_default(file_name: &str) -> Self {
        match Self::from_file(file_name) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("⚠️  Error reading config'{file_name}': {e}");
                eprintln!("➡️  Using default config(in-memory)...");
                MiguxConfig::default()
            }
        }
    }

    fn apply_defaults(&mut self) {
        // GLOBAL
        let def_global = GlobalConfig::default();

        if self.http.cache_dir.is_some() {
            if self.http.cache_default_ttl_secs.is_none() {
                self.http.cache_default_ttl_secs = Some(30);
            }
            if self.http.cache_max_object_bytes.is_none() {
                self.http.cache_max_object_bytes = Some(1048576);
            }
        }

        if self.global.worker_processes == 0 {
            self.global.worker_processes = def_global.worker_processes;
        }
        if self.global.worker_connections == 0 {
            self.global.worker_connections = def_global.worker_connections;
        }
        if self.global.log_level.is_empty() {
            self.global.log_level = def_global.log_level.clone();
        }
        if self.global.error_log.is_empty() {
            self.global.error_log = def_global.error_log.clone();
        }

        // HTTP
        let def_http = HttpConfig::default();

        if self.http.access_log.is_empty() {
            self.http.access_log = def_http.access_log.clone();
        }
        if self.http.keepalive_timeout_secs == 0 {
            self.http.keepalive_timeout_secs = def_http.keepalive_timeout_secs;
        }
        if self.http.client_read_timeout_secs == 0 {
            self.http.client_read_timeout_secs = def_http.client_read_timeout_secs;
        }
        if self.http.proxy_connect_timeout_secs == 0 {
            self.http.proxy_connect_timeout_secs = def_http.proxy_connect_timeout_secs;
        }
        if self.http.proxy_read_timeout_secs == 0 {
            self.http.proxy_read_timeout_secs = def_http.proxy_read_timeout_secs;
        }
        if self.http.proxy_write_timeout_secs == 0 {
            self.http.proxy_write_timeout_secs = def_http.proxy_write_timeout_secs;
        }
        if self.http.proxy_pool_max_per_addr == 0 {
            self.http.proxy_pool_max_per_addr = def_http.proxy_pool_max_per_addr;
        }
        if self.http.proxy_pool_idle_timeout_secs == 0 {
            self.http.proxy_pool_idle_timeout_secs = def_http.proxy_pool_idle_timeout_secs;
        }
        if self.http.max_request_headers_bytes == 0 {
            self.http.max_request_headers_bytes = def_http.max_request_headers_bytes;
        }
        if self.http.max_request_body_bytes == 0 {
            self.http.max_request_body_bytes = def_http.max_request_body_bytes;
        }
        if self.http.max_upstream_response_headers_bytes == 0 {
            self.http.max_upstream_response_headers_bytes =
                def_http.max_upstream_response_headers_bytes;
        }
        if self.http.max_upstream_response_body_bytes == 0 {
            self.http.max_upstream_response_body_bytes = def_http.max_upstream_response_body_bytes;
        }

        // SERVERS
        let def_server = ServerConfig::default();
        let def_tls = TlsConfig::default();

        for s in self.servers.values_mut() {
            if s.listen.is_empty() {
                s.listen = def_server.listen.clone();
            }
            if s.server_name.is_empty() {
                s.server_name = def_server.server_name.clone();
            }
            if s.root.is_empty() {
                s.root = def_server.root.clone();
            }
            if s.index.is_empty() {
                s.index = def_server.index.clone();
            }
            if let Some(tls) = s.tls.as_mut() {
                if tls.listen.is_empty() {
                    tls.listen = def_tls.listen.clone();
                }
            }
        }

        // LOCATIONS
        for l in self.location.values_mut() {
            if l.root.is_none()
                && let Some(srv) = self.servers.get(&l.server)
            {
                l.root = Some(srv.root.clone());
            }

            if l.index.is_none()
                && let Some(srv) = self.servers.get(&l.server)
            {
                l.index = Some(srv.index.clone());
            }
        }
    }

    pub fn print(&self) {
        println!("================ MIGUX CONFIG ================");

        println!("\n[global]");
        println!("  worker_processes     = {}", self.global.worker_processes);
        println!(
            "  worker_connections   = {}",
            self.global.worker_connections
        );
        println!("  log_level            = {}", self.global.log_level);
        println!("  error_log            = {}", self.global.error_log);

        println!("\n[http]");
        println!("  sendfile             = {}", self.http.sendfile);
        println!(
            "  keepalive_timeout    = {}",
            self.http.keepalive_timeout_secs
        );
        println!("  access_log           = {}", self.http.access_log);
        println!(
            "  client_read_timeout_secs = {}",
            self.http.client_read_timeout_secs
        );
        println!(
            "  proxy_connect_timeout_secs = {}",
            self.http.proxy_connect_timeout_secs
        );
        println!(
            "  proxy_read_timeout_secs = {}",
            self.http.proxy_read_timeout_secs
        );
        println!(
            "  proxy_write_timeout_secs = {}",
            self.http.proxy_write_timeout_secs
        );
        println!(
            "  proxy_pool_max_per_addr = {}",
            self.http.proxy_pool_max_per_addr
        );
        println!(
            "  proxy_pool_idle_timeout_secs = {}",
            self.http.proxy_pool_idle_timeout_secs
        );
        println!(
            "  max_request_headers_bytes = {}",
            self.http.max_request_headers_bytes
        );
        println!(
            "  max_request_body_bytes = {}",
            self.http.max_request_body_bytes
        );
        println!(
            "  max_upstream_response_headers_bytes = {}",
            self.http.max_upstream_response_headers_bytes
        );
        println!(
            "  max_upstream_response_body_bytes = {}",
            self.http.max_upstream_response_body_bytes
        );
        println!("  cache_dir       = {:?}", self.http.cache_dir);
        println!(
            "  cache_default_ttl_secs       = {:?}",
            self.http.cache_default_ttl_secs
        );
        println!(
            "  cache_max_object_bytes       = {:?}",
            self.http.cache_max_object_bytes
        );

        println!("\n[upstream]");
        for (name, up) in &self.upstream {
            println!("  upstream {}:", name);
            println!("    server   = {}", up.server);
        }

        println!("\n[server]");
        for (name, srv) in &self.servers {
            println!("  server {}:", name);
            println!("    listen      = {}", srv.listen);
            println!("    server_name = {}", srv.server_name);
            println!("    root        = {}", srv.root);
            println!("    index       = {}", srv.index);
            if let Some(tls) = &srv.tls {
                println!("    tls.listen        = {}", tls.listen);
                println!("    tls.cert_path     = {}", tls.cert_path);
                println!("    tls.key_path      = {}", tls.key_path);
                println!("    tls.redirect_http = {}", tls.redirect_http);
                println!("    tls.http2         = {}", tls.http2);
            }
        }

        println!("\n[location]");
        for (name, loc) in &self.location {
            println!("  location {}:", name);
            println!("    server       = {}", loc.server);
            println!("    path         = {}", loc.path);
            println!(
                "    type         = {:?}",
                loc.r#type // enum, lo mostramos con Debug
            );
            println!("    root         = {:?}", loc.root);
            println!("    index        = {:?}", loc.index);
            println!("    upstream     = {:?}", loc.upstream);
            println!("    strip_prefix = {:?}", loc.strip_prefix);
        }

        println!("==============================================");
    }
}
