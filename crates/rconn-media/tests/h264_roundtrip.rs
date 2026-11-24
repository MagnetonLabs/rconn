//! Real encode -> decode through Cisco's openh264. Needs `scripts/fetch-openh264.sh`.

use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder};
use std::time::Instant;

/// A desktop-ish frame: gradient background, a "window" moving right, some text-like stripes.
fn synthetic(width: usize, height: usize, t: usize) -> Frame {
    let mut bgra = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let mut px = [(x * 255 / width) as u8, (y * 255 / height) as u8, 96, 255];
            let (wx, wy) = (40 + t * 8, 60);
            if (wx..wx + 200).contains(&x) && (wy..wy + 120).contains(&y) {
                px = if (y / 4) % 2 == 0 {
                    [250, 250, 250, 255]
                } else {
                    [30, 30, 30, 255]
                };
            }
            bgra[i..i + 4].copy_from_slice(&px);
        }
    }
    Frame {
        width,
        height,
        bgra,
        captured_at: Instant::now(),
    }
}

/// PSNR (dB) between BGRA source and RGBA decode, luma-ish over all channels.
fn psnr(src_bgra: &[u8], rgba: &[u8]) -> f64 {
    let mut se = 0f64;
    let mut n = 0f64;
    for (s, d) in src_bgra.as_chunks::<4>().0.iter().zip(rgba.chunks_exact(4)) {
        for (a, b) in [(s[2], d[0]), (s[1], d[1]), (s[0], d[2])] {
            let e = f64::from(a) - f64::from(b);
            se += e * e;
            n += 1.0;
        }
    }
    10.0 * (255.0f64.powi(2) / (se / n)).log10()
}

#[test]
fn encode_decode_preserves_the_picture() {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    assert!(
        lib.exists(),
        "missing {}; run scripts/fetch-openh264.sh",
        lib.display()
    );
    let (w, h) = (640, 360);
    let mut enc = VideoEncoder::new(
        &lib,
        EncoderSettings {
            bitrate_bps: 3_000_000,
            max_fps: 30.0,
            threads: 0,
        },
    )
    .unwrap();
    let mut dec = VideoDecoder::new(&lib).unwrap();

    let mut decoded = 0;
    let mut worst = f64::MAX;
    let started = Instant::now();
    for t in 0..30 {
        let frame = synthetic(w, h, t);
        let Some(au) = enc.encode(&frame).unwrap() else {
            continue;
        };
        if t == 0 {
            assert!(
                au.windows(4).any(|w| w == [0, 0, 0, 1]),
                "Annex-B start codes expected"
            );
        }
        if let Some(out) = dec.decode(&au).unwrap() {
            assert_eq!((out.width, out.height), (w, h));
            worst = worst.min(psnr(&frame.bgra, &out.rgba));
            decoded += 1;
        }
    }
    let per_frame = started.elapsed() / 30;
    eprintln!(
        "decoded {decoded}/30, worst PSNR {worst:.1} dB, {per_frame:?}/frame encode+decode at {w}x{h}"
    );
    assert!(decoded >= 25, "decoded only {decoded} of 30 frames");
    assert!(worst > 30.0, "picture too degraded: {worst:.1} dB");
}

#[test]
fn odd_dimensions_are_cropped_not_rejected() {
    let lib = local_binary_path().unwrap();
    let mut enc = VideoEncoder::new(&lib, EncoderSettings::default()).unwrap();
    let mut dec = VideoDecoder::new(&lib).unwrap();
    let au = enc
        .encode(&synthetic(321, 241, 0))
        .unwrap()
        .expect("first frame is never skipped");
    let out = dec.decode(&au).unwrap().expect("IDR decodes immediately");
    assert_eq!((out.width, out.height), (320, 240));
}

#[test]
fn keyframe_on_request_and_after_bitrate_change() {
    let lib = local_binary_path().unwrap();
    let mut enc = VideoEncoder::new(&lib, EncoderSettings::default()).unwrap();
    let is_idr = |au: &[u8]| {
        au.windows(5)
            .any(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 5)
    };
    assert!(is_idr(
        &enc.encode(&synthetic(320, 240, 0)).unwrap().unwrap()
    ));
    let p = enc.encode(&synthetic(320, 240, 1)).unwrap().unwrap();
    assert!(!is_idr(&p), "second frame should be a P-frame");
    enc.request_keyframe();
    assert!(is_idr(
        &enc.encode(&synthetic(320, 240, 2)).unwrap().unwrap()
    ));
    enc.set_bitrate(1_000_000);
    assert!(is_idr(
        &enc.encode(&synthetic(320, 240, 3)).unwrap().unwrap()
    ));
}
