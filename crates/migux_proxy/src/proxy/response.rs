use tokio::{io::AsyncReadExt, net::TcpStream};
use tracing::{debug, instrument, warn};

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
/// 3) Decide como leer el body:
///    - chunked: lee hasta EOF (y marca NO reusable)
///    - content-length: lee exactamente CL bytes (y decide reusable segun headers)
///    - sin CL: lee hasta EOF (y NO reusable)
///
/// Devuelve:
/// (response_bytes_completa, reusable_bool)
///
/// LIMITACIONES / DEUDA TECNICA (IMPORTANTE)
///
/// 1) Transfer-Encoding: chunked NO implementado (solo "passthrough hasta EOF")
///    - En HTTP/1.1 el upstream suele mantener keep-alive, por lo que "leer hasta EOF"
///      puede bloquearse indefinidamente.
///    - No se parsea el framing de chunks (hex size + CRLF + data + CRLF + 0-chunk).
///    - No se soportan trailers.
///
/// 2) No se soportan trailers de chunked (Trailer / headers finales).
///
/// 3) No se respeta semantica de "no-body":
///    - Respuestas 1xx, 204, 304 no llevan body.
///    - Respuestas a HEAD no deben incluir body.
///    -> Actualmente podrias intentar leer body y bloquearte o gastar lecturas innecesarias.
///
/// 4) Content-Length fragil / sin validaciones:
///    - No detecta multiples Content-Length conflictivos.
///    - Limite de body existe, pero no hay streaming.
///    - Si CL es invalido (no numerico / overflow), no hay manejo fino.
///
/// 5) Manejo simplista de Connection:
///    - No parsea tokens completos (ej. "keep-alive, upgrade").
///    - No contempla "Proxy-Connection" legacy.
///    - No evalua Upgrade / 101 Switching Protocols.
///
/// 6) Timeouts:
///    - Se aplican en la capa superior (Proxy::serve) para evitar bloqueos largos.
///
/// 7) BUG critico con keep-alive: bytes "sobrantes" se pierden
///    - En lecturas TCP puede llegar MAS de lo que falta para completar el body.
///    - Este metodo hace `take = remaining.min(n)` y descarta el resto.
///    - Eso rompe reutilizacion (pipelining, o simplemente siguiente respuesta pegada).
///    -> Solucion: buffer por conexion (BytesMut) para conservar leftovers.
///
/// 8) No soporta Upgrade/WebSocket (101):
///    - Tras 101, el stream deja de ser HTTP framing y pasa a ser bidireccional.
///    - Este metodo lo trataria como "body" y lo romperia.
///
/// 9) Parsing de headers "best-effort":
///    - `from_utf8_lossy` + `.lines()` es suficiente para casos normales, pero no es
///      un parser HTTP robusto (repetidos, espacios raros, etc.).
///
/// 10) No streaming:
///    - Se acumula toda la respuesta en memoria (Vec<u8>).
///    - En proxy real se suele streamear al cliente o imponer limites configurables.
#[instrument(skip(stream))]
pub(super) async fn read_http_response(
    stream: &mut TcpStream,
    max_headers: usize,
    max_body: usize,
) -> anyhow::Result<(Vec<u8>, bool)> {
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
        if buf.len() > max_headers {
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
            if max_body > 0 && response_bytes.len() - (headers_end + 4) > max_body {
                anyhow::bail!("Upstream response body too large");
            }
        }
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            response_bytes.extend_from_slice(&tmp[..n]);
            if max_body > 0 && response_bytes.len() - (headers_end + 4) > max_body {
                anyhow::bail!("Upstream response body too large");
            }
        }

        debug!(
            target: "migux::proxy",
            "Chunked response detected; returning non-reusable connection (chunk parser not implemented)"
        );
        return Ok((response_bytes, false));
    }

    // Caso Content-Length: leer exactamente CL bytes (o lo mejor posible)
    if let Some(cl) = content_length {
        if max_body > 0 && cl > max_body {
            anyhow::bail!("Upstream response body too large");
        }
        let mut body = Vec::with_capacity(cl);

        // Copia lo que ya habia
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

        // Reutilizacion:
        // - HTTP/1.0: solo reusable si Connection: keep-alive explicito
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
            if max_body > 0 && response_bytes.len() - (headers_end + 4) > max_body {
                anyhow::bail!("Upstream response body too large");
            }
        }
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            response_bytes.extend_from_slice(&tmp[..n]);
            if max_body > 0 && response_bytes.len() - (headers_end + 4) > max_body {
                anyhow::bail!("Upstream response body too large");
            }
        }

        debug!(
            target: "migux::proxy",
            "No Content-Length; read until EOF; connection not reusable"
        );
        Ok((response_bytes, false))
    }
}
