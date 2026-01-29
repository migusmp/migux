use bytes::BytesMut;
use migux_config::HttpConfig;
use migux_http::responses::{send_400, send_408, send_413, send_431};
use tokio::time::Duration;
use tracing::{debug, instrument, warn};

use super::timeouts::{read_more, ReadOutcome};
use super::ClientStream;

/// Reads a full HTTP request:
/// - Reads until `\r\n\r\n` (end of headers)
/// - Extracts Content-Length if present
/// - Reads the full body if required
/// - Returns (headers as String, body as Vec<u8>)
#[derive(Debug)]
pub(crate) struct ParsedRequest {
    pub(crate) headers: String,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) http_version: String,
    pub(crate) content_length: usize,
    pub(crate) is_chunked: bool,
    pub(crate) close_after: bool,
    pub(crate) body_start: usize,
}

#[instrument(skip(stream, buf, http), fields())]
pub(crate) async fn read_http_request(
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

pub(crate) fn extract_host_header(headers: &str) -> Option<String> {
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

fn find_headers_end(buf: &BytesMut) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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
