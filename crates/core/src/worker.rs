use std::sync::Arc;

use tokio::{io::AsyncReadExt, net::TcpStream};

use migux_config::{LocationType, MiguxConfig};

use crate::ServerRuntime;

// Submódulos del worker
mod proxy;
mod responses;
mod routing;
mod static_files;

use proxy::serve_proxy;
use responses::send_404;
use routing::{match_location, parse_request_line, select_default_server};
use static_files::serve_static;

/// Punto de entrada del "worker lógico" por conexión.
pub async fn handle_connection(
    mut stream: TcpStream,
    servers: Arc<Vec<ServerRuntime>>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    // 1) Leemos la request completa (cabeceras + body)
    let (req_headers, req_body) = read_http_request(&mut stream).await?;

    if req_headers.is_empty() {
        // conexión vacía
        return Ok(());
    }

    // 2) Parsear línea de request
    let (method, path) = parse_request_line(&req_headers);

    println!("[worker] method = {}, path = {}", method, path);

    // 3) Seleccionar server
    let server = select_default_server(&servers);
    println!(
        "[worker] using server '{}' (root = {}, index = {})",
        server.name, server.config.root, server.config.index
    );

    if server.locations.is_empty() {
        send_404(&mut stream).await?;
        return Ok(());
    }

    // 4) Seleccionar location
    let location = match_location(&server.locations, path);
    println!(
        "[worker] matched location: server={}, path={}, type={:?}",
        location.server, location.path, location.r#type
    );

    // 5) Despachar según tipo de location
    match location.r#type {
        LocationType::Static => {
            // Para estáticos, solo permitimos GET y HEAD
            if method != "GET" && method != "HEAD" {
                send_404(&mut stream).await?;
                return Ok(());
            }

            serve_static(&mut stream, server, location, path).await?;
        }
        LocationType::Proxy => {
            // Proxy: aceptamos cualquier método y reenviamos headers + body
            serve_proxy(
                &mut stream,
                server,
                location,
                path,
                &req_headers,
                &req_body,
                &cfg,
            )
            .await?;
        }
    }

    Ok(())
}

/// Lee una petición HTTP completa:
/// - lee hasta encontrar `\r\n\r\n` (fin de cabeceras)
/// - busca `Content-Length`
/// - si existe, lee el body completo
/// - devuelve (cabeceras como String, body como Vec<u8>)
async fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];

    let headers_end;

    // 1) Leer hasta encontrar "\r\n\r\n"
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            // conexión cerrada sin nada
            if buf.is_empty() {
                return Ok((String::new(), Vec::new()));
            } else {
                // no hemos encontrado fin de cabeceras, pero hay datos;
                // tratamos todo como cabeceras "raras" sin body
                headers_end = buf.len();
                break;
            }
        }

        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = pos;
            break;
        }

        // si quieres, aquí podrías limitar tamaño de cabeceras para evitar abusos
    }

    let header_bytes = &buf[..headers_end];
    let headers_str = String::from_utf8_lossy(header_bytes).to_string();

    // 2) Buscar Content-Length
    let mut content_length = 0usize;
    for line in headers_str.lines() {
        let line_lower = line.to_ascii_lowercase();
        if let Some(rest) = line_lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = len;
            }
        }
    }

    // 3) Calcular cuánto body ya hemos leído
    let already_read_body = buf.len().saturating_sub(headers_end + 4);
    let mut body = Vec::new();
    if already_read_body > 0 && headers_end + 4 <= buf.len() {
        body.extend_from_slice(&buf[headers_end + 4..]);
    }

    // 4) Leer el resto del body si falta
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Ok((headers_str, body))
}
