use migux_config::LocationConfig;
use tracing::{debug, warn};

use crate::ServerRuntime;

/// Extracts (method, path) from the first line of the raw HTTP request.
pub fn parse_request_line(req_str: &str) -> (&str, &str) {
    if let Some(first_line) = req_str.lines().next() {
        debug!(target: "migux::http", request_line = %first_line, "Parsed HTTP request line");

        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("/");

        debug!(
            target: "migux::http",
            %method,
            %path,
            "Extracted method and path from request line"
        );

        (method, path)
    } else {
        warn!(
            target: "migux::http",
            "Request did not contain a first line; falling back to default method/path"
        );
        ("-", "/")
    }
}

/// For now, select the first server bound to this listen address.
/// In the future this could implement name-based or SNI-based selection.
pub fn select_default_server<'a>(servers: &'a [ServerRuntime]) -> &'a ServerRuntime {
    // Assumes there is at least one server per listen group.
    &servers[0]
}

/// Selects the `location` whose `path` is the longest prefix of the request path.
/// If no match is found, falls back to the first location.
pub fn match_location<'a>(locations: &'a [LocationConfig], path: &str) -> &'a LocationConfig {
    let loc = locations
        .iter()
        .filter(|loc| path.starts_with(&loc.path))
        .max_by_key(|loc| loc.path.len())
        .unwrap_or(&locations[0]);

    debug!(
        target: "migux::router",
        request_path = %path,
        matched_location_path = %loc.path,
        "Matched location by longest-prefix strategy"
    );

    loc
}
