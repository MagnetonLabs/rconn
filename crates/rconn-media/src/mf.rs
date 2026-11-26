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
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_LowDelayVBR,
    eAVEncH264VProfile_Base,
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
}

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
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err)?;
        }
        if std::env::var("RCONN_ENCODER").is_ok_and(|v| v == "software") {
            return Self::open_software(width, height, fps, bitrate_bps);
        }
        // Every GPU encoder registered, best first: one that fails to start (a vendor's
        // MFT left behind by an old driver, a second GPU's) must not hide the next.
        let mut why = CodecError::OpenH264("no GPU H.264 encoder".into());
        for activate in Self::candidates(true, None).unwrap_or_default() {
            match Self::open(activate, true, None, width, height, fps, bitrate_bps)
                .and_then(|mut e| e.self_test().map(|()| e))
            {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::debug!(error = %e, "GPU encoder unusable; trying the next");
                    why = e;
                }
            }
        }
        tracing::info!(error = %why, "no usable GPU encoder; using software");
        Self::open_software(width, height, fps, bitrate_bps)
    }

    /// Microsoft's software encoder.
    fn open_software(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        let activate = Self::candidates(false, None)?
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::OpenH264("no H.264 encoder MFT".into()))?;
        Self::open(activate, false, None, width, height, fps, bitrate_bps)
    }

    /// Is a hardware H.264 encoder registered (without starting it)?
    pub fn hardware_available() -> bool {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                return false;
            }
        }
        Self::candidates(true, None).is_ok_and(|c| !c.is_empty())
    }

    /// Is this the GPU's encoder?
    pub fn is_hardware(&self) -> bool {
        self.events.is_some()
    }

    /// The registered NV12 -> H.264 encoders of the given kind, in Media Foundation's
    /// order of merit; those of PCI vendor `vendor` (if given) first.
    fn candidates(hardware: bool, vendor: Option<u32>) -> Result<Vec<IMFActivate>, CodecError> {
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
            guidSubtype: MFVideoFormat_H264,
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
    fn open(
        activate: IMFActivate,
        hardware: bool,
        d3d: Option<&IMFDXGIDeviceManager>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        let (mft, codec, name, events, ltr_slots) =
            match Self::start(&activate, hardware, d3d, (width, height, fps, bitrate_bps)) {
                Ok(started) => started,
                Err(e) => {
                    // Don't leave a half-started encoder's threads behind.
                    let _ = unsafe { activate.ShutdownObject() };
                    return Err(e);
                }
            };
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(err)?;
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
        })
    }

    /// Activate and configure the encoder: (transform, codec API, name, async events).
    fn start(
        activate: &IMFActivate,
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
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(0u32)); // keyframes on request
                // No B-frames: every frame comes out as soon as it is encoded.
                let _ =
                    codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                // Same quality floor as the openh264 path: never blocky text.
                let _ = codec.SetValue(&CODECAPI_AVEncVideoMaxQP, &VARIANT::from(32u32));
            }
            // Long-term references for repairing losses without a keyframe (Windows 8.1+;
            // optional for vendors' encoders, so asked for and checked, never assumed).
            // `RCONN_ENCODER_LTR=0` turns it off for an encoder that misbehaves with it.
            let ltr_slots = match &codec {
                Some(_) if std::env::var("RCONN_ENCODER_LTR").is_ok_and(|v| v == "0") => 0,
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
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
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
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
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
