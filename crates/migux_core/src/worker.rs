use std::{net::SocketAddr, sync::Arc};

use migux_http::responses::{send_404, send_408, send_413, send_431};
use migux_proxy::Proxy;
use migux_static::serve_static;
use tokio::{
    io::AsyncReadExt,
    net::TcpStream,
    time::{timeout, Duration},
};
use tracing::{debug, info, instrument, warn};

use migux_config::{HttpConfig, LocationType, MiguxConfig};

use crate::ServerRuntime;

mod routing;

use routing::{match_location, parse_request_line, select_default_server};

/// Entry point for a “logical worker” that handles a single connection.
#[instrument(
    skip(stream, servers, proxy, cfg),
    fields(
        client = %client_addr,
    )
)]
pub async fn handle_connection(
    mut stream: TcpStream,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    info!(target: "migux::worker", "Handling new client connection");

    // 1) Read the entire HTTP request (headers + optional body)
    let (req_headers, req_body) = read_http_request(&mut stream, &cfg.http).await?;

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

            // ✅ Cache según location.cache (y solo GET cachea)
            // serve_static_cached(&mut stream, &server.config, location, &method, path, &cache)
            //     .await?;
            serve_static(&mut stream, &server.config, location, path).await?;
        }

        LocationType::Proxy => {
            debug!(
                target: "migux::proxy",
                %path,
                "Forwarding request to upstream proxy"
            );

            proxy
                .serve(
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
#[instrument(skip(stream, http), fields())]
async fn read_http_request(
    stream: &mut TcpStream,
    http: &HttpConfig,
) -> anyhow::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let headers_end;
    let read_timeout = Duration::from_secs(http.client_read_timeout_secs);
    let max_headers = http.max_request_headers_bytes as usize;
    let max_body = http.max_request_body_bytes as usize;

    // 1) Read until header terminator
    loop {
        let n = match timeout(read_timeout, stream.read(&mut tmp)).await {
            Ok(res) => res?,
            Err(_) => {
                send_408(stream).await?;
                return Ok((String::new(), Vec::new()));
            }
        };
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

        if max_headers > 0 && buf.len() > max_headers {
            send_431(stream).await?;
            return Ok((String::new(), Vec::new()));
        }
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
        if max_body > 0 && content_length > max_body {
            send_413(stream).await?;
            return Ok((String::new(), Vec::new()));
        }
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
        if max_body > 0 && already_read_body > max_body {
            send_413(stream).await?;
            return Ok((String::new(), Vec::new()));
        }
        body.extend_from_slice(&buf[headers_end + 4..]);
    }

    // 4) Read remaining body if needed
    while body.len() < content_length {
        let n = match timeout(read_timeout, stream.read(&mut tmp)).await {
            Ok(res) => res?,
            Err(_) => {
                send_408(stream).await?;
                return Ok((String::new(), Vec::new()));
            }
        };
        if n == 0 {
            warn!(
                target: "migux::http",
                expected = content_length,
                got = body.len(),
                "Client closed connection before body was fully received"
            );
            break;
        }
        let remaining = content_length - body.len();
        let take = remaining.min(n);
        body.extend_from_slice(&tmp[..take]);
    }

    Ok((headers_str, body))
}
