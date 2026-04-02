//! VideoToolbox hardware decoder for H.264.

#[cfg(not(target_os = "macos"))]
use anyhow::bail;
use anyhow::Result;
use crossbeam_channel::Receiver;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use crate::transport::video_rx::DecodedFrame;

#[cfg(target_os = "macos")]
use core_foundation::base::{kCFAllocatorDefault, TCFType};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::{
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferRef,
};

/// A decoded frame ready for presentation.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct RenderFrame {
    #[cfg(target_os = "macos")]
    pub pixel_buffer: CVPixelBuffer,
    pub width: u32,
    pub height: u32,
    pub frame_seq: u32,
    pub timestamp_us: u64,
    pub received_at_us: u64,
    pub decode_submitted_at_us: u64,
    pub decoded_at_us: u64,
}

#[cfg(target_os = "macos")]
unsafe impl Send for RenderFrame {}

pub struct VideoToolboxDecoder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
use anyhow::Context;
#[cfg(target_os = "macos")]
static FIRST_DECODED_FRAME_LOGGED: AtomicBool = AtomicBool::new(false);

impl VideoToolboxDecoder {
    pub fn start(frame_rx: Receiver<DecodedFrame>) -> Result<(Self, Receiver<RenderFrame>)> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = frame_rx;
            bail!("streamd-client video decode is only supported on macOS");
        }

        #[cfg(target_os = "macos")]
        {
            use crossbeam_channel::Sender;
            use std::time::{Duration, Instant};
            use tracing::{debug, info, warn};

            use vt::*;

            struct FrameMetadata {
                frame_seq: u32,
                timestamp_us: u64,
                received_at_us: u64,
                decode_submitted_at_us: u64,
            }

            struct DecodeSubmitStats {
                submitted_frames: u32,
                total_queue_us: u64,
                window_started_at: Instant,
            }

            impl DecodeSubmitStats {
                fn new() -> Self {
                    Self {
                        submitted_frames: 0,
                        total_queue_us: 0,
                        window_started_at: Instant::now(),
                    }
                }

                fn record_submit(&mut self, queue_us: u32) {
                    self.submitted_frames += 1;
                    self.total_queue_us += queue_us as u64;
                }

                fn maybe_log(&mut self) {
                    if self.window_started_at.elapsed() < Duration::from_secs(1) {
                        return;
                    }

                    let avg_queue_us = if self.submitted_frames > 0 {
                        (self.total_queue_us / self.submitted_frames as u64) as u32
                    } else {
                        0
                    };
                    info!(
                        "decoder telemetry: submitted={} queue_to_submit={}µs",
                        self.submitted_frames, avg_queue_us
                    );
                    *self = Self::new();
                }
            }

            extern "C" fn decode_callback(
                refcon: *mut std::ffi::c_void,
                source_frame: *mut std::ffi::c_void,
                status: vt::OSStatus,
                _flags: u32,
                image: vt::CVPixelBufferRef,
                _pts: i64,
                _duration: i64,
            ) {
                let metadata = if source_frame.is_null() {
                    None
                } else {
                    Some(unsafe { Box::from_raw(source_frame as *mut FrameMetadata) })
                };

                if status != vt::noErr || image.is_null() {
                    return;
                }

                let Some(metadata) = metadata else {
                    return;
                };
                let render_tx = unsafe { &*(refcon as *const Sender<RenderFrame>) };

                match retain_pixel_buffer(
                    image,
                    metadata.frame_seq,
                    metadata.timestamp_us,
                    metadata.received_at_us,
                    metadata.decode_submitted_at_us,
                ) {
                    Ok(frame) => match render_tx.try_send(frame) {
                        Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
                    },
                    Err(err) => warn!("retain decoded frame failed: {err:#}"),
                }
            }

            let stop = Arc::new(AtomicBool::new(false));
            let stop_clone = stop.clone();
            let (render_tx, render_rx) = crossbeam_channel::bounded(4);

            let thread = std::thread::Builder::new()
                .name("streamd-decode".into())
                .spawn(move || {
                    let decoder_spec = build_decoder_spec();
                    let image_attrs = build_image_buffer_attributes();

                    let mut format_desc: CMVideoFormatDescriptionRef = std::ptr::null_mut();
                    let mut session: VTDecompressionSessionRef = std::ptr::null_mut();
                    let mut decode_stats = DecodeSubmitStats::new();

                    let callback_tx = Box::new(render_tx.clone());
                    let callback_tx_ptr = Box::into_raw(callback_tx) as *mut std::ffi::c_void;

                    info!("VideoToolbox decode thread started");

                    while !stop_clone.load(Ordering::Relaxed) {
                        let frame = match frame_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(frame) => frame,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        };

                        if frame.is_keyframe {
                            let (new_sps, new_pps) = extract_sps_pps(&frame.data);
                            if let (Some(new_sps), Some(new_pps)) = (new_sps, new_pps) {
                                recreate_session(
                                    &mut session,
                                    &mut format_desc,
                                    &new_sps,
                                    &new_pps,
                                    &decoder_spec,
                                    &image_attrs,
                                    callback_tx_ptr,
                                    decode_callback,
                                );
                            }
                        }

                        if session.is_null() {
                            debug!("waiting for first IDR frame to create VT session");
                            continue;
                        }

                        let avcc_data = annexb_to_avcc(&frame.data);
                        if avcc_data.is_empty() {
                            continue;
                        }

                        let mut block_buf: *mut std::ffi::c_void = std::ptr::null_mut();
                        let data_len = avcc_data.len();
                        let status = unsafe {
                            CMBlockBufferCreateWithMemoryBlock(
                                kCFAllocatorDefault,
                                std::ptr::null(),
                                data_len,
                                kCFAllocatorDefault,
                                std::ptr::null(),
                                0,
                                data_len,
                                0,
                                &mut block_buf,
                            )
                        };
                        if status != noErr {
                            warn!("CMBlockBufferCreateWithMemoryBlock: {status}");
                            continue;
                        }

                        let status = unsafe {
                            CMBlockBufferReplaceDataBytes(
                                avcc_data.as_ptr() as *const std::ffi::c_void,
                                block_buf,
                                0,
                                data_len,
                            )
                        };
                        if status != noErr {
                            warn!("CMBlockBufferReplaceDataBytes: {status}");
                            unsafe { cf_release(block_buf as *const std::ffi::c_void) };
                            continue;
                        }

                        let mut sample_buf: CMSampleBufferRef = std::ptr::null_mut();
                        let status = unsafe {
                            CMSampleBufferCreateReady(
                                kCFAllocatorDefault,
                                block_buf as *const std::ffi::c_void,
                                format_desc,
                                1,
                                0,
                                std::ptr::null(),
                                1,
                                &data_len,
                                &mut sample_buf,
                            )
                        };
                        if status != noErr {
                            warn!("CMSampleBufferCreateReady: {status}");
                            unsafe { cf_release(block_buf as *const std::ffi::c_void) };
                            continue;
                        }

                        let decode_submitted_at_us = now_local_us();
                        decode_stats.record_submit(
                            decode_submitted_at_us.saturating_sub(frame.received_at_us) as u32,
                        );
                        decode_stats.maybe_log();

                        let metadata = Box::new(FrameMetadata {
                            frame_seq: frame.frame_seq,
                            timestamp_us: frame.timestamp_us,
                            received_at_us: frame.received_at_us,
                            decode_submitted_at_us,
                        });
                        let metadata_ptr = Box::into_raw(metadata) as *mut std::ffi::c_void;
                        let status = unsafe {
                            VTDecompressionSessionDecodeFrame(
                                session,
                                sample_buf,
                                0,
                                metadata_ptr,
                                std::ptr::null_mut(),
                            )
                        };
                        if status != noErr {
                            unsafe {
                                drop(Box::from_raw(metadata_ptr as *mut FrameMetadata));
                            }
                            debug!("VTDecompressionSessionDecodeFrame: {status}");
                        }

                        unsafe {
                            cf_release(sample_buf as *const std::ffi::c_void);
                            cf_release(block_buf as *const std::ffi::c_void);
                        }
                    }

                    unsafe {
                        destroy_session(&mut session, &mut format_desc);
                        drop(Box::from_raw(
                            callback_tx_ptr as *mut crossbeam_channel::Sender<RenderFrame>,
                        ));
                    }

                    info!("VideoToolbox decode thread exited");
                })
                .context("spawn decode thread")?;

            Ok((
                Self {
                    stop,
                    thread: Some(thread),
                },
                render_rx,
            ))
        }
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn recreate_session(
    session: &mut vt::VTDecompressionSessionRef,
    format_desc: &mut vt::CMVideoFormatDescriptionRef,
    sps: &[u8],
    pps: &[u8],
    decoder_spec: &vt::CfDictionary,
    image_attrs: &vt::CfDictionary,
    callback_tx_ptr: *mut std::ffi::c_void,
    decode_callback: vt::DecodeCallback,
) {
    use tracing::{error, info, warn};
    use vt::*;

    unsafe {
        destroy_session(session, format_desc);
    }

    let param_sets: [*const u8; 2] = [sps.as_ptr(), pps.as_ptr()];
    let param_sizes: [usize; 2] = [sps.len(), pps.len()];
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            std::ptr::null(),
            2,
            param_sets.as_ptr(),
            param_sizes.as_ptr(),
            4,
            format_desc,
        )
    };
    if status != noErr {
        error!("CMVideoFormatDescriptionCreateFromH264ParameterSets: {status}");
        return;
    }

    let callback = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(decode_callback),
        decompressionOutputRefCon: callback_tx_ptr,
    };
    let status = unsafe {
        VTDecompressionSessionCreate(
            std::ptr::null(),
            *format_desc,
            decoder_spec.as_concrete_TypeRef() as *const std::ffi::c_void,
            image_attrs.as_concrete_TypeRef() as *const std::ffi::c_void,
            &callback,
            session,
        )
    };
    if status != noErr {
        error!("VTDecompressionSessionCreate: {status}");
        return;
    }

    let status = unsafe { set_real_time(*session) };
    if status != noErr {
        warn!("VTSessionSetProperty(RealTime): {status}");
    }
    info!("VideoToolbox session ready from keyframe");
}

#[cfg(target_os = "macos")]
fn retain_pixel_buffer(
    image: CVPixelBufferRef,
    frame_seq: u32,
    timestamp_us: u64,
    received_at_us: u64,
    decode_submitted_at_us: u64,
) -> Result<RenderFrame> {
    use tracing::info;

    let pixel_buffer = unsafe { CVPixelBuffer::wrap_under_get_rule(image) };
    let width = pixel_buffer.get_width() as u32;
    let height = pixel_buffer.get_height() as u32;
    let pixel_format = pixel_buffer.get_pixel_format();
    let decoded_at_us = now_local_us();

    anyhow::ensure!(
        pixel_buffer.is_planar(),
        "decoder returned a non-planar pixel buffer"
    );
    anyhow::ensure!(
        pixel_buffer.get_plane_count() == 2,
        "decoder returned {} planes instead of NV12",
        pixel_buffer.get_plane_count()
    );
    anyhow::ensure!(
        pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        "decoder returned unsupported pixel format {pixel_format:#x}"
    );

    if !FIRST_DECODED_FRAME_LOGGED.swap(true, Ordering::Relaxed) {
        info!(
            "VideoToolbox produced first frame seq={} {}x{} format={pixel_format:#x}",
            frame_seq, width, height
        );
    }

    Ok(RenderFrame {
        pixel_buffer,
        width,
        height,
        frame_seq,
        timestamp_us,
        received_at_us,
        decode_submitted_at_us,
        decoded_at_us,
    })
}

#[cfg(target_os = "macos")]
fn now_local_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(target_os = "macos")]
mod vt {
    #![allow(improper_ctypes, non_snake_case, non_upper_case_globals)]

    use core_foundation::{
        base::{CFRelease, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        number::CFNumber,
        string::{CFString, CFStringRef},
    };
    use std::ffi::c_void;
    use std::os::raw::c_int;

    pub type CfDictionary = CFDictionary<CFString, core_foundation::base::CFType>;
    pub type DecodeCallback =
        extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CVPixelBufferRef, i64, i64);

    #[repr(C)]
    pub struct OpaqueVTDecompressionSession {
        _private: [u8; 0],
    }
    pub type VTDecompressionSessionRef = *mut OpaqueVTDecompressionSession;

    #[repr(C)]
    pub struct OpaqueCMVideoFormatDescription {
        _private: [u8; 0],
    }
    pub type CMVideoFormatDescriptionRef = *mut OpaqueCMVideoFormatDescription;

    #[repr(C)]
    pub struct OpaqueCMSampleBuffer {
        _private: [u8; 0],
    }
    pub type CMSampleBufferRef = *mut OpaqueCMSampleBuffer;

    pub type CVPixelBufferRef = core_video::pixel_buffer::CVPixelBufferRef;
    pub type OSStatus = i32;
    pub const noErr: OSStatus = 0;
    pub const K_CV_PIXEL_FORMAT_TYPE_420V: i32 =
        core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32;

    #[repr(C)]
    pub struct VTDecompressionOutputCallbackRecord {
        pub decompressionOutputCallback: Option<DecodeCallback>,
        pub decompressionOutputRefCon: *mut c_void,
    }

    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        pub static kVTDecompressionPropertyKey_RealTime: CFStringRef;
        pub static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: CFStringRef;

        pub fn VTDecompressionSessionCreate(
            allocator: *const c_void,
            videoFormatDescription: CMVideoFormatDescriptionRef,
            videoDecoderSpecification: *const c_void,
            destinationImageBufferAttributes: *const c_void,
            outputCallback: *const VTDecompressionOutputCallbackRecord,
            decompressionSessionOut: *mut VTDecompressionSessionRef,
        ) -> OSStatus;

        pub fn VTDecompressionSessionDecodeFrame(
            session: VTDecompressionSessionRef,
            sampleBuffer: CMSampleBufferRef,
            decodeFlags: u32,
            sourceFrameRefCon: *mut c_void,
            infoFlagsOut: *mut u32,
        ) -> OSStatus;

        pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
        pub fn VTDecompressionSessionWaitForAsynchronousFrames(
            session: VTDecompressionSessionRef,
        ) -> OSStatus;
        pub fn VTSessionSetProperty(
            session: *mut c_void,
            propertyKey: *const c_void,
            propertyValue: *const c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
            allocator: *const c_void,
            parameterSetCount: usize,
            parameterSetPointers: *const *const u8,
            parameterSetSizes: *const usize,
            nalUnitHeaderLength: c_int,
            formatDescriptionOut: *mut CMVideoFormatDescriptionRef,
        ) -> OSStatus;

        pub fn CMSampleBufferCreateReady(
            allocator: *const c_void,
            dataBuffer: *const c_void,
            formatDescription: CMVideoFormatDescriptionRef,
            numSamples: usize,
            numSampleTimingEntries: usize,
            sampleTimingArray: *const c_void,
            numSampleSizeEntries: usize,
            sampleSizeArray: *const usize,
            sampleBufferOut: *mut CMSampleBufferRef,
        ) -> OSStatus;

        pub fn CMBlockBufferCreateWithMemoryBlock(
            structureAllocator: *const c_void,
            memoryBlock: *const c_void,
            blockLength: usize,
            blockAllocator: *const c_void,
            customBlockSource: *const c_void,
            offsetToData: usize,
            dataLength: usize,
            flags: u32,
            blockBufferOut: *mut *mut c_void,
        ) -> OSStatus;

        pub fn CMBlockBufferReplaceDataBytes(
            sourceBytes: *const c_void,
            destinationBuffer: *mut c_void,
            offsetIntoDestination: usize,
            dataLength: usize,
        ) -> OSStatus;
    }

    pub unsafe fn destroy_session(
        session: &mut VTDecompressionSessionRef,
        format_desc: &mut CMVideoFormatDescriptionRef,
    ) {
        if !(*session).is_null() {
            let _ = VTDecompressionSessionWaitForAsynchronousFrames(*session);
            VTDecompressionSessionInvalidate(*session);
            *session = std::ptr::null_mut();
        }
        if !(*format_desc).is_null() {
            CFRelease(*format_desc as *const c_void);
            *format_desc = std::ptr::null_mut();
        }
    }

    pub unsafe fn cf_release(ptr: *const c_void) {
        if !ptr.is_null() {
            CFRelease(ptr);
        }
    }

    pub fn build_decoder_spec() -> CfDictionary {
        let key = unsafe {
            CFString::wrap_under_get_rule(
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
            )
        };
        let value = CFBoolean::true_value().as_CFType();
        CFDictionary::from_CFType_pairs(&[(key, value)])
    }

    pub fn build_image_buffer_attributes() -> CfDictionary {
        let pixel_format_key = unsafe {
            CFString::wrap_under_get_rule(
                core_video::pixel_buffer::kCVPixelBufferPixelFormatTypeKey,
            )
        };
        let pixel_format_value = CFNumber::from(K_CV_PIXEL_FORMAT_TYPE_420V).as_CFType();
        let iosurface_key = unsafe {
            CFString::wrap_under_get_rule(
                core_video::pixel_buffer::kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let iosurface_value: CfDictionary = CFDictionary::from_CFType_pairs(&[]);

        CFDictionary::from_CFType_pairs(&[
            (pixel_format_key, pixel_format_value),
            (iosurface_key, iosurface_value.as_CFType()),
        ])
    }

    pub unsafe fn set_real_time(session: VTDecompressionSessionRef) -> OSStatus {
        let key = CFString::wrap_under_get_rule(kVTDecompressionPropertyKey_RealTime);
        let value = CFBoolean::true_value();
        VTSessionSetProperty(
            session as *mut c_void,
            key.as_concrete_TypeRef() as *const c_void,
            value.as_concrete_TypeRef() as *const c_void,
        )
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn extract_sps_pps(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    let nals = split_nals(data);
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        match nal_type {
            7 => sps = Some(nal.to_vec()),
            8 => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut nal_start = 0usize;
    let mut in_nal = false;

    while i + 2 < data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let sc4 = i + 3 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;

        if sc3 || sc4 {
            if in_nal && i > nal_start {
                nals.push(&data[nal_start..i]);
            }
            nal_start = if sc4 { i + 4 } else { i + 3 };
            in_nal = true;
            i += if sc4 { 4 } else { 3 };
            continue;
        }
        i += 1;
    }
    if in_nal && nal_start < data.len() {
        nals.push(&data[nal_start..]);
    }
    nals
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let nals = split_nals(data);
    let mut out = Vec::with_capacity(data.len());
    for nal in nals {
        let len = nal.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}
