use migux_config::MiguxConfig;
use migux_core::master::Master;
use utils::init_tracing;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = match MiguxConfig::from_file("migux.conf") {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error reading migux config from migux.conf: {e}");
            eprintln!("Using default config...");
            MiguxConfig::default()
        }
    };

    let master = Master::new(cfg);
    master.run().await?;

    Ok(())
}
