use tokio::{fs, io::AsyncWriteExt, net::TcpStream};

use crate::ServerRuntime;

/// Helper genérico para enviar una respuesta HTTP con cuerpo binario.
pub async fn send_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// Helper para respuestas de texto plano.
async fn send_text_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> anyhow::Result<()> {
    send_response(stream, status, "text/plain; charset=utf-8", body.as_bytes()).await
}

pub async fn send_404(stream: &mut TcpStream) -> anyhow::Result<()> {
    send_text_response(stream, "404 Not Found", "404 Not Found\n").await
}

pub async fn send_501(stream: &mut TcpStream) -> anyhow::Result<()> {
    send_text_response(
        stream,
        "501 Not Implemented",
        "501 Not Implemented (proxy TODO)\n",
    )
    .await
}

pub async fn send_500(stream: &mut TcpStream) -> anyhow::Result<()> {
    send_text_response(
        stream,
        "500 Internal Server Error",
        "Internal Server Error\n",
    )
    .await
}

/// (DEPRECATED) servir index usando la misma lógica genérica.
pub async fn serve_index(stream: &mut TcpStream, server: &ServerRuntime) -> anyhow::Result<()> {
    let file_path = format!("{}/{}", server.config.root, server.config.index);
    println!("[worker] serving file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            send_response(stream, "200 OK", "text/html; charset=utf-8", &body).await?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[worker] index not found {}: {:?}", file_path, e);
            send_404(stream).await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            send_500(stream).await?;
        }
    }

    Ok(())
}
