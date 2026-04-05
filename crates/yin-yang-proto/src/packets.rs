//! On-wire packet definitions for the Yin-Yang video and input channels.
//!
//! Video frames are split into fragments and delivered as QUIC unreliable
//! datagrams on the shared control connection (see `VideoTransport`).
//! Input events travel over a QUIC unidirectional stream.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Video transport
// ---------------------------------------------------------------------------

/// Header prepended to every video datagram fragment. 24 bytes, fixed layout.
///
/// A single compressed frame may be split into multiple fragments because QUIC
/// datagram payloads are bounded by the path MTU. Slices allow the decoder to
/// start on the first slice while the second is still in flight (NVENC
/// sliceMode=3). Each slice is additionally protected with XOR parity FEC over
/// fixed-size groups of data fragments so the receiver can recover one lost
/// fragment per group without waiting for an IDR.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct VideoPacketHeader {
    /// Monotonically increasing frame counter (wraps at u32::MAX).
    pub frame_seq: u32,
    /// Microseconds since Unix epoch at encode-complete time.
    pub timestamp_us: u64,
    /// Which slice of the frame this packet belongs to (0-indexed).
    pub slice_idx: u8,
    /// Bitfield of frame and packet flags.
    pub flags: VideoFlags,
    /// Data-fragment index within the current slice (0-indexed) for normal
    /// packets, or the FEC group index for parity packets.
    pub frag_idx: u16,
    /// Total number of data fragments in the current slice.
    pub frag_total: u16,
    /// Total compressed payload bytes in the current slice.
    pub slice_len: u32,
    /// Payload size used for all non-final data fragments in the slice.
    pub frag_payload_size: u16,
}

impl VideoPacketHeader {
    pub const SIZE: usize = 24; // serialized as fixed little-endian bytes

    pub fn is_keyframe(&self) -> bool {
        self.flags.contains(VideoFlags::KEY_FRAME)
    }

    pub fn is_last_slice(&self) -> bool {
        self.flags.contains(VideoFlags::LAST_SLICE)
    }

    pub fn is_fec_parity(&self) -> bool {
        self.flags.contains(VideoFlags::FEC_PARITY)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoFlags: u8 {
        /// This frame is an IDR (keyframe).
        const KEY_FRAME  = 0b0000_0001;
        /// This packet belongs to the final slice in the frame.
        const LAST_SLICE = 0b0000_0010;
        /// This packet carries XOR parity bytes rather than encoded video data.
        const FEC_PARITY = 0b0000_0100;
    }
}

/// Number of data fragments protected by one XOR parity datagram.
///
/// Each parity packet can recover any single lost data fragment within its
/// group. A group size of 4 keeps overhead at 25% while still repairing the
/// most common isolated datagram loss pattern.
pub const VIDEO_FEC_DATA_SHARDS: usize = 4;

/// How the server delivers video frames to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VideoTransport {
    /// Video fragments are sent as QUIC unreliable datagrams on the existing
    /// control connection. No additional port or socket is required on either
    /// end — the client-initiated QUIC connection handles NAT traversal for
    /// both directions.
    QuicDatagram,
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
    /// Stable logical client session identifier reused across reconnects.
    pub client_session_id: String,
    /// Whether the server may adapt bitrate/FPS during the session.
    pub adaptive_streaming: bool,
    pub max_fps: u8,
    pub min_fps: u8,
    /// Zero means "use the server default floor".
    pub min_bitrate_bps: u32,
    /// Zero means "use the server preset ceiling".
    pub max_bitrate_bps: u32,
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
    /// How the server will deliver video to the client.
    pub video_transport: VideoTransport,
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
    /// Client-side rolling telemetry used by the server adaptation loop.
    ClientTelemetry(ClientTelemetry),
    /// Client requests an immediate IDR keyframe (e.g. after packet loss).
    RequestIdr,
    /// Heartbeat with server-side telemetry.
    Heartbeat(ServerTelemetry),
    /// Reliable cursor shape update from the server.
    CursorShape(RemoteCursorShape),
    /// Cursor state update. Usually delivered as a QUIC datagram, with this
    /// stream variant available as a compatibility fallback.
    CursorState(RemoteCursorState),
    /// Graceful shutdown.
    Goodbye,
    /// Client reports that `frame_seq` was unrecoverably lost.
    ///
    /// The server responds by calling `NvEncInvalidateRefFrames` for the
    /// matching encoded frame so subsequent P-frames reference only clean
    /// frames, avoiding a full IDR cycle.
    FrameLost(u32),
}

/// Per-window telemetry stamped by the client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTelemetry {
    /// Number of unrecoverable frames dropped by the reassembler.
    pub unrecoverable_frames: u32,
    /// Number of frames that required FEC repair before decode.
    pub recovered_frames: u32,
    /// Number of individual fragments recovered by FEC.
    pub recovered_fragments: u32,
    /// Number of frames presented by the renderer.
    pub presented_frames: u32,
    /// Number of frames dropped by the renderer while keeping latency low.
    pub render_dropped_frames: u32,
    /// Average time from datagram reassembly to decode submission.
    pub avg_decode_queue_us: u32,
    /// Average time from decode completion to render submission.
    pub avg_render_queue_us: u32,
}

/// Per-heartbeat telemetry stamped by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTelemetry {
    /// Average time spent waiting for/obtaining a frame from capture.
    pub avg_capture_wait_us: u32,
    /// Average time spent converting/preparing the frame for encode.
    pub avg_capture_convert_us: u32,
    /// Average encode time in microseconds over the last second.
    pub avg_encode_us: u32,
    /// Average time spent packetising and sending the frame.
    pub avg_send_us: u32,
    /// Average time spent waiting for QUIC datagram send buffer space.
    pub avg_send_wait_us: u32,
    /// Maximum time spent waiting for QUIC datagram send buffer space.
    pub max_send_wait_us: u32,
    /// Average total pipeline time per frame.
    pub avg_pipeline_us: u32,
    /// Current send queue depth in frames.
    pub send_queue_frames: u8,
    /// Number of IDR frames sent in the last second.
    pub idr_count: u8,
    /// Number of frames processed in the last telemetry window.
    pub frame_count: u32,
    /// Current encoder bitrate target in bits per second.
    pub encoder_bitrate_bps: u32,
    /// Current target framerate for capture/encode pacing.
    pub target_fps: u8,
    /// Number of video datagrams successfully sent in the last telemetry window.
    pub video_datagrams_sent: u32,
    /// Number of video datagrams dropped after a failed send in the last telemetry window.
    pub video_datagrams_dropped: u32,
    /// Effective maximum QUIC datagram size used for the current path.
    pub max_datagram_size: u32,
    /// Effective encoded payload bytes per video fragment.
    pub fragment_payload_bytes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteCursorShapeKind {
    Color,
    MaskedColor,
    Monochrome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCursorShape {
    pub generation: u64,
    pub kind: RemoteCursorShapeKind,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCursorState {
    pub timestamp_us: u64,
    pub generation: u64,
    pub visible: bool,
    pub x: i32,
    pub y: i32,
}

/// Protocol version. Both sides must agree or the server rejects the session.
pub const PROTOCOL_VERSION: u32 = 9;

/// Parse a `VideoPacketHeader` from the first 24 bytes of a datagram payload.
/// Returns `(header, remaining_payload)` on success.
pub fn parse_video_header(buf: &[u8]) -> Option<(VideoPacketHeader, &[u8])> {
    if buf.len() < VideoPacketHeader::SIZE {
        return None;
    }
    let frame_seq = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let timestamp_us = u64::from_le_bytes(buf[4..12].try_into().unwrap());
    let slice_idx = buf[12];
    let flags = VideoFlags::from_bits_truncate(buf[13]);
    let frag_idx = u16::from_le_bytes(buf[14..16].try_into().unwrap());
    let frag_total = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    let slice_len = u32::from_le_bytes(buf[18..22].try_into().unwrap());
    let frag_payload_size = u16::from_le_bytes(buf[22..24].try_into().unwrap());
    Some((
        VideoPacketHeader {
            frame_seq,
            timestamp_us,
            slice_idx,
            flags,
            frag_idx,
            frag_total,
            slice_len,
            frag_payload_size,
        },
        &buf[VideoPacketHeader::SIZE..],
    ))
}

/// Conservative maximum video payload bytes per QUIC datagram on internet paths.
/// Accounts for QUIC short-header overhead (~28 bytes), the 1-byte datagram
/// type tag, and the 24-byte video fragment header, leaving room for the
/// encoded video data. The actual limit is negotiated at runtime via
/// `Connection::max_datagram_size()`; this constant is the safe fallback.
pub const MTU_WAN: usize = 1200;
