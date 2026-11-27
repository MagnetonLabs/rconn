//! The host's screen changes size mid-stream (the viewer resized its window): each size
//! is a new encoder, a new sequence header and a keyframe. The viewer's receive path
//! (RTP reassembly, decoder, keyframe requests) must keep showing pictures at the new
//! size, also when the keyframe of a new size is lost on the way.
//!
//! The software decoder is what the Windows viewer uses; on macOS the hardware decoder
//! (VideoToolbox) is checked too. Needs `scripts/fetch-openh264.sh`.

use bytes::Bytes;
use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder};
use rconn_peer::video::{H264_PAYLOAD_TYPE, KeyframeNeed, VideoReceiver};
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::packetizer::Payloader;
use std::time::{Duration, Instant};

/// One simulated step: 10 frames a second (enough for the logic, quick in debug builds).
const STEP: Duration = Duration::from_millis(100);

/// A flat desktop with a box at `t` (moving when the screen is "busy").
fn screen(w: usize, h: usize, t: usize) -> Frame {
    let mut bgra = vec![180u8; w * h * 4];
    let x0 = (40 + t * 12) % (w - 100);
    for y in 60..140 {
        let row = &mut bgra[(y * w + x0) * 4..(y * w + x0 + 80) * 4];
        row.fill(30);
    }
    Frame {
        width: w,
        height: h,
        bgra,
        captured_at: Instant::now(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Loss {
    None,
    /// Every packet of the new size's keyframe.
    WholeKeyframe,
    /// A run of packets from the middle of it.
    PartOfKeyframe,
}

/// What the viewer showed: picture sizes in order, and how many keyframes it asked for.
struct Outcome {
    sizes: Vec<(usize, usize)>,
    requests: u32,
}

/// Sizes as in the field report: 1496x934, then 1280x800, then 1920x1080 for 4 s.
/// `busy`: the screen changes every frame; otherwise it is still after each change
/// (only keyframes asked for are sent), the case that froze for good.
fn run(make_decoder: &dyn Fn() -> VideoDecoder, loss: Loss, busy: bool) -> Outcome {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    let schedule = [((1496, 934), 20), ((1280, 800), 10), ((1920, 1080), 40)];
    let mut dec = make_decoder();
    let mut rx = VideoReceiver::default();
    let mut need = KeyframeNeed::default();
    let t0 = Instant::now();
    let (mut seq, mut step) = (0u16, 0u32);
    let mut host_wants_key = false;
    let mut out = Outcome {
        sizes: Vec::new(),
        requests: 0,
    };
    for (n, &((w, h), steps)) in schedule.iter().enumerate() {
        // A new size: the host's capture restarts with a new encoder.
        let mut enc = VideoEncoder::new(
            &lib,
            EncoderSettings {
                slice_bytes: Some(1100), // several slices, like Media Foundation's
                ..EncoderSettings::default()
            },
        )
        .unwrap();
        for k in 0..steps {
            let now = t0 + STEP * step;
            step += 1;
            let first = k == 0;
            // As the host's stream does: a new capture's first keyframe is sent again
            // after 1 s if nothing else went out (STILL_REPEAT in the host agent).
            let repeat = n > 0 && !busy && k == 10;
            if first || busy || host_wants_key || repeat {
                if std::mem::take(&mut host_wants_key) || repeat {
                    enc.request_keyframe();
                }
                let t = if busy { k as usize } else { 0 };
                if let Some(au) = enc.encode(&screen(w, h, t)).unwrap() {
                    let mut packets = packetize(&au, 90 * STEP.as_millis() as u32 * step, &mut seq);
                    let last_size = n == schedule.len() - 1;
                    if first && last_size {
                        match loss {
                            Loss::None => {}
                            Loss::WholeKeyframe => packets.clear(),
                            Loss::PartOfKeyframe => {
                                let m = packets.len() / 2;
                                packets.drain(m - 3..m + 3);
                            }
                        }
                    }
                    for p in packets {
                        if !p.payload.is_empty() {
                            need.data(now);
                        }
                        for au in rx.push(p) {
                            need.frame(now);
                            match dec.decode(&au) {
                                Ok(Some(f)) => {
                                    need.picture();
                                    out.sizes.push((f.width, f.height));
                                }
                                Ok(None) => {}
                                Err(_) => need.lost(),
                            }
                        }
                        if rx.take_loss() {
                            need.lost();
                        }
                    }
                }
            }
            // The viewer's timer; a request reaches the host for its next frame.
            if need.poll(now) {
                out.requests += 1;
                host_wants_key = true;
            }
        }
    }
    out
}

/// One RTP frame (Annex-B access unit) at `timestamp`, sequence numbers from `seq`.
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

/// Builds a fresh decoder of one kind.
type MakeDecoder = Box<dyn Fn() -> VideoDecoder>;

fn decoders() -> Vec<(&'static str, MakeDecoder)> {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    assert!(
        lib.exists(),
        "missing {}; run scripts/fetch-openh264.sh",
        lib.display()
    );
    let sw = lib.clone();
    #[allow(unused_mut)]
    let mut v: Vec<(&'static str, MakeDecoder)> = vec![(
        "openh264 (the Windows viewer's)",
        Box::new(move || VideoDecoder::software(&sw).unwrap()),
    )];
    #[cfg(target_os = "macos")]
    v.push((
        "VideoToolbox",
        Box::new(move || VideoDecoder::new(&lib).unwrap()),
    ));
    v
}

/// Every size reaches the screen, in order, ending at the last one.
fn assert_follows_every_size(name: &str, o: &Outcome, case: &str) {
    let mut seen: Vec<(usize, usize)> = o.sizes.clone();
    seen.dedup();
    assert_eq!(
        seen,
        [(1496, 934), (1280, 800), (1920, 1080)],
        "{name}, {case}: sizes shown"
    );
}

#[test]
fn every_new_size_is_shown_when_nothing_is_lost() {
    for (name, make) in decoders() {
        for busy in [true, false] {
            let o = run(&*make, Loss::None, busy);
            assert_follows_every_size(name, &o, "no loss");
            assert_eq!(o.requests, 0, "{name}: nothing to ask for");
            if busy {
                let last = o.sizes.iter().filter(|&&s| s == (1920, 1080)).count();
                assert!(
                    last >= 35,
                    "{name}: pictures keep coming at 1920x1080: {last}"
                );
            }
        }
    }
}

#[test]
fn a_lost_keyframe_at_the_new_size_is_recovered() {
    for (name, make) in decoders() {
        for loss in [Loss::WholeKeyframe, Loss::PartOfKeyframe] {
            for busy in [true, false] {
                let case = format!(
                    "{} keyframe lost, {} screen",
                    if loss == Loss::WholeKeyframe {
                        "whole"
                    } else {
                        "part of the"
                    },
                    if busy { "busy" } else { "still" }
                );
                let o = run(&*make, loss, busy);
                assert_follows_every_size(name, &o, &case);
                // A still screen whose keyframe lost all but its tail (VideoToolbox's
                // keyframe of this flat screen is 7 packets) gives the viewer nothing to
                // decode or count as lost; the host's repeat of that keyframe (after 1 s)
                // gets there as the viewer's stall timer (1 s) would ask for it.
                let fewest = u32::from(busy || loss == Loss::WholeKeyframe);
                assert!(
                    (fewest..=4).contains(&o.requests),
                    "{name}, {case}: asked for a keyframe, without a storm: {}",
                    o.requests
                );
                if busy {
                    let last = o.sizes.iter().filter(|&&s| s == (1920, 1080)).count();
                    assert!(
                        last >= 25,
                        "{name}, {case}: pictures keep coming at 1920x1080: {last}"
                    );
                }
            }
        }
    }
}
