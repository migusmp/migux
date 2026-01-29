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
) -> String {
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

    let connection_value = if keep_alive { "keep-alive" } else { "close" };
    headers.push(("Connection".to_string(), connection_value.to_string()));

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
