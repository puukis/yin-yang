//! On-wire packet definitions for the streamd video and input channels.
//!
//! Video frames are split into fragments and sent over raw UDP.
//! Input events travel over a QUIC unidirectional stream.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Video transport
// ---------------------------------------------------------------------------

/// Header prepended to every UDP video datagram. 16 bytes.
///
/// A single compressed frame may be split into multiple fragments.
/// Slices allow the decoder to start on the first slice before the second
/// has been encoded (NVENC sliceMode=3).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct VideoPacketHeader {
    /// Monotonically increasing frame counter (wraps at u32::MAX).
    pub frame_seq: u32,
    /// Microseconds since Unix epoch at encode-complete time.
    pub timestamp_us: u64,
    /// Which slice of the frame this fragment belongs to (0-indexed).
    pub slice_idx: u8,
    /// Bitfield of frame flags.
    pub flags: VideoFlags,
    /// Fragment index within the current slice (0-indexed).
    pub frag_idx: u16,
    /// Total number of fragments in the current slice.
    pub frag_total: u16,
}

impl VideoPacketHeader {
    pub const SIZE: usize = 18; // serialized via bincode (little-endian)

    pub fn is_keyframe(&self) -> bool {
        self.flags.contains(VideoFlags::KEY_FRAME)
    }

    pub fn is_last_slice(&self) -> bool {
        self.flags.contains(VideoFlags::LAST_SLICE)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoFlags: u8 {
        /// This frame is an IDR (keyframe).
        const KEY_FRAME  = 0b0000_0001;
        /// This is the final slice in the frame.
        const LAST_SLICE = 0b0000_0010;
    }
}

// ---------------------------------------------------------------------------
// Input events
// ---------------------------------------------------------------------------

/// A single input event sent from the Mac client to the server.
/// Serialized with bincode, prefixed by an 8-byte timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputPacket {
    /// Microseconds since Unix epoch when the event was captured.
    pub timestamp_us: u64,
    pub event: InputEvent,
}

/// All input event variants forwarded from client to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative mouse movement (delta pixels).
    MouseMove { dx: i16, dy: i16 },
    /// Mouse button press/release.
    MouseButton { button: MouseButton, pressed: bool },
    /// Scroll wheel.
    MouseScroll { dx: f32, dy: f32 },
    /// Keyboard key press/release using USB HID usage codes.
    /// Using HID codes ensures layout-independence between Mac and Linux/Windows.
    KeyEvent {
        hid_usage: u32,
        pressed: bool,
        modifiers: KeyModifiers,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Side(u8),
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct KeyModifiers: u32 {
        const SHIFT   = 1 << 0;
        const CTRL    = 1 << 1;
        const ALT     = 1 << 2;
        const META    = 1 << 3;
        const CAPS    = 1 << 4;
        const NUMLOCK = 1 << 5;
    }
}

// ---------------------------------------------------------------------------
// QUIC control messages (stream #0)
// ---------------------------------------------------------------------------

/// Sent by the client immediately after QUIC connection is established.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    /// Protocol version — must match server.
    pub version: u32,
    pub max_fps: u8,
    pub width: u32,
    pub height: u32,
    /// Ordered preference list; server picks the first it supports.
    pub preferred_codecs: Vec<Codec>,
    /// Stable server-provided display identifier to capture.
    /// If omitted, the server chooses a default display.
    pub display_id: Option<String>,
}

/// Server response to `SessionRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAccept {
    pub codec: Codec,
    pub fps: u8,
    pub width: u32,
    pub height: u32,
    /// UDP port the server will send video to (on the same address).
    pub video_udp_port: u16,
    /// UDP port the client should bind for receiving video.
    pub client_video_udp_port: u16,
    /// The display the server selected for this session.
    pub selected_display: DisplayInfo,
}

/// Server rejection (bad version, unsupported codec, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReject {
    pub reason: String,
}

/// Codec identifier, ordered from lowest to highest latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// A display/output currently available on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Stable machine-readable identifier used in `SessionRequest.display_id`.
    pub id: String,
    /// Display ordinal in the server-provided list.
    pub index: u32,
    /// Short name for the display when the platform exposes one.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Current pixel width of the display.
    pub width: u32,
    /// Current pixel height of the display.
    pub height: u32,
}

/// Control message envelope — all messages on QUIC stream #0 use this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Ask the server for the currently available capture displays.
    QueryDisplays,
    /// Server response to `QueryDisplays`.
    AvailableDisplays(Vec<DisplayInfo>),
    SessionRequest(SessionRequest),
    SessionAccept(SessionAccept),
    SessionReject(SessionReject),
    /// Client requests an immediate IDR keyframe (e.g. after packet loss).
    RequestIdr,
    /// Heartbeat with server-side telemetry.
    Heartbeat(ServerTelemetry),
    /// Graceful shutdown.
    Goodbye,
}

/// Per-heartbeat telemetry stamped by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTelemetry {
    /// Average encode time in microseconds over the last second.
    pub avg_encode_us: u32,
    /// Current UDP send queue depth in frames.
    pub send_queue_frames: u8,
    /// Number of IDR frames sent in the last second.
    pub idr_count: u8,
}

pub const PROTOCOL_VERSION: u32 = 2;

/// Parse a `VideoPacketHeader` from the first 18 bytes of a UDP datagram.
/// Returns `(header, remaining_payload)` on success.
pub fn parse_video_header(buf: &[u8]) -> Option<(VideoPacketHeader, &[u8])> {
    if buf.len() < 18 {
        return None;
    }
    let frame_seq = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let timestamp_us = u64::from_le_bytes(buf[4..12].try_into().unwrap());
    let slice_idx = buf[12];
    let flags = VideoFlags::from_bits_truncate(buf[13]);
    let frag_idx = u16::from_le_bytes(buf[14..16].try_into().unwrap());
    let frag_total = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    Some((
        VideoPacketHeader {
            frame_seq,
            timestamp_us,
            slice_idx,
            flags,
            frag_idx,
            frag_total,
        },
        &buf[18..],
    ))
}

/// Maximum UDP payload for internet-safe packets.
pub const MTU_WAN: usize = 1400;
/// Maximum UDP payload when jumbo frames are available (LAN).
pub const MTU_LAN: usize = 8900;
