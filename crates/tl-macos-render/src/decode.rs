//! VideoToolbox decompression session. Input: Annex B HEVC/H.264 with
//! parameter sets on IDR (SPEC §5). Codec auto-detected from stream.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use core_foundation::base::{CFRelease, CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use parking_lot::Mutex;
use tl_proto::{Codec, CodecCaps, EncodedUnit};

use crate::annexb;
use crate::vt::*;

/// Decoded frame (retained CVPixelBuffer). Must be `Send`.
pub struct DecodedFrame {
    pixel_buffer: CVPixelBufferRef,
    pts_us: i64,
    width: u32,
    height: u32,
    pixel_format: u32,
}

impl DecodedFrame {
    pub fn pts_us(&self) -> i64 {
        self.pts_us
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    /// `CVPixelFormatType` FourCC of the underlying buffer, e.g. `'BGRA'`
    /// when VideoToolbox honored the BGRA request, `'420v'`/`'420f'` (NV12)
    /// or `'P010'` otherwise. The presenter keys its upload path off this.
    pub fn pixel_format(&self) -> u32 {
        self.pixel_format
    }
    /// Borrow the retained pixel buffer (valid for the frame's lifetime).
    pub(crate) fn cv_pixel_buffer(&self) -> CVPixelBufferRef {
        self.pixel_buffer
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        // SAFETY: pixel_buffer was retained in the VT output callback and is
        // released exactly once here. CoreFoundation release is thread-safe.
        unsafe { CFRelease(self.pixel_buffer) }
    }
}

// SAFETY: CVPixelBuffer is a CoreFoundation object; retain/release are
// thread-safe and the decoded contents are immutable once delivered.
unsafe impl Send for DecodedFrame {}

/// One live VT decompression session plus the format description it was
/// built from.
struct Session {
    ptr: VTDecompressionSessionRef,
    format_desc: CMFormatDescriptionRef,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: both handles are live, owned CF/VT objects. Wait ensures no
        // output callback is in flight before Invalidate releases the session
        // (the callback's refcon points into the Decoder that owns us).
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.ptr);
            VTDecompressionSessionInvalidate(self.ptr);
            CFRelease(self.ptr);
            CFRelease(self.format_desc);
        }
    }
}

/// VideoToolbox decompression session. Input: Annex B HEVC/H.264 with
/// parameter sets on IDR (SPEC §5). Codec auto-detected from stream.
pub struct Decoder {
    session: Option<Session>,
    codec: Option<Codec>,
    /// Current parameter sets ([vps, sps, pps] HEVC / [sps, pps] H.264).
    config: Vec<Vec<u8>>,
    /// Frames delivered by the VT output callback, drained per `decode` call.
    output: Arc<Mutex<Vec<DecodedFrame>>>,
    logged_output_format: bool,
}

// SAFETY: VT decompression sessions are documented thread-safe; all mutable
// Rust state is either taken through `&mut self` or behind `output`'s Mutex.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: None,
            codec: None,
            config: Vec::new(),
            output: Arc::new(Mutex::new(Vec::new())),
            logged_output_format: false,
        })
    }

    /// 0..=n frames out (reorder-free low-delay stream ⇒ usually 0..=1).
    pub fn decode(&mut self, unit: &EncodedUnit) -> Result<Vec<DecodedFrame>> {
        let nals = annexb::split(&unit.data);
        if nals.is_empty() {
            return Ok(Vec::new());
        }

        // Codec auto-detection: only trust parameter-set NAL types, and only
        // on keyframes (param sets MUST be prepended there per SPEC §5) to
        // avoid misreading slice NALs (e.g. H.264 0x41 looks like HEVC VPS).
        let codec = self
            .codec
            .or(if unit.keyframe { detect_codec(&nals) } else { None });
        let Some(codec) = codec else {
            log::debug!("decoder: dropping unit before codec detection (waiting for IDR)");
            return Ok(Vec::new());
        };

        // Extract parameter sets (first occurrence wins).
        let required = match codec {
            Codec::Hevc => 3,
            Codec::H264 => 2,
            other => bail!("decoder: unsupported codec {other:?}"),
        };
        let mut sets: Vec<Option<&[u8]>> = vec![None; required];
        let mut vcl_nals: Vec<&[u8]> = Vec::with_capacity(nals.len());
        for nal in &nals {
            match param_index(codec, nal) {
                Some(i) if i < required && sets[i].is_none() => sets[i] = Some(*nal),
                Some(_) => {} // duplicate/ignored parameter set
                None => vcl_nals.push(*nal),
            }
        }

        // (Re)create the session when the config changes (SPEC §5: parameter
        // sets on every IDR let us notice stream reconfiguration).
        if sets.iter().all(Option::is_some) {
            let new_config: Vec<Vec<u8>> =
                sets.iter().map(|s| s.expect("all checked").to_vec()).collect();
            if self.codec != Some(codec) || self.config != new_config {
                log::info!(
                    "decoder: (re)creating VT session, codec={codec:?}, {} parameter set(s)",
                    new_config.len()
                );
                let session = self.create_session(codec, &new_config)?;
                self.session = Some(session); // old session dropped here
                self.codec = Some(codec);
                self.config = new_config;
            }
        }

        if self.session.is_none() {
            log::debug!("decoder: no session yet (no parameter sets seen); dropping unit");
            return Ok(Vec::new());
        }
        if vcl_nals.is_empty() {
            return Ok(Vec::new()); // config-only unit
        }

        self.submit_frame(&vcl_nals, unit.pts_us)?;

        let frames = std::mem::take(&mut *self.output.lock());
        if !self.logged_output_format && !frames.is_empty() {
            self.logged_output_format = true;
            let fmt = frames[0].pixel_format();
            if fmt == kCVPixelFormatType_32BGRA {
                log::info!("decoder: VT honored the BGRA output request");
            } else {
                log::info!(
                    "decoder: VT output format is {fmt:#010x} (BGRA requested); \
                     presenter converts via shader"
                );
            }
        }
        Ok(frames)
    }

    fn create_session(&self, codec: Codec, sets: &[Vec<u8>]) -> Result<Session> {
        let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut desc: CMFormatDescriptionRef = ptr::null_mut();
        // SAFETY: pointer/size arrays are valid for the call; `desc` is
        // written on success (status checked below).
        let status = unsafe {
            match codec {
                Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    ptr::null(),
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    ptr::null(),
                    &mut desc,
                ),
                Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null(),
                    sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut desc,
                ),
                other => return Err(anyhow!("decoder: unsupported codec {other:?}")),
            }
        };
        if status != noErr || desc.is_null() {
            bail!("CMVideoFormatDescriptionCreateFrom*ParameterSets failed: OSStatus {status}");
        }

        let attrs = destination_pixel_buffer_attrs();
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: output_callback,
            // Valid for the Decoder's lifetime: sessions are waited on +
            // invalidated before the Decoder (and its `output`) can drop.
            decompressionOutputRefCon: Arc::as_ptr(&self.output) as *mut c_void,
        };
        let mut session: VTDecompressionSessionRef = ptr::null_mut();
        // SAFETY: all inputs valid; output checked for null + status.
        let status = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                desc,
                ptr::null(),
                attrs.as_concrete_TypeRef(),
                &callback,
                &mut session,
            )
        };
        if status != noErr || session.is_null() {
            // SAFETY: desc is a live owned object; session creation failed.
            unsafe { CFRelease(desc) };
            bail!("VTDecompressionSessionCreate failed: OSStatus {status}");
        }
        Ok(Session { ptr: session, format_desc: desc })
    }

    /// Convert the unit's VCL NALs to length-prefixed form and decode one
    /// access unit synchronously (async flag + immediate wait for a flush
    /// point per frame — matches the low-delay, latest-wins pipeline).
    fn submit_frame(&mut self, vcl_nals: &[&[u8]], pts_us: i64) -> Result<()> {
        let session = self.session.as_ref().expect("session checked");

        let total: usize = vcl_nals.iter().map(|n| 4 + n.len()).sum();
        let mut avcc = Vec::with_capacity(total);
        for nal in vcl_nals {
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }

        let mut block: CMBlockBufferRef = ptr::null_mut();
        // SAFETY: null memoryBlock ⇒ CM allocates `blockLength` bytes itself
        // (AssureMemoryNow), filled by ReplaceDataBytes below; status checked.
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                avcc.len(),
                ptr::null(),
                ptr::null(),
                0,
                avcc.len(),
                kCMBlockBufferAssureMemoryNowFlag,
                &mut block,
            )
        };
        if status != noErr || block.is_null() {
            bail!("CMBlockBufferCreateWithMemoryBlock failed: OSStatus {status}");
        }
        // SAFETY: block owns `avcc.len()` writable bytes; source slice valid.
        let status = unsafe { CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), block, 0, avcc.len()) };
        if status != noErr {
            // SAFETY: block is a live owned object.
            unsafe { CFRelease(block) };
            bail!("CMBlockBufferReplaceDataBytes failed: OSStatus {status}");
        }

        let timing = CMSampleTimingInfo {
            duration: CMTime::INVALID,
            presentationTimeStamp: CMTime::pts_us(pts_us),
            decodeTimeStamp: CMTime::INVALID,
        };
        let sample_sizes = [avcc.len()];
        let mut sample: CMSampleBufferRef = ptr::null_mut();
        // SAFETY: block + format desc are live; timing/size arrays valid;
        // status checked.
        let status = unsafe {
            CMSampleBufferCreateReady(
                ptr::null(),
                block,
                session.format_desc,
                1,
                1,
                &timing,
                1,
                sample_sizes.as_ptr(),
                &mut sample,
            )
        };
        // SAFETY: block is a live owned object, no longer needed either way.
        unsafe { CFRelease(block) };
        if status != noErr || sample.is_null() {
            bail!("CMSampleBufferCreateReady failed: OSStatus {status}");
        }

        let mut info_flags: VTDecodeInfoFlags = 0;
        // SAFETY: session + sample are live; VT retains the sample buffer for
        // asynchronous processing, and we wait for completion before
        // releasing our reference below.
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                session.ptr,
                sample,
                kVTDecodeFrame_EnableAsynchronousDecompression,
                ptr::null_mut(),
                &mut info_flags,
            )
        };
        // SAFETY: session is live; waits for the async decode above.
        let wait_status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(session.ptr) };
        // SAFETY: sample is a live owned object; safe to release after the wait.
        unsafe { CFRelease(sample) };
        if status != noErr {
            bail!("VTDecompressionSessionDecodeFrame failed: OSStatus {status}");
        }
        if wait_status != noErr {
            bail!("VTDecompressionSessionWaitForAsynchronousFrames failed: OSStatus {wait_status}");
        }
        if info_flags & kVTDecodeInfo_FrameDropped != 0 {
            log::warn!("decoder: VT dropped a frame (infoFlags={info_flags:#x})");
        }
        Ok(())
    }
}

/// Destination image buffer attributes: request 32BGRA output (SPEC task);
/// VT falls back to its native format (NV12) when it can't honor it, which
/// the presenter detects per frame via `DecodedFrame::pixel_format()`.
fn destination_pixel_buffer_attrs() -> CFDictionary<CFType, CFType> {
    // SAFETY: the kCVPixelBuffer* keys are valid immutable framework constants
    // (get-rule wrap, no ownership taken).
    unsafe {
        let fmt_key = CFString::wrap_under_get_rule(kCVPixelBufferPixelFormatTypeKey);
        let metal_key = CFString::wrap_under_get_rule(kCVPixelBufferMetalCompatibilityKey);
        let iso_key = CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey);
        let fmt_val = CFNumber::from(kCVPixelFormatType_32BGRA as i32);
        let metal_val = CFBoolean::true_value();
        let iso_val: CFDictionary<CFType, CFType> = CFDictionary::from_CFType_pairs(&[]);
        let pairs = [
            (fmt_key.as_CFType(), fmt_val.as_CFType()),
            (metal_key.as_CFType(), metal_val.as_CFType()),
            (iso_key.as_CFType(), iso_val.as_CFType()),
        ];
        CFDictionary::from_CFType_pairs(&pairs)
    }
}

/// Detect the codec from parameter-set NAL types. HEVC and H.264 NAL header
/// layouts don't collide for VPS/SPS/PPS types in practice.
fn detect_codec(nals: &[&[u8]]) -> Option<Codec> {
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        if nal.len() >= 2 && (32..=34).contains(&((nal[0] >> 1) & 0x3f)) {
            return Some(Codec::Hevc);
        }
        if matches!(nal[0] & 0x1f, 7 | 8) {
            return Some(Codec::H264);
        }
    }
    None
}

/// Parameter-set index for a NAL under the given codec:
/// HEVC → 0=vps(32), 1=sps(33), 2=pps(34); H.264 → 0=sps(7), 1=pps(8).
fn param_index(codec: Codec, nal: &[u8]) -> Option<usize> {
    if nal.is_empty() {
        return None;
    }
    match codec {
        Codec::Hevc if nal.len() >= 2 => match (nal[0] >> 1) & 0x3f {
            32 => Some(0),
            33 => Some(1),
            34 => Some(2),
            _ => None,
        },
        Codec::H264 => match nal[0] & 0x1f {
            7 => Some(0),
            8 => Some(1),
            _ => None,
        },
        _ => None,
    }
}

/// VT output callback: retain the image buffer, wrap as DecodedFrame, queue.
extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    image_buffer: CVImageBufferRef,
    pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the address of the Decoder's `Arc<Mutex<…>>` inner
    // value; the Decoder outlives every session (sessions are waited on and
    // invalidated before drop), so this pointer is always valid here. We only
    // ever borrow it.
    let queue = unsafe { &*(refcon as *const Mutex<Vec<DecodedFrame>>) };
    if status != noErr {
        log::error!("decoder: VT output callback error: OSStatus {status}");
        return;
    }
    if info_flags & kVTDecodeInfo_FrameDropped != 0 || image_buffer.is_null() {
        log::warn!("decoder: frame dropped by VT (infoFlags={info_flags:#x})");
        return;
    }
    // SAFETY: image_buffer is valid for the callback's duration; we retain it
    // so the DecodedFrame owns a reference past the callback's return.
    unsafe { core_foundation::base::CFRetain(image_buffer) };
    // SAFETY: image_buffer is a valid CVPixelBuffer (checked non-null above).
    let (width, height, pixel_format) = unsafe {
        (
            CVPixelBufferGetWidth(image_buffer),
            CVPixelBufferGetHeight(image_buffer),
            CVPixelBufferGetPixelFormatType(image_buffer),
        )
    };
    let frame = DecodedFrame {
        pixel_buffer: image_buffer,
        pts_us: if pts.is_valid() { pts.to_us() } else { 0 },
        width: width as u32,
        height: height as u32,
        pixel_format,
    };
    queue.lock().push(frame);
}

/// Local hardware decode capabilities, used to build `TargetCaps`
/// (SPEC §4 step 2). Reflects actual VT hardware support.
pub fn decoder_caps() -> Vec<CodecCaps> {
    // SAFETY: pure query function, no state.
    let hevc_hw = unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC) != 0 };
    // SAFETY: pure query function, no state.
    let h264_hw = unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_H264) != 0 };

    // NOTE(deviation): VT exposes no separate "10-bit supported" query; on
    // Apple Silicon 10-bit HEVC hw decode rides with HEVC hw decode, so hdr10
    // mirrors hevc_hw. chroma444 is never offered (NV12/420 pipeline).
    vec![
        CodecCaps {
            codec: Codec::Hevc,
            max_width: if hevc_hw { 8192 } else { 1920 },
            max_height: if hevc_hw { 4320 } else { 1080 },
            hdr10: hevc_hw,
            chroma444: false,
            hw: hevc_hw,
        },
        CodecCaps {
            codec: Codec::H264,
            max_width: if h264_hw { 4096 } else { 1920 },
            max_height: if h264_hw { 2304 } else { 1080 },
            hdr10: false,
            chroma444: false,
            hw: h264_hw,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_include_hw_hevc() {
        let caps = decoder_caps();
        assert!(!caps.is_empty(), "decoder_caps must not be empty");
        let hevc = caps.iter().find(|c| c.codec == Codec::Hevc).expect("HEVC entry");
        assert!(hevc.hw, "HEVC hardware decode must be supported on this Mac");
        assert!(hevc.max_width >= 3840 && hevc.max_height >= 2160);
        let h264 = caps.iter().find(|c| c.codec == Codec::H264).expect("H.264 entry");
        assert!(h264.hw, "H.264 hardware decode must be supported on this Mac");
    }

    #[test]
    fn codec_detection_does_not_confuse_h264_slice_for_hevc_vps() {
        // 0x41: H.264 non-IDR slice (type 1); HEVC-wise it reads as VPS(32).
        // detect_codec is only fed keyframe units where param sets come first.
        let sps_h264: &[u8] = &[0x67, 0x42, 0x00, 0x1f];
        let pps_h264: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
        assert_eq!(detect_codec(&[sps_h264, pps_h264]), Some(Codec::H264));
        let vps_hevc: &[u8] = &[0x40, 1];
        let sps_hevc: &[u8] = &[0x42, 1];
        let pps_hevc: &[u8] = &[0x44, 1];
        assert_eq!(detect_codec(&[vps_hevc, sps_hevc, pps_hevc]), Some(Codec::Hevc));
        assert_eq!(detect_codec(&[&[0x65, 0x88][..]]), None); // bare IDR: no params
    }
}
