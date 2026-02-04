mod global;
mod http;
mod location;
mod migux;
mod server;
mod tls;
mod upstream;
mod validation;

pub use global::GlobalConfig;
pub use http::HttpConfig;
pub use location::{LocationConfig, LocationType};
pub use migux::MiguxConfig;
pub use server::ServerConfig;
pub use tls::TlsConfig;
pub use upstream::{UpstreamConfig, UpstreamHealthConfig, UpstreamServers};
pub use validation::ConfigReport;
