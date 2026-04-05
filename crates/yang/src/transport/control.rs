//! QUIC control channel — session handshake, IDR requests, heartbeats.
//!
//! The client keeps one logical session alive across transport reconnects:
//!
//! 1. A long-lived supervisor owns the stable `client_session_id`, render
//!    output queue, cursor store, and reconnect policy.
//! 2. Each live QUIC connection owns per-connection tasks for control,
//!    datagram dispatch, input forwarding, and decode.
//! 3. When the connection drops, the supervisor tears down the old decoder,
//!    releases local input capture, and reconnects with the same logical
//!    session parameters.

use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use quinn::{ClientConfig, Endpoint, TransportConfig};
use rustls::pki_types::ServerName;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};
use yin_yang_proto::{
    control::{
        decode_cursor_datagram, decode_msg, encode_input_datagram, encode_msg, DATAGRAM_TAG_CURSOR,
        DATAGRAM_TAG_VIDEO,
    },
    input::encode_packet,
    packets::{
        Codec, ControlMsg, DisplayInfo, InputEvent, InputPacket, SessionAccept, SessionRequest,
        PROTOCOL_VERSION,
    },
};

use crate::{
    cursor::RemoteCursorStore,
    decode::videotoolbox::{RenderFrame, VideoToolboxDecoder},
    input::capture::InputCapture,
    telemetry::{ClientTelemetryAccumulator, SharedClientTelemetry},
    transport::video_rx::{DecodedFrame, ReassemblyLoss, VideoFrameReassembler},
};

/// Minimum interval between IDR fallback requests.
///
/// `FrameLost` is sent immediately on every loss event to trigger fast ref
/// invalidation.  A `RequestIdr` is also sent if this interval has elapsed
/// since the last one — providing a safety net for sustained loss where
/// `invalidateRefFrames` cannot help because the entire reference chain is
/// corrupt.
const IDR_FALLBACK_MIN_INTERVAL: Duration = Duration::from_millis(250);
const CLIENT_TELEMETRY_INTERVAL: Duration = Duration::from_millis(500);
const QUIC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const QUIC_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const RECONNECT_BACKOFFS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
const RECONNECT_BUDGET: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub client_session_id: String,
    pub adaptive_streaming: bool,
    pub display_selector: Option<String>,
    pub list_displays: bool,
    pub max_fps: u8,
    pub min_fps: u8,
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub interpolate: bool,
}

pub struct ClientSession {
    render_rx: Option<Receiver<RenderFrame>>,
    pub width: u32,
    pub height: u32,
    shutdown: Arc<AtomicBool>,
    supervisor_task: Option<tokio::task::JoinHandle<Result<()>>>,
    cursor_store: Arc<RemoteCursorStore>,
    telemetry: SharedClientTelemetry,
}

struct ActiveConnection {
    connection: Option<quinn::Connection>,
    control_tx: Option<UnboundedSender<ControlMsg>>,
    input_capture: Option<InputCapture>,
    input_task: Option<tokio::task::JoinHandle<Result<()>>>,
    control_send_task: Option<tokio::task::JoinHandle<Result<()>>>,
    control_task: Option<tokio::task::JoinHandle<Result<()>>>,
    datagram_task: Option<tokio::task::JoinHandle<Result<()>>>,
    decoder: Option<VideoToolboxDecoder>,
    connection_shutdown: Arc<AtomicBool>,
    disconnect_rx: UnboundedReceiver<()>,
}

struct ConnectedSession {
    active: ActiveConnection,
    accepted: SessionAccept,
}

enum SupervisorEvent {
    Shutdown,
    Disconnected,
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

    pub fn telemetry(&self) -> SharedClientTelemetry {
        self.telemetry.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(supervisor_task) = self.supervisor_task.take() {
            match supervisor_task.await {
                Ok(result) => result?,
                Err(err) => bail!("session supervisor join error: {err}"),
            }
        }
        Ok(())
    }
}

impl ActiveConnection {
    async fn wait_for_event(&mut self, shutdown: &Arc<AtomicBool>) -> SupervisorEvent {
        let mut poll = tokio::time::interval(Duration::from_millis(50));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = poll.tick() => {
                    if shutdown.load(Ordering::Relaxed) {
                        return SupervisorEvent::Shutdown;
                    }
                }
                event = self.disconnect_rx.recv() => {
                    let _ = event;
                    return if shutdown.load(Ordering::Relaxed) {
                        SupervisorEvent::Shutdown
                    } else {
                        SupervisorEvent::Disconnected
                    };
                }
            }
        }
    }

    async fn shutdown(mut self, graceful: bool) -> Result<()> {
        self.connection_shutdown.store(true, Ordering::Relaxed);

        if let Some(input_capture) = self.input_capture.take() {
            input_capture.release();
        }

        if graceful {
            if let Some(control_tx) = self.control_tx.as_ref() {
                let _ = control_tx.send(ControlMsg::Goodbye);
            }
        }

        drop(self.control_tx.take());

        if let Some(connection) = self.connection.take() {
            connection.close(
                if graceful { 0u32 } else { 1u32 }.into(),
                if graceful {
                    b"client shutdown".as_slice()
                } else {
                    b"connection cleanup".as_slice()
                },
            );
        }

        drop(self.decoder.take());

        join_task("input", self.input_task.take()).await;
        join_task("control", self.control_task.take()).await;
        join_task("datagram", self.datagram_task.take()).await;
        join_task("control-send", self.control_send_task.take()).await;

        Ok(())
    }
}

async fn join_task(name: &str, task: Option<tokio::task::JoinHandle<Result<()>>>) {
    let Some(task) = task else {
        return;
    };

    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("{name} task error: {err:#}"),
        Err(err) => warn!("{name} task join error: {err}"),
    }
}

pub async fn run_client(server_addr: SocketAddr, options: ClientOptions) -> Result<()> {
    let interpolate = options.interpolate;
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
        session.telemetry(),
        interpolate,
    );
    let shutdown_result = session.shutdown().await;
    render_result.and(shutdown_result)
}

/// List displays available on the server without starting a session.
pub async fn list_displays(server_addr: SocketAddr) -> Result<Vec<DisplayInfo>> {
    let endpoint = make_client_endpoint()?;
    let conn = connect_to_server(&endpoint, server_addr).await?;
    let (mut send, _recv, displays) = open_control_and_query_displays(&conn).await?;
    let _ = send_msg(&mut send, ControlMsg::Goodbye).await;
    let _ = send.finish();
    conn.close(0u32.into(), b"list displays");
    Ok(displays)
}

pub async fn connect_client_session(
    server_addr: SocketAddr,
    options: ClientOptions,
) -> Result<Option<ClientSession>> {
    let endpoint = make_client_endpoint()?;

    if options.list_displays {
        let conn = connect_to_server(&endpoint, server_addr).await?;
        let (mut send, _recv, displays) = open_control_and_query_displays(&conn).await?;
        print_displays(&displays);
        let _ = send_msg(&mut send, ControlMsg::Goodbye).await;
        let _ = send.finish();
        conn.close(0u32.into(), b"list displays");
        return Ok(None);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let cursor_store = Arc::new(RemoteCursorStore::default());
    let telemetry = ClientTelemetryAccumulator::shared();
    let (render_tx, render_rx) = crossbeam_channel::bounded(1);
    let callback_render_rx = render_rx.clone();

    let connected = connect_active_connection(
        &endpoint,
        server_addr,
        &options,
        cursor_store.clone(),
        render_tx.clone(),
        callback_render_rx.clone(),
        telemetry.clone(),
    )
    .await?;

    let width = connected.accepted.width;
    let height = connected.accepted.height;
    let supervisor_task = tokio::spawn(supervise_session(
        endpoint,
        server_addr,
        options,
        cursor_store.clone(),
        render_tx,
        callback_render_rx,
        telemetry.clone(),
        shutdown.clone(),
        connected.active,
    ));

    Ok(Some(ClientSession {
        render_rx: Some(render_rx),
        width,
        height,
        shutdown,
        supervisor_task: Some(supervisor_task),
        cursor_store,
        telemetry,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn supervise_session(
    endpoint: Endpoint,
    server_addr: SocketAddr,
    options: ClientOptions,
    cursor_store: Arc<RemoteCursorStore>,
    render_tx: Sender<RenderFrame>,
    callback_render_rx: Receiver<RenderFrame>,
    telemetry: SharedClientTelemetry,
    shutdown: Arc<AtomicBool>,
    mut active: ActiveConnection,
) -> Result<()> {
    loop {
        match active.wait_for_event(&shutdown).await {
            SupervisorEvent::Shutdown => {
                active.shutdown(true).await?;
                return Ok(());
            }
            SupervisorEvent::Disconnected => {
                info!("connection dropped; starting reconnect attempts");
                active.shutdown(false).await?;
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let reconnect_deadline = Instant::now() + RECONNECT_BUDGET;
        let mut attempt = 0usize;
        let mut reconnected = None;

        while !shutdown.load(Ordering::Relaxed) && Instant::now() < reconnect_deadline {
            let backoff = RECONNECT_BACKOFFS[attempt.min(RECONNECT_BACKOFFS.len() - 1)];
            tokio::time::sleep(backoff).await;
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }

            match connect_active_connection(
                &endpoint,
                server_addr,
                &options,
                cursor_store.clone(),
                render_tx.clone(),
                callback_render_rx.clone(),
                telemetry.clone(),
            )
            .await
            {
                Ok(connected) => {
                    info!(
                        "reconnected: {:?} {}x{}@{}fps display={} ({})",
                        connected.accepted.codec,
                        connected.accepted.width,
                        connected.accepted.height,
                        connected.accepted.fps,
                        connected.accepted.selected_display.name,
                        connected.accepted.selected_display.id,
                    );
                    reconnected = Some(connected.active);
                    break;
                }
                Err(err) => {
                    warn!(
                        "reconnect attempt {} failed: {err:#}",
                        attempt.saturating_add(1)
                    );
                }
            }

            attempt = attempt.saturating_add(1);
        }

        let Some(next_active) = reconnected else {
            warn!(
                "reconnect budget exhausted after {:?}; closing session",
                RECONNECT_BUDGET
            );
            shutdown.store(true, Ordering::Relaxed);
            return Ok(());
        };

        active = next_active;
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_active_connection(
    endpoint: &Endpoint,
    server_addr: SocketAddr,
    options: &ClientOptions,
    cursor_store: Arc<RemoteCursorStore>,
    render_tx: Sender<RenderFrame>,
    callback_render_rx: Receiver<RenderFrame>,
    telemetry: SharedClientTelemetry,
) -> Result<ConnectedSession> {
    let conn = connect_to_server(endpoint, server_addr).await?;
    let (mut send, mut recv, displays) = open_control_and_query_displays(&conn).await?;

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

    let request = SessionRequest {
        version: PROTOCOL_VERSION,
        client_session_id: options.client_session_id.clone(),
        adaptive_streaming: options.adaptive_streaming,
        max_fps: options.max_fps,
        min_fps: options.min_fps,
        min_bitrate_bps: options.min_bitrate_bps,
        max_bitrate_bps: options.max_bitrate_bps,
        width,
        height,
        preferred_codecs: vec![Codec::H264, Codec::Hevc],
        display_id: Some(selected_display.id.clone()),
    };
    info!(
        "requesting session {}x{} at up to {}fps for display={} ({}) adaptive={}",
        width,
        height,
        request.max_fps,
        selected_display.name,
        selected_display.id,
        if request.adaptive_streaming {
            "on"
        } else {
            "off"
        }
    );
    send_msg(&mut send, ControlMsg::SessionRequest(request)).await?;

    let response = read_control_msg(&mut recv).await?;
    let accepted = match response {
        ControlMsg::SessionAccept(session) => session,
        ControlMsg::SessionReject(reject) => bail!("server rejected session: {}", reject.reason),
        other => bail!("unexpected session response: {other:?}"),
    };

    info!(
        "session accepted: {:?} {}x{}@{}fps display={} ({}) video: QUIC datagrams",
        accepted.codec,
        accepted.width,
        accepted.height,
        accepted.fps,
        accepted.selected_display.name,
        accepted.selected_display.id,
    );

    let (disconnect_tx, disconnect_rx) = unbounded_channel::<()>();
    let (control_tx, control_rx) = unbounded_channel::<ControlMsg>();
    let connection_shutdown = Arc::new(AtomicBool::new(false));

    let control_send_notify = disconnect_tx.clone();
    let control_send_telemetry = telemetry.clone();
    let control_send_task = tokio::spawn(async move {
        let result = control_send_loop(send, control_rx, control_send_telemetry).await;
        let _ = control_send_notify.send(());
        result
    });

    let input_stream = conn.open_uni().await.context("open input stream")?;
    let (input_tx, input_rx) = crossbeam_channel::bounded(1024);
    let input_capture = InputCapture::start(input_tx)?;
    let input_runtime = tokio::runtime::Handle::current();
    let input_shutdown = connection_shutdown.clone();
    let input_conn = conn.clone();
    let input_task = tokio::task::spawn_blocking(move || {
        forward_input_loop(
            input_rx,
            input_stream,
            input_conn,
            input_runtime,
            input_shutdown,
        )
    });

    let (frame_tx, frame_rx) = crossbeam_channel::bounded::<DecodedFrame>(8);
    let datagram_conn = conn.clone();
    let datagram_cursor_store = cursor_store.clone();
    let datagram_control_tx = control_tx.clone();
    let datagram_telemetry = telemetry.clone();
    let datagram_notify = disconnect_tx.clone();
    let datagram_task = tokio::spawn(async move {
        let result = datagram_dispatch_loop(
            datagram_conn,
            datagram_cursor_store,
            datagram_control_tx,
            frame_tx,
            datagram_telemetry,
        )
        .await;
        let _ = datagram_notify.send(());
        result
    });

    let decoder = VideoToolboxDecoder::start_with_output(frame_rx, render_tx, callback_render_rx)?;

    let control_cursor_store = cursor_store.clone();
    let control_notify = disconnect_tx;
    let control_task = tokio::spawn(async move {
        let result = control_loop(recv, control_cursor_store).await;
        let _ = control_notify.send(());
        result
    });

    Ok(ConnectedSession {
        active: ActiveConnection {
            connection: Some(conn),
            control_tx: Some(control_tx),
            input_capture: Some(input_capture),
            input_task: Some(input_task),
            control_send_task: Some(control_send_task),
            control_task: Some(control_task),
            datagram_task: Some(datagram_task),
            decoder: Some(decoder),
            connection_shutdown,
            disconnect_rx,
        },
        accepted,
    })
}

async fn connect_to_server(
    endpoint: &Endpoint,
    server_addr: SocketAddr,
) -> Result<quinn::Connection> {
    let conn = endpoint
        .connect(server_addr, "yin-yang")
        .context("QUIC connect")?
        .await
        .context("QUIC handshake")?;

    info!("connected to {}", conn.remote_address());
    info!(
        "QUIC datagrams: max payload {} bytes",
        conn.max_datagram_size()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unavailable".into())
    );

    Ok(conn)
}

async fn open_control_and_query_displays(
    conn: &quinn::Connection,
) -> Result<(quinn::SendStream, quinn::RecvStream, Vec<DisplayInfo>)> {
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;

    send_msg(&mut send, ControlMsg::QueryDisplays).await?;
    let displays = match read_control_msg(&mut recv).await? {
        ControlMsg::AvailableDisplays(displays) => displays,
        ControlMsg::SessionReject(reject) => {
            bail!("server rejected display query: {}", reject.reason)
        }
        other => bail!("unexpected display-list response: {other:?}"),
    };

    Ok((send, recv, displays))
}

/// Reads all QUIC unreliable datagrams on the connection and dispatches them
/// by type tag.
async fn datagram_dispatch_loop(
    conn: quinn::Connection,
    cursor_store: Arc<RemoteCursorStore>,
    control_tx: UnboundedSender<ControlMsg>,
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    telemetry: SharedClientTelemetry,
) -> Result<()> {
    let mut reassembler = VideoFrameReassembler::new();
    let mut last_idr_fallback_at: Option<Instant> = None;

    loop {
        let bytes = match conn.read_datagram().await {
            Ok(bytes) => bytes,
            Err(err) => {
                info!("datagram dispatch loop stopped: {err}");
                break;
            }
        };

        match bytes.first() {
            Some(&DATAGRAM_TAG_VIDEO) => {
                telemetry.record_bytes_received(bytes.len());
                let outcome = reassembler.push_datagram(&bytes[1..]);
                if let Some(loss) = outcome.loss {
                    telemetry.record_reassembly(
                        u32::try_from(loss.dropped_frames).unwrap_or(u32::MAX),
                        0,
                        false,
                    );
                    report_frame_loss(&control_tx, loss, &mut last_idr_fallback_at);
                }
                if outcome.recovered_fragments > 0 || outcome.recovered_frame {
                    telemetry.record_reassembly(
                        0,
                        outcome.recovered_fragments as u32,
                        outcome.recovered_frame,
                    );
                }
                if let Some(frame) = outcome.frame {
                    match frame_tx.try_send(frame) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(_)) => {}
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
                    }
                }
            }
            Some(&DATAGRAM_TAG_CURSOR) => {
                if let Some(state) = decode_cursor_datagram(&bytes) {
                    cursor_store.apply_state(state);
                } else {
                    warn!("received malformed cursor datagram ({} bytes)", bytes.len());
                }
            }
            Some(&tag) => {
                warn!("received datagram with unknown type tag {tag:#04x} — ignoring");
            }
            None => {
                warn!("received empty datagram — ignoring");
            }
        }
    }

    Ok(())
}

async fn control_send_loop(
    mut send: quinn::SendStream,
    mut control_rx: UnboundedReceiver<ControlMsg>,
    telemetry: SharedClientTelemetry,
) -> Result<()> {
    let mut interval = tokio::time::interval(CLIENT_TELEMETRY_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                send_msg(&mut send, ControlMsg::ClientTelemetry(telemetry.drain().proto)).await?;
            }
            maybe_msg = control_rx.recv() => {
                let Some(msg) = maybe_msg else {
                    break;
                };
                send_msg(&mut send, msg.clone()).await?;
                if matches!(msg, ControlMsg::Goodbye) {
                    break;
                }
            }
        }
    }

    send.finish().context("finish control stream")?;
    Ok(())
}

/// Report unrecoverable frame loss to the server.
///
/// **Fast path:** `FrameLost(frame_seq)` is sent immediately for every loss
/// event so the server can call `NvEncInvalidateRefFrames`.  For isolated
/// losses (one or two frames) this allows recovery in a single frame without
/// an IDR.
///
/// **Fallback path:** If `IDR_FALLBACK_MIN_INTERVAL` has elapsed since the
/// last `RequestIdr`, one is sent as well.  This catches sustained loss where
/// the entire P-frame reference chain is corrupt and `invalidateRefFrames`
/// alone cannot help — the decoder needs a clean I-frame to resync.
fn report_frame_loss(
    control_tx: &UnboundedSender<ControlMsg>,
    loss: ReassemblyLoss,
    last_idr_fallback_at: &mut Option<Instant>,
) {
    match control_tx.send(ControlMsg::FrameLost(loss.frame_seq)) {
        Ok(()) => {
            info!(
                "sent FrameLost(seq={}) after dropping {} unrecoverable frame(s)",
                loss.frame_seq, loss.dropped_frames
            );
        }
        Err(err) => warn!("failed to send FrameLost after frame loss: {err}"),
    }

    let now = Instant::now();
    let idr_due = last_idr_fallback_at
        .as_ref()
        .is_none_or(|last| now.duration_since(*last) >= IDR_FALLBACK_MIN_INTERVAL);

    if idr_due {
        match control_tx.send(ControlMsg::RequestIdr) {
            Ok(()) => {
                *last_idr_fallback_at = Some(now);
                info!(
                    "requested IDR fallback after frame loss (seq={}, dropped={})",
                    loss.frame_seq, loss.dropped_frames
                );
            }
            Err(err) => warn!("failed to send IDR fallback after frame loss: {err}"),
        }
    }
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
                    "server telemetry: capture_wait={}µs encode={}µs send={}µs send_wait(avg/max)={}/{}µs pipeline={}µs fps={} bitrate={:.1}Mbps frames={} dropped_dgrams={}",
                    t.avg_capture_wait_us,
                    t.avg_encode_us,
                    t.avg_send_us,
                    t.avg_send_wait_us,
                    t.max_send_wait_us,
                    t.avg_pipeline_us,
                    t.target_fps,
                    t.encoder_bitrate_bps as f64 / 1e6,
                    t.frame_count,
                    t.video_datagrams_dropped,
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
    conn: quinn::Connection,
    runtime: tokio::runtime::Handle,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match input_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) => match &packet.event {
                InputEvent::MouseMove { .. } => {
                    // Mouse moves are idempotent: only the latest delta matters.
                    // Send as an unreliable datagram to avoid head-of-line blocking
                    // on the reliable input stream stalling subsequent events.
                    if let Err(err) = conn.send_datagram(encode_input_datagram(&packet)) {
                        warn!("mouse move datagram dropped: {err}");
                    }
                }
                _ => {
                    // Key presses, mouse buttons, and scroll events require
                    // reliable ordered delivery; keep them on the stream.
                    let bytes = encode_packet(&packet);
                    runtime
                        .block_on(input_stream.write_all(&bytes))
                        .context("write input packet")?;
                }
            },
            Err(RecvTimeoutError::Timeout) if shutdown.load(Ordering::Relaxed) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    input_stream.finish().context("finish input stream")?;
    Ok(())
}

fn make_client_endpoint() -> Result<Endpoint> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .context("build QUIC client config")?,
    ));
    let mut transport_config = TransportConfig::default();
    transport_config.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    transport_config.datagram_send_buffer_size(256 * 1024);
    transport_config.keep_alive_interval(Some(QUIC_KEEPALIVE_INTERVAL));
    transport_config.max_idle_timeout(Some(
        QUIC_MAX_IDLE_TIMEOUT
            .try_into()
            .context("convert client QUIC idle timeout")?,
    ));
    client_config.transport_config(Arc::new(transport_config));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?).context("create client endpoint")?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

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
