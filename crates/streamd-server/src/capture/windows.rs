//! Windows desktop capture via DXGI Desktop Duplication.

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender as FrameSender;
use std::{
    ffi::{c_void, CString},
    slice,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use streamd_proto::packets::{
    DisplayInfo, RemoteCursorShape, RemoteCursorShapeKind, RemoteCursorState,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use windows::{
    core::{Interface, PCSTR},
    Win32::Graphics::{
        Direct3D::{
            Fxc::D3DCompile, ID3DBlob, ID3DInclude, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL,
            D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        },
        Direct3D11::{
            D3D11CreateDevice, ID3D11Buffer, ID3D11DepthStencilView, ID3D11Device,
            ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView,
            ID3D11Resource, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
        },
        Dxgi::{
            Common::{
                DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8X8_UNORM,
                DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UINT, DXGI_FORMAT_R8_UINT,
                DXGI_SAMPLE_DESC,
            },
            CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput6,
            IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
            DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC,
        },
    },
};

use crate::capture::{CaptureFrame, CaptureStats, CursorEvent, D3d11TextureHandle, ShmPixelFormat};

const FRAME_TIMEOUT_MS: u32 = 500;
const NVIDIA_VENDOR_ID: u32 = 0x10DE;

// DXGI_OUTDUPL_POINTER_SHAPE_TYPE values
const CURSOR_TYPE_MONOCHROME: u32 = 1;
const CURSOR_TYPE_COLOR: u32 = 2;
const CURSOR_TYPE_MASKED_COLOR: u32 = 4;
const SOURCE_TRANSFER_SRGB: u32 = 0;
const SOURCE_TRANSFER_LINEAR_FP16: u32 = 1;

const HDR_SHADER_SOURCE: &str = include_str!("hdr_fp16_to_bgra.hlsl");

/// Internal pixel format for captured frames before conversion.
#[derive(Debug, Clone, Copy)]
enum WindowsFrameFormat {
    Bgra8,
    Bgrx8,
    /// HDR FP16 — converted to BGRA8 either on the GPU or during CPU readback.
    RgbaF16,
}

impl WindowsFrameFormat {
    fn src_bytes_per_pixel(self) -> usize {
        match self {
            WindowsFrameFormat::Bgra8 | WindowsFrameFormat::Bgrx8 => 4,
            WindowsFrameFormat::RgbaF16 => 8,
        }
    }

    fn shm_format(self) -> ShmPixelFormat {
        match self {
            WindowsFrameFormat::Bgra8 | WindowsFrameFormat::RgbaF16 => ShmPixelFormat::Argb8888,
            WindowsFrameFormat::Bgrx8 => ShmPixelFormat::Xrgb8888,
        }
    }
}

/// Cursor state tracked across frames.
#[derive(Default, Clone)]
struct CursorState {
    visible: bool,
    /// Cursor position in virtual screen coordinates.
    x: i32,
    y: i32,
    hot_x: i32,
    hot_y: i32,
    width: u32,
    height: u32,
    pitch: u32,
    shape_type: u32,
    shape_generation: u64,
    shape_data: Vec<u8>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CursorShaderParams {
    cursor_origin: [i32; 2],
    cursor_size: [u32; 2],
    cursor_row_bytes: u32,
    cursor_type: u32,
    cursor_visible: u32,
    source_transfer: u32,
    _padding0: [u32; 4],
}

struct GpuConvertedFrame {
    texture: ID3D11Texture2D,
    resource_id: u64,
}

struct ShaderInputTexture {
    _texture: ID3D11Texture2D,
    resource: ID3D11Resource,
    srv: ID3D11ShaderResourceView,
}

struct GpuFrameRenderer {
    device: ID3D11Device,
    width: u32,
    height: u32,
    resource_id: u64,
    fp16_input: Option<ShaderInputTexture>,
    bgra_input: Option<ShaderInputTexture>,
    bgrx_input: Option<ShaderInputTexture>,
    output_texture: ID3D11Texture2D,
    output_rtv: ID3D11RenderTargetView,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    constant_buffer: ID3D11Buffer,
    null_color_srv: ID3D11ShaderResourceView,
    null_mono_srv: ID3D11ShaderResourceView,
}

impl GpuFrameRenderer {
    fn new(device: &ID3D11Device, width: u32, height: u32, resource_id: u64) -> Result<Self> {
        let vertex_shader_bytes = compile_shader(HDR_SHADER_SOURCE, "vs_main", "vs_4_0")
            .context("compile HDR vertex shader")?;
        let pixel_shader_bytes = compile_shader(HDR_SHADER_SOURCE, "ps_main", "ps_4_0")
            .context("compile HDR pixel shader")?;

        let mut vertex_shader = None;
        unsafe { device.CreateVertexShader(&vertex_shader_bytes, None, Some(&mut vertex_shader)) }
            .context("CreateVertexShader for HDR path")?;
        let vertex_shader = vertex_shader.context("CreateVertexShader returned no shader")?;

        let mut pixel_shader = None;
        unsafe { device.CreatePixelShader(&pixel_shader_bytes, None, Some(&mut pixel_shader)) }
            .context("CreatePixelShader for HDR path")?;
        let pixel_shader = pixel_shader.context("CreatePixelShader returned no shader")?;

        let output_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut output_texture = None;
        unsafe { device.CreateTexture2D(&output_desc, None, Some(&mut output_texture)) }
            .context("CreateTexture2D for HDR shader output")?;
        let output_texture =
            output_texture.context("CreateTexture2D returned no HDR output texture")?;
        let output_resource: ID3D11Resource = output_texture
            .cast()
            .context("cast HDR shader output texture to ID3D11Resource")?;
        let mut output_rtv = None;
        unsafe { device.CreateRenderTargetView(&output_resource, None, Some(&mut output_rtv)) }
            .context("CreateRenderTargetView for HDR shader output")?;
        let output_rtv = output_rtv.context("CreateRenderTargetView returned no HDR RTV")?;

        let constant_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<CursorShaderParams>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut constant_buffer = None;
        unsafe { device.CreateBuffer(&constant_desc, None, Some(&mut constant_buffer)) }
            .context("CreateBuffer for HDR cursor constants")?;
        let constant_buffer =
            constant_buffer.context("CreateBuffer returned no HDR constant buffer")?;

        let (_, null_color_srv) =
            create_srv_texture(device, 1, 1, DXGI_FORMAT_R8G8B8A8_UINT, &[0, 0, 0, 0], 4)
                .context("create dummy color cursor texture")?;
        let (_, null_mono_srv) = create_srv_texture(device, 1, 1, DXGI_FORMAT_R8_UINT, &[0], 1)
            .context("create dummy monochrome cursor texture")?;

        Ok(Self {
            device: device.clone(),
            width,
            height,
            resource_id,
            fp16_input: None,
            bgra_input: None,
            bgrx_input: None,
            output_texture,
            output_rtv,
            vertex_shader,
            pixel_shader,
            constant_buffer,
            null_color_srv,
            null_mono_srv,
        })
    }

    fn matches(&self, width: u32, height: u32) -> bool {
        self.width == width && self.height == height
    }

    fn convert(
        &mut self,
        context: &ID3D11DeviceContext,
        source_texture: &ID3D11Texture2D,
        source_format: DXGI_FORMAT,
    ) -> Result<GpuConvertedFrame> {
        let (input_resource, input_srv) = self
            .shader_input(source_format)
            .with_context(|| format!("prepare shader input for {:?}", source_format))?;
        let source_resource: ID3D11Resource = source_texture
            .cast()
            .context("cast desktop duplication texture to ID3D11Resource")?;
        unsafe {
            context.CopyResource(&input_resource, &source_resource);
        }

        let params = CursorShaderParams {
            cursor_origin: [0, 0],
            cursor_size: [0, 0],
            cursor_row_bytes: 0,
            cursor_type: 0,
            cursor_visible: 0,
            source_transfer: if source_format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                SOURCE_TRANSFER_LINEAR_FP16
            } else {
                SOURCE_TRANSFER_SRGB
            },
            _padding0: [0; 4],
        };

        unsafe {
            context.UpdateSubresource(
                &self.constant_buffer,
                0,
                None,
                &params as *const CursorShaderParams as *const c_void,
                0,
                0,
            );
        }

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let srvs = [
            Some(input_srv),
            Some(self.null_color_srv.clone()),
            Some(self.null_mono_srv.clone()),
        ];
        let rtvs = [Some(self.output_rtv.clone())];
        let constant_buffers = [Some(self.constant_buffer.clone())];
        let null_srvs: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];
        let null_rtvs: [Option<ID3D11RenderTargetView>; 1] = [None];

        unsafe {
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.RSSetViewports(Some(&[viewport]));
            context.PSSetConstantBuffers(0, Some(&constant_buffers));
            context.PSSetShaderResources(0, Some(&srvs));
            context.OMSetRenderTargets(Some(&rtvs), None::<&ID3D11DepthStencilView>);
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&null_srvs));
            context.OMSetRenderTargets(Some(&null_rtvs), None::<&ID3D11DepthStencilView>);
        }

        Ok(GpuConvertedFrame {
            texture: self.output_texture.clone(),
            resource_id: self.resource_id,
        })
    }

    fn shader_input(
        &mut self,
        format: DXGI_FORMAT,
    ) -> Result<(ID3D11Resource, ID3D11ShaderResourceView)> {
        let slot = match format {
            DXGI_FORMAT_R16G16B16A16_FLOAT => &mut self.fp16_input,
            DXGI_FORMAT_B8G8R8A8_UNORM => &mut self.bgra_input,
            DXGI_FORMAT_B8G8R8X8_UNORM => &mut self.bgrx_input,
            _ => bail!("unsupported GPU shader input format {format:?}"),
        };

        if slot.is_none() {
            *slot = Some(
                create_shader_input_texture(&self.device, self.width, self.height, format)
                    .with_context(|| format!("create shader input texture for {:?}", format))?,
            );
        }

        let slot = slot
            .as_ref()
            .context("shader input missing after allocation")?;
        Ok((slot.resource.clone(), slot.srv.clone()))
    }
}

pub struct WindowsCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    staging_texture: Option<ID3D11Texture2D>,
    staging_resource: Option<ID3D11Resource>,
    gpu_renderer: Option<GpuFrameRenderer>,
    gpu_fp16_enabled: bool,
    next_gpu_resource_id: u64,
    frame_tx: FrameSender<CaptureFrame>,
    cursor_tx: UnboundedSender<CursorEvent>,
    output_name: String,
    /// Top-left corner of this display in virtual screen coordinates.
    display_origin: (i32, i32),
    cursor: CursorState,
    last_cursor_shape_sent_generation: u64,
}

impl WindowsCapture {
    pub fn new(
        display_id: Option<&str>,
        frame_tx: FrameSender<CaptureFrame>,
        cursor_tx: UnboundedSender<CursorEvent>,
    ) -> Result<Self> {
        let selected = select_output(display_id).context("find a desktop output for capture")?;
        let (device, context) =
            create_device(&selected.adapter).context("create D3D11 device for capture")?;
        let duplication = duplicate_output(&selected.output, &device)
            .context("create DXGI desktop duplication session")?;

        let display_origin = selected.origin;
        let gpu_fp16_enabled = selected.vendor_id == NVIDIA_VENDOR_ID;

        info!(
            "Windows desktop duplication initialised on output {} ({})",
            selected.info.name, selected.info.id
        );
        if !gpu_fp16_enabled {
            info!(
                "capture output {} is not on an NVIDIA adapter; HDR FP16 frames will use CPU conversion",
                selected.info.id
            );
        }

        Ok(Self {
            device,
            context,
            output: selected.output,
            duplication,
            staging_texture: None,
            staging_resource: None,
            gpu_renderer: None,
            gpu_fp16_enabled,
            next_gpu_resource_id: 1,
            frame_tx,
            cursor_tx,
            output_name: selected.info.name,
            display_origin,
            cursor: CursorState::default(),
            last_cursor_shape_sent_generation: 0,
        })
    }

    pub fn d3d11_device(&self) -> ID3D11Device {
        self.device.clone()
    }

    pub fn disable_gpu_fp16(&mut self) {
        if self.gpu_fp16_enabled {
            warn!(
                "disabling Windows HDR GPU path on output {}; falling back to CPU conversion",
                self.output_name
            );
        }
        self.gpu_fp16_enabled = false;
        self.gpu_renderer = None;
    }

    pub fn pump(&mut self) -> Result<()> {
        loop {
            let acquire_started_at = Instant::now();
            let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
            let mut resource: Option<IDXGIResource> = None;
            match unsafe {
                self.duplication
                    .AcquireNextFrame(FRAME_TIMEOUT_MS, &mut frame_info, &mut resource)
            } {
                Ok(()) => {
                    let acquire_wait_us = duration_to_us(acquire_started_at.elapsed());
                    let _release = ReleaseFrameGuard::new(&self.duplication);

                    if frame_info.LastMouseUpdateTime != 0 {
                        self.cursor.visible = frame_info.PointerPosition.Visible.as_bool();
                        self.cursor.x = frame_info.PointerPosition.Position.x;
                        self.cursor.y = frame_info.PointerPosition.Position.y;
                    }

                    if frame_info.PointerShapeBufferSize > 0 {
                        let buf_size = frame_info.PointerShapeBufferSize;
                        let mut shape_buf = vec![0u8; buf_size as usize];
                        let mut shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO =
                            unsafe { std::mem::zeroed() };
                        let mut actual_size = 0u32;
                        if unsafe {
                            self.duplication.GetFramePointerShape(
                                buf_size,
                                shape_buf.as_mut_ptr() as *mut _,
                                &mut actual_size,
                                &mut shape_info,
                            )
                        }
                        .is_ok()
                        {
                            shape_buf.truncate(actual_size as usize);
                            self.cursor.shape_data = shape_buf;
                            self.cursor.width = shape_info.Width;
                            self.cursor.height = shape_info.Height;
                            self.cursor.pitch = shape_info.Pitch;
                            self.cursor.hot_x = shape_info.HotSpot.x;
                            self.cursor.hot_y = shape_info.HotSpot.y;
                            self.cursor.shape_type = shape_info.Type;
                            self.cursor.shape_generation =
                                self.cursor.shape_generation.wrapping_add(1);
                        }
                    }

                    self.publish_cursor_shape_if_needed()
                        .context("publish cursor shape update")?;

                    let resource = resource.context("desktop duplication returned no frame")?;
                    let texture: ID3D11Texture2D =
                        resource.cast().context("cast frame to ID3D11Texture2D")?;
                    let texture_desc = get_texture_desc(&texture);
                    let frame_format =
                        pixel_format_from_dxgi(texture_desc.Format).with_context(|| {
                            format!(
                                "unsupported desktop duplication format {:?} on output {}",
                                texture_desc.Format, self.output_name
                            )
                        })?;
                    let timestamp_us = capture_timestamp_us();
                    self.publish_cursor_state(timestamp_us)
                        .context("publish cursor state update")?;

                    if self.gpu_fp16_enabled {
                        let context = self.context.clone();
                        match self.ensure_gpu_renderer(texture_desc.Width, texture_desc.Height) {
                            Ok(renderer) => {
                                let convert_started_at = Instant::now();
                                let gpu_frame = renderer
                                    .convert(&context, &texture, texture_desc.Format)
                                    .context("convert desktop frame on GPU")?;
                                self.frame_tx
                                    .send(CaptureFrame::D3d11Texture {
                                        texture: D3d11TextureHandle::new(gpu_frame.texture),
                                        resource_id: gpu_frame.resource_id,
                                        width: texture_desc.Width,
                                        height: texture_desc.Height,
                                        timestamp_us,
                                        stats: CaptureStats {
                                            acquire_wait_us,
                                            convert_us: duration_to_us(
                                                convert_started_at.elapsed(),
                                            ),
                                        },
                                    })
                                    .context("capture frame receiver dropped")?;
                                return Ok(());
                            }
                            Err(err) => {
                                warn!(
                                    "failed to initialise HDR GPU path on {}: {err:#}; falling back to CPU conversion",
                                    self.output_name
                                );
                                self.disable_gpu_fp16();
                            }
                        }
                    }

                    let convert_started_at = Instant::now();
                    ensure_staging_texture(
                        &self.device,
                        &mut self.staging_texture,
                        &mut self.staging_resource,
                        &texture_desc,
                    )
                    .context("prepare D3D11 staging texture")?;

                    let staging_resource = self
                        .staging_resource
                        .as_ref()
                        .context("staging resource missing after allocation")?;
                    let source_resource: ID3D11Resource = texture
                        .cast()
                        .context("cast desktop texture to ID3D11Resource")?;

                    unsafe {
                        self.context
                            .CopyResource(staging_resource, &source_resource);
                    }

                    let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
                    unsafe {
                        self.context
                            .Map(staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    }
                    .context("map staging texture for CPU readback")?;

                    let copy_result = copy_mapped_frame(
                        &mapped,
                        texture_desc.Width,
                        texture_desc.Height,
                        frame_format,
                    );
                    unsafe {
                        self.context.Unmap(staging_resource, 0);
                    }
                    let (data, stride) = copy_result?;

                    self.frame_tx
                        .send(CaptureFrame::Shm {
                            data,
                            width: texture_desc.Width,
                            height: texture_desc.Height,
                            stride,
                            format: frame_format.shm_format(),
                            timestamp_us,
                            stats: CaptureStats {
                                acquire_wait_us,
                                convert_us: duration_to_us(convert_started_at.elapsed()),
                            },
                        })
                        .context("capture frame receiver dropped")?;
                    return Ok(());
                }
                Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                    warn!(
                        "desktop duplication access lost on output {}: {}; recreating session",
                        self.output_name, err
                    );
                    self.recreate_duplication()
                        .context("recreate DXGI desktop duplication session")?;
                }
                Err(err) => {
                    return Err(anyhow!(
                        "AcquireNextFrame failed on {}: {err}",
                        self.output_name
                    ));
                }
            }
        }
    }

    fn ensure_gpu_renderer(&mut self, width: u32, height: u32) -> Result<&mut GpuFrameRenderer> {
        let recreate = match self.gpu_renderer.as_ref() {
            Some(renderer) => !renderer.matches(width, height),
            None => true,
        };

        if recreate {
            let resource_id = self.next_gpu_resource_id;
            self.next_gpu_resource_id = self.next_gpu_resource_id.wrapping_add(1);
            self.gpu_renderer = Some(
                GpuFrameRenderer::new(&self.device, width, height, resource_id)
                    .context("create desktop GPU renderer")?,
            );
        }

        self.gpu_renderer
            .as_mut()
            .context("desktop GPU renderer missing after allocation")
    }

    fn recreate_duplication(&mut self) -> Result<()> {
        self.duplication = duplicate_output(&self.output, &self.device)
            .context("duplicate output after access loss")?;
        self.staging_texture = None;
        self.staging_resource = None;
        self.gpu_renderer = None;
        Ok(())
    }

    fn publish_cursor_shape_if_needed(&mut self) -> Result<()> {
        if self.cursor.shape_generation == 0
            || self.cursor.shape_generation == self.last_cursor_shape_sent_generation
        {
            return Ok(());
        }

        if let Some(shape) = build_remote_cursor_shape(&self.cursor)? {
            self.cursor_tx
                .send(CursorEvent::Shape(shape))
                .context("cursor event receiver dropped")?;
        }
        self.last_cursor_shape_sent_generation = self.cursor.shape_generation;
        Ok(())
    }

    fn publish_cursor_state(&self, timestamp_us: u64) -> Result<()> {
        let (x, y) = cursor_position_relative_to_display(&self.cursor, self.display_origin);
        self.cursor_tx
            .send(CursorEvent::State(RemoteCursorState {
                timestamp_us,
                generation: self.cursor.shape_generation,
                visible: self.cursor.visible,
                x,
                y,
            }))
            .context("cursor event receiver dropped")?;
        Ok(())
    }
}

fn build_remote_cursor_shape(cursor: &CursorState) -> Result<Option<RemoteCursorShape>> {
    match cursor.shape_type {
        CURSOR_TYPE_COLOR | CURSOR_TYPE_MASKED_COLOR => {
            if cursor.width == 0 || cursor.height == 0 || cursor.shape_data.is_empty() {
                return Ok(None);
            }

            let data = repack_color_cursor_rgba(cursor)?;
            Ok(Some(RemoteCursorShape {
                generation: cursor.shape_generation,
                kind: if cursor.shape_type == CURSOR_TYPE_MASKED_COLOR {
                    RemoteCursorShapeKind::MaskedColor
                } else {
                    RemoteCursorShapeKind::Color
                },
                width: cursor.width,
                height: cursor.height,
                pitch: cursor
                    .width
                    .checked_mul(4)
                    .context("cursor row pitch overflow")?,
                data,
            }))
        }
        CURSOR_TYPE_MONOCHROME => {
            if cursor.width == 0 || cursor.height == 0 || cursor.shape_data.is_empty() {
                return Ok(None);
            }

            let visible_height = cursor.height / 2;
            let row_bytes = ((cursor.width + 31) / 32) * 4;
            let expected_len = usize::try_from(row_bytes)
                .context("monochrome cursor row pitch overflow")?
                .checked_mul(
                    usize::try_from(cursor.height).context("monochrome cursor height overflow")?,
                )
                .context("monochrome cursor buffer size overflow")?;
            let mut data = vec![0u8; expected_len];
            let copy_len = data.len().min(cursor.shape_data.len());
            data[..copy_len].copy_from_slice(&cursor.shape_data[..copy_len]);

            Ok(Some(RemoteCursorShape {
                generation: cursor.shape_generation,
                kind: RemoteCursorShapeKind::Monochrome,
                width: cursor.width,
                height: visible_height,
                pitch: row_bytes,
                data,
            }))
        }
        _ => Ok(None),
    }
}

fn cursor_position_relative_to_display(
    cursor: &CursorState,
    display_origin: (i32, i32),
) -> (i32, i32) {
    (
        cursor.x - display_origin.0 - cursor.hot_x,
        cursor.y - display_origin.1 - cursor.hot_y,
    )
}

fn ensure_staging_texture(
    device: &ID3D11Device,
    staging_texture: &mut Option<ID3D11Texture2D>,
    staging_resource: &mut Option<ID3D11Resource>,
    source_desc: &D3D11_TEXTURE2D_DESC,
) -> Result<()> {
    let recreate = match staging_texture.as_ref() {
        Some(existing) => {
            let desc = get_texture_desc(existing);
            desc.Width != source_desc.Width
                || desc.Height != source_desc.Height
                || desc.Format != source_desc.Format
        }
        None => true,
    };

    if !recreate {
        return Ok(());
    }

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: source_desc.Width,
        Height: source_desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: source_desc.Format,
        SampleDesc: source_desc.SampleDesc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut new_staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut new_staging)) }
        .context("CreateTexture2D for staging readback")?;
    let new_staging = new_staging.context("CreateTexture2D returned no staging texture")?;
    let new_staging_resource: ID3D11Resource = new_staging
        .cast()
        .context("cast staging texture to ID3D11Resource")?;

    *staging_texture = Some(new_staging);
    *staging_resource = Some(new_staging_resource);
    Ok(())
}

struct SelectedOutput {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    info: DisplayInfo,
    origin: (i32, i32),
    vendor_id: u32,
}

struct ReleaseFrameGuard {
    duplication: IDXGIOutputDuplication,
    active: bool,
}

impl ReleaseFrameGuard {
    fn new(duplication: &IDXGIOutputDuplication) -> Self {
        Self {
            duplication: duplication.clone(),
            active: true,
        }
    }
}

impl Drop for ReleaseFrameGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(enumerate_outputs()?
        .into_iter()
        .map(|output| output.info)
        .collect())
}

fn select_output(display_id: Option<&str>) -> Result<SelectedOutput> {
    let outputs = enumerate_outputs()?;
    if outputs.is_empty() {
        bail!("no attached desktop output was found for capture");
    }

    if let Some(display_id) = display_id {
        return outputs
            .into_iter()
            .find(|output| output.info.id == display_id)
            .with_context(|| format!("Windows display {display_id:?} is not available"));
    }

    Ok(outputs
        .into_iter()
        .next()
        .expect("checked non-empty output list"))
}

fn enumerate_outputs() -> Result<Vec<SelectedOutput>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut displays = Vec::new();
    let mut display_index = 0u32;

    let mut adapter_index = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(anyhow!("EnumAdapters1({adapter_index}) failed: {err}")),
        };
        let adapter_desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1")?;
        let adapter_name = wide_string(&adapter_desc.Description);

        let mut output_index = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(err) => {
                    return Err(anyhow!(
                        "EnumOutputs({adapter_index}, {output_index}) failed: {err}"
                    ))
                }
            };

            let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
            if desc.AttachedToDesktop.as_bool() {
                let output1: IDXGIOutput1 = output.cast().context("cast output to IDXGIOutput1")?;
                let name = output_name(&desc);
                let description = windows_display_description(&adapter_name, &name);
                let origin = (desc.DesktopCoordinates.left, desc.DesktopCoordinates.top);
                displays.push(SelectedOutput {
                    adapter: adapter.clone(),
                    output: output1,
                    origin,
                    vendor_id: adapter_desc.VendorId,
                    info: DisplayInfo {
                        id: windows_display_id(adapter_index as u32, output_index as u32),
                        index: display_index,
                        name,
                        description,
                        width: display_width(&desc),
                        height: display_height(&desc),
                    },
                });
                display_index += 1;
            }

            output_index += 1;
        }

        adapter_index += 1;
    }

    if displays.is_empty() {
        bail!("no attached desktop output was found for capture");
    }

    Ok(displays)
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];

    let mut device = None;
    let mut context = None;
    let mut selected_feature = D3D_FEATURE_LEVEL_11_0;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected_feature),
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;

    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no device context")?;
    info!("D3D11 capture device initialised at feature level {selected_feature:?}");
    Ok((device, context))
}

fn duplicate_output(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication> {
    if let Ok(output6) = output.cast::<IDXGIOutput6>() {
        let formats = [DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT];
        match unsafe { output6.DuplicateOutput1(device, 0, &formats) } {
            Ok(duplication) => {
                let desc: DXGI_OUTDUPL_DESC = unsafe { duplication.GetDesc() };
                info!(
                    "desktop duplication ready (DXGI 1.6): {}x{}",
                    desc.ModeDesc.Width, desc.ModeDesc.Height
                );
                return Ok(duplication);
            }
            Err(err) => {
                warn!("DuplicateOutput1 failed ({err}), falling back to DuplicateOutput");
            }
        }
    }

    let duplication =
        unsafe { output.DuplicateOutput(device) }.context("IDXGIOutput1::DuplicateOutput")?;
    let desc: DXGI_OUTDUPL_DESC = unsafe { duplication.GetDesc() };
    pixel_format_from_dxgi(desc.ModeDesc.Format).with_context(|| {
        format!(
            "unsupported desktop duplication format {:?} — disable HDR to use SDR capture",
            desc.ModeDesc.Format
        )
    })?;
    info!(
        "desktop duplication ready: {}x{}",
        desc.ModeDesc.Width, desc.ModeDesc.Height
    );
    Ok(duplication)
}

fn get_texture_desc(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
    unsafe {
        texture.GetDesc(&mut desc);
    }
    desc
}

fn copy_mapped_frame(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
    format: WindowsFrameFormat,
) -> Result<(Vec<u8>, u32)> {
    let width = usize::try_from(width).context("capture width overflow")?;
    let height = usize::try_from(height).context("capture height overflow")?;
    let src_stride = usize::try_from(mapped.RowPitch).context("mapped row pitch overflow")?;
    let src_bytes_per_row = width * format.src_bytes_per_pixel();
    let dst_bytes_per_row = width * 4;

    if mapped.pData.is_null() {
        bail!("desktop duplication returned a null staging mapping");
    }
    if src_stride < src_bytes_per_row {
        bail!("desktop duplication row pitch {src_stride} < {src_bytes_per_row}");
    }

    let dst_stride = u32::try_from(dst_bytes_per_row).context("capture stride overflow")?;
    let mut data = vec![
        0u8;
        dst_bytes_per_row
            .checked_mul(height)
            .context("capture buffer size overflow")?
    ];
    let src_base = mapped.pData.cast::<u8>();

    match format {
        WindowsFrameFormat::Bgra8 | WindowsFrameFormat::Bgrx8 => {
            for row in 0..height {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src_base.add(row * src_stride),
                        data.as_mut_ptr().add(row * dst_bytes_per_row),
                        dst_bytes_per_row,
                    );
                }
            }
        }
        WindowsFrameFormat::RgbaF16 => {
            for row in 0..height {
                for col in 0..width {
                    let src = row * src_stride + col * 8;
                    let dst = row * dst_bytes_per_row + col * 4;
                    unsafe {
                        let r = f16_to_srgb_u8(u16::from_le_bytes([
                            *src_base.add(src),
                            *src_base.add(src + 1),
                        ]));
                        let g = f16_to_srgb_u8(u16::from_le_bytes([
                            *src_base.add(src + 2),
                            *src_base.add(src + 3),
                        ]));
                        let b = f16_to_srgb_u8(u16::from_le_bytes([
                            *src_base.add(src + 4),
                            *src_base.add(src + 5),
                        ]));
                        data[dst] = b;
                        data[dst + 1] = g;
                        data[dst + 2] = r;
                        data[dst + 3] = 255;
                    }
                }
            }
        }
    }

    Ok((data, dst_stride))
}

#[inline]
fn f16_to_f32(bits: u16) -> f32 {
    let sign = bits >> 15;
    let exp = (bits >> 10) & 0x1f;
    let mantissa = (bits & 0x3ff) as u32;

    if sign != 0 {
        return 0.0;
    }

    if exp == 31 {
        return 1.0;
    }

    if exp == 0 {
        mantissa as f32 * (1.0 / (1024.0 * 16384.0))
    } else {
        f32::from_bits(((exp as u32 + 127 - 15) << 23) | (mantissa << 13))
    }
}

#[inline]
fn linear_to_srgb_u8(linear: f32) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let srgb = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (srgb.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[inline]
fn f16_to_srgb_u8(bits: u16) -> u8 {
    linear_to_srgb_u8(f16_to_f32(bits))
}

fn pixel_format_from_dxgi(format: DXGI_FORMAT) -> Option<WindowsFrameFormat> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM => Some(WindowsFrameFormat::Bgra8),
        DXGI_FORMAT_B8G8R8X8_UNORM => Some(WindowsFrameFormat::Bgrx8),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(WindowsFrameFormat::RgbaF16),
        _ => None,
    }
}

fn output_name(desc: &DXGI_OUTPUT_DESC) -> String {
    wide_string(&desc.DeviceName)
}

fn wide_string(chars: &[u16]) -> String {
    let end = chars.iter().position(|&ch| ch == 0).unwrap_or(chars.len());
    String::from_utf16_lossy(&chars[..end])
}

fn windows_display_id(adapter_index: u32, output_index: u32) -> String {
    format!("windows:{adapter_index}:{output_index}")
}

fn windows_display_description(adapter_name: &str, output_name: &str) -> Option<String> {
    let adapter_name = adapter_name.trim();
    if adapter_name.is_empty() || adapter_name == output_name {
        None
    } else {
        Some(format!("{adapter_name} / {output_name}"))
    }
}

fn display_width(desc: &DXGI_OUTPUT_DESC) -> u32 {
    (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32
}

fn display_height(desc: &DXGI_OUTPUT_DESC) -> u32 {
    (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32
}

fn capture_timestamp_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn duration_to_us(duration: std::time::Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

fn compile_shader(source: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    let entry = CString::new(entry).context("shader entry point contains interior NUL")?;
    let target = CString::new(target).context("shader target contains interior NUL")?;
    let mut code = None;
    let mut errors = None;

    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(err) = result {
        let details = errors
            .as_ref()
            .map(blob_to_string)
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or_default();
        if details.is_empty() {
            return Err(anyhow!("D3DCompile({entry:?}, {target:?}) failed: {err}"));
        }
        return Err(anyhow!(
            "D3DCompile({entry:?}, {target:?}) failed: {err}: {details}"
        ));
    }

    let code = code.context("D3DCompile returned no shader blob")?;
    Ok(blob_bytes(&code).to_vec())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn blob_to_string(blob: &ID3DBlob) -> String {
    String::from_utf8_lossy(blob_bytes(blob)).trim().to_string()
}

fn create_srv_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    data: &[u8],
    row_pitch: u32,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const c_void,
        SysMemPitch: row_pitch,
        SysMemSlicePitch: row_pitch.saturating_mul(height),
    };

    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init_data), Some(&mut texture)) }
        .context("CreateTexture2D for shader resource")?;
    let texture = texture.context("CreateTexture2D returned no shader resource texture")?;
    let resource: ID3D11Resource = texture
        .cast()
        .context("cast shader resource texture to ID3D11Resource")?;
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&resource, None, Some(&mut srv)) }
        .context("CreateShaderResourceView for texture")?;
    let srv = srv.context("CreateShaderResourceView returned no texture SRV")?;
    Ok((texture, srv))
}

fn create_shader_input_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ShaderInputTexture> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D for desktop shader input")?;
    let texture = texture.context("CreateTexture2D returned no desktop shader input texture")?;
    let resource: ID3D11Resource = texture
        .cast()
        .context("cast desktop shader input texture to ID3D11Resource")?;
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&resource, None, Some(&mut srv)) }
        .context("CreateShaderResourceView for desktop shader input")?;
    let srv = srv.context("CreateShaderResourceView returned no desktop shader input SRV")?;
    Ok(ShaderInputTexture {
        _texture: texture,
        resource,
        srv,
    })
}

fn repack_color_cursor_rgba(cursor: &CursorState) -> Result<Vec<u8>> {
    let width = usize::try_from(cursor.width).context("cursor width overflow")?;
    let height = usize::try_from(cursor.height).context("cursor height overflow")?;
    let src_pitch = usize::try_from(cursor.pitch).context("cursor pitch overflow")?;
    let dst_pitch = width.checked_mul(4).context("cursor row size overflow")?;
    let mut out = vec![
        0u8;
        dst_pitch
            .checked_mul(height)
            .context("cursor buffer overflow")?
    ];

    for row in 0..height {
        let src_row = row
            .checked_mul(src_pitch)
            .context("cursor source row offset overflow")?;
        let dst_row = row
            .checked_mul(dst_pitch)
            .context("cursor destination row offset overflow")?;
        for col in 0..width {
            let src = src_row + col * 4;
            let dst = dst_row + col * 4;
            if src + 3 >= cursor.shape_data.len() {
                continue;
            }
            out[dst] = cursor.shape_data[src + 2];
            out[dst + 1] = cursor.shape_data[src + 1];
            out[dst + 2] = cursor.shape_data[src];
            out[dst + 3] = cursor.shape_data[src + 3];
        }
    }

    Ok(out)
}
