use std::sync::Arc;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use migux_config::{LocationConfig, MiguxConfig};

use crate::ServerRuntime;

use super::responses::send_404;

/// Aplica strip_prefix estilo nginx:
/// - Si req_path empieza por location_path, quita el prefijo.
/// - Garantiza que el resultado empieza por "/".
/// - Si queda vacío, devuelve "/".
pub fn strip_prefix_path(req_path: &str, location_path: &str) -> String {
    // Si no matchea, devolvemos req_path tal cual.
    if !req_path.starts_with(location_path) {
        return req_path.to_string();
    }

    // Cola después del prefijo
    let mut tail = req_path[location_path.len()..].to_string();

    // Si la cola está vacía → "/"
    if tail.is_empty() {
        return "/".to_string();
    }

    // Si no empieza por "/", lo insertamos
    if !tail.starts_with('/') {
        tail.insert(0, '/');
    }

    tail
}

/// Lógica de proxy simple:
/// - strip_prefix del path
/// - reescribe primera línea del request
/// - conecta a upstream (por ahora 127.0.0.1:3000)
/// - hace de "túnel" entre cliente y upstream
pub async fn serve_proxy(
    client_stream: &mut TcpStream,
    _server: &ServerRuntime,
    location: &LocationConfig,
    req_path: &str,
    req_headers: &str,
    req_body: &[u8],
    cfg: &Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    // 0) Resolver upstream desde config
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location sin campo 'upstream'"))?;

    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' no encontrado", upstream_name))?;

    let upstream_addr = &upstream_cfg.server;

    // 1) strip_prefix: quitamos location.path del path de la request
    let upstream_path = strip_prefix_path(req_path, &location.path);

    // 2) Reescribir primera línea: "METHOD /algo HTTP/1.1"
    let mut lines = req_headers.lines();

    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            // request rara → 400/404
            send_404(client_stream).await?;
            return Ok(());
        }
    };

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let _old_path = parts.next().unwrap_or("/");
    let http_version = parts.next().unwrap_or("HTTP/1.1");

    // el resto de cabeceras tal cual
    let rest_of_headers: String = lines.map(|l| format!("{l}\r\n")).collect();

    // Reconstruimos cabeceras con ruta ya reescrita
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n"); // fin de cabeceras
    out.extend_from_slice(req_body); // añadimos el body tal cual

    println!(
        "[proxy] {} {} → upstream '{}' ({}) path: {}",
        method, req_path, upstream_name, upstream_addr, upstream_path
    );

    // 3) Conectar a upstream
    let mut upstream_stream = TcpStream::connect(upstream_addr).await?;

    // 4) Enviar request reescrita + body
    upstream_stream.write_all(&out).await?;

    // 5) Leer respuesta del upstream y hacérsela "túnel" al cliente
    let mut buf = [0u8; 4096];

    loop {
        let n = upstream_stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        client_stream.write_all(&buf[..n]).await?;
    }

    client_stream.flush().await?;

    Ok(())
}
