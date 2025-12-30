use migux_http::responses::{send_404, send_500, send_response};
use mime_guess::mime;
use tokio::{fs, net::TcpStream};

use migux_config::{LocationConfig, ServerConfig};

/// Sirve archivos estáticos según server.root/location.root + index.
pub async fn serve_static(
    stream: &mut TcpStream,
    server_cfg: &ServerConfig,
    location: &LocationConfig,
    req_path: &str,
) -> anyhow::Result<()> {
    let root = location.root.as_deref().unwrap_or(&server_cfg.root);
    let index = location.index.as_deref().unwrap_or(&server_cfg.index);

    // Resolver path relativo dentro de `root`
    let rel = resolve_relative_path(req_path, &location.path, index);

    // Si la ruta del archivo estatico no existe, retorna un código 404
    if rel.is_none() {
        // no matchea realmente esa location
        send_404(stream).await?;
        return Ok(());
    }

    let rel = rel.unwrap();
    let file_path = format!("{}/{}", root, rel);
    println!("[worker] static file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            // Guess the MIME type based on the file extension
            // (e.g. .html -> text/html, .css -> text/css, .png -> image/png)
            let mime = mime_guess::from_path(&file_path).first_or_octet_stream();

            // Build the Content-Type header
            // - For text-based files, explicitly add UTF-8 charset
            // - For binary files (images, videos, etc.), do not add charset
            let content_type = if mime.type_() == mime::TEXT {
                format!("{}; charset=utf-8", mime.essence_str())
            } else {
                mime.essence_str().to_string()
            };

            // Send a 200 OK response with the correct Content-Type
            send_response(stream, "200 OK", &content_type, &body).await?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[worker] file not found {}: {:?}", file_path, e);
            send_404(stream).await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            send_500(stream).await?;
        }
    }

    Ok(())
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
        let mut tail = &req_path[location_path.len()..]; // strip prefix

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
