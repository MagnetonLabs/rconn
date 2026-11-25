//! Video over WebRTC: H.264 codec registration, send-side congestion control (GCC) with
//! its estimate published for the encoder, and the H.264 track on each side.

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use rtc::interceptor::{BandwidthEstimator, EstimatorStats, Gcc, PacketReport, Registry};
use rtc::media::Sample;
use rtc::media::io::sample_builder::SampleBuilder;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    CongestionFeedback, configure_congestion_control, register_default_interceptors,
};
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::PeerConnection;
use webrtc::rtp_transceiver::RtpSender;

pub const H264_PAYLOAD_TYPE: u8 = 102;
/// Constrained Baseline, level 3.1, non-interleaved packetization: what openh264 emits.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

pub const MIN_BITRATE_BPS: f64 = 300_000.0;
/// High enough for a sharp full-resolution desktop from the first second; congestion
/// control lowers it within a few round trips on a slow link.
pub const START_BITRATE_BPS: f64 = 8_000_000.0;
pub const MAX_BITRATE_BPS: f64 = 25_000_000.0;

pub const OPUS_PAYLOAD_TYPE: u8 = 111;

fn opus() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn h264() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Latest congestion-control estimate in bits/s, shared with the encoder loop.
#[derive(Clone)]
pub struct TargetBitrate(Arc<AtomicU64>);

impl TargetBitrate {
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// GCC wrapped so every estimate update is published to [`TargetBitrate`].
struct Reporting<E: BandwidthEstimator> {
    inner: E,
    target: Arc<AtomicU64>,
}

impl<E: BandwidthEstimator> Reporting<E> {
    fn publish(&self) {
        self.target
            .store(self.inner.target_bitrate().to_bits(), Ordering::Relaxed);
    }
}

impl<E: BandwidthEstimator> BandwidthEstimator for Reporting<E> {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.inner.on_reports(now, reports);
        self.publish();
    }
    fn target_bitrate(&self) -> f64 {
        self.inner.target_bitrate()
    }
    fn handle_timeout(&mut self, now: Instant) {
        self.inner.handle_timeout(now);
        self.publish();
    }
    fn poll_timeout(&self) -> Option<Instant> {
        self.inner.poll_timeout()
    }
    fn stats(&self) -> EstimatorStats {
        self.inner.stats()
    }
}

/// Media engine (H.264) and interceptors (NACK, reports, TWCC + GCC + pacer).
pub fn media_stack() -> Result<(MediaEngine, Registry, TargetBitrate)> {
    let mut engine = MediaEngine::default();
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: h264(),
            payload_type: H264_PAYLOAD_TYPE,
        },
        RtpCodecKind::Video,
    )?;
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)?;
    let gcc = Gcc::new(START_BITRATE_BPS, MIN_BITRATE_BPS, MAX_BITRATE_BPS);
    let target = Arc::new(AtomicU64::new(gcc.target_bitrate().to_bits()));
    let estimator = Reporting {
        inner: gcc,
        target: target.clone(),
    };
    let registry =
        configure_congestion_control(registry, estimator, CongestionFeedback::Twcc, &mut engine)?;
    Ok((engine, registry, TargetBitrate(target)))
}

/// The host's outgoing H.264 track.
/// A host-side outgoing media track (the screen's H.264, or the sound's Opus).
pub struct VideoSender {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    started: Instant,
}

impl VideoSender {
    /// Attach a video track. Call on the answerer after `set_remote_description` and before
    /// `create_answer`, so it binds to the viewer's recv-only transceiver.
    pub async fn attach(pc: &dyn PeerConnection) -> Result<Self> {
        let ssrc: u32 = rand_u32();
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "rconn-screen".into(),
                "rconn-screen-video".into(),
                "screen".into(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: h264(),
                    ..Default::default()
                }],
            ),
        )?);
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .context("adding video track")?;
        Ok(Self {
            track,
            sender,
            started: Instant::now(),
        })
    }

    /// Send one Annex-B access unit lasting `duration`.
    pub async fn write(&self, access_unit: Vec<u8>, duration: Duration) -> Result<()> {
        let pt = self
            .sender
            .get_parameters()
            .await?
            .rtp_parameters
            .codecs
            .first()
            .map(|c| c.payload_type)
            .ok_or_else(|| anyhow!("video sender has no negotiated codec yet"))?;
        let ssrc = *self
            .track
            .ssrcs()
            .await
            .first()
            .ok_or_else(|| anyhow!("video track has no SSRC"))?;
        self.track
            .sample_writer(ssrc, pt)
            .write_sample(&Sample {
                data: Bytes::from(access_unit),
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
        Ok(())
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

fn rand_u32() -> u32 {
    let b = *uuid::Uuid::new_v4().as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The host's outgoing Opus track (host sound, phase 7).
pub struct AudioSender {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

impl AudioSender {
    /// Attach after `set_remote_description`, before `create_answer` (binds to the
    /// viewer's recv-only audio transceiver).
    pub async fn attach(pc: &dyn PeerConnection) -> Result<Self> {
        let ssrc: u32 = rand_u32();
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "rconn-sound".into(),
                "rconn-sound-audio".into(),
                "sound".into(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: opus(),
                    ..Default::default()
                }],
            ),
        )?);
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .context("adding audio track")?;
        Ok(Self { track, sender })
    }

    /// Send one Opus packet covering `duration` (20 ms frames in practice).
    pub async fn write(&self, packet: Vec<u8>, duration: Duration) -> Result<()> {
        let pt = self
            .sender
            .get_parameters()
            .await?
            .rtp_parameters
            .codecs
            .first()
            .map(|c| c.payload_type)
            .ok_or_else(|| anyhow!("audio sender has no negotiated codec yet"))?;
        let ssrc = *self
            .track
            .ssrcs()
            .await
            .first()
            .ok_or_else(|| anyhow!("audio track has no SSRC"))?;
        self.track
            .sample_writer(ssrc, pt)
            .write_sample(&Sample {
                data: Bytes::from(packet),
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
        Ok(())
    }
}

/// Reassembles RTP packets into H.264 access units on the viewer.
pub struct VideoReceiver {
    builder: SampleBuilder<H264Packet>,
    /// Packets were lost for good since the last `take_loss`: the picture that follows
    /// is missing data, so the viewer should ask for a keyframe.
    lost: bool,
}

impl Default for VideoReceiver {
    fn default() -> Self {
        // Room for big frames: a detailed full-size keyframe (a photo wallpaper at
        // 2992x1878) is ~1 MB, ~900 packets; at 512 it could never complete. 4096 packets
        // covers ~4.5 MB frames.
        //
        // But give up on a frame with a lost packet after 250 ms of newer video (a
        // retransmit takes a round trip): a packet-count limit alone meant one lost packet
        // froze the picture until 4096 more arrived, 15-20 s at desktop bitrates.
        Self {
            builder: SampleBuilder::new(4096, H264Packet::default(), 90_000)
                .with_max_time_delay(LOSS_GIVE_UP),
            lost: false,
        }
    }
}

impl VideoReceiver {
    /// Feed one RTP packet; returns any access units it completed (Annex-B).
    pub fn push(&mut self, packet: rtc::rtp::Packet) -> Vec<Bytes> {
        let now = Instant::now();
        self.builder.push(now, packet);
        let mut out = Vec::new();
        while let Some(sample) = self.builder.pop(now) {
            self.lost |= sample.prev_dropped_packets > 0;
            out.push(sample.data);
        }
        out
    }

    /// Were packets lost for good since the last call? (Then ask the host for a keyframe:
    /// later pictures build on the incomplete one.)
    pub fn take_loss(&mut self) -> bool {
        std::mem::take(&mut self.lost)
    }
}

/// How long the viewer waits for a missing packet before giving up on its frame.
pub const LOSS_GIVE_UP: Duration = Duration::from_millis(250);

#[cfg(test)]
mod receiver_tests {
    use super::*;
    use rtc::rtp::codec::h264::H264Payloader;
    use rtc::rtp::packetizer::Payloader;

    /// Packetize `au` as one RTP frame at `timestamp`, starting at sequence `seq`.
    fn packets(au: &[u8], timestamp: u32, seq: &mut u16) -> Vec<rtc::rtp::Packet> {
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

    /// A detailed full-size keyframe (~1 MB, ~900 packets) must come out whole: at a
    /// 512-packet limit it was silently discarded and the viewer never got a picture.
    #[test]
    fn a_one_megabyte_keyframe_is_reassembled() {
        let mut big = vec![0, 0, 0, 1, 0x65]; // IDR slice
        big.extend((0..1_050_000u32).map(|i| (i % 251) as u8 | 1)); // no start codes inside
        let small = [0u8, 0, 0, 1, 0x41, 7, 7, 7]; // the next frame, to close the first
        let mut rx = VideoReceiver::default();
        let mut seq = 100;
        let mut out = Vec::new();
        for p in packets(&big, 1000, &mut seq)
            .into_iter()
            .chain(packets(&small, 4000, &mut seq))
        {
            out.extend(rx.push(p));
        }
        let first = out.first().expect("the keyframe came out");
        assert!(
            first.len() >= big.len() - 4,
            "whole frame: {} bytes",
            first.len()
        );
        assert!(!rx.take_loss());
    }

    /// One packet lost for good must not freeze the picture: the frame is given up once
    /// newer video is LOSS_GIVE_UP ahead, later frames flow, and the loss is reported.
    #[test]
    fn a_lost_packet_costs_one_frame_not_the_stream() {
        let mut rx = VideoReceiver::default();
        let mut seq = 7;
        let mut big = vec![0, 0, 0, 1, 0x41];
        big.extend((0..20_000u32).map(|i| (i % 251) as u8 | 1));
        let mut packets_in = packets(&big, 0, &mut seq);
        packets_in.remove(3); // lost, never retransmitted
        let mut out = Vec::new();
        for p in packets_in {
            out.extend(rx.push(p));
        }
        // 30 fps of small frames after it: 0.5 s of video.
        for k in 1..=15u32 {
            // (Frames of a few bytes: a 1-byte slice would be dropped as padding.)
            for p in packets(
                &[0, 0, 0, 1, 0x41, k as u8 | 1, 9, 9, 9],
                k * 3000,
                &mut seq,
            ) {
                out.extend(rx.push(p));
            }
        }
        assert!(out.len() >= 10, "later frames came through: {}", out.len());
        assert!(rx.take_loss(), "the loss is reported (ask for a keyframe)");
    }
}
