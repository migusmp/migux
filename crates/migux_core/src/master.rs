use std::{sync::Arc, time::Duration};

use dashmap::{self, DashMap};
use migux_cache::manager::CacheManager;
use migux_config::MiguxConfig;
use tokio::{net::TcpListener, sync::Semaphore};
use tracing::{debug, error, info, instrument, warn};

use crate::{build_servers_by_listen, worker::handle_connection, ServerRuntime, ServersByListen};

pub struct CacheStore {
    pub responses: DashMap<String, Vec<u8>>,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self {
            responses: DashMap::new(),
        }
    }
}

#[allow(dead_code)]
pub struct Master {
    cfg: Arc<MiguxConfig>,
    servers_by_listen: Arc<ServersByListen>,
}

impl Master {
    pub fn new(cfg: MiguxConfig) -> Self {
        let cfg = Arc::new(cfg);
        let servers_by_listen = Arc::new(build_servers_by_listen(&cfg));

        Self {
            cfg,
            servers_by_listen,
        }
    }

    /// Starts the master process: initializes listeners and spawns accept loops.
    #[instrument(skip(self), fields(
        worker_processes = %self.cfg.global.worker_processes,
        worker_connections = %self.cfg.global.worker_connections,
        log_level = %self.cfg.global.log_level,
    ))]
    pub async fn run(self) -> anyhow::Result<()> {
        info!(target: "migux::master", "Starting MIGUX MASTER");

        info!(
            target: "migux::master",
            worker_processes = self.cfg.global.worker_processes,
            worker_connections = self.cfg.global.worker_connections,
            log_level = %self.cfg.global.log_level,
            "Global configuration loaded"
        );

        // Global limit for concurrent connections across the entire process
        let max_conns = self.cfg.global.worker_connections as usize;

        // We initialize the semaphore with the maximum number of configured connections
        let semaphore = Arc::new(Semaphore::new(max_conns));

        let cfg = self.cfg.clone();

        info!(
            target: "migux::master",
            max_conns,
            "Global connection semaphore initialized"
        );

        // One accept-loop per listening socket
        for (listen_addr, servers) in self.servers_by_listen.iter() {
            info!(
                target: "migux::master",
                listen = %listen_addr,
                num_servers = servers.len(),
                "Creating Tokio listener"
            );

            let listener = match TcpListener::bind(listen_addr).await {
                Ok(l) => {
                    info!(
                        target: "migux::master",
                        listen = %listen_addr,
                        "Bind() successful"
                    );
                    l
                }
                Err(e) => {
                    error!(
                        target: "migux::master",
                        listen = %listen_addr,
                        error = ?e,
                        "Failed to bind listener"
                    );
                    return Err(e.into());
                }
            };

            let addr = listen_addr.clone();
            let sem_clone = semaphore.clone();
            let servers_for_listener = servers.clone();
            let cfg_clone = cfg.clone();

            tokio::spawn(async move {
                if let Err(e) = accept_loop(
                    listener,
                    addr.clone(),
                    sem_clone,
                    Arc::new(servers_for_listener),
                    cfg_clone,
                )
                .await
                {
                    error!(
                        target: "migux::master",
                        listen = %addr,
                        error = ?e,
                        "accept_loop exited with an error"
                    );
                } else {
                    warn!(
                        target: "migux::master",
                        listen = %addr,
                        "accept_loop exited cleanly (possible shutdown)"
                    );
                }
            });
        }

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

#[instrument(
    skip(listener, semaphore, servers, cfg),
    fields(
        listen = %listen_addr,
        max_permits = semaphore.available_permits(),
    )
)]
async fn accept_loop(
    listener: TcpListener,
    listen_addr: String,
    semaphore: Arc<Semaphore>,
    servers: Arc<Vec<ServerRuntime>>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    info!(
        target: "migux::master",
        listen = %listen_addr,
        "accept_loop started for listening socket"
    );

    loop {
        // Clone once per iteration to avoid move issues
        let listen_this = listen_addr.clone();

        // Wait for a new incoming connection
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(
                    target: "migux::master",
                    listen = %listen_this,
                    error = ?e,
                    "Failed to accept connection"
                );
                return Err(e.into());
            }
        };

        // Acquire a permit (global connection limit)
        let sem_for_permit = semaphore.clone();

        // Permits must be acquired via Semaphore::acquire_owned to be movable across the task boundary
        let permit = match sem_for_permit.acquire_owned().await {
            Ok(p) => p,
            Err(e) => {
                error!(
                    target: "migux::master",
                    listen = %listen_this,
                    error = ?e,
                    "Failed to acquire connection permit"
                );
                return Err(e.into());
            }
        };

        // Returns the current number of available permits
        let in_flight = semaphore.available_permits();

        debug!(
            target: "migux::master",
            listen = %listen_this,
            client_addr = %addr,
            in_flight = in_flight,
            "New connection accepted"
        );

        let servers_clone = servers.clone();
        let cfg_clone = cfg.clone();
        let listen_for_span = listen_this.clone();
        let cache_manager = Arc::new(CacheManager::new());

        tokio::spawn(async move {
            let span = tracing::info_span!(
                "worker_connection",
                client_addr = %addr,
                listen = %listen_for_span,
            );
            let _enter = span.enter();

            debug!(
                target: "migux::worker",
                "Worker spawned for incoming connection"
            );

            if let Err(e) =
                handle_connection(stream, addr, servers_clone, cfg_clone, cache_manager).await
            {
                error!(
                    target: "migux::worker",
                    client_addr = %addr,
                    error = ?e,
                    "Error while handling connection"
                );
            } else {
                debug!(
                    target: "migux::worker",
                    client_addr = %addr,
                    "Connection handled successfully"
                );
            }

            drop(permit);

            debug!(
                target: "migux::master",
                client_addr = %addr,
                "Permit released after connection closed"
            );
        });
    }
}
