use std::collections::HashMap;

use migux_config::{LocationConfig, LocationType, MiguxConfig};

use crate::{structs::ServerRuntime, types::ServersByListen};

pub mod master;
pub mod structs;
pub mod types;
pub mod worker;

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
               root = "/home/migus/var/www/public"
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
