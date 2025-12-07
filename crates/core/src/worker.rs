use std::sync::Arc;

use tokio::{io::AsyncReadExt, net::TcpStream};

use migux_config::{LocationType, MiguxConfig};

use crate::ServerRuntime;

// Submódulos del worker
mod proxy;
mod responses;
mod routing;
mod static_files;

use proxy::serve_proxy;
use responses::send_404;
use routing::{match_location, parse_request_line, select_default_server};
use static_files::serve_static;

/// Punto de entrada del "worker lógico" por conexión.
pub async fn handle_connection(
    mut stream: TcpStream,
    servers: Arc<Vec<ServerRuntime>>,
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await?;

    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);

    // ------------ parse request line ------------
    let (method, path) = parse_request_line(&req_str);

    println!("[worker] method = {}, path = {}", method, path);

    let server = select_default_server(&servers);
    println!(
        "[worker] using server '{}' (root = {}, index = {})",
        server.name, server.config.root, server.config.index
    );

    if server.locations.is_empty() {
        send_404(&mut stream).await?;
        return Ok(());
    }

    // Por ahora solo GET
    if method != "GET" {
        send_404(&mut stream).await?;
        return Ok(());
    }

    let location = match_location(&server.locations, path);
    println!(
        "[worker] matched location: server={}, path={}, type={:?}",
        location.server, location.path, location.r#type
    );

    match location.r#type {
        LocationType::Static => {
            serve_static(&mut stream, server, location, path).await?;
        }
        LocationType::Proxy => {
            serve_proxy(&mut stream, server, location, path, &req_str, &cfg).await?;
        }
    }

    Ok(())
}
