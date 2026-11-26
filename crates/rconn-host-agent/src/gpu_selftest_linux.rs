//! `--selftest-gpu N` on Linux: measure the capture -> encode paths on this machine.
//!
//! A full-screen X11 window (Xwayland on a Wayland desktop) repaints at 60 Hz, so every
//! capture is a whole-screen change. Then N frames through each path, as streaming runs
//! them:
//! 1. **openh264**: capture to memory, BGRA->I420 on the CPU, openh264 (as before 0.1.7);
//! 2. **va-upload**: capture to memory, I420->NV12 upload, VA-API encode on the GPU;
//! 3. **va-zerocopy**: the screen stream's DMA-BUFs imported into VA-API, converted
//!    BGRx->NV12 by the video processor, encoded (Wayland only; the compositor must offer
//!    DMA-BUFs).
//!
//! Each line says which path really ran (a path that isn't available falls back, as
//! streaming would), its per-stage times and fps. Each stream is decoded with openh264
//! and its last picture compared with the desktop; the video processor's colours are
//! compared with the CPU conversion.

use anyhow::{Context, Result};
use rconn_media::capture::{self, Captured, Capturer, Surface};
use rconn_media::vaapi;
use rconn_media::video::{
    DecodedFrame, EncoderSettings, Frame, PixelLayout, VideoDecoder, VideoEncoder,
};
use rconn_media::yuv::{I420, i420_to_rgba, nv12_to_rgba};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BITRATE: u32 = 8_000_000;
const FPS: u32 = 30;

fn settings() -> EncoderSettings {
    EncoderSettings {
        bitrate_bps: BITRATE,
        max_fps: FPS as f32,
        threads: 0,
        slice_bytes: None,
    }
}

/// Capture and encode as `stream.rs` does: GPU surfaces when the encoder takes them and
/// the capturer can deliver them, converted pictures in memory otherwise.
struct Frames {
    cap: Box<dyn Capturer>,
    surfaces: bool,
    picture: Option<I420>,
    surface: Option<Surface>,
}

impl Frames {
    fn memory(cap: Box<dyn Capturer>) -> Self {
        Self {
            cap,
            surfaces: false,
            picture: None,
            surface: None,
        }
    }

    fn new(mut cap: Box<dyn Capturer>, enc: &VideoEncoder, max: (usize, usize)) -> Self {
        let surfaces = enc.wants_surfaces() && cap.use_surfaces(max);
        Self {
            surfaces,
            ..Self::memory(cap)
        }
    }

    fn next(&mut self, timeout: Duration, max: (usize, usize)) -> Result<Option<Captured>> {
        Ok(if self.surfaces {
            self.cap.next_surface(timeout)?.map(|(s, c)| {
                self.surface = Some(s);
                c
            })
        } else {
            self.cap.next_into(timeout, &mut self.picture, max)?
        })
    }

    fn dimensions(&self) -> Option<(usize, usize)> {
        match (&self.surface, &self.picture) {
            (Some(s), _) => Some(s.dimensions()),
            (None, Some(p)) => Some(p.dimensions()),
            (None, None) => None,
        }
    }

    fn encode(&mut self, enc: &mut VideoEncoder, rows: (usize, usize)) -> Result<Option<Vec<u8>>> {
        Ok(match (&self.surface, &self.picture) {
            (Some(s), _) => enc.encode_surface(s)?,
            (None, Some(p)) => enc.encode_yuv(p, Some(rows))?,
            (None, None) => None,
        })
    }
}

/// Process CPU seconds so far (`/proc/self/stat`, 100 ticks/s as on every mainstream
/// kernel).
fn cpu_seconds() -> f64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            // Fields after the command name (which may hold spaces) in parentheses.
            let rest = s.rsplit_once(')')?.1;
            let f: Vec<&str> = rest.split_whitespace().collect();
            let (u, k): (f64, f64) = (f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?);
            Some((u + k) / 100.0)
        })
        .unwrap_or(0.0)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Per-frame timings of one path.
#[derive(Default)]
struct Timings {
    capture: Vec<f64>,
    encode: Vec<f64>,
    /// VA-API stages within `encode`: upload, convert (VPP), GPU encode.
    upload: Vec<f64>,
    convert: Vec<f64>,
    gpu: Vec<f64>,
    bytes: usize,
    wall: f64,
    cpu: f64,
}

impl Timings {
    fn line(&self, name: &str, ran: &str, engine: &str) -> String {
        let n = self.capture.len().max(1) as f64;
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let mut total: Vec<f64> = self
            .capture
            .iter()
            .zip(&self.encode)
            .map(|(a, b)| a + b)
            .collect();
        total.sort_by(f64::total_cmp);
        let pct = |p: f64| {
            total
                .get(((total.len() as f64 - 1.0) * p) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let mut s = format!(
            "{name}: ran {ran} [{engine}]: {} frames, capture {:.2} ms + encode {:.2} ms = {:.2} ms/frame (p50 {:.2}, p95 {:.2}); {:.1} fps; CPU {:.0}% of one core; {:.0} kbit/frame",
            self.capture.len(),
            mean(&self.capture),
            mean(&self.encode),
            mean(&total),
            pct(0.5),
            pct(0.95),
            n / self.wall.max(1e-9),
            self.cpu / self.wall.max(1e-9) * 100.0,
            self.bytes as f64 * 8.0 / 1000.0 / n,
        );
        if !self.gpu.is_empty() {
            let _ = write!(
                s,
                "\n  VA-API stages: upload {:.2} ms, import+convert {:.2} ms, GPU encode {:.2} ms",
                mean(&self.upload),
                mean(&self.convert),
                mean(&self.gpu)
            );
        }
        s
    }
}

/// Capture and encode `n` changed frames through `frames`.
fn drive(
    frames: &mut Frames,
    enc: &mut VideoEncoder,
    n: usize,
    max: (usize, usize),
) -> Result<(Timings, Vec<Vec<u8>>)> {
    let mut t = Timings::default();
    let mut aus = Vec::new();
    let (wall0, cpu0) = (Instant::now(), cpu_seconds());
    let deadline = Instant::now() + Duration::from_secs(60);
    while t.capture.len() < n && Instant::now() < deadline {
        let t0 = Instant::now();
        let Some(c) = frames.next(Duration::from_millis(100), max)? else {
            continue;
        };
        let capture = t0.elapsed();
        let t1 = Instant::now();
        let out = frames.encode(enc, c.rows)?;
        t.encode.push(ms(t1.elapsed()));
        t.capture.push(ms(capture));
        if let Some(s) = enc.gpu_stages() {
            t.upload.push(ms(s.upload));
            t.convert.push(ms(s.convert));
            t.gpu.push(ms(s.encode));
        }
        if let Some(au) = out {
            t.bytes += au.len();
            aus.push(au);
        }
    }
    t.wall = wall0.elapsed().as_secs_f64();
    t.cpu = cpu_seconds() - cpu0;
    Ok((t, aus))
}

/// Freeze the test window and encode its still picture a few times, so whatever an
/// encoder holds back comes out and the last picture can be compared with the desktop.
fn settle(
    frames: &mut Frames,
    enc: &mut VideoEncoder,
    max: (usize, usize),
    aus: &mut Vec<Vec<u8>>,
) -> Result<()> {
    PAUSED.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150));
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        frames
            .next(Duration::from_millis(30), max)
            .context("capture while still")?;
    }
    let rows = (0, frames.dimensions().map_or(0, |d| d.1));
    for _ in 0..3 {
        aus.extend(frames.encode(enc, rows).context("encode while still")?);
    }
    Ok(())
}

/// The (still) desktop as sent within `max`, from a fresh capturer. Unfreezes the test
/// window.
fn snapshot(max: (usize, usize)) -> Result<Option<Frame>> {
    let mut cap = capture::primary_display().context("second capturer")?;
    let until = Instant::now() + Duration::from_secs(3);
    let mut desk = None;
    while desk.is_none() && Instant::now() < until {
        REPAINT.store(true, Ordering::Relaxed);
        desk = cap
            .next_frame(Duration::from_millis(100))
            .context("desktop snapshot")?;
    }
    PAUSED.store(false, Ordering::Relaxed);
    Ok(desk.map(|f| f.fit_within(max.0, max.1)))
}

/// PSNR (dB) of the decoded RGBA `pic` against BGRA `desk` of the same size.
fn psnr(pic: &[u8], desk: &[u8]) -> f64 {
    let mut sum = 0f64;
    let mut count = 0f64;
    for (p, d) in pic.as_chunks::<4>().0.iter().zip(desk.as_chunks::<4>().0) {
        for (a, b) in [(p[0], d[2]), (p[1], d[1]), (p[2], d[0])] {
            let e = f64::from(a) - f64::from(b);
            sum += e * e;
            count += 1.0;
        }
    }
    let mse = sum / count.max(1.0);
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// With `RCONN_SELFTEST_DUMP=<dir>`, the compared pictures are written there as PPM.
fn dump(label: &str, w: usize, h: usize, rgb: impl Iterator<Item = u8>) {
    if let Some(dir) = std::env::var_os("RCONN_SELFTEST_DUMP") {
        let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
        data.extend(rgb);
        let _ = std::fs::write(Path::new(&dir).join(format!("{label}.ppm")), data);
    }
}

/// Decode `aus` with openh264 and compare the last picture with the desktop `desk`.
fn check_stream(
    label: &str,
    openh264: &Path,
    aus: &[Vec<u8>],
    desk: Option<&Frame>,
) -> Result<String> {
    let mut dec = VideoDecoder::software(openh264)?;
    let mut pictures = 0;
    let mut last = None;
    let mut errors = 0;
    for au in aus {
        match dec.decode(au) {
            Ok(Some(p)) => {
                pictures += 1;
                last = Some(p);
            }
            Ok(None) => {}
            Err(_) => errors += 1,
        }
    }
    let mut s = format!("decoded {pictures}/{} units", aus.len());
    if errors > 0 {
        let _ = write!(s, " ({errors} decode errors)");
    }
    if let (Some(p), Some(d)) = (last, desk) {
        if (p.width, p.height) == (d.width & !1, d.height & !1) {
            let rgba = to_rgba(&p);
            let crop: Vec<u8> = d
                .bgra
                .chunks_exact(d.width * 4)
                .take(p.height)
                .flat_map(|row| &row[..p.width * 4])
                .copied()
                .collect();
            let _ = write!(
                s,
                ", last picture {:.1} dB PSNR vs the desktop",
                psnr(&rgba, &crop)
            );
            dump(
                &format!("{label}-decoded"),
                p.width,
                p.height,
                rgba.as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|px| [px[0], px[1], px[2]]),
            );
            dump(
                &format!("{label}-desktop"),
                p.width,
                p.height,
                crop.as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|px| [px[2], px[1], px[0]]),
            );
        } else {
            let _ = write!(s, ", last picture {}x{}", p.width, p.height);
        }
    }
    Ok(s)
}

/// A decoded picture as RGBA (BT.601 limited, as viewers show untagged streams).
fn to_rgba(p: &DecodedFrame) -> Vec<u8> {
    let (w, h) = (p.width, p.height);
    let (cw, ch) = p.chroma_size();
    let mut out = vec![0u8; w * h * 4];
    let chroma = &p.data[w * h..];
    match p.layout {
        PixelLayout::Nv12 => nv12_to_rgba((p.y(), chroma), (w, cw * 2), (w, h), &mut out),
        PixelLayout::I420 => i420_to_rgba(
            (p.y(), &chroma[..cw * ch], &chroma[cw * ch..]),
            (w, cw, cw),
            (w, h),
            &mut out,
        ),
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    OpenH264,
    VaUpload,
    VaZeroCopy,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Route::OpenH264 => "openh264",
            Route::VaUpload => "va-upload",
            Route::VaZeroCopy => "va-zerocopy",
        }
    }
}

/// The path a run really took.
fn ran(frames: &Frames, enc: &VideoEncoder) -> &'static str {
    match (frames.surfaces, enc.engine_name().starts_with("VA-API")) {
        (true, true) => "va-zerocopy",
        (false, true) => "va-upload",
        _ => "openh264",
    }
}

fn run_path(
    report: &mut String,
    path: Route,
    n: usize,
    openh264: &Path,
    max: (usize, usize),
) -> Result<()> {
    vaapi::retry();
    let mut enc = match path {
        Route::OpenH264 => VideoEncoder::software(openh264, settings())?,
        _ => VideoEncoder::new(openh264, settings())?,
    };
    let cap = capture::primary_display()?;
    let mut frames = match path {
        Route::VaZeroCopy => Frames::new(cap, &enc, max),
        _ => Frames::memory(cap),
    };
    if path == Route::VaZeroCopy && !frames.surfaces {
        writeln!(
            report,
            "va-zerocopy: not available here ({}); streaming uses the memory path",
            vaapi::last_failure().unwrap_or_else(|| {
                if capture::uses_portal() {
                    "the VA driver has no video processor, or RCONN_GPU=0".into()
                } else {
                    "X11 capture has no DMA-BUFs".into()
                }
            })
        )?;
        return Ok(());
    }
    let (t, mut aus) = drive(&mut frames, &mut enc, n, max)?;
    let took = ran(&frames, &enc);
    writeln!(report, "{}", t.line(path.name(), took, enc.engine_name()))?;
    if path == Route::VaUpload && took == "openh264" {
        writeln!(
            report,
            "  (VA-API encoder unavailable: {})",
            vaapi::last_failure().unwrap_or_else(|| "RCONN_VAAPI=0?".into())
        )?;
    }
    settle(&mut frames, &mut enc, max, &mut aus)?;
    let size = frames.dimensions();
    drop(frames);
    let desk = snapshot(max)?;
    if let Some((w, h)) = size {
        writeln!(report, "  sent at {w}x{h}")?;
    }
    writeln!(
        report,
        "  {}",
        check_stream(path.name(), openh264, &aus, desk.as_ref())?
    )?;
    Ok(())
}

pub fn run(n: usize, openh264: &Path, max: (usize, usize)) -> Result<String> {
    let n = n.max(6);
    let mut report = String::new();
    writeln!(
        report,
        "capture: {}",
        if capture::uses_portal() {
            "Wayland (desktop portal, PipeWire)"
        } else {
            "X11 (MIT-SHM)"
        }
    )?;
    match vaapi::describe() {
        Ok(d) => writeln!(report, "VA-API: {d}")?,
        Err(e) => writeln!(report, "VA-API: unavailable: {e}")?,
    }
    let _window = match Animator::start() {
        Ok(a) => Some(a),
        Err(e) => {
            writeln!(
                report,
                "no test window ({e:#}); frames only come while the desktop changes"
            )?;
            None
        }
    };
    std::thread::sleep(Duration::from_millis(500));
    for path in [Route::OpenH264, Route::VaUpload, Route::VaZeroCopy] {
        if let Err(e) = run_path(&mut report, path, n, openh264, max) {
            writeln!(report, "{}: failed: {e:#}", path.name())?;
        }
    }
    // The video processor's colours against the CPU conversion, at the size sent.
    if vaapi::describe().is_ok() {
        let size = capture::primary_display()
            .ok()
            .and_then(|mut c| {
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    REPAINT.store(true, Ordering::Relaxed);
                    if let Ok(Some(f)) = c.next_frame(Duration::from_millis(100)) {
                        return Some(rconn_media::dmabuf::fitted(f.width, f.height, max));
                    }
                }
                None
            })
            .unwrap_or((1920, 1080));
        match vaapi::colour_report(size) {
            Ok(c) => writeln!(
                report,
                "video processor vs CPU, colour bars at {}x{}: Y mean |diff| {:.2}, UV mean |diff| {:.2}, worst bar {}; {}",
                size.0,
                size.1,
                c.y_mean,
                c.uv_mean,
                c.bar_max,
                c.refused
                    .map_or("passed (BT.601 limited)".into(), |e| format!(
                        "refused: {e}"
                    ))
            )?,
            Err(e) => writeln!(report, "video processor colour check: {e}")?,
        }
    }
    Ok(report)
}

static TICK: AtomicU32 = AtomicU32::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
static REPAINT: AtomicBool = AtomicBool::new(false);

/// A full-screen X11 window that repaints at 60 Hz, so every capture changes.
struct Animator {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Animator {
    fn start() -> Result<Self> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{
            ConnectionExt as _, CreateGCAux, CreateWindowAux, WindowClass,
        };
        let (conn, screen) = x11rb::connect(None).context("X display")?;
        let root = &conn.setup().roots[screen];
        let (w, h) = (root.width_in_pixels, root.height_in_pixels);
        let win = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root.root,
            0,
            0,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(root.black_pixel),
        )?;
        conn.map_window(win)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new())?;
        let font = conn.generate_id()?;
        let has_font = conn
            .open_font(font, b"fixed")
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if has_font {
            conn.change_gc(gc, &x11rb::protocol::xproto::ChangeGCAux::new().font(font))?;
        }
        conn.flush()?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("rconn-selftest-window".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(16));
                    if PAUSED.load(Ordering::Relaxed) && !REPAINT.swap(false, Ordering::Relaxed) {
                        continue;
                    }
                    if !PAUSED.load(Ordering::Relaxed) {
                        TICK.fetch_add(1, Ordering::Relaxed);
                    }
                    if paint(&conn, win, gc, (w, h), has_font).is_err() {
                        break;
                    }
                }
                let _ = conn.destroy_window(win);
                let _ = conn.flush();
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

fn paint(
    conn: &impl x11rb::connection::Connection,
    win: u32,
    gc: u32,
    (w, h): (u16, u16),
    text: bool,
) -> Result<()> {
    use x11rb::protocol::xproto::{ChangeGCAux, ConnectionExt as _, Rectangle};
    const PALETTE: [u32; 5] = [0xc0_3030, 0x30_a030, 0x20_50c0, 0xe0_e0e0, 0x80_8080];
    let tick = TICK.load(Ordering::Relaxed) as i32;
    let (wi, hi) = (i32::from(w), i32::from(h).max(1));
    let colour = |c| ChangeGCAux::new().foreground(c);
    conn.change_gc(gc, &colour(PALETTE[(tick / 30) as usize % 5]))?;
    conn.poly_fill_rectangle(
        win,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }],
    )?;
    conn.change_gc(gc, &colour(0x10_1010))?;
    let stripes: Vec<Rectangle> = (0..24)
        .map(|i| Rectangle {
            x: 40,
            y: ((i * hi / 24 + tick * 7) % hi) as i16,
            width: (wi - 80).max(1) as u16,
            height: 10,
        })
        .collect();
    conn.poly_fill_rectangle(win, gc, &stripes)?;
    if text {
        for i in 0..30 {
            let line = format!("Rconn GPU self-test, frame {tick}, line {i}: the quick brown fox");
            conn.image_text8(
                win,
                gc,
                (60 + (tick * 3 + i * 11) % 200) as i16,
                (20 + i * 28) as i16,
                line.as_bytes(),
            )?;
        }
    }
    conn.flush()?;
    Ok(())
}

impl Drop for Animator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
