//! Linux (X11) capture: the root window via MIT-SHM, converting only what XDamage
//! reports as changed. Wayland sessions need the desktop portal instead (an X client on
//! Xwayland sees only other X clients).

#![allow(unsafe_code)] // SysV shared memory (shmget/shmat/shmdt/shmctl) for MIT-SHM

use super::{CaptureError, Captured, Capturer, convert_whole};
use crate::video::Frame;
use crate::yuv::{I420, Rect};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

/// How often to look for damage while waiting (the connection has no blocking read with
/// a timeout); well under a frame at 30 fps.
const POLL: Duration = Duration::from_millis(4);

fn backend(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("X11 {what}: {e}"))
}

/// A SysV shared-memory segment attached to us and to the X server.
struct Segment {
    seg: shm::Seg,
    id: i32,
    ptr: *mut u8,
    len: usize,
}

impl Segment {
    fn new(conn: &RustConnection, len: usize) -> Result<Self, CaptureError> {
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err(backend("shmget", std::io::Error::last_os_error()));
        }
        let ptr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if ptr as isize == -1 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(backend("shmat", e));
        }
        let seg = Self {
            seg: conn.generate_id().map_err(|e| backend("id", e))?,
            id,
            ptr: ptr.cast(),
            len,
        };
        conn.shm_attach(seg.seg, id as u32, false)
            .map_err(|e| backend("shm attach", e))?
            .check()
            .map_err(|e| backend("shm attach", e))?;
        // Freed once both sides detach, even if we crash.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        Ok(seg)
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn release(&self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg);
        let _ = conn.flush();
        unsafe { libc::shmdt(self.ptr.cast()) };
        let _ = self.id;
    }
}

pub struct X11Capturer {
    conn: RustConnection,
    root: Window,
    width: usize,
    height: usize,
    damage: damage::Damage,
    region: xfixes::Region,
    /// `None` when the server has no MIT-SHM (a remote display): plain GetImage.
    shm: Option<Segment>,
    shm_available: bool,
    /// The next picture must be complete (first frame, size change).
    want_full: bool,
}

// The raw segment pointer is only used from the thread that owns the capturer.
unsafe impl Send for X11Capturer {}

impl X11Capturer {
    pub fn new() -> Result<Self, CaptureError> {
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
            && std::env::var_os("RCONN_X11_ON_WAYLAND").is_none()
        {
            return Err(CaptureError::Backend(
                "this is a Wayland session: log in with \"Ubuntu on Xorg\" (X11) for now".into(),
            ));
        }
        let (conn, screen) = x11rb::connect(None).map_err(|e| backend("connect", e))?;
        let s = &conn.setup().roots[screen];
        let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
        if s.root_depth != 24 && s.root_depth != 32 {
            return Err(CaptureError::UnsupportedFormat(s.root_depth.into()));
        }
        conn.damage_query_version(1, 1)
            .map_err(|e| backend("DAMAGE", e))?
            .reply()
            .map_err(|e| backend("DAMAGE", e))?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| backend("XFIXES", e))?
            .reply()
            .map_err(|e| backend("XFIXES", e))?;
        let shm_available = conn
            .shm_query_version()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let damage = conn.generate_id().map_err(|e| backend("id", e))?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)
            .map_err(|e| backend("damage", e))?;
        let region = conn.generate_id().map_err(|e| backend("id", e))?;
        conn.xfixes_create_region(region, &[])
            .map_err(|e| backend("region", e))?;
        // Resolution changes (RandR), so the picture is rebuilt at the new size.
        if conn
            .randr_query_version(1, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
        {
            let _ = conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE);
        }
        conn.flush().map_err(|e| backend("flush", e))?;
        tracing::info!(
            width,
            height,
            shm = shm_available,
            "X11 capture of the root window"
        );
        Ok(Self {
            conn,
            root,
            width: width.into(),
            height: height.into(),
            damage,
            region,
            shm: None,
            shm_available,
            want_full: true,
        })
    }

    /// Wait up to `timeout` for damage. Returns the changed rectangles (or `[]`).
    fn wait_damage(&mut self, timeout: Duration) -> Result<Vec<Rect>, CaptureError> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut damaged = false;
            while let Some(ev) = self
                .conn
                .poll_for_event()
                .map_err(|e| backend("event", e))?
            {
                match ev {
                    Event::DamageNotify(_) => damaged = true,
                    Event::RandrScreenChangeNotify(n) => {
                        // Rotation swaps the reported sizes.
                        let rotated = n.rotation
                            & (randr::Rotation::ROTATE90 | randr::Rotation::ROTATE270)
                            != randr::Rotation::from(0u16);
                        let (w, h) = if rotated {
                            (n.height, n.width)
                        } else {
                            (n.width, n.height)
                        };
                        self.resize(w.into(), h.into());
                        damaged = true;
                    }
                    _ => {}
                }
            }
            if damaged || self.want_full {
                return self.take_damage();
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Vec::new());
            }
            std::thread::sleep(POLL.min(deadline - now));
        }
    }

    fn resize(&mut self, width: usize, height: usize) {
        if (width, height) != (self.width, self.height) {
            tracing::info!(width, height, "X11 screen size changed");
            (self.width, self.height) = (width, height);
            self.want_full = true;
        }
    }

    /// Move the accumulated damage into our region and read it out.
    fn take_damage(&mut self) -> Result<Vec<Rect>, CaptureError> {
        self.conn
            .damage_subtract(self.damage, x11rb::NONE, self.region)
            .map_err(|e| backend("damage subtract", e))?;
        let reply = self
            .conn
            .xfixes_fetch_region(self.region)
            .map_err(|e| backend("fetch region", e))?
            .reply()
            .map_err(|e| backend("fetch region", e))?;
        Ok(reply
            .rectangles
            .iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .map(|r| Rect {
                x: r.x.max(0) as usize,
                y: r.y.max(0) as usize,
                w: r.width.into(),
                h: r.height.into(),
            })
            .collect())
    }

    /// The whole screen as BGRA, `width * 4` bytes per row.
    fn grab(&mut self) -> Result<GrabbedImage<'_>, CaptureError> {
        let (w, h) = (self.width, self.height);
        let len = w * h * 4;
        if self.shm_available && self.shm.as_ref().is_none_or(|s| s.len != len) {
            if let Some(old) = self.shm.take() {
                old.release(&self.conn);
            }
            match Segment::new(&self.conn, len) {
                Ok(s) => self.shm = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "MIT-SHM unusable; using plain GetImage");
                    self.shm_available = false;
                }
            }
        }
        let plane_mask = !0;
        if let Some(seg) = &self.shm {
            self.conn
                .shm_get_image(
                    self.root,
                    0,
                    0,
                    w as u16,
                    h as u16,
                    plane_mask,
                    ImageFormat::Z_PIXMAP.into(),
                    seg.seg,
                    0,
                )
                .map_err(|e| backend("shm get image", e))?
                .reply()
                .map_err(|e| backend("shm get image", e))?;
            return Ok(GrabbedImage::Shared(seg.bytes()));
        }
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                w as u16,
                h as u16,
                plane_mask,
            )
            .map_err(|e| backend("get image", e))?
            .reply()
            .map_err(|e| backend("get image", e))?;
        if reply.data.len() < len {
            return Err(backend("get image", "short image (not 32 bits per pixel?)"));
        }
        Ok(GrabbedImage::Owned(reply.data))
    }
}

enum GrabbedImage<'a> {
    Shared(&'a [u8]),
    Owned(Vec<u8>),
}

impl GrabbedImage<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Shared(b) => b,
            Self::Owned(v) => v,
        }
    }
}

impl Capturer for X11Capturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        if self.wait_damage(timeout)?.is_empty() && !self.want_full {
            return Ok(None);
        }
        self.want_full = false;
        let (width, height) = (self.width, self.height);
        let bgra = self.grab()?.bytes().to_vec();
        Ok(Some(Frame {
            width,
            height,
            bgra,
            captured_at: Instant::now(),
        }))
    }

    fn next_into(
        &mut self,
        timeout: Duration,
        picture: &mut Option<I420>,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        let damaged = self.wait_damage(timeout)?;
        if damaged.is_empty() && !self.want_full {
            return Ok(None);
        }
        let (w, h) = (self.width, self.height);
        let (ew, eh) = (w & !1, h & !1);
        if ew == 0 || eh == 0 {
            return Ok(None);
        }
        if w > max.0 || h > max.1 {
            // Halved to fit: no partial updates on this (rare) path.
            self.want_full = false;
            let bgra = self.grab()?.bytes().to_vec();
            let frame = Frame {
                width: w,
                height: h,
                bgra,
                captured_at: Instant::now(),
            };
            return Ok(convert_whole(frame, picture, max));
        }
        let whole = Rect {
            x: 0,
            y: 0,
            w: ew,
            h: eh,
        };
        let fresh = self.want_full || picture.as_ref().is_none_or(|p| p.dimensions() != (ew, eh));
        let rects: Vec<Rect> = if fresh {
            vec![whole]
        } else {
            damaged
                .into_iter()
                .filter_map(|r| r.even_within(ew, eh))
                .collect()
        };
        if rects.is_empty() {
            return Ok(None);
        }
        self.want_full = false;
        let image = self.grab()?;
        let pic = match picture {
            Some(p) if p.dimensions() == (ew, eh) => p,
            _ => picture.insert(I420::new(ew, eh)),
        };
        pic.fill_rects_from_bgra(image.bytes(), w * 4, &rects);
        let area: usize = rects.iter().map(|r| r.w * r.h).sum();
        let top = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap_or(eh);
        Ok(Some(Captured {
            rows: (top, bottom),
            changed: (area as f32 / (ew * eh) as f32).min(1.0),
            captured_at: Instant::now(),
        }))
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        if let Some(seg) = self.shm.take() {
            seg.release(&self.conn);
        }
        let _ = self.conn.damage_destroy(self.damage);
        let _ = self.conn.xfixes_destroy_region(self.region);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an X server (`DISPLAY`), e.g. `xvfb-run cargo test`; skipped otherwise.
    #[test]
    fn captures_the_screen_and_then_only_changes() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        let mut cap = X11Capturer::new().expect("X11 capturer");
        let mut picture = None;
        let first = cap
            .next_into(Duration::from_millis(500), &mut picture, (8192, 8192))
            .unwrap()
            .expect("a first, complete picture");
        assert_eq!(first.changed, 1.0);
        let (w, _) = picture.as_ref().unwrap().dimensions();
        assert_eq!(w, cap.width & !1);

        // Nothing drawn: nothing to send.
        let idle = cap
            .next_into(Duration::from_millis(50), &mut picture, (8192, 8192))
            .unwrap();
        assert!(idle.is_none(), "{idle:?}");

        // Draw a 40x30 rectangle on the root window: only that area changes.
        let conn = &cap.conn;
        let gc = conn.generate_id().unwrap();
        conn.create_gc(
            gc,
            cap.root,
            &x11rb::protocol::xproto::CreateGCAux::new().foreground(0x00ff_0000),
        )
        .unwrap();
        let rect = x11rb::protocol::xproto::Rectangle {
            x: 10,
            y: 20,
            width: 40,
            height: 30,
        };
        conn.poly_fill_rectangle(cap.root, gc, &[rect]).unwrap();
        conn.flush().unwrap();
        let change = cap
            .next_into(Duration::from_millis(500), &mut picture, (8192, 8192))
            .unwrap()
            .expect("the drawn rectangle");
        assert_eq!(change.rows, (20, 50));
        assert!(change.changed < 0.05, "{change:?}");
    }
}
