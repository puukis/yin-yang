//! Helpers for framing control messages on a QUIC stream.
//!
//! Each message is length-prefixed: 4-byte little-endian `u32` followed by
//! the bincode-encoded `ControlMsg` payload.

use crate::packets::RemoteCursorState;
use bincode::{
    config::standard,
    serde::{decode_from_slice, encode_to_vec},
};
use bytes::Bytes;

use crate::packets::ControlMsg;

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

/// Encode a cursor-state datagram for QUIC datagram transport.
pub fn encode_cursor_datagram(state: &RemoteCursorState) -> Bytes {
    encode_to_vec(state, standard())
        .expect("bincode encode cursor datagram")
        .into()
}

/// Decode a cursor-state datagram received over QUIC datagrams.
pub fn decode_cursor_datagram(buf: &[u8]) -> Option<RemoteCursorState> {
    let (state, _) = decode_from_slice::<RemoteCursorState, _>(buf, standard()).ok()?;
    Some(state)
}
