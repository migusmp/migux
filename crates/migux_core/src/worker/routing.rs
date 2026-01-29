use migux_config::LocationConfig;
use tracing::debug;

use crate::ServerRuntime;

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
