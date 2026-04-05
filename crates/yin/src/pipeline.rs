//! Pipeline orchestration: capture → encode → QUIC datagram send.
//!
//! The pipeline runs on a dedicated OS thread pinned to physical cores 0-3
//! with SCHED_FIFO priority 50 to prevent kernel preemption mid-frame.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};
use yin_yang_proto::packets::{ClientTelemetry, Codec, ServerTelemetry};

#[cfg(target_os = "linux")]
use crate::capture::wayland::{CaptureMode, WaylandCapture};
#[cfg(target_os = "windows")]
use crate::capture::windows::WindowsCapture;
use crate::capture::{CaptureFrame, CaptureStats, CursorEvent};
use crate::encode::nvenc::{NvencConfig, NvencEncoder};
use crate::transport::video_tx::{FrameSendStats, QuicVideoSender};

const FRAME_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ADAPTIVE_WINDOW_INTERVAL: Duration = Duration::from_millis(500);
const CLIENT_TELEMETRY_MAX_AGE: Duration = Duration::from_secs(2);
const ADAPTIVE_RECONFIGURE_COOLDOWN: Duration = Duration::from_secs(3);
const DEGRADED_WINDOWS_FOR_BITRATE_STEP: u8 = 3;
const DEGRADED_WINDOWS_FOR_FPS_STEP: u8 = 6;
const CLEAN_WINDOWS_FOR_BITRATE_STEP: u8 = 8;
const CLEAN_WINDOWS_FOR_FPS_STEP: u8 = 12;

#[derive(Debug, Clone, Copy)]
pub struct AdaptiveStreamConfig {
    pub enabled: bool,
    pub min_fps: u8,
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
}

#[derive(Debug, Clone)]
struct LatestClientTelemetry {
    sample: ClientTelemetry,
    received_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct AdaptiveStreamController {
    min_fps: u8,
    max_fps: u8,
    min_bitrate_bps: u32,
    max_bitrate_bps: u32,
    current_fps: u8,
    current_bitrate_bps: u32,
    degraded_windows: u8,
    clean_windows: u8,
    clean_at_ceiling_windows: u8,
    last_reconfigure_at: Option<Instant>,
}

impl AdaptiveStreamController {
    fn new(base_config: &NvencConfig, bounds: AdaptiveStreamConfig) -> Self {
        let max_fps = base_config.fps.clamp(1, 120);
        let min_fps = bounds.min_fps.clamp(1, max_fps);
        let max_bitrate_bps = if bounds.max_bitrate_bps > 0 {
            bounds.max_bitrate_bps.min(base_config.bitrate_bps)
        } else {
            base_config.bitrate_bps
        };
        let default_floor = (max_bitrate_bps / 4).max(4_000_000).min(max_bitrate_bps);
        let min_bitrate_bps = if bounds.min_bitrate_bps > 0 {
            bounds.min_bitrate_bps.min(max_bitrate_bps)
        } else {
            default_floor
        };

        Self {
            min_fps,
            max_fps,
            min_bitrate_bps,
            max_bitrate_bps,
            current_fps: max_fps,
            current_bitrate_bps: max_bitrate_bps,
            degraded_windows: 0,
            clean_windows: 0,
            clean_at_ceiling_windows: 0,
            last_reconfigure_at: None,
        }
    }

    fn current_fps(&self) -> u8 {
        self.current_fps
    }

    fn current_bitrate_bps(&self) -> u32 {
        self.current_bitrate_bps
    }

    fn current_config(&self, mut base_config: NvencConfig) -> NvencConfig {
        base_config.fps = self.current_fps;
        base_config.bitrate_bps = self.current_bitrate_bps;
        base_config
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.current_fps.max(1) as u64)
    }

    fn observe_window(
        &mut self,
        telemetry: &ServerTelemetry,
        latest_client_telemetry: Option<&LatestClientTelemetry>,
        now: Instant,
    ) -> Option<(u8, u32)> {
        let frame_interval_us = 1_000_000u32 / self.current_fps.max(1) as u32;
        let fresh_client_telemetry = latest_client_telemetry
            .filter(|latest| now.duration_since(latest.received_at) <= CLIENT_TELEMETRY_MAX_AGE);
        let degraded = telemetry.video_datagrams_dropped > 0
            || telemetry.avg_send_wait_us > 1_000
            || telemetry.max_send_wait_us > 5_000
            || fresh_client_telemetry.is_some_and(|latest| {
                latest.sample.unrecoverable_frames > 0
                    || latest.sample.render_dropped_frames > 0
                    || latest.sample.avg_render_queue_us > frame_interval_us.saturating_mul(2)
            });
        let clean = !degraded && telemetry.avg_send_wait_us < 500;
        let cooldown_ready = self
            .last_reconfigure_at
            .is_none_or(|last| now.duration_since(last) >= ADAPTIVE_RECONFIGURE_COOLDOWN);

        let previous = (self.current_fps, self.current_bitrate_bps);

        if degraded {
            self.degraded_windows = self.degraded_windows.saturating_add(1);
            self.clean_windows = 0;
            self.clean_at_ceiling_windows = 0;
            if self.current_bitrate_bps > self.min_bitrate_bps
                && self.degraded_windows >= DEGRADED_WINDOWS_FOR_BITRATE_STEP
                && cooldown_ready
            {
                self.current_bitrate_bps =
                    reduce_bitrate(self.current_bitrate_bps, self.min_bitrate_bps);
                self.degraded_windows = 0;
                self.last_reconfigure_at = Some(now);
            } else if self.current_bitrate_bps == self.min_bitrate_bps
                && self.current_fps > self.min_fps
                && self.degraded_windows >= DEGRADED_WINDOWS_FOR_FPS_STEP
                && cooldown_ready
            {
                self.current_fps = reduce_fps(self.current_fps, self.min_fps);
                self.degraded_windows = 0;
                self.last_reconfigure_at = Some(now);
            }
        } else if clean {
            self.degraded_windows = 0;
            if fresh_client_telemetry.is_none() {
                self.clean_windows = 0;
                self.clean_at_ceiling_windows = 0;
            } else if self.current_bitrate_bps < self.max_bitrate_bps {
                self.clean_windows = self.clean_windows.saturating_add(1);
                self.clean_at_ceiling_windows = 0;
                if self.clean_windows >= CLEAN_WINDOWS_FOR_BITRATE_STEP && cooldown_ready {
                    self.current_bitrate_bps =
                        increase_bitrate(self.current_bitrate_bps, self.max_bitrate_bps);
                    self.clean_windows = 0;
                    self.last_reconfigure_at = Some(now);
                }
            } else if self.current_fps < self.max_fps {
                self.clean_windows = 0;
                self.clean_at_ceiling_windows = self.clean_at_ceiling_windows.saturating_add(1);
                if self.clean_at_ceiling_windows >= CLEAN_WINDOWS_FOR_FPS_STEP && cooldown_ready {
                    self.current_fps = increase_fps(self.current_fps, self.max_fps);
                    self.clean_at_ceiling_windows = 0;
                    self.last_reconfigure_at = Some(now);
                }
            } else {
                self.clean_windows = 0;
                self.clean_at_ceiling_windows = 0;
            }
        } else {
            self.degraded_windows = 0;
            self.clean_windows = 0;
            self.clean_at_ceiling_windows = 0;
        }

        let current = (self.current_fps, self.current_bitrate_bps);
        (current != previous).then_some(current)
    }
}

fn reduce_bitrate(current: u32, floor: u32) -> u32 {
    if current <= floor {
        return floor;
    }

    let reduced = ((current as u64 * 85) / 100) as u32;
    floor.max(reduced.min(current.saturating_sub(1)))
}

fn increase_bitrate(current: u32, ceiling: u32) -> u32 {
    if current >= ceiling {
        return ceiling;
    }

    let increased = ((current as u64 * 105) / 100) as u32;
    ceiling.min(increased.max(current.saturating_add(1)))
}

fn reduce_fps(current: u8, floor: u8) -> u8 {
    if current <= floor {
        return floor;
    }

    let reduced = ((u32::from(current) * 80) / 100) as u8;
    floor.max(reduced.min(current.saturating_sub(1)))
}

fn increase_fps(current: u8, ceiling: u8) -> u8 {
    if current >= ceiling {
        return ceiling;
    }

    let increased = (u32::from(current) * 110).div_ceil(100) as u8;
    ceiling.min(increased.max(current.saturating_add(1)))
}

/// Statistics tracked by the pipeline thread for telemetry.
struct Stats {
    total_capture_wait_us: u64,
    total_capture_convert_us: u64,
    total_encode_us: u64,
    total_send_us: u64,
    total_send_wait_us: u64,
    total_pipeline_us: u64,
    max_send_wait_us: u32,
    frame_count: u32,
    idr_count: u8,
    video_datagrams_sent: u32,
    video_datagrams_dropped: u32,
    max_datagram_size: u32,
    fragment_payload_bytes: u32,
}

impl Stats {
    fn new() -> Self {
        Self {
            total_capture_wait_us: 0,
            total_capture_convert_us: 0,
            total_encode_us: 0,
            total_send_us: 0,
            total_send_wait_us: 0,
            total_pipeline_us: 0,
            max_send_wait_us: 0,
            frame_count: 0,
            idr_count: 0,
            video_datagrams_sent: 0,
            video_datagrams_dropped: 0,
            max_datagram_size: 0,
            fragment_payload_bytes: 0,
        }
    }

    fn record(
        &mut self,
        capture_stats: CaptureStats,
        encode_us: u32,
        send_us: u32,
        pipeline_us: u32,
        is_keyframe: bool,
        frame_send_stats: FrameSendStats,
    ) {
        self.total_capture_wait_us += capture_stats.acquire_wait_us as u64;
        self.total_capture_convert_us += capture_stats.convert_us as u64;
        self.total_encode_us += encode_us as u64;
        self.total_send_us += send_us as u64;
        self.total_send_wait_us += frame_send_stats.send_wait_us;
        self.total_pipeline_us += pipeline_us as u64;
        self.max_send_wait_us = self.max_send_wait_us.max(frame_send_stats.max_send_wait_us);
        self.frame_count += 1;
        self.video_datagrams_sent = self
            .video_datagrams_sent
            .saturating_add(frame_send_stats.datagrams_sent);
        self.video_datagrams_dropped = self
            .video_datagrams_dropped
            .saturating_add(frame_send_stats.datagrams_dropped);
        self.max_datagram_size = frame_send_stats.max_datagram_size;
        self.fragment_payload_bytes = frame_send_stats.fragment_payload_bytes;
        if is_keyframe {
            self.idr_count = self.idr_count.saturating_add(1);
        }
    }

    fn drain(&mut self, encoder_bitrate_bps: u32, target_fps: u8) -> ServerTelemetry {
        let avg = |total: u64, sample_count: u32| {
            if sample_count > 0 {
                (total / sample_count as u64) as u32
            } else {
                0
            }
        };
        let telemetry = ServerTelemetry {
            avg_capture_wait_us: avg(self.total_capture_wait_us, self.frame_count),
            avg_capture_convert_us: avg(self.total_capture_convert_us, self.frame_count),
            avg_encode_us: avg(self.total_encode_us, self.frame_count),
            avg_send_us: avg(self.total_send_us, self.frame_count),
            avg_send_wait_us: avg(
                self.total_send_wait_us,
                (self.video_datagrams_sent + self.video_datagrams_dropped).max(1),
            ),
            max_send_wait_us: self.max_send_wait_us,
            avg_pipeline_us: avg(self.total_pipeline_us, self.frame_count),
            send_queue_frames: 0,
            idr_count: self.idr_count,
            frame_count: self.frame_count,
            encoder_bitrate_bps,
            target_fps,
            video_datagrams_sent: self.video_datagrams_sent,
            video_datagrams_dropped: self.video_datagrams_dropped,
            max_datagram_size: self.max_datagram_size,
            fragment_payload_bytes: self.fragment_payload_bytes,
        };
        self.total_capture_wait_us = 0;
        self.total_capture_convert_us = 0;
        self.total_encode_us = 0;
        self.total_send_us = 0;
        self.total_send_wait_us = 0;
        self.total_pipeline_us = 0;
        self.max_send_wait_us = 0;
        self.frame_count = 0;
        self.idr_count = 0;
        self.video_datagrams_sent = 0;
        self.video_datagrams_dropped = 0;
        telemetry
    }
}

/// A running pipeline instance.
pub struct PipelineHandle {
    idr_requested: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    client_telemetry_tx: Sender<ClientTelemetry>,
    telemetry_rx: Option<UnboundedReceiver<ServerTelemetry>>,
    cursor_rx: Option<UnboundedReceiver<CursorEvent>>,
    frame_lost_tx: Sender<u32>,
}

impl PipelineHandle {
    /// Spawn the capture → encode → send pipeline on a dedicated OS thread.
    ///
    /// `conn` is the QUIC connection to the client. The pipeline thread calls
    /// `conn.send_datagram()` directly; `quinn::Connection::send_datagram` is
    /// a synchronous, thread-safe method that enqueues the datagram and wakes
    /// the quinn driver task running on the Tokio runtime.
    pub fn start(
        codec: Codec,
        fps: u8,
        width: u32,
        height: u32,
        display_id: Option<String>,
        adaptive_config: AdaptiveStreamConfig,
        conn: quinn::Connection,
    ) -> Result<Self> {
        let idr_requested = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (client_telemetry_tx, client_telemetry_rx) = unbounded::<ClientTelemetry>();
        let (telemetry_tx, telemetry_rx) = unbounded_channel::<ServerTelemetry>();
        let (cursor_tx, cursor_rx) = unbounded_channel::<CursorEvent>();
        let (frame_lost_tx, frame_lost_rx) = unbounded::<u32>();
        let runtime = tokio::runtime::Handle::current();

        let idr_flag = idr_requested.clone();
        let stop = stop_flag.clone();

        std::thread::Builder::new()
            .name("yin-pipeline".into())
            .spawn(move || {
                // Apply SCHED_FIFO + core affinity on Linux
                #[cfg(target_os = "linux")]
                apply_realtime_scheduling();

                pipeline_thread(
                    codec,
                    fps,
                    width,
                    height,
                    display_id,
                    adaptive_config,
                    conn,
                    runtime,
                    client_telemetry_rx,
                    frame_lost_rx,
                    idr_flag,
                    stop,
                    telemetry_tx,
                    cursor_tx,
                );
            })?;

        Ok(Self {
            idr_requested,
            stop_flag,
            client_telemetry_tx,
            telemetry_rx: Some(telemetry_rx),
            cursor_rx: Some(cursor_rx),
            frame_lost_tx,
        })
    }

    pub fn request_idr(&self) {
        self.idr_requested.store(true, Ordering::Relaxed);
    }

    /// Notify the pipeline that `frame_seq` was lost in transit.
    ///
    /// The pipeline thread will call `NvEncInvalidateRefFrames` for the
    /// matching frame so subsequent P-frames skip the corrupt reference,
    /// allowing the stream to recover without a full IDR cycle.
    pub fn notify_frame_lost(&self, frame_seq: u32) {
        let _ = self.frame_lost_tx.send(frame_seq);
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    pub fn update_client_telemetry(&self, telemetry: ClientTelemetry) {
        let _ = self.client_telemetry_tx.send(telemetry);
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop_flag.clone()
    }

    pub fn take_telemetry_rx(&mut self) -> UnboundedReceiver<ServerTelemetry> {
        self.telemetry_rx
            .take()
            .expect("telemetry receiver already taken")
    }

    pub fn take_cursor_rx(&mut self) -> UnboundedReceiver<CursorEvent> {
        self.cursor_rx
            .take()
            .expect("cursor receiver already taken")
    }
}

// ---------------------------------------------------------------------------
// Real-time scheduling (Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn apply_realtime_scheduling() {
    use nix::sched::{sched_setaffinity, CpuSet};
    use nix::unistd::Pid;

    let mut cpuset = CpuSet::new();
    for i in 0..4 {
        let _ = cpuset.set(i);
    }
    if let Err(e) = sched_setaffinity(Pid::from_raw(0), &cpuset) {
        warn!("sched_setaffinity failed: {e} — continuing without core pinning");
    }

    #[cfg(target_os = "linux")]
    unsafe {
        let param = nix::libc::sched_param { sched_priority: 50 };
        let ret = nix::libc::sched_setscheduler(
            0,
            nix::libc::SCHED_FIFO,
            &param as *const nix::libc::sched_param,
        );
        if ret != 0 {
            warn!("sched_setscheduler(SCHED_FIFO, 50) failed — run with CAP_SYS_NICE or as root");
        } else {
            info!("pipeline thread: SCHED_FIFO priority 50, cores 0-3");
        }
    }
}

fn initialise_frame_deadline(frame_interval: Duration) -> Instant {
    Instant::now()
        .checked_add(frame_interval)
        .unwrap_or_else(Instant::now)
}

fn pace_until_next_frame(next_frame_deadline: &mut Instant, frame_interval: Duration) {
    wait_until(*next_frame_deadline);

    let now = Instant::now();
    while *next_frame_deadline <= now {
        *next_frame_deadline = next_frame_deadline
            .checked_add(frame_interval)
            .unwrap_or_else(|| now.checked_add(frame_interval).unwrap_or(now));
    }
}

fn wait_until(deadline: Instant) {
    const COARSE_SLEEP_SLACK: Duration = Duration::from_millis(2);
    const YIELD_SLACK: Duration = Duration::from_micros(200);

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            break;
        };

        if remaining > COARSE_SLEEP_SLACK {
            std::thread::sleep(remaining - COARSE_SLEEP_SLACK);
        } else if remaining > YIELD_SLACK {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

fn drain_latest_client_telemetry(
    client_telemetry_rx: &Receiver<ClientTelemetry>,
    latest_client_telemetry: &mut Option<LatestClientTelemetry>,
) {
    while let Ok(sample) = client_telemetry_rx.try_recv() {
        *latest_client_telemetry = Some(LatestClientTelemetry {
            sample,
            received_at: Instant::now(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_emit_telemetry_window(
    stats: &mut Stats,
    last_window: &mut Instant,
    latest_client_telemetry: Option<&LatestClientTelemetry>,
    telemetry_tx: &UnboundedSender<ServerTelemetry>,
    adaptive_enabled: bool,
    controller: &mut AdaptiveStreamController,
    encoder: &mut NvencEncoder,
    base_config: &NvencConfig,
) -> Result<Option<Duration>> {
    let now = Instant::now();
    if now.duration_since(*last_window) < ADAPTIVE_WINDOW_INTERVAL {
        return Ok(None);
    }

    let telemetry = stats.drain(controller.current_bitrate_bps(), controller.current_fps());
    let _ = telemetry_tx.send(telemetry.clone());
    *last_window = now;

    if !adaptive_enabled || telemetry.frame_count == 0 {
        return Ok(None);
    }

    if let Some((fps, bitrate_bps)) =
        controller.observe_window(&telemetry, latest_client_telemetry, now)
    {
        encoder
            .reconfigure(controller.current_config(base_config.clone()))
            .context("reconfigure NVENC encoder")?;
        info!(
            "adaptive stream control: target={}fps bitrate={:.1}Mbps",
            fps,
            bitrate_bps as f64 / 1e6,
        );
        return Ok(Some(controller.frame_interval()));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Pipeline thread body
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn pipeline_thread(
    codec: Codec,
    fps: u8,
    width: u32,
    height: u32,
    display_id: Option<String>,
    adaptive_config: AdaptiveStreamConfig,
    conn: quinn::Connection,
    runtime: tokio::runtime::Handle,
    client_telemetry_rx: Receiver<ClientTelemetry>,
    frame_lost_rx: Receiver<u32>,
    idr_requested: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    telemetry_tx: UnboundedSender<ServerTelemetry>,
    cursor_tx: UnboundedSender<CursorEvent>,
) {
    let stop_flag_for_log = stop_flag.clone();
    if let Err(err) = run_pipeline_thread(
        codec,
        fps,
        width,
        height,
        display_id,
        adaptive_config,
        conn,
        runtime,
        client_telemetry_rx,
        frame_lost_rx,
        idr_requested,
        stop_flag,
        telemetry_tx,
        cursor_tx,
    ) {
        if stop_flag_for_log.load(Ordering::Relaxed) {
            info!("pipeline thread stopped");
        } else {
            warn!("pipeline thread stopped with error: {err:#}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_thread(
    codec: Codec,
    fps: u8,
    width: u32,
    height: u32,
    display_id: Option<String>,
    adaptive_config: AdaptiveStreamConfig,
    conn: quinn::Connection,
    runtime: tokio::runtime::Handle,
    client_telemetry_rx: Receiver<ClientTelemetry>,
    frame_lost_rx: Receiver<u32>,
    idr_requested: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    telemetry_tx: UnboundedSender<ServerTelemetry>,
    cursor_tx: UnboundedSender<CursorEvent>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    let _ = &cursor_tx;

    info!(
        "pipeline thread: {codec:?} {width}x{height}@{fps}fps display={} → QUIC datagrams",
        display_id.as_deref().unwrap_or("default")
    );

    // QuicVideoSender queries max_datagram_size() from the connection to size
    // fragments appropriately for the negotiated path MTU, and uses the Tokio
    // runtime handle to apply backpressure instead of silently dropping older
    // unsent datagrams when bursty frames momentarily fill Quinn's buffer.
    let mut sender = QuicVideoSender::new(conn, runtime);

    let mut stats = Stats::new();
    let mut frame_seq: u32 = 0;
    let mut last_window = Instant::now();
    let mut latest_client_telemetry: Option<LatestClientTelemetry> = None;

    #[cfg(target_os = "linux")]
    {
        let (frame_tx, frame_rx) = bounded::<CaptureFrame>(2);
        let (mut capture_mode, mut capture, first_frame) =
            initialise_wayland_capture(display_id.as_deref(), &frame_tx, &frame_rx, &stop_flag)
                .context("initialise Wayland capture")?;
        let (mut capture_width, mut capture_height) = first_frame_dimensions(&first_frame)?;
        if capture_width != width || capture_height != height {
            warn!(
                "capture dimensions are {capture_width}x{capture_height}, requested session was {width}x{height}"
            );
        }

        let mut base_config = encoder_config(codec, capture_width, capture_height, fps);
        let mut controller = AdaptiveStreamController::new(&base_config, adaptive_config);
        let mut frame_interval = controller.frame_interval();
        let mut next_frame_deadline = initialise_frame_deadline(frame_interval);
        let mut encoder = NvencEncoder::new(controller.current_config(base_config.clone()))
            .context("initialise NVENC encoder")?;
        let mut registered_dmabufs = HashSet::new();
        let mut pending_frame = Some(first_frame);
        let mut force_idr_after_capture_reset = false;
        // Ring of (frame_seq, timestamp_us) for the most recent frames.
        // Used to look up the timestamp when the client reports a FrameLost.
        let mut ref_frame_timestamps: VecDeque<(u32, u64)> = VecDeque::new();

        while !stop_flag.load(Ordering::Relaxed) {
            drain_latest_client_telemetry(&client_telemetry_rx, &mut latest_client_telemetry);
            invalidate_lost_ref_frames(&frame_lost_rx, &mut ref_frame_timestamps, &mut encoder);

            let frame_started_at = Instant::now();
            let frame = match pending_frame.take() {
                Some(frame) => frame,
                None => match receive_wayland_frame(&mut capture, &frame_rx, &stop_flag) {
                    Ok(frame) => frame,
                    Err(err)
                        if capture_mode == CaptureMode::DmaBuf
                            && !stop_flag.load(Ordering::Relaxed) =>
                    {
                        warn!("Wayland DMA-BUF capture failed at runtime, falling back to SHM: {err:#}");
                        let mut shm_capture = WaylandCapture::new(
                            CaptureMode::Shm,
                            display_id.as_deref(),
                            frame_tx.clone(),
                        )
                        .context("reinitialise Wayland SHM capture fallback")?;
                        let frame = receive_wayland_frame(&mut shm_capture, &frame_rx, &stop_flag)
                            .context("capture first SHM frame after DMA-BUF fallback")?;
                        capture = shm_capture;
                        capture_mode = CaptureMode::Shm;
                        registered_dmabufs.clear();
                        force_idr_after_capture_reset = true;
                        frame
                    }
                    Err(err) => return Err(err).context("receive Wayland frame"),
                },
            };
            let mut force_idr = idr_requested.swap(false, Ordering::Relaxed)
                || frame_seq == 0
                || std::mem::take(&mut force_idr_after_capture_reset);

            let (encoded, capture_stats) = match frame {
                CaptureFrame::Shm {
                    data,
                    width,
                    height,
                    stride,
                    format: _format,
                    timestamp_us,
                    stats,
                } => {
                    if width != capture_width || height != capture_height {
                        warn!(
                            "capture size changed: {capture_width}x{capture_height} -> {width}x{height}; reinitialising NVENC"
                        );
                        capture_width = width;
                        capture_height = height;
                        base_config = encoder_config(codec, capture_width, capture_height, fps);
                        controller = AdaptiveStreamController::new(&base_config, adaptive_config);
                        frame_interval = controller.frame_interval();
                        next_frame_deadline = initialise_frame_deadline(frame_interval);
                        encoder = NvencEncoder::new(controller.current_config(base_config.clone()))
                            .context("reinitialise NVENC encoder after resize")?;
                        registered_dmabufs.clear();
                        force_idr = true;
                    }

                    (
                        encoder
                            .encode_argb_frame(&data, stride, timestamp_us, force_idr)
                            .context("encode Wayland SHM frame")?,
                        stats,
                    )
                }
                CaptureFrame::DmaBuf {
                    fd,
                    buffer_id,
                    width,
                    height,
                    pitch,
                    offset,
                    allocation_size,
                    format: _format,
                    modifier: _modifier,
                    timestamp_us,
                    stats,
                } => {
                    if width != capture_width || height != capture_height {
                        warn!(
                            "capture size changed: {capture_width}x{capture_height} -> {width}x{height}; reinitialising NVENC"
                        );
                        capture_width = width;
                        capture_height = height;
                        base_config = encoder_config(codec, capture_width, capture_height, fps);
                        controller = AdaptiveStreamController::new(&base_config, adaptive_config);
                        frame_interval = controller.frame_interval();
                        next_frame_deadline = initialise_frame_deadline(frame_interval);
                        encoder = NvencEncoder::new(controller.current_config(base_config.clone()))
                            .context("reinitialise NVENC encoder after resize")?;
                        registered_dmabufs.clear();
                        force_idr = true;
                    }

                    if registered_dmabufs.insert(buffer_id) {
                        let mapping_size = u64::from(pitch)
                            .checked_mul(u64::from(height))
                            .context("DMA-BUF mapping size overflow")?;
                        encoder
                            .register_dmabuf_argb_resource(
                                buffer_id,
                                fd,
                                allocation_size,
                                u64::from(offset),
                                mapping_size,
                                pitch,
                            )
                            .with_context(|| {
                                format!("register Wayland DMA-BUF buffer {buffer_id} with NVENC")
                            })?;
                    }

                    (
                        encoder
                            .encode_registered_dmabuf(buffer_id, timestamp_us, force_idr)
                            .context("encode Wayland DMA-BUF frame")?,
                        stats,
                    )
                }
            };

            let send_started_at = Instant::now();
            let frame_send_stats =
                sender.send_frame(&encoded.slices, encoded.is_keyframe, encoded.timestamp_us);
            let send_us = duration_to_us(send_started_at.elapsed());

            stats.record(
                capture_stats,
                encoded.encode_us,
                send_us,
                duration_to_us(frame_started_at.elapsed()),
                encoded.is_keyframe,
                frame_send_stats,
            );
            record_ref_frame_timestamp(&mut ref_frame_timestamps, frame_seq, encoded.timestamp_us);
            frame_seq = frame_seq.wrapping_add(1);

            drain_latest_client_telemetry(&client_telemetry_rx, &mut latest_client_telemetry);
            if let Some(new_interval) = maybe_emit_telemetry_window(
                &mut stats,
                &mut last_window,
                latest_client_telemetry.as_ref(),
                &telemetry_tx,
                adaptive_config.enabled,
                &mut controller,
                &mut encoder,
                &base_config,
            )? {
                frame_interval = new_interval;
                next_frame_deadline = initialise_frame_deadline(frame_interval);
            }

            pace_until_next_frame(&mut next_frame_deadline, frame_interval);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let (frame_tx, frame_rx) = bounded::<CaptureFrame>(2);
        let mut capture = WindowsCapture::new(display_id.as_deref(), frame_tx, cursor_tx.clone())
            .context("initialise Windows capture")?;
        let first_frame = receive_windows_frame(&mut capture, &frame_rx, &stop_flag)
            .context("capture first frame")?;
        let (mut capture_width, mut capture_height) = first_frame_dimensions(&first_frame)?;
        if capture_width != width || capture_height != height {
            warn!(
                "capture dimensions are {capture_width}x{capture_height}, requested session was {width}x{height}"
            );
        }

        let mut pending_frame = Some(first_frame);
        let mut base_config = encoder_config(codec, capture_width, capture_height, fps);
        let mut controller = AdaptiveStreamController::new(&base_config, adaptive_config);
        let mut frame_interval = controller.frame_interval();
        let mut next_frame_deadline = initialise_frame_deadline(frame_interval);
        // Ring of (frame_seq, timestamp_us) for the most recent frames.
        // Used to look up the timestamp when the client reports a FrameLost.
        let mut ref_frame_timestamps: VecDeque<(u32, u64)> = VecDeque::new();
        let mut encoder = match build_windows_encoder(
            &capture,
            matches!(
                pending_frame.as_ref(),
                Some(CaptureFrame::D3d11Texture { .. })
            ),
            controller.current_config(base_config.clone()),
        ) {
            Ok(encoder) => encoder,
            Err(err)
                if matches!(
                    pending_frame.as_ref(),
                    Some(CaptureFrame::D3d11Texture { .. })
                ) =>
            {
                warn!(
                    "failed to initialise D3D11 NVENC path for first HDR frame: {err:#}; retrying with CPU conversion"
                );
                capture.disable_gpu_fp16();
                let fallback_frame = receive_windows_frame(&mut capture, &frame_rx, &stop_flag)
                    .context("capture fallback Windows frame after D3D11 init failure")?;
                let (width, height) = first_frame_dimensions(&fallback_frame)?;
                capture_width = width;
                capture_height = height;
                pending_frame = Some(fallback_frame);
                build_windows_encoder(
                    &capture,
                    false,
                    controller.current_config(base_config.clone()),
                )
                .context("initialise fallback CUDA-backed NVENC encoder")?
            }
            Err(err) => return Err(err).context("initialise NVENC encoder"),
        };

        while !stop_flag.load(Ordering::Relaxed) {
            drain_latest_client_telemetry(&client_telemetry_rx, &mut latest_client_telemetry);
            invalidate_lost_ref_frames(&frame_lost_rx, &mut ref_frame_timestamps, &mut encoder);

            let frame_started_at = Instant::now();
            let frame = match pending_frame.take() {
                Some(frame) => frame,
                None => receive_windows_frame(&mut capture, &frame_rx, &stop_flag)?,
            };
            let mut force_idr = idr_requested.swap(false, Ordering::Relaxed) || frame_seq == 0;
            let frame_uses_d3d11 = matches!(frame, CaptureFrame::D3d11Texture { .. });

            let (encoded, capture_stats) = match frame {
                CaptureFrame::Shm {
                    data,
                    width,
                    height,
                    stride,
                    format: _format,
                    timestamp_us,
                    stats,
                } => {
                    if width != capture_width
                        || height != capture_height
                        || frame_uses_d3d11 != encoder.uses_d3d11_input()
                    {
                        warn!(
                            "capture size changed: {capture_width}x{capture_height} -> {width}x{height}; reinitialising NVENC"
                        );
                        capture_width = width;
                        capture_height = height;
                        base_config = encoder_config(codec, capture_width, capture_height, fps);
                        controller = AdaptiveStreamController::new(&base_config, adaptive_config);
                        frame_interval = controller.frame_interval();
                        next_frame_deadline = initialise_frame_deadline(frame_interval);
                        encoder = build_windows_encoder(
                            &capture,
                            false,
                            controller.current_config(base_config.clone()),
                        )
                        .context("reinitialise NVENC encoder after Windows frame change")?;
                        force_idr = true;
                    }

                    (
                        encoder
                            .encode_argb_frame(&data, stride, timestamp_us, force_idr)
                            .context("encode Windows desktop frame")?,
                        stats,
                    )
                }
                CaptureFrame::D3d11Texture {
                    texture,
                    resource_id,
                    width,
                    height,
                    timestamp_us,
                    stats,
                } => {
                    if width != capture_width
                        || height != capture_height
                        || frame_uses_d3d11 != encoder.uses_d3d11_input()
                    {
                        warn!(
                            "capture size or input mode changed: {capture_width}x{capture_height} -> {width}x{height}; reinitialising NVENC"
                        );
                        capture_width = width;
                        capture_height = height;
                        base_config = encoder_config(codec, capture_width, capture_height, fps);
                        controller = AdaptiveStreamController::new(&base_config, adaptive_config);
                        frame_interval = controller.frame_interval();
                        next_frame_deadline = initialise_frame_deadline(frame_interval);
                        match build_windows_encoder(
                            &capture,
                            true,
                            controller.current_config(base_config.clone()),
                        ) {
                            Ok(new_encoder) => {
                                encoder = new_encoder;
                                force_idr = true;
                            }
                            Err(err) => {
                                warn!(
                                    "failed to reinitialise D3D11 NVENC path: {err:#}; retrying with CPU conversion"
                                );
                                capture.disable_gpu_fp16();
                                pending_frame = Some(
                                    receive_windows_frame(&mut capture, &frame_rx, &stop_flag)
                                        .context(
                                        "capture fallback Windows frame after D3D11 reinit failure",
                                    )?,
                                );
                                continue;
                            }
                        }
                    }

                    (
                        encoder
                            .encode_d3d11_texture(&texture, resource_id, timestamp_us, force_idr)
                            .context("encode Windows HDR D3D11 frame")?,
                        stats,
                    )
                }
                #[cfg(target_os = "linux")]
                CaptureFrame::DmaBuf { .. } => {
                    unreachable!("Windows capture does not emit DMA-BUF frames")
                }
            };

            let send_started_at = Instant::now();
            let frame_send_stats =
                sender.send_frame(&encoded.slices, encoded.is_keyframe, encoded.timestamp_us);
            let send_us = duration_to_us(send_started_at.elapsed());

            stats.record(
                capture_stats,
                encoded.encode_us,
                send_us,
                duration_to_us(frame_started_at.elapsed()),
                encoded.is_keyframe,
                frame_send_stats,
            );
            record_ref_frame_timestamp(&mut ref_frame_timestamps, frame_seq, encoded.timestamp_us);
            frame_seq = frame_seq.wrapping_add(1);

            drain_latest_client_telemetry(&client_telemetry_rx, &mut latest_client_telemetry);
            if let Some(new_interval) = maybe_emit_telemetry_window(
                &mut stats,
                &mut last_window,
                latest_client_telemetry.as_ref(),
                &telemetry_tx,
                adaptive_config.enabled,
                &mut controller,
                &mut encoder,
                &base_config,
            )? {
                frame_interval = new_interval;
                next_frame_deadline = initialise_frame_deadline(frame_interval);
            }

            pace_until_next_frame(&mut next_frame_deadline, frame_interval);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (
            codec,
            fps,
            width,
            height,
            display_id,
            adaptive_config,
            conn,
            client_telemetry_rx,
            frame_lost_rx,
            idr_requested,
            stop_flag,
            telemetry_tx,
            cursor_tx,
        );
        bail!("the server pipeline is only implemented on Linux and Windows");
    }

    info!("pipeline thread stopped");
    Ok(())
}

/// Maximum number of frame timestamps retained for ref-frame invalidation
/// lookups.  At 120 fps and a ~500 ms worst-case RTT this covers ~60 frames;
/// 128 gives comfortable headroom without material memory cost.
const REF_FRAME_RING_SIZE: usize = 128;

/// Append a (frame_seq, timestamp_us) entry to the ring, evicting the oldest
/// entry when the ring is full.
fn record_ref_frame_timestamp(ring: &mut VecDeque<(u32, u64)>, frame_seq: u32, timestamp_us: u64) {
    if ring.len() >= REF_FRAME_RING_SIZE {
        ring.pop_front();
    }
    ring.push_back((frame_seq, timestamp_us));
}

/// Drain all pending `FrameLost` notifications and call
/// `NvEncInvalidateRefFrames` for each one that still has a known timestamp.
///
/// Errors from `invalidate_ref_frame` are logged as warnings rather than
/// bubbled up; a failed invalidation is non-fatal — the stream falls back to
/// the existing IDR-based recovery path if the decoder cannot cope.
fn invalidate_lost_ref_frames(
    frame_lost_rx: &Receiver<u32>,
    ring: &mut VecDeque<(u32, u64)>,
    encoder: &mut NvencEncoder,
) {
    while let Ok(frame_seq) = frame_lost_rx.try_recv() {
        if let Some(&(_, timestamp_us)) = ring.iter().find(|&&(seq, _)| seq == frame_seq) {
            if let Err(err) = encoder.invalidate_ref_frame(timestamp_us) {
                warn!(
                    "ref frame invalidation failed for frame_seq={frame_seq} ts={timestamp_us}: {err:#}"
                );
            } else {
                info!("invalidated ref frame seq={frame_seq} ts={timestamp_us}");
            }
        } else {
            // Timestamp no longer in the ring (very old loss report) — fall
            // back to IDR.  The client should have already sent RequestIdr
            // as a fallback when FrameLost doesn't produce a clean stream
            // quickly enough.
            warn!(
                "FrameLost for seq={frame_seq} has no timestamp in ring; cannot invalidate ref frame"
            );
        }
    }
}

fn encoder_config(codec: Codec, width: u32, height: u32, fps: u8) -> NvencConfig {
    match codec {
        Codec::H264 => NvencConfig::lan_h264(width, height, fps),
        Codec::Hevc => NvencConfig::wan_hevc(width, height, fps),
        Codec::Av1 => NvencConfig::lan_h264(width, height, fps),
    }
}

#[cfg(target_os = "linux")]
fn initialise_wayland_capture(
    display_id: Option<&str>,
    frame_tx: &crossbeam_channel::Sender<CaptureFrame>,
    frame_rx: &Receiver<CaptureFrame>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<(CaptureMode, WaylandCapture, CaptureFrame)> {
    match WaylandCapture::new(CaptureMode::DmaBuf, display_id, frame_tx.clone()) {
        Ok(mut capture) => {
            info!("pipeline thread: using Wayland DMA-BUF capture");
            match receive_wayland_frame(&mut capture, frame_rx, stop_flag) {
                Ok(frame) => Ok((CaptureMode::DmaBuf, capture, frame)),
                Err(err) => {
                    warn!(
                        "DMA-BUF capture failed during first frame, falling back to SHM: {err:#}"
                    );
                    drop(capture);
                    let mut shm_capture =
                        WaylandCapture::new(CaptureMode::Shm, display_id, frame_tx.clone())
                            .context("initialise Wayland SHM capture fallback")?;
                    let frame = receive_wayland_frame(&mut shm_capture, frame_rx, stop_flag)
                        .context("capture first SHM frame")?;
                    Ok((CaptureMode::Shm, shm_capture, frame))
                }
            }
        }
        Err(err) => {
            warn!("DMA-BUF capture unavailable, falling back to SHM: {err:#}");
            let mut shm_capture =
                WaylandCapture::new(CaptureMode::Shm, display_id, frame_tx.clone())
                    .context("initialise Wayland SHM capture fallback")?;
            let frame = receive_wayland_frame(&mut shm_capture, frame_rx, stop_flag)
                .context("capture first SHM frame")?;
            Ok((CaptureMode::Shm, shm_capture, frame))
        }
    }
}

#[cfg(target_os = "linux")]
fn receive_wayland_frame(
    capture: &mut WaylandCapture,
    frame_rx: &Receiver<CaptureFrame>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<CaptureFrame> {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            anyhow::bail!("Wayland capture stopped");
        }
        capture.pump()?;
        match frame_rx.recv_timeout(FRAME_WAIT_POLL_INTERVAL) {
            Ok(frame) => return Ok(frame),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("Wayland capture channel closed"))
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn receive_windows_frame(
    capture: &mut WindowsCapture,
    frame_rx: &Receiver<CaptureFrame>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<CaptureFrame> {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            anyhow::bail!("Windows capture stopped");
        }
        capture.pump()?;
        match frame_rx.recv_timeout(FRAME_WAIT_POLL_INTERVAL) {
            Ok(frame) => return Ok(frame),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("Windows capture channel closed"))
            }
        }
    }
}

fn first_frame_dimensions(frame: &CaptureFrame) -> Result<(u32, u32)> {
    match frame {
        CaptureFrame::Shm { width, height, .. } => Ok((*width, *height)),
        #[cfg(target_os = "linux")]
        CaptureFrame::DmaBuf { width, height, .. } => Ok((*width, *height)),
        #[cfg(target_os = "windows")]
        CaptureFrame::D3d11Texture { width, height, .. } => Ok((*width, *height)),
    }
}

#[cfg(target_os = "windows")]
fn build_windows_encoder(
    capture: &WindowsCapture,
    use_d3d11: bool,
    config: NvencConfig,
) -> Result<NvencEncoder> {
    if use_d3d11 {
        NvencEncoder::new_d3d11(config, &capture.d3d11_device())
            .context("initialise D3D11 NVENC encoder")
    } else {
        NvencEncoder::new(config).context("initialise CUDA-backed NVENC encoder")
    }
}

fn duration_to_us(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config(fps: u8, bitrate_bps: u32) -> NvencConfig {
        NvencConfig {
            width: 1920,
            height: 1080,
            fps,
            bitrate_bps,
            h264: true,
        }
    }

    fn server_telemetry(
        avg_send_wait_us: u32,
        max_send_wait_us: u32,
        video_datagrams_dropped: u32,
    ) -> ServerTelemetry {
        ServerTelemetry {
            avg_capture_wait_us: 0,
            avg_capture_convert_us: 0,
            avg_encode_us: 0,
            avg_send_us: 0,
            avg_send_wait_us,
            max_send_wait_us,
            avg_pipeline_us: 0,
            send_queue_frames: 0,
            idr_count: 0,
            frame_count: 30,
            encoder_bitrate_bps: 0,
            target_fps: 60,
            video_datagrams_sent: 100,
            video_datagrams_dropped,
            max_datagram_size: 1200,
            fragment_payload_bytes: 1175,
        }
    }

    fn client_telemetry(
        received_at: Instant,
        render_dropped_frames: u32,
        avg_render_queue_us: u32,
    ) -> LatestClientTelemetry {
        LatestClientTelemetry {
            sample: ClientTelemetry {
                unrecoverable_frames: 0,
                recovered_frames: 0,
                recovered_fragments: 0,
                presented_frames: 30,
                render_dropped_frames,
                avg_decode_queue_us: 0,
                avg_render_queue_us,
            },
            received_at,
        }
    }

    #[test]
    fn controller_reduces_bitrate_before_fps() {
        let base = base_config(60, 40_000_000);
        let mut controller = AdaptiveStreamController::new(
            &base,
            AdaptiveStreamConfig {
                enabled: true,
                min_fps: 30,
                min_bitrate_bps: 10_000_000,
                max_bitrate_bps: 40_000_000,
            },
        );

        let degraded = server_telemetry(2_000, 6_000, 1);
        let mut now = Instant::now();
        assert_eq!(
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now),
            None
        );
        now += ADAPTIVE_WINDOW_INTERVAL;
        assert_eq!(
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now),
            None
        );
        now += ADAPTIVE_WINDOW_INTERVAL;
        assert_eq!(
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now),
            Some((60, 34_000_000))
        );
        assert_eq!(controller.current_fps(), 60);

        while controller.current_bitrate_bps() > 10_000_000 {
            now += ADAPTIVE_WINDOW_INTERVAL;
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now);
        }
        assert_eq!(controller.current_bitrate_bps(), 10_000_000);
        assert_eq!(controller.current_fps(), 60);

        let original_fps = controller.current_fps();
        while controller.current_fps() == original_fps {
            now += ADAPTIVE_WINDOW_INTERVAL;
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now);
        }
        assert_eq!(controller.current_fps(), 48);
    }

    #[test]
    fn controller_requires_fresh_client_telemetry_for_upgrades() {
        let base = base_config(60, 20_000_000);
        let mut controller = AdaptiveStreamController::new(
            &base,
            AdaptiveStreamConfig {
                enabled: true,
                min_fps: 30,
                min_bitrate_bps: 5_000_000,
                max_bitrate_bps: 20_000_000,
            },
        );

        let degraded = server_telemetry(2_000, 6_000, 1);
        let clean = server_telemetry(100, 200, 0);
        let mut now = Instant::now();

        for _ in 0..2 {
            assert_eq!(
                controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now),
                None
            );
            now += ADAPTIVE_WINDOW_INTERVAL;
        }
        assert_eq!(
            controller.observe_window(&degraded, Some(&client_telemetry(now, 0, 0)), now),
            Some((60, 17_000_000))
        );
        assert_eq!(controller.current_bitrate_bps(), 17_000_000);

        let stale = LatestClientTelemetry {
            sample: ClientTelemetry::default(),
            received_at: now - CLIENT_TELEMETRY_MAX_AGE - Duration::from_millis(1),
        };
        for _ in 0..12 {
            now += ADAPTIVE_WINDOW_INTERVAL;
            assert_eq!(controller.observe_window(&clean, Some(&stale), now), None);
        }
        assert_eq!(controller.current_bitrate_bps(), 17_000_000);

        for _ in 0..(CLEAN_WINDOWS_FOR_BITRATE_STEP - 1) {
            now += ADAPTIVE_WINDOW_INTERVAL;
            assert_eq!(
                controller.observe_window(&clean, Some(&client_telemetry(now, 0, 0)), now),
                None
            );
        }
        now += ADAPTIVE_WINDOW_INTERVAL;
        assert_eq!(
            controller.observe_window(&clean, Some(&client_telemetry(now, 0, 0)), now),
            Some((60, 17_850_000))
        );
    }

    #[test]
    fn controller_treats_render_queue_pressure_as_degraded() {
        let base = base_config(60, 20_000_000);
        let mut controller = AdaptiveStreamController::new(
            &base,
            AdaptiveStreamConfig {
                enabled: true,
                min_fps: 30,
                min_bitrate_bps: 5_000_000,
                max_bitrate_bps: 20_000_000,
            },
        );

        let clean_send = server_telemetry(100, 200, 0);
        let mut now = Instant::now();
        assert_eq!(
            controller.observe_window(&clean_send, Some(&client_telemetry(now, 0, 40_000)), now),
            None
        );
        now += ADAPTIVE_WINDOW_INTERVAL;
        assert_eq!(
            controller.observe_window(&clean_send, Some(&client_telemetry(now, 0, 40_000)), now),
            None
        );
        now += ADAPTIVE_WINDOW_INTERVAL;
        assert_eq!(
            controller.observe_window(&clean_send, Some(&client_telemetry(now, 0, 40_000)), now),
            Some((60, 17_000_000))
        );
    }
}
