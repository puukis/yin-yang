//! Video frame reassembly from QUIC unreliable datagrams.
//!
//! The server sends each encoded frame as one or more datagram fragments, each
//! carrying a fixed-size `VideoPacketHeader` followed by compressed video data
//! or XOR parity bytes. `VideoFrameReassembler` accumulates those packets and
//! emits a complete `DecodedFrame` once every data fragment of every slice in a
//! frame has arrived.
//!
//! Because QUIC datagrams are unreliable, fragments can still be lost. The
//! receiver first attempts single-fragment recovery inside each FEC group. If a
//! frame remains incomplete once it falls sufficiently far behind newer
//! traffic, the reassembler evicts it and signals that the client should
//! request an IDR keyframe from the server.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};
use yin_yang_proto::packets::{parse_video_header as parse_header, VIDEO_FEC_DATA_SHARDS};

// QUIC datagrams can arrive slightly later than newer frames when the sender is
// pacing a large burst such as a 4K/120 frame. Keep incomplete frames around
// long enough to catch those stragglers before escalating to an IDR request.
const FRAME_REORDER_WINDOW: u32 = 32;

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

/// Loss signal emitted by the reassembler when a frame became unrecoverable.
pub struct ReassemblyLoss {
    pub frame_seq: u32,
    pub dropped_frames: usize,
}

/// Result of feeding one video datagram into the reassembler.
pub struct PushOutcome {
    pub frame: Option<DecodedFrame>,
    pub loss: Option<ReassemblyLoss>,
    pub recovered_fragments: u16,
    pub recovered_frame: bool,
}

impl PushOutcome {
    fn empty() -> Self {
        Self {
            frame: None,
            loss: None,
            recovered_fragments: 0,
            recovered_frame: false,
        }
    }
}

/// Per-frame reassembly state.
struct FrameState {
    slices: HashMap<u8, SliceState>,
    num_slices_expected: Option<u8>,
    timestamp_us: u64,
    is_keyframe: bool,
}

impl FrameState {
    fn new(timestamp_us: u64, is_keyframe: bool) -> Self {
        Self {
            slices: HashMap::new(),
            num_slices_expected: None,
            timestamp_us,
            is_keyframe,
        }
    }

    fn recovered_fragments(&self) -> u32 {
        self.slices
            .values()
            .map(|slice| slice.recovered_fragments as u32)
            .sum()
    }
}

struct SliceState {
    data_frags: Vec<Option<Vec<u8>>>,
    parity_frags: Vec<Option<Vec<u8>>>,
    frag_total: u16,
    slice_len: u32,
    frag_payload_size: u16,
    recovered_fragments: u16,
}

impl SliceState {
    fn new(frag_total: u16, slice_len: u32, frag_payload_size: u16) -> Option<Self> {
        if frag_total == 0 {
            return None;
        }

        let group_count = usize::from(frag_total).div_ceil(VIDEO_FEC_DATA_SHARDS);
        Some(Self {
            data_frags: vec![None; frag_total as usize],
            parity_frags: vec![None; group_count],
            frag_total,
            slice_len,
            frag_payload_size,
            recovered_fragments: 0,
        })
    }

    fn matches_metadata(&self, frag_total: u16, slice_len: u32, frag_payload_size: u16) -> bool {
        self.frag_total == frag_total
            && self.slice_len == slice_len
            && self.frag_payload_size == frag_payload_size
    }

    fn insert_data(&mut self, frag_idx: u16, payload: Vec<u8>) -> bool {
        let Some(slot) = self.data_frags.get_mut(frag_idx as usize) else {
            return false;
        };
        if slot.is_none() {
            *slot = Some(payload);
            return true;
        }
        false
    }

    fn insert_parity(&mut self, group_idx: u16, payload: Vec<u8>) -> bool {
        let Some(slot) = self.parity_frags.get_mut(group_idx as usize) else {
            return false;
        };
        if slot.is_none() {
            *slot = Some(payload);
            return true;
        }
        false
    }

    fn is_complete(&self) -> bool {
        self.data_frags.iter().all(Option::is_some)
    }

    fn assemble(&self) -> Vec<u8> {
        self.data_frags
            .iter()
            .flatten()
            .flat_map(|frag| frag.iter().copied())
            .collect()
    }

    fn expected_payload_len(&self, frag_idx: usize) -> Option<usize> {
        if frag_idx >= self.data_frags.len() {
            return None;
        }

        let full_payload_len = usize::from(self.frag_payload_size);
        if frag_idx + 1 < self.data_frags.len() {
            return Some(full_payload_len);
        }

        let preceding = full_payload_len.checked_mul(self.data_frags.len().saturating_sub(1))?;
        let remaining = usize::try_from(self.slice_len)
            .ok()?
            .checked_sub(preceding)?;
        Some(remaining)
    }

    fn group_range(&self, group_idx: usize) -> Option<std::ops::Range<usize>> {
        let start = group_idx.checked_mul(VIDEO_FEC_DATA_SHARDS)?;
        if start >= self.data_frags.len() {
            return None;
        }
        let end = (start + VIDEO_FEC_DATA_SHARDS).min(self.data_frags.len());
        Some(start..end)
    }

    fn expected_group_payload_len(&self, group_idx: usize) -> Option<usize> {
        let range = self.group_range(group_idx)?;
        range
            .map(|frag_idx| self.expected_payload_len(frag_idx))
            .collect::<Option<Vec<_>>>()
            .and_then(|lengths| lengths.into_iter().max())
    }

    fn maybe_recover_group(&mut self, group_idx: usize) -> bool {
        let Some(parity) = self
            .parity_frags
            .get(group_idx)
            .and_then(|payload| payload.as_ref())
        else {
            return false;
        };
        let Some(group_range) = self.group_range(group_idx) else {
            return false;
        };
        let Some(group_payload_len) = self.expected_group_payload_len(group_idx) else {
            return false;
        };

        if parity.len() != group_payload_len {
            return false;
        }

        let mut missing_frag_idx = None;
        let mut missing_count = 0usize;
        for frag_idx in group_range.clone() {
            if self.data_frags[frag_idx].is_none() {
                missing_frag_idx = Some(frag_idx);
                missing_count += 1;
            }
        }

        if missing_count != 1 {
            return false;
        }

        let missing_frag_idx = missing_frag_idx.expect("checked exactly one missing fragment");
        let Some(expected_missing_len) = self.expected_payload_len(missing_frag_idx) else {
            return false;
        };

        let mut recovered = parity.clone();
        for frag_idx in group_range {
            if frag_idx == missing_frag_idx {
                continue;
            }
            let Some(fragment) = self.data_frags[frag_idx].as_ref() else {
                return false;
            };
            for (dst, src) in recovered.iter_mut().zip(fragment.iter().copied()) {
                *dst ^= src;
            }
        }
        recovered.truncate(expected_missing_len);

        self.data_frags[missing_frag_idx] = Some(recovered);
        self.recovered_fragments = self.recovered_fragments.saturating_add(1);
        true
    }
}

/// Statistics window logged once per second.
struct ReassemblerStats {
    assembled_frames: u32,
    dropped_frames: u32,
    recovered_fragments: u32,
    recovered_frames: u32,
    window_started_at: Instant,
}

impl ReassemblerStats {
    fn new() -> Self {
        Self {
            assembled_frames: 0,
            dropped_frames: 0,
            recovered_fragments: 0,
            recovered_frames: 0,
            window_started_at: Instant::now(),
        }
    }

    fn record_assembled(&mut self) {
        self.assembled_frames += 1;
    }

    fn record_dropped(&mut self, frames: usize) {
        self.dropped_frames = self.dropped_frames.saturating_add(frames as u32);
    }

    fn record_recovered_fragment(&mut self) {
        self.recovered_fragments += 1;
    }

    fn record_recovered_frame(&mut self) {
        self.recovered_frames += 1;
    }

    fn maybe_log(&mut self) {
        if self.window_started_at.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        info!(
            "video reassembler: assembled={} dropped={} recovered_fragments={} recovered_frames={}",
            self.assembled_frames,
            self.dropped_frames,
            self.recovered_fragments,
            self.recovered_frames
        );
        *self = Self::new();
    }
}

/// Stateful reassembler that turns a stream of raw datagram payloads (the
/// bytes *after* the `DATAGRAM_TAG_VIDEO` byte has been stripped) into
/// complete `DecodedFrame`s.
pub struct VideoFrameReassembler {
    /// In-flight reassembly state, keyed by `frame_seq`.
    frames: HashMap<u32, FrameState>,
    stats: ReassemblerStats,
    first_fragment_logged: bool,
    first_frame_logged: bool,
    max_seen_frame_seq: Option<u32>,
}

impl Default for VideoFrameReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoFrameReassembler {
    pub fn new() -> Self {
        Self {
            frames: HashMap::new(),
            stats: ReassemblerStats::new(),
            first_fragment_logged: false,
            first_frame_logged: false,
            max_seen_frame_seq: None,
        }
    }

    /// Feed one datagram payload (tag byte already stripped).
    ///
    /// Returns a decoded frame when a full frame was assembled and a loss
    /// signal when one or more frames became unrecoverable.
    pub fn push_datagram(&mut self, data: &[u8]) -> PushOutcome {
        let Some((hdr, payload)) = parse_header(data) else {
            debug!(
                "video reassembler: unparsable datagram ({} bytes)",
                data.len()
            );
            return PushOutcome::empty();
        };

        self.max_seen_frame_seq = Some(match self.max_seen_frame_seq {
            Some(current) if is_older_than(hdr.frame_seq, current) => current,
            _ => hdr.frame_seq,
        });

        if !self.first_fragment_logged {
            info!(
                "video reassembler: first fragment seq={} slice={} frag={} keyframe={} parity={}",
                hdr.frame_seq,
                hdr.slice_idx,
                hdr.frag_idx,
                hdr.is_keyframe(),
                hdr.is_fec_parity()
            );
            self.first_fragment_logged = true;
        }

        let mut loss = None;
        let mut drop_frame = false;
        let mut recovered_fragments = 0u16;

        {
            let entry = self
                .frames
                .entry(hdr.frame_seq)
                .or_insert_with(|| FrameState::new(hdr.timestamp_us, hdr.is_keyframe()));
            entry.timestamp_us = hdr.timestamp_us;
            entry.is_keyframe |= hdr.is_keyframe();

            if hdr.is_last_slice() {
                entry.num_slices_expected = Some(match entry.num_slices_expected {
                    Some(existing) => existing.max(hdr.slice_idx.saturating_add(1)),
                    None => hdr.slice_idx.saturating_add(1),
                });
            }

            let slice = match entry.slices.entry(hdr.slice_idx) {
                std::collections::hash_map::Entry::Occupied(existing) => Some(existing.into_mut()),
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    match SliceState::new(hdr.frag_total, hdr.slice_len, hdr.frag_payload_size) {
                        Some(slice) => Some(vacant.insert(slice)),
                        None => {
                            warn!(
                                "dropping malformed frame {} slice {}: zero data fragments",
                                hdr.frame_seq, hdr.slice_idx
                            );
                            drop_frame = true;
                            None
                        }
                    }
                }
            };

            if let Some(slice) = slice {
                if !slice.matches_metadata(hdr.frag_total, hdr.slice_len, hdr.frag_payload_size) {
                    warn!(
                        "dropping frame {} after inconsistent slice metadata on slice {}",
                        hdr.frame_seq, hdr.slice_idx
                    );
                    drop_frame = true;
                } else if hdr.is_fec_parity() {
                    let expected_len = match slice.expected_group_payload_len(hdr.frag_idx as usize)
                    {
                        Some(expected_len) => expected_len,
                        None => {
                            warn!(
                                "dropping malformed parity packet for frame {} slice {} group {}",
                                hdr.frame_seq, hdr.slice_idx, hdr.frag_idx
                            );
                            drop_frame = true;
                            0
                        }
                    };

                    if !drop_frame && payload.len() != expected_len {
                        warn!(
                            "dropping malformed parity payload for frame {} slice {} group {}: got {} bytes expected {}",
                            hdr.frame_seq,
                            hdr.slice_idx,
                            hdr.frag_idx,
                            payload.len(),
                            expected_len
                        );
                        drop_frame = true;
                    } else if !drop_frame {
                        let _ = slice.insert_parity(hdr.frag_idx, payload.to_vec());
                        if slice.maybe_recover_group(hdr.frag_idx as usize) {
                            recovered_fragments = recovered_fragments.saturating_add(1);
                        }
                    }
                } else {
                    let expected_len = match slice.expected_payload_len(hdr.frag_idx as usize) {
                        Some(expected_len) => expected_len,
                        None => {
                            warn!(
                                "dropping malformed data packet for frame {} slice {} frag {}",
                                hdr.frame_seq, hdr.slice_idx, hdr.frag_idx
                            );
                            drop_frame = true;
                            0
                        }
                    };

                    if !drop_frame && payload.len() != expected_len {
                        warn!(
                            "dropping malformed data payload for frame {} slice {} frag {}: got {} bytes expected {}",
                            hdr.frame_seq,
                            hdr.slice_idx,
                            hdr.frag_idx,
                            payload.len(),
                            expected_len
                        );
                        drop_frame = true;
                    } else if !drop_frame && slice.insert_data(hdr.frag_idx, payload.to_vec()) {
                        let group_idx = usize::from(hdr.frag_idx) / VIDEO_FEC_DATA_SHARDS;
                        if slice.maybe_recover_group(group_idx) {
                            recovered_fragments = recovered_fragments.saturating_add(1);
                        }
                    }
                }
            } else {
                drop_frame = true;
            }
        }

        for _ in 0..recovered_fragments {
            self.stats.record_recovered_fragment();
        }

        if drop_frame {
            let _ = self.frames.remove(&hdr.frame_seq);
            self.stats.record_dropped(1);
            self.stats.maybe_log();
            return PushOutcome {
                frame: None,
                loss: Some(ReassemblyLoss {
                    frame_seq: hdr.frame_seq,
                    dropped_frames: 1,
                }),
                recovered_fragments,
                recovered_frame: false,
            };
        }

        let frame_complete = self
            .frames
            .get(&hdr.frame_seq)
            .and_then(|entry| entry.num_slices_expected.map(|total| (entry, total)))
            .is_some_and(|(entry, total)| {
                (0..total).all(|slice_idx| {
                    entry
                        .slices
                        .get(&slice_idx)
                        .is_some_and(SliceState::is_complete)
                })
            });

        let mut recovered_frame = false;
        let frame = if frame_complete {
            let state = self.frames.remove(&hdr.frame_seq).expect("frame exists");
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
            if state.recovered_fragments() > 0 {
                recovered_frame = true;
                self.stats.record_recovered_frame();
            }
            if !self.first_frame_logged {
                info!(
                    "video reassembler: first complete frame seq={} keyframe={}",
                    hdr.frame_seq, state.is_keyframe
                );
                self.first_frame_logged = true;
            }
            Some(frame)
        } else {
            None
        };

        if let Some(stale_loss) = self.evict_stale_frames() {
            loss = Some(stale_loss);
        }
        self.stats.maybe_log();

        PushOutcome {
            frame,
            loss,
            recovered_fragments,
            recovered_frame,
        }
    }

    fn evict_stale_frames(&mut self) -> Option<ReassemblyLoss> {
        let max_seen_frame_seq = self.max_seen_frame_seq?;
        let stale_frames = self
            .frames
            .keys()
            .copied()
            .filter(|&frame_seq| is_significantly_older(max_seen_frame_seq, frame_seq))
            .collect::<Vec<_>>();

        if stale_frames.is_empty() {
            return None;
        }

        let mut oldest = stale_frames[0];
        for frame_seq in &stale_frames[1..] {
            if is_older_than(*frame_seq, oldest) {
                oldest = *frame_seq;
            }
        }

        for frame_seq in &stale_frames {
            let _ = self.frames.remove(frame_seq);
        }
        self.stats.record_dropped(stale_frames.len());

        Some(ReassemblyLoss {
            frame_seq: oldest,
            dropped_frames: stale_frames.len(),
        })
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn is_older_than(lhs: u32, rhs: u32) -> bool {
    let distance = rhs.wrapping_sub(lhs);
    distance != 0 && distance < (u32::MAX / 2)
}

fn is_significantly_older(reference: u32, seq: u32) -> bool {
    let distance = reference.wrapping_sub(seq);
    distance > FRAME_REORDER_WINDOW && distance < (u32::MAX / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yin_yang_proto::packets::{VideoFlags, VideoPacketHeader};

    #[allow(clippy::too_many_arguments)]
    fn build_packet(
        frame_seq: u32,
        slice_idx: u8,
        flags: VideoFlags,
        frag_idx: u16,
        frag_total: u16,
        slice_len: u32,
        frag_payload_size: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = Vec::with_capacity(VideoPacketHeader::SIZE + payload.len());
        packet.extend_from_slice(&frame_seq.to_le_bytes());
        packet.extend_from_slice(&123_u64.to_le_bytes());
        packet.push(slice_idx);
        packet.push(flags.bits());
        packet.extend_from_slice(&frag_idx.to_le_bytes());
        packet.extend_from_slice(&frag_total.to_le_bytes());
        packet.extend_from_slice(&slice_len.to_le_bytes());
        packet.extend_from_slice(&frag_payload_size.to_le_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn recovers_single_lost_fragment_from_parity() {
        let mut reassembler = VideoFrameReassembler::new();

        let frag_total = 4;
        let frag_payload_size = 3;
        let slice_len = 12;
        let fragments = [b"abc", b"def", b"ghi", b"jkl"];
        let parity = {
            let mut parity = vec![0u8; 3];
            for fragment in fragments {
                for (dst, src) in parity.iter_mut().zip(fragment.iter().copied()) {
                    *dst ^= src;
                }
            }
            parity
        };

        for (frag_idx, fragment) in fragments.into_iter().enumerate() {
            if frag_idx == 2 {
                continue;
            }
            let outcome = reassembler.push_datagram(&build_packet(
                1,
                0,
                VideoFlags::LAST_SLICE,
                frag_idx as u16,
                frag_total,
                slice_len,
                frag_payload_size,
                fragment,
            ));
            assert!(outcome.frame.is_none());
            assert!(outcome.loss.is_none());
        }

        let outcome = reassembler.push_datagram(&build_packet(
            1,
            0,
            VideoFlags::LAST_SLICE | VideoFlags::FEC_PARITY,
            0,
            frag_total,
            slice_len,
            frag_payload_size,
            &parity,
        ));

        let frame = outcome.frame.expect("frame should be recovered");
        assert_eq!(frame.data, b"abcdefghijkl");
        assert!(outcome.loss.is_none());
    }

    #[test]
    fn signals_loss_after_unrecoverable_gap() {
        let mut reassembler = VideoFrameReassembler::new();

        let partial = build_packet(10, 0, VideoFlags::LAST_SLICE, 0, 2, 6, 3, b"abc");
        let outcome = reassembler.push_datagram(&partial);
        assert!(outcome.frame.is_none());
        assert!(outcome.loss.is_none());

        let complete_frame = build_packet(50, 0, VideoFlags::LAST_SLICE, 0, 1, 3, 3, b"xyz");
        let outcome = reassembler.push_datagram(&complete_frame);
        assert_eq!(
            outcome.frame.expect("new frame should complete").data,
            b"xyz"
        );
        assert!(outcome.loss.is_some());
        let loss = outcome.loss.expect("loss signal expected");
        assert_eq!(loss.frame_seq, 10);
        assert_eq!(loss.dropped_frames, 1);
    }
}
