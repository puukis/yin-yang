//! QUIC control channel — session handshake, IDR requests, heartbeats.

use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use quinn::{ClientConfig, Endpoint, TransportConfig};
use rustls::pki_types::ServerName;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use streamd_proto::{
    control::{decode_cursor_datagram, decode_msg, encode_msg},
    input::encode_packet,
    packets::{Codec, ControlMsg, DisplayInfo, InputPacket, SessionRequest, PROTOCOL_VERSION},
};
use tracing::{info, warn};

use crate::cursor::RemoteCursorStore;
use crate::decode::videotoolbox::{RenderFrame, VideoToolboxDecoder};
use crate::input::capture::InputCapture;
use crate::transport::video_rx::VideoReceiver;

#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub display_selector: Option<String>,
    pub list_displays: bool,
}

pub struct ClientSession {
    render_rx: Option<Receiver<RenderFrame>>,
    pub width: u32,
    pub height: u32,
    shutdown: Arc<AtomicBool>,
    connection: Option<quinn::Connection>,
    send: Option<quinn::SendStream>,
    input_capture: Option<InputCapture>,
    input_task: Option<tokio::task::JoinHandle<Result<()>>>,
    control_task: Option<tokio::task::JoinHandle<Result<()>>>,
    datagram_task: Option<tokio::task::JoinHandle<Result<()>>>,
    video_receiver: Option<VideoReceiver>,
    decoder: Option<VideoToolboxDecoder>,
    cursor_store: Arc<RemoteCursorStore>,
}

impl ClientSession {
    pub fn take_render_rx(&mut self) -> Result<Receiver<RenderFrame>> {
        self.render_rx
            .take()
            .context("render receiver was already taken")
    }

    pub fn shutdown_signal(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    pub fn cursor_store(&self) -> Arc<RemoteCursorStore> {
        self.cursor_store.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);

        if let Some(input_capture) = self.input_capture.take() {
            input_capture.release();
        }

        if let Some(mut send) = self.send.take() {
            let _ = send_msg(&mut send, ControlMsg::Goodbye).await;
            let _ = send.finish();
        }

        if let Some(connection) = self.connection.take() {
            connection.close(0u32.into(), b"client shutdown");
        }

        drop(self.decoder.take());
        drop(self.video_receiver.take());

        if let Some(input_task) = self.input_task.take() {
            match input_task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("input task error: {err:#}"),
                Err(err) => warn!("input task join error: {err}"),
            }
        }

        if let Some(control_task) = self.control_task.take() {
            match control_task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("control task error: {err:#}"),
                Err(err) => warn!("control task join error: {err}"),
            }
        }

        if let Some(datagram_task) = self.datagram_task.take() {
            match datagram_task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("cursor datagram task error: {err:#}"),
                Err(err) => warn!("cursor datagram task join error: {err}"),
            }
        }

        Ok(())
    }
}

pub async fn run_client(server_addr: SocketAddr, options: ClientOptions) -> Result<()> {
    let Some(mut session) = connect_client_session(server_addr, options).await? else {
        return Ok(());
    };
    let render_rx = session.take_render_rx()?;
    let render_result = crate::render::metal::VideoRenderer::run(
        render_rx,
        session.width,
        session.height,
        session.cursor_store(),
        session.shutdown_signal(),
    );
    let shutdown_result = session.shutdown().await;
    render_result.and(shutdown_result)
}

pub async fn connect_client_session(
    server_addr: SocketAddr,
    options: ClientOptions,
) -> Result<Option<ClientSession>> {
    let endpoint = make_client_endpoint()?;

    // Try LAN direct first, fall back to provided address (Tailscale / WireGuard IP)
    let conn = endpoint
        .connect(server_addr, "streamd")
        .context("QUIC connect")?
        .await
        .context("QUIC handshake")?;

    info!("connected to {}", conn.remote_address());
    info!(
        "QUIC datagrams {}",
        if conn.max_datagram_size().is_some() {
            "enabled"
        } else {
            "unavailable"
        }
    );

    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;

    send_msg(&mut send, ControlMsg::QueryDisplays).await?;
    let displays = match read_control_msg(&mut recv).await? {
        ControlMsg::AvailableDisplays(displays) => displays,
        ControlMsg::SessionReject(reject) => {
            bail!("server rejected display query: {}", reject.reason)
        }
        other => bail!("unexpected display-list response: {other:?}"),
    };

    if options.list_displays {
        print_displays(&displays);
        let _ = send_msg(&mut send, ControlMsg::Goodbye).await;
        let _ = send.finish();
        return Ok(None);
    }

    let selected_display = select_display(&displays, options.display_selector.as_deref())?;
    let width = if selected_display.width > 0 {
        selected_display.width
    } else {
        1920
    };
    let height = if selected_display.height > 0 {
        selected_display.height
    } else {
        1080
    };

    // Send SessionRequest
    let req = SessionRequest {
        version: PROTOCOL_VERSION,
        max_fps: 60,
        width,
        height,
        preferred_codecs: vec![Codec::H264, Codec::Hevc],
        display_id: Some(selected_display.id.clone()),
    };
    send_msg(&mut send, ControlMsg::SessionRequest(req)).await?;

    // Wait for accept/reject
    let response = read_control_msg(&mut recv).await?;
    let session = match response {
        ControlMsg::SessionAccept(s) => s,
        ControlMsg::SessionReject(r) => bail!("server rejected session: {}", r.reason),
        other => bail!("unexpected response: {other:?}"),
    };

    info!(
        "session accepted: {:?} {}x{}@{}fps, display={} ({}), video udp:{}",
        session.codec,
        session.width,
        session.height,
        session.fps,
        session.selected_display.name,
        session.selected_display.id,
        session.video_udp_port
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let cursor_store = Arc::new(RemoteCursorStore::default());

    let input_stream = conn.open_uni().await.context("open input stream")?;
    let (input_tx, input_rx) = crossbeam_channel::bounded(1024);
    let input_capture = InputCapture::start(input_tx)?;
    let input_runtime = tokio::runtime::Handle::current();
    let input_shutdown = shutdown.clone();
    let input_task = tokio::task::spawn_blocking(move || {
        forward_input_loop(input_rx, input_stream, input_runtime, input_shutdown)
    });

    let video_port = session.client_video_udp_port;
    let (video_receiver, frame_rx) = VideoReceiver::start(video_port, server_addr.ip())?;
    let (decoder, render_rx) = VideoToolboxDecoder::start(frame_rx)?;
    let control_shutdown = shutdown.clone();
    let control_cursor_store = cursor_store.clone();
    let datagram_shutdown = shutdown.clone();
    let datagram_conn = conn.clone();
    let datagram_cursor_store = cursor_store.clone();

    let control_task = tokio::spawn(async move {
        let result = control_loop(recv, control_cursor_store).await;
        control_shutdown.store(true, Ordering::Relaxed);
        if let Err(err) = &result {
            warn!("control loop ended: {err:#}");
        }
        result
    });

    let datagram_task = tokio::spawn(async move {
        let result = cursor_datagram_loop(datagram_conn, datagram_cursor_store).await;
        datagram_shutdown.store(true, Ordering::Relaxed);
        if let Err(err) = &result {
            warn!("cursor datagram loop ended: {err:#}");
        }
        result
    });

    Ok(Some(ClientSession {
        render_rx: Some(render_rx),
        width: session.width,
        height: session.height,
        shutdown,
        connection: Some(conn),
        send: Some(send),
        input_capture: Some(input_capture),
        input_task: Some(input_task),
        control_task: Some(control_task),
        datagram_task: Some(datagram_task),
        video_receiver: Some(video_receiver),
        decoder: Some(decoder),
        cursor_store,
    }))
}

fn select_display(displays: &[DisplayInfo], selector: Option<&str>) -> Result<DisplayInfo> {
    if displays.is_empty() {
        bail!("the server did not report any capture displays");
    }

    let Some(selector) = selector
        .map(str::trim)
        .filter(|selector| !selector.is_empty())
    else {
        return Ok(displays[0].clone());
    };

    if let Ok(index) = selector.parse::<u32>() {
        if let Some(display) = displays.iter().find(|display| display.index == index) {
            return Ok(display.clone());
        }
    }

    if let Some(display) = displays.iter().find(|display| display.id == selector) {
        return Ok(display.clone());
    }

    if let Some(display) = displays
        .iter()
        .find(|display| display.name.eq_ignore_ascii_case(selector))
    {
        return Ok(display.clone());
    }

    if let Some(display) = displays.iter().find(|display| {
        display
            .description
            .as_deref()
            .is_some_and(|description| description.eq_ignore_ascii_case(selector))
    }) {
        return Ok(display.clone());
    }

    bail!(
        "no display matched selector {selector:?}; available selectors are ids, numeric indexes, names, or descriptions"
    )
}

fn print_displays(displays: &[DisplayInfo]) {
    if displays.is_empty() {
        println!("No displays available.");
        return;
    }

    for display in displays {
        let description = display
            .description
            .as_deref()
            .map(|description| format!(" ({description})"))
            .unwrap_or_default();
        println!(
            "[{}] {} {} {}x{}{}",
            display.index, display.id, display.name, display.width, display.height, description
        );
    }
}

async fn control_loop(
    mut recv: quinn::RecvStream,
    cursor_store: Arc<RemoteCursorStore>,
) -> Result<()> {
    loop {
        match read_control_msg(&mut recv).await {
            Ok(ControlMsg::Heartbeat(t)) => {
                info!(
                    "server telemetry: capture_wait={}µs capture_convert={}µs encode={}µs send={}µs pipeline={}µs frames={} idr_count={}",
                    t.avg_capture_wait_us,
                    t.avg_capture_convert_us,
                    t.avg_encode_us,
                    t.avg_send_us,
                    t.avg_pipeline_us,
                    t.frame_count,
                    t.idr_count
                );
            }
            Ok(ControlMsg::CursorShape(shape)) => cursor_store.apply_shape(shape),
            Ok(ControlMsg::CursorState(state)) => cursor_store.apply_state(state),
            Ok(ControlMsg::Goodbye) => {
                info!("server disconnected");
                break;
            }
            Ok(other) => warn!("unexpected: {other:?}"),
            Err(err) => {
                info!("server disconnected: {err:#}");
                break;
            }
        }
    }

    Ok(())
}

async fn cursor_datagram_loop(
    conn: quinn::Connection,
    cursor_store: Arc<RemoteCursorStore>,
) -> Result<()> {
    loop {
        match conn.read_datagram().await {
            Ok(bytes) => {
                if let Some(state) = decode_cursor_datagram(&bytes) {
                    cursor_store.apply_state(state);
                } else {
                    warn!("received invalid cursor datagram ({} bytes)", bytes.len());
                }
            }
            Err(err) => {
                info!("cursor datagram loop stopped: {err}");
                break;
            }
        }
    }

    Ok(())
}

async fn read_control_msg(recv: &mut quinn::RecvStream) -> Result<ControlMsg> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.context("read length")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 1 << 20 {
        bail!("control message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.context("read payload")?;
    let mut framed = Vec::with_capacity(4 + len);
    framed.extend_from_slice(&len_buf);
    framed.extend_from_slice(&buf);
    let (msg, _) = decode_msg(&framed).context("decode")?;
    Ok(msg)
}

async fn send_msg(send: &mut quinn::SendStream, msg: ControlMsg) -> Result<()> {
    let bytes = encode_msg(&msg);
    send.write_all(&bytes).await.context("write")?;
    Ok(())
}

fn forward_input_loop(
    input_rx: Receiver<InputPacket>,
    mut input_stream: quinn::SendStream,
    runtime: tokio::runtime::Handle,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match input_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) => {
                let bytes = encode_packet(&packet);
                runtime
                    .block_on(input_stream.write_all(&bytes))
                    .context("write input packet")?;
            }
            Err(RecvTimeoutError::Timeout) if shutdown.load(Ordering::Relaxed) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    input_stream.finish().context("finish input stream")?;
    Ok(())
}

fn make_client_endpoint() -> Result<Endpoint> {
    // Accept any self-signed cert from the server (personal use — no CA).
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .context("build QUIC client config")?,
    ));
    let mut transport_config = TransportConfig::default();
    transport_config.datagram_receive_buffer_size(Some(64 * 1024));
    transport_config.datagram_send_buffer_size(64 * 1024);
    client_config.transport_config(Arc::new(transport_config));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?).context("create client endpoint")?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// Certificate verifier that accepts any server certificate.
/// Appropriate for a personal LAN/VPN tool where you control both ends.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dh_params: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dhs: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
