use std::collections::HashMap;

use migux_config::{LocationConfig, LocationType, MiguxConfig};

use crate::{
    structs::ServerRuntime,
    types::{ServersByListen, TlsListenConfig, TlsServersByListen},
};

pub mod http2;
pub mod master;
pub mod structs;
pub mod types;
pub mod worker;

/// Build a map of TCP listen address -> servers bound to that address.
pub fn build_servers_by_listen(cfg: &MiguxConfig) -> ServersByListen {
    let mut map: ServersByListen = HashMap::new();

    /*                                                              [server.main]
     * server_name contains a domains name              =>          listen = "0.0.0.0:8080"
     * server_cfg contains full config of a server      =>          server_name = "localhost"
     *                                                  =>          root = "/var/www/public"
     *                                                  =>          index = "index.html"
     * */
    for (server_name, server_cfg) in &cfg.servers {
        let listen_key = server_cfg.listen.clone(); // "Ej: 0.0.0.0:8080"

        /*
            * Filter locations that belong
            * to this server (`location.*` with server = "main")

              [location.main_root]
               server = "main"
               path = "/"
               type = "static"
               root = "var/www/public"
               index = "index.html"

        */
        let mut locations: Vec<LocationConfig> = cfg
            .location
            .values()
            .filter(|loc| loc.server == *server_name)
            .cloned()
            .collect();

        // If locations is empty, create a default location for this server
        if locations.is_empty() {
            locations.push(LocationConfig {
                server: server_name.clone(),
                path: "/".into(),
                r#type: LocationType::Static,
                root: Some(server_cfg.root.clone()),
                index: Some(server_cfg.index.clone()),
                upstream: None,
                strip_prefix: None,
                cache: None,
            });
        }

        // Add the ServerRuntime of each server to the HashMap
        map.entry(listen_key).or_default().push(ServerRuntime::new(
            server_name.clone(),
            server_cfg.clone(),
            locations,
        ));
    }
    map
}

/// Build a map of TLS listen address -> servers bound to that address.
pub fn build_tls_servers_by_listen(cfg: &MiguxConfig) -> TlsServersByListen {
    let mut map: TlsServersByListen = HashMap::new();

    for (server_name, server_cfg) in &cfg.servers {
        let Some(tls_cfg) = &server_cfg.tls else {
            continue;
        };

        let listen_key = tls_cfg.listen.clone();

        let mut locations: Vec<LocationConfig> = cfg
            .location
            .values()
            .filter(|loc| loc.server == *server_name)
            .cloned()
            .collect();

        // If locations is empty, create a default location for this server
        if locations.is_empty() {
            locations.push(LocationConfig {
                server: server_name.clone(),
                path: "/".into(),
                r#type: LocationType::Static,
                root: Some(server_cfg.root.clone()),
                index: Some(server_cfg.index.clone()),
                upstream: None,
                strip_prefix: None,
                cache: None,
            });
        }

        let runtime = ServerRuntime::new(server_name.clone(), server_cfg.clone(), locations);

        map.entry(listen_key.clone())
            .and_modify(|entry| {
                entry.servers.push(runtime.clone());
            })
            .or_insert_with(|| TlsListenConfig::new(listen_key, tls_cfg.clone(), vec![runtime]));
    }

    map
}
