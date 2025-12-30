use mime_guess::mime;
use tokio::{fs, io::AsyncWriteExt, net::TcpStream};

use migux_config::{LocationConfig, ServerConfig};

fn build_response_bytes(status: &str, content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut headers = String::new();

    headers.push_str(&format!("HTTP/1.1 {}\r\n", status));
    headers.push_str(&format!("Content-Length: {}\r\n", body.len()));

    if let Some(ct) = content_type {
        headers.push_str(&format!("Content-Type: {}\r\n", ct));
    }

    // básico (luego le metes keep-alive si quieres)
    headers.push_str("Connection: close\r\n");
    headers.push_str("\r\n");

    let mut out = headers.into_bytes();
    out.extend_from_slice(body);
    out
}

fn build_404() -> Vec<u8> {
    let body = b"404 Not Found";
    build_response_bytes("404 Not Found", Some("text/plain; charset=utf-8"), body)
}

fn build_500() -> Vec<u8> {
    let body = b"500 Internal Server Error";
    build_response_bytes(
        "500 Internal Server Error",
        Some("text/plain; charset=utf-8"),
        body,
    )
}

pub async fn serve_static(
    stream: &mut TcpStream,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
) -> anyhow::Result<()> {
    let resp = serve_static_bytes(server_cfg, location, req_path).await?;
    stream.write_all(&resp).await?;
    Ok(())
}

pub async fn serve_static_bytes(
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
) -> anyhow::Result<Vec<u8>> {
    let root = location.root.as_deref().unwrap_or(&server_cfg.root);
    let index = location.index.as_deref().unwrap_or(&server_cfg.index);

    // Resolver path relativo dentro de `root`
    let rel = resolve_relative_path(req_path, &location.path, index);

    // Si no matchea realmente esa location → 404
    let Some(rel) = rel else {
        return Ok(build_404());
    };

    let file_path = format!("{}/{}", root, rel);

    match fs::read(&file_path).await {
        Ok(body) => {
            let mime = mime_guess::from_path(&file_path).first_or_octet_stream();

            let content_type = if mime.type_() == mime::TEXT {
                format!("{}; charset=utf-8", mime.essence_str())
            } else {
                mime.essence_str().to_string()
            };

            Ok(build_response_bytes("200 OK", Some(&content_type), &body))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(build_404()),
        Err(_) => Ok(build_500()),
    }
}

/// Resuelve la ruta relativa al root, teniendo en cuenta el index y el prefijo.
fn resolve_relative_path(req_path: &str, location_path: &str, index: &str) -> Option<String> {
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
