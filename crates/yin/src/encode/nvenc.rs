//! NVENC encoder — direct SDK usage via bindgen-generated bindings.
//!
//! # Header setup
//!
//! The repo vendors `nvEncodeAPI.h` under
//! `third_party/nv-codec-headers/include/ffnvcodec/` and uses it by default.
//! Set `NVENC_HEADER_PATH` or `NVENC_INCLUDE_DIR` if you need to override that
//! with a different SDK revision.
//!
//! `build.rs` generates `nvenc_bindings.rs` via bindgen and sets the
//! `have_nvenc` cfg flag. This file compiles only when the header was found.
//!
//! # Key latency parameters
//!
//! | Parameter          | Value                                 | Why |
//! |--------------------|---------------------------------------|-----|
//! | presetGUID         | `NV_ENC_PRESET_P1_GUID`               | Fastest preset (Ada Lovelace) |
//! | tuningInfo         | `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`| Disables lookahead, B-frames |
//! | sliceMode          | 3                                     | Row-based slices |
//! | sliceModeData      | 2                                     | 2 slices → emit top half before bottom encodes |
//! | rateControlMode    | `NV_ENC_PARAMS_RC_CBR`                | Predictable bitrate for network pacing |
//! | vbvBufferSize      | `bitrate / fps`                       | 1-frame VBV → no encode stalls |
//! | idrPeriod          | `NVENC_INFINITE_GOPLENGTH`            | IDR only on demand |
//! | repeatSPSPPS       | 1                                     | Every frame carries SPS/PPS |
//!
//! # GPU-direct path (zero CPU copies)
//!
//! Frames arrive as CUDA device pointers from the DMA-BUF capture path:
//! ```text
//! DMA-BUF fd → cuImportExternalMemory → CUdeviceptr
//!           → NvEncRegisterResource(RESOURCE_TYPE_CUDADEVICEPTR)
//!           → NvEncMapInputResource → NV_ENC_INPUT_PTR
//!           → NvEncEncodePicture (encoding happens on the NVENC ASIC)
//!           → NvEncLockBitstream → encoded NAL bytes in CPU-accessible buffer
//! ```
//!
//! On Windows, replace `CUdeviceptr` with `ID3D11Texture2D *` and use
//! `RESOURCE_TYPE_DIRECTX`.

#[cfg(have_nvenc)]
mod ffi {
    #![allow(
        clippy::all,
        dead_code,
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        unnecessary_transmutes,
        unused
    )]
    include!(concat!(env!("OUT_DIR"), "/nvenc_bindings.rs"));
}

use anyhow::{bail, Context, Result};
use tracing::info;

/// Encoded output: one or more H.264/HEVC NAL slice buffers.
/// Each element is a separate slice (enables slice-level decode pipelining).
pub struct EncodedFrame {
    /// Vec of slice payloads in order (slice 0 = top rows, slice 1 = bottom rows).
    pub slices: Vec<Vec<u8>>,
    pub is_keyframe: bool,
    pub timestamp_us: u64,
    /// Encode duration in microseconds (for telemetry).
    pub encode_us: u32,
}

/// Format of an imported CUDA/NVENC input surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencInputFormat {
    Argb,
}

/// Configuration passed to `NvencEncoder::new`.
#[derive(Debug, Clone)]
pub struct NvencConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    /// Target average bitrate in bits per second.
    pub bitrate_bps: u32,
    /// `true` = H.264, `false` = HEVC.
    pub h264: bool,
}

impl NvencConfig {
    /// Recommended LAN configuration for lowest latency.
    pub fn lan_h264(width: u32, height: u32, fps: u8) -> Self {
        // Scale bitrate with the actual pixel rate instead of using one fixed
        // 50 Mbps target for every desktop size. The old fixed setting was
        // acceptable at 1080p60 but too aggressive for higher-resolution
        // desktop content, which can show visible whole-frame quality pumping
        // on even small motion changes.
        let pixels_per_second = u64::from(width) * u64::from(height) * u64::from(fps.max(1));
        let bitrate_bps = match pixels_per_second {
            n if n <= 1920_u64 * 1080 * 60 => 50_000_000,
            n if n <= 2560_u64 * 1440 * 60 => 80_000_000,
            n if n <= 3840_u64 * 2160 * 60 => 140_000_000,
            n if n <= 3840_u64 * 2160 * 120 => 220_000_000,
            _ => 260_000_000,
        };
        Self {
            width,
            height,
            fps,
            bitrate_bps,
            h264: true,
        }
    }

    /// WAN configuration: HEVC for ~40% better compression.
    pub fn wan_hevc(width: u32, height: u32, fps: u8) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_bps: 15_000_000,
            h264: false,
        }
    }

    /// VBV buffer size in bits = bitrate / fps = exactly one frame's worth.
    pub fn vbv_size(&self) -> u32 {
        self.bitrate_bps / self.fps as u32
    }
}

// ---------------------------------------------------------------------------
// Real implementation — compiled only when NVENC headers are present
// ---------------------------------------------------------------------------

#[cfg(have_nvenc)]
pub use real::NvencEncoder;

#[cfg(have_nvenc)]
mod real {
    use super::*;
    use ffi::*;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::ptr;
    use std::time::Instant;

    #[cfg(target_os = "windows")]
    use crate::capture::D3d11TextureHandle;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use libloading::Library;
    #[cfg(target_os = "linux")]
    use std::os::fd::OwnedFd;
    #[cfg(target_os = "windows")]
    use windows::{core::Interface, Win32::Graphics::Direct3D11::ID3D11Device};

    #[cfg(target_os = "linux")]
    mod cuda {
        use anyhow::{bail, Context, Result};
        use libloading::Library;
        use std::ffi::c_void;
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::sync::Arc;
        use tracing::warn;

        type CudaResult = i32;
        type CudaDevice = i32;
        type CudaContextRaw = *mut c_void;
        type CudaExternalMemoryRaw = *mut c_void;
        type CudaDevicePtr = u64;
        type CuInitFn = unsafe extern "C" fn(u32) -> CudaResult;
        type CuDeviceGetFn = unsafe extern "C" fn(*mut CudaDevice, i32) -> CudaResult;
        type CuCtxCreateFn =
            unsafe extern "C" fn(*mut CudaContextRaw, u32, CudaDevice) -> CudaResult;
        type CuCtxDestroyFn = unsafe extern "C" fn(CudaContextRaw) -> CudaResult;
        type CuImportExternalMemoryFn = unsafe extern "C" fn(
            *mut CudaExternalMemoryRaw,
            *const CudaExternalMemoryHandleDesc,
        ) -> CudaResult;
        type CuDestroyExternalMemoryFn = unsafe extern "C" fn(CudaExternalMemoryRaw) -> CudaResult;
        type CuExternalMemoryGetMappedBufferFn = unsafe extern "C" fn(
            *mut CudaDevicePtr,
            CudaExternalMemoryRaw,
            *const CudaExternalMemoryBufferDesc,
        ) -> CudaResult;

        const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 1;

        #[repr(C)]
        struct CudaExternalMemoryHandleDesc {
            r#type: u32,
            handle: CudaExternalMemoryHandleUnion,
            size: u64,
            flags: u32,
            reserved: [u32; 16],
        }

        #[repr(C)]
        union CudaExternalMemoryHandleUnion {
            fd: i32,
            win32: CudaExternalMemoryWin32Handle,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CudaExternalMemoryWin32Handle {
            handle: *mut c_void,
            name: *const c_void,
        }

        #[repr(C)]
        struct CudaExternalMemoryBufferDesc {
            offset: u64,
            size: u64,
            flags: u32,
            reserved: [u32; 16],
        }

        struct DriverApi {
            _library: Library,
            cu_init: CuInitFn,
            cu_device_get: CuDeviceGetFn,
            cu_ctx_create: CuCtxCreateFn,
            cu_ctx_destroy: CuCtxDestroyFn,
            cu_import_external_memory: CuImportExternalMemoryFn,
            cu_destroy_external_memory: CuDestroyExternalMemoryFn,
            cu_external_memory_get_mapped_buffer: CuExternalMemoryGetMappedBufferFn,
        }

        pub struct CudaContext {
            raw: CudaContextRaw,
            driver: Arc<DriverApi>,
        }

        pub struct ExternalMemory {
            raw: CudaExternalMemoryRaw,
            driver: Arc<DriverApi>,
        }

        impl CudaContext {
            pub fn create_default() -> Result<Self> {
                unsafe {
                    let driver = Arc::new(load_driver_api()?);

                    let status = (driver.cu_init)(0);
                    if status != 0 {
                        bail!("cuInit failed: {status}");
                    }

                    let mut device = 0;
                    let status = (driver.cu_device_get)(&mut device, 0);
                    if status != 0 {
                        bail!("cuDeviceGet(0) failed: {status}");
                    }

                    let mut raw = std::ptr::null_mut();
                    let status = (driver.cu_ctx_create)(&mut raw, 0, device);
                    if status != 0 {
                        bail!("cuCtxCreate_v2 failed: {status}");
                    }

                    Ok(Self { raw, driver })
                }
            }

            pub fn as_ptr(&self) -> *mut c_void {
                self.raw
            }

            pub fn import_dmabuf(
                &self,
                fd: OwnedFd,
                allocation_size: u64,
                offset: u64,
                mapping_size: u64,
            ) -> Result<(ExternalMemory, u64)> {
                unsafe {
                    let desc = CudaExternalMemoryHandleDesc {
                        r#type: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
                        handle: CudaExternalMemoryHandleUnion { fd: fd.as_raw_fd() },
                        size: allocation_size,
                        flags: 0,
                        reserved: [0; 16],
                    };

                    let mut raw = std::ptr::null_mut();
                    let status = (self.driver.cu_import_external_memory)(&mut raw, &desc);
                    drop(fd);
                    if status != 0 {
                        bail!("cuImportExternalMemory failed: {status}");
                    }

                    let buffer_desc = CudaExternalMemoryBufferDesc {
                        offset,
                        size: mapping_size,
                        flags: 0,
                        reserved: [0; 16],
                    };
                    let mut mapped_ptr = 0;
                    let status = (self.driver.cu_external_memory_get_mapped_buffer)(
                        &mut mapped_ptr,
                        raw,
                        &buffer_desc,
                    );
                    if status != 0 {
                        let _ = (self.driver.cu_destroy_external_memory)(raw);
                        bail!("cuExternalMemoryGetMappedBuffer failed: {status}");
                    }

                    Ok((
                        ExternalMemory {
                            raw,
                            driver: self.driver.clone(),
                        },
                        mapped_ptr,
                    ))
                }
            }
        }

        fn load_driver_api() -> Result<DriverApi> {
            let candidates = [
                std::path::PathBuf::from("libcuda.so.1"),
                std::path::PathBuf::from("libcuda.so"),
            ];
            let (library, library_name) =
                load_first_library(&candidates).context("load CUDA runtime library for NVENC")?;

            let cu_init = unsafe {
                *library
                    .get::<CuInitFn>(b"cuInit\0")
                    .with_context(|| format!("load cuInit from {}", library_name.display()))?
            };
            let cu_device_get = unsafe {
                *library
                    .get::<CuDeviceGetFn>(b"cuDeviceGet\0")
                    .with_context(|| format!("load cuDeviceGet from {}", library_name.display()))?
            };
            let cu_ctx_create = unsafe {
                *library
                    .get::<CuCtxCreateFn>(b"cuCtxCreate_v2\0")
                    .with_context(|| {
                        format!("load cuCtxCreate_v2 from {}", library_name.display())
                    })?
            };
            let cu_ctx_destroy = unsafe {
                *library
                    .get::<CuCtxDestroyFn>(b"cuCtxDestroy_v2\0")
                    .with_context(|| {
                        format!("load cuCtxDestroy_v2 from {}", library_name.display())
                    })?
            };
            let cu_import_external_memory = unsafe {
                *library
                    .get::<CuImportExternalMemoryFn>(b"cuImportExternalMemory\0")
                    .with_context(|| {
                        format!(
                            "load cuImportExternalMemory from {}",
                            library_name.display()
                        )
                    })?
            };
            let cu_destroy_external_memory = unsafe {
                *library
                    .get::<CuDestroyExternalMemoryFn>(b"cuDestroyExternalMemory\0")
                    .with_context(|| {
                        format!(
                            "load cuDestroyExternalMemory from {}",
                            library_name.display()
                        )
                    })?
            };
            let cu_external_memory_get_mapped_buffer = unsafe {
                *library
                    .get::<CuExternalMemoryGetMappedBufferFn>(b"cuExternalMemoryGetMappedBuffer\0")
                    .with_context(|| {
                        format!(
                            "load cuExternalMemoryGetMappedBuffer from {}",
                            library_name.display()
                        )
                    })?
            };

            Ok(DriverApi {
                _library: library,
                cu_init,
                cu_device_get,
                cu_ctx_create,
                cu_ctx_destroy,
                cu_import_external_memory,
                cu_destroy_external_memory,
                cu_external_memory_get_mapped_buffer,
            })
        }

        fn load_first_library(
            candidates: &[std::path::PathBuf],
        ) -> Result<(Library, std::path::PathBuf)> {
            let mut errors = Vec::new();
            for candidate in candidates {
                match unsafe { Library::new(candidate) } {
                    Ok(library) => return Ok((library, candidate.clone())),
                    Err(err) => errors.push(format!("{}: {err}", candidate.display())),
                }
            }

            bail!(
                "failed to load CUDA runtime library; tried: {}",
                errors.join("; ")
            )
        }

        impl Drop for CudaContext {
            fn drop(&mut self) {
                if self.raw.is_null() {
                    return;
                }

                unsafe {
                    let status = (self.driver.cu_ctx_destroy)(self.raw);
                    if status != 0 {
                        warn!("cuCtxDestroy_v2 failed: {status}");
                    }
                }
            }
        }

        impl Drop for ExternalMemory {
            fn drop(&mut self) {
                if self.raw.is_null() {
                    return;
                }

                unsafe {
                    let status = (self.driver.cu_destroy_external_memory)(self.raw);
                    if status != 0 {
                        warn!("cuDestroyExternalMemory failed: {status}");
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod cuda {
        #[cfg(target_os = "windows")]
        use anyhow::Context;
        use anyhow::{bail, Result};
        #[cfg(target_os = "windows")]
        use libloading::Library;
        use std::ffi::c_void;

        #[cfg(target_os = "windows")]
        type CudaResult = i32;
        #[cfg(target_os = "windows")]
        type CudaDevice = i32;
        #[cfg(target_os = "windows")]
        type CudaContextRaw = *mut c_void;

        #[cfg(target_os = "windows")]
        type CuInitFn = unsafe extern "system" fn(u32) -> CudaResult;
        #[cfg(target_os = "windows")]
        type CuDeviceGetFn = unsafe extern "system" fn(*mut CudaDevice, i32) -> CudaResult;
        #[cfg(target_os = "windows")]
        type CuCtxCreateFn =
            unsafe extern "system" fn(*mut CudaContextRaw, u32, CudaDevice) -> CudaResult;
        #[cfg(target_os = "windows")]
        type CuCtxDestroyFn = unsafe extern "system" fn(CudaContextRaw) -> CudaResult;

        #[allow(dead_code)]
        pub struct ExternalMemory;

        #[cfg(target_os = "windows")]
        struct DriverApi {
            _library: Library,
            cu_init: CuInitFn,
            cu_device_get: CuDeviceGetFn,
            cu_ctx_create: CuCtxCreateFn,
            cu_ctx_destroy: CuCtxDestroyFn,
        }

        #[cfg(target_os = "windows")]
        pub struct CudaContext {
            raw: CudaContextRaw,
            driver: DriverApi,
        }

        #[cfg(not(target_os = "windows"))]
        pub struct CudaContext;

        impl CudaContext {
            pub fn create_default() -> Result<Self> {
                #[cfg(target_os = "windows")]
                unsafe {
                    let library =
                        Library::new("nvcuda.dll").context("load nvcuda.dll for NVENC")?;
                    let cu_init = *library
                        .get::<CuInitFn>(b"cuInit\0")
                        .context("load cuInit from nvcuda.dll")?;
                    let cu_device_get = *library
                        .get::<CuDeviceGetFn>(b"cuDeviceGet\0")
                        .context("load cuDeviceGet from nvcuda.dll")?;
                    let cu_ctx_create = *library
                        .get::<CuCtxCreateFn>(b"cuCtxCreate_v2\0")
                        .context("load cuCtxCreate_v2 from nvcuda.dll")?;
                    let cu_ctx_destroy = *library
                        .get::<CuCtxDestroyFn>(b"cuCtxDestroy_v2\0")
                        .context("load cuCtxDestroy_v2 from nvcuda.dll")?;

                    let driver = DriverApi {
                        _library: library,
                        cu_init,
                        cu_device_get,
                        cu_ctx_create,
                        cu_ctx_destroy,
                    };

                    let status = (driver.cu_init)(0);
                    if status != 0 {
                        bail!("cuInit failed: {status}");
                    }

                    let mut device = 0;
                    let status = (driver.cu_device_get)(&mut device, 0);
                    if status != 0 {
                        bail!("cuDeviceGet(0) failed: {status}");
                    }

                    let mut raw = std::ptr::null_mut();
                    let status = (driver.cu_ctx_create)(&mut raw, 0, device);
                    if status != 0 {
                        bail!("cuCtxCreate_v2 failed: {status}");
                    }

                    Ok(Self { raw, driver })
                }

                #[cfg(not(target_os = "windows"))]
                bail!("automatic CUDA context creation is only implemented on Linux")
            }

            pub fn as_ptr(&self) -> *mut c_void {
                #[cfg(target_os = "windows")]
                {
                    self.raw
                }

                #[cfg(not(target_os = "windows"))]
                {
                    std::ptr::null_mut()
                }
            }
        }

        #[cfg(target_os = "windows")]
        impl Drop for CudaContext {
            fn drop(&mut self) {
                if self.raw.is_null() {
                    return;
                }

                unsafe {
                    let _ = (self.driver.cu_ctx_destroy)(self.raw);
                }
            }
        }

        #[cfg(target_os = "windows")]
        impl ExternalMemory {}
    }

    use cuda::CudaContext;
    #[cfg(target_os = "linux")]
    use cuda::ExternalMemory;

    // ---------------------------------------------------------------------------
    // Version constants — these are C macros so bindgen doesn't emit them.
    // Formula from nvEncodeAPI.h:
    //   NVENCAPI_VERSION = MAJOR | (MINOR << 24)
    //   NVENCAPI_STRUCT_VERSION(ver) = NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
    // ---------------------------------------------------------------------------
    const NVENCAPI_VERSION: u32 = 13u32;
    const fn sv(ver: u32) -> u32 {
        NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
    }

    const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = sv(2);
    const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = sv(1);
    const NV_ENC_CONFIG_VER: u32 = sv(9) | (1u32 << 31);
    const NV_ENC_PRESET_CONFIG_VER: u32 = sv(5) | (1u32 << 31);
    const NV_ENC_INITIALIZE_PARAMS_VER: u32 = sv(7) | (1u32 << 31);
    const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = sv(2) | (1u32 << 31);
    const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = sv(2);
    const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = sv(1);
    #[allow(dead_code)]
    const NV_ENC_REGISTER_RESOURCE_VER: u32 = sv(5);
    const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = sv(1);
    #[allow(dead_code)]
    const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = sv(4);
    const NV_ENC_PIC_PARAMS_VER: u32 = sv(7) | (1u32 << 31);
    const NV_ENC_LOCK_BITSTREAM_VER: u32 = sv(2) | (1u32 << 31);
    const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
    #[allow(clippy::unnecessary_cast)]
    const NV_ENC_PIC_FLAG_FORCEIDR_U32: u32 = NV_ENC_PIC_FLAG_FORCEIDR as u32;

    const NV_ENC_CODEC_H264_GUID_VALUE: GUID = GUID {
        Data1: 0x6bc8_2762,
        Data2: 0x4e63,
        Data3: 0x4ca4,
        Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
    };
    const NV_ENC_CODEC_HEVC_GUID_VALUE: GUID = GUID {
        Data1: 0x790c_dc88,
        Data2: 0x4522,
        Data3: 0x4d7b,
        Data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
    };
    const NV_ENC_PRESET_P1_GUID_VALUE: GUID = GUID {
        Data1: 0xfc0a_8d3e,
        Data2: 0x45f8,
        Data3: 0x4cf8,
        Data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
    };

    // Number of pre-allocated input/output buffer pairs.
    const RING_DEPTH: usize = 3;

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    struct NvencLibrary {
        _library: Library,
    }

    #[cfg(target_os = "windows")]
    enum SessionDevice {
        Cuda { _context: CudaContext },
        D3d11 { _device: ID3D11Device },
    }

    pub struct NvencEncoder {
        encoder: *mut c_void,
        api: NV_ENCODE_API_FUNCTION_LIST,
        config: NvencConfig,
        #[cfg(target_os = "windows")]
        _session_device: SessionDevice,
        #[cfg(not(target_os = "windows"))]
        _cuda_ctx: CudaContext,
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        _nvenc_library: NvencLibrary,
        in_bufs: Vec<NV_ENC_INPUT_PTR>,
        out_bufs: Vec<NV_ENC_OUTPUT_PTR>,
        #[cfg(target_os = "linux")]
        dmabuf_resources: HashMap<u64, DmabufResource>,
        #[cfg(target_os = "windows")]
        d3d11_resources: HashMap<u64, D3d11RegisteredResource>,
        ring_idx: usize,
    }

    unsafe impl Send for NvencEncoder {}

    #[cfg(target_os = "linux")]
    struct DmabufResource {
        external_memory: ExternalMemory,
        registered: usize,
        pitch: u32,
        format: NvencInputFormat,
    }

    #[cfg(target_os = "windows")]
    struct D3d11RegisteredResource {
        texture: D3d11TextureHandle,
        registered: usize,
        format: NvencInputFormat,
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn load_nvenc_api(api: &mut NV_ENCODE_API_FUNCTION_LIST) -> Result<NvencLibrary> {
        #[cfg(target_os = "windows")]
        type CreateInstanceFn =
            unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
        #[cfg(target_os = "linux")]
        type CreateInstanceFn =
            unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

        #[cfg(target_os = "windows")]
        let candidates = [std::path::PathBuf::from("nvEncodeAPI64.dll")];
        #[cfg(target_os = "linux")]
        let candidates = nvenc_library_candidates();

        let (library, library_name) = load_first_library(&candidates)?;
        let create_instance = unsafe {
            *library
                .get::<CreateInstanceFn>(b"NvEncodeAPICreateInstance\0")
                .with_context(|| {
                    format!(
                        "load NvEncodeAPICreateInstance from {}",
                        library_name.display()
                    )
                })?
        };
        let status = unsafe { create_instance(api) };
        if status != NV_ENC_SUCCESS {
            bail!("NvEncodeAPICreateInstance failed: {status:?}");
        }

        Ok(NvencLibrary { _library: library })
    }

    #[cfg(target_os = "linux")]
    fn nvenc_library_candidates() -> Vec<std::path::PathBuf> {
        let mut candidates = Vec::new();
        if let Some(dir) = std::env::var_os("NVENC_LIB_DIR") {
            let dir = std::path::PathBuf::from(dir);
            candidates.push(dir.join("libnvidia-encode.so.1"));
            candidates.push(dir.join("libnvidia-encode.so"));
        }
        candidates.push(std::path::PathBuf::from("libnvidia-encode.so.1"));
        candidates.push(std::path::PathBuf::from("libnvidia-encode.so"));
        candidates
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn load_first_library(
        candidates: &[std::path::PathBuf],
    ) -> Result<(Library, std::path::PathBuf)> {
        let mut errors = Vec::new();
        for candidate in candidates {
            match unsafe { Library::new(candidate) } {
                Ok(library) => return Ok((library, candidate.clone())),
                Err(err) => errors.push(format!("{}: {err}", candidate.display())),
            }
        }

        bail!(
            "failed to load NVENC runtime library; tried: {}",
            errors.join("; ")
        )
    }

    impl NvencEncoder {
        /// Initialise the NVENC encoder.
        pub fn new(config: NvencConfig) -> Result<Self> {
            let device_ctx =
                CudaContext::create_default().context("create CUDA context for NVENC")?;
            Self::with_cuda_context(config, device_ctx)
        }

        /// Mark a previously encoded frame as a lost reference so NVENC routes
        /// subsequent P-frames around it.  `frame_timestamp_us` must match the
        /// `inputTimeStamp` value that was supplied to `NvEncEncodePicture` for
        /// the lost frame.
        ///
        /// After this call the next encoded P-frame will reference only frames
        /// whose timestamps have not been invalidated, producing one visually
        /// imperfect frame followed by a clean stream — without requiring an IDR.
        pub fn invalidate_ref_frame(&mut self, frame_timestamp_us: u64) -> Result<()> {
            let Some(invalidate_fn) = self.api.nvEncInvalidateRefFrames else {
                bail!("nvEncInvalidateRefFrames is not available in the loaded NVENC library");
            };
            let status = unsafe { invalidate_fn(self.encoder, frame_timestamp_us) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncInvalidateRefFrames failed: {status:?}");
            }
            Ok(())
        }

        pub fn reconfigure(&mut self, config: NvencConfig) -> Result<()> {
            if config.width != self.config.width || config.height != self.config.height {
                bail!(
                    "NVENC reconfigure only supports bitrate/FPS changes, not resolution changes"
                );
            }
            if config.h264 != self.config.h264 {
                bail!("NVENC reconfigure only supports bitrate/FPS changes, not codec changes");
            }

            let codec_guid = if config.h264 {
                NV_ENC_CODEC_H264_GUID_VALUE
            } else {
                NV_ENC_CODEC_HEVC_GUID_VALUE
            };
            let preset_p1_guid = NV_ENC_PRESET_P1_GUID_VALUE;

            let mut enc_config = NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            let mut preset_cfg = NV_ENC_PRESET_CONFIG {
                version: NV_ENC_PRESET_CONFIG_VER,
                presetCfg: enc_config,
                ..Default::default()
            };
            let status = unsafe {
                (self.api.nvEncGetEncodePresetConfigEx.unwrap())(
                    self.encoder,
                    codec_guid,
                    preset_p1_guid,
                    NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset_cfg,
                )
            };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncGetEncodePresetConfigEx failed during reconfigure: {status:?}");
            }

            enc_config = preset_cfg.presetCfg;
            enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
            enc_config.rcParams.averageBitRate = config.bitrate_bps;
            enc_config.rcParams.maxBitRate = config.bitrate_bps * 6 / 5;
            enc_config.rcParams.vbvBufferSize = config.vbv_size();
            enc_config.rcParams.vbvInitialDelay = config.vbv_size();
            enc_config.rcParams.set_enableMinQP(1);
            enc_config.rcParams.minQP.qpInterP = 20;
            enc_config.rcParams.minQP.qpInterB = 20;
            enc_config.rcParams.minQP.qpIntra = 20;

            unsafe {
                if config.h264 {
                    enc_config.encodeCodecConfig.h264Config.sliceMode = 3;
                    enc_config.encodeCodecConfig.h264Config.sliceModeData = 2;
                    enc_config.encodeCodecConfig.h264Config.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
                    enc_config.encodeCodecConfig.h264Config.set_outputAUD(0);
                    enc_config.encodeCodecConfig.h264Config.set_disableSPSPPS(0);
                } else {
                    enc_config.encodeCodecConfig.hevcConfig.sliceMode = 3;
                    enc_config.encodeCodecConfig.hevcConfig.sliceModeData = 2;
                    enc_config.encodeCodecConfig.hevcConfig.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
                }
            }

            enc_config.gopLength = NVENC_INFINITE_GOPLENGTH;
            enc_config.frameIntervalP = 1;

            let init_params = NV_ENC_INITIALIZE_PARAMS {
                version: NV_ENC_INITIALIZE_PARAMS_VER,
                encodeGUID: codec_guid,
                presetGUID: preset_p1_guid,
                tuningInfo: NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeWidth: config.width,
                encodeHeight: config.height,
                darWidth: config.width,
                darHeight: config.height,
                frameRateNum: config.fps as u32,
                frameRateDen: 1,
                encodeConfig: &mut enc_config,
                enableEncodeAsync: 0,
                enablePTD: 1,
                ..Default::default()
            };
            let mut reconfigure_params = NV_ENC_RECONFIGURE_PARAMS {
                version: NV_ENC_RECONFIGURE_PARAMS_VER,
                reserved: 0,
                reInitEncodeParams: init_params,
                _bitfield_align_1: [],
                _bitfield_1: NV_ENC_RECONFIGURE_PARAMS::new_bitfield_1(0, 1, 0),
                reserved2: 0,
            };

            let status = unsafe {
                (self.api.nvEncReconfigureEncoder.unwrap())(self.encoder, &mut reconfigure_params)
            };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncReconfigureEncoder failed: {status:?}");
            }

            self.config = config;
            info!(
                "NVENC encoder reconfigured: {} {}x{}@{}fps {:.0}Mbps",
                if self.config.h264 { "H.264" } else { "HEVC" },
                self.config.width,
                self.config.height,
                self.config.fps,
                self.config.bitrate_bps as f64 / 1e6,
            );
            Ok(())
        }

        #[cfg(target_os = "windows")]
        pub fn new_d3d11(config: NvencConfig, device: &ID3D11Device) -> Result<Self> {
            Self::with_d3d11_device(config, device.clone())
        }

        #[cfg(target_os = "windows")]
        pub fn uses_d3d11_input(&self) -> bool {
            matches!(&self._session_device, SessionDevice::D3d11 { .. })
        }

        /// Initialise the NVENC encoder using an existing CUDA context.
        fn with_cuda_context(config: NvencConfig, device_ctx: CudaContext) -> Result<Self> {
            // Load the NVENC entry point
            let mut api = NV_ENCODE_API_FUNCTION_LIST {
                version: NV_ENCODE_API_FUNCTION_LIST_VER,
                ..Default::default()
            };
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let nvenc_library = load_nvenc_api(&mut api)?;

            // Open encoder session on the CUDA device
            let mut open_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: NV_ENC_DEVICE_TYPE_CUDA,
                device: device_ctx.as_ptr(),
                apiVersion: NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut encoder: *mut c_void = ptr::null_mut();
            let status =
                unsafe { (api.nvEncOpenEncodeSessionEx.unwrap())(&mut open_params, &mut encoder) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncOpenEncodeSessionEx failed: {status:?}");
            }

            // Choose codec GUID (extern statics require unsafe to read)
            let codec_guid = if config.h264 {
                NV_ENC_CODEC_H264_GUID_VALUE
            } else {
                NV_ENC_CODEC_HEVC_GUID_VALUE
            };
            let preset_p1_guid = NV_ENC_PRESET_P1_GUID_VALUE;

            // Build encoder config
            let mut enc_config = NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            };

            let preset_config_params = NV_ENC_PRESET_CONFIG {
                version: NV_ENC_PRESET_CONFIG_VER,
                presetCfg: enc_config,
                ..Default::default()
            };

            // Fetch ultra-low-latency preset
            let mut preset_cfg = preset_config_params;
            let status = unsafe {
                (api.nvEncGetEncodePresetConfigEx.unwrap())(
                    encoder,
                    codec_guid,
                    preset_p1_guid,
                    NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset_cfg,
                )
            };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncGetEncodePresetConfigEx failed: {status:?}");
            }

            enc_config = preset_cfg.presetCfg;

            // Override RC and slice parameters for minimum latency
            enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
            enc_config.rcParams.averageBitRate = config.bitrate_bps;
            enc_config.rcParams.maxBitRate = config.bitrate_bps * 6 / 5; // 20% headroom
            enc_config.rcParams.vbvBufferSize = config.vbv_size();
            enc_config.rcParams.vbvInitialDelay = config.vbv_size();
            enc_config.rcParams.set_enableMinQP(1);
            enc_config.rcParams.minQP.qpInterP = 20;
            enc_config.rcParams.minQP.qpInterB = 20;
            enc_config.rcParams.minQP.qpIntra = 20;

            // Union field access requires unsafe
            unsafe {
                if config.h264 {
                    // slice parallelism: 2 slices per frame
                    enc_config.encodeCodecConfig.h264Config.sliceMode = 3;
                    enc_config.encodeCodecConfig.h264Config.sliceModeData = 2;
                    enc_config.encodeCodecConfig.h264Config.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
                    enc_config.encodeCodecConfig.h264Config.set_outputAUD(0);
                    enc_config.encodeCodecConfig.h264Config.set_disableSPSPPS(0);
                } else {
                    enc_config.encodeCodecConfig.hevcConfig.sliceMode = 3;
                    enc_config.encodeCodecConfig.hevcConfig.sliceModeData = 2;
                    enc_config.encodeCodecConfig.hevcConfig.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
                }
            }

            enc_config.gopLength = NVENC_INFINITE_GOPLENGTH;
            enc_config.frameIntervalP = 1; // no B-frames

            // Initialise the encoder
            let mut init_params = NV_ENC_INITIALIZE_PARAMS {
                version: NV_ENC_INITIALIZE_PARAMS_VER,
                encodeGUID: codec_guid,
                presetGUID: preset_p1_guid,
                tuningInfo: NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeWidth: config.width,
                encodeHeight: config.height,
                darWidth: config.width,
                darHeight: config.height,
                frameRateNum: config.fps as u32,
                frameRateDen: 1,
                encodeConfig: &mut enc_config,
                enableEncodeAsync: 0,
                enablePTD: 1, // picture type decision by NVENC
                ..Default::default()
            };
            let status =
                unsafe { (api.nvEncInitializeEncoder.unwrap())(encoder, &mut init_params) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncInitializeEncoder failed: {status:?}");
            }

            info!(
                "NVENC encoder initialised: {} {}x{}@{}fps {:.0}Mbps",
                if config.h264 { "H.264" } else { "HEVC" },
                config.width,
                config.height,
                config.fps,
                config.bitrate_bps as f64 / 1e6,
            );

            // Pre-allocate output bitstream buffers
            let mut out_bufs = Vec::with_capacity(RING_DEPTH);
            for _ in 0..RING_DEPTH {
                let mut create_bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                let status = unsafe {
                    (api.nvEncCreateBitstreamBuffer.unwrap())(encoder, &mut create_bitstream)
                };
                if status != NV_ENC_SUCCESS {
                    bail!("nvEncCreateBitstreamBuffer failed: {status:?}");
                }
                out_bufs.push(create_bitstream.bitstreamBuffer);
            }

            // Pre-allocate ARGB input buffers for SHM/system-memory uploads.
            let mut in_bufs = Vec::with_capacity(RING_DEPTH);
            for _ in 0..RING_DEPTH {
                let mut create_input = NV_ENC_CREATE_INPUT_BUFFER {
                    version: NV_ENC_CREATE_INPUT_BUFFER_VER,
                    width: config.width,
                    height: config.height,
                    bufferFmt: NV_ENC_BUFFER_FORMAT_ARGB,
                    ..Default::default()
                };
                let status =
                    unsafe { (api.nvEncCreateInputBuffer.unwrap())(encoder, &mut create_input) };
                if status != NV_ENC_SUCCESS {
                    bail!("nvEncCreateInputBuffer failed: {status:?}");
                }
                in_bufs.push(create_input.inputBuffer);
            }

            Ok(Self {
                encoder,
                api,
                config,
                #[cfg(target_os = "windows")]
                _session_device: SessionDevice::Cuda {
                    _context: device_ctx,
                },
                #[cfg(not(target_os = "windows"))]
                _cuda_ctx: device_ctx,
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                _nvenc_library: nvenc_library,
                in_bufs,
                out_bufs,
                #[cfg(target_os = "linux")]
                dmabuf_resources: HashMap::new(),
                #[cfg(target_os = "windows")]
                d3d11_resources: HashMap::new(),
                ring_idx: 0,
            })
        }

        #[cfg(target_os = "windows")]
        fn with_d3d11_device(config: NvencConfig, device: ID3D11Device) -> Result<Self> {
            let mut api = NV_ENCODE_API_FUNCTION_LIST {
                version: NV_ENCODE_API_FUNCTION_LIST_VER,
                ..Default::default()
            };
            let nvenc_library = load_nvenc_api(&mut api)?;

            let mut open_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: NV_ENC_DEVICE_TYPE_DIRECTX,
                device: device.as_raw(),
                apiVersion: NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut encoder: *mut c_void = ptr::null_mut();
            let status =
                unsafe { (api.nvEncOpenEncodeSessionEx.unwrap())(&mut open_params, &mut encoder) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncOpenEncodeSessionEx failed for D3D11: {status:?}");
            }

            let codec_guid = if config.h264 {
                NV_ENC_CODEC_H264_GUID_VALUE
            } else {
                NV_ENC_CODEC_HEVC_GUID_VALUE
            };
            let preset_p1_guid = NV_ENC_PRESET_P1_GUID_VALUE;

            let mut enc_config = NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            let preset_config_params = NV_ENC_PRESET_CONFIG {
                version: NV_ENC_PRESET_CONFIG_VER,
                presetCfg: enc_config,
                ..Default::default()
            };
            let mut preset_cfg = preset_config_params;
            let status = unsafe {
                (api.nvEncGetEncodePresetConfigEx.unwrap())(
                    encoder,
                    codec_guid,
                    preset_p1_guid,
                    NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset_cfg,
                )
            };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncGetEncodePresetConfigEx failed: {status:?}");
            }
            enc_config = preset_cfg.presetCfg;

            enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
            enc_config.rcParams.averageBitRate = config.bitrate_bps;
            enc_config.rcParams.maxBitRate = config.bitrate_bps * 6 / 5;
            enc_config.rcParams.vbvBufferSize = config.vbv_size();
            enc_config.rcParams.vbvInitialDelay = config.vbv_size();
            enc_config.rcParams.set_enableMinQP(1);
            enc_config.rcParams.minQP.qpInterP = 20;
            enc_config.rcParams.minQP.qpInterB = 20;
            enc_config.rcParams.minQP.qpIntra = 20;

            unsafe {
                if config.h264 {
                    enc_config.encodeCodecConfig.h264Config.sliceMode = 3;
                    enc_config.encodeCodecConfig.h264Config.sliceModeData = 2;
                    enc_config.encodeCodecConfig.h264Config.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
                    enc_config.encodeCodecConfig.h264Config.set_outputAUD(0);
                    enc_config.encodeCodecConfig.h264Config.set_disableSPSPPS(0);
                } else {
                    enc_config.encodeCodecConfig.hevcConfig.sliceMode = 3;
                    enc_config.encodeCodecConfig.hevcConfig.sliceModeData = 2;
                    enc_config.encodeCodecConfig.hevcConfig.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    enc_config.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
                }
            }

            enc_config.gopLength = NVENC_INFINITE_GOPLENGTH;
            enc_config.frameIntervalP = 1;

            let mut init_params = NV_ENC_INITIALIZE_PARAMS {
                version: NV_ENC_INITIALIZE_PARAMS_VER,
                encodeGUID: codec_guid,
                presetGUID: preset_p1_guid,
                tuningInfo: NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeWidth: config.width,
                encodeHeight: config.height,
                darWidth: config.width,
                darHeight: config.height,
                frameRateNum: config.fps as u32,
                frameRateDen: 1,
                encodeConfig: &mut enc_config,
                enableEncodeAsync: 0,
                enablePTD: 1,
                ..Default::default()
            };
            let status =
                unsafe { (api.nvEncInitializeEncoder.unwrap())(encoder, &mut init_params) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncInitializeEncoder failed: {status:?}");
            }

            info!(
                "NVENC encoder initialised (D3D11): {} {}x{}@{}fps {:.0}Mbps",
                if config.h264 { "H.264" } else { "HEVC" },
                config.width,
                config.height,
                config.fps,
                config.bitrate_bps as f64 / 1e6,
            );

            let mut out_bufs = Vec::with_capacity(RING_DEPTH);
            for _ in 0..RING_DEPTH {
                let mut create_bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                let status = unsafe {
                    (api.nvEncCreateBitstreamBuffer.unwrap())(encoder, &mut create_bitstream)
                };
                if status != NV_ENC_SUCCESS {
                    bail!("nvEncCreateBitstreamBuffer failed: {status:?}");
                }
                out_bufs.push(create_bitstream.bitstreamBuffer);
            }

            Ok(Self {
                encoder,
                api,
                config,
                _session_device: SessionDevice::D3d11 { _device: device },
                _nvenc_library: nvenc_library,
                in_bufs: Vec::new(),
                out_bufs,
                d3d11_resources: HashMap::new(),
                ring_idx: 0,
            })
        }

        #[cfg(target_os = "linux")]
        /// Register a CUDA device pointer as an NVENC input resource.
        /// Returns an opaque handle to be passed to `encode_frame`.
        ///
        /// Call once per DMA-BUF-imported CUDA buffer at session start.
        fn register_cuda_resource(
            &mut self,
            cuda_ptr: u64, // CUdeviceptr
            pitch: u32,
            format: NvencInputFormat,
        ) -> Result<usize> {
            let mut reg = NV_ENC_REGISTER_RESOURCE {
                version: NV_ENC_REGISTER_RESOURCE_VER,
                resourceType: NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                width: self.config.width,
                height: self.config.height,
                pitch,
                resourceToRegister: cuda_ptr as *mut c_void,
                bufferFormat: nvenc_buffer_format(format),
                ..Default::default()
            };
            let status =
                unsafe { (self.api.nvEncRegisterResource.unwrap())(self.encoder, &mut reg) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncRegisterResource failed: {status:?}");
            }
            Ok(reg.registeredResource as usize)
        }

        #[cfg(target_os = "linux")]
        pub fn register_dmabuf_argb_resource(
            &mut self,
            resource_id: u64,
            fd: OwnedFd,
            allocation_size: u64,
            offset: u64,
            mapping_size: u64,
            pitch: u32,
        ) -> Result<()> {
            if self.dmabuf_resources.contains_key(&resource_id) {
                return Ok(());
            }

            let (external_memory, cuda_ptr) = self
                ._cuda_ctx
                .import_dmabuf(fd, allocation_size, offset, mapping_size)
                .context("import DMA-BUF into CUDA")?;
            let registered = self
                .register_cuda_resource(cuda_ptr, pitch, NvencInputFormat::Argb)
                .context("register imported DMA-BUF with NVENC")?;

            self.dmabuf_resources.insert(
                resource_id,
                DmabufResource {
                    external_memory,
                    registered,
                    pitch,
                    format: NvencInputFormat::Argb,
                },
            );
            Ok(())
        }

        #[cfg(target_os = "linux")]
        pub fn encode_registered_dmabuf(
            &mut self,
            resource_id: u64,
            timestamp_us: u64,
            force_idr: bool,
        ) -> Result<EncodedFrame> {
            let (registered, pitch, format) = self
                .dmabuf_resources
                .get(&resource_id)
                .map(|resource| (resource.registered, resource.pitch, resource.format))
                .context("attempted to encode an unregistered DMA-BUF resource")?;
            self.encode_registered_resource(registered, pitch, format, timestamp_us, force_idr)
        }

        #[cfg(target_os = "linux")]
        fn encode_registered_resource(
            &mut self,
            registered: usize,
            pitch: u32,
            format: NvencInputFormat,
            timestamp_us: u64,
            force_idr: bool,
        ) -> Result<EncodedFrame> {
            let t0 = Instant::now();
            let ring = self.ring_idx % RING_DEPTH;
            self.ring_idx += 1;

            // Map the registered resource to get an NV_ENC_INPUT_PTR
            let mut map = NV_ENC_MAP_INPUT_RESOURCE {
                version: NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: registered as NV_ENC_REGISTERED_PTR,
                ..Default::default()
            };
            let status =
                unsafe { (self.api.nvEncMapInputResource.unwrap())(self.encoder, &mut map) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncMapInputResource failed: {status:?}");
            }
            let input_ptr = map.mappedResource;

            // Submit the frame for encoding
            let pic_params = NV_ENC_PIC_PARAMS {
                version: NV_ENC_PIC_PARAMS_VER,
                inputBuffer: input_ptr,
                outputBitstream: self.out_bufs[ring],
                inputWidth: self.config.width,
                inputHeight: self.config.height,
                inputPitch: pitch,
                encodePicFlags: if force_idr {
                    NV_ENC_PIC_FLAG_FORCEIDR_U32
                } else {
                    0
                },
                inputTimeStamp: timestamp_us,
                pictureStruct: NV_ENC_PIC_STRUCT_FRAME,
                bufferFmt: nvenc_buffer_format(format),
                ..Default::default()
            };

            let status = unsafe {
                (self.api.nvEncEncodePicture.unwrap())(self.encoder, &mut { pic_params })
            };
            if status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT {
                // Unmap before bailing
                let _ =
                    unsafe { (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, input_ptr) };
                bail!("nvEncEncodePicture failed: {status:?}");
            }

            // Lock and read the output bitstream
            let mut lock = NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.out_bufs[ring],
                ..Default::default()
            };
            let status = unsafe { (self.api.nvEncLockBitstream.unwrap())(self.encoder, &mut lock) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncLockBitstream failed: {status:?}");
            }

            let is_keyframe =
                lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;

            // Copy bitstream bytes out of the locked buffer
            let bitstream = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            };
            let frame_data = bitstream.to_vec();

            unsafe {
                (self.api.nvEncUnlockBitstream.unwrap())(self.encoder, self.out_bufs[ring]);
                (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, input_ptr);
            }

            let encode_us = t0.elapsed().as_micros() as u32;

            // Split into slices: scan for NAL start codes (00 00 00 01)
            // Each slice boundary is a new NAL with nal_unit_type = slice_layer_without_partitioning
            let slices = split_into_slices(&frame_data);

            Ok(EncodedFrame {
                slices,
                is_keyframe,
                timestamp_us,
                encode_us,
            })
        }

        #[cfg(target_os = "windows")]
        fn register_directx_resource(
            &mut self,
            texture: &D3d11TextureHandle,
            format: NvencInputFormat,
        ) -> Result<usize> {
            let mut reg = NV_ENC_REGISTER_RESOURCE {
                version: NV_ENC_REGISTER_RESOURCE_VER,
                resourceType: NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                width: self.config.width,
                height: self.config.height,
                pitch: 0,
                subResourceIndex: 0,
                resourceToRegister: texture.as_raw_resource(),
                bufferFormat: nvenc_buffer_format(format),
                bufferUsage: NV_ENC_INPUT_IMAGE,
                ..Default::default()
            };
            let status =
                unsafe { (self.api.nvEncRegisterResource.unwrap())(self.encoder, &mut reg) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncRegisterResource failed for D3D11 texture: {status:?}");
            }
            Ok(reg.registeredResource as usize)
        }

        #[cfg(target_os = "windows")]
        pub fn encode_d3d11_texture(
            &mut self,
            texture: &D3d11TextureHandle,
            resource_id: u64,
            timestamp_us: u64,
            force_idr: bool,
        ) -> Result<EncodedFrame> {
            let (registered, format) =
                if let Some(resource) = self.d3d11_resources.get(&resource_id) {
                    (resource.registered, resource.format)
                } else {
                    let registered = self
                        .register_directx_resource(texture, NvencInputFormat::Argb)
                        .context("register D3D11 texture with NVENC")?;
                    self.d3d11_resources.insert(
                        resource_id,
                        D3d11RegisteredResource {
                            texture: texture.clone(),
                            registered,
                            format: NvencInputFormat::Argb,
                        },
                    );
                    (registered, NvencInputFormat::Argb)
                };

            let t0 = Instant::now();
            let ring = self.ring_idx % RING_DEPTH;
            self.ring_idx += 1;

            let mut map = NV_ENC_MAP_INPUT_RESOURCE {
                version: NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: registered as NV_ENC_REGISTERED_PTR,
                ..Default::default()
            };
            let status =
                unsafe { (self.api.nvEncMapInputResource.unwrap())(self.encoder, &mut map) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncMapInputResource failed for D3D11 texture: {status:?}");
            }
            let input_ptr = map.mappedResource;

            let pic_params = NV_ENC_PIC_PARAMS {
                version: NV_ENC_PIC_PARAMS_VER,
                inputBuffer: input_ptr,
                outputBitstream: self.out_bufs[ring],
                inputWidth: self.config.width,
                inputHeight: self.config.height,
                inputPitch: self.config.width,
                encodePicFlags: if force_idr {
                    NV_ENC_PIC_FLAG_FORCEIDR_U32
                } else {
                    0
                },
                inputTimeStamp: timestamp_us,
                pictureStruct: NV_ENC_PIC_STRUCT_FRAME,
                bufferFmt: nvenc_buffer_format(format),
                ..Default::default()
            };

            let status = unsafe {
                (self.api.nvEncEncodePicture.unwrap())(self.encoder, &mut { pic_params })
            };
            if status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT {
                let _ =
                    unsafe { (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, input_ptr) };
                bail!("nvEncEncodePicture failed for D3D11 texture: {status:?}");
            }

            let mut lock = NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.out_bufs[ring],
                ..Default::default()
            };
            let status = unsafe { (self.api.nvEncLockBitstream.unwrap())(self.encoder, &mut lock) };
            if status != NV_ENC_SUCCESS {
                let _ =
                    unsafe { (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, input_ptr) };
                bail!("nvEncLockBitstream failed: {status:?}");
            }

            let is_keyframe =
                lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;
            let bitstream = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            };
            let frame_data = bitstream.to_vec();

            unsafe {
                (self.api.nvEncUnlockBitstream.unwrap())(self.encoder, self.out_bufs[ring]);
                (self.api.nvEncUnmapInputResource.unwrap())(self.encoder, input_ptr);
            }

            let encode_us = t0.elapsed().as_micros() as u32;
            let slices = split_into_slices(&frame_data);

            Ok(EncodedFrame {
                slices,
                is_keyframe,
                timestamp_us,
                encode_us,
            })
        }

        /// Encode one CPU-accessible ARGB/XRGB frame copied from Wayland SHM.
        pub fn encode_argb_frame(
            &mut self,
            data: &[u8],
            stride: u32,
            timestamp_us: u64,
            force_idr: bool,
        ) -> Result<EncodedFrame> {
            if self.in_bufs.is_empty() {
                bail!("this NVENC session was initialised for D3D11 direct input");
            }

            let t0 = Instant::now();
            let ring = self.ring_idx % RING_DEPTH;
            self.ring_idx += 1;

            let input_buffer = self.in_bufs[ring];
            let mut lock = NV_ENC_LOCK_INPUT_BUFFER {
                version: NV_ENC_LOCK_INPUT_BUFFER_VER,
                inputBuffer: input_buffer,
                ..Default::default()
            };
            let status =
                unsafe { (self.api.nvEncLockInputBuffer.unwrap())(self.encoder, &mut lock) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncLockInputBuffer failed: {status:?}");
            }

            let copy_result = copy_argb_frame(
                data,
                stride,
                self.config.width,
                self.config.height,
                lock.bufferDataPtr,
                lock.pitch,
            );

            let unlock_status =
                unsafe { (self.api.nvEncUnlockInputBuffer.unwrap())(self.encoder, input_buffer) };
            if unlock_status != NV_ENC_SUCCESS {
                bail!("nvEncUnlockInputBuffer failed: {unlock_status:?}");
            }
            copy_result?;

            let pic_params = NV_ENC_PIC_PARAMS {
                version: NV_ENC_PIC_PARAMS_VER,
                inputBuffer: input_buffer,
                outputBitstream: self.out_bufs[ring],
                inputWidth: self.config.width,
                inputHeight: self.config.height,
                inputPitch: lock.pitch,
                encodePicFlags: if force_idr {
                    NV_ENC_PIC_FLAG_FORCEIDR_U32
                } else {
                    0
                },
                inputTimeStamp: timestamp_us,
                pictureStruct: NV_ENC_PIC_STRUCT_FRAME,
                bufferFmt: NV_ENC_BUFFER_FORMAT_ARGB,
                ..Default::default()
            };

            let status = unsafe {
                (self.api.nvEncEncodePicture.unwrap())(self.encoder, &mut { pic_params })
            };
            if status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT {
                bail!("nvEncEncodePicture failed: {status:?}");
            }

            let mut lock = NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.out_bufs[ring],
                ..Default::default()
            };
            let status = unsafe { (self.api.nvEncLockBitstream.unwrap())(self.encoder, &mut lock) };
            if status != NV_ENC_SUCCESS {
                bail!("nvEncLockBitstream failed: {status:?}");
            }

            let is_keyframe =
                lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;

            let bitstream = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            };
            let frame_data = bitstream.to_vec();

            unsafe {
                (self.api.nvEncUnlockBitstream.unwrap())(self.encoder, self.out_bufs[ring]);
            }

            let encode_us = t0.elapsed().as_micros() as u32;
            let slices = split_into_slices(&frame_data);

            Ok(EncodedFrame {
                slices,
                is_keyframe,
                timestamp_us,
                encode_us,
            })
        }
    }

    impl Drop for NvencEncoder {
        fn drop(&mut self) {
            unsafe {
                #[cfg(target_os = "linux")]
                for (_, resource) in self.dmabuf_resources.drain() {
                    let _ = (self.api.nvEncUnregisterResource.unwrap())(
                        self.encoder,
                        resource.registered as NV_ENC_REGISTERED_PTR,
                    );
                    drop(resource.external_memory);
                }
                #[cfg(target_os = "windows")]
                for (_, resource) in self.d3d11_resources.drain() {
                    let _ = (self.api.nvEncUnregisterResource.unwrap())(
                        self.encoder,
                        resource.registered as NV_ENC_REGISTERED_PTR,
                    );
                    drop(resource.texture);
                }
                for &buf in &self.in_bufs {
                    (self.api.nvEncDestroyInputBuffer.unwrap())(self.encoder, buf);
                }
                for &buf in &self.out_bufs {
                    (self.api.nvEncDestroyBitstreamBuffer.unwrap())(self.encoder, buf);
                }
                (self.api.nvEncDestroyEncoder.unwrap())(self.encoder);
            }
        }
    }

    fn nvenc_buffer_format(format: NvencInputFormat) -> NV_ENC_BUFFER_FORMAT {
        match format {
            NvencInputFormat::Argb => NV_ENC_BUFFER_FORMAT_ARGB,
        }
    }

    fn copy_argb_frame(
        data: &[u8],
        src_stride: u32,
        width: u32,
        height: u32,
        dst_ptr: *mut c_void,
        dst_stride: u32,
    ) -> Result<()> {
        let bytes_per_row = width.checked_mul(4).context("ARGB row size overflow")? as usize;
        let src_stride = usize::try_from(src_stride).context("source stride overflow")?;
        let dst_stride = usize::try_from(dst_stride).context("destination stride overflow")?;
        let height = usize::try_from(height).context("frame height overflow")?;

        if dst_ptr.is_null() {
            bail!("NVENC returned a null input buffer mapping");
        }
        if src_stride < bytes_per_row {
            bail!("Wayland SHM stride {src_stride} is smaller than {bytes_per_row}");
        }
        if dst_stride < bytes_per_row {
            bail!("NVENC input pitch {dst_stride} is smaller than {bytes_per_row}");
        }

        let required_len = src_stride
            .checked_mul(height)
            .context("source frame size overflow")?;
        if data.len() < required_len {
            bail!(
                "Wayland SHM frame too small: got {} bytes, need {required_len}",
                data.len()
            );
        }

        let dst = dst_ptr.cast::<u8>();
        for row in 0..height {
            let src_offset = row * src_stride;
            let dst_offset = row * dst_stride;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr().add(src_offset),
                    dst.add(dst_offset),
                    bytes_per_row,
                );
            }
        }

        Ok(())
    }

    /// Split a contiguous NAL buffer into individual slice payloads.
    /// Uses Annex-B start codes (00 00 01 or 00 00 00 01) as delimiters.
    fn split_into_slices(data: &[u8]) -> Vec<Vec<u8>> {
        let mut slices: Vec<Vec<u8>> = Vec::new();
        let mut start = 0usize;
        let mut i = 0usize;

        while i + 3 < data.len() {
            let is_start_code = (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
                || (i + 4 < data.len()
                    && data[i] == 0
                    && data[i + 1] == 0
                    && data[i + 2] == 0
                    && data[i + 3] == 1);

            if is_start_code && i > start {
                // Check if the following NAL type is a slice (VCL)
                let nal_offset = if data[i + 2] == 1 { i + 3 } else { i + 4 };
                if nal_offset < data.len() {
                    let nal_type = data[nal_offset] & 0x1F;
                    // H.264 VCL NAL types: 1 (non-IDR), 5 (IDR)
                    // Only start a new slice entry at VCL boundaries
                    if nal_type == 1 || nal_type == 5 {
                        slices.push(data[start..i].to_vec());
                        start = i;
                    }
                }
            }
            i += 1;
        }
        if start < data.len() {
            slices.push(data[start..].to_vec());
        }
        if slices.is_empty() {
            slices.push(data.to_vec());
        }
        slices
    }
}

// ---------------------------------------------------------------------------
// Stub — used when NVENC headers are not installed
// ---------------------------------------------------------------------------

#[cfg(not(have_nvenc))]
pub struct NvencEncoder;

#[cfg(not(have_nvenc))]
impl NvencEncoder {
    pub fn new(_config: NvencConfig) -> Result<Self> {
        anyhow::bail!(
            "NVENC not available — nvEncodeAPI.h could not be located.\n\
             The repo normally vendors it under third_party/nv-codec-headers.\n\
             Override with NVENC_HEADER_PATH or NVENC_INCLUDE_DIR and rebuild."
        )
    }

    pub fn reconfigure(&mut self, _config: NvencConfig) -> Result<()> {
        anyhow::bail!("NVENC not available")
    }

    pub fn invalidate_ref_frame(&mut self, _frame_timestamp_us: u64) -> Result<()> {
        anyhow::bail!("NVENC not available")
    }

    #[cfg(target_os = "windows")]
    pub fn new_d3d11(
        _config: NvencConfig,
        _device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    ) -> Result<Self> {
        anyhow::bail!("NVENC not available")
    }

    #[cfg(target_os = "windows")]
    pub fn uses_d3d11_input(&self) -> bool {
        false
    }

    pub fn encode_argb_frame(
        &mut self,
        _data: &[u8],
        _stride: u32,
        _timestamp_us: u64,
        _force_idr: bool,
    ) -> Result<EncodedFrame> {
        anyhow::bail!("NVENC not available")
    }

    #[cfg(target_os = "windows")]
    pub fn encode_d3d11_texture(
        &mut self,
        _texture: &crate::capture::D3d11TextureHandle,
        _resource_id: u64,
        _timestamp_us: u64,
        _force_idr: bool,
    ) -> Result<EncodedFrame> {
        anyhow::bail!("NVENC not available")
    }

    #[cfg(target_os = "linux")]
    pub fn register_dmabuf_argb_resource(
        &mut self,
        _resource_id: u64,
        _fd: std::os::fd::OwnedFd,
        _allocation_size: u64,
        _offset: u64,
        _mapping_size: u64,
        _pitch: u32,
    ) -> Result<()> {
        anyhow::bail!("NVENC not available")
    }

    #[cfg(target_os = "linux")]
    pub fn encode_registered_dmabuf(
        &mut self,
        _resource_id: u64,
        _timestamp_us: u64,
        _force_idr: bool,
    ) -> Result<EncodedFrame> {
        anyhow::bail!("NVENC not available")
    }
}
