use std::{net::SocketAddr, sync::Arc};

use bytes::BytesMut;
use migux_config::{LocationConfig, LocationType, MiguxConfig};
use migux_http::responses::send_405_with_allow;
use migux_proxy::Proxy;
use migux_static::serve_static_cached;
use tokio::time::Duration;
use tracing::{debug, warn};

use super::ClientStream;
use super::request::ParsedRequest;
use super::timeouts::{discard_chunked_body, discard_content_length};
use crate::ServerRuntime;

pub(crate) async fn dispatch_location(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    cfg: &Arc<MiguxConfig>,
    server: &ServerRuntime,
    location: &LocationConfig,
    req: &ParsedRequest,
    proxy: &Proxy,
    client_addr: &SocketAddr,
    is_tls: bool,
) -> anyhow::Result<bool> {
    let method = req.method.as_str();
    let path = req.path.as_str();
    let hsts_header = build_hsts_header(server, is_tls);

    match location.r#type {
        LocationType::Static => {
            if method != "GET" && method != "HEAD" {
                warn!(
                    target: "migux::worker",
                    %method,
                    "Unsupported method for static file; returning 405"
                );
                send_405_with_allow(stream, "GET, HEAD").await?;
                return Ok(true);
            }

            debug!(
                target: "migux::static",
                %path,
                "Serving static file"
            );

            let keep_alive = !req.close_after;
            serve_static_cached(
                stream,
                &cfg.http,
                &server.config,
                location,
                method,
                &req.headers,
                path,
                keep_alive,
                hsts_header.as_deref(),
            )
            .await?;

            // Discard request body (if any) so keep-alive doesn't break.
            if req.is_chunked {
                let _ = discard_chunked_body(
                    stream,
                    buf,
                    Duration::from_secs(cfg.http.client_read_timeout_secs),
                    cfg.http.max_request_body_bytes as usize,
                )
                .await;
            } else if req.content_length > 0 {
                let _ = discard_content_length(
                    stream,
                    buf,
                    req.content_length,
                    Duration::from_secs(cfg.http.client_read_timeout_secs),
                )
                .await;
            }
        }
        LocationType::Proxy => {
            debug!(
                target: "migux::proxy",
                %path,
                "Forwarding request to upstream proxy"
            );

            proxy
                .serve(
                    stream,
                    buf,
                    location,
                    &req.headers,
                    method,
                    path,
                    &req.http_version,
                    req.content_length,
                    req.is_chunked,
                    is_tls,
                    hsts_header.as_deref(),
                    cfg,
                    client_addr,
                )
                .await?;
        }
    }

    Ok(false)
}

fn build_hsts_header(server: &ServerRuntime, is_tls: bool) -> Option<String> {
    if !is_tls {
        return None;
    }

    let tls = server.config.tls.as_ref()?;
    let max_age = tls.hsts_max_age_secs()?;
    if max_age == 0 {
        return None;
    }

    let mut value = format!("max-age={}", max_age);
    if tls.hsts_include_subdomains() {
        value.push_str("; includeSubDomains");
    }
    Some(value)
}
