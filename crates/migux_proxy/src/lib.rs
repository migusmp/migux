use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

use dashmap::DashMap;
use migux_http::responses::{send_404, send_502};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, error, info, instrument, warn};

use migux_config::{LocationConfig, MiguxConfig, UpstreamConfig, UpstreamServers};

/// Global map: upstream name -> counter for round-robin
static UPSTREAM_COUNTERS: OnceLock<DashMap<String, AtomicUsize>> = OnceLock::new();

/// Global pool of persistent connections per upstream ("host:port")
static UPSTREAM_POOLS: OnceLock<DashMap<String, Vec<TcpStream>>> = OnceLock::new();

fn upstream_counters() -> &'static DashMap<String, AtomicUsize> {
    UPSTREAM_COUNTERS.get_or_init(|| DashMap::new())
}

fn upstream_pools() -> &'static DashMap<String, Vec<TcpStream>> {
    UPSTREAM_POOLS.get_or_init(|| DashMap::new())
}

/// Apply nginx-style strip_prefix:
/// - If `req_path` starts with `location_path`, remove the prefix.
/// - Ensure the result starts with "/".
/// - If empty, return "/".
pub fn strip_prefix_path(req_path: &str, location_path: &str) -> String {
    // If it doesn't match, return the original path unchanged.
    if !req_path.starts_with(location_path) {
        return req_path.to_string();
    }

    // Tail after the prefix
    let mut tail = req_path[location_path.len()..].to_string();

    // If the tail is empty → "/"
    if tail.is_empty() {
        return "/".to_string();
    }

    // Ensure it starts with "/"
    if !tail.starts_with('/') {
        tail.insert(0, '/');
    }

    tail
}

/// Parse possible `server` formats:
/// - "127.0.0.1:3000"
/// - "[\"127.0.0.1:3000\", \"127.0.0.1:3001\"]"
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1]; // without [ ]
        inner
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                let part = part.trim_matches('"');
                if part.is_empty() {
                    None
                } else {
                    Some(part.to_string())
                }
            })
            .collect()
    } else {
        vec![trimmed.to_string()]
    }
}

/// Normalize `UpstreamConfig` into a Vec<String> of "host:port"
fn normalize_servers(cfg: &UpstreamConfig) -> anyhow::Result<Vec<String>> {
    let servers: Vec<String> = match &cfg.server {
        UpstreamServers::One(s) => parse_servers_from_one(s),
        UpstreamServers::Many(list) => list.clone(),
    };

    if servers.is_empty() {
        anyhow::bail!("Upstream has no configured servers");
    }

    Ok(servers)
}

/// Returns the list of upstream servers in the order they should be tried:
/// - If only one or strategy != round_robin → the original list
/// - If round_robin and multiple: [current, next, next, ...] (for fallback)
fn choose_upstream_addrs_rr_order(
    upstream_name: &str,
    upstream_cfg: &UpstreamConfig,
) -> anyhow::Result<Vec<String>> {
    let servers = normalize_servers(upstream_cfg)?;

    if servers.len() == 1 {
        return Ok(servers);
    }

    let strategy = upstream_cfg.strategy.as_deref().unwrap_or("single");
    if strategy != "round_robin" {
        return Ok(servers);
    }

    let counters = upstream_counters();
    let entry = counters
        .entry(upstream_name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));

    let idx = entry.fetch_add(1, Ordering::Relaxed);
    let start = idx % servers.len();

    let mut ordered = Vec::with_capacity(servers.len());
    for i in 0..servers.len() {
        let pos = (start + i) % servers.len();
        ordered.push(servers[pos].clone());
    }

    Ok(ordered)
}

/// Rewrites headers for proxy:
/// - removes previous X-Forwarded-*
/// - preserves the original Host
/// - appends:
///   * X-Forwarded-For
///   * X-Real-IP
///   * X-Forwarded-Proto
///   * X-Forwarded-Host
fn rewrite_proxy_headers(req_headers: &str, client_ip: &str) -> String {
    // Skip the first line (request line)
    let mut lines = req_headers.lines();
    let _ = lines.next();

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut host_value: Option<String> = None;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();

            if name.eq_ignore_ascii_case("host") {
                host_value = Some(value.clone());
            }

            if name.eq_ignore_ascii_case("x-forwarded-for")
                || name.eq_ignore_ascii_case("x-real-ip")
                || name.eq_ignore_ascii_case("x-forwarded-proto")
                || name.eq_ignore_ascii_case("x-forwarded-host")
            {
                continue;
            }

            headers.push((name, value));
        }
    }

    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
    headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    headers.push(("X-Forwarded-Proto".to_string(), "http".to_string()));

    if let Some(h) = host_value {
        headers.push(("X-Forwarded-Host".to_string(), h));
    }

    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }

    out
}

/// Takes an upstream connection from the pool or creates a new one.
#[instrument(skip())]
async fn checkout_upstream_stream(addr: &str) -> anyhow::Result<TcpStream> {
    let pools = upstream_pools();

    if let Some(mut entry) = pools.get_mut(addr) {
        if let Some(stream) = entry.pop() {
            debug!(
                target: "migux::proxy",
                upstream = %addr,
                "Reusing persistent upstream connection"
            );
            return Ok(stream);
        }
    }

    info!(
        target: "migux::proxy",
        upstream = %addr,
        "Creating new upstream connection"
    );
    let stream = TcpStream::connect(addr).await?;
    Ok(stream)
}

/// Returns a healthy upstream connection back to the pool so it can be reused.
fn checkin_upstream_stream(addr: &str, stream: TcpStream) {
    let pools = upstream_pools();
    pools
        .entry(addr.to_string())
        .or_insert_with(Vec::new)
        .push(stream);

    debug!(
        target: "migux::proxy",
        upstream = %addr,
        "Returned upstream connection to pool"
    );
}

/// Reads an HTTP response from the upstream and decides whether the connection
/// can be reused:
/// - Reads headers until `\r\n\r\n`
/// - Looks for Content-Length and Connection
/// - If Content-Length present: reads exactly that many bytes; connection may be reused
///   (unless `Connection: close`).
/// - If NO Content-Length: reads until EOF and does NOT reuse the connection.
#[instrument(skip(stream))]
async fn read_http_response(stream: &mut TcpStream) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut headers_end: Option<usize> = None;

    // Read until end of headers
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                anyhow::bail!("Upstream closed the connection without sending a response");
            } else {
                anyhow::bail!("Upstream closed the connection while reading headers");
            }
        }
        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = Some(pos);
            break;
        }

        if buf.len() > 64 * 1024 {
            anyhow::bail!("Upstream response headers are too large");
        }
    }

    let headers_end = headers_end.unwrap();
    let header_bytes = &buf[..headers_end];
    let header_str = String::from_utf8_lossy(header_bytes);

    let mut content_length: Option<usize> = None;
    let mut connection_close = false;

    let mut lines = header_str.lines();

    if let Some(status_line) = lines.next() {
        debug!(
            target: "migux::proxy",
            status_line = %status_line,
            "Received upstream status line"
        );
    }

    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }
        if let Some(rest) = lower.strip_prefix("connection:") {
            if rest.trim() == "close" {
                connection_close = true;
            }
        }
    }

    let mut response_bytes = Vec::new();
    // Headers + CRLFCRLF
    response_bytes.extend_from_slice(&buf[..headers_end + 4]);

    let already_body = buf.len().saturating_sub(headers_end + 4);

    if let Some(cl) = content_length {
        let mut body = Vec::with_capacity(cl);

        if already_body > 0 {
            let initial = &buf[headers_end + 4..];
            let to_take = initial.len().min(cl);
            body.extend_from_slice(&initial[..to_take]);
        }

        while body.len() < cl {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                warn!(
                    target: "migux::proxy",
                    expected = cl,
                    got = body.len(),
                    "Upstream closed connection before full body was read"
                );
                break;
            }
            let remaining = cl - body.len();
            let take = remaining.min(n);
            body.extend_from_slice(&tmp[..take]);
        }

        response_bytes.extend_from_slice(&body);

        let reusable = !connection_close;
        debug!(
            target: "migux::proxy",
            content_length = cl,
            reusable,
            "Finished reading upstream response with Content-Length"
        );
        Ok((response_bytes, reusable))
    } else {
        // No Content-Length → read until EOF and do not reuse
        if already_body > 0 {
            response_bytes.extend_from_slice(&buf[headers_end + 4..]);
        }
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            response_bytes.extend_from_slice(&tmp[..n]);
        }

        debug!(
            target: "migux::proxy",
            "Finished reading upstream response without Content-Length (connection not reusable)"
        );
        Ok((response_bytes, false))
    }
}

/// Proxy logic:
/// - Resolves upstream and strategy (round_robin or single)
/// - Applies strip_prefix to the request path
/// - Rewrites the request line (METHOD PATH HTTP/x.y)
/// - Rewrites headers and injects X-Forwarded-*
/// - Tries multiple upstreams in order (fallback)
/// - Reuses persistent connections when possible (keep-alive)
/// - Streams upstream response back to the client
#[instrument(
    skip(client_stream, location, req_headers, req_body, cfg),
    fields(
        client = %client_addr,
        location_path = %location.path,
    )
)]
pub async fn serve_proxy(
    client_stream: &mut TcpStream,
    location: &LocationConfig,
    req_path: &str,
    req_headers: &str,
    req_body: &[u8],
    cfg: &Arc<MiguxConfig>,
    client_addr: &SocketAddr,
) -> anyhow::Result<()> {
    // 0) Resolve upstream from config
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location missing 'upstream' field"))?;

    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' not found in config", upstream_name))?;

    // Candidates in preference order (round-robin + fallback)
    let candidate_addrs = choose_upstream_addrs_rr_order(upstream_name, upstream_cfg)?;

    let client_ip = client_addr.ip().to_string();

    // 1) strip_prefix: remove location.path from the request path
    let upstream_path = strip_prefix_path(req_path, &location.path);

    // 2) Parse the original request line
    let mut lines = req_headers.lines();

    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            warn!(
                target: "migux::proxy",
                "Missing request line in headers; returning 404"
            );
            send_404(client_stream).await?;
            return Ok(());
        }
    };

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let _old_path = parts.next().unwrap_or("/");
    let http_version = parts.next().unwrap_or("HTTP/1.1");

    debug!(
        target: "migux::proxy",
        %method,
        original_path = %req_path,
        upstream_path = %upstream_path,
        http_version = %http_version,
        upstream = %upstream_name,
        "Preparing proxied request"
    );

    // 3) Rewrite headers with X-Forwarded-*
    let rest_of_headers = rewrite_proxy_headers(req_headers, &client_ip);

    // Rebuild request for upstream
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n"); // end of headers
    out.extend_from_slice(req_body); // raw body

    let mut last_err: Option<anyhow::Error> = None;

    // 4) Try connecting to upstreams in order (fallback + keep-alive)
    for upstream_addr in &candidate_addrs {
        let mut upstream_stream = match checkout_upstream_stream(upstream_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!(
                    target: "migux::proxy",
                    upstream = %upstream_addr,
                    error = ?e,
                    "Failed to get upstream connection"
                );
                last_err = Some(e);
                continue;
            }
        };

        info!(
            target: "migux::proxy",
            method = %method,
            original_path = %req_path,
            upstream = %upstream_name,
            upstream_addr = %upstream_addr,
            upstream_path = %upstream_path,
            "Forwarding request to upstream"
        );

        if let Err(e) = upstream_stream.write_all(&out).await {
            error!(
                target: "migux::proxy",
                upstream_addr = %upstream_addr,
                error = ?e,
                "Error writing request to upstream"
            );
            last_err = Some(e.into());
            // Do not return this connection to the pool
            continue;
        }

        // Read upstream response (single HTTP response) and decide if the
        // connection can be reused.
        let (resp_bytes, reusable) = match read_http_response(&mut upstream_stream).await {
            Ok(r) => r,
            Err(e) => {
                error!(
                    target: "migux::proxy",
                    upstream_addr = %upstream_addr,
                    error = ?e,
                    "Error reading response from upstream"
                );
                last_err = Some(e);
                continue;
            }
        };

        if let Err(e) = client_stream.write_all(&resp_bytes).await {
            error!(
                target: "migux::proxy",
                error = ?e,
                "Error writing proxied response back to client"
            );
            last_err = Some(e.into());
            // Even if the client write fails, the upstream connection may still be valid,
            // but to keep logic simple we do not return it to the pool in this case.
            continue;
        }

        if let Err(e) = client_stream.flush().await {
            warn!(
                target: "migux::proxy",
                error = ?e,
                "Error flushing response to client"
            );
        }

        // Success: if upstream supports keep-alive, return connection to pool
        if reusable {
            checkin_upstream_stream(upstream_addr, upstream_stream);
        }

        return Ok(());
    }

    // All upstreams failed → 502 Bad Gateway
    error!(
        target: "migux::proxy",
        upstream = %upstream_name,
        error = ?last_err,
        "All upstreams failed; returning 502 Bad Gateway"
    );
    send_502(client_stream).await?;
    Ok(())
}
