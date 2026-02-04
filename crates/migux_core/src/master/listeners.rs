use std::sync::Arc;

use migux_proxy::Proxy;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use super::accept::{accept_loop, accept_loop_tls, bind_listener};
use super::tls::{load_tls_acceptor, tls_listener_ready};
use super::Master;

impl Master {
    pub(super) async fn spawn_http_listeners(
        &self,
        semaphore: Arc<Semaphore>,
        proxy: Arc<Proxy>,
    ) -> anyhow::Result<()> {
        for (listen_addr, servers) in self.servers_by_listen.iter() {
            info!(
                target: "migux::master",
                listen = %listen_addr,
                num_servers = servers.len(),
                "Preparing HTTP listener"
            );

            let listener = bind_listener(listen_addr, "http").await?;
            let addr = listen_addr.clone();
            let servers = Arc::new(servers.clone());
            let cfg = self.cfg.clone();
            let proxy = proxy.clone();
            let semaphore = semaphore.clone();

            tokio::spawn(async move {
                let listen_for_log = addr.clone();
                if let Err(e) =
                    accept_loop(listener, addr, semaphore, servers, proxy, cfg).await
                {
                    error!(
                        target: "migux::master",
                        listen = %listen_for_log,
                        error = ?e,
                        "accept_loop exited with an error"
                    );
                } else {
                    warn!(
                        target: "migux::master",
                        listen = %listen_for_log,
                        "accept_loop exited cleanly (possible shutdown)"
                    );
                }
            });
        }

        Ok(())
    }

    pub(super) async fn spawn_tls_listeners(
        &self,
        semaphore: Arc<Semaphore>,
        proxy: Arc<Proxy>,
    ) -> anyhow::Result<()> {
        for (listen_addr, tls_cfg) in self.tls_servers_by_listen.iter() {
            if !tls_listener_ready(listen_addr, tls_cfg) {
                continue;
            }

            let tls_acceptor = match load_tls_acceptor(&tls_cfg.tls) {
                Ok(a) => a,
                Err(e) => {
                    error!(
                        target: "migux::master",
                        listen = %listen_addr,
                        error = ?e,
                        "Failed to load TLS config; skipping TLS listener"
                    );
                    continue;
                }
            };

            info!(
                target: "migux::master",
                listen = %listen_addr,
                num_servers = tls_cfg.servers.len(),
                "Preparing TLS listener"
            );

            let listener = bind_listener(listen_addr, "tls").await?;
            let addr = listen_addr.clone();
            let servers = Arc::new(tls_cfg.servers.clone());
            let cfg = self.cfg.clone();
            let proxy = proxy.clone();
            let semaphore = semaphore.clone();

            tokio::spawn(async move {
                let listen_for_log = addr.clone();
                if let Err(e) = accept_loop_tls(
                    listener,
                    addr,
                    tls_acceptor,
                    semaphore,
                    servers,
                    proxy,
                    cfg,
                )
                .await
                {
                    error!(
                        target: "migux::master",
                        listen = %listen_for_log,
                        error = ?e,
                        "accept_loop_tls exited with an error"
                    );
                } else {
                    warn!(
                        target: "migux::master",
                        listen = %listen_for_log,
                        "accept_loop_tls exited cleanly (possible shutdown)"
                    );
                }
            });
        }

        Ok(())
    }
}
