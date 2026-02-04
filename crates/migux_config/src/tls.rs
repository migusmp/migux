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
}
