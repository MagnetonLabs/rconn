//! H.264 encoding with Windows Media Foundation (the encoder built into Windows).
//!
//! Uses the first matching synchronous encoder MFT: Microsoft's own software encoder,
//! which spreads work across cores, or a vendor encoder where one is registered.
//! Output is an Annex-B byte stream, like openh264's.
//!
//! Unsafe is confined to this module: COM calls into Media Foundation, plus copying a
//! frame into / out of an `IMFMediaBuffer` while it is locked, within its stated length.
#![allow(unsafe_code)]

use crate::codec::CodecError;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxQP, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFMediaType, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_LowDelayVBR, eAVEncH264VProfile_Base,
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
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    width: u32,
    height: u32,
    fps: u32,
    frame: u64,
    name: String,
    out_size: u32,
}

impl MfH264Encoder {
    /// A low-latency H.264 encoder for `width` x `height` NV12 frames.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self, CodecError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err)?;
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .map_err(err)?;
            let activates = std::slice::from_raw_parts(list, count as usize);
            let activate = activates.iter().flatten().next().cloned();
            windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
            let activate =
                activate.ok_or_else(|| CodecError::OpenH264("no H.264 encoder MFT".into()))?;
            let name = {
                let mut buf = [0u16; 256];
                let mut len = 0u32;
                match activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) {
                    Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
                    Err(_) => "unknown MFT".into(),
                }
            };
            let mft: IMFTransform = activate.ActivateObject().map_err(err)?;

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
                // Same quality floor as the openh264 path: never blocky text.
                let _ = codec.SetValue(&CODECAPI_AVEncVideoMaxQP, &VARIANT::from(32u32));
            }

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
            mft.SetOutputType(0, &out_type, 0).map_err(err)?;

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
            mft.SetInputType(0, &in_type, 0).map_err(err)?;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(err)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(err)?;
            let info = mft.GetOutputStreamInfo(0).map_err(err)?;
            Ok(Self {
                mft,
                codec,
                width,
                height,
                fps,
                frame: 0,
                name,
                out_size: info.cbSize.max(width * height * 3 / 2),
            })
        }
    }

    /// Make the next frame a keyframe (a viewer joined or lost data).
    pub fn force_keyframe(&mut self) {
        if let Some(c) = &self.codec {
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
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

    /// Encode one NV12 frame (`width * height * 3 / 2` bytes); returns the Annex-B output
    /// available so far (may be empty while the encoder buffers).
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<u8>, CodecError> {
        let need = (self.width * self.height * 3 / 2) as usize;
        if nv12.len() < need {
            return Err(CodecError::OpenH264("NV12 frame too small".into()));
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(need as u32).map_err(err)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(err)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, need);
            buffer.Unlock().map_err(err)?;
            buffer.SetCurrentLength(need as u32).map_err(err)?;
            let sample: IMFSample = MFCreateSample().map_err(err)?;
            sample.AddBuffer(&buffer).map_err(err)?;
            let tick = 10_000_000 / i64::from(self.fps);
            sample
                .SetSampleTime(self.frame as i64 * tick)
                .map_err(err)?;
            sample.SetSampleDuration(tick).map_err(err)?;
            self.frame += 1;
            self.mft.ProcessInput(0, &sample, 0).map_err(err)?;

            let mut out = Vec::new();
            loop {
                let obuf = MFCreateMemoryBuffer(self.out_size).map_err(err)?;
                let osample: IMFSample = MFCreateSample().map_err(err)?;
                osample.AddBuffer(&obuf).map_err(err)?;
                let mut data = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(osample.clone())),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.mft.ProcessOutput(0, &mut data, &mut status);
                std::mem::ManuallyDrop::drop(&mut data[0].pSample);
                std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
                match r {
                    Ok(()) => {
                        let mut p = std::ptr::null_mut();
                        let mut len = 0u32;
                        obuf.Lock(&mut p, None, Some(&mut len)).map_err(err)?;
                        out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
                        obuf.Unlock().map_err(err)?;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) => return Err(err(e)),
                }
            }
            Ok(out)
        }
    }
}
