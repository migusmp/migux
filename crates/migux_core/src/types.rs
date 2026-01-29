use std::collections::HashMap;

use crate::structs::ServerRuntime;
use migux_config::TlsConfig;

pub type ListenAddr = String;
pub type ServersByListen = HashMap<ListenAddr, Vec<ServerRuntime>>;

#[derive(Debug, Clone)]
pub struct TlsListenConfig {
    pub listen: ListenAddr,
    pub tls: TlsConfig,
    pub servers: Vec<ServerRuntime>,
}

pub type TlsServersByListen = HashMap<ListenAddr, TlsListenConfig>;
