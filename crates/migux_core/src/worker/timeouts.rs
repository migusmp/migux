use bytes::{Buf, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};

use super::ClientStream;

pub(crate) enum ReadOutcome {
    Read(usize),
    Timeout,
}

pub(crate) async fn read_more(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    timeout_dur: Duration,
) -> anyhow::Result<ReadOutcome> {
    let mut tmp = [0u8; 4096];
    match timeout(timeout_dur, stream.read(&mut tmp)).await {
        Ok(res) => {
            let n = res?;
            if n > 0 {
                buf.extend_from_slice(&tmp[..n]);
            }
            Ok(ReadOutcome::Read(n))
        }
        Err(_) => Ok(ReadOutcome::Timeout),
    }
}

pub(crate) enum ChunkedBodyError {
    Timeout,
    Invalid,
    TooLarge,
    Io,
}

pub(crate) async fn discard_chunked_body(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    read_timeout: Duration,
    max_body: usize,
) -> Result<(), ChunkedBodyError> {
    let mut body_bytes = 0usize;

    loop {
        let line = read_line_bytes(stream, buf, read_timeout).await?;
        let size_str = match std::str::from_utf8(&line[..line.len() - 2]) {
            Ok(s) => s.split(';').next().unwrap_or("").trim(),
            Err(_) => return Err(ChunkedBodyError::Invalid),
        };
        let chunk_size =
            usize::from_str_radix(size_str, 16).map_err(|_| ChunkedBodyError::Invalid)?;

        if chunk_size == 0 {
            loop {
                let trailer = read_line_bytes(stream, buf, read_timeout).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        body_bytes = body_bytes.saturating_add(chunk_size);
        if max_body > 0 && body_bytes > max_body {
            return Err(ChunkedBodyError::TooLarge);
        }

        discard_exact(stream, buf, chunk_size + 2, read_timeout).await?;
    }
}

pub(crate) async fn discard_content_length(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    mut remaining: usize,
    read_timeout: Duration,
) -> Result<(), ChunkedBodyError> {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = remaining.min(buf.len());
            buf.advance(take);
            remaining -= take;
            continue;
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
    Ok(())
}

async fn read_line_bytes(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    read_timeout: Duration,
) -> Result<Vec<u8>, ChunkedBodyError> {
    loop {
        if let Some(end) = find_crlf(buf, 0) {
            let line = buf.split_to(end + 2);
            return Ok(line.to_vec());
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
}

async fn discard_exact(
    stream: &mut dyn ClientStream,
    buf: &mut BytesMut,
    mut remaining: usize,
    read_timeout: Duration,
) -> Result<(), ChunkedBodyError> {
    while remaining > 0 {
        if !buf.is_empty() {
            let take = remaining.min(buf.len());
            buf.advance(take);
            remaining -= take;
            continue;
        }
        match read_more(stream, buf, read_timeout)
            .await
            .map_err(|_| ChunkedBodyError::Io)?
        {
            ReadOutcome::Timeout => return Err(ChunkedBodyError::Timeout),
            ReadOutcome::Read(0) => return Err(ChunkedBodyError::Invalid),
            ReadOutcome::Read(_) => {}
        }
    }
    Ok(())
}

fn find_crlf(buf: &BytesMut, start: usize) -> Option<usize> {
    buf[start..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}
