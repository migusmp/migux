/// =======================================================
/// HEADER REWRITE (proxy semantics)
/// =======================================================
///
/// Reglas que aplicas:
/// - Quitas X-Forwarded-* previos (para no duplicar/corromper)
/// - Quitas hop-by-hop headers (por especificacion HTTP proxy)
/// - Guardas Host original para meterlo en X-Forwarded-Host
/// - Anades:
///   - X-Forwarded-For
///   - X-Real-IP
///   - X-Forwarded-Proto
///   - X-Forwarded-Host (si habia Host)
///
/// Y ademas:
/// - Controla `Connection` hacia upstream (keep-alive o close)
///   segun la politica que decidas en el caller.
pub(super) fn rewrite_proxy_headers(
    req_headers: &str,
    client_ip: &str,
    keep_alive: bool,
    body_len: usize,
    is_chunked: bool,
) -> String {
    let connection_tokens = collect_connection_tokens(req_headers);
    let mut lines = req_headers.lines();
    let _ = lines.next(); // request line (GET /... HTTP/1.1)

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
            let name_lower = name_trim.to_ascii_lowercase();

            // Captura Host original
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

            // Drop hop-by-hop headers
            // (no deben ser reenviados por un proxy)
            if name_trim.eq_ignore_ascii_case("connection")
                || name_trim.eq_ignore_ascii_case("keep-alive")
                || name_trim.eq_ignore_ascii_case("proxy-connection")
                || name_trim.eq_ignore_ascii_case("te")
                || name_trim.eq_ignore_ascii_case("trailer")
                || name_trim.eq_ignore_ascii_case("transfer-encoding")
                || name_trim.eq_ignore_ascii_case("upgrade")
                || name_trim.eq_ignore_ascii_case("content-length")
            {
                continue;
            }

            if connection_tokens.contains(&name_lower) {
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

    let connection_value = if keep_alive { "keep-alive" } else { "close" };
    headers.push(("Connection".to_string(), connection_value.to_string()));

    if is_chunked {
        headers.push(("Transfer-Encoding".to_string(), "chunked".to_string()));
    } else {
        headers.push(("Content-Length".to_string(), body_len.to_string()));
    }

    // Serializa headers con CRLF
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }

    out
}

fn collect_connection_tokens(req_headers: &str) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    let mut lines = req_headers.lines();
    let _ = lines.next(); // skip request line
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in split_header_tokens(value) {
            tokens.insert(token);
        }
    }
    tokens
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

#[cfg(test)]
mod tests {
    use super::rewrite_proxy_headers;

    #[test]
    fn rewrite_proxy_headers_drops_connection_token_headers() {
        let req = "GET / HTTP/1.1\r\nHost: example\r\nConnection: \"Foo\", keep-alive\r\nFoo: bar\r\nX-Test: ok\r\n\r\n";
        let out = rewrite_proxy_headers(req, "127.0.0.1", true, 0, false);
        assert!(!out.contains("\r\nFoo:"));
        assert!(out.contains("\r\nX-Test: ok\r\n"));
        assert!(out.contains("\r\nConnection: keep-alive\r\n"));
    }

    #[test]
    fn rewrite_proxy_headers_sets_chunked_without_content_length() {
        let req = "POST /upload HTTP/1.1\r\nHost: example\r\nTransfer-Encoding: chunked\r\nContent-Length: 10\r\n\r\n";
        let out = rewrite_proxy_headers(req, "127.0.0.1", true, 10, true);
        assert!(out.contains("\r\nTransfer-Encoding: chunked\r\n"));
        assert!(!out.contains("\r\nContent-Length: 10\r\n"));
    }
}
