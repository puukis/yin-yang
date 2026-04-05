mod cursor;
mod decode;
mod input;
mod render;
mod telemetry;
mod transport;

use anyhow::{Context, Result};
use clap::Parser;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

#[cfg(target_os = "macos")]
use cocoa::{appkit::NSScreen, base::nil};
#[cfg(target_os = "macos")]
use core_graphics::display::CGDisplay;
#[cfg(target_os = "macos")]
use objc::{msg_send, sel, sel_impl};
#[cfg(target_os = "macos")]
use std::net::SocketAddr;

#[derive(Debug, Parser)]
#[command(
    name = "yang",
    version,
    about = "Low-latency macOS QUIC remote desktop client."
)]
struct Args {
    /// Address of the Yin server.
    #[arg(value_name = "SERVER_ADDR", default_value = "127.0.0.1:9000")]
    server_addr: std::net::SocketAddr,

    /// Select a display by index, stable id, exact name, or exact description.
    #[arg(long, value_name = "ID|INDEX|NAME")]
    display: Option<String>,

    /// List displays exported by the server and exit.
    #[arg(long)]
    list_displays: bool,

    /// Maximum stream framerate to request from the server.
    #[arg(long, value_name = "FPS")]
    max_fps: Option<u8>,

    /// Minimum stream framerate the adaptive controller may fall back to.
    #[arg(long, value_name = "FPS")]
    min_fps: Option<u8>,

    /// Lower bitrate bound for adaptive control, in megabits per second.
    #[arg(long, value_name = "MBPS")]
    min_bitrate_mbps: Option<u32>,

    /// Upper bitrate bound for adaptive control, in megabits per second.
    #[arg(long, value_name = "MBPS")]
    max_bitrate_mbps: Option<u32>,

    /// Disable automatic bitrate/FPS adaptation and hold the requested targets fixed.
    /// Fixed-rate mode is the default.
    #[arg(long, conflicts_with = "adaptive_rate")]
    fixed_rate: bool,

    /// Enable automatic bitrate/FPS adaptation for this session.
    #[arg(long, conflicts_with = "fixed_rate")]
    adaptive_rate: bool,

    /// Enable client-side optical-flow frame interpolation.
    ///
    /// Synthesises a mid-frame between every pair of consecutive decoded
    /// frames using block-matching optical flow and a GPU warp-blend pass.
    /// Doubles the apparent frame rate (e.g. 60 fps → 120 fps on a ProMotion
    /// display) at the cost of ~3 ms additional GPU compute per source frame.
    /// Best enabled on displays whose refresh rate is at least twice the
    /// stream frame rate.
    #[arg(long)]
    interpolate: bool,

    /// Tracing filter passed to tracing-subscriber.
    #[arg(long, env = "RUST_LOG", default_value = "yang=debug,info")]
    log_filter: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    install_rustls_crypto_provider()?;

    tracing_subscriber::fmt()
        .with_env_filter(args.log_filter.clone())
        .init();

    let (server_addr, options) = client_options_from_args(args);

    info!("yang connecting to {server_addr}");

    #[cfg(target_os = "macos")]
    if !options.list_displays {
        return run_macos_client(server_addr, options);
    }

    let runtime = build_runtime()?;
    runtime.block_on(transport::control::run_client(server_addr, options))
}

fn client_options_from_args(
    args: Args,
) -> (std::net::SocketAddr, transport::control::ClientOptions) {
    let adaptive_streaming = args.adaptive_rate && !args.fixed_rate;
    let max_fps = args
        .max_fps
        .unwrap_or_else(default_requested_max_fps)
        .clamp(1, 120);
    let min_fps = args
        .min_fps
        .unwrap_or(if max_fps >= 30 { 30 } else { max_fps })
        .clamp(1, max_fps);
    let options = transport::control::ClientOptions {
        client_session_id: new_client_session_id(),
        adaptive_streaming,
        list_displays: args.list_displays,
        display_selector: args.display,
        max_fps,
        min_fps,
        min_bitrate_bps: mbps_to_bps(args.min_bitrate_mbps),
        max_bitrate_bps: mbps_to_bps(args.max_bitrate_mbps),
        interpolate: args.interpolate,
    };
    (args.server_addr, options)
}

fn mbps_to_bps(mbps: Option<u32>) -> u32 {
    mbps.unwrap_or(0).saturating_mul(1_000_000)
}

fn new_client_session_id() -> String {
    let now_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("yang-{}-{now_us}", std::process::id())
}

#[cfg(target_os = "macos")]
fn default_requested_max_fps() -> u8 {
    nsscreen_maximum_frames_per_second()
        .or_else(cg_main_display_refresh_rate_hz)
        .unwrap_or(60)
        .clamp(30, 120)
}

#[cfg(not(target_os = "macos"))]
fn default_requested_max_fps() -> u8 {
    60
}

#[cfg(target_os = "macos")]
fn nsscreen_maximum_frames_per_second() -> Option<u8> {
    unsafe {
        let screen = NSScreen::mainScreen(nil);
        if screen == nil {
            return None;
        }

        let max_fps: isize = msg_send![screen, maximumFramesPerSecond];
        (max_fps > 0).then_some(max_fps as u8)
    }
}

#[cfg(target_os = "macos")]
fn cg_main_display_refresh_rate_hz() -> Option<u8> {
    let refresh_rate = CGDisplay::main().display_mode()?.refresh_rate();
    (refresh_rate >= 1.0).then_some(refresh_rate.round().clamp(1.0, 120.0) as u8)
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
    let interpolate = options.interpolate;
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
        session.cursor_store(),
        session.shutdown_signal(),
        session.telemetry(),
        interpolate,
    );
    let shutdown_result = runtime.block_on(session.shutdown());

    render_result.and(shutdown_result)
}

#[cfg(test)]
mod tests {
    use super::{client_options_from_args, Args};
    use clap::Parser;

    #[test]
    fn parses_default_values() {
        let args = Args::try_parse_from(["yang"]).expect("args should parse");
        let (server_addr, options) = client_options_from_args(args);
        assert_eq!(server_addr.to_string(), "127.0.0.1:9000");
        assert!(options.display_selector.is_none());
        assert!(!options.list_displays);
        assert!(!options.client_session_id.is_empty());
        assert!(!options.adaptive_streaming);
        assert_eq!(options.max_fps, super::default_requested_max_fps());
        assert_eq!(
            options.min_fps,
            if options.max_fps >= 30 {
                30
            } else {
                options.max_fps
            }
        );
        assert_eq!(options.min_bitrate_bps, 0);
        assert_eq!(options.max_bitrate_bps, 0);
    }

    #[test]
    fn parses_display_selection() {
        let args = Args::try_parse_from([
            "yang",
            "192.168.1.50:9000",
            "--display",
            "wayland:68",
            "--list-displays",
            "--max-fps",
            "120",
            "--min-fps",
            "48",
            "--min-bitrate-mbps",
            "12",
            "--max-bitrate-mbps",
            "40",
            "--adaptive-rate",
            "--log-filter",
            "info,yang=trace",
        ])
        .expect("args should parse");
        let (server_addr, options) = client_options_from_args(args);
        assert_eq!(server_addr.to_string(), "192.168.1.50:9000");
        assert_eq!(options.display_selector.as_deref(), Some("wayland:68"));
        assert!(options.list_displays);
        assert!(options.adaptive_streaming);
        assert_eq!(options.max_fps, 120);
        assert_eq!(options.min_fps, 48);
        assert_eq!(options.min_bitrate_bps, 12_000_000);
        assert_eq!(options.max_bitrate_bps, 40_000_000);
    }

    #[test]
    fn fixed_rate_flag_keeps_adaptation_disabled() {
        let args = Args::try_parse_from(["yang", "--fixed-rate"]).expect("args should parse");
        let (_, options) = client_options_from_args(args);
        assert!(!options.adaptive_streaming);
    }
}
