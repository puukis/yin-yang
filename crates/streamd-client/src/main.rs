mod decode;
mod input;
mod render;
mod transport;

use anyhow::{bail, Context, Result};
use tracing::info;

#[cfg(target_os = "macos")]
use std::net::SocketAddr;

fn main() -> Result<()> {
    install_rustls_crypto_provider()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "streamd_client=debug,info".into()),
        )
        .init();

    let Some((server_addr, options)) = parse_args()? else {
        return Ok(());
    };

    info!("streamd-client connecting to {server_addr}");

    #[cfg(target_os = "macos")]
    if !options.list_displays {
        return run_macos_client(server_addr, options);
    }

    let runtime = build_runtime()?;
    runtime.block_on(transport::control::run_client(server_addr, options))
}

fn parse_args() -> Result<Option<(std::net::SocketAddr, transport::control::ClientOptions)>> {
    let mut server_addr = None;
    let mut options = transport::control::ClientOptions::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                return Ok(None);
            }
            "--list-displays" => {
                options.list_displays = true;
            }
            "--display" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--display requires a value"))?;
                options.display_selector = Some(value);
            }
            _ if arg.starts_with("--display=") => {
                options.display_selector = Some(arg["--display=".len()..].to_string());
            }
            _ if arg.starts_with('-') => bail!("unknown flag: {arg}"),
            _ if server_addr.is_none() => {
                server_addr = Some(arg.parse()?);
            }
            _ => bail!("unexpected extra argument: {arg}"),
        }
    }

    let server_addr = server_addr.unwrap_or_else(|| {
        "127.0.0.1:9000"
            .parse()
            .expect("default server address is valid")
    });
    Ok(Some((server_addr, options)))
}

fn print_usage() {
    println!("Usage: streamd-client [server_addr] [--display <id|index|name>] [--list-displays]");
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

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime")
}

#[cfg(target_os = "macos")]
fn run_macos_client(
    server_addr: SocketAddr,
    options: transport::control::ClientOptions,
) -> Result<()> {
    let runtime = build_runtime()?;
    let Some(mut session) = runtime.block_on(transport::control::connect_client_session(
        server_addr,
        options,
    ))?
    else {
        return Ok(());
    };

    let render_rx = session.take_render_rx()?;
    info!("starting macOS Metal renderer on the main thread");
    let render_result = render::metal::VideoRenderer::run(
        render_rx,
        session.width,
        session.height,
        session.shutdown_signal(),
    );
    let shutdown_result = runtime.block_on(session.shutdown());

    render_result.and(shutdown_result)
}
