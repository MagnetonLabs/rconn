//! Scrolling and window moves at 1920x1080: full pictures (the encoder finds the motion)
//! against copy instructions plus the uncovered strip.
//!
//! `RCONN_SCROLL_PAGES=<dir> cargo test --release -p rconn-media scroll_bench -- --ignored --nocapture`
//!
//! `<dir>` holds pages rendered with real fonts: `<name>.page` (tall BGRA content),
//! `<name>.chrome` (1920x1080 BGRA window around it) and `<name>.spec` (`page W H`,
//! `viewport X Y W H`, `scrollbar X W`). Without it, a synthetic text page is used.

use super::*;
use crate::video::{EncoderSettings, VideoDecoder, VideoEncoder};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, QpRange,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use std::time::Instant;

const W: usize = 1920;
const H: usize = 1080;

struct Page {
    name: String,
    chrome: Vec<u8>,
    content: Vec<u8>,
    content_w: usize,
    content_h: usize,
    viewport: Rect,
    scrollbar: (usize, usize),
}

fn load(dir: &std::path::Path, name: &str) -> Option<Page> {
    let spec = std::fs::read_to_string(dir.join(format!("{name}.spec"))).ok()?;
    let mut v: HashMap<&str, Vec<usize>> = HashMap::new();
    for line in spec.lines() {
        let mut it = line.split_whitespace();
        let key = it.next()?;
        v.insert(key, it.map(|n| n.parse().unwrap()).collect());
    }
    let (p, vp, bar) = (&v["page"], &v["viewport"], &v["scrollbar"]);
    Some(Page {
        name: name.into(),
        chrome: std::fs::read(dir.join(format!("{name}.chrome"))).ok()?,
        content: std::fs::read(dir.join(format!("{name}.page"))).ok()?,
        content_w: p[0],
        content_h: p[1],
        viewport: Rect {
            x: vp[0],
            y: vp[1],
            w: vp[2],
            h: vp[3],
        },
        scrollbar: (bar[0], bar[1]),
    })
}

/// Dark text on white in lines of "words" made of glyph-like 7x12 bitmaps.
fn synthetic() -> Page {
    let (cw, ch) = (1600, 8000);
    let mut content = vec![255u8; cw * ch * 4];
    let mut seed = 0x2545_f491_u32;
    let mut rand = |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed % n
    };
    let glyphs: Vec<[u16; 12]> = (0..64)
        .map(|_| std::array::from_fn(|r| if r < 2 || r > 10 { 0 } else { rand(128) as u16 }))
        .collect();
    for line in 0..ch / 20 {
        let (y0, mut x) = (line * 20 + 4, 40 + 32 * rand(4) as usize);
        let end = 200 + rand(1300) as usize;
        while x + 8 < end.min(cw) {
            for _ in 0..1 + rand(9) {
                let g = &glyphs[rand(64) as usize];
                for (r, bits) in g.iter().enumerate() {
                    for c in 0..7 {
                        if bits >> c & 1 == 1 {
                            let i = ((y0 + r) * cw + x + c) * 4;
                            content[i..i + 3].copy_from_slice(&[40, 30, 30]);
                        }
                    }
                }
                x += 8;
            }
            x += 8;
        }
    }
    let mut chrome = vec![0u8; W * H * 4];
    for (i, px) in chrome.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % W, i / W);
        *px = [(60 + x / 40) as u8, (60 + y / 40) as u8, 70, 255];
    }
    Page {
        name: "synthetic".into(),
        chrome,
        content,
        content_w: cw,
        content_h: ch,
        viewport: Rect {
            x: 300,
            y: 40,
            w: 1600,
            h: 1020,
        },
        scrollbar: (1904, 16),
    }
}

fn pages() -> Vec<Page> {
    match std::env::var_os("RCONN_SCROLL_PAGES") {
        Some(dir) => ["editor", "browser"]
            .iter()
            .filter_map(|n| load(std::path::Path::new(&dir), n))
            .collect(),
        None => vec![synthetic()],
    }
}

/// The screen with the page scrolled to `offset`, BGRA.
fn render(p: &Page, offset: usize, out: &mut [u8]) {
    out.copy_from_slice(&p.chrome);
    let vp = p.viewport;
    for y in 0..vp.h {
        let s = ((offset + y) * p.content_w) * 4;
        let d = ((vp.y + y) * W + vp.x) * 4;
        out[d..d + vp.w * 4].copy_from_slice(&p.content[s..s + vp.w * 4]);
    }
    let thumb = vp.h * vp.h / p.content_h;
    let top = vp.y + offset * (vp.h - thumb) / (p.content_h - vp.h);
    for y in vp.y..vp.y + vp.h {
        let c = if (top..top + thumb).contains(&y) {
            160
        } else {
            238
        };
        for x in p.scrollbar.0..(p.scrollbar.0 + p.scrollbar.1).min(W) {
            out[(y * W + x) * 4..][..4].copy_from_slice(&[c, c, c, 255]);
        }
    }
}

/// A window (part of the page) dragged across a smooth wallpaper; with its true move.
fn render_drag(p: &Page, pos: (usize, usize), out: &mut [u8]) {
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % W, i / W);
        *px = [(90 + x / 16) as u8, (40 + y / 8) as u8, 120, 255];
    }
    let (ww, wh) = (1000, 640);
    for y in 0..wh {
        let d = ((pos.1 + y) * W + pos.0) * 4;
        if y < 30 {
            out[d..d + ww * 4].fill(210);
        } else {
            let s = ((2000 + y) * p.content_w) * 4;
            out[d..d + ww * 4].copy_from_slice(&p.content[s..s + ww * 4]);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Whole pictures; the encoder finds the motion (how Rconn streams today).
    Full,
    /// Copy instructions found by row hashing (macOS, Linux).
    CopyRect,
    /// Copy instructions from the OS's move rectangles (Windows DXGI).
    CopyRectHints,
}

#[derive(Clone, Copy, Debug)]
enum Rc {
    /// Constant quantizer: bytes show what the content costs.
    Qp(u8),
    /// Rconn's encoder settings at this bitrate: quality and dropped frames show it.
    Rate(u32),
}

enum Enc {
    Rate(Box<VideoEncoder>),
    Qp(Box<Encoder>),
}

impl Enc {
    fn new(lib: &std::path::Path, rc: Rc) -> Enc {
        match rc {
            Rc::Rate(bps) => Enc::Rate(Box::new(
                VideoEncoder::new(
                    lib,
                    EncoderSettings {
                        bitrate_bps: bps,
                        max_fps: 30.0,
                        threads: 0,
                        slice_bytes: None,
                    },
                )
                .unwrap(),
            )),
            Rc::Qp(q) => {
                let api = crate::codec::load_openh264(lib).unwrap();
                let cfg = EncoderConfig::new()
                    .usage_type(UsageType::ScreenContentRealTime)
                    .rate_control_mode(RateControlMode::Off)
                    .bitrate(BitRate::from_bps(50_000_000))
                    .max_frame_rate(FrameRate::from_hz(30.0))
                    .complexity(Complexity::Low)
                    .qp(QpRange::new(q, q))
                    .adaptive_quantization(false)
                    .background_detection(false)
                    .skip_frames(false)
                    .intra_frame_period(IntraFramePeriod::from_num_frames(0))
                    .sps_pps_strategy(SpsPpsStrategy::ConstantId);
                Enc::Qp(Box::new(Encoder::with_api_config(api, cfg).unwrap()))
            }
        }
    }

    fn encode(&mut self, pic: &I420) -> Option<Vec<u8>> {
        match self {
            Enc::Rate(e) => e.encode_yuv(pic, None).unwrap(),
            Enc::Qp(e) => Some(e.encode(pic).unwrap().to_vec()).filter(|v| !v.is_empty()),
        }
    }
}

#[derive(Default)]
struct Outcome {
    /// Bytes per frame (0 = dropped), frame 0 (the keyframe) excluded.
    bytes: Vec<usize>,
    /// PSNR of what the viewer shows against the screen, per frame.
    psnr: Vec<f64>,
    dropped: usize,
    encode_ms: f64,
    prepare_ms: f64,
    resyncs: usize,
    moved: f64,
    encoded: f64,
}

fn psnr(screen_bgra: &[u8], shown_rgba: &[u8]) -> f64 {
    let mut se = 0u64;
    for (s, d) in screen_bgra
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shown_rgba.as_chunks::<4>().0)
    {
        for (a, b) in [(s[2], d[0]), (s[1], d[1]), (s[0], d[2])] {
            let e = i64::from(a) - i64::from(b);
            se += (e * e) as u64;
        }
    }
    let mse = se as f64 / (screen_bgra.len() / 4 * 3) as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Stream `frames` (BGRA screens, with the moves the OS would report) and measure.
fn run(
    lib: &std::path::Path,
    frames: &mut dyn FnMut(usize, &mut [u8]) -> Vec<Move>,
    n: usize,
    mode: Mode,
    rc: Rc,
    label: &str,
) -> Outcome {
    use std::io::Write;
    // RCONN_SCROLL_DUMP=<dir>: the pictures given to the encoder, as raw I420, for
    // trying other encoders on them.
    let mut dump = std::env::var_os("RCONN_SCROLL_DUMP")
        .filter(|_| matches!(rc, Rc::Qp(_)))
        .map(|d| {
            let name = format!("{label} {mode:?}.yuv").replace(' ', "_");
            std::fs::File::create(std::path::Path::new(&d).join(name)).unwrap()
        });
    let mut save = |p: &I420| {
        if let Some(f) = dump.as_mut() {
            for plane in [p.y(), p.u(), p.v()] {
                f.write_all(plane).unwrap();
            }
        }
    };
    let mut enc = Enc::new(lib, rc);
    let mut dec = VideoDecoder::software(lib).unwrap();
    let mut composer = Composer::new();
    let mut compositor = Compositor::new();
    let mut screen = vec![0u8; W * H * 4];
    let mut pic = I420::new(W, H);
    let mut shown: Option<Vec<u8>> = None;
    let mut out = Outcome::default();
    let mut hints = Vec::new();
    for i in 0..n {
        hints.extend(frames(i, &mut screen));
        pic.fill_from_bgra(&screen, W);
        let t = Instant::now();
        let au = match mode {
            Mode::Full => {
                save(&pic);
                enc.encode(&pic)
            }
            Mode::CopyRect | Mode::CopyRectHints => {
                let given = if matches!(mode, Mode::CopyRectHints) {
                    std::mem::take(&mut hints)
                } else {
                    hints.clear();
                    Vec::new()
                };
                let t = Instant::now();
                let canvas = composer.prepare(&pic, &given);
                out.prepare_ms += t.elapsed().as_secs_f64() * 1e3;
                save(canvas);
                let au = enc.encode(canvas);
                let stats = composer.last();
                out.moved += f64::from(stats.moved);
                out.encoded += f64::from(stats.encoded);
                match au {
                    Some(au) => Some(composer.sent(&pic, au)),
                    None => {
                        composer.no_output(true);
                        None
                    }
                }
            }
        };
        out.encode_ms += t.elapsed().as_secs_f64() * 1e3;
        if std::env::var_os("RCONN_SCROLL_TRACE").is_some() {
            let bytes = au.as_ref().map_or(0, Vec::len);
            eprintln!("{label} {mode:?} {i}: {:?} {bytes} B", composer.last());
        }
        if let Some(au) = &au {
            let (bits, ops) = split(au);
            if let Some(mut f) = dec.decode(&bits).unwrap() {
                match compositor.compose(ops.as_ref(), &mut f) {
                    Composed::Show => shown = Some(f.rgba),
                    Composed::Resync => out.resyncs += 1,
                }
            }
        } else {
            out.dropped += 1;
        }
        if i > 0 {
            out.bytes.push(au.as_ref().map_or(0, Vec::len));
            out.psnr
                .push(shown.as_ref().map_or(0.0, |s| psnr(&screen, s)));
        }
    }
    out.encode_ms /= n as f64;
    out.prepare_ms /= n as f64;
    out.moved /= n as f64;
    out.encoded /= n as f64;
    out
}

fn report(seq: &str, mode: Mode, rc: Rc, o: &Outcome) -> f64 {
    let mean = o.bytes.iter().sum::<usize>() as f64 / o.bytes.len() as f64;
    let mut sorted = o.bytes.clone();
    sorted.sort_unstable();
    let p95 = sorted[sorted.len() * 95 / 100];
    let psnr = o.psnr.iter().sum::<f64>() / o.psnr.len() as f64;
    let min = o.psnr.iter().copied().fold(f64::INFINITY, f64::min);
    println!(
        "| {seq} | {mode:?} | {rc:?} | {:.0} | {p95} | {:.2} | {} | {psnr:.2} | {min:.2} | {:.1} | {:.2} | {:.0}% | {:.1}% | {} |",
        mean,
        mean * 8.0 * 30.0 / 1e6,
        o.dropped,
        o.encode_ms,
        o.prepare_ms,
        o.moved * 100.0,
        o.encoded * 100.0,
        o.resyncs,
    );
    mean
}

/// Scroll positions per frame (30 fps).
fn schedules(max: usize, step: usize) -> Vec<(&'static str, Vec<usize>)> {
    let mut flicks = vec![0];
    let pattern = [57, 45, 34, 25, 18, 12, 8, 5, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0];
    while flicks.len() < 150 {
        for d in pattern {
            let last = *flicks.last().unwrap();
            flicks.push((last + d).min(max));
        }
    }
    flicks.truncate(150);
    let steady = |v: usize| (0..150).map(|i| (i * v).min(max)).collect::<Vec<_>>();
    let page_down: Vec<usize> = (0..150).map(|i| (i / 18 * step).min(max)).collect();
    vec![
        ("wheel flicks", flicks),
        ("steady 8 px", steady(8)),
        ("steady 30 px", steady(30)),
        ("page down", page_down),
    ]
}

/// The instructions alone, with a lossless "codec": the viewer's picture must match the
/// screen in every frame (bit for bit while no move had an odd distance).
#[test]
#[ignore = "slow (1080p pictures); run with --release"]
fn instructions_rebuild_every_scrolled_picture() {
    for page in pages() {
        let max = page.content_h - page.viewport.h;
        for (name, offsets) in schedules(max, page.viewport.h - 40) {
            let mut composer = Composer::new();
            let mut compositor = Compositor::new();
            let mut screen = vec![0u8; W * H * 4];
            let mut pic = I420::new(W, H);
            for (i, &offset) in offsets.iter().enumerate() {
                render(&page, offset, &mut screen);
                pic.fill_from_bgra(&screen, W);
                let canvas = composer.prepare(&pic, &[]);
                let mut decoded = DecodedFrame {
                    width: W,
                    height: H,
                    rgba: vec![0; W * H * 4],
                };
                crate::yuv::i420_to_rgba(
                    (canvas.y(), canvas.u(), canvas.v()),
                    canvas.strides(),
                    (W, H),
                    &mut decoded.rgba,
                );
                let au = composer.sent(&pic, vec![0, 0, 0, 1, 0x65, 0x88]);
                let ops = split(&au).1.unwrap();
                if std::env::var_os("RCONN_SCROLL_TRACE").is_some()
                    && composer.last().encoded > 0.04
                {
                    eprintln!("{} {name} {i}: {ops:?}", page.name);
                }
                assert_eq!(compositor.compose(Some(&ops), &mut decoded), Composed::Show);
                let q = psnr(&screen, &decoded.rgba);
                assert!(
                    q > 30.0,
                    "{} {name} frame {i} (offset {offset}): {q:.1} dB, {ops:?}",
                    page.name
                );
            }
        }
    }
}

#[test]
#[ignore = "benchmark, run by hand"]
fn scroll_bench() {
    let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
        return;
    };
    println!(
        "| sequence | mode | rate control | bytes/frame | p95 | Mb/s at 30 fps | dropped | PSNR dB | min PSNR | encode ms | prepare ms | moved | encoded | resyncs |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let rcs = [Rc::Qp(26), Rc::Rate(2_000_000), Rc::Rate(6_000_000)];
    // RCONN_SCROLL_ONLY=<text>: only sequences whose name contains it.
    let only = std::env::var("RCONN_SCROLL_ONLY").ok();
    for page in pages() {
        let max = page.content_h - page.viewport.h;
        for (name, offsets) in schedules(max, page.viewport.h - 40) {
            let seq = format!("{} {name}", page.name);
            if only.as_ref().is_some_and(|o| !seq.contains(o.as_str())) {
                continue;
            }
            for rc in rcs {
                let mut base = 0.0;
                for mode in [Mode::Full, Mode::CopyRect] {
                    let mut frames = |i: usize, out: &mut [u8]| {
                        render(&page, offsets[i], out);
                        Vec::new()
                    };
                    let o = run(&lib, &mut frames, offsets.len(), mode, rc, &seq);
                    let mean = report(&seq, mode, rc, &o);
                    if matches!(mode, Mode::Full) {
                        base = mean;
                    } else {
                        println!("|   | saving | | {:.0}% |", (1.0 - mean / base) * 100.0);
                    }
                }
            }
        }
        // A window dragged diagonally, then still, then back.
        let path: Vec<(usize, usize)> = (0..120)
            .map(|i| match i {
                0..40 => (100 + 11 * i, 100 + 6 * i),
                40..60 => (540, 340),
                _ => (540 - 7 * (i - 60), 340 - 3 * (i - 60)),
            })
            .collect();
        let move_of = |i: usize| -> Vec<Move> {
            if i == 0 || path[i] == path[i - 1] {
                return Vec::new();
            }
            let ((fx, fy), (x, y)) = (path[i - 1], path[i]);
            vec![Move {
                from_x: fx,
                from_y: fy,
                to: Rect {
                    x,
                    y,
                    w: 1000,
                    h: 640,
                },
            }]
        };
        for rc in rcs {
            let mut base = 0.0;
            for mode in [Mode::Full, Mode::CopyRect, Mode::CopyRectHints] {
                let mut frames = |i: usize, out: &mut [u8]| {
                    render_drag(&page, path[i], out);
                    move_of(i)
                };
                let seq = format!("{} window drag", page.name);
                let o = run(&lib, &mut frames, path.len(), mode, rc, &seq);
                let mean = report(&seq, mode, rc, &o);
                if matches!(mode, Mode::Full) {
                    base = mean;
                } else {
                    println!("|   | saving | | {:.0}% |", (1.0 - mean / base) * 100.0);
                }
            }
        }
    }
}
