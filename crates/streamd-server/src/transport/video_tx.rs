//! QUIC datagram video sender.
//!
//! `QuicVideoSender` fragments encoded NAL slices into MTU-sized chunks and
//! delivers each chunk as a QUIC unreliable datagram on the existing control
//! connection. Because the client opened the QUIC connection outbound, both
//! LAN and WAN topologies work without any inbound port forwarding on the
//! client.
//!
//! Each datagram wire format:
//! ```text
//! [DATAGRAM_TAG_VIDEO (1 byte)]
//! [VideoPacketHeader  (18 bytes)]
//! [encoded video data (variable)]
//! ```
//!
//! Datagrams are unreliable and unordered, matching raw-UDP semantics. A lost
//! fragment causes the receiver to drop the incomplete frame and request an IDR.
//!
//! ## Fragment payload sizing
//!
//! The maximum video data bytes that fit in one datagram is derived from the
//! QUIC-negotiated path MTU reported by [`Connection::max_datagram_size`]:
//!
//! ```text
//! fragment_payload = max_datagram_size − 1 (type tag) − 18 (VideoPacketHeader)
//! ```
//!
//! This value is re-queried on every call to [`QuicVideoSender::send_frame`]
//! rather than being cached at construction time. The reason is that QUIC
//! performs PMTU probing asynchronously after the handshake completes, so
//! `max_datagram_size` on the first few frames typically returns `None` —
//! causing a conservative `MTU_WAN` (1200-byte) fallback — and only settles
//! to the true path MTU (often 1400+ bytes on LAN) a few hundred milliseconds
//! later. Querying per-frame means those early-probe benefits are captured
//! automatically without any manual reconnect or configuration change.
//!
//! All slices within a single frame share the same `fragment_payload` value
//! computed at the start of that frame. This is required for correctness: the
//! `frag_total` field in [`VideoPacketHeader`] must match across every fragment
//! of a given slice, so the payload size must not change mid-frame.

use quinn::{Connection, SendDatagramError};
use streamd_proto::control::encode_video_datagram;
use streamd_proto::packets::{VideoFlags, VideoPacketHeader, MTU_WAN};
use tracing::{debug, info, warn};

/// Sends encoded video frames over QUIC unreliable datagrams.
pub struct QuicVideoSender {
    conn: Connection,
    /// Per-sender frame sequence counter. Not shared across threads; the
    /// pipeline thread is the sole caller.
    frame_seq: u32,
    /// Fragment payload size used in the previous frame, kept solely for
    /// change-detection logging. Zero on first use so the first frame always
    /// emits an info log with its effective payload size.
    last_fragment_payload: usize,
}

impl QuicVideoSender {
    /// Create a sender for the given QUIC connection.
    ///
    /// The fragment payload size is **not** queried here. It is derived from
    /// the live `max_datagram_size()` at the start of every
    /// [`send_frame`](Self::send_frame) call, so QUIC PMTU probing results are
    /// reflected automatically as they arrive.
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            frame_seq: 0,
            last_fragment_payload: 0,
        }
    }

    /// Query the current maximum video data bytes per datagram fragment.
    ///
    /// Called once at the start of each [`send_frame`](Self::send_frame) so
    /// every slice in that frame uses a consistent `fragment_payload`. The
    /// value is **not** cached between frames: each call reflects the latest
    /// PMTU probing result from the QUIC stack.
    ///
    /// # Fallback and floor
    ///
    /// - If `max_datagram_size()` returns `None` (PMTU probing not yet
    ///   complete), `MTU_WAN` (1200 bytes) is used as the conservative
    ///   internet-safe default.
    /// - The result is clamped to `MTU_WAN` from below so that a pathological
    ///   QUIC implementation reporting a very small MTU cannot produce a
    ///   fragment_payload of zero or a runaway fragment count.
    ///
    /// # Logging
    ///
    /// Emits an `info!` log the first time a payload size is established and
    /// again whenever it changes (upward as PMTU probing succeeds, or downward
    /// if the path degrades). This makes it easy to confirm in logs that LAN
    /// sessions are using the full negotiated MTU rather than the conservative
    /// WAN fallback.
    fn current_fragment_payload(&mut self) -> usize {
        let max_dg = self
            .conn
            .max_datagram_size()
            // PMTU probing not yet complete — use the conservative WAN floor.
            .unwrap_or(MTU_WAN)
            // Guard against a QUIC implementation reporting an unexpectedly
            // small MTU. MTU_WAN is always a safe lower bound.
            .max(MTU_WAN);

        // Subtract the 1-byte datagram type tag and the fixed 18-byte header.
        let payload = max_dg - 1 - VideoPacketHeader::SIZE;

        if payload != self.last_fragment_payload {
            if self.last_fragment_payload == 0 {
                info!(
                    "video fragment payload: {} bytes/datagram \
                     (max_datagram_size={} bytes)",
                    payload, max_dg,
                );
            } else {
                info!(
                    "video fragment payload changed: {} → {} bytes/datagram \
                     (max_datagram_size={} bytes)",
                    self.last_fragment_payload, payload, max_dg,
                );
            }
            self.last_fragment_payload = payload;
        }

        payload
    }

    /// Send one encoded frame consisting of one or more NAL slices.
    ///
    /// Each slice is fragmented independently so the receiver can begin
    /// decoding slice 0 while slice 1 is still in flight (NVENC sliceMode=3).
    ///
    /// The QUIC path MTU is queried **once** at the start of this call. All
    /// slices and all fragments within this frame use the same `fragment_payload`
    /// value, which is required so that `frag_total` in every
    /// [`VideoPacketHeader`] is consistent and the reassembler can correctly
    /// determine when a slice is complete.
    pub fn send_frame(&mut self, slices: &[Vec<u8>], is_keyframe: bool, timestamp_us: u64) {
        let frame_seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let num_slices = slices.len();

        // Derive the fragment payload once for this frame. Using a single
        // consistent value across all slices is required for correctness (see
        // module doc). The per-frame query — rather than a cached value from
        // connection setup — ensures that PMTU probing results are reflected
        // as soon as the QUIC stack reports them.
        let fragment_payload = self.current_fragment_payload();

        for (slice_idx, slice_data) in slices.iter().enumerate() {
            let is_last_slice = slice_idx == num_slices - 1;
            self.send_slice(
                frame_seq,
                timestamp_us,
                slice_idx as u8,
                is_last_slice,
                is_keyframe && slice_idx == 0,
                slice_data,
                fragment_payload,
            );
        }
    }

    fn send_slice(
        &self,
        frame_seq: u32,
        timestamp_us: u64,
        slice_idx: u8,
        is_last_slice: bool,
        is_keyframe: bool,
        data: &[u8],
        fragment_payload: usize,
    ) {
        // div_ceil: last chunk may be smaller than fragment_payload.
        let total_frags = data.len().div_ceil(fragment_payload) as u16;

        for (frag_idx, chunk) in data.chunks(fragment_payload).enumerate() {
            let frag_idx = frag_idx as u16;
            let mut flags = VideoFlags::empty();
            if is_keyframe && frag_idx == 0 {
                flags |= VideoFlags::KEY_FRAME;
            }
            if is_last_slice && frag_idx == total_frags - 1 {
                flags |= VideoFlags::LAST_SLICE;
            }

            // Build the fixed 18-byte header followed by the payload chunk.
            let mut packet = Vec::with_capacity(VideoPacketHeader::SIZE + chunk.len());
            packet.extend_from_slice(&frame_seq.to_le_bytes()); // 4
            packet.extend_from_slice(&timestamp_us.to_le_bytes()); // 8
            packet.push(slice_idx); // 1
            packet.push(flags.bits()); // 1
            packet.extend_from_slice(&frag_idx.to_le_bytes()); // 2
            packet.extend_from_slice(&total_frags.to_le_bytes()); // 2
            packet.extend_from_slice(chunk);

            let datagram = encode_video_datagram(&packet);
            match self.conn.send_datagram(datagram) {
                Ok(()) => {}
                Err(SendDatagramError::TooLarge) => {
                    // The path MTU shrank between the per-frame query at the
                    // start of send_frame and this individual send. This is a
                    // benign TOCTOU race: the receiver will evict this
                    // incomplete frame and request an IDR keyframe. The next
                    // frame's call to current_fragment_payload() will observe
                    // the reduced max_datagram_size and fragment accordingly.
                    debug!(
                        "video datagram too large ({} bytes) — path MTU shrank since frame start",
                        packet.len() + 1,
                    );
                }
                Err(SendDatagramError::UnsupportedByPeer) => {
                    warn!("peer does not support QUIC datagrams — video will not be delivered");
                }
                Err(SendDatagramError::Disabled) => {
                    warn!(
                        "QUIC datagrams disabled on this connection — video will not be delivered"
                    );
                }
                Err(SendDatagramError::ConnectionLost(e)) => {
                    debug!("connection lost while sending video datagram: {e}");
                }
            }
        }
    }
}
