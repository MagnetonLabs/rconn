//! The codec ladder end to end on a Mac, without a network: VideoToolbox encode (H.264,
//! then HEVC at the same bitrate) -> frame tag -> RTP packetization (the payloaders the
//! host's track uses) -> the viewer's reassembly -> VideoToolbox decode, on a synthetic
//! desktop (text, a scrolling page, a window being dragged, typing). Prints bytes, PSNR
//! and times per codec:
//!
//! `cargo test --release -p rconn-peer --test codec_ladder -- --nocapture`
#![cfg(target_os = "macos")]

use bytes::Bytes;
use rconn_media::video::{
    EncoderSettings, Frame, VideoCodec, VideoDecoder, VideoEncoder, is_keyframe,
    sequence_header_of, sequence_info,
};
use rconn_peer::frames::{self, FrameTag};
use rconn_peer::video::{VideoReceiver, payload_type};
use rtc::rtp::packetizer::Payloader;
use std::time::{Duration, Instant};

const W: usize = 1280;
const H: usize = 720;
const FRAMES: usize = 90;
const FPS: f32 = 30.0;

/// A desktop: a page of text that scrolls for a second, a window dragged across it, a
/// line being typed.
fn desktop(t: usize) -> Frame {
    let scroll = if (20..50).contains(&t) {
        (t - 20) * 6
    } else if t >= 50 {
        180
    } else {
        0
    };
    let (wx, wy) = (100 + (t.saturating_sub(55)).min(30) * 12, 140);
    let typed = t / 2;
    let mut bgra = vec![0u8; W * H * 4];
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % W, i / W);
        let mut c = [246u8, 246, 246, 255];
        // Title bar and a sidebar.
        if y < 28 {
            c = [60, 60, 66, 255];
        } else if x < 180 {
            c = [232, 228, 224, 255];
        }
        // Page text (scrolls).
        let py = y + scroll;
        let (line, col) = (py / 18, x / 8);
        let in_text = y >= 28 && x >= 200 && (py % 18) > 3 && (py % 18) < 15;
        let glyph = ((x % 8) * 3 + (py % 18) * 5 + line * 7 + col * 3) % 7 < 3;
        let word = (col * 7 + line * 13) % 11 != 0;
        if in_text && glyph && word && col < 140 - (line * 37) % 50 {
            c = [24, 24, 28, 255];
        }
        // Typing on the last line.
        if (690..706).contains(&y) && x >= 200 && (x - 200) / 8 < typed && glyph {
            c = [20, 20, 160, 255];
        }
        // A dragged window with its own text.
        if (wx..wx + 420).contains(&x) && (wy..wy + 300).contains(&y) {
            c = if y < wy + 24 {
                [40, 110, 200, 255]
            } else if (y - wy) % 16 > 4 && ((x - wx) * 5 + y) % 9 < 4 && x > wx + 12 {
                [30, 30, 30, 255]
            } else {
                [255, 255, 255, 255]
            };
        }
        *px = c;
    }
    Frame {
        width: W,
        height: H,
        bgra,
        captured_at: Instant::now(),
    }
}

/// PSNR (dB) of the decoded RGBA against the BGRA source, over R, G and B.
fn psnr(src_bgra: &[u8], rgba: &[u8]) -> f64 {
    let (mut se, mut n) = (0f64, 0f64);
    for (s, d) in src_bgra
        .as_chunks::<4>()
        .0
        .iter()
        .zip(rgba.as_chunks::<4>().0)
    {
        for (a, b) in [(s[2], d[0]), (s[1], d[1]), (s[0], d[2])] {
            let e = f64::from(a) - f64::from(b);
            se += e * e;
            n += 1.0;
        }
    }
    10.0 * (255.0f64.powi(2) / (se / n).max(1e-9)).log10()
}

fn packetize(codec: VideoCodec, au: Vec<u8>, ts: u32, seq: &mut u16) -> Vec<rtc::rtp::Packet> {
    // What `VideoSender::write` does with each codec.
    let au = match codec {
        VideoCodec::H265 => rconn_peer::rtp_codecs::four_byte_start_codes(au),
        _ => au,
    };
    let au = Bytes::from(au);
    let payloads = match codec {
        VideoCodec::H264 => rtc::rtp::codec::h264::H264Payloader::default().payload(1200, &au),
        VideoCodec::H265 => rtc::rtp::codec::h265::HevcPayloader.payload(1200, &au),
        VideoCodec::Av1 => rtc::rtp::codec::av1::Av1Payloader {}.payload(1200, &au),
    }
    .expect("payload");
    let n = payloads.len();
    payloads
        .into_iter()
        .enumerate()
        .map(|(i, payload)| {
            *seq = seq.wrapping_add(1);
            rtc::rtp::Packet {
                header: rtc::rtp::header::Header {
                    version: 2,
                    marker: i + 1 == n,
                    payload_type: payload_type(codec),
                    sequence_number: *seq,
                    timestamp: ts,
                    ssrc: 7,
                    ..Default::default()
                },
                payload,
            }
        })
        .collect()
}

#[derive(Debug, Default)]
struct Run {
    engine: String,
    bytes: usize,
    key_bytes: usize,
    packets: usize,
    frames_out: usize,
    psnr: f64,
    min_psnr: f64,
    encode: Duration,
    decode: Duration,
}

fn run(codec: VideoCodec, bitrate_bps: u32, frames: &[Frame]) -> Run {
    let lib = rconn_media::cisco::local_binary_path().expect("Cisco binary path");
    let mut enc = VideoEncoder::new(
        &lib,
        EncoderSettings {
            bitrate_bps,
            max_fps: FPS,
            threads: 0,
            slice_bytes: None,
        },
    )
    .expect("encoder");
    enc.set_codec(codec);
    let mut dec = VideoDecoder::new(&lib).expect("decoder");
    let mut rx = VideoReceiver::default();
    let mut r = Run {
        min_psnr: f64::MAX,
        ..Run::default()
    };
    let mut seq = 0u16;
    for (k, frame) in frames.iter().enumerate() {
        // A live bitrate change (no keyframe), then a keyframe asked for.
        if k == 40 {
            enc.set_bitrate(bitrate_bps * 2);
        }
        if k == 60 {
            enc.request_keyframe();
        }
        let t0 = Instant::now();
        let Some(au) = enc.encode(frame).expect("encode") else {
            continue;
        };
        r.encode += t0.elapsed();
        r.engine = enc.engine_name().to_owned();
        let info = enc.last_frame();
        assert_eq!(info.key, is_keyframe(codec, &au), "{codec:?} frame {k}");
        match k {
            0 | 60 => {
                assert!(info.key, "{codec:?}: keyframe at {k}");
                // Self-contained: the stream headers ride with it, and say the size.
                let sps = sequence_header_of(codec, &au).expect("stream headers");
                let s = sequence_info(codec, sps).expect("readable");
                assert_eq!((s.width, s.height), (W, H), "{codec:?}");
            }
            40 => assert!(!info.key, "{codec:?}: a bitrate change makes no keyframe"),
            _ => {}
        }
        r.bytes += au.len();
        if info.key {
            r.key_bytes += au.len();
        }
        let au = frames::tag(
            codec,
            au,
            &FrameTag {
                id: info.id,
                key: info.key,
                ..FrameTag::default()
            },
        );
        let packets = packetize(codec, au, (k as u32) * 3000, &mut seq);
        r.packets += packets.len();
        for p in packets {
            for au in rx.push(p) {
                assert_eq!(rx.codec(), codec, "codec from the payload type");
                dec.set_codec(rx.codec()).expect("a decoder for the codec");
                let (bits, tag) = frames::take(codec, &au);
                assert_eq!(tag.map(|t| t.id), Some(info.id), "the tag rode along");
                let t0 = Instant::now();
                let pic = dec
                    .decode(&bits)
                    .expect("decode")
                    .expect("a picture per frame");
                r.decode += t0.elapsed();
                assert_eq!((pic.width, pic.height), (W, H));
                let p = psnr(&frame.bgra, &pic.to_rgba());
                r.psnr += p;
                r.min_psnr = r.min_psnr.min(p);
                r.frames_out += 1;
            }
        }
    }
    assert!(!rx.take_loss());
    r.psnr /= r.frames_out.max(1) as f64;
    r
}

#[test]
fn hevc_round_trips_and_is_compared_with_h264() {
    if !rconn_media::vt_encode::encoder_exists(VideoCodec::H265, true) {
        eprintln!("no hardware HEVC encoder here; skipping");
        return;
    }
    let frames: Vec<Frame> = (0..FRAMES).map(desktop).collect();
    println!(
        "{W}x{H}, {FRAMES} frames at {FPS} fps (text page scrolling, a window dragged, typing)"
    );
    println!(
        "| codec | bitrate | bytes | keyframe bytes | packets | mean PSNR | min PSNR | encode ms/frame | decode ms/frame | engine |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for kbps in [1000, 3000] {
        let mut results = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let r = run(codec, kbps * 1000, &frames);
            assert_eq!(r.frames_out, FRAMES, "{codec:?}: every frame decoded");
            assert!(r.psnr > 25.0, "{codec:?}: {:.1} dB", r.psnr);
            let n = r.frames_out as f64;
            println!(
                "| {} | {} kbit/s | {} | {} | {} | {:.2} dB | {:.2} dB | {:.2} | {:.2} | {} |",
                codec.name(),
                kbps,
                r.bytes,
                r.key_bytes,
                r.packets,
                r.psnr,
                r.min_psnr,
                r.encode.as_secs_f64() * 1e3 / n,
                r.decode.as_secs_f64() * 1e3 / n,
                r.engine
            );
            results.push(r);
        }
        println!(
            "HEVC vs H.264 at {kbps} kbit/s: {:+.1} % bytes, {:+.2} dB",
            (results[1].bytes as f64 / results[0].bytes as f64 - 1.0) * 100.0,
            results[1].psnr - results[0].psnr
        );
    }
}
