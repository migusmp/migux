use std::sync::Arc;

use dashmap::{self, DashMap};
use migux_config::MiguxConfig;
use tokio::net::TcpListener;

use crate::{build_servers_by_listen, ServersByListen};

pub struct CacheStore {
    pub responses: DashMap<String, Vec<u8>>,
}

impl CacheStore {
    pub fn new() -> Self {
        Self {
            responses: DashMap::new(),
        }
    }
}

pub struct Master {
    cfg: Arc<MiguxConfig>,
    servers_by_listen: Arc<ServersByListen>,
    cache: CacheStore,
}

impl Master {
    pub fn new(cfg: MiguxConfig) -> Self {
        let cfg = Arc::new(cfg);
        let servers_by_listen = Arc::new(build_servers_by_listen(&cfg));

        Self {
            cfg,
            servers_by_listen,
            cache: CacheStore::new(),
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        println!("========== MIGUX MASTER ==========");
        println!("worker_processes   = {}", self.cfg.global.worker_processes);
        println!(
            "worker_connections = {}",
            self.cfg.global.worker_connections
        );
        println!("log_level          = {}", self.cfg.global.log_level);

        println!("\n[listen sockets (Tokio)]");
        for (listen_addr, servers) in self.servers_by_listen.iter() {
            // Crear listener de Tokio
            let listener = TcpListener::bind(listen_addr).await?;

            println!(
                "  - tokio listener en {listen_addr} ({} servers)",
                servers.len()
            );

            // De momento lo soltamos aquí.
            // Más adelante este `listener` se lo pasaremos a los workers.
            drop(listener);
        }

        println!("==================================");

        Ok(())
    }
}
