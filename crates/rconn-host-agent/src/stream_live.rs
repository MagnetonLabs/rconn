//! The host loop on the real screen (Windows): DXGI capture and the encoder the host
//! picks, with a window of ours in front of everything playing a benchmark scenario
//! ([`crate::encode_bench::Desktop`]) at `RCONN_LIVE_HZ` (60) for `RCONN_BENCH_SECS` (8).
//!
//! `RCONN_LIVE_SCENARIO` = idle | typing | scrolling (default) | video | mix (`RCONN_LIVE_KBPS`: the link, default 25000); the viewer shows
//! the picture `RCONN_BENCH_VIEW` large (default: the screen's size); input is reported
//! every 100 ms (someone interacting) unless `RCONN_LIVE_INPUT=0`. `RCONN_LIVE_SIZE=WxH`:
//! the window covers only that much (top left; a browser scrolling under its toolbars),
//! `RCONN_LIVE_COPY=1`: the viewer takes copy instructions.
//! `stream-tests.exe live_loop --ignored --nocapture` (`RUST_LOG=info`: the stats lines).
//!
//! Unsafe: the window (class, procedure) and GDI drawing, as in `indicator.rs`.
#![allow(unsafe_code)]

use super::*;
use crate::encode_bench::{Desktop, Scenario};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, GetDC, PAINTSTRUCT,
    ReleaseDC, SRCCOPY, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
    HTTRANSPARENT, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
    SetProcessDPIAware, TranslateMessage, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Plays `scenario` at `hz` until `until` in a window `size` large (the whole screen if
/// `None`); the pictures it drew.
fn animate(
    scenario: Scenario,
    hz: u32,
    size: Option<(usize, usize)>,
    until: Instant,
    ready: std::sync::mpsc::Sender<()>,
) -> u32 {
    unsafe {
        let _ = SetProcessDPIAware();
        let (w, h) = size.map_or(
            (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)),
            |(w, h)| (w as i32, h as i32),
        );
        let instance = GetModuleHandleW(None).expect("module");
        let class = w!("rconn_live_bench");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("rconn live bench"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("window");
        let (w, h) = (w as usize, h as usize);
        let frames = (hz as usize) * (until - Instant::now()).as_secs() as usize + 64;
        let mut desk = Desktop::new(scenario, (w, h), frames);
        let blit = |desk: &Desktop, r: rconn_media::yuv::Rect| {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(r.h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = &desk.screen()[r.y * w * 4..(r.y + r.h) * w * 4];
            let dc = GetDC(Some(hwnd));
            StretchDIBits(
                dc,
                r.x as i32,
                r.y as i32,
                r.w as i32,
                r.h as i32,
                r.x as i32,
                0,
                r.w as i32,
                r.h as i32,
                Some(rows.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ReleaseDC(Some(hwnd), dc);
        };
        let whole = rconn_media::yuv::Rect { x: 0, y: 0, w, h };
        blit(&desk, whole);
        let _ = ready.send(());
        let period = Duration::from_secs(1) / hz;
        let mut next = Instant::now();
        let mut k = 0;
        while Instant::now() < until {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            k += 1;
            for r in desk.step(k) {
                blit(&desk, r);
            }
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        let _ = DestroyWindow(hwnd);
        k as u32
    }
}

#[test]
#[ignore = "the real screen, by hand on Windows"]
fn live_loop() {
    let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
        return;
    };
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }
    let env = |k: &str| std::env::var(k).ok();
    let scenario = match env("RCONN_LIVE_SCENARIO").as_deref() {
        Some("idle") => Scenario::Idle,
        Some("typing") => Scenario::Typing,
        Some("video") => Scenario::Video,
        Some("mix") => Scenario::Mix,
        _ => Scenario::Scrolling,
    };
    let hz: u32 = env("RCONN_LIVE_HZ")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let secs: u64 = env("RCONN_BENCH_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let input = env("RCONN_LIVE_INPUT").as_deref() != Some("0");
    // `RCONN_LIVE_KBPS`: the link's bitrate (default 25000).
    let bps = env("RCONN_LIVE_KBPS")
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(25_000_000, |k| k * 1000);
    let size_knob = |k: &str| {
        env(k).and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?))
        })
    };
    let window = size_knob("RCONN_LIVE_SIZE");
    // Still for a second at first (the first keyframe goes out), then the scenario.
    let warmup = Duration::from_millis(1500);
    let t0 = Instant::now();
    let until = t0 + warmup + Duration::from_secs(secs);
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let animator = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        animate(scenario, hz, window, until, ready_tx)
    });
    ready.recv().expect("window up");
    let keyframes = KeyframeRequests::default();
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let view = env("RCONN_BENCH_VIEW")
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((sw as u32, sh as u32));
    keyframes.set_view_size(view.0, view.1);
    if env("RCONN_LIVE_COPY").as_deref() == Some("1") {
        keyframes.enable_copy_rects();
    }
    let ctx = Ctx {
        cfg: StreamConfig {
            openh264: lib,
            max_fps: 30,
            max_bitrate_bps: bps,
            max_size: (3840, 2400),
            motion_half_res: false,
            software: false,
            codec: rconn_media::caps::VideoCodec::H264,
        },
        target: TargetBitrate::fixed(f64::from(bps)),
        want_key: Arc::default(),
        feedback: keyframes.feedback.clone(),
        stop: Arc::default(),
        first_sent: AtomicBool::new(false),
        next_id: AtomicU32::new(0),
        sending: Arc::default(),
        copy_rects: keyframes.copy_rects.clone(),
        view: keyframes.view.clone(),
    };
    let (tx, mut rx) = mpsc::channel(2);
    let make = || rconn_media::capture::primary_display().map_err(anyhow::Error::from);
    let mut out: Vec<(Instant, usize, bool)> = Vec::new();
    let mut generations = 0;
    std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let tx = tx;
            // As `supervise` does: a lost capture starts a new generation.
            while !ctx.stop.load(Ordering::Relaxed) {
                generations += 1;
                if let Err(e) = run_generation(&make, &ctx, &tx) {
                    println!("generation {generations}: {e:#}");
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        });
        s.spawn(|| {
            let mut last = Instant::now();
            while Instant::now() < until {
                if input
                    && last.elapsed() >= Duration::from_millis(100)
                    && Instant::now() > t0 + warmup
                {
                    note_input();
                    last = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            ctx.stop.store(true, Ordering::Relaxed);
        });
        // `RCONN_LIVE_FIT_MS=3000+6000`: the viewer asks the screen to fit its window (at
        // the screen's own size: "already fits") at these times, as `Control::Resize` does.
        let fits: Vec<u64> = env("RCONN_LIVE_FIT_MS")
            .map(|v| v.split('+').filter_map(|t| t.parse().ok()).collect())
            .unwrap_or_default();
        s.spawn(move || {
            for at in fits {
                let when = t0 + Duration::from_millis(at);
                std::thread::sleep(when.saturating_duration_since(Instant::now()));
                let set = crate::display::Fit::default().apply(sw as u32, sh as u32);
                println!("fit at {at} ms: {set:?}");
            }
        });
        while let Some((au, at)) = rx.blocking_recv() {
            let (au, tag) = frames::take(rconn_media::caps::VideoCodec::H264, &au);
            if let Some(t) = tag {
                keyframes.frame_feedback(Feedback {
                    decoded: t.id,
                    lost: false,
                });
            }
            out.push((at, au.len(), tag.is_some_and(|t| t.key)));
        }
        worker.join().unwrap();
    });
    let drawn = animator.join().unwrap();
    let moving: Vec<_> = out
        .iter()
        .filter(|f| f.0 >= t0 + warmup + Duration::from_millis(500))
        .collect();
    let mut gaps: Vec<f64> = moving
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).as_secs_f64() * 1e3)
        .collect();
    gaps.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        gaps.get(((gaps.len() as f64 - 1.0) * p) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let span = moving
        .last()
        .zip(moving.first())
        .map_or(0.0, |(l, f)| (l.0 - f.0).as_secs_f64());
    println!(
        "{scenario:?} {sw}x{sh} view {}x{} source {:.1} Hz: {} frames, {:.1} fps, gap ms p50 {:.1} p90 {:.1} max {:.0}, {:.0} KB/pic, keys {}, generations {generations}",
        view.0,
        view.1,
        f64::from(drawn) / (secs as f64),
        moving.len(),
        (moving.len().max(1) - 1) as f64 / span.max(1e-9),
        pct(0.5),
        pct(0.9),
        pct(1.0),
        moving.iter().map(|f| f.1).sum::<usize>() as f64 / 1024.0 / moving.len().max(1) as f64,
        out.iter().filter(|f| f.2).count(),
    );
}
