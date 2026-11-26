//! Loss repair end to end, without a keyframe: openh264 with long-term references on the
//! host, frames tagged with ids and sent as RTP, one packet of a frame lost on the way,
//! the viewer's receiver, integrity tracker and decoder, its feedback back to the host
//! two frames later (a round trip). The picture must continue, bit-exact with a viewer
//! that lost nothing, with no keyframe but the first.
//!
//! `cargo test -p rconn-peer --test loss_repair -- --nocapture` prints the numbers.
//! Needs `scripts/fetch-openh264.sh`.

use bytes::Bytes;
use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder, has_idr};
use rconn_peer::frames::{self, Feedback, FrameTag, Integrity, Verdict};
use rconn_peer::video::{H264_PAYLOAD_TYPE, VideoReceiver};
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::packetizer::Payloader;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33);
/// Feedback reaches the host this many frames later.
const ROUND_TRIP_FRAMES: usize = 2;

/// A light desktop with text-like strokes; a line grows by one "character" a frame.
fn screen(w: usize, h: usize, k: usize) -> Frame {
    let mut bgra = vec![240u8; w * h * 4];
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % w, i / w);
        let line = y / 16;
        let col = x / 8;
        let typed = line != 10 || col < k % 70;
        if y % 16 > 3 && (x * 3 + y * 5 + line) % 7 < 3 && (col + line) % 9 != 0 && typed {
            *px = [40, 40, 40, 255];
        }
    }
    Frame {
        width: w,
        height: h,
        bgra,
        captured_at: Instant::now(),
    }
}

fn packetize(au: &[u8], timestamp: u32, seq: &mut u16) -> Vec<rtc::rtp::Packet> {
    let payloads = H264Payloader::default()
        .payload(1200, &Bytes::copy_from_slice(au))
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
                    payload_type: H264_PAYLOAD_TYPE,
                    sequence_number: *seq,
                    timestamp,
                    ssrc: 1,
                    ..Default::default()
                },
                payload,
            }
        })
        .collect()
}

fn encoder(lib: &std::path::Path) -> VideoEncoder {
    let settings = EncoderSettings {
        bitrate_bps: 3_000_000,
        slice_bytes: Some(1100),
        ..EncoderSettings::default()
    };
    #[cfg(target_os = "macos")]
    let mut e = VideoEncoder::software(lib, settings).unwrap();
    #[cfg(not(target_os = "macos"))]
    let mut e = VideoEncoder::new(lib, settings).unwrap();
    e.set_long_term_refs(true);
    e
}

#[test]
fn a_lost_packet_is_repaired_without_a_keyframe() {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    assert!(lib.exists(), "run scripts/fetch-openh264.sh");
    let mut enc = encoder(&lib);
    let mut reference = VideoDecoder::software(&lib).unwrap();
    let mut dec = VideoDecoder::software(&lib).unwrap();
    let mut rx = VideoReceiver::default();
    let mut integrity = Integrity::default();
    let mut in_flight: VecDeque<(usize, Feedback)> = VecDeque::new();
    let t0 = Instant::now();
    let (mut seq, mut sent) = (0u16, 0usize);
    // Packets lost: one from the middle of these frames (counted in frames sent).
    let losses = [60usize, 150, 240];
    let mut lost_at: Vec<usize> = Vec::new();
    let mut repaired_at: Vec<usize> = Vec::new();
    let (mut keys, mut repairs, mut shown, mut identical) = (0, 0, 0, 0);
    let mut first_key_bytes = 0;
    let mut repair_bytes = Vec::new();
    let mut reference_pictures = std::collections::HashMap::new();
    for k in 0..330 {
        let now = t0 + FRAME * k as u32;
        // Feedback that has made its way back.
        while in_flight.front().is_some_and(|&(at, _)| at <= k) {
            let (_, f) = in_flight.pop_front().unwrap();
            if f.lost {
                enc.request_ltr_recovery(f.decoded);
            } else {
                enc.ltr_acked(f.decoded);
            }
        }
        let Some(au) = enc.encode(&screen(640, 400, k)).unwrap() else {
            continue;
        };
        let info = enc.last_frame();
        keys += u32::from(has_idr(&au));
        if info.key {
            first_key_bytes = au.len();
        }
        if info.recovers_from.is_some() {
            repairs += 1;
            repair_bytes.push(au.len());
        }
        let au = frames::tag(
            rconn_peer::video::VideoCodec::H264,
            au,
            &FrameTag {
                id: info.id,
                key: info.key,
                recovers_from: info.recovers_from,
                host_us: 0,
            },
        );
        if let Some(p) = reference.decode(&au).unwrap() {
            reference_pictures.insert(info.id, p.data);
        }
        let mut packets = packetize(&au, 3000 * k as u32, &mut seq);
        if losses.contains(&sent) {
            let m = packets.len() / 2;
            packets.remove(m);
            lost_at.push(k);
        }
        sent += 1;
        for p in packets {
            for au in rx.push(p) {
                // As the viewer does: no tag means the frame's head was lost.
                let Some(tag) = frames::read(rconn_peer::video::VideoCodec::H264, &au) else {
                    integrity.untagged();
                    continue;
                };
                rx.set_early_after_gaps(true);
                if integrity.frame(&tag) == Verdict::Skip {
                    continue;
                }
                if tag.recovers_from.is_some() {
                    repaired_at.push(k);
                }
                match dec.decode(&au) {
                    Ok(Some(p)) => {
                        shown += 1;
                        if reference_pictures.get(&tag.id) == Some(&p.data) {
                            identical += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => integrity.decode_failed(tag.id),
                }
            }
        }
        if let Some(f) = integrity.poll(now) {
            in_flight.push_back((k + ROUND_TRIP_FRAMES, f));
        }
    }
    let recovery_ms: Vec<u128> = lost_at
        .iter()
        .zip(&repaired_at)
        .map(|(&l, &r)| (FRAME * (r - l) as u32).as_millis())
        .collect();
    println!(
        "{} losses: {repairs} repairs ({repair_bytes:?} bytes, first keyframe {first_key_bytes}), \
         {keys} keyframe(s), picture back after {recovery_ms:?} ms (frame interval {} ms, \
         round trip {} frames), {shown} pictures shown, {identical} identical to a lossless viewer's",
        losses.len(),
        FRAME.as_millis(),
        ROUND_TRIP_FRAMES
    );
    assert_eq!(keys, 1, "no keyframe but the first");
    assert_eq!(repairs, losses.len());
    assert_eq!(repaired_at.len(), losses.len(), "every loss repaired");
    assert_eq!(shown, identical, "every picture shown is exact");
    assert!(shown > 300, "the picture continued: {shown}");
}
