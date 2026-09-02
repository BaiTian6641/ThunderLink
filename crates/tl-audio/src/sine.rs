//! Synthetic sine tone source (SPEC §12.7 validation: "a synthetic 1 kHz
//! sine tone source (no TCC needed)"). Also the headless test signal for
//! codec round-trips.

use std::f64::consts::TAU;

use crate::{FRAME_LEN, FRAME_SAMPLES, SAMPLE_RATE};

/// Tone amplitude as a fraction of full scale.
const AMPLITUDE: f64 = 0.4;
const I16_FULL_SCALE: f64 = i16::MAX as f64;

/// Phase-continuous sine generator: one 10 ms frame of interleaved
/// stereo i16 PCM per [`SineSource::next_frame`] call. Phase is carried
/// across calls, so consecutive frames concatenate into a continuous
/// tone (no clicks at frame boundaries).
pub struct SineSource {
    freq_hz: f64,
    /// Current phase in cycles, folded into `[0, 1)` so long streams
    /// never lose f64 precision.
    phase: f64,
}

impl SineSource {
    pub fn new(freq_hz: f64) -> Self {
        Self {
            freq_hz,
            phase: 0.0,
        }
    }

    /// Next frame: exactly [`FRAME_LEN`] interleaved stereo samples
    /// (both channels carry the same tone).
    pub fn next_frame(&mut self) -> Vec<i16> {
        let step = self.freq_hz / SAMPLE_RATE as f64;
        let mut out = Vec::with_capacity(FRAME_LEN);
        for _ in 0..FRAME_SAMPLES {
            let sample = (self.phase * TAU).sin() * AMPLITUDE * I16_FULL_SCALE;
            self.phase += step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            out.push(sample as i16);
            out.push(sample as i16);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One full-scale unit, the unit the "5 % of amplitude" continuity
    /// bound is measured against. The largest adjacent-sample step a
    /// 440 Hz tone at 48 kHz can take is
    /// `A·2·sin(π·f/fs) ≈ 755` (≈ 2.3 % of full scale); any phase reset
    /// jumps by up to `2A ≈ 26214`. 5 % of full scale (1638) sits well
    /// above the physical bound and far below any discontinuity.
    const JUMP_LIMIT: f64 = 0.05 * I16_FULL_SCALE;

    #[test]
    fn frame_shape_is_480x2() {
        let mut src = SineSource::new(440.0);
        let f = src.next_frame();
        assert_eq!(f.len(), 960);
        // Interleaved stereo: both channels identical.
        for pair in f.chunks(2) {
            assert_eq!(pair[0], pair[1]);
        }
        // Peak stays at the requested amplitude (0.4 full scale ± 1 LSB).
        let peak = f.iter().map(|&s| s.unsigned_abs()).max().unwrap();
        assert!((peak as f64 - 0.4 * I16_FULL_SCALE).abs() <= 2.0);
    }

    #[test]
    fn phase_continuous_across_frames() {
        let mut src = SineSource::new(440.0);
        let mut prev_last: Option<i16> = None;
        for i in 0..200 {
            let f = src.next_frame();
            if let Some(last) = prev_last {
                let jump = (f[0] as i64 - last as i64).abs();
                assert!(
                    (jump as f64) < JUMP_LIMIT,
                    "frame {i}: boundary jump {jump} exceeds {JUMP_LIMIT}"
                );
            }
            prev_last = Some(f[FRAME_LEN - 1]);
        }
    }

    #[test]
    fn matches_closed_form_reference() {
        // Exact continuity: global mono sample m equals
        // round(A·sin(2π·f·m/fs)) regardless of frame boundaries.
        let freq = 440.0;
        let mut src = SineSource::new(freq);
        let mut m = 0usize;
        for _ in 0..5 {
            for pair in src.next_frame().chunks(2) {
                let want = (freq * m as f64 / SAMPLE_RATE as f64 * TAU).sin()
                    * 0.4
                    * I16_FULL_SCALE;
                for &s in pair {
                    assert!(
                        (s as f64 - want).abs() <= 1.0,
                        "sample {m}: {s} vs closed form {want:.1}"
                    );
                }
                m += 1;
            }
        }
    }
}
