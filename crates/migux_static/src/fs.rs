//! Filesystem/path helpers for static serving.

pub(crate) struct PathResolver;

impl PathResolver {
    /// Resolve a request path to a relative file path within the location root.
    pub(crate) fn resolve_relative_path(
        req_path: &str,
        location_path: &str,
        index: &str,
    ) -> Option<String> {
        let req_path = strip_query(req_path);

        if !is_safe_request_path(req_path) {
            return None;
        }

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

fn strip_query(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn is_safe_request_path(path: &str) -> bool {
    let decoded = decode_path_for_check(path);
    if decoded.contains("//") {
        return false;
    }
    if decoded.contains('\\') {
        return false;
    }
    for segment in decoded.split('/') {
        if segment == ".." {
            return false;
        }
    }
    true
}

fn decode_path_for_check(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h1), Some(h2)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                let value = (h1 << 4) | h2;
                match value {
                    b'.' | b'/' | b'\\' => out.push(value as char),
                    _ => {
                        out.push('%');
                        out.push(bytes[i + 1] as char);
                        out.push(bytes[i + 2] as char);
                    }
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
