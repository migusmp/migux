use serde::Deserialize;

// =======================================================
// GLOBAL CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub worker_processes: u8,
    pub worker_connections: u16,
    pub log_level: String,
    pub error_log: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            worker_processes: 1,
            worker_connections: 1024,
            log_level: "info".into(),
            error_log: "/var/log/migux/error.log".into(),
        }
    }
}

impl GlobalConfig {
    pub fn worker_processes(&self) -> u8 {
        self.worker_processes
    }

    pub fn worker_connections(&self) -> u16 {
        self.worker_connections
    }

    pub fn log_level(&self) -> &str {
        &self.log_level
    }

    pub fn error_log(&self) -> &str {
        &self.error_log
    }

    pub(crate) fn apply_defaults_from(&mut self, defaults: &GlobalConfig) {
        if self.worker_processes == 0 {
            self.worker_processes = defaults.worker_processes;
        }
        if self.worker_connections == 0 {
            self.worker_connections = defaults.worker_connections;
        }
        if self.log_level.is_empty() {
            self.log_level = defaults.log_level.clone();
        }
        if self.error_log.is_empty() {
            self.error_log = defaults.error_log.clone();
        }
    }
}
