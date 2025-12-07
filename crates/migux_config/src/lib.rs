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
#[derive(Debug, Deserialize, Clone)]
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
// LOCATION TYPE (enum tipado)
// =======================================================
#[derive(Debug, Deserialize, Clone)]
pub enum LocationType {
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "proxy")]
    Proxy,
}

impl Default for LocationType {
    fn default() -> Self {
        LocationType::Static
    }
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
    #[serde(rename = "server")]
    pub servers: HashMap<String, ServerConfig>,

    #[serde(default)]
    pub location: HashMap<String, LocationConfig>,
}

// Default “en memoria”: sin leer fichero
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
            .add_source(
                config::File::new(file_name, config::FileFormat::Ini).required(false), // si no existe, no peta
            )
            .build()?; // Propaga error de build

        let mut cfg: MiguxConfig = built.try_deserialize()?;

        cfg.apply_defaults();
        Ok(cfg)
    }

    pub fn from_file_or_default(file_name: &str) -> Self {
        match Self::from_file(file_name) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("⚠️  Error leyendo configuración '{file_name}': {e}");
                eprintln!("➡️  Usando configuración por defecto (in-memory)...");
                MiguxConfig::default()
            }
        }
    }

    fn apply_defaults(&mut self) {
        // GLOBAL
        let def_global = GlobalConfig::default();

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
        // (Opcional) si quieres evitar keepalive = 0:
        // if self.http.keepalive_timeout_secs == 0 {
        //     self.http.keepalive_timeout_secs = def_http.keepalive_timeout_secs;
        // }

        // SERVERS
        let def_server = ServerConfig::default();

        for (_, s) in &mut self.servers {
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
        }

        // LOCATIONS
        for (_, l) in &mut self.location {
            // Default root = root del server al que pertenece
            if l.root.is_none() {
                if let Some(srv) = self.servers.get(&l.server) {
                    l.root = Some(srv.root.clone());
                }
            }

            // Default index = index del server
            if l.index.is_none() {
                if let Some(srv) = self.servers.get(&l.server) {
                    l.index = Some(srv.index.clone());
                }
            }

            // Si el tipo es Proxy, idealmente upstream debería ser Some(...)
            // Aquí podrías loggear un warning si falta.
            // if matches!(l.r#type, LocationType::Proxy) && l.upstream.is_none() {
            //     eprintln!("⚠️  Location '{}' es proxy pero no tiene upstream", l.path);
            // }
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
        for (name, srv) in &self.servers {
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

    #[test]
    fn default_config_builds() {
        let cfg = MiguxConfig::default();
        assert!(cfg.global.worker_processes >= 1);
    }
}
