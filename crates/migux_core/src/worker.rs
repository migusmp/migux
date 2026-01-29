use std::{net::SocketAddr, sync::Arc};

use bytes::BytesMut;
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

use routing::{match_location, select_default_server};

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

        let mut close_after = req.close_after;

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
                    break;
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
                close_after = true;
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
                        &req.headers,
                        &req.body,
                        &cfg,
                        &client_addr,
                    )
                    .await?;
            }
        }

        if close_after {
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

/// Reads a full HTTP request:
/// - Reads until `\r\n\r\n` (end of headers)
/// - Extracts Content-Length if present
/// - Reads the full body if required
/// - Returns (headers as String, body as Vec<u8>)
#[derive(Debug)]
struct ParsedRequest {
    headers: String,
    body: Vec<u8>,
    method: String,
    path: String,
    close_after: bool,
}

#[instrument(skip(stream, buf, http), fields())]
async fn read_http_request(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    http: &HttpConfig,
    idle_timeout: Duration,
) -> anyhow::Result<Option<ParsedRequest>> {
    let read_timeout = Duration::from_secs(http.client_read_timeout_secs);
    let max_headers = http.max_request_headers_bytes as usize;
    let max_body = http.max_request_body_bytes as usize;

    let headers_end = loop {
        if let Some(pos) = find_headers_end(buf) {
            break pos;
        }

        if max_headers > 0 && buf.len() > max_headers {
            send_431(stream).await?;
            return Ok(None);
        }

        let timeout_dur = if buf.is_empty() { idle_timeout } else { read_timeout };
        match read_more(stream, buf, timeout_dur).await? {
            ReadOutcome::Timeout => {
                if buf.is_empty() {
                    return Ok(None);
                }
                send_408(stream).await?;
                return Ok(None);
            }
            ReadOutcome::Read(0) => return Ok(None),
            ReadOutcome::Read(_) => {}
        }
    };

    let header_bytes = &buf[..headers_end];
    let headers_str = String::from_utf8_lossy(header_bytes).to_string();

    debug!(
        target: "migux::http",
        header_len = headers_str.len(),
        "Parsed HTTP headers"
    );

    let (method, path, _http_version, content_length, close_after) =
        parse_request_metadata(&headers_str);

    if content_length > 0 {
        if max_body > 0 && content_length > max_body {
            send_413(stream).await?;
            return Ok(None);
        }
        debug!(
            target: "migux::http",
            content_length,
            "Detected Content-Length header"
        );
    }

    let total_len = headers_end + 4 + content_length;
    while buf.len() < total_len {
        match read_more(stream, buf, read_timeout).await? {
            ReadOutcome::Timeout => {
                send_408(stream).await?;
                return Ok(None);
            }
            ReadOutcome::Read(0) => return Ok(None),
            ReadOutcome::Read(_) => {}
        }
    }

    let req_bytes = buf.split_to(total_len);
    let body = req_bytes[headers_end + 4..].to_vec();

    Ok(Some(ParsedRequest {
        headers: headers_str,
        body,
        method,
        path,
        close_after,
    }))
}

fn find_headers_end(buf: &BytesMut) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

enum ReadOutcome {
    Read(usize),
    Timeout,
}

async fn read_more(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    timeout_dur: Duration,
) -> anyhow::Result<ReadOutcome> {
    let mut tmp = [0u8; 4096];
    match timeout(timeout_dur, stream.read(&mut tmp)).await {
        Ok(res) => {
            let n = res?;
            if n > 0 {
                buf.extend_from_slice(&tmp[..n]);
            }
            Ok(ReadOutcome::Read(n))
        }
        Err(_) => Ok(ReadOutcome::Timeout),
    }
}

fn parse_request_metadata(headers: &str) -> (String, String, String, usize, bool) {
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("-").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let http_version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut content_length = 0usize;
    let mut connection_close = false;
    let mut connection_keep_alive = false;

    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = len;
            }
        }
        if let Some(rest) = lower.strip_prefix("connection:") {
            let v = rest.trim();
            if v.contains("close") {
                connection_close = true;
            }
            if v.contains("keep-alive") {
                connection_keep_alive = true;
            }
        }
    }

    let close_after = if http_version == "HTTP/1.0" {
        !connection_keep_alive
    } else {
        connection_close
    };

    (method, path, http_version, content_length, close_after)
}
