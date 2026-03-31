//! QUIC control channel — session handshake, IDR requests, heartbeats.

use anyhow::{bail, Context, Result};
use crossbeam_channel::TrySendError;
use quinn::{Endpoint, RecvStream, SendStream, ServerConfig};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use streamd_proto::{
    control::{decode_msg, encode_msg},
    input::decode_packet,
    packets::{
        Codec, ControlMsg, DisplayInfo, InputPacket, SessionAccept, SessionReject, SessionRequest,
        PROTOCOL_VERSION,
    },
};
use tracing::{debug, error, info, warn};

use crate::pipeline::PipelineHandle;

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

    if req.version != PROTOCOL_VERSION {
        send_msg(
            &mut send,
            ControlMsg::SessionReject(SessionReject {
                reason: format!(
                    "version mismatch: client={} server={PROTOCOL_VERSION}",
                    req.version
                ),
            }),
        )
        .await?;
        bail!("version mismatch");
    }

    // Negotiate codec
    let codec = negotiate_codec(&req);
    let fps = req.max_fps.min(120).max(1);
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
        req.width.max(640).min(7680)
    };
    let height = if selected_display.height > 0 {
        selected_display.height
    } else {
        req.height.max(480).min(4320)
    };

    // Video UDP port: server binds on (control_port + 1)
    let video_port: u16 = conn.local_ip().map(|_| 9001u16).unwrap_or(9001);

    let accept = SessionAccept {
        codec,
        fps,
        width,
        height,
        video_udp_port: video_port,
        client_video_udp_port: video_port,
        selected_display: selected_display.clone(),
    };
    info!(
        "session accepted: {codec:?} {width}x{height}@{fps}fps display={} ({}) → udp:{video_port}",
        selected_display.name, selected_display.id
    );
    send_msg(&mut send, ControlMsg::SessionAccept(accept)).await?;

    let (input_tx, input_rx) = crossbeam_channel::bounded(1024);
    #[cfg(target_os = "linux")]
    let _input_injector = crate::input::linux::LinuxInputInjector::start(input_rx)?;
    #[cfg(target_os = "windows")]
    let _input_injector = crate::input::windows::WindowsInputInjector::start(input_rx)?;
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

    // Start the pipeline (capture + encode + send)
    let video_remote = SocketAddr::new(remote.ip(), video_port);

    let pipeline = PipelineHandle::start(
        codec,
        fps,
        width,
        height,
        Some(selected_display.id.clone()),
        video_port,
        video_remote,
    )?;

    // Control loop: heartbeats + IDR requests
    loop {
        tokio::select! {
            msg = read_control_msg(&mut recv) => {
                match msg {
                    Ok(ControlMsg::RequestIdr) => {
                        info!("IDR requested by client");
                        pipeline.request_idr();
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
            telemetry = pipeline.next_telemetry() => {
                if let Some(t) = telemetry {
                    send_msg(&mut send, ControlMsg::Heartbeat(t)).await?;
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
        // Server supports H264 and Hevc
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
    // Read 4-byte length prefix
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
    let cert = CertificateParams::new(vec!["streamd".to_string()])
        .context("cert params")?
        .self_signed(&key_pair)
        .context("self-sign cert")?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());

    let server_config = ServerConfig::with_single_cert(vec![cert_der], key_der)
        .context("build server TLS config")?;

    let endpoint = Endpoint::server(server_config, bind_addr).context("create QUIC endpoint")?;
    Ok(endpoint)
}
