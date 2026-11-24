//! Encoder throughput at 1080p for different thread counts. Run explicitly:
//! cargo nextest run -p rconn-media --release --run-ignored only -E 'test(encode_threads)' --no-capture

use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, Frame, VideoEncoder};
use std::time::Instant;

#[test]
#[ignore = "benchmark"]
fn encode_threads_1080p() {
    let lib = local_binary_path().unwrap();
    let (w, h) = (1920usize, 1080usize);
    let frames: Vec<Frame> = (0..60)
        .map(|t| {
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    let boxed = (t * 16..t * 16 + 400).contains(&x) && (200..500).contains(&y);
                    let v = if boxed { 255 } else { ((x ^ y) & 0xff) as u8 };
                    bgra[i..i + 4].copy_from_slice(&[v, v / 2, 80, 255]);
                }
            }
            Frame {
                width: w,
                height: h,
                bgra,
                captured_at: Instant::now(),
            }
        })
        .collect();
    for threads in [1u16, 2, 4, 0] {
        let mut enc = VideoEncoder::new(
            &lib,
            EncoderSettings {
                bitrate_bps: 6_000_000,
                max_fps: 30.0,
                threads,
            },
        )
        .unwrap();
        let _ = enc.encode(&frames[0]).unwrap(); // warm-up + IDR
        let t0 = Instant::now();
        for f in &frames[1..] {
            let _ = enc.encode(f).unwrap();
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / (frames.len() - 1) as f64;
        eprintln!("threads={threads}: {ms:.1} ms/frame");
    }
}

#[test]
#[ignore = "benchmark"]
fn bgra_to_yuv_1080p() {
    use openh264::formats::{BgraSliceU8, YUVBuffer};
    let (w, h) = (1920usize, 1080usize);
    let bgra: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
    let mut yuv = YUVBuffer::new(w, h);
    let t0 = Instant::now();
    for _ in 0..30 {
        yuv.read_rgb(BgraSliceU8::new(std::hint::black_box(&bgra), (w, h)));
        std::hint::black_box(&yuv);
    }
    eprintln!(
        "bgra->yuv: {:.1} ms/frame",
        t0.elapsed().as_secs_f64() * 1000.0 / 30.0
    );
}
