//! HTTP response builders for static file serving.

type HeaderPair<'a> = (&'a str, &'a str);

const HTTP_VERSION: &str = "HTTP/1.1";
const CRLF: &str = "\r\n";
const HEADER_CONTENT_LENGTH: &str = "Content-Length";
const HEADER_CONTENT_TYPE: &str = "Content-Type";
const HEADER_CONNECTION: &str = "Connection";
const CONNECTION_KEEP_ALIVE: &str = "keep-alive";
const CONNECTION_CLOSE: &str = "close";
const TEXT_PLAIN_UTF8: &str = "text/plain; charset=utf-8";

/// Metadata required to render the response header section.
struct ResponseHead<'a> {
    status: &'a str,
    content_type: Option<&'a str>,
    content_length: usize,
    keep_alive: bool,
    extra_headers: &'a [HeaderPair<'a>],
}

impl<'a> ResponseHead<'a> {
    /// Create a response head that describes the status line and headers.
    fn new(
        status: &'a str,
        content_type: Option<&'a str>,
        content_length: usize,
        keep_alive: bool,
        extra_headers: &'a [HeaderPair<'a>],
    ) -> Self {
        Self {
            status,
            content_type,
            content_length,
            keep_alive,
            extra_headers,
        }
    }

    /// Render the header section into a String.
    fn to_string(&self) -> String {
        let mut headers = String::with_capacity(self.header_len_hint());
        write_status_line(&mut headers, self.status);
        write_header(
            &mut headers,
            HEADER_CONTENT_LENGTH,
            &self.content_length.to_string(),
        );

        if let Some(ct) = self.content_type {
            write_header(&mut headers, HEADER_CONTENT_TYPE, ct);
        }

        for (name, value) in self.extra_headers {
            write_header(&mut headers, name, value);
        }

        let connection = connection_value(self.keep_alive);
        write_header(&mut headers, HEADER_CONNECTION, connection);
        headers.push_str(CRLF);
        headers
    }

    /// Estimate the size of the header block to reduce reallocations.
    fn header_len_hint(&self) -> usize {
        let mut len = 0;
        len += HTTP_VERSION.len() + 1 + self.status.len() + CRLF.len();
        len += HEADER_CONTENT_LENGTH.len() + 2 + decimal_len(self.content_length) + CRLF.len();

        if let Some(ct) = self.content_type {
            len += HEADER_CONTENT_TYPE.len() + 2 + ct.len() + CRLF.len();
        }

        for (name, value) in self.extra_headers {
            len += name.len() + 2 + value.len() + CRLF.len();
        }

        let connection = connection_value(self.keep_alive);
        len += HEADER_CONNECTION.len() + 2 + connection.len() + CRLF.len();
        len += CRLF.len();
        len
    }
}

/// Return the correct Connection header value for the keep-alive setting.
fn connection_value(keep_alive: bool) -> &'static str {
    if keep_alive {
        CONNECTION_KEEP_ALIVE
    } else {
        CONNECTION_CLOSE
    }
}

/// Compute the number of decimal digits required to render a usize.
fn decimal_len(mut value: usize) -> usize {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

/// Append an HTTP status line to the output buffer.
fn write_status_line(out: &mut String, status: &str) {
    out.push_str(HTTP_VERSION);
    out.push(' ');
    out.push_str(status);
    out.push_str(CRLF);
}

/// Append a single header line to the output buffer.
fn write_header(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
    out.push_str(CRLF);
}

/// Combine a rendered header block with an optional body.
fn write_response(head: ResponseHead<'_>, body: Option<&[u8]>) -> Vec<u8> {
    let mut out = head.to_string().into_bytes();
    if let Some(body) = body {
        out.extend_from_slice(body);
    }
    out
}

/// Central builder for static HTTP responses.
pub(crate) struct ResponseBuilder;

impl ResponseBuilder {
    /// Build an HTTP/1.1 response with optional keep-alive and extra headers.
    pub(crate) fn build_with_headers(
        status: &str,
        content_type: Option<&str>,
        content_length: usize,
        keep_alive: bool,
        extra_headers: &[HeaderPair<'_>],
        body: Option<&[u8]>,
    ) -> Vec<u8> {
        if body.is_none() && content_length == 0 && content_type.is_none() {
            return Self::build_empty(status, keep_alive, extra_headers);
        }

        let head = ResponseHead::new(
            status,
            content_type,
            content_length,
            keep_alive,
            extra_headers,
        );
        write_response(head, body)
    }

    /// Build an HTTP/1.1 response with a body and optional keep-alive.
    pub(crate) fn build(
        status: &str,
        content_type: Option<&str>,
        body: &[u8],
        keep_alive: bool,
    ) -> Vec<u8> {
        Self::build_with_headers(
            status,
            content_type,
            body.len(),
            keep_alive,
            &[],
            Some(body),
        )
    }

    /// Build an HTTP response with no body payload.
    pub(crate) fn build_empty(
        status: &str,
        keep_alive: bool,
        extra_headers: &[HeaderPair<'_>],
    ) -> Vec<u8> {
        let head = ResponseHead::new(status, None, 0, keep_alive, extra_headers);
        write_response(head, None)
    }

    /// Build a text/plain response with UTF-8 charset.
    pub(crate) fn plain_text(status: &str, body: &str, keep_alive: bool) -> Vec<u8> {
        Self::build(status, Some(TEXT_PLAIN_UTF8), body.as_bytes(), keep_alive)
    }

    /// Build a 404 response with a plain-text body.
    pub(crate) fn not_found(keep_alive: bool) -> Vec<u8> {
        Self::plain_text("404 Not Found", "404 Not Found", keep_alive)
    }

    /// Build a 500 response with a plain-text body.
    pub(crate) fn internal_error(keep_alive: bool) -> Vec<u8> {
        Self::plain_text("500 Internal Server Error", "500 Internal Server Error", keep_alive)
    }
}
