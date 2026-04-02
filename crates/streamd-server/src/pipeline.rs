//! Pipeline orchestration: capture → encode → QUIC datagram send.
//!
//! The pipeline runs on a dedicated OS thread pinned to physical cores 0-3
//! with SCHED_FIFO priority 50 to prevent kernel preemption mid-frame.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver};
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use streamd_proto::packets::{Codec, ServerTelemetry};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

#[cfg(target_os = "linux")]
use crate::capture::wayland::{CaptureMode, WaylandCapture};
#[cfg(target_os = "windows")]
use crate::capture::windows::WindowsCapture;
use crate::capture::{CaptureFrame, CaptureStats, CursorEvent};
use crate::encode::nvenc::{NvencConfig, NvencEncoder};
use crate::transport::video_tx::QuicVideoSender;

/// Statistics tracked by the pipeline thread for telemetry.
struct Stats {
    total_capture_wait_us: u64,
    total_capture_convert_us: u64,
    total_encode_us: u64,
    total_send_us: u64,
    total_pipeline_us: u64,
    frame_count: u32,
    idr_count: u8,
}

impl Stats {
    fn new() -> Self {
        Self {
            total_capture_wait_us: 0,
            total_capture_convert_us: 0,
            total_encode_us: 0,
            total_send_us: 0,
            total_pipeline_us: 0,
            frame_count: 0,
            idr_count: 0,
        }
    }

    fn record(
        &mut self,
        capture_stats: CaptureStats,
        encode_us: u32,
        send_us: u32,
        pipeline_us: u32,
        is_keyframe: bool,
    ) {
        self.total_capture_wait_us += capture_stats.acquire_wait_us as u64;
        self.total_capture_convert_us += capture_stats.convert_us as u64;
        self.total_encode_us += encode_us as u64;
        self.total_send_us += send_us as u64;
        self.total_pipeline_us += pipeline_us as u64;
        self.frame_count += 1;
        if is_keyframe {
            self.idr_count = self.idr_count.saturating_add(1);
        }
    }

    fn drain(&mut self) -> ServerTelemetry {
        let avg = |total: u64, frame_count: u32| {
            if frame_count > 0 {
                (total / frame_count as u64) as u32
            } else {
                0
            }
        };
        let telemetry = ServerTelemetry {
            avg_capture_wait_us: avg(self.total_capture_wait_us, self.frame_count),
            avg_capture_convert_us: avg(self.total_capture_convert_us, self.frame_count),
            avg_encode_us: avg(self.total_encode_us, self.frame_count),
            avg_send_us: avg(self.total_send_us, self.frame_count),
            avg_pipeline_us: avg(self.total_pipeline_us, self.frame_count),
            send_queue_frames: 0,
            idr_count: self.idr_count,
            frame_count: self.frame_count,
        };
        self.total_capture_wait_us = 0;
        self.total_capture_convert_us = 0;
        self.total_encode_us = 0;
        self.total_send_us = 0;
        self.total_pipeline_us = 0;
        self.frame_count = 0;
        self.idr_count = 0;
        telemetry
    }
}

/// A running pipeline instance.
pub struct PipelineHandle {
    idr_requested: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    telemetry_rx: Option<UnboundedReceiver<ServerTelemetry>>,
    cursor_rx: Option<UnboundedReceiver<CursorEvent>>,
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
        conn: quinn::Connection,
    ) -> Result<Self> {
        let idr_requested = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (telemetry_tx, telemetry_rx) = unbounded_channel::<ServerTelemetry>();
        let (cursor_tx, cursor_rx) = unbounded_channel::<CursorEvent>();

        let idr_flag = idr_requested.clone();
        let stop = stop_flag.clone();

        std::thread::Builder::new()
            .name("streamd-pipeline".into())
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
                    conn,
                    idr_flag,
                    stop,
                    telemetry_tx,
                    cursor_tx,
                );
            })?;

        Ok(Self {
            idr_requested,
            stop_flag,
            telemetry_rx: Some(telemetry_rx),
            cursor_rx: Some(cursor_rx),
        })
    }

    pub fn request_idr(&self) {
        self.idr_requested.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
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
    conn: quinn::Connection,
    idr_requested: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    telemetry_tx: UnboundedSender<ServerTelemetry>,
    cursor_tx: UnboundedSender<CursorEvent>,
) {
    if let Err(err) = run_pipeline_thread(
        codec,
        fps,
        width,
        height,
        display_id,
        conn,
        idr_requested,
        stop_flag,
        telemetry_tx,
        cursor_tx,
    ) {
        warn!("pipeline thread stopped with error: {err:#}");
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_thread(
    codec: Codec,
    fps: u8,
    width: u32,
    height: u32,
    display_id: Option<String>,
    conn: quinn::Connection,
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

    let frame_interval = Duration::from_nanos(1_000_000_000 / fps as u64);

    // QuicVideoSender queries max_datagram_size() from the connection to size
    // fragments appropriately for the negotiated path MTU.
    let mut sender = QuicVideoSender::new(conn);

    let mut stats = Stats::new();
    let mut frame_seq: u32 = 0;
    let mut last_telemetry = Instant::now();

    #[cfg(target_os = "linux")]
    {
        let (frame_tx, frame_rx) = bounded::<CaptureFrame>(2);
        let (mut capture_mode, mut capture, first_frame) =
            initialise_wayland_capture(display_id.as_deref(), &frame_tx, &frame_rx)
                .context("initialise Wayland capture")?;
        let (mut capture_width, mut capture_height) = first_frame_dimensions(&first_frame)?;
        if capture_width != width || capture_height != height {
            warn!(
                "capture dimensions are {capture_width}x{capture_height}, requested session was {width}x{height}"
            );
        }

        let mut encoder =
            NvencEncoder::new(encoder_config(codec, capture_width, capture_height, fps))
                .context("initialise NVENC encoder")?;
        let mut registered_dmabufs = HashSet::new();
        let mut pending_frame = Some(first_frame);
        let mut force_idr_after_capture_reset = false;

        while !stop_flag.load(Ordering::Relaxed) {
            let frame_started_at = Instant::now();
            let frame = match pending_frame.take() {
                Some(frame) => frame,
                None => match receive_wayland_frame(&mut capture, &frame_rx) {
                    Ok(frame) => frame,
                    Err(err) if capture_mode == CaptureMode::DmaBuf => {
                        warn!("Wayland DMA-BUF capture failed at runtime, falling back to SHM: {err:#}");
                        let mut shm_capture = WaylandCapture::new(
                            CaptureMode::Shm,
                            display_id.as_deref(),
                            frame_tx.clone(),
                        )
                        .context("reinitialise Wayland SHM capture fallback")?;
                        let frame = receive_wayland_frame(&mut shm_capture, &frame_rx)
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
                        encoder = NvencEncoder::new(encoder_config(
                            codec,
                            capture_width,
                            capture_height,
                            fps,
                        ))
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
                        encoder = NvencEncoder::new(encoder_config(
                            codec,
                            capture_width,
                            capture_height,
                            fps,
                        ))
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
            sender.send_frame(&encoded.slices, encoded.is_keyframe, encoded.timestamp_us);
            let send_us = duration_to_us(send_started_at.elapsed());

            stats.record(
                capture_stats,
                encoded.encode_us,
                send_us,
                duration_to_us(frame_started_at.elapsed()),
                encoded.is_keyframe,
            );
            frame_seq = frame_seq.wrapping_add(1);

            if last_telemetry.elapsed() >= Duration::from_secs(1) {
                let t = stats.drain();
                let _ = telemetry_tx.send(t);
                last_telemetry = Instant::now();
            }

            let frame_elapsed = frame_started_at.elapsed();
            if frame_elapsed < frame_interval {
                std::thread::sleep(frame_interval - frame_elapsed);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let (frame_tx, frame_rx) = bounded::<CaptureFrame>(2);
        let mut capture = WindowsCapture::new(display_id.as_deref(), frame_tx, cursor_tx.clone())
            .context("initialise Windows capture")?;
        let first_frame =
            receive_windows_frame(&mut capture, &frame_rx).context("capture first frame")?;
        let (mut capture_width, mut capture_height) = first_frame_dimensions(&first_frame)?;
        if capture_width != width || capture_height != height {
            warn!(
                "capture dimensions are {capture_width}x{capture_height}, requested session was {width}x{height}"
            );
        }

        let mut pending_frame = Some(first_frame);
        let mut encoder = match build_windows_encoder(
            &capture,
            matches!(
                pending_frame.as_ref(),
                Some(CaptureFrame::D3d11Texture { .. })
            ),
            encoder_config(codec, capture_width, capture_height, fps),
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
                let fallback_frame = receive_windows_frame(&mut capture, &frame_rx)
                    .context("capture fallback Windows frame after D3D11 init failure")?;
                let (width, height) = first_frame_dimensions(&fallback_frame)?;
                capture_width = width;
                capture_height = height;
                pending_frame = Some(fallback_frame);
                build_windows_encoder(
                    &capture,
                    false,
                    encoder_config(codec, capture_width, capture_height, fps),
                )
                .context("initialise fallback CUDA-backed NVENC encoder")?
            }
            Err(err) => return Err(err).context("initialise NVENC encoder"),
        };

        while !stop_flag.load(Ordering::Relaxed) {
            let frame_started_at = Instant::now();
            let frame = match pending_frame.take() {
                Some(frame) => frame,
                None => receive_windows_frame(&mut capture, &frame_rx)?,
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
                        encoder = build_windows_encoder(
                            &capture,
                            false,
                            encoder_config(codec, capture_width, capture_height, fps),
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
                        match build_windows_encoder(
                            &capture,
                            true,
                            encoder_config(codec, capture_width, capture_height, fps),
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
                                pending_frame =
                                    Some(receive_windows_frame(&mut capture, &frame_rx).context(
                                        "capture fallback Windows frame after D3D11 reinit failure",
                                    )?);
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
            sender.send_frame(&encoded.slices, encoded.is_keyframe, encoded.timestamp_us);
            let send_us = duration_to_us(send_started_at.elapsed());

            stats.record(
                capture_stats,
                encoded.encode_us,
                send_us,
                duration_to_us(frame_started_at.elapsed()),
                encoded.is_keyframe,
            );
            frame_seq = frame_seq.wrapping_add(1);

            if last_telemetry.elapsed() >= Duration::from_secs(1) {
                let t = stats.drain();
                let _ = telemetry_tx.send(t);
                last_telemetry = Instant::now();
            }

            let frame_elapsed = frame_started_at.elapsed();
            if frame_elapsed < frame_interval {
                std::thread::sleep(frame_interval - frame_elapsed);
            }
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
            conn,
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
) -> Result<(CaptureMode, WaylandCapture, CaptureFrame)> {
    match WaylandCapture::new(CaptureMode::DmaBuf, display_id, frame_tx.clone()) {
        Ok(mut capture) => {
            info!("pipeline thread: using Wayland DMA-BUF capture");
            match receive_wayland_frame(&mut capture, frame_rx) {
                Ok(frame) => Ok((CaptureMode::DmaBuf, capture, frame)),
                Err(err) => {
                    warn!(
                        "DMA-BUF capture failed during first frame, falling back to SHM: {err:#}"
                    );
                    drop(capture);
                    let mut shm_capture =
                        WaylandCapture::new(CaptureMode::Shm, display_id, frame_tx.clone())
                            .context("initialise Wayland SHM capture fallback")?;
                    let frame = receive_wayland_frame(&mut shm_capture, frame_rx)
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
            let frame = receive_wayland_frame(&mut shm_capture, frame_rx)
                .context("capture first SHM frame")?;
            Ok((CaptureMode::Shm, shm_capture, frame))
        }
    }
}

#[cfg(target_os = "linux")]
fn receive_wayland_frame(
    capture: &mut WaylandCapture,
    frame_rx: &Receiver<CaptureFrame>,
) -> Result<CaptureFrame> {
    capture.pump()?;
    frame_rx.recv().context("Wayland capture channel closed")
}

#[cfg(target_os = "windows")]
fn receive_windows_frame(
    capture: &mut WindowsCapture,
    frame_rx: &Receiver<CaptureFrame>,
) -> Result<CaptureFrame> {
    capture.pump()?;
    frame_rx.recv().context("Windows capture channel closed")
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
