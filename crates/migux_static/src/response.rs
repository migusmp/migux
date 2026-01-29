//! HTTP response builders for static file serving.

pub(crate) struct ResponseBuilder;

impl ResponseBuilder {
    /// Build an HTTP/1.1 response with optional keep-alive.
    pub(crate) fn build(
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
    pub(crate) fn not_found(keep_alive: bool) -> Vec<u8> {
        let body = b"404 Not Found";
        Self::build("404 Not Found", Some("text/plain; charset=utf-8"), body, keep_alive)
    }

    /// Build a 500 response.
    pub(crate) fn internal_error(keep_alive: bool) -> Vec<u8> {
        let body = b"500 Internal Server Error";
        Self::build(
            "500 Internal Server Error",
            Some("text/plain; charset=utf-8"),
            body,
            keep_alive,
        )
    }
}
