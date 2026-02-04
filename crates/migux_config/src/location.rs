use serde::Deserialize;

use crate::ServerConfig;

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
    pub root: Option<String>, // only static content
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

impl LocationConfig {
    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn location_type(&self) -> &LocationType {
        &self.r#type
    }

    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    pub fn root_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.root.as_deref().unwrap_or(default)
    }

    pub fn index(&self) -> Option<&str> {
        self.index.as_deref()
    }

    pub fn index_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.index.as_deref().unwrap_or(default)
    }

    pub fn upstream(&self) -> Option<&str> {
        self.upstream.as_deref()
    }

    pub fn strip_prefix(&self) -> Option<&str> {
        self.strip_prefix.as_deref()
    }

    pub fn cache(&self) -> Option<bool> {
        self.cache
    }

    pub(crate) fn apply_defaults_from_server(&mut self, server: &ServerConfig) {
        if self.root.is_none() {
            self.root = Some(server.root.clone());
        }
        if self.index.is_none() {
            self.index = Some(server.index.clone());
        }
    }
}
