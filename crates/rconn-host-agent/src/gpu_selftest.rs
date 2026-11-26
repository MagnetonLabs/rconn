//! `--selftest-gpu N`: measure the Windows capture -> encode paths on this machine.
//!
//! A full-screen window repaints at 60 Hz (moving stripes, changing colours) so every
//! capture is a whole-screen change. Then, N frames each:
//! 1. memory path: Desktop Duplication -> CPU copy and BGRA->I420 -> encoder (as before);
//! 2. GPU path: Desktop Duplication -> GPU copy -> video processor -> D3D11 hardware
//!    encoder (only where such an encoder exists), a third of it at half size;
//! 3. the GPU stages alone, on any GPU with a video processor (VMs too): conversion,
//!    an NV12 readback, and the memory encoder fed with it; plus a colour check of the
//!    video processor against the CPU conversion.
//!
//! Each path's output is decoded with openh264 and compared with the desktop.

#[cfg(not(windows))]
pub fn run(
    _frames: usize,
    _openh264: &std::path::Path,
    _max: (usize, usize),
) -> anyhow::Result<String> {
    anyhow::bail!("--selftest-gpu is for Windows hosts")
}

#[cfg(windows)]
pub use imp::run;

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: the test window, GetProcessTimes
mod imp {
    use crate::frames::Frames;
    use anyhow::{Context, Result};
    use rconn_media::capture::Capturer;
    use rconn_media::capture::dxgi::DxgiCapturer;
    use rconn_media::d3d11::{Converter, GpuDevice, Readback, fitted};
    use rconn_media::mf::MfH264Encoder;
    use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder};
    use rconn_media::yuv::I420;
    use std::fmt::Write as _;
    use std::path::Path;
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

    /// Process CPU seconds so far.
    fn cpu_seconds() -> f64 {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut c, mut e, mut k, mut u) = Default::default();
        let t = |f: FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }
            .map_or(0.0, |()| (t(k) + t(u)) as f64 / 1e7)
    }

    /// Per-frame timings of one path.
    #[derive(Default)]
    struct Timings {
        capture: Vec<f64>,
        encode: Vec<f64>,
        bytes: usize,
        wall: f64,
        cpu: f64,
    }

    impl Timings {
        fn line(&self, name: &str) -> String {
            let n = self.capture.len().max(1) as f64;
            let mean = |v: &[f64]| v.iter().sum::<f64>() / n;
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
            };
            format!(
                "{name}: {} frames, capture {:.2} ms + encode {:.2} ms = {:.2} ms/frame (p50 {:.2}, p95 {:.2}, max {:.2}); {:.1} fps; CPU {:.0}% of one core; {:.0} kbit/frame",
                self.capture.len(),
                mean(&self.capture),
                mean(&self.encode),
                mean(&total),
                pct(0.5).unwrap_or(0.0),
                pct(0.95).unwrap_or(0.0),
                total.last().copied().unwrap_or(0.0),
                n / self.wall.max(1e-9),
                self.cpu / self.wall.max(1e-9) * 100.0,
                self.bytes as f64 * 8.0 / 1000.0 / n,
            )
        }
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// Capture and encode `n` changed frames through `frames`; at half size for the
    /// middle third if `halve`. Returns the timings and the encoded stream.
    fn drive(
        frames: &mut Frames,
        enc: &mut VideoEncoder,
        n: usize,
        max: (usize, usize),
        halve: bool,
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
            let i = t.capture.len();
            let half = halve && (n / 3..2 * n / 3).contains(&i);
            let t1 = Instant::now();
            let out = frames.encode(enc, half, c.rows)?;
            t.encode.push(ms(t1.elapsed()));
            t.capture.push(ms(capture));
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
            aus.extend(
                frames
                    .encode(enc, false, rows)
                    .context("encode while still")?,
            );
        }
        Ok(())
    }

    /// The (still) desktop as sent within `max`, from a fresh duplication (one process
    /// can't duplicate an output twice: drop the path's capturer first). Unfreezes the
    /// test window.
    fn snapshot(max: (usize, usize)) -> Result<Option<Frame>> {
        let mut cap = DxgiCapturer::new(0).context("second capturer")?;
        let until = Instant::now() + Duration::from_secs(2);
        let mut desk = None;
        while desk.is_none() && Instant::now() < until {
            // A new duplication may start with a stale (black) image: repaint the same
            // still picture until a real one arrives.
            REPAINT.store(true, Ordering::Relaxed);
            desk = cap
                .next_frame(Duration::from_millis(100))
                .context("desktop snapshot")?
                .filter(|f| {
                    f.bgra
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|px| px[..3] != [0, 0, 0])
                });
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

    /// Decode `aus` and compare the last picture with the desktop `desk` (BGRA, as sent).
    /// With `RCONN_SELFTEST_DUMP=<dir>`, the compared pictures are written there as PPM.
    fn dump(label: &str, w: usize, h: usize, rgb: impl Iterator<Item = u8>) {
        if let Some(dir) = std::env::var_os("RCONN_SELFTEST_DUMP") {
            let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
            data.extend(rgb);
            let _ = std::fs::write(Path::new(&dir).join(format!("{label}.ppm")), data);
        }
    }

    fn check_stream(
        label: &str,
        openh264: &Path,
        aus: &[Vec<u8>],
        desk: Option<&Frame>,
    ) -> Result<String> {
        let mut dec = VideoDecoder::software(openh264)?;
        let mut pictures = 0;
        let mut last = None;
        for au in aus {
            if let Some(p) = dec.decode(au)? {
                pictures += 1;
                last = Some(p);
            }
        }
        let mut s = format!("decoded {pictures}/{} units", aus.len());
        if let (Some(p), Some(d)) = (last, desk) {
            if (p.width, p.height) == (d.width & !1, d.height & !1) {
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
                    psnr(&p.rgba, &crop)
                );
                dump(
                    &format!("{label}-decoded"),
                    p.width,
                    p.height,
                    p.rgba
                        .as_chunks::<4>()
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

    /// The kept GPU desktop picture, read back and fitted to `max` (as sent).
    fn gpu_desktop(cap: &mut DxgiCapturer, gpu: &GpuDevice, max: (usize, usize)) -> Option<Frame> {
        let f = cap.gpu_frame()?.clone();
        let bgra = Readback::new().read(gpu, &f.texture).ok()?;
        Some(
            Frame {
                width: f.width,
                height: f.height,
                bgra,
                captured_at: Instant::now(),
            }
            .fit_within(max.0, max.1),
        )
    }

    pub fn run(n: usize, openh264: &Path, max: (usize, usize)) -> Result<String> {
        let n = n.max(6);
        let mut report = String::new();
        let _window = Animator::start()?;
        std::thread::sleep(Duration::from_millis(500));

        // 1. Memory path (what every host did before).
        {
            let cap = DxgiCapturer::new(0)?;
            let mut enc = VideoEncoder::new(openh264, settings())?;
            let mut frames = Frames::memory(Box::new(cap));
            let (t, mut aus) = drive(&mut frames, &mut enc, n, max, false)?;
            settle(&mut frames, &mut enc, max, &mut aus)?;
            let (w, h) = frames.dimensions().unwrap_or_default();
            drop(frames);
            let desk = snapshot(max)?;
            writeln!(report, "desktop sent at {w}x{h}")?;
            writeln!(report, "{} [{}]", t.line("memory path"), enc.engine_name())?;
            writeln!(
                report,
                "  {}",
                check_stream("memory", openh264, &aus, desk.as_ref())?
            )?;
        }

        // 2. GPU path, as streaming uses it (Frames picks it when it works).
        let gpu = {
            let mut cap = DxgiCapturer::new(0)?;
            match cap.enable_gpu(max) {
                Ok(g) => {
                    writeln!(
                        report,
                        "GPU: {} (vendor {:#06x}); video processor converts BGRA->NV12",
                        g.adapter, g.vendor_id
                    )?;
                    Some(g)
                }
                Err(e) => {
                    writeln!(report, "GPU: no usable video processor: {e}")?;
                    None
                }
            }
        };
        if gpu.is_some() {
            let mut hardware = true;
            for attempt in 0..2 {
                let mut enc = VideoEncoder::new(openh264, settings())?;
                let mut frames =
                    Frames::new(rconn_media::capture::primary_display()?, &mut enc, max);
                if frames.on_gpu() {
                    let name = if hardware {
                        "GPU path"
                    } else {
                        "GPU path, software encoder reading textures back (plumbing check only)"
                    };
                    let (t, mut aus) = drive(&mut frames, &mut enc, n, max, true)?;
                    writeln!(report, "{} [{}]", t.line(name), enc.engine_name())?;
                    settle(&mut frames, &mut enc, max, &mut aus)?;
                    drop(frames);
                    let desk = snapshot(max)?;
                    writeln!(
                        report,
                        "  {}",
                        check_stream("gpu", openh264, &aus, desk.as_ref())?
                    )?;
                    break;
                }
                if attempt == 0 {
                    writeln!(
                        report,
                        "GPU path: not available here (no D3D11-aware hardware H.264 encoder); streaming keeps the memory path"
                    )?;
                    // Run the same texture path with Microsoft's software encoder, which
                    // Media Foundation feeds by reading each texture back.
                    // SAFETY: set before any other thread of this process reads it (the
                    // test window's thread never reads the environment).
                    unsafe { std::env::set_var("RCONN_TEXTURE_ENCODER", "software") };
                    crate::frames::retry_gpu();
                    hardware = false;
                }
            }
        }

        // 3. The GPU stages alone (any GPU with a video processor, VMs included).
        if gpu.is_some() {
            stages(&mut report, n, openh264, max)?;
        }
        Ok(report)
    }

    fn stages(report: &mut String, n: usize, openh264: &Path, max: (usize, usize)) -> Result<()> {
        let mut cap = DxgiCapturer::new(0)?;
        let gpu = cap.enable_gpu(max)?;
        let (w, h) = cap.output_size()?;
        let (fw, fh) = fitted(w, h, max);
        let mut conv = Converter::new(&gpu, (w as u32, h as u32), (fw as u32, fh as u32))
            .map_err(anyhow::Error::msg)?;
        let nv12_tex = gpu
            .nv12_texture((fw as u32, fh as u32), None)
            .map_err(anyhow::Error::msg)?;
        let mut rb = Readback::new();
        let mut mf = MfH264Encoder::new(fw as u32, fh as u32, FPS, BITRATE)?;
        let (mut cap_ms, mut conv_ms, mut rb_ms, mut enc_ms) = (vec![], vec![], vec![], vec![]);
        let mut aus = Vec::new();
        let mut nv12 = Vec::new();
        let (wall0, cpu0) = (Instant::now(), cpu_seconds());
        let deadline = Instant::now() + Duration::from_secs(60);
        while cap_ms.len() < n && Instant::now() < deadline {
            let t0 = Instant::now();
            if cap.next_gpu(Duration::from_millis(100), max)?.is_none() {
                continue;
            }
            gpu.finish().map_err(anyhow::Error::msg)?;
            cap_ms.push(ms(t0.elapsed()));
            let frame = cap.gpu_frame().context("no GPU frame")?.clone();
            let t1 = Instant::now();
            conv.convert(&frame.texture, &nv12_tex)
                .map_err(anyhow::Error::msg)?;
            gpu.finish().map_err(anyhow::Error::msg)?;
            conv_ms.push(ms(t1.elapsed()));
            let t2 = Instant::now();
            nv12 = rb.read(&gpu, &nv12_tex).map_err(anyhow::Error::msg)?;
            rb_ms.push(ms(t2.elapsed()));
            let t3 = Instant::now();
            let out = mf.encode(&nv12)?;
            enc_ms.push(ms(t3.elapsed()));
            if !out.is_empty() {
                aus.push(out);
            }
        }
        let wall = wall0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        let k = cap_ms.len().max(1) as f64;
        let mean = |v: &[f64]| v.iter().sum::<f64>() / k;
        writeln!(
            report,
            "GPU stages: {} frames: capture (GPU copy) {:.2} ms, convert+scale {:.2} ms, NV12 readback {:.2} ms, encode from memory {:.2} ms [{}]; {:.1} fps; CPU {:.0}% of one core",
            cap_ms.len(),
            mean(&cap_ms),
            mean(&conv_ms),
            mean(&rb_ms),
            mean(&enc_ms),
            mf.name(),
            k / wall.max(1e-9),
            cpu / wall.max(1e-9) * 100.0
        )?;
        // Colour check: the video processor against the CPU conversion, on colour bars
        // and on the desktop.
        writeln!(
            report,
            "  video processor vs CPU, colour bars: {}",
            colour_check(&gpu, &colour_bars())?
        )?;
        if let Some(desk) = gpu_desktop(&mut cap, &gpu, max) {
            writeln!(
                report,
                "  video processor vs CPU, desktop: {}",
                colour_check(&gpu, &desk)?
            )?;
        }
        let _ = nv12;
        writeln!(
            report,
            "  {}",
            check_stream("stages", openh264, &aus, None)?
        )?;

        // A D3D11-aware software encoder would let this machine run the texture path
        // end to end without hardware.
        match MfH264Encoder::new_d3d11(&gpu, fw as u32, fh as u32, FPS, BITRATE, true) {
            Ok(mut e) => {
                let frame = cap.gpu_frame().context("no GPU frame")?.clone();
                let t = Instant::now();
                let mut bytes = 0;
                for _ in 0..n {
                    bytes += e.encode_frame(&frame)?.len();
                }
                writeln!(
                    report,
                    "  texture encoder {} ({}): {:.2} ms/frame, {bytes} bytes",
                    e.name(),
                    if e.is_hardware() {
                        "hardware"
                    } else {
                        "software"
                    },
                    ms(t.elapsed()) / n as f64
                )?;
            }
            Err(e) => writeln!(report, "  no encoder takes textures here: {e}")?,
        }
        Ok(())
    }

    /// 8 vertical bars (white, yellow, cyan, green, magenta, red, blue, black) over a
    /// grey ramp, 512x256.
    fn colour_bars() -> Frame {
        let (w, h) = (512usize, 256usize);
        let bars: [[u8; 3]; 8] = [
            [255, 255, 255],
            [255, 255, 0],
            [0, 255, 255],
            [0, 255, 0],
            [255, 0, 255],
            [255, 0, 0],
            [0, 0, 255],
            [0, 0, 0],
        ];
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let [r, g, b] = if y < h / 2 {
                bars[x * 8 / w].map(|c| (u32::from(c) * 3 / 4) as u8)
            } else {
                [(x / 2) as u8; 3]
            };
            *px = [b, g, r, 255];
        }
        Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        }
    }

    /// Convert `f` (BGRA) on the GPU and on the CPU; how far apart are the results?
    fn colour_check(gpu: &GpuDevice, f: &Frame) -> Result<String> {
        let (w, h) = (f.width & !1, f.height & !1);
        let size = (f.width as u32, f.height as u32);
        let src = gpu
            .bgra_texture(size, Some(&f.bgra))
            .map_err(anyhow::Error::msg)?;
        let dst = gpu
            .nv12_texture((w as u32, h as u32), None)
            .map_err(anyhow::Error::msg)?;
        Converter::new(gpu, size, (w as u32, h as u32))
            .and_then(|mut c| c.convert(&src, &dst))
            .map_err(anyhow::Error::msg)?;
        let gpu_nv12 = Readback::new()
            .read(gpu, &dst)
            .map_err(anyhow::Error::msg)?;
        let mut i420 = I420::new(w, h);
        i420.fill_from_bgra(&f.bgra, f.width);
        let cpu_nv12 = i420.to_nv12();
        let y_len = w * h;
        let diff = |a: &[u8], b: &[u8]| {
            let n = a.len().max(1) as f64;
            let abs: u64 = a
                .iter()
                .zip(b)
                .map(|(x, y)| u64::from(x.abs_diff(*y)))
                .sum();
            let bias: i64 = a
                .iter()
                .zip(b)
                .map(|(x, y)| i64::from(*x) - i64::from(*y))
                .sum();
            let max = a
                .iter()
                .zip(b)
                .map(|(x, y)| x.abs_diff(*y))
                .max()
                .unwrap_or(0);
            (abs as f64 / n, bias as f64 / n, max)
        };
        let (ym, yb, yx) = diff(&gpu_nv12[..y_len], &cpu_nv12[..y_len]);
        let (cm, cb, cx) = diff(&gpu_nv12[y_len..], &cpu_nv12[y_len..]);
        Ok(format!(
            "Y mean |diff| {ym:.2} (bias {yb:+.2}, max {yx}), UV mean |diff| {cm:.2} (bias {cb:+.2}, max {cx})"
        ))
    }

    /// A full-screen window that repaints at 60 Hz, so every capture changes.
    struct Animator {
        thread: Option<std::thread::JoinHandle<()>>,
        hwnd: isize,
    }

    static TICK: AtomicU32 = AtomicU32::new(0);
    static PAUSED: AtomicBool = AtomicBool::new(false);
    static REPAINT: AtomicBool = AtomicBool::new(false);

    impl Animator {
        fn start() -> Result<Self> {
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || unsafe { window_thread(tx) });
            let hwnd = rx
                .recv_timeout(Duration::from_secs(5))
                .context("test window did not start")??;
            Ok(Self {
                thread: Some(thread),
                hwnd,
            })
        }
    }

    impl Drop for Animator {
        fn drop(&mut self) {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    unsafe fn window_thread(ready: std::sync::mpsc::Sender<Result<isize>>) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, GetSystemMetrics, MSG, RegisterClassW,
            SM_CXSCREEN, SM_CYSCREEN, SetTimer, TranslateMessage, WNDCLASSW, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
        };
        use windows::core::w;
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("RconnGpuSelftest"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("RconnGpuSelftest"),
                w!("Rconn GPU self-test"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
                None,
                None,
                Some(instance.into()),
                None,
            );
            let hwnd: HWND = match hwnd {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready.send(Err(e.into()));
                    return;
                }
            };
            SetTimer(Some(hwnd), 1, 16, None);
            let _ = ready.send(Ok(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::Foundation::{COLORREF, LRESULT, RECT};
        use windows::Win32::Graphics::Gdi::{
            BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect,
            PAINTSTRUCT, SetBkMode, TRANSPARENT, TextOutW,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, GetClientRect, PostQuitMessage, WM_DESTROY, WM_PAINT, WM_TIMER,
        };
        unsafe {
            match msg {
                WM_TIMER => {
                    if PAUSED.load(Ordering::Relaxed) {
                        if REPAINT.swap(false, Ordering::Relaxed) {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                        return LRESULT(0);
                    }
                    TICK.fetch_add(1, Ordering::Relaxed);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    LRESULT(0)
                }
                WM_PAINT => {
                    let tick = TICK.load(Ordering::Relaxed) as i32;
                    let mut ps = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut ps);
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    const PALETTE: [u32; 5] = [
                        0x0030_30c0,
                        0x0030_a030,
                        0x00c0_5020,
                        0x00e0_e0e0,
                        0x0080_8080,
                    ];
                    let bg = CreateSolidBrush(COLORREF(PALETTE[(tick / 30) as usize % 5]));
                    FillRect(dc, &r, bg);
                    let _ = DeleteObject(bg.into());
                    let ink = CreateSolidBrush(COLORREF(0x0010_1010));
                    let h = r.bottom.max(1);
                    for i in 0..24 {
                        let y = (i * h / 24 + tick * 7) % h;
                        FillRect(
                            dc,
                            &RECT {
                                left: r.left + 40,
                                top: y,
                                right: r.right - 40,
                                bottom: y + 10,
                            },
                            ink,
                        );
                    }
                    let _ = DeleteObject(ink.into());
                    SetBkMode(dc, TRANSPARENT);
                    for i in 0..30 {
                        let line: Vec<u16> = format!(
                            "Rconn GPU self-test, frame {tick}, line {i}: the quick brown fox"
                        )
                        .encode_utf16()
                        .collect();
                        let _ = TextOutW(dc, 60 + (tick * 3 + i * 11) % 200, 20 + i * 28, &line);
                    }
                    let _ = EndPaint(hwnd, &ps);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}
