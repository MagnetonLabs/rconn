//! Encoder bake-off on this machine's real screen: openh264 vs Media Foundation H.264.
//!   bakeoff <openh264.dll> [frames]
//! Captures one real frame, builds three scenarios from it (scroll, video, typing) at
//! full and half resolution, and prints ms/frame, size and quality (PSNR) per encoder.
//! Nothing is saved; the captured picture never leaves this process.

#[cfg(not(windows))]
fn main() {
    eprintln!("bakeoff: Windows only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(windows)]
mod imp {

    use rconn_media::mf::MfH264Encoder;
    use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder};
    use rconn_media::yuv::I420;
    use std::time::{Duration, Instant};

    fn grab() -> anyhow::Result<Frame> {
        let mut cap = rconn_media::capture::primary_display()?;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if let Some(f) = cap.next_frame(Duration::from_millis(100))? {
                return Ok(f);
            }
        }
        anyhow::bail!("no screen frame in 5 s")
    }

    /// Frame `k` of a scenario built from the real `base` screen.
    fn scenario(base: &Frame, name: &str, k: usize) -> Frame {
        let (w, h) = (base.width, base.height);
        let mut bgra = base.bgra.clone();
        match name {
            // Whole page scrolling 12 px per frame.
            "scroll" => {
                let shift = (k * 12) % h;
                let row = w * 4;
                bgra.rotate_left(shift * row);
            }
            // A 16:9 video in the middle third of the screen: smooth, moving content.
            "video" => {
                let (vw, vh) = (w / 2, w / 2 * 9 / 16);
                let (x0, y0) = ((w - vw) / 2, (h - vh) / 2);
                for y in 0..vh {
                    for x in 0..vw {
                        let i = ((y0 + y) * w + x0 + x) * 4;
                        let t = k as f32 * 0.15;
                        let v = |f: f32| ((f.sin() * 0.5 + 0.5) * 255.0) as u8;
                        bgra[i] = v(x as f32 * 0.013 + t);
                        bgra[i + 1] = v(y as f32 * 0.017 - t * 1.3);
                        bgra[i + 2] = v((x + y) as f32 * 0.009 + t * 0.7);
                    }
                }
            }
            // Typing: a short line of text-like blocks appears character by character.
            _ => {
                let (x0, y0) = (w / 4, h / 3);
                for c in 0..k.min(60) {
                    for y in 0..24 {
                        for x in 0..10 {
                            if (x * 7 + y * 3 + c) % 5 < 2 {
                                let i = ((y0 + y) * w + x0 + c * 12 + x) * 4;
                                bgra[i..i + 3].copy_from_slice(&[20, 20, 20]);
                            }
                        }
                    }
                }
            }
        }
        Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        }
    }

    fn luma(b: u8, g: u8, r: u8) -> f64 {
        0.114 * f64::from(b) + 0.587 * f64::from(g) + 0.299 * f64::from(r)
    }

    /// PSNR (dB) of a decoded RGBA picture against the BGRA source (luma).
    fn psnr(src: &Frame, rgba: &[u8], w: usize, h: usize) -> f64 {
        let mut se = 0.0;
        for y in 0..h {
            for x in 0..w {
                let s = (y * src.width + x) * 4;
                let d = (y * w + x) * 4;
                let a = luma(src.bgra[s], src.bgra[s + 1], src.bgra[s + 2]);
                let b = luma(rgba[d + 2], rgba[d + 1], rgba[d]);
                se += (a - b) * (a - b);
            }
        }
        let mse = se / (w * h) as f64;
        if mse < 1e-9 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    struct Score {
        ms: Vec<f64>,
        bytes: usize,
        psnr: Vec<f64>,
    }

    impl Score {
        fn line(&self, label: &str) -> String {
            let mut ms = self.ms.clone();
            ms.sort_by(f64::total_cmp);
            let avg = ms.iter().sum::<f64>() / ms.len() as f64;
            let p95 = ms[(ms.len() * 95 / 100).min(ms.len() - 1)];
            let q = self.psnr.iter().sum::<f64>() / self.psnr.len().max(1) as f64;
            format!(
                "{label:<34} {avg:6.1} ms avg {p95:6.1} ms p95 {:7.0} kB/s @30fps  PSNR {q:5.1} dB",
                self.bytes as f64 / ms.len() as f64 * 30.0 / 1000.0
            )
        }
    }

    fn run_openh264(lib: &std::path::Path, frames: &[Frame]) -> anyhow::Result<Score> {
        let mut enc = VideoEncoder::new(
            lib,
            EncoderSettings {
                bitrate_bps: 8_000_000,
                max_fps: 30.0,
                threads: 0,
                slice_bytes: None,
            },
        )?;
        let mut dec = VideoDecoder::new(lib)?;
        let mut s = Score {
            ms: vec![],
            bytes: 0,
            psnr: vec![],
        };
        for f in frames {
            let t = Instant::now();
            let au = enc.encode(f)?;
            s.ms.push(t.elapsed().as_secs_f64() * 1e3);
            if let Some(au) = au {
                s.bytes += au.len();
                if let Ok(Some(d)) = dec.decode(&au) {
                    s.psnr.push(psnr(f, &d.rgba, d.width, d.height));
                }
            }
        }
        Ok(s)
    }

    fn run_mf(lib: &std::path::Path, frames: &[Frame]) -> anyhow::Result<(Score, String)> {
        let (w, h) = (frames[0].width & !1, frames[0].height & !1);
        let mut enc = MfH264Encoder::new(w as u32, h as u32, 30, 8_000_000)?;
        let name = enc.name().to_owned();
        let mut dec = VideoDecoder::new(lib)?;
        let mut yuv = I420::new(w, h);
        let mut s = Score {
            ms: vec![],
            bytes: 0,
            psnr: vec![],
        };
        let mut decoded = 0;
        for f in frames {
            let t = Instant::now();
            yuv.fill_from_bgra(&f.bgra, f.width);
            let au = enc.encode(&yuv.to_nv12())?;
            s.ms.push(t.elapsed().as_secs_f64() * 1e3);
            s.bytes += au.len();
            if !au.is_empty()
                && let Ok(Some(d)) = dec.decode(&au)
            {
                // Compare with the frame this output belongs to (allowing encoder delay).
                let src = &frames[decoded.min(frames.len() - 1)];
                decoded += 1;
                s.psnr.push(psnr(src, &d.rgba, d.width, d.height));
            }
        }
        Ok((s, name))
    }

    pub fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let lib = std::path::PathBuf::from(
            args.get(1)
                .cloned()
                .unwrap_or_else(|| r"C:\Program Files\rconn\openh264-2.7.0-win-arm64.dll".into()),
        );
        let n: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(45);
        let full = grab()?;
        println!(
            "screen {}x{}, {} cores, {n} frames per scenario",
            full.width,
            full.height,
            std::thread::available_parallelism().map_or(1, |c| c.get())
        );
        for (res_label, base) in [
            ("full", full.clone()),
            (
                "half",
                full.clone().fit_within(full.width / 2, full.height / 2),
            ),
        ] {
            for sc in ["scroll", "video", "typing"] {
                let frames: Vec<Frame> = (0..n).map(|k| scenario(&base, sc, k)).collect();
                let label = format!("{res_label} {}x{} {sc}", base.width, base.height);
                match run_openh264(&lib, &frames) {
                    Ok(s) => println!("{}", s.line(&format!("openh264  {label}"))),
                    Err(e) => println!("openh264  {label}: error {e}"),
                }
                match run_mf(&lib, &frames) {
                    Ok((s, name)) => {
                        println!("{}   [{name}]", s.line(&format!("MediaFdn  {label}")))
                    }
                    Err(e) => println!("MediaFdn  {label}: error {e}"),
                }
            }
        }
        Ok(())
    }
}
