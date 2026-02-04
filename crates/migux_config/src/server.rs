use serde::Deserialize;

use crate::TlsConfig;

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

impl ServerConfig {
    pub fn listen(&self) -> &str {
        &self.listen
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn index(&self) -> &str {
        &self.index
    }

    pub fn tls(&self) -> Option<&TlsConfig> {
        self.tls.as_ref()
    }

    pub(crate) fn apply_defaults_from(&mut self, defaults: &ServerConfig) {
        if self.listen.is_empty() {
            self.listen = defaults.listen.clone();
        }
        if self.server_name.is_empty() {
            self.server_name = defaults.server_name.clone();
        }
        if self.root.is_empty() {
            self.root = defaults.root.clone();
        }
        if self.index.is_empty() {
            self.index = defaults.index.clone();
        }
    }
}
