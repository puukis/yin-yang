//! C FFI — symbols consumed by the Yang macOS Swift app (`apps/yang/`).
//!
//! # Memory contract
//!
//! 1. `yang_connect` allocates a `YangSession` on the heap and returns a raw pointer.
//! 2. The caller must keep the pointer alive until it calls `yang_disconnect`.
//! 3. `yang_disconnect` blocks the calling thread until the render thread and
//!    network supervisor have stopped cleanly.
//! 4. `yang_free` deallocates the `YangSession`; call it exactly once after
//!    `yang_disconnect` returns.
//!
//! # Threading
//!
//! `yang_connect` blocks — call it from a background thread.
//! `yang_disconnect` / `yang_free` may be called from any thread.
//! The `stats_cb` fires from a background thread ~1 Hz; dispatch to the main
//! thread inside the callback before touching any UI.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tracing::warn;

// ---------------------------------------------------------------------------
// Once-guard: rustls crypto provider + tracing
// ---------------------------------------------------------------------------

static FFI_INIT: std::sync::Once = std::sync::Once::new();

fn ffi_init() {
    FFI_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "yang=debug,info".to_owned());
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}

// ---------------------------------------------------------------------------
// Public C types
// ---------------------------------------------------------------------------

/// Options for [`yang_connect`].
///
/// All pointer fields are read only during the `yang_connect` call; they need
/// not remain valid after it returns.
#[repr(C)]
pub struct YangConnectOptions {
    /// Null-terminated server address, e.g. `"192.168.1.50:9000"`.
    pub server_addr: *const c_char,
    /// Null-terminated display selector (index, name, or id), or `NULL` for
    /// the server's first display.
    pub display_selector: *const c_char,
    /// Maximum bitrate cap in Mbps; `0` = unlimited.
    pub max_bitrate_mbps: u32,
    /// Minimum bitrate floor for adaptive control in Mbps; `0` = no floor.
    pub min_bitrate_mbps: u32,
    /// Maximum frames per second to request.
    pub max_fps: u8,
    /// Minimum frames per second the adaptive controller may target.
    pub min_fps: u8,
    /// Enable automatic bitrate/FPS adaptation.
    pub adaptive_streaming: bool,
    /// Enable GPU optical-flow frame interpolation.
    pub interpolate: bool,
}

/// Live stream statistics delivered to the [`YangStatsCallback`] ~once per second.
#[repr(C)]
pub struct YangStats {
    /// Presented frames in the last second.
    pub fps: f32,
    /// Estimated receive bitrate in Mbps (placeholder — always 0 until
    /// bitrate tracking is added to `ClientTelemetryAccumulator`).
    pub bitrate_mbps: f32,
    /// Frames successfully decoded and presented.
    pub frames_decoded: u32,
    /// Frames dropped by the render queue (arrived late or render saturated).
    pub frames_dropped: u32,
    /// Frames lost unrecoverably (FEC could not reconstruct).
    pub unrecoverable_frames: u32,
}

/// Information about one display exported by the server.
#[repr(C)]
pub struct YangDisplayInfo {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    /// Short display name (null-terminated; may be empty).
    pub name: [c_char; 128],
    /// Stable machine-readable display id (null-terminated).
    pub id: [c_char; 128],
    /// Human-readable description (null-terminated; may be empty).
    pub description: [c_char; 256],
}

/// Callback invoked ~1 Hz from a background thread with current stream stats.
///
/// `userdata` is the value passed to [`yang_connect`].  Dispatch to the main
/// thread before touching any UI state.
pub type YangStatsCallback =
    Option<unsafe extern "C" fn(stats: *const YangStats, userdata: *mut c_void)>;

// ---------------------------------------------------------------------------
// Session handle (opaque to callers)
// ---------------------------------------------------------------------------

pub struct YangSession {
    runtime: tokio::runtime::Runtime,
    /// `Option` so we can `take()` for the async shutdown call.
    session: Option<crate::transport::control::ClientSession>,
    /// Shared shutdown flag — also owned by the render loop and supervisor.
    shutdown: Arc<AtomicBool>,
    render_thread: Option<std::thread::JoinHandle<()>>,
    stats_stop: Arc<AtomicBool>,
    stats_thread: Option<std::thread::JoinHandle<()>>,
    /// Server-reported stream dimensions (pixels).
    initial_width: u32,
    initial_height: u32,
}

// ---------------------------------------------------------------------------
// Helper: send raw pointer across thread boundaries
// ---------------------------------------------------------------------------

struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}
impl SendPtr {
    fn get(self) -> *mut c_void {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Exported functions
// ---------------------------------------------------------------------------

/// Connect to a Yin server and start streaming into `ca_metal_layer`.
///
/// # Safety
///
/// Blocks until the QUIC session is established (or fails).
/// **Do not call from the macOS main thread.**
///
/// `ca_metal_layer` must be a valid `CAMetalLayer *` owned by the caller.
/// Rust configures the layer (pixel format, device, drawable count) but never
/// retains or releases the ObjC object.
///
/// Returns a non-null `YangSession *` on success, `NULL` on failure.
#[no_mangle]
pub unsafe extern "C" fn yang_connect(
    opts: *const YangConnectOptions,
    ca_metal_layer: *mut c_void,
    stats_cb: YangStatsCallback,
    stats_userdata: *mut c_void,
) -> *mut YangSession {
    ffi_init();

    if opts.is_null() || ca_metal_layer.is_null() {
        return std::ptr::null_mut();
    }
    let opts = &*opts;

    // Parse server address
    let server_addr_str = match CStr::from_ptr(opts.server_addr).to_str() {
        Ok(s) => s,
        Err(_) => {
            warn!("yang_connect: server_addr is not valid UTF-8");
            return std::ptr::null_mut();
        }
    };
    let server_addr: std::net::SocketAddr = match server_addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!("yang_connect: failed to parse server address '{server_addr_str}': {e}");
            return std::ptr::null_mut();
        }
    };

    // Optional display selector (copy before runtime takes over the stack frame)
    let display_selector = if opts.display_selector.is_null() {
        None
    } else {
        match CStr::from_ptr(opts.display_selector).to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_owned()),
            _ => None,
        }
    };

    let max_fps = opts.max_fps.clamp(1, 120);
    let min_fps = opts.min_fps.clamp(1, max_fps);

    let client_opts = crate::transport::control::ClientOptions {
        client_session_id: new_session_id(),
        adaptive_streaming: opts.adaptive_streaming,
        list_displays: false,
        display_selector,
        max_fps,
        min_fps,
        max_bitrate_bps: opts.max_bitrate_mbps.saturating_mul(1_000_000),
        min_bitrate_bps: opts.min_bitrate_mbps.saturating_mul(1_000_000),
        interpolate: opts.interpolate,
    };

    // Build the async runtime and establish the QUIC session
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn!("yang_connect: failed to build Tokio runtime: {e}");
            return std::ptr::null_mut();
        }
    };

    let mut client_session = match runtime.block_on(
        crate::transport::control::connect_client_session(server_addr, client_opts),
    ) {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!("yang_connect: server returned no session");
            return std::ptr::null_mut();
        }
        Err(e) => {
            warn!("yang_connect: connection failed: {e:#}");
            return std::ptr::null_mut();
        }
    };

    // Extract everything we need before moving client_session into the struct
    let render_rx = match client_session.take_render_rx() {
        Ok(rx) => rx,
        Err(e) => {
            warn!("yang_connect: failed to take render channel: {e:#}");
            runtime.block_on(client_session.shutdown()).ok();
            return std::ptr::null_mut();
        }
    };
    let initial_width = client_session.width;
    let initial_height = client_session.height;
    let cursor_store = client_session.cursor_store();
    let shutdown = client_session.shutdown_signal();
    let telemetry = client_session.telemetry();
    let interpolate = opts.interpolate;

    // Spawn the render thread (runs until shutdown or channel closes)
    let layer_send = SendPtr(ca_metal_layer);
    let shutdown_for_render = shutdown.clone();
    let render_thread = std::thread::Builder::new()
        .name("yang-render".to_owned())
        .spawn(move || {
            crate::render::metal::render_loop_macos_ffi(
                layer_send.get(),
                render_rx,
                initial_width,
                initial_height,
                cursor_store,
                shutdown_for_render,
                telemetry,
                interpolate,
            );
        })
        .expect("spawn yang-render thread");

    // Spawn the stats thread (optional)
    let stats_stop = Arc::new(AtomicBool::new(false));
    let stats_thread = stats_cb.map(|cb| {
        let stop = stats_stop.clone();
        let telem = client_session.telemetry();
        let ud_send = SendPtr(stats_userdata);
        std::thread::Builder::new()
            .name("yang-stats".to_owned())
            .spawn(move || stats_loop(telem, cb, ud_send.get(), stop))
            .expect("spawn yang-stats thread")
    });

    let session = Box::new(YangSession {
        runtime,
        session: Some(client_session),
        shutdown,
        render_thread: Some(render_thread),
        stats_stop,
        stats_thread,
        initial_width,
        initial_height,
    });

    Box::into_raw(session)
}

/// Signal shutdown and block until the render thread and network session stop.
///
/// After this returns, call [`yang_free`] to release the session memory.
///
/// # Safety
#[no_mangle]
pub unsafe extern "C" fn yang_disconnect(ptr: *mut YangSession) {
    if ptr.is_null() {
        return;
    }
    let session = &mut *ptr;

    // Signal both the render loop and the session supervisor to stop
    session.shutdown.store(true, Ordering::Relaxed);

    // Stop stats thread first (fast)
    session.stats_stop.store(true, Ordering::Relaxed);
    if let Some(t) = session.stats_thread.take() {
        let _ = t.join();
    }

    // Wait for the render thread
    if let Some(t) = session.render_thread.take() {
        let _ = t.join();
    }

    // Shut down the async session supervisor
    if let Some(client_session) = session.session.take() {
        session.runtime.block_on(client_session.shutdown()).ok();
    }
}

/// Deallocate the session.  Must be called after [`yang_disconnect`] returns.
///
/// # Safety
#[no_mangle]
pub unsafe extern "C" fn yang_free(ptr: *mut YangSession) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}

/// Return the stream's pixel dimensions as negotiated with the server.
///
/// Call after [`yang_connect`] succeeds; safe to call from any thread.
/// Returns `(0, 0)` if `ptr` is null.
///
/// # Safety
#[no_mangle]
pub unsafe extern "C" fn yang_stream_size(
    ptr: *const YangSession,
    out_width: *mut u32,
    out_height: *mut u32,
) {
    let (w, h) = if ptr.is_null() {
        (0, 0)
    } else {
        let s = &*ptr;
        (s.initial_width, s.initial_height)
    };
    if !out_width.is_null() {
        *out_width = w;
    }
    if !out_height.is_null() {
        *out_height = h;
    }
}

/// Query the displays available on the server synchronously.
///
/// Writes up to `max_count` entries into `out`.
/// Returns the number of displays written, or `-1` on error.
///
/// # Safety
#[no_mangle]
pub unsafe extern "C" fn yang_list_displays(
    server_addr: *const c_char,
    out: *mut YangDisplayInfo,
    max_count: c_int,
) -> c_int {
    ffi_init();

    if server_addr.is_null() || out.is_null() || max_count <= 0 {
        return -1;
    }

    let addr_str = match CStr::from_ptr(server_addr).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let addr: std::net::SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!("yang_list_displays: bad address '{addr_str}': {e}");
            return -1;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn!("yang_list_displays: failed to build runtime: {e}");
            return -1;
        }
    };

    let displays = match runtime.block_on(crate::transport::control::list_displays(addr)) {
        Ok(d) => d,
        Err(e) => {
            warn!("yang_list_displays: {e:#}");
            return -1;
        }
    };

    let count = displays.len().min(max_count as usize);
    let out_slice = std::slice::from_raw_parts_mut(out, count);

    for (i, info) in displays.iter().take(count).enumerate() {
        let mut entry = YangDisplayInfo {
            index: info.index,
            width: info.width,
            height: info.height,
            name: [0; 128],
            id: [0; 128],
            description: [0; 256],
        };
        copy_str_to_cchar_buf(&info.name, &mut entry.name);
        copy_str_to_cchar_buf(&info.id, &mut entry.id);
        if let Some(ref desc) = info.description {
            copy_str_to_cchar_buf(desc, &mut entry.description);
        }
        out_slice[i] = entry;
    }

    count as c_int
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("yang-ffi-{}-{us}", std::process::id())
}

fn copy_str_to_cchar_buf(s: &str, buf: &mut [c_char]) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(buf.len() - 1);
    for (i, &b) in bytes[..len].iter().enumerate() {
        buf[i] = b as c_char;
    }
    buf[len] = 0;
}

fn stats_loop(
    telemetry: crate::telemetry::SharedClientTelemetry,
    cb: unsafe extern "C" fn(*const YangStats, *mut c_void),
    userdata: *mut c_void,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let sample = telemetry.drain();
        let bitrate_mbps = (sample.received_bytes as f32 * 8.0) / 1_000_000.0;
        let stats = YangStats {
            fps: sample.proto.presented_frames as f32,
            bitrate_mbps,
            frames_decoded: sample.proto.presented_frames,
            frames_dropped: sample.proto.render_dropped_frames,
            unrecoverable_frames: sample.proto.unrecoverable_frames,
        };
        unsafe { cb(&stats, userdata) };
    }
}
