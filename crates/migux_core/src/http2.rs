use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::error;

use crate::worker::handle_connection;
use crate::ServerRuntime;
use migux_config::MiguxConfig;
use migux_proxy::Proxy;

const IN_MEMORY_STREAM_CAPACITY: usize = 64 * 1024;

pub async fn serve_h2_connection<S>(
    stream: S,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let service = service_fn(move |req: Request<Incoming>| {
        let servers = servers.clone();
        let proxy = proxy.clone();
        let cfg = cfg.clone();
        async move { handle_h2_request(req, client_addr, servers, proxy, cfg).await }
    });

    http2::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await
        .context("HTTP/2 connection error")?;

    Ok(())
}

async fn handle_h2_request(
    req: Request<Incoming>,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
) -> Result<hyper::Response<Full<Bytes>>, hyper::Error> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(target: "migux::http2", error = ?e, "Failed to read HTTP/2 body");
            return Ok(simple_h2_response(StatusCode::BAD_REQUEST, b"Bad Request"));
        }
    };

    let req_bytes = build_http1_request(&parts, body_bytes.as_ref());
    let resp_bytes = match run_http1_pipeline(
        req_bytes,
        client_addr,
        servers,
        proxy,
        cfg,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(target: "migux::http2", error = ?e, "HTTP/1 pipeline failed");
            return Ok(simple_h2_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                b"Internal Server Error",
            ));
        }
    };

    match parse_http1_response(&resp_bytes) {
        Ok((status, headers, body)) => {
            let mut builder = hyper::Response::builder().status(status);
            for (name, value) in headers.iter() {
                builder = builder.header(name, value);
            }
            let response = builder
                .body(Full::new(Bytes::from(body)))
                .unwrap_or_else(|_| {
                    hyper::Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Full::new(Bytes::from_static(b"Internal Server Error")))
                        .expect("building fallback response")
                });
            Ok(response)
        }
        Err(e) => {
            error!(target: "migux::http2", error = ?e, "Failed to parse HTTP/1 response");
            Ok(simple_h2_response(
                StatusCode::BAD_GATEWAY,
                b"Bad Gateway",
            ))
        }
    }
}

fn build_http1_request(parts: &http::request::Parts, body: &[u8]) -> Vec<u8> {
    let method = parts.method.as_str();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let mut out = Vec::new();
    out.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());

    let mut has_host = false;
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        match name_str {
            "host" => has_host = true,
            "connection" | "proxy-connection" | "keep-alive" | "upgrade" => continue,
            "transfer-encoding" | "content-length" => continue,
            _ => {}
        }

        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if !has_host {
        if let Some(authority) = parts.uri.authority() {
            out.extend_from_slice(b"Host: ");
            out.extend_from_slice(authority.as_str().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

async fn run_http1_pipeline(
    req_bytes: Vec<u8>,
    client_addr: SocketAddr,
    servers: Arc<Vec<ServerRuntime>>,
    proxy: Arc<Proxy>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<Vec<u8>> {
    let (mut client_io, server_io) = tokio::io::duplex(IN_MEMORY_STREAM_CAPACITY);

    let server_task = tokio::spawn(async move {
        if let Err(e) = handle_connection(
            Box::new(server_io),
            client_addr,
            servers,
            proxy,
            cfg,
            true,
        )
        .await
        {
            error!(target: "migux::http2", error = ?e, "HTTP/1 handler error");
        }
    });

    client_io.write_all(&req_bytes).await?;
    client_io.shutdown().await?;

    let mut resp_bytes = Vec::new();
    client_io.read_to_end(&mut resp_bytes).await?;

    let _ = server_task.await;

    Ok(resp_bytes)
}

fn parse_http1_response(
    bytes: &[u8],
) -> anyhow::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    let parsed = resp.parse(bytes).context("parse http/1 response")?;
    let header_len = match parsed {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => anyhow::bail!("incomplete http/1 response"),
    };

    let status = resp.code.unwrap_or(500);
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut header_map = HeaderMap::new();
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;

    for header in resp.headers.iter() {
        let name = header.name;
        let value = header.value;
        let name_lower = name.to_ascii_lowercase();
        match name_lower.as_str() {
            "connection" | "proxy-connection" | "keep-alive" | "upgrade" => continue,
            "transfer-encoding" => {
                let val = String::from_utf8_lossy(value).to_ascii_lowercase();
                if val.split(',').any(|v| v.trim().trim_matches('"') == "chunked") {
                    is_chunked = true;
                }
                continue;
            }
            "content-length" => {
                if let Ok(s) = std::str::from_utf8(value) {
                    if let Ok(len) = s.trim().parse::<usize>() {
                        content_length = Some(len);
                    }
                }
                continue;
            }
            _ => {}
        }

        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value),
        ) {
            header_map.append(name, value);
        }
    }

    let mut body = bytes[header_len..].to_vec();
    if is_chunked {
        body = decode_chunked(&body)?;
    } else if let Some(len) = content_length {
        if body.len() > len {
            body.truncate(len);
        }
    }

    header_map.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string())?,
    );

    Ok((status, header_map, body))
}

fn decode_chunked(body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut input = body;

    loop {
        let Some(line_end) = find_crlf(input) else {
            anyhow::bail!("invalid chunked encoding: missing size line");
        };
        let line = &input[..line_end];
        input = &input[line_end + 2..];

        let line_str = std::str::from_utf8(line)?;
        let size_str = line_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .context("invalid chunk size in chunked body")?;

        if size == 0 {
            // Optional trailers follow; no need to parse them for our use case.
            break;
        }

        if input.len() < size + 2 {
            anyhow::bail!("invalid chunked encoding: chunk too short");
        }

        out.extend_from_slice(&input[..size]);
        input = &input[size..];

        if !input.starts_with(b"\r\n") {
            anyhow::bail!("invalid chunked encoding: missing CRLF after chunk");
        }
        input = &input[2..];
    }

    Ok(out)
}

fn find_crlf(input: &[u8]) -> Option<usize> {
    input.windows(2).position(|w| w == b"\r\n")
}

fn simple_h2_response(status: StatusCode, body: &'static [u8]) -> hyper::Response<Full<Bytes>> {
    let len = body.len().to_string();
    hyper::Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, len)
        .body(Full::new(Bytes::from_static(body)))
        .unwrap_or_else(|_| {
            hyper::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from_static(b"Internal Server Error")))
                .expect("building fallback response")
        })
}
