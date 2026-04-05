//! macOS frame presenter.
#![cfg_attr(target_os = "macos", allow(unexpected_cfgs))]

use anyhow::{bail, Result};
use crossbeam_channel::Receiver;
use std::sync::{atomic::AtomicBool, Arc};

use crate::{
    cursor::RemoteCursorStore, decode::videotoolbox::RenderFrame, telemetry::SharedClientTelemetry,
};

#[cfg(target_os = "macos")]
use anyhow::{anyhow, Context};
#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_video::{
    metal_texture::{CVMetalTexture, CVMetalTextureGetTexture},
    metal_texture_cache::CVMetalTextureCache,
    pixel_buffer::{
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    },
};
#[cfg(target_os = "macos")]
use metal::{
    foreign_types::ForeignTypeRef as _, CommandQueue, CompileOptions, Device, DeviceRef,
    MTLClearColor, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLStorageMode,
    MTLStoreAction, MTLTextureType, MTLTextureUsage, MetalLayer, MetalLayerRef,
    RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState, Texture,
    TextureDescriptor, TextureRef,
};
#[cfg(target_os = "macos")]
use objc::{msg_send, sel, sel_impl};
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "macos")]
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_os = "macos")]
use tracing::{info, warn};
#[cfg(target_os = "macos")]
use yin_yang_proto::packets::{RemoteCursorShape, RemoteCursorShapeKind};

pub struct VideoRenderer;

impl VideoRenderer {
    pub fn run(
        render_rx: Receiver<RenderFrame>,
        initial_width: u32,
        initial_height: u32,
        cursor_store: Arc<RemoteCursorStore>,
        shutdown: Arc<AtomicBool>,
        telemetry: SharedClientTelemetry,
        interpolate: bool,
    ) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            render_loop_macos(
                render_rx,
                initial_width,
                initial_height,
                cursor_store,
                shutdown,
                telemetry,
                interpolate,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (
                render_rx,
                initial_width,
                initial_height,
                cursor_store,
                shutdown,
                telemetry,
                interpolate,
            );
            bail!("yang video presentation is only supported on macOS");
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    tex_coord: [f32; 2],
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ColorConversionParams {
    full_range: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CursorRenderParams {
    cursor_origin: [i32; 2],
    cursor_size: [u32; 2],
    cursor_row_bytes: u32,
    cursor_kind: u32,
    cursor_visible: u32,
    cursor_has_shape: u32,
    frame_size: [u32; 2],
    _padding: [u32; 2],
}

#[cfg(target_os = "macos")]
struct RendererState {
    device: Device,
    /// Owned layer — `Some` in the CLI path (Rust creates the window + layer).
    owned_layer: Option<MetalLayer>,
    /// Borrowed layer pointer — non-null in the FFI/Swift path (Swift owns the layer).
    ext_layer_ptr: *mut std::ffi::c_void,
    command_queue: CommandQueue,
    pipeline_state: RenderPipelineState,
    texture_cache: CVMetalTextureCache,
    null_color_texture: Texture,
    null_mono_texture: Texture,
    color_cursor_texture: Option<Texture>,
    mono_cursor_texture: Option<Texture>,
    current_cursor_generation: Option<u64>,
    /// GPU optical-flow frame interpolator. `None` when `--interpolate` is not set.
    interpolator: Option<crate::render::interpolator::FrameInterpolator>,
}

#[cfg(target_os = "macos")]
struct RenderStats {
    presented_frames: u32,
    dropped_frames: u32,
    total_decode_queue_us: u64,
    total_decode_us: u64,
    total_render_queue_us: u64,
    total_present_cpu_us: u64,
    window_started_at: Instant,
}

#[cfg(target_os = "macos")]
impl RenderStats {
    fn new() -> Self {
        Self {
            presented_frames: 0,
            dropped_frames: 0,
            total_decode_queue_us: 0,
            total_decode_us: 0,
            total_render_queue_us: 0,
            total_present_cpu_us: 0,
            window_started_at: Instant::now(),
        }
    }

    fn record_presented(
        &mut self,
        frame: &RenderFrame,
        render_queue_us: u32,
        present_cpu_us: u32,
        dropped_frames: u32,
    ) {
        self.presented_frames += 1;
        self.dropped_frames += dropped_frames;
        self.total_decode_queue_us += frame
            .decode_submitted_at_us
            .saturating_sub(frame.received_at_us);
        self.total_decode_us += frame
            .decoded_at_us
            .saturating_sub(frame.decode_submitted_at_us);
        self.total_render_queue_us += render_queue_us as u64;
        self.total_present_cpu_us += present_cpu_us as u64;
    }

    fn maybe_log(&mut self) {
        if self.window_started_at.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }

        let avg = |total: u64, frames: u32| {
            if frames > 0 {
                (total / frames as u64) as u32
            } else {
                0
            }
        };

        info!(
            "renderer telemetry: presented={} dropped={} decode_queue={}µs decode={}µs render_queue={}µs present_cpu={}µs",
            self.presented_frames,
            self.dropped_frames,
            avg(self.total_decode_queue_us, self.presented_frames),
            avg(self.total_decode_us, self.presented_frames),
            avg(self.total_render_queue_us, self.presented_frames),
            avg(self.total_present_cpu_us, self.presented_frames),
        );

        *self = Self::new();
    }
}

#[cfg(target_os = "macos")]
const VIDEO_SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexIn {
    float2 position;
    float2 tex_coord;
};

struct VertexOut {
    float4 position [[position]];
    float2 tex_coord;
};

struct ColorConversionParams {
    uint full_range;
};

struct CursorRenderParams {
    int2 cursor_origin;
    uint2 cursor_size;
    uint cursor_row_bytes;
    uint cursor_kind;
    uint cursor_visible;
    uint cursor_has_shape;
    uint2 frame_size;
    uint2 _padding;
};

vertex VertexOut video_vertex(
    const device VertexIn* vertices [[buffer(0)]],
    uint vertex_id [[vertex_id]]
) {
    VertexOut vertex_out;
    const VertexIn vertex_in = vertices[vertex_id];
    vertex_out.position = float4(vertex_in.position, 0.0, 1.0);
    vertex_out.tex_coord = vertex_in.tex_coord;
    return vertex_out;
}

fragment float4 video_fragment(
    VertexOut in [[stage_in]],
    texture2d<float> luma_tex [[texture(0)]],
    texture2d<float> chroma_tex [[texture(1)]],
    texture2d<uint> cursor_color_tex [[texture(2)]],
    texture2d<uint> cursor_mono_tex [[texture(3)]],
    constant ColorConversionParams& params [[buffer(0)]],
    constant CursorRenderParams& cursor [[buffer(1)]]
) {
    constexpr sampler tex_sampler(coord::normalized, address::clamp_to_edge, filter::linear);

    const float y = luma_tex.sample(tex_sampler, in.tex_coord).r;
    const float2 uv = chroma_tex.sample(tex_sampler, in.tex_coord).rg - float2(0.5, 0.5);

    float luma = y;
    if (params.full_range == 0) {
        luma = max(y - (16.0 / 255.0), 0.0) * (255.0 / 219.0);
    }

    const float r = luma + 1.59603 * uv.y;
    const float g = luma - 0.39176 * uv.x - 0.81297 * uv.y;
    const float b = luma + 2.01723 * uv.x;
    float3 rgb = clamp(float3(r, g, b), 0.0, 1.0);

    if (cursor.cursor_visible == 0 || cursor.cursor_has_shape == 0) {
        return float4(rgb, 1.0);
    }

    const uint frame_width = max(cursor.frame_size.x, 1u);
    const uint frame_height = max(cursor.frame_size.y, 1u);
    const uint px = min((uint)(in.tex_coord.x * frame_width), frame_width - 1);
    const uint py = min((uint)(in.tex_coord.y * frame_height), frame_height - 1);
    const int2 cursor_pos = int2((int)px, (int)py) - cursor.cursor_origin;

    if (cursor_pos.x < 0 || cursor_pos.y < 0
        || (uint)cursor_pos.x >= cursor.cursor_size.x
        || (uint)cursor_pos.y >= cursor.cursor_size.y) {
        return float4(rgb, 1.0);
    }

    if (cursor.cursor_kind == 1u || cursor.cursor_kind == 2u) {
        const uint4 sample = cursor_color_tex.read(uint2((uint)cursor_pos.x, (uint)cursor_pos.y));
        const float3 cursor_rgb = float3(sample.r, sample.g, sample.b) / 255.0;
        const uint alpha = sample.a;

        if (cursor.cursor_kind == 1u) {
            if (alpha == 0u) {
                return float4(rgb, 1.0);
            }
            const float a = (float)alpha / 255.0;
            rgb = mix(rgb, cursor_rgb, a);
        } else {
            if (alpha == 255u) {
                const uint3 base_rgb = uint3(round(rgb * 255.0));
                rgb = float3(base_rgb ^ sample.rgb) / 255.0;
            } else {
                rgb = cursor_rgb;
            }
        }

        return float4(clamp(rgb, 0.0, 1.0), 1.0);
    }

    if (cursor.cursor_kind == 3u) {
        const uint byte_col = (uint)cursor_pos.x / 8u;
        const uint bit = 0x80u >> ((uint)cursor_pos.x % 8u);
        const uint and_byte = cursor_mono_tex.read(uint2(byte_col, (uint)cursor_pos.y)).r;
        const uint xor_byte = cursor_mono_tex.read(
            uint2(byte_col, (uint)cursor_pos.y + cursor.cursor_size.y)
        ).r;
        const bool and_bit = (and_byte & bit) != 0u;
        const bool xor_bit = (xor_byte & bit) != 0u;

        if (!and_bit && !xor_bit) {
            rgb = float3(0.0);
        } else if (!and_bit && xor_bit) {
            rgb = float3(1.0);
        } else if (and_bit && xor_bit) {
            rgb = 1.0 - rgb;
        }

        return float4(clamp(rgb, 0.0, 1.0), 1.0);
    }

    return float4(rgb, 1.0);
}

"#;

#[cfg(target_os = "macos")]
fn render_loop_macos(
    render_rx: Receiver<RenderFrame>,
    initial_width: u32,
    initial_height: u32,
    cursor_store: Arc<RemoteCursorStore>,
    shutdown: Arc<AtomicBool>,
    telemetry: SharedClientTelemetry,
    interpolate: bool,
) -> Result<()> {
    use cocoa::{
        appkit::{
            NSApp, NSApplication, NSApplicationActivationPolicyRegular, NSBackingStoreBuffered,
            NSView, NSWindow, NSWindowStyleMask,
        },
        base::{nil, NO, YES},
        foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString},
    };
    use objc::rc::autoreleasepool;
    use std::time::Duration;

    unsafe {
        info!("macOS renderer startup: creating autorelease pool");
        let app_pool = NSAutoreleasePool::new(nil);
        info!("macOS renderer startup: acquiring NSApplication");
        let app = NSApp();
        info!("macOS renderer startup: configuring NSApplication");
        app.setActivationPolicy_(NSApplicationActivationPolicyRegular);
        info!("macOS renderer startup: finishing launch");
        app.finishLaunching();

        info!(
            "macOS renderer startup: creating window for initial size {}x{}",
            initial_width, initial_height
        );
        let window = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
            NSRect::new(
                NSPoint::new(0., 0.),
                NSSize::new(initial_width as f64, initial_height as f64),
            ),
            NSWindowStyleMask::NSTitledWindowMask
                | NSWindowStyleMask::NSClosableWindowMask
                | NSWindowStyleMask::NSMiniaturizableWindowMask
                | NSWindowStyleMask::NSResizableWindowMask,
            NSBackingStoreBuffered,
            NO,
        );
        window.center();
        window.setReleasedWhenClosed_(NO);
        window.setTitle_(NSString::alloc(nil).init_str("Yang"));

        info!("macOS renderer startup: creating Metal renderer state");
        let content_view = window.contentView();
        let mut renderer = RendererState::new(interpolate, initial_width, initial_height)?;
        info!("macOS renderer startup: attaching CAMetalLayer");
        content_view.setWantsLayer(YES);
        content_view.setLayer(<*mut _>::cast(renderer.owned_layer.as_mut().unwrap().as_mut()));
        sync_layer_frame(content_view, renderer.layer());
        info!("macOS renderer startup: sizing window and layer");
        resize_window_and_layer(
            window,
            content_view,
            renderer.layer(),
            initial_width,
            initial_height,
        );

        info!("macOS renderer startup: showing window");
        window.makeKeyAndOrderFront_(nil);
        info!("macOS renderer startup: activating application");
        app.activateIgnoringOtherApps_(YES);
        info!("macOS renderer startup: entering render loop");

        let mut current_size = (initial_width, initial_height);
        let mut first_frame_logged = false;
        let mut queued_frame = None;
        let mut render_stats = RenderStats::new();
        // Previous decoded frame retained for optical-flow interpolation.
        let mut prev_frame: Option<RenderFrame> = None;

        loop {
            pump_app_events(app);
            sync_layer_frame(content_view, renderer.layer());

            if shutdown.load(Ordering::Relaxed) || window.isVisible() != YES {
                break;
            }

            let mut disconnected = false;
            let mut dropped_frames = 0u32;

            let frame = match queued_frame.take() {
                Some(frame) => frame,
                None => match render_rx.recv_timeout(Duration::from_millis(8)) {
                    Ok(frame) => frame,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                },
            };

            loop {
                match render_rx.try_recv() {
                    Ok(frame) => {
                        if queued_frame.replace(frame).is_some() {
                            dropped_frames = dropped_frames.saturating_add(1);
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if !first_frame_logged {
                info!(
                    "Metal renderer received first frame seq={} {}x{}",
                    frame.frame_seq, frame.width, frame.height
                );
                first_frame_logged = true;
            }
            if current_size != (frame.width, frame.height) {
                current_size = (frame.width, frame.height);
                resize_window_and_layer(
                    window,
                    content_view,
                    renderer.layer(),
                    frame.width,
                    frame.height,
                );
                // Discard the previous frame so we never interpolate across a
                // resolution boundary, then resize the interpolator's GPU textures.
                prev_frame = None;
                if let Some(ref mut interp) = renderer.interpolator {
                    if let Err(e) =
                        interp.resize(&renderer.device, frame.width as u64, frame.height as u64)
                    {
                        warn!("interpolator resize failed: {e:#}");
                    }
                }
            }

            // ── GPU optical-flow interpolation ────────────────────────────────
            // If interpolation is enabled and we have a previous frame, run the
            // motion-compensated warp pipeline and present the synthesised
            // mid-frame before the real decoded frame, doubling perceived frame
            // rate without ghosting artefacts.
            //
            // Suppressed on keyframes: IDR boundaries are decoder-recovery or
            // scene-change points; interpolating across them produces artefacts.
            if renderer.interpolator.is_some() {
                if let Some(ref prev) = prev_frame {
                    if !frame.is_keyframe {
                        let pixel_format = frame.pixel_buffer.get_pixel_format();
                        let full_range =
                            pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
                        autoreleasepool(|| {
                            if let Err(err) = present_interpolated_frame(
                                &mut renderer,
                                &prev.pixel_buffer,
                                &frame.pixel_buffer,
                                full_range,
                            ) {
                                warn!(
                                    "present interpolated frame before seq={} failed: {err:#}",
                                    frame.frame_seq
                                );
                            }
                        });
                    }
                }
            }

            // ── Real decoded frame ────────────────────────────────────────────
            autoreleasepool(|| {
                let present_started_at_us = now_local_us();
                if let Err(err) = present_frame(&mut renderer, &frame, &cursor_store) {
                    warn!("present frame {} failed: {err:#}", frame.frame_seq);
                } else {
                    let present_finished_at_us = now_local_us();
                    let present_cpu_us = duration_to_u32_us(std::time::Duration::from_micros(
                        present_finished_at_us.saturating_sub(present_started_at_us),
                    ));
                    let render_queue_us =
                        present_started_at_us.saturating_sub(frame.decoded_at_us) as u32;
                    telemetry.record_render(
                        frame
                            .decode_submitted_at_us
                            .saturating_sub(frame.received_at_us) as u32,
                        render_queue_us,
                        dropped_frames,
                    );
                    render_stats.record_presented(
                        &frame,
                        render_queue_us,
                        present_cpu_us,
                        dropped_frames,
                    );
                }
            });
            render_stats.maybe_log();

            // Retain the current frame for the next iteration's interpolation.
            if renderer.interpolator.is_some() {
                prev_frame = Some(frame);
            }

            if disconnected && queued_frame.is_none() {
                break;
            }
        }

        window.close();
        app_pool.drain();
    }

    Ok(())
}

#[cfg(target_os = "macos")]
impl RendererState {
    fn new(interpolate: bool, initial_width: u32, initial_height: u32) -> Result<Self> {
        let device = Device::system_default().context("create Metal device")?;
        let command_queue = device.new_command_queue();
        let pipeline_state = build_pipeline_state(&device)?;
        let texture_cache = CVMetalTextureCache::new(None, device.clone(), None)
            .map_err(|status| anyhow!("create CVMetalTextureCache failed: {status}"))?;
        let null_color_texture =
            create_u8_texture(&device, MTLPixelFormat::RGBA8Uint, 1, 1, &[0, 0, 0, 0], 4)?;
        let null_mono_texture = create_u8_texture(&device, MTLPixelFormat::R8Uint, 1, 1, &[0], 1)?;

        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_display_sync_enabled(true);
        layer.set_presents_with_transaction(false);
        // With interpolation enabled, two drawables are submitted per decoded
        // frame (interpolated + real), so the queue must be deep enough that
        // the second next_drawable() call never blocks.  Three slots suffice.
        layer.set_maximum_drawable_count(if interpolate { 3 } else { 2 });
        layer.set_opaque(true);
        layer.set_framebuffer_only(true);
        layer.remove_all_animations();

        let interpolator = if interpolate {
            match crate::render::interpolator::FrameInterpolator::new(
                &device,
                initial_width as u64,
                initial_height as u64,
            ) {
                Ok(i) => Some(i),
                Err(e) => {
                    warn!(
                        "GPU optical-flow interpolator init failed, interpolation disabled: {e:#}"
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            device,
            owned_layer: Some(layer),
            ext_layer_ptr: std::ptr::null_mut(),
            command_queue,
            pipeline_state,
            texture_cache,
            null_color_texture,
            null_mono_texture,
            color_cursor_texture: None,
            mono_cursor_texture: None,
            current_cursor_generation: None,
            interpolator,
        })
    }

    /// Returns a reference to the active `CAMetalLayer`, whether owned or borrowed.
    fn layer(&self) -> &MetalLayerRef {
        if let Some(ref l) = self.owned_layer {
            l.as_ref()
        } else {
            unsafe { MetalLayerRef::from_ptr(self.ext_layer_ptr as *mut _) }
        }
    }

    /// Construct a renderer that renders into a `CAMetalLayer` owned by Swift.
    ///
    /// Swift creates and attaches the layer to a view; Rust configures it here
    /// (pixel format, device, drawable count) and renders into it from a
    /// background thread.  The caller is responsible for keeping the layer alive
    /// until the render loop stops.
    fn new_ffi(
        ext_layer_ptr: *mut std::ffi::c_void,
        interpolate: bool,
        initial_width: u32,
        initial_height: u32,
    ) -> Result<Self> {
        use cocoa::foundation::NSSize;

        let device = Device::system_default().context("create Metal device")?;
        let command_queue = device.new_command_queue();
        let pipeline_state = build_pipeline_state(&device)?;
        let texture_cache = CVMetalTextureCache::new(None, device.clone(), None)
            .map_err(|status| anyhow!("create CVMetalTextureCache failed: {status}"))?;
        let null_color_texture =
            create_u8_texture(&device, MTLPixelFormat::RGBA8Uint, 1, 1, &[0, 0, 0, 0], 4)?;
        let null_mono_texture =
            create_u8_texture(&device, MTLPixelFormat::R8Uint, 1, 1, &[0], 1)?;

        // Configure the CAMetalLayer that Swift handed us.
        unsafe {
            let layer_ref: &MetalLayerRef = MetalLayerRef::from_ptr(ext_layer_ptr as *mut _);
            layer_ref.set_device(&device);
            layer_ref.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            layer_ref.set_display_sync_enabled(true);
            layer_ref.set_presents_with_transaction(false);
            layer_ref.set_maximum_drawable_count(if interpolate { 3 } else { 2 });
            layer_ref.set_opaque(true);
            layer_ref.set_framebuffer_only(true);
            layer_ref.remove_all_animations();
            let _: () = msg_send![
                ext_layer_ptr as *mut objc::runtime::Object,
                setDrawableSize: NSSize::new(initial_width as f64, initial_height as f64)
            ];
        }

        let interpolator = if interpolate {
            match crate::render::interpolator::FrameInterpolator::new(
                &device,
                initial_width as u64,
                initial_height as u64,
            ) {
                Ok(i) => Some(i),
                Err(e) => {
                    warn!(
                        "GPU optical-flow interpolator init failed, interpolation disabled: {e:#}"
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            device,
            owned_layer: None,
            ext_layer_ptr,
            command_queue,
            pipeline_state,
            texture_cache,
            null_color_texture,
            null_mono_texture,
            color_cursor_texture: None,
            mono_cursor_texture: None,
            current_cursor_generation: None,
            interpolator,
        })
    }
}

#[cfg(target_os = "macos")]
fn build_pipeline_state(device: &DeviceRef) -> Result<RenderPipelineState> {
    let library = device
        .new_library_with_source(VIDEO_SHADER, &CompileOptions::new())
        .map_err(|err| anyhow!("compile Metal shader library: {err}"))?;
    let vertex = library
        .get_function("video_vertex", None)
        .map_err(|err| anyhow!("load video_vertex: {err}"))?;
    let fragment = library
        .get_function("video_fragment", None)
        .map_err(|err| anyhow!("load video_fragment: {err}"))?;

    let descriptor = RenderPipelineDescriptor::new();
    descriptor.set_vertex_function(Some(&vertex));
    descriptor.set_fragment_function(Some(&fragment));
    descriptor
        .color_attachments()
        .object_at(0)
        .context("missing Metal color attachment 0")?
        .set_pixel_format(MTLPixelFormat::BGRA8Unorm);

    device
        .new_render_pipeline_state(&descriptor)
        .map_err(|err| anyhow!("create Metal render pipeline: {err}"))
}

/// Present a motion-compensated interpolated frame synthesised by the GPU
/// optical-flow pipeline in [`crate::render::interpolator::FrameInterpolator`].
///
/// Wraps both NV12 pixel buffers in `CVMetalTexture` objects and passes the
/// raw `MTLTexture` references directly to the interpolator, which encodes
/// all four GPU passes (downsample, block-match, warp, blit) into a single
/// command buffer.  The same Y-axis convention as `present_frame` is used.
#[cfg(target_os = "macos")]
fn present_interpolated_frame(
    state: &mut RendererState,
    prev_pixel_buffer: &CVPixelBuffer,
    curr_pixel_buffer: &CVPixelBuffer,
    full_range: bool,
) -> Result<()> {
    let Some(ref interpolator) = state.interpolator else {
        return Ok(());
    };

    // Wrap both NV12 pixel buffers in CVMetalTextures (zero-copy IOSurface path).
    let prev_y_cv = create_cv_metal_texture(
        &state.texture_cache,
        prev_pixel_buffer,
        MTLPixelFormat::R8Unorm,
        prev_pixel_buffer.get_width_of_plane(0),
        prev_pixel_buffer.get_height_of_plane(0),
        0,
    )?;
    let prev_uv_cv = create_cv_metal_texture(
        &state.texture_cache,
        prev_pixel_buffer,
        MTLPixelFormat::RG8Unorm,
        prev_pixel_buffer.get_width_of_plane(1),
        prev_pixel_buffer.get_height_of_plane(1),
        1,
    )?;
    let curr_y_cv = create_cv_metal_texture(
        &state.texture_cache,
        curr_pixel_buffer,
        MTLPixelFormat::R8Unorm,
        curr_pixel_buffer.get_width_of_plane(0),
        curr_pixel_buffer.get_height_of_plane(0),
        0,
    )?;
    let curr_uv_cv = create_cv_metal_texture(
        &state.texture_cache,
        curr_pixel_buffer,
        MTLPixelFormat::RG8Unorm,
        curr_pixel_buffer.get_width_of_plane(1),
        curr_pixel_buffer.get_height_of_plane(1),
        1,
    )?;

    // Borrow the underlying MTLTextures without taking ownership (same pattern
    // as present_frame — see that function for the retain/release rationale).
    let prev_y_raw = unsafe { CVMetalTextureGetTexture(prev_y_cv.as_concrete_TypeRef()) };
    if prev_y_raw.is_null() {
        bail!("CVMetalTexture (prev Y) carries no MTLTexture");
    }
    let prev_uv_raw = unsafe { CVMetalTextureGetTexture(prev_uv_cv.as_concrete_TypeRef()) };
    if prev_uv_raw.is_null() {
        bail!("CVMetalTexture (prev UV) carries no MTLTexture");
    }
    let curr_y_raw = unsafe { CVMetalTextureGetTexture(curr_y_cv.as_concrete_TypeRef()) };
    if curr_y_raw.is_null() {
        bail!("CVMetalTexture (curr Y) carries no MTLTexture");
    }
    let curr_uv_raw = unsafe { CVMetalTextureGetTexture(curr_uv_cv.as_concrete_TypeRef()) };
    if curr_uv_raw.is_null() {
        bail!("CVMetalTexture (curr UV) carries no MTLTexture");
    }

    let prev_y: &TextureRef = unsafe { TextureRef::from_ptr(prev_y_raw) };
    let prev_uv: &TextureRef = unsafe { TextureRef::from_ptr(prev_uv_raw) };
    let curr_y: &TextureRef = unsafe { TextureRef::from_ptr(curr_y_raw) };
    let curr_uv: &TextureRef = unsafe { TextureRef::from_ptr(curr_uv_raw) };

    // CVMetalTextureIsFlipped() returns false for VideoToolbox NV12 frames.
    // The vertex convention in fullscreen_vertices treats `flipped=true` as
    // "compensate for bottom-origin data", so we negate — matching present_frame.
    let flipped = !prev_y_cv.is_flipped();

    let Some(drawable) = state.layer().next_drawable() else {
        return Ok(());
    };

    let command_buffer = state.command_queue.new_command_buffer();

    interpolator.encode(
        command_buffer,
        drawable.texture(),
        prev_y,
        prev_uv,
        curr_y,
        curr_uv,
        full_range,
        flipped,
    )?;

    command_buffer.present_drawable(drawable);
    command_buffer.commit();

    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn pump_app_events(app: cocoa::base::id) {
    use cocoa::{
        appkit::{NSApplication, NSEventMask},
        base::{nil, YES},
        foundation::{NSDate, NSDefaultRunLoopMode},
    };

    loop {
        let event = app.nextEventMatchingMask_untilDate_inMode_dequeue_(
            NSEventMask::NSAnyEventMask.bits(),
            NSDate::distantPast(nil),
            NSDefaultRunLoopMode,
            YES,
        );
        if event == nil {
            break;
        }
        app.sendEvent_(event);
    }
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
unsafe fn sync_layer_frame(content_view: cocoa::base::id, layer: &MetalLayerRef) {
    use cocoa::foundation::NSRect;

    let bounds: NSRect = msg_send![content_view, bounds];
    let _: () = msg_send![layer, setFrame: bounds];
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
unsafe fn resize_window_and_layer(
    window: cocoa::base::id,
    content_view: cocoa::base::id,
    layer: &MetalLayerRef,
    width_px: u32,
    height_px: u32,
) {
    use cocoa::{
        appkit::{CGFloat, NSWindow},
        foundation::NSSize,
    };

    let scale: CGFloat = msg_send![window, backingScaleFactor];
    let scale = if scale > 0.0 { scale as f64 } else { 1.0 };
    let content_size = NSSize::new(width_px as f64 / scale, height_px as f64 / scale);
    let drawable_size = NSSize::new(width_px as f64, height_px as f64);

    window.setContentAspectRatio_(content_size);
    window.setContentSize_(content_size);
    let _: () = msg_send![layer, setContentsScale: scale];
    let _: () = msg_send![layer, setDrawableSize: drawable_size];
    sync_layer_frame(content_view, layer);
}

#[cfg(target_os = "macos")]
fn present_frame(
    state: &mut RendererState,
    frame: &RenderFrame,
    cursor_store: &RemoteCursorStore,
) -> Result<()> {
    let pixel_buffer = &frame.pixel_buffer;
    let pixel_format = pixel_buffer.get_pixel_format();
    let full_range = if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
        false
    } else if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
        true
    } else {
        bail!("unsupported pixel format for Metal presenter: {pixel_format:#x}");
    };

    let y_cv_texture = create_cv_metal_texture(
        &state.texture_cache,
        pixel_buffer,
        MTLPixelFormat::R8Unorm,
        pixel_buffer.get_width_of_plane(0),
        pixel_buffer.get_height_of_plane(0),
        0,
    )?;
    let uv_cv_texture = create_cv_metal_texture(
        &state.texture_cache,
        pixel_buffer,
        MTLPixelFormat::RG8Unorm,
        pixel_buffer.get_width_of_plane(1),
        pixel_buffer.get_height_of_plane(1),
        1,
    )?;

    // `CVMetalTextureGetTexture` returns a non-retained pointer valid for the
    // lifetime of the CVMetalTexture. The `get_texture()` helper wraps it in an
    // owned `Texture` via `Texture::from_ptr`, which assumes a +1 retain that
    // was never performed, so it over-releases the object on drop and corrupts
    // the texture cache after a few frames.  Obtain a borrowed `&TextureRef`
    // directly instead — no ownership, no retain/release.
    let y_tex_raw = unsafe { CVMetalTextureGetTexture(y_cv_texture.as_concrete_TypeRef()) };
    if y_tex_raw.is_null() {
        bail!("CVMetalTexture did not expose a luma MTLTexture");
    }
    let y_texture: &TextureRef = unsafe { TextureRef::from_ptr(y_tex_raw) };
    let uv_tex_raw = unsafe { CVMetalTextureGetTexture(uv_cv_texture.as_concrete_TypeRef()) };
    if uv_tex_raw.is_null() {
        bail!("CVMetalTexture did not expose a chroma MTLTexture");
    }
    let uv_texture: &TextureRef = unsafe { TextureRef::from_ptr(uv_tex_raw) };

    let cursor_snapshot = cursor_store.snapshot_for(frame.timestamp_us);
    let mut cursor_params = update_cursor_resources(state, cursor_snapshot.as_ref())?;
    cursor_params.frame_size = [frame.width, frame.height];
    let color_cursor_texture = state
        .color_cursor_texture
        .as_ref()
        .unwrap_or(&state.null_color_texture);
    let mono_cursor_texture = state
        .mono_cursor_texture
        .as_ref()
        .unwrap_or(&state.null_mono_texture);

    let Some(drawable) = state.layer().next_drawable() else {
        return Ok(());
    };

    let pass_descriptor = RenderPassDescriptor::new();
    let color_attachment = pass_descriptor
        .color_attachments()
        .object_at(0)
        .context("missing Metal color attachment 0")?;
    color_attachment.set_texture(Some(drawable.texture()));
    color_attachment.set_load_action(MTLLoadAction::Clear);
    color_attachment.set_store_action(MTLStoreAction::Store);
    color_attachment.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 1.0));

    let command_buffer = state.command_queue.new_command_buffer();
    let encoder = command_buffer.new_render_command_encoder(pass_descriptor);
    // CVMetalTextureIsFlipped() returns false for VideoToolbox IOSurface-backed
    // frames on macOS — but the vertex tex-coord mapping in fullscreen_vertices
    // treats `flipped=true` as "compensate for flipped data", so we must negate.
    let vertices = fullscreen_vertices(!y_cv_texture.is_flipped());
    let conversion = ColorConversionParams {
        full_range: full_range as u32,
    };

    encoder.set_render_pipeline_state(&state.pipeline_state);
    encoder.set_vertex_bytes(
        0,
        std::mem::size_of_val(&vertices) as u64,
        vertices.as_ptr().cast(),
    );
    encoder.set_fragment_texture(0, Some(y_texture));
    encoder.set_fragment_texture(1, Some(uv_texture));
    encoder.set_fragment_texture(2, Some(color_cursor_texture));
    encoder.set_fragment_texture(3, Some(mono_cursor_texture));
    encoder.set_fragment_bytes(
        0,
        std::mem::size_of::<ColorConversionParams>() as u64,
        (&conversion as *const ColorConversionParams).cast(),
    );
    encoder.set_fragment_bytes(
        1,
        std::mem::size_of::<CursorRenderParams>() as u64,
        (&cursor_params as *const CursorRenderParams).cast(),
    );
    encoder.draw_primitives(MTLPrimitiveType::TriangleStrip, 0, 4);
    encoder.end_encoding();

    command_buffer.present_drawable(drawable);
    command_buffer.commit();

    Ok(())
}

#[cfg(target_os = "macos")]
fn create_cv_metal_texture(
    texture_cache: &CVMetalTextureCache,
    pixel_buffer: &CVPixelBuffer,
    pixel_format: MTLPixelFormat,
    width: usize,
    height: usize,
    plane_index: usize,
) -> Result<CVMetalTexture> {
    match texture_cache.create_texture_from_image(
        pixel_buffer.as_concrete_TypeRef(),
        None,
        pixel_format,
        width,
        height,
        plane_index,
    ) {
        Ok(texture) => Ok(texture),
        Err(first_status) => {
            texture_cache.flush(0);
            texture_cache
                .create_texture_from_image(
                    pixel_buffer.as_concrete_TypeRef(),
                    None,
                    pixel_format,
                    width,
                    height,
                    plane_index,
                )
                .map_err(|second_status| {
                    anyhow!(
                        "create CVMetalTexture failed for plane {plane_index} ({pixel_format:?}): first={first_status} second={second_status}"
                    )
                })
        }
    }
}

#[cfg(target_os = "macos")]
fn update_cursor_resources(
    state: &mut RendererState,
    snapshot: Option<&crate::cursor::CursorSnapshot>,
) -> Result<CursorRenderParams> {
    let Some(snapshot) = snapshot else {
        state.color_cursor_texture = None;
        state.mono_cursor_texture = None;
        state.current_cursor_generation = None;
        return Ok(CursorRenderParams::default());
    };

    let Some(shape) = snapshot.shape.as_deref() else {
        state.color_cursor_texture = None;
        state.mono_cursor_texture = None;
        state.current_cursor_generation = None;
        return Ok(cursor_params_from_snapshot(snapshot, false));
    };

    if state.current_cursor_generation != Some(shape.generation) {
        state.color_cursor_texture = None;
        state.mono_cursor_texture = None;

        match shape.kind {
            RemoteCursorShapeKind::Color | RemoteCursorShapeKind::MaskedColor => {
                if shape.width > 0 && shape.height > 0 && !shape.data.is_empty() {
                    state.color_cursor_texture = Some(create_u8_texture(
                        &state.device,
                        MTLPixelFormat::RGBA8Uint,
                        shape.width,
                        shape.height,
                        &shape.data,
                        shape.pitch,
                    )?);
                }
            }
            RemoteCursorShapeKind::Monochrome => {
                if shape.width > 0 && shape.height > 0 && !shape.data.is_empty() {
                    state.mono_cursor_texture = Some(create_u8_texture(
                        &state.device,
                        MTLPixelFormat::R8Uint,
                        shape.pitch,
                        shape.height.saturating_mul(2),
                        &shape.data,
                        shape.pitch,
                    )?);
                }
            }
        }

        state.current_cursor_generation = Some(shape.generation);
    }

    Ok(cursor_params_from_snapshot(
        snapshot,
        snapshot.state.visible && snapshot.shape.is_some(),
    ))
}

#[cfg(target_os = "macos")]
fn create_u8_texture(
    device: &DeviceRef,
    pixel_format: MTLPixelFormat,
    width: u32,
    height: u32,
    data: &[u8],
    bytes_per_row: u32,
) -> Result<Texture> {
    let descriptor = TextureDescriptor::new();
    descriptor.set_texture_type(MTLTextureType::D2);
    descriptor.set_pixel_format(pixel_format);
    descriptor.set_width(width as u64);
    descriptor.set_height(height as u64);
    descriptor.set_storage_mode(MTLStorageMode::Shared);
    descriptor.set_usage(MTLTextureUsage::ShaderRead);
    let texture = device.new_texture(&descriptor);
    texture.replace_region(
        MTLRegion::new_2d(0, 0, width as u64, height as u64),
        0,
        data.as_ptr().cast(),
        bytes_per_row as u64,
    );
    Ok(texture)
}

#[cfg(target_os = "macos")]
fn cursor_params_from_snapshot(
    snapshot: &crate::cursor::CursorSnapshot,
    has_shape: bool,
) -> CursorRenderParams {
    let (cursor_size, cursor_row_bytes, cursor_kind) = snapshot
        .shape
        .as_deref()
        .map(|shape| {
            (
                [shape.width, shape.height],
                shape.pitch,
                cursor_kind_value(shape),
            )
        })
        .unwrap_or(([0, 0], 0, 0));

    CursorRenderParams {
        cursor_origin: [snapshot.state.x, snapshot.state.y],
        cursor_size,
        cursor_row_bytes,
        cursor_kind,
        cursor_visible: snapshot.state.visible as u32,
        cursor_has_shape: has_shape as u32,
        frame_size: [0, 0],
        _padding: [0; 2],
    }
}

#[cfg(target_os = "macos")]
fn cursor_kind_value(shape: &RemoteCursorShape) -> u32 {
    match shape.kind {
        RemoteCursorShapeKind::Color => 1,
        RemoteCursorShapeKind::MaskedColor => 2,
        RemoteCursorShapeKind::Monochrome => 3,
    }
}

#[cfg(target_os = "macos")]
fn now_local_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(target_os = "macos")]
fn duration_to_u32_us(duration: std::time::Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

#[cfg(target_os = "macos")]
fn fullscreen_vertices(flipped: bool) -> [Vertex; 4] {
    let (top_v, bottom_v) = if flipped { (1.0, 0.0) } else { (0.0, 1.0) };
    [
        Vertex {
            position: [-1.0, -1.0],
            tex_coord: [0.0, bottom_v],
        },
        Vertex {
            position: [1.0, -1.0],
            tex_coord: [1.0, bottom_v],
        },
        Vertex {
            position: [-1.0, 1.0],
            tex_coord: [0.0, top_v],
        },
        Vertex {
            position: [1.0, 1.0],
            tex_coord: [1.0, top_v],
        },
    ]
}

// ---------------------------------------------------------------------------
// FFI render path — used by the macOS Swift GUI app
// ---------------------------------------------------------------------------

/// Render loop for the Swift GUI app.
///
/// Unlike [`render_loop_macos`], this function:
/// - Runs on any thread — Swift owns the window/view; Rust only renders into
///   the provided `CAMetalLayer`.
/// - Does **not** create an `NSWindow`, call `pump_app_events`, or resize a
///   window.  The Swift view handles layout.
/// - Updates the layer's `drawableSize` when the video stream resolution changes.
///
/// Returns when `shutdown` is set or the `render_rx` sender drops.
#[cfg(target_os = "macos")]
pub(crate) fn render_loop_macos_ffi(
    ext_layer_ptr: *mut std::ffi::c_void,
    render_rx: crossbeam_channel::Receiver<crate::decode::videotoolbox::RenderFrame>,
    initial_width: u32,
    initial_height: u32,
    cursor_store: std::sync::Arc<crate::cursor::RemoteCursorStore>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    telemetry: crate::telemetry::SharedClientTelemetry,
    interpolate: bool,
) {
    use core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    use objc::rc::autoreleasepool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut renderer =
        match RendererState::new_ffi(ext_layer_ptr, interpolate, initial_width, initial_height) {
            Ok(r) => r,
            Err(e) => {
                warn!("FFI renderer init failed: {e:#}");
                return;
            }
        };

    info!(
        "FFI Metal renderer started ({}×{}, interpolate={})",
        initial_width, initial_height, interpolate
    );

    let mut current_size = (initial_width, initial_height);
    let mut first_frame_logged = false;
    let mut queued_frame: Option<crate::decode::videotoolbox::RenderFrame> = None;
    let mut render_stats = RenderStats::new();
    let mut prev_frame: Option<crate::decode::videotoolbox::RenderFrame> = None;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let mut disconnected = false;
        let mut dropped_frames = 0u32;

        let frame = match queued_frame.take() {
            Some(f) => f,
            None => match render_rx.recv_timeout(Duration::from_millis(8)) {
                Ok(f) => f,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            },
        };

        loop {
            match render_rx.try_recv() {
                Ok(f) => {
                    if queued_frame.replace(f).is_some() {
                        dropped_frames = dropped_frames.saturating_add(1);
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if !first_frame_logged {
            info!(
                "FFI Metal renderer received first frame seq={} {}×{}",
                frame.frame_seq, frame.width, frame.height
            );
            first_frame_logged = true;
        }

        if current_size != (frame.width, frame.height) {
            current_size = (frame.width, frame.height);
            unsafe {
                let size = cocoa::foundation::NSSize::new(frame.width as f64, frame.height as f64);
                let _: () = msg_send![
                    ext_layer_ptr as *mut objc::runtime::Object,
                    setDrawableSize: size
                ];
            }
            prev_frame = None;
            if let Some(ref mut interp) = renderer.interpolator {
                if let Err(e) =
                    interp.resize(&renderer.device, frame.width as u64, frame.height as u64)
                {
                    warn!("interpolator resize failed: {e:#}");
                }
            }
        }

        if renderer.interpolator.is_some() {
            if let Some(ref prev) = prev_frame {
                if !frame.is_keyframe {
                    let full_range = frame.pixel_buffer.get_pixel_format()
                        == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
                    autoreleasepool(|| {
                        if let Err(e) = present_interpolated_frame(
                            &mut renderer,
                            &prev.pixel_buffer,
                            &frame.pixel_buffer,
                            full_range,
                        ) {
                            warn!(
                                "present interpolated frame before seq={} failed: {e:#}",
                                frame.frame_seq
                            );
                        }
                    });
                }
            }
        }

        autoreleasepool(|| {
            let present_started_at_us = now_local_us();
            if let Err(e) = present_frame(&mut renderer, &frame, &cursor_store) {
                warn!("present frame {} failed: {e:#}", frame.frame_seq);
            } else {
                let present_finished_at_us = now_local_us();
                let present_cpu_us = duration_to_u32_us(std::time::Duration::from_micros(
                    present_finished_at_us.saturating_sub(present_started_at_us),
                ));
                let render_queue_us =
                    present_started_at_us.saturating_sub(frame.decoded_at_us) as u32;
                telemetry.record_render(
                    frame
                        .decode_submitted_at_us
                        .saturating_sub(frame.received_at_us) as u32,
                    render_queue_us,
                    dropped_frames,
                );
                render_stats.record_presented(&frame, render_queue_us, present_cpu_us, dropped_frames);
            }
        });
        render_stats.maybe_log();

        if renderer.interpolator.is_some() {
            prev_frame = Some(frame);
        }

        if disconnected && queued_frame.is_none() {
            break;
        }
    }

    info!("FFI Metal renderer stopped");
}
