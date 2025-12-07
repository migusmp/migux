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

/// Pool global de conexiones persistentes por upstream ("host:port")
static UPSTREAM_POOLS: OnceLock<DashMap<String, Vec<TcpStream>>> = OnceLock::new();

fn upstream_counters() -> &'static DashMap<String, AtomicUsize> {
    UPSTREAM_COUNTERS.get_or_init(|| DashMap::new())
}

fn upstream_pools() -> &'static DashMap<String, Vec<TcpStream>> {
    UPSTREAM_POOLS.get_or_init(|| DashMap::new())
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

/// Parsea posibles formatos de server:
/// - "127.0.0.1:3000"
/// - "[\"127.0.0.1:3000\", \"127.0.0.1:3001\"]"
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1]; // sin [ ]
        inner
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                let part = part.trim_matches('"');
                if part.is_empty() {
                    None
                } else {
                    Some(part.to_string())
                }
            })
            .collect()
    } else {
        vec![trimmed.to_string()]
    }
}

/// Normaliza UpstreamConfig a un Vec<String> de "host:port"
fn normalize_servers(cfg: &UpstreamConfig) -> anyhow::Result<Vec<String>> {
    let servers: Vec<String> = match &cfg.server {
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

        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();

            if name.eq_ignore_ascii_case("host") {
                host_value = Some(value.clone());
            }

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

    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
    headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    headers.push(("X-Forwarded-Proto".to_string(), "http".to_string()));

    if let Some(h) = host_value {
        headers.push(("X-Forwarded-Host".to_string(), h));
    }

    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }

    out
}

/// Saca una conexión de upstream del pool o crea una nueva
async fn checkout_upstream_stream(addr: &str) -> anyhow::Result<TcpStream> {
    let pools = upstream_pools();

    if let Some(mut entry) = pools.get_mut(addr) {
        if let Some(stream) = entry.pop() {
            println!("[proxy] reusing upstream connection to {}", addr);
            return Ok(stream);
        }
    }

    println!("[proxy] creating new upstream connection to {}", addr);
    let stream = TcpStream::connect(addr).await?;
    Ok(stream)
}

/// Devuelve una conexión sana al pool para poder reutilizarla
fn checkin_upstream_stream(addr: &str, stream: TcpStream) {
    let pools = upstream_pools();
    pools
        .entry(addr.to_string())
        .or_insert_with(Vec::new)
        .push(stream);
}

/// Lee una respuesta HTTP del upstream y decide si se puede reutilizar la conexión.
/// - Lee cabeceras hasta "\r\n\r\n"
/// - Busca Content-Length y Connection
/// - Si hay Content-Length: lee exactamente ese número de bytes y deja la conexión viva (si no es "Connection: close").
/// - Si NO hay Content-Length: lee hasta EOF y NO reutiliza la conexión.
async fn read_http_response(stream: &mut TcpStream) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut headers_end: Option<usize> = None;

    // Leer hasta encontrar fin de cabeceras
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                anyhow::bail!("upstream cerró la conexión sin enviar respuesta");
            } else {
                anyhow::bail!("upstream cerró la conexión mientras se leían cabeceras");
            }
        }
        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = Some(pos);
            break;
        }

        if buf.len() > 64 * 1024 {
            anyhow::bail!("cabeceras de respuesta demasiado grandes");
        }
    }

    let headers_end = headers_end.unwrap();
    let header_bytes = &buf[..headers_end];
    let header_str = String::from_utf8_lossy(header_bytes);

    let mut content_length: Option<usize> = None;
    let mut connection_close = false;

    let mut lines = header_str.lines();

    if let Some(status_line) = lines.next() {
        println!("[proxy] upstream status: {}", status_line);
    }

    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }
        if let Some(rest) = lower.strip_prefix("connection:") {
            if rest.trim() == "close" {
                connection_close = true;
            }
        }
    }

    let mut response_bytes = Vec::new();
    // Cabeceras + separador
    response_bytes.extend_from_slice(&buf[..headers_end + 4]);

    let already_body = buf.len().saturating_sub(headers_end + 4);

    if let Some(cl) = content_length {
        let mut body = Vec::with_capacity(cl);

        if already_body > 0 {
            let initial = &buf[headers_end + 4..];
            let to_take = initial.len().min(cl);
            body.extend_from_slice(&initial[..to_take]);
        }

        while body.len() < cl {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            let remaining = cl - body.len();
            let take = remaining.min(n);
            body.extend_from_slice(&tmp[..take]);
        }

        response_bytes.extend_from_slice(&body);

        let reusable = !connection_close;
        Ok((response_bytes, reusable))
    } else {
        // No hay Content-Length -> leer hasta EOF y no reutilizar
        if already_body > 0 {
            response_bytes.extend_from_slice(&buf[headers_end + 4..]);
        }
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            response_bytes.extend_from_slice(&tmp[..n]);
        }
        Ok((response_bytes, false))
    }
}

/// Lógica de proxy:
/// - resuelve upstream y estrategia (round_robin o single)
/// - aplica strip_prefix sobre el path
/// - reescribe la primera línea del request (METHOD PATH HTTP/x.y)
/// - reescribe headers e inyecta X-Forwarded-*
/// - intenta varios upstreams en orden (fallback)
/// - reutiliza conexiones persistentes (keep-alive) cuando es posible
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

    // IP real del cliente
    let client_ip = client_addr.ip().to_string();

    // 1) strip_prefix: quitamos location.path del path de la request
    let upstream_path = strip_prefix_path(req_path, &location.path);

    // 2) Parsear primera línea de la request original
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

    // 3) Reescribir headers con X-Forwarded-*
    let rest_of_headers = rewrite_proxy_headers(req_headers, &client_ip);

    // Reconstruimos request para upstream
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n"); // fin cabeceras
    out.extend_from_slice(req_body); // body tal cual

    let mut last_err: Option<anyhow::Error> = None;

    // 4) Intentar conectar a los upstreams en orden (fallback + keep-alive)
    for upstream_addr in &candidate_addrs {
        let mut upstream_stream = match checkout_upstream_stream(upstream_addr).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[proxy] error obteniendo conexión a upstream {}: {:?}",
                    upstream_addr, e
                );
                last_err = Some(e);
                continue;
            }
        };

        println!(
            "[proxy] {} {} → upstream '{}' ({}) path: {}",
            method, req_path, upstream_name, upstream_addr, upstream_path
        );

        if let Err(e) = upstream_stream.write_all(&out).await {
            eprintln!(
                "[proxy] error escribiendo a upstream {}: {:?}",
                upstream_addr, e
            );
            last_err = Some(e.into());
            // No devolvemos esta conexión al pool
            continue;
        }

        // Leer respuesta del upstream (1 respuesta HTTP) y decidir si se reusa la conexión
        let (resp_bytes, reusable) = match read_http_response(&mut upstream_stream).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "[proxy] error leyendo respuesta de upstream {}: {:?}",
                    upstream_addr, e
                );
                last_err = Some(e);
                continue;
            }
        };

        if let Err(e) = client_stream.write_all(&resp_bytes).await {
            eprintln!("[proxy] error escribiendo al cliente: {:?}", e);
            last_err = Some(e.into());
            // aunque falle el cliente, la conexión a upstream podría seguir sana,
            // pero para simplificar no la devolvemos al pool si la escritura falla.
            continue;
        }

        if let Err(e) = client_stream.flush().await {
            eprintln!("[proxy] error haciendo flush al cliente: {:?}", e);
            // aun así, consideramos la conexión a upstream usable o no según 'reusable'
        }

        // ✅ ÉXITO: si el upstream quiere keep-alive, devolvemos la conexión al pool
        if reusable {
            checkin_upstream_stream(upstream_addr, upstream_stream);
        }

        return Ok(());
    }

    // Si ninguno ha funcionado → 502 Bad Gateway
    eprintln!(
        "[proxy] todos los upstreams de '{}' han fallado: {:?}",
        upstream_name, last_err
    );
    send_502(client_stream).await?;
    Ok(())
}
