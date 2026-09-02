//! x264 software H.264 encoder (SPEC §5/§10, docs/LINUX-PORT.md):
//! `ultrafast` preset + `zerolatency` tune, no B-frames, ABR rate
//! control, Annex B output with SPS/PPS repeated on every IDR.
//!
//! Bindings go through `x264-sys` directly (the safe `x264` 0.5 wrapper
//! exposes no way to set `b_repeat_headers` or to force IDR frames, both
//! required by SPEC §5/§6) — same raw-FFI pattern as the macOS crate's
//! VideoToolbox binding.

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::c_int;

use anyhow::{anyhow, bail, Result};
use x264_sys::x264::*;

use tl_proto::caps::{Chroma, Codec, StreamConfig};
use tl_proto::packet::EncodedUnit;

use super::frame::{convert, I420, RawFrame};

/// x264 software encoder: one input frame in, zero or one `EncodedUnit`
/// out (zerolatency tune ⇒ no reordering, no delay).
pub struct Encoder {
    /// Owned encoder handle: created in [`Encoder::new`], closed exactly
    /// once in `Drop`. Never aliased outside this struct.
    enc: *mut x264_t,
    width: u32,
    height: u32,
    /// Scratch I420 planes, allocated once and refilled per frame.
    planes: I420,
    /// pts of submitted frames in submit order. Zerolatency output order
    /// == submit order, so the front entry is the pts of the next output
    /// picture — this is how input pts map onto output units without
    /// relying on x264's internal timebase.
    pending_pts: VecDeque<i64>,
    force_idr: bool,
}

impl Encoder {
    /// Open an encoder for `cfg`. H.264 only (this crate is the fallback
    /// software path; HEVC goes to VAAPI in v2 per docs/LINUX-PORT.md).
    pub fn new(cfg: &StreamConfig) -> Result<Self> {
        match cfg.codec {
            Codec::H264 => {}
            other => {
                return Err(anyhow!(
                    "x264 encoder supports H.264 only, got {other:?} (cfg: {cfg:?})"
                ))
            }
        }
        if cfg.width == 0 || cfg.height == 0 {
            bail!("encoder dimensions must be > 0, got {}x{}", cfg.width, cfg.height);
        }
        if cfg.width % 2 != 0 || cfg.height % 2 != 0 {
            bail!(
                "I420 needs even dimensions, got {}x{}",
                cfg.width,
                cfg.height
            );
        }
        if cfg.chroma != Chroma::Yuv420 {
            bail!("x264 encoder supports Yuv420 only, got {:?}", cfg.chroma);
        }
        let fps = ((cfg.fps_millihertz + 500) / 1000).max(1);
        let keyint = (fps * 2).max(2) as c_int;

        let preset = CString::new("ultrafast").expect("NUL-free literal");
        let tune = CString::new("zerolatency").expect("NUL-free literal");

        let mut param = std::mem::MaybeUninit::<x264_param_t>::uninit();
        // SAFETY: `param` is a valid out-pointer for one x264_param_t;
        // x264_param_default_preset fully initializes it (it starts with
        // x264_param_default). The preset/tune pointers are NUL-terminated
        // CStrings kept alive above. Non-zero return means the preset/tune
        // name was rejected — we bail before assume_init.
        let rc = unsafe {
            x264_param_default_preset(param.as_mut_ptr(), preset.as_ptr(), tune.as_ptr())
        };
        if rc != 0 {
            bail!("x264_param_default_preset(ultrafast, zerolatency) failed: {rc}");
        }
        // SAFETY: initialized by the successful call above.
        let mut param = unsafe { param.assume_init() };

        // Bitstream: Annex B start codes, parameter sets repeated in front
        // of every IDR (SPEC §5) so any receiver can join mid-stream.
        param.b_annexb = 1;
        param.b_repeat_headers = 1;
        param.b_aud = 0;

        // Low delay: no B-frames (also implied by zerolatency, set
        // explicitly), no scenecut-driven IDRs — IDR exactly on keyint or
        // request_idr.
        param.i_bframe = 0;
        param.i_scenecut_threshold = 0;
        param.i_keyint_max = keyint;
        param.i_keyint_min = (fps as c_int).min(keyint);

        // Rate control: one-pass ABR at the negotiated bitrate.
        param.rc.i_rc_method = X264_RC_ABR as c_int;
        param.rc.i_bitrate = cfg.bitrate_kbps as c_int;
        param.rc.b_stat_read = 0;
        param.rc.b_stat_write = 0;

        // Fixed cadence (CFR): x264 derives its internal timebase from
        // the rational fps. Input pts is carried by our pending_pts queue.
        param.i_fps_num = cfg.fps_millihertz;
        param.i_fps_den = 1000;

        // Geometry/format.
        param.i_csp = X264_CSP_I420 as c_int;
        param.i_width = cfg.width as c_int;
        param.i_height = cfg.height as c_int;

        // Keep x264's own stderr quiet (real errors still surface); crate
        // logging happens through `log`.
        param.i_log_level = X264_LOG_ERROR as c_int;

        // SAFETY: `param` fully initialized above; on success we own the
        // returned handle (closed in Drop), on failure it is NULL.
        let enc = unsafe { x264_encoder_open(&mut param) };
        if enc.is_null() {
            bail!("x264_encoder_open failed for {cfg:?}");
        }
        log::debug!(
            "x264 encoder: {}x{} @ {}/{} fps, ABR {} kbps, keyint {}, annexb, repeat-headers, no B-frames",
            cfg.width,
            cfg.height,
            cfg.fps_millihertz,
            1000,
            cfg.bitrate_kbps,
            keyint
        );

        Ok(Self {
            enc,
            width: cfg.width,
            height: cfg.height,
            planes: I420::new(cfg.width, cfg.height),
            pending_pts: VecDeque::new(),
            force_idr: false,
        })
    }

    /// Encode one frame. Synchronous; with the zerolatency tune exactly
    /// one picture comes back per submitted frame (empty only if x264
    /// buffered it, which must not happen with zero lookahead).
    pub fn encode(&mut self, frame: &RawFrame) -> Result<Vec<EncodedUnit>> {
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "frame is {}x{}, encoder is {}x{}",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }
        convert(frame, &mut self.planes);
        let [y, u, v] = self.planes.planes();
        let cw = self.planes.chroma_width() as c_int;

        let mut pic: x264_picture_t = unsafe { std::mem::zeroed() };
        // SAFETY: `pic` is a valid, fully-written x264_picture_t buffer;
        // x264_picture_init fills it with the documented defaults.
        unsafe { x264_picture_init(&mut pic) };
        pic.i_pts = frame.pts_us;
        pic.i_type = if self.force_idr {
            self.force_idr = false;
            X264_TYPE_KEYFRAME as c_int
        } else {
            X264_TYPE_AUTO as c_int
        };
        pic.img.i_csp = X264_CSP_I420 as c_int;
        pic.img.i_plane = 3;
        pic.img.i_stride = [self.width as c_int, cw, cw, 0];
        // The plane pointers are `*mut` because that is the C struct's
        // shape; x264 only reads them for the duration of the call below.
        pic.img.plane = [
            y.as_ptr() as *mut u8,
            u.as_ptr() as *mut u8,
            v.as_ptr() as *mut u8,
            std::ptr::null_mut(),
        ];

        self.pending_pts.push_back(frame.pts_us);

        let mut nals: *mut x264_nal_t = std::ptr::null_mut();
        let mut nal_count: c_int = 0;
        let mut pic_out: x264_picture_t = unsafe { std::mem::zeroed() };
        // SAFETY: `self.enc` is a live handle (non-null from
        // x264_encoder_open, closed only in Drop). `pic` is fully
        // initialized with valid plane pointers into `self.planes`, which
        // outlive this call; x264 copies the pixels synchronously. The
        // NAL array it stores into `nals` is encoder-owned and valid
        // until the next x264_encoder_encode call — copied out below
        // before returning.
        let ret = unsafe {
            x264_encoder_encode(self.enc, &mut nals, &mut nal_count, &mut pic, &mut pic_out)
        };
        if ret < 0 {
            self.pending_pts.pop_back();
            bail!("x264_encoder_encode failed: {ret}");
        }
        if nal_count <= 0 || nals.is_null() {
            // Frame buffered (should not happen with zerolatency); its
            // pts stays queued and will stamp the eventual output.
            return Ok(Vec::new());
        }

        let keyframe = pic_out.b_keyframe != 0;
        // Zerolatency: output order == submit order, so the front pts is
        // this frame's pts.
        let pts_us = self
            .pending_pts
            .pop_front()
            .unwrap_or(frame.pts_us);

        // Annex B stream: the returned NAL payloads are contiguous and
        // each starts with a start code (b_annexb=1).
        let mut data = Vec::new();
        for i in 0..nal_count as usize {
            // SAFETY: `nals` points to `nal_count` contiguous NALs owned
            // by the encoder, valid until the next encode call (nothing
            // else touches the encoder in between).
            let nal = unsafe { *nals.add(i) };
            // SAFETY: p_payload names i_payload readable bytes of that
            // NAL, same validity window as above.
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(nal.p_payload, nal.i_payload as usize)
            });
        }

        Ok(vec![EncodedUnit { pts_us, keyframe, data }])
    }

    /// Force the next encoded frame to be an IDR (SPEC §6 `IdrRequest`
    /// path). The repeated headers guarantee SPS/PPS precede it.
    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `enc` came from x264_encoder_open and is closed exactly
        // once, here; nothing else uses the encoder afterwards.
        if !self.enc.is_null() {
            unsafe { x264_encoder_close(self.enc) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsrc::TestPattern;

    fn h264_cfg() -> StreamConfig {
        StreamConfig {
            codec: Codec::H264,
            width: 320,
            height: 180,
            fps_millihertz: 60_000,
            bitrate_kbps: 2_000,
            chroma: Chroma::Yuv420,
            hdr: false,
        }
    }

    /// Split an Annex B stream into NAL type codes (header & 0x1f).
    /// Returns one entry per start code with a non-empty NAL after it.
    fn nal_types(data: &[u8]) -> Vec<u8> {
        let mut starts: Vec<(usize, usize)> = Vec::new(); // (nal header idx, start-code len)
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let sc = if i > 0 && data[i - 1] == 0 { 4 } else { 3 };
                starts.push((i + 3, sc));
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut types = Vec::new();
        for (k, &(s, _)) in starts.iter().enumerate() {
            let end = starts
                .get(k + 1)
                .map(|&(ns, sc)| ns - sc)
                .unwrap_or(data.len());
            if end > s {
                types.push(data[s] & 0x1f);
            }
        }
        types
    }

    fn starts_with_start_code(data: &[u8]) -> bool {
        data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
    }

    #[test]
    fn rejects_non_h264_configs() {
        let mut cfg = h264_cfg();
        cfg.codec = Codec::Hevc;
        let err = Encoder::new(&cfg).err().unwrap().to_string();
        assert!(err.contains("H.264"), "unexpected error: {err}");
        cfg.codec = Codec::Av1;
        assert!(Encoder::new(&cfg).is_err());
    }

    #[test]
    fn rejects_odd_dimensions() {
        let mut cfg = h264_cfg();
        cfg.width = 321;
        assert!(Encoder::new(&cfg).is_err());
    }

    #[test]
    fn encodes_annexb_stream_over_30_frames() {
        let cfg = h264_cfg();
        let mut enc = Encoder::new(&cfg).unwrap();
        let mut src = TestPattern::new(cfg.width, cfg.height, 60);

        let mut units: Vec<EncodedUnit> = Vec::new();
        let mut pts_in: Vec<i64> = Vec::new();
        for _ in 0..30 {
            let frame = src.next().unwrap();
            pts_in.push(frame.pts_us);
            units.extend(enc.encode(&frame).unwrap());
            if pts_in.len() == 20 {
                enc.request_idr();
            }
        }
        // Zerolatency: one output unit per input frame, pts preserved in
        // submit order.
        assert_eq!(units.len(), 30, "one unit per frame");
        for (u, &pts) in units.iter().zip(&pts_in) {
            assert_eq!(u.pts_us, pts);
        }

        // First unit: IDR with parameter sets at its head (SPEC §5).
        assert!(starts_with_start_code(&units[0].data));
        assert!(units[0].keyframe);
        let types0 = nal_types(&units[0].data);
        // SPS(7), PPS(8) lead the stream, then the IDR slice (5). (SEI(6)
        // would be tolerated before the slices; assert what we emit.)
        assert_eq!(types0.first(), Some(&7), "SPS first: {types0:?}");
        assert_eq!(types0.get(1), Some(&8), "PPS second: {types0:?}");
        assert!(types0.contains(&5), "IDR slice present: {types0:?}");
        assert_eq!(types0.last(), Some(&5));

        for (i, u) in units.iter().enumerate() {
            assert!(starts_with_start_code(&u.data), "unit {i} lacks start code");
            let types = nal_types(&u.data);
            assert!(!types.is_empty(), "unit {i} parses to no NALs");
            // No B-frames: every picture is an IDR (5) or P (1) slice.
            assert!(
                types.contains(&1) || types.contains(&5),
                "unit {i} has no slice NAL: {types:?}"
            );
            // Keyframe flag must match the actual bitstream (x264's
            // b_keyframe == IDR slice present).
            assert_eq!(
                u.keyframe,
                types.contains(&5),
                "unit {i} keyframe flag vs NAL types: {types:?}"
            );
        }

        // request_idr at frame 20 (past the 120-frame keyint): the unit
        // must be an IDR with SPS/PPS in front of it.
        assert!(units[20].keyframe);
        let types20 = nal_types(&units[20].data);
        assert_eq!(types0.first(), Some(&7));
        assert!(types20.windows(2).any(|w| w == [7, 8]), "SPS+PPS on forced IDR: {types20:?}");
        assert!(types20.contains(&5));
        // Natural IDRs only where forced (keyint=120 > 30 frames).
        let idrs: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
        assert_eq!(idrs, vec![0, 20], "unexpected IDR positions: {idrs:?}");

        // Bitstream sizes are sane (non-empty; IDR bigger than P usually).
        assert!(units.iter().all(|u| !u.data.is_empty()));
    }
}
