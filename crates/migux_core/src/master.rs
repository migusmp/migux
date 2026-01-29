use std::{sync::Arc, time::Duration};

use dashmap::{self, DashMap};
use migux_config::MiguxConfig;
use migux_proxy::Proxy;
use tokio::{net::TcpListener, sync::Semaphore};
use tracing::{debug, error, info, instrument, warn};

use crate::{
    build_servers_by_listen,
    build_tls_servers_by_listen,
    http2::serve_h2_connection,
    worker::handle_connection,
    ServerRuntime,
    ServersByListen,
};
use migux_config::TlsConfig;
use std::{fs::File, io::BufReader};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

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
        let proxy = Arc::new(Proxy::new());
        proxy.start_health_checks(cfg.clone());

        info!(
            target: "migux::master",
            max_conns,
            "Global connection semaphore initialized"
        );

        // One accept-loop per listening socket (HTTP)
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
            let proxy_clone = proxy.clone();

            tokio::spawn(async move {
                if let Err(e) = accept_loop(
                    listener,
                    addr.clone(),
                    sem_clone,
                    Arc::new(servers_for_listener),
                    proxy_clone,
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

        // One accept-loop per TLS listening socket
        for (listen_addr, tls_cfg) in self.tls_servers_by_listen.iter() {
            if tls_cfg.tls.cert_path.is_empty() || tls_cfg.tls.key_path.is_empty() {
                warn!(
                    target: "migux::master",
                    listen = %listen_addr,
                    "TLS config missing cert/key path; skipping TLS listener"
                );
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
                "Creating Tokio TLS listener"
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
                        "Failed to bind TLS listener"
                    );
                    return Err(e.into());
                }
            };

            let addr = listen_addr.clone();
            let sem_clone = semaphore.clone();
            let servers_for_listener = tls_cfg.servers.clone();
            let cfg_clone = cfg.clone();
            let proxy_clone = proxy.clone();

            tokio::spawn(async move {
                if let Err(e) = accept_loop_tls(
                    listener,
                    addr.clone(),
                    tls_acceptor,
                    sem_clone,
                    Arc::new(servers_for_listener),
                    proxy_clone,
                    cfg_clone,
                )
                .await
                {
                    error!(
                        target: "migux::master",
                        listen = %addr,
                        error = ?e,
                        "accept_loop_tls exited with an error"
                    );
                } else {
                    warn!(
                        target: "migux::master",
                        listen = %addr,
                        "accept_loop_tls exited cleanly (possible shutdown)"
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
    skip(listener, semaphore, servers, proxy, cfg),
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
    proxy: Arc<Proxy>,
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
        let proxy_clone = proxy.clone();
        let cfg_clone = cfg.clone();
        let listen_for_span = listen_this.clone();

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

            if let Err(e) = handle_connection(
                Box::new(stream),
                addr,
                servers_clone,
                proxy_clone,
                cfg_clone,
                false,
            )
            .await
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

#[instrument(
    skip(listener, acceptor, semaphore, servers, proxy, cfg),
    fields(
        listen = %listen_addr,
        max_permits = semaphore.available_permits(),
    )
)]
/// Accept loop for TLS listeners; dispatches HTTP/2 via ALPN when enabled.
async fn accept_loop_tls(
    listener: TcpListener,
    listen_addr: String,
    acceptor: TlsAcceptor,
    semaphore: Arc<Semaphore>,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    info!(
        target: "migux::master",
        listen = %listen_addr,
        "accept_loop_tls started for TLS listening socket"
    );

    loop {
        let listen_this = listen_addr.clone();

        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(
                    target: "migux::master",
                    listen = %listen_this,
                    error = ?e,
                    "Failed to accept TLS connection"
                );
                return Err(e.into());
            }
        };

        let sem_for_permit = semaphore.clone();
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

        let in_flight = semaphore.available_permits();
        debug!(
            target: "migux::master",
            listen = %listen_this,
            client_addr = %addr,
            in_flight = in_flight,
            "New TLS connection accepted"
        );

        let servers_clone = servers.clone();
        let proxy_clone = proxy.clone();
        let cfg_clone = cfg.clone();
        let acceptor_clone = acceptor.clone();
        let listen_for_span = listen_this.clone();

        tokio::spawn(async move {
            let _permit = permit;
            let span = tracing::info_span!(
                "worker_tls_connection",
                client_addr = %addr,
                listen = %listen_for_span,
            );
            let _enter = span.enter();

            debug!(
                target: "migux::worker",
                "Worker spawned for incoming TLS connection"
            );

            let tls_stream = match acceptor_clone.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        target: "migux::worker",
                        client_addr = %addr,
                        error = ?e,
                        "TLS handshake failed"
                    );
                    return;
                }
            };

            let alpn = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .map(|v| v.to_vec());

            let servers_h2 = servers_clone.clone();
            let proxy_h2 = proxy_clone.clone();
            let cfg_h2 = cfg_clone.clone();

            if matches!(alpn.as_deref(), Some(b"h2")) {
                if let Err(e) = serve_h2_connection(
                    tls_stream,
                    addr,
                    servers_h2,
                    proxy_h2,
                    cfg_h2,
                )
                .await
                {
                    error!(
                        target: "migux::worker",
                        client_addr = %addr,
                        error = ?e,
                        "Error while handling HTTP/2 TLS connection"
                    );
                } else {
                    debug!(
                        target: "migux::worker",
                        client_addr = %addr,
                        "HTTP/2 TLS connection handled successfully"
                    );
                }
            } else if let Err(e) = handle_connection(
                Box::new(tls_stream),
                addr,
                servers_clone,
                proxy_clone,
                cfg_clone,
                true,
            )
            .await
            {
                error!(
                    target: "migux::worker",
                    client_addr = %addr,
                    error = ?e,
                    "Error while handling TLS connection"
                );
            } else {
                debug!(
                    target: "migux::worker",
                    client_addr = %addr,
                    "TLS connection handled successfully"
                );
            }
        });
    }
}

/// Build a TLS acceptor from configured certificate/key paths.
fn load_tls_acceptor(cfg: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(&cfg.cert_path)?;
    let key = load_private_key(&cfg.key_path)?;

    let mut config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("Invalid TLS config: {e}"))?;
    if cfg.http2 {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    } else {
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Load PEM-encoded certificates from disk.
fn load_certs(path: &str) -> anyhow::Result<Vec<rustls::Certificate>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)?;
    if certs.is_empty() {
        anyhow::bail!("No certificates found in {}", path);
    }
    Ok(certs.into_iter().map(rustls::Certificate).collect())
}

/// Load a PEM-encoded private key (PKCS8 or RSA) from disk.
fn load_private_key(path: &str) -> anyhow::Result<rustls::PrivateKey> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let keys = rustls_pemfile::pkcs8_private_keys(&mut reader)?;
    if let Some(key) = keys.into_iter().next() {
        return Ok(rustls::PrivateKey(key));
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let keys = rustls_pemfile::rsa_private_keys(&mut reader)?;
    if let Some(key) = keys.into_iter().next() {
        return Ok(rustls::PrivateKey(key));
    }

    anyhow::bail!("No private keys found in {}", path);
}
