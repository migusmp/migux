//! Streaming and parsing of upstream HTTP/1 responses.
//!
//! Handles header parsing, chunked transfer decoding, and body forwarding
//! while enforcing configured limits.

use bytes::BytesMut;
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::{timeout, Duration},
};
use tracing::{debug, instrument, warn};

use super::pool::PooledStream;

/// =======================================================
/// HTTP RESPONSE STREAMER
/// =======================================================
///
/// Lee la respuesta HTTP desde upstream y la va escribiendo al cliente:
/// - Headers: parse + forward
/// - Body:
///   - chunked: parsea chunks y los forwardea
///   - content-length: forwardea exactamente CL bytes
///   - sin CL: read-to-EOF (no reusable)
/// Stream an upstream HTTP response to the client and return whether the
/// upstream connection is reusable.
#[instrument(skip(upstream, client_stream))]
pub(super) async fn stream_http_response<S>(
    upstream: &mut PooledStream,
    client_stream: &mut S,
    method: &str,
    read_timeout: Duration,
    max_headers: usize,
    max_body: usize,
) -> anyhow::Result<bool>
where
    S: AsyncWrite + Unpin,
{
    let headers_end = read_response_headers(upstream, read_timeout, max_headers).await?;
    let headers_bytes = upstream.read_buf.split_to(headers_end + 4);
    let header_len = headers_bytes.len().saturating_sub(4);

    let info = parse_response_headers(&headers_bytes[..header_len])?;
    let no_body = is_no_body(method, info.status_code);

    client_stream.write_all(&headers_bytes).await?;

    let mut reusable = if info.is_http10 {
        info.connection_keep_alive && !info.connection_close
    } else {
        !info.connection_close
    };

    if no_body {
        return Ok(reusable);
    }

    if info.is_chunked {
        stream_chunked_body(upstream, client_stream, read_timeout, max_body).await?;
        return Ok(reusable);
    }

    if let Some(cl) = info.content_length {
        if max_body > 0 && cl > max_body {
            anyhow::bail!("Upstream response body too large");
        }
        let complete = stream_content_length(upstream, client_stream, cl, read_timeout).await?;
        if !complete {
            reusable = false;
        }
        return Ok(reusable);
    }

    // Sin Content-Length y no chunked: leer hasta EOF -> no reusable
    stream_until_eof(upstream, client_stream, read_timeout, max_body).await?;
    Ok(false)
}

async fn read_response_headers(
    upstream: &mut PooledStream,
    read_timeout: Duration,
    max_headers: usize,
) -> anyhow::Result<usize> {
    loop {
        if let Some(pos) = find_headers_end(&upstream.read_buf) {
            return Ok(pos);
        }

        if max_headers > 0 && upstream.read_buf.len() > max_headers {
            anyhow::bail!("Upstream response headers too large");
        }

        let n = read_more(upstream, read_timeout).await?;
        if n == 0 {
            anyhow::bail!("Upstream closed connection while reading headers");
        }
    }
}

async fn read_more(upstream: &mut PooledStream, read_timeout: Duration) -> anyhow::Result<usize> {
    let mut tmp = [0u8; 8192];
    let n = match timeout(read_timeout, upstream.stream.read(&mut tmp)).await {
        Ok(res) => res?,
        Err(_) => anyhow::bail!("Upstream read timeout"),
    };
    if n > 0 {
        upstream.read_buf.extend_from_slice(&tmp[..n]);
    }
    Ok(n)
}

fn find_headers_end(buf: &BytesMut) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parsed response metadata used to drive body handling.
#[derive(Debug, Default)]
struct ResponseInfo {
    content_length: Option<usize>,
    connection_close: bool,
    connection_keep_alive: bool,
    is_http10: bool,
    is_chunked: bool,
    status_code: Option<u16>,
}

/// Tracks Content-Length parsing state for duplicate header detection.
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

/// Parse HTTP response headers and extract body/connection metadata.
fn parse_response_headers(header_bytes: &[u8]) -> anyhow::Result<ResponseInfo> {
    let header_str = String::from_utf8_lossy(header_bytes);
    let mut info = ResponseInfo::default();
    let mut content_length = ContentLengthState::default();

    let mut lines = header_str.lines();

    if let Some(status_line) = lines.next() {
        debug!(target: "migux::proxy", status_line = %status_line, "Received upstream status line");
        if status_line.starts_with("HTTP/1.0") {
            info.is_http10 = true;
        }
        info.status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok());
    }

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
            "connection" => {
                for token in split_header_tokens(value) {
                    match token.as_str() {
                        "close" => info.connection_close = true,
                        "keep-alive" => info.connection_keep_alive = true,
                        _ => {}
                    }
                }
            }
            "transfer-encoding" => {
                for token in split_header_tokens(value) {
                    if token == "chunked" {
                        info.is_chunked = true;
                    }
                }
            }
            _ => {}
        }
    }

    if content_length.invalid {
        if content_length.conflict {
            anyhow::bail!("Conflicting Content-Length in upstream response");
        }
        anyhow::bail!("Invalid Content-Length in upstream response");
    }

    info.content_length = content_length.value;

    Ok(info)
}

fn is_no_body(method: &str, status_code: Option<u16>) -> bool {
    if method.eq_ignore_ascii_case("HEAD") {
        return true;
    }
    match status_code {
        Some(code) if (100..200).contains(&code) => true,
        Some(204) | Some(304) => true,
        _ => false,
    }
}

async fn stream_content_length<S>(
    upstream: &mut PooledStream,
    client_stream: &mut S,
    mut remaining: usize,
    read_timeout: Duration,
) -> anyhow::Result<bool>
where
    S: AsyncWrite + Unpin,
{
    while remaining > 0 {
        if upstream.read_buf.is_empty() {
            let n = read_more(upstream, read_timeout).await?;
            if n == 0 {
                warn!(
                    target: "migux::proxy",
                    expected = remaining,
                    "Upstream closed before full body was read"
                );
                return Ok(false);
            }
        }

        let take = remaining.min(upstream.read_buf.len());
        let chunk = upstream.read_buf.split_to(take);
        client_stream.write_all(&chunk).await?;
        remaining -= take;
    }

    Ok(true)
}

async fn stream_until_eof<S>(
    upstream: &mut PooledStream,
    client_stream: &mut S,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut body_bytes = 0usize;

    if !upstream.read_buf.is_empty() {
        body_bytes += upstream.read_buf.len();
        if max_body > 0 && body_bytes > max_body {
            anyhow::bail!("Upstream response body too large");
        }
        let chunk = upstream.read_buf.split_to(upstream.read_buf.len());
        client_stream.write_all(&chunk).await?;
    }

    loop {
        let n = read_more(upstream, read_timeout).await?;
        if n == 0 {
            break;
        }
        body_bytes += n;
        if max_body > 0 && body_bytes > max_body {
            anyhow::bail!("Upstream response body too large");
        }
        let chunk = upstream.read_buf.split_to(n);
        client_stream.write_all(&chunk).await?;
    }

    Ok(())
}

async fn stream_chunked_body<S>(
    upstream: &mut PooledStream,
    client_stream: &mut S,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut body_bytes = 0usize;

    loop {
        let line = read_line(upstream, read_timeout).await?;
        client_stream.write_all(&line).await?;

        let line_str = String::from_utf8_lossy(&line);
        let size_str = line_str.trim().trim_end_matches('\r').trim_end_matches('\n');
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_str, 16)
            .map_err(|_| anyhow::anyhow!("Invalid chunk size"))?;

        if chunk_size == 0 {
            // Trailers: forward until empty line
            loop {
                let trailer = read_line(upstream, read_timeout).await?;
                client_stream.write_all(&trailer).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        if max_body > 0 && body_bytes + chunk_size > max_body {
            anyhow::bail!("Upstream response body too large");
        }

        let total = chunk_size + 2; // data + CRLF
        read_exact_from_buf(upstream, client_stream, read_timeout, total).await?;

        body_bytes += chunk_size;
    }
}

async fn read_line(upstream: &mut PooledStream, read_timeout: Duration) -> anyhow::Result<Vec<u8>> {
    loop {
        if let Some(pos) = upstream
            .read_buf
            .windows(2)
            .position(|w| w == b"\r\n")
        {
            let line = upstream.read_buf.split_to(pos + 2);
            return Ok(line.to_vec());
        }

        let n = read_more(upstream, read_timeout).await?;
        if n == 0 {
            anyhow::bail!("Upstream closed connection while reading chunked line");
        }
    }
}

async fn read_exact_from_buf<S>(
    upstream: &mut PooledStream,
    client_stream: &mut S,
    read_timeout: Duration,
    mut remaining: usize,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    while remaining > 0 {
        if upstream.read_buf.is_empty() {
            let n = read_more(upstream, read_timeout).await?;
            if n == 0 {
                anyhow::bail!("Upstream closed connection while reading chunked body");
            }
        }

        let take = remaining.min(upstream.read_buf.len());
        let chunk = upstream.read_buf.split_to(take);
        client_stream.write_all(&chunk).await?;
        remaining -= take;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_response_headers;

    #[test]
    fn parse_response_headers_accepts_duplicate_content_length() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n";
        let info = parse_response_headers(headers).expect("expected ok");
        assert_eq!(info.content_length, Some(5));
    }

    #[test]
    fn parse_response_headers_rejects_conflicting_content_length() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n";
        let err = parse_response_headers(headers).unwrap_err();
        assert!(err.to_string().contains("Conflicting Content-Length"));
    }

    #[test]
    fn parse_response_headers_rejects_invalid_content_length() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n";
        let err = parse_response_headers(headers).unwrap_err();
        assert!(err.to_string().contains("Invalid Content-Length"));
    }

    #[test]
    fn parse_response_headers_detects_chunked_and_connection_tokens() {
        let headers = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, \"chunked\"\r\nConnection: \"close\"\r\n\r\n";
        let info = parse_response_headers(headers).expect("expected ok");
        assert!(info.is_chunked);
        assert!(info.connection_close);
    }
}
