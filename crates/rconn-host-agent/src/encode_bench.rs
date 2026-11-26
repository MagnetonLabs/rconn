//! `--selftest-encode N`: the software encode path without a capturer, on any OS.
//!
//! Synthetic 1920x1080 desktops go through what `stream.rs` does with a captured picture:
//! the changed areas are converted from BGRA to I420 (as the capturers do), then the
//! picture is encoded. Four scenarios, N frames each:
//! * idle: a blinking caret (one 2x18 area per frame);
//! * typing: one character and the caret per frame;
//! * scrolling: the whole screen moves up 7 px per frame;
//! * video: a 1280x720 player in the middle plays noisy footage.
//!
//! Per engine and scenario it prints the time per stage, the frame rate the host could
//! keep if it did nothing else, the CPU used (100 % = one core) and the bytes per frame.
//! Run it on the machine in question: `rconn-host-agent --selftest-encode 120`.

use anyhow::Result;
use rconn_media::video::{EncoderSettings, VideoEncoder};
use rconn_media::yuv::{I420, Rect};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

const W: usize = 1920;
const H: usize = 1080;
/// Pixels the page moves per scrolled frame.
const SCROLL_STEP: usize = 7;
/// The video player's area.
const VIDEO: Rect = Rect {
    x: 320,
    y: 180,
    w: 1280,
    h: 720,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Idle,
    Typing,
    Scrolling,
    Video,
}

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
        }
    }
}

/// A text page taller than the screen (scrolling shows more of it), BGRA.
fn page(rows: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; W * rows * 4];
    for (y, row) in bgra.as_chunks_mut::<{ W * 4 }>().0.iter_mut().enumerate() {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = page_pixel(x, y);
        }
    }
    bgra
}

/// One pixel of the page: a title bar, a sidebar, and lines of glyph-like strokes.
fn page_pixel(x: usize, y: usize) -> [u8; 4] {
    if y < 40 {
        return [60, 50, 45, 255]; // title bar (fixed: `page` rows below it scroll)
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

/// Produces the scenario's frames: the current BGRA desktop and what changed.
struct Desktop {
    scenario: Scenario,
    page: Vec<u8>,
    screen: Vec<u8>,
    seed: u32,
}

impl Desktop {
    fn new(scenario: Scenario, frames: usize) -> Self {
        let rows = H + if scenario == Scenario::Scrolling {
            frames * SCROLL_STEP
        } else {
            0
        };
        let page = page(rows);
        let screen = page[..W * H * 4].to_vec();
        Self {
            scenario,
            page,
            screen,
            seed: 0x1234_5678,
        }
    }

    fn fill(&mut self, r: Rect, mut px: impl FnMut(usize, usize) -> [u8; 4]) {
        for y in r.y..r.y + r.h {
            let row = &mut self.screen[(y * W + r.x) * 4..(y * W + r.x + r.w) * 4];
            for (i, p) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *p = px(r.x + i, y);
            }
        }
    }

    /// Advance to frame `k` (from 1); the areas that changed since frame `k - 1`.
    fn step(&mut self, k: usize) -> Vec<Rect> {
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
                let off = k * SCROLL_STEP;
                let top = 40;
                let src = &self.page[(top + off) * W * 4..(H + off) * W * 4];
                self.screen[top * W * 4..].copy_from_slice(src);
                vec![Rect {
                    x: 0,
                    y: top,
                    w: W,
                    h: H - top,
                }]
            }
            Scenario::Video => {
                let mut seed = self.seed;
                self.fill(VIDEO, |x, y| {
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
                vec![VIDEO]
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
    pub encode: Duration,
    /// From the first counted frame to the last, paced at the stream's frame rate.
    pub wall: Duration,
    pub cpu_secs: f64,
    pub bytes: usize,
}

impl Measured {
    /// Conversion per frame handed over (ms).
    pub fn convert_ms(&self) -> f64 {
        self.convert.as_secs_f64() * 1e3 / self.frames.max(1) as f64
    }

    /// Encoder time per picture put out (ms), skipped frames' time included.
    pub fn encode_ms(&self) -> f64 {
        self.encode.as_secs_f64() * 1e3 / self.coded.max(1) as f64
    }

    /// Pictures per second if the host did nothing but convert and encode.
    pub fn max_fps(&self) -> f64 {
        self.coded as f64 / (self.convert + self.encode).as_secs_f64().max(1e-9)
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
        name: "openh264 (screen mode)".into(),
        make: Box::new(move || Ok(VideoEncoder::software(&lib, settings())?)),
    }];
    let lib = openh264.to_owned();
    out.push(Engine {
        name: "openh264 (camera mode, RCONN_OPENH264_LTR=1)".into(),
        make: Box::new(move || {
            let mut e = VideoEncoder::software(&lib, settings())?;
            e.set_long_term_refs(true);
            Ok(e)
        }),
    });
    if cfg!(any(windows, target_os = "macos")) {
        let lib = openh264.to_owned();
        out.push(Engine {
            name: if cfg!(windows) {
                "Media Foundation (as the host picks it)".into()
            } else {
                "VideoToolbox (hardware; for reference)".into()
            },
            make: Box::new(move || Ok(VideoEncoder::new(&lib, settings())?)),
        });
    }
    out
}

/// Frames before counting starts: the first keyframe and the rate controller settling.
const WARMUP: usize = 15;

/// Run `frames` frames of `scenario` through `enc`, offered at the stream's frame rate
/// (as fast as possible when the encoder can't keep up), as `stream.rs` does while
/// someone interacts.
pub fn measure(enc: &mut VideoEncoder, scenario: Scenario, frames: usize) -> Result<Measured> {
    let mut desk = Desktop::new(scenario, frames + WARMUP + 1);
    let mut pic = I420::new(W, H);
    pic.fill_from_bgra(&desk.screen, W);
    enc.encode_yuv(&pic, None)?;
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
        let rows = rects.iter().fold(pending.unwrap_or((H, 0)), |(a, b), r| {
            (a.min(r.y), b.max(r.y + r.h))
        });
        let rows = (rows.0 & !1, rows.1.next_multiple_of(2).min(H));
        let t0 = Instant::now();
        pic.fill_rects_from_bgra(&desk.screen, W * 4, &rects);
        let t1 = Instant::now();
        let out = enc.encode_yuv(&pic, Some(rows))?;
        m.convert += t1 - t0;
        m.encode += t1.elapsed();
        m.frames += 1;
        match out {
            Some(au) => {
                m.bytes += au.len();
                m.coded += 1;
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
    let mut out = String::new();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    writeln!(
        out,
        "{W}x{H}, {frames} frames per scenario offered at {} fps, {cores} logical CPUs, {} kbps, encoder max {} fps",
        offer_fps(),
        settings().bitrate_bps / 1000,
        settings().max_fps
    )?;
    writeln!(
        out,
        "{:<46} {:<10} {:>8} {:>8} {:>8} {:>7} {:>6} {:>8} {:>5}",
        "engine", "scenario", "conv ms", "enc ms", "max fps", "fps", "CPU%", "KB/pic", "skip"
    )?;
    for engine in engines(openh264) {
        let mut name = engine.name.clone();
        for scenario in Scenario::ALL {
            let mut enc = match (engine.make)() {
                Ok(e) => e,
                Err(e) => {
                    writeln!(out, "{name:<46} unavailable: {e:#}")?;
                    break;
                }
            };
            let m = measure(&mut enc, scenario, frames)?;
            if name.starts_with("Media Foundation") {
                name = enc.engine_name().to_owned(); // which MFT it found
            }
            writeln!(
                out,
                "{name:<46} {:<10} {:>8.2} {:>8.2} {:>8.1} {:>7.1} {:>6.0} {:>8.1} {:>5}",
                scenario.name(),
                m.convert_ms(),
                m.encode_ms(),
                m.max_fps(),
                m.fps(),
                m.cpu_pct(),
                m.kb_per_picture(),
                m.skipped()
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
        for s in Scenario::ALL {
            let mut d = Desktop::new(s, 4);
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
                    let (x, y) = (i % W, i / W);
                    assert!(
                        rects
                            .iter()
                            .any(|r| (r.x..r.x + r.w).contains(&x) && (r.y..r.y + r.h).contains(&y)),
                        "{s:?}: change at {x},{y} not reported"
                    );
                }
            }
            assert_ne!(before, d.screen, "{s:?} changed nothing");
        }
    }

    /// `cargo test --release -p rconn-host-agent encode_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn report() {
        let Some(lib) = rconn_media::cisco::local_binary_path() else {
            return;
        };
        let frames = std::env::var("FRAMES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(90);
        println!("{}", run(frames, &lib).unwrap());
    }
}
