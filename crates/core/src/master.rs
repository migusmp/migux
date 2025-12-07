use std::{sync::Arc, time::Duration};
use tokio::fs;

use dashmap::{self, DashMap};
use migux_config::MiguxConfig;
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
    mut stream: tokio::net::TcpStream,
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

    // 👉 De momento: GET + "/" → servir index.html
    if method == "GET" && path == "/" {
        serve_index(&mut stream, server).await?;
    } else {
        // cualquier otra ruta → 404 simple
        send_404(&mut stream).await?;
    }

    Ok(())
}

fn select_default_server<'a>(servers: &'a [ServerRuntime]) -> &'a ServerRuntime {
    // De momento, simplemente el primero
    &servers[0]
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
