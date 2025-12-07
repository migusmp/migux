use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

use dashmap::DashMap;
use migux_http::responses::{send_404, send_502};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use migux_config::{LocationConfig, MiguxConfig, UpstreamConfig, UpstreamServers};

/// Mapa global: nombre de upstream -> contador para round robin
static UPSTREAM_COUNTERS: OnceLock<DashMap<String, AtomicUsize>> = OnceLock::new();

fn upstream_counters() -> &'static DashMap<String, AtomicUsize> {
    UPSTREAM_COUNTERS.get_or_init(|| DashMap::new())
}

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

/// Normaliza UpstreamConfig a un Vec<String> de "host:port"
/// Normaliza UpstreamConfig a un Vec<String> de "host:port"
fn normalize_servers(cfg: &UpstreamConfig) -> anyhow::Result<Vec<String>> {
    let servers: Vec<String> = match &cfg.server {
        // Ojo: tu parser mete el array entero como String aquí
        UpstreamServers::One(s) => parse_servers_from_one(s),
        UpstreamServers::Many(list) => list.clone(),
    };

    if servers.is_empty() {
        anyhow::bail!("Upstream sin servidores configurados");
    }

    Ok(servers)
}

/// Devuelve la lista de servidores en el orden en que deben intentarse:
/// - Si solo hay uno o estrategia != round_robin → [único servidor]
/// - Si round_robin y varios: [actual, siguiente, siguiente, ...] (para fallback)
fn choose_upstream_addrs_rr_order(
    upstream_name: &str,
    upstream_cfg: &UpstreamConfig,
) -> anyhow::Result<Vec<String>> {
    let servers = normalize_servers(upstream_cfg)?;

    if servers.len() == 1 {
        return Ok(servers);
    }

    let strategy = upstream_cfg.strategy.as_deref().unwrap_or("single");
    if strategy != "round_robin" {
        return Ok(servers);
    }

    let counters = upstream_counters();
    let entry = counters
        .entry(upstream_name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));

    let idx = entry.fetch_add(1, Ordering::Relaxed);
    let start = idx % servers.len();

    let mut ordered = Vec::with_capacity(servers.len());
    for i in 0..servers.len() {
        let pos = (start + i) % servers.len();
        ordered.push(servers[pos].clone());
    }

    Ok(ordered)
}

/// Reescribe los headers para proxy:
/// - elimina X-Forwarded-* previos
/// - conserva Host original
/// - añade:
///   * X-Forwarded-For
///   * X-Real-IP
///   * X-Forwarded-Proto
///   * X-Forwarded-Host
fn rewrite_proxy_headers(req_headers: &str, client_ip: &str) -> String {
    // Saltamos la primera línea (request line)
    let mut lines = req_headers.lines();
    let _ = lines.next();

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut host_value: Option<String> = None;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Esperamos formato "Nombre: valor"
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();

            // Guardamos el Host original
            if name.eq_ignore_ascii_case("host") {
                host_value = Some(value.clone());
            }

            // Ignoramos X-Forwarded-* existentes: los regeneramos nosotros
            if name.eq_ignore_ascii_case("x-forwarded-for")
                || name.eq_ignore_ascii_case("x-real-ip")
                || name.eq_ignore_ascii_case("x-forwarded-proto")
                || name.eq_ignore_ascii_case("x-forwarded-host")
            {
                continue;
            }

            headers.push((name, value));
        }
    }

    // Añadimos nuestros X-Forwarded-*
    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
    headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    headers.push(("X-Forwarded-Proto".to_string(), "http".to_string()));

    if let Some(h) = host_value {
        headers.push(("X-Forwarded-Host".to_string(), h));
    }

    // Reconstruimos en formato "Header: valor\r\n"
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }

    out
}

/// Lógica de proxy:
/// - resuelve upstream y estrategia (round_robin o single)
/// - aplica strip_prefix sobre el path
/// - reescribe la primera línea del request (METHOD PATH HTTP/x.y)
/// - reenvía headers + body tal cual al upstream
/// - intenta varios upstreams en orden (fallback) si alguno no conecta
/// - hace de "túnel" de la respuesta de upstream al cliente
pub async fn serve_proxy(
    client_stream: &mut TcpStream,
    location: &LocationConfig,
    req_path: &str,
    req_headers: &str,
    req_body: &[u8],
    cfg: &Arc<MiguxConfig>,
    client_addr: &SocketAddr,
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

    // Lista de candidatos en orden de preferencia (round-robin + fallback)
    let candidate_addrs = choose_upstream_addrs_rr_order(upstream_name, upstream_cfg)?;

    // 👇 IP real del cliente
    let client_ip = client_addr.ip().to_string();

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

    let rest_of_headers = rewrite_proxy_headers(req_headers, &client_ip);

    // Reconstruimos request para upstream
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n"); // fin cabeceras
    out.extend_from_slice(req_body); // body tal cual

    // 3) Intentar conectar a los upstreams en orden (fallback)
    let mut last_err: Option<anyhow::Error> = None;

    for upstream_addr in &candidate_addrs {
        println!(
            "[proxy] {} {} → upstream '{}' ({}) path: {}",
            method, req_path, upstream_name, upstream_addr, upstream_path
        );

        match TcpStream::connect(upstream_addr).await {
            Ok(mut upstream_stream) => {
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
                return Ok(()); // ✅ ÉXITO con este upstream
            }
            Err(e) => {
                eprintln!(
                    "[proxy] error conectando a upstream {}: {:?}",
                    upstream_addr, e
                );
                last_err = Some(e.into());
                // seguimos con el siguiente upstream
            }
        }
    }

    // Si ninguno ha funcionado → 502 Bad Gateway
    eprintln!(
        "[proxy] todos los upstreams de '{}' han fallado: {:?}",
        upstream_name, last_err
    );
    send_502(client_stream).await?;
    Ok(())
}
