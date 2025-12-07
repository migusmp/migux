use migux_config::LocationConfig;

use crate::ServerRuntime;

/// Extrae (método, path) de la primera línea del request.
pub fn parse_request_line(req_str: &str) -> (&str, &str) {
    if let Some(first_line) = req_str.lines().next() {
        println!("[worker] request line: {}", first_line);
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("/");
        (method, path)
    } else {
        ("-", "/")
    }
}

/// De momento, elegimos siempre el primer server del listen.
pub fn select_default_server<'a>(servers: &'a [ServerRuntime]) -> &'a ServerRuntime {
    &servers[0]
}

/// Elige la location cuyo `path` sea prefijo más largo del path de la request.
pub fn match_location<'a>(locations: &'a [LocationConfig], path: &str) -> &'a LocationConfig {
    locations
        .iter()
        .filter(|loc| path.starts_with(&loc.path))
        .max_by_key(|loc| loc.path.len())
        .unwrap_or(&locations[0])
}
