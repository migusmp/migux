use std::{net::SocketAddr, sync::Arc};

use migux_cache::manager::CacheManager;
use migux_http::responses::send_404;
use migux_proxy::serve_proxy;
use migux_static::serve_static;
use tokio::{io::AsyncReadExt, net::TcpStream};
use tracing::{debug, info, instrument, warn};

use migux_config::{LocationType, MiguxConfig};

use crate::ServerRuntime;

mod routing;

use routing::{match_location, parse_request_line, select_default_server};

/// Entry point for a “logical worker” that handles a single connection.
#[instrument(
    skip(stream, servers, cfg),
    fields(
        client = %client_addr,
    )
)]
pub async fn handle_connection(
    mut stream: TcpStream,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    cfg: Arc<MiguxConfig>,
    cache: Arc<CacheManager>,
) -> anyhow::Result<()> {
    info!(target: "migux::worker", "Handling new client connection");

    // 1) Read the entire HTTP request (headers + optional body)
    let (req_headers, req_body) = read_http_request(&mut stream).await?;

    if req_headers.is_empty() {
        debug!(target: "migux::worker", "Empty request received; closing connection");
        return Ok(());
    }

    // 2) Parse request line
    let (method, path) = parse_request_line(&req_headers);
    debug!(
        target: "migux::worker",
        %method,
        %path,
        "Parsed HTTP request line"
    );

    // 3) Select server for this connection
    let server = select_default_server(&servers);
    debug!(
        target: "migux::worker",
        server = %server.name,
        root = %server.config.root,
        index = %server.config.index,
        "Selected server for request"
    );

    if server.locations.is_empty() {
        warn!(
            target: "migux::worker",
            server = %server.name,
            "Server has no locations; returning 404"
        );
        send_404(&mut stream).await?;
        return Ok(());
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

    // 5) Dispatch according to location type
    match location.r#type {
        LocationType::Static => {
            if method != "GET" && method != "HEAD" {
                warn!(
                    target: "migux::worker",
                    %method,
                    "Unsupported method for static file; returning 404"
                );
                send_404(&mut stream).await?;
                return Ok(());
            }

            debug!(
                target: "migux::static",
                %path,
                "Serving static file"
            );

            serve_static(&mut stream, &server.config, location, path).await?;
        }

        LocationType::Proxy => {
            debug!(
                target: "migux::proxy",
                %path,
                "Forwarding request to upstream proxy"
            );

            serve_proxy(
                &mut stream,
                location,
                path,
                &req_headers,
                &req_body,
                &cfg,
                &client_addr,
            )
            .await?;
        }
    }

    info!(
        target: "migux::worker",
        %client_addr,
        "Finished handling connection"
    );

    Ok(())
}

/// Reads a full HTTP request:
/// - Reads until `\r\n\r\n` (end of headers)
/// - Extracts Content-Length if present
/// - Reads the full body if required
/// - Returns (headers as String, body as Vec<u8>)
#[instrument(skip(stream), fields())]
async fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let headers_end;

    // 1) Read until header terminator
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                debug!(
                    target: "migux::http",
                    "Connection closed before request was received"
                );
                return Ok((String::new(), Vec::new()));
            } else {
                warn!(
                    target: "migux::http",
                    "Connection closed but headers were incomplete"
                );
                headers_end = buf.len();
                break;
            }
        }

        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = pos;
            break;
        }

        // Optional: enforce max header size for safety
    }

    let header_bytes = &buf[..headers_end];
    let headers_str = String::from_utf8_lossy(header_bytes).to_string();

    debug!(
        target: "migux::http",
        header_len = headers_str.len(),
        "Parsed HTTP headers"
    );

    // 2) Parse Content-Length
    let mut content_length = 0usize;
    for line in headers_str.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = len;
            }
        }
    }

    if content_length > 0 {
        debug!(
            target: "migux::http",
            content_length,
            "Detected Content-Length header"
        );
    }

    // 3) Extract any portion of body already read
    let already_read_body = buf.len().saturating_sub(headers_end + 4);
    let mut body = Vec::new();

    if already_read_body > 0 && headers_end + 4 <= buf.len() {
        body.extend_from_slice(&buf[headers_end + 4..]);
    }

    // 4) Read remaining body if needed
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            warn!(
                target: "migux::http",
                expected = content_length,
                got = body.len(),
                "Client closed connection before body was fully received"
            );
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Ok((headers_str, body))
}
