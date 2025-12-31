use std::{
    net::SocketAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

pub mod proxy;

use dashmap::DashMap;
use migux_config::{LocationConfig, MiguxConfig, UpstreamConfig, UpstreamServers};
use migux_http::responses::{send_404, send_502};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, error, info, instrument, warn};

/// =======================================================
/// PROXY / UPSTREAM STATE (GLOBAL)
/// =======================================================
///
/// Aquí guardas estado global para:
/// 1) round-robin counters por upstream name
/// 2) pools de conexiones persistentes por "host:port"
///
/// OJO: esto vive globalmente en el proceso.
/// - OnceLock asegura init perezosa (lazy) 1 vez
/// - DashMap permite concurrencia sin bloquear todo el mapa
/// Global map: upstream name -> counter for round-robin
///
/// Ej: upstream "app" -> AtomicUsize (0,1,2,3...)
/// Con esto eliges el índice inicial del round robin.
///
/// AtomicUsize:
/// - fetch_add incrementa sin locks
/// - Relaxed está bien para “contador best effort” (no sincronización estricta)
static UPSTREAM_COUNTERS: OnceLock<DashMap<String, AtomicUsize>> = OnceLock::new();

/// Global pool of persistent connections per upstream ("host:port")
///
/// Key: "127.0.0.1:3000"
/// Value: Vec<TcpStream> (stack LIFO de sockets reutilizables)
///
/// ⚠️ MEJORA: Vec<TcpStream> no es ideal:
/// - no hay límite de tamaño
/// - no hay health-check (puede devolverte sockets muertos)
/// - no hay TTL / idle timeout
/// - LIFO puede ser ok, pero no controlas “fairness”
///
/// Aun así, para un primer proxy está bien.
static UPSTREAM_POOLS: OnceLock<DashMap<String, Vec<TcpStream>>> = OnceLock::new();

/// Helper para obtener el mapa de contadores inicializado
fn upstream_counters() -> &'static DashMap<String, AtomicUsize> {
    UPSTREAM_COUNTERS.get_or_init(|| DashMap::new())
}

/// Helper para obtener el mapa de pools inicializado
fn upstream_pools() -> &'static DashMap<String, Vec<TcpStream>> {
    UPSTREAM_POOLS.get_or_init(|| DashMap::new())
}

/// Crea una conexión nueva (fresh) a un upstream.
/// Se usa como “fallback” si una conexión reutilizada estaba muerta.
async fn connect_fresh(addr: &str) -> anyhow::Result<TcpStream> {
    Ok(TcpStream::connect(addr).await?)
}

/// =======================================================
/// URL REWRITE: strip_prefix tipo nginx
/// =======================================================
///
/// Esto implementa la idea típica:
/// location /api/ { proxy_pass http://app; }  ->  /api/users -> /users
///
/// - Si req_path NO empieza por location_path => no toca nada
/// - Si el “tail” queda vacío => "/"
/// - Asegura que empiece por '/'
pub fn strip_prefix_path(req_path: &str, location_path: &str) -> String {
    if !req_path.starts_with(location_path) {
        return req_path.to_string();
    }

    // parte restante después del prefijo
    let mut tail = req_path[location_path.len()..].to_string();

    // exact match: "/api" -> "/"
    if tail.is_empty() {
        return "/".to_string();
    }

    // si quedó "users" en lugar de "/users"
    if !tail.starts_with('/') {
        tail.insert(0, '/');
    }

    tail
}

/// =======================================================
/// UPSTREAM CONFIG PARSING / NORMALIZATION
/// =======================================================

/// Parse possible `server` formats:
///
/// Soporta:
/// - "127.0.0.1:3000"
/// - ["127.0.0.1:3000", "127.0.0.1:3001"]
///
/// (Esto parece compat con un “raw string” que a veces te llega con [] en texto)
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    // Caso "array" como texto: ["a","b"]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .split(',')
            .filter_map(|part| {
                // limpia comillas y espacios
                let part = part.trim().trim_matches('"');
                if part.is_empty() {
                    None
                } else {
                    Some(part.to_string())
                }
            })
            .collect()
    } else {
        // Caso normal: un solo server
        vec![trimmed.to_string()]
    }
}

/// Normalize `UpstreamConfig` into a Vec<String> of "host:port"
///
/// Tu config permite:
/// - UpstreamServers::One("...")
/// - UpstreamServers::Many(vec![...])
///
/// Esta función deja SIEMPRE un Vec<String> usable.
/// Si queda vacío, error.
fn normalize_servers(cfg: &UpstreamConfig) -> anyhow::Result<Vec<String>> {
    let servers: Vec<String> = match &cfg.server {
        UpstreamServers::One(s) => parse_servers_from_one(s),
        UpstreamServers::Many(list) => list.clone(),
    };

    if servers.is_empty() {
        anyhow::bail!("Upstream has no configured servers");
    }

    Ok(servers)
}

/// Returns upstream servers in rr order + fallback order
///
/// Objetivo:
/// - Si estrategia != round_robin => devuelve tal cual
/// - Si round_robin => rota la lista para que el primero sea el “elegido”
///
/// Resultado: Vec ordenado:
///   [primero_elegido, segundo, tercero, ...]
/// Y así en el `for` intentas el primero y si falla vas probando el resto (fallback).
fn choose_upstream_addrs_rr_order(
    upstream_name: &str,
    upstream_cfg: &UpstreamConfig,
) -> anyhow::Result<Vec<String>> {
    let servers = normalize_servers(upstream_cfg)?;

    // Si solo hay uno, no hay nada que elegir
    if servers.len() == 1 {
        return Ok(servers);
    }

    // Por defecto: "single" si falta
    let strategy = upstream_cfg.strategy.as_deref().unwrap_or("single");
    if strategy != "round_robin" {
        return Ok(servers);
    }

    // Counter global por upstream_name
    let counters = upstream_counters();

    // Inserta contador si no existe
    let entry = counters
        .entry(upstream_name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));

    // idx: 0,1,2,3...
    let idx = entry.fetch_add(1, Ordering::Relaxed);
    // start: idx mod N => posición de inicio para rotar
    let start = idx % servers.len();

    // Construye lista rotada:
    // start..end, luego 0..start
    let mut ordered = Vec::with_capacity(servers.len());
    for i in 0..servers.len() {
        let pos = (start + i) % servers.len();
        ordered.push(servers[pos].clone());
    }

    Ok(ordered)
}

/// =======================================================
/// HEADER REWRITE (proxy semantics)
/// =======================================================
///
/// Reglas que aplicas:
/// - Quitas X-Forwarded-* previos (para no duplicar/corromper)
/// - Quitas hop-by-hop headers (por especificación HTTP proxy)
/// - Guardas Host original para meterlo en X-Forwarded-Host
/// - Añades:
///   - X-Forwarded-For
///   - X-Real-IP
///   - X-Forwarded-Proto
///   - X-Forwarded-Host (si había Host)
///
/// Y además:
/// - Fuerzas `Connection: close` hacia upstream.
///   Esto simplifica MUCHO al principio porque:
///   - no tienes que manejar chunked bien
///   - no tienes que manejar pipeline/keepalive real
///
/// ⚠️ OJO: ahora mismo también tienes un pool de conexiones.
/// Forzar Connection: close reduce la utilidad del pool.
/// (aunque tu `read_http_response` puede decidir "reusable").
///
fn rewrite_proxy_headers(req_headers: &str, client_ip: &str) -> String {
    let mut lines = req_headers.lines();
    let _ = lines.next(); // request line (GET /... HTTP/1.1)

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut host_value: Option<String> = None;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some((name, value)) = line.split_once(':') {
            let name_trim = name.trim().to_string();
            let value_trim = value.trim().to_string();

            // Captura Host original
            if name_trim.eq_ignore_ascii_case("host") {
                host_value = Some(value_trim.clone());
            }

            // Drop previous forwarded headers
            if name_trim.eq_ignore_ascii_case("x-forwarded-for")
                || name_trim.eq_ignore_ascii_case("x-real-ip")
                || name_trim.eq_ignore_ascii_case("x-forwarded-proto")
                || name_trim.eq_ignore_ascii_case("x-forwarded-host")
            {
                continue;
            }

            // Drop hop-by-hop headers
            // (no deben ser reenviados por un proxy)
            if name_trim.eq_ignore_ascii_case("connection")
                || name_trim.eq_ignore_ascii_case("keep-alive")
                || name_trim.eq_ignore_ascii_case("proxy-connection")
                || name_trim.eq_ignore_ascii_case("te")
                || name_trim.eq_ignore_ascii_case("trailer")
                || name_trim.eq_ignore_ascii_case("transfer-encoding")
                || name_trim.eq_ignore_ascii_case("upgrade")
            {
                continue;
            }

            headers.push((name_trim, value_trim));
        }
    }

    // Add forward headers
    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
    headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    headers.push(("X-Forwarded-Proto".to_string(), "http".to_string()));

    if let Some(h) = host_value {
        headers.push(("X-Forwarded-Host".to_string(), h));
    }

    // Forzar cierre hacia upstream
    headers.push(("Connection".to_string(), "close".to_string()));

    // Serializa headers con CRLF
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push_str("\r\n");
    }

    out
}

/// =======================================================
/// CONNECTION POOL (checkout / checkin)
/// =======================================================

/// Takes an upstream connection from the pool or creates a new one.
///
/// Flujo:
/// - Intenta entry.pop() (LIFO) del pool para ese addr
/// - Si hay, (debería) reutilizarlo
/// - Si no hay, conecta nuevo
///
/// ⚠️ BUG actual:
/// En tu código, si encuentra uno en el pool lo "pop"pea en `_stream`
/// pero NO lo devuelve, y luego igualmente crea uno nuevo.
/// Es decir: ahora mismo *SIEMPRE conectas nuevo*.
/// (Además: el socket que sacas del pool se pierde -> leak lógico)
///
/// Te lo marco aquí para arreglarlo luego.
#[instrument(skip())]
async fn checkout_upstream_stream(addr: &str) -> anyhow::Result<TcpStream> {
    let pools = upstream_pools();

    // Intenta sacar uno del pool
    if let Some(mut entry) = pools.get_mut(addr)
        && let Some(_stream) = entry.pop()
    {
        debug!(target: "migux::proxy", upstream = %addr, "Reusing pooled upstream connection");
        // TODO: devolver `_stream` aquí:
        // return Ok(_stream);
    }

    info!(target: "migux::proxy", upstream = %addr, "Creating new upstream connection");
    Ok(TcpStream::connect(addr).await?)
}

/// Returns an upstream connection back to the pool so it can be reused.
///
/// Solo se llama si `read_http_response` decide que la conexión es reusable.
fn checkin_upstream_stream(addr: &str, stream: TcpStream) {
    let pools = upstream_pools();
    pools
        .entry(addr.to_string())
        .or_insert_with(Vec::new)
        .push(stream);

    debug!(target: "migux::proxy", upstream = %addr, "Returned upstream connection to pool");
}

/// =======================================================
/// HTTP RESPONSE READER (muy importante)
/// =======================================================
///
/// Lee una respuesta HTTP desde upstream:
/// 1) Lee hasta encontrar "\r\n\r\n" (fin de headers)
/// 2) Parse:
///    - Content-Length
///    - Connection: close / keep-alive
///    - HTTP/1.0 o 1.1
///    - Transfer-Encoding: chunked
/// 3) Decide cómo leer el body:
///    - chunked: lee hasta EOF (y marca NO reusable)
///    - content-length: lee exactamente CL bytes (y decide reusable según headers)
///    - sin CL: lee hasta EOF (y NO reusable)
///
/// Devuelve:
/// (response_bytes_completa, reusable_bool)
///
/// ⚠️ Limitaciones:
/// - No parseas chunked (solo “passthrough” leyendo hasta EOF).
/// - No parseas respuestas con trailers.
/// - No manejas pipeline.
/// - No manejas “Connection: close” forzado vs pool.
/// - Si el upstream usa keep-alive y no manda CL ni chunked, esto se complica.
#[instrument(skip(stream))]
async fn read_http_response(stream: &mut TcpStream) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut headers_end: Option<usize> = None;

    // 1) Leer hasta headers end
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                anyhow::bail!("Upstream closed connection without sending a response");
            } else {
                anyhow::bail!("Upstream closed connection while reading headers");
            }
        }
        buf.extend_from_slice(&tmp[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = Some(pos);
            break;
        }

        // Seguridad: limite de headers
        if buf.len() > 64 * 1024 {
            anyhow::bail!("Upstream response headers too large");
        }
    }

    let headers_end = headers_end.unwrap();
    let header_bytes = &buf[..headers_end];
    let header_str = String::from_utf8_lossy(header_bytes);

    // Flags / parsed values
    let mut content_length: Option<usize> = None;
    let mut connection_close = false;
    let mut connection_keep_alive = false;
    let mut is_http10 = false;
    let mut is_chunked = false;

    let mut lines = header_str.lines();

    // 2) Status line: detect HTTP/1.0
    if let Some(status_line) = lines.next() {
        debug!(target: "migux::proxy", status_line = %status_line, "Received upstream status line");
        if status_line.contains("HTTP/1.0") {
            is_http10 = true;
        }
    }

    // 3) Parse headers relevantes
    for line in lines {
        let lower = line.to_ascii_lowercase();

        if let Some(rest) = lower.strip_prefix("content-length:")
            && let Ok(len) = rest.trim().parse::<usize>()
        {
            content_length = Some(len);
        }

        if let Some(rest) = lower.strip_prefix("connection:") {
            let v = rest.trim();
            if v == "close" {
                connection_close = true;
            } else if v.contains("keep-alive") {
                connection_keep_alive = true;
            }
        }

        if let Some(rest) = lower.strip_prefix("transfer-encoding:")
            && rest.trim().contains("chunked")
        {
            is_chunked = true;
        }
    }

    // response_bytes empieza con headers + CRLFCRLF
    let mut response_bytes = Vec::new();
    response_bytes.extend_from_slice(&buf[..headers_end + 4]);

    // bytes de body que ya llegaron en el primer buffer
    let already_body = buf.len().saturating_sub(headers_end + 4);

    // Caso chunked (sin parser): read-to-EOF y no reusable
    if is_chunked {
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

        debug!(
            target: "migux::proxy",
            "Chunked response detected; returning non-reusable connection (chunk parser not implemented)"
        );
        return Ok((response_bytes, false));
    }

    // Caso Content-Length: leer exactamente CL bytes (o lo mejor posible)
    if let Some(cl) = content_length {
        let mut body = Vec::with_capacity(cl);

        // Copia lo que ya había
        if already_body > 0 {
            let initial = &buf[headers_end + 4..];
            let to_take = initial.len().min(cl);
            body.extend_from_slice(&initial[..to_take]);
        }

        // Sigue leyendo hasta completar CL
        while body.len() < cl {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                warn!(
                    target: "migux::proxy",
                    expected = cl,
                    got = body.len(),
                    "Upstream closed before full body was read"
                );
                break;
            }
            let remaining = cl - body.len();
            let take = remaining.min(n);
            body.extend_from_slice(&tmp[..take]);
        }

        response_bytes.extend_from_slice(&body);

        // Reutilización:
        // - HTTP/1.0: solo reusable si Connection: keep-alive explícito
        // - HTTP/1.1: reusable si no hay Connection: close
        let reusable = if is_http10 {
            connection_keep_alive && !connection_close
        } else {
            !connection_close
        };

        debug!(
            target: "migux::proxy",
            content_length = cl,
            reusable,
            http10 = is_http10,
            "Finished reading upstream response with Content-Length"
        );

        Ok((response_bytes, reusable))
    } else {
        // Sin Content-Length (y no chunked): read-to-EOF y NO reusable
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

        debug!(
            target: "migux::proxy",
            "No Content-Length; read until EOF; connection not reusable"
        );
        Ok((response_bytes, false))
    }
}

/// =======================================================
/// MAIN PROXY HANDLER
/// =======================================================
///
/// Este es el “entrypoint” de una location proxy:
/// - resuelve upstream por nombre
/// - aplica rr order
/// - hace strip_prefix
/// - reescribe headers
/// - forward request
/// - lee response
/// - la escribe al cliente
/// - si reusable, devuelve conexión a pool
#[instrument(
    skip(client_stream, location, req_headers, req_body, cfg),
    fields(client = %client_addr, location_path = %location.path)
)]
pub async fn serve_proxy(
    client_stream: &mut TcpStream,
    location: &LocationConfig,
    req_path: &str,
    req_headers: &str,
    req_body: &[u8],
    cfg: &Arc<MiguxConfig>,
    client_addr: &SocketAddr,
) -> anyhow::Result<()> {
    // 1) localizar el upstream de la location
    let upstream_name = location
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Proxy location missing 'upstream' field"))?;

    // 2) encontrar su config
    let upstream_cfg = cfg
        .upstream
        .get(upstream_name)
        .ok_or_else(|| anyhow::anyhow!("Upstream '{}' not found in config", upstream_name))?;

    // 3) obtener candidatos en orden rr (y fallback)
    let candidate_addrs = choose_upstream_addrs_rr_order(upstream_name, upstream_cfg)?;
    let client_ip = client_addr.ip().to_string();

    // 4) strip_prefix para upstream path
    let upstream_path = strip_prefix_path(req_path, &location.path);

    // 5) parse request line del cliente
    let mut lines = req_headers.lines();
    let first_line = match lines.next() {
        Some(l) => l,
        None => {
            warn!(target: "migux::proxy", "Missing request line; returning 404");
            send_404(client_stream).await?;
            return Ok(());
        }
    };

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let _old_path = parts.next().unwrap_or("/");
    let http_version = parts.next().unwrap_or("HTTP/1.1");

    debug!(
        target: "migux::proxy",
        %method,
        original_path = %req_path,
        upstream_path = %upstream_path,
        http_version = %http_version,
        upstream = %upstream_name,
        "Preparing proxied request"
    );

    // 6) reescribir headers para upstream
    let rest_of_headers = rewrite_proxy_headers(req_headers, &client_ip);

    // 7) construir request completa (start line + headers + blank line + body)
    let mut out = Vec::new();
    let start_line = format!("{method} {upstream_path} {http_version}\r\n");
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(rest_of_headers.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(req_body);

    let mut last_err: Option<anyhow::Error> = None;

    // 8) intentar cada upstream (primero elegido por rr, luego fallback)
    for upstream_addr in &candidate_addrs {
        // 8.1) sacar del pool o conectar
        let mut upstream_stream = match checkout_upstream_stream(upstream_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!(target: "migux::proxy", upstream=%upstream_addr, error=?e, "Failed to get upstream connection");
                last_err = Some(e);
                continue;
            }
        };

        info!(
            target: "migux::proxy",
            method = %method,
            original_path = %req_path,
            upstream = %upstream_name,
            upstream_addr = %upstream_addr,
            upstream_path = %upstream_path,
            "Forwarding request to upstream"
        );

        // 8.2) write request
        //
        // Si falla, asumes que es un socket reutilizado muerto.
        // Intentas UNA vez reconectar fresh al mismo upstream_addr.
        if let Err(e) = upstream_stream.write_all(&out).await {
            error!(
                target: "migux::proxy",
                upstream_addr = %upstream_addr,
                error = ?e,
                "Write failed (likely dead pooled socket). Retrying with fresh connection"
            );
            last_err = Some(e.into());

            match connect_fresh(upstream_addr).await {
                Ok(mut fresh) => {
                    if let Err(e2) = fresh.write_all(&out).await {
                        error!(
                            target: "migux::proxy",
                            upstream_addr = %upstream_addr,
                            error = ?e2,
                            "Write failed even with fresh connection"
                        );
                        last_err = Some(e2.into());
                        continue;
                    }
                    upstream_stream = fresh;
                }
                Err(e2) => {
                    error!(
                        target: "migux::proxy",
                        upstream_addr = %upstream_addr,
                        error = ?e2,
                        "Failed to connect fresh after pooled write failure"
                    );
                    last_err = Some(e2);
                    continue;
                }
            }
        }

        // 8.3) leer respuesta completa del upstream + decidir reusabilidad
        let (resp_bytes, reusable) = match read_http_response(&mut upstream_stream).await {
            Ok(r) => r,
            Err(e) => {
                error!(
                    target: "migux::proxy",
                    upstream_addr = %upstream_addr,
                    error = ?e,
                    "Error reading response from upstream"
                );
                last_err = Some(e);
                continue;
            }
        };

        // 8.4) devolver respuesta al cliente
        if let Err(e) = client_stream.write_all(&resp_bytes).await {
            error!(target: "migux::proxy", error=?e, "Error writing proxied response back to client");
            last_err = Some(e.into());
            continue;
        }

        if let Err(e) = client_stream.flush().await {
            warn!(target: "migux::proxy", error=?e, "Error flushing response to client");
        }

        // 8.5) si reusable, devolver socket al pool
        if reusable {
            checkin_upstream_stream(upstream_addr, upstream_stream);
        }

        // éxito: ya hemos respondido al cliente
        return Ok(());
    }

    // 9) si todos fallan => 502
    error!(
        target: "migux::proxy",
        upstream = %upstream_name,
        error = ?last_err,
        "All upstreams failed; returning 502"
    );
    send_502(client_stream).await?;
    Ok(())
}
