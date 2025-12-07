use std::{sync::Arc, time::Duration};
use tokio::{fs, net::TcpStream};

use dashmap::{self, DashMap};
use migux_config::{LocationConfig, LocationType, MiguxConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
};

use crate::{build_servers_by_listen, ServerRuntime, ServersByListen};

pub struct CacheStore {
    pub responses: DashMap<String, Vec<u8>>,
}

impl CacheStore {
    pub fn new() -> Self {
        Self {
            responses: DashMap::new(),
        }
    }
}

pub struct Master {
    cfg: Arc<MiguxConfig>,
    servers_by_listen: Arc<ServersByListen>,
    cache: CacheStore,
}

impl Master {
    pub fn new(cfg: MiguxConfig) -> Self {
        let cfg = Arc::new(cfg);
        let servers_by_listen = Arc::new(build_servers_by_listen(&cfg));

        Self {
            cfg,
            servers_by_listen,
            cache: CacheStore::new(),
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        println!("========== MIGUX MASTER ==========");
        println!("worker_processes   = {}", self.cfg.global.worker_processes);
        println!(
            "worker_connections = {}",
            self.cfg.global.worker_connections
        );
        println!("log_level          = {}", self.cfg.global.log_level);

        // 👇 límite de conexiones simultáneas en TODO el proceso (por ahora)
        let max_conns = self.cfg.global.worker_connections as usize;
        let semaphore = Arc::new(Semaphore::new(max_conns));

        println!("\n[listen sockets (Tokio)]");
        for (listen_addr, servers) in self.servers_by_listen.iter() {
            // Crear listener de Tokio
            let listener = TcpListener::bind(listen_addr).await?;

            println!(
                "  - tokio listener en {listen_addr} ({} servers)",
                servers.len()
            );

            let addr = listen_addr.clone();
            let sem_clone = semaphore.clone();
            let servers_for_listener = servers.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    accept_loop(listener, addr, sem_clone, Arc::new(servers_for_listener)).await
                {
                    eprintln!("[accept-loop] error: {e:?}");
                }
            });
        }

        println!("==================================");

        // Mantener el proceso vivo (por ahora) hasta que le hagas Ctrl+C
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    listen_addr: String,
    semaphore: Arc<Semaphore>,
    servers: Arc<Vec<ServerRuntime>>,
) -> anyhow::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;

        // 👇 Pedir un "permiso" del semáforo (si se agota, espera)
        let permit = semaphore.clone().acquire_owned().await?;
        println!("[master] new connection in {listen_addr} from {addr}");

        let servers_clone = servers.clone();

        // Aquí creamos el "worker lógico" por conexión
        tokio::spawn(async move {
            // Cuando este future termine, el `permit` se suelta solo al salir del scope
            if let Err(e) = handle_connection(stream, servers_clone).await {
                eprintln!("[worker] error handling {addr}: {e:?}");
            }
            drop(permit); // explícito para que se entienda
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    servers: Arc<Vec<ServerRuntime>>,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;

    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);

    let mut method = "-";
    let mut path = "/";

    if let Some(first_line) = req_str.lines().next() {
        println!("[worker] request line: {}", first_line);
        let mut parts = first_line.split_whitespace();
        method = parts.next().unwrap_or("-");
        path = parts.next().unwrap_or("/");
        println!("[worker] method = {}, path = {}", method, path);
    }

    let server = select_default_server(&servers);
    println!(
        "[worker] using server '{}' (root = {}, index = {})",
        server.name, server.config.root, server.config.index
    );

    if server.locations.is_empty() {
        // por si acaso no hubiera locations (no es tu caso)
        send_404(&mut stream).await?;
        return Ok(());
    }

    let location = match_location(&server.locations, path);
    println!(
        "[worker] matched location: server={}, path={}, type={:?}",
        location.server, location.path, location.r#type
    );

    if method != "GET" {
        // De momento solo GET
        send_404(&mut stream).await?;
        return Ok(());
    }

    match location.r#type {
        LocationType::Static => {
            serve_static(&mut stream, server, location, path).await?;
        }
        LocationType::Proxy => {
            // TODO: proxy → upstream.app
            send_501(&mut stream).await?;
        }
    }

    Ok(())
}

async fn serve_static(
    stream: &mut TcpStream,
    server: &ServerRuntime,
    location: &LocationConfig,
    req_path: &str,
) -> anyhow::Result<()> {
    // root efectiva: location.root o server.root
    let root = location.root.as_deref().unwrap_or(&server.config.root);

    // index efectivo: location.index o server.index
    let index = location.index.as_deref().unwrap_or(&server.config.index);

    // Path relativo dentro del root
    let rel = if req_path == location.path {
        // si piden justo el path de la location → index
        index.to_string()
    } else if req_path.starts_with(&location.path) {
        let mut tail = &req_path[location.path.len()..]; // quitar prefijo de location
        if tail.starts_with('/') {
            tail = &tail[1..];
        }
        if tail.is_empty() {
            index.to_string()
        } else {
            tail.to_string()
        }
    } else if req_path == "/" && location.path == "/" {
        // caso típico: location "/" y petición "/"
        index.to_string()
    } else {
        // no encaja bien → 404
        send_404(stream).await?;
        return Ok(());
    };

    let file_path = format!("{}/{}", root, rel);
    println!("[worker] static file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );

            stream.write_all(response.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            let body = b"Internal Server Error\n";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.flush().await?;
        }
    }

    Ok(())
}

async fn send_501(stream: &mut TcpStream) -> anyhow::Result<()> {
    let body = b"501 Not Implemented (proxy TODO)\n";
    let response = format!(
        "HTTP/1.1 501 Not Implemented\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
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

fn select_default_server<'a>(servers: &'a [ServerRuntime]) -> &'a ServerRuntime {
    // De momento, simplemente el primero
    &servers[0]
}

fn match_location<'a>(locations: &'a [LocationConfig], path: &str) -> &'a LocationConfig {
    // Elegimos la location cuyo `path` sea prefijo del path de la request
    // y con el `path` más largo (estilo nginx).
    locations
        .iter()
        .filter(|loc| path.starts_with(&loc.path))
        .max_by_key(|loc| loc.path.len())
        .unwrap_or_else(|| {
            // fallback: primera location, si no hay prefijo que encaje
            &locations[0]
        })
}

async fn serve_index(
    stream: &mut tokio::net::TcpStream,
    server: &ServerRuntime,
) -> anyhow::Result<()> {
    let file_path = format!("{}/{}", server.config.root, server.config.index);
    println!("[worker] serving file: {}", file_path);

    match fs::read(&file_path).await {
        Ok(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );

            stream.write_all(response.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
        }
        Err(e) => {
            eprintln!("[worker] error reading {}: {:?}", file_path, e);
            // si falla el archivo, devolvemos 500
            let body = b"Internal Server Error\n";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.flush().await?;
        }
    }

    Ok(())
}
async fn send_404(stream: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
    let body = b"404 Not Found\n";
    let response = format!(
        "HTTP/1.1 404 Not Found\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
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
