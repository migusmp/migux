use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

pub fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,migux=debug,migux_proxy=debug,migux_http=debug"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .compact()
                .with_target(true)
                .with_thread_ids(false),
        )
        .init();
}
