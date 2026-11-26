//! macOS hardware H.264 decoding through VideoToolbox.
//!
//! Annex-B access units in, NV12 planes out. The pixels are copied as decoded (no colour
//! conversion here): the viewer converts on the GPU with the stream's own colour
//! signalling ([`crate::sps`]), where the hardware would guess BT.709 for HD sizes while
//! hosts encode BT.601.

#![allow(unsafe_code, non_upper_case_globals)]

use rconn_core::codec::VideoCodec;
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
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
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
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        alloc: CFTypeRef,
        count: usize,
        sets: *const *const u8,
        sizes: *const usize,
        nal_length_size: i32,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreate(
        alloc: CFTypeRef,
        codec: u32,
        width: i32,
        height: i32,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFTypeRef;
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
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
}

pub(crate) const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
pub(crate) const kCMVideoCodecType_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
pub(crate) const kCMVideoCodecType_AV1: u32 = u32::from_be_bytes(*b"av01");

/// The Core Media codec type of `codec`.
pub(crate) fn codec_type(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => kCMVideoCodecType_H264,
        VideoCodec::H265 => kCMVideoCodecType_HEVC,
        VideoCodec::Av1 => kCMVideoCodecType_AV1,
    }
}

/// Does this machine have a hardware decoder for `codec`? (HEVC: Macs from 2017 and all
/// Apple silicon; AV1: M3, A17 Pro and later.)
pub fn hardware_decodes(codec: VideoCodec) -> bool {
    // SAFETY: a plain query.
    unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) != 0 }
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
    codec: VideoCodec,
    /// The stream headers the session was made for: H.264 SPS, PPS; HEVC VPS, SPS, PPS;
    /// the AV1 sequence header OBU.
    sets: Vec<Vec<u8>>,
    format: Option<Cf>,
    session: Option<Cf>,
    /// The sample handed to VideoToolbox: length-prefixed NAL units, or AV1 OBUs.
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
    /// An H.264 decoder; the session is created at the first SPS/PPS.
    pub fn new() -> Self {
        Self::for_codec(VideoCodec::H264)
    }

    /// A `codec` decoder; the session is created at the first stream headers.
    pub fn for_codec(codec: VideoCodec) -> Self {
        Self {
            codec,
            sets: Vec::new(),
            format: None,
            session: None,
            avcc: Vec::new(),
        }
    }

    /// Split an access unit into its stream headers (all of them, or none) and the
    /// sample VideoToolbox decodes (`self.avcc`).
    fn take_apart(&mut self, au: &[u8]) -> Option<Vec<Vec<u8>>> {
        self.avcc.clear();
        match self.codec {
            VideoCodec::H264 | VideoCodec::H265 => {
                let hevc = self.codec == VideoCodec::H265;
                // H.264: SPS 7, PPS 8. HEVC: VPS 32, SPS 33, PPS 34.
                let mut sets: [Option<&[u8]>; 3] = [None; 3];
                for nal in crate::video::nal_units(au) {
                    let (kind, picture) = if hevc {
                        let t = (nal[0] >> 1) & 0x3f;
                        (
                            t.checked_sub(32).filter(|&i| i < 3),
                            t < 32 && nal.len() > 2,
                        )
                    } else {
                        let t = nal[0] & 0x1f;
                        (
                            t.checked_sub(6).filter(|&i| (1..3).contains(&i)),
                            (1..=5).contains(&t),
                        )
                    };
                    if let Some(i) = kind {
                        sets[usize::from(i)] = Some(nal);
                    } else if picture {
                        self.avcc
                            .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        self.avcc.extend_from_slice(nal);
                    }
                }
                let want = if hevc { &sets[..] } else { &sets[1..] };
                want.iter()
                    .map(|s| s.map(<[u8]>::to_vec))
                    .collect::<Option<Vec<_>>>()
            }
            VideoCodec::Av1 => {
                // The sample is the temporal unit without temporal delimiters, padding
                // or our own metadata (AV1 in ISOBMFF); the sequence header stays in it.
                let mut header = None;
                for u in rconn_core::codec::units(VideoCodec::Av1, au) {
                    let obu = &au[u.start..u.end];
                    match rconn_core::codec::obu::kind(obu[0]) {
                        rconn_core::codec::obu::SEQUENCE_HEADER => header = Some(obu.to_vec()),
                        rconn_core::codec::obu::TEMPORAL_DELIMITER
                        | rconn_core::codec::obu::PADDING => continue,
                        _ => {}
                    }
                    self.avcc.extend_from_slice(obu);
                }
                header.map(|h| vec![h])
            }
        }
    }

    /// Decode one access unit (Annex-B; AV1: OBUs) into tight NV12 planes in `nv12`.
    /// Returns the picture size, or `None` if this unit held no picture.
    pub fn decode(
        &mut self,
        annex_b: &[u8],
        nv12: &mut Vec<u8>,
    ) -> Result<Option<(usize, usize)>, String> {
        if let Some(sets) = self.take_apart(annex_b)
            && (sets != self.sets || self.session.is_none())
        {
            self.sets = sets;
            self.open()?;
        }
        let pictures = match self.codec {
            VideoCodec::Av1 => rconn_core::codec::units(VideoCodec::Av1, &self.avcc)
                .iter()
                .any(|u| {
                    rconn_core::codec::kind(VideoCodec::Av1, &self.avcc, u)
                        != rconn_core::codec::UnitKind::Other
                        && rconn_core::codec::kind(VideoCodec::Av1, &self.avcc, u)
                            != rconn_core::codec::UnitKind::UserData
                }),
            _ => !self.avcc.is_empty(),
        };
        if !pictures {
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

    /// A format description for the stream headers in `self.sets` (owned by the caller).
    fn format_description(&self) -> Result<CFTypeRef, String> {
        let sets: Vec<*const u8> = self.sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = self.sets.iter().map(Vec::len).collect();
        let mut format: CFTypeRef = ptr::null();
        // SAFETY: the parameter sets outlive the calls, which copy them.
        let st = unsafe {
            match self.codec {
                VideoCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null(),
                    sets.len(),
                    sets.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut format,
                ),
                VideoCodec::H265 => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    ptr::null(),
                    sets.len(),
                    sets.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    ptr::null(),
                    &mut format,
                ),
                VideoCodec::Av1 => return av1_format(&self.sets[0]),
            }
        };
        if st != 0 {
            return Err(format!("{} format description: {st}", self.codec.name()));
        }
        Ok(format)
    }

    fn open(&mut self) -> Result<(), String> {
        self.session = None;
        self.format = None;
        // SAFETY: plain CoreFoundation/CoreMedia/VideoToolbox calls with valid pointers;
        // every created object is owned by a `Cf`.
        unsafe {
            let format = Cf(self.format_description()?);

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

/// The `av1C` box (AV1 codec configuration record) for a sequence header OBU.
fn av1c(obu: &[u8]) -> Option<(Vec<u8>, (usize, usize))> {
    let (_, payload) = rconn_core::codec::obu_parts(obu)?;
    let s = crate::sps::parse_av1(&obu[payload])?;
    let mut b = vec![
        0x81, // marker, version 1
        (s.profile << 5) | (s.level & 0x1f),
        (s.tier << 7)
            | (u8::from(s.high_bitdepth) << 6)
            | (u8::from(s.twelve_bit) << 5)
            | (u8::from(s.mono) << 4)
            | (u8::from(s.subsampling.0) << 3)
            | (u8::from(s.subsampling.1) << 2)
            | (s.chroma_position & 3),
        0, // no initial presentation delay
    ];
    // configOBUs: the sequence header, with its size field.
    if obu[0] & 0x02 != 0 {
        b.extend_from_slice(obu);
    } else {
        b.push(obu[0] | 0x02);
        let header = if obu[0] & 0x04 != 0 { 2 } else { 1 };
        b.extend_from_slice(&obu[1..header]);
        rconn_core::codec::put_leb128((obu.len() - header) as u64, &mut b);
        b.extend_from_slice(&obu[header..]);
    }
    Some((b, (s.info.width, s.info.height)))
}

/// An AV1 format description (owned by the caller) from a sequence header OBU.
fn av1_format(obu: &[u8]) -> Result<CFTypeRef, String> {
    let (record, (w, h)) = av1c(obu).ok_or("unreadable AV1 sequence header")?;
    const UTF8: u32 = 0x0800_0100;
    // SAFETY: plain CoreFoundation/CoreMedia calls; every object made here is owned by a
    // `Cf` (the description, returned, by the caller).
    unsafe {
        let data = Cf(CFDataCreate(
            ptr::null(),
            record.as_ptr(),
            record.len() as isize,
        ));
        let key = Cf(CFStringCreateWithBytes(
            ptr::null(),
            b"av1C".as_ptr(),
            4,
            UTF8,
            0,
        ));
        let atoms = Cf(CFDictionaryCreate(
            ptr::null(),
            [key.0].as_ptr(),
            [data.0].as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ));
        let ext = Cf(CFDictionaryCreate(
            ptr::null(),
            [kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms].as_ptr(),
            [atoms.0].as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ));
        let mut format: CFTypeRef = ptr::null();
        let st = CMVideoFormatDescriptionCreate(
            ptr::null(),
            kCMVideoCodecType_AV1,
            w as i32,
            h as i32,
            ext.0,
            &mut format,
        );
        if st != 0 {
            return Err(format!("AV1 format description: {st}"));
        }
        Ok(format)
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
