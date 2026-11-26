//! macOS H.264 encoding through VideoToolbox, set up for the lowest latency.
//!
//! One frame in, one Annex-B access unit out (VideoToolbox's low-latency rate controller,
//! no frame reordering), keyframes only when asked for, and a bitrate that changes while
//! encoding (no new session, no keyframe). Input is either the host's I420 picture
//! (copied into a GPU-shared NV12 buffer, changed rows only) or a capturer's own
//! IOSurface-backed pixel buffer (ScreenCaptureKit), handed over without a copy.
//!
//! The encoder tried first is Apple's hardware encoder with low-latency rate control; then
//! low-latency rate control on whatever encoder VideoToolbox picks; then a plain session.
//! The caller ([`crate::video::VideoEncoder`]) falls back to openh264 if none can be made.
//!
//! Colours: the picture is tagged and converted as BT.601 limited range, the same as the
//! openh264 path, which is what both viewer decoders assume.

#![allow(unsafe_code, non_upper_case_globals)]

use crate::capture::Surface;
use crate::video::EncoderSettings;
use crate::yuv::I420;
use openh264::formats::YUVSource;
use rconn_core::codec::VideoCodec;
use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::time::Instant;

type OSStatus = i32;
type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const kCMTimeInvalid: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    sample: CFTypeRef,
);

/// Only its address is used.
#[repr(C)]
struct Opaque {
    _p: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    static kCFTypeArrayCallBacks: Opaque;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        key_cb: *const Opaque,
        value_cb: *const Opaque,
    ) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFArrayCreate(
        alloc: CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        cb: *const Opaque,
    ) -> CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFTypeRef;
    static kCVPixelBufferWidthKey: CFTypeRef;
    static kCVPixelBufferHeightKey: CFTypeRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    static kCVImageBufferYCbCrMatrixKey: CFTypeRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFTypeRef;
    fn CVPixelBufferCreate(
        alloc: CFTypeRef,
        width: usize,
        height: usize,
        format: u32,
        attrs: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CFTypeRef, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CFTypeRef, plane: usize) -> usize;
    fn CVBufferSetAttachment(buffer: CFTypeRef, key: CFTypeRef, value: CFTypeRef, mode: u32);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFTypeRef;
    fn CMSampleBufferGetDataBuffer(sample: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetFormatDescription(sample: CFTypeRef) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: CFTypeRef, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(block: CFTypeRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        block: CFTypeRef,
        offset: usize,
        len: usize,
        dest: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format: CFTypeRef,
        index: usize,
        set: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        format: CFTypeRef,
        index: usize,
        set: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFTypeRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFTypeRef;
    static kVTCompressionPropertyKey_RealTime: CFTypeRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFTypeRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFTypeRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFTypeRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFTypeRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFTypeRef;
    static kVTCompressionPropertyKey_MaximizePowerEfficiency: CFTypeRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFTypeRef;
    static kVTCompressionPropertyKey_MaxH264SliceBytes: CFTypeRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFTypeRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFTypeRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFTypeRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFTypeRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFTypeRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFTypeRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        data_alloc: CFTypeRef,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: CFTypeRef,
        image: CFTypeRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFTypeRef,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: CFTypeRef);
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
    fn VTSessionCopyProperty(
        session: CFTypeRef,
        key: CFTypeRef,
        alloc: CFTypeRef,
        out: *mut c_void,
    ) -> OSStatus;
    static kVTVideoEncoderList_CodecType: CFTypeRef;
    static kVTVideoEncoderList_IsHardwareAccelerated: CFTypeRef;
    fn VTCopyVideoEncoderList(options: CFTypeRef, out: *mut CFTypeRef) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> u8;
}

/// Is there a VideoToolbox encoder for `codec` (a hardware one, if `hardware`)? Read from
/// the encoder list, without starting one.
pub fn encoder_exists(codec: rconn_core::codec::VideoCodec, hardware: bool) -> bool {
    let want = crate::vt::codec_type(codec);
    // SAFETY: the list is ours (released by `Cf`); its dictionaries and values are only
    // read while it lives.
    unsafe {
        let mut list: CFTypeRef = ptr::null();
        if VTCopyVideoEncoderList(ptr::null(), &mut list) != 0 || list.is_null() {
            return false;
        }
        let list = Cf(list);
        (0..CFArrayGetCount(list.0)).any(|i| {
            let e = CFArrayGetValueAtIndex(list.0, i);
            let kind = CFDictionaryGetValue(e, kVTVideoEncoderList_CodecType);
            let mut v = 0i32;
            if kind.is_null()
                || CFNumberGetValue(kind, kCFNumberSInt32Type, (&raw mut v).cast()) == 0
                || v as u32 != want
            {
                return false;
            }
            !hardware
                || CFDictionaryGetValue(e, kVTVideoEncoderList_IsHardwareAccelerated)
                    == kCFBooleanTrue
        })
    }
}

const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");
const kCFNumberSInt32Type: isize = 3;
const kCFNumberFloat64Type: isize = 6;
const kVTEncodeInfo_FrameDropped: u32 = 1 << 1;
const kCVAttachmentMode_ShouldPropagate: u32 = 1;

/// Keyframes come from requests (a new viewer, a decoder that lost its place), not a timer.
const NO_KEYFRAME_TIMER: i32 = i32::MAX;
/// The hard cap over one second, as a multiple of the average bitrate. Lets a keyframe or
/// a busy moment go over the average a little without the network queue building up.
const BURST: f64 = 1.5;

/// Owns one Core Foundation reference.
struct Cf(CFTypeRef);

impl Cf {
    fn number_i32(v: i32) -> Cf {
        // SAFETY: CFNumberCreate copies the value.
        Cf(unsafe { CFNumberCreate(ptr::null(), kCFNumberSInt32Type, (&raw const v).cast()) })
    }

    fn number_f64(v: f64) -> Cf {
        // SAFETY: CFNumberCreate copies the value.
        Cf(unsafe { CFNumberCreate(ptr::null(), kCFNumberFloat64Type, (&raw const v).cast()) })
    }

    fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
        let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
        // SAFETY: valid CF objects; the dictionary retains them.
        Cf(unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we hold one reference, released exactly once.
            unsafe { CFRelease(self.0) }
        }
    }
}

/// What the output callback hands back for the frame being encoded.
#[derive(Default)]
struct Output {
    /// Set by every callback.
    done: bool,
    status: OSStatus,
    /// Annex-B, with the parameter sets (H.264 SPS/PPS, HEVC VPS/SPS/PPS) in front of
    /// keyframes; empty if the frame was dropped.
    annex_b: Vec<u8>,
    key: bool,
    /// The session encodes HEVC (kept across frames).
    hevc: bool,
}

/// Encoded frame, written by the output callback (on a VideoToolbox thread, while
/// `encode` waits for it in `VTCompressionSessionCompleteFrames`).
extern "C" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    sample: CFTypeRef,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the encoder's boxed `Mutex<Output>`, alive until the session is
    // invalidated (in `Drop`, after which no callback runs).
    let slot = unsafe { &*refcon.cast::<Mutex<Output>>() };
    let mut out = slot.lock().unwrap_or_else(|e| e.into_inner());
    out.done = true;
    out.status = status;
    out.annex_b.clear();
    out.key = false;
    if status != 0 || sample.is_null() || info & kVTEncodeInfo_FrameDropped != 0 {
        return;
    }
    // SAFETY: a valid encoded sample buffer, read only during this callback.
    unsafe {
        out.key = is_sync(sample);
        if out.key {
            let hevc = out.hevc;
            append_parameter_sets(sample, hevc, &mut out.annex_b);
        }
        append_annex_b(sample, &mut out.annex_b);
    }
}

/// A keyframe has no `NotSync` attachment (or one set to false).
unsafe fn is_sync(sample: CFTypeRef) -> bool {
    // SAFETY: attachments belong to `sample`; we only read them.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
            return true;
        }
        let dict = CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync);
        not_sync.is_null() || not_sync == kCFBooleanFalse
    }
}

/// The parameter sets from the sample's format description, each behind a start code.
unsafe fn append_parameter_sets(sample: CFTypeRef, hevc: bool, out: &mut Vec<u8>) {
    let get = if hevc {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
    } else {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex
    };
    // SAFETY: the format description belongs to `sample`; the parameter set pointers it
    // returns stay valid while it does.
    unsafe {
        let format = CMSampleBufferGetFormatDescription(sample);
        if format.is_null() {
            return;
        }
        let (mut set, mut size, mut count, mut header) = (ptr::null(), 0usize, 0usize, 0i32);
        if get(format, 0, &mut set, &mut size, &mut count, &mut header) != 0 {
            return;
        }
        for i in 0..count {
            if get(
                format,
                i,
                &mut set,
                &mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            ) == 0
                && !set.is_null()
            {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(std::slice::from_raw_parts(set, size));
            }
        }
    }
}

/// The sample's NAL units (AVCC: 4-byte big-endian lengths) with start codes instead.
unsafe fn append_annex_b(sample: CFTypeRef, out: &mut Vec<u8>) {
    // SAFETY: the block buffer belongs to `sample`; `CMBlockBufferCopyDataBytes` writes
    // exactly `len` bytes into the space reserved for them.
    let avcc = unsafe {
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return;
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast()) != 0 {
            return;
        }
        avcc
    };
    avcc_to_annex_b(&avcc, out);
}

/// 4-byte length prefixes -> 4-byte start codes. A truncated unit ends the conversion.
fn avcc_to_annex_b(avcc: &[u8], out: &mut Vec<u8>) {
    let mut rest = avcc;
    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
        let n = u32::from_be_bytes(*len) as usize;
        if n == 0 || n > tail.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&tail[..n]);
        rest = &tail[n..];
    }
}

/// One way of asking VideoToolbox for an encoder, best first.
#[derive(Clone, Copy, Debug)]
enum Tier {
    HardwareLowLatency,
    LowLatency,
    Plain,
}

pub struct VtEncoder {
    session: Cf,
    /// The output callback's slot; boxed so its address stays put.
    output: Box<Mutex<Output>>,
    width: usize,
    height: usize,
    /// The NV12 copy of the host's I420 picture (`encode_i420`), created on first use.
    nv12: Option<Cf>,
    nv12_filled: bool,
    force_key: Cf,
    started: Instant,
    last_pts: i64,
    bitrate_bps: u32,
    name: String,
    hardware: bool,
    codec: VideoCodec,
}

// SAFETY: the session and buffers are used only through `&mut self`; VideoToolbox
// sessions may be used from any thread, and the output slot is behind a mutex.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// A `codec` session (H.264 or HEVC) for `width` x `height` (even) pictures.
    pub fn new(
        codec: VideoCodec,
        width: usize,
        height: usize,
        settings: &EncoderSettings,
    ) -> Result<Self, String> {
        if codec == VideoCodec::Av1 {
            return Err("VideoToolbox has no AV1 encoder".into());
        }
        let mut errors = Vec::new();
        for tier in [Tier::HardwareLowLatency, Tier::LowLatency, Tier::Plain] {
            match Self::with_tier(codec, width, height, settings, tier) {
                Ok(e) => return Ok(e),
                Err(e) => errors.push(format!("{tier:?}: {e}")),
            }
        }
        Err(errors.join("; "))
    }

    fn with_tier(
        codec: VideoCodec,
        width: usize,
        height: usize,
        settings: &EncoderSettings,
        tier: Tier,
    ) -> Result<Self, String> {
        // SAFETY: plain CoreFoundation/VideoToolbox calls with valid pointers; every
        // created object is owned by a `Cf`.
        unsafe {
            let t = kCFBooleanTrue;
            let spec = match tier {
                Tier::HardwareLowLatency => Cf::dict(&[
                    (kVTVideoEncoderSpecification_EnableLowLatencyRateControl, t),
                    (
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                        t,
                    ),
                ]),
                Tier::LowLatency => {
                    Cf::dict(&[(kVTVideoEncoderSpecification_EnableLowLatencyRateControl, t)])
                }
                Tier::Plain => Cf(ptr::null()),
            };
            let format = Cf::number_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
            let (w, h) = (Cf::number_i32(width as i32), Cf::number_i32(height as i32));
            let iosurface = Cf::dict(&[]);
            let source = Cf::dict(&[
                (kCVPixelBufferPixelFormatTypeKey, format.0),
                (kCVPixelBufferWidthKey, w.0),
                (kCVPixelBufferHeightKey, h.0),
                (kCVPixelBufferIOSurfacePropertiesKey, iosurface.0),
            ]);
            let output = Box::new(Mutex::new(Output {
                hevc: codec == VideoCodec::H265,
                ..Output::default()
            }));
            let mut session: CFTypeRef = ptr::null();
            let st = VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                crate::vt::codec_type(codec),
                spec.0,
                source.0,
                ptr::null(),
                on_output,
                (&raw const *output).cast_mut().cast(),
                &mut session,
            );
            if st != 0 || session.is_null() {
                return Err(format!("VTCompressionSessionCreate: {st}"));
            }
            let mut enc = Self {
                session: Cf(session),
                output,
                width,
                height,
                nv12: None,
                nv12_filled: false,
                force_key: Cf::dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, t)]),
                started: Instant::now(),
                last_pts: -1,
                bitrate_bps: 0,
                name: String::new(),
                hardware: false,
                codec,
            };
            enc.configure(settings)?;
            // The low-latency encoder does not answer "using hardware?"; the first tier
            // requires it.
            let hardware = match tier {
                Tier::HardwareLowLatency => Some(true),
                _ => enc.copy_bool(kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder),
            };
            enc.hardware = hardware == Some(true);
            enc.name = format!(
                "VideoToolbox{} ({})",
                if codec == VideoCodec::H264 {
                    ""
                } else {
                    " HEVC"
                },
                [
                    hardware.map(|h| if h { "hardware" } else { "software" }),
                    (!matches!(tier, Tier::Plain)).then_some("low latency"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
            );
            let st = VTCompressionSessionPrepareToEncodeFrames(enc.session.0);
            if st != 0 {
                return Err(format!("VTCompressionSessionPrepareToEncodeFrames: {st}"));
            }
            Ok(enc)
        }
    }

    fn configure(&mut self, s: &EncoderSettings) -> Result<(), String> {
        let (t, f) = unsafe { (kCFBooleanTrue, kCFBooleanFalse) };
        // SAFETY: property keys and values are valid CF objects.
        unsafe {
            self.require(kVTCompressionPropertyKey_RealTime, t, "RealTime")?;
            self.require(
                kVTCompressionPropertyKey_AllowFrameReordering,
                f,
                "AllowFrameReordering",
            )?;
            // HEVC: Main (8-bit 4:2:0), what every HEVC decoder has.
            if self.codec == VideoCodec::H265 {
                self.require(
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_HEVC_Main_AutoLevel,
                    "ProfileLevel",
                )?;
            // Constrained Baseline is what the session description offers
            // (profile-level-id 42e01f) and what openh264 sends; every viewer decodes it.
            } else if self.set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
            ) != 0
            {
                self.require(
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Baseline_AutoLevel,
                    "ProfileLevel",
                )?;
            }
            let interval = Cf::number_i32(NO_KEYFRAME_TIMER);
            self.require(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                interval.0,
                "MaxKeyFrameInterval",
            )?;
            let fps = Cf::number_f64(f64::from(s.max_fps.max(1.0)));
            self.set(kVTCompressionPropertyKey_ExpectedFrameRate, fps.0);
            // Optional: not every encoder (or tier) has them.
            self.set(kVTCompressionPropertyKey_MaximizePowerEfficiency, f);
            self.set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                t,
            );
            // No MaxAllowedFrameQP cap (openh264's path caps at 32): measured here, the
            // low-latency rate controller then drops most frames of a scrolling Retina
            // screen (11 of 30 out at 2992x1870, 12 Mbit/s) instead of coarsening them.
            self.set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            );
            if let Some(n) = s.slice_bytes.filter(|_| self.codec == VideoCodec::H264) {
                let n = Cf::number_i32(n.min(i32::MAX as u32) as i32);
                self.set(kVTCompressionPropertyKey_MaxH264SliceBytes, n.0);
            }
        }
        self.set_bitrate(s.bitrate_bps);
        Ok(())
    }

    /// Set a session property; the status (0 = applied).
    fn set(&self, key: CFTypeRef, value: CFTypeRef) -> OSStatus {
        // SAFETY: valid session, key and value.
        unsafe { VTSessionSetProperty(self.session.0, key, value) }
    }

    fn require(&self, key: CFTypeRef, value: CFTypeRef, what: &str) -> Result<(), String> {
        match self.set(key, value) {
            0 => Ok(()),
            st => Err(format!("{what}: {st}")),
        }
    }

    /// A boolean session property; `None` if the encoder does not have it.
    fn copy_bool(&self, key: CFTypeRef) -> Option<bool> {
        let mut value: CFTypeRef = ptr::null();
        // SAFETY: valid session and key; the copied value is released by `Cf`.
        let st = unsafe {
            VTSessionCopyProperty(self.session.0, key, ptr::null(), (&raw mut value).cast())
        };
        let value = Cf(value);
        // SAFETY: reading a static CF constant.
        (st == 0).then(|| value.0 == unsafe { kCFBooleanTrue })
    }

    /// "VideoToolbox (hardware, low latency)" and the like.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// The next `encode_i420` copies the whole picture, whatever rows it is told changed.
    pub fn forget_picture(&mut self) {
        self.nv12_filled = false;
    }

    /// Change the target bitrate while encoding: no new session, no keyframe.
    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.bitrate_bps || bps == 0 {
            return;
        }
        self.bitrate_bps = bps;
        let avg = Cf::number_i32(bps.min(i32::MAX as u32) as i32);
        let bytes = Cf::number_f64(f64::from(bps) / 8.0 * BURST);
        let secs = Cf::number_f64(1.0);
        let values = [bytes.0, secs.0];
        // SAFETY: valid CF numbers; the array retains them.
        let limits =
            Cf(unsafe { CFArrayCreate(ptr::null(), values.as_ptr(), 2, &kCFTypeArrayCallBacks) });
        // SAFETY: static property keys.
        let (avg_key, limit_key) = unsafe {
            (
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_DataRateLimits,
            )
        };
        let st = self.set(avg_key, avg.0);
        if st != 0 {
            tracing::warn!(status = st, "VideoToolbox refused the bitrate");
        }
        let st = self.set(limit_key, limits.0);
        if st != 0 {
            tracing::debug!(status = st, "VideoToolbox refused a data rate limit");
        }
    }

    /// Encode the host's picture. `changed_rows` (even) are the only rows that differ from
    /// the previous picture given here; `None` means "any". Returns the access unit and
    /// whether it is a keyframe, or `None` if the encoder dropped the frame.
    pub fn encode_i420(
        &mut self,
        yuv: &I420,
        changed_rows: Option<(usize, usize)>,
        key: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, String> {
        if yuv.dimensions() != (self.width, self.height) {
            return Err("picture size differs from the session's".into());
        }
        if self.nv12.is_none() {
            self.nv12 = Some(new_nv12_buffer(self.width, self.height)?);
            self.nv12_filled = false;
        }
        let rows = match changed_rows {
            Some(r) if self.nv12_filled => r,
            _ => (0, self.height),
        };
        let pb = self.nv12.as_ref().expect("created above").0;
        // SAFETY: a buffer of our own, locked while written, NV12 at the session size.
        unsafe { write_nv12(pb, yuv, rows)? };
        self.nv12_filled = true;
        self.encode_buffer(pb, key)
    }

    /// Encode a captured surface as it is (ScreenCaptureKit's IOSurface: no copy). NV12 or
    /// BGRA; VideoToolbox converts BGRA on the GPU, and scales a different size.
    pub fn encode_surface(
        &mut self,
        surface: &Surface,
        key: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, String> {
        // The surface keeps its pixel buffer alive for the call.
        self.encode_buffer(surface.as_ptr(), key)
    }

    fn encode_buffer(
        &mut self,
        pb: CFTypeRef,
        key: bool,
    ) -> Result<Option<(Vec<u8>, bool)>, String> {
        // Real time stamps: the rate controller spends bits per second, and the frame rate
        // varies (an idle screen sends almost nothing).
        let pts = (self.started.elapsed().as_micros() as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        let time = CMTime {
            value: pts,
            timescale: 1_000_000,
            flags: 1, // valid
            epoch: 0,
        };
        let hevc = self.codec == VideoCodec::H265;
        *self.output.lock().unwrap_or_else(|e| e.into_inner()) = Output {
            hevc,
            ..Output::default()
        };
        let props = if key { self.force_key.0 } else { ptr::null() };
        // SAFETY: valid session and pixel buffer (retained by VideoToolbox while in use);
        // we wait for the frame before returning.
        let (st, done, info) = unsafe {
            let mut info = 0u32;
            let st = VTCompressionSessionEncodeFrame(
                self.session.0,
                pb,
                time,
                kCMTimeInvalid,
                props,
                ptr::null_mut(),
                &mut info,
            );
            let done = VTCompressionSessionCompleteFrames(self.session.0, time);
            (st, done, info)
        };
        if st != 0 {
            // -12903 (kVTInvalidSessionErr): the session was torn down under us (sleep,
            // GPU change). The caller makes a new one.
            return Err(format!("VTCompressionSessionEncodeFrame: {st}"));
        }
        if done != 0 {
            return Err(format!("VTCompressionSessionCompleteFrames: {done}"));
        }
        let mut out = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if !out.done {
            if info & kVTEncodeInfo_FrameDropped != 0 {
                return Ok(None); // dropped before it reached the encoder
            }
            return Err("no output for the frame".into());
        }
        if out.status != 0 {
            return Err(format!("encode failed: {}", out.status));
        }
        if out.annex_b.is_empty() {
            return Ok(None);
        }
        Ok(Some((std::mem::take(&mut out.annex_b), out.key)))
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: valid session; after Invalidate no callback runs, so the output slot
        // may go. The session itself is released by `Cf`.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session.0, kCMTimeInvalid);
            VTCompressionSessionInvalidate(self.session.0);
        }
    }
}

/// An IOSurface-backed NV12 (BT.601 video range) buffer, shared with the GPU.
fn new_nv12_buffer(width: usize, height: usize) -> Result<Cf, String> {
    // SAFETY: plain CoreVideo calls; the buffer is owned by a `Cf`.
    unsafe {
        let iosurface = Cf::dict(&[]);
        let attrs = Cf::dict(&[(kCVPixelBufferIOSurfacePropertiesKey, iosurface.0)]);
        let mut pb: CFTypeRef = ptr::null();
        let st = CVPixelBufferCreate(
            ptr::null(),
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            attrs.0,
            &mut pb,
        );
        if st != 0 || pb.is_null() {
            return Err(format!("CVPixelBufferCreate: {st}"));
        }
        CVBufferSetAttachment(
            pb,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            kCVAttachmentMode_ShouldPropagate,
        );
        Ok(Cf(pb))
    }
}

/// Copy rows `r0..r1` of `yuv` into the NV12 buffer `pb` (same size).
///
/// # Safety
/// `pb` must be a valid NV12 pixel buffer of `yuv`'s size, not in use by anyone else.
unsafe fn write_nv12(pb: CFTypeRef, yuv: &I420, (r0, r1): (usize, usize)) -> Result<(), String> {
    let (w, h) = yuv.dimensions();
    let (r0, r1) = (r0 & !1, r1.next_multiple_of(2).min(h));
    if r1 <= r0 {
        return Ok(());
    }
    // SAFETY: the buffer is locked while we write each plane within `stride * rows`.
    unsafe {
        let st = CVPixelBufferLockBaseAddress(pb, 0);
        if st != 0 {
            return Err(format!("CVPixelBufferLockBaseAddress: {st}"));
        }
        let (ys, uvs) = (
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
        );
        let (yp, uvp) = (
            CVPixelBufferGetBaseAddressOfPlane(pb, 0).cast_mut(),
            CVPixelBufferGetBaseAddressOfPlane(pb, 1).cast_mut(),
        );
        if yp.is_null() || uvp.is_null() || ys < w || uvs < w {
            CVPixelBufferUnlockBaseAddress(pb, 0);
            return Err("NV12 buffer has no planes".into());
        }
        let y_plane = std::slice::from_raw_parts_mut(yp, ys * h);
        let uv_plane = std::slice::from_raw_parts_mut(uvp, uvs * (h / 2));
        let src_y = yuv.y();
        let (u, v) = (yuv.u(), yuv.v());
        let half = w / 2;
        std::thread::scope(|s| {
            s.spawn(|| {
                for row in r0..r1 {
                    y_plane[row * ys..row * ys + w].copy_from_slice(&src_y[row * w..row * w + w]);
                }
            });
            for cy in r0 / 2..r1 / 2 {
                let dst = &mut uv_plane[cy * uvs..cy * uvs + w];
                let (ur, vr) = (
                    &u[cy * half..(cy + 1) * half],
                    &v[cy * half..(cy + 1) * half],
                );
                for (x, pair) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    *pair = [ur[x], vr[x]];
                }
            }
        });
        CVPixelBufferUnlockBaseAddress(pb, 0);
    }
    Ok(())
}

#[cfg(test)]
impl VtEncoder {
    /// Tear the session down as sleep/wake or a GPU change would.
    pub(crate) fn invalidate_for_test(&mut self) {
        // SAFETY: valid session; it stays allocated (owned by `Cf`), only unusable.
        unsafe { VTCompressionSessionInvalidate(self.session.0) }
    }
}

#[cfg(test)]
mod tests {
    //! `cargo test --release -p rconn-media vt_encode -- --nocapture` prints timings.
    use super::*;
    use crate::video::{VideoDecoder, VideoEncoder, has_idr};
    use std::ptr::NonNull;
    use std::time::Duration;

    #[test]
    fn avcc_lengths_become_start_codes() {
        let avcc = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x06];
        let mut out = Vec::new();
        avcc_to_annex_b(&avcc, &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x06]);
        let mut out = Vec::new();
        avcc_to_annex_b(&[0, 0, 0, 9, 1], &mut out);
        assert!(out.is_empty(), "truncated unit dropped");
    }

    /// A desktop-like NV12 picture twice the height `h`, so frame `k` (scrolled `k * 8`
    /// rows) is a window into it: text-like stripes on light grey, a coloured band.
    struct Pattern {
        w: usize,
        h: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    impl Pattern {
        fn new(w: usize, h: usize) -> Self {
            let rows = 2 * h;
            let mut y = vec![0u8; w * rows];
            let mut uv = vec![128u8; w * rows / 2];
            for r in 0..rows {
                for x in 0..w {
                    let ink = (r / 14) % 3 != 0 && (x / 7 + r / 14) % 5 != 0;
                    y[r * w + x] = if ink { 40 } else { 220 } + ((x / 64) % 4) as u8;
                }
            }
            for r in 0..rows / 2 {
                for x in 0..w / 2 {
                    if (r / 40) % 4 == 1 {
                        uv[r * w + 2 * x] = (60 + (x % 100)) as u8;
                        uv[r * w + 2 * x + 1] = 180;
                    }
                }
            }
            Self { w, h, y, uv }
        }

        /// Frame `k`'s planes (strides `w`).
        fn frame(&self, k: usize) -> (&[u8], &[u8]) {
            let off = ((k * 8) % self.h) & !1;
            (
                &self.y[off * self.w..(off + self.h) * self.w],
                &self.uv[off / 2 * self.w..(off + self.h) / 2 * self.w],
            )
        }

        fn i420(&self, k: usize) -> I420 {
            let mut pic = I420::new(self.w, self.h);
            pic.fill_from_nv12(self.frame(k), (self.w, self.w));
            pic
        }

        /// The RGBA the viewer should show for frame `k`.
        fn rgba(&self, k: usize) -> Vec<u8> {
            let mut rgba = vec![0u8; self.w * self.h * 4];
            crate::yuv::nv12_to_rgba(self.frame(k), (self.w, self.w), (self.w, self.h), &mut rgba);
            rgba
        }
    }

    /// An IOSurface-backed pixel buffer of `format`, filled by `fill(planes, strides)`.
    fn pixel_buffer(
        w: usize,
        h: usize,
        format: u32,
        fill: impl FnOnce(&mut [&mut [u8]], &[usize]),
    ) -> Surface {
        // SAFETY: plain CoreVideo calls on a buffer of our own, locked while written.
        unsafe {
            let pb = if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
                new_nv12_buffer(w, h).unwrap()
            } else {
                let iosurface = Cf::dict(&[]);
                let attrs = Cf::dict(&[(kCVPixelBufferIOSurfacePropertiesKey, iosurface.0)]);
                let mut pb: CFTypeRef = ptr::null();
                assert_eq!(
                    CVPixelBufferCreate(ptr::null(), w, h, format, attrs.0, &mut pb),
                    0
                );
                Cf(pb)
            };
            assert_eq!(CVPixelBufferLockBaseAddress(pb.0, 0), 0);
            let planes = if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
                2
            } else {
                1
            };
            let strides: Vec<usize> = (0..planes)
                .map(|p| {
                    if planes == 1 {
                        CVPixelBufferGetBytesPerRow(pb.0)
                    } else {
                        CVPixelBufferGetBytesPerRowOfPlane(pb.0, p)
                    }
                })
                .collect();
            let mut slices: Vec<&mut [u8]> = (0..planes)
                .map(|p| {
                    let (base, rows) = if planes == 1 {
                        (CVPixelBufferGetBaseAddress(pb.0), h)
                    } else {
                        (
                            CVPixelBufferGetBaseAddressOfPlane(pb.0, p).cast_mut(),
                            if p == 0 { h } else { h / 2 },
                        )
                    };
                    std::slice::from_raw_parts_mut(base, strides[p] * rows)
                })
                .collect();
            fill(&mut slices, &strides);
            CVPixelBufferUnlockBaseAddress(pb.0, 0);
            Surface::from_pixel_buffer(NonNull::new(pb.0.cast_mut()).unwrap())
        }
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVPixelBufferGetBaseAddress(pb: CFTypeRef) -> *mut u8;
        fn CVPixelBufferGetBytesPerRow(pb: CFTypeRef) -> usize;
        fn CVBufferCopyAttachment(pb: CFTypeRef, key: CFTypeRef, mode: *mut u32) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    }

    fn nv12_surface(p: &Pattern, k: usize) -> Surface {
        let (y, uv) = p.frame(k);
        pixel_buffer(
            p.w,
            p.h,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            |planes, strides| {
                for r in 0..p.h {
                    planes[0][r * strides[0]..r * strides[0] + p.w]
                        .copy_from_slice(&y[r * p.w..(r + 1) * p.w]);
                }
                for r in 0..p.h / 2 {
                    planes[1][r * strides[1]..r * strides[1] + p.w]
                        .copy_from_slice(&uv[r * p.w..(r + 1) * p.w]);
                }
            },
        )
    }

    /// User + system CPU seconds of this process (all threads). VideoToolbox's own
    /// service process is not included.
    fn cpu_seconds() -> f64 {
        #[repr(C)]
        struct Timespec {
            sec: i64,
            nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: u32, ts: *mut Timespec) -> i32;
        }
        const CLOCK_PROCESS_CPUTIME_ID: u32 = 12;
        let mut ts = Timespec { sec: 0, nsec: 0 };
        // SAFETY: writes the struct we pass.
        unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        ts.sec as f64 + ts.nsec as f64 / 1e9
    }

    fn percentile(ms: &mut [f64], p: f64) -> f64 {
        ms.sort_by(f64::total_cmp);
        ms[((ms.len() - 1) as f64 * p).round() as usize]
    }

    /// Encodes `n` frames paced at 60 fps (the rate controller spends bits per second of
    /// real time); `encode(k)` is timed. Returns each frame's unit (`None`: dropped by the
    /// rate controller), encode ms and CPU ms per frame.
    fn run(
        n: usize,
        mut encode: impl FnMut(usize) -> Option<Vec<u8>>,
    ) -> (Vec<Option<Vec<u8>>>, Vec<f64>, f64) {
        let mut out = Vec::new();
        let mut ms = Vec::new();
        let mut cpu = 0.0;
        let start = Instant::now();
        for k in 0..n {
            let slot = start + Duration::from_millis(16) * k as u32;
            if let Some(wait) = slot.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let (c0, t0) = (cpu_seconds(), Instant::now());
            let au = encode(k);
            ms.push(t0.elapsed().as_secs_f64() * 1e3);
            cpu += cpu_seconds() - c0;
            out.push(au);
        }
        (out, ms, cpu * 1e3 / n as f64)
    }

    fn settings() -> EncoderSettings {
        EncoderSettings {
            bitrate_bps: 12_000_000,
            max_fps: 60.0,
            threads: 0,
            slice_bytes: None,
        }
    }

    fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
        let sum: u64 = a
            .iter()
            .zip(b)
            .map(|(p, q)| u64::from(p.abs_diff(*q)))
            .sum();
        sum as f64 / a.len() as f64
    }

    fn kbits(units: &[Option<Vec<u8>>]) -> usize {
        units.iter().flatten().map(Vec::len).sum::<usize>() * 8 / 1000
    }

    fn sent(units: &[Option<Vec<u8>>]) -> usize {
        units.iter().flatten().count()
    }

    /// VideoToolbox at the Retina sizes the host sends, through both input paths, against
    /// openh264; every unit decodes on both decoders, at the right size and colours.
    #[test]
    fn encodes_desktop_sizes_that_both_decoders_read() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        const N: usize = 30;
        for (w, h) in [(1920, 1080), (2992, 1870)] {
            let p = Pattern::new(w, h);
            let surfaces: Vec<Surface> = (0..N).map(|k| nv12_surface(&p, k)).collect();
            let pictures: Vec<I420> = (0..N).map(|k| p.i420(k)).collect();

            let mut vt = VideoEncoder::new(&lib, settings()).unwrap();
            let (aus, mut ms, cpu) = run(N, |k| vt.encode_surface(&surfaces[k]).unwrap());
            let name = vt.engine_name().to_owned();
            assert!(name.starts_with("VideoToolbox"), "engine: {name}");

            let mut vt_yuv = VideoEncoder::new(&lib, settings()).unwrap();
            let (yuv_aus, mut ms_yuv, cpu_yuv) =
                run(N, |k| vt_yuv.encode_yuv(&pictures[k], None).unwrap());
            let mut sw = VideoEncoder::software(&lib, settings()).unwrap();
            let (sw_aus, mut ms_sw, cpu_sw) =
                run(N, |k| sw.encode_yuv(&pictures[k], None).unwrap());
            assert_eq!(sw.engine_name(), "openh264");

            println!(
                "{w}x{h} {name}\n  surface: median {:.2} ms, p95 {:.2} ms, CPU {cpu:.2} ms/frame, \
                 {} of {N} sent, {} kbit\n  from I420: median {:.2} ms, p95 {:.2} ms, CPU \
                 {cpu_yuv:.2} ms/frame, {} of {N} sent\n  openh264: median {:.2} ms, p95 {:.2} \
                 ms, CPU {cpu_sw:.2} ms/frame, {} of {N} sent, {} kbit",
                percentile(&mut ms, 0.5),
                percentile(&mut ms, 0.95),
                sent(&aus),
                kbits(&aus),
                percentile(&mut ms_yuv, 0.5),
                percentile(&mut ms_yuv, 0.95),
                sent(&yuv_aus),
                percentile(&mut ms_sw, 0.5),
                percentile(&mut ms_sw, 0.95),
                sent(&sw_aus),
                kbits(&sw_aus),
            );
            // The low-latency rate controller may drop a frame rather than queue it (the
            // host sends the change with the next one), but not most of them.
            assert!(sent(&aus) >= N * 3 / 4, "frames sent: {}", sent(&aus));
            let first = aus[0].as_ref().expect("the first frame is sent");
            assert!(has_idr(first), "starts with a keyframe");
            assert!(
                crate::video::sequence_header(first).is_some(),
                "keyframe carries SPS"
            );
            assert!(
                aus[1..].iter().flatten().all(|au| !has_idr(au)),
                "no keyframes unless asked for"
            );
            let last = (0..N).rev().find(|&k| aus[k].is_some()).unwrap();

            for (decoder, mut dec) in [
                ("VideoToolbox", VideoDecoder::new(&lib).unwrap()),
                ("openh264", VideoDecoder::software(&lib).unwrap()),
            ] {
                for (k, au) in aus.iter().enumerate() {
                    let Some(au) = au else { continue };
                    let f = dec
                        .decode(au)
                        .unwrap_or_else(|e| panic!("{decoder} {w}x{h} frame {k}: {e}"))
                        .unwrap_or_else(|| panic!("{decoder} {w}x{h} frame {k}: no picture"));
                    assert_eq!((f.width, f.height), (w, h), "{decoder}");
                    if k == 0 || k == last {
                        let d = mean_diff(&f.to_rgba(), &p.rgba(k));
                        println!("  {decoder} decodes frame {k}: mean difference {d:.2}");
                        assert!(d < 8.0, "{decoder} {w}x{h} frame {k} differs: {d:.2}");
                    }
                }
            }
        }
    }

    /// A new bitrate applies to the running session: frames shrink, no keyframe. A
    /// keyframe request gives one keyframe, with its stream headers.
    #[test]
    fn bitrate_changes_live_and_keyframes_come_on_request() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let (w, h) = (1280, 720);
        let p = Pattern::new(w, h);
        let mut enc = VideoEncoder::new(&lib, settings()).unwrap();
        let (aus, _, _) = run(90, |k| {
            match k {
                45 => enc.set_bitrate(1_500_000),
                80 => enc.request_keyframe(),
                _ => {}
            }
            enc.encode_surface(&nv12_surface(&p, k)).unwrap()
        });
        let keys: Vec<usize> = (0..aus.len())
            .filter(|&k| aus[k].as_deref().is_some_and(has_idr))
            .collect();
        // A keyframe asked for is owed until one comes out (a dropped frame doesn't count).
        let asked = (80..90)
            .find(|&k| aus[k].is_some())
            .expect("frames after 80");
        assert_eq!(
            keys,
            [0, asked],
            "keyframes: the first, and the one asked for"
        );
        assert!(crate::video::sequence_header(aus[asked].as_ref().unwrap()).is_some());
        let rate = |r: std::ops::Range<usize>| kbits(&aus[r.clone()]) as f64 / r.len() as f64;
        let (before, after) = (rate(20..45), rate(55..80));
        println!(
            "{}: {before:.0} kbit/frame at 12 Mbit/s, {after:.0} at 1.5 Mbit/s; {} of 90 sent",
            enc.engine_name(),
            sent(&aus)
        );
        assert!(
            after < before * 0.5,
            "lower bitrate took effect: {before:.0} -> {after:.0}"
        );
    }

    /// BGRA surfaces (ScreenCaptureKit's default format) are converted by VideoToolbox
    /// with the BT.601 matrix the viewers use: colours survive the trip.
    #[test]
    fn bgra_surfaces_keep_their_colours() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let (w, h) = (640, 360);
        let colours: [[u8; 4]; 4] = [
            [40, 40, 200, 255],   // red (BGRA)
            [40, 200, 40, 255],   // green
            [200, 60, 40, 255],   // blue
            [128, 128, 128, 255], // grey
        ];
        let colour_at = |x: usize, y: usize| colours[(x * 2 / w) + 2 * (y * 2 / h)];
        let surface = pixel_buffer(w, h, u32::from_be_bytes(*b"BGRA"), |planes, strides| {
            for y in 0..h {
                for x in 0..w {
                    let i = y * strides[0] + x * 4;
                    planes[0][i..i + 4].copy_from_slice(&colour_at(x, y));
                }
            }
        });
        let mut enc = VideoEncoder::new(&lib, settings()).unwrap();
        let mut dec = VideoDecoder::new(&lib).unwrap();
        let mut last = None;
        for _ in 0..3 {
            let au = enc.encode_surface(&surface).unwrap().expect("a frame");
            last = dec.decode(&au).unwrap().or(last);
        }
        let f = last.expect("a picture");
        for (x, y) in [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ] {
            let got = &f.to_rgba()[(y * w + x) * 4..(y * w + x) * 4 + 3];
            let [b, g, r, _] = colour_at(x, y);
            let want = [r, g, b];
            let off = got
                .iter()
                .zip(want)
                .map(|(a, b)| a.abs_diff(b))
                .max()
                .unwrap();
            assert!(off <= 6, "at ({x}, {y}): got {got:?}, want {want:?}");
        }
    }

    /// A session torn down under the encoder (sleep/wake, GPU change) is replaced: the next
    /// frame still comes out, as a keyframe a viewer can start from.
    #[test]
    fn a_lost_session_is_replaced() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let p = Pattern::new(640, 360);
        let mut enc = VideoEncoder::new(&lib, settings()).unwrap();
        for k in 0..3 {
            enc.encode_yuv(&p.i420(k), None).unwrap();
        }
        enc.vt_for_test()
            .expect("VideoToolbox")
            .invalidate_for_test();
        let au = enc
            .encode_yuv(&p.i420(3), Some((0, 8)))
            .unwrap()
            .expect("a frame");
        assert!(has_idr(&au), "the new session starts with a keyframe");
        assert!(enc.engine_name().starts_with("VideoToolbox"));
        let mut dec = VideoDecoder::software(&lib).unwrap();
        let f = dec.decode(&au).unwrap().expect("decodes on its own");
        assert!(
            mean_diff(&f.to_rgba(), &p.rgba(3)) < 8.0,
            "the whole picture, not a stale one"
        );
    }

    /// Without VideoToolbox, a surface is converted on the CPU for openh264.
    #[test]
    fn software_encoder_reads_surfaces_too() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let p = Pattern::new(320, 240);
        let mut enc = VideoEncoder::software(&lib, settings()).unwrap();
        assert!(!enc.wants_surfaces());
        let au = enc
            .encode_surface(&nv12_surface(&p, 2))
            .unwrap()
            .expect("a frame");
        assert_eq!(enc.engine_name(), "openh264");
        let f = VideoDecoder::software(&lib)
            .unwrap()
            .decode(&au)
            .unwrap()
            .expect("a picture");
        assert!(mean_diff(&f.to_rgba(), &p.rgba(2)) < 8.0);
    }

    /// ScreenCaptureKit's own surfaces, straight into the encoder: the capture-to-unit path
    /// the host runs. Reads (never changes) the real screen; nothing is kept.
    /// `cargo test --release -p rconn-media screen_surfaces -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the Screen Recording permission; reads the real screen"]
    fn screen_surfaces_go_straight_to_the_encoder() {
        use crate::capture::Capturer;
        let lib = crate::cisco::local_binary_path().expect("openh264 path");
        for max in [(16384, 16384), (1920, 1080)] {
            let mut cap = crate::capture::macos::MacCapturer::new().expect("capture");
            assert!(cap.use_surfaces(max), "surface mode");
            let mut enc = VideoEncoder::new(&lib, settings()).unwrap();
            let mut dec = VideoDecoder::new(&lib).unwrap();
            let (mut frames, mut ms, mut size) = (0, Vec::new(), (0, 0));
            let start = Instant::now();
            // An idle screen sends nothing: ask for a keyframe now and then, as viewers do.
            while start.elapsed() < Duration::from_secs(3) {
                let Some((s, c)) = cap.next_surface(Duration::from_millis(250)).unwrap() else {
                    enc.request_keyframe();
                    continue;
                };
                let t0 = Instant::now();
                // SAFETY: reads an attachment of the surface's valid pixel buffer.
                let bt601 = unsafe {
                    let m = Cf(CVBufferCopyAttachment(
                        s.as_ptr(),
                        kCVImageBufferYCbCrMatrixKey,
                        ptr::null_mut(),
                    ));
                    !m.0.is_null() && CFEqual(m.0, kCVImageBufferYCbCrMatrix_ITU_R_601_4) != 0
                };
                assert!(bt601, "ScreenCaptureKit converts with BT.601");
                let au = enc.encode_surface(&s).unwrap();
                ms.push(t0.elapsed().as_secs_f64() * 1e3);
                if let Some(au) = au {
                    let f = dec.decode(&au).unwrap().expect("a picture");
                    size = (f.width, f.height);
                    frames += 1;
                }
                assert!(c.captured_at.elapsed() < Duration::from_secs(1));
            }
            let n = ms.len();
            println!(
                "max {max:?}: {} -> {size:?}, {frames} of {n} surfaces sent, encode median \
                 {:.2} ms, p95 {:.2} ms",
                enc.engine_name(),
                if n > 0 { percentile(&mut ms, 0.5) } else { 0.0 },
                if n > 0 {
                    percentile(&mut ms, 0.95)
                } else {
                    0.0
                },
            );
            assert!(frames > 0, "at least one picture");
            assert!(size.0 <= max.0 && size.1 <= max.1, "fits {max:?}: {size:?}");
        }
    }
}
