#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

#[cfg(target_os = "windows")]
use ::windows::{core::Interface, Win32::Graphics::Direct3D11::ID3D11Texture2D};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use anyhow::bail;
use anyhow::Result;
use yin_yang_proto::packets::{DisplayInfo, RemoteCursorShape, RemoteCursorState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmPixelFormat {
    Xrgb8888,
    Argb8888,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufPixelFormat {
    Xrgb8888,
    Argb8888,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStats {
    pub acquire_wait_us: u32,
    pub convert_us: u32,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Debug, Clone)]
pub enum CursorEvent {
    Shape(RemoteCursorShape),
    State(RemoteCursorState),
}

#[cfg(target_os = "windows")]
#[derive(Clone)]
pub struct D3d11TextureHandle {
    texture: ID3D11Texture2D,
}

#[cfg(target_os = "windows")]
impl D3d11TextureHandle {
    pub(crate) fn new(texture: ID3D11Texture2D) -> Self {
        Self { texture }
    }

    pub(crate) fn as_raw_resource(&self) -> *mut std::ffi::c_void {
        self.texture.as_raw()
    }
}

/// A captured frame ready for encoding.
pub enum CaptureFrame {
    /// Frame data in a CPU-accessible shared memory buffer.
    /// Layout is little-endian XRGB8888 or ARGB8888.
    Shm {
        data: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
        format: ShmPixelFormat,
        timestamp_us: u64,
        stats: CaptureStats,
    },
    /// Frame as a DMA-BUF file descriptor pointing at GPU memory.
    #[cfg(target_os = "linux")]
    DmaBuf {
        fd: OwnedFd,
        buffer_id: u64,
        width: u32,
        height: u32,
        pitch: u32,
        offset: u32,
        allocation_size: u64,
        format: DmabufPixelFormat,
        modifier: u64,
        timestamp_us: u64,
        stats: CaptureStats,
    },
    /// Frame as a persistent D3D11 texture ready for direct NVENC encoding.
    #[cfg(target_os = "windows")]
    D3d11Texture {
        texture: D3d11TextureHandle,
        resource_id: u64,
        width: u32,
        height: u32,
        timestamp_us: u64,
        stats: CaptureStats,
    },
}

#[cfg(target_os = "linux")]
pub mod wayland;

#[cfg(target_os = "windows")]
pub mod windows;

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(target_os = "linux")]
    {
        wayland::list_displays()
    }

    #[cfg(target_os = "windows")]
    {
        windows::list_displays()
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        bail!("display enumeration is only implemented on Linux and Windows");
    }
}
