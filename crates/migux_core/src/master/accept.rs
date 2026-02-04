use std::{net::SocketAddr, sync::Arc};

use migux_config::MiguxConfig;
use migux_proxy::Proxy;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, instrument};

use crate::{http2::serve_h2_connection, worker::handle_connection, ServerRuntime};

pub(crate) async fn bind_listener(
    listen_addr: &str,
    kind: &'static str,
) -> anyhow::Result<TcpListener> {
    info!(
        target: "migux::master",
        listen = %listen_addr,
        listener = kind,
        "Binding listener"
    );

    match TcpListener::bind(listen_addr).await {
        Ok(listener) => {
            info!(
                target: "migux::master",
                listen = %listen_addr,
                listener = kind,
                "Bind() successful"
            );
            Ok(listener)
        }
        Err(e) => {
            error!(
                target: "migux::master",
                listen = %listen_addr,
                listener = kind,
                error = ?e,
                "Failed to bind listener"
            );
            Err(e.into())
        }
    }
}

struct AcceptedConn {
    stream: TcpStream,
    addr: SocketAddr,
    permit: OwnedSemaphorePermit,
}

async fn accept_with_permit(
    listener: &TcpListener,
    listen_addr: &str,
    semaphore: &Arc<Semaphore>,
    kind: &'static str,
) -> anyhow::Result<AcceptedConn> {
    let (stream, addr) = match listener.accept().await {
        Ok(pair) => pair,
        Err(e) => {
            error!(
                target: "migux::master",
                listen = %listen_addr,
                listener = kind,
                error = ?e,
                "Failed to accept connection"
            );
            return Err(e.into());
        }
    };

    let permit = match semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(e) => {
            error!(
                target: "migux::master",
                listen = %listen_addr,
                listener = kind,
                error = ?e,
                "Failed to acquire connection permit"
            );
            return Err(e.into());
        }
    };

    let available = semaphore.available_permits();
    debug!(
        target: "migux::master",
        listen = %listen_addr,
        listener = kind,
        client_addr = %addr,
        available_permits = available,
        "Connection accepted"
    );

    Ok(AcceptedConn {
        stream,
        addr,
        permit,
    })
}

#[instrument(
    skip(listener, semaphore, servers, proxy, cfg),
    fields(
        listen = %listen_addr,
        available_permits = semaphore.available_permits(),
    )
)]
pub(crate) async fn accept_loop(
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
        let AcceptedConn { stream, addr, permit } =
            accept_with_permit(&listener, &listen_addr, &semaphore, "http").await?;

        let servers_clone = servers.clone();
        let proxy_clone = proxy.clone();
        let cfg_clone = cfg.clone();
        let listen_for_span = listen_addr.clone();

        tokio::spawn(async move {
            let _permit = permit;
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
        available_permits = semaphore.available_permits(),
    )
)]
/// Accept loop for TLS listeners; dispatches HTTP/2 via ALPN when enabled.
pub(crate) async fn accept_loop_tls(
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
        let AcceptedConn { stream, addr, permit } =
            accept_with_permit(&listener, &listen_addr, &semaphore, "tls").await?;

        let servers_clone = servers.clone();
        let proxy_clone = proxy.clone();
        let cfg_clone = cfg.clone();
        let acceptor_clone = acceptor.clone();
        let listen_for_span = listen_addr.clone();

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

            let alpn = tls_stream.get_ref().1.alpn_protocol().map(|v| v.to_vec());

            if matches!(alpn.as_deref(), Some(b"h2")) {
                if let Err(e) =
                    serve_h2_connection(tls_stream, addr, servers_clone, proxy_clone, cfg_clone)
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
