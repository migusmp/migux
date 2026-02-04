mod accept;
mod listeners;
mod startup;
mod tls;

use std::{sync::Arc, time::Duration};

use migux_config::MiguxConfig;
use tracing::{info, instrument};

use crate::{build_servers_by_listen, build_tls_servers_by_listen, ServersByListen};

pub use crate::structs::CacheStore;

#[allow(dead_code)]
pub struct Master {
    cfg: Arc<MiguxConfig>,
    servers_by_listen: Arc<ServersByListen>,
    tls_servers_by_listen: Arc<crate::types::TlsServersByListen>,
}

impl Master {
    pub fn new(cfg: MiguxConfig) -> Self {
        let cfg = Arc::new(cfg);
        let servers_by_listen = Arc::new(build_servers_by_listen(&cfg));
        let tls_servers_by_listen = Arc::new(build_tls_servers_by_listen(&cfg));

        Self {
            cfg,
            servers_by_listen,
            tls_servers_by_listen,
        }
    }

    /// Starts the master process: initializes listeners and spawns accept loops.
    #[instrument(skip(self), fields(
        worker_processes = %self.cfg.global.worker_processes,
        worker_connections = %self.cfg.global.worker_connections,
        log_level = %self.cfg.global.log_level,
    ))]
    pub async fn run(self) -> anyhow::Result<()> {
        self.log_startup();

        let semaphore = self.init_semaphore();
        let proxy = self.start_proxy();

        self.spawn_http_listeners(semaphore.clone(), proxy.clone())
            .await?;
        self.spawn_tls_listeners(semaphore, proxy).await?;

        info!(
            target: "migux::master",
            "Master initialized. Waiting for incoming connections (Ctrl+C to stop)..."
        );

        // Keep the master process alive
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
}
