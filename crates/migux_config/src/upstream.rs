use serde::Deserialize;

// =======================================================
// UPSTREAM CONFIG + DEFAULTS
// =======================================================
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UpstreamServers {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub server: UpstreamServers,
    pub strategy: Option<String>,
    pub health: UpstreamHealthConfig,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            server: UpstreamServers::One(String::new()),
            strategy: Some("round_robin".to_string()),
            health: UpstreamHealthConfig::default(),
        }
    }
}

impl UpstreamConfig {
    pub fn server(&self) -> &UpstreamServers {
        &self.server
    }

    pub fn strategy(&self) -> Option<&str> {
        self.strategy.as_deref()
    }

    pub fn health(&self) -> &UpstreamHealthConfig {
        &self.health
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
/// Health/circuit-breaker configuration for an upstream pool.
pub struct UpstreamHealthConfig {
    /// Failures before marking an upstream down.
    pub fail_threshold: u32,
    /// Cooldown time in seconds before retrying a down node.
    pub cooldown_secs: u64,
    /// Enable active TCP health checks.
    pub active: bool,
    /// Interval for active checks in seconds.
    pub interval_secs: u64,
    /// Timeout for active checks in seconds.
    pub timeout_secs: u64,
}

impl Default for UpstreamHealthConfig {
    fn default() -> Self {
        Self {
            fail_threshold: 1,
            cooldown_secs: 10,
            active: false,
            interval_secs: 10,
            timeout_secs: 1,
        }
    }
}

impl UpstreamHealthConfig {
    pub fn fail_threshold(&self) -> u32 {
        self.fail_threshold
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }
}

impl std::fmt::Display for UpstreamServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamServers::One(s) => write!(f, "{s}"),
            UpstreamServers::Many(list) => write!(f, "{:?}", list),
        }
    }
}
