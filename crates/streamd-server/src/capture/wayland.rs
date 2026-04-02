//! Wayland screen capture via `ext-image-copy-capture-v1`.
//!
//! This backend supports either shared-memory capture or DMA-BUF capture
//! backed by GBM allocations on the compositor-advertised DRM device.

use anyhow::{bail, Context, Result};
use crossbeam_channel::Sender;
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;
use std::ffi::{c_void, CString};
use std::fs::{File, OpenOptions};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use streamd_proto::packets::DisplayInfo;
use tracing::{debug, info, warn};
use wayland_client::{
    globals::{registry_queue_init, Global, GlobalListContents},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::capture::{CaptureFrame, CaptureStats, DmabufPixelFormat, ShmPixelFormat};

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

static NEXT_DMABUF_BUFFER_ID: AtomicU64 = AtomicU64::new(1);

impl ShmPixelFormat {
    fn from_wayland(format: wl_shm::Format) -> Option<Self> {
        match format {
            wl_shm::Format::Xrgb8888 => Some(Self::Xrgb8888),
            wl_shm::Format::Argb8888 => Some(Self::Argb8888),
            _ => None,
        }
    }

    fn to_wayland(self) -> wl_shm::Format {
        match self {
            Self::Xrgb8888 => wl_shm::Format::Xrgb8888,
            Self::Argb8888 => wl_shm::Format::Argb8888,
        }
    }
}

impl DmabufPixelFormat {
    fn drm_format(self) -> u32 {
        match self {
            Self::Xrgb8888 => drm_fourcc(b'X', b'R', b'2', b'4'),
            Self::Argb8888 => drm_fourcc(b'A', b'R', b'2', b'4'),
        }
    }

    fn from_drm_format(format: u32) -> Option<Self> {
        match format {
            f if f == drm_fourcc(b'X', b'R', b'2', b'4') => Some(Self::Xrgb8888),
            f if f == drm_fourcc(b'A', b'R', b'2', b'4') => Some(Self::Argb8888),
            _ => None,
        }
    }
}

/// Whether to capture via DMA-BUF or SHM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    DmaBuf,
    Shm,
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let conn = Connection::connect_to_env().context("connect to Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init::<OutputDiscoveryState>(&conn).context("init registry")?;
    let registry = globals.registry().clone();
    let qh = event_queue.handle();
    let output_globals = output_globals(globals.contents());
    if output_globals.is_empty() {
        bail!("wl_output global unavailable on the Wayland compositor");
    }

    let mut state = OutputDiscoveryState::default();
    for (index, global) in output_globals.into_iter().enumerate() {
        let version = global.version.min(4);
        let output = registry.bind(global.name, version, &qh, global.name);
        state.bound_outputs.push(output);
        state.outputs.insert(
            global.name,
            DiscoveredOutput {
                global_name: global.name,
                index: index as u32,
                make: None,
                model: None,
                name: None,
                description: None,
                width: 0,
                height: 0,
            },
        );
    }

    event_queue
        .roundtrip(&mut state)
        .context("Wayland output discovery roundtrip")?;
    event_queue
        .roundtrip(&mut state)
        .context("Wayland output discovery roundtrip")?;

    Ok(state.into_display_infos())
}

fn output_globals(contents: &GlobalListContents) -> Vec<Global> {
    let mut outputs = contents
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == wl_output::WlOutput::interface().name)
        .collect::<Vec<_>>();
    outputs.sort_by_key(|global| global.name);
    outputs
}

fn select_output_global(contents: &GlobalListContents, display_id: Option<&str>) -> Result<Global> {
    let outputs = output_globals(contents);
    if outputs.is_empty() {
        bail!("wl_output global unavailable on the Wayland compositor");
    }

    if let Some(display_id) = display_id {
        let Some(global_name) = parse_wayland_display_id(display_id) else {
            bail!("unsupported Wayland display id {display_id:?}");
        };
        return outputs
            .into_iter()
            .find(|global| global.name == global_name)
            .with_context(|| {
                format!("Wayland output {display_id} was not advertised by the compositor")
            });
    }

    Ok(outputs
        .into_iter()
        .next()
        .expect("checked non-empty output list"))
}

fn wayland_display_id(global_name: u32) -> String {
    format!("wayland:{global_name}")
}

fn parse_wayland_display_id(display_id: &str) -> Option<u32> {
    display_id
        .strip_prefix("wayland:")
        .or_else(|| display_id.strip_prefix("wl:"))
        .unwrap_or(display_id)
        .parse()
        .ok()
}

#[derive(Default)]
struct OutputDiscoveryState {
    outputs: std::collections::BTreeMap<u32, DiscoveredOutput>,
    bound_outputs: Vec<wl_output::WlOutput>,
}

struct DiscoveredOutput {
    global_name: u32,
    index: u32,
    make: Option<String>,
    model: Option<String>,
    name: Option<String>,
    description: Option<String>,
    width: u32,
    height: u32,
}

impl OutputDiscoveryState {
    fn into_display_infos(self) -> Vec<DisplayInfo> {
        self.outputs
            .into_values()
            .map(|output| {
                let fallback_name = output
                    .name
                    .clone()
                    .or_else(|| combine_make_model(output.make.as_deref(), output.model.as_deref()))
                    .unwrap_or_else(|| format!("wayland-output-{}", output.global_name));
                let description = output
                    .description
                    .clone()
                    .or_else(|| combine_make_model(output.make.as_deref(), output.model.as_deref()))
                    .filter(|description| description != &fallback_name);

                DisplayInfo {
                    id: wayland_display_id(output.global_name),
                    index: output.index,
                    name: fallback_name,
                    description,
                    width: output.width,
                    height: output.height,
                }
            })
            .collect()
    }
}

fn combine_make_model(make: Option<&str>, model: Option<&str>) -> Option<String> {
    match (make.map(str::trim), model.map(str::trim)) {
        (Some(make), Some(model)) if !make.is_empty() && !model.is_empty() => {
            Some(format!("{make} {model}"))
        }
        (Some(make), _) if !make.is_empty() => Some(make.to_string()),
        (_, Some(model)) if !model.is_empty() => Some(model.to_string()),
        _ => None,
    }
}

/// Owns the Wayland connection and capture session for one output.
pub struct WaylandCapture {
    _conn: Connection,
    event_queue: EventQueue<State>,
    state: State,
}

impl WaylandCapture {
    /// Connect to the Wayland display and start capturing the primary output.
    ///
    /// Frames are sent on `frame_tx`. Call `next_frame()` in a loop to pump the
    /// event queue and trigger captures.
    pub fn new(
        mode: CaptureMode,
        display_id: Option<&str>,
        frame_tx: Sender<CaptureFrame>,
    ) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland display")?;
        let (globals, event_queue) =
            registry_queue_init::<State>(&conn).context("init registry")?;
        let qh = event_queue.handle();

        let state = State::new(&globals, &qh, mode, display_id, frame_tx.clone())
            .context("init capture state")?;

        info!(
            "Wayland capture initialised (mode={mode:?}, display={})",
            display_id.unwrap_or("default")
        );
        Ok(Self {
            _conn: conn,
            event_queue,
            state,
        })
    }

    /// Drive the Wayland event loop until one frame is delivered to `frame_tx`.
    /// Call this in a tight loop on a dedicated thread.
    pub fn pump(&mut self) -> Result<()> {
        let delivered_before = self.state.frames_emitted;
        while self.state.frames_emitted == delivered_before {
            self.event_queue
                .blocking_dispatch(&mut self.state)
                .context("Wayland dispatch")?;

            if let Some(err) = self.state.take_fatal_error() {
                return Err(err);
            }
            if self.state.stopped {
                bail!("Wayland capture session stopped");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal Wayland state machine
// ---------------------------------------------------------------------------

struct State {
    mode: CaptureMode,
    frame_tx: Sender<CaptureFrame>,
    _output: wl_output::WlOutput,
    shm: wl_shm::WlShm,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    source_manager: ExtOutputImageCaptureSourceManagerV1,
    capture_manager: ExtImageCopyCaptureManagerV1,
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    buf_width: u32,
    buf_height: u32,
    shm_format: Option<ShmPixelFormat>,
    dmabuf_device: Option<u64>,
    dmabuf_format: Option<DmabufFormatSelection>,
    session_configured: bool,
    constraints_dirty: bool,
    shm_buf: Option<ShmBuffer>,
    dmabuf_buf: Option<DmabufBuffer>,
    frame_inflight: bool,
    pending_timestamp_us: Option<u64>,
    frames_emitted: u64,
    stopped: bool,
    fatal_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DmabufFormatSelection {
    format: DmabufPixelFormat,
    modifier: u64,
}

impl State {
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
        mode: CaptureMode,
        display_id: Option<&str>,
        frame_tx: Sender<CaptureFrame>,
    ) -> Result<Self> {
        let selected_output = select_output_global(globals.contents(), display_id)?;
        let output =
            globals
                .registry()
                .bind(selected_output.name, selected_output.version.min(4), qh, ());

        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(qh, 1..=1, ())
            .context("bind wl_shm")?;

        let source_manager = globals
            .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(qh, 1..=1, ())
            .context("bind ext_output_image_capture_source_manager_v1")?;
        let capture_manager = globals
            .bind::<ExtImageCopyCaptureManagerV1, _, _>(qh, 1..=1, ())
            .context("bind ext_image_copy_capture_manager_v1")?;
        let dmabuf = match globals.bind::<ZwpLinuxDmabufV1, _, _>(qh, 2..=5, ()) {
            Ok(dmabuf) => Some(dmabuf),
            Err(err) if mode == CaptureMode::Shm => {
                debug!("linux-dmabuf global unavailable, staying on SHM capture: {err:#}");
                None
            }
            Err(err) => {
                return Err(err).context("bind zwp_linux_dmabuf_v1");
            }
        };

        let source = source_manager.create_source(&output, qh, ());
        let session = capture_manager.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            qh,
            (),
        );

        Ok(Self {
            mode,
            frame_tx,
            _output: output,
            shm,
            dmabuf,
            source_manager,
            capture_manager,
            source,
            session,
            buf_width: 0,
            buf_height: 0,
            shm_format: None,
            dmabuf_device: None,
            dmabuf_format: None,
            session_configured: false,
            constraints_dirty: false,
            shm_buf: None,
            dmabuf_buf: None,
            frame_inflight: false,
            pending_timestamp_us: None,
            frames_emitted: 0,
            stopped: false,
            fatal_error: None,
        })
    }

    fn take_fatal_error(&mut self) -> Option<anyhow::Error> {
        self.fatal_error.take().map(anyhow::Error::msg)
    }

    fn set_fatal_error(&mut self, err: anyhow::Error) {
        if self.fatal_error.is_none() {
            let message = format!("{err:#}");
            warn!("Wayland capture error: {message}");
            self.fatal_error = Some(message);
        }
    }

    fn maybe_start_capture(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        if self.stopped || self.frame_inflight || !self.session_configured {
            return Ok(());
        }

        let session = self.session.clone();
        let (buffer, width, height) = match self.mode {
            CaptureMode::Shm => {
                self.ensure_shm_buffer(qh)?;
                let shm_buf = self
                    .shm_buf
                    .as_ref()
                    .context("capture requested before SHM buffer allocation")?;
                (shm_buf.buffer.clone(), shm_buf.width, shm_buf.height)
            }
            CaptureMode::DmaBuf => {
                self.ensure_dmabuf_buffer(qh)?;
                let dmabuf_buf = self
                    .dmabuf_buf
                    .as_ref()
                    .context("capture requested before DMA-BUF allocation")?;
                (
                    dmabuf_buf.buffer.clone(),
                    dmabuf_buf.width,
                    dmabuf_buf.height,
                )
            }
        };

        let frame = session.create_frame(qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();

        self.pending_timestamp_us = None;
        self.frame_inflight = true;
        Ok(())
    }

    fn ensure_shm_buffer(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let format = self
            .shm_format
            .context("compositor did not advertise an SHM format supported by streamd")?;
        if self.buf_width == 0 || self.buf_height == 0 {
            bail!("compositor reported an invalid capture size");
        }

        let recreate = self.shm_buf.as_ref().is_none_or(|shm_buf| {
            shm_buf.width != self.buf_width
                || shm_buf.height != self.buf_height
                || shm_buf.format != format
        });

        if recreate {
            self.shm_buf = Some(alloc_shm_buffer(
                &self.shm,
                qh,
                self.buf_width,
                self.buf_height,
                format,
            )?);
            info!(
                "Wayland SHM buffer allocated: {}x{} {:?}",
                self.buf_width, self.buf_height, format
            );
        }

        self.constraints_dirty = false;
        Ok(())
    }

    fn ensure_dmabuf_buffer(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let dmabuf = self
            .dmabuf
            .as_ref()
            .context("linux-dmabuf global is unavailable for DMA-BUF capture")?;
        let device_id = self
            .dmabuf_device
            .context("compositor did not advertise a DMA-BUF device")?;
        let selection = self
            .dmabuf_format
            .context("compositor did not advertise a supported DMA-BUF format/modifier")?;

        if self.buf_width == 0 || self.buf_height == 0 {
            bail!("compositor reported an invalid capture size");
        }

        let recreate = self.dmabuf_buf.as_ref().is_none_or(|dmabuf_buf| {
            dmabuf_buf.width != self.buf_width
                || dmabuf_buf.height != self.buf_height
                || dmabuf_buf.format != selection.format
                || dmabuf_buf.modifier != selection.modifier
        });

        if recreate {
            self.dmabuf_buf = Some(alloc_dmabuf_buffer(
                dmabuf,
                qh,
                self.buf_width,
                self.buf_height,
                selection,
                device_id,
            )?);
            info!(
                "Wayland DMA-BUF buffer allocated: {}x{} {:?} modifier={:#x}",
                self.buf_width, self.buf_height, selection.format, selection.modifier
            );
        }

        self.constraints_dirty = false;
        Ok(())
    }

    fn handle_frame_ready(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        self.frame_inflight = false;
        let timestamp_us = self.pending_timestamp_us.take().unwrap_or_default();
        let send_result = match self.mode {
            CaptureMode::Shm => {
                let (data, width, height, stride, format) = {
                    let shm_buf = self
                        .shm_buf
                        .as_ref()
                        .context("frame became ready before a SHM buffer was available")?;
                    let bytes = unsafe {
                        std::slice::from_raw_parts(shm_buf.map.cast::<u8>().as_ptr(), shm_buf.size)
                    }
                    .to_vec();
                    (
                        bytes,
                        shm_buf.width,
                        shm_buf.height,
                        shm_buf.stride,
                        shm_buf.format,
                    )
                };

                self.frame_tx.send(CaptureFrame::Shm {
                    data,
                    width,
                    height,
                    stride,
                    format,
                    timestamp_us,
                    stats: CaptureStats::default(),
                })
            }
            CaptureMode::DmaBuf => {
                let dmabuf_buf = self
                    .dmabuf_buf
                    .as_ref()
                    .context("frame became ready before a DMA-BUF was available")?;
                let fd = dmabuf_buf.export_fd()?;
                self.frame_tx.send(CaptureFrame::DmaBuf {
                    fd,
                    buffer_id: dmabuf_buf.id,
                    width: dmabuf_buf.width,
                    height: dmabuf_buf.height,
                    pitch: dmabuf_buf.pitch,
                    offset: dmabuf_buf.offset,
                    allocation_size: dmabuf_buf.allocation_size,
                    format: dmabuf_buf.format,
                    modifier: dmabuf_buf.modifier,
                    timestamp_us,
                    stats: CaptureStats::default(),
                })
            }
        };

        frame.destroy();

        send_result.context("capture frame receiver dropped")?;
        self.frames_emitted += 1;
        self.maybe_start_capture(qh)?;
        Ok(())
    }

    fn handle_frame_failed(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        reason: WEnum<ext_image_copy_capture_frame_v1::FailureReason>,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        self.frame_inflight = false;
        self.pending_timestamp_us = None;
        frame.destroy();

        match reason {
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Unknown) => {
                warn!("Wayland frame capture failed with an unspecified compositor error");
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints) => {
                warn!("Wayland capture buffer no longer matches compositor constraints");
                self.constraints_dirty = true;
                self.shm_buf = None;
                self.dmabuf_buf = None;
            }
            WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Stopped) => {
                warn!("Wayland compositor stopped the active capture session");
                self.stopped = true;
                return Ok(());
            }
            WEnum::Value(other) => {
                warn!("Wayland frame capture failed with unhandled reason {other:?}");
            }
            WEnum::Unknown(raw) => {
                warn!("Wayland frame capture failed with unknown reason code {raw}");
            }
        }

        self.maybe_start_capture(qh)?;
        Ok(())
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.session.destroy();
        self.source.destroy();
        self.capture_manager.destroy();
        self.source_manager.destroy();
        if let Some(dmabuf) = self.dmabuf.take() {
            dmabuf.destroy();
        }
    }
}

struct ShmBuffer {
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    map: NonNull<c_void>,
    size: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: ShmPixelFormat,
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();

        if let Err(err) = unsafe { munmap(self.map, self.size) } {
            warn!("munmap of Wayland SHM buffer failed: {err}");
        }
    }
}

struct DmabufBuffer {
    id: u64,
    gbm_device: NonNull<GbmDevice>,
    bo: NonNull<GbmBo>,
    buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
    allocation_size: u64,
    format: DmabufPixelFormat,
    modifier: u64,
    _drm_node: File,
}

impl DmabufBuffer {
    fn export_fd(&self) -> Result<OwnedFd> {
        let fd = unsafe { gbm_bo_get_fd(self.bo.as_ptr()) };
        if fd < 0 {
            bail!("gbm_bo_get_fd failed for DMA-BUF capture buffer");
        }

        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for DmabufBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe {
            gbm_bo_destroy(self.bo.as_ptr());
            gbm_device_destroy(self.gbm_device.as_ptr());
        }
    }
}

#[repr(C)]
struct GbmDevice {
    _private: [u8; 0],
}

#[repr(C)]
struct GbmBo {
    _private: [u8; 0],
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut GbmDevice;
    fn gbm_device_destroy(device: *mut GbmDevice);
    fn gbm_device_is_format_supported(device: *mut GbmDevice, format: u32, flags: u32) -> i32;
    fn gbm_bo_create(
        device: *mut GbmDevice,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut GbmBo;
    fn gbm_bo_create_with_modifiers2(
        device: *mut GbmDevice,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> *mut GbmBo;
    fn gbm_bo_get_stride(bo: *mut GbmBo) -> u32;
    fn gbm_bo_get_offset(bo: *mut GbmBo, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut GbmBo) -> u64;
    fn gbm_bo_get_plane_count(bo: *mut GbmBo) -> i32;
    fn gbm_bo_get_fd(bo: *mut GbmBo) -> i32;
    fn gbm_bo_destroy(bo: *mut GbmBo);
}

fn alloc_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
    format: ShmPixelFormat,
) -> Result<ShmBuffer> {
    let stride = width
        .checked_mul(4)
        .context("SHM stride overflow for capture buffer")?;
    let size_u32 = stride
        .checked_mul(height)
        .context("SHM size overflow for capture buffer")?;
    let size = usize::try_from(size_u32).context("SHM buffer size does not fit usize")?;
    let size_i32 = i32::try_from(size).context("SHM buffer too large for wl_shm")?;
    let len = NonZeroUsize::new(size).context("cannot allocate a zero-sized SHM buffer")?;

    let name = CString::new("streamd-shm").expect("literal has no NUL bytes");
    let fd = memfd_create(name.as_c_str(), MemFdCreateFlag::MFD_CLOEXEC)
        .context("memfd_create for Wayland SHM buffer")?;
    ftruncate(&fd, size as i64).context("resize Wayland SHM buffer")?;

    let map = unsafe {
        mmap(
            None,
            len,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            &fd,
            0,
        )
    }
    .context("mmap Wayland SHM buffer")?;

    let pool = shm.create_pool(fd.as_fd(), size_i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format.to_wayland(),
        qh,
        (),
    );

    Ok(ShmBuffer {
        pool,
        buffer,
        map,
        size,
        width,
        height,
        stride,
        format,
    })
}

fn alloc_dmabuf_buffer(
    dmabuf: &ZwpLinuxDmabufV1,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
    selection: DmabufFormatSelection,
    device_id: u64,
) -> Result<DmabufBuffer> {
    let drm_node = open_drm_node_for_device(device_id)?;
    let gbm_device = unsafe { gbm_create_device(drm_node.as_raw_fd()) };
    let gbm_device = NonNull::new(gbm_device).context("gbm_create_device returned null")?;

    let gbm_flags = GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR;
    let drm_format = selection.format.drm_format();
    let supported =
        unsafe { gbm_device_is_format_supported(gbm_device.as_ptr(), drm_format, gbm_flags) };
    if supported == 0 {
        bail!("GBM device does not support format {drm_format:#x} with flags {gbm_flags:#x}");
    }

    let mut bo = unsafe {
        gbm_bo_create_with_modifiers2(
            gbm_device.as_ptr(),
            width,
            height,
            drm_format,
            &selection.modifier,
            1,
            gbm_flags,
        )
    };
    if bo.is_null() && selection.modifier == DRM_FORMAT_MOD_LINEAR {
        bo = unsafe { gbm_bo_create(gbm_device.as_ptr(), width, height, drm_format, gbm_flags) };
    }
    let bo = NonNull::new(bo).context("failed to allocate GBM DMA-BUF buffer")?;

    let plane_count = unsafe { gbm_bo_get_plane_count(bo.as_ptr()) };
    if plane_count != 1 {
        bail!("only single-plane DMA-BUF buffers are supported, got {plane_count}");
    }

    let pitch = unsafe { gbm_bo_get_stride(bo.as_ptr()) };
    let offset = unsafe { gbm_bo_get_offset(bo.as_ptr(), 0) };
    let modifier = unsafe { gbm_bo_get_modifier(bo.as_ptr()) };
    let allocation_size = u64::from(offset)
        .checked_add(u64::from(pitch).saturating_mul(u64::from(height)))
        .context("DMA-BUF allocation size overflow")?;

    let plane_fd = {
        let fd = unsafe { gbm_bo_get_fd(bo.as_ptr()) };
        if fd < 0 {
            bail!("gbm_bo_get_fd failed while creating wl_buffer for DMA-BUF capture");
        }
        unsafe { OwnedFd::from_raw_fd(fd) }
    };

    let params = dmabuf.create_params(qh, ());
    params.add(
        plane_fd.as_fd(),
        0,
        offset,
        pitch,
        (modifier >> 32) as u32,
        modifier as u32,
    );
    let buffer = params.create_immed(
        width as i32,
        height as i32,
        drm_format,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();
    drop(plane_fd);

    Ok(DmabufBuffer {
        id: NEXT_DMABUF_BUFFER_ID.fetch_add(1, AtomicOrdering::Relaxed),
        gbm_device,
        bo,
        buffer,
        width,
        height,
        pitch,
        offset,
        allocation_size,
        format: selection.format,
        modifier,
        _drm_node: drm_node,
    })
}

fn open_drm_node_for_device(device_id: u64) -> Result<File> {
    let dri_dir = std::fs::read_dir("/dev/dri").context("read /dev/dri")?;
    let mut render_nodes = Vec::new();
    let mut primary_nodes = Vec::new();

    for entry in dri_dir {
        let entry = entry.context("read /dev/dri entry")?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("renderD") && !name.starts_with("card") {
            continue;
        }

        let metadata = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        if metadata.rdev() != device_id {
            continue;
        }

        if name.starts_with("renderD") {
            render_nodes.push(path);
        } else {
            primary_nodes.push(path);
        }
    }

    render_nodes.sort();
    primary_nodes.sort();

    let path = render_nodes
        .into_iter()
        .chain(primary_nodes)
        .next()
        .context("no /dev/dri node matched the compositor-advertised DMA-BUF device")?;

    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open DRM node {}", path.display()))
}

fn parse_dmabuf_device(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        4 => Some(u32::from_ne_bytes(bytes.try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

fn parse_dmabuf_modifiers(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .filter_map(|chunk| Some(u64::from_ne_bytes(chunk.try_into().ok()?)))
        .collect()
}

fn maybe_update_dmabuf_format(
    current: Option<DmabufFormatSelection>,
    drm_format: u32,
    modifiers: &[u64],
) -> Option<DmabufFormatSelection> {
    let pixel_format = DmabufPixelFormat::from_drm_format(drm_format)?;
    if !modifiers.contains(&DRM_FORMAT_MOD_LINEAR) {
        return current;
    }

    let candidate = DmabufFormatSelection {
        format: pixel_format,
        modifier: DRM_FORMAT_MOD_LINEAR,
    };

    match (current, candidate.format) {
        (Some(existing), DmabufPixelFormat::Argb8888)
            if existing.format == DmabufPixelFormat::Xrgb8888 =>
        {
            Some(existing)
        }
        _ => Some(candidate),
    }
}

const fn drm_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

fn maybe_update_shm_format(
    current: Option<ShmPixelFormat>,
    advertised: WEnum<wl_shm::Format>,
) -> Option<ShmPixelFormat> {
    match advertised {
        WEnum::Value(format) => match ShmPixelFormat::from_wayland(format) {
            Some(ShmPixelFormat::Xrgb8888) => Some(ShmPixelFormat::Xrgb8888),
            Some(ShmPixelFormat::Argb8888) => {
                if current == Some(ShmPixelFormat::Xrgb8888) {
                    current
                } else {
                    Some(ShmPixelFormat::Argb8888)
                }
            }
            None => current,
        },
        WEnum::Unknown(raw) => {
            debug!("ignoring unknown wl_shm format code {raw}");
            current
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for OutputDiscoveryState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for OutputDiscoveryState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(global_name) else {
            return;
        };

        match event {
            wl_output::Event::Geometry { make, model, .. } => {
                output.make = Some(make);
                output.model = Some(model);
            }
            wl_output::Event::Mode { width, height, .. } if width > 0 && height > 0 => {
                output.width = width as u32;
                output.height = height as u32;
            }
            wl_output::Event::Name { name } => {
                output.name = Some(name);
            }
            wl_output::Event::Description { description } => {
                output.description = Some(description);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            width,
            height,
            refresh,
            ..
        } = event
        {
            debug!("output mode: {width}x{height} refresh={refresh}");
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event {
            debug!("wl_shm format advertised: {format:?}");
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: <ExtOutputImageCaptureSourceManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: <ExtImageCaptureSourceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: <ExtImageCopyCaptureManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buf_width = width;
                state.buf_height = height;
                state.constraints_dirty = true;
                debug!("Wayland capture size updated to {}x{}", width, height);
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                let updated = maybe_update_shm_format(state.shm_format, format);
                if updated != state.shm_format {
                    debug!("selected SHM capture format: {updated:?}");
                }
                state.shm_format = updated;
                state.constraints_dirty = true;
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                let parsed = parse_dmabuf_device(&device);
                if parsed != state.dmabuf_device {
                    debug!("selected DMA-BUF capture device: {parsed:?}");
                }
                if parsed.is_none() {
                    warn!(
                        "failed to parse compositor DMA-BUF device advertisement ({} bytes)",
                        device.len()
                    );
                }
                state.dmabuf_device = parsed;
                state.constraints_dirty = true;
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let parsed_modifiers = parse_dmabuf_modifiers(&modifiers);
                let updated =
                    maybe_update_dmabuf_format(state.dmabuf_format, format, &parsed_modifiers);
                if updated != state.dmabuf_format {
                    debug!("selected DMA-BUF capture format: {updated:?}");
                } else {
                    debug!(
                        "DMA-BUF format advertisement ignored: format={format:#x} modifiers={parsed_modifiers:?}"
                    );
                }
                state.dmabuf_format = updated;
                state.constraints_dirty = true;
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.session_configured = true;
                if let Err(err) = state.maybe_start_capture(qh) {
                    state.set_fatal_error(err);
                }
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                warn!("Wayland capture session was stopped by the compositor");
                state.stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        debug!("zwp_linux_dmabuf_v1 event: {event:?}");
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { .. } => {
                debug!("zwp_linux_buffer_params_v1 created a wl_buffer");
            }
            zwp_linux_buffer_params_v1::Event::Failed => {
                state.set_fatal_error(anyhow::anyhow!(
                    "compositor rejected the client DMA-BUF buffer import"
                ));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                debug!("Wayland frame transform: {transform:?}");
            }
            ext_image_copy_capture_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                debug!("Wayland frame damage: x={x} y={y} w={width} h={height}");
            }
            ext_image_copy_capture_frame_v1::Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let seconds = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                state.pending_timestamp_us =
                    Some(seconds.saturating_mul(1_000_000) + (tv_nsec as u64 / 1_000));
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                if let Err(err) = state.handle_frame_ready(frame, qh) {
                    state.set_fatal_error(err);
                }
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                if let Err(err) = state.handle_frame_failed(frame, reason, qh) {
                    state.set_fatal_error(err);
                }
            }
            _ => {}
        }
    }
}
