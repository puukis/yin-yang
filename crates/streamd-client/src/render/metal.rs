//! macOS frame presenter.

use anyhow::{bail, Result};
use crossbeam_channel::Receiver;
use std::sync::{atomic::AtomicBool, Arc};

use crate::decode::videotoolbox::RenderFrame;

#[cfg(target_os = "macos")]
use anyhow::{anyhow, Context};
#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_video::{
    metal_texture::CVMetalTexture,
    metal_texture_cache::CVMetalTextureCache,
    pixel_buffer::{
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    },
};
#[cfg(target_os = "macos")]
use metal::{
    CommandQueue, CompileOptions, Device, DeviceRef, MTLClearColor, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLStoreAction, MetalLayer, MetalLayerRef, RenderPassDescriptor,
    RenderPipelineDescriptor, RenderPipelineState,
};
#[cfg(target_os = "macos")]
use objc::{msg_send, sel, sel_impl};
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "macos")]
use tracing::warn;

pub struct VideoRenderer;

impl VideoRenderer {
    pub fn run(
        render_rx: Receiver<RenderFrame>,
        initial_width: u32,
        initial_height: u32,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return render_loop_macos(render_rx, initial_width, initial_height, shutdown);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (render_rx, initial_width, initial_height, shutdown);
            bail!("streamd-client video presentation is only supported on macOS");
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
struct RendererState {
    layer: MetalLayer,
    command_queue: CommandQueue,
    pipeline_state: RenderPipelineState,
    texture_cache: CVMetalTextureCache,
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

vertex VertexOut video_vertex(
    const device VertexIn* vertices [[buffer(0)]],
    uint vertex_id [[vertex_id]]
) {
    VertexOut out;
    const VertexIn vertex = vertices[vertex_id];
    out.position = float4(vertex.position, 0.0, 1.0);
    out.tex_coord = vertex.tex_coord;
    return out;
}

fragment float4 video_fragment(
    VertexOut in [[stage_in]],
    texture2d<float> luma_tex [[texture(0)]],
    texture2d<float> chroma_tex [[texture(1)]],
    constant ColorConversionParams& params [[buffer(0)]]
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

    return float4(clamp(float3(r, g, b), 0.0, 1.0), 1.0);
}
"#;

#[cfg(target_os = "macos")]
fn render_loop_macos(
    render_rx: Receiver<RenderFrame>,
    initial_width: u32,
    initial_height: u32,
    shutdown: Arc<AtomicBool>,
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
    use std::{thread, time::Duration};

    unsafe {
        let app_pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyRegular);
        app.finishLaunching();

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
        window.setTitle_(NSString::alloc(nil).init_str("streamd"));

        let content_view = window.contentView();
        let mut renderer = RendererState::new()?;
        content_view.setWantsLayer(YES);
        content_view.setLayer(<*mut _>::cast(renderer.layer.as_mut()));
        sync_layer_frame(content_view, renderer.layer.as_ref());
        resize_window_and_layer(
            window,
            content_view,
            renderer.layer.as_ref(),
            initial_width,
            initial_height,
        );

        window.makeKeyAndOrderFront_(nil);
        app.activateIgnoringOtherApps_(YES);

        let mut current_size = (initial_width, initial_height);

        loop {
            pump_app_events(app);
            sync_layer_frame(content_view, renderer.layer.as_ref());

            if shutdown.load(Ordering::Relaxed) || window.isVisible() != YES {
                break;
            }

            let mut latest_frame = None;
            let mut disconnected = false;
            loop {
                match render_rx.try_recv() {
                    Ok(frame) => latest_frame = Some(frame),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if let Some(frame) = latest_frame {
                if current_size != (frame.width, frame.height) {
                    current_size = (frame.width, frame.height);
                    resize_window_and_layer(
                        window,
                        content_view,
                        renderer.layer.as_ref(),
                        frame.width,
                        frame.height,
                    );
                }

                autoreleasepool(|| {
                    if let Err(err) = present_frame(&mut renderer, &frame) {
                        warn!("present frame {} failed: {err:#}", frame.frame_seq);
                    }
                });
            } else if disconnected {
                break;
            }

            thread::sleep(Duration::from_millis(4));
        }

        window.close();
        app_pool.drain();
    }

    Ok(())
}

#[cfg(target_os = "macos")]
impl RendererState {
    fn new() -> Result<Self> {
        let device = Device::system_default().context("create Metal device")?;
        let command_queue = device.new_command_queue();
        let pipeline_state = build_pipeline_state(&device)?;
        let texture_cache = CVMetalTextureCache::new(None, device.clone(), None)
            .map_err(|status| anyhow!("create CVMetalTextureCache failed: {status}"))?;

        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_opaque(true);
        layer.set_framebuffer_only(true);
        layer.remove_all_animations();

        Ok(Self {
            layer,
            command_queue,
            pipeline_state,
            texture_cache,
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
unsafe fn sync_layer_frame(content_view: cocoa::base::id, layer: &MetalLayerRef) {
    use cocoa::foundation::NSRect;

    let bounds: NSRect = msg_send![content_view, bounds];
    let _: () = msg_send![layer, setFrame: bounds];
}

#[cfg(target_os = "macos")]
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
fn present_frame(state: &mut RendererState, frame: &RenderFrame) -> Result<()> {
    let pixel_buffer = &frame.pixel_buffer;
    let pixel_format = pixel_buffer.get_pixel_format();
    let full_range = match pixel_format {
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange => false,
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange => true,
        other => bail!("unsupported pixel format for Metal presenter: {other:#x}"),
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
    let y_texture = y_cv_texture
        .get_texture()
        .context("CVMetalTexture did not expose a luma MTLTexture")?;
    let uv_texture = uv_cv_texture
        .get_texture()
        .context("CVMetalTexture did not expose a chroma MTLTexture")?;

    let Some(drawable) = state.layer.next_drawable() else {
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
    let vertices = fullscreen_vertices(y_cv_texture.is_flipped());
    let conversion = ColorConversionParams {
        full_range: full_range as u32,
    };

    encoder.set_render_pipeline_state(&state.pipeline_state);
    encoder.set_vertex_bytes(
        0,
        std::mem::size_of_val(&vertices) as u64,
        vertices.as_ptr().cast(),
    );
    encoder.set_fragment_texture(0, Some(&y_texture));
    encoder.set_fragment_texture(1, Some(&uv_texture));
    encoder.set_fragment_bytes(
        0,
        std::mem::size_of::<ColorConversionParams>() as u64,
        (&conversion as *const ColorConversionParams).cast(),
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
