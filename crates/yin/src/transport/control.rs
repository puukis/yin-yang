//! QUIC control channel — session handshake, IDR requests, heartbeats.

use anyhow::{bail, Context, Result};
use crossbeam_channel::TrySendError;
use quinn::{Endpoint, RecvStream, SendDatagramError, SendStream, ServerConfig, TransportConfig};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use tracing::{debug, error, info, warn};
use yin_yang_proto::{
    control::{
        decode_input_datagram, decode_msg, encode_cursor_datagram, encode_msg, DATAGRAM_TAG_INPUT,
    },
    input::decode_packet,
    packets::{
        Codec, ControlMsg, DisplayInfo, InputPacket, SessionAccept, SessionReject, SessionRequest,
        VideoTransport, PROTOCOL_VERSION,
    },
};

use crate::{
    capture::CursorEvent,
    pipeline::{AdaptiveStreamConfig, PipelineHandle},
};

const QUIC_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const QUIC_MAX_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const APP_ERR_SESSION_REPLACED: u32 = 0x100;

#[derive(Clone)]
struct RegisteredSession {
    token: u64,
    conn: quinn::Connection,
    stop_flag: Arc<AtomicBool>,
}

struct SessionRegistration {
    client_session_id: String,
    token: u64,
}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        unregister_session(&self.client_session_id, self.token);
    }
}

static SESSION_REGISTRY: OnceLock<Mutex<HashMap<String, RegisteredSession>>> = OnceLock::new();
static NEXT_SESSION_TOKEN: AtomicU64 = AtomicU64::new(1);

fn session_registry() -> &'static Mutex<HashMap<String, RegisteredSession>> {
    SESSION_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_session(
    client_session_id: &str,
    conn: &quinn::Connection,
    stop_flag: Arc<AtomicBool>,
) -> u64 {
    let token = NEXT_SESSION_TOKEN.fetch_add(1, Ordering::Relaxed);
    let replaced = {
        let mut registry = session_registry()
            .lock()
            .expect("session registry mutex poisoned");
        registry.insert(
            client_session_id.to_owned(),
            RegisteredSession {
                token,
                conn: conn.clone(),
                stop_flag,
            },
        )
    };

    if let Some(old) = replaced {
        old.stop_flag.store(true, Ordering::Relaxed);
        old.conn
            .close(APP_ERR_SESSION_REPLACED.into(), b"replaced by reconnect");
    }

    token
}

fn unregister_session(client_session_id: &str, token: u64) {
    let mut registry = session_registry()
        .lock()
        .expect("session registry mutex poisoned");
    if registry
        .get(client_session_id)
        .is_some_and(|session| session.token == token)
    {
        registry.remove(client_session_id);
    }
}

fn protocol_version_reject(version: u32) -> Option<String> {
    (version != PROTOCOL_VERSION).then(|| {
        format!(
            "version mismatch: client={} server={PROTOCOL_VERSION}",
            version
        )
    })
}

pub async fn run_server(bind_addr: SocketAddr) -> Result<()> {
    let endpoint = make_server_endpoint(bind_addr)?;
    info!("QUIC endpoint listening on {}", endpoint.local_addr()?);

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming).await {
                error!("connection error: {e:#}");
            }
        });
    }
    Ok(())
}

async fn handle_connection(incoming: quinn::Incoming) -> Result<()> {
    let conn = incoming.await.context("accept connection")?;
    let remote = conn.remote_address();
    info!("new connection from {remote}");

    // Open bidirectional control stream
    let (mut send, mut recv) = conn.accept_bi().await.context("accept control stream")?;

    // Handle initial display queries, then read SessionRequest.
    let req = loop {
        match read_control_msg(&mut recv).await? {
            ControlMsg::QueryDisplays => {
                let displays = match crate::capture::list_displays().context("enumerate displays") {
                    Ok(displays) => displays,
                    Err(err) => {
                        send_msg(
                            &mut send,
                            ControlMsg::SessionReject(SessionReject {
                                reason: format!("display enumeration failed: {err:#}"),
                            }),
                        )
                        .await?;
                        return Err(err);
                    }
                };
                send_msg(&mut send, ControlMsg::AvailableDisplays(displays)).await?;
            }
            ControlMsg::SessionRequest(req) => break req,
            ControlMsg::Goodbye => {
                info!("client {remote} disconnected before starting a session");
                return Ok(());
            }
            other => {
                send_msg(
                    &mut send,
                    ControlMsg::SessionReject(SessionReject {
                        reason: "expected QueryDisplays or SessionRequest".into(),
                    }),
                )
                .await?;
                bail!("unexpected pre-session message: {other:?}");
            }
        }
    };

    if let Some(reason) = protocol_version_reject(req.version) {
        send_msg(
            &mut send,
            ControlMsg::SessionReject(SessionReject { reason }),
        )
        .await?;
        bail!("version mismatch");
    }
    let client_session_id = req.client_session_id.trim().to_owned();
    if client_session_id.is_empty() {
        send_msg(
            &mut send,
            ControlMsg::SessionReject(SessionReject {
                reason: "client_session_id must not be empty".into(),
            }),
        )
        .await?;
        bail!("empty client_session_id");
    }

    // Negotiate codec
    let codec = negotiate_codec(&req);
    let fps = req.max_fps.clamp(1, 120);
    let adaptive_config = AdaptiveStreamConfig {
        enabled: req.adaptive_streaming,
        min_fps: if req.min_fps > 0 {
            req.min_fps.clamp(1, fps)
        } else if fps >= 30 {
            30
        } else {
            fps
        },
        min_bitrate_bps: req.min_bitrate_bps,
        max_bitrate_bps: req.max_bitrate_bps,
    };
    let displays = match crate::capture::list_displays().context("enumerate displays for session") {
        Ok(displays) => displays,
        Err(err) => {
            send_msg(
                &mut send,
                ControlMsg::SessionReject(SessionReject {
                    reason: format!("display enumeration failed: {err:#}"),
                }),
            )
            .await?;
            return Err(err);
        }
    };
    let selected_display = match resolve_display(&displays, req.display_id.as_deref()) {
        Ok(display) => display,
        Err(err) => {
            send_msg(
                &mut send,
                ControlMsg::SessionReject(SessionReject {
                    reason: format!("{err:#}"),
                }),
            )
            .await?;
            return Err(err);
        }
    };
    let width = if selected_display.width > 0 {
        selected_display.width
    } else {
        req.width.clamp(640, 7680)
    };
    let height = if selected_display.height > 0 {
        selected_display.height
    } else {
        req.height.clamp(480, 4320)
    };

    let accept = SessionAccept {
        codec,
        fps,
        width,
        height,
        // Video is always delivered as QUIC datagrams on this connection —
        // no additional port or socket is required on either end.
        video_transport: VideoTransport::QuicDatagram,
        selected_display: selected_display.clone(),
    };
    info!(
        "session accepted: {codec:?} {width}x{height}@{fps}fps display={} ({}) adaptive={} → QUIC datagrams",
        selected_display.name,
        selected_display.id,
        if adaptive_config.enabled { "on" } else { "off" }
    );
    send_msg(&mut send, ControlMsg::SessionAccept(accept)).await?;

    let (input_tx, input_rx) = crossbeam_channel::bounded(1024);
    #[cfg(target_os = "linux")]
    let _input_injector = crate::input::linux::LinuxInputInjector::start(input_rx)?;
    #[cfg(target_os = "windows")]
    let _input_injector = crate::input::windows::WindowsInputInjector::start(input_rx)?;
    // Clone so the datagram dispatch arm in the control loop can also inject
    // mouse-move packets without competing with the reliable stream task.
    let datagram_input_tx = input_tx.clone();
    let input_conn = conn.clone();
    let input_task = tokio::spawn(async move {
        match input_conn.accept_uni().await {
            Ok(input_stream) => {
                debug!("client {remote} opened input stream");
                if let Err(err) = input_loop(input_stream, input_tx).await {
                    warn!("input stream ended: {err:#}");
                }
            }
            Err(err) => {
                warn!("accept input stream failed: {err:#}");
            }
        }
    });

    // Start the pipeline (capture → encode → QUIC datagram send).
    // Pass the connection so the pipeline thread can call conn.send_datagram()
    // directly from the non-async pipeline thread.
    let mut pipeline = PipelineHandle::start(
        codec,
        fps,
        width,
        height,
        Some(selected_display.id.clone()),
        adaptive_config,
        conn.clone(),
    )?;
    let _session_registration = SessionRegistration {
        client_session_id: client_session_id.clone(),
        token: register_session(&client_session_id, &conn, pipeline.stop_flag()),
    };
    let mut telemetry_rx = pipeline.take_telemetry_rx();
    let mut cursor_rx = pipeline.take_cursor_rx();

    let mut cursor_datagrams_supported = conn.max_datagram_size().is_some();

    // Control loop: heartbeats + IDR requests + mouse-move datagrams
    loop {
        tokio::select! {
            msg = read_control_msg(&mut recv) => {
                match msg {
                    Ok(ControlMsg::RequestIdr) => {
                        info!("IDR requested by client");
                        pipeline.request_idr();
                    }
                    Ok(ControlMsg::FrameLost(frame_seq)) => {
                        pipeline.notify_frame_lost(frame_seq);
                    }
                    Ok(ControlMsg::ClientTelemetry(sample)) => {
                        pipeline.update_client_telemetry(sample);
                    }
                    Ok(ControlMsg::Goodbye) | Err(_) => {
                        info!("client {remote} disconnected");
                        break;
                    }
                    Ok(other) => {
                        warn!("unexpected control msg: {other:?}");
                    }
                }
            }
            telemetry = telemetry_rx.recv() => {
                if let Some(t) = telemetry {
                    send_msg(&mut send, ControlMsg::Heartbeat(t)).await?;
                }
            }
            cursor_event = cursor_rx.recv() => {
                match cursor_event {
                    Some(CursorEvent::Shape(shape)) => {
                        send_msg(&mut send, ControlMsg::CursorShape(shape)).await?;
                    }
                    Some(CursorEvent::State(state)) => {
                        if cursor_datagrams_supported {
                            match conn.send_datagram(encode_cursor_datagram(&state)) {
                                Ok(()) => {}
                                Err(SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled | SendDatagramError::TooLarge) => {
                                    cursor_datagrams_supported = false;
                                    send_msg(&mut send, ControlMsg::CursorState(state)).await?;
                                }
                                Err(SendDatagramError::ConnectionLost(err)) => {
                                    info!("client {remote} disconnected while sending cursor datagram: {err}");
                                    break;
                                }
                            }
                        } else {
                            send_msg(&mut send, ControlMsg::CursorState(state)).await?;
                        }
                    }
                    None => break,
                }
            }
            dgram = conn.read_datagram() => {
                match dgram {
                    Ok(bytes) => {
                        match bytes.first() {
                            Some(&DATAGRAM_TAG_INPUT) => {
                                if let Some(packet) = decode_input_datagram(&bytes) {
                                    match datagram_input_tx.try_send(packet) {
                                        Ok(()) => {}
                                        Err(TrySendError::Full(_)) => {
                                            debug!("dropping mouse move datagram: input queue full");
                                        }
                                        Err(TrySendError::Disconnected(_)) => break,
                                    }
                                } else {
                                    warn!("received malformed input datagram ({} bytes)", bytes.len());
                                }
                            }
                            Some(&tag) => {
                                warn!("received datagram with unknown type tag {tag:#04x} from client — ignoring");
                            }
                            None => {
                                warn!("received empty datagram from client — ignoring");
                            }
                        }
                    }
                    Err(err) => {
                        info!("client {remote} disconnected (datagram channel): {err}");
                        break;
                    }
                }
            }
        }
    }

    input_task.abort();
    pipeline.stop();
    Ok(())
}

fn negotiate_codec(req: &SessionRequest) -> Codec {
    for &c in &req.preferred_codecs {
        if matches!(c, Codec::H264 | Codec::Hevc) {
            return c;
        }
    }
    Codec::H264
}

fn resolve_display(displays: &[DisplayInfo], requested_id: Option<&str>) -> Result<DisplayInfo> {
    if displays.is_empty() {
        bail!("the server did not expose any capture displays");
    }

    if let Some(requested_id) = requested_id {
        return displays
            .iter()
            .find(|display| display.id == requested_id)
            .cloned()
            .with_context(|| format!("requested display {requested_id:?} is not available"));
    }

    Ok(displays[0].clone())
}

async fn read_control_msg(recv: &mut RecvStream) -> Result<ControlMsg> {
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
    let (msg, _) = decode_msg(&framed).context("decode control msg")?;
    Ok(msg)
}

async fn send_msg(send: &mut SendStream, msg: ControlMsg) -> Result<()> {
    let bytes = encode_msg(&msg);
    send.write_all(&bytes).await.context("write control msg")?;
    Ok(())
}

async fn input_loop(
    mut recv: quinn::RecvStream,
    packet_tx: crossbeam_channel::Sender<InputPacket>,
) -> Result<()> {
    loop {
        let packet = read_input_packet(&mut recv).await?;
        match packet_tx.try_send(packet) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => debug!("dropping input packet: injector queue full"),
            Err(TrySendError::Disconnected(_)) => break,
        }
    }

    Ok(())
}

async fn read_input_packet(recv: &mut RecvStream) -> Result<InputPacket> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .context("read input length")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 1 << 20 {
        bail!("input packet too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .context("read input payload")?;
    let mut framed = Vec::with_capacity(4 + len);
    framed.extend_from_slice(&len_buf);
    framed.extend_from_slice(&buf);
    let (packet, _) = decode_packet(&framed).context("decode input packet")?;
    Ok(packet)
}

fn make_server_endpoint(bind_addr: SocketAddr) -> Result<Endpoint> {
    let key_pair = KeyPair::generate().context("generate key pair")?;
    let cert = CertificateParams::new(vec!["yin-yang".to_string()])
        .context("cert params")?
        .self_signed(&key_pair)
        .context("self-sign cert")?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());

    let mut server_config = ServerConfig::with_single_cert(vec![cert_der], key_der)
        .context("build server TLS config")?;
    let mut transport_config = TransportConfig::default();
    // Send buffer large enough to absorb a full keyframe burst at high bitrate
    // before the congestion window catches up (~4 MB covers several 1080p IDR frames).
    transport_config.datagram_send_buffer_size(4 * 1024 * 1024);
    // Receive buffer for incoming cursor or future client datagrams.
    transport_config.datagram_receive_buffer_size(Some(256 * 1024));
    // Keep sessions alive while the desktop is static and no input/events are
    // flowing. A streaming session should not depend on local mouse movement
    // to remain connected.
    transport_config.keep_alive_interval(Some(QUIC_KEEPALIVE_INTERVAL));
    transport_config.max_idle_timeout(Some(
        QUIC_MAX_IDLE_TIMEOUT
            .try_into()
            .context("convert server QUIC idle timeout")?,
    ));
    server_config.transport_config(Arc::new(transport_config));

    let endpoint = Endpoint::server(server_config, bind_addr).context("create QUIC endpoint")?;
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::protocol_version_reject;
    use yin_yang_proto::packets::PROTOCOL_VERSION;

    #[test]
    fn rejects_previous_protocol_versions() {
        let previous = PROTOCOL_VERSION.saturating_sub(1);
        let reason = protocol_version_reject(previous).expect("older version should be rejected");
        assert!(reason.contains(&format!("client={previous}")));
        assert!(protocol_version_reject(PROTOCOL_VERSION).is_none());
    }
}
