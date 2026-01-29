use std::{net::SocketAddr, sync::Arc};

use bytes::{Buf, BytesMut};
use migux_http::responses::{send_400, send_404, send_408, send_413, send_431, send_redirect};
use migux_proxy::Proxy;
use migux_static::serve_static_cached;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    time::{timeout, Duration},
};
use tracing::{debug, info, instrument, warn};

use migux_config::{HttpConfig, LocationType, MiguxConfig};

use crate::ServerRuntime;

mod routing;

use routing::{match_location, select_default_server};

pub trait ClientStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ClientStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Entry point for a “logical worker” that handles a single connection.
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

        let mut close_after = req.close_after;

        // Drop headers from buffer; keep body/leftovers for streaming or next request.
        if req.body_start > 0 {
            buf.advance(req.body_start);
        }

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

                serve_static_cached(
                    &mut stream,
                    &cfg.http,
                    &server.config,
                    location,
                    method,
                    path,
                )
                .await?;
                close_after = true;

                // Discard request body (if any) so keep-alive doesn't break.
                if req.is_chunked {
                    let _ = discard_chunked_body(
                        &mut stream,
                        &mut buf,
                        Duration::from_secs(cfg.http.client_read_timeout_secs),
                        cfg.http.max_request_body_bytes as usize,
                    )
                    .await;
                } else if req.content_length > 0 {
                    let _ = discard_content_length(
                        &mut stream,
                        &mut buf,
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
                        &mut stream,
                        &mut buf,
                        location,
                        &req.headers,
                        method,
                        path,
                        &req.http_version,
                        req.content_length,
                        req.is_chunked,
                        is_tls,
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
    method: String,
    path: String,
    http_version: String,
    content_length: usize,
    is_chunked: bool,
    close_after: bool,
    body_start: usize,
}

#[instrument(skip(stream, buf, http), fields())]
async fn read_http_request(
    stream: &mut dyn ClientStream,
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

    let meta = match parse_request_metadata(&headers_str) {
        Ok(meta) => meta,
        Err(err) => {
            warn!(
                target: "migux::http",
                error = ?err,
                "Invalid request headers"
            );
            send_400(stream).await?;
            return Ok(None);
        }
    };

    let RequestMetadata {
        method,
        path,
        http_version,
        mut content_length,
        close_after,
        is_chunked,
    } = meta;

    if is_chunked && content_length > 0 {
        warn!(
            target: "migux::http",
            content_length,
            "Ignoring Content-Length because Transfer-Encoding is chunked"
        );
        content_length = 0;
    }

    if !is_chunked && content_length > 0 {
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

    Ok(Some(ParsedRequest {
        headers: headers_str,
        method,
        path,
        http_version,
        content_length,
        is_chunked,
        close_after,
        body_start: headers_end + 4,
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
    stream: &mut dyn ClientStream,
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

#[derive(Debug)]
struct RequestMetadata {
    method: String,
    path: String,
    http_version: String,
    content_length: usize,
    close_after: bool,
    is_chunked: bool,
}

#[derive(Debug)]
enum HeaderParseError {
    InvalidContentLength,
    ConflictingContentLength,
}

#[derive(Default)]
struct ContentLengthState {
    value: Option<usize>,
    invalid: bool,
    conflict: bool,
}

impl ContentLengthState {
    fn add(&mut self, raw: &str) {
        let mut any = false;
        for part in raw.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            any = true;
            match trimmed.parse::<usize>() {
                Ok(len) => {
                    if let Some(prev) = self.value {
                        if prev != len {
                            self.conflict = true;
                            self.invalid = true;
                        }
                    } else {
                        self.value = Some(len);
                    }
                }
                Err(_) => {
                    self.invalid = true;
                }
            }
        }
        if !any {
            self.invalid = true;
        }
    }
}

fn split_header_tokens(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split(',').filter_map(|token| {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(
                trimmed
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_ascii_lowercase(),
            )
        }
    })
}

fn parse_request_metadata(headers: &str) -> Result<RequestMetadata, HeaderParseError> {
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("-").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let http_version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut content_length = ContentLengthState::default();
    let mut connection_close = false;
    let mut connection_keep_alive = false;
    let mut is_chunked = false;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        let name_lower = name.to_ascii_lowercase();

        match name_lower.as_str() {
            "content-length" => {
                content_length.add(value);
            }
            "connection" | "proxy-connection" => {
                for token in split_header_tokens(value) {
                    match token.as_str() {
                        "close" => connection_close = true,
                        "keep-alive" => connection_keep_alive = true,
                        _ => {}
                    }
                }
            }
            "transfer-encoding" => {
                for token in split_header_tokens(value) {
                    if token == "chunked" {
                        is_chunked = true;
                    }
                }
            }
            _ => {}
        }
    }

    if content_length.invalid {
        let err = if content_length.conflict {
            HeaderParseError::ConflictingContentLength
        } else {
            HeaderParseError::InvalidContentLength
        };
        return Err(err);
    }

    let close_after = if http_version == "HTTP/1.0" {
        !connection_keep_alive || connection_close
    } else {
        connection_close
    };

    Ok(RequestMetadata {
        method,
        path,
        http_version,
        content_length: content_length.value.unwrap_or(0),
        close_after,
        is_chunked,
    })
}

enum ChunkedBodyError {
    Timeout,
    Invalid,
    TooLarge,
    Io,
}

async fn discard_chunked_body(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    read_timeout: Duration,
    max_body: usize,
) -> Result<(), ChunkedBodyError> {
    let mut body_bytes = 0usize;

    loop {
        let line = read_line_bytes(stream, buf, read_timeout).await?;
        let size_str = match std::str::from_utf8(&line[..line.len() - 2]) {
            Ok(s) => s.split(';').next().unwrap_or("").trim(),
            Err(_) => return Err(ChunkedBodyError::Invalid),
        };
        let chunk_size = usize::from_str_radix(size_str, 16)
            .map_err(|_| ChunkedBodyError::Invalid)?;

        if chunk_size == 0 {
            loop {
                let trailer = read_line_bytes(stream, buf, read_timeout).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        body_bytes = body_bytes.saturating_add(chunk_size);
        if max_body > 0 && body_bytes > max_body {
            return Err(ChunkedBodyError::TooLarge);
        }

        discard_exact(stream, buf, chunk_size + 2, read_timeout).await?;
    }
}

async fn read_line_bytes(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    read_timeout: Duration,
) -> Result<Vec<u8>, ChunkedBodyError> {
    loop {
        if let Some(end) = find_crlf(buf, 0) {
            let line = buf.split_to(end + 2);
            return Ok(line.to_vec());
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
}

async fn discard_exact(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    mut remaining: usize,
    read_timeout: Duration,
) -> Result<(), ChunkedBodyError> {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = remaining.min(buf.len());
            buf.advance(take);
            remaining -= take;
            continue;
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
    Ok(())
}

async fn discard_content_length(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    mut remaining: usize,
    read_timeout: Duration,
) -> Result<(), ChunkedBodyError> {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = remaining.min(buf.len());
            buf.advance(take);
            remaining -= take;
            continue;
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
    Ok(())
}

fn find_crlf(buf: &BytesMut, start: usize) -> Option<usize> {
    buf[start..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}

fn extract_host_header(headers: &str) -> Option<String> {
    for line in headers.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("host") {
            let host = value.trim();
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{parse_request_metadata, HeaderParseError};

    #[test]
    fn parse_request_metadata_accepts_duplicate_content_length() {
        let headers = "POST /upload HTTP/1.1\r\nHost: example\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n";
        let meta = parse_request_metadata(headers).expect("expected ok");
        assert_eq!(meta.content_length, 5);
    }

    #[test]
    fn parse_request_metadata_rejects_conflicting_content_length() {
        let headers = "POST /upload HTTP/1.1\r\nHost: example\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n";
        let err = parse_request_metadata(headers).unwrap_err();
        assert!(matches!(err, HeaderParseError::ConflictingContentLength));
    }

    #[test]
    fn parse_request_metadata_rejects_invalid_content_length() {
        let headers = "POST /upload HTTP/1.1\r\nHost: example\r\nContent-Length: nope\r\n\r\n";
        let err = parse_request_metadata(headers).unwrap_err();
        assert!(matches!(err, HeaderParseError::InvalidContentLength));
    }

    #[test]
    fn parse_request_metadata_connection_tokens() {
        let headers = "GET / HTTP/1.1\r\nConnection: \"keep-alive\", close\r\n\r\n";
        let meta = parse_request_metadata(headers).expect("expected ok");
        assert!(meta.close_after);
    }

    #[test]
    fn parse_request_metadata_detects_chunked_with_tokens() {
        let headers = "POST / HTTP/1.1\r\nTransfer-Encoding: gzip, \"chunked\"\r\nContent-Length: 10\r\n\r\n";
        let meta = parse_request_metadata(headers).expect("expected ok");
        assert!(meta.is_chunked);
        assert_eq!(meta.content_length, 10);
    }
}
