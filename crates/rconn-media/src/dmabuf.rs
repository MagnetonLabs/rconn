//! Linux screens kept on the GPU: a DMA-BUF from the Wayland screen stream, handed to
//! the VA-API encoder ([`crate::vaapi`]) without a CPU copy.
//!
//! Unsafe: duplicating a file descriptor PipeWire owns, which is valid while the buffer
//! it belongs to is dequeued (the only time it is read).
#![allow(unsafe_code)]

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

/// `DRM_FORMAT_MOD_LINEAR`: no tiling. Every GPU that renders the desktop can write it
/// and every VA driver can read it.
pub const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "the driver's implicit layout".
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*c)
}
/// DRM pixel formats (little-endian channel order, so XRGB8888 is B, G, R, X in memory).
pub const DRM_XRGB8888: u32 = fourcc(b"XR24");
pub const DRM_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_XBGR8888: u32 = fourcc(b"XB24");
pub const DRM_ABGR8888: u32 = fourcc(b"AB24");

/// The modifiers offered to the compositor for screen buffers. Linear only: mutter (and
/// other compositors) then render the screen into a linear buffer, which any Intel/AMD
/// VA driver imports. Tiled layouts would save the compositor's copy but need the GPU's
/// own list (EGL), and a wrong guess fails the import.
pub fn offered_modifiers() -> Vec<u64> {
    vec![MOD_LINEAR]
}

/// One plane of a DMA-BUF picture.
#[derive(Debug)]
pub struct Plane {
    pub fd: Arc<OwnedFd>,
    pub offset: u32,
    pub stride: u32,
    /// Size of the whole buffer `fd` refers to.
    pub size: u32,
    /// Identifies the buffer behind `fd` (the same buffer arrives again and again; the
    /// encoder imports it once).
    pub inode: u64,
}

/// A captured screen in GPU memory.
#[derive(Debug)]
pub struct DmaBuf {
    pub width: u32,
    pub height: u32,
    /// A `DRM_*` format above.
    pub drm_format: u32,
    pub modifier: u64,
    pub planes: Vec<Plane>,
}

impl Plane {
    /// Our own reference to PipeWire's buffer `fd` (a duplicate), with its size and
    /// identity; `max_size` (PipeWire's) if the buffer can't tell its size.
    pub fn from_raw(fd: RawFd, offset: u32, stride: u32, max_size: u32) -> std::io::Result<Self> {
        if fd < 0 {
            return Err(std::io::Error::other("no file descriptor"));
        }
        // SAFETY: PipeWire keeps `fd` open while its buffer is dequeued, which is when
        // this runs; we only duplicate it.
        let owned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        let mut file = File::from(owned);
        let inode = file.metadata()?.ino();
        let size = file
            .seek(SeekFrom::End(0))
            .ok()
            .and_then(|s| u32::try_from(s).ok())
            .filter(|&s| s > 0)
            .unwrap_or(max_size.saturating_add(offset));
        Ok(Self {
            fd: Arc::new(file.into()),
            offset,
            stride,
            size,
            inode,
        })
    }
}

/// A captured screen kept on the GPU ([`crate::capture::Capturer::next_surface`]), to be
/// converted and encoded at [`Surface::dimensions`].
#[derive(Clone, Debug)]
pub struct Surface {
    pub frame: Arc<DmaBuf>,
    /// The size it is sent at (the screen halved to fit the largest size), even.
    pub size: (usize, usize),
}

impl Surface {
    pub fn dimensions(&self) -> (usize, usize) {
        self.size
    }
}

/// The size a `w` x `h` screen is sent at within `max`: halved until it fits (as
/// [`crate::video::Frame::fit_within`]), rounded down to even.
pub fn fitted(w: usize, h: usize, max: (usize, usize)) -> (usize, usize) {
    let (mut w, mut h) = (w, h);
    while (w > max.0 || h > max.1) && w >= 4 && h >= 4 {
        w /= 2;
        h /= 2;
    }
    (w & !1, h & !1)
}

/// Rows `rows` of a `from_h` tall picture, in a picture `to_h` tall (even, covering).
pub fn scale_rows(rows: (usize, usize), from_h: usize, to_h: usize) -> (usize, usize) {
    if from_h == 0 || from_h == to_h {
        return (rows.0 & !1, rows.1.next_multiple_of(2).min(to_h));
    }
    let top = rows.0 * to_h / from_h;
    let bottom = (rows.1 * to_h).div_ceil(from_h);
    (top & !1, bottom.next_multiple_of(2).min(to_h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_formats_are_the_kernel_values() {
        // drm_fourcc.h
        assert_eq!(DRM_XRGB8888, 0x3432_5258);
        assert_eq!(DRM_ARGB8888, 0x3432_5241);
        assert_eq!(DRM_XBGR8888, 0x3432_4258);
        assert_eq!(DRM_ABGR8888, 0x3432_4241);
    }

    #[test]
    fn only_linear_is_offered() {
        assert_eq!(offered_modifiers(), vec![MOD_LINEAR]);
        assert!(!offered_modifiers().contains(&MOD_INVALID));
    }

    #[test]
    fn fitted_matches_frame_halving() {
        assert_eq!(fitted(2992, 1878, (1920, 1080)), (1496, 938));
        assert_eq!(fitted(1921, 1081, (3840, 2400)), (1920, 1080));
        assert_eq!(fitted(7680, 4320, (3840, 2400)), (3840, 2160));
    }

    #[test]
    fn rows_scale_to_the_sent_size() {
        assert_eq!(scale_rows((10, 21), 1080, 1080), (10, 22));
        assert_eq!(scale_rows((100, 201), 2160, 1080), (50, 102));
        assert_eq!(scale_rows((0, 2160), 2160, 1080), (0, 1080));
    }

    #[test]
    fn a_plane_refers_to_its_own_copy_of_the_fd() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let mut f = tempfile_in_memory();
        f.write_all(&[0u8; 4096]).unwrap();
        let p = Plane::from_raw(f.as_raw_fd(), 0, 64, 1).unwrap();
        assert_ne!(p.fd.as_raw_fd(), f.as_raw_fd());
        assert_eq!(p.size, 4096);
        assert_eq!(p.inode, f.metadata().unwrap().ino());
        assert!(Plane::from_raw(-1, 0, 0, 0).is_err());
    }

    fn tempfile_in_memory() -> File {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rconn-dmabuf-test-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let _ = std::fs::remove_file(&path);
        f
    }
}
