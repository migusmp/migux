//! Filesystem/path helpers for static serving.

pub(crate) struct PathResolver;

impl PathResolver {
    /// Resolve a request path to a relative file path within the location root.
    pub(crate) fn resolve_relative_path(
        req_path: &str,
        location_path: &str,
        index: &str,
    ) -> Option<String> {
        if req_path == "/" && location_path == "/" {
            return Some(index.to_string());
        }

        if req_path == location_path {
            return Some(index.to_string());
        }

        if req_path.starts_with(location_path) {
            let mut tail = &req_path[location_path.len()..];

            if tail.starts_with('/') {
                tail = &tail[1..];
            }

            if tail.is_empty() {
                Some(index.to_string())
            } else {
                Some(tail.to_string())
            }
        } else {
            None
        }
    }
}
