//! Where does the host's CPU go? Process CPU time per stage, on this machine's real screen.
//!   cpuprof [seconds] [encoder kbps]
//! 1. capture only (DXGI, whatever is changing on screen right now)
//! 2. colour conversion only (BGRA -> I420 -> NV12), on the captured frames
//! 3. Media Foundation encode only, on the converted frames
//!
//! Nothing is saved; captured pictures never leave this process.

#[cfg(not(windows))]
fn main() {
    eprintln!("cpuprof: Windows only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use rconn_media::mf::MfH264Encoder;
    use rconn_media::video::Frame;
    use rconn_media::yuv::I420;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    /// User + kernel CPU seconds used by this process so far (all threads).
    fn cpu() -> f64 {
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }
            .expect("GetProcessTimes");
        let t = |f: FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
        (t(k) + t(u)) as f64 / 1e7
    }

    fn report(label: &str, n: usize, cpu_s: f64, wall_s: f64) {
        let n = n.max(1) as f64;
        println!(
            "{label:<26} {:6.1} ms CPU/frame  {:6.1} ms wall/frame  ({:4.0}% of one core at 30 fps)",
            cpu_s * 1e3 / n,
            wall_s * 1e3 / n,
            cpu_s / n * 30.0 * 100.0
        );
    }

    pub fn run() -> anyhow::Result<()> {
        let secs: u64 = std::env::args()
            .nth(1)
            .and_then(|a| a.parse().ok())
            .unwrap_or(5);
        let mut cap = rconn_media::capture::primary_display()?;

        // 1. Capture.
        let mut frames: Vec<Frame> = Vec::new();
        let mut n = 0usize;
        let (c0, t0) = (cpu(), Instant::now());
        while t0.elapsed() < Duration::from_secs(secs) {
            if let Some(f) = cap.next_frame(Duration::from_millis(33))? {
                n += 1;
                if frames.len() < 20 {
                    frames.push(f);
                }
            }
        }
        let (cap_cpu, cap_wall) = (cpu() - c0, t0.elapsed().as_secs_f64());
        let (w, h) = match frames.first() {
            Some(f) => (f.width & !1, f.height & !1),
            None => anyhow::bail!("the screen didn't change in {secs} s; open something animated"),
        };
        println!(
            "screen {w}x{h}, {} cores; {n} frames captured in {secs} s ({:.1} fps)",
            std::thread::available_parallelism().map_or(1, |n| n.get()),
            n as f64 / cap_wall
        );
        report("capture (DXGI copy-out)", n, cap_cpu, cap_wall);

        // 2. Conversion.
        let mut yuv = I420::new(w, h);
        let mut nv12 = Vec::new();
        let reps = 60;
        let (c0, t0) = (cpu(), Instant::now());
        for k in 0..reps {
            let f = &frames[k % frames.len()];
            yuv.fill_from_bgra(&f.bgra, f.width);
            nv12 = yuv.to_nv12();
        }
        report(
            "convert BGRA->I420->NV12",
            reps,
            cpu() - c0,
            t0.elapsed().as_secs_f64(),
        );
        let (c0, t0) = (cpu(), Instant::now());
        for k in 0..reps {
            let f = &frames[k % frames.len()];
            yuv.fill_from_bgra(&f.bgra, f.width);
        }
        report(
            "  of which BGRA->I420",
            reps,
            cpu() - c0,
            t0.elapsed().as_secs_f64(),
        );
        let nv12s: Vec<Vec<u8>> = frames
            .iter()
            .map(|f| {
                yuv.fill_from_bgra(&f.bgra, f.width);
                yuv.to_nv12()
            })
            .collect();
        drop(nv12);

        // 3. Encode (the real frames in order, then again: mostly-static desktop content).
        let kbps: u32 = std::env::args()
            .nth(2)
            .and_then(|a| a.parse().ok())
            .unwrap_or(8_000);
        let mut enc = MfH264Encoder::new(w as u32, h as u32, 30, kbps * 1000)?;
        println!("encoder: {} at {kbps} kbps", enc.name());
        let (c0, t0) = (cpu(), Instant::now());
        let mut bytes = 0;
        for k in 0..reps {
            bytes += enc.encode(&nv12s[k % nv12s.len()])?.len();
        }
        report(
            "Media Foundation encode",
            reps,
            cpu() - c0,
            t0.elapsed().as_secs_f64(),
        );
        // A completely unchanged frame: the floor cost of encoding "nothing changed".
        let (c0, t0) = (cpu(), Instant::now());
        for _ in 0..reps {
            bytes += enc.encode(&nv12s[0])?.len();
        }
        report(
            "  same frame repeated",
            reps,
            cpu() - c0,
            t0.elapsed().as_secs_f64(),
        );
        println!("({} kB encoded)", bytes / 1000);
        Ok(())
    }
}
