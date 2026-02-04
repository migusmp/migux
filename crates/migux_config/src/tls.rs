use serde::Deserialize;

// =======================================================
// TLS CONFIG
// =======================================================
#[derive(Debug, Deserialize, Clone)]
// #[serde(default)]
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
    /// Optional HSTS max-age in seconds (None disables HSTS).
    pub hsts_max_age_secs: Option<u64>,
    /// Enable includeSubDomains in the HSTS header when set to true.
    pub hsts_include_subdomains: Option<bool>,
}

impl TlsConfig {
    pub fn listen(&self) -> &str {
        &self.listen
    }

    pub fn cert_path(&self) -> &str {
        &self.cert_path
    }

    pub fn key_path(&self) -> &str {
        &self.key_path
    }

    pub fn redirect_http(&self) -> bool {
        self.redirect_http
    }

    pub fn http2(&self) -> bool {
        self.http2
    }

    pub fn hsts_max_age_secs(&self) -> Option<u64> {
        self.hsts_max_age_secs
    }

    pub fn hsts_include_subdomains(&self) -> bool {
        self.hsts_include_subdomains.unwrap_or(false)
    }
}
