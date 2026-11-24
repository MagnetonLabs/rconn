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
pub const START_BITRATE_BPS: f64 = 2_500_000.0;
pub const MAX_BITRATE_BPS: f64 = 20_000_000.0;

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
}

impl Default for VideoReceiver {
    fn default() -> Self {
        // Hold up to ~512 packets for reordering before giving up on a frame.
        Self {
            builder: SampleBuilder::new(512, H264Packet::default(), 90_000),
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
            out.push(sample.data);
        }
        out
    }
}
