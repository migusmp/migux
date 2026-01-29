/// =======================================================
/// URL REWRITE: strip_prefix tipo nginx
/// =======================================================
///
/// Esto implementa la idea tipica:
/// location /api/ { proxy_pass http://app; }  ->  /api/users -> /users
///
/// - Si req_path NO empieza por location_path => no toca nada
/// - Si el "tail" queda vacio => "/"
/// - Asegura que empiece por '/'
pub(super) fn strip_prefix_path(req_path: &str, location_path: &str) -> String {
    if !req_path.starts_with(location_path) {
        return req_path.to_string();
    }

    // parte restante despues del prefijo
    let mut tail = req_path[location_path.len()..].to_string();

    // exact match: "/api" -> "/"
    if tail.is_empty() {
        return "/".to_string();
    }

    // si quedo "users" en lugar de "/users"
    if !tail.starts_with('/') {
        tail.insert(0, '/');
    }

    tail
}
