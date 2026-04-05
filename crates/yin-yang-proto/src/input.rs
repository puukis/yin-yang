//! Helpers for framing input packets on a QUIC unidirectional stream.
//!
//! Each packet is length-prefixed: 4-byte little-endian `u32` followed by
//! the bincode-encoded `InputPacket` payload.

use crate::packets::InputPacket;
use bincode::{
    config::standard,
    serde::{decode_from_slice, encode_to_vec},
};

/// Encode an `InputPacket` into length-prefixed bytes ready to write to QUIC.
pub fn encode_packet(packet: &InputPacket) -> Vec<u8> {
    let payload = encode_to_vec(packet, standard()).expect("bincode encode");
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode an `InputPacket` from a length-prefixed byte slice.
/// Returns `(packet, bytes_consumed)`.
pub fn decode_packet(buf: &[u8]) -> Option<(InputPacket, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let (packet, _) = decode_from_slice::<InputPacket, _>(&buf[4..4 + len], standard()).ok()?;
    Some((packet, 4 + len))
}
