use bytes::BytesMut;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{timeout, Duration},
};
use tracing::{debug, instrument, warn};

use super::PooledStream;

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
#[instrument(skip(upstream, client_stream))]
pub(super) async fn stream_http_response(
    upstream: &mut PooledStream,
    client_stream: &mut TcpStream,
    method: &str,
    read_timeout: Duration,
    max_headers: usize,
    max_body: usize,
) -> anyhow::Result<bool> {
    let headers_end = read_response_headers(upstream, read_timeout, max_headers).await?;
    let headers_bytes = upstream.read_buf.split_to(headers_end + 4);
    let header_len = headers_bytes.len().saturating_sub(4);

    let info = parse_response_headers(&headers_bytes[..header_len]);
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

#[derive(Default)]
struct ResponseInfo {
    content_length: Option<usize>,
    connection_close: bool,
    connection_keep_alive: bool,
    is_http10: bool,
    is_chunked: bool,
    status_code: Option<u16>,
}

fn parse_response_headers(header_bytes: &[u8]) -> ResponseInfo {
    let header_str = String::from_utf8_lossy(header_bytes);
    let mut info = ResponseInfo::default();

    let mut lines = header_str.lines();

    if let Some(status_line) = lines.next() {
        debug!(target: "migux::proxy", status_line = %status_line, "Received upstream status line");
        if status_line.contains("HTTP/1.0") {
            info.is_http10 = true;
        }
        info.status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok());
    }

    for line in lines {
        let lower = line.to_ascii_lowercase();

        if let Some(rest) = lower.strip_prefix("content-length:")
            && let Ok(len) = rest.trim().parse::<usize>()
        {
            info.content_length = Some(len);
        }

        if let Some(rest) = lower.strip_prefix("connection:") {
            let v = rest.trim();
            if v.contains("close") {
                info.connection_close = true;
            }
            if v.contains("keep-alive") {
                info.connection_keep_alive = true;
            }
        }

        if let Some(rest) = lower.strip_prefix("transfer-encoding:")
            && rest.trim().contains("chunked")
        {
            info.is_chunked = true;
        }
    }

    info
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

async fn stream_content_length(
    upstream: &mut PooledStream,
    client_stream: &mut TcpStream,
    mut remaining: usize,
    read_timeout: Duration,
) -> anyhow::Result<bool> {
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

async fn stream_until_eof(
    upstream: &mut PooledStream,
    client_stream: &mut TcpStream,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()> {
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

async fn stream_chunked_body(
    upstream: &mut PooledStream,
    client_stream: &mut TcpStream,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()> {
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

async fn read_exact_from_buf(
    upstream: &mut PooledStream,
    client_stream: &mut TcpStream,
    read_timeout: Duration,
    mut remaining: usize,
) -> anyhow::Result<()> {
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
