//! tl-macos-audio — macOS system-audio capture and playback for ThunderLink
//! (SPEC §12.2 / §12.5).
//!
//! * [`SystemTap`] captures whole-system audio through a Core Audio process
//!   tap (`AudioHardwareCreateProcessTap`, public since macOS 14.2) exposed
//!   as an input stream of a private aggregate device, converted in the IO
//!   thread to 48 kHz interleaved stereo f32.
//! * [`Output`] plays 48 kHz interleaved stereo f32 through the default
//!   output device via a `kAudioUnitSubType_DefaultOutput` AudioUnit.
//!
//! Implementation notes (deviations from the original contract, dictated by
//! the SDK headers on this machine):
//!
//! * `CATapDescription` is an Objective-C **class**
//!   (`CoreAudio.framework/Headers/CATapDescription.h`), not a C struct; it
//!   is built through the Objective-C runtime, mirroring insidegui/AudioCap.
//! * Sample-rate conversion uses `AudioConverterFillComplexBuffer`, not
//!   `AudioConverterConvertComplexBuffer` — the SDK header explicitly fails
//!   the latter "for any conversion where there is a variable relationship
//!   between the input and output data buffer sizes. This includes sample
//!   rate conversions".
//! * The IOProc registers via the public function-pointer API
//!   `AudioDeviceCreateIOProcID` (available since 10.5, non-deprecated)
//!   instead of `AudioDeviceCreateIOProcIDWithBlock`, which would require
//!   hand-rolling an Objective-C block in Rust.
//! * `AudioUnitStart`/`AudioUnitStop` are link-time macros for
//!   `AudioOutputUnitStart`/`AudioOutputUnitStop` (AudioOutputUnit.h) — the
//!   FFI binds the real symbols.
//! * CoreAudio hands out `AudioBufferList`s that are only 4-byte aligned in
//!   practice; every access to a caller-provided list goes through
//!   unaligned-safe helpers in `ffi`.
//! * The tap IOProc deliberately ignores `outOutputData` (nothing plays
//!   through the private aggregate; the HAL may hand the client unformatted
//!   output descriptors — touching them PAC-faults on arm64).
//!
//! Permissions: process taps fall under the macOS audio-capture TCC grant
//! (bundled apps need `NSAudioCaptureUsageDescription`); denial surfaces as
//! an error mentioning "permission" or as a permanently silent tap.
//!
//! The crate is empty on non-macOS targets.
#![cfg(target_os = "macos")]

mod ffi;
mod objc;
mod output;
mod ring;
mod system_tap;

pub use output::Output;
pub use system_tap::SystemTap;

#[cfg(test)]
mod tests {
    use super::*;

    fn logger() {
        // Plain stderr logger (not is_test capture) so logs survive a crash.
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Info)
            .is_test(false)
            .try_init();
    }

    /// `SystemTap::new` depends on machine state (TCC grant, default output
    /// device, macOS version): it may legitimately succeed or fail headless.
    /// The contract: never panic, always clean up, and stay re-creatable.
    #[test]
    fn system_tap_new_is_tolerant_and_cleans_up() {
        logger();
        match SystemTap::new() {
            Ok(mut tap) => {
                assert_eq!(tap.sample_rate(), 48_000.0);
                let pcm = tap.next_pcm(480);
                assert_eq!(pcm.len(), 960); // 480 frames × interleaved stereo
                drop(tap);
                // Aggregate destruction is asynchronous; a second creation
                // with a fresh UID proves the teardown released everything.
                match SystemTap::new() {
                    Ok(mut t2) => {
                        assert_eq!(t2.next_pcm(16).len(), 32);
                    }
                    Err(e) => eprintln!("second SystemTap::new failed (tolerated): {e:#}"),
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                eprintln!("SystemTap::new failed headless (tolerated): {msg}");
                assert!(!msg.is_empty());
            }
        }
    }

    /// The default-output AudioUnit exists on any Mac, no TCC involved:
    /// full lifecycle must work headless, playing silence.
    #[test]
    fn output_lifecycle_plays_silence() {
        let mut out = Output::new().expect("default output unit must open on macOS");
        // Silence before start just buffers.
        out.write(&[0.0; 960]);
        out.start().expect("start");
        std::thread::sleep(std::time::Duration::from_millis(120));
        out.write(&vec![0.0; 960]);
        assert!(out.buffered_samples() <= 96_000);
        std::thread::sleep(std::time::Duration::from_millis(60));
        out.stop().expect("stop");
        // Restart after stop is part of the contract.
        out.start().expect("restart");
        out.stop().expect("stop again");
        // Odd trailing sample is dropped, not misaligning the channel pairs.
        out.write(&[0.5, -0.5, 0.25]);
        drop(out);
    }

    /// TL_E2E=1 only: play a 2 s 440 Hz sine through `Output` and capture it
    /// through `SystemTap`, requiring ≥ 1 s of non-silent capture. Skips
    /// gracefully (with measured levels) when the tap sees only silence —
    /// muted output, no audible system audio, or a TCC denial.
    #[test]
    fn e2e_sine_through_system_tap() {
        logger();
        if std::env::var("TL_E2E").ok().as_deref() != Some("1") {
            eprintln!("skipping (set TL_E2E=1 and ensure audible system audio)");
            return;
        }
        let mut tap = match SystemTap::new() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("SKIP: SystemTap::new failed: {e:#}");
                return;
            }
        };
        let mut out = Output::new().expect("output unit");
        out.start().expect("start output");

        // 440 Hz at amplitude 0.3, written in 10 ms chunks while capturing.
        let chunk_frames = 480usize;
        let total_chunks = 200usize; // 2 s
        let mut phase = 0.0f64;
        let omega = 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
        let mut captured_peak = 0.0f32;
        let mut captured_energy = 0.0f64;
        let mut loud_100ms_windows = 0usize;
        let mut window_peak = 0.0f32;
        for i in 0..total_chunks {
            let mut chunk = Vec::with_capacity(chunk_frames * 2);
            for _ in 0..chunk_frames {
                let s = (phase.sin() * 0.3) as f32;
                chunk.push(s);
                chunk.push(s);
                phase += omega;
            }
            out.write(&chunk);
            let pcm = tap.next_pcm(chunk_frames);
            for s in &pcm {
                let m = s.abs();
                if m > captured_peak {
                    captured_peak = m;
                }
                if m > window_peak {
                    window_peak = m;
                }
                captured_energy += (*s as f64) * (*s as f64);
            }
            if (i + 1) % 10 == 0 {
                if window_peak > 1e-3 {
                    loud_100ms_windows += 1;
                }
                window_peak = 0.0;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        out.stop().expect("stop output");
        drop(out);
        drop(tap);

        let rms = (captured_energy / (total_chunks * chunk_frames * 2) as f64).sqrt();
        eprintln!(
            "e2e levels: peak={captured_peak:.5} rms={rms:.5} loud_windows={loud_100ms_windows}/20"
        );
        if captured_peak < 1e-3 {
            eprintln!(
                "SKIP: tap measured only silence (no audible system audio or audio-capture \
                 permission denied): peak={captured_peak:.5} rms={rms:.5}"
            );
            return;
        }
        assert!(
            loud_100ms_windows >= 10,
            "expected ≥ 1 s of non-silent capture, got {loud_100ms_windows} × 100 ms windows \
             (peak {captured_peak:.5}, rms {rms:.5})"
        );
    }
}
