use std::sync::Arc;
use tokio::{fs, io::AsyncWriteExt};

use migux_config::{LocationConfig, LocationType};
use tokio::{io::AsyncReadExt, net::TcpStream};

use crate::ServerRuntime;

pub async fn handle_connection(
    mut stream: TcpStream,
    servers: Arc<Vec<ServerRuntime>>,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;

    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);

    let mut method = "-";
    let mut path = "/";

    if let Some(first_line) = req_str.lines().next() {
        println!("[worker] request line: {}", first_line);
        let mut parts = first_line.split_whitespace();
        method = parts.next().unwrap_or("-");
        path = parts.next().unwrap_or("/");
        println!("[worker] method = {}, path = {}", method, path);
    }

    let server = select_default_server(&servers);
    println!(
        "[worker] using server '{}' (root = {}, index = {})",
        server.name, server.config.root, server.config.index
    );

    if server.locations.is_empty() {
        // por si acaso no hubiera locations (no es tu caso)
        send_404(&mut stream).await?;
        return Ok(());
    }

    let location = match_location(&server.locations, path);
    println!(
        "[worker] matched location: server={}, path={}, type={:?}",
        location.server, location.path, location.r#type
    );

    if method != "GET" {
        // De momento solo GET
        send_404(&mut stream).await?;
        return Ok(());
    }

    match location.r#type {
        LocationType::Static => {
            serve_static(&mut stream, server, location, path).await?;
        }
        LocationType::Proxy => {
            // TODO:
            // - conectar a upstream.app (127.0.0.1:3000)
            // - hacer strip_prefix(location.path) sobre `path`
            // - reenviar request y stream de la respuesta
            send_501(&mut stream).await?;
        }
    }

    Ok(())
}

pub async fn serve_static(
    stream: &mut TcpStream,
    server: &ServerRuntime,
    location: &LocationConfig,
    req_path: &str,
) -> anyhow::Result<()> {
    // root efectiva: location.root o server.root
    let root = location.root.as_deref().unwrap_or(&server.config.root);

    // index efectivo: location.index o server.index
    let index = location.index.as_deref().unwrap_or(&server.config.index);

    // Path relativo dentro del root
    let rel = if req_path == location.path {
        // si piden justo el path de la location → index
        index.to_string()
    } else if req_path.starts_with(&location.path) {
        let mut tail = &req_path[location.path.len()..]; // quitar prefijo de location
        if tail.starts_with('/') {
            tail = &tail[1..];
        }
        if tail.is_empty() {
            index.to_string()
        } else {
            tail.to_string()
        }
    } else if req_path == "/" && location.path == "/" {
        // caso típico: location "/" y petición "/"
        index.to_string()
    } else {
        // no encaja bien → 404
        send_404(stream).await?;
        return Ok(());
    };

    let file_path = format!("{}/{}", root, rel);
    println!("[worker] static file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );

            stream.write_all(response.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            let body = b"Internal Server Error\n";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.flush().await?;
        }
    }

    Ok(())
}

pub async fn send_501(stream: &mut TcpStream) -> anyhow::Result<()> {
    let body = b"501 Not Implemented (proxy TODO)\n";
    let response = format!(
        "HTTP/1.1 501 Not Implemented\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

pub fn select_default_server<'a>(servers: &'a [ServerRuntime]) -> &'a ServerRuntime {
    // De momento, simplemente el primero
    &servers[0]
}

pub fn match_location<'a>(locations: &'a [LocationConfig], path: &str) -> &'a LocationConfig {
    // Elegimos la location cuyo `path` sea prefijo del path de la request
    // y con el `path` más largo (estilo nginx).
    locations
        .iter()
        .filter(|loc| path.starts_with(&loc.path))
        .max_by_key(|loc| loc.path.len())
        .unwrap_or_else(|| {
            // fallback: primera location, si no hay prefijo que encaje
            &locations[0]
        })
}

pub async fn serve_index(
    stream: &mut tokio::net::TcpStream,
    server: &ServerRuntime,
) -> anyhow::Result<()> {
    let file_path = format!("{}/{}", server.config.root, server.config.index);
    println!("[worker] serving file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );

            stream.write_all(response.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            // si falla el archivo, devolvemos 500
            let body = b"Internal Server Error\n";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.flush().await?;
        }
    }

    Ok(())
}
pub async fn send_404(stream: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
    let body = b"404 Not Found\n";
    let response = format!(
        "HTTP/1.1 404 Not Found\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    Ok(())
}
