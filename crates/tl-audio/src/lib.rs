//! ThunderLink audio pipeline (SPEC §12): Opus codec wrappers, synthetic
//! sine tone source, UDP audio channel, and the receiver jitter buffer.
//!
//! Platform-neutral by design — OS capture (§12.2) and output (§12.5)
//! live in the per-OS crates. Everything here operates on fixed 10 ms
//! frames of 48 kHz stereo interleaved i16 PCM, one Opus packet per
//! frame, fire-and-forget UDP (§12.1).

#![forbid(unsafe_code)]

pub mod chan;
pub mod codec;
pub mod jitter;
pub mod sine;

pub use chan::{AudioPacket, AudioRx, AudioTx};
pub use codec::{OpusDecoder, OpusEncoder};
pub use jitter::{JitterBuffer, JitterStats, PopResult};
pub use sine::SineSource;

/// Pipeline sample rate (SPEC §12.1): 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channel count: stereo.
pub const CHANNELS: usize = 2;
/// Samples per channel per frame (SPEC §12.2/§12.3): 10 ms @ 48 kHz.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize) / 100;
/// One interleaved stereo frame: `FRAME_SAMPLES * CHANNELS` i16 samples.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
