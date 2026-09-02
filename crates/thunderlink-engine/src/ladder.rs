//! Adaptive bitrate ladder (SPEC §8): the receiver's ~500 ms
//! `Feedback::Report` (loss + jitter) drives the encoder bitrate within
//! [25%, 150%] of the negotiated base. Hooks were wired from day one;
//! this is the policy.
//!
//! Policy (deliberately conservative — a TB link is nearly lossless, so
//! sustained loss means real trouble, not congestion):
//! - loss ≥ 10% over two consecutive reports  → ×0.70 (down)
//! - loss ≥ 3% over two consecutive reports   → ×0.85 (down)
//! - loss == 0 && jitter < 2 ms for six consecutive reports → ×1.15 (up)
//! - anything else                            → hold
//!
//! Every change clamps to [MIN_MULT, MAX_MULT] × base and quantizes to
//! whole kbps. Feedback cadence is 500 ms (SPEC §5), so a full down-ramp
//! is ~1 s and an up-ramp ~3 s.

/// Minimum bitrate as a fraction of the negotiated base (SPEC §8: 25%).
pub const MIN_MULT: f64 = 0.25;
/// Maximum bitrate as a fraction of the negotiated base (SPEC §8: 150%).
pub const MAX_MULT: f64 = 1.5;
/// Down-step multiplier on sustained heavy loss.
const DOWN_HEAVY: f64 = 0.70;
/// Down-step multiplier on sustained moderate loss.
const DOWN_LIGHT: f64 = 0.85;
/// Up-step multiplier after a clean streak.
const UP: f64 = 1.15;
/// Consecutive clean reports (jitter < 2 ms, zero loss) before ramping up.
const CLEAN_STREAK: u32 = 6;

/// Adaptive ladder state over one streaming session.
#[derive(Debug)]
pub struct BitrateLadder {
    base_kbps: u32,
    current_kbps: u32,
    /// Reports in a row with loss ≥ 10% / ≥ 3% (heavy count first).
    loss_streak: (u32, u32),
    /// Reports in a row with zero loss and jitter < 2 ms.
    clean_streak: u32,
    /// Reports seen (for tests/stats).
    pub reports: u64,
}

/// What the ladder decided for one incoming report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderAction {
    /// Keep the current bitrate.
    Hold,
    /// New target bitrate (kbps), clamped to the band.
    Set(u32),
}

impl BitrateLadder {
    pub fn new(base_kbps: u32) -> Self {
        Self {
            base_kbps: base_kbps.max(1),
            current_kbps: base_kbps,
            loss_streak: (0, 0),
            clean_streak: 0,
            reports: 0,
        }
    }

    pub fn base_kbps(&self) -> u32 {
        self.base_kbps
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// Feed one receiver report; returns the action for the encoder.
    /// `lost_packets`/`received_frames` are the report's period counters
    /// (SPEC §5/§6); jitter is microseconds.
    pub fn report(&mut self, lost_packets: u64, received_frames: u64, jitter_us: u32) -> LadderAction {
        self.reports += 1;
        // Loss ratio per report period; frame count is the sane
        // denominator proxy (per-packet counts are not on the wire).
        let loss = if received_frames == 0 {
            0.0
        } else {
            lost_packets as f64 / received_frames as f64
        };
        let clean = loss == 0.0 && jitter_us < 2_000;

        if loss >= 0.10 {
            self.loss_streak.0 += 1;
            self.loss_streak.1 += 1;
        } else if loss >= 0.03 {
            self.loss_streak.0 = 0;
            self.loss_streak.1 += 1;
        } else {
            self.loss_streak = (0, 0);
        }
        self.clean_streak = if clean { self.clean_streak + 1 } else { 0 };

        let mult = if self.loss_streak.0 >= 2 {
            Some(DOWN_HEAVY)
        } else if self.loss_streak.1 >= 2 {
            Some(DOWN_LIGHT)
        } else if self.clean_streak >= CLEAN_STREAK {
            Some(UP)
        } else {
            None
        };
        let Some(mult) = mult else { return LadderAction::Hold };

        let target = (self.current_kbps as f64 * mult).round() as u32;
        let clamped = target.clamp(
            ((self.base_kbps as f64 * MIN_MULT).round()) as u32,
            ((self.base_kbps as f64 * MAX_MULT).round()) as u32,
        );
        if clamped == self.current_kbps {
            return LadderAction::Hold; // pinned at a band edge
        }
        self.current_kbps = clamped;
        LadderAction::Set(clamped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_on_moderate_noise() {
        let mut l = BitrateLadder::new(100_000);
        // Alternate 1% loss / clean forever: never two-in-a-row, never a
        // six clean streak.
        for i in 0..40 {
            let a = if i % 2 == 0 {
                l.report(10, 1000, 500)
            } else {
                l.report(0, 1000, 100)
            };
            assert_eq!(a, LadderAction::Hold, "iteration {i}");
        }
        assert_eq!(l.current_kbps(), 100_000);
    }

    #[test]
    fn ramps_down_on_sustained_loss_then_recovers() {
        let mut l = BitrateLadder::new(100_000);
        assert_eq!(l.report(200, 1000, 5_000), LadderAction::Hold); // streak 1
        assert_eq!(l.report(200, 1000, 5_000), LadderAction::Set(70_000)); // ×0.70
        // Clean streak ramps back up in 6-report steps.
        for _ in 0..5 {
            assert_eq!(l.report(0, 1000, 100), LadderAction::Hold);
        }
        assert_eq!(l.report(0, 1000, 100), LadderAction::Set(80_500)); // ×1.15
    }

    #[test]
    fn clamps_at_band_edges() {
        let mut l = BitrateLadder::new(100_000);
        // Hammer heavy loss: floor is 25% = 25_000.
        let mut last = LadderAction::Hold;
        for _ in 0..40 {
            last = l.report(500, 1000, 9_000);
        }
        assert_eq!(l.current_kbps(), 25_000);
        assert_eq!(last, LadderAction::Hold); // pinned at the floor
        // Clean forever: ceiling is 150% = 150_000.
        let mut last = LadderAction::Hold;
        for _ in 0..60 {
            last = l.report(0, 1000, 100);
        }
        assert_eq!(l.current_kbps(), 150_000);
        assert_eq!(last, LadderAction::Hold); // pinned at the ceiling
    }

    #[test]
    fn light_loss_step_is_smaller() {
        let mut l = BitrateLadder::new(200_000);
        l.report(50, 1000, 100); // 5% loss, streak 1
        assert_eq!(l.report(50, 1000, 100), LadderAction::Set(170_000)); // ×0.85
    }

    #[test]
    fn zero_received_frames_is_clean_not_loss() {
        let mut l = BitrateLadder::new(100_000);
        assert_eq!(l.report(0, 0, 100), LadderAction::Hold);
        assert_eq!(l.loss_streak, (0, 0));
    }
}
