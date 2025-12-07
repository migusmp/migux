use migux_config::MiguxConfig;
use migux_core::master::Master;
use utils::init_tracing;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = match MiguxConfig::from_file("migux.conf") {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error leyendo migux.conf: {e}");
            eprintln!("Continuando con configuración por defecto...");
            MiguxConfig::default()
        }
    };
    cfg.print();

    let master = Master::new(cfg);
    let _ = master.run().await?;

    Ok(())
}
