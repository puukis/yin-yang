//! Windows desktop capture via DXGI Desktop Duplication.

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender;
use std::time::{SystemTime, UNIX_EPOCH};
use streamd_proto::packets::DisplayInfo;
use tracing::{info, warn};
use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
            D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        },
        Dxgi::{
            Common::{
                DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8X8_UNORM,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
            },
            CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput6,
            IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
            DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC,
        },
    },
};

use crate::capture::{CaptureFrame, ShmPixelFormat};

const FRAME_TIMEOUT_MS: u32 = 500;

// DXGI_OUTDUPL_POINTER_SHAPE_TYPE values
const CURSOR_TYPE_MONOCHROME: u32 = 1;
const CURSOR_TYPE_COLOR: u32 = 2;
const CURSOR_TYPE_MASKED_COLOR: u32 = 4;

/// Internal pixel format for captured frames before CPU conversion.
#[derive(Debug, Clone, Copy)]
enum WindowsFrameFormat {
    Bgra8,
    Bgrx8,
    /// HDR FP16 — converted to BGRA8 during CPU readback.
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
#[derive(Default)]
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
    shape_data: Vec<u8>,
}

pub struct WindowsCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    staging_texture: Option<ID3D11Texture2D>,
    staging_resource: Option<ID3D11Resource>,
    frame_tx: Sender<CaptureFrame>,
    output_name: String,
    /// Top-left corner of this display in virtual screen coordinates.
    display_origin: (i32, i32),
    cursor: CursorState,
}

impl WindowsCapture {
    pub fn new(display_id: Option<&str>, frame_tx: Sender<CaptureFrame>) -> Result<Self> {
        let selected = select_output(display_id).context("find a desktop output for capture")?;
        let (device, context) =
            create_device(&selected.adapter).context("create D3D11 device for capture")?;
        let duplication = duplicate_output(&selected.output, &device)
            .context("create DXGI desktop duplication session")?;

        let display_origin = selected.origin;

        info!(
            "Windows desktop duplication initialised on output {} ({})",
            selected.info.name, selected.info.id
        );

        Ok(Self {
            device,
            context,
            output: selected.output,
            duplication,
            staging_texture: None,
            staging_resource: None,
            frame_tx,
            output_name: selected.info.name,
            display_origin,
            cursor: CursorState::default(),
        })
    }

    pub fn pump(&mut self) -> Result<()> {
        loop {
            let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
            let mut resource: Option<IDXGIResource> = None;
            match unsafe {
                self.duplication
                    .AcquireNextFrame(FRAME_TIMEOUT_MS, &mut frame_info, &mut resource)
            } {
                Ok(()) => {
                    let _release = ReleaseFrameGuard::new(&self.duplication);

                    // Update cursor position/visibility from this frame's metadata.
                    if frame_info.LastMouseUpdateTime != 0 {
                        self.cursor.visible =
                            frame_info.PointerPosition.Visible.as_bool();
                        self.cursor.x = frame_info.PointerPosition.Position.x;
                        self.cursor.y = frame_info.PointerPosition.Position.y;
                    }

                    // Fetch new cursor shape if it changed this frame.
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
                            self.cursor.shape_data = shape_buf;
                            self.cursor.width = shape_info.Width;
                            self.cursor.height = shape_info.Height;
                            self.cursor.pitch = shape_info.Pitch;
                            self.cursor.hot_x = shape_info.HotSpot.x;
                            self.cursor.hot_y = shape_info.HotSpot.y;
                            self.cursor.shape_type = shape_info.Type;
                        }
                    }

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

                    ensure_staging_texture(
                        &self.device,
                        &mut self.staging_texture,
                        &mut self.staging_resource,
                        &texture_desc,
                    )
                    .context("prepare D3D11 staging texture")?;

                    self.staging_texture
                        .as_ref()
                        .context("staging texture missing after allocation")?;
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
                    let (mut data, stride) = copy_result?;

                    // Composite the hardware cursor on top of the frame.
                    if self.cursor.visible && !self.cursor.shape_data.is_empty() {
                        composite_cursor(
                            &mut data,
                            texture_desc.Width,
                            texture_desc.Height,
                            stride,
                            &self.cursor,
                            self.display_origin,
                        );
                    }

                    self.frame_tx
                        .send(CaptureFrame::Shm {
                            data,
                            width: texture_desc.Width,
                            height: texture_desc.Height,
                            stride,
                            format: frame_format.shm_format(),
                            timestamp_us: capture_timestamp_us(),
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

    fn recreate_duplication(&mut self) -> Result<()> {
        self.duplication = duplicate_output(&self.output, &self.device)
            .context("duplicate output after access loss")?;
        self.staging_texture = None;
        self.staging_resource = None;
        Ok(())
    }
}

/// Composite the hardware cursor into `frame` (BGRA8, packed).
fn composite_cursor(
    frame: &mut [u8],
    frame_w: u32,
    frame_h: u32,
    frame_stride: u32,
    cursor: &CursorState,
    display_origin: (i32, i32),
) {
    // Convert virtual-screen cursor position → display-local, then apply hot-spot offset.
    let x0 = cursor.x - display_origin.0 - cursor.hot_x;
    let y0 = cursor.y - display_origin.1 - cursor.hot_y;

    let (cw, ch) = match cursor.shape_type {
        CURSOR_TYPE_MONOCHROME => (cursor.width as i32, cursor.height as i32 / 2),
        _ => (cursor.width as i32, cursor.height as i32),
    };

    for cy in 0..ch {
        for cx in 0..cw {
            let fx = x0 + cx;
            let fy = y0 + cy;
            if fx < 0 || fy < 0 || fx >= frame_w as i32 || fy >= frame_h as i32 {
                continue;
            }
            let fi = fy as usize * frame_stride as usize + fx as usize * 4;
            if fi + 3 >= frame.len() {
                continue;
            }

            match cursor.shape_type {
                CURSOR_TYPE_COLOR => {
                    let si = cy as usize * cursor.pitch as usize + cx as usize * 4;
                    if si + 3 >= cursor.shape_data.len() {
                        continue;
                    }
                    let a = cursor.shape_data[si + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let ia = 255 - a;
                    // Cursor is BGRA8; blend over frame BGRA8.
                    frame[fi] =
                        ((cursor.shape_data[si] as u32 * a + frame[fi] as u32 * ia) / 255) as u8;
                    frame[fi + 1] = ((cursor.shape_data[si + 1] as u32 * a
                        + frame[fi + 1] as u32 * ia)
                        / 255) as u8;
                    frame[fi + 2] = ((cursor.shape_data[si + 2] as u32 * a
                        + frame[fi + 2] as u32 * ia)
                        / 255) as u8;
                }
                CURSOR_TYPE_MASKED_COLOR => {
                    let si = cy as usize * cursor.pitch as usize + cx as usize * 4;
                    if si + 3 >= cursor.shape_data.len() {
                        continue;
                    }
                    let a = cursor.shape_data[si + 3];
                    if a == 0xFF {
                        // XOR with background.
                        frame[fi] ^= cursor.shape_data[si];
                        frame[fi + 1] ^= cursor.shape_data[si + 1];
                        frame[fi + 2] ^= cursor.shape_data[si + 2];
                    } else {
                        // Replace (alpha = 0 means opaque here).
                        frame[fi] = cursor.shape_data[si];
                        frame[fi + 1] = cursor.shape_data[si + 1];
                        frame[fi + 2] = cursor.shape_data[si + 2];
                    }
                }
                CURSOR_TYPE_MONOCHROME => {
                    // AND mask is first half, XOR mask is second half.
                    // Each row is 4-byte aligned, 1 bit per pixel.
                    let row_bytes = ((cw as usize + 31) / 32) * 4;
                    let byte_col = cx as usize / 8;
                    let bit = 0x80u8 >> (cx as usize % 8);

                    let and_idx = cy as usize * row_bytes + byte_col;
                    let xor_idx = (cy as usize + ch as usize) * row_bytes + byte_col;
                    if xor_idx >= cursor.shape_data.len() {
                        continue;
                    }

                    let and_bit = (cursor.shape_data[and_idx] & bit) != 0;
                    let xor_bit = (cursor.shape_data[xor_idx] & bit) != 0;

                    match (and_bit, xor_bit) {
                        (false, false) => {
                            frame[fi] = 0;
                            frame[fi + 1] = 0;
                            frame[fi + 2] = 0;
                        }
                        (false, true) => {
                            frame[fi] = 255;
                            frame[fi + 1] = 255;
                            frame[fi + 2] = 255;
                        }
                        (true, false) => {} // transparent
                        (true, true) => {
                            frame[fi] = !frame[fi];
                            frame[fi + 1] = !frame[fi + 1];
                            frame[fi + 2] = !frame[fi + 2];
                        }
                    }
                }
                _ => {}
            }
        }
    }
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
}

struct ReleaseFrameGuard<'a> {
    duplication: &'a IDXGIOutputDuplication,
    active: bool,
}

impl<'a> ReleaseFrameGuard<'a> {
    fn new(duplication: &'a IDXGIOutputDuplication) -> Self {
        Self {
            duplication,
            active: true,
        }
    }
}

impl Drop for ReleaseFrameGuard<'_> {
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
                let origin = (
                    desc.DesktopCoordinates.left,
                    desc.DesktopCoordinates.top,
                );
                displays.push(SelectedOutput {
                    adapter: adapter.clone(),
                    output: output1,
                    origin,
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
    // DuplicateOutput1 (DXGI 1.6): list both formats so NVIDIA HDR drivers can
    // return FP16 frames when they can't do SDR conversion.
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

    // Fallback: DXGI 1.2.
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
                        let r = f16_to_u8(u16::from_le_bytes([
                            *src_base.add(src),
                            *src_base.add(src + 1),
                        ]));
                        let g = f16_to_u8(u16::from_le_bytes([
                            *src_base.add(src + 2),
                            *src_base.add(src + 3),
                        ]));
                        let b = f16_to_u8(u16::from_le_bytes([
                            *src_base.add(src + 4),
                            *src_base.add(src + 5),
                        ]));
                        let a = f16_to_u8(u16::from_le_bytes([
                            *src_base.add(src + 6),
                            *src_base.add(src + 7),
                        ]));
                        data[dst] = b;
                        data[dst + 1] = g;
                        data[dst + 2] = r;
                        data[dst + 3] = a;
                    }
                }
            }
        }
    }

    Ok((data, dst_stride))
}

#[inline]
fn f16_to_u8(bits: u16) -> u8 {
    let sign = bits >> 15;
    let exp = (bits >> 10) & 0x1f;
    let mantissa = (bits & 0x3ff) as u32;

    if sign != 0 {
        return 0;
    }

    let f: f32 = if exp == 0 {
        mantissa as f32 * (1.0 / (1024.0 * 16384.0))
    } else if exp == 31 {
        return 255;
    } else {
        f32::from_bits(((exp as u32 + 127 - 15) << 23) | (mantissa << 13))
    };

    (f.min(1.0) * 255.0) as u8
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
