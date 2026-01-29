use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Helper genérico para enviar una respuesta HTTP con cuerpo binario.
pub async fn send_response<W: AsyncWrite + Unpin + ?Sized>(
    stream: &mut W,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Server: migux/0.1.0\r\n\
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
async fn send_text_response<W: AsyncWrite + Unpin + ?Sized>(
    stream: &mut W,
    status: &str,
    body: &str,
) -> anyhow::Result<()> {
    send_response(stream, status, "text/plain; charset=utf-8", body.as_bytes()).await
}

pub async fn send_404<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "404 Not Found", "404 Not Found\n").await
}

pub async fn send_501<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(
        stream,
        "501 Not Implemented",
        "501 Not Implemented (proxy TODO)\n",
    )
    .await
}

pub async fn send_500<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(
        stream,
        "500 Internal Server Error",
        "Internal Server Error\n",
    )
    .await
}

pub async fn send_502<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "502 Bad Gateway", "502 Bad Gateway\n").await
}

pub async fn send_405<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "405 Method Not Allowed", "405 Method Not Allowed\n").await
}

pub async fn send_400<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "400 Bad Request", "400 Bad Request\n").await
}

pub async fn send_408<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "408 Request Timeout", "408 Request Timeout\n").await
}

pub async fn send_413<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(stream, "413 Payload Too Large", "413 Payload Too Large\n").await
}

pub async fn send_431<W: AsyncWrite + Unpin + ?Sized>(stream: &mut W) -> anyhow::Result<()> {
    send_text_response(
        stream,
        "431 Request Header Fields Too Large",
        "431 Request Header Fields Too Large\n",
    )
    .await
}

pub async fn send_redirect<W: AsyncWrite + Unpin + ?Sized>(
    stream: &mut W,
    location: &str,
) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Server: migux/0.1.0\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
