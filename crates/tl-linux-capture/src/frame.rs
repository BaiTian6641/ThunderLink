//! Frame buffer type shared by the capture sources and the encoder, plus
//! the BGRA→I420 colorspace conversion (BT.601 limited range).

use anyhow::{bail, Result};

/// One frame of desktop/synthetic video: tightly packed 32BGRA bytes
/// (`stride == width * 4`, little-endian B,G,R,A per pixel), with a
/// wall-clock presentation timestamp in µs stamped at the source
/// (`tl_proto::time::now_us` for live capture).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
    pub bgra: Vec<u8>,
}

impl RawFrame {
    /// Construct while validating the packed-layout invariant
    /// (`bgra.len() == width * height * 4`).
    pub fn new(width: u32, height: u32, pts_us: i64, bgra: Vec<u8>) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("frame dimensions must be > 0, got {width}x{height}");
        }
        let expect = width as usize * height as usize * 4;
        if bgra.len() != expect {
            bail!(
                "bgra buffer is {} bytes; need {expect} ({width}x{height}x4, tightly packed)",
                bgra.len()
            );
        }
        Ok(Self { width, height, pts_us, bgra })
    }
}

/// Reusable planar I420 buffer: Y plane (`width*height`), then U, then V
/// (`chroma_width*chroma_height` each, 2x2-subsampled), all tightly
/// packed. Resized once per stream and refilled by [`convert`].
pub(crate) struct I420 {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl I420 {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        Self {
            width,
            height,
            data: vec![0; width * height + 2 * cw * ch],
        }
    }

    /// (Y, U, V) planes, tightly packed with `stride == plane width`.
    pub(crate) fn planes(&self) -> [&[u8]; 3] {
        let cw = self.width.div_ceil(2);
        let ch = self.height.div_ceil(2);
        let ysz = self.width * self.height;
        let csz = cw * ch;
        [
            &self.data[..ysz],
            &self.data[ysz..ysz + csz],
            &self.data[ysz + csz..ysz + 2 * csz],
        ]
    }

    /// Chroma plane width (== Y-plane stride of U/V).
    pub(crate) fn chroma_width(&self) -> usize {
        self.width.div_ceil(2)
    }
}

/// BT.601 limited-range ("studio swing") BGRA→YUV integer conversion, the
/// classic libswscale/libyuv coefficients:
///
/// ```text
/// Y = ((  66*R + 129*G +  25*B + 128) >> 8) +  16   (16..=235)
/// U = (( -38*R -  74*G + 112*B + 128) >> 8) + 128
/// V = (( 112*R -  94*G -  18*B + 128) >> 8) + 128
/// ```
///
/// Chroma is box-averaged over each 2x2 luma block; odd trailing
/// rows/columns reuse the last pixel. `out` must have been created with
/// the frame's dimensions (the encoder does this once per stream).
pub(crate) fn convert(frame: &RawFrame, out: &mut I420) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    debug_assert_eq!(out.width, w);
    debug_assert_eq!(out.height, h);
    debug_assert_eq!(frame.bgra.len(), w * h * 4);

    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let ysz = w * h;
    let csz = cw * ch;
    let (y, rest) = out.data.split_at_mut(ysz);
    let (u, v) = rest.split_at_mut(csz);

    for (i, px) in frame.bgra.chunks_exact(4).enumerate() {
        let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
        // Quotient is 0..=219 for in-range RGB; +16 gives 16..=235.
        let yq = (66 * r + 129 * g + 25 * b + 128) >> 8;
        y[i] = (yq + 16).clamp(0, 255) as u8;
    }

    for cy in 0..ch {
        let row0 = cy * 2;
        let row1 = (row0 + 1).min(h - 1);
        for cx in 0..cw {
            let x0 = cx * 2;
            let x1 = (x0 + 1).min(w - 1);
            let (mut bs, mut gs, mut rs) = (0i32, 0i32, 0i32);
            for &row in &[row0, row1] {
                for &x in &[x0, x1] {
                    let p = (row * w + x) * 4;
                    bs += frame.bgra[p] as i32;
                    gs += frame.bgra[p + 1] as i32;
                    rs += frame.bgra[p + 2] as i32;
                }
            }
            // Always exactly 4 samples (odd edges duplicate), so >> 2 is
            // an exact average.
            let (b, g, r) = (bs >> 2, gs >> 2, rs >> 2);
            let i = cy * cw + cx;
            u[i] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            v[i] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid-color 8x8 frame → every Y/U/V sample equals the known
    /// BT.601 limited-range value (no subsampling error for solids).
    #[test]
    fn bgra_to_i420_known_pixels() {
        // (B, G, R) → (Y, U, V) reference values, tolerance ±2 for
        // rounding differences.
        let cases: &[(&str, (u8, u8, u8), (u8, u8, u8))] = &[
            ("white", (255, 255, 255), (235, 128, 128)),
            ("black", (0, 0, 0), (16, 128, 128)),
            ("red", (0, 0, 255), (82, 90, 240)),
            ("green", (0, 255, 0), (145, 54, 34)),
            ("blue", (255, 0, 0), (41, 240, 110)),
        ];
        for &(name, (b, g, r), (ey, eu, ev)) in cases {
            let px = [b, g, r, 255];
            let bgra = px.repeat(8 * 8);
            let frame = RawFrame::new(8, 8, 0, bgra).unwrap();
            let mut planes = I420::new(8, 8);
            convert(&frame, &mut planes);
            let [y, u, v] = planes.planes();
            let yv = y[0];
            let uv = u[0];
            let vv = v[0];
            assert!(
                (yv as i32 - ey as i32).abs() <= 2,
                "{name}: Y={yv}, want {ey}±2"
            );
            assert!(
                (uv as i32 - eu as i32).abs() <= 2,
                "{name}: U={uv}, want {eu}±2"
            );
            assert!(
                (vv as i32 - ev as i32).abs() <= 2,
                "{name}: V={vv}, want {ev}±2"
            );
            // Solid in, solid out: every sample identical.
            assert!(y.iter().all(|&s| s == yv), "{name}: Y plane not uniform");
            assert!(u.iter().all(|&s| s == uv), "{name}: U plane not uniform");
            assert!(v.iter().all(|&s| s == vv), "{name}: V plane not uniform");
        }
    }

    /// Odd dimensions: chroma dims round up; no panics, correct lengths.
    #[test]
    fn i420_layout_odd_dims() {
        let frame = RawFrame::new(5, 3, 0, vec![128u8; 5 * 3 * 4]).unwrap();
        let mut planes = I420::new(5, 3);
        convert(&frame, &mut planes);
        let [y, u, v] = planes.planes();
        assert_eq!(y.len(), 5 * 3);
        assert_eq!(u.len(), 3 * 2);
        assert_eq!(v.len(), 3 * 2);
    }
}
