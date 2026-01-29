use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use migux_config::{UpstreamConfig, UpstreamServers};

/// =======================================================
/// UPSTREAM CONFIG PARSING / NORMALIZATION
/// =======================================================
///
/// Parse possible `server` formats:
///
/// Soporta:
/// - "127.0.0.1:3000"
/// - ["127.0.0.1:3000", "127.0.0.1:3001"]
///
/// (Esto parece compat con un "raw string" que a veces te llega con [] en texto)
fn parse_servers_from_one(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    // Caso "array" como texto: ["a","b"]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .split(',')
            .filter_map(|part| {
                // limpia comillas y espacios
                let part = part.trim().trim_matches('"');
                if part.is_empty() {
                    None
                } else {
                    Some(part.to_string())
                }
            })
            .collect()
    } else {
        // Caso normal: un solo server
        vec![trimmed.to_string()]
    }
}

/// Normalize `UpstreamConfig` into a Vec<String> of "host:port"
///
/// Tu config permite:
/// - UpstreamServers::One("...")
/// - UpstreamServers::Many(vec![...])
///
/// Esta funcion deja SIEMPRE un Vec<String> usable.
/// Si queda vacio, error.
fn normalize_servers(cfg: &UpstreamConfig) -> anyhow::Result<Vec<String>> {
    let servers: Vec<String> = match &cfg.server {
        UpstreamServers::One(s) => parse_servers_from_one(s),
        UpstreamServers::Many(list) => list.clone(),
    };

    if servers.is_empty() {
        anyhow::bail!("Upstream has no configured servers");
    }

    Ok(servers)
}

/// Returns upstream servers in rr order + fallback order
///
/// Objetivo:
/// - Si estrategia != round_robin => devuelve tal cual
/// - Si round_robin => rota la lista para que el primero sea el "elegido"
///
/// Resultado: Vec ordenado:
///   [primero_elegido, segundo, tercero, ...]
/// Y asi en el `for` intentas el primero y si falla vas probando el resto (fallback).
pub(super) fn choose_upstream_addrs_rr_order(
    counters: &DashMap<String, AtomicUsize>,
    upstream_name: &str,
    upstream_cfg: &UpstreamConfig,
) -> anyhow::Result<Vec<String>> {
    let servers = normalize_servers(upstream_cfg)?;

    // Si solo hay uno, no hay nada que elegir
    if servers.len() == 1 {
        return Ok(servers);
    }

    // Por defecto: "single" si falta
    let strategy = upstream_cfg.strategy.as_deref().unwrap_or("single");
    if strategy != "round_robin" {
        return Ok(servers);
    }

    // Counter global por upstream_name
    let entry = counters
        .entry(upstream_name.to_string())
        .or_insert_with(|| AtomicUsize::new(0));

    // idx: 0,1,2,3...
    let idx = entry.fetch_add(1, Ordering::Relaxed);
    // start: idx mod N => posicion de inicio para rotar
    let start = idx % servers.len();

    // Construye lista rotada:
    // start..end, luego 0..start
    let mut ordered = Vec::with_capacity(servers.len());
    for i in 0..servers.len() {
        let pos = (start + i) % servers.len();
        ordered.push(servers[pos].clone());
    }

    Ok(ordered)
}
