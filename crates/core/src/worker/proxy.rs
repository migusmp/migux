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
    req_raw: &str,
    cfg: &Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    // 1) strip_prefix: quitamos location.path del path de la request
    let upstream_path = strip_prefix_path(req_path, &location.path);
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location sin campo 'upstream'"))?;

    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' no encontrado", upstream_name))?;

    let upstream_addr = &upstream_cfg.server; // ej: "127.0.0.1:3000"

    // 2) Reescribir primera línea: "METHOD /algo HTTP/1.1"
    let mut lines = req_raw.lines();

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

    let rest_of_request: String = lines.map(|l| format!("{l}\r\n")).collect();

    let new_request = format!("{method} {upstream_path} {http_version}\r\n{rest_of_request}\r\n");

    println!(
        "[proxy] {} {} → upstream path: {}",
        method, req_path, upstream_path
    );

    println!("[proxy] upstream '{}' → {}", upstream_name, upstream_addr);

    // 3) Conectar a upstream (por ahora hardcodeado)
    let mut upstream_stream = TcpStream::connect(upstream_addr).await?;
    // Más adelante: usar _cfg para coger upstream.app de la config

    // 4) Enviar request reescrita al upstream
    upstream_stream.write_all(new_request.as_bytes()).await?;

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
