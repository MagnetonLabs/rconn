//! macOS hardware H.264 decoding through VideoToolbox.
//!
//! Annex-B access units in, NV12 planes out. The pixels are copied as decoded (no colour
//! conversion here): the viewer converts on the GPU with the stream's own colour
//! signalling ([`crate::sps`]), where the hardware would guess BT.709 for HD sizes while
//! hosts encode BT.601.

#![allow(unsafe_code, non_upper_case_globals)]

use std::ffi::c_void;
use std::ptr;

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

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    image: CFTypeRef,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

/// Only its address is used.
#[repr(C)]
struct Opaque {
    _p: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorNull: CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        key_cb: *const Opaque,
        value_cb: *const Opaque,
    ) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFTypeRef;
    fn CVPixelBufferLockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CFTypeRef, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CFTypeRef, plane: usize) -> usize;
    fn CVPixelBufferGetWidth(pb: CFTypeRef) -> usize;
    fn CVPixelBufferGetHeight(pb: CFTypeRef) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: CFTypeRef,
        count: usize,
        sets: *const *const u8,
        sizes: *const usize,
        nal_length_size: i32,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: CFTypeRef,
        memory: *mut c_void,
        len: usize,
        block_alloc: CFTypeRef,
        custom_source: *const c_void,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        alloc: CFTypeRef,
        data: CFTypeRef,
        format: CFTypeRef,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes_count: isize,
        sizes: *const usize,
        out: *mut CFTypeRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFTypeRef;
    static kVTDecompressionPropertyKey_RealTime: CFTypeRef;
    fn VTDecompressionSessionCreate(
        alloc: CFTypeRef,
        format: CFTypeRef,
        decoder_spec: CFTypeRef,
        image_attrs: CFTypeRef,
        callback: *const OutputCallbackRecord,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: CFTypeRef,
        sample: CFTypeRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: CFTypeRef) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: CFTypeRef);
    fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
}

const kCFNumberSInt32Type: isize = 3;
const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: i32 = i32::from_be_bytes(*b"420v");
const kCVPixelBufferLock_ReadOnly: u64 = 1;

/// Owns one Core Foundation reference.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we hold one reference, released exactly once.
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Decoded picture, written by the output callback.
extern "C" fn on_output(
    _refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    _info: u32,
    image: CFTypeRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    if status == 0 && !image.is_null() && !frame_refcon.is_null() {
        // SAFETY: `frame_refcon` is the `Option<Cf>` slot `decode` passes in and keeps
        // alive until the frame is out; the image is retained for that slot.
        unsafe {
            *frame_refcon.cast::<Option<Cf>>() = Some(Cf(CFRetain(image)));
        }
    }
}

pub struct VtDecoder {
    sps: Vec<u8>,
    pps: Vec<u8>,
    format: Option<Cf>,
    session: Option<Cf>,
    avcc: Vec<u8>,
}

// SAFETY: the session and format are used only through `&mut self`; VideoToolbox
// sessions may be used from any thread.
unsafe impl Send for VtDecoder {}

impl Default for VtDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VtDecoder {
    /// The session is created at the first SPS/PPS.
    pub fn new() -> Self {
        Self {
            sps: Vec::new(),
            pps: Vec::new(),
            format: None,
            session: None,
            avcc: Vec::new(),
        }
    }

    /// Decode one Annex-B access unit into tight NV12 planes in `nv12`. Returns the
    /// picture size, or `None` if this unit held no picture.
    pub fn decode(
        &mut self,
        annex_b: &[u8],
        nv12: &mut Vec<u8>,
    ) -> Result<Option<(usize, usize)>, String> {
        let (mut sps, mut pps) = (None, None);
        self.avcc.clear();
        for nal in crate::video::nal_units(annex_b) {
            match nal[0] & 0x1f {
                7 => sps = Some(nal),
                8 => pps = Some(nal),
                1..=5 => {
                    self.avcc
                        .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    self.avcc.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if let (Some(s), Some(p)) = (sps, pps)
            && (s != self.sps.as_slice() || p != self.pps.as_slice() || self.session.is_none())
        {
            self.sps = s.to_vec();
            self.pps = p.to_vec();
            self.open()?;
        }
        if self.avcc.is_empty() {
            return Ok(None);
        }
        let (Some(session), Some(format)) = (&self.session, &self.format) else {
            // Picture data without stream headers to read it by (decoder lost its place,
            // or the stream changed size): say so, so the viewer asks for a keyframe.
            return Err("waiting for a keyframe with stream headers".into());
        };

        let mut image: Option<Cf> = None;
        // SAFETY: plain CoreMedia/VideoToolbox calls; `self.avcc` and `image` outlive
        // the synchronous decode (we wait for the frame before returning).
        let status = unsafe {
            let mut block: CFTypeRef = ptr::null();
            let st = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                self.avcc.as_mut_ptr().cast(),
                self.avcc.len(),
                kCFAllocatorNull,
                ptr::null(),
                0,
                self.avcc.len(),
                0,
                &mut block,
            );
            if st != 0 {
                return Err(format!("CMBlockBufferCreateWithMemoryBlock: {st}"));
            }
            let block = Cf(block);
            let mut sample: CFTypeRef = ptr::null();
            let size = self.avcc.len();
            let st = CMSampleBufferCreateReady(
                ptr::null(),
                block.0,
                format.0,
                1,
                0,
                ptr::null(),
                1,
                &size,
                &mut sample,
            );
            if st != 0 {
                return Err(format!("CMSampleBufferCreateReady: {st}"));
            }
            let sample = Cf(sample);
            let mut info = 0u32;
            let st = VTDecompressionSessionDecodeFrame(
                session.0,
                sample.0,
                0, // synchronous
                (&raw mut image).cast(),
                &mut info,
            );
            VTDecompressionSessionWaitForAsynchronousFrames(session.0);
            st
        };
        if status != 0 {
            // A lost session (sleep, GPU change) is rebuilt at the next keyframe.
            self.session = None;
            return Err(format!("VTDecompressionSessionDecodeFrame: {status}"));
        }
        let Some(image) = image else {
            return Ok(None);
        };
        Ok(Some(copy_nv12(&image, nv12)))
    }

    fn open(&mut self) -> Result<(), String> {
        self.session = None;
        self.format = None;
        // SAFETY: plain CoreFoundation/CoreMedia/VideoToolbox calls with valid pointers;
        // every created object is owned by a `Cf`.
        unsafe {
            let sets = [self.sps.as_ptr(), self.pps.as_ptr()];
            let sizes = [self.sps.len(), self.pps.len()];
            let mut format: CFTypeRef = ptr::null();
            let st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                2,
                sets.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut format,
            );
            if st != 0 {
                return Err(format!(
                    "CMVideoFormatDescriptionCreateFromH264ParameterSets: {st}"
                ));
            }
            let format = Cf(format);

            let spec_keys = [kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder];
            let spec_values = [kCFBooleanTrue];
            let spec = Cf(CFDictionaryCreate(
                ptr::null(),
                spec_keys.as_ptr(),
                spec_values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            ));
            let nv12 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
            let pixel_format = Cf(CFNumberCreate(
                ptr::null(),
                kCFNumberSInt32Type,
                (&raw const nv12).cast(),
            ));
            let attr_keys = [kCVPixelBufferPixelFormatTypeKey];
            let attr_values = [pixel_format.0];
            let attrs = Cf(CFDictionaryCreate(
                ptr::null(),
                attr_keys.as_ptr(),
                attr_values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            ));
            let callback = OutputCallbackRecord {
                callback: on_output,
                refcon: ptr::null_mut(),
            };
            let mut session: CFTypeRef = ptr::null();
            let st = VTDecompressionSessionCreate(
                ptr::null(),
                format.0,
                spec.0,
                attrs.0,
                &callback,
                &mut session,
            );
            if st != 0 {
                return Err(format!("VTDecompressionSessionCreate: {st}"));
            }
            VTSessionSetProperty(
                session,
                kVTDecompressionPropertyKey_RealTime,
                kCFBooleanTrue,
            );
            self.format = Some(format);
            self.session = Some(Cf(session));
        }
        Ok(())
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            // SAFETY: valid session; released by `Cf` afterwards.
            unsafe { VTDecompressionSessionInvalidate(s.0) }
        }
    }
}

/// NV12 pixel buffer -> tight NV12 planes (Y rows, then interleaved CbCr rows).
fn copy_nv12(image: &Cf, out: &mut Vec<u8>) -> (usize, usize) {
    // SAFETY: the buffer is locked read-only while we read each plane's
    // `stride * rows` bytes.
    unsafe {
        let pb = image.0;
        CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
        let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
        let (ys, uvs) = (
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
        );
        let ch = h.div_ceil(2);
        let cw = w.div_ceil(2) * 2;
        let y = std::slice::from_raw_parts(CVPixelBufferGetBaseAddressOfPlane(pb, 0), ys * h);
        let uv = std::slice::from_raw_parts(CVPixelBufferGetBaseAddressOfPlane(pb, 1), uvs * ch);
        out.clear();
        out.reserve(w * h + cw * ch);
        for row in y.chunks_exact(ys) {
            out.extend_from_slice(&row[..w]);
        }
        for row in uv.chunks_exact(uvs) {
            out.extend_from_slice(&row[..cw]);
        }
        CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
        (w, h)
    }
}
