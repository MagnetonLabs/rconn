//! DXGI Desktop Duplication capture.
//!
//! Unsafe is confined to this module: it is COM/FFI into DXGI and D3D11. windows-rs wraps
//! every interface in a ref-counted RAII type, results are checked, and the only raw
//! memory access is the mapped staging texture, read strictly within `RowPitch * Height`.
#![allow(unsafe_code)]

use super::{CaptureError, Capturer};
use crate::video::Frame;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

/// Attach the calling thread to the desktop currently receiving input: the user's
/// desktop normally, Winlogon at the lock/login screen or during UAC. Only a process
/// with access to that desktop (SYSTEM in the console session) can open Winlogon; for a
/// normal user process this fails at the lock screen and capture waits.
fn follow_input_desktop() -> Result<(), CaptureError> {
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    const GENERIC_ALL: u32 = 0x1000_0000;
    unsafe {
        let desk = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
        )
        .map_err(|e| {
            CaptureError::Backend(format!(
                "OpenInputDesktop: {e} (no access to the active desktop)"
            ))
        })?;
        // The previous desktop handle (if any) is deliberately leaked: closing a desktop
        // still assigned to a thread fails, and switches are rare.
        SetThreadDesktop(desk)
            .map_err(|e| CaptureError::Backend(format!("SetThreadDesktop: {e}")))?;
    }
    Ok(())
}

fn backend(e: windows::core::Error) -> CaptureError {
    CaptureError::Backend(format!("{e} ({:#x})", e.code().0))
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Copy the next acquired image even if DXGI marks it unchanged: right after
    /// (re)duplication it is the whole current desktop, and an idle screen sends no other.
    want_full: bool,
    /// When this capturer started (for the no-first-frame fallback).
    started: Instant,
}

/// With no first image from DXGI after this long, grab one through GDI instead.
const FIRST_FRAME_FALLBACK: Duration = Duration::from_millis(500);

// The COM objects are only used from the thread that owns the capturer at a time.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    /// Capture output `index` (0 = primary) of the first adapter that has it.
    pub fn new(index: u32) -> Result<Self, CaptureError> {
        // Bind this (fresh) thread to the active desktop before any D3D object exists:
        // afterwards the thread may own hidden windows and can no longer switch.
        follow_input_desktop()?;
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(backend)?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let Ok(output) = adapter.EnumOutputs(index) else {
                    continue;
                };
                let adapter: IDXGIAdapter = adapter.cast().map_err(backend)?;
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .map_err(backend)?;
                let (Some(device), Some(context)) = (device, context) else {
                    return Err(CaptureError::Backend(
                        "D3D11CreateDevice returned no device".into(),
                    ));
                };
                return Ok(Self {
                    device,
                    context,
                    output: output.cast().map_err(backend)?,
                    dup: None,
                    staging: None,
                    want_full: true,
                    started: Instant::now(),
                });
            }
            Err(CaptureError::NoOutput)
        }
    }

    fn duplication(&mut self) -> Result<&IDXGIOutputDuplication, CaptureError> {
        if self.dup.is_none() {
            if let Err(e) = follow_input_desktop() {
                tracing::debug!(error = %e, "re-attach to input desktop failed; trying duplication anyway");
            }
            // Fails with E_ACCESSDENIED outside an interactive desktop (session 0, or the
            // secure desktop until phase 5's desktop switching).
            self.dup = Some(unsafe { self.output.DuplicateOutput(&self.device) }.map_err(backend)?);
            self.want_full = true;
        }
        Ok(self.dup.as_ref().expect("set above"))
    }

    fn staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D, CaptureError> {
        if let Some((tex, w, h)) = &self.staging
            && *w == desc.Width
            && *h == desc.Height
        {
            return Ok(tex.clone());
        }
        let sd = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&sd, None, Some(&mut tex)) }.map_err(backend)?;
        let tex = tex.ok_or_else(|| CaptureError::Backend("no staging texture".into()))?;
        self.staging = Some((tex.clone(), desc.Width, desc.Height));
        Ok(tex)
    }

    fn copy_out(&mut self, resource: IDXGIResource) -> Result<Frame, CaptureError> {
        let tex: ID3D11Texture2D = resource.cast().map_err(backend)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::UnsupportedFormat(desc.Format.0 as u32));
        }
        let staging = self.staging(&desc)?;
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        let mut bgra = vec![0u8; w * h * 4];
        unsafe {
            self.context.CopyResource(&staging, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(backend)?;
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h);
            for (y, row) in bgra.chunks_exact_mut(w * 4).enumerate() {
                row.copy_from_slice(&src[y * pitch..y * pitch + w * 4]);
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        })
    }
}

impl DxgiCapturer {
    /// The whole output read through GDI (works when DXGI has no new image to give).
    fn gdi_snapshot(&self) -> Result<Frame, CaptureError> {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SRCCOPY, SelectObject,
        };
        let desc = unsafe { self.output.GetDesc() }.map_err(backend)?;
        let r = desc.DesktopCoordinates;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return Err(CaptureError::Backend("output has no size".into()));
        }
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp.into());
            let copied = BitBlt(
                mem,
                0,
                0,
                w,
                h,
                Some(screen),
                r.left,
                r.top,
                SRCCOPY | CAPTUREBLT,
            );
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let lines = GetDIBits(
                mem,
                bmp,
                0,
                h as u32,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            copied.map_err(backend)?;
            if lines == 0 {
                return Err(CaptureError::Backend("GetDIBits copied nothing".into()));
            }
        }
        for px in bgra.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(Frame {
            width: w as usize,
            height: h as usize,
            bgra,
            captured_at: Instant::now(),
        })
    }
}

impl Capturer for DxgiCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let dup = self.duplication()?.clone();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // Nothing presented yet (idle desktop; a VM whose window is hidden on the
                // host Mac presents nothing at all): read the desktop directly once, so a
                // new viewer always gets a picture.
                if self.want_full && self.started.elapsed() > FIRST_FRAME_FALLBACK {
                    let frame = self.gdi_snapshot()?;
                    self.want_full = false;
                    tracing::info!(
                        w = frame.width,
                        h = frame.height,
                        "first picture taken via GDI (DXGI had none)"
                    );
                    return Ok(Some(frame));
                }
                return Ok(None);
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Desktop switch, resolution change, full-screen app: start over.
                tracing::debug!("desktop duplication lost; recreating");
                self.dup = None;
                return Ok(None);
            }
            Err(e) => return Err(backend(e)),
        }
        let result = match resource {
            // LastPresentTime == 0 means only the cursor moved, except for the first image
            // after duplication starts, which we always take.
            Some(r) if info.LastPresentTime != 0 || self.want_full => {
                let frame = self.copy_out(r);
                if frame.is_ok() {
                    self.want_full = false;
                }
                frame.map(Some)
            }
            _ => Ok(None),
        };
        let _ = unsafe { dup.ReleaseFrame() };
        result
    }
}
