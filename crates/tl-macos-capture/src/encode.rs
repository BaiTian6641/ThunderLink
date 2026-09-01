//! VideoToolbox hardware encoder (SPEC §5/§10): HEVC/H.264, real-time, no
//! B-frames, low-delay rate control, Annex B output with parameter sets
//! prepended on every IDR.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMTime};
use parking_lot::{Condvar, Mutex};
use tl_proto::{Codec, EncodedUnit, StreamConfig};

use super::capture::{cmtime_to_us, CapturedFrame};

type OSStatus = i32;

/// Opaque VTCompressionSessionRef (a CoreFoundation type).
type VTCompressionSessionRef = *mut c_void;

type VTCompressionOutputCallback = Option<
    unsafe extern "C" fn(
        output_callback_refcon: *mut c_void,
        source_frame_refcon: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        sample_buffer: *mut objc2_core_media::CMSampleBuffer,
    ),
>;

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        output_callback: VTCompressionOutputCallback,
        output_callback_refcon: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(
        session: VTCompressionSessionRef,
        key: &CFString,
        value: *const CFType,
    ) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: &objc2_core_video::CVPixelBuffer,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: *const c_void,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    static kVTCompressionPropertyKey_RealTime: &'static CFString;
    static kVTCompressionPropertyKey_AllowFrameReordering: &'static CFString;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: &'static CFString;
    static kVTCompressionPropertyKey_ExpectedFrameRate: &'static CFString;
    static kVTCompressionPropertyKey_AverageBitRate: &'static CFString;
    static kVTCompressionPropertyKey_ProfileLevel: &'static CFString;
    static kVTCompressionPropertyKey_MaxFrameDelayCount: &'static CFString;
    static kVTProfileLevel_HEVC_Main_AutoLevel: &'static CFString;
    static kVTProfileLevel_HEVC_Main10_AutoLevel: &'static CFString;
    static kVTProfileLevel_H264_High_AutoLevel: &'static CFString;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: &'static CFString;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Maximum queued-but-unconsumed output units. Real-time VT keeps up with
/// the caller; this only bounds pathological stalls.
const MAX_QUEUED_UNITS: usize = 64;

/// Shared state between the VT output callback thread and `encode()`.
struct OutState {
    inner: Mutex<OutInner>,
    cond: Condvar,
    hevc: bool,
}

struct OutInner {
    queue: VecDeque<EncodedUnit>,
    error: Option<String>,
}

/// VideoToolbox compression session: HEVC/H.264, real-time, no B-frames,
/// low-delay rate control, Annex B output with parameter sets prepended
/// on every IDR (SPEC §5).
pub struct Encoder {
    session: VTCompressionSessionRef,
    /// Boxed callback state, also handed to VT as the callback refcon.
    /// Reclaimed in Drop after session invalidation.
    state: *mut OutState,
    force_idr: AtomicBool,
    force_dict: CFRetained<CFDictionary<CFString, CFBoolean>>,
    fps: u32,
}

impl Encoder {
    pub fn new(cfg: &StreamConfig) -> Result<Self> {
        let (codec_type, hevc) = match cfg.codec {
            Codec::Hevc => (kCMVideoCodecType_HEVC, true),
            Codec::H264 => (kCMVideoCodecType_H264, false),
            other => return Err(anyhow!("VideoToolbox encoder does not support {other:?}")),
        };
        if cfg.width == 0 || cfg.height == 0 {
            return Err(anyhow!("encoder dimensions must be > 0"));
        }
        let fps = ((cfg.fps_millihertz + 500) / 1000).max(1);

        let state = Box::into_raw(Box::new(OutState {
            inner: Mutex::new(OutInner {
                queue: VecDeque::new(),
                error: None,
            }),
            cond: Condvar::new(),
            hevc,
        }));

        let mut session: VTCompressionSessionRef = std::ptr::null_mut();
        // SAFETY: all out-params are valid pointers; `state` outlives the
        // session (reclaimed in Drop after VTCompressionSessionInvalidate).
        let status = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),
                cfg.width as i32,
                cfg.height as i32,
                codec_type,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                Some(output_callback),
                state as *mut c_void,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            // SAFETY: session creation failed, so VT never saw `state`.
            drop(unsafe { Box::from_raw(state) });
            return Err(anyhow!("VTCompressionSessionCreate failed: {status}"));
        }

        let result = (|| {
            // SAFETY: statics are valid CFString keys; session is valid.
            unsafe {
                set_prop(
                    session,
                    kVTCompressionPropertyKey_RealTime,
                    CFBoolean::new(true) as *const CFBoolean as *const CFType,
                )?;
                set_prop(
                    session,
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    CFBoolean::new(false) as *const CFBoolean as *const CFType,
                )?;
                // ≈2s GOP.
                let gop = CFNumber::new_i32((fps * 2) as i32);
                set_prop(
                    session,
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    CFRetained::as_ptr(&gop).as_ptr() as *const CFType,
                )?;
                let fr = CFNumber::new_f64(fps as f64);
                set_prop(
                    session,
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    CFRetained::as_ptr(&fr).as_ptr() as *const CFType,
                )?;
                let br = CFNumber::new_i64(cfg.bitrate_kbps as i64 * 1000);
                set_prop(
                    session,
                    kVTCompressionPropertyKey_AverageBitRate,
                    CFRetained::as_ptr(&br).as_ptr() as *const CFType,
                )?;
                // Low-delay hint: never buffer frames for reordering.
                // Not supported by all encoder instances (RealTime already
                // implies minimal delay), so failure is non-fatal.
                let delay = CFNumber::new_i32(0);
                if let Err(e) = set_prop(
                    session,
                    kVTCompressionPropertyKey_MaxFrameDelayCount,
                    CFRetained::as_ptr(&delay).as_ptr() as *const CFType,
                ) {
                    log::debug!("{e:#}");
                }
                let profile = if hevc {
                    if cfg.hdr {
                        kVTProfileLevel_HEVC_Main10_AutoLevel
                    } else {
                        kVTProfileLevel_HEVC_Main_AutoLevel
                    }
                } else {
                    kVTProfileLevel_H264_High_AutoLevel
                };
                set_prop(
                    session,
                    kVTCompressionPropertyKey_ProfileLevel,
                    profile as *const CFString as *const CFType,
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            // SAFETY: session valid and not yet invalidated.
            unsafe {
                VTCompressionSessionInvalidate(session);
                CFRelease(session as *const c_void);
                drop(Box::from_raw(state));
            }
            return Err(e);
        }

        // Frame-properties dictionary used when forcing an IDR.
        // SAFETY: the static key and CFBoolean are valid CF objects.
        let force_dict = unsafe {
            CFDictionary::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true)],
            )
        };

        Ok(Self {
            session,
            state,
            force_idr: AtomicBool::new(false),
            force_dict,
            fps,
        })
    }

    /// Synchronous. Returns 0..=n units (≥1 in normal operation).
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedUnit>> {
        // SAFETY: plain CMTime constructors with valid timescales.
        let (pts, duration) = unsafe {
            (
                CMTime::new(frame.pts_us(), 1_000_000),
                CMTime::new(1_000_000 / self.fps as i64, 1_000_000),
            )
        };
        let props = if self.force_idr.swap(false, Ordering::SeqCst) {
            CFRetained::as_ptr(&self.force_dict).as_ptr() as *const c_void
        } else {
            std::ptr::null()
        };
        // SAFETY: session alive; the pixel buffer is valid and VT retains it
        // for the duration of encoding. `props` is null or a valid dictionary.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                frame.pixel_buffer(),
                pts,
                duration,
                props,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(anyhow!("VTCompressionSessionEncodeFrame failed: {status}"));
        }

        // SAFETY: `state` is valid until Drop (after session invalidation).
        let state = unsafe { &*self.state };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut guard = state.inner.lock();
        loop {
            if let Some(err) = guard.error.take() {
                return Err(anyhow!("encoder output error: {err}"));
            }
            if !guard.queue.is_empty() {
                return Ok(guard.queue.drain(..).collect());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("encoder produced no output within 5s"));
            }
            state.cond.wait_for(&mut guard, remaining);
        }
    }

    /// Force the next output frame to be an IDR.
    pub fn request_idr(&mut self) {
        self.force_idr.store(true, Ordering::SeqCst);
    }
}

// SAFETY: VTCompressionSession is thread-safe for serialized EncodeFrame
// calls; all other shared state lives in `OutState` behind a Mutex+Condvar.
// The CF dictionary is immutable after creation. Drop invalidates the
// session (stopping callbacks) before reclaiming the refcon box.
unsafe impl Send for Encoder {}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: session valid. CompleteFrames blocks until all pending
        // output callbacks have fired; Invalidate stops all further callback
        // activity, so reclaiming `state` afterwards cannot race.
        unsafe {
            let st = VTCompressionSessionCompleteFrames(self.session, kCMTimeInvalid);
            if st != 0 {
                log::warn!("VTCompressionSessionCompleteFrames failed: {st}");
            }
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as *const c_void);
            drop(Box::from_raw(self.state));
        }
    }
}

/// SAFETY: `key`/`value` must be valid CF objects; VT retains the value.
unsafe fn set_prop(
    session: VTCompressionSessionRef,
    key: &'static CFString,
    value: *const CFType,
) -> Result<()> {
    // SAFETY: forwarded from caller; session is valid.
    let status = unsafe { VTSessionSetProperty(session, key, value) };
    if status != 0 {
        return Err(anyhow!("VTSessionSetProperty({key}) failed: {status}"));
    }
    Ok(())
}

/// VT output callback: convert the CMSampleBuffer (AVCC) to an Annex B
/// EncodedUnit and queue it for `encode()`.
unsafe extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    sample_buffer: *mut objc2_core_media::CMSampleBuffer,
) {
    // A panic must never unwind into VideoToolbox.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: forwarded callback arguments; refcon is our OutState.
        unsafe { handle_output(refcon, status, sample_buffer) }
    }));
}

/// SAFETY: called only from `output_callback` with VT-provided arguments.
unsafe fn handle_output(
    refcon: *mut c_void,
    status: OSStatus,
    sample_buffer: *mut objc2_core_media::CMSampleBuffer,
) {
    use objc2_core_media::{
        kCMSampleAttachmentKey_NotSync, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };

    // SAFETY: `refcon` is the OutState pointer installed at session creation;
    // the Encoder outlives all callbacks (invalidated before reclaim).
    let state = unsafe { &*(refcon as *const OutState) };

    if status != 0 || sample_buffer.is_null() {
        let mut g = state.inner.lock();
        g.error = Some(format!("VT encode callback status {status}"));
        state.cond.notify_one();
        return;
    }
    // SAFETY: non-null valid sample buffer delivered by VT for this call.
    let sbuf = unsafe { &*sample_buffer };

    // Keyframe = no NotSync attachment on the (single) sample.
    let keyframe = {
        // SAFETY: valid sample buffer.
        let attachments = unsafe { sbuf.sample_attachments_array(false) };
        match attachments {
            Some(arr) if !arr.is_empty() => {
                // SAFETY: sample attachment arrays contain CFDictionary
                // elements by CoreMedia contract; this only re-types the
                // array's element parameter.
                let arr = unsafe {
                    CFRetained::cast_unchecked::<CFArray<CFDictionary<CFString, CFType>>>(arr)
                };
                match arr.get(0) {
                    // SAFETY: valid dictionary; static key is a valid CFString.
                    Some(dict) => !dict.contains_key(unsafe { kCMSampleAttachmentKey_NotSync }),
                    None => true,
                }
            }
            _ => true,
        }
    };

    // SAFETY: valid sample buffer.
    let pts_us = cmtime_to_us(unsafe { sbuf.presentation_time_stamp() });

    // Parameter sets from the format description, prepended on every IDR.
    let mut params: Vec<u8> = Vec::new();
    let mut nal_len = 4usize;
    if keyframe {
        // SAFETY: valid sample buffer.
        let desc = unsafe { sbuf.format_description() };
        if let Some(desc) = desc {
            let mut count = 0usize;
            let mut nl: std::ffi::c_int = 0;
            // SAFETY: valid format description; null out-params permitted.
            let st = unsafe {
                if state.hevc {
                    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        &desc,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut count,
                        &mut nl,
                    )
                } else {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &desc,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut count,
                        &mut nl,
                    )
                }
            };
            if st == 0 {
                if nl > 0 {
                    nal_len = nl as usize;
                }
                for i in 0..count {
                    let mut ptr: *const u8 = std::ptr::null();
                    let mut size = 0usize;
                    // SAFETY: valid description; out-params are valid pointers.
                    let st = unsafe {
                        if state.hevc {
                            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                                &desc,
                                i,
                                &mut ptr,
                                &mut size,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            )
                        } else {
                            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                                &desc,
                                i,
                                &mut ptr,
                                &mut size,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            )
                        }
                    };
                    // SAFETY: on success ptr/size describe memory owned by the
                    // (retained) format description, valid for its lifetime.
                    if st == 0 && !ptr.is_null() && size > 0 {
                        params.extend_from_slice(&[0, 0, 0, 1]);
                        params.extend_from_slice(unsafe {
                            std::slice::from_raw_parts(ptr, size)
                        });
                    }
                }
            }
        }
    }

    // Compressed payload (AVCC length-prefixed NALs).
    // SAFETY: valid sample buffer.
    let data = unsafe { sbuf.data_buffer() };
    let Some(block) = data else {
        let mut g = state.inner.lock();
        g.error = Some("VT output sample had no data buffer".into());
        state.cond.notify_one();
        return;
    };
    // SAFETY: valid block buffer.
    let total = unsafe { block.data_length() };
    let mut raw = vec![0u8; total];
    if total > 0 {
        // SAFETY: `raw` is `total` bytes long; block buffer is valid.
        let st = unsafe {
            block.copy_data_bytes(
                0,
                total,
                NonNull::new(raw.as_mut_ptr() as *mut c_void).expect("non-null vec"),
            )
        };
        if st != 0 {
            let mut g = state.inner.lock();
            g.error = Some(format!("CMBlockBufferCopyDataBytes failed: {st}"));
            state.cond.notify_one();
            return;
        }
    }
    let Some(payload) = avcc_to_annex_b(&raw, nal_len) else {
        let mut g = state.inner.lock();
        g.error = Some("malformed AVCC payload from encoder".into());
        state.cond.notify_one();
        return;
    };

    let mut data = params;
    data.extend_from_slice(&payload);

    let mut g = state.inner.lock();
    if g.queue.len() >= MAX_QUEUED_UNITS {
        g.queue.pop_front();
        log::warn!("encoder output queue full; dropping oldest unit");
    }
    g.queue.push_back(EncodedUnit {
        pts_us,
        keyframe,
        data,
    });
    state.cond.notify_one();
}

/// Convert an AVCC (length-prefixed) sample to Annex B (start-code) form.
fn avcc_to_annex_b(raw: &[u8], nal_len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    let mut pos = 0usize;
    while pos + nal_len <= raw.len() {
        let mut n = 0usize;
        for &b in &raw[pos..pos + nal_len] {
            n = (n << 8) | b as usize;
        }
        pos += nal_len;
        if n == 0 || pos + n > raw.len() {
            return None;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&raw[pos..pos + n]);
        pos += n;
    }
    if pos != raw.len() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsrc::TestPattern;
    use tl_proto::Chroma;

    fn cfg(codec: Codec) -> StreamConfig {
        StreamConfig {
            codec,
            width: 640,
            height: 480,
            fps_millihertz: 60_000,
            bitrate_kbps: 20_000,
            chroma: Chroma::Yuv420,
            hdr: false,
        }
    }

    /// NAL header bytes (first byte after each 00 00 00 01 start code).
    fn nal_header_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 4 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
                out.push(data[i + 4]);
                i += 4;
            } else {
                i += 1;
            }
        }
        out
    }

    fn encode_sixty(codec: Codec) -> (Encoder, Vec<EncodedUnit>, TestPattern) {
        let mut enc = Encoder::new(&cfg(codec)).expect("create encoder");
        let mut src = TestPattern::new(640, 480, 60);
        let mut units = Vec::new();
        for _ in 0..60 {
            let f = src.next().expect("frame");
            units.extend(enc.encode(&f).expect("encode"));
        }
        (enc, units, src)
    }

    fn check_common(units: &[EncodedUnit]) {
        assert!(units.len() >= 50, "expected ~60 units, got {}", units.len());
        assert!(units[0].keyframe, "first unit must be a keyframe");
        for (i, u) in units.iter().enumerate() {
            assert!(
                u.data.starts_with(&[0, 0, 0, 1]),
                "unit {i} must start with an Annex B start code"
            );
        }
        for w in units.windows(2) {
            assert!(w[1].pts_us > w[0].pts_us, "pts must be monotonic");
        }
    }

    #[test]
    fn encode_hevc_annex_b() {
        let (mut enc, units, mut src) = encode_sixty(Codec::Hevc);
        check_common(&units);
        let headers = nal_header_bytes(&units[0].data);
        assert!(headers.contains(&0x40), "keyframe must contain VPS: {headers:02x?}");
        assert!(headers.contains(&0x42), "keyframe must contain SPS: {headers:02x?}");
        assert!(headers.contains(&0x44), "keyframe must contain PPS: {headers:02x?}");

        // request_idr forces an IDR on the next frame.
        enc.request_idr();
        let mut got_idr = false;
        for _ in 0..4 {
            let f = src.next().unwrap();
            if enc.encode(&f).unwrap().iter().any(|u| u.keyframe) {
                got_idr = true;
                break;
            }
        }
        assert!(got_idr, "request_idr must yield a keyframe promptly");
    }

    #[test]
    fn encode_h264_annex_b() {
        let (_enc, units, _src) = encode_sixty(Codec::H264);
        check_common(&units);
        // Compare NAL unit *type* (low 5 bits): VT emits SPS/PPS with
        // nal_ref_idc=01 (0x27/0x28) rather than the textbook 0x67/0x68.
        let types: Vec<u8> = nal_header_bytes(&units[0].data)
            .iter()
            .map(|b| b & 0x1F)
            .collect();
        assert!(types.contains(&7), "keyframe must contain SPS: {types:02x?}");
        assert!(types.contains(&8), "keyframe must contain PPS: {types:02x?}");
    }
}
