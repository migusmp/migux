use std::collections::HashMap;

use migux_config::{LocationConfig, LocationType, MiguxConfig, ServerConfig};

pub mod master;

pub type ListenAddr = String;

#[derive(Debug, Clone)]
pub struct ServerRuntime {
    /// Nombre lógico: "main", "api", etc. (clave del HashMap)
    pub name: String,
    pub config: ServerConfig,
    pub locations: Vec<LocationConfig>,
}

pub type ServersByListen = HashMap<ListenAddr, Vec<ServerRuntime>>;

pub fn build_servers_by_listen(cfg: &MiguxConfig) -> ServersByListen {
    let mut map: ServersByListen = HashMap::new();

    for (server_name, server_cfg) in &cfg.servers {
        let listen_key = server_cfg.listen.clone(); // "0.0.0.0:8080"

        // Filtramos locations que pertenecen a este server (`location.*` con server = "main")
        let mut locations: Vec<LocationConfig> = cfg
            .location
            .values()
            .filter(|loc| loc.server == *server_name)
            .cloned()
            .collect();

        // 👇 SI NO HAY LOCATIONS, CREAMOS UNA POR DEFECTO
        if locations.is_empty() {
            locations.push(LocationConfig {
                server: server_name.clone(),
                path: "/".into(),
                r#type: LocationType::Static,
                root: Some(server_cfg.root.clone()),
                index: Some(server_cfg.index.clone()),
                upstream: None,
                strip_prefix: None,
            });
        }

        map.entry(listen_key).or_default().push(ServerRuntime {
            name: server_name.clone(),
            config: server_cfg.clone(),
            locations,
        });
    }

    map
}

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
