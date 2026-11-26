//! The GPU's video processor (VA-API VPP): captured DMA-BUF screens are imported as VA
//! surfaces (`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`) and converted BGRx->NV12, BT.601
//! limited range (the colours of the CPU conversion, [`crate::yuv`]), scaled to the
//! encoder's size on the way.

use super::Display;
use super::ffi::*;
use crate::dmabuf::{DRM_ABGR8888, DRM_ARGB8888, DRM_XBGR8888, DRM_XRGB8888, DmaBuf};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

/// Imported screen buffers kept (a screen stream cycles through a handful).
const MAX_IMPORTS: usize = 16;

/// The VA fourcc for a DRM format: the same bytes in memory (XRGB8888 is B, G, R, X).
pub(super) fn va_fourcc(drm: u32) -> Option<u32> {
    Some(match drm {
        DRM_XRGB8888 => VA_FOURCC_BGRX,
        DRM_ARGB8888 => VA_FOURCC_BGRA,
        DRM_XBGR8888 => VA_FOURCC_RGBX,
        DRM_ABGR8888 => VA_FOURCC_RGBA,
        _ => return None,
    })
}

/// What identifies an imported buffer: the same memory with the same layout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    inodes: Vec<u64>,
    offsets: Vec<u32>,
    strides: Vec<u32>,
    drm_format: u32,
    modifier: u64,
    size: (u32, u32),
}

impl Key {
    fn of(f: &DmaBuf) -> Self {
        Self {
            inodes: f.planes.iter().map(|p| p.inode).collect(),
            offsets: f.planes.iter().map(|p| p.offset).collect(),
            strides: f.planes.iter().map(|p| p.stride).collect(),
            drm_format: f.drm_format,
            modifier: f.modifier,
            size: (f.width, f.height),
        }
    }
}

struct Import {
    key: Key,
    surface: VASurfaceID,
    /// The buffer stays ours while the surface uses it.
    _fds: Vec<Arc<OwnedFd>>,
}

/// The result of [`Vpp::check_colours`].
#[derive(Debug, Clone)]
pub struct ColourCheck {
    /// Mean absolute difference from the CPU conversion over the whole picture, Y and
    /// interleaved UV.
    pub y_mean: f64,
    pub uv_mean: f64,
    /// Largest difference at the centre of a colour bar.
    pub bar_max: u8,
    /// Why the video processor may not be used (colours off by more than 4 at a bar).
    pub refused: Option<String>,
}

pub(super) struct Vpp {
    d: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    imports: Vec<Import>,
}

// SAFETY: as for the encoder that owns it.
unsafe impl Send for Vpp {}

fn bars_bgrx(w: usize, h: usize) -> Vec<u8> {
    let mut bgrx = vec![0u8; w * h * 4];
    for row in bgrx.chunks_exact_mut(w * 4) {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let [r, g, b] = BARS[x * 8 / w];
            *px = [b, g, r, 255];
        }
    }
    bgrx
}

/// 75 % colour bars (white, yellow, cyan, green, magenta, red, blue, black).
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

/// Compare the video processor's NV12 (`got`, packed, `w` x `h`) of the bars with the
/// CPU conversion.
pub(super) fn compare_bars(got: &[u8], w: usize, h: usize) -> ColourCheck {
    let bgrx = bars_bgrx(w, h);
    let mut i420 = crate::yuv::I420::new(w, h);
    i420.fill_from_bgra(&bgrx, w);
    let want = i420.to_nv12();
    let mean = |a: &[u8], b: &[u8]| {
        let s: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        s as f64 / a.len().max(1) as f64
    };
    let n = w * h;
    let mut bar_max = 0u8;
    let mut refused = None;
    for (i, rgb) in BARS.iter().enumerate() {
        let (x, y) = (((2 * i + 1) * w / 16) & !1, (h / 2) & !1);
        let at = |p: &[u8]| {
            [
                p[y * w + x],
                p[n + (y / 2) * w + x],
                p[n + (y / 2) * w + x + 1],
            ]
        };
        let (g, wv) = (at(got), at(&want));
        let worst = g
            .iter()
            .zip(wv)
            .map(|(a, b)| a.abs_diff(b))
            .max()
            .unwrap_or(0);
        bar_max = bar_max.max(worst);
        if worst > 4 && refused.is_none() {
            refused = Some(format!(
                "video processor colours are not BT.601 limited range (RGB {rgb:?}: YUV {g:?}, want {wv:?})"
            ));
        }
    }
    ColourCheck {
        y_mean: mean(&got[..n], &want[..n]),
        uv_mean: mean(&got[n..], &want[n..]),
        bar_max,
        refused,
    }
}

impl Vpp {
    /// A video processor writing into `target` (`w` x `h`, NV12).
    pub fn new(d: Arc<Display>, target: VASurfaceID, w: u32, h: u32) -> Result<Self, String> {
        if !d.caps.vpp {
            return Err("the VA driver has no video processor".into());
        }
        let (va, dpy) = (d.va, d.dpy);
        let mut config = VA_INVALID_ID;
        let s = unsafe {
            (va.vaCreateConfig)(
                dpy,
                VAProfileNone,
                VAEntrypointVideoProc,
                std::ptr::null_mut(),
                0,
                &mut config,
            )
        };
        d.check(s, "VPP config")?;
        let mut v = Self {
            d: d.clone(),
            config,
            context: VA_INVALID_ID,
            imports: Vec::new(),
        };
        let mut targets = [target];
        let s = unsafe {
            (va.vaCreateContext)(
                dpy,
                config,
                w as i32,
                h as i32,
                VA_PROGRESSIVE,
                targets.as_mut_ptr(),
                1,
                &mut v.context,
            )
        };
        d.check(s, "VPP context")?;
        Ok(v)
    }

    /// The VA surface for screen buffer `f` (imported on first sight).
    pub fn import(&mut self, f: &DmaBuf) -> Result<VASurfaceID, String> {
        let key = Key::of(f);
        if let Some(i) = self.imports.iter().position(|i| i.key == key) {
            // Most recently used last: the oldest goes first when full.
            let import = self.imports.remove(i);
            let id = import.surface;
            self.imports.push(import);
            return Ok(id);
        }
        let fourcc = va_fourcc(f.drm_format)
            .ok_or_else(|| format!("screen format {:#010x} can't be imported", f.drm_format))?;
        if f.planes.is_empty() || f.planes.len() > 4 {
            return Err(format!("a screen buffer with {} planes", f.planes.len()));
        }
        let mut desc: VADRMPRIMESurfaceDescriptor = zeroed();
        desc.fourcc = fourcc;
        desc.width = f.width;
        desc.height = f.height;
        desc.num_objects = f.planes.len() as u32;
        desc.num_layers = 1;
        desc.layers[0].drm_format = f.drm_format;
        desc.layers[0].num_planes = f.planes.len() as u32;
        for (i, p) in f.planes.iter().enumerate() {
            desc.objects[i] = VADRMPRIMEObject {
                fd: p.fd.as_raw_fd(),
                size: p.size,
                drm_format_modifier: f.modifier,
            };
            desc.layers[0].object_index[i] = i as u32;
            desc.layers[0].offset[i] = p.offset;
            desc.layers[0].pitch[i] = p.stride;
        }
        let mut attribs = [
            VASurfaceAttrib::int(
                VASurfaceAttribMemoryType,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            ),
            VASurfaceAttrib::ptr(
                VASurfaceAttribExternalBufferDescriptor,
                (&mut desc as *mut VADRMPRIMESurfaceDescriptor).cast::<c_void>(),
            ),
        ];
        let mut id = VA_INVALID_SURFACE;
        // SAFETY: `desc` and its fds outlive the call; the driver takes its own reference
        // to the buffers.
        let s = unsafe {
            (self.d.va.vaCreateSurfaces)(
                self.d.dpy,
                VA_RT_FORMAT_RGB32,
                f.width,
                f.height,
                &mut id,
                1,
                attribs.as_mut_ptr(),
                attribs.len() as u32,
            )
        };
        self.d.check(s, "importing the screen buffer (DRM PRIME)")?;
        if self.imports.len() >= MAX_IMPORTS {
            let mut old = self.imports.remove(0);
            unsafe { (self.d.va.vaDestroySurfaces)(self.d.dpy, &mut old.surface, 1) };
        }
        tracing::debug!(
            w = f.width,
            h = f.height,
            format = format_args!("{:#010x}", f.drm_format),
            modifier = format_args!("{:#x}", f.modifier),
            stride = f.planes[0].stride,
            "imported a screen buffer into VA-API"
        );
        self.imports.push(Import {
            key,
            surface: id,
            _fds: f.planes.iter().map(|p| p.fd.clone()).collect(),
        });
        Ok(id)
    }

    /// Convert (and scale) `src` (RGB) into `dst` (NV12), BT.601 limited range.
    pub fn convert(&mut self, src: VASurfaceID, dst: VASurfaceID) -> Result<(), String> {
        let mut p: VAProcPipelineParameterBuffer = zeroed();
        p.surface = src;
        p.surface_color_standard = VAProcColorStandardSRGB;
        p.output_color_standard = VAProcColorStandardBT601;
        p.output_background_color = 0xff00_0000;
        p.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
        p.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;
        let (va, dpy, ctx) = (self.d.va, self.d.dpy, self.context);
        let bytes = bytes_of(&p);
        let mut buf = VA_INVALID_ID;
        let s = unsafe {
            (va.vaCreateBuffer)(
                dpy,
                ctx,
                VAProcPipelineParameterBufferType,
                bytes.len() as u32,
                1,
                bytes.as_ptr() as *mut c_void,
                &mut buf,
            )
        };
        self.d.check(s, "VPP parameters")?;
        let done = self
            .d
            .check(unsafe { (va.vaBeginPicture)(dpy, ctx, dst) }, "VPP begin")
            .and_then(|()| {
                let r = self.d.check(
                    unsafe { (va.vaRenderPicture)(dpy, ctx, &mut buf, 1) },
                    "VPP render",
                );
                let e = self
                    .d
                    .check(unsafe { (va.vaEndPicture)(dpy, ctx) }, "VPP end");
                r.and(e)
            });
        unsafe { (va.vaDestroyBuffer)(dpy, buf) };
        done
    }

    /// Convert colour bars into `dst` (`w` x `h`, NV12) and compare with the CPU
    /// conversion; `nv12` is an NV12 image of that size to read the result back with.
    pub fn check_colours(
        &mut self,
        dst: VASurfaceID,
        w: u32,
        h: u32,
        nv12: &VAImage,
    ) -> Result<ColourCheck, String> {
        let (va, dpy) = (self.d.va, self.d.dpy);
        let (wu, hu) = (w as usize, h as usize);
        // A BGRX source surface holding the bars.
        let format = self
            .rgb_image_format()
            .ok_or("the VA driver has no BGRX/BGRA image format")?;
        let mut attribs = [VASurfaceAttrib::int(
            VASurfaceAttribPixelFormat,
            format.fourcc as i32,
        )];
        let mut src = VA_INVALID_SURFACE;
        let s = unsafe {
            (va.vaCreateSurfaces)(
                dpy,
                VA_RT_FORMAT_RGB32,
                w,
                h,
                &mut src,
                1,
                attribs.as_mut_ptr(),
                1,
            )
        };
        self.d.check(s, "RGB test surface")?;
        let mut fmt = format;
        let mut image: VAImage = zeroed();
        let made = self.d.check(
            unsafe { (va.vaCreateImage)(dpy, &mut fmt, w as i32, h as i32, &mut image) },
            "RGB test image",
        );
        let made_ok = made.is_ok();
        let result = made.and_then(|()| {
            let bars = bars_bgrx(wu, hu);
            self.with_mapped(&image, |mem| {
                let (o, p) = (image.offsets[0] as usize, image.pitches[0] as usize);
                if p < wu * 4 || o + p * (hu - 1) + wu * 4 > mem.len() {
                    return Err("RGB image layout does not fit".to_string());
                }
                for (y, row) in bars.chunks_exact(wu * 4).enumerate() {
                    mem[o + y * p..o + y * p + wu * 4].copy_from_slice(row);
                }
                Ok(())
            })?;
            self.d.check(
                unsafe { (va.vaPutImage)(dpy, src, image.image_id, 0, 0, w, h, 0, 0, w, h) },
                "upload of the test bars",
            )?;
            self.convert(src, dst)?;
            self.d
                .check(unsafe { (va.vaSyncSurface)(dpy, dst) }, "VPP sync")?;
            self.d.check(
                unsafe { (va.vaGetImage)(dpy, dst, 0, 0, w, h, nv12.image_id) },
                "readback of the converted bars",
            )?;
            let mut packed = vec![0u8; wu * hu * 3 / 2];
            self.with_mapped(nv12, |mem| {
                let planes = [
                    (nv12.offsets[0] as usize, nv12.pitches[0] as usize, 0, hu),
                    (
                        nv12.offsets[1] as usize,
                        nv12.pitches[1] as usize,
                        wu * hu,
                        hu / 2,
                    ),
                ];
                for (o, p, at, rows) in planes {
                    if p < wu || o + p * (rows - 1) + wu > mem.len() {
                        return Err("NV12 image layout does not fit".to_string());
                    }
                    for y in 0..rows {
                        packed[at + y * wu..at + (y + 1) * wu]
                            .copy_from_slice(&mem[o + y * p..o + y * p + wu]);
                    }
                }
                Ok(())
            })?;
            Ok(compare_bars(&packed, wu, hu))
        });
        unsafe {
            if made_ok {
                (va.vaDestroyImage)(dpy, image.image_id);
            }
            (va.vaDestroySurfaces)(dpy, &mut src, 1);
        }
        result
    }

    /// Run `f` on the mapped bytes of `image`.
    fn with_mapped<R>(
        &self,
        image: &VAImage,
        f: impl FnOnce(&mut [u8]) -> Result<R, String>,
    ) -> Result<R, String> {
        let (va, dpy) = (self.d.va, self.d.dpy);
        let mut p: *mut c_void = std::ptr::null_mut();
        self.d.check(
            unsafe { (va.vaMapBuffer)(dpy, image.buf, &mut p) },
            "map image",
        )?;
        // SAFETY: the mapped image buffer holds `data_size` bytes until unmapped.
        let mem =
            unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), image.data_size as usize) };
        let r = f(mem);
        unsafe { (va.vaUnmapBuffer)(dpy, image.buf) };
        r
    }

    /// The driver's BGRX (else BGRA) image format.
    fn rgb_image_format(&self) -> Option<VAImageFormat> {
        let (va, dpy) = (self.d.va, self.d.dpy);
        let max = unsafe { (va.vaMaxNumImageFormats)(dpy) }.max(0) as usize;
        let mut formats = vec![zeroed::<VAImageFormat>(); max];
        let mut n = 0;
        let s = unsafe { (va.vaQueryImageFormats)(dpy, formats.as_mut_ptr(), &mut n) };
        if s != VA_STATUS_SUCCESS {
            return None;
        }
        formats.truncate((n.max(0) as usize).min(max));
        [VA_FOURCC_BGRX, VA_FOURCC_BGRA]
            .iter()
            .find_map(|&c| formats.iter().find(|f| f.fourcc == c).copied())
    }
}

impl Drop for Vpp {
    fn drop(&mut self) {
        let (va, dpy) = (self.d.va, self.d.dpy);
        unsafe {
            for mut i in self.imports.drain(..) {
                (va.vaDestroySurfaces)(dpy, &mut i.surface, 1);
            }
            if self.context != VA_INVALID_ID {
                (va.vaDestroyContext)(dpy, self.context);
            }
            if self.config != VA_INVALID_ID {
                (va.vaDestroyConfig)(dpy, self.config);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_formats_map_to_the_same_byte_order() {
        assert_eq!(va_fourcc(DRM_XRGB8888), Some(VA_FOURCC_BGRX));
        assert_eq!(va_fourcc(DRM_ARGB8888), Some(VA_FOURCC_BGRA));
        assert_eq!(va_fourcc(DRM_XBGR8888), Some(VA_FOURCC_RGBX));
        assert_eq!(va_fourcc(DRM_ABGR8888), Some(VA_FOURCC_RGBA));
        assert_eq!(va_fourcc(u32::from_le_bytes(*b"NV12")), None);
    }

    #[test]
    fn the_cpu_conversion_passes_its_own_colour_check() {
        let (w, h) = (256, 64);
        let mut i420 = crate::yuv::I420::new(w, h);
        i420.fill_from_bgra(&bars_bgrx(w, h), w);
        let c = compare_bars(&i420.to_nv12(), w, h);
        assert!(c.refused.is_none());
        assert_eq!((c.bar_max, c.y_mean, c.uv_mean), (0, 0.0, 0.0));
    }

    #[test]
    fn bt709_colours_are_refused() {
        // The same bars converted with BT.709 coefficients (limited range), as drivers
        // that ignore the requested standard write for HD sizes.
        let (w, h) = (256, 64);
        let bgrx = bars_bgrx(w, h);
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                let px = &bgrx[(y * w + x) * 4..];
                let (b, g, r) = (f64::from(px[0]), f64::from(px[1]), f64::from(px[2]));
                let yy = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
                nv12[y * w + x] = yy.round() as u8;
                if x % 2 == 0 && y % 2 == 0 {
                    let u = 128.0 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224.0 / 255.0;
                    let v = 128.0 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224.0 / 255.0;
                    let i = w * h + (y / 2) * w + x;
                    nv12[i] = u.round() as u8;
                    nv12[i + 1] = v.round() as u8;
                }
            }
        }
        let c = compare_bars(&nv12, w, h);
        assert!(c.refused.is_some(), "{c:?}");
    }
}
