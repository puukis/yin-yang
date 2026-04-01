//! Helpers for framing control messages and datagrams.
//!
//! Control messages on the QUIC bidirectional stream are length-prefixed:
//! 4-byte little-endian `u32` followed by the bincode-encoded `ControlMsg`
//! payload.
//!
//! QUIC unreliable datagrams carry a 1-byte type tag as the very first byte so
//! a single `conn.read_datagram()` call can dispatch between video fragments and
//! cursor state updates:
//!
//! | Tag | Content |
//! |-----|---------|
//! | `DATAGRAM_TAG_CURSOR` (0x01) | bincode-encoded `RemoteCursorState` |
//! | `DATAGRAM_TAG_VIDEO`  (0x02) | 18-byte `VideoPacketHeader` + encoded video data |

use crate::packets::RemoteCursorState;
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

/// Wrap a pre-built video fragment packet (18-byte header + encoded data) in a
/// tagged QUIC datagram ready to pass to `Connection::send_datagram`.
///
/// Wire format: `[DATAGRAM_TAG_VIDEO (1 byte)] [VideoPacketHeader (18 bytes)] [video data]`
pub fn encode_video_datagram(packet: &[u8]) -> Bytes {
    let mut bytes = Vec::with_capacity(1 + packet.len());
    bytes.push(DATAGRAM_TAG_VIDEO);
    bytes.extend_from_slice(packet);
    Bytes::from(bytes)
}
