use std::collections::HashMap;

use crate::structs::ServerRuntime;
use migux_config::TlsConfig;

/// Normalized listen address key (e.g. "0.0.0.0:8080").
pub type ListenAddr = String;
/// Map of listen address to configured servers.
pub type ServersByListen = HashMap<ListenAddr, Vec<ServerRuntime>>;

#[derive(Debug, Clone)]
/// TLS listener configuration paired with its servers.
pub struct TlsListenConfig {
    /// Listen address for TLS.
    pub listen: ListenAddr,
    /// TLS settings for this listener.
    pub tls: TlsConfig,
    /// Servers bound to this TLS listener.
    pub servers: Vec<ServerRuntime>,
}

/// Map of TLS listen address to TLS config + servers.
pub type TlsServersByListen = HashMap<ListenAddr, TlsListenConfig>;
