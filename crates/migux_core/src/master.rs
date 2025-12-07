use std::{sync::Arc, time::Duration};

use dashmap::{self, DashMap};
use migux_config::MiguxConfig;
use tokio::{net::TcpListener, sync::Semaphore};

use crate::{build_servers_by_listen, worker::handle_connection, ServerRuntime, ServersByListen};

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

        let cfg = self.cfg.clone();

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
            let cfg_clone = cfg.clone();

            tokio::spawn(async move {
                if let Err(e) = accept_loop(
                    listener,
                    addr,
                    sem_clone,
                    Arc::new(servers_for_listener),
                    cfg_clone,
                )
                .await
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
    cfg: Arc<MiguxConfig>,
) -> anyhow::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;

        // 👇 Pedir un "permiso" del semáforo (si se agota, espera)
        let permit = semaphore.clone().acquire_owned().await?;
        println!("[master] new connection in {listen_addr} from {addr}");

        let servers_clone = servers.clone();
        let cfg_clone = cfg.clone();

        // Aquí creamos el "worker lógico" por conexión
        tokio::spawn(async move {
            // Cuando este future termine, el `permit` se suelta solo al salir del scope
            if let Err(e) = handle_connection(stream, addr, servers_clone, cfg_clone).await {
                eprintln!("[worker] error handling {addr}: {e:?}");
            }
            drop(permit); // explícito para que se entienda
        });
    }
}
