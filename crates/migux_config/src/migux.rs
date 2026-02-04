use serde::Deserialize;
use std::collections::HashMap;

use crate::{GlobalConfig, HttpConfig, LocationConfig, ServerConfig, UpstreamConfig};
use crate::validation::{validate, ConfigReport};

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
    pub fn has_tls_servers(&self) -> bool {
        self.servers.values().any(|s| s.tls.is_some())
    }

    pub fn global(&self) -> &GlobalConfig {
        &self.global
    }

    pub fn http(&self) -> &HttpConfig {
        &self.http
    }

    pub fn upstreams(&self) -> &HashMap<String, UpstreamConfig> {
        &self.upstream
    }

    pub fn upstream(&self, name: &str) -> Option<&UpstreamConfig> {
        self.upstream.get(name)
    }

    pub fn servers(&self) -> &HashMap<String, ServerConfig> {
        &self.servers
    }

    pub fn server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.get(name)
    }

    pub fn locations(&self) -> &HashMap<String, LocationConfig> {
        &self.location
    }

    pub fn location(&self, name: &str) -> Option<&LocationConfig> {
        self.location.get(name)
    }

    /// Validate the configuration and return a report of warnings and errors.
    pub fn validate(&self) -> ConfigReport {
        validate(self)
    }

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
            Ok(cfg) => {
                let report = cfg.validate();
                if report.has_errors() {
                    eprintln!("⚠️  Invalid config in '{file_name}':");
                    eprintln!("{}", report.format());
                    eprintln!("➡️  Using default config(in-memory)...");
                    MiguxConfig::default()
                } else {
                    if !report.warnings().is_empty() {
                        eprintln!("⚠️  Config warnings in '{file_name}':");
                        eprintln!("{}", report.format());
                    }
                    cfg
                }
            }
            Err(e) => {
                eprintln!("⚠️  Error reading config'{file_name}': {e}");
                eprintln!("➡️  Using default config(in-memory)...");
                MiguxConfig::default()
            }
        }
    }

    fn apply_defaults(&mut self) {
        self.http.apply_cache_defaults();

        let def_global = GlobalConfig::default();
        self.global.apply_defaults_from(&def_global);

        let def_http = HttpConfig::default();
        self.http.apply_defaults_from(&def_http);

        let def_server = ServerConfig::default();
        for server in self.servers.values_mut() {
            server.apply_defaults_from(&def_server);
        }

        for location in self.location.values_mut() {
            if let Some(server) = self.servers.get(&location.server) {
                location.apply_defaults_from_server(server);
            }
        }
    }

    pub fn print(&self) {
        println!("================ MIGUX CONFIG ================");
        self.print_global();
        self.print_http();
        self.print_upstreams();
        self.print_servers();
        self.print_locations();
        println!("==============================================");
    }

    fn print_global(&self) {
        println!("\n[global]");
        println!("  worker_processes     = {}", self.global.worker_processes);
        println!(
            "  worker_connections   = {}",
            self.global.worker_connections
        );
        println!("  log_level            = {}", self.global.log_level);
        println!("  error_log            = {}", self.global.error_log);
    }

    fn print_http(&self) {
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
    }

    fn print_upstreams(&self) {
        println!("\n[upstream]");
        for (name, up) in &self.upstream {
            println!("  upstream {}:", name);
            println!("    server   = {}", up.server);
        }
    }

    fn print_servers(&self) {
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
                if let Some(max_age) = tls.hsts_max_age_secs {
                    println!("    tls.hsts_max_age_secs = {}", max_age);
                    println!(
                        "    tls.hsts_include_subdomains = {}",
                        tls.hsts_include_subdomains.unwrap_or(false)
                    );
                }
            }
        }
    }

    fn print_locations(&self) {
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
    }
}
