use std::{collections::HashSet, net::SocketAddr, path::Path};

use crate::{LocationType, MiguxConfig, UpstreamServers};

/// Validation output for a loaded Migux configuration.
#[derive(Debug, Default)]
pub struct ConfigReport {
    warnings: Vec<String>,
    errors: Vec<String>,
}

impl ConfigReport {
    /// Returns true when no errors were found.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns true when at least one error was found.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Returns the collected warning messages.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Returns the collected error messages.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Render warnings and errors into a readable, multi-line string.
    pub fn format(&self) -> String {
        let mut out = String::new();
        if !self.errors.is_empty() {
            out.push_str("Errors:\n");
            for err in &self.errors {
                out.push_str("  - ");
                out.push_str(err);
                out.push('\n');
            }
        }
        if !self.warnings.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Warnings:\n");
            for warn in &self.warnings {
                out.push_str("  - ");
                out.push_str(warn);
                out.push('\n');
            }
        }
        out
    }

    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }
}

/// Validate a Migux configuration and return a report of issues.
pub fn validate(cfg: &MiguxConfig) -> ConfigReport {
    let mut report = ConfigReport::default();

    validate_http_cache(cfg, &mut report);
    validate_upstreams(cfg, &mut report);
    validate_servers(cfg, &mut report);
    validate_locations(cfg, &mut report);

    report
}

fn validate_http_cache(cfg: &MiguxConfig, report: &mut ConfigReport) {
    let Some(cache_dir) = cfg.http.cache_dir.as_deref() else {
        return;
    };

    let cache_path = Path::new(cache_dir);
    if cache_path.exists() {
        if !cache_path.is_dir() {
            report.error(format!(
                "http.cache_dir '{cache_dir}' exists but is not a directory"
            ));
        }
    } else {
        report.warn(format!(
            "http.cache_dir '{cache_dir}' does not exist; it will be created at runtime"
        ));
    }

    if cfg.http.cache_default_ttl_secs.unwrap_or(0) == 0 {
        report.warn("http.cache_default_ttl_secs is 0; cache entries will not be stored");
    }

    if cfg.http.cache_max_object_bytes.unwrap_or(0) == 0 {
        report.warn("http.cache_max_object_bytes is 0; cache is effectively disabled");
    }
}

fn validate_upstreams(cfg: &MiguxConfig, report: &mut ConfigReport) {
    for (name, upstream) in &cfg.upstream {
        match &upstream.server {
            UpstreamServers::One(server) => {
                if server.trim().is_empty() {
                    report.error(format!("upstream '{name}' has an empty server address"));
                }
            }
            UpstreamServers::Many(servers) => {
                if servers.is_empty() {
                    report.error(format!("upstream '{name}' has no servers configured"));
                }
                for (idx, server) in servers.iter().enumerate() {
                    if server.trim().is_empty() {
                        report.error(format!(
                            "upstream '{name}' server entry at index {idx} is empty"
                        ));
                    }
                }
            }
        }
    }
}

fn validate_servers(cfg: &MiguxConfig, report: &mut ConfigReport) {
    if cfg.servers.is_empty() {
        report.error("no [server] sections found; at least one server is required");
        return;
    }

    let mut http_listens = HashSet::new();
    let mut tls_listens = HashSet::new();

    for (name, server) in &cfg.servers {
        if server.listen.trim().is_empty() {
            report.error(format!("server '{name}' has an empty listen address"));
        } else {
            if server.listen.parse::<SocketAddr>().is_err() {
                report.warn(format!(
                    "server '{name}' listen '{listen}' is not a socket address; DNS resolution will be used",
                    listen = server.listen
                ));
            }
            http_listens.insert(server.listen.clone());
        }

        if server.root.trim().is_empty() {
            report.warn(format!("server '{name}' has an empty root path"));
        } else if !Path::new(&server.root).exists() {
            report.warn(format!(
                "server '{name}' root '{root}' does not exist",
                root = server.root
            ));
        }

        if let Some(tls) = &server.tls {
            if tls.listen.trim().is_empty() {
                report.error(format!(
                    "server '{name}' enables TLS but tls.listen is empty"
                ));
            } else {
                tls_listens.insert(tls.listen.clone());
            }

            if tls.cert_path.trim().is_empty() || tls.key_path.trim().is_empty() {
                report.error(format!(
                    "server '{name}' TLS config requires cert_path and key_path"
                ));
            } else {
                if !Path::new(&tls.cert_path).is_file() {
                    report.error(format!(
                        "server '{name}' TLS cert_path '{path}' not found",
                        path = tls.cert_path
                    ));
                }
                if !Path::new(&tls.key_path).is_file() {
                    report.error(format!(
                        "server '{name}' TLS key_path '{path}' not found",
                        path = tls.key_path
                    ));
                }
            }
        }
    }

    for listen in tls_listens {
        if http_listens.contains(&listen) {
            report.error(format!(
                "TLS listen '{listen}' conflicts with an HTTP listen; use separate ports"
            ));
        }
    }
}

fn validate_locations(cfg: &MiguxConfig, report: &mut ConfigReport) {
    for (name, location) in &cfg.location {
        if location.server.trim().is_empty() {
            report.error(format!("location '{name}' has an empty server reference"));
            continue;
        }

        let server = match cfg.servers.get(&location.server) {
            Some(server) => server,
            None => {
                report.error(format!(
                    "location '{name}' references unknown server '{server}'",
                    server = location.server
                ));
                continue;
            }
        };

        if location.path.trim().is_empty() {
            report.error(format!("location '{name}' has an empty path"));
        } else if !location.path.starts_with('/') {
            report.error(format!(
                "location '{name}' path '{path}' must start with '/'",
                path = location.path
            ));
        }

        if let Some(prefix) = location.strip_prefix.as_deref() {
            if !location.path.starts_with(prefix) {
                report.warn(format!(
                    "location '{name}' strip_prefix '{prefix}' does not match path '{path}'",
                    prefix = prefix,
                    path = location.path
                ));
            }
        }

        match &location.r#type {
            LocationType::Static => {
                if location.upstream.is_some() {
                    report.warn(format!(
                        "location '{name}' is static but defines an upstream"
                    ));
                }

                let root = location
                    .root
                    .as_deref()
                    .unwrap_or_else(|| server.root.as_str());
                if !root.trim().is_empty() && !Path::new(root).exists() {
                    report.warn(format!(
                        "location '{name}' root '{root}' does not exist",
                        root = root
                    ));
                }
            }
            LocationType::Proxy => {
                let Some(upstream) = location.upstream.as_deref() else {
                    report.error(format!(
                        "location '{name}' is proxy but no upstream is configured"
                    ));
                    continue;
                };
                if !cfg.upstream.contains_key(upstream) {
                    report.error(format!(
                        "location '{name}' references unknown upstream '{upstream}'",
                        upstream = upstream
                    ));
                }
            }
        }

        if location.cache == Some(true) {
            if !matches!(&location.r#type, LocationType::Static) {
                report.warn(format!(
                    "location '{name}' enables cache but is not static"
                ));
            }
        }
    }
}
