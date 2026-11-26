//! Desktop Duplication with the picture kept on the GPU.
//!
//! [`DxgiCapturer::enable_gpu`] moves duplication onto a video-capable device
//! ([`GpuDevice`]); from then on [`DxgiCapturer::next_gpu`] copies what changed into a
//! desktop texture on that device (a GPU-to-GPU copy, no readback) for the hardware
//! encoder to convert and encode in place. The same Desktop Duplication session is used
//! as on the CPU path, so the lock screen, UAC and the secure desktop work the same.
//!
//! What DXGI reports per frame is kept: the moved areas ([`MoveRect`]) and the dirty
//! areas, in desktop pixels ([`DxgiCapturer::last_moves`], [`DxgiCapturer::last_dirty`]).
//!
//! Unsafe: COM calls into DXGI/D3D11 (as in the parent module), and uploading the GDI
//! fallback picture from a buffer of exactly `width * height * 4` bytes.

use super::{CaptureError, Captured, DxgiCapturer, FIRST_FRAME_FALLBACK, Held, backend};
use crate::d3d11::{Converter, GpuDevice, GpuFrame, fitted};
use crate::yuv::Rect;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{DXGI_OUTDUPL_MOVE_RECT, IDXGIDevice, IDXGIResource};
use windows::core::Interface;

/// An area DXGI reports as moved (scrolled, a window dragged): the pixels now at `to`
/// were at `from` (top-left) in the previous picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveRect {
    pub from: (usize, usize),
    pub to: Rect,
}

pub(super) struct GpuState {
    gpu: GpuDevice,
    /// The capturer's own device, put back by [`DxgiCapturer::disable_gpu`].
    cpu: (ID3D11Device, ID3D11DeviceContext),
    /// The current desktop picture.
    frame: Option<GpuFrame>,
    /// The size it is sent at (within `max`).
    fitted: (usize, usize),
    moves: Vec<MoveRect>,
    dirty: Vec<Rect>,
}

fn gpu_backend(e: String) -> CaptureError {
    CaptureError::Backend(format!("D3D11: {e}"))
}

impl DxgiCapturer {
    /// Keep frames on the GPU from now on: duplication moves to a video-capable device on
    /// the same adapter, whose video processor is checked to convert this desktop (sent
    /// within `max`). Returns that device, for the encoder. On error nothing changes.
    pub fn enable_gpu(&mut self, max: (usize, usize)) -> Result<GpuDevice, CaptureError> {
        if let Some(s) = &self.gpu {
            return Ok(s.gpu.clone());
        }
        let adapter = unsafe {
            self.device
                .cast::<IDXGIDevice>()
                .and_then(|d| d.GetAdapter())
                .map_err(backend)?
        };
        let gpu = GpuDevice::new(&adapter).map_err(gpu_backend)?;
        let (w, h) = self.output_size()?;
        let out = fitted(w, h, max);
        Converter::new(&gpu, (w as u32, h as u32), (out.0 as u32, out.1 as u32))
            .map_err(gpu_backend)?;
        let cpu = (
            std::mem::replace(&mut self.device, gpu.device.clone()),
            std::mem::replace(&mut self.context, gpu.context.clone()),
        );
        self.dup = None;
        self.staging = None;
        self.want_full = true;
        self.gpu = Some(GpuState {
            gpu: gpu.clone(),
            cpu,
            frame: None,
            fitted: out,
            moves: Vec::new(),
            dirty: Vec::new(),
        });
        Ok(gpu)
    }

    /// Check that the GPU's video processor writes the colours viewers expect for this
    /// desktop sent within `max` (see [`Converter::check_colours`]).
    pub fn check_gpu_colours(&self, max: (usize, usize)) -> Result<(), CaptureError> {
        let s = self
            .gpu
            .as_ref()
            .ok_or_else(|| CaptureError::Backend("GPU capture is not enabled".into()))?;
        let (w, h) = self.output_size()?;
        let out = fitted(w, h, max);
        Converter::new(&s.gpu, (w as u32, h as u32), (out.0 as u32, out.1 as u32))
            .and_then(|mut c| c.check_colours())
            .map_err(gpu_backend)
    }

    /// Back to the CPU path (the capturer's own device), e.g. when no encoder takes
    /// textures.
    pub fn disable_gpu(&mut self) {
        if let Some(s) = self.gpu.take() {
            (self.device, self.context) = s.cpu;
            self.dup = None;
            self.staging = None;
            self.want_full = true;
        }
    }

    /// Is the picture kept on the GPU?
    pub fn gpu_enabled(&self) -> bool {
        self.gpu.is_some()
    }

    /// The current desktop picture on the GPU (after [`Self::next_gpu`] produced one).
    pub fn gpu_frame(&self) -> Option<&GpuFrame> {
        self.gpu.as_ref()?.frame.as_ref()
    }

    /// The size the picture is sent at: the desktop halved to fit `max` (as on the CPU
    /// path), even.
    pub fn gpu_dimensions(&self) -> Option<(usize, usize)> {
        let s = self.gpu.as_ref()?;
        s.frame.as_ref().map(|_| s.fitted)
    }

    /// Areas DXGI reported as moved in the last frame [`Self::next_gpu`] took (desktop
    /// pixels; empty after a full copy).
    pub fn last_moves(&self) -> &[MoveRect] {
        self.gpu.as_ref().map_or(&[], |s| &s.moves)
    }

    /// Areas DXGI reported as changed (not moved) in the last frame taken.
    pub fn last_dirty(&self) -> &[Rect] {
        self.gpu.as_ref().map_or(&[], |s| &s.dirty)
    }

    /// The desktop's size in pixels.
    pub fn output_size(&self) -> Result<(usize, usize), CaptureError> {
        let r = unsafe { self.output.GetDesc() }
            .map_err(backend)?
            .DesktopCoordinates;
        let (w, h) = ((r.right - r.left).max(0), (r.bottom - r.top).max(0));
        if w == 0 || h == 0 {
            return Err(CaptureError::Backend("output has no size".into()));
        }
        Ok((w as usize, h as usize))
    }

    /// The GPU path's `next_into`: wait up to `timeout` for a change and copy it into the
    /// kept desktop texture. `Ok(None)` means nothing changed. `Captured::rows` are in
    /// the sent (fitted) picture's rows.
    pub fn next_gpu(
        &mut self,
        timeout: Duration,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        if self.gpu.is_none() {
            return Err(CaptureError::Backend("GPU capture is not enabled".into()));
        }
        let result = match self.hold(timeout)? {
            Some(held) => match self.image(&held) {
                Some(image) => self.apply_gpu(&held, image, max),
                None => Ok(None),
            },
            None if self.want_full && self.started.elapsed() > FIRST_FRAME_FALLBACK => {
                // No first image from DXGI (idle desktop, a hidden VM window): read the
                // desktop through GDI once and upload it.
                let frame = self.gdi_snapshot()?;
                tracing::info!(
                    w = frame.width,
                    h = frame.height,
                    "first picture taken via GDI (DXGI had none)"
                );
                let (w, h) = (frame.width, frame.height);
                let tex = self.desktop_texture(w, h)?;
                unsafe {
                    self.context.UpdateSubresource(
                        &tex,
                        0,
                        None,
                        frame.bgra.as_ptr().cast(),
                        (w * 4) as u32,
                        0,
                    );
                }
                let s = self.gpu.as_mut().expect("checked above");
                s.fitted = fitted(w, h, max);
                s.moves.clear();
                s.dirty.clear();
                Ok(Some(Captured {
                    rows: (0, s.fitted.1),
                    changed: 1.0,
                    captured_at: frame.captured_at,
                }))
            }
            None => Ok(None),
        };
        if matches!(result, Ok(Some(_))) {
            self.want_full = false;
        }
        result
    }

    /// The kept desktop texture at `w` x `h` (made anew after a size change).
    fn desktop_texture(&mut self, w: usize, h: usize) -> Result<ID3D11Texture2D, CaptureError> {
        let s = self.gpu.as_mut().expect("GPU capture enabled");
        if let Some(f) = &s.frame
            && (f.width, f.height) == (w, h)
        {
            return Ok(f.texture.clone());
        }
        let texture = s
            .gpu
            .bgra_texture((w as u32, h as u32), None)
            .map_err(gpu_backend)?;
        s.frame = Some(GpuFrame {
            texture: texture.clone(),
            width: w,
            height: h,
        });
        Ok(texture)
    }

    /// Copy what changed in `image` into the kept desktop texture.
    fn apply_gpu(
        &mut self,
        held: &Held,
        image: IDXGIResource,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        let captured_at = Instant::now();
        let tex: ID3D11Texture2D = image.cast().map_err(backend)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::UnsupportedFormat(desc.Format.0 as u32));
        }
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        if w < 2 || h < 2 {
            return Ok(None);
        }
        let resized = self
            .gpu
            .as_ref()
            .and_then(|s| s.frame.as_ref())
            .is_none_or(|f| (f.width, f.height) != (w, h));
        let fresh = self.want_full || resized;
        let kept = self.desktop_texture(w, h)?;
        let whole = Rect { x: 0, y: 0, w, h };
        let metadata = if fresh {
            None
        } else {
            frame_metadata(held, w, h)
        };
        let s = self.gpu.as_mut().expect("GPU capture enabled");
        s.fitted = fitted(w, h, max);
        let rects: Vec<Rect> = match &metadata {
            Some((moves, dirty)) => moves
                .iter()
                .map(|m| m.to)
                .chain(dirty.iter().copied())
                .collect(),
            None => vec![whole],
        };
        (s.moves, s.dirty) = metadata.unwrap_or_default();
        if rects.is_empty() {
            return Ok(None);
        }
        unsafe {
            if rects == [whole] {
                self.context.CopyResource(&kept, &tex);
            } else {
                // The acquired image is the whole current desktop: copying the moved
                // areas' destinations and the dirty areas brings the kept one up to date.
                for r in &rects {
                    let b = D3D11_BOX {
                        left: r.x as u32,
                        top: r.y as u32,
                        front: 0,
                        right: (r.x + r.w) as u32,
                        bottom: (r.y + r.h) as u32,
                        back: 1,
                    };
                    self.context.CopySubresourceRegion(
                        &kept,
                        0,
                        r.x as u32,
                        r.y as u32,
                        0,
                        &tex,
                        0,
                        Some(&b),
                    );
                }
            }
        }
        let area: usize = rects.iter().map(|r| r.w * r.h).sum();
        let top = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap_or(h);
        let fh = s.fitted.1;
        let scale = |y: usize| (y * fh).div_ceil(h).min(fh);
        Ok(Some(Captured {
            rows: (
                (top * fh / h) & !1,
                scale(bottom).next_multiple_of(2).min(fh),
            ),
            changed: (area as f32 / (w * h) as f32).min(1.0),
            captured_at,
        }))
    }
}

/// The moved and dirty areas DXGI reports for `held`, clipped to `w` x `h`; `None` if it
/// can't say (then everything counts as changed).
fn frame_metadata(held: &Held, w: usize, h: usize) -> Option<(Vec<MoveRect>, Vec<Rect>)> {
    let size = held.info.TotalMetadataBufferSize as usize;
    if size == 0 {
        return None;
    }
    let clip = |r: RECT| {
        let (x0, y0) = (r.left.max(0) as usize, r.top.max(0) as usize);
        let (x1, y1) = (
            (r.right.max(0) as usize).min(w),
            (r.bottom.max(0) as usize).min(h),
        );
        (x1 > x0 && y1 > y0).then_some(Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    };
    let move_size = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
    let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); size / move_size + 1];
    let mut used = 0u32;
    unsafe {
        held.dup.GetFrameMoveRects(
            (moves.len() * move_size) as u32,
            moves.as_mut_ptr(),
            &mut used,
        )
    }
    .ok()?;
    moves.truncate(used as usize / move_size);
    let moves = moves
        .iter()
        .filter_map(|m| {
            Some(MoveRect {
                from: (
                    m.SourcePoint.x.max(0) as usize,
                    m.SourcePoint.y.max(0) as usize,
                ),
                to: clip(m.DestinationRect)?,
            })
        })
        .collect();
    let rect_size = std::mem::size_of::<RECT>();
    let mut dirty = vec![RECT::default(); size / rect_size + 1];
    unsafe {
        held.dup.GetFrameDirtyRects(
            (dirty.len() * rect_size) as u32,
            dirty.as_mut_ptr(),
            &mut used,
        )
    }
    .ok()?;
    dirty.truncate(used as usize / rect_size);
    Some((moves, dirty.into_iter().filter_map(clip).collect()))
}
