//! X11: the screen size through RandR, as `xrandr --output <primary> --mode <m>` does,
//! using only modes the output lists. The screen (root window) grows before and shrinks
//! after the CRTC changes, so every CRTC always fits in it.

use super::{Mode, pick_fit, pick_restore};
use std::sync::Mutex;
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// What this host changed first: the CRTC and its mode then.
struct Before {
    crtc: randr::Crtc,
    mode: randr::Mode,
    size: Mode,
}

static BEFORE: Mutex<Option<Before>> = Mutex::new(None);

fn x11(what: &str, e: impl std::fmt::Display) -> String {
    format!("X11 {what}: {e}")
}

/// The primary output's CRTC and the modes the output lists.
struct Output {
    crtc: randr::Crtc,
    info: randr::GetCrtcInfoReply,
    /// (mode id, size and rate), in the output's order.
    modes: Vec<(randr::Mode, Mode)>,
    preferred: Option<usize>,
}

impl Output {
    fn current(&self) -> Option<Mode> {
        self.modes
            .iter()
            .find(|(id, _)| *id == self.info.mode)
            .map(|&(_, m)| m)
    }

    fn rotated(&self) -> bool {
        self.info.rotation & (randr::Rotation::ROTATE90 | randr::Rotation::ROTATE270)
            != randr::Rotation::from(0u16)
    }

    fn as_modes(&self) -> Vec<Mode> {
        self.modes.iter().map(|&(_, m)| m).collect()
    }
}

struct Screen {
    conn: RustConnection,
    root: Window,
    /// Root window size in millimetres (kept in proportion, so the DPI stays).
    mm: (u32, u32),
    res: randr::GetScreenResourcesCurrentReply,
}

impl Screen {
    fn open() -> Result<Self, String> {
        let (conn, n) = x11rb::connect(None).map_err(|e| x11("connect", e))?;
        let s = &conn.setup().roots[n];
        let (root, mm) = (
            s.root,
            (
                u32::from(s.width_in_millimeters),
                u32::from(s.height_in_millimeters),
            ),
        );
        conn.randr_query_version(1, 3)
            .map_err(|e| x11("RandR", e))?
            .reply()
            .map_err(|e| x11("RandR", e))?;
        let res = conn
            .randr_get_screen_resources_current(root)
            .map_err(|e| x11("screen resources", e))?
            .reply()
            .map_err(|e| x11("screen resources", e))?;
        Ok(Self {
            conn,
            root,
            mm,
            res,
        })
    }

    fn crtc_info(&self, crtc: randr::Crtc) -> Result<randr::GetCrtcInfoReply, String> {
        self.conn
            .randr_get_crtc_info(crtc, self.res.config_timestamp)
            .map_err(|e| x11("CRTC", e))?
            .reply()
            .map_err(|e| x11("CRTC", e))
    }

    /// The primary output, else the first one that is on.
    fn output(&self) -> Result<Output, String> {
        let primary = self
            .conn
            .randr_get_output_primary(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.output)
            .filter(|&o| o != x11rb::NONE);
        let candidates = primary.into_iter().chain(self.res.outputs.iter().copied());
        for output in candidates {
            let Ok(info) = self
                .conn
                .randr_get_output_info(output, self.res.config_timestamp)
                .map_err(|e| x11("output", e))?
                .reply()
            else {
                continue;
            };
            if info.crtc == x11rb::NONE {
                continue;
            }
            let modes = info
                .modes
                .iter()
                .filter_map(|&id| {
                    let m = self.res.modes.iter().find(|m| m.id == id)?;
                    let dots = u32::from(m.htotal) * u32::from(m.vtotal);
                    let hz = if dots > 0 {
                        f64::from(m.dot_clock) / f64::from(dots)
                    } else {
                        0.0
                    };
                    Some((
                        id,
                        Mode {
                            width: m.width.into(),
                            height: m.height.into(),
                            hz,
                        },
                    ))
                })
                .collect::<Vec<_>>();
            let preferred = (info.num_preferred > 0 && !modes.is_empty()).then_some(0);
            return Ok(Output {
                crtc: info.crtc,
                info: self.crtc_info(info.crtc)?,
                modes,
                preferred,
            });
        }
        Err("no X11 output is on".into())
    }

    /// Put `mode` on the output's CRTC, resizing the screen to hold every CRTC.
    fn switch(&self, out: &Output, mode: randr::Mode, size: Mode) -> Result<(), String> {
        let (w, h) = if out.rotated() {
            (size.height, size.width)
        } else {
            (size.width, size.height)
        };
        let extent = |x: i16, y: i16, w: u32, h: u32| {
            (
                u32::try_from(i32::from(x)).unwrap_or(0) + w,
                u32::try_from(i32::from(y)).unwrap_or(0) + h,
            )
        };
        let mut need = extent(out.info.x, out.info.y, w, h);
        for &crtc in self.res.crtcs.iter().filter(|&&c| c != out.crtc) {
            let c = self.crtc_info(crtc)?;
            if c.mode != x11rb::NONE {
                let (cw, ch) = extent(c.x, c.y, c.width.into(), c.height.into());
                need = (need.0.max(cw), need.1.max(ch));
            }
        }
        let g = self
            .conn
            .get_geometry(self.root)
            .map_err(|e| x11("geometry", e))?
            .reply()
            .map_err(|e| x11("geometry", e))?;
        let now = (u32::from(g.width), u32::from(g.height));
        let grown = (now.0.max(need.0), now.1.max(need.1));
        let _ = self.conn.grab_server();
        let r = (|| {
            if grown != now {
                self.screen_size(grown, now)?;
            }
            let reply = self
                .conn
                .randr_set_crtc_config(
                    out.crtc,
                    x11rb::CURRENT_TIME,
                    self.res.config_timestamp,
                    out.info.x,
                    out.info.y,
                    mode,
                    out.info.rotation,
                    &out.info.outputs,
                )
                .map_err(|e| x11("set CRTC", e))?
                .reply()
                .map_err(|e| x11("set CRTC", e))?;
            if reply.status != randr::SetConfig::SUCCESS {
                if grown != now {
                    let _ = self.screen_size(now, now);
                }
                return Err(format!("X11 refused the mode ({:?})", reply.status));
            }
            // The mode is on; a screen larger than its CRTCs is still valid (Xwayland,
            // for one, refuses to resize it), so this last step may fail.
            if need != grown
                && let Err(e) = self.screen_size(need, now)
            {
                tracing::warn!(error = %e, "X11 screen not shrunk to the new mode");
            }
            Ok(())
        })();
        let _ = self.conn.ungrab_server();
        let _ = self.conn.flush();
        r
    }

    /// Resize the root window to `px`, its millimetres in proportion to `was`.
    fn screen_size(&self, px: (u32, u32), was: (u32, u32)) -> Result<(), String> {
        let mm = |px: u32, was: u32, mm: u32| {
            if was > 0 && mm > 0 {
                (u64::from(mm) * u64::from(px) / u64::from(was)) as u32
            } else {
                (f64::from(px) * 25.4 / 96.0) as u32 // 96 dpi
            }
        };
        let to16 = |v: u32| u16::try_from(v).map_err(|_| format!("screen size {v} too large"));
        self.conn
            .randr_set_screen_size(
                self.root,
                to16(px.0)?,
                to16(px.1)?,
                mm(px.0, was.0, self.mm.0),
                mm(px.1, was.1, self.mm.1),
            )
            .map_err(|e| x11("screen size", e))?
            .check()
            .map_err(|e| x11("screen size", e))
    }
}

pub fn set(width: u32, height: u32) -> Result<Option<(u32, u32)>, String> {
    let screen = Screen::open()?;
    let out = screen.output()?;
    let current = out
        .current()
        .ok_or("the output's current mode is not one it lists")?;
    let turn = |(w, h): (u32, u32)| if out.rotated() { (h, w) } else { (w, h) };
    let i = pick_fit(&out.as_modes(), turn((width, height)), current.hz)
        .ok_or("the output lists no modes")?;
    let (id, size) = out.modes[i];
    if (size.width, size.height) == (current.width, current.height) {
        return Ok(None);
    }
    screen.switch(&out, id, size)?;
    BEFORE.lock().expect("display state").get_or_insert(Before {
        crtc: out.crtc,
        mode: out.info.mode,
        size: current,
    });
    Ok(Some(turn((size.width, size.height))))
}

pub fn restore() {
    let Some(before) = BEFORE.lock().expect("display state").take() else {
        return;
    };
    let r = (|| {
        let screen = Screen::open()?;
        let out = screen.output()?;
        if out.crtc != before.crtc {
            return Err("the primary output changed meanwhile".to_owned());
        }
        let (id, size) = match out.modes.iter().find(|(id, _)| *id == before.mode) {
            Some(&m) => m,
            None => {
                let i = pick_restore(&out.as_modes(), before.size, out.preferred)
                    .ok_or("the output lists no modes")?;
                out.modes[i]
            }
        };
        if id == out.info.mode {
            return Ok(());
        }
        screen.switch(&out, id, size)
    })();
    if let Err(e) = r {
        tracing::warn!(error = %e, "could not put the screen size back");
    }
}
