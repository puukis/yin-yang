//! UDP video receiver: reassembles fragmented frames and hands them to the decoder.

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, info, warn};

use streamd_proto::packets::parse_video_header as parse_header;

/// A reassembled, ready-to-decode frame.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct DecodedFrame {
    /// Concatenated NAL data for the entire frame (all slices, in order).
    pub data: Vec<u8>,
    pub frame_seq: u32,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
}

pub struct VideoReceiver {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoReceiver {
    pub fn start(
        local_port: u16,
        server_ip: IpAddr,
    ) -> Result<(Self, crossbeam_channel::Receiver<DecodedFrame>)> {
        let bind_addr: SocketAddr = format!("0.0.0.0:{local_port}").parse()?;
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("create UDP recv socket")?;

        socket.set_reuse_address(true)?;
        socket.set_recv_buffer_size(8 * 1024 * 1024)?;
        // SO_BUSY_POLL: busy-wait up to 50µs before blocking, keeping the path hot.
        // setsockopt(SO_BUSY_POLL, 50) — available on Linux only.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let val: libc::c_int = 50;
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_BUSY_POLL,
                    &val as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&val) as libc::socklen_t,
                );
            }
        }
        socket.bind(&bind_addr.into())?;
        let udp: std::net::UdpSocket = socket.into();
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;

        info!("video receiver listening on {bind_addr} for server {server_ip}");

        let (frame_tx, frame_rx) = crossbeam_channel::bounded(8);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let thread = std::thread::Builder::new()
            .name("streamd-video-rx".into())
            .spawn(move || {
                receive_loop(udp, frame_tx, stop_clone);
            })
            .context("spawn video receive thread")?;

        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            frame_rx,
        ))
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Per-frame reassembly state.
struct FrameState {
    /// slice_idx → vec of (frag_idx, data) pairs
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

fn receive_loop(
    socket: std::net::UdpSocket,
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    // Circular buffer of reassembly state, indexed by frame_seq % 128
    let mut frames: HashMap<u32, FrameState> = HashMap::new();
    let mut first_packet_logged = false;
    let mut first_fragment_logged = false;
    let mut first_parse_failure_logged = false;
    let mut first_frame_logged = false;

    while !stop.load(Ordering::Relaxed) {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(result) => result,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => {
                debug!("UDP recv error: {e}");
                continue;
            }
        };

        if !first_packet_logged {
            info!("video receiver received first UDP datagram from {peer} ({n} bytes)");
            first_packet_logged = true;
        }

        let Some((hdr, payload)) = parse_header(&buf[..n]) else {
            if !first_parse_failure_logged {
                warn!("video receiver dropped unparsable UDP datagram from {peer} ({n} bytes)");
                first_parse_failure_logged = true;
            }
            continue;
        };

        if !first_fragment_logged {
            info!(
                "video receiver accepted first fragment seq={} slice={} frag={}/{} keyframe={}",
                hdr.frame_seq,
                hdr.slice_idx,
                hdr.frag_idx + 1,
                hdr.frag_total,
                hdr.is_keyframe()
            );
            first_fragment_logged = true;
        }

        let entry = frames.entry(hdr.frame_seq).or_insert_with(|| FrameState {
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

        // Check if all slices in this frame are complete
        let all_complete = if let Some(total) = entry.num_slices_expected {
            (0..total).all(|i| entry.slices.get(&i).map_or(false, |s| s.is_complete()))
        } else {
            false
        };

        if all_complete {
            // Assemble slices in order
            let state = frames.remove(&hdr.frame_seq).unwrap();
            let mut data = Vec::new();
            let mut slice_ids: Vec<u8> = state.slices.keys().copied().collect();
            slice_ids.sort_unstable();
            for id in slice_ids {
                data.extend_from_slice(&state.slices[&id].assemble());
            }

            match frame_tx.try_send(DecodedFrame {
                data,
                frame_seq: hdr.frame_seq,
                timestamp_us: state.timestamp_us,
                is_keyframe: state.is_keyframe,
            }) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {
                    if !first_frame_logged {
                        info!(
                            "video receiver assembled first frame seq={} keyframe={}",
                            hdr.frame_seq, state.is_keyframe
                        );
                        first_frame_logged = true;
                    }
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
            }

            // Evict frames older than this one (they will never complete)
            let seq = hdr.frame_seq;
            frames.retain(|&k, _| k > seq || seq.wrapping_sub(k) < 64);
        }
    }

    info!("video receive thread exited");
}
