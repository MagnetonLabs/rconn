//! Direct3D 11 pieces of the GPU-resident Windows pipeline: a video-capable device, the
//! GPU's video processor turning the captured BGRA desktop into NV12 (scaled on the way:
//! fitting `max_size`, half size during motion), and NV12 textures for the encoder.
//!
//! Capture ([`crate::capture::dxgi`]) keeps the desktop in a texture on this device,
//! [`Converter`] writes NV12 next to it, and the hardware encoder ([`crate::mf`]) reads
//! that texture through the same device: no frame crosses to the CPU.
//!
//! Unsafe is confined to this module: COM calls into D3D11, and reading a mapped staging
//! texture strictly within `RowPitch * rows` (readback, used by the self-test).
#![allow(unsafe_code)]

use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VIDEO_ENCODER, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Query, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::core::Interface;

/// A D3D11 failure, as text (callers wrap it in their own error type).
pub type GpuError = String;

fn e(what: &str) -> impl Fn(windows::core::Error) -> GpuError + '_ {
    move |err| format!("{what}: {err} ({:#x})", err.code().0)
}

/// A D3D11 device that can run the video processor and feed a hardware encoder. Clones
/// share the device; the immediate context is used by one thread at a time (the stream
/// thread), with multithread protection on for the encoder's own threads.
#[derive(Clone)]
pub struct GpuDevice {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    /// PCI vendor of the adapter (0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel, 0x4D4F4351
    /// Qualcomm): hardware encoders of the same vendor are tried first.
    pub vendor_id: u32,
    pub adapter: String,
}

// SAFETY: D3D11 devices are free-threaded; the immediate context is only used by the
// thread that currently owns the capturer/encoder (they move together, never shared), and
// multithread protection serialises the encoder's internal use of it.
unsafe impl Send for GpuDevice {}

impl GpuDevice {
    /// A device on `adapter` with video support (fails on adapters without a video
    /// processor, e.g. some basic display drivers).
    pub fn new(adapter: &IDXGIAdapter) -> Result<Self, GpuError> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(e("D3D11CreateDevice (video)"))?;
        }
        let (Some(device), Some(context)) = (device, context) else {
            return Err("D3D11CreateDevice returned no device".into());
        };
        // The encoder MFT uses this device from its own threads.
        let mt: ID3D11Multithread = context.cast().map_err(e("ID3D11Multithread"))?;
        let _ = unsafe { mt.SetMultithreadProtected(true) };
        let desc = unsafe { adapter.GetDesc() }.map_err(e("adapter description"))?;
        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Ok(Self {
            device,
            context,
            vendor_id: desc.VendorId,
            adapter: String::from_utf16_lossy(&desc.Description[..name_len]),
        })
    }

    /// Wait until the GPU has finished everything queued so far (timing, readback).
    pub fn finish(&self) -> Result<(), GpuError> {
        let mut query: Option<ID3D11Query> = None;
        unsafe {
            self.device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .map_err(e("CreateQuery"))?;
            let query = query.ok_or("no query")?;
            self.context.End(&query);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let mut done = windows::core::BOOL(0);
                let r = self.context.GetData(
                    &query,
                    Some((&raw mut done).cast()),
                    std::mem::size_of::<windows::core::BOOL>() as u32,
                    0,
                );
                if r.is_ok() && done.as_bool() {
                    return Ok(());
                }
                if std::time::Instant::now() > deadline {
                    return Err("GPU did not finish within 2 s".into());
                }
                std::thread::yield_now();
            }
        }
    }

    fn texture(
        &self,
        (w, h): (u32, u32),
        format: DXGI_FORMAT,
        bind: D3D11_BIND_FLAG,
        init: Option<&[u8]>,
    ) -> Result<ID3D11Texture2D, GpuError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let pitch = if format == DXGI_FORMAT_NV12 { w } else { w * 4 };
        let need = if format == DXGI_FORMAT_NV12 {
            (w * h * 3 / 2) as usize
        } else {
            (w * h * 4) as usize
        };
        if init.is_some_and(|d| d.len() < need) {
            return Err(format!("initial data too small for {w}x{h}"));
        }
        let data = init.map(|d| D3D11_SUBRESOURCE_DATA {
            pSysMem: d.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        });
        let mut tex = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, data.as_ref().map(|d| d as *const _), Some(&mut tex))
        }
        .map_err(e("CreateTexture2D"))?;
        tex.ok_or_else(|| "CreateTexture2D returned nothing".into())
    }

    /// A BGRA texture the video processor can read (the kept desktop picture).
    /// `init` (if any) is `w * h * 4` bytes.
    pub fn bgra_texture(
        &self,
        size: (u32, u32),
        init: Option<&[u8]>,
    ) -> Result<ID3D11Texture2D, GpuError> {
        self.texture(
            size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            init,
        )
    }

    /// An NV12 texture the video processor writes and an encoder reads. `init` (if any)
    /// is `w * h * 3 / 2` bytes: the Y plane, then interleaved U/V.
    pub fn nv12_texture(
        &self,
        size: (u32, u32),
        init: Option<&[u8]>,
    ) -> Result<ID3D11Texture2D, GpuError> {
        // Encoders read "video encoder" textures directly; not every driver allows the
        // flag together with render-target use, so it is optional.
        self.texture(
            size,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER,
            init,
        )
        .or_else(|_| self.texture(size, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET, init))
    }
}

/// A picture kept on the GPU: the captured desktop (BGRA), `width` x `height`.
#[derive(Clone)]
pub struct GpuFrame {
    pub texture: ID3D11Texture2D,
    pub width: usize,
    pub height: usize,
}

// SAFETY: a texture reference; used by the thread that owns the capturer and encoder.
unsafe impl Send for GpuFrame {}

/// BT.601 limited range, the same colours as the CPU conversion ([`crate::yuv`]).
const RGB_FULL: D3D11_VIDEO_PROCESSOR_COLOR_SPACE = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    // Usage 0 (playback), RGB_Range 0 (0-255).
    _bitfield: 0,
};
const YUV_601_STUDIO: D3D11_VIDEO_PROCESSOR_COLOR_SPACE = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    // YCbCr_Matrix 0 (BT.601), Nominal_Range (bits 4-5) 1 = 16-235.
    _bitfield: 1 << 4,
};

/// The GPU's video processor, converting (and scaling) a BGRA texture of one size into
/// NV12 textures of another.
pub struct Converter {
    gpu: GpuDevice,
    video: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
    /// Views of the textures last used (the kept desktop; the encoder's NV12 ring).
    in_view: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    out_views: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

// SAFETY: as for `GpuDevice`: owned and used by one thread at a time.
unsafe impl Send for Converter {}

impl Converter {
    /// Converts `input`-sized BGRA into `output`-sized NV12 (both even, non-zero). The
    /// top-left even part of an odd-sized input is used, as the CPU path does.
    pub fn new(gpu: &GpuDevice, input: (u32, u32), output: (u32, u32)) -> Result<Self, GpuError> {
        if output.0 == 0 || output.1 == 0 || output.0 % 2 == 1 || output.1 % 2 == 1 {
            return Err(format!("bad output size {output:?}"));
        }
        let video: ID3D11VideoDevice = gpu.device.cast().map_err(e("ID3D11VideoDevice"))?;
        let vctx: ID3D11VideoContext = gpu.context.cast().map_err(e("ID3D11VideoContext"))?;
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: rate,
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        unsafe {
            let enumerator = video
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(e("CreateVideoProcessorEnumerator"))?;
            let supports = |f: DXGI_FORMAT, what: i32| {
                enumerator
                    .CheckVideoProcessorFormat(f)
                    .is_ok_and(|s| s as i32 & what != 0)
            };
            if !supports(
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0,
            ) || !supports(
                DXGI_FORMAT_NV12,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0,
            ) {
                return Err("video processor can't convert BGRA to NV12".into());
            }
            let processor = video
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(e("CreateVideoProcessor"))?;
            vctx.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &RGB_FULL);
            vctx.VideoProcessorSetOutputColorSpace(&processor, &YUV_601_STUDIO);
            // Some drivers only honour the newer colour-space calls (and otherwise pick
            // BT.709 for HD sizes: measured on Parallels' adapter). Viewers decode BT.601.
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                v1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                );
            }
            // No driver "enhancements" (denoise, edge enhancement) on screen content.
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let src = RECT {
                left: 0,
                top: 0,
                right: (input.0 & !1) as i32,
                bottom: (input.1 & !1) as i32,
            };
            let dst = RECT {
                left: 0,
                top: 0,
                right: output.0 as i32,
                bottom: output.1 as i32,
            };
            vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            Ok(Self {
                gpu: gpu.clone(),
                video,
                vctx,
                enumerator,
                processor,
                input,
                output,
                in_view: None,
                out_views: Vec::new(),
            })
        }
    }

    pub fn input(&self) -> (u32, u32) {
        self.input
    }

    pub fn output(&self) -> (u32, u32) {
        self.output
    }

    /// Queue the conversion of `src` (BGRA, input size) into `dst` (NV12, output size).
    pub fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
    ) -> Result<(), GpuError> {
        unsafe {
            if self.in_view.as_ref().is_none_or(|(t, _)| t != src) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut view = None;
                self.video
                    .CreateVideoProcessorInputView(src, &self.enumerator, &desc, Some(&mut view))
                    .map_err(e("CreateVideoProcessorInputView"))?;
                self.in_view = Some((src.clone(), view.ok_or("no input view")?));
            }
            let out = match self.out_views.iter().position(|(t, _)| t == dst) {
                Some(i) => self.out_views[i].1.clone(),
                None => {
                    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    };
                    let mut view = None;
                    self.video
                        .CreateVideoProcessorOutputView(
                            dst,
                            &self.enumerator,
                            &desc,
                            Some(&mut view),
                        )
                        .map_err(e("CreateVideoProcessorOutputView"))?;
                    let view = view.ok_or("no output view")?;
                    if self.out_views.len() >= 8 {
                        self.out_views.remove(0);
                    }
                    self.out_views.push((dst.clone(), view.clone()));
                    view
                }
            };
            let input = self.in_view.as_ref().map(|(_, v)| v.clone());
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input),
                ..Default::default()
            };
            let r = self.vctx.VideoProcessorBlt(
                &self.processor,
                &out,
                0,
                std::slice::from_ref(&stream),
            );
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            r.map_err(e("VideoProcessorBlt"))
        }
    }

    pub fn device(&self) -> &GpuDevice {
        &self.gpu
    }

    /// Check that the video processor really writes BT.601 limited range, the colours
    /// viewers decode. Some drivers ignore the colour-space calls and pick BT.709 for
    /// HD sizes (Parallels' adapter does): saturated colours would come out wrong, so
    /// such a processor must not be used. Converts colour bars once (a full-size upload
    /// and readback).
    pub fn check_colours(&mut self) -> Result<(), GpuError> {
        const BARS: [[u8; 3]; 8] = [
            [191, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
            [16, 16, 16],
        ];
        let (iw, ih) = (self.input.0 as usize, self.input.1 as usize);
        let (ow, oh) = (self.output.0 as usize, self.output.1 as usize);
        let mut bgra = vec![0u8; iw * ih * 4];
        for row in bgra.chunks_exact_mut(iw * 4) {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let [r, g, b] = BARS[x * 8 / iw];
                *px = [b, g, r, 255];
            }
        }
        let gpu = self.gpu.clone();
        let src = gpu.bgra_texture(self.input, Some(&bgra))?;
        let dst = gpu.nv12_texture(self.output, None)?;
        self.convert(&src, &dst)?;
        self.in_view = None; // don't keep the test texture alive
        let nv12 = Readback::new().read(&gpu, &dst)?;
        for (i, [r, g, b]) in BARS.into_iter().enumerate() {
            let mut want = crate::yuv::I420::new(2, 2);
            want.fill_from_bgra(&[b, g, r, 255].repeat(4), 2);
            let want = want.to_nv12(); // Y Y Y Y U V
            let (x, y) = (((2 * i + 1) * ow / 16) & !1, (oh / 2) & !1);
            let got = [
                nv12[y * ow + x],
                nv12[ow * oh + (y / 2) * ow + x],
                nv12[ow * oh + (y / 2) * ow + x + 1],
            ];
            let want = [want[0], want[4], want[5]];
            if got.iter().zip(want).any(|(g, w)| g.abs_diff(w) > 4) {
                return Err(format!(
                    "video processor colours are not BT.601 limited range (RGB {:?}: YUV {got:?}, want {want:?})",
                    [r, g, b]
                ));
            }
        }
        Ok(())
    }
}

/// Copies GPU textures back to memory: checks and the self-test only (the GPU path
/// itself never reads pictures back).
pub struct Readback {
    staging: Option<(ID3D11Texture2D, u32, u32, DXGI_FORMAT)>,
}

// SAFETY: as for `GpuDevice`.
unsafe impl Send for Readback {}

impl Default for Readback {
    fn default() -> Self {
        Self::new()
    }
}

impl Readback {
    pub fn new() -> Self {
        Self { staging: None }
    }

    /// The pixels of `tex` (BGRA: 4 bytes per pixel; NV12: Y plane then UV), tightly
    /// packed.
    pub fn read(&mut self, gpu: &GpuDevice, tex: &ID3D11Texture2D) -> Result<Vec<u8>, GpuError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h, f) = (desc.Width, desc.Height, desc.Format);
        let staging = match &self.staging {
            Some((t, sw, sh, sf)) if (*sw, *sh, *sf) == (w, h, f) => t.clone(),
            _ => {
                let sd = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut t = None;
                unsafe { gpu.device.CreateTexture2D(&sd, None, Some(&mut t)) }
                    .map_err(e("staging texture"))?;
                let t = t.ok_or("no staging texture")?;
                self.staging = Some((t.clone(), w, h, f));
                t
            }
        };
        let (row, rows) = if f == DXGI_FORMAT_NV12 {
            (w as usize, h as usize * 3 / 2)
        } else {
            (w as usize * 4, h as usize)
        };
        let mut out = vec![0u8; row * rows];
        unsafe {
            gpu.context.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(e("Map"))?;
            let pitch = mapped.RowPitch as usize;
            // NV12: the UV plane follows the Y plane at `pitch * height`.
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows);
            for (y, dst) in out.chunks_exact_mut(row).enumerate() {
                dst.copy_from_slice(&src[y * pitch..y * pitch + row]);
            }
            gpu.context.Unmap(&staging, 0);
        }
        Ok(out)
    }
}

/// The size a `w` x `h` desktop is sent at within `max`: halved until it fits (as
/// [`crate::video::Frame::fit_within`]), rounded down to even.
pub fn fitted(w: usize, h: usize, max: (usize, usize)) -> (usize, usize) {
    let (mut w, mut h) = (w, h);
    while (w > max.0 || h > max.1) && w >= 4 && h >= 4 {
        w /= 2;
        h /= 2;
    }
    (w & !1, h & !1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fitted_matches_frame_halving() {
        assert_eq!(super::fitted(2992, 1878, (1920, 1080)), (1496, 938));
        assert_eq!(super::fitted(1921, 1081, (3840, 2400)), (1920, 1080));
        assert_eq!(super::fitted(7680, 4320, (3840, 2400)), (3840, 2160));
    }
}
