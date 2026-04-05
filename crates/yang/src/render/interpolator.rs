//! GPU motion-compensated frame interpolation.
//!
//! Synthesises one mid-frame between every pair of consecutive decoded frames
//! using hierarchical block-matching optical flow computed entirely in Metal
//! compute shaders. This replaces the original 50/50 cross-fade blend with a
//! true MCFI (Motion Compensated Frame Interpolation) pass:
//!
//! ```text
//! prev NV12 ──┬── downsample 4× ──┐
//!             │                   ├── block-match → flow texture
//! curr NV12 ──┴── downsample 4× ──┘        │
//!             │                             ▼
//!             └─────────────── warp + blend → RGBA output
//!                                                  │
//!                                            blit to drawable
//! ```
//!
//! # Algorithm
//!
//! 1. **Downsample** (compute): Both luma planes are averaged 4× to produce
//!    (W/4, H/4) `R8Unorm` scratch textures.
//! 2. **Block matching** (compute): The image is divided into 8×8 blocks in
//!    downsampled space (= 32×32 full-resolution pixels per block).  Each
//!    Metal threadgroup handles one block; 81 threads (9×9 search window,
//!    radius = 4 at ¼ scale = ±16 full-res pixels) each test one candidate
//!    offset via SAD.  Thread 0 picks the minimum and writes the result to a
//!    `RG32Float` flow texture.
//! 3. **Warp** (compute): For each output pixel the flow is sampled
//!    bilinearly from the block-resolution flow texture and converted to a
//!    full-resolution UV offset.  The pixel is synthesised at t = 0.5 by
//!    sampling prev at (coord − ½·flow) and curr at (coord + ½·flow) and
//!    blending 50/50, then converting YCbCr → RGB.
//! 4. **Blit** (render): The `RGBA8Unorm` output texture is copied to the
//!    `CAMetalLayer` drawable via a simple fullscreen quad.
//!
//! All four passes run in a single `MTLCommandBuffer`.  GPU time is
//! approximately 2–5 ms at 1080 p on Apple Silicon (M1 and later).

#![cfg(target_os = "macos")]

use anyhow::{anyhow, Context, Result};
use metal::{
    CommandBufferRef, CompileOptions, ComputePipelineState, DeviceRef, MTLClearColor,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLSize, MTLStorageMode, MTLStoreAction,
    MTLTextureType, MTLTextureUsage, RenderPassDescriptor, RenderPipelineDescriptor,
    RenderPipelineState, Texture, TextureDescriptor, TextureRef,
};
use tracing::info;

// ---------------------------------------------------------------------------
// Metal Shading Language sources
// ---------------------------------------------------------------------------

const COMPUTE_SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

// ─── Downsample luma 4× ─────────────────────────────────────────────────────
// One thread per output pixel.  Averages a 4×4 neighbourhood in the source
// luma plane, clamping at texture boundaries.
kernel void downsample_4x(
    texture2d<float, access::read>  src [[texture(0)]],
    texture2d<float, access::write> dst [[texture(1)]],
    uint2 gid [[thread_position_in_grid]]
) {
    if (gid.x >= dst.get_width() || gid.y >= dst.get_height()) return;
    uint2 base = gid * 4;
    float sum  = 0.0;
    uint  sw   = src.get_width();
    uint  sh   = src.get_height();
    for (int dy = 0; dy < 4; dy++)
        for (int dx = 0; dx < 4; dx++)
            sum += src.read(uint2(min(base.x + (uint)dx, sw - 1),
                                  min(base.y + (uint)dy, sh - 1))).r;
    dst.write(float4(sum * 0.0625, 0.0, 0.0, 1.0), gid);
}

// ─── Block-match optical flow ────────────────────────────────────────────────
// One Metal threadgroup per 8×8 block in the downsampled luma image.
// 81 threads per threadgroup (9×9 search window, radius = 4 in ¼-res pixels
// = ±16 full-resolution pixels).  Each thread computes the SAD for one
// candidate offset; thread 0 picks the minimum and writes the flow vector.
constexpr int BM_BLOCK  = 8;
constexpr int BM_RADIUS = 4;
constexpr int BM_SW     = 2 * BM_RADIUS + 1;  // 9
constexpr int BM_NC     = BM_SW * BM_SW;        // 81

kernel void block_match_flow(
    texture2d<float, access::read>  prev_ds [[texture(0)]],
    texture2d<float, access::read>  curr_ds [[texture(1)]],
    texture2d<float, access::write> flow    [[texture(2)]],
    uint2 group_id  [[threadgroup_position_in_grid]],
    uint  thread_id [[thread_index_in_threadgroup]]
) {
    // Defensive guard — in practice the threadgroup size is exactly BM_NC.
    if (thread_id >= (uint)BM_NC) return;

    int cand_x = (int)(thread_id % (uint)BM_SW) - BM_RADIUS;
    int cand_y = (int)(thread_id / (uint)BM_SW) - BM_RADIUS;

    int2 origin = int2(group_id) * BM_BLOCK;
    int  w      = (int)prev_ds.get_width();
    int  h      = (int)prev_ds.get_height();

    float sad = 0.0;
    for (int py = 0; py < BM_BLOCK; py++) {
        for (int px = 0; px < BM_BLOCK; px++) {
            int2 pp = origin + int2(px, py);
            int2 cp = pp + int2(cand_x, cand_y);
            uint2 ppc = uint2(clamp(pp.x, 0, w-1), clamp(pp.y, 0, h-1));
            uint2 cpc = uint2(clamp(cp.x, 0, w-1), clamp(cp.y, 0, h-1));
            sad += abs(prev_ds.read(ppc).r - curr_ds.read(cpc).r);
        }
    }

    // Deposit into threadgroup-shared storage, then thread 0 reduces.
    threadgroup float  tg_sad[BM_NC];
    threadgroup float2 tg_off[BM_NC];
    tg_sad[thread_id] = sad;
    tg_off[thread_id] = float2(cand_x, cand_y);

    threadgroup_barrier(mem_flags::mem_threadgroup);

    if (thread_id == 0) {
        float  min_sad = tg_sad[0];
        float2 best    = tg_off[0];
        for (int i = 1; i < BM_NC; i++) {
            if (tg_sad[i] < min_sad) {
                min_sad = tg_sad[i];
                best    = tg_off[i];
            }
        }
        flow.write(float4(best.x, best.y, 0.0, 1.0), group_id);
    }
}

// ─── Motion-compensated warp ─────────────────────────────────────────────────
// One thread per output pixel.  Reads the bilinearly-upsampled block-level
// flow, converts it to a full-resolution UV offset, and produces a t = 0.5
// in-between sample by blending warped prev and curr in YCbCr space before
// converting to RGB.
struct WarpParams { uint full_range; uint width; uint height; uint _pad; };

kernel void optical_flow_warp(
    texture2d<float, access::sample> prev_luma   [[texture(0)]],
    texture2d<float, access::sample> prev_chroma [[texture(1)]],
    texture2d<float, access::sample> curr_luma   [[texture(2)]],
    texture2d<float, access::sample> curr_chroma [[texture(3)]],
    texture2d<float, access::sample> flow_tex    [[texture(4)]],
    texture2d<float, access::write>  output      [[texture(5)]],
    constant WarpParams&             params      [[buffer(0)]],
    uint2 gid [[thread_position_in_grid]]
) {
    if (gid.x >= params.width || gid.y >= params.height) return;

    float2 full_sz = float2(params.width, params.height);
    // Normalised coordinate at the centre of this pixel.
    float2 base    = (float2(gid) + 0.5) / full_sz;

    // Flow is in ¼-res pixel units.  For t = 0.5:
    //   half-offset (UV) = flow × (4 / 2) / full_sz = flow × 2 / full_sz
    constexpr sampler fs(coord::normalized, address::clamp_to_edge, filter::linear);
    float2 flow    = flow_tex.sample(fs, base).rg;
    float2 half_uv = flow * 2.0 / full_sz;

    float2 prev_uv = clamp(base - half_uv, 0.0, 1.0);
    float2 curr_uv = clamp(base + half_uv, 0.0, 1.0);

    constexpr sampler s(coord::normalized, address::clamp_to_edge, filter::linear);

    float  py  = prev_luma.sample(s, prev_uv).r;
    float2 puv = prev_chroma.sample(s, prev_uv).rg;
    float  cy  = curr_luma.sample(s, curr_uv).r;
    float2 cuv = curr_chroma.sample(s, curr_uv).rg;

    float  y  = (py  + cy)  * 0.5;
    float2 uv = (puv + cuv) * 0.5 - 0.5;

    float luma = y;
    if (params.full_range == 0)
        luma = max(y - (16.0 / 255.0), 0.0) * (255.0 / 219.0);

    float r = clamp(luma + 1.59603 * uv.y,                   0.0, 1.0);
    float g = clamp(luma - 0.39176 * uv.x - 0.81297 * uv.y, 0.0, 1.0);
    float b = clamp(luma + 2.01723 * uv.x,                   0.0, 1.0);
    output.write(float4(r, g, b, 1.0), gid);
}
"#;

const PRESENT_SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexIn  { float2 position; float2 tex_coord; };
struct VertexOut { float4 position [[position]]; float2 tex_coord; };

vertex VertexOut rgb_vert(
    const device VertexIn* v [[buffer(0)]],
    uint vid [[vertex_id]]
) {
    VertexOut o;
    o.position  = float4(v[vid].position, 0.0, 1.0);
    o.tex_coord = v[vid].tex_coord;
    return o;
}

fragment float4 rgb_frag(
    VertexOut in [[stage_in]],
    texture2d<float> tex [[texture(0)]]
) {
    constexpr sampler s(coord::normalized, address::clamp_to_edge, filter::linear);
    return tex.sample(s, in.tex_coord);
}
"#;

// ---------------------------------------------------------------------------
// Rust structures
// ---------------------------------------------------------------------------

#[repr(C)]
struct WarpParams {
    full_range: u32,
    width: u32,
    height: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    tex_coord: [f32; 2],
}

// ---------------------------------------------------------------------------
// FrameInterpolator
// ---------------------------------------------------------------------------

/// GPU motion-compensated frame interpolator.
///
/// Allocates GPU-private intermediate textures on construction and on
/// [`resize`](Self::resize).  All GPU work is encoded into the caller-provided
/// [`CommandBufferRef`]; no CPU readback occurs.
pub struct FrameInterpolator {
    downsample_pipeline: ComputePipelineState,
    block_match_pipeline: ComputePipelineState,
    warp_pipeline: ComputePipelineState,
    present_pipeline: RenderPipelineState,

    /// R8Unorm luma scratch at (W/4, H/4).
    prev_ds: Texture,
    curr_ds: Texture,
    /// RG32Float block-level flow at (ceil(W/32), ceil(H/32)).
    flow_tex: Texture,
    /// RGBA8Unorm full-resolution synthesised mid-frame.
    output: Texture,

    width: u64,
    height: u64,
}

impl FrameInterpolator {
    /// Create a new interpolator for frames of `width × height` pixels.
    ///
    /// Compiles all Metal pipelines and allocates GPU-private textures.
    /// Returns an error if shader compilation or texture allocation fails.
    pub fn new(device: &DeviceRef, width: u64, height: u64) -> Result<Self> {
        let (downsample_pipeline, block_match_pipeline, warp_pipeline) =
            build_compute_pipelines(device)?;
        let present_pipeline = build_present_pipeline(device)?;

        let (prev_ds, curr_ds, flow_tex, output) = alloc_textures(device, width, height)?;

        info!(
            "FrameInterpolator: GPU optical-flow interpolation initialised ({}×{})",
            width, height
        );

        Ok(Self {
            downsample_pipeline,
            block_match_pipeline,
            warp_pipeline,
            present_pipeline,
            prev_ds,
            curr_ds,
            flow_tex,
            output,
            width,
            height,
        })
    }

    /// Reallocate the intermediate textures for a new resolution.
    ///
    /// A no-op when the dimensions are unchanged.
    pub fn resize(&mut self, device: &DeviceRef, width: u64, height: u64) -> Result<()> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        let (prev_ds, curr_ds, flow_tex, output) = alloc_textures(device, width, height)?;
        self.prev_ds = prev_ds;
        self.curr_ds = curr_ds;
        self.flow_tex = flow_tex;
        self.output = output;
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Encode the full four-pass interpolation pipeline into `cmd_buf` and
    /// blit the result into `target` (a `CAMetalLayer` drawable texture).
    ///
    /// Passes run in order within the same command buffer via separate compute
    /// and render command encoders; Metal enforces sequential execution across
    /// encoder boundaries, providing the required read-after-write guarantees.
    ///
    /// `flipped` mirrors the Y-axis convention of the source NV12 textures —
    /// pass `!CVMetalTextureIsFlipped(luma_cv_texture)`, the same value used
    /// by `present_frame`'s fullscreen quad.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        cmd_buf: &CommandBufferRef,
        target: &TextureRef,
        prev_y: &TextureRef,
        prev_uv: &TextureRef,
        curr_y: &TextureRef,
        curr_uv: &TextureRef,
        full_range: bool,
        flipped: bool,
    ) -> Result<()> {
        let ds_w = self.width.div_ceil(4);
        let ds_h = self.height.div_ceil(4);

        // ── Pass 1 & 2: Downsample both luma planes 4× ─────────────────────
        // Both dispatches share the same pipeline and encoder.  They write to
        // different textures so they may run concurrently within the encoder.
        {
            let tg = MTLSize {
                width: 16,
                height: 16,
                depth: 1,
            };
            let grid = MTLSize {
                width: ds_w,
                height: ds_h,
                depth: 1,
            };

            let enc = cmd_buf.new_compute_command_encoder();
            enc.set_compute_pipeline_state(&self.downsample_pipeline);

            enc.set_texture(0, Some(prev_y));
            enc.set_texture(1, Some(&self.prev_ds));
            enc.dispatch_threads(grid, tg);

            enc.set_texture(0, Some(curr_y));
            enc.set_texture(1, Some(&self.curr_ds));
            enc.dispatch_threads(grid, tg);

            enc.end_encoding();
        }

        // ── Pass 3: Block-match optical flow ────────────────────────────────
        // One threadgroup per 8×8 block in the downsampled image; 81 threads
        // per group (9×9 search window).  The encoder boundary after Pass 2
        // ensures prev_ds and curr_ds are fully written before being read here.
        {
            const BLOCK: u64 = 8;
            const TG_THREADS: u64 = 81; // BM_NC = (2*4+1)^2

            let flow_w = ds_w.div_ceil(BLOCK);
            let flow_h = ds_h.div_ceil(BLOCK);

            let enc = cmd_buf.new_compute_command_encoder();
            enc.set_compute_pipeline_state(&self.block_match_pipeline);
            enc.set_texture(0, Some(&self.prev_ds));
            enc.set_texture(1, Some(&self.curr_ds));
            enc.set_texture(2, Some(&self.flow_tex));
            enc.dispatch_thread_groups(
                MTLSize {
                    width: flow_w,
                    height: flow_h,
                    depth: 1,
                },
                MTLSize {
                    width: TG_THREADS,
                    height: 1,
                    depth: 1,
                },
            );
            enc.end_encoding();
        }

        // ── Pass 4: Motion-compensated warp → RGBA output ──────────────────
        // Reads flow_tex (written in Pass 3) and both full-resolution NV12
        // planes.  The encoder boundary after Pass 3 ensures flow_tex is ready.
        {
            let params = WarpParams {
                full_range: full_range as u32,
                width: self.width as u32,
                height: self.height as u32,
                _pad: 0,
            };

            let enc = cmd_buf.new_compute_command_encoder();
            enc.set_compute_pipeline_state(&self.warp_pipeline);
            enc.set_texture(0, Some(prev_y));
            enc.set_texture(1, Some(prev_uv));
            enc.set_texture(2, Some(curr_y));
            enc.set_texture(3, Some(curr_uv));
            enc.set_texture(4, Some(&self.flow_tex));
            enc.set_texture(5, Some(&self.output));
            enc.set_bytes(
                0,
                std::mem::size_of::<WarpParams>() as u64,
                (&params as *const WarpParams).cast(),
            );
            enc.dispatch_threads(
                MTLSize {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                },
                MTLSize {
                    width: 16,
                    height: 16,
                    depth: 1,
                },
            );
            enc.end_encoding();
        }

        // ── Pass 5: Blit RGBA output to CAMetalLayer drawable ───────────────
        // The encoder boundary after Pass 4 ensures output is fully written.
        {
            let pass = RenderPassDescriptor::new();
            let attachment = pass
                .color_attachments()
                .object_at(0)
                .context("missing color attachment 0 (interp present)")?;
            attachment.set_texture(Some(target));
            attachment.set_load_action(MTLLoadAction::Clear);
            attachment.set_store_action(MTLStoreAction::Store);
            attachment.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 1.0));

            let vertices = fullscreen_vertices(flipped);

            let enc = cmd_buf.new_render_command_encoder(pass);
            enc.set_render_pipeline_state(&self.present_pipeline);
            enc.set_vertex_bytes(
                0,
                std::mem::size_of_val(&vertices) as u64,
                vertices.as_ptr().cast(),
            );
            enc.set_fragment_texture(0, Some(&self.output));
            enc.draw_primitives(MTLPrimitiveType::TriangleStrip, 0, 4);
            enc.end_encoding();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pipeline builders
// ---------------------------------------------------------------------------

fn build_compute_pipelines(
    device: &DeviceRef,
) -> Result<(
    ComputePipelineState,
    ComputePipelineState,
    ComputePipelineState,
)> {
    let lib = device
        .new_library_with_source(COMPUTE_SHADERS, &CompileOptions::new())
        .map_err(|e| anyhow!("compile interpolation compute shaders: {e}"))?;

    let ds_fn = lib
        .get_function("downsample_4x", None)
        .map_err(|e| anyhow!("load downsample_4x: {e}"))?;
    let bm_fn = lib
        .get_function("block_match_flow", None)
        .map_err(|e| anyhow!("load block_match_flow: {e}"))?;
    let warp_fn = lib
        .get_function("optical_flow_warp", None)
        .map_err(|e| anyhow!("load optical_flow_warp: {e}"))?;

    let ds_pipeline = device
        .new_compute_pipeline_state_with_function(&ds_fn)
        .map_err(|e| anyhow!("create downsample pipeline: {e}"))?;
    let bm_pipeline = device
        .new_compute_pipeline_state_with_function(&bm_fn)
        .map_err(|e| anyhow!("create block_match pipeline: {e}"))?;
    let warp_pipeline = device
        .new_compute_pipeline_state_with_function(&warp_fn)
        .map_err(|e| anyhow!("create warp pipeline: {e}"))?;

    Ok((ds_pipeline, bm_pipeline, warp_pipeline))
}

fn build_present_pipeline(device: &DeviceRef) -> Result<RenderPipelineState> {
    let lib = device
        .new_library_with_source(PRESENT_SHADER, &CompileOptions::new())
        .map_err(|e| anyhow!("compile interp present shader: {e}"))?;

    let vert = lib
        .get_function("rgb_vert", None)
        .map_err(|e| anyhow!("load rgb_vert: {e}"))?;
    let frag = lib
        .get_function("rgb_frag", None)
        .map_err(|e| anyhow!("load rgb_frag: {e}"))?;

    let desc = RenderPipelineDescriptor::new();
    desc.set_vertex_function(Some(&vert));
    desc.set_fragment_function(Some(&frag));
    desc.color_attachments()
        .object_at(0)
        .context("missing color attachment 0 (interp present)")?
        .set_pixel_format(MTLPixelFormat::BGRA8Unorm);

    device
        .new_render_pipeline_state(&desc)
        .map_err(|e| anyhow!("create interp present pipeline: {e}"))
}

// ---------------------------------------------------------------------------
// Texture allocation
// ---------------------------------------------------------------------------

fn alloc_textures(
    device: &DeviceRef,
    width: u64,
    height: u64,
) -> Result<(Texture, Texture, Texture, Texture)> {
    let ds_w = width.div_ceil(4);
    let ds_h = height.div_ceil(4);
    let flow_w = ds_w.div_ceil(8);
    let flow_h = ds_h.div_ceil(8);

    let prev_ds = new_private_rw_texture(device, MTLPixelFormat::R8Unorm, ds_w, ds_h)?;
    let curr_ds = new_private_rw_texture(device, MTLPixelFormat::R8Unorm, ds_w, ds_h)?;
    let flow_tex = new_private_rw_texture(device, MTLPixelFormat::RG32Float, flow_w, flow_h)?;
    let output = new_private_rw_texture(device, MTLPixelFormat::RGBA8Unorm, width, height)?;

    Ok((prev_ds, curr_ds, flow_tex, output))
}

fn new_private_rw_texture(
    device: &DeviceRef,
    fmt: MTLPixelFormat,
    width: u64,
    height: u64,
) -> Result<Texture> {
    let desc = TextureDescriptor::new();
    desc.set_texture_type(MTLTextureType::D2);
    desc.set_pixel_format(fmt);
    desc.set_width(width);
    desc.set_height(height);
    desc.set_storage_mode(MTLStorageMode::Private);
    desc.set_usage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
    let t = device.new_texture(&desc);
    if t.width() == 0 {
        anyhow::bail!(
            "failed to allocate GPU texture {:?} {}×{}",
            fmt,
            width,
            height
        );
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Vertex helpers
// ---------------------------------------------------------------------------

fn fullscreen_vertices(flipped: bool) -> [Vertex; 4] {
    let (top_v, bottom_v) = if flipped {
        (1.0_f32, 0.0_f32)
    } else {
        (0.0_f32, 1.0_f32)
    };
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
