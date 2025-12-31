use anyhow::Ok;
use dashmap::DashMap;
use std::sync::atomic::AtomicUsize;
use tokio::net::TcpStream;

/// =======================================================
/// PROXY STATE
/// =======================================================
///
/// Este struct contiene **TODO el estado mutable del proxy**.
/// Es decir: cosas que deben sobrevivir entre requests
/// y ser seguras en concurrencia (varios clientes a la vez).
///
/// En nginx real esto viviría en memoria compartida
/// entre workers; en Migux vive dentro de un `Arc<Proxy>`.
pub struct Proxy {
    /// ---------------------------------------------------
    /// Round-robin counters por upstream
    /// ---------------------------------------------------
    ///
    /// Key: upstream name (ej: "app", "api", "auth")
    /// Value: contador atómico incremental
    ///
    /// ¿Para qué sirve?
    /// - Implementar **round-robin real** entre múltiples
    ///   servidores de un mismo upstream.
    ///
    /// Ejemplo:
    ///   upstream app {
    ///     server 127.0.0.1:3000;
    ///     server 127.0.0.1:3001;
    ///   }
    ///
    /// Flujo:
    /// - Cada request hace: counter.fetch_add(1)
    /// - idx = counter % num_servers
    /// - Ese idx decide qué backend va primero
    ///
    /// ¿Por qué AtomicUsize?
    /// - Incremento lock-free
    /// - No necesitamos orden perfecto, solo reparto
    ///
    /// ¿Por qué DashMap?
    /// - Varios upstreams en paralelo
    /// - Inserción lazy sin bloquear todo el mapa
    ///
    /// ⚠️ Importante:
    /// - El orden NO es determinista entre threads
    /// - Eso es correcto y deseable para balanceo
    rr_counters: DashMap<String, AtomicUsize>,

    /// ---------------------------------------------------
    /// Connection pools por upstream address
    /// ---------------------------------------------------
    ///
    /// Key: dirección concreta del backend
    ///      "127.0.0.1:3000"
    ///
    /// Value: pool de conexiones TCP reutilizables
    ///        (LIFO simple con Vec)
    ///
    /// ¿Para qué sirve?
    /// - Evitar hacer `TcpStream::connect()` en cada request
    /// - Reutilizar sockets keep-alive si el upstream lo permite
    ///
    /// Flujo:
    /// - checkout():
    ///     * intenta pop() del Vec
    ///     * si no hay, crea conexión nueva
    ///
    /// - checkin():
    ///     * si la respuesta fue reusable
    ///     * devuelve el TcpStream al Vec
    ///
    /// ¿Por qué por *addr* y no por upstream name?
    /// - Porque un upstream puede tener varios servidores
    /// - Cada backend mantiene su propio pool
    ///
    /// ¿Por qué Vec<TcpStream>?
    /// - Simple y rápido (push/pop)
    /// - LIFO suele funcionar bien para keep-alive
    ///
    /// ⚠️ Limitaciones (conocidas y aceptadas):
    /// - No hay health-check
    /// - No hay TTL / idle timeout
    /// - Un socket muerto puede colarse (se detecta al escribir)
    /// - No hay fairness entre conexiones
    ///
    pools: DashMap<String, Vec<TcpStream>>,

    /// ---------------------------------------------------
    /// Límite de conexiones por backend
    /// ---------------------------------------------------
    ///
    /// Máximo de sockets que se guardan en el pool
    /// por cada "host:port".
    ///
    /// Evita:
    /// - Crecimiento infinito del Vec
    /// - Consumo de FDs innecesarios
    ///
    /// Política típica:
    /// - Si el pool está lleno → se cierra el socket
    ///   en vez de devolverlo al pool
    ///
    /// Esto es similar a:
    ///   keepalive_requests / keepalive_connections
    ///   en servidores reales.
    max_pool_per_addr: usize,
}

impl Proxy {
    /// Crea una nueva instancia del proxy
    ///
    /// Normalmente se envuelve en:
    ///   Arc<Proxy>
    ///
    /// Y se comparte entre todos los workers / handlers.
    pub fn new(max_pool_per_addr: usize) -> Self {
        Self {
            // Contadores RR vacíos
            // Se crean bajo demanda cuando aparece un upstream
            rr_counters: DashMap::new(),

            // Pools vacíos
            // Cada backend crea su pool cuando se usa por primera vez
            pools: DashMap::new(),

            // Límite configurable (ej: 32, 64)
            max_pool_per_addr,
        }
    }

    /// Asegura que existe un contador RR para este upstream
    /// (si ya existe, no hace nada)
    pub fn ensure_upstream_counter(&self, upstream_name: &str) {
        self.rr_counters
            .entry(upstream_name.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
    }

    pub async fn checkout_upstream_stream(&self, addr: &str) -> anyhow::Result<TcpStream> {
        // Intentamos reutilizar la conexión
        if let Some(mut entry) = self.pools.get_mut(addr)
            && let Some(stream) = entry.pop()
        {
            return Ok(stream);
        }

        // Si no hay, conectamos uno nuevo
        Ok(TcpStream::connect(addr).await?)
    }

    pub fn checkin_upstream_stream(&self, addr: &str, stream: TcpStream) {
        let mut entry = self.pools.entry(addr.to_string()).or_default();

        // limite para que no crezca infinito
        if entry.len() < self.max_pool_per_addr {
            entry.push(stream);
        }

        // si esta lleno, se dropea y se cierra al salir
    }

    // TODO
    // - rewrite_proxy_headers
    // - read_http_response
    // - serve_proxy (como método)
}
