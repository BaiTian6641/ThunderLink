//! Time helpers. Timestamps are wall-clock microseconds; only deltas
//! matter within one session (no cross-host clock sync in v1).

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock microseconds since Unix epoch.
pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
