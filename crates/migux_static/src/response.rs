//! HTTP response builders for static file serving.

/// Build an HTTP/1.1 response with optional keep-alive.
pub(crate) fn build_response_bytes(
    status: &str,
    content_type: Option<&str>,
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let mut headers = String::new();

    headers.push_str(&format!("HTTP/1.1 {}\r\n", status));
    headers.push_str(&format!("Content-Length: {}\r\n", body.len()));

    if let Some(ct) = content_type {
        headers.push_str(&format!("Content-Type: {}\r\n", ct));
    }

    if keep_alive {
        headers.push_str("Connection: keep-alive\r\n");
    } else {
        headers.push_str("Connection: close\r\n");
    }
    headers.push_str("\r\n");

    let mut out = headers.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Build a 404 response.
pub(crate) fn build_404(keep_alive: bool) -> Vec<u8> {
    let body = b"404 Not Found";
    build_response_bytes("404 Not Found", Some("text/plain; charset=utf-8"), body, keep_alive)
}

/// Build a 500 response.
pub(crate) fn build_500(keep_alive: bool) -> Vec<u8> {
    let body = b"500 Internal Server Error";
    build_response_bytes(
        "500 Internal Server Error",
        Some("text/plain; charset=utf-8"),
        body,
        keep_alive,
    )
}
