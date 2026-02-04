use std::{fs::File, io::BufReader, sync::Arc};

use migux_config::TlsConfig;
use tokio_rustls::{rustls, TlsAcceptor};
use tracing::warn;

use crate::types::TlsListenConfig;

pub(crate) fn tls_listener_ready(listen_addr: &str, tls_cfg: &TlsListenConfig) -> bool {
    if tls_cfg.tls.cert_path.is_empty() || tls_cfg.tls.key_path.is_empty() {
        warn!(
            target: "migux::master",
            listen = %listen_addr,
            "TLS config missing cert/key path; skipping TLS listener"
        );
        return false;
    }

    if tls_cfg.servers.is_empty() {
        warn!(
            target: "migux::master",
            listen = %listen_addr,
            "TLS config missing servers; skipping TLS listener"
        );
        return false;
    }

    true
}

/// Build a TLS acceptor from configured certificate/key paths.
pub(crate) fn load_tls_acceptor(cfg: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(&cfg.cert_path)?;
    let key = load_private_key(&cfg.key_path)?;

    let mut config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("Invalid TLS config: {e}"))?;
    if cfg.http2 {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    } else {
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Load PEM-encoded certificates from disk.
fn load_certs(path: &str) -> anyhow::Result<Vec<rustls::Certificate>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)?;
    if certs.is_empty() {
        anyhow::bail!("No certificates found in {}", path);
    }
    Ok(certs.into_iter().map(rustls::Certificate).collect())
}

/// Load a PEM-encoded private key (PKCS8 or RSA) from disk.
fn load_private_key(path: &str) -> anyhow::Result<rustls::PrivateKey> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let keys = rustls_pemfile::pkcs8_private_keys(&mut reader)?;
    if let Some(key) = keys.into_iter().next() {
        return Ok(rustls::PrivateKey(key));
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let keys = rustls_pemfile::rsa_private_keys(&mut reader)?;
    if let Some(key) = keys.into_iter().next() {
        return Ok(rustls::PrivateKey(key));
    }

    anyhow::bail!("No private keys found in {}", path);
}
