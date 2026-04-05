mod capture;
mod encode;
mod input;
mod pipeline;
mod transport;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use yin::cli::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    install_rustls_crypto_provider()?;

    tracing_subscriber::fmt()
        .with_env_filter(args.log_filter.clone())
        .init();

    info!("yin starting on {}", args.bind_addr);
    transport::control::run_server(args.bind_addr).await
}

fn install_rustls_crypto_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| {
            anyhow::anyhow!(
                "failed to install rustls ring CryptoProvider; another provider may already be active"
            )
        })
        .context("install rustls CryptoProvider")?;
    Ok(())
}
