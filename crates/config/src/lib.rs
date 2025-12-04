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
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            sendfile: true,
            keepalive_timeout_secs: 65,
            access_log: "/var/log/migux/access.log".into(),
        }
    }
}

// =======================================================
// UPSTREAM CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub server: String,
    pub strategy: String,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            server: "".into(),
            strategy: "round_robin".into(),
        }
    }
}

// =======================================================
// SERVER CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
    pub server_name: String,
    pub root: String,
    pub index: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            server_name: "localhost".into(),
            root: "./public".into(),
            index: "index.html".into(),
        }
    }
}

// =======================================================
// LOCATION CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LocationConfig {
    pub server: String,
    pub path: String,
    pub r#type: String,       // static | proxy
    pub root: Option<String>, // solo para static
    pub index: Option<String>,
    pub upstream: Option<String>,
    pub strip_prefix: Option<String>,
}

impl Default for LocationConfig {
    fn default() -> Self {
        Self {
            server: "main".into(),
            path: "/".into(),
            r#type: "static".into(),
            root: None,
            index: None,
            upstream: None,
            strip_prefix: None,
        }
    }
}

// =======================================================
// MIGUX CONFIG — Config principal
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
    pub server: HashMap<String, ServerConfig>,

    #[serde(default)]
    pub location: HashMap<String, LocationConfig>,
}

impl MiguxConfig {
    pub fn default() -> Self {
        Self::from_file("migux.conf")
    }

    pub fn from_file(file_name: &str) -> Self {
        let mut cfg: MiguxConfig = config::Config::builder()
            .add_source(
                config::File::new(file_name, config::FileFormat::Ini).required(false), // si no existe, no peta
            )
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();

        cfg.apply_defaults();
        cfg
    }

    fn apply_defaults(&mut self) {
        // GLOBAL
        if self.global.worker_processes == 0 {
            self.global.worker_processes = GlobalConfig::default().worker_processes;
        }
        if self.global.worker_connections == 0 {
            self.global.worker_connections = GlobalConfig::default().worker_connections;
        }
        if self.global.log_level.is_empty() {
            self.global.log_level = GlobalConfig::default().log_level;
        }
        if self.global.error_log.is_empty() {
            self.global.error_log = GlobalConfig::default().error_log;
        }

        // HTTP
        if self.http.access_log.is_empty() {
            self.http.access_log = HttpConfig::default().access_log;
        }

        // SERVERS
        for (_, s) in &mut self.server {
            if s.listen.is_empty() {
                s.listen = ServerConfig::default().listen;
            }
            if s.server_name.is_empty() {
                s.server_name = ServerConfig::default().server_name;
            }
            if s.root.is_empty() {
                s.root = ServerConfig::default().root;
            }
            if s.index.is_empty() {
                s.index = ServerConfig::default().index;
            }
        }

        // LOCATIONS
        for (_, l) in &mut self.location {
            if l.r#type.is_empty() {
                l.r#type = "static".into();
            }

            // Default root = root del server al que pertenece
            if l.root.is_none() {
                if let Some(srv) = self.server.get(&l.server) {
                    l.root = Some(srv.root.clone());
                }
            }

            // Default index = index del server
            if l.index.is_none() {
                if let Some(srv) = self.server.get(&l.server) {
                    l.index = Some(srv.index.clone());
                }
            }
        }
    }

    // opcional: para ver qué se ha cargado
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

        println!("\n[upstream]");
        for (name, up) in &self.upstream {
            println!("  upstream {}:", name);
            println!("    server   = {}", up.server);
            println!("    strategy = {}", up.strategy);
        }

        println!("\n[server]");
        for (name, srv) in &self.server {
            println!("  server {}:", name);
            println!("    listen      = {}", srv.listen);
            println!("    server_name = {}", srv.server_name);
            println!("    root        = {}", srv.root);
            println!("    index       = {}", srv.index);
        }

        println!("\n[location]");
        for (name, loc) in &self.location {
            println!("  location {}:", name);
            println!("    server       = {}", loc.server);
            println!("    path         = {}", loc.path);
            println!("    type         = {}", loc.r#type);
            println!("    root         = {:?}", loc.root);
            println!("    index        = {:?}", loc.index);
            println!("    upstream     = {:?}", loc.upstream);
            println!("    strip_prefix = {:?}", loc.strip_prefix);
        }

        println!("==============================================");
    }
}

// =======================================================
// TEST
// =======================================================
pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
