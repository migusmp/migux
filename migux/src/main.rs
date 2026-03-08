use std::env;

use migux_config::MiguxConfig;
use migux_core::master::Master;
use utils::init_tracing;

/// Obtiene la ruta del archivo de configuración:
/// 1) variable de entorno MIGUX_CONFIG
/// 2) argumento -c o --config en la línea de comandos
/// 3) por defecto "migux.conf"
fn config_path() -> String {
    if let Ok(path) = env::var("MIGUX_CONFIG") {
        if !path.is_empty() {
            return path;
        }
    }
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if (arg == "-c" || arg == "--config") && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            return path.to_string();
        }
        i += 1;
    }
    "migux.conf".to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config_path = config_path();
    let cfg = match MiguxConfig::from_file(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error reading migux config from '{}': {e}", config_path);
            eprintln!("Using default config...");
            MiguxConfig::default()
        }
    };

    let report = cfg.validate();
    for warning in report.warnings() {
        eprintln!("WARNING: {warning}");
    }
    if report.has_errors() {
        eprintln!("Invalid configuration:");
        for error in report.errors() {
            eprintln!("  - {error}");
        }
        return Err(anyhow::anyhow!("invalid configuration"));
    }

    let master = Master::new(cfg);
    master.run().await?;

    Ok(())
}
