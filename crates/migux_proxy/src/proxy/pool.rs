//! Connection pooling helpers for upstream sockets.

use std::time::Instant;

use bytes::BytesMut;
use tokio::{
    net::TcpStream,
    time::{timeout, Duration},
};
use tracing::{debug, info, instrument};

use super::Proxy;

/// A pooled upstream connection with its read buffer.
pub(super) struct PooledStream {
    pub(super) stream: TcpStream,
    pub(super) read_buf: BytesMut,
    pub(super) last_used: Instant,
}

impl PooledStream {
    pub(super) fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: BytesMut::new(),
            last_used: Instant::now(),
        }
    }
}

impl Proxy {
    /// Takes an upstream connection from the pool or creates a new one.
    ///
    /// Flow:
    /// - Try entry.pop() (LIFO) from pool for addr
    /// - Reuse if still within idle TTL
    /// - Otherwise create a new connection
    #[instrument(skip(self))]
    pub(super) async fn checkout_upstream_stream(
        &self,
        addr: &str,
        connect_timeout: Duration,
        idle_ttl: Duration,
    ) -> anyhow::Result<PooledStream> {
        if let Some(mut entry) = self.pools.get_mut(addr) {
            while let Some(pooled) = entry.pop() {
                if idle_ttl.is_zero() || pooled.last_used.elapsed() <= idle_ttl {
                    debug!(target: "migux::proxy", upstream = %addr, "Reusing pooled upstream connection");
                    return Ok(pooled);
                }
                debug!(target: "migux::proxy", upstream = %addr, "Dropping idle pooled connection");
            }
        }

        info!(target: "migux::proxy", upstream = %addr, "Creating new upstream connection");
        let stream = connect_with_timeout(addr, connect_timeout).await?;
        Ok(PooledStream::new(stream))
    }

    /// Returns an upstream connection back to the pool so it can be reused.
    pub(super) fn checkin_upstream_stream(&self, addr: &str, mut pooled: PooledStream, max_pool: usize) {
        pooled.last_used = Instant::now();
        let mut entry = self.pools.entry(addr.to_string()).or_insert_with(Vec::new);
        if entry.len() >= max_pool {
            debug!(target: "migux::proxy", upstream = %addr, "Pool full; dropping connection");
            return;
        }
        entry.push(pooled);

        debug!(target: "migux::proxy", upstream = %addr, "Returned upstream connection to pool");
    }
}

/// Create a fresh upstream connection (used when a pooled socket is dead).
pub(super) async fn connect_fresh(addr: &str, timeout_dur: Duration) -> anyhow::Result<PooledStream> {
    let stream = connect_with_timeout(addr, timeout_dur).await?;
    Ok(PooledStream::new(stream))
}

/// Connect to an upstream with a timeout.
pub(super) async fn connect_with_timeout(addr: &str, timeout_dur: Duration) -> anyhow::Result<TcpStream> {
    match timeout(timeout_dur, TcpStream::connect(addr)).await {
        Ok(res) => Ok(res?),
        Err(_) => anyhow::bail!("Upstream connect timeout to {}", addr),
    }
}
