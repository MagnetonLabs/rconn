//! What refinement buys on a text screen, measured: the viewer's picture against the
//! screen before and after the passes, and what the passes cost.
//!
//! `cargo test --release -p rconn-media refine::measure -- --nocapture` prints the
//! numbers (`--ignored` adds VideoToolbox, in real time).
//!
//! The stream is simulated on a virtual clock: typing (or scrolling) at 30 frames a
//! second, then a still screen, with the passes [`Refiner`] asks for, when it asks.

use super::*;
use crate::copyrect::{self, Composer, Compositor};
use crate::video::{DecodedFrame, EncoderSettings, Frame, PixelLayout, VideoDecoder, VideoEncoder};
use crate::yuv::I420;
use openh264::formats::YUVSource;

/// A text-heavy screen: lines of glyph-like 5x9 bitmaps in 7x14 cells, with an
/// anti-aliasing halo, in several colours on white (left half) and in light colours on a
/// dark editor pane (right half). `typed` glyphs of line 3 are shown (typing); the text
/// is scrolled up by `scroll` pixels.
pub(crate) fn text_screen(w: usize, h: usize, typed: usize, scroll: usize) -> Frame {
    let hash = |a: usize, b: usize| {
        let mut x = (a as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (b as u64);
        x ^= x >> 29;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^ (x >> 32)
    };
    let ink_on_white = [
        [20, 20, 20],
        [160, 40, 30],
        [30, 30, 170],
        [20, 120, 40],
        [120, 30, 140],
    ];
    let ink_on_dark = [
        [220, 220, 220],
        [240, 200, 90],
        [120, 200, 250],
        [200, 130, 220],
    ];
    let mut bgra = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let ty = y + scroll;
            let (line, col) = (ty / 14, x / 7);
            let (gx, gy) = (x % 7, ty % 14);
            let dark = x >= w / 2;
            let bg = if dark { [30, 30, 36] } else { [250, 250, 250] };
            // Words of 2..6 glyphs separated by spaces; lines of varied length.
            let word = hash(line, col / 6);
            let blank = (col % 6) >= 2 + (word % 5) as usize
                || (col * 7 % (w / 2)) > (w / 2) * (3 + (hash(line, 99) % 5) as usize) / 8
                || (line == 3 && col >= typed && !dark);
            let palette: &[[u8; 3]] = if dark { &ink_on_dark } else { &ink_on_white };
            let ink = palette[(word >> 8) as usize % palette.len()];
            let glyph = hash(line * 1000 + col, 7);
            let on = |dx: isize, dy: isize| {
                let (px, py) = (gx as isize - 1 + dx, gy as isize - 3 + dy);
                (0..5).contains(&px)
                    && (0..9).contains(&py)
                    && (glyph >> ((px * 9 + py) % 64)) & 1 == 1
            };
            let c = if blank {
                bg
            } else if on(0, 0) {
                ink
            } else if on(1, 0) || on(-1, 0) || on(0, 1) || on(0, -1) {
                // Anti-aliased edge: a third of the way to the ink.
                std::array::from_fn(|i| ((2 * bg[i] as u16 + ink[i] as u16) / 3) as u8)
            } else {
                bg
            };
            let o = (y * w + x) * 4;
            bgra[o..o + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
        }
    }
    Frame {
        width: w,
        height: h,
        bgra,
        captured_at: Instant::now(),
    }
}

fn psnr(mse: f64) -> f64 {
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// The viewer's picture against the screen's: (luma PSNR, PSNR over all three planes,
/// largest luma error).
pub(crate) fn quality(src: &I420, out: &DecodedFrame) -> (f64, f64, u8) {
    assert_eq!(out.layout, PixelLayout::I420);
    let se = |a: &[u8], b: &[u8]| -> (f64, u8) {
        a.iter().zip(b).fold((0.0, 0), |(s, m), (&p, &q)| {
            let d = p.abs_diff(q);
            (s + f64::from(d) * f64::from(d), m.max(d))
        })
    };
    let (y, max) = se(src.y(), out.y());
    let (u, _) = se(src.u(), out.chroma1());
    let (v, _) = se(src.v(), out.chroma2());
    let n = src.y().len() as f64;
    (psnr(y / n), psnr((y + u + v) / (n * 1.5)), max)
}

/// The stream as the host sends it: an encoder, with copy instructions or without.
struct Host {
    enc: VideoEncoder,
    composer: Option<Composer>,
    refiner: Refiner,
    bps: u32,
}

/// The stream as the viewer shows it.
struct Viewer {
    dec: VideoDecoder,
    compositor: Compositor,
    shown: Option<DecodedFrame>,
}

impl Viewer {
    fn show(&mut self, au: &[u8]) {
        let (h264, ops) = copyrect::split(au);
        if let Some(mut f) = self.dec.decode(&h264).unwrap() {
            assert_eq!(
                self.compositor.compose(ops.as_ref(), &mut f),
                copyrect::Composed::Show
            );
            self.shown = Some(f);
        }
    }
}

#[derive(Debug, Default)]
struct Report {
    /// Bytes of the frames while the screen changed.
    changing_bytes: usize,
    /// Quality when the screen stopped changing, and after the passes.
    before: (f64, f64, u8),
    after: (f64, f64, u8),
    /// (QP, bytes, luma PSNR after it) per pass.
    passes: Vec<(u8, usize, f64)>,
    /// From the last change to the last pass.
    secs: f64,
    keyframes: u32,
    /// Time spent encoding the passes.
    encode: Duration,
}

impl Host {
    fn new(lib: &std::path::Path, bps: u32, copy_rects: bool, hardware: bool) -> Self {
        let settings = EncoderSettings {
            bitrate_bps: bps,
            ..EncoderSettings::default()
        };
        // Otherwise openh264 (Windows without a GPU would pick Microsoft's software
        // encoder, which makes no refinement passes).
        let enc = if hardware {
            VideoEncoder::new(lib, settings).unwrap()
        } else {
            VideoEncoder::software(lib, settings).unwrap()
        };
        Self {
            enc,
            composer: copy_rects.then(Composer::new),
            refiner: Refiner::new(Instant::now()),
            bps,
        }
    }

    /// Encode `pic` (at `now`): a change, or a refinement pass at `refine`.
    fn encode(&mut self, pic: &I420, now: Instant, refine: Option<u8>) -> Option<Vec<u8>> {
        if let Some(qp) = refine {
            self.enc.refine_next(qp);
            if let Some(c) = self.composer.as_mut() {
                c.request_full();
            }
        }
        let au = match self.composer.as_mut() {
            Some(c) => {
                let (canvas, found) = c.prepare(pic, &[]);
                match self.enc.encode_yuv(canvas, found.rows).unwrap() {
                    Some(au) => Some(c.sent(pic, au)),
                    None => {
                        c.no_output(true);
                        None
                    }
                }
            }
            None => self.enc.encode_yuv(pic, None).unwrap(),
        };
        let info = self.enc.last_frame();
        match &au {
            Some(au) => self
                .refiner
                .sent(now, au.len(), info.qp, info.refine, info.key, self.bps),
            None if refine.is_some() => self.refiner.dropped(now),
            None => {}
        }
        au
    }
}

/// `frames` changing pictures at 30 fps, then still until the passes end (in virtual
/// time; `real_time` sleeps instead, for encoders that keep their own clock).
fn run(
    host: &mut Host,
    viewer: &mut Viewer,
    frames: impl Iterator<Item = Frame>,
    real_time: bool,
) -> (Report, I420) {
    let mut report = Report::default();
    let t0 = Instant::now();
    let mut t = t0;
    let mut pic: Option<I420> = None;
    let tick = Duration::from_millis(33);
    let wait = |t: &mut Instant, until: Instant| {
        if real_time {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            *t = Instant::now();
        } else {
            *t = until;
        }
    };
    // A change the rate controller dropped is owed: the stream sends it with the next
    // frame, or on its own once things are still.
    let mut owed = false;
    let mut last_change = t;
    let mut frames = frames.peekable();
    while frames.peek().is_some() || owed {
        if let Some(f) = frames.next() {
            let p = pic.get_or_insert_with(|| I420::new(f.width, f.height));
            p.fill_from_bgra(&f.bgra, f.width);
            host.refiner.changed(t);
            last_change = t;
        }
        let p = pic.as_ref().expect("a frame");
        owed = true;
        if let Some(au) = host.encode(p, t, None) {
            report.changing_bytes += au.len();
            report.keyframes += u32::from(host.enc.last_frame().key);
            viewer.show(&au);
            owed = false;
        }
        let next = t + tick;
        wait(&mut t, next);
    }
    let pic = pic.expect("frames");
    report.before = quality(&pic, viewer.shown.as_ref().unwrap());
    let mut last_pass = last_change;
    while let Some(due) = host.refiner.due() {
        let until = due.max(t);
        wait(&mut t, until);
        let Some(qp) = host.refiner.next(t) else {
            continue;
        };
        let t1 = Instant::now();
        let au = host.encode(&pic, t, Some(qp));
        report.encode += t1.elapsed();
        if let Some(au) = au {
            let info = host.enc.last_frame();
            report.keyframes += u32::from(info.key);
            assert!(info.refine && !info.key, "passes are P-frames");
            viewer.show(&au);
            let q = quality(&pic, viewer.shown.as_ref().unwrap());
            report.passes.push((info.qp.unwrap_or(qp), au.len(), q.0));
        } else {
            report.passes.push((qp, 0, f64::NAN));
        }
        last_pass = t;
    }
    report.after = quality(&pic, viewer.shown.as_ref().unwrap());
    report.secs = (last_pass - last_change).as_secs_f64();
    (report, pic)
}

fn print(what: &str, r: &Report) {
    let extra: usize = r.passes.iter().map(|p| p.1).sum();
    println!(
        "{what}: luma PSNR {:.2} -> {:.2} dB (all planes {:.2} -> {:.2}, max luma error {} -> {}); \
         {} passes, {} bytes ({:.0}% of the {} bytes sent while changing), done {:.2} s after the last change, {:.1} ms encoding per pass; \
         passes (QP, bytes, luma dB): {:?}",
        r.before.0,
        r.after.0,
        r.before.1,
        r.after.1,
        r.before.2,
        r.after.2,
        r.passes.len(),
        extra,
        extra as f64 * 100.0 / r.changing_bytes.max(1) as f64,
        r.changing_bytes,
        r.secs,
        r.encode.as_secs_f64() * 1000.0 / r.passes.len().max(1) as f64,
        r.passes
            .iter()
            .map(|&(q, b, p)| (q, b, (p * 100.0).round() / 100.0))
            .collect::<Vec<_>>()
    );
}

fn viewer(lib: &std::path::Path) -> Viewer {
    Viewer {
        dec: VideoDecoder::software(lib).unwrap(),
        compositor: Compositor::new(),
        shown: None,
    }
}

/// Typing on a 1280x800 text screen at 1 and 4 Mbit/s, then still (openh264).
#[test]
fn typing_then_still() {
    let Some(lib) = crate::cisco::local_binary_path() else {
        return;
    };
    let (w, h) = (1280, 800);
    for bps in [1_000_000, 4_000_000] {
        let mut host = Host::new(&lib, bps, false, false);
        let mut view = viewer(&lib);
        let typing = (0..30).map(|k| text_screen(w, h, 40 + k, 0));
        let (r, _) = run(&mut host, &mut view, typing, false);
        print(&format!("openh264 typing, {} kbit/s", bps / 1000), &r);
        assert_eq!(r.keyframes, 1, "only the first frame");
        assert!(r.after.0 >= r.before.0, "{r:?}");
        if r.before.0 < 40.0 {
            assert!(r.after.0 > r.before.0 + 5.0, "{r:?}");
        }
        assert!(r.passes.len() as u32 <= MAX_PASSES);
        assert!(r.passes.iter().all(|p| p.0 >= FLOOR_QP));
        // Paced: never faster than the target rate.
        let extra: usize = r.passes.iter().map(|p| p.1).sum();
        let first_pass_at = SETTLE.as_secs_f64();
        let on_the_wire =
            (extra - r.passes.last().map_or(0, |p| p.1)) as f64 * 8.0 / f64::from(bps);
        assert!(r.secs + 0.05 >= first_pass_at.max(on_the_wire), "{r:?}");
    }
}

/// Scrolling with copy instructions (the Windows default): moved text stays as sharp as
/// when it was first sent, and the strips scrolled in come at the rate controller's
/// quantizer. The passes make the canvas the screen picture, shown whole, and sharpen it.
#[test]
fn scrolling_with_copy_instructions_then_still() {
    let Some(lib) = crate::cisco::local_binary_path() else {
        return;
    };
    let (w, h) = (1280, 800);
    let bps = 2_000_000;
    let mut host = Host::new(&lib, bps, true, false);
    let mut view = viewer(&lib);
    let scrolling = (0..30).map(|k| text_screen(w, h, 60, k * 14));
    let (r, _) = run(&mut host, &mut view, scrolling, false);
    print(
        &format!(
            "openh264 + copy instructions, scrolling, {} kbit/s",
            bps / 1000
        ),
        &r,
    );
    assert_eq!(r.keyframes, 1, "only the first frame");
    assert!(r.after.0 > r.before.0 + 3.0, "{r:?}");
}

/// Off (`RCONN_REFINE=0` in the host): what the viewer is left with is what the last
/// change gave it; the same as [`Report::before`].
#[test]
fn without_refinement_the_picture_stays_coarse() {
    let Some(lib) = crate::cisco::local_binary_path() else {
        return;
    };
    let (w, h) = (640, 400);
    let mut host = Host::new(&lib, 300_000, false, false);
    let mut view = viewer(&lib);
    let typing = (0..10).map(|k| text_screen(w, h, 20 + k, 0));
    let mut pic = I420::new(w, h);
    let t = Instant::now();
    for f in typing {
        pic.fill_from_bgra(&f.bgra, w);
        if let Some(au) = host.encode(&pic, t, None) {
            view.show(&au);
        }
    }
    let q = quality(&pic, view.shown.as_ref().unwrap());
    assert!(q.0 < 40.0, "coarse at 300 kbit/s: {q:?}");
}

/// VideoToolbox (hardware where there is one), in real time: its rate controller keeps
/// its own clock and drops passes that would break its one-second data rate limit.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "real time, hardware: run by hand"]
fn videotoolbox_typing_then_still() {
    let Some(lib) = crate::cisco::local_binary_path() else {
        return;
    };
    let (w, h) = (1280, 800);
    for bps in [1_000_000, 4_000_000] {
        let mut host = Host::new(&lib, bps, false, true);
        let mut view = viewer(&lib);
        let typing = (0..30).map(|k| text_screen(w, h, 40 + k, 0));
        let (r, _) = run(&mut host, &mut view, typing, true);
        print(
            &format!("{} typing, {} kbit/s", host.enc.engine_name(), bps / 1000),
            &r,
        );
        assert!(r.after.0 >= r.before.0, "{r:?}");
    }
}
