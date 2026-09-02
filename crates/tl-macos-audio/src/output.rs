//! Default-output playback via an AudioUnit output component
//! (`kAudioUnitType_Output` / `kAudioUnitSubType_DefaultOutput`): the input
//! scope is set to 48 kHz interleaved stereo f32 (the unit converts to the
//! hardware format internally) and a render callback drains an SPSC ring
//! that [`Output::write`] fills.
use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};

use crate::ffi as ca;
use crate::ring::SpscRing;

/// Rate/format of everything pushed through [`Output::write`] (SPEC §12.5).
pub const OUTPUT_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
/// ~0.34 s of interleaved stereo — the session layer's jitter buffer sits in
/// front of this; the ring only absorbs scheduling jitter.
const RING_SAMPLES: usize = 1 << 15;

/// Playback handle to the default system output device. [`Output::write`] is
/// the producer side of a lock-free ring; the AudioUnit render callback is
/// the consumer. Starved render cycles emit silence.
pub struct Output {
    unit: ca::AudioUnit,
    /// Keeps the ring alive at least until the unit is disposed.
    _ring: Arc<SpscRing>,
    ring: Arc<SpscRing>,
    started: bool,
}

/// Render callback: pull from the ring into the (interleaved stereo) buffer
/// the output unit requests; silence whatever the ring cannot supply.
unsafe extern "C" fn render_callback(
    refcon: *mut c_void,
    _flags: *mut ca::AudioUnitRenderActionFlags,
    _timestamp: *const c_void,
    _bus: ca::AudioUnitElement,
    frames: ca::UInt32,
    io_data: *mut ca::AudioBufferList,
) -> ca::OSStatus {
    // SAFETY: refcon is the Arc'd ring pointer registered in Output::new;
    // the unit is stopped and disposed before the Arc can die (Output::drop),
    // so no callback can race the ring's destruction.
    let ring = unsafe { &*refcon.cast::<SpscRing>() };
    if io_data.is_null() {
        return 0;
    }
    let n = ca::abl_buffer_count(io_data.cast());
    if n == 1 {
        // SAFETY: index 0 < count checked above.
        let b = ca::abl_buffer(io_data.cast(), 0);
        if !b.data.is_null() && b.byte_size >= 4 {
            let cap_samples = b.byte_size as usize / 4;
            // SAFETY: the render contract grants write access to mData for
            // mDataByteSize bytes during the callback; the hardware buffer is
            // at least 4-byte aligned for f32 samples.
            let buf =
                unsafe { std::slice::from_raw_parts_mut(b.data.cast::<f32>(), cap_samples) };
            let want = (frames as usize * CHANNELS as usize).min(cap_samples);
            let got = ring.pop(&mut buf[..want]);
            buf[got..].fill(0.0);
        }
        return 0;
    }
    // Unexpected layout (we configured one interleaved buffer): stay silent.
    for i in 0..n {
        // SAFETY: i < count of the unit-provided list.
        let b = ca::abl_buffer(io_data.cast(), i);
        if !b.data.is_null() {
            // SAFETY: render contract grants write access for this cycle.
            unsafe { std::ptr::write_bytes(b.data.cast::<u8>(), 0, b.byte_size as usize) };
        }
    }
    0
}

impl Output {
    /// Open the default output device at 48 kHz interleaved stereo f32.
    /// Initializes (but does not start) the unit; audio starts at
    /// [`Output::start`].
    pub fn new() -> Result<Output> {
        let desc = ca::AudioComponentDescription {
            componentType: ca::K_AUDIO_UNIT_TYPE_OUTPUT,
            componentSubType: ca::K_AUDIO_UNIT_SUBTYPE_DEFAULT_OUTPUT,
            componentManufacturer: 0, // any manufacturer
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: desc is a valid AudioComponentDescription.
        let component = unsafe { ca::AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        if component.is_null() {
            bail!("no default-output AudioUnit component found");
        }
        let mut unit: ca::AudioUnit = std::ptr::null_mut();
        // SAFETY: component handle from FindNext; out pointer valid.
        let st = unsafe { ca::AudioComponentInstanceNew(component, &mut unit) };
        if st != 0 || unit.is_null() {
            bail!(
                "AudioComponentInstanceNew failed: {} (null: {})",
                ca::osstatus_str(st),
                unit.is_null()
            );
        }
        let rollback_unit = unit;
        let built = (|| -> Result<Output> {
            let asbd = ca::AudioStreamBasicDescription {
                mSampleRate: OUTPUT_RATE,
                mFormatID: ca::K_AUDIO_FORMAT_LINEAR_PCM,
                mFormatFlags: ca::K_AUDIO_FORMAT_FLAG_IS_FLOAT
                    | ca::K_AUDIO_FORMAT_FLAG_IS_PACKED,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: CHANNELS,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            // SAFETY: valid pointers/sizes for this property.
            let st = unsafe {
                ca::AudioUnitSetProperty(
                    unit,
                    ca::K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    ca::K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &asbd as *const _ as *const c_void,
                    std::mem::size_of::<ca::AudioStreamBasicDescription>() as u32,
                )
            };
            if st != 0 {
                return Err(anyhow!(
                    "setting 48 kHz stereo f32 stream format failed: {}",
                    ca::osstatus_str(st)
                ));
            }

            let ring = Arc::new(SpscRing::with_capacity_samples(RING_SAMPLES));
            let cb = ca::AURenderCallbackStruct {
                inputProc: render_callback,
                inputProcRefCon: Arc::as_ptr(&ring) as *mut c_void,
            };
            // SAFETY: valid pointers/sizes for this property.
            let st = unsafe {
                ca::AudioUnitSetProperty(
                    unit,
                    ca::K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                    ca::K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &cb as *const _ as *const c_void,
                    std::mem::size_of::<ca::AURenderCallbackStruct>() as u32,
                )
            };
            if st != 0 {
                return Err(anyhow!(
                    "setting render callback failed: {}",
                    ca::osstatus_str(st)
                ));
            }
            // SAFETY: unit is a valid instance.
            let st = unsafe { ca::AudioUnitInitialize(unit) };
            if st != 0 {
                return Err(anyhow!(
                    "AudioUnitInitialize failed: {}",
                    ca::osstatus_str(st)
                ));
            }
            Ok(Output { unit, _ring: ring.clone(), ring, started: false })
        })();
        match built {
            Ok(o) => {
                log::info!("output opened: default device, {OUTPUT_RATE} Hz stereo f32");
                Ok(o)
            }
            Err(e) => {
                // SAFETY: unit was created successfully above; dispose it once.
                unsafe { ca::AudioComponentInstanceDispose(rollback_unit) };
                Err(e)
            }
        }
    }

    /// Queue interleaved stereo f32 samples (48 kHz). A trailing odd sample
    /// (incomplete frame) is dropped. Non-blocking; on overflow the oldest
    /// buffered samples are overwritten (stale audio is dropped, not delayed).
    pub fn write(&mut self, interleaved_stereo: &[f32]) {
        let n = interleaved_stereo.len() & !1;
        if n > 0 {
            self.ring.push(&interleaved_stereo[..n]);
        }
    }

    /// Begin pulling from the ring to the hardware. Idempotent.
    pub fn start(&mut self) -> Result<()> {
        if !self.started {
            // SAFETY: unit is a valid, initialized instance.
            let st = unsafe { ca::AudioOutputUnitStart(self.unit) };
            if st != 0 {
                return Err(anyhow!("AudioUnitStart failed: {}", ca::osstatus_str(st)));
            }
            self.started = true;
        }
        Ok(())
    }

    /// Stop playback (subsequent render cycles are silent until the next
    /// start). Idempotent.
    pub fn stop(&mut self) -> Result<()> {
        if self.started {
            // SAFETY: unit is a valid, initialized, started instance.
            let st = unsafe { ca::AudioOutputUnitStop(self.unit) };
            if st != 0 {
                return Err(anyhow!("AudioUnitStop failed: {}", ca::osstatus_str(st)));
            }
            self.started = false;
        }
        Ok(())
    }

    /// Samples buffered but not yet rendered (diagnostics).
    pub fn buffered_samples(&self) -> usize {
        self.ring.available()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Stop before dispose so no render callback can be in flight when
        // the ring Arc dies with this struct.
        // SAFETY: unit is the valid instance from new(); dispose releases
        // everything, making double-stop/uninit unnecessary.
        unsafe {
            let _ = ca::AudioOutputUnitStop(self.unit);
            let _ = ca::AudioUnitUninitialize(self.unit);
            ca::AudioComponentInstanceDispose(self.unit);
        }
    }
}
