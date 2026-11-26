//! VA-API on Linux (Intel and AMD GPUs): H.264 encoding on the GPU, and the GPU's video
//! processor turning captured DMA-BUF screens into NV12 without a CPU copy.
//!
//! Two ways in:
//! * **va-upload**: the converted picture (any capturer, X11 too) is copied into a VA
//!   surface and encoded there ([`VaH264Encoder::encode_nv12`]).
//! * **va-zerocopy**: the Wayland screen stream hands over DMA-BUFs
//!   ([`crate::dmabuf::Surface`]); they are imported as VA surfaces, converted
//!   BGRx->NV12 (BT.601 limited, as the CPU conversion) and scaled by the video processor,
//!   then encoded ([`VaH264Encoder::encode_dmabuf`]).
//!
//! libva is loaded at run time ([`ffi`]): missing libva, no render node, or no H.264
//! encode entry point means openh264, with one log line. Toggles:
//! * `RCONN_VAAPI=0`: never use VA-API (openh264 encodes, as before 0.1.7);
//! * `RCONN_ENCODER=openh264`: the same;
//! * `RCONN_GPU=0`: VA-API may encode, but frames always pass through memory (no
//!   DMA-BUF capture);
//! * `RCONN_VAAPI_DEVICE=/dev/dri/renderD129`: the render node to use (default: the
//!   first one with an H.264 encoder).
//!
//! Unsafe: calls into libva, and reading/writing its mapped buffers strictly within the
//! sizes libva reports.
#![allow(unsafe_code, non_upper_case_globals)] // libva's own constant names

mod encode;
mod ffi;
mod headers;
mod vpp;

pub use encode::{Stages, VaH264Encoder};
pub use vpp::ColourCheck;

use ffi::*;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// After VA-API fails (encoder or zero-copy path), it is tried again only after this long.
const RETRY_AFTER: Duration = Duration::from_secs(600);

/// What the GPU's VA driver can do for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    pub profile: VAProfile,
    pub entrypoint: VAEntrypoint,
    /// `VA_RC_VBR` or `VA_RC_CBR`.
    pub rc_mode: u32,
    /// A video processor (VPP) entry point: DMA-BUF conversion is possible.
    pub vpp: bool,
    /// The driver takes a packed sequence header (our SPS and PPS).
    pub packed_sequence: bool,
}

/// An initialized VA display on a DRM render node, shared by the process's encoders.
pub(crate) struct Display {
    pub va: &'static Va,
    pub dpy: VADisplay,
    /// The render node; the display uses it until the process ends.
    _node: std::fs::File,
    pub device: PathBuf,
    pub vendor: String,
    pub caps: Caps,
}

// SAFETY: libva displays may be used from any thread; each encoder (context) is used by
// one thread at a time.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    /// `Err` with libva's description unless `s` is success.
    pub fn check(&self, s: VAStatus, what: &str) -> Result<(), String> {
        if s == VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string for any status.
        let msg = unsafe { CStr::from_ptr((self.va.vaErrorStr)(s)) };
        Err(format!("{what}: {} ({s:#x})", msg.to_string_lossy()))
    }

    fn open(va: &'static Va, path: &Path) -> Result<Self, String> {
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: a valid DRM fd, kept open as long as the display (`_node`).
        let dpy = unsafe { (va.vaGetDisplayDRM)(node.as_raw_fd()) };
        if dpy.is_null() {
            return Err(format!("{}: no VA display", path.display()));
        }
        unsafe {
            // libva prints to stderr by default; route it to the log instead.
            if let Some(f) = va.vaSetErrorCallback {
                f(dpy, Some(log_message), std::ptr::null_mut());
            }
            if let Some(f) = va.vaSetInfoCallback {
                f(dpy, Some(log_message), std::ptr::null_mut());
            }
        }
        let (mut major, mut minor) = (0, 0);
        let s = unsafe { (va.vaInitialize)(dpy, &mut major, &mut minor) };
        let mut d = Self {
            va,
            dpy,
            _node: node,
            device: path.to_owned(),
            vendor: String::new(),
            caps: Caps {
                profile: VAProfileNone,
                entrypoint: 0,
                rc_mode: 0,
                vpp: false,
                packed_sequence: false,
            },
        };
        if let Err(e) = d.check(s, "vaInitialize") {
            unsafe { (va.vaTerminate)(dpy) };
            return Err(format!("{}: {e}", path.display()));
        }
        // SAFETY: a static string owned by the driver, or null.
        let vendor = unsafe { (va.vaQueryVendorString)(dpy) };
        if !vendor.is_null() {
            d.vendor = unsafe { CStr::from_ptr(vendor) }
                .to_string_lossy()
                .into_owned();
        }
        match d.probe() {
            Some(caps) => {
                d.caps = caps;
                Ok(d)
            }
            None => {
                unsafe { (va.vaTerminate)(dpy) };
                Err(format!(
                    "{} ({}): no H.264 encoder with rate control",
                    path.display(),
                    d.vendor
                ))
            }
        }
    }

    fn profiles(&self) -> Vec<VAProfile> {
        let max = unsafe { (self.va.vaMaxNumProfiles)(self.dpy) }.max(0);
        let mut v = vec![0; max as usize];
        let mut n: c_int = 0;
        let s = unsafe { (self.va.vaQueryConfigProfiles)(self.dpy, v.as_mut_ptr(), &mut n) };
        if s != VA_STATUS_SUCCESS {
            return Vec::new();
        }
        v.truncate(n.clamp(0, max) as usize);
        v
    }

    fn entrypoints(&self, profile: VAProfile) -> Vec<VAEntrypoint> {
        let max = unsafe { (self.va.vaMaxNumEntrypoints)(self.dpy) }.max(0);
        let mut v = vec![0; max as usize];
        let mut n: c_int = 0;
        let s = unsafe {
            (self.va.vaQueryConfigEntrypoints)(self.dpy, profile, v.as_mut_ptr(), &mut n)
        };
        if s != VA_STATUS_SUCCESS {
            return Vec::new();
        }
        v.truncate(n.clamp(0, max) as usize);
        v
    }

    /// The value of config attribute `attr` (0 when not supported).
    fn attribute(&self, profile: VAProfile, ep: VAEntrypoint, attr: VAConfigAttribType) -> u32 {
        let mut a = VAConfigAttrib {
            type_: attr,
            value: 0,
        };
        let s = unsafe { (self.va.vaGetConfigAttributes)(self.dpy, profile, ep, &mut a, 1) };
        if s != VA_STATUS_SUCCESS || a.value == VA_ATTRIB_NOT_SUPPORTED {
            0
        } else {
            a.value
        }
    }

    fn probe(&self) -> Option<Caps> {
        let profiles = self.profiles();
        let mut candidates = Vec::new();
        for p in profiles.iter().copied() {
            for ep in self.entrypoints(p) {
                if matches!(ep, VAEntrypointEncSlice | VAEntrypointEncSliceLP) {
                    let rt = self.attribute(p, ep, VAConfigAttribRTFormat);
                    let rc = self.attribute(p, ep, VAConfigAttribRateControl);
                    if rt & VA_RT_FORMAT_YUV420 != 0 {
                        candidates.push((p, ep, rc));
                    }
                }
            }
        }
        let vpp = self
            .entrypoints(VAProfileNone)
            .contains(&VAEntrypointVideoProc);
        let (profile, entrypoint, rc_mode) = choose_encoder(&candidates)?;
        let packed = self.attribute(profile, entrypoint, VAConfigAttribEncPackedHeaders);
        Some(Caps {
            profile,
            entrypoint,
            rc_mode,
            vpp,
            packed_sequence: packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0,
        })
    }

    pub fn profile_name(&self) -> &'static str {
        match self.caps.profile {
            VAProfileH264ConstrainedBaseline => "Constrained Baseline",
            VAProfileH264Main => "Main",
            VAProfileH264High => "High",
            _ => "?",
        }
    }
}

unsafe extern "C" fn log_message(_ctx: *mut c_void, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    // SAFETY: libva passes a NUL-terminated message.
    let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    tracing::debug!(target: "libva", "{}", msg.trim_end());
}

/// The encoder to use among `(profile, entrypoint, rate-control modes)`: Constrained
/// Baseline (what every viewer decodes, and what the session offers), else Main with
/// CAVLC; the full-featured entry point before the low-power one (which needs firmware
/// for rate control on some Intel GPUs); and only with bitrate control (VBR, else CBR):
/// a constant-quantizer stream would ignore the link's bandwidth.
pub(crate) fn choose_encoder(
    candidates: &[(VAProfile, VAEntrypoint, u32)],
) -> Option<(VAProfile, VAEntrypoint, u32)> {
    let rank_profile = |p: VAProfile| match p {
        VAProfileH264ConstrainedBaseline => Some(0),
        VAProfileH264Main => Some(1),
        _ => None,
    };
    let rank_ep = |e: VAEntrypoint| match e {
        VAEntrypointEncSlice => 0,
        _ => 1,
    };
    candidates
        .iter()
        .filter_map(|&(p, e, rc)| Some((rank_profile(p)?, rank_ep(e), p, e, choose_rc(rc)?)))
        .min_by_key(|&(rp, re, ..)| (rp, re))
        .map(|(_, _, p, e, rc)| (p, e, rc))
}

/// VBR (small frames for a still screen, like the other platforms' low-delay VBR), else
/// CBR; `None` without either.
pub(crate) fn choose_rc(modes: u32) -> Option<u32> {
    if modes & VA_RC_VBR != 0 {
        Some(VA_RC_VBR)
    } else if modes & VA_RC_CBR != 0 {
        Some(VA_RC_CBR)
    } else {
        None
    }
}

fn lib() -> Result<&'static Va, String> {
    static LIB: OnceLock<Result<Va, String>> = OnceLock::new();
    LIB.get_or_init(Va::load).as_ref().map_err(Clone::clone)
}

/// Render nodes to try, in order.
fn render_nodes(forced: Option<&str>) -> Vec<PathBuf> {
    if let Some(p) = forced.filter(|p| !p.is_empty()) {
        return vec![PathBuf::from(p)];
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("renderD"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The process's VA display: the first render node whose driver has an H.264 encoder.
pub(crate) fn display() -> Result<Arc<Display>, String> {
    static DISPLAY: OnceLock<Result<Arc<Display>, String>> = OnceLock::new();
    DISPLAY
        .get_or_init(|| {
            let va = lib()?;
            let forced = std::env::var("RCONN_VAAPI_DEVICE").ok();
            let nodes = render_nodes(forced.as_deref());
            if nodes.is_empty() {
                return Err("no DRM render node (/dev/dri/renderD*)".into());
            }
            let mut tried = Vec::new();
            for node in nodes {
                match Display::open(va, &node) {
                    Ok(d) => {
                        tracing::debug!(
                            device = %d.device.display(),
                            vendor = %d.vendor,
                            caps = ?d.caps,
                            "VA-API display"
                        );
                        return Ok(Arc::new(d));
                    }
                    Err(e) => tried.push(e),
                }
            }
            Err(tried.join("; "))
        })
        .clone()
}

fn env_off(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no"))
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

static ENCODER_FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);
static ZERO_COPY_FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);
static ENCODER_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn recently(at: &Mutex<Option<Instant>>) -> bool {
    at.lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| t.elapsed() < RETRY_AFTER)
}

/// The policy behind [`encoder_allowed`] and [`zero_copy_allowed`], without the
/// process state: (encoder, zero-copy).
pub(crate) fn policy(
    vaapi: Option<&str>,
    encoder: Option<&str>,
    gpu: Option<&str>,
    encoder_failed: bool,
    zero_copy_failed: bool,
) -> (bool, bool) {
    let enc = !env_off(vaapi) && encoder != Some("openh264") && !encoder_failed;
    let zc = enc && !env_off(gpu) && encoder != Some("software") && !zero_copy_failed;
    (enc, zc)
}

/// May an encoder be built on VA-API? Not with `RCONN_VAAPI=0` or
/// `RCONN_ENCODER=openh264`, not for a while after one failed, and not without a usable
/// display (the first answer probes the GPU).
pub fn encoder_allowed() -> bool {
    let (enc, _) = policy(
        env("RCONN_VAAPI").as_deref(),
        env("RCONN_ENCODER").as_deref(),
        None,
        recently(&ENCODER_FAILED_AT),
        false,
    );
    if !enc {
        return false;
    }
    match display() {
        Ok(_) => true,
        Err(e) => {
            note_unavailable(&e);
            false
        }
    }
}

static LAST_REASON: Mutex<Option<String>> = Mutex::new(None);

fn remember(why: &str) {
    *LAST_REASON.lock().unwrap_or_else(|e| e.into_inner()) = Some(why.to_owned());
}

/// Why VA-API (or its DMA-BUF path) was last given up on, for the self-test.
pub fn last_failure() -> Option<String> {
    LAST_REASON
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// One info line the first time VA-API can't be used; debug afterwards.
fn note_unavailable(why: &str) {
    remember(why);
    if !ENCODER_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            reason = why,
            "VA-API H.264 encoder unavailable; encoding with openh264"
        );
    } else {
        tracing::debug!(reason = why, "VA-API H.264 encoder unavailable");
    }
}

/// A VA-API encoder failed to start or to encode: openh264 for a while.
pub fn encoder_failed(why: &str) {
    note_unavailable(why);
    *ENCODER_FAILED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// May frames stay on the GPU (DMA-BUF capture, VPP conversion, VA-API encode)? Needs
/// [`encoder_allowed`], a video processor, not `RCONN_GPU=0`, and no recent failure of
/// that path.
pub fn zero_copy_allowed() -> bool {
    let (_, zc) = policy(
        env("RCONN_VAAPI").as_deref(),
        env("RCONN_ENCODER").as_deref(),
        env("RCONN_GPU").as_deref(),
        recently(&ENCODER_FAILED_AT),
        recently(&ZERO_COPY_FAILED_AT),
    );
    zc && encoder_allowed() && display().is_ok_and(|d| d.caps.vpp)
}

/// The DMA-BUF path failed (no DMA-BUF offered, wrong colours, an import or encode
/// error): capture to memory for a while.
pub fn zero_copy_failed(why: &str) {
    remember(why);
    tracing::info!(error = why, "DMA-BUF capture off; frames go through memory");
    *ZERO_COPY_FAILED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// Forget earlier failures (the self-test runs each path in turn).
pub fn retry() {
    *ENCODER_FAILED_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *ZERO_COPY_FAILED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Device, driver and what it offers, for logs and the self-test; `Err` says why there
/// is no VA-API here.
pub fn describe() -> Result<String, String> {
    let d = display()?;
    Ok(format!(
        "{} ({}): H.264 {} via {}, {} rate control, {} stream headers, video processor {}",
        d.device.display(),
        d.vendor,
        d.profile_name(),
        if d.caps.entrypoint == VAEntrypointEncSliceLP {
            "low-power encoder"
        } else {
            "encoder"
        },
        if d.caps.rc_mode == VA_RC_VBR {
            "VBR"
        } else {
            "CBR"
        },
        if d.caps.packed_sequence {
            "our"
        } else {
            "the driver's"
        },
        if d.caps.vpp { "yes" } else { "no" },
    ))
}

/// Does the video processor convert a `size` picture to BT.601 limited range, as the CPU
/// conversion does? Checked once per size (colour bars through a scratch encoder); a
/// failure is checked again next time (the path is off meanwhile, [`zero_copy_failed`]).
pub fn check_zero_copy(size: (usize, usize)) -> Result<(), String> {
    type Checked = Vec<((usize, usize), Result<(), String>)>;
    static CHECKED: Mutex<Checked> = Mutex::new(Vec::new());
    if let Some((_, r)) = CHECKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(s, _)| *s == size)
    {
        return r.clone();
    }
    let r = VaH264Encoder::new(size.0 as u32, size.1 as u32, 30, 4_000_000, None)
        .map_err(|e| e.to_string())
        .and_then(|mut e| e.check_colours().map(|_| ()).map_err(|e| e.to_string()));
    if r.is_ok() {
        CHECKED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((size, r.clone()));
    }
    r
}

/// The video processor against the CPU conversion on colour bars at `size` (the
/// self-test prints the numbers; [`check_zero_copy`] is the gate).
pub fn colour_report(size: (usize, usize)) -> Result<ColourCheck, String> {
    VaH264Encoder::new(size.0 as u32, size.1 as u32, 30, 4_000_000, None)
        .and_then(|mut e| e.colour_check())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_choice_prefers_baseline_full_encoder_with_bitrate_control() {
        let cb = VAProfileH264ConstrainedBaseline;
        let main = VAProfileH264Main;
        let high = VAProfileH264High;
        let (es, lp) = (VAEntrypointEncSlice, VAEntrypointEncSliceLP);
        let vbr = VA_RC_CBR | VA_RC_VBR | VA_RC_CQP;
        // Intel Gen9-11 (iHD): both entry points; the full one wins, with VBR.
        assert_eq!(
            choose_encoder(&[(main, es, vbr), (cb, lp, vbr), (cb, es, vbr)]),
            Some((cb, es, VA_RC_VBR))
        );
        // Intel Gen12+: low-power only.
        assert_eq!(
            choose_encoder(&[(cb, lp, VA_RC_CBR)]),
            Some((cb, lp, VA_RC_CBR))
        );
        // Low power without HuC firmware: constant QP only, so unusable.
        assert_eq!(choose_encoder(&[(cb, lp, VA_RC_CQP)]), None);
        // No Constrained Baseline: Main (CAVLC); High is never picked.
        assert_eq!(
            choose_encoder(&[(high, es, vbr), (main, es, VA_RC_CBR)]),
            Some((main, es, VA_RC_CBR))
        );
        assert_eq!(choose_encoder(&[(high, es, vbr)]), None);
        assert_eq!(choose_encoder(&[]), None);
    }

    #[test]
    fn toggles() {
        // (RCONN_VAAPI, RCONN_ENCODER, RCONN_GPU, failed, zc failed) -> (encoder, zero-copy)
        assert_eq!(policy(None, None, None, false, false), (true, true));
        assert_eq!(policy(Some("0"), None, None, false, false), (false, false));
        assert_eq!(
            policy(Some("off"), None, None, false, false),
            (false, false)
        );
        assert_eq!(policy(Some("1"), None, None, false, false), (true, true));
        assert_eq!(
            policy(None, Some("openh264"), None, false, false),
            (false, false)
        );
        assert_eq!(policy(None, None, Some("0"), false, false), (true, false));
        assert_eq!(
            policy(None, Some("software"), None, false, false),
            (true, false)
        );
        assert_eq!(policy(None, None, None, true, false), (false, false));
        assert_eq!(policy(None, None, None, false, true), (true, false));
    }

    #[test]
    fn forced_render_node() {
        assert_eq!(
            render_nodes(Some("/dev/dri/renderD129")),
            vec![PathBuf::from("/dev/dri/renderD129")]
        );
    }

    /// Needs an Intel/AMD GPU with a VA-API H.264 encoder (and libva) on this machine.
    #[test]
    #[ignore = "needs a VA-API capable GPU"]
    fn gpu_vaapi_display_opens() {
        eprintln!("{}", describe().expect("VA-API display"));
    }
}
