use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

use dashmap::DashMap;
use migux_config::{LocationConfig, MiguxConfig, UpstreamConfig, UpstreamServers};
use migux_http::responses::{send_404, send_502};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, error, info, instrument, warn};

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

async fn connect_fresh(addr: &str) -> anyhow::Result<TcpStream> {
    Ok(TcpStream::connect(addr).await?)
}

/// Apply nginx-style strip_prefix:
pub fn strip_prefix_path(req_path: &str, location_path: &str) -> String {
    if !req_path.starts_with(location_path) {
        return req_path.to_string();
    }

    let mut tail = req_path[location_path.len()..].to_string();

    if tail.is_empty() {
        return "/".to_string();
    }

    if !tail.starts_with('/') {
        tail.insert(0, '/');
    }

    tail
}

/// Parse possible `server` formats:
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .split(',')
            .filter_map(|part| {
                let part = part.trim().trim_matches('"');
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

/// Returns upstream servers in rr order + fallback order
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
/// - removes hop-by-hop headers (Connection, TE, etc.)
/// - preserves original Host (for X-Forwarded-Host)
/// - appends X-Forwarded-*
///
/// IMPORTANT: We also force `Connection: close` to upstream by default here to
/// reduce weird keep-alive behavior while you iterate. If you want full keep-alive
/// later, remove it and implement chunked + http/1.0 rules properly.
fn rewrite_proxy_headers(req_headers: &str, client_ip: &str) -> String {
    let mut lines = req_headers.lines();
    let _ = lines.next(); // request line

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut host_value: Option<String> = None;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some((name, value)) = line.split_once(':') {
            let name_trim = name.trim().to_string();
            let value_trim = value.trim().to_string();

            if name_trim.eq_ignore_ascii_case("host") {
                host_value = Some(value_trim.clone());
            }

            // Drop previous forwarded headers
            if name_trim.eq_ignore_ascii_case("x-forwarded-for")
                || name_trim.eq_ignore_ascii_case("x-real-ip")
                || name_trim.eq_ignore_ascii_case("x-forwarded-proto")
                || name_trim.eq_ignore_ascii_case("x-forwarded-host")
            {
                continue;
            }

            // Drop hop-by-hop headers (proxy must manage these)
            if name_trim.eq_ignore_ascii_case("connection")
                || name_trim.eq_ignore_ascii_case("keep-alive")
                || name_trim.eq_ignore_ascii_case("proxy-connection")
                || name_trim.eq_ignore_ascii_case("te")
                || name_trim.eq_ignore_ascii_case("trailer")
                || name_trim.eq_ignore_ascii_case("transfer-encoding")
                || name_trim.eq_ignore_ascii_case("upgrade")
            {
                continue;
            }

            headers.push((name_trim, value_trim));
        }
    }

    // Add forward headers
    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
    headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    headers.push(("X-Forwarded-Proto".to_string(), "http".to_string()));

    if let Some(h) = host_value {
        headers.push(("X-Forwarded-Host".to_string(), h));
    }

    // Force upstream close for now (safer during early proxy work)
    headers.push(("Connection".to_string(), "close".to_string()));

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
            debug!(target: "migux::proxy", upstream = %addr, "Reusing pooled upstream connection");
            return Ok(stream);
        }
    }

    info!(target: "migux::proxy", upstream = %addr, "Creating new upstream connection");
    Ok(TcpStream::connect(addr).await?)
}

/// Returns an upstream connection back to the pool so it can be reused.
fn checkin_upstream_stream(addr: &str, stream: TcpStream) {
    let pools = upstream_pools();
    pools
        .entry(addr.to_string())
        .or_insert_with(Vec::new)
        .push(stream);

    debug!(target: "migux::proxy", upstream = %addr, "Returned upstream connection to pool");
}

/// Reads an HTTP response from the upstream.
/// Decides reusability:
/// - If Content-Length present and Connection != close => reusable
/// - If HTTP/1.0 => ONLY reusable if `Connection: keep-alive`
/// - If Transfer-Encoding: chunked => mark NOT reusable (until you implement chunk parsing)
#[instrument(skip(stream))]
async fn read_http_response(stream: &mut TcpStream) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut headers_end: Option<usize> = None;

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                anyhow::bail!("Upstream closed connection without sending a response");
            } else {
                anyhow::bail!("Upstream closed connection while reading headers");
            }
        }
        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = Some(pos);
            break;
        }

        if buf.len() > 64 * 1024 {
            anyhow::bail!("Upstream response headers too large");
        }
    }

    let headers_end = headers_end.unwrap();
    let header_bytes = &buf[..headers_end];
    let header_str = String::from_utf8_lossy(header_bytes);

    let mut content_length: Option<usize> = None;
    let mut connection_close = false;
    let mut connection_keep_alive = false;
    let mut is_http10 = false;
    let mut is_chunked = false;

    let mut lines = header_str.lines();

    if let Some(status_line) = lines.next() {
        debug!(target: "migux::proxy", status_line = %status_line, "Received upstream status line");
        if status_line.contains("HTTP/1.0") {
            is_http10 = true;
        }
    }

    for line in lines {
        let lower = line.to_ascii_lowercase();

        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }

        if let Some(rest) = lower.strip_prefix("connection:") {
            let v = rest.trim();
            if v == "close" {
                connection_close = true;
            } else if v.contains("keep-alive") {
                connection_keep_alive = true;
            }
        }

        if let Some(rest) = lower.strip_prefix("transfer-encoding:") {
            if rest.trim().contains("chunked") {
                is_chunked = true;
            }
        }
    }

    let mut response_bytes = Vec::new();
    response_bytes.extend_from_slice(&buf[..headers_end + 4]);

    let already_body = buf.len().saturating_sub(headers_end + 4);

    // If chunked and we don't parse it yet, safest is: read to EOF and never reuse
    if is_chunked {
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
            "Chunked response detected; returning non-reusable connection (chunk parser not implemented)"
        );
        return Ok((response_bytes, false));
    }

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
                    "Upstream closed before full body was read"
                );
                break;
            }
            let remaining = cl - body.len();
            let take = remaining.min(n);
            body.extend_from_slice(&tmp[..take]);
        }

        response_bytes.extend_from_slice(&body);

        // HTTP/1.0: only reuse if explicit keep-alive and not close
        let reusable = if is_http10 {
            connection_keep_alive && !connection_close
        } else {
            !connection_close
        };

        debug!(
            target: "migux::proxy",
            content_length = cl,
            reusable,
            http10 = is_http10,
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
            "No Content-Length; read until EOF; connection not reusable"
        );
        Ok((response_bytes, false))
    }
}

#[instrument(
    skip(client_stream, location, req_headers, req_body, cfg),
    fields(client = %client_addr, location_path = %location.path)
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
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location missing 'upstream' field"))?;

    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' not found in config", upstream_name))?;

    let candidate_addrs = choose_upstream_addrs_rr_order(upstream_name, upstream_cfg)?;
    let client_ip = client_addr.ip().to_string();
    let upstream_path = strip_prefix_path(req_path, &location.path);

    let mut lines = req_headers.lines();
    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            warn!(target: "migux::proxy", "Missing request line; returning 404");
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

    let rest_of_headers = rewrite_proxy_headers(req_headers, &client_ip);

    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(req_body);

    let mut last_err: Option<anyhow::Error> = None;

    for upstream_addr in &candidate_addrs {
        // 1) take from pool or connect
        let mut upstream_stream = match checkout_upstream_stream(upstream_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!(target: "migux::proxy", upstream=%upstream_addr, error=?e, "Failed to get upstream connection");
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

        // 2) write: if pooled socket is dead, retry once with fresh connection to SAME addr
        if let Err(e) = upstream_stream.write_all(&out).await {
            error!(
                target: "migux::proxy",
                upstream_addr = %upstream_addr,
                error = ?e,
                "Write failed (likely dead pooled socket). Retrying with fresh connection"
            );
            last_err = Some(e.into());

            match connect_fresh(upstream_addr).await {
                Ok(mut fresh) => {
                    if let Err(e2) = fresh.write_all(&out).await {
                        error!(
                            target: "migux::proxy",
                            upstream_addr = %upstream_addr,
                            error = ?e2,
                            "Write failed even with fresh connection"
                        );
                        last_err = Some(e2.into());
                        continue;
                    }
                    upstream_stream = fresh;
                }
                Err(e2) => {
                    error!(
                        target: "migux::proxy",
                        upstream_addr = %upstream_addr,
                        error = ?e2,
                        "Failed to connect fresh after pooled write failure"
                    );
                    last_err = Some(e2);
                    continue;
                }
            }
        }

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
            error!(target: "migux::proxy", error=?e, "Error writing proxied response back to client");
            last_err = Some(e.into());
            continue;
        }

        if let Err(e) = client_stream.flush().await {
            warn!(target: "migux::proxy", error=?e, "Error flushing response to client");
        }

        // If reusable, put it back in pool
        if reusable {
            checkin_upstream_stream(upstream_addr, upstream_stream);
        }

        return Ok(());
    }

    error!(
        target: "migux::proxy",
        upstream = %upstream_name,
        error = ?last_err,
        "All upstreams failed; returning 502"
    );
    send_502(client_stream).await?;
    Ok(())
}
