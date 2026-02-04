//! Per-connection HTTP/1 handler.
//!
//! Reads client requests, selects the matching server/location, and dispatches
//! to static or proxy handlers while respecting keep-alive and timeouts.

use std::{net::SocketAddr, sync::Arc};

use bytes::{Buf, BytesMut};
use migux_http::responses::{send_404, send_405_with_allow, send_redirect, send_response};
use migux_static::cache_metrics_snapshot;
use migux_proxy::Proxy;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Duration;
use tracing::{debug, info, instrument, warn};

use migux_config::MiguxConfig;

use crate::ServerRuntime;

mod dispatch;
mod request;
mod routing;
mod timeouts;

use dispatch::dispatch_location;
use request::{extract_host_header, read_http_request, ParsedRequest};
use routing::{match_location, select_default_server};

pub trait ClientStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ClientStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Entry point for a "logical worker" that handles a single connection.
#[instrument(
    skip(stream, servers, proxy, cfg),
    fields(
        client = %client_addr,
    )
)]
pub async fn handle_connection(
    mut stream: Box<dyn ClientStream>,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
    is_tls: bool,
) -> anyhow::Result<()> {
    info!(target: "migux::worker", "Handling new client connection");

    let mut buf = BytesMut::new();
    let mut first_request = true;

    loop {
        let idle_timeout = if first_request {
            Duration::from_secs(cfg.http.client_read_timeout_secs)
        } else {
            Duration::from_secs(cfg.http.keepalive_timeout_secs)
        };

        // 1) Read one HTTP request (headers + optional body)
        let req = match read_http_request(&mut stream, &mut buf, &cfg.http, idle_timeout).await? {
            Some(req) => req,
            None => break,
        };

        if req.headers.is_empty() {
            debug!(target: "migux::worker", "Empty request received; closing connection");
            break;
        }

        // 2) Parse request line
        let method = req.method.as_str();
        let path = req.path.as_str();
        debug!(
            target: "migux::worker",
            %method,
            %path,
            "Parsed HTTP request line"
        );

        if maybe_handle_cache_metrics(&mut stream, &req, client_addr).await? {
            break;
        }

        // 3) Select server for this connection
        let server = select_default_server(&servers);
        debug!(
            target: "migux::worker",
            server = %server.name,
            root = %server.config.root,
            index = %server.config.index,
            "Selected server for request"
        );

        if !is_tls {
            if let Some(tls_cfg) = &server.config.tls {
                if tls_cfg.redirect_http {
                    let host = extract_host_header(&req.headers)
                        .unwrap_or_else(|| server.config.server_name.clone());
                    let location = build_https_redirect(&host, &req.path, &tls_cfg.listen);
                    send_redirect(&mut stream, &location).await?;
                    break;
                }
            }
        }

        if server.locations.is_empty() {
            warn!(
                target: "migux::worker",
                server = %server.name,
                "Server has no locations; returning 404"
            );
            send_404(&mut stream).await?;
            break;
        }

        // 4) Match location
        let location = match_location(&server.locations, path);
        debug!(
            target: "migux::worker",
            location_server = %location.server,
            location_path = %location.path,
            location_type = ?location.r#type,
            "Matched location"
        );

        let close_after = req.close_after;

        // Drop headers from buffer; keep body/leftovers for streaming or next request.
        if req.body_start > 0 {
            buf.advance(req.body_start);
        }

        // 5) Dispatch according to location type
        let force_close = dispatch_location(
            &mut stream,
            &mut buf,
            &cfg,
            server,
            location,
            &req,
            &proxy,
            &client_addr,
            is_tls,
        )
        .await?;

        if force_close || close_after {
            break;
        }

        first_request = false;
    }

    info!(
        target: "migux::worker",
        %client_addr,
        "Finished handling connection"
    );

    Ok(())
}

const CACHE_METRICS_PATH: &str = "/_migux/cache";

fn strip_query(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

async fn maybe_handle_cache_metrics(
    stream: &mut dyn ClientStream,
    req: &ParsedRequest,
    client_addr: SocketAddr,
) -> anyhow::Result<bool> {
    let mut path = strip_query(req.path.as_str());
    if path.len() > 1 {
        path = path.trim_end_matches('/');
    }

    if path != CACHE_METRICS_PATH {
        return Ok(false);
    }

    if !client_addr.ip().is_loopback() {
        send_404(stream).await?;
        return Ok(true);
    }

    if req.method != "GET" && req.method != "HEAD" {
        send_405_with_allow(stream, "GET, HEAD").await?;
        return Ok(true);
    }

    let body = if req.method == "HEAD" {
        String::new()
    } else {
        let metrics = cache_metrics_snapshot();
        format!(
            "{{\"memory_hits\":{},\"memory_misses\":{},\"disk_hits\":{},\"disk_misses\":{}}}",
            metrics.memory_hits, metrics.memory_misses, metrics.disk_hits, metrics.disk_misses
        )
    };

    send_response(
        stream,
        "200 OK",
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await?;

    Ok(true)
}

fn build_https_redirect(host: &str, path: &str, tls_listen: &str) -> String {
    let (host_part, _) = split_host_port(host);
    let mut port = None;
    if let Ok(addr) = tls_listen.parse::<std::net::SocketAddr>() {
        port = Some(addr.port());
    }
    match port {
        Some(443) | None => format!("https://{}{}", host_part, path),
        Some(p) => format!("https://{}:{}{}", host_part, p, path),
    }
}

fn split_host_port(host: &str) -> (String, Option<String>) {
    let host = host.trim();
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            let host_part = host[..=end].to_string();
            let rest = &host[end + 1..];
            if let Some(port) = rest.strip_prefix(':') {
                return (host_part, Some(port.to_string()));
            }
            return (host_part, None);
        }
    }

    if let Some(idx) = host.rfind(':') {
        let (left, right) = host.split_at(idx);
        let port = &right[1..];
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return (left.to_string(), Some(port.to_string()));
        }
    }

    (host.to_string(), None)
}
