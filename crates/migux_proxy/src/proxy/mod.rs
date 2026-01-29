use std::{net::SocketAddr, sync::atomic::AtomicUsize, sync::Arc};

use bytes::{Buf, BytesMut};
use dashmap::DashMap;
use migux_config::{LocationConfig, MiguxConfig};
use migux_http::responses::send_502;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{timeout, Duration},
};
use tracing::{debug, error, info, instrument};

mod headers;
mod path;
mod response;
mod upstream;

/// =======================================================
/// PROXY STATE
/// =======================================================
///
/// Este struct contiene TODO el estado mutable del proxy.
/// Es decir: cosas que deben sobrevivir entre requests
/// y ser seguras en concurrencia (varios clientes a la vez).
///
/// En nginx real esto viviria en memoria compartida
/// entre workers; en Migux vive dentro de un `Arc<Proxy>`.
pub struct Proxy {
    /// Round-robin counters por upstream
    rr_counters: DashMap<String, AtomicUsize>,

    /// Connection pools por upstream address
    pools: DashMap<String, Vec<PooledStream>>,
}

pub(super) struct PooledStream {
    stream: TcpStream,
    read_buf: BytesMut,
}

impl PooledStream {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: BytesMut::new(),
        }
    }
}

impl Proxy {
    /// Crea una nueva instancia del proxy
    ///
    /// Normalmente se envuelve en:
    ///   Arc<Proxy>
    ///
    /// Y se comparte entre todos los workers / handlers.
    pub fn new() -> Self {
        Self {
            rr_counters: DashMap::new(),
            pools: DashMap::new(),
        }
    }

    /// Takes an upstream connection from the pool or creates a new one.
    ///
    /// Flujo:
    /// - Intenta entry.pop() (LIFO) del pool para ese addr
    /// - Si hay, (deberia) reutilizarlo
    /// - Si no hay, conecta nuevo
    #[instrument(skip(self))]
    async fn checkout_upstream_stream(
        &self,
        addr: &str,
        connect_timeout: Duration,
    ) -> anyhow::Result<PooledStream> {
        // Intenta sacar uno del pool
        if let Some(mut entry) = self.pools.get_mut(addr)
            && let Some(pooled) = entry.pop()
        {
            debug!(target: "migux::proxy", upstream = %addr, "Reusing pooled upstream connection");
            return Ok(pooled);
        }

        info!(target: "migux::proxy", upstream = %addr, "Creating new upstream connection");
        let stream = connect_with_timeout(addr, connect_timeout).await?;
        Ok(PooledStream::new(stream))
    }

    /// Returns an upstream connection back to the pool so it can be reused.
    ///
    /// Solo se llama si el streamer decide que la conexion es reusable.
    fn checkin_upstream_stream(&self, addr: &str, pooled: PooledStream) {
        self.pools
            .entry(addr.to_string())
            .or_insert_with(Vec::new)
            .push(pooled);

        debug!(target: "migux::proxy", upstream = %addr, "Returned upstream connection to pool");
    }

    /// Entry point de una location proxy.
    ///
    /// - resuelve upstream por nombre
    /// - aplica rr order
    /// - hace strip_prefix
    /// - reescribe headers
    /// - forward request
    /// - lee response
    /// - la escribe al cliente
    /// - si reusable, devuelve conexion a pool
    #[instrument(
        skip(self, client_stream, client_buf, location, req_headers, cfg),
        fields(client = %client_addr, location_path = %location.path)
    )]
    pub async fn serve(
        &self,
        client_stream: &mut TcpStream,
        client_buf: &mut BytesMut,
        location: &LocationConfig,
        req_headers: &str,
        method: &str,
        req_path: &str,
        http_version: &str,
        content_length: usize,
        is_chunked: bool,
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
        let candidate_addrs = upstream::choose_upstream_addrs_rr_order(
            &self.rr_counters,
            upstream_name,
            upstream_cfg,
        )?;
        let client_ip = client_addr.ip().to_string();
        let connect_timeout = Duration::from_secs(cfg.http.proxy_connect_timeout_secs);
        let write_timeout = Duration::from_secs(cfg.http.proxy_write_timeout_secs);
        let read_timeout = Duration::from_secs(cfg.http.proxy_read_timeout_secs);
        let client_read_timeout = Duration::from_secs(cfg.http.client_read_timeout_secs);
        let max_resp_headers = cfg.http.max_upstream_response_headers_bytes as usize;
        let max_resp_body = cfg.http.max_upstream_response_body_bytes as usize;

        // 4) strip_prefix para upstream path
        let upstream_path = path::strip_prefix_path(req_path, &location.path);

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
        let keep_alive = http_version != "HTTP/1.0";
        let rest_of_headers = headers::rewrite_proxy_headers(
            req_headers,
            &client_ip,
            keep_alive,
            content_length,
            is_chunked,
        );

        // 7) construir request completa (start line + headers + blank line + body)
        let mut out = Vec::new();
        let start_line = format!("{method} {upstream_path} {http_version}\r\n");
        out.extend_from_slice(start_line.as_bytes());
        out.extend_from_slice(rest_of_headers.as_bytes());
        out.extend_from_slice(b"\r\n");

        let mut last_err: Option<anyhow::Error> = None;

        // 8) intentar cada upstream (primero elegido por rr, luego fallback)
        for upstream_addr in &candidate_addrs {
            // 8.1) sacar del pool o conectar
            let mut upstream_stream =
                match self.checkout_upstream_stream(upstream_addr, connect_timeout).await {
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
            match timeout(write_timeout, upstream_stream.stream.write_all(&out)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!(
                        target: "migux::proxy",
                        upstream_addr = %upstream_addr,
                        error = ?e,
                        "Write failed (likely dead pooled socket). Retrying with fresh connection"
                    );

                    match connect_fresh(upstream_addr, connect_timeout).await {
                        Ok(mut fresh) => {
                            match timeout(write_timeout, fresh.stream.write_all(&out)).await {
                                Ok(Ok(())) => {
                                    upstream_stream = fresh;
                                }
                                Ok(Err(e2)) => {
                                    error!(
                                        target: "migux::proxy",
                                        upstream_addr = %upstream_addr,
                                        error = ?e2,
                                        "Write failed even with fresh connection"
                                    );
                                    last_err = Some(e2.into());
                                    continue;
                                }
                                Err(_) => {
                                    error!(
                                        target: "migux::proxy",
                                        upstream_addr = %upstream_addr,
                                        "Write timed out even with fresh connection"
                                    );
                                    last_err = Some(anyhow::anyhow!(
                                        "Upstream write timeout to {}",
                                        upstream_addr
                                    ));
                                    continue;
                                }
                            }
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
                Err(_) => {
                    error!(
                        target: "migux::proxy",
                        upstream_addr = %upstream_addr,
                        "Write timed out. Retrying with fresh connection"
                    );

                    match connect_fresh(upstream_addr, connect_timeout).await {
                        Ok(mut fresh) => {
                            match timeout(write_timeout, fresh.stream.write_all(&out)).await {
                                Ok(Ok(())) => {
                                    upstream_stream = fresh;
                                }
                                Ok(Err(e2)) => {
                                    error!(
                                        target: "migux::proxy",
                                        upstream_addr = %upstream_addr,
                                        error = ?e2,
                                        "Write failed even with fresh connection"
                                    );
                                    last_err = Some(e2.into());
                                    continue;
                                }
                                Err(_) => {
                                    error!(
                                        target: "migux::proxy",
                                        upstream_addr = %upstream_addr,
                                        "Write timed out even with fresh connection"
                                    );
                                    last_err = Some(anyhow::anyhow!(
                                        "Upstream write timeout to {}",
                                        upstream_addr
                                    ));
                                    continue;
                                }
                            }
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
            }

            // 8.3) stream request body to upstream (if any)
            stream_request_body(
                client_stream,
                client_buf,
                &mut upstream_stream.stream,
                is_chunked,
                content_length,
                client_read_timeout,
                cfg.http.max_request_body_bytes as usize,
            )
            .await?;

            // 8.4) leer respuesta del upstream y streamear al cliente
            let reusable = match response::stream_http_response(
                &mut upstream_stream,
                client_stream,
                method,
                read_timeout,
                max_resp_headers,
                max_resp_body,
            )
            .await
            {
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

            // 8.5) si reusable, devolver socket al pool
            if reusable {
                self.checkin_upstream_stream(upstream_addr, upstream_stream);
            }

            // exito: ya hemos respondido al cliente
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
}

async fn stream_request_body(
    client_stream: &mut TcpStream,
    client_buf: &mut BytesMut,
    upstream_stream: &mut TcpStream,
    is_chunked: bool,
    content_length: usize,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()> {
    if is_chunked {
        stream_chunked_body(
            client_stream,
            client_buf,
            upstream_stream,
            read_timeout,
            max_body,
        )
        .await?;
        return Ok(());
    }

    if content_length == 0 {
        return Ok(());
    }

    if max_body > 0 && content_length > max_body {
        anyhow::bail!("Client request body too large");
    }

    stream_exact(
        client_stream,
        client_buf,
        upstream_stream,
        content_length,
        read_timeout,
    )
    .await
}

async fn stream_chunked_body(
    client_stream: &mut TcpStream,
    client_buf: &mut BytesMut,
    upstream_stream: &mut TcpStream,
    read_timeout: Duration,
    max_body: usize,
) -> anyhow::Result<()> {
    let mut body_bytes = 0usize;

    loop {
        let line = read_line_bytes(client_stream, client_buf, read_timeout).await?;
        upstream_stream.write_all(&line).await?;

        let line_str = String::from_utf8_lossy(&line);
        let size_str = line_str.trim().trim_end_matches('\r').trim_end_matches('\n');
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_str, 16)
            .map_err(|_| anyhow::anyhow!("Invalid chunk size"))?;

        if chunk_size == 0 {
            loop {
                let trailer = read_line_bytes(client_stream, client_buf, read_timeout).await?;
                upstream_stream.write_all(&trailer).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        if max_body > 0 && body_bytes + chunk_size > max_body {
            anyhow::bail!("Client request body too large");
        }

        stream_exact(
            client_stream,
            client_buf,
            upstream_stream,
            chunk_size + 2,
            read_timeout,
        )
        .await?;

        body_bytes += chunk_size;
    }
}

async fn read_line_bytes(
    client_stream: &mut TcpStream,
    client_buf: &mut BytesMut,
    read_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    loop {
        if let Some(end) = find_crlf(client_buf, 0) {
            let line = client_buf.split_to(end + 2);
            return Ok(line.to_vec());
        }
        read_more_client(client_stream, client_buf, read_timeout).await?;
    }
}

async fn stream_exact(
    client_stream: &mut TcpStream,
    client_buf: &mut BytesMut,
    upstream_stream: &mut TcpStream,
    mut remaining: usize,
    read_timeout: Duration,
) -> anyhow::Result<()> {
    while remaining > 0 {
        if !client_buf.is_empty() {
            let take = remaining.min(client_buf.len());
            upstream_stream.write_all(&client_buf[..take]).await?;
            client_buf.advance(take);
            remaining -= take;
            continue;
        }

        let mut tmp = [0u8; 4096];
        let n = match timeout(read_timeout, client_stream.read(&mut tmp)).await {
            Ok(res) => res?,
            Err(_) => anyhow::bail!("Client read timeout"),
        };
        if n == 0 {
            anyhow::bail!("Client closed connection while streaming body");
        }

        if n > remaining {
            upstream_stream.write_all(&tmp[..remaining]).await?;
            client_buf.extend_from_slice(&tmp[remaining..n]);
            remaining = 0;
        } else {
            upstream_stream.write_all(&tmp[..n]).await?;
            remaining -= n;
        }
    }
    Ok(())
}

async fn read_more_client(
    client_stream: &mut TcpStream,
    client_buf: &mut BytesMut,
    read_timeout: Duration,
) -> anyhow::Result<()> {
    let mut tmp = [0u8; 4096];
    let n = match timeout(read_timeout, client_stream.read(&mut tmp)).await {
        Ok(res) => res?,
        Err(_) => anyhow::bail!("Client read timeout"),
    };
    if n == 0 {
        anyhow::bail!("Client closed connection");
    }
    client_buf.extend_from_slice(&tmp[..n]);
    Ok(())
}

fn find_crlf(buf: &BytesMut, start: usize) -> Option<usize> {
    buf[start..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}

impl Default for Proxy {
    fn default() -> Self {
        Self::new()
    }
}

/// Crea una conexion nueva (fresh) a un upstream.
/// Se usa como fallback si una conexion reutilizada estaba muerta.
async fn connect_fresh(addr: &str, timeout_dur: Duration) -> anyhow::Result<PooledStream> {
    let stream = connect_with_timeout(addr, timeout_dur).await?;
    Ok(PooledStream::new(stream))
}

async fn connect_with_timeout(addr: &str, timeout_dur: Duration) -> anyhow::Result<TcpStream> {
    match timeout(timeout_dur, TcpStream::connect(addr)).await {
        Ok(res) => Ok(res?),
        Err(_) => anyhow::bail!("Upstream connect timeout to {}", addr),
    }
}
