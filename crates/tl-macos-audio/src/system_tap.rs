//! Whole-system audio capture via a Core Audio process tap (public API since
//! macOS 14.2), following the insidegui/AudioCap topology:
//!
//! 1. `CATapDescription` (stereo global tap, unmuted) →
//!    `AudioHardwareCreateProcessTap` → tap object;
//! 2. private aggregate device with the default output device as clocking
//!    sub-device and the tap in the tap list → exposes the tap as an *input*
//!    stream at the tap's native format;
//! 3. an IOProc converts every input buffer to 48 kHz interleaved stereo f32
//!    (`AudioConverterFillComplexBuffer`) and pushes it into an SPSC ring.
//!
//! TCC: creating a process tap can require the macOS audio-capture permission
//! (bundled apps: `NSAudioCaptureUsageDescription`). Denial surfaces as an
//! error mentioning "permission", or — on some OS versions — as a tap that
//! only ever yields silence.
use std::ffi::c_void;
use std::fmt;

use anyhow::{anyhow, bail, Context, Result};

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use crate::ffi as ca;
use crate::objc;
use crate::ring::SpscRing;

/// Rate of every sample `next_pcm` returns (SPEC §12.2: 48 kHz stereo f32).
pub const OUTPUT_RATE: f64 = 48_000.0;
/// Channels of every sample `next_pcm` returns.
pub const OUTPUT_CHANNELS: u32 = 2;
/// CATapMuteBehavior CATapUnmuted (CATapDescription.h:32).
const CATAP_UNMUTED: i64 = 0;
/// Ring capacity: ~0.7 s of interleaved stereo at 48 kHz — far more than the
/// 10 ms frames the encoder pulls.
const RING_SAMPLES: usize = 1 << 16;
/// Upper bound of frames accepted from one IO cycle (scratch buffer size).
const MAX_IO_FRAMES: usize = 8192;

/// State shared with the HAL IO thread. Owned by `SystemTap`; the IOProc gets
/// a raw pointer that stays valid until `AudioDeviceDestroyIOProcID` returns.
struct IoState {
    ring: SpscRing,
    converter: ca::AudioConverterRef,
    in_asbd: ca::AudioStreamBasicDescription,
    /// Interleaved-stereo conversion scratch, IO-thread-only.
    scratch: Vec<f32>,
    /// Input buffer list served to the converter during the current IO cycle.
    pending: Option<PendingInput>,
}

struct PendingInput {
    abl: *const c_void,
    frames_left: u32,
    offset_frames: u32,
}

impl Drop for IoState {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            // SAFETY: converter came from AudioConverterNew and is disposed once.
            let st = unsafe { ca::AudioConverterDispose(self.converter) };
            if st != 0 {
                log::warn!("AudioConverterDispose failed: {}", ca::osstatus_str(st));
            }
        }
    }
}

/// Whole-system Core Audio process tap. Move it between threads freely; all
/// cross-thread access goes through the lock-free SPSC ring.
pub struct SystemTap {
    state: *mut IoState,
    io_proc: ca::AudioDeviceIOProcID,
    /// Aggregate device exposing the tap.
    device: ca::AudioObjectID,
    tap: ca::AudioObjectID,
    /// Last ring-overflow count seen by `next_pcm` (log throttling).
    last_dropped: std::cell::Cell<usize>,
}

// SAFETY: SystemTap owns its IoState exclusively; every cross-thread access
// (IO thread pushes, owner pops) goes through the lock-free SPSC protocol
// documented on SpscRing, and the HAL only touches the state between
// AudioDeviceStart and AudioDeviceDestroyIOProcID.
unsafe impl Send for SystemTap {}

impl fmt::Debug for SystemTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemTap")
            .field("tap", &self.tap)
            .field("device", &self.device)
            .finish()
    }
}

/// Hint appended to errors on paths where TCC (the audio-capture permission
/// prompt) is a plausible cause.
const PERMISSION_HINT: &str = " — this can be a macOS permission denial: allow this \
application (or the terminal running it) to record audio in System Settings › Privacy & \
Security, and note bundled apps need NSAudioCaptureUsageDescription";

fn check(status: ca::OSStatus, what: &str, permission: bool) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    let mut msg = format!("{} failed: {}", what, ca::osstatus_str(status));
    if permission {
        msg.push_str(PERMISSION_HINT);
    }
    Err(anyhow!(msg))
}

fn prop_addr(
    selector: ca::AudioObjectPropertySelector,
    scope: ca::AudioObjectPropertyScope,
) -> ca::AudioObjectPropertyAddress {
    ca::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: ca::ELEMENT_MAIN,
    }
}

/// Read a fixed-size property into a `T` (ASBDs, object IDs…).
///
/// # Safety
/// `T` must be the exact type the selector returns.
unsafe fn get_prop<T>(
    object: ca::AudioObjectID,
    address: &ca::AudioObjectPropertyAddress,
    what: &str,
) -> Result<T> {
    let mut size = std::mem::size_of::<T>() as ca::UInt32;
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    // SAFETY: caller guarantees T matches the property's type; all pointers
    // are valid for the duration of the call.
    let st = unsafe {
        ca::AudioObjectGetPropertyData(
            object,
            address,
            0,
            std::ptr::null(),
            &mut size,
            out.as_mut_ptr().cast(),
        )
    };
    check(st, what, false)?;
    // SAFETY: AudioObjectGetPropertyData wrote a T on success.
    Ok(unsafe { out.assume_init() })
}

/// Read a CFString property the caller releases (device/tap UIDs).
///
/// # Safety
/// The selector must return a CFStringRef under the create rule.
unsafe fn get_cfstring_prop(
    object: ca::AudioObjectID,
    selector: ca::AudioObjectPropertySelector,
    what: &str,
) -> Result<String> {
    let mut s = std::mem::MaybeUninit::<*const c_void>::uninit();
    let mut size = std::mem::size_of_val(&s) as ca::UInt32;
    // SAFETY: selector returns CFStringRef; pointers valid for the call.
    let st = unsafe {
        ca::AudioObjectGetPropertyData(
            object,
            &prop_addr(selector, ca::SCOPE_GLOBAL),
            0,
            std::ptr::null(),
            &mut size,
            s.as_mut_ptr().cast(),
        )
    };
    check(st, what, false)?;
    // SAFETY: assume_init is justified by the successful GetPropertyData;
    // wrap_under_create_rule adopts the +1 reference the HAL gave us.
    let cf = unsafe {
        <CFString as TCFType>::wrap_under_create_rule(s.assume_init().cast())
    };
    Ok(cf.to_string())
}

/// The converter's input-data callback: serves the IO cycle's pending input
/// buffer list zero-copy, signalling end-of-data once drained.
unsafe extern "C" fn conv_input_proc(
    _converter: ca::AudioConverterRef,
    io_number_packets: *mut ca::UInt32,
    io_data: *mut ca::AudioBufferList,
    _packet_desc: *mut c_void,
    user: *mut c_void,
) -> ca::OSStatus {
    // SAFETY: `user` is the IoState pointer registered with the IOProc; it is
    // valid until AudioDeviceDestroyIOProcID returns (SystemTap::drop), and
    // the converter invokes us synchronously inside the IOProc.
    let st = unsafe { &mut *user.cast::<IoState>() };
    let requested = unsafe { *io_number_packets };
    let drained = st
        .pending
        .as_ref()
        .map(|p| p.frames_left == 0 || requested == 0)
        .unwrap_or(true);
    if drained {
        // No (more) input: report zero packets, noErr — per AudioConverter.h
        // this makes FillComplexBuffer return what it already produced.
        unsafe { *io_number_packets = 0 };
        return 0;
    }
    let p = st.pending.as_mut().unwrap();
    let give = requested.min(p.frames_left);
    let bpf = st.in_asbd.mBytesPerFrame;
    let byte_offset = p.offset_frames as usize * bpf as usize;
    let count = ca::abl_buffer_count(p.abl);
    for i in 0..count {
        // SAFETY: i < count of the HAL-provided list.
        let src = ca::abl_buffer(p.abl, i);
        // SAFETY: io_data is provided by AudioConverterFillComplexBuffer and
        // holds at least as many buffers as the input format has; we re-point
        // mData into the HAL-owned input buffer, which outlives this
        // synchronous conversion.
        unsafe {
            ca::abl_write_buffer(
                io_data.cast(),
                i,
                ca::BufferDesc {
                    channels: src.channels,
                    byte_size: give * bpf,
                    data: src.data.cast::<u8>().add(byte_offset).cast(),
                },
            )
        };
    }
    // SAFETY: same io_data capacity contract as above.
    unsafe { ca::abl_set_buffer_count(io_data.cast(), count as u32) };
    p.frames_left -= give;
    p.offset_frames += give;
    unsafe { *io_number_packets = give };
    0
}

/// The HAL IOProc: receives the tap's input at its native format, converts to
/// 48 kHz interleaved stereo f32 and pushes it into the ring. The output
/// side is deliberately ignored (nothing plays through the private
/// aggregate; see the comment in the body).
unsafe extern "C" fn io_proc(
    _device: ca::AudioObjectID,
    _now: *const c_void,
    in_input: *const ca::AudioBufferList,
    _in_time: *const c_void,
    _out_output: *mut ca::AudioBufferList,
    _out_time: *const c_void,
    user: *mut c_void,
) -> ca::OSStatus {
    // SAFETY: `user` is the IoState pointer registered in SystemTap::new,
    // valid until AudioDeviceDestroyIOProcID returns in Drop — after which no
    // further callbacks can arrive.
    let st = unsafe { &mut *user.cast::<IoState>() };

    // outOutputData is deliberately ignored: the aggregate's output side is
    // inherited from the clocking sub-device and nothing ever plays through
    // this private device. The HAL may hand the client descriptors it has
    // never formatted (touching them crashed with a PAC-faulted pointer on
    // this macOS 26 machine); AudioCap's tap IO block ignores the output
    // side as well.

    if in_input.is_null() {
        return 0;
    }
    let bpf = st.in_asbd.mBytesPerFrame;
    if bpf == 0 {
        return 0;
    }
    // SAFETY: index 0 < buffer count (HAL always delivers ≥1 input buffer).
    let b0 = ca::abl_buffer(in_input.cast(), 0);
    // For interleaved input mBytesPerFrame covers all channels; for
    // non-interleaved it covers one channel and every buffer has the same
    // frame count — the formula holds either way.
    let frames = b0.byte_size / bpf;
    if frames == 0 || frames as usize > MAX_IO_FRAMES {
        return 0;
    }

    st.pending = Some(PendingInput {
        abl: in_input.cast(),
        frames_left: frames,
        offset_frames: 0,
    });

    // Worst-case output frames for this input chunk (+2 for SRC filter tails).
    let ratio = OUTPUT_RATE / st.in_asbd.mSampleRate;
    let want = ((frames as f64 * ratio).ceil() as usize + 2).min(st.scratch.len() / 2);
    let mut produced: ca::UInt32 = want as ca::UInt32;
    let mut abl = ca::AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [ca::AudioBuffer {
            mNumberChannels: OUTPUT_CHANNELS,
            mDataByteSize: (st.scratch.len() * std::mem::size_of::<f32>()) as ca::UInt32,
            mData: st.scratch.as_mut_ptr().cast(),
        }],
    };
    // SAFETY: converter, state and ABL are all valid; conv_input_proc is the
    // matching typed callback. Called on the IO thread only.
    let err = unsafe {
        ca::AudioConverterFillComplexBuffer(
            st.converter,
            conv_input_proc,
            (st as *mut IoState).cast(),
            &mut produced,
            &mut abl,
            std::ptr::null_mut(),
        )
    };
    if err == 0 && produced > 0 {
        let samples = (produced as usize * OUTPUT_CHANNELS as usize).min(st.scratch.len());
        st.ring.push(&st.scratch[..samples]);
    }
    // Conversion errors on the realtime thread are silently skipped; the
    // next IO cycle retries with fresh input.
    0
}

/// Build the 48 kHz interleaved stereo f32 target ASBD.
fn output_asbd() -> ca::AudioStreamBasicDescription {
    ca::AudioStreamBasicDescription {
        mSampleRate: OUTPUT_RATE,
        mFormatID: ca::K_AUDIO_FORMAT_LINEAR_PCM,
        mFormatFlags: ca::K_AUDIO_FORMAT_FLAG_IS_FLOAT | ca::K_AUDIO_FORMAT_FLAG_IS_PACKED,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: OUTPUT_CHANNELS,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Construct the whole-system, unmuted, stereo-mixdown CATapDescription via
/// the Objective-C runtime and return the retained object.
fn make_tap_description() -> Result<objc::ObjcId> {
    let _pool = objc::AutoreleasePool::new();
    let class = objc::get_class(c"CATapDescription")?;
    let alloc = unsafe { objc::msg_send_id0(class, objc::sel(c"alloc")) }
        .context("allocating CATapDescription")?;
    // Toll-free bridging: an empty CFArray *is* an empty NSArray.
    let exclude = CFArray::<CFType>::from_CFTypes(&[]);
    let desc = unsafe {
        objc::msg_send_id1(
            alloc,
            objc::sel(c"initStereoGlobalTapButExcludeProcesses:"),
            exclude.as_concrete_TypeRef().cast::<c_void>().cast_mut(),
        )
    }
    .context("initializing whole-system stereo CATapDescription")?;
    // muteBehavior = CATapUnmuted: captured audio still reaches the hardware.
    unsafe { objc::msg_send_long1_void(desc, objc::sel(c"setMuteBehavior:"), CATAP_UNMUTED) };
    Ok(desc)
}

impl SystemTap {
    /// Tap the whole system's audio: whole-system stereo CATapDescription
    /// (unmuted, mixdown), process tap, private aggregate device exposing the
    /// tap as input, IOProc + converter running.
    ///
    /// Fails with an error mentioning "permission" when macOS TCC plausibly
    /// denied the audio-capture grant.
    pub fn new() -> Result<SystemTap> {
        // Resource handles for rollback.
        let mut tap: ca::AudioObjectID = 0;
        let mut tap_desc: objc::ObjcId = std::ptr::null_mut();
        let mut aggregate: ca::AudioObjectID = 0;
        let mut state: *mut IoState = std::ptr::null_mut();
        let mut io_handle: ca::AudioDeviceIOProcID = std::ptr::null_mut();
        let mut device_started = false;

        let built = (|| -> Result<f64> {
            // -- default output device (clocking sub-device) ----------------
            let default_dev: ca::AudioObjectID = unsafe {
                get_prop(
                    ca::K_AUDIO_OBJECT_SYSTEM_OBJECT,
                    &prop_addr(
                        ca::K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
                        ca::SCOPE_GLOBAL,
                    ),
                    "getting default output device",
                )
            }?;
            if default_dev == 0 {
                bail!("no default output device on this Mac");
            }
            let device_uid: String = unsafe {
                get_cfstring_prop(
                    default_dev,
                    ca::K_AUDIO_DEVICE_PROPERTY_DEVICE_UID,
                    "reading default output device UID",
                )
            }?;

            // -- process tap ------------------------------------------------
            tap_desc = make_tap_description()?;
            let mut tap_id: ca::AudioObjectID = 0;
            // SAFETY: tap_desc is a live CATapDescription object; tap_id out
            // pointer valid.
            let st = unsafe { ca::AudioHardwareCreateProcessTap(tap_desc, &mut tap_id) };
            check(st, "AudioHardwareCreateProcessTap (whole-system tap)", true)?;
            tap = tap_id;
            let tap_uid: String = unsafe {
                get_cfstring_prop(tap, ca::K_AUDIO_TAP_PROPERTY_UID, "reading tap UID")
            }
            .context("reading the created tap's UID")?;

            // -- aggregate device: output device clocks, tap feeds input ----
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let device_uid_cf = CFString::new(&device_uid);
            let sub_device = CFDictionary::from_CFType_pairs(&[(
                CFString::new("uid"), // kAudioSubDeviceUIDKey
                device_uid_cf.as_CFType(),
            )]);
            let sub_tap = CFDictionary::from_CFType_pairs(&[(
                CFString::new("uid"), // kAudioSubTapUIDKey
                CFString::new(&tap_uid).as_CFType(),
            )]);
            let pairs: Vec<(CFString, CFType)> = vec![
                // kAudioAggregateDeviceNameKey
                (CFString::new("name"), CFString::new("ThunderLink System Tap").as_CFType()),
                // kAudioAggregateDeviceUIDKey (unique: destruction is async)
                (
                    CFString::new("uid"),
                    CFString::new(&format!("ThunderLink.SystemTap.{now}")).as_CFType(),
                ),
                // kAudioAggregateDeviceMainSubDeviceKey — the output device clocks the aggregate
                (CFString::new("master"), device_uid_cf.as_CFType()),
                // kAudioAggregateDeviceSubDeviceListKey
                (
                    CFString::new("subdevices"),
                    CFArray::from_CFTypes(&[sub_device.as_CFType()]).as_CFType(),
                ),
                // kAudioAggregateDeviceTapListKey — the tap feeds the input side
                (
                    CFString::new("taps"),
                    CFArray::from_CFTypes(&[sub_tap.as_CFType()]).as_CFType(),
                ),
                // kAudioAggregateDeviceTapAutoStartKey
                (CFString::new("tapautostart"), CFNumber::from(1i32).as_CFType()),
                // kAudioAggregateDeviceIsPrivateKey
                (CFString::new("private"), CFNumber::from(1i32).as_CFType()),
            ];
            let description = CFDictionary::from_CFType_pairs(&pairs);
            let mut agg_id: ca::AudioObjectID = 0;
            // SAFETY: CFDictionaryRef valid; out pointer valid.
            let st = unsafe {
                ca::AudioHardwareCreateAggregateDevice(
                    description.as_concrete_TypeRef().cast::<c_void>(),
                    &mut agg_id,
                )
            };
            check(st, "AudioHardwareCreateAggregateDevice (tap input device)", true)?;
            aggregate = agg_id;

            // -- input stream native format ---------------------------------
            let mut streams = [ca::AudioObjectID::MAX; 8];
            let mut size = std::mem::size_of_val(&streams) as ca::UInt32;
            // SAFETY: buffers valid for the declared size; HAL will not write
            // past the actual property size, which we bounds-check below.
            let st = unsafe {
                ca::AudioObjectGetPropertyData(
                    aggregate,
                    &prop_addr(ca::K_AUDIO_DEVICE_PROPERTY_STREAMS, ca::SCOPE_INPUT),
                    0,
                    std::ptr::null(),
                    &mut size,
                    streams.as_mut_ptr().cast(),
                )
            };
            check(st, "querying aggregate input streams", false)?;
            let n_streams = (size / 4) as usize;
            if n_streams == 0 || n_streams > streams.len() || streams[0] == ca::AudioObjectID::MAX
            {
                bail!("aggregate device exposes no input stream for the tap{PERMISSION_HINT}");
            }
            let stream = streams[0];
            let in_asbd: ca::AudioStreamBasicDescription = unsafe {
                get_prop(
                    stream,
                    &prop_addr(
                        ca::K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT,
                        ca::SCOPE_GLOBAL,
                    ),
                    "reading tap stream format",
                )
            }?;
            if in_asbd.mFormatID != ca::K_AUDIO_FORMAT_LINEAR_PCM {
                bail!(
                    "tap delivers non-PCM audio (format id {})",
                    ca::osstatus_str(in_asbd.mFormatID as i32)
                );
            }
            if in_asbd.mSampleRate <= 0.0 || in_asbd.mBytesPerFrame == 0 {
                bail!("tap stream format has invalid rate/frame size");
            }
            if in_asbd.mChannelsPerFrame != OUTPUT_CHANNELS {
                bail!(
                    "tap stream has {} channels (stereo mixdown expected)",
                    in_asbd.mChannelsPerFrame
                );
            }
            log::info!(
                "process tap native format: {:.0} Hz, {} ch, {} bytes/frame, flags {:#x}",
                in_asbd.mSampleRate,
                in_asbd.mChannelsPerFrame,
                in_asbd.mBytesPerFrame,
                in_asbd.mFormatFlags
            );

            // -- converter: native → 48 kHz interleaved stereo f32 ----------
            let out_asbd = output_asbd();
            let mut converter: ca::AudioConverterRef = std::ptr::null_mut();
            // SAFETY: both ASBDs valid; out pointer valid.
            let st = unsafe { ca::AudioConverterNew(&in_asbd, &out_asbd, &mut converter) };
            check(st, "AudioConverterNew (tap → 48k stereo f32)", false)
                .context("creating the tap format converter")?;
            if converter.is_null() {
                bail!("AudioConverterNew returned NULL");
            }

            // -- IOProc + start ---------------------------------------------
            let io = Box::new(IoState {
                ring: SpscRing::with_capacity_samples(RING_SAMPLES),
                converter,
                in_asbd,
                scratch: vec![0.0; MAX_IO_FRAMES * OUTPUT_CHANNELS as usize],
                pending: None,
            });
            state = Box::into_raw(io);
            let mut io_id: ca::AudioDeviceIOProcID = std::ptr::null_mut();
            // SAFETY: state pointer valid for the box's lifetime; io_proc has
            // the AudioDeviceIOProc signature.
            let st = unsafe {
                ca::AudioDeviceCreateIOProcID(aggregate, io_proc, state.cast(), &mut io_id)
            };
            check(st, "AudioDeviceCreateIOProcID", false)?;
            io_handle = io_id;
            // SAFETY: io_id from the successful create above.
            let st = unsafe { ca::AudioDeviceStart(aggregate, io_handle) };
            check(st, "AudioDeviceStart (aggregate/tap)", true)?;
            device_started = true;

            Ok(in_asbd.mSampleRate)
        })();

        // The description object is no longer needed (the HAL retains what it
        // needs); release our +1 reference from alloc.
        if !tap_desc.is_null() {
            // SAFETY: single release of an owned, live object.
            unsafe { objc::release(tap_desc) };
        }

        match built {
            Ok(native_rate) => {
                log::info!(
                    "system tap running (native {native_rate:.0} Hz → {OUTPUT_RATE} Hz stereo)"
                );
                Ok(SystemTap {
                    state,
                    io_proc: io_handle,
                    device: aggregate,
                    tap,
                    last_dropped: std::cell::Cell::new(0),
                })
            }
            Err(e) => {
                // Roll back everything that was created (bounded: a started
                // but silent aggregate stalls the same way as in Drop).
                teardown_bounded(HalHandles {
                    device: aggregate,
                    tap,
                    io_proc: io_handle,
                    state,
                    started: device_started,
                });
                Err(e)
            }
        }
    }

    /// Sample rate of the PCM returned by [`SystemTap::next_pcm`] — always
    /// 48 kHz (the tap's native format is converted in the IOProc).
    pub fn sample_rate(&self) -> f64 {
        OUTPUT_RATE
    }

    /// Drain up to `want_frames` frames of interleaved stereo f32. Starved
    /// frames are zeros. Never blocks, never fails.
    pub fn next_pcm(&mut self, want_frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; want_frames * OUTPUT_CHANNELS as usize];
        // SAFETY: state is valid until Drop begins; this is the consumer side
        // of the documented SPSC protocol, so concurrent producer access is
        // accounted for.
        let ring = unsafe { &(*self.state).ring };
        ring.pop(&mut out);
        let dropped = ring.dropped();
        if dropped > self.last_dropped.get() {
            log::debug!("capture ring overflowed: {dropped} samples dropped in total");
            self.last_dropped.set(dropped);
        }
        out
    }
}

/// HAL handles to tear down, in creation order.
#[derive(Clone, Copy)]
struct HalHandles {
    device: ca::AudioObjectID,
    tap: ca::AudioObjectID,
    io_proc: ca::AudioDeviceIOProcID,
    state: *mut IoState,
    started: bool,
}

// SAFETY: the handles are raw HAL identifiers plus the IoState pointer,
// which the teardown thread only dereferences after
// AudioDeviceDestroyIOProcID has returned (no further IOProc callbacks).
unsafe impl Send for HalHandles {}

/// How long `Drop` waits for HAL teardown before letting it continue in the
/// background. `AudioDeviceStop` blocks ~30 s and
/// `AudioDeviceDestroyIOProcID` ~60 s more when the device never ran an IO
/// cycle — which happens exactly when the tap only ever saw silence (a
/// quiet system or a TCC denial), as measured on macOS 26.
const TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn teardown_bounded(h: HalHandles) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        // Use the struct as a whole so the closure captures it wholesale
        // (disjoint field capture would grab the raw pointers individually,
        // bypassing the manual Send impl above).
        let h = h;
        // SAFETY: handles mirror exactly what new() created; each call is
        // guarded by its own validity flag, and `state` is freed only after
        // AudioDeviceDestroyIOProcID has returned, so the IOProc can no
        // longer fire.
        unsafe {
            if h.started {
                let _ = ca::AudioDeviceStop(h.device, h.io_proc);
            }
            if !h.io_proc.is_null() {
                let _ = ca::AudioDeviceDestroyIOProcID(h.device, h.io_proc);
            }
            if !h.state.is_null() {
                drop(Box::from_raw(h.state));
            }
            if h.device != 0 {
                let _ = ca::AudioHardwareDestroyAggregateDevice(h.device);
            }
            if h.tap != 0 {
                let _ = ca::AudioHardwareDestroyProcessTap(h.tap);
            }
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(TEARDOWN_TIMEOUT).is_err() {
        // The HAL is stalling (see TEARDOWN_TIMEOUT). The worker keeps
        // running and finishes whenever the HAL unblocks (it stays sound:
        // state is freed only after the IOProc is destroyed); if the process
        // exits first, the HAL objects die with it. Never block the caller.
        log::warn!(
            "Core Audio teardown stalled (silent tap: the HAL waits ~90 s for IO cycles \
             that never ran); continuing in the background"
        );
    }
    drop(worker); // detach on stall; joins implicitly on fast path exit
}

impl Drop for SystemTap {
    fn drop(&mut self) {
        // Order matters: stop callbacks, destroy the IOProc, only then free
        // the state it points at, and finally tear down the device graph —
        // all bounded by TEARDOWN_TIMEOUT (see teardown_bounded).
        teardown_bounded(HalHandles {
            device: self.device,
            tap: self.tap,
            io_proc: self.io_proc,
            state: self.state,
            started: true,
        });
    }
}
