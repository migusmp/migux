use std::sync::Arc;

use migux_proxy::Proxy;
use tokio::sync::Semaphore;
use tracing::info;

use super::Master;

impl Master {
    pub(super) fn log_startup(&self) {
        info!(target: "migux::master", "Starting MIGUX MASTER");
        info!(
            target: "migux::master",
            worker_processes = self.cfg.global.worker_processes,
            worker_connections = self.cfg.global.worker_connections,
            log_level = %self.cfg.global.log_level,
            "Global configuration loaded"
        );
    }

    pub(super) fn init_semaphore(&self) -> Arc<Semaphore> {
        let max_conns = self.cfg.global.worker_connections as usize;
        let semaphore = Arc::new(Semaphore::new(max_conns));
        info!(
            target: "migux::master",
            max_conns,
            "Global connection semaphore initialized"
        );
        semaphore
    }

    pub(super) fn start_proxy(&self) -> Arc<Proxy> {
        let proxy = Arc::new(Proxy::new());
        proxy.start_health_checks(self.cfg.clone());
        proxy
    }
}
