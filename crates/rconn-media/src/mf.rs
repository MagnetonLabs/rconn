//! H.264 encoding with Windows Media Foundation (the encoder built into Windows).
//!
//! Prefers the GPU's hardware encoder (NVIDIA NVENC, Intel Quick Sync, AMD AMF register
//! one as an asynchronous MFT): near-zero CPU. It must pass a short self-test first;
//! otherwise, or with `RCONN_ENCODER=software`, Microsoft's software encoder is used
//! (a synchronous MFT that spreads work across cores). Output is an Annex-B byte stream.
//!
//! Unsafe is confined to this module: COM calls into Media Foundation, plus copying a
//! frame into / out of an `IMFMediaBuffer` while it is locked, within its stated length.
#![allow(unsafe_code)]

use crate::codec::CodecError;
use rconn_core::codec::VideoCodec;
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVEncVideoLTRBufferControl, CODECAPI_AVEncVideoMarkLTRFrame, CODECAPI_AVEncVideoMaxQP,
    CODECAPI_AVEncVideoUseLTRFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_LongTermReferenceFrameInfo, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncAV1VProfile_Main_420_8, eAVEncCommonRateControlMode_LowDelayVBR, eAVEncH264VProfile_Base,
    eAVEncH265VProfile_Main_420_8,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

fn err(e: windows::core::Error) -> CodecError {
    CodecError::OpenH264(format!("Media Foundation: {e}"))
}

// SAFETY: the MFT is created in (and the process stays in) the multithreaded apartment,
// and this wrapper is used by one thread at a time (it is moved, never shared).
unsafe impl Send for MfH264Encoder {}

pub struct MfH264Encoder {
    /// Kept to shut the encoder down when this is dropped (see `Drop`).
    activate: IMFActivate,
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    width: u32,
    height: u32,
    fps: u32,
    frame: u64,
    name: String,
    out_size: u32,
    /// The encoder allocates its own output samples (hardware encoders do).
    provides_samples: bool,
    /// Hardware (asynchronous) encoders: their event queue, and how many frames they
    /// have asked for that we haven't given yet.
    events: Option<IMFMediaEventGenerator>,
    credits: u32,
    /// Encoders fed with D3D11 textures (see [`gpu`]): the conversion into their input.
    gpu: Option<Box<gpu::GpuInput>>,
    /// Long-term reference slots the encoder accepted (0: no LTR support).
    ltr_slots: u8,
    /// The last output sample's `MFSampleExtension_LongTermReferenceFrameInfo`, if the
    /// encoder sets it.
    ltr_info: Option<u32>,
    /// The encoder took a maximum QP: refinement passes lower it for one frame.
    max_qp: bool,
    /// What it encodes.
    kind: VideoCodec,
}

/// Quantizer ceiling of ordinary frames (as on the openh264 path): never blocky text.
pub const MAX_QP: u8 = 32;
/// Quantizer ceiling of Microsoft's software encoder. With [`MAX_QP`] its rate controller
/// could not hold the bitrate: a 2.28 MB keyframe of a 2992x1878 desktop (2.3 s at
/// 8 Mbps), 1.7 MB pictures of it scrolling at 2688x1686 (16 KB with this ceiling). It
/// takes the ceiling only when it starts (changes while encoding are ignored: no
/// refinement passes), so a still screen is sharpened by re-encoding it (the host's
/// top-off), each picture finer within the bitrate.
pub const SOFTWARE_MAX_QP: u8 = 44;
/// The widest picture given to Microsoft's software encoder (`RCONN_MF_MAX_WIDTH`
/// overrides it). Wider pictures make larger keyframes (a 2992x1878 text screen: 218 KB
/// against 174 KB at 2688 px). What it fails on, dense text scrolling (0.5 MB a picture
/// at 2688 px, 1.2 MB at 2992 px, 17-18 dB either way; Windows 11 ARM VM,
/// `encode_bench::probe_widths`), fails at any width: those pictures go to openh264 at
/// half size (`stream::MF_OVERSIZE`).
pub const SOFTWARE_MAX_WIDTH: usize = 2688;

/// [`SOFTWARE_MAX_WIDTH`], or `RCONN_MF_MAX_WIDTH` (comparisons).
pub fn software_max_width() -> usize {
    std::env::var("RCONN_MF_MAX_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&w: &usize| w >= 64)
        .unwrap_or(SOFTWARE_MAX_WIDTH)
}
/// Frames between keyframes asked of Microsoft's software encoder: "0" (its default) put
/// one in every 30 frames (2.28 MB each at 2992x1878); keyframes come on request.
const SOFTWARE_GOP: u32 = 100_000;

/// Long-term reference slots asked for: one confirmed by the viewer, one being marked.
const LTR_SLOTS: u8 = 2;
/// `CODECAPI_AVEncVideoLTRBufferControl`'s trust mode 1, "trust until": an LTR frame stays
/// valid until the application marks another into its slot.
const LTR_TRUST_UNTIL: u32 = 1 << 16;
/// `CODECAPI_AVEncVideoUseLTRFrame`'s usage mode 1: predict *only* from the LTR frames in
/// the bitmap (not from the short-term frames the viewer may have lost).
const LTR_USE_ONLY: u32 = 1 << 16;

/// D3D11 texture input (the GPU-resident pipeline).
#[path = "mf_gpu.rs"]
mod gpu;

/// A configured encoder: transform, codec API, friendly name, async event queue,
/// long-term reference slots.
type Started = (
    IMFTransform,
    Option<ICodecAPI>,
    String,
    Option<IMFMediaEventGenerator>,
    u8,
);

impl MfH264Encoder {
    /// A low-latency H.264 encoder for `width` x `height` NV12 frames: the GPU's if it
    /// works, Microsoft's software encoder otherwise.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self, CodecError> {
        Self::new_codec(VideoCodec::H264, width, height, fps, bitrate_bps)
    }

    /// A low-latency `kind` encoder: the GPU's if one works. H.264 falls back to
    /// Microsoft's software encoder; HEVC and AV1 only use a software MFT (one a Store
    /// extension installed) when `RCONN_CODEC` forces that codec, and are never bundled.
    pub fn new_codec(
        kind: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err)?;
        }
        let software_ok = kind == VideoCodec::H264 || VideoCodec::from_env() == Some(kind);
        if software_ok && std::env::var("RCONN_ENCODER").is_ok_and(|v| v == "software") {
            return Self::open_software(kind, width, height, fps, bitrate_bps);
        }
        // Every GPU encoder registered, best first: one that fails to start (a vendor's
        // MFT left behind by an old driver, a second GPU's) must not hide the next.
        let mut why = CodecError::OpenH264(format!("no GPU {} encoder", kind.name()));
        for activate in Self::candidates(kind, true, None).unwrap_or_default() {
            match Self::open(activate, kind, true, None, width, height, fps, bitrate_bps)
                .and_then(|mut e| e.self_test().map(|()| e))
            {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::debug!(error = %e, "GPU encoder unusable; trying the next");
                    why = e;
                }
            }
        }
        if !software_ok {
            return Err(why);
        }
        tracing::info!(error = %why, "no usable GPU encoder; using software");
        Self::open_software(kind, width, height, fps, bitrate_bps)
    }

    /// Microsoft's software encoder (H.264), or a forced codec's software MFT.
    fn open_software(
        kind: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        let activate = Self::candidates(kind, false, None)?
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::OpenH264(format!("no {} encoder MFT", kind.name())))?;
        Self::open(activate, kind, false, None, width, height, fps, bitrate_bps)
    }

    /// Is a hardware H.264 encoder registered (without starting it)?
    pub fn hardware_available() -> bool {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                return false;
            }
        }
        Self::candidates(VideoCodec::H264, true, None).is_ok_and(|c| !c.is_empty())
    }

    /// Is this the GPU's encoder?
    pub fn is_hardware(&self) -> bool {
        self.events.is_some()
    }

    /// The registered NV12 -> `codec` encoders of the given kind, in Media Foundation's
    /// order of merit; those of PCI vendor `vendor` (if given) first.
    fn candidates(
        codec: VideoCodec,
        hardware: bool,
        vendor: Option<u32>,
    ) -> Result<Vec<IMFActivate>, CodecError> {
        let flags = if hardware {
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
        } else {
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER
        };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype(codec),
        };
        unsafe {
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG(flags.0),
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .map_err(err)?;
            // Take ownership of every entry, so each is released when dropped. Freeing
            // only the array leaked every entry, and with it the encoder it activates:
            // worker threads and frame buffers, per encoder built.
            let mut owned: Vec<IMFActivate> = (0..count as usize)
                .filter_map(|i| std::ptr::read(list.add(i)))
                .collect();
            windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
            if let Some(v) = vendor {
                // Stable: keeps Media Foundation's order within each group.
                owned.sort_by_key(|a| vendor_of(a) != Some(v));
            }
            Ok(owned)
        }
    }

    /// Start the encoder `activate` makes. With `d3d`, it reads D3D11 textures from that
    /// device manager's device (the encoder must be D3D11-aware).
    #[allow(clippy::too_many_arguments)]
    fn open(
        activate: IMFActivate,
        kind: VideoCodec,
        hardware: bool,
        d3d: Option<&IMFDXGIDeviceManager>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        let (mft, codec, name, events, ltr_slots) = match Self::start(
            &activate,
            kind,
            hardware,
            d3d,
            (width, height, fps, bitrate_bps),
        ) {
            Ok(started) => started,
            Err(e) => {
                // Don't leave a half-started encoder's threads behind.
                let _ = unsafe { activate.ShutdownObject() };
                return Err(e);
            }
        };
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(err)?;
        // A maximum QP the encoder takes (Windows 8+ encoders may change it while
        // encoding; vendors' are not obliged to, and then say so here or ignore it).
        // (Not AV1: its quantizer scale is 0-255.)
        let max_qp = kind != VideoCodec::Av1
            && codec.as_ref().is_some_and(|c| unsafe {
                c.IsSupported(&CODECAPI_AVEncVideoMaxQP).is_ok()
                    && c.SetValue(
                        &CODECAPI_AVEncVideoMaxQP,
                        &VARIANT::from(u32::from(max_qp_for(hardware))),
                    )
                    .is_ok()
            });
        Ok(Self {
            activate,
            mft,
            codec,
            width,
            height,
            fps,
            frame: 0,
            name,
            out_size: info.cbSize.max(width * height * 3 / 2),
            provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
            events,
            credits: 0,
            gpu: None,
            ltr_slots,
            ltr_info: None,
            max_qp,
            kind,
        })
    }

    /// Activate and configure the encoder: (transform, codec API, name, async events).
    fn start(
        activate: &IMFActivate,
        kind: VideoCodec,
        hardware: bool,
        d3d: Option<&IMFDXGIDeviceManager>,
        (width, height, fps, bitrate_bps): (u32, u32, u32, u32),
    ) -> Result<Started, CodecError> {
        unsafe {
            let name = {
                let mut buf = [0u16; 256];
                let mut len = 0u32;
                match activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) {
                    Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
                    Err(_) => "unknown MFT".into(),
                }
            };
            let named = |e: windows::core::Error| CodecError::OpenH264(format!("{name}: {e}"));
            let mft: IMFTransform = activate.ActivateObject().map_err(named)?;
            let attributes = mft.GetAttributes().ok();
            // Asynchronous (hardware) MFTs must be unlocked before use, and talk through
            // events: "need input" / "have output".
            let events = if hardware {
                mft.GetAttributes()
                    .and_then(|a| a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1))
                    .map_err(named)?;
                Some(mft.cast::<IMFMediaEventGenerator>().map_err(named)?)
            } else {
                None
            };
            if let Some(a) = &attributes {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            // Texture input: the device goes to the encoder before any media type, as the
            // types it accepts may depend on it.
            if let Some(manager) = d3d {
                let aware = attributes
                    .as_ref()
                    .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok())
                    .unwrap_or(0);
                if aware == 0 {
                    return Err(CodecError::OpenH264(format!("{name} is not D3D11-aware")));
                }
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .map_err(named)?;
            }

            // Low-latency, constant-ish quality settings (before setting types).
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = codec.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32),
                );
                let _ = codec.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(0u32)); // fastest
                // Keyframes on request (hardware encoders take 0 for "never"; Microsoft's
                // software encoder takes it for 30 frames).
                let gop = if hardware { 0 } else { SOFTWARE_GOP };
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
                // No B-frames: every frame comes out as soon as it is encoded.
                let _ =
                    codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                // Same quality floor as the openh264 path: never blocky text (looser on
                // the software encoder, see `SOFTWARE_MAX_QP`).
                // (H.264 and HEVC share the 0-51 scale; AV1's is 0-255: left alone.)
                if kind != VideoCodec::Av1 {
                    let _ = codec.SetValue(
                        &CODECAPI_AVEncVideoMaxQP,
                        &VARIANT::from(u32::from(max_qp_for(hardware))),
                    );
                }
            }
            if let Some(codec) = &codec {
                experimental_settings(codec);
            }
            // Long-term references for repairing losses without a keyframe (Windows 8.1+;
            // optional for vendors' encoders, so asked for and checked, never assumed).
            // `RCONN_ENCODER_LTR=0` turns it off for an encoder that misbehaves with it.
            // Not on Microsoft's software encoder: its repairs broke the decoder
            // (openh264's, on a Windows 11 ARM VM, `ltr_tests`), so its losses get a
            // keyframe.
            let ltr_slots = match &codec {
                Some(_) if std::env::var("RCONN_ENCODER_LTR").is_ok_and(|v| v == "0") => 0,
                Some(_) if !hardware => 0,
                Some(codec) => {
                    let ok = codec
                        .IsSupported(&CODECAPI_AVEncVideoLTRBufferControl)
                        .is_ok()
                        && codec
                            .SetValue(
                                &CODECAPI_AVEncVideoLTRBufferControl,
                                &VARIANT::from(LTR_TRUST_UNTIL | u32::from(LTR_SLOTS)),
                            )
                            .is_ok()
                        && codec.IsSupported(&CODECAPI_AVEncVideoMarkLTRFrame).is_ok()
                        && codec.IsSupported(&CODECAPI_AVEncVideoUseLTRFrame).is_ok();
                    if ok { LTR_SLOTS } else { 0 }
                }
                None => 0,
            };
            tracing::info!(encoder = %name, ltr_slots, "Media Foundation encoder configured");

            let size = (u64::from(width) << 32) | u64::from(height);
            let rate = (u64::from(fps) << 32) | 1;
            let out_type: IMFMediaType = MFCreateMediaType().map_err(err)?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(err)?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &subtype(kind))
                .map_err(err)?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
                .map_err(err)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(err)?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(err)?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(err)?;
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile(kind))
                .map_err(err)?;
            mft.SetOutputType(0, &out_type, 0).map_err(named)?;

            let in_type: IMFMediaType = MFCreateMediaType().map_err(err)?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(err)?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(err)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(err)?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(err)?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(err)?;
            mft.SetInputType(0, &in_type, 0).map_err(named)?;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(named)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(named)?;
            Ok((mft, codec, name, events, ltr_slots))
        }
    }

    /// Make the next frame a keyframe (a viewer joined or lost data).
    pub fn force_keyframe(&mut self) {
        if let Some(c) = &self.codec {
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
    }

    /// Long-term reference slots usable with [`Self::mark_ltr`] / [`Self::use_ltr`]
    /// (0: the encoder has no LTR support; repairs need a keyframe).
    pub fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }

    /// Make the next frame given to the encoder the long-term reference in `slot`.
    pub fn mark_ltr(&mut self, slot: u8) -> bool {
        self.ltr_set(&CODECAPI_AVEncVideoMarkLTRFrame, u32::from(slot))
    }

    /// Predict the next frame only from the long-term reference in `slot` (a loss
    /// repair: the viewer has that picture, maybe not the ones after it).
    pub fn use_ltr(&mut self, slot: u8) -> bool {
        self.ltr_set(&CODECAPI_AVEncVideoUseLTRFrame, LTR_USE_ONLY | (1 << slot))
    }

    fn ltr_set(&mut self, api: &windows::core::GUID, value: u32) -> bool {
        match &self.codec {
            Some(c) if self.ltr_slots > 0 => {
                unsafe { c.SetValue(api, &VARIANT::from(value)) }.is_ok()
            }
            _ => false,
        }
    }

    /// What the encoder said about the last output's references, if it says:
    /// (slot this frame was marked into, bitmap of the slots it predicted from).
    pub fn last_ltr_info(&self) -> Option<(Option<u8>, u16)> {
        self.ltr_info.map(|v| {
            let marked = (v >> 16) as u16;
            ((marked != 0xffff).then_some(marked as u8), v as u16)
        })
    }

    /// Can a frame be coded under a lower quantizer ceiling ([`Self::set_max_qp`])?
    /// Not on Microsoft's software encoder: it accepts the change and ignores it.
    pub fn can_refine(&self) -> bool {
        self.max_qp && self.is_hardware()
    }

    /// Code the next frames no coarser than quantizer `qp` (a refinement pass), or back to
    /// [`MAX_QP`]. `false` if the encoder refused it.
    pub fn set_max_qp(&mut self, qp: u8) -> bool {
        match &self.codec {
            Some(c) if self.max_qp => {
                unsafe { c.SetValue(&CODECAPI_AVEncVideoMaxQP, &VARIANT::from(u32::from(qp))) }
                    .is_ok()
            }
            _ => false,
        }
    }

    /// Change the target bitrate on the fly (no restart, no keyframe).
    pub fn set_bitrate(&mut self, bps: u32) {
        if let Some(c) = &self.codec {
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps)) };
        }
    }

    /// The encoder in use (e.g. "H264 Encoder MFT", or a GPU vendor's).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it encodes.
    pub fn codec(&self) -> VideoCodec {
        self.kind
    }

    /// A GPU encoder must turn a few frames into output before we trust it.
    fn self_test(&mut self) -> Result<(), CodecError> {
        let gray = vec![128u8; (self.width * self.height * 3 / 2) as usize];
        for _ in 0..4 {
            if !self.encode(&gray)?.is_empty() {
                self.force_keyframe(); // the viewer never saw these frames
                return Ok(());
            }
        }
        Err(CodecError::OpenH264(format!(
            "{} produced no output in its self-test",
            self.name
        )))
    }

    /// Encode one NV12 frame (`width * height * 3 / 2` bytes); returns the Annex-B output
    /// available so far (may be empty while the encoder buffers).
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<u8>, CodecError> {
        let need = (self.width * self.height * 3 / 2) as usize;
        if nv12.len() < need {
            return Err(CodecError::OpenH264("NV12 frame too small".into()));
        }
        let buffer = unsafe {
            let buffer = MFCreateMemoryBuffer(need as u32).map_err(err)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(err)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, need);
            buffer.Unlock().map_err(err)?;
            buffer.SetCurrentLength(need as u32).map_err(err)?;
            buffer
        };
        self.submit(&buffer)
    }

    /// Encode the frame in `buffer` (memory, or a D3D11 texture); returns the Annex-B
    /// output available so far.
    fn submit(&mut self, buffer: &IMFMediaBuffer) -> Result<Vec<u8>, CodecError> {
        let sample = unsafe {
            let sample: IMFSample = MFCreateSample().map_err(err)?;
            sample.AddBuffer(buffer).map_err(err)?;
            let tick = 10_000_000 / i64::from(self.fps);
            sample
                .SetSampleTime(self.frame as i64 * tick)
                .map_err(err)?;
            sample.SetSampleDuration(tick).map_err(err)?;
            sample
        };
        self.frame += 1;
        let mut out = Vec::new();
        match self.events.clone() {
            None => {
                unsafe { self.mft.ProcessInput(0, &sample, 0) }.map_err(err)?;
                while self.output_once(&mut out)? {}
            }
            Some(events) => {
                // Collect anything finished meanwhile, then wait (briefly) to be asked.
                let deadline = Instant::now() + Duration::from_millis(500);
                while self.credits == 0 {
                    if !self.pump(&events, &mut out, deadline)? {
                        return Err(CodecError::OpenH264(format!(
                            "{} stopped asking for frames",
                            self.name
                        )));
                    }
                }
                unsafe { self.mft.ProcessInput(0, &sample, 0) }.map_err(err)?;
                self.credits -= 1;
                // Low-latency encoders finish each frame right away: wait for it.
                let had = out.len();
                let deadline = Instant::now() + Duration::from_millis(50);
                while out.len() == had && self.pump(&events, &mut out, deadline)? {}
            }
        }
        Ok(out)
    }

    /// Handle one event from an asynchronous encoder, waiting until `deadline` for it.
    /// `false` if none came.
    fn pump(
        &mut self,
        events: &IMFMediaEventGenerator,
        out: &mut Vec<u8>,
        deadline: Instant,
    ) -> Result<bool, CodecError> {
        loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let kind = unsafe { ev.GetType() }.map_err(err)?;
                    if kind == METransformNeedInput.0 as u32 {
                        self.credits += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.output_once(out)?;
                    }
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_micros(250));
                }
                Err(e) => return Err(err(e)),
            }
        }
    }

    /// Take one output sample, appending it to `out`. `false` if the encoder has none.
    fn output_once(&mut self, out: &mut Vec<u8>) -> Result<bool, CodecError> {
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                let obuf = MFCreateMemoryBuffer(self.out_size).map_err(err)?;
                let osample: IMFSample = MFCreateSample().map_err(err)?;
                osample.AddBuffer(&obuf).map_err(err)?;
                Some(osample)
            };
            let mut data = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut data, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut data[0].pSample);
            std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
            match r {
                Ok(()) => {
                    if let Some(sample) = sample {
                        self.ltr_info = sample
                            .GetUINT32(&MFSampleExtension_LongTermReferenceFrameInfo)
                            .ok();
                        let buf = sample.ConvertToContiguousBuffer().map_err(err)?;
                        let mut p = std::ptr::null_mut();
                        let mut len = 0u32;
                        buf.Lock(&mut p, None, Some(&mut len)).map_err(err)?;
                        out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
                        buf.Unlock().map_err(err)?;
                    }
                    Ok(true)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Some hardware encoders settle their output format with the first
                    // frame: take the type they now offer and ask again.
                    let t = self.mft.GetOutputAvailableType(0, 0).map_err(err)?;
                    self.mft.SetOutputType(0, &t, 0).map_err(err)?;
                    let info = self.mft.GetOutputStreamInfo(0).map_err(err)?;
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    self.out_size = info.cbSize.max(self.out_size);
                    self.output_once(out)
                }
                Err(e) => Err(err(e)),
            }
        }
    }
}

/// `RCONN_MF_SET=Name:value+Name:value`: encoder settings to try (benches), from a short
/// list of the codec API's; each attempt is logged with its result.
fn experimental_settings(codec: &ICodecAPI) {
    use windows::Win32::Media::MediaFoundation as m;
    let Ok(list) = std::env::var("RCONN_MF_SET") else {
        return;
    };
    for item in list.split('+').filter(|s| !s.is_empty()) {
        let Some((name, value)) = item.split_once(':') else {
            continue;
        };
        let Ok(value) = value.parse::<u32>() else {
            continue;
        };
        let api = match name {
            "MinQP" => m::CODECAPI_AVEncVideoMinQP,
            "MaxQP" => CODECAPI_AVEncVideoMaxQP,
            "Quality" => m::CODECAPI_AVEncCommonQuality,
            "QualityVsSpeed" => CODECAPI_AVEncCommonQualityVsSpeed,
            "RateControl" => CODECAPI_AVEncCommonRateControlMode,
            "Threads" => m::CODECAPI_AVEncNumWorkerThreads,
            "SliceMode" => m::CODECAPI_AVEncSliceControlMode,
            "SliceSize" => m::CODECAPI_AVEncSliceControlSize,
            "RefFrames" => m::CODECAPI_AVEncVideoMaxNumRefFrame,
            "ContentType" => m::CODECAPI_AVEncVideoContentType,
            "Level" => m::CODECAPI_AVEncMPVLevel,
            "Cabac" => m::CODECAPI_AVEncH264CABACEnable,
            "LowLatency" => CODECAPI_AVLowLatencyMode,
            "MotionTradeoff" => m::CODECAPI_AVEncVideoCBRMotionTradeoff,
            "IntraRefresh" => m::CODECAPI_AVEncVideoGradualIntraRefresh,
            _ => {
                tracing::warn!(name, "RCONN_MF_SET: unknown setting");
                continue;
            }
        };
        let v = if name == "LowLatency" || name == "Cabac" {
            VARIANT::from(value != 0)
        } else {
            VARIANT::from(value)
        };
        let supported = unsafe { codec.IsSupported(&api) }.is_ok();
        let set = unsafe { codec.SetValue(&api, &v) };
        tracing::info!(name, value, supported, ok = set.is_ok(), "RCONN_MF_SET");
    }
}

/// The quantizer ceiling an encoder starts with.
fn max_qp_for(hardware: bool) -> u8 {
    if hardware { MAX_QP } else { SOFTWARE_MAX_QP }
}

/// The Media Foundation subtype of `codec`.
/// The profile asked for: the one every decoder of the codec has (8-bit 4:2:0).
fn profile(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => eAVEncH264VProfile_Base.0 as u32,
        VideoCodec::H265 => eAVEncH265VProfile_Main_420_8.0 as u32,
        VideoCodec::Av1 => eAVEncAV1VProfile_Main_420_8.0 as u32,
    }
}

pub(crate) fn subtype(codec: VideoCodec) -> windows::core::GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::H265 => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

/// Is an NV12 -> `codec` encoder registered (a hardware one, if `hardware`)? Enumerated
/// only, not started.
pub fn encoder_exists(codec: VideoCodec, hardware: bool) -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
            return false;
        }
    }
    MfH264Encoder::candidates(codec, hardware, None).is_ok_and(|c| !c.is_empty())
}

/// The PCI vendor of a hardware encoder ("VEN_10DE" -> 0x10DE).
fn vendor_of(activate: &IMFActivate) -> Option<u32> {
    let mut buf = [0u16; 64];
    let mut len = 0u32;
    unsafe {
        activate
            .GetString(
                &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
                &mut buf,
                Some(&mut len),
            )
            .ok()?;
    }
    let s = String::from_utf16_lossy(&buf[..len as usize]);
    u32::from_str_radix(s.trim().strip_prefix("VEN_")?, 16).ok()
}

impl Drop for MfH264Encoder {
    /// An activated MFT lives until its activation object shuts it down; dropping our
    /// references alone can leave its worker threads and buffers behind.
    fn drop(&mut self) {
        unsafe {
            let _ = self.activate.ShutdownObject();
        }
    }
}
