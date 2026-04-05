//! Helpers for framing control messages and datagrams.
//!
//! Control messages on the QUIC bidirectional stream are length-prefixed:
//! 4-byte little-endian `u32` followed by the bincode-encoded `ControlMsg`
//! payload.
//!
//! QUIC unreliable datagrams carry a 1-byte type tag as the very first byte so
//! a single `conn.read_datagram()` call can dispatch between video fragments,
//! cursor state updates, and mouse-move input events:
//!
//! | Tag | Direction | Content |
//! |-----|-----------|---------|
//! | `DATAGRAM_TAG_CURSOR` (0x01) | server→client | bincode-encoded `RemoteCursorState` |
//! | `DATAGRAM_TAG_VIDEO`  (0x02) | server→client | 24-byte `VideoPacketHeader` + encoded video or FEC parity data |
//! | `DATAGRAM_TAG_INPUT`  (0x03) | client→server | bincode-encoded `InputPacket` carrying `InputEvent::MouseMove` |

use crate::packets::{InputPacket, RemoteCursorState};
use bincode::{
    config::standard,
    serde::{decode_from_slice, encode_to_vec},
};
use bytes::Bytes;

use crate::packets::ControlMsg;

/// Datagram type tag for cursor state updates.
pub const DATAGRAM_TAG_CURSOR: u8 = 0x01;

/// Datagram type tag for video fragment packets.
pub const DATAGRAM_TAG_VIDEO: u8 = 0x02;

/// Datagram type tag for unreliable mouse-move input packets (client→server).
///
/// Only `InputEvent::MouseMove` is routed this way. All other input events
/// (key presses, mouse buttons, scroll) remain on the reliable input stream
/// so that ordering and delivery guarantees are preserved.
pub const DATAGRAM_TAG_INPUT: u8 = 0x03;

/// Encode a `ControlMsg` into length-prefixed bytes ready to write to a QUIC stream.
pub fn encode_msg(msg: &ControlMsg) -> Vec<u8> {
    let payload = encode_to_vec(msg, standard()).expect("bincode encode");
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a `ControlMsg` from a length-prefixed byte slice.
/// Returns `(msg, bytes_consumed)`.
pub fn decode_msg(buf: &[u8]) -> Option<(ControlMsg, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let (msg, _) = decode_from_slice::<ControlMsg, _>(&buf[4..4 + len], standard()).ok()?;
    Some((msg, 4 + len))
}

/// Encode a cursor-state update as a tagged QUIC datagram.
///
/// Wire format: `[DATAGRAM_TAG_CURSOR (1 byte)] [bincode(RemoteCursorState)]`
pub fn encode_cursor_datagram(state: &RemoteCursorState) -> Bytes {
    let payload = encode_to_vec(state, standard()).expect("bincode encode cursor datagram");
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(DATAGRAM_TAG_CURSOR);
    bytes.extend_from_slice(&payload);
    Bytes::from(bytes)
}

/// Decode a cursor-state datagram received over QUIC.
/// Returns `None` if the tag byte is missing or wrong, or decoding fails.
pub fn decode_cursor_datagram(buf: &[u8]) -> Option<RemoteCursorState> {
    if buf.first() != Some(&DATAGRAM_TAG_CURSOR) {
        return None;
    }
    let (state, _) = decode_from_slice::<RemoteCursorState, _>(&buf[1..], standard()).ok()?;
    Some(state)
}

/// Wrap a pre-built video fragment packet (24-byte header + encoded or parity data) in a
/// tagged QUIC datagram ready to pass to `Connection::send_datagram`.
///
/// Wire format: `[DATAGRAM_TAG_VIDEO (1 byte)] [VideoPacketHeader (24 bytes)] [video or parity data]`
pub fn encode_video_datagram(packet: &[u8]) -> Bytes {
    let mut bytes = Vec::with_capacity(1 + packet.len());
    bytes.push(DATAGRAM_TAG_VIDEO);
    bytes.extend_from_slice(packet);
    Bytes::from(bytes)
}

/// Encode an `InputPacket` as an unreliable QUIC datagram (client→server).
///
/// Wire format: `[DATAGRAM_TAG_INPUT (1 byte)] [bincode(InputPacket)]`
///
/// Only `InputEvent::MouseMove` events should be sent this way; all other
/// input events travel on the reliable input stream.
pub fn encode_input_datagram(packet: &InputPacket) -> Bytes {
    let payload = encode_to_vec(packet, standard()).expect("bincode encode input datagram");
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(DATAGRAM_TAG_INPUT);
    bytes.extend_from_slice(&payload);
    Bytes::from(bytes)
}

/// Decode an input datagram received over QUIC.
///
/// Returns `None` if the tag byte is missing or wrong, or bincode decoding fails.
pub fn decode_input_datagram(buf: &[u8]) -> Option<InputPacket> {
    if buf.first() != Some(&DATAGRAM_TAG_INPUT) {
        return None;
    }
    let (packet, _) = decode_from_slice::<InputPacket, _>(&buf[1..], standard()).ok()?;
    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::{decode_msg, encode_msg};
    use crate::packets::{
        ClientTelemetry, Codec, ControlMsg, DisplayInfo, ServerTelemetry, SessionRequest,
        PROTOCOL_VERSION,
    };

    #[test]
    fn round_trips_session_request_with_adaptive_bounds() {
        let request = SessionRequest {
            version: PROTOCOL_VERSION,
            client_session_id: "client-session-1".into(),
            adaptive_streaming: true,
            max_fps: 120,
            min_fps: 48,
            min_bitrate_bps: 12_000_000,
            max_bitrate_bps: 40_000_000,
            width: 2560,
            height: 1440,
            preferred_codecs: vec![Codec::Hevc, Codec::H264],
            display_id: Some("display-0".into()),
        };

        let encoded = encode_msg(&ControlMsg::SessionRequest(request.clone()));
        let (decoded, consumed) = decode_msg(&encoded).expect("decode session request");
        assert_eq!(consumed, encoded.len());

        match decoded {
            ControlMsg::SessionRequest(decoded_request) => {
                assert_eq!(decoded_request.version, request.version);
                assert_eq!(decoded_request.client_session_id, request.client_session_id);
                assert_eq!(
                    decoded_request.adaptive_streaming,
                    request.adaptive_streaming
                );
                assert_eq!(decoded_request.max_fps, request.max_fps);
                assert_eq!(decoded_request.min_fps, request.min_fps);
                assert_eq!(decoded_request.min_bitrate_bps, request.min_bitrate_bps);
                assert_eq!(decoded_request.max_bitrate_bps, request.max_bitrate_bps);
                assert_eq!(decoded_request.width, request.width);
                assert_eq!(decoded_request.height, request.height);
                assert_eq!(decoded_request.preferred_codecs.len(), 2);
                assert_eq!(decoded_request.display_id, request.display_id);
            }
            other => panic!("unexpected decoded control message: {other:?}"),
        }
    }

    #[test]
    fn round_trips_client_and_server_telemetry() {
        let client = ClientTelemetry {
            unrecoverable_frames: 2,
            recovered_frames: 3,
            recovered_fragments: 5,
            presented_frames: 120,
            render_dropped_frames: 4,
            avg_decode_queue_us: 450,
            avg_render_queue_us: 800,
        };
        let server = ServerTelemetry {
            avg_capture_wait_us: 100,
            avg_capture_convert_us: 200,
            avg_encode_us: 300,
            avg_send_us: 400,
            avg_send_wait_us: 500,
            max_send_wait_us: 900,
            avg_pipeline_us: 1_200,
            send_queue_frames: 0,
            idr_count: 1,
            frame_count: 60,
            encoder_bitrate_bps: 25_000_000,
            target_fps: 60,
            video_datagrams_sent: 1000,
            video_datagrams_dropped: 7,
            max_datagram_size: 1200,
            fragment_payload_bytes: 1175,
        };

        let (decoded_client, _) =
            decode_msg(&encode_msg(&ControlMsg::ClientTelemetry(client.clone())))
                .expect("decode client telemetry");
        let (decoded_server, _) = decode_msg(&encode_msg(&ControlMsg::Heartbeat(server.clone())))
            .expect("decode server telemetry");

        match decoded_client {
            ControlMsg::ClientTelemetry(decoded) => {
                assert_eq!(decoded.unrecoverable_frames, client.unrecoverable_frames);
                assert_eq!(decoded.recovered_frames, client.recovered_frames);
                assert_eq!(decoded.recovered_fragments, client.recovered_fragments);
                assert_eq!(decoded.presented_frames, client.presented_frames);
                assert_eq!(decoded.render_dropped_frames, client.render_dropped_frames);
                assert_eq!(decoded.avg_decode_queue_us, client.avg_decode_queue_us);
                assert_eq!(decoded.avg_render_queue_us, client.avg_render_queue_us);
            }
            other => panic!("unexpected decoded client telemetry message: {other:?}"),
        }

        match decoded_server {
            ControlMsg::Heartbeat(decoded) => {
                assert_eq!(decoded.avg_capture_wait_us, server.avg_capture_wait_us);
                assert_eq!(
                    decoded.avg_capture_convert_us,
                    server.avg_capture_convert_us
                );
                assert_eq!(decoded.avg_encode_us, server.avg_encode_us);
                assert_eq!(decoded.avg_send_us, server.avg_send_us);
                assert_eq!(decoded.avg_send_wait_us, server.avg_send_wait_us);
                assert_eq!(decoded.max_send_wait_us, server.max_send_wait_us);
                assert_eq!(decoded.avg_pipeline_us, server.avg_pipeline_us);
                assert_eq!(decoded.send_queue_frames, server.send_queue_frames);
                assert_eq!(decoded.idr_count, server.idr_count);
                assert_eq!(decoded.frame_count, server.frame_count);
                assert_eq!(decoded.encoder_bitrate_bps, server.encoder_bitrate_bps);
                assert_eq!(decoded.target_fps, server.target_fps);
                assert_eq!(decoded.video_datagrams_sent, server.video_datagrams_sent);
                assert_eq!(
                    decoded.video_datagrams_dropped,
                    server.video_datagrams_dropped
                );
                assert_eq!(decoded.max_datagram_size, server.max_datagram_size);
                assert_eq!(
                    decoded.fragment_payload_bytes,
                    server.fragment_payload_bytes
                );
            }
            other => panic!("unexpected decoded server telemetry message: {other:?}"),
        }
    }

    #[test]
    fn round_trips_available_displays() {
        let displays = vec![DisplayInfo {
            id: "display-0".into(),
            index: 0,
            name: "Primary".into(),
            description: Some("Main monitor".into()),
            width: 3840,
            height: 2160,
        }];

        let (decoded, _) = decode_msg(&encode_msg(&ControlMsg::AvailableDisplays(
            displays.clone(),
        )))
        .expect("decode displays");

        match decoded {
            ControlMsg::AvailableDisplays(decoded_displays) => {
                assert_eq!(decoded_displays.len(), 1);
                assert_eq!(decoded_displays[0].id, displays[0].id);
                assert_eq!(decoded_displays[0].name, displays[0].name);
                assert_eq!(decoded_displays[0].description, displays[0].description);
                assert_eq!(decoded_displays[0].width, displays[0].width);
                assert_eq!(decoded_displays[0].height, displays[0].height);
            }
            other => panic!("unexpected decoded display message: {other:?}"),
        }
    }
}
