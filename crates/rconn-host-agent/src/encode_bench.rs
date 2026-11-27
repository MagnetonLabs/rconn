//! `--selftest-encode N`: the software encode path without a capturer, on any OS.
//!
//! Synthetic desktops (1920x1080 unless `RCONN_BENCH_SIZE`, e.g. `2992x1878`) go through
//! what `stream.rs` does with a captured picture: the changed areas are converted from
//! BGRA to I420 (as the capturers do), scaled to the viewer's size where the host would
//! (`RCONN_BENCH_VIEW`, default 1280x720), then encoded. Four scenarios, N frames each:
//! * idle: a blinking caret (one 2x18 area per frame);
//! * typing: one character and the caret per frame;
//! * scrolling: the whole screen moves up 7 px per frame;
//! * video: a player of two thirds the screen's size plays noisy footage.
//!
//! Frames are offered at the stream's rate (30 fps, `RCONN_BENCH_FPS`), as while someone
//! interacts. Per engine and scenario it prints the time per stage, the pictures per
//! second put out (and the most it could if it did nothing else), the CPU used
//! (100 % = one core), bytes per picture and frames the rate controller skipped.
//! Run it on the machine in question: `rconn-host-agent --selftest-encode 120`
//! (`RCONN_BENCH_QUALITY=1`: also how sharp each size policy leaves the picture,
//! [`quality_report`]).

use anyhow::{Context, Result};
use rconn_media::video::{EncoderSettings, VideoEncoder};
use rconn_media::yuv::{I420, Rect, Scaler};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// Pixels the page moves per scrolled frame.
const SCROLL_STEP: usize = 7;
/// Height of the title bar, which doesn't scroll.
const TITLE: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Idle,
    Typing,
    Scrolling,
    Video,
    /// Live bench only (`stream_live.rs`, at 60 Hz): a working session in 10 s rounds,
    /// pauses of at most 1.5 s: a scroll (2.5 s), still, drawing (3 s: a stroke growing
    /// every frame, the canvas redrawn every 0.5 s, 60 % of the screen), still, typing.
    #[cfg_attr(not(all(test, windows)), allow(dead_code))]
    Mix,
}

/// [`Scenario::Mix`]: frames per round, and where each part starts.
const MIX_ROUND: usize = 600;
const MIX_SCROLL_END: usize = 150;
const MIX_DRAW: (usize, usize) = (240, 420);
const MIX_TYPE: usize = 510;

impl Scenario {
    pub const ALL: [Scenario; 4] = [
        Scenario::Idle,
        Scenario::Typing,
        Scenario::Scrolling,
        Scenario::Video,
    ];

    fn name(self) -> &'static str {
        match self {
            Scenario::Idle => "idle",
            Scenario::Typing => "typing",
            Scenario::Scrolling => "scrolling",
            Scenario::Video => "video",
            Scenario::Mix => "mix",
        }
    }
}

/// One pixel of the page: a title bar, a sidebar, and lines of glyph-like strokes.
fn page_pixel(x: usize, y: usize) -> [u8; 4] {
    if y < TITLE {
        return [60, 50, 45, 255];
    }
    if x < 240 {
        let c = if (y / 32).is_multiple_of(2) { 238 } else { 230 };
        return [c, c, c, 255];
    }
    let (line, col) = (y / 18, (x - 240) / 9);
    let in_text = (y % 18) > 3 && (y % 18) < 15 && (col * 7 + line * 13) % 11 != 0;
    let glyph = (((x - 240) % 9) * 3 + (y % 18) * 5 + line + col) % 7 < 3;
    let c = if in_text && glyph { 30 } else { 250 };
    [c, c, c.saturating_sub((line % 3) as u8 * 20), 255]
}

/// A pixel of a lighter page, for [`Scenario::Mix`] (`RCONN_LIVE_PAGE=dense`: the one
/// above): 24 px lines of ragged length, words, letters of a few strokes; closer to a
/// real document than [`page_pixel`], whose every 9x18 cell is inked (a 2688x1686
/// keyframe of it from Microsoft's encoder: 1.67 MB; of a real desktop: 113 KB).
fn light_pixel(x: usize, y: usize) -> [u8; 4] {
    if y < TITLE || x < 240 {
        return page_pixel(x, y);
    }
    let (line, col) = (y / 24, (x - 240) / 9);
    let (cx, cy) = ((x - 240) % 9, y % 24);
    let len = 30 + (line * 37) % 90;
    let in_word = col < len && (col * 7 + line * 13) % 6 != 0 && line % 7 != 6;
    let stroke = match (col + line) % 3 {
        0 => cx == 1 || cx == 5 || cy == 13,
        1 => cx == 1 || cy == 8 || cy == 17,
        _ => cx == 3 || (cy == 8 && cx < 6),
    };
    let c = if in_word && (8..18).contains(&cy) && cx < 7 && stroke {
        40
    } else {
        250
    };
    [c, c, c, 255]
}

/// Produces the scenario's frames: the current BGRA desktop and what changed.
pub(crate) struct Desktop {
    scenario: Scenario,
    w: usize,
    h: usize,
    /// A text page taller than the screen (scrolling shows more of it), BGRA.
    page: Vec<u8>,
    screen: Vec<u8>,
    seed: u32,
}

impl Desktop {
    pub(crate) fn new(scenario: Scenario, (w, h): (usize, usize), frames: usize) -> Self {
        let rows = h + match scenario {
            Scenario::Scrolling => frames * SCROLL_STEP,
            Scenario::Mix => MIX_SCROLL_END * SCROLL_STEP,
            _ => 0,
        };
        let light =
            scenario == Scenario::Mix && std::env::var("RCONN_LIVE_PAGE").as_deref() != Ok("dense");
        let pixel = if light { light_pixel } else { page_pixel };
        let mut page = vec![0u8; w * rows * 4];
        for (y, row) in page.chunks_exact_mut(w * 4).enumerate() {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *px = pixel(x, y);
            }
        }
        let screen = page[..w * h * 4].to_vec();
        Self {
            scenario,
            w,
            h,
            page,
            screen,
            seed: 0x1234_5678,
        }
    }

    /// The desktop now (BGRA), and its size.
    #[cfg(test)]
    pub(crate) fn screen(&self) -> &[u8] {
        &self.screen
    }

    #[cfg(test)]
    pub(crate) fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    fn video_area(&self) -> Rect {
        let (vw, vh) = ((self.w * 2 / 3) & !1, (self.h * 2 / 3) & !1);
        Rect {
            x: ((self.w - vw) / 2) & !1,
            y: ((self.h - vh) / 2) & !1,
            w: vw,
            h: vh,
        }
    }

    fn fill(&mut self, r: Rect, mut px: impl FnMut(usize, usize) -> [u8; 4]) {
        let w = self.w;
        for y in r.y..r.y + r.h {
            let row = &mut self.screen[(y * w + r.x) * 4..(y * w + r.x + r.w) * 4];
            for (i, p) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *p = px(r.x + i, y);
            }
        }
    }

    /// Advance to frame `k` (from 1); the areas that changed since frame `k - 1`.
    pub(crate) fn step(&mut self, k: usize) -> Vec<Rect> {
        let caret_at = |k: usize| Rect {
            x: 240 + 9 * (10 + k % 150),
            y: 18 * 20,
            w: 2,
            h: 18,
        };
        match self.scenario {
            Scenario::Idle => {
                let r = caret_at(0);
                let on = k % 2 == 1;
                self.fill(r, |x, y| if on { [0, 0, 0, 255] } else { page_pixel(x, y) });
                vec![r]
            }
            Scenario::Typing => {
                // The character under the old caret position appears; the caret moves on.
                let old = caret_at(k - 1);
                let cell = Rect { w: 9, ..old };
                let new = caret_at(k);
                self.fill(cell, |x, y| {
                    let g = ((x * 5 + y * 3 + k) % 7) < 3 && (y % 18) > 3 && (y % 18) < 15;
                    if g {
                        [20, 20, 20, 255]
                    } else {
                        [250, 250, 250, 255]
                    }
                });
                self.fill(new, |_, _| [0, 0, 0, 255]);
                vec![cell, new]
            }
            Scenario::Scrolling => {
                // Below the title bar the page moves up.
                let (w, h) = (self.w, self.h);
                let off = k * SCROLL_STEP;
                let src = &self.page[(TITLE + off) * w * 4..(h + off) * w * 4];
                self.screen[TITLE * w * 4..].copy_from_slice(src);
                vec![Rect {
                    x: 0,
                    y: TITLE,
                    w,
                    h: h - TITLE,
                }]
            }
            Scenario::Mix => {
                let p = (k - 1) % MIX_ROUND;
                let round = (k - 1) / MIX_ROUND;
                let (w, h) = (self.w, self.h);
                if p < MIX_SCROLL_END {
                    let off = (p + 1) * SCROLL_STEP;
                    let src = &self.page[(TITLE + off) * w * 4..(h + off) * w * 4];
                    self.screen[TITLE * w * 4..].copy_from_slice(src);
                    return vec![Rect {
                        x: 0,
                        y: TITLE,
                        w,
                        h: h - TITLE,
                    }];
                }
                if (MIX_DRAW.0..MIX_DRAW.1).contains(&p) {
                    let i = p - MIX_DRAW.0;
                    // The canvas: line art around a centre that moves each redraw.
                    let canvas = Rect {
                        x: (w / 10) & !1,
                        y: (h / 8) & !1,
                        w: (w * 8 / 10) & !1,
                        h: (h * 3 / 4) & !1,
                    };
                    let mut out = Vec::new();
                    if i.is_multiple_of(30) {
                        let (cx, cy) = (
                            (canvas.x + (i * 37 + round * 101) % canvas.w) as i64,
                            (canvas.y + (i * 23 + round * 53) % canvas.h) as i64,
                        );
                        self.fill(canvas, |x, y| {
                            let d = (x as i64 - cx).pow(2) + (y as i64 - cy).pow(2);
                            let c = if (d / 900) % 9 == 0 { 40 } else { 245 };
                            [c, c, 250, 255]
                        });
                        out.push(canvas);
                    }
                    // The stroke: a 16 px pen moving 10 px a frame along a wave.
                    let x = canvas.x + 20 + (i * 10) % (canvas.w - 60);
                    let y = canvas.y
                        + canvas.h / 2
                        + ((i as f32 / 9.0).sin() * (canvas.h as f32 / 3.0)) as usize;
                    let pen = Rect {
                        x: x & !1,
                        y: y & !1,
                        w: 16,
                        h: 16,
                    };
                    self.fill(pen, |_, _| [30, 30, 200, 255]);
                    out.push(pen);
                    return out;
                }
                if p >= MIX_TYPE {
                    let cell = Rect {
                        x: 240 + 9 * (10 + p % 150),
                        y: 18 * 20,
                        w: 9,
                        h: 18,
                    };
                    self.fill(cell, |x, y| {
                        let g = ((x * 5 + y * 3 + k) % 7) < 3 && (y % 18) > 3 && (y % 18) < 15;
                        if g {
                            [20, 20, 20, 255]
                        } else {
                            [250, 250, 250, 255]
                        }
                    });
                    return vec![cell];
                }
                Vec::new()
            }
            Scenario::Video => {
                let area = self.video_area();
                let mut seed = self.seed;
                self.fill(area, |x, y| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (seed >> 24) as usize % 24;
                    [
                        ((x + k * 5) % 256 / 2 + 60 + noise) as u8,
                        ((y + k * 3) % 256 / 2 + 50 + noise) as u8,
                        (((x / 4) ^ (y / 4)) % 128 + 40 + noise) as u8,
                        255,
                    ]
                });
                self.seed = seed;
                vec![area]
            }
        }
    }
}

/// CPU seconds this process has used so far (all threads).
fn process_cpu() -> f64 {
    crate::stream::cpu_times().0
}

/// One engine's numbers for one scenario.
#[derive(Debug, Default, Clone)]
pub struct Measured {
    /// Frames handed to the encoder, and pictures it put out (the rest it skipped).
    pub frames: usize,
    pub coded: usize,
    pub convert: Duration,
    pub scale: Duration,
    pub encode: Duration,
    /// From the first counted frame to the last, paced at the offered frame rate.
    pub wall: Duration,
    pub cpu_secs: f64,
    pub bytes: usize,
    /// Keyframes among the pictures (unasked: the encoder decided).
    pub keys: usize,
}

impl Measured {
    fn per_frame(&self, d: Duration) -> f64 {
        d.as_secs_f64() * 1e3 / self.frames.max(1) as f64
    }

    /// Encoder time per picture put out (ms), skipped frames' time included.
    pub fn encode_ms(&self) -> f64 {
        self.encode.as_secs_f64() * 1e3 / self.coded.max(1) as f64
    }

    /// Pictures per second if the host did nothing but convert, scale and encode.
    pub fn max_fps(&self) -> f64 {
        self.coded as f64
            / (self.convert + self.scale + self.encode)
                .as_secs_f64()
                .max(1e-9)
    }

    /// Pictures per second put out while offered frames at the stream's rate.
    pub fn fps(&self) -> f64 {
        self.coded as f64 / self.wall.as_secs_f64().max(1e-9)
    }

    /// CPU used meanwhile (100 = one core busy).
    pub fn cpu_pct(&self) -> f64 {
        self.cpu_secs / self.wall.as_secs_f64().max(1e-9) * 100.0
    }

    pub fn skipped(&self) -> usize {
        self.frames - self.coded
    }

    pub fn kb_per_picture(&self) -> f64 {
        self.bytes as f64 / 1024.0 / self.coded.max(1) as f64
    }
}

/// An encoder set up as the host would build it, by name.
pub struct Engine {
    pub name: String,
    pub make: Box<dyn Fn() -> Result<VideoEncoder>>,
}

/// A number from the environment (by-hand runs), or `default`.
fn knob(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &f32| *v > 0.0)
        .unwrap_or(default)
}

/// `WxH` from the environment, or `default`.
fn size_knob(name: &str, default: (usize, usize)) -> Result<(usize, usize)> {
    let Ok(v) = std::env::var(name) else {
        return Ok(default);
    };
    let (w, h) = v
        .split_once('x')
        .with_context(|| format!("{name} must look like 1920x1080"))?;
    Ok((w.parse::<usize>()? & !1, h.parse::<usize>()? & !1))
}

/// The host's settings for a 30 fps stream at a typical bandwidth estimate
/// (`RCONN_BENCH_KBPS`, default 8000).
pub fn settings() -> EncoderSettings {
    EncoderSettings {
        bitrate_bps: (knob("RCONN_BENCH_KBPS", 8000.0) * 1000.0) as u32,
        max_fps: 30.0,
        ..EncoderSettings::default()
    }
}

/// How often frames are offered (`RCONN_BENCH_FPS`, default the stream's 30): lower
/// rates are what background changes get.
fn offer_fps() -> f32 {
    knob("RCONN_BENCH_FPS", settings().max_fps)
}

/// The engines to compare on this machine: openh264 as the host builds it, and the
/// platform's own encoder where there is one.
pub fn engines(openh264: &Path) -> Vec<Engine> {
    let lib = openh264.to_owned();
    let mut out = vec![Engine {
        name: "openh264".into(),
        make: Box::new(move || Ok(VideoEncoder::software(&lib, settings())?)),
    }];
    if cfg!(any(windows, target_os = "macos")) {
        // What the host picks here (a GPU's encoder where there is one).
        let lib = openh264.to_owned();
        out.push(Engine {
            name: "as the host picks it".into(),
            make: Box::new(move || Ok(VideoEncoder::new(&lib, settings())?)),
        });
    }
    #[cfg(windows)]
    {
        let lib = openh264.to_owned();
        out.push(Engine {
            name: "Media Foundation".into(),
            make: Box::new(move || {
                let mut e = VideoEncoder::new(&lib, settings())?;
                e.set_microsoft_software(true);
                Ok(e)
            }),
        });
    }
    out
}

/// Run `frames` frames of `scenario` on a `size` screen through `enc`, scaled to `to`
/// first if given.
pub fn measure(
    enc: &mut VideoEncoder,
    scenario: Scenario,
    size: (usize, usize),
    to: Option<(usize, usize)>,
    frames: usize,
) -> Result<Measured> {
    /// Frames before counting starts: the first keyframe and the rate controller settling.
    const WARMUP: usize = 15;
    let (w, h) = size;
    let mut desk = Desktop::new(scenario, size, frames + WARMUP + 1);
    let mut pic = I420::new(w, h);
    pic.fill_from_bgra(&desk.screen, w);
    let scaler = to.map(|to| Scaler::new(size, to));
    enc.set_scaled_input(to.is_some());
    let mut small = None;
    if let Some(s) = &scaler {
        s.scale(&pic, &mut small, None);
    }
    enc.encode_yuv(small.as_ref().unwrap_or(&pic), None)?;
    let interval = Duration::from_secs(1).div_f32(offer_fps());
    let mut m = Measured::default();
    let mut next = Instant::now();
    let mut started = None;
    // Rows changed since the last picture put out (a skipped frame's change rides along).
    let mut pending: Option<(usize, usize)> = None;
    for k in 1..=frames + WARMUP {
        let rects = desk.step(k);
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next = next.max(now) + interval;
        if k == WARMUP + 1 {
            m = Measured::default();
            started = Some((Instant::now(), process_cpu()));
        }
        let rows = rects.iter().fold(pending.unwrap_or((h, 0)), |(a, b), r| {
            (a.min(r.y), b.max(r.y + r.h))
        });
        let rows = (rows.0 & !1, rows.1.next_multiple_of(2).min(h));
        let t0 = Instant::now();
        pic.fill_rects_from_bgra(&desk.screen, w * 4, &rects);
        let t1 = Instant::now();
        let (input, input_rows) = match &scaler {
            Some(s) => {
                let r = s.scale(&pic, &mut small, Some(rows));
                (small.as_ref().expect("allocated"), r)
            }
            None => (&pic, rows),
        };
        let t2 = Instant::now();
        let out = enc.encode_yuv(input, Some(input_rows))?;
        m.convert += t1 - t0;
        m.scale += t2 - t1;
        m.encode += t2.elapsed();
        m.frames += 1;
        match out {
            Some(au) => {
                m.bytes += au.len();
                m.coded += 1;
                m.keys += usize::from(enc.last_frame().key);
                pending = None;
            }
            None => pending = Some(rows),
        }
    }
    if let Some((t, cpu)) = started {
        m.wall = t.elapsed();
        m.cpu_secs = process_cpu() - cpu;
    }
    Ok(m)
}

/// The whole report, as printed by `--selftest-encode`.
pub fn run(frames: usize, openh264: &Path) -> Result<String> {
    let frames = frames.max(10);
    let size = size_knob("RCONN_BENCH_SIZE", (1920, 1080))?;
    let view = size_knob("RCONN_BENCH_VIEW", (1280, 720))?;
    let to = crate::stream::view_target(size, Some(view));
    let mut out = String::new();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    writeln!(
        out,
        "screen {}x{}, viewer {}x{} (encoded at {}), {frames} frames per scenario offered at {} fps, {cores} logical CPUs, {} kbps",
        size.0,
        size.1,
        view.0,
        view.1,
        to.map_or("full size".into(), |(w, h)| format!("{w}x{h}")),
        offer_fps(),
        settings().bitrate_bps / 1000,
    )?;
    writeln!(
        out,
        "{:<44} {:<10} {:>7} {:>7} {:>7} {:>8} {:>6} {:>6} {:>8} {:>5} {:>4}",
        "engine",
        "scenario",
        "conv ms",
        "scale",
        "enc ms",
        "max fps",
        "fps",
        "CPU%",
        "KB/pic",
        "skip",
        "keys"
    )?;
    // `RCONN_BENCH_ENGINES`: engines whose name has one of these `+`-separated words.
    let only = std::env::var("RCONN_BENCH_ENGINES").ok();
    for engine in engines(openh264).into_iter().filter(|e| {
        only.as_ref()
            .is_none_or(|o| o.split('+').any(|o| e.name.contains(o)))
    }) {
        // The full screen, then (if the viewer is smaller) at the viewer's size.
        for scaled in std::iter::once(None).chain(to.map(Some)) {
            let mut name = engine.name.clone();
            for scenario in Scenario::ALL {
                let mut enc = match (engine.make)() {
                    Ok(e) => e,
                    Err(e) => {
                        writeln!(out, "{name:<44} unavailable: {e:#}")?;
                        break;
                    }
                };
                let m = measure(&mut enc, scenario, size, scaled, frames)?;
                if !enc.engine_name().is_empty() {
                    name = enc.engine_name().to_owned(); // which encoder it found
                }
                let label = match scaled {
                    Some((w, h)) => format!("{name} @{w}x{h}"),
                    None => name.clone(),
                };
                writeln!(
                    out,
                    "{label:<44} {:<10} {:>7.2} {:>7.2} {:>7.2} {:>8.1} {:>6.1} {:>6.0} {:>8.1} {:>5} {:>4}",
                    scenario.name(),
                    m.per_frame(m.convert),
                    m.per_frame(m.scale),
                    m.encode_ms(),
                    m.max_fps(),
                    m.fps(),
                    m.cpu_pct(),
                    m.kb_per_picture(),
                    m.skipped(),
                    m.keys
                )?;
            }
        }
    }
    // `RCONN_BENCH_QUALITY=1`: the size policies compared too (`quality_report`).
    if std::env::var("RCONN_BENCH_QUALITY").is_ok_and(|v| v == "1") {
        out += &quality_report(frames, openh264)?;
    }
    Ok(out)
}

/// Luma PSNR (dB) of `got` against `want` (same size).
fn luma_psnr(want: &[u8], got: &[u8]) -> f64 {
    let mse = want
        .iter()
        .zip(got)
        .map(|(&a, &b)| f64::from(a.abs_diff(b)).powi(2))
        .sum::<f64>()
        / want.len().max(1) as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

/// How the host sizes the picture and sharpens a still screen (for [`quality`]).
#[derive(Clone, Copy, Debug)]
pub enum Policy {
    /// Always at this size (`None`: the screen's). Once still: refinement passes where
    /// the host makes them, else its top-off re-encodes.
    Fixed { size: Option<(usize, usize)> },
    /// Half size while a large area keeps changing, the full screen once it has been
    /// still for a second (a keyframe), then refinement passes.
    HalfInMotion,
}

/// One policy's numbers for one scenario: motion, then the screen still.
#[derive(Debug, Default, Clone)]
pub struct Quality {
    /// Encoded sizes while it moved, and at the end.
    pub motion_size: (usize, usize),
    pub still_size: (usize, usize),
    pub fps: f64,
    pub encode_ms: f64,
    /// Longest time between two pictures out while it moved.
    pub max_gap_ms: f64,
    pub kb_per_picture: f64,
    pub keys: usize,
    /// How far behind the link (at the target bitrate) was when the motion ended.
    pub lag_ms: f64,
    /// Luma PSNR at the viewer's size: mean over the moving pictures, and at the end.
    pub psnr_motion: f64,
    pub psnr_final: f64,
    /// From the last change until the viewer has a picture within 0.5 dB of the final
    /// one, the link's backlog included.
    pub sharp_ms: f64,
    /// Bytes sent after the screen went still.
    pub still_kb: f64,
}

/// A unit out (or a skip), the time taken, and the size the picture was encoded at.
type Sized = (Option<Vec<u8>>, Duration, (usize, usize));

/// Pictures handed to the encoder at the size a policy picks.
struct Sizer {
    screen: (usize, usize),
    scaler: Option<Scaler>,
    small: Option<I420>,
    half: Option<I420>,
}

impl Sizer {
    /// Encode `pic` at `size` (`None`: as it is), or halved.
    fn encode(
        &mut self,
        enc: &mut VideoEncoder,
        pic: &I420,
        size: Option<(usize, usize)>,
        halve: bool,
        rows: Option<(usize, usize)>,
    ) -> Result<Sized> {
        let t0 = Instant::now();
        let input = if halve {
            pic.half_into(&mut self.half);
            enc.set_scaled_input(true);
            self.half.as_ref().expect("allocated")
        } else if let Some(to) = size {
            if self
                .scaler
                .as_ref()
                .is_none_or(|s| s.sizes() != (self.screen, to))
            {
                self.scaler = Some(Scaler::new(self.screen, to));
                self.small = None;
            }
            let fresh = self.small.is_none();
            self.scaler.as_ref().expect("built").scale(
                pic,
                &mut self.small,
                rows.filter(|_| !fresh),
            );
            enc.set_scaled_input(true);
            self.small.as_ref().expect("allocated")
        } else {
            enc.set_scaled_input(false);
            pic
        };
        let dims = input.dimensions();
        let au = enc.encode_yuv(input, None)?;
        Ok((au, t0.elapsed(), dims))
    }
}

/// Run `scenario` for `frames` frames at 30 fps through `enc` as `policy` sizes it, then
/// keep the screen still (up to `CONVERGE_MAX`) as the host would (refinement passes paced by
/// [`rconn_media::refine::Refiner`] on a simulated clock, or top-offs; encodes are real),
/// and score what the viewer decodes (with openh264 from `lib`).
#[allow(clippy::too_many_lines)]
pub fn quality(
    enc: &mut VideoEncoder,
    policy: Policy,
    scenario: Scenario,
    screen: (usize, usize),
    frames: usize,
    lib: &Path,
) -> Result<Quality> {
    use rconn_media::refine::Refiner;
    const WARMUP: usize = 15;
    const MOTION_SETTLE: Duration = Duration::from_secs(1);
    let (w, h) = screen;
    let target = settings().bitrate_bps;
    let send_time = |bytes: usize| Duration::from_secs_f64(bytes as f64 * 8.0 / f64::from(target));
    let mut desk = Desktop::new(scenario, screen, frames + WARMUP + 1);
    let mut pic = I420::new(w, h);
    pic.fill_from_bgra(&desk.screen, w);
    let mut sizer = Sizer {
        screen,
        scaler: None,
        small: None,
        half: None,
    };
    // (frame k, encoded at, unit) of every picture out, for the replay.
    let mut out: Vec<(usize, Instant, Vec<u8>)> = Vec::new();
    let mut refiner = Refiner::new(Instant::now());
    let size = match policy {
        Policy::Fixed { size } => size,
        Policy::HalfInMotion => None,
    };
    let interval = Duration::from_secs(1) / 30;
    let mut next = Instant::now();
    let mut started = None;
    let mut last_out: Option<Instant> = None;
    let mut q = Quality::default();
    let (mut encode_time, mut coded, mut bytes) = (Duration::ZERO, 0usize, 0usize);
    let mut link_free = Instant::now();
    let (mut streak, mut halving) = (0, false);
    for k in 0..=frames + WARMUP {
        let rects = if k == 0 { Vec::new() } else { desk.step(k) };
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next = next.max(now) + interval;
        if k == WARMUP + 1 {
            started = Some(Instant::now());
        }
        pic.fill_rects_from_bgra(&desk.screen, w * 4, &rects);
        let changed = rects.iter().map(|r| r.w * r.h).sum::<usize>() as f32 / (w * h) as f32;
        let big = changed >= 0.25;
        streak = if big { streak + 1 } else { 0 };
        if matches!(policy, Policy::HalfInMotion) && streak >= 6 {
            halving = true;
        }
        let (top, bottom) = rects
            .iter()
            .fold((h, 0), |(a, b), r| (a.min(r.y), b.max(r.y + r.h)));
        let rows = (top < bottom).then(|| (top & !1, bottom.next_multiple_of(2).min(h)));
        let (au, took, dims) = sizer.encode(enc, &pic, size, halving, rows)?;
        let t = Instant::now();
        if !rects.is_empty() {
            refiner.changed(t - took);
        }
        q.motion_size = dims;
        if let Some(au) = au {
            let info = enc.last_frame();
            refiner.sent(t, au.len(), info.qp, info.refine, info.key, target);
            if started.is_some() {
                encode_time += took;
                coded += 1;
                bytes += au.len();
                q.keys += usize::from(info.key);
                if let Some(l) = last_out {
                    q.max_gap_ms = q.max_gap_ms.max((t - l).as_secs_f64() * 1e3);
                }
                last_out = Some(t);
            }
            link_free = link_free.max(t) + send_time(au.len());
            out.push((k, t, au));
        }
    }
    let end = Instant::now();
    let wall = started.map_or(Duration::ZERO, |s| end - s);
    q.fps = coded as f64 / wall.as_secs_f64().max(1e-9);
    q.encode_ms = encode_time.as_secs_f64() * 1e3 / coded.max(1) as f64;
    q.kb_per_picture = bytes as f64 / 1024.0 / coded.max(1) as f64;
    q.lag_ms = link_free.saturating_duration_since(end).as_secs_f64() * 1e3;
    let motion_units = out.len();
    // The screen stays still: simulated time from here.
    let mut clock = end;
    let mut still_bytes = 0usize;
    let k_last = frames + WARMUP;
    q.still_size = q.motion_size;
    if halving {
        clock += MOTION_SETTLE;
        halving = false;
        let (au, took, dims) = sizer.encode(enc, &pic, None, false, None)?;
        clock += took;
        q.still_size = dims;
        if let Some(au) = au {
            let info = enc.last_frame();
            refiner.sent(clock, au.len(), info.qp, info.refine, info.key, target);
            still_bytes += au.len();
            out.push((k_last, clock, au));
        }
    }
    // As the host decides (`stream.rs`).
    let passes =
        enc.can_refine() && (q.still_size == screen || !enc.engine_name().starts_with("openh264"));
    // openh264 (camera mode): a few, 100 ms apart; others until no large picture comes.
    let counted = enc.engine_name().starts_with("openh264");
    let large = target as usize / 8 / 30 / 2;
    let mut topoffs = 0;
    let mut last_large = clock;
    while clock < end + crate::stream::CONVERGE_MAX {
        if !passes {
            if counted && topoffs == crate::stream::TOPOFF_FRAMES
                || !counted && clock - last_large >= crate::stream::CONVERGE_QUIET
            {
                break;
            }
            topoffs += 1;
            clock += if counted {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(1) / 30
            };
        } else {
            let Some(due) = refiner.due() else { break };
            clock = clock.max(due);
            let Some(qp) = refiner.next(clock) else { break };
            enc.refine_next(qp);
        }
        let (au, took, _) = sizer.encode(enc, &pic, size, halving, Some((0, 0)))?;
        clock += took;
        match au {
            Some(au) => {
                let info = enc.last_frame();
                refiner.sent(clock, au.len(), info.qp, info.refine, info.key, target);
                if au.len() >= large {
                    last_large = clock;
                }
                still_bytes += au.len();
                out.push((k_last, clock, au));
            }
            None => refiner.dropped(clock),
        }
    }
    q.still_kb = still_bytes as f64 / 1024.0;
    // Replay: decode everything, compare with the same desktop, both at the viewer's size.
    let mut dec = rconn_media::video::VideoDecoder::software(lib)?;
    let mut desk = Desktop::new(scenario, screen, frames + WARMUP + 1);
    let mut want = I420::new(w, h);
    want.fill_from_bgra(&desk.screen, w);
    let mut at = 0;
    let mut motion_psnr = Vec::new();
    let mut link_free = out.first().map_or(end, |u| u.1);
    let mut still: Vec<(Instant, f64)> = Vec::new();
    for (i, (k, t, au)) in out.iter().enumerate() {
        while at < *k {
            at += 1;
            let rects = desk.step(at);
            want.fill_rects_from_bgra(&desk.screen, w * 4, &rects);
        }
        link_free = link_free.max(*t) + send_time(au.len());
        let Some(f) = dec.decode(au)? else { continue };
        let counted = *k > WARMUP;
        let last_moving = i + 1 == motion_units;
        if !(i >= motion_units || last_moving || (counted && i % 3 == 0)) {
            continue;
        }
        // Against the screen as the encoder got it (scaled the same way): what the codec
        // lost. (Text compared across resampling grids measures the grid.)
        let dims = (f.width, f.height);
        let p = if dims == (w, h) {
            luma_psnr(want.luma(), f.y())
        } else {
            let mut at_size = None;
            if dims == ((w / 2) & !1, (h / 2) & !1) {
                want.half_into(&mut at_size);
            } else {
                Scaler::new((w, h), dims).scale(&want, &mut at_size, None);
            }
            luma_psnr(at_size.expect("allocated").luma(), f.y())
        };
        if std::env::var("RCONN_BENCH_CURVE").is_ok() && i + 1 >= motion_units {
            eprintln!(
                "  {:?} {}x{} {} B: {p:.1} dB at +{:.0} ms",
                scenario,
                dims.0,
                dims.1,
                au.len(),
                link_free.saturating_duration_since(end).as_secs_f64() * 1e3
            );
        }
        if i < motion_units && counted {
            motion_psnr.push(p);
        }
        if i >= motion_units || last_moving {
            still.push((link_free, p));
        }
    }
    q.psnr_motion = motion_psnr.iter().sum::<f64>() / motion_psnr.len().max(1) as f64;
    q.psnr_final = still.iter().map(|s| s.1).fold(0.0, f64::max);
    q.sharp_ms = still
        .iter()
        .find(|s| s.1 >= q.psnr_final - 0.5)
        .map_or(0.0, |s| {
            s.0.saturating_duration_since(end).as_secs_f64() * 1e3
        });
    Ok(q)
}

/// An encoder maker for [`quality_report`].
type Make = Box<dyn Fn() -> Result<VideoEncoder>>;

/// The quality report: each policy through each scenario, at `RCONN_BENCH_SIZE` for a
/// viewer showing it `RCONN_BENCH_VIEW` wide (`RCONN_BENCH_ENGINES`: policies whose name
/// has one of these `+`-separated words).
pub fn quality_report(frames: usize, openh264: &Path) -> Result<String> {
    let size = size_knob("RCONN_BENCH_SIZE", (1920, 1080))?;
    let view = size_knob("RCONN_BENCH_VIEW", (1280, 720))?;
    let at_view = crate::stream::view_target(size, Some(view));
    let soft = |lib: std::path::PathBuf| -> Make {
        Box::new(move || Ok(VideoEncoder::software(&lib, settings())?))
    };
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut runs: Vec<(String, Make, Policy)> = vec![
        (
            "openh264 @1216x763 (0.1.9 cap floor)".into(),
            soft(openh264.to_owned()),
            Policy::Fixed {
                size: crate::stream::view_target(size, Some((1216, 763))),
            },
        ),
        (
            "openh264 half moving, full still (B)".into(),
            soft(openh264.to_owned()),
            Policy::HalfInMotion,
        ),
        (
            "openh264 at the viewer's size".into(),
            soft(openh264.to_owned()),
            Policy::Fixed { size: at_view },
        ),
    ];
    #[cfg(windows)]
    {
        let mf = |lib: std::path::PathBuf| -> Make {
            Box::new(move || {
                let mut e = VideoEncoder::new(&lib, settings())?;
                e.set_microsoft_software(true);
                Ok(e)
            })
        };
        runs.push((
            "MF at the viewer's size".into(),
            mf(openh264.to_owned()),
            Policy::Fixed { size: at_view },
        ));
        runs.push((
            "MF width-capped".into(),
            mf(openh264.to_owned()),
            Policy::Fixed {
                size: crate::stream::fit_width(
                    size,
                    at_view,
                    Some(rconn_media::mf::software_max_width()),
                ),
            },
        ));
        runs.push((
            "MF half moving, full still".into(),
            mf(openh264.to_owned()),
            Policy::HalfInMotion,
        ));
    }
    let only = std::env::var("RCONN_BENCH_ENGINES").ok();
    let mut out = String::new();
    writeln!(
        out,
        "screen {}x{}, viewer {}x{}, {frames} frames at 30 fps then still, {} kbps; PSNR against the screen at the encoded size",
        size.0,
        size.1,
        view.0,
        view.1,
        settings().bitrate_bps / 1000
    )?;
    writeln!(
        out,
        "{:<38} {:<9} {:>9} {:>9} {:>5} {:>6} {:>6} {:>7} {:>4} {:>6} {:>6} {:>6} {:>6} {:>7}",
        "policy",
        "scenario",
        "moving",
        "still",
        "fps",
        "enc ms",
        "gap ms",
        "KB/pic",
        "keys",
        "lag ms",
        "dB mov",
        "dB end",
        "sharp",
        "still KB"
    )?;
    for (name, make, policy) in &runs {
        if only
            .as_ref()
            .is_some_and(|o| !o.split('+').any(|o| name.contains(o)))
        {
            continue;
        }
        for scenario in Scenario::ALL {
            let mut enc = make()?;
            let q = quality(&mut enc, *policy, scenario, size, frames, openh264)?;
            let dims = |(w, h): (usize, usize)| format!("{w}x{h}");
            writeln!(
                out,
                "{name:<38} {:<9} {:>9} {:>9} {:>5.1} {:>6.1} {:>6.0} {:>7.1} {:>4} {:>6.0} {:>6.1} {:>6.1} {:>6.0} {:>7.0}",
                scenario.name(),
                dims(q.motion_size),
                dims(q.still_size),
                q.fps,
                q.encode_ms,
                q.max_gap_ms,
                q.kb_per_picture,
                q.keys,
                q.lag_ms,
                q.psnr_motion,
                q.psnr_final,
                q.sharp_ms,
                q.still_kb
            )?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_change_what_they_report() {
        let (w, h) = (1280, 720);
        for s in Scenario::ALL {
            let mut d = Desktop::new(s, (w, h), 4);
            let before = d.screen.clone();
            let rects = d.step(1);
            // Every changed pixel lies inside a reported area.
            for (i, (a, b)) in before
                .as_chunks::<4>()
                .0
                .iter()
                .zip(d.screen.as_chunks::<4>().0)
                .enumerate()
            {
                if a != b {
                    let (x, y) = (i % w, i / w);
                    assert!(
                        rects.iter().any(
                            |r| (r.x..r.x + r.w).contains(&x) && (r.y..r.y + r.h).contains(&y)
                        ),
                        "{s:?}: change at {x},{y} not reported"
                    );
                }
            }
            assert_ne!(before, d.screen, "{s:?} changed nothing");
        }
    }

    /// Luma PSNR (dB) of `decoded` against `want`.
    fn psnr(want: &I420, decoded: &rconn_media::video::DecodedFrame) -> f64 {
        let mse = want
            .luma()
            .iter()
            .zip(decoded.y())
            .map(|(&a, &b)| f64::from(a.abs_diff(b)).powi(2))
            .sum::<f64>()
            / want.luma().len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    }

    /// Text scrolled at the viewer's size, then still: how sharp it is after the last
    /// scrolled picture, and after each re-encode of the still picture ("top-off").
    /// `cargo test --release -p rconn-host-agent sharpness_after_scrolling -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn sharpness_after_scrolling() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let (size, to) = ((1920, 1080), (1280, 720));
        let mut enc = VideoEncoder::software(&lib, settings()).unwrap();
        enc.set_scaled_input(true);
        let mut dec = rconn_media::video::VideoDecoder::software(&lib).unwrap();
        let mut desk = Desktop::new(Scenario::Scrolling, size, 61);
        let mut pic = I420::new(size.0, size.1);
        pic.fill_from_bgra(&desk.screen, size.0);
        let scaler = Scaler::new(size, to);
        let mut small = None;
        let interval = Duration::from_secs(1) / 30;
        let mut last = None;
        let mut send = |enc: &mut VideoEncoder, small: &I420, dec: &mut _| {
            std::thread::sleep(interval);
            if let Some(au) = enc.encode_yuv(small, None).unwrap()
                && let Some(f) = rconn_media::video::VideoDecoder::decode(dec, &au).unwrap()
            {
                last = Some(f);
            }
            last.as_ref().map(|f| psnr(small, f))
        };
        for k in 0..=60 {
            if k > 0 {
                let rects = desk.step(k);
                pic.fill_rects_from_bgra(&desk.screen, size.0 * 4, &rects);
            }
            scaler.scale(&pic, &mut small, None);
            send(&mut enc, small.as_ref().unwrap(), &mut dec);
        }
        let mut line = format!(
            "after scrolling {:.1} dB",
            send(&mut enc, small.as_ref().unwrap(), &mut dec).unwrap_or(0.0)
        );
        for k in 1..=6 {
            let p = send(&mut enc, small.as_ref().unwrap(), &mut dec).unwrap_or(0.0);
            line += &format!(", top-off {k}: {p:.1}");
        }
        println!("{line}");
    }

    /// Luma PSNR (dB) of `got` (`gw` x `gh`, upscaled bilinearly when smaller) against
    /// `want` (`w` x `h`): how close the viewer's picture is to the screen's own pixels.
    fn psnr_native(
        want: &[u8],
        (w, h): (usize, usize),
        got: &[u8],
        (gw, gh): (usize, usize),
    ) -> f64 {
        let mut se = 0.0f64;
        for y in 0..h {
            let fy = (y as f64 + 0.5) * gh as f64 / h as f64 - 0.5;
            let y0 = (fy.floor().max(0.0) as usize).min(gh - 1);
            let y1 = (y0 + 1).min(gh - 1);
            let ty = (fy - y0 as f64).clamp(0.0, 1.0);
            for x in 0..w {
                let fx = (x as f64 + 0.5) * gw as f64 / w as f64 - 0.5;
                let x0 = (fx.floor().max(0.0) as usize).min(gw - 1);
                let x1 = (x0 + 1).min(gw - 1);
                let tx = (fx - x0 as f64).clamp(0.0, 1.0);
                let p = |x: usize, y: usize| f64::from(got[y * gw + x]);
                let v = (p(x0, y0) * (1.0 - tx) + p(x1, y0) * tx) * (1.0 - ty)
                    + (p(x0, y1) * (1.0 - tx) + p(x1, y1) * tx) * ty;
                se += (f64::from(want[y * w + x]) - v).powi(2);
            }
        }
        10.0 * (255.0f64 * 255.0 / (se / (w * h) as f64).max(1e-9)).log10()
    }

    /// How the host's encoder (Microsoft's software one on a Windows host without a GPU
    /// encoder) does at each width (`RCONN_PROBE_WIDTHS`, default 2688,2704,2992) of a
    /// 2992x1878 text screen (`RCONN_LIVE_PAGE=light`: the lighter page), 8 Mbps
    /// (`RCONN_BENCH_KBPS`): its first keyframe, a still picture re-encoded 30 times, a
    /// keyframe asked for then (a switch back after motion), a 60-frame scroll (7 px a
    /// frame), and the still picture after: bytes, encode time, keyframes, and luma
    /// PSNR against the encoded picture and against the screen's own pixels.
    /// `encode-tests.exe probe_widths --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn probe_widths() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let (sw, sh) = size_knob("RCONN_BENCH_SIZE", (2992, 1878)).unwrap();
        let widths: Vec<usize> = std::env::var("RCONN_PROBE_WIDTHS")
            .unwrap_or_else(|_| "2688,2704,2992".into())
            .split(',')
            .filter_map(|v| v.parse().ok())
            .collect();
        let scroll = 60;
        println!(
            "screen {sw}x{sh}, {} kbps; per phase: KB (mean/max), ms/frame, keys, PSNR dB vs encoded (min/last), vs screen (last)",
            settings().bitrate_bps / 1000
        );
        for w in widths {
            let h = ((w * sh + sw / 2) / sw) & !1;
            let mut desk = Desktop::new(Scenario::Scrolling, (sw, sh), scroll + 4);
            if std::env::var("RCONN_LIVE_PAGE").as_deref() == Ok("light") {
                for (y, row) in desk.page.chunks_exact_mut(sw * 4).enumerate() {
                    for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = light_pixel(x, y);
                    }
                }
                let n = desk.screen.len();
                desk.screen.copy_from_slice(&desk.page[..n]);
            }
            let mut screen = I420::new(sw, sh);
            screen.fill_from_bgra(&desk.screen, sw);
            let scaler = (w != sw).then(|| Scaler::new((sw, sh), (w, h)));
            let mut scaled = None;
            let mut enc = VideoEncoder::new(&lib, settings()).unwrap();
            let mut dec = rconn_media::video::VideoDecoder::software(&lib).unwrap();
            let mut last: Option<rconn_media::video::DecodedFrame> = None;
            let mut first_engine = String::new();
            // One phase: `n` frames (`step` changes the screen before each); its line.
            let mut phase =
                |name: &str,
                 n: usize,
                 desk: &mut Desktop,
                 screen: &mut I420,
                 enc: &mut VideoEncoder,
                 key: bool,
                 step: &mut dyn FnMut(&mut Desktop) -> Vec<Rect>| {
                    let (mut bytes, mut max, mut keys, mut ms) = (0usize, 0usize, 0, 0.0);
                    let mut min_psnr = f64::MAX;
                    let mut last_psnr = 0.0;
                    let mut coded = 0;
                    for i in 0..n {
                        let t = Instant::now();
                        let rects = step(desk);
                        if !rects.is_empty() {
                            screen.fill_rects_from_bgra(&desk.screen, sw * 4, &rects);
                        }
                        let pic = match &scaler {
                            Some(s) => {
                                s.scale(screen, &mut scaled, None);
                                scaled.as_ref().unwrap()
                            }
                            None => &*screen,
                        };
                        if key && i == 0 {
                            enc.request_keyframe();
                        }
                        let t0 = Instant::now();
                        let au = enc.encode_yuv(pic, None).unwrap();
                        ms += t0.elapsed().as_secs_f64() * 1e3;
                        if first_engine.is_empty() {
                            first_engine = enc.engine_name().to_owned();
                        }
                        if let Some(au) = au {
                            coded += 1;
                            bytes += au.len();
                            max = max.max(au.len());
                            keys += usize::from(enc.last_frame().key);
                            if let Some(f) = dec.decode(&au).unwrap() {
                                last = Some(f);
                            }
                        }
                        if let Some(f) = &last
                            && f.y().len() == pic.luma().len()
                        {
                            let p = psnr_native(
                                pic.luma(),
                                pic.dimensions(),
                                f.y(),
                                (f.width, f.height),
                            );
                            min_psnr = min_psnr.min(p);
                            last_psnr = p;
                        }
                        std::thread::sleep(
                            (Duration::from_secs(1) / 30).saturating_sub(t.elapsed()),
                        );
                    }
                    let native = last.as_ref().map_or(0.0, |f| {
                        psnr_native(screen.luma(), (sw, sh), f.y(), (f.width, f.height))
                    });
                    println!(
                        "{w}x{h} {name:<10} {:>7.1}/{:>7.1} KB {:>5.1} ms {keys} keys {:>5.1}/{:>5.1} dB, native {native:>5.1} dB ({coded}/{n} coded)",
                        bytes as f64 / 1024.0 / coded.max(1) as f64,
                        max as f64 / 1024.0,
                        ms / n as f64,
                        if min_psnr == f64::MAX { 0.0 } else { min_psnr },
                        last_psnr,
                    );
                };
            let mut still = |_: &mut Desktop| Vec::new();
            phase(
                "first",
                1,
                &mut desk,
                &mut screen,
                &mut enc,
                false,
                &mut still,
            );
            phase(
                "still x30",
                30,
                &mut desk,
                &mut screen,
                &mut enc,
                false,
                &mut still,
            );
            phase(
                "key again",
                1,
                &mut desk,
                &mut screen,
                &mut enc,
                true,
                &mut still,
            );
            phase(
                "still x10",
                10,
                &mut desk,
                &mut screen,
                &mut enc,
                false,
                &mut still,
            );
            let mut k = 0;
            let mut scrolling = |d: &mut Desktop| {
                k += 1;
                d.step(k)
            };
            phase(
                "scroll x60",
                scroll,
                &mut desk,
                &mut screen,
                &mut enc,
                false,
                &mut scrolling,
            );
            phase(
                "still x30",
                30,
                &mut desk,
                &mut screen,
                &mut enc,
                false,
                &mut still,
            );
            println!("{w}x{h} engine: {first_engine}");
        }
    }

    /// Size policies compared: speed, bytes, and how sharp the viewer's picture is while
    /// it moves and after (`quality_report`).
    /// `cargo test --release -p rconn-host-agent quality -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn quality() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let frames = std::env::var("FRAMES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(60);
        println!("{}", quality_report(frames, &lib).unwrap());
    }

    /// `cargo test --release -p rconn-host-agent encode_bench -- --ignored --nocapture`
    /// (`FRAMES=n`, and the `RCONN_BENCH_*` settings above).
    #[test]
    #[ignore = "timing, run by hand"]
    fn report() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let frames = std::env::var("FRAMES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(90);
        println!("{}", run(frames, &lib).unwrap());
    }
}
