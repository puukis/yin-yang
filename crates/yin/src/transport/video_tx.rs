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
//! [VideoPacketHeader  (24 bytes)]
//! [encoded video data or XOR parity bytes (variable)]
//! ```
//!
//! Datagrams are unreliable and unordered, matching raw-UDP semantics. A lost
//! fragment is first repaired by XOR parity FEC when exactly one fragment is
//! missing from a protection group. Unrecoverable loss causes the receiver to
//! drop the incomplete frame and request an IDR.
//!
//! ## Fragment payload sizing
//!
//! The maximum video data bytes that fit in one datagram is derived from the
//! QUIC-negotiated path MTU reported by [`Connection::max_datagram_size`]:
//!
//! ```text
//! fragment_payload = max_datagram_size − 1 (type tag) − 24 (VideoPacketHeader)
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
use tracing::{debug, info, warn};
use yin_yang_proto::control::encode_video_datagram;
use yin_yang_proto::packets::{VideoFlags, VideoPacketHeader, MTU_WAN, VIDEO_FEC_DATA_SHARDS};

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameSendStats {
    pub send_wait_us: u64,
    pub max_send_wait_us: u32,
    pub datagrams_sent: u32,
    pub datagrams_dropped: u32,
    pub max_datagram_size: u32,
    pub fragment_payload_bytes: u32,
}

#[derive(Debug, Clone, Copy)]
struct DatagramLimits {
    max_datagram_size: u32,
    fragment_payload_bytes: u32,
}

/// Sends encoded video frames over QUIC unreliable datagrams.
pub struct QuicVideoSender {
    conn: Connection,
    runtime: tokio::runtime::Handle,
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
    pub fn new(conn: Connection, runtime: tokio::runtime::Handle) -> Self {
        Self {
            conn,
            runtime,
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
    /// - When Quinn already knows a smaller live application-datagram limit,
    ///   that limit must be respected exactly. Clamping it back up to
    ///   `MTU_WAN` would manufacture oversized datagrams that Quinn rejects.
    ///
    /// # Logging
    ///
    /// Emits an `info!` log the first time a payload size is established and
    /// again whenever it changes (upward as PMTU probing succeeds, or downward
    /// if the path degrades). This makes it easy to confirm in logs that LAN
    /// sessions are using the full negotiated MTU rather than the conservative
    /// WAN fallback.
    fn current_fragment_limits(&mut self) -> DatagramLimits {
        let max_dg = self
            .conn
            .max_datagram_size()
            // PMTU probing not yet complete — use the conservative WAN floor.
            .unwrap_or(MTU_WAN);

        // Subtract the 1-byte datagram type tag and the fixed 24-byte header.
        let payload = max_dg.saturating_sub(1 + VideoPacketHeader::SIZE).max(1);

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

        DatagramLimits {
            max_datagram_size: max_dg as u32,
            fragment_payload_bytes: payload as u32,
        }
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
    pub fn send_frame(
        &mut self,
        slices: &[Vec<u8>],
        is_keyframe: bool,
        timestamp_us: u64,
    ) -> FrameSendStats {
        let frame_seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let num_slices = slices.len();

        // Derive the fragment payload once for this frame. Using a single
        // consistent value across all slices is required for correctness (see
        // module doc). The per-frame query — rather than a cached value from
        // connection setup — ensures that PMTU probing results are reflected
        // as soon as the QUIC stack reports them.
        let limits = self.current_fragment_limits();
        let mut stats = FrameSendStats {
            max_datagram_size: limits.max_datagram_size,
            fragment_payload_bytes: limits.fragment_payload_bytes,
            ..Default::default()
        };
        let fragment_payload = limits.fragment_payload_bytes as usize;

        for (slice_idx, slice_data) in slices.iter().enumerate() {
            let is_last_slice = slice_idx == num_slices - 1;
            if !self.send_slice(
                frame_seq,
                timestamp_us,
                slice_idx as u8,
                is_last_slice,
                is_keyframe,
                slice_data,
                fragment_payload,
                &mut stats,
            ) {
                return stats;
            }
        }

        stats
    }

    #[allow(clippy::too_many_arguments)]
    fn send_slice(
        &self,
        frame_seq: u32,
        timestamp_us: u64,
        slice_idx: u8,
        is_last_slice: bool,
        is_keyframe: bool,
        data: &[u8],
        fragment_payload: usize,
        stats: &mut FrameSendStats,
    ) -> bool {
        let total_frags = data.len().div_ceil(fragment_payload) as u16;
        let slice_len = data.len() as u32;
        let Ok(frag_payload_size) = u16::try_from(fragment_payload) else {
            warn!(
                "fragment payload {} exceeds u16 wire format; dropping slice {} of frame {}",
                fragment_payload, slice_idx, frame_seq
            );
            return false;
        };
        let chunks = data
            .chunks(fragment_payload)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();

        for (frag_idx, chunk) in chunks.iter().enumerate() {
            let mut flags = VideoFlags::empty();
            if is_keyframe {
                flags |= VideoFlags::KEY_FRAME;
            }
            if is_last_slice {
                flags |= VideoFlags::LAST_SLICE;
            }

            if !self.send_packet(
                frame_seq,
                timestamp_us,
                slice_idx,
                flags,
                frag_idx as u16,
                total_frags,
                slice_len,
                frag_payload_size,
                chunk,
                stats,
            ) {
                return false;
            }
        }

        for (group_idx, group) in chunks.chunks(VIDEO_FEC_DATA_SHARDS).enumerate() {
            let parity_len = group.iter().map(Vec::len).max().unwrap_or(0);
            if parity_len == 0 {
                continue;
            }

            let mut parity = vec![0u8; parity_len];
            for chunk in group {
                for (dst, src) in parity.iter_mut().zip(chunk.iter().copied()) {
                    *dst ^= src;
                }
            }

            let mut flags = VideoFlags::FEC_PARITY;
            if is_keyframe {
                flags |= VideoFlags::KEY_FRAME;
            }
            if is_last_slice {
                flags |= VideoFlags::LAST_SLICE;
            }

            if !self.send_packet(
                frame_seq,
                timestamp_us,
                slice_idx,
                flags,
                group_idx as u16,
                total_frags,
                slice_len,
                frag_payload_size,
                &parity,
                stats,
            ) {
                return false;
            }
        }

        true
    }

    #[allow(clippy::too_many_arguments)]
    fn send_packet(
        &self,
        frame_seq: u32,
        timestamp_us: u64,
        slice_idx: u8,
        flags: VideoFlags,
        frag_idx: u16,
        frag_total: u16,
        slice_len: u32,
        frag_payload_size: u16,
        payload: &[u8],
        stats: &mut FrameSendStats,
    ) -> bool {
        let mut packet = Vec::with_capacity(VideoPacketHeader::SIZE + payload.len());
        packet.extend_from_slice(&frame_seq.to_le_bytes());
        packet.extend_from_slice(&timestamp_us.to_le_bytes());
        packet.push(slice_idx);
        packet.push(flags.bits());
        packet.extend_from_slice(&frag_idx.to_le_bytes());
        packet.extend_from_slice(&frag_total.to_le_bytes());
        packet.extend_from_slice(&slice_len.to_le_bytes());
        packet.extend_from_slice(&frag_payload_size.to_le_bytes());
        packet.extend_from_slice(payload);

        let datagram = encode_video_datagram(&packet);
        let datagram_len = datagram.len();
        let send_started_at = std::time::Instant::now();
        let send_result = if self.conn.datagram_send_buffer_space() >= datagram_len {
            self.conn.send_datagram(datagram)
        } else {
            self.runtime
                .block_on(self.conn.send_datagram_wait(datagram))
        };

        if send_started_at.elapsed() >= std::time::Duration::from_millis(1) {
            debug!(
                "video datagram send backpressure: waited {:?} for {} bytes (frame {} slice {} frag {})",
                send_started_at.elapsed(),
                datagram_len,
                frame_seq,
                slice_idx,
                frag_idx
            );
        }
        let wait_us = send_started_at.elapsed().as_micros().min(u32::MAX as u128) as u32;
        stats.send_wait_us += wait_us as u64;
        stats.max_send_wait_us = stats.max_send_wait_us.max(wait_us);

        match send_result {
            Ok(()) => {
                stats.datagrams_sent = stats.datagrams_sent.saturating_add(1);
                true
            }
            Err(SendDatagramError::TooLarge) => {
                stats.datagrams_dropped = stats.datagrams_dropped.saturating_add(1);
                debug!(
                    "video datagram too large ({} bytes) — live max_datagram_size={:?}; dropping the rest of this frame so the next frame can re-fragment",
                    packet.len() + 1,
                    self.conn.max_datagram_size(),
                );
                false
            }
            Err(SendDatagramError::UnsupportedByPeer) => {
                stats.datagrams_dropped = stats.datagrams_dropped.saturating_add(1);
                warn!("peer does not support QUIC datagrams — video will not be delivered");
                false
            }
            Err(SendDatagramError::Disabled) => {
                stats.datagrams_dropped = stats.datagrams_dropped.saturating_add(1);
                warn!("QUIC datagrams disabled on this connection — video will not be delivered");
                false
            }
            Err(SendDatagramError::ConnectionLost(e)) => {
                stats.datagrams_dropped = stats.datagrams_dropped.saturating_add(1);
                debug!("connection lost while sending video datagram: {e}");
                false
            }
        }
    }
}
