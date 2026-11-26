//! H.264 decoding with Windows Media Foundation's decoder (built into Windows 8+, x64 and
//! ARM64), on the GPU where it can.
//!
//! The decoder MFT gets a Direct3D 11 device through an `IMFDXGIDeviceManager`, which
//! makes it decode with DXVA (the GPU's video engine) into NV12 textures. Only a GPU that
//! lists an H.264 decoder profile is used; without one the viewer decodes with openh264
//! ([`crate::decode`]). The same MFT can decode on the CPU (`RCONN_DECODER=mf-software`,
//! for comparisons): on a measured 8-core ARM64 VM it was no faster than openh264.
//! Low-latency mode is on (a picture out for every access unit in; our streams have no
//! B-frames, so there is nothing to reorder).
//!
//! Output is NV12, copied to memory: hardware pictures through a staging texture (one GPU
//! copy and a map), software ones straight from the sample buffer. A new frame size inside
//! the stream arrives as `MF_E_TRANSFORM_STREAM_CHANGE` and is met by choosing the output
//! type again.
//!
//! Unsafe is confined to this module: COM calls into Media Foundation and Direct3D 11,
//! and reading a locked/mapped buffer within the size and pitch it reports.
#![allow(unsafe_code)]

use crate::codec::CodecError;
use rconn_core::codec::VideoCodec;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoDevice,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVDecVideoAcceleration_H264, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
    MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoArea,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

fn err(what: &str, e: windows::core::Error) -> CodecError {
    CodecError::OpenH264(format!("Media Foundation decoder: {what}: {e}"))
}

/// The registered `codec` -> NV12 decoders, best first. HEVC and AV1 decoders come with
/// Store extensions ("HEVC Video Extensions", "AV1 Video Extension"), so they may be
/// missing on any machine.
fn decoders(codec: VideoCodec) -> Vec<IMFActivate> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
            return Vec::new();
        }
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: crate::mf::subtype(codec),
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let flags = MFT_ENUM_FLAG_SYNCMFT
            | MFT_ENUM_FLAG_ASYNCMFT
            | MFT_ENUM_FLAG_HARDWARE
            | MFT_ENUM_FLAG_LOCALMFT
            | MFT_ENUM_FLAG_SORTANDFILTER;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .is_err()
        {
            return Vec::new();
        }
        // Each entry is owned (released when dropped), then the array is freed.
        let owned = (0..count as usize)
            .filter_map(|i| std::ptr::read(list.add(i)))
            .collect();
        windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
        owned
    }
}

/// Is a `codec` decoder installed that [`MfH264Decoder::new_codec`] can use (a
/// synchronous MFT)? H.264's is part of Windows.
pub fn decoder_exists(codec: VideoCodec) -> bool {
    codec == VideoCodec::H264
        || decoders(codec)
            .iter()
            .any(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 0)
}

/// The Direct3D 11 device the decoder works on, and a staging texture to read from.
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
    staging: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
}

impl Gpu {
    fn new(kind: VideoCodec) -> Result<Self, CodecError> {
        unsafe {
            let levels: [D3D_FEATURE_LEVEL; 4] = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| err("D3D11CreateDevice", e))?;
            let device: ID3D11Device =
                device.ok_or_else(|| CodecError::OpenH264("no D3D11 device".into()))?;
            let context = context.ok_or_else(|| CodecError::OpenH264("no D3D11 context".into()))?;
            // Virtual and basic display adapters have no video engine: say so up front.
            let video: ID3D11VideoDevice =
                device.cast().map_err(|e| err("no D3D11 video device", e))?;
            let want = match kind {
                VideoCodec::H264 => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
                VideoCodec::H265 => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
                VideoCodec::Av1 => D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0,
            };
            let has = (0..video.GetVideoDecoderProfileCount())
                .filter_map(|i| video.GetVideoDecoderProfile(i).ok())
                .any(|g| g == want);
            if !has {
                return Err(CodecError::OpenH264(format!(
                    "the GPU has no {} decoder (DXVA profile)",
                    kind.name()
                )));
            }
            // The decoder uses the device from its own threads too.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
            let manager: IMFDXGIDeviceManager =
                manager.ok_or_else(|| CodecError::OpenH264("no DXGI device manager".into()))?;
            manager
                .ResetDevice(&device, token)
                .map_err(|e| err("ResetDevice", e))?;
            Ok(Self {
                device,
                context,
                manager,
                staging: None,
            })
        }
    }

    /// Copy a decoded texture (one slice of the decoder's texture array) to memory.
    fn read(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        (width, height): (usize, usize),
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let want = D3D11_TEXTURE2D_DESC {
                MipLevels: 1,
                ArraySize: 1,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let reuse = self.staging.as_ref().is_some_and(|(_, d)| {
                (d.Width, d.Height, d.Format) == (want.Width, want.Height, want.Format)
            });
            if !reuse {
                let mut t = None;
                self.device
                    .CreateTexture2D(&want, None, Some(&mut t))
                    .map_err(|e| err("staging texture", e))?;
                self.staging = t.map(|t| (t, want));
            }
            let Some((staging, _)) = &self.staging else {
                return Err(CodecError::OpenH264("no staging texture".into()));
            };
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, texture, subresource, None);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .map_err(|e| err("Map", e))?;
            let pitch = map.RowPitch as usize;
            let base = map.pData.cast::<u8>().cast_const();
            // NV12 in a mapped texture: the CbCr plane follows `Height` rows of luma.
            let (w, h) = (
                width.min(want.Width as usize),
                height.min(want.Height as usize),
            );
            let total = pitch * want.Height as usize * 3 / 2;
            let src = std::slice::from_raw_parts(base, total);
            copy_nv12(src, pitch, pitch * want.Height as usize, (w, h), out);
            self.context.Unmap(staging, 0);
            Ok(())
        }
    }
}

/// Copy NV12 rows (`pitch` bytes apart, CbCr plane at `uv_offset`) into tight planes.
fn copy_nv12(
    src: &[u8],
    pitch: usize,
    uv_offset: usize,
    (w, h): (usize, usize),
    out: &mut Vec<u8>,
) {
    let (cw, ch) = (w.div_ceil(2) * 2, h.div_ceil(2));
    out.clear();
    out.reserve(w * h + cw * ch);
    for row in 0..h {
        out.extend_from_slice(&src[row * pitch..row * pitch + w]);
    }
    for row in 0..ch {
        let a = uv_offset + row * pitch;
        out.extend_from_slice(&src[a..a + cw]);
    }
}

/// The negotiated output: the decoder's buffer size and pitch, and the picture in it.
#[derive(Clone, Copy, Debug)]
struct Output {
    frame: (usize, usize),
    stride: usize,
    display: Option<(usize, usize)>,
}

pub struct MfH264Decoder {
    mft: IMFTransform,
    gpu: Option<Gpu>,
    output: Option<Output>,
    provides_samples: bool,
    out_size: u32,
    time: i64,
    /// Whether the last picture came out of GPU memory.
    on_gpu: bool,
    /// What it decodes.
    kind: VideoCodec,
    /// The activation an enumerated (HEVC, AV1) decoder came from: shut down on drop.
    activate: Option<IMFActivate>,
}

// SAFETY: created in (and the process stays in) the multithreaded apartment; the decoder
// is used by one thread at a time (moved, never shared). The D3D11 device is
// multithread-protected.
unsafe impl Send for MfH264Decoder {}

impl MfH264Decoder {
    /// Microsoft's H.264 decoder, on the GPU (`hardware`) or the CPU.
    pub fn new(hardware: bool) -> Result<Self, CodecError> {
        Self::new_codec(VideoCodec::H264, hardware)
    }

    /// A `kind` decoder: Microsoft's for H.264 (part of Windows); for HEVC and AV1 the
    /// first synchronous decoder MFT installed for it ("HEVC Video Extensions", "AV1 Video
    /// Extension"; asynchronous vendor MFTs are skipped).
    pub fn new_codec(kind: VideoCodec, hardware: bool) -> Result<Self, CodecError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| err("MFStartup", e))?;
            let (mft, activate): (IMFTransform, Option<IMFActivate>) = if kind == VideoCodec::H264 {
                let mft = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| err("create", e))?;
                (mft, None)
            } else {
                let activate = decoders(kind)
                    .into_iter()
                    .find(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0)
                    .ok_or_else(|| {
                        CodecError::OpenH264(format!("no {} decoder installed", kind.name()))
                    })?;
                let mft = activate
                    .ActivateObject::<IMFTransform>()
                    .map_err(|e| err("activate", e))?;
                (mft, Some(activate))
            };
            let attrs = mft.GetAttributes().map_err(|e| err("attributes", e))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                if kind == VideoCodec::H264 {
                    let _ = c.SetValue(
                        &CODECAPI_AVDecVideoAcceleration_H264,
                        &VARIANT::from(u32::from(hardware)),
                    );
                }
            }
            let gpu = if hardware {
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    return Err(CodecError::OpenH264(
                        "decoder is not Direct3D 11 aware".into(),
                    ));
                }
                let gpu = Gpu::new(kind)?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                    .map_err(|e| err("SET_D3D_MANAGER", e))?;
                Some(gpu)
            } else {
                None
            };

            let input: IMFMediaType = MFCreateMediaType().map_err(|e| err("media type", e))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("input type", e))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &crate::mf::subtype(kind))
                .map_err(|e| err("input type", e))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| err("input type", e))?;
            mft.SetInputType(0, &input, 0)
                .map_err(|e| err("SetInputType", e))?;
            let mut dec = Self {
                mft,
                gpu,
                output: None,
                provides_samples: false,
                out_size: 0,
                time: 0,
                on_gpu: false,
                kind,
                activate,
            };
            dec.choose_output()?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("BEGIN_STREAMING", e))?;
            dec.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("START_OF_STREAM", e))?;
            Ok(dec)
        }
    }

    /// Decoding on the GPU (a Direct3D 11 device was accepted)?
    pub fn is_hardware(&self) -> bool {
        self.gpu.is_some()
    }

    /// Did the last picture come out of GPU memory (DXVA decoding really in use)?
    pub fn last_on_gpu(&self) -> bool {
        self.on_gpu
    }

    /// Pick NV12 among the output types the decoder offers (again after a stream change).
    fn choose_output(&mut self) -> Result<(), CodecError> {
        unsafe {
            let mut i = 0;
            let chosen = loop {
                let t = self
                    .mft
                    .GetOutputAvailableType(0, i)
                    .map_err(|e| err("no NV12 output", e))?;
                if t.GetGUID(&MF_MT_SUBTYPE)
                    .is_ok_and(|g| g == MFVideoFormat_NV12)
                {
                    break t;
                }
                i += 1;
            };
            self.mft
                .SetOutputType(0, &chosen, 0)
                .map_err(|e| err("SetOutputType", e))?;
            let size = chosen.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let frame = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
            let stride = chosen
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| (s as i32).unsigned_abs() as usize)
                .unwrap_or(frame.0)
                .max(frame.0);
            let mut area = MFVideoArea::default();
            let display = chosen
                .GetBlob(
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    std::slice::from_raw_parts_mut(
                        (&raw mut area).cast::<u8>(),
                        size_of::<MFVideoArea>(),
                    ),
                    None,
                )
                .ok()
                .map(|()| (area.Area.cx.max(0) as usize, area.Area.cy.max(0) as usize))
                .filter(|&(w, h)| w > 0 && h > 0);
            self.output = Some(Output {
                frame,
                stride,
                display,
            });
            let info = self
                .mft
                .GetOutputStreamInfo(0)
                .map_err(|e| err("GetOutputStreamInfo", e))?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.out_size = info
                .cbSize
                .max((frame.0 * frame.1 * 3 / 2).try_into().unwrap_or(u32::MAX));
            Ok(())
        }
    }

    /// Decode one access unit (Annex-B; AV1: a temporal unit's OBUs) into tight NV12
    /// planes in `out`. Returns the picture size, or `None` if the unit gave no picture
    /// (yet). `size_hint` is the SPS's cropped size, used where the decoder reports none.
    pub fn decode(
        &mut self,
        annex_b: &[u8],
        size_hint: Option<(usize, usize)>,
        out: &mut Vec<u8>,
    ) -> Result<Option<(usize, usize)>, CodecError> {
        // A temporal unit starts with a temporal delimiter, which RTP leaves out.
        let with_delimiter;
        let annex_b = if self.kind == VideoCodec::Av1
            && annex_b.first().is_none_or(|&b| {
                rconn_core::codec::obu::kind(b) != rconn_core::codec::obu::TEMPORAL_DELIMITER
            }) {
            with_delimiter = [&[0x12, 0x00][..], annex_b].concat();
            &with_delimiter[..]
        } else {
            annex_b
        };
        let sample = unsafe {
            let len = u32::try_from(annex_b.len())
                .map_err(|_| CodecError::OpenH264("access unit too large".into()))?;
            let buffer = MFCreateMemoryBuffer(len).map_err(|e| err("buffer", e))?;
            let mut ptr = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| err("Lock", e))?;
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
            buffer.Unlock().map_err(|e| err("Unlock", e))?;
            buffer
                .SetCurrentLength(len)
                .map_err(|e| err("SetCurrentLength", e))?;
            let sample: IMFSample = MFCreateSample().map_err(|e| err("sample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| err("AddBuffer", e))?;
            // 100 ns units; only the order matters (low-latency: no reordering).
            sample
                .SetSampleTime(self.time * 333_333)
                .map_err(|e| err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(333_333)
                .map_err(|e| err("SetSampleDuration", e))?;
            sample
        };
        self.time += 1;
        let mut got = None;
        match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // Pictures still waiting: take them, then the decoder takes input again.
                got = self.drain(size_hint, out)?.or(got);
                unsafe { self.mft.ProcessInput(0, &sample, 0) }
                    .map_err(|e| err("ProcessInput", e))?;
            }
            Err(e) => return Err(err("ProcessInput", e)),
        }
        Ok(self.drain(size_hint, out)?.or(got))
    }

    /// Take every picture the decoder has; the last one stays in `out`.
    fn drain(
        &mut self,
        size_hint: Option<(usize, usize)>,
        out: &mut Vec<u8>,
    ) -> Result<Option<(usize, usize)>, CodecError> {
        let mut got = None;
        // A stream change is followed by the picture; a few rounds are plenty.
        for _ in 0..8 {
            match self.output_once(size_hint, out)? {
                Step::Picture(size) => got = Some(size),
                Step::StreamChange => self.choose_output()?,
                Step::NeedInput => break,
            }
        }
        Ok(got)
    }

    fn output_once(
        &mut self,
        size_hint: Option<(usize, usize)>,
        out: &mut Vec<u8>,
    ) -> Result<Step, CodecError> {
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                let buf = MFCreateMemoryBuffer(self.out_size).map_err(|e| err("buffer", e))?;
                let s: IMFSample = MFCreateSample().map_err(|e| err("sample", e))?;
                s.AddBuffer(&buf).map_err(|e| err("AddBuffer", e))?;
                Some(s)
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
                    let Some(sample) = sample else {
                        return Ok(Step::NeedInput);
                    };
                    let size = self.copy_out(&sample, size_hint, out)?;
                    Ok(Step::Picture(size))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Step::NeedInput),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Step::StreamChange),
                Err(e) => Err(err("ProcessOutput", e)),
            }
        }
    }

    /// Copy a decoded sample's NV12 picture into `out`; its displayed size.
    fn copy_out(
        &mut self,
        sample: &IMFSample,
        size_hint: Option<(usize, usize)>,
        out: &mut Vec<u8>,
    ) -> Result<(usize, usize), CodecError> {
        let o = self
            .output
            .ok_or_else(|| CodecError::OpenH264("no output type".into()))?;
        let clamp = |(w, h): (usize, usize)| (w.min(o.frame.0) & !1, h.min(o.frame.1) & !1);
        let size = clamp(o.display.or(size_hint).unwrap_or(o.frame));
        unsafe {
            let buffer: IMFMediaBuffer = sample
                .GetBufferByIndex(0)
                .map_err(|e| err("GetBufferByIndex", e))?;
            if let (Some(gpu), Ok(dxgi)) = (self.gpu.as_mut(), buffer.cast::<IMFDXGIBuffer>()) {
                let mut texture: *mut std::ffi::c_void = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture)
                    .map_err(|e| err("GetResource", e))?;
                let texture = ID3D11Texture2D::from_raw(texture);
                let index = dxgi
                    .GetSubresourceIndex()
                    .map_err(|e| err("GetSubresourceIndex", e))?;
                gpu.read(&texture, index, size, out)?;
                self.on_gpu = true;
                return Ok(size);
            }
            self.on_gpu = false;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
                b2.Lock2D(&mut p, &mut pitch)
                    .map_err(|e| err("Lock2D", e))?;
                let pitch = pitch.unsigned_abs() as usize;
                let src = std::slice::from_raw_parts(p.cast_const(), pitch * o.frame.1 * 3 / 2);
                copy_nv12(src, pitch, pitch * o.frame.1, size, out);
                b2.Unlock2D().map_err(|e| err("Unlock2D", e))?;
            } else {
                let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
                buffer
                    .Lock(&mut p, None, Some(&mut len))
                    .map_err(|e| err("Lock", e))?;
                let need = o.stride * o.frame.1 * 3 / 2;
                if (len as usize) < need {
                    let _ = buffer.Unlock();
                    return Err(CodecError::OpenH264(format!(
                        "decoded buffer {len} bytes, expected {need}"
                    )));
                }
                let src = std::slice::from_raw_parts(p.cast_const(), need);
                copy_nv12(src, o.stride, o.stride * o.frame.1, size, out);
                buffer.Unlock().map_err(|e| err("Unlock", e))?;
            }
        }
        Ok(size)
    }
}

enum Step {
    Picture((usize, usize)),
    StreamChange,
    NeedInput,
}

impl Drop for MfH264Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if self.gpu.is_some() {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
            if let Some(a) = &self.activate {
                let _ = a.ShutdownObject();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_rows_are_copied_without_padding() {
        // 4x2 picture in a buffer with 8-byte rows.
        let pitch = 8;
        let mut src = vec![0u8; pitch * 3];
        src[..4].copy_from_slice(&[1, 2, 3, 4]);
        src[8..12].copy_from_slice(&[5, 6, 7, 8]);
        src[16..20].copy_from_slice(&[9, 10, 11, 12]);
        let mut out = Vec::new();
        copy_nv12(&src, pitch, 16, (4, 2), &mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }
}
