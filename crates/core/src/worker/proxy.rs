use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};

use dashmap::DashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use migux_config::{LocationConfig, MiguxConfig, UpstreamConfig, UpstreamServers};

use crate::ServerRuntime;

use super::responses::send_404;

/// Mapa global: nombre de upstream -> contador para round robin
static UPSTREAM_COUNTERS: OnceLock<DashMap<String, AtomicUsize>> = OnceLock::new();

fn upstream_counters() -> &'static DashMap<String, AtomicUsize> {
    UPSTREAM_COUNTERS.get_or_init(|| DashMap::new())
}

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

/// Elige una dirección concreta "host:port" para un upstream dado.
/// Soporta:
/// - server = "127.0.0.1:3000"
/// - server = ["127.0.0.1:3000", "127.0.0.1:3001"] + strategy = "round_robin"
/// Parsea posibles formatos de server:
/// - "127.0.0.1:3000"
/// - "[\"127.0.0.1:3000\", \"127.0.0.1:3001\"]"
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    // Formato tipo ["127.0.0.1:3000", "127.0.0.1:3001"]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1]; // sin [ ]
        inner
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                // quitar comillas si las hay
                let part = part.trim_matches('"');
                if part.is_empty() {
                    None
                } else {
                    Some(part.to_string())
                }
            })
            .collect()
    } else {
        // formato simple "127.0.0.1:3000"
        vec![trimmed.to_string()]
    }
}

/// Elige una dirección concreta "host:port" para un upstream dado.
/// Soporta:
/// - server = "127.0.0.1:3000"
/// - server = ["127.0.0.1:3000", "127.0.0.1:3001"] (aunque venga como String)
fn choose_upstream_addr(
    upstream_name: &str,
    upstream_cfg: &UpstreamConfig,
) -> anyhow::Result<String> {
    // Normalizamos a Vec<String>
    let servers: Vec<String> = match &upstream_cfg.server {
        UpstreamServers::One(s) => parse_servers_from_one(s),
        UpstreamServers::Many(list) => list.clone(),
    };

    if servers.is_empty() {
        anyhow::bail!(
            "Upstream '{}' no tiene servidores configurados",
            upstream_name
        );
    }

    // Si solo hay uno o la estrategia no es round_robin → siempre el primero
    let strategy = upstream_cfg.strategy.as_deref().unwrap_or("single");
    if servers.len() == 1 || strategy != "round_robin" {
        return Ok(servers[0].clone());
    }

    // Round robin real
    let counters = upstream_counters();
    let entry = counters
        .entry(upstream_name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));

    let idx = entry.fetch_add(1, Ordering::Relaxed);
    let addr = servers[idx % servers.len()].clone();

    Ok(addr)
}

/// Lógica de proxy:
/// - resuelve upstream y estrategia (round_robin o single)
/// - aplica strip_prefix sobre el path
/// - reescribe la primera línea del request (METHOD PATH HTTP/x.y)
/// - reenvía headers + body tal cual al upstream
/// - hace de "túnel" de la respuesta de upstream al cliente
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
    // 0) Resolver upstream desde config
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location sin campo 'upstream'"))?;

    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' no encontrado", upstream_name))?;

    // 🔁 Elegimos la dirección con round robin (o single)
    let upstream_addr = choose_upstream_addr(upstream_name, upstream_cfg)?;

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

    let rest_of_headers: String = lines.map(|l| format!("{l}\r\n")).collect();

    // Reconstruimos request para upstream
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n"); // fin cabeceras
    out.extend_from_slice(req_body); // body tal cual

    println!(
        "[proxy] {} {} → upstream '{}' ({}) path: {}",
        method, req_path, upstream_name, upstream_addr, upstream_path
    );

    // 3) Conectar a upstream
    let mut upstream_stream = TcpStream::connect(upstream_addr).await?;

    // 4) Enviar request al upstream
    upstream_stream.write_all(&out).await?;

    // 5) Túnel de respuesta
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
