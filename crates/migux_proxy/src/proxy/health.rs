//! Upstream health tracking and active checks.

use std::time::{Duration, Instant};

use migux_config::{MiguxConfig, UpstreamConfig};
use tokio::time::interval;
use tracing::warn;

use super::{upstream, Proxy};
use super::pool::connect_with_timeout;

/// Health policy derived from upstream configuration.
#[derive(Debug, Clone)]
pub(super) struct HealthPolicy {
    pub(super) fail_threshold: u32,
    pub(super) cooldown: Duration,
}

/// Health state tracked per upstream address.
#[derive(Debug, Clone, Default)]
pub(super) struct UpstreamHealth {
    pub(super) failures: u32,
    pub(super) down_until: Option<Instant>,
}

impl Proxy {
    /// Start background active health checks for upstream pools.
    pub fn start_health_checks(self: &std::sync::Arc<Self>, cfg: std::sync::Arc<MiguxConfig>) {
        for (upstream_name, upstream_cfg) in cfg.upstream.iter() {
            if !upstream_cfg.health.active {
                continue;
            }

            let servers = match upstream::normalize_servers(upstream_cfg) {
                Ok(list) => list,
                Err(err) => {
                    warn!(
                        target: "migux::proxy",
                        upstream = %upstream_name,
                        error = ?err,
                        "Skipping health checks due to invalid upstream config"
                    );
                    continue;
                }
            };

            let policy = health_policy(upstream_cfg);
            let interval_secs = upstream_cfg.health.interval_secs.max(1);
            let timeout_secs = upstream_cfg.health.timeout_secs.max(1);
            let check_interval = Duration::from_secs(interval_secs);
            let check_timeout = Duration::from_secs(timeout_secs);
            let proxy = std::sync::Arc::clone(self);
            let upstream_name = upstream_name.clone();

            tokio::spawn(async move {
                let mut ticker = interval(check_interval);
                loop {
                    ticker.tick().await;
                    for addr in &servers {
                        let ok = connect_with_timeout(addr, check_timeout).await.is_ok();
                        if ok {
                            proxy.record_success(&upstream_name, addr);
                        } else {
                            proxy.record_failure(&upstream_name, addr, &policy);
                        }
                    }
                }
            });
        }
    }

    /// Filter upstream addresses to only those currently healthy.
    pub(super) fn filter_healthy_addrs(
        &self,
        upstream_name: &str,
        addrs: Vec<String>,
    ) -> Vec<String> {
        let now = Instant::now();
        let mut healthy = Vec::new();
        for addr in &addrs {
            if self.is_healthy(upstream_name, addr, now) {
                healthy.push(addr.clone());
            }
        }
        if healthy.is_empty() {
            addrs
        } else {
            healthy
        }
    }

    fn is_healthy(&self, upstream_name: &str, addr: &str, now: Instant) -> bool {
        let key = health_key(upstream_name, addr);
        if let Some(mut entry) = self.health.get_mut(&key) {
            if let Some(until) = entry.down_until {
                if until > now {
                    return false;
                }
                entry.down_until = None;
                entry.failures = 0;
            }
        }
        true
    }

    /// Record a connection failure and update circuit-breaker state.
    pub(super) fn record_failure(&self, upstream_name: &str, addr: &str, policy: &HealthPolicy) {
        let key = health_key(upstream_name, addr);
        let mut entry = self
            .health
            .entry(key)
            .or_insert_with(UpstreamHealth::default);
        entry.failures = entry.failures.saturating_add(1);
        let threshold = policy.fail_threshold.max(1);
        if entry.failures >= threshold {
            entry.down_until = Some(Instant::now() + policy.cooldown);
            tracing::debug!(
                target: "migux::proxy",
                upstream = %upstream_name,
                addr = %addr,
                "Marking upstream as down"
            );
        }
    }

    /// Record a successful connection and clear failure state.
    pub(super) fn record_success(&self, upstream_name: &str, addr: &str) {
        let key = health_key(upstream_name, addr);
        if let Some(mut entry) = self.health.get_mut(&key) {
            entry.failures = 0;
            entry.down_until = None;
        }
    }
}

/// Build a health policy from upstream config.
pub(super) fn health_policy(cfg: &UpstreamConfig) -> HealthPolicy {
    let threshold = cfg.health.fail_threshold.max(1);
    let cooldown_secs = cfg.health.cooldown_secs;
    HealthPolicy {
        fail_threshold: threshold,
        cooldown: Duration::from_secs(cooldown_secs),
    }
}

fn health_key(upstream_name: &str, addr: &str) -> String {
    format!("{upstream_name}|{addr}")
}

#[cfg(test)]
mod tests {
    use super::{health_key, HealthPolicy, Proxy, UpstreamHealth};
    use std::time::{Duration, Instant};

    #[test]
    fn filter_healthy_addrs_skips_down_nodes() {
        let proxy = Proxy::new();
        let policy = HealthPolicy {
            fail_threshold: 1,
            cooldown: Duration::from_secs(60),
        };
        proxy.record_failure("api", "127.0.0.1:3000", &policy);
        let addrs = vec![
            "127.0.0.1:3000".to_string(),
            "127.0.0.1:3001".to_string(),
        ];
        let filtered = proxy.filter_healthy_addrs("api", addrs);
        assert_eq!(filtered, vec!["127.0.0.1:3001".to_string()]);
    }

    #[test]
    fn filter_healthy_addrs_falls_back_when_all_down() {
        let proxy = Proxy::new();
        let policy = HealthPolicy {
            fail_threshold: 1,
            cooldown: Duration::from_secs(60),
        };
        proxy.record_failure("api", "127.0.0.1:3000", &policy);
        proxy.record_failure("api", "127.0.0.1:3001", &policy);
        let addrs = vec![
            "127.0.0.1:3000".to_string(),
            "127.0.0.1:3001".to_string(),
        ];
        let filtered = proxy.filter_healthy_addrs("api", addrs.clone());
        assert_eq!(filtered, addrs);
    }

    #[test]
    fn filter_healthy_addrs_recovers_after_cooldown() {
        let proxy = Proxy::new();
        let key = health_key("api", "127.0.0.1:3000");
        proxy.health.insert(
            key,
            UpstreamHealth {
                failures: 1,
                down_until: Some(Instant::now() - Duration::from_secs(1)),
            },
        );
        let addrs = vec!["127.0.0.1:3000".to_string()];
        let filtered = proxy.filter_healthy_addrs("api", addrs);
        assert_eq!(filtered, vec!["127.0.0.1:3000".to_string()]);
    }
}
