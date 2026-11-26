//! Media Foundation encoders fed with D3D11 textures: the captured desktop is converted
//! to NV12 by the GPU's video processor ([`crate::d3d11::Converter`]) and handed to a
//! D3D11-aware hardware encoder on the same device, so no picture crosses to the CPU.
//!
//! Unsafe: COM calls into Media Foundation (as in the parent module).

use super::{MfH264Encoder, err};
use crate::codec::CodecError;
use crate::d3d11::{Converter, GpuDevice, GpuFrame};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFDXGIDeviceManager, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFSTARTUP_FULL, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::Interface;

/// NV12 textures in flight: the encoder may still read one while the next is written.
const RING: usize = 4;

pub(super) struct GpuInput {
    gpu: GpuDevice,
    /// Kept alive for the encoder, which holds the device through it.
    _manager: IMFDXGIDeviceManager,
    converter: Option<Converter>,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
}

fn gpu_err(e: String) -> CodecError {
    CodecError::OpenH264(format!("D3D11: {e}"))
}

impl MfH264Encoder {
    /// A low-latency H.264 encoder for `width` x `height` reading D3D11 textures on
    /// `gpu`: the first D3D11-aware hardware encoder (the adapter's vendor first) that
    /// starts and passes a self-test. With `software` (tests, and machines without one:
    /// `RCONN_TEXTURE_ENCODER=software`), Microsoft's software encoder is tried last: it
    /// is not D3D11-aware, so Media Foundation reads each texture back to memory for it.
    /// That exercises the whole texture path, but is no faster than the memory path.
    pub fn new_d3d11(
        gpu: &GpuDevice,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        software: bool,
    ) -> Result<Self, CodecError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err)?;
        }
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(err)?;
        }
        let manager =
            manager.ok_or_else(|| CodecError::OpenH264("no DXGI device manager".into()))?;
        unsafe { manager.ResetDevice(&gpu.device, token) }.map_err(err)?;

        let mut tried = Vec::new();
        let mut kinds = vec![true];
        if software {
            kinds.push(false);
        }
        for hardware in kinds {
            let vendor = hardware.then_some(gpu.vendor_id);
            for activate in Self::candidates(hardware, vendor).unwrap_or_default() {
                let started = Self::open(
                    activate,
                    hardware,
                    hardware.then_some(&manager),
                    width,
                    height,
                    fps,
                    bitrate_bps,
                )
                .and_then(|mut e| {
                    e.gpu = Some(Box::new(GpuInput {
                        gpu: gpu.clone(),
                        _manager: manager.clone(),
                        converter: None,
                        ring: Vec::new(),
                        next: 0,
                    }));
                    e.self_test_texture().map(|()| e)
                });
                match started {
                    Ok(e) => return Ok(e),
                    Err(e) => {
                        tracing::debug!(error = %e, "encoder can't take D3D11 textures");
                        tried.push(e.to_string());
                    }
                }
            }
        }
        Err(CodecError::OpenH264(if tried.is_empty() {
            format!("no GPU H.264 encoder (adapter {})", gpu.adapter)
        } else {
            format!(
                "no GPU H.264 encoder takes D3D11 textures on {}: {}",
                gpu.adapter,
                tried.join("; ")
            )
        }))
    }

    /// Does this encoder read D3D11 textures (see [`Self::new_d3d11`])?
    pub fn takes_textures(&self) -> bool {
        self.gpu.is_some()
    }

    /// Convert `frame` (the captured desktop, any size) to NV12 at the encoder's size on
    /// the GPU and encode it; returns the Annex-B output available so far.
    pub fn encode_frame(&mut self, frame: &GpuFrame) -> Result<Vec<u8>, CodecError> {
        let size = (self.width, self.height);
        let input = self
            .gpu
            .as_mut()
            .ok_or_else(|| CodecError::OpenH264("encoder does not take textures".into()))?;
        let src = (frame.width as u32, frame.height as u32);
        if input.converter.as_ref().is_none_or(|c| c.input() != src) {
            input.converter = Some(Converter::new(&input.gpu, src, size).map_err(gpu_err)?);
        }
        if input.ring.len() < RING {
            let t = input.gpu.nv12_texture(size, None).map_err(gpu_err)?;
            input.ring.push(t);
        }
        let dst = input.ring[input.next % input.ring.len()].clone();
        input.next = input.next.wrapping_add(1);
        input
            .converter
            .as_mut()
            .expect("set above")
            .convert(&frame.texture, &dst)
            .map_err(gpu_err)?;
        self.encode_texture(&dst)
    }

    /// Encode one NV12 texture of the encoder's size (on its device).
    pub fn encode_texture(&mut self, nv12: &ID3D11Texture2D) -> Result<Vec<u8>, CodecError> {
        let buffer = unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false).map_err(err)?;
            // Some encoders check the buffer's length, which a surface buffer starts
            // without.
            if let Ok(len) = buffer
                .cast::<IMF2DBuffer>()
                .and_then(|b| b.GetContiguousLength())
            {
                let _ = buffer.SetCurrentLength(len);
            }
            buffer
        };
        self.submit(&buffer)
    }

    /// A texture encoder must turn a few gray frames into output before we trust it.
    fn self_test_texture(&mut self) -> Result<(), CodecError> {
        let input = self.gpu.as_ref().expect("set by new_d3d11");
        let gray = vec![128u8; (self.width * self.height * 3 / 2) as usize];
        let tex = input
            .gpu
            .nv12_texture((self.width, self.height), Some(&gray))
            .map_err(gpu_err)?;
        for _ in 0..4 {
            if !self.encode_texture(&tex)?.is_empty() {
                self.force_keyframe(); // the viewer never saw these frames
                return Ok(());
            }
        }
        Err(CodecError::OpenH264(format!(
            "{} produced no output from textures in its self-test",
            self.name
        )))
    }
}
