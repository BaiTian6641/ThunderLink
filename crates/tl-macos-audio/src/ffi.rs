//! Raw FFI declarations mirroring the macOS SDK headers shipped on this
//! machine (`xcrun --show-sdk-path` → MacOSX.sdk). Every declaration below
//! carries a comment naming its source header. Only the subset CoreAudio /
//! AudioToolbox / libobjc surface that this crate uses is declared.
//!
//! Struct-layout source of truth for `CATapDescription`: it is **not** a C
//! struct — `CoreAudio.framework/Headers/CATapDescription.h` declares an
//! Objective-C class. It is therefore constructed through the Objective-C
//! runtime in [`crate::objc`], and `AudioHardwareCreateProcessTap` (declared
//! in `AudioHardwareTapping.h`, public since macOS 14.2) takes the object
//! pointer as an opaque `*mut c_void`.
#![allow(non_snake_case)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type UInt32 = u32;
pub type Float64 = f64;

// Opaque CoreAudio/AudioToolbox handles (C typedefs of pointers to opaque
// structs).
pub type AudioObjectID = UInt32;
pub type AudioObjectPropertySelector = UInt32;
pub type AudioObjectPropertyScope = UInt32;
pub type AudioObjectPropertyElement = UInt32;
pub type AudioFormatID = UInt32;
pub type AudioFormatFlags = UInt32;
pub type OSType = UInt32;
pub type AudioUnitPropertyID = UInt32;
pub type AudioUnitScope = UInt32;
pub type AudioUnitElement = UInt32;
pub type AudioUnitRenderActionFlags = UInt32;
pub type AudioConverterRef = *mut c_void;
pub type AudioComponent = *mut c_void; // struct OpaqueAudioComponent *
pub type AudioUnit = *mut c_void; // AudioComponentInstance
pub type AudioDeviceIOProcID = *mut c_void;
pub type CFDictionaryRef = *const c_void; // core-foundation owns real CF types

/// Four-char-code constant (`'xxxx'` in C): the C macro packs the four ASCII
/// bytes big-endian into a 32-bit integer, which is exactly
/// `u32::from_be_bytes`.
pub const fn cc(s: &[u8; 4]) -> UInt32 {
    u32::from_be_bytes(*s)
}

// --- AudioHardware.h ------------------------------------------------------

/// `kAudioObjectSystemObject` (AudioHardware.h:110).
pub const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
/// `kAudioHardwarePropertyDefaultOutputDevice` = 'dOut' (AudioHardware.h:610).
pub const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: AudioObjectPropertySelector =
    cc(b"dOut");
/// `kAudioDevicePropertyDeviceUID` = 'uid ' (AudioHardwareBase.h:734).
pub const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: AudioObjectPropertySelector = cc(b"uid ");
/// `kAudioDevicePropertyStreams` = 'stm#' (AudioHardwareBase.h:744).
pub const K_AUDIO_DEVICE_PROPERTY_STREAMS: AudioObjectPropertySelector = cc(b"stm#");
/// `kAudioTapPropertyUID` = 'tuid' (AudioHardware.h:2025) — CFString, create rule.
pub const K_AUDIO_TAP_PROPERTY_UID: AudioObjectPropertySelector = cc(b"tuid");

/// `kAudioObjectPropertyScopeGlobal` = 'glob' (AudioHardwareBase.h:203).
pub const SCOPE_GLOBAL: AudioObjectPropertyScope = cc(b"glob");
/// `kAudioDevicePropertyScopeInput` = kAudioObjectPropertyScopeInput = 'inpt'
/// (AudioHardwareBase.h:204).
pub const SCOPE_INPUT: AudioObjectPropertyScope = cc(b"inpt");
/// `kAudioObjectPropertyElementMain` = 0 (AudioHardwareBase.h:207).
pub const ELEMENT_MAIN: AudioObjectPropertyElement = 0;

// --- AudioHardwareBase.h:1122 / CoreAudioBaseTypes.h -----------------------

/// `kAudioStreamPropertyVirtualFormat` = 'sfmt'.
pub const K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT: AudioObjectPropertySelector = cc(b"sfmt");

// --- CoreAudioBaseTypes.h --------------------------------------------------

/// `kAudioFormatLinearPCM` = 'lpcm'.
pub const K_AUDIO_FORMAT_LINEAR_PCM: AudioFormatID = cc(b"lpcm");
/// `kAudioFormatFlagIsFloat` = 1 << 0.
pub const K_AUDIO_FORMAT_FLAG_IS_FLOAT: AudioFormatFlags = 1 << 0;
/// `kAudioFormatFlagIsPacked` = 1 << 3.
pub const K_AUDIO_FORMAT_FLAG_IS_PACKED: AudioFormatFlags = 1 << 3;

// --- AudioToolbox AUComponent.h / AudioUnitProperties.h -------------------

/// `kAudioUnitType_Output` = 'auou' (AudioToolbox AUComponent.h:193).
pub const K_AUDIO_UNIT_TYPE_OUTPUT: OSType = cc(b"auou");
/// `kAudioUnitSubType_DefaultOutput` = 'def ' (AudioToolbox AUComponent.h:298).
pub const K_AUDIO_UNIT_SUBTYPE_DEFAULT_OUTPUT: OSType = cc(b"def ");
/// `kAudioUnitProperty_StreamFormat` = 8 (AudioUnitProperties.h:900).
pub const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: AudioUnitPropertyID = 8;
/// `kAudioUnitProperty_SetRenderCallback` = 23 (AudioUnitProperties.h:910).
pub const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: AudioUnitPropertyID = 23;
/// `kAudioUnitScope_Input` = 1. The AudioUnit scope enum lives in
/// AudioToolboxCore (absent from the CLT SDK's shim headers); the value is
/// stable public ABI since macOS 10.0 (Apple AudioUnit headers).
pub const K_AUDIO_UNIT_SCOPE_INPUT: AudioUnitScope = 1;

// --- Structs (CoreAudioBaseTypes.h, AudioComponent.h, AudioUnitProperties.h)

/// `AudioObjectPropertyAddress` (AudioHardwareBase.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: AudioObjectPropertySelector,
    pub mScope: AudioObjectPropertyScope,
    pub mElement: AudioObjectPropertyElement,
}

/// `AudioStreamBasicDescription` (CoreAudioBaseTypes.h:271) — 40 bytes,
/// 8-byte aligned; every field verified against the header.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: Float64,
    pub mFormatID: AudioFormatID,
    pub mFormatFlags: AudioFormatFlags,
    pub mBytesPerPacket: UInt32,
    pub mFramesPerPacket: UInt32,
    pub mBytesPerFrame: UInt32,
    pub mChannelsPerFrame: UInt32,
    pub mBitsPerChannel: UInt32,
    pub mReserved: UInt32,
}

/// `AudioBuffer` (CoreAudioBaseTypes.h:169).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub mNumberChannels: UInt32,
    pub mDataByteSize: UInt32,
    pub mData: *mut c_void,
}

/// `AudioBufferList` (CoreAudioBaseTypes.h:179) — `mBuffers` is a flexible
/// array; the SDK declares it `[1]`. Use this type only for lists WE
/// allocate (properly aligned); lists handed to us by CoreAudio are only
/// guaranteed 4-byte aligned in practice, so all reads/writes of
/// caller-provided lists go through the unaligned-safe helpers below.
#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: UInt32,
    pub mBuffers: [AudioBuffer; 1],
}

const ABL_BUFFERS_OFFSET: usize = 4; // after mNumberBuffers
const AB_SIZE: usize = 16; // sizeof(AudioBuffer) = 4 + 4 + 8

/// A snapshot of one `AudioBuffer` descriptor, read unaligned-safe.
#[derive(Clone, Copy)]
pub struct BufferDesc {
    pub channels: UInt32,
    pub byte_size: UInt32,
    pub data: *mut c_void,
}

/// Number of buffers in a caller-provided `AudioBufferList`.
///
/// # Safety
/// The first 4 bytes at `abl` must be readable.
pub unsafe fn abl_buffer_count(abl: *const c_void) -> usize {
    // SAFETY: u32 read at any alignment; caller guarantees readability.
    unsafe { std::ptr::read_unaligned(abl.cast::<u8>() as *const UInt32) as usize }
}

/// Read buffer descriptor `i` of a caller-provided `AudioBufferList`.
///
/// # Safety
/// `i` must be < the list's buffer count.
pub unsafe fn abl_buffer(abl: *const c_void, i: usize) -> BufferDesc {
    let b = (abl as *const u8).add(ABL_BUFFERS_OFFSET + i * AB_SIZE);
    // SAFETY: unaligned reads of the descriptor fields; caller guarantees
    // the descriptor memory is readable.
    unsafe {
        BufferDesc {
            channels: std::ptr::read_unaligned(b as *const UInt32),
            byte_size: std::ptr::read_unaligned(b.add(4) as *const UInt32),
            data: std::ptr::read_unaligned(b.add(8) as *const *mut c_void),
        }
    }
}

/// Write buffer descriptor `i` of a caller-provided `AudioBufferList`.
///
/// # Safety
/// `i` must be < the list's buffer count and the caller must own write
/// access to the descriptor memory.
pub unsafe fn abl_write_buffer(abl: *mut c_void, i: usize, desc: BufferDesc) {
    let b = (abl as *mut u8).add(ABL_BUFFERS_OFFSET + i * AB_SIZE);
    // SAFETY: unaligned writes of the descriptor fields; caller guarantees
    // writability.
    unsafe {
        std::ptr::write_unaligned(b as *mut UInt32, desc.channels);
        std::ptr::write_unaligned(b.add(4) as *mut UInt32, desc.byte_size);
        std::ptr::write_unaligned(b.add(8) as *mut *mut c_void, desc.data);
    }
}

/// Set the buffer count of a caller-provided `AudioBufferList`.
///
/// # Safety
/// The first 4 bytes at `abl` must be writable.
pub unsafe fn abl_set_buffer_count(abl: *mut c_void, count: UInt32) {
    // SAFETY: unaligned u32 write; caller guarantees writability.
    unsafe { std::ptr::write_unaligned(abl.cast::<u8>() as *mut UInt32, count) }
}

/// `AudioComponentDescription` (AudioComponent.h:267; `#pragma pack(4)` —
/// five u32s, natural 4-alignment, 20 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioComponentDescription {
    pub componentType: OSType,
    pub componentSubType: OSType,
    pub componentManufacturer: OSType,
    pub componentFlags: UInt32,
    pub componentFlagsMask: UInt32,
}

/// `AURenderCallbackStruct` (AudioUnitProperties.h:1083).
#[repr(C)]
pub struct AURenderCallbackStruct {
    pub inputProc: AURenderCallback,
    pub inputProcRefCon: *mut c_void,
}

/// `AudioDeviceIOProc` (AudioHardware.h:786). Timestamp params are opaque to
/// us, hence `c_void`.
pub type AudioDeviceIOProc = unsafe extern "C" fn(
    inDevice: AudioObjectID,
    inNow: *const c_void,
    inInputData: *const AudioBufferList,
    inInputTime: *const c_void,
    outOutputData: *mut AudioBufferList,
    inOutputTime: *const c_void,
    inClientData: *mut c_void,
) -> OSStatus;

/// `AURenderCallback` (AudioUnitProperties.h).
pub type AURenderCallback = unsafe extern "C" fn(
    inRefCon: *mut c_void,
    ioActionFlags: *mut AudioUnitRenderActionFlags,
    inTimeStamp: *const c_void,
    inBusNumber: UInt32,
    inNumberFrames: UInt32,
    ioData: *mut AudioBufferList,
) -> OSStatus;

/// `AudioConverterComplexInputDataProc` (AudioConverter.h:806). The packet
/// description out-param is unused for LinearPCM → `*mut c_void`.
pub type AudioConverterComplexInputDataProc = unsafe extern "C" fn(
    inAudioConverter: AudioConverterRef,
    ioNumberDataPackets: *mut UInt32,
    ioData: *mut AudioBufferList,
    outDataPacketDescription: *mut c_void,
    inUserData: *mut c_void,
) -> OSStatus;

/// Format an `OSStatus` the way CoreAudio diagnostics do: positive values that
/// are four printable ASCII chars show as `'cccc'`, everything else decimal.
pub fn osstatus_str(status: OSStatus) -> String {
    let b = u32::try_from(status).map(|u| u.to_be_bytes()).unwrap_or([0; 4]);
    if b.iter().all(|c| (0x20..=0x7e).contains(c)) {
        if let Ok(s) = std::str::from_utf8(&b) {
            return format!("'{}' ({})", s, status);
        }
    }
    format!("{}", status)
}

// --- extern declarations ----------------------------------------------------

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    // AudioHardware.h:291
    pub fn AudioObjectGetPropertyData(
        inObjectID: AudioObjectID,
        inAddress: *const AudioObjectPropertyAddress,
        inQualifierDataSize: UInt32,
        inQualifierData: *const c_void,
        ioDataSize: *mut UInt32,
        outData: *mut c_void,
    ) -> OSStatus;
    // AudioHardware.h:667
    pub fn AudioHardwareCreateAggregateDevice(
        inDescription: CFDictionaryRef,
        outDeviceID: *mut AudioObjectID,
    ) -> OSStatus;
    // AudioHardware.h:680 — destruction is asynchronous; UIDs must be unique.
    pub fn AudioHardwareDestroyAggregateDevice(inDeviceID: AudioObjectID) -> OSStatus;
    // AudioHardwareTapping.h:43 — takes the CATapDescription ObjC object.
    pub fn AudioHardwareCreateProcessTap(
        inDescription: *mut c_void,
        outTapID: *mut AudioObjectID,
    ) -> OSStatus;
    // AudioHardwareTapping.h:54
    pub fn AudioHardwareDestroyProcessTap(inTapID: AudioObjectID) -> OSStatus;
    // AudioHardware.h:1377 (public since 10.5, non-deprecated function-pointer
    // IOProc registration).
    pub fn AudioDeviceCreateIOProcID(
        inDevice: AudioObjectID,
        inProc: AudioDeviceIOProc,
        inClientData: *mut c_void,
        outIOProcID: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    // AudioHardware.h:1418
    pub fn AudioDeviceDestroyIOProcID(
        inDevice: AudioObjectID,
        inIOProcID: AudioDeviceIOProcID,
    ) -> OSStatus;
    // AudioHardware.h:1435
    pub fn AudioDeviceStart(
        inDevice: AudioObjectID,
        inProcID: AudioDeviceIOProcID,
    ) -> OSStatus;
    // AudioHardware.h:1473
    pub fn AudioDeviceStop(
        inDevice: AudioObjectID,
        inProcID: AudioDeviceIOProcID,
    ) -> OSStatus;
}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    // AudioConverter.h:562
    pub fn AudioConverterNew(
        inSourceFormat: *const AudioStreamBasicDescription,
        inDestinationFormat: *const AudioStreamBasicDescription,
        outAudioConverter: *mut AudioConverterRef,
    ) -> OSStatus;
    // AudioConverter.h:629
    pub fn AudioConverterDispose(inAudioConverter: AudioConverterRef) -> OSStatus;
    // AudioConverter.h:865 — pull-model conversion; the only public complex
    // converter entry point that supports sample-rate conversion (the header
    // explicitly rejects AudioConverterConvertComplexBuffer for SRC).
    pub fn AudioConverterFillComplexBuffer(
        inAudioConverter: AudioConverterRef,
        inInputDataProc: AudioConverterComplexInputDataProc,
        inInputDataProcUserData: *mut c_void,
        ioOutputDataPacketSize: *mut UInt32,
        outOutputData: *mut AudioBufferList,
        outPacketDescription: *mut c_void,
    ) -> OSStatus;
    // AudioComponent.h:390
    pub fn AudioComponentFindNext(
        inComponent: AudioComponent,
        inDesc: *const AudioComponentDescription,
    ) -> AudioComponent;
    // AudioComponent.h:494
    pub fn AudioComponentInstanceNew(
        inComponent: AudioComponent,
        outInstance: *mut AudioUnit,
    ) -> OSStatus;
    // AudioComponent.h:529
    pub fn AudioComponentInstanceDispose(inInstance: AudioUnit) -> OSStatus;
    // AUComponent.h:1310
    pub fn AudioUnitSetProperty(
        inUnit: AudioUnit,
        inID: AudioUnitPropertyID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        inData: *const c_void,
        inDataSize: UInt32,
    ) -> OSStatus;
    // AUComponent.h:1193
    pub fn AudioUnitInitialize(inUnit: AudioUnit) -> OSStatus;
    // AUComponent.h:1213
    pub fn AudioUnitUninitialize(inUnit: AudioUnit) -> OSStatus;
    // AudioOutputUnit.h:24/44 — the header's AudioUnitStart/Stop are macros
    // for these real symbols (both exported by AudioToolbox).
    pub fn AudioOutputUnitStart(inUnit: AudioUnit) -> OSStatus;
    pub fn AudioOutputUnitStop(inUnit: AudioUnit) -> OSStatus;
}
