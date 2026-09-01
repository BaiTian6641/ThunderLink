//! Direct extern "C" bindings for VideoToolbox / CoreMedia / CoreVideo /
//! CoreGraphics where crates lack coverage (SPEC §9 guidance).
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use std::ffi::c_void;

use core_foundation::base::CFAllocatorRef;
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::string::CFStringRef;

pub type OSStatus = i32;
pub type CVReturn = i32;
pub type Boolean = u8;
pub type FourCharCode = u32;
pub type CMItemCount = i64;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const kCMVideoCodecType_H264: FourCharCode = u32::from_be_bytes(*b"avc1");
pub const kCMVideoCodecType_HEVC: FourCharCode = u32::from_be_bytes(*b"hvc1");

pub const kCVPixelFormatType_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");
pub const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");
pub const kCVPixelFormatType_420YpCbCr8BiPlanarFullRange: u32 = u32::from_be_bytes(*b"420f");
pub const kCVPixelFormatType_OneComponent10: u32 = u32::from_be_bytes(*b"P010");

pub const kCMTimeFlags_Valid: u32 = 1 << 0;
pub const kCMBlockBufferAssureMemoryNowFlag: u32 = 1 << 0;

pub const kVTDecodeFrame_EnableAsynchronousDecompression: u32 = 1 << 0;
pub const kVTDecodeInfo_FrameDropped: u32 = 1 << 1;

pub const kCVReturnSuccess: CVReturn = 0;
pub const noErr: OSStatus = 0;

// ---------------------------------------------------------------------------
// CoreMedia types
// ---------------------------------------------------------------------------

pub type CMFormatDescriptionRef = *mut c_void;
pub type CMVideoFormatDescriptionRef = CMFormatDescriptionRef;
pub type CMBlockBufferRef = *mut c_void;
pub type CMSampleBufferRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl CMTime {
    pub const INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

    pub fn pts_us(pts_us: i64) -> Self {
        CMTime { value: pts_us, timescale: 1_000_000, flags: kCMTimeFlags_Valid, epoch: 0 }
    }

    pub fn is_valid(&self) -> bool {
        self.flags & kCMTimeFlags_Valid != 0 && self.timescale != 0
    }

    /// Convert to microseconds (i128 intermediate; µs timescale is the fast path).
    pub fn to_us(self) -> i64 {
        if self.timescale == 1_000_000 {
            self.value
        } else {
            (self.value as i128 * 1_000_000 / self.timescale as i128) as i64
        }
    }
}

#[repr(C)]
pub struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentationTimeStamp: CMTime,
    pub decodeTimeStamp: CMTime,
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        paramSetCount: usize,
        paramSetPointers: *const *const u8,
        paramSetSizes: *const usize,
        NALUnitHeaderLength: i32,
        extensions: CFDictionaryRef,
        formatDescriptionOut: *mut CMFormatDescriptionRef,
    ) -> OSStatus;

    pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        paramSetCount: usize,
        paramSetPointers: *const *const u8,
        paramSetSizes: *const usize,
        NALUnitHeaderLength: i32,
        formatDescriptionOut: *mut CMFormatDescriptionRef,
    ) -> OSStatus;

    pub fn CMBlockBufferCreateWithMemoryBlock(
        structureAllocator: CFAllocatorRef,
        memoryBlock: *mut c_void,
        blockLength: usize,
        blockAllocator: CFAllocatorRef,
        customBlockSource: *const c_void,
        offsetToData: usize,
        dataLength: usize,
        flags: u32,
        blockBufferOut: *mut CMBlockBufferRef,
    ) -> OSStatus;

    pub fn CMBlockBufferReplaceDataBytes(
        sourceBytes: *const c_void,
        destinationBuffer: CMBlockBufferRef,
        offsetIntoDestination: usize,
        dataLength: usize,
    ) -> OSStatus;

    pub fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        dataBuffer: CMBlockBufferRef,
        formatDescription: CMFormatDescriptionRef,
        numSamples: CMItemCount,
        numSampleTimingEntries: CMItemCount,
        sampleTimingArray: *const CMSampleTimingInfo,
        numSampleSizeEntries: CMItemCount,
        sampleSizeArray: *const usize,
        sampleBufferOut: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

// ---------------------------------------------------------------------------
// VideoToolbox
// ---------------------------------------------------------------------------

pub type VTDecompressionSessionRef = *mut c_void;
pub type VTDecodeInfoFlags = u32;

pub type VTDecompressionOutputCallback = extern "C" fn(
    decompressionOutputRefCon: *mut c_void,
    sourceFrameRefCon: *mut c_void,
    status: OSStatus,
    infoFlags: VTDecodeInfoFlags,
    imageBuffer: CVImageBufferRef,
    presentationTimeStamp: CMTime,
    presentationDuration: CMTime,
);

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub decompressionOutputCallback: VTDecompressionOutputCallback,
    pub decompressionOutputRefCon: *mut c_void,
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    pub fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        videoFormatDescription: CMVideoFormatDescriptionRef,
        videoDecoderSpecification: CFDictionaryRef,
        destinationImageBufferAttributes: CFDictionaryRef,
        outputCallback: *const VTDecompressionOutputCallbackRecord,
        decompressionSessionOut: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sampleBuffer: CMSampleBufferRef,
        decodeFrameFlags: u32,
        sourceFrameRefCon: *mut c_void,
        infoFlagsOut: *mut VTDecodeInfoFlags,
    ) -> OSStatus;

    pub fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);

    pub fn VTIsHardwareDecodeSupported(codecType: FourCharCode) -> Boolean;
}

// ---------------------------------------------------------------------------
// CoreVideo
// ---------------------------------------------------------------------------

pub type CVPixelBufferRef = *mut c_void;
pub type CVImageBufferRef = CVPixelBufferRef;
pub type CVMetalTextureRef = *mut c_void;
pub type CVMetalTextureCacheRef = *mut c_void;
pub type CVDisplayLinkRef = *mut c_void;

pub type CVDisplayLinkOutputCallback = extern "C" fn(
    displayLink: CVDisplayLinkRef,
    inNow: *const c_void,
    inOutputTime: *const c_void,
    flagsIn: u64,
    flagsOut: *mut u64,
    displayLinkContext: *mut c_void,
) -> i32;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;

    pub fn CVPixelBufferGetWidth(pixelBuffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pixelBuffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pixelBuffer: CVPixelBufferRef) -> u32;

    pub fn CVMetalTextureCacheCreate(
        allocator: CFAllocatorRef,
        cacheAttributes: CFDictionaryRef,
        metalDevice: *mut c_void,
        textureAttributes: CFDictionaryRef,
        cacheOut: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;

    pub fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: CFAllocatorRef,
        textureCache: CVMetalTextureCacheRef,
        sourceImage: CVImageBufferRef,
        textureAttributes: CFDictionaryRef,
        pixelFormat: u64,
        width: usize,
        height: usize,
        planeIndex: usize,
        textureOut: *mut CVMetalTextureRef,
    ) -> CVReturn;

    pub fn CVMetalTextureCacheFlush(textureCache: CVMetalTextureCacheRef, options: u64);

    /// Returns the id<MTLTexture> backing a CVMetalTexture (get-rule: the
    /// texture stays alive as long as the CVMetalTexture is retained).
    pub fn CVMetalTextureGetTexture(texture: CVMetalTextureRef) -> *mut c_void;

    pub fn CVDisplayLinkCreateWithCGDisplay(
        displayID: u32,
        displayLinkOut: *mut CVDisplayLinkRef,
    ) -> CVReturn;
    pub fn CVDisplayLinkSetOutputCallback(
        displayLink: CVDisplayLinkRef,
        callback: CVDisplayLinkOutputCallback,
        userInfo: *mut c_void,
    ) -> CVReturn;
    pub fn CVDisplayLinkStart(displayLink: CVDisplayLinkRef) -> CVReturn;
    pub fn CVDisplayLinkStop(displayLink: CVDisplayLinkRef) -> CVReturn;
}

// ---------------------------------------------------------------------------
// CoreGraphics
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGMainDisplayID() -> u32;
}
