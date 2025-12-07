use std::{sync::Arc, time::Duration};

use dashmap::{self, DashMap};
use migux_config::MiguxConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
};

use crate::{build_servers_by_listen, ServersByListen};

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

            tokio::spawn(async move {
                if let Err(e) = accept_loop(listener, addr, sem_clone).await {
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
) -> anyhow::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;

        // 👇 Pedir un "permiso" del semáforo (si se agota, espera)
        let permit = semaphore.clone().acquire_owned().await?;
        println!("[master] new connection in {listen_addr} from {addr}");

        // Aquí creamos el "worker lógico" por conexión
        tokio::spawn(async move {
            // Cuando este future termine, el `permit` se suelta solo al salir del scope
            if let Err(e) = handle_connection(stream).await {
                eprintln!("[worker] error handling {addr}: {e:?}");
            }
            drop(permit); // explícito para que se entienda
        });
    }
}

async fn handle_connection(mut stream: tokio::net::TcpStream) -> anyhow::Result<()> {
    // 1. Leer algunos bytes de la request
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;

    if n == 0 {
        // El cliente se ha ido
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);

    // 2. Sacar la primera línea: "GET / HTTP/1.1"
    if let Some(first_line) = req_str.lines().next() {
        println!("[worker] request line: {}", first_line);

        // Intentamos separar método y path
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("-");
        println!("[worker] method = {}, path = {}", method, path);
    } else {
        println!("[worker] request sin primera línea?");
    }

    // 3. Responder algo sencillo (HTTP 200)
    let body = b"Hello from Migux!\n";

    let response = format!(
        "HTTP/1.1 200 OK\r\n\
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
