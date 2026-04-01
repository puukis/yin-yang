//! Video frame reassembly from QUIC unreliable datagrams.
//!
//! The server sends each encoded frame as one or more datagram fragments, each
//! carrying an 18-byte `VideoPacketHeader` followed by compressed video data.
//! `VideoFrameReassembler` accumulates those fragments and emits a complete
//! `DecodedFrame` once every fragment of every slice in a frame has arrived.
//!
//! Because QUIC datagrams are unreliable, fragments can be lost. The receiver
//! evicts any incomplete frames whose sequence number falls more than 64
//! frames behind the latest completed frame, allowing the decoder to recover
//! by requesting an IDR (keyframe) from the server.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

use streamd_proto::packets::parse_video_header as parse_header;

/// A reassembled, ready-to-decode frame.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct DecodedFrame {
    /// Concatenated NAL data for the entire frame (all slices, in order).
    pub data: Vec<u8>,
    pub frame_seq: u32,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
    /// Local wall-clock time (µs since Unix epoch) when the frame was fully
    /// reassembled. Used to measure end-to-end latency.
    pub received_at_us: u64,
}

/// Per-frame reassembly state.
struct FrameState {
    slices: HashMap<u8, SliceState>,
    num_slices_expected: Option<u8>,
    timestamp_us: u64,
    is_keyframe: bool,
}

struct SliceState {
    frags: Vec<Option<Vec<u8>>>,
    received: u16,
    total: u16,
}

impl SliceState {
    fn new(total: u16) -> Self {
        Self {
            frags: vec![None; total as usize],
            received: 0,
            total,
        }
    }

    fn insert(&mut self, idx: u16, data: Vec<u8>) {
        if self.frags[idx as usize].is_none() {
            self.frags[idx as usize] = Some(data);
            self.received += 1;
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.total
    }

    fn assemble(&self) -> Vec<u8> {
        self.frags
            .iter()
            .flatten()
            .flat_map(|v| v.iter().copied())
            .collect()
    }
}

/// Statistics window logged once per second.
struct ReassemblerStats {
    assembled_frames: u32,
    dropped_frames: u32,
    window_started_at: std::time::Instant,
}

impl ReassemblerStats {
    fn new() -> Self {
        Self {
            assembled_frames: 0,
            dropped_frames: 0,
            window_started_at: std::time::Instant::now(),
        }
    }

    fn record_assembled(&mut self) {
        self.assembled_frames += 1;
    }

    fn record_dropped(&mut self) {
        self.dropped_frames += 1;
    }

    fn maybe_log(&mut self) {
        if self.window_started_at.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        info!(
            "video reassembler: assembled={} dropped={}",
            self.assembled_frames, self.dropped_frames
        );
        *self = Self::new();
    }
}

/// Stateful reassembler that turns a stream of raw datagram payloads (the
/// bytes *after* the `DATAGRAM_TAG_VIDEO` byte has been stripped) into
/// complete `DecodedFrame`s.
///
/// Call `push_datagram` for each video datagram payload received from
/// `Connection::read_datagram`. It returns `Some(frame)` when the last
/// fragment of a frame arrives, `None` otherwise.
pub struct VideoFrameReassembler {
    /// In-flight reassembly state, keyed by `frame_seq`.
    frames: HashMap<u32, FrameState>,
    stats: ReassemblerStats,
    first_fragment_logged: bool,
    first_frame_logged: bool,
}

impl VideoFrameReassembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
            stats: ReassemblerStats::new(),
            first_fragment_logged: false,
            first_frame_logged: false,
        }
    }

    /// Feed one datagram payload (tag byte already stripped).
    ///
    /// Returns `Some(frame)` when a complete frame is assembled, `None`
    /// while waiting for more fragments.
    pub fn push_datagram(&mut self, data: &[u8]) -> Option<DecodedFrame> {
        let Some((hdr, payload)) = parse_header(data) else {
            debug!(
                "video reassembler: unparsable datagram ({} bytes)",
                data.len()
            );
            return None;
        };

        if !self.first_fragment_logged {
            info!(
                "video reassembler: first fragment seq={} slice={} frag={}/{} keyframe={}",
                hdr.frame_seq,
                hdr.slice_idx,
                hdr.frag_idx + 1,
                hdr.frag_total,
                hdr.is_keyframe()
            );
            self.first_fragment_logged = true;
        }

        let entry = self
            .frames
            .entry(hdr.frame_seq)
            .or_insert_with(|| FrameState {
                slices: HashMap::new(),
                num_slices_expected: None,
                timestamp_us: hdr.timestamp_us,
                is_keyframe: hdr.is_keyframe(),
            });

        if hdr.is_last_slice() {
            entry.num_slices_expected = Some(hdr.slice_idx + 1);
        }

        let slice = entry
            .slices
            .entry(hdr.slice_idx)
            .or_insert_with(|| SliceState::new(hdr.frag_total));

        slice.insert(hdr.frag_idx, payload.to_vec());

        // Check if every slice of this frame is complete.
        let all_complete = if let Some(total) = entry.num_slices_expected {
            (0..total).all(|i| entry.slices.get(&i).map_or(false, |s| s.is_complete()))
        } else {
            false
        };

        if !all_complete {
            return None;
        }

        let state = self.frames.remove(&hdr.frame_seq).unwrap();
        let mut assembled = Vec::new();
        let mut slice_ids: Vec<u8> = state.slices.keys().copied().collect();
        slice_ids.sort_unstable();
        for id in slice_ids {
            assembled.extend_from_slice(&state.slices[&id].assemble());
        }

        let received_at_us = now_us();
        let frame = DecodedFrame {
            data: assembled,
            frame_seq: hdr.frame_seq,
            timestamp_us: state.timestamp_us,
            is_keyframe: state.is_keyframe,
            received_at_us,
        };

        self.stats.record_assembled();
        if !self.first_frame_logged {
            info!(
                "video reassembler: first complete frame seq={} keyframe={}",
                hdr.frame_seq, state.is_keyframe
            );
            self.first_frame_logged = true;
        }
        self.stats.maybe_log();

        // Evict incomplete frames that are too far behind to ever complete.
        let seq = hdr.frame_seq;
        let before = self.frames.len();
        self.frames
            .retain(|&k, _| k > seq || seq.wrapping_sub(k) < 64);
        let evicted = before - self.frames.len();
        for _ in 0..evicted {
            self.stats.record_dropped();
        }

        Some(frame)
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
