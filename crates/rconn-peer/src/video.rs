//! Video over WebRTC: codec registration (H.264, and HEVC/AV1 where both ends can),
//! send-side congestion control ([`Bwe`]) with its estimate published for the encoder,
//! and the video track on each side.

use crate::bwe::{Bwe, PACING_FACTOR};
use crate::rtp_codecs::{CodecDepacketizer, four_byte_start_codes};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
pub use rconn_core::codec::VideoCodec;
use rtc::interceptor::{
    BandwidthEstimator, CongestionControlBuilder, EstimatorStats, PacerBuilder, PacketReport,
    Registry, Slot, TwccReceiverBuilder, TwccSenderBuilder,
};
use rtc::media::Sample;
use rtc::media::io::sample_builder::SampleBuilder;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    configure_twcc_receiver_only, register_default_interceptors,
};
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::rtp::packetizer::Depacketizer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::PeerConnection;
use webrtc::rtp_transceiver::RtpSender;

pub const H264_PAYLOAD_TYPE: u8 = 102;
/// Constrained Baseline, level 3.1, non-interleaved packetization: what openh264 emits.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
/// HEVC (RFC 7798) and AV1: offered only by viewers that decode them (0.1.7 on), so older
/// peers never see these payload types.
pub const H265_PAYLOAD_TYPE: u8 = 104;
pub const AV1_PAYLOAD_TYPE: u8 = 105;

/// The payload type each side registers `codec` under. The host answers with the
/// viewer's numbers, and the viewer tells the codecs apart by them.
pub fn payload_type(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => H264_PAYLOAD_TYPE,
        VideoCodec::H265 => H265_PAYLOAD_TYPE,
        VideoCodec::Av1 => AV1_PAYLOAD_TYPE,
    }
}

/// The codec registered under payload type `pt`.
pub fn codec_of_payload_type(pt: u8) -> Option<VideoCodec> {
    VideoCodec::LADDER
        .into_iter()
        .find(|&c| payload_type(c) == pt)
}

/// The video codecs an SDP lists (the `a=rtpmap` lines of its video sections).
pub fn sdp_video_codecs(sdp: &str) -> Vec<VideoCodec> {
    let mut out = Vec::new();
    let mut video = false;
    for line in sdp.lines() {
        if let Some(m) = line.strip_prefix("m=") {
            video = m.starts_with("video");
        } else if let Some(map) = line.strip_prefix("a=rtpmap:")
            && video
            && let Some(name) = map.split_whitespace().nth(1)
            && let Some(name) = name.split('/').next()
            && let Some(c) = VideoCodec::LADDER
                .into_iter()
                .find(|c| c.rtpmap_name().eq_ignore_ascii_case(name))
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

pub const MIN_BITRATE_BPS: f64 = 300_000.0;
/// High enough for a sharp full-resolution desktop from the first second; congestion
/// control lowers it within a few round trips on a slow link.
pub const START_BITRATE_BPS: f64 = 8_000_000.0;
pub const MAX_BITRATE_BPS: f64 = 25_000_000.0;
/// The encoder aims at this share of the estimate; the rest is room for retransmissions,
/// sound and the encoder's own overshoot.
pub const ENCODER_SHARE: f64 = 0.9;
/// What sound takes of the estimate (Opus at its usual rate), bits/s.
const AUDIO_BPS: f64 = 64_000.0;
/// The most the pacer sends back-to-back, bits: four full packets. `rtc`'s default is
/// 100 ms of the rate, a line-rate burst of ~125 KB at 10 Mb/s after any idle moment,
/// which the bottleneck queues and the delay detector reads as congestion.
pub const PACER_BURST_BITS: f64 = 4.0 * 1200.0 * 8.0;
/// How often the viewer reports arrivals (`rtc`'s default is 100 ms).
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

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

fn video_codec(codec: VideoCodec) -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: match codec {
            VideoCodec::H264 => MIME_TYPE_H264,
            VideoCodec::H265 => MIME_TYPE_HEVC,
            VideoCodec::Av1 => MIME_TYPE_AV1,
        }
        .to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: match codec {
            VideoCodec::H264 => H264_FMTP,
            _ => "",
        }
        .to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Latest congestion-control result, shared with the encoder loop: the encoder's target
/// ([`Self::get`]) and the estimate it is derived from ([`Self::estimate`]), bits/s.
#[derive(Clone)]
pub struct TargetBitrate {
    value: Arc<AtomicU64>,
    estimate: Arc<AtomicU64>,
    shield: Arc<Mutex<Option<Shield>>>,
}

/// Names one [`TargetBitrate::shield`] call.
#[derive(Clone, Copy, Debug)]
pub struct ShieldTicket {
    fresh: bool,
    until: Instant,
}

/// The estimate held up through a burst the host causes itself (see
/// [`TargetBitrate::shield`]).
#[derive(Clone, Copy, Debug)]
struct Shield {
    floor: f64,
    until: Instant,
}

impl TargetBitrate {
    fn new(estimate: f64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(encoder_target(estimate).to_bits())),
            estimate: Arc::new(AtomicU64::new(estimate.to_bits())),
            shield: Arc::default(),
        }
    }

    /// A target derived from a fixed `estimate` (bits/s), for streams without a peer
    /// connection (tests, benchmarks).
    pub fn fixed(estimate: f64) -> Self {
        Self::new(estimate)
    }

    /// What the encoder should aim at, bits/s.
    pub fn get(&self) -> f64 {
        f64::from_bits(self.value.load(Ordering::Relaxed))
    }

    /// The bandwidth estimate itself, bits/s (logs).
    pub fn estimate(&self) -> f64 {
        f64::from_bits(self.estimate.load(Ordering::Relaxed))
    }

    fn store(&self, estimate: f64) {
        self.estimate.store(estimate.to_bits(), Ordering::Relaxed);
        self.value
            .store(encoder_target(estimate).to_bits(), Ordering::Relaxed);
    }

    /// The host is about to cause a burst on purpose (the screen changes size: a new
    /// encoder, a full-size keyframe). Its queueing reads to congestion control like a
    /// congested link and collapsed the estimate after every resize (8.5 -> 1 -> 0.3
    /// Mb/s). For `window` the estimate is kept at least at today's value, and if it
    /// has fallen below that by the end, it starts again from it. Returns a ticket for
    /// [`Self::cancel_shield`].
    pub fn shield(&self, window: Duration) -> ShieldTicket {
        let floor = self.estimate();
        let mut s = self.shield.lock().expect("shield");
        let until = Instant::now() + window;
        // Only a shield this call started may be cancelled by it.
        let fresh = s.is_none();
        *s = Some(match *s {
            // Back-to-back resizes keep the value from before the first.
            Some(old) => Shield {
                floor: old.floor.max(floor),
                until: old.until.max(until),
            },
            None => Shield { floor, until },
        });
        ShieldTicket { fresh, until }
    }

    /// The burst did not happen after all (the screen already had that size): drop the
    /// shield, unless a later one has replaced it.
    pub fn cancel_shield(&self, ticket: ShieldTicket) {
        let mut s = self.shield.lock().expect("shield");
        if ticket.fresh && s.is_some_and(|n| n.until == ticket.until) {
            *s = None;
        }
    }
}

/// The encoder's share of an estimate.
fn encoder_target(estimate: f64) -> f64 {
    (estimate * ENCODER_SHARE - AUDIO_BPS).max(MIN_BITRATE_BPS)
}

/// The estimator wrapped so every update is published to [`TargetBitrate`], held up
/// through a deliberate burst ([`TargetBitrate::shield`]), and the pacer runs at
/// [`PACING_FACTOR`] x the estimate.
struct Reporting<E: BandwidthEstimator> {
    inner: E,
    /// A fresh estimator starting at the given bits/s.
    restart: fn(f64) -> E,
    target: TargetBitrate,
    drops: DropLog,
}

/// Logs large estimate drops and how long the estimate takes to come back: the measure
/// for self-inflicted collapses (a drop by more than half, then the time to 90 %).
#[derive(Default)]
struct DropLog {
    /// (when, estimate) over the last two seconds.
    recent: VecDeque<(Instant, f64)>,
    /// A drop in progress: the estimate before it, and when it happened.
    open: Option<(f64, Instant)>,
}

impl DropLog {
    fn note(&mut self, now: Instant, estimate: f64) {
        if let Some((before, at)) = self.open {
            if estimate >= 0.9 * before {
                tracing::info!(
                    to_kbps = (estimate / 1000.0) as u64,
                    after_ms = now.saturating_duration_since(at).as_millis() as u64,
                    "bandwidth estimate recovered to 90% of its level before the drop"
                );
                self.open = None;
            }
        } else if let Some(peak) = self.recent.iter().map(|&(_, e)| e).reduce(f64::max)
            && estimate < 0.5 * peak
        {
            tracing::warn!(
                from_kbps = (peak / 1000.0) as u64,
                to_kbps = (estimate / 1000.0) as u64,
                "bandwidth estimate dropped by more than half"
            );
            self.open = Some((peak, now));
        }
        if self
            .recent
            .back()
            .is_none_or(|&(t, _)| now >= t + Duration::from_millis(50))
        {
            self.recent.push_back((now, estimate));
        }
        while self
            .recent
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > Duration::from_secs(2))
        {
            self.recent.pop_front();
        }
    }
}

impl<E: BandwidthEstimator> Reporting<E> {
    fn publish(&mut self, now: Instant) {
        let mut estimate = self.inner.target_bitrate();
        let shield = *self.target.shield.lock().expect("shield");
        if let Some(s) = shield {
            if now < s.until {
                estimate = estimate.max(s.floor);
            } else {
                if estimate < s.floor {
                    self.inner = (self.restart)(s.floor);
                    estimate = self.inner.target_bitrate();
                }
                *self.target.shield.lock().expect("shield") = None;
            }
        }
        self.target.store(estimate);
        self.drops.note(now, estimate);
    }
}

impl<E: BandwidthEstimator> BandwidthEstimator for Reporting<E> {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.inner.on_reports(now, reports);
        self.publish(now);
    }
    /// What the pacer is asked for.
    fn target_bitrate(&self) -> f64 {
        self.target.estimate() * PACING_FACTOR
    }
    fn handle_timeout(&mut self, now: Instant) {
        self.inner.handle_timeout(now);
        self.publish(now);
    }
    fn poll_timeout(&self) -> Option<Instant> {
        self.inner.poll_timeout()
    }
    fn stats(&self) -> EstimatorStats {
        self.inner.stats()
    }
}

/// Media engine (H.264 and the `extra` video codecs, Opus) and interceptors (NACK,
/// reports, TWCC + [`Bwe`] + pacer). `extra`: what a viewer decodes, or a host encodes,
/// besides H.264 (which every peer has).
pub fn media_stack(extra: &[VideoCodec]) -> Result<(MediaEngine, Registry, TargetBitrate)> {
    let mut engine = MediaEngine::default();
    // Best first: the order of the offer.
    for codec in VideoCodec::LADDER {
        if codec != VideoCodec::H264 && !extra.contains(&codec) {
            continue;
        }
        engine.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: video_codec(codec),
                payload_type: payload_type(codec),
            },
            RtpCodecKind::Video,
        )?;
    }
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)?;
    let bwe = |start| Bwe::new(start, MIN_BITRATE_BPS, MAX_BITRATE_BPS);
    let inner = bwe(START_BITRATE_BPS);
    let target = TargetBitrate::new(inner.target_bitrate());
    let estimator = Reporting {
        inner,
        restart: bwe,
        target: target.clone(),
        drops: DropLog::default(),
    };
    let pacing = estimator.target_bitrate();
    // What `configure_congestion_control` sets up, with the pacer and the feedback
    // interval configured: the transport-wide CC feedback type and header extension in
    // the media engine (registered by the receiver-only helper; its chain is not used),
    // then send history, sequence numbers, pacer and arrival recorder.
    configure_twcc_receiver_only(Registry::new(), &mut engine)?;
    let registry = registry
        .with(
            Slot::CongestionControl,
            CongestionControlBuilder::new(estimator).build(),
        )
        .with(Slot::TwccSender, TwccSenderBuilder::new().build())
        .with(
            Slot::Pacer,
            // Start at the starting estimate, not `rtc`'s 1 Mb/s: the first keyframe goes
            // out before any feedback.
            PacerBuilder::new()
                .with_target_bitrate(pacing)
                .with_burst_bits(PACER_BURST_BITS)
                .build(),
        )
        .with(
            Slot::TwccReceiver,
            TwccReceiverBuilder::new()
                .with_interval(FEEDBACK_INTERVAL)
                .build(),
        );
    Ok((engine, registry, target))
}

/// The host's outgoing screen track, in the codec chosen for the session.
pub struct VideoSender {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    started: Instant,
    codec: VideoCodec,
}

impl VideoSender {
    /// Attach a video track sending `codec` (one the viewer offered). Call on the answerer
    /// after `set_remote_description` and before `create_answer`, so it binds to the
    /// viewer's recv-only transceiver.
    pub async fn attach(pc: &dyn PeerConnection, codec: VideoCodec) -> Result<Self> {
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
                    codec: video_codec(codec),
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
            codec,
        })
    }

    /// The codec this track sends.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Send one access unit (Annex-B for H.264/H.265, OBUs for AV1) lasting `duration`.
    pub async fn write(&self, access_unit: Vec<u8>, duration: Duration) -> Result<()> {
        let codecs = self.sender.get_parameters().await?.rtp_parameters.codecs;
        let pt = codecs
            .iter()
            .find(|c| {
                c.rtp_codec
                    .mime_type
                    .eq_ignore_ascii_case(self.codec.mime())
            })
            .map(|c| c.payload_type)
            .ok_or_else(|| anyhow!("video sender has no negotiated {} yet", self.codec.name()))?;
        let access_unit = if self.codec == VideoCodec::H265 {
            four_byte_start_codes(access_unit)
        } else {
            access_unit
        };
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

/// Reassembles RTP packets into access units on the viewer: Annex-B for H.264 and H.265,
/// OBUs for AV1. The codec follows the packets' payload type.
pub struct VideoReceiver {
    builder: SampleBuilder<CodecDepacketizer>,
    codec: VideoCodec,
    /// Packets were lost for good since the last `take_loss`: the picture that follows
    /// is missing data, so the viewer should ask for a keyframe.
    lost: bool,
    /// The frame arriving now, while its packets come in order right after the previous
    /// frame's last one (so none of it can be missing).
    frame: Vec<rtc::rtp::Packet>,
    /// Sequence number of the last packet that ended a frame.
    frame_end: Option<u16>,
    /// RTP timestamps of the frames handed out lately (each frame goes out once).
    delivered: VecDeque<u32>,
    /// When the first packet of the frame in progress arrived.
    frame_started: Option<Instant>,
    /// First to last packet of the last frame completed in order (stats).
    assembly: Option<Duration>,
    /// Also hand out complete frames that follow a gap at once (see
    /// [`Self::set_early_after_gaps`]).
    early_after_gaps: bool,
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
            builder: builder(VideoCodec::H264),
            codec: VideoCodec::H264,
            lost: false,
            frame: Vec::new(),
            frame_end: None,
            delivered: VecDeque::new(),
            frame_started: None,
            assembly: None,
            early_after_gaps: false,
        }
    }
}

fn builder(codec: VideoCodec) -> SampleBuilder<CodecDepacketizer> {
    SampleBuilder::new(4096, CodecDepacketizer::new(codec), 90_000)
        .with_max_time_delay(LOSS_GIVE_UP)
}

impl VideoReceiver {
    /// The codec of the stream (H.264 until a packet says otherwise).
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Feed one RTP packet; returns any access units it completed (Annex-B).
    ///
    /// The sample builder hands a frame out only once a packet of the *next* frame
    /// arrives. A still screen sends no next frame: the keyframe after a screen-size
    /// change (or any last change) waited, unseen, until something else moved. So a
    /// frame whose packets all came in order, from right after the previous frame's end
    /// to its own end, goes out at once; the builder's later copy of it is dropped.
    pub fn push(&mut self, packet: rtc::rtp::Packet) -> Vec<Bytes> {
        let now = Instant::now();
        if let Some(c) = codec_of_payload_type(packet.header.payload_type)
            && c != self.codec
        {
            // Not within a session today (the codec is negotiated once), but a stream
            // in another codec must never reach the old depacketizer.
            *self = Self {
                codec: c,
                builder: builder(c),
                early_after_gaps: self.early_after_gaps,
                ..Self::default()
            };
        }
        let early = self.follow(&packet);
        self.builder.push(now, packet);
        let mut out = Vec::new();
        while let Some(sample) = self.builder.pop(now) {
            self.lost |= sample.prev_dropped_packets > 0;
            if self.deliver(sample.packet_timestamp) {
                out.push(sample.data);
            }
        }
        if let Some((timestamp, data)) = early
            && self.deliver(timestamp)
        {
            out.push(data);
        }
        out
    }

    /// Track the frame in progress; the whole frame if `p` completed it.
    fn follow(&mut self, p: &rtc::rtp::Packet) -> Option<(u32, Bytes)> {
        let (seq, ts) = (p.header.sequence_number, p.header.timestamp);
        // Right after the previous frame's end, or the stream's first packet.
        let follows_end = self.frame_end.is_none_or(|end| end == seq.wrapping_sub(1));
        // After a gap, a frame starting with the first packet of a NAL unit may still be
        // missing a leading NAL unit; tagged streams catch that (no tag, or an id gap).
        let may_start = follows_end || self.early_after_gaps;
        if p.payload.is_empty() {
            // Padding: keeps the chain unbroken, belongs to no frame.
            if follows_end {
                self.frame_end = Some(seq);
            }
            self.frame.clear();
            return None;
        }
        let continues = self.frame.last().is_some_and(|l| {
            l.header.timestamp == ts && l.header.sequence_number.wrapping_add(1) == seq
        });
        if continues {
            self.frame.push(p.clone());
        } else if may_start && CodecDepacketizer::new(self.codec).is_partition_head(&p.payload) {
            self.frame = vec![p.clone()];
            self.frame_started = Some(Instant::now());
        } else {
            self.frame.clear();
        }
        if !p.header.marker {
            return None;
        }
        self.frame_end = Some(seq);
        let frame = std::mem::take(&mut self.frame);
        if !frame.is_empty() {
            self.assembly = self.frame_started.take().map(|t| t.elapsed());
        }
        if frame.is_empty() {
            return None;
        }
        let mut depacketizer = CodecDepacketizer::new(self.codec);
        let mut data = Vec::new();
        for p in &frame {
            data.extend_from_slice(&depacketizer.depacketize(&p.payload).ok()?);
        }
        (!data.is_empty()).then(|| (ts, Bytes::from(data)))
    }

    /// Hand out the frame at `timestamp` unless it already went out.
    fn deliver(&mut self, timestamp: u32) -> bool {
        if self.delivered.contains(&timestamp) {
            return false;
        }
        if self.delivered.len() == 32 {
            self.delivered.pop_front();
        }
        self.delivered.push_back(timestamp);
        true
    }

    /// Were packets lost for good since the last call? (Then ask the host for a keyframe:
    /// later pictures build on the incomplete one.)
    /// Hand out a complete frame at once even when packets before it were lost, instead
    /// of after [`LOSS_GIVE_UP`]: for streams whose frames carry ids, where the viewer
    /// knows from the ids what is missing and asks the host to repair it. Saves 250 ms
    /// per loss; frames can then come out of order (a late retransmission), which the
    /// ids also show.
    pub fn set_early_after_gaps(&mut self, on: bool) {
        self.early_after_gaps = on;
    }

    /// How long the last frame took to arrive, its first packet to its last (frames
    /// completed in order; `None` since the last call otherwise).
    pub fn take_assembly(&mut self) -> Option<Duration> {
        self.assembly.take()
    }

    pub fn take_loss(&mut self) -> bool {
        std::mem::take(&mut self.lost)
    }
}

/// How long the viewer waits for a missing packet before giving up on its frame.
pub const LOSS_GIVE_UP: Duration = Duration::from_millis(250);

/// Video arriving without a picture for this long: ask for a keyframe.
pub const STALL: Duration = Duration::from_secs(1);
/// Requests after a picture came: at most this often (each costs a full-size encode).
const RETRY_AFTER_PICTURE: Duration = Duration::from_millis(300);
/// Unanswered requests back off from `STALL` up to this: a full-size keyframe can take
/// seconds on a slow link, and asking again meanwhile only queues more of them.
const RETRY_MAX: Duration = Duration::from_secs(4);

/// When the viewer asks the host for a keyframe: after lost packets or a decode error,
/// and whenever video keeps arriving with no picture coming out of it. Some decoders say
/// nothing when a picture can't be made (VideoToolbox, given frames that build on a
/// keyframe it never saw, returns no error and no picture), so a lost keyframe after the
/// host changed screen size froze the picture for good: nothing ever asked for another.
#[derive(Debug, Default)]
pub struct KeyframeNeed {
    /// Packets were lost or a frame failed to decode, not yet asked about.
    owed: bool,
    /// Video has arrived since the last picture, since then.
    waiting_since: Option<Instant>,
    last_request: Option<Instant>,
    /// Requests since the last picture.
    unanswered: u32,
}

impl KeyframeNeed {
    /// Packets were lost for good, or a frame failed to decode.
    pub fn lost(&mut self) {
        self.owed = true;
    }

    /// Video data (an RTP packet with payload) arrived.
    pub fn data(&mut self, now: Instant) {
        self.waiting_since.get_or_insert(now);
    }

    /// A picture came out of the decoder.
    pub fn picture(&mut self) {
        self.waiting_since = None;
        self.unanswered = 0;
    }

    /// Should the viewer ask for a keyframe now? Call after every packet and on a timer
    /// (a lost keyframe on a still screen is followed by no packets at all).
    pub fn poll(&mut self, now: Instant) -> bool {
        let stalled = self
            .waiting_since
            .is_some_and(|t| now.saturating_duration_since(t) >= STALL);
        if !self.owed && !stalled {
            return false;
        }
        let gap = match self.unanswered {
            0 => RETRY_AFTER_PICTURE,
            n => (STALL * 2u32.saturating_pow(n - 1)).min(RETRY_MAX),
        };
        if self
            .last_request
            .is_some_and(|t| now.saturating_duration_since(t) < gap)
        {
            return false; // still owed: asked again once the gap is over
        }
        self.owed = false;
        self.last_request = Some(now);
        self.unanswered = self.unanswered.saturating_add(1);
        true
    }
}

#[cfg(test)]
mod keyframe_need_tests {
    use super::*;

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn video_without_a_picture_asks_for_a_keyframe_and_backs_off() {
        let t0 = Instant::now();
        let mut k = KeyframeNeed::default();
        k.picture();
        // P-frames of a stream whose keyframe was lost: data, no picture, no error.
        let mut asked = Vec::new();
        for t in (0..12_000).step_by(50) {
            k.data(ms(t0, t));
            if k.poll(ms(t0, t)) {
                asked.push(t);
            }
        }
        assert_eq!(
            asked,
            [1000, 2000, 4000, 8000],
            "1 s, then backing off to 4 s"
        );
    }

    #[test]
    fn a_picture_ends_the_stall() {
        let t0 = Instant::now();
        let mut k = KeyframeNeed::default();
        k.data(t0);
        assert!(!k.poll(ms(t0, 900)));
        k.picture();
        assert!(!k.poll(ms(t0, 1500)), "pictures are coming: nothing to ask");
        // An idle screen sends nothing: no data, no request, however long.
        assert!(!k.poll(ms(t0, 60_000)));
    }

    #[test]
    fn a_still_screen_after_a_lost_keyframe_is_noticed_without_more_packets() {
        let t0 = Instant::now();
        let mut k = KeyframeNeed::default();
        k.data(t0); // part of a keyframe that never completes; nothing follows
        assert!(!k.poll(ms(t0, 500)));
        assert!(k.poll(ms(t0, 1000)), "the timer finds it");
    }

    #[test]
    fn a_loss_inside_the_throttle_is_remembered_not_dropped() {
        let t0 = Instant::now();
        let mut k = KeyframeNeed::default();
        k.lost();
        assert!(k.poll(t0), "first loss: ask at once");
        k.picture(); // the keyframe came
        k.lost(); // and soon another loss
        assert!(!k.poll(ms(t0, 100)), "throttled");
        assert!(
            k.poll(ms(t0, 300)),
            "...but still asked once the gap is over"
        );
        assert!(!k.poll(ms(t0, 2000)), "and only once");
    }

    #[test]
    fn decode_errors_back_off_while_no_picture_comes() {
        let t0 = Instant::now();
        let mut k = KeyframeNeed::default();
        let mut asked = Vec::new();
        // An error on every frame at 30 fps, no picture for 10 s (a slow link).
        for t in (0..10_000).step_by(33) {
            k.data(ms(t0, t));
            k.lost();
            if k.poll(ms(t0, t)) {
                asked.push(t);
            }
        }
        assert_eq!(asked, [0, 1023, 3036, 7062], "not every 300 ms");
    }
}

#[cfg(test)]
mod shield_tests {
    use super::*;

    /// An estimator that falls to 1 Mb/s at the first feedback.
    struct Falls(f64);

    impl BandwidthEstimator for Falls {
        fn on_reports(&mut self, _now: Instant, _reports: &[PacketReport]) {
            self.0 = 1_000_000.0;
        }
        fn target_bitrate(&self) -> f64 {
            self.0
        }
    }

    #[test]
    fn a_shielded_burst_does_not_collapse_the_estimate() {
        let target = TargetBitrate::new(8_000_000.0);
        let mut r = Reporting {
            inner: Falls(8_000_000.0),
            restart: Falls,
            target: target.clone(),
            drops: DropLog::default(),
        };
        target.shield(Duration::from_millis(500));
        let now = Instant::now();
        r.on_reports(now, &[]);
        assert_eq!(target.estimate(), 8_000_000.0, "held during the window");
        assert_eq!(target.get(), encoder_target(8_000_000.0));
        assert_eq!(
            r.target_bitrate(),
            8_000_000.0 * PACING_FACTOR,
            "the pacer sees the same"
        );
        // The window ends with the estimate still down: it restarts from before.
        r.handle_timeout(now + Duration::from_secs(1));
        assert_eq!(target.estimate(), 8_000_000.0);
        assert_eq!(
            r.inner.0, 8_000_000.0,
            "estimator restarted at the old value"
        );
        // Afterwards congestion control is in charge again.
        r.on_reports(now + Duration::from_secs(2), &[]);
        assert_eq!(target.estimate(), 1_000_000.0);
    }

    #[test]
    fn without_a_shield_the_estimate_is_published_as_is() {
        let target = TargetBitrate::new(8_000_000.0);
        let mut r = Reporting {
            inner: Falls(8_000_000.0),
            restart: Falls,
            target: target.clone(),
            drops: DropLog::default(),
        };
        r.on_reports(Instant::now(), &[]);
        assert_eq!(target.estimate(), 1_000_000.0);
        assert_eq!(target.get(), encoder_target(1_000_000.0));
    }
}

#[cfg(test)]
mod receiver_tests {
    use super::*;
    use rtc::rtp::codec::h264::H264Payloader;
    use rtc::rtp::packetizer::Payloader;

    /// Packetize `au` as one RTP frame at `timestamp`, starting at sequence `seq`.
    fn packets(au: &[u8], timestamp: u32, seq: &mut u16) -> Vec<rtc::rtp::Packet> {
        packets_in(VideoCodec::H264, au, timestamp, seq)
    }

    /// The same in `codec`, with the payloader the host's track uses for it.
    fn packets_in(
        codec: VideoCodec,
        au: &[u8],
        timestamp: u32,
        seq: &mut u16,
    ) -> Vec<rtc::rtp::Packet> {
        let au = Bytes::copy_from_slice(au);
        let payloads = match codec {
            VideoCodec::H264 => H264Payloader::default().payload(1200, &au),
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

    /// The last frame of a burst (a still screen after a change) comes out on its own
    /// last packet, not only when a next frame arrives; and only once.
    #[test]
    fn a_complete_frame_comes_out_without_waiting_for_the_next() {
        let mut rx = VideoReceiver::default();
        let mut seq = 40;
        let mut key = vec![0, 0, 0, 1, 0x65];
        key.extend((0..5_000u32).map(|i| (i % 251) as u8 | 1));
        let mut out = Vec::new();
        for p in packets(&[0, 0, 0, 1, 0x41, 5, 5, 5], 0, &mut seq) {
            out.extend(rx.push(p));
        }
        for p in packets(&key, 3000, &mut seq) {
            out.extend(rx.push(p));
        }
        assert!(
            out.iter().any(|au| au.len() >= key.len() - 4),
            "the keyframe is out with nothing after it: {} unit(s)",
            out.len()
        );
        let before = out.len();
        for p in packets(&[0, 0, 0, 1, 0x41, 7, 7, 7], 6000, &mut seq) {
            out.extend(rx.push(p));
        }
        assert_eq!(out.len(), before + 1, "the next frame, no second copy");
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

    /// A keyframe-sized and a small access unit of `codec`.
    fn units_of(codec: VideoCodec, k: u8) -> (Vec<u8>, Vec<u8>) {
        let body = |n: usize| (0..n).map(move |i| (i % 251) as u8 | 1);
        match codec {
            VideoCodec::H264 => unreachable!(),
            VideoCodec::H265 => {
                let mut key = vec![0, 0, 0, 1, 0x40, 1, 0x0c, 0, 0, 0, 1, 0x26, 1, 0xaf];
                key.extend(body(30_000));
                let mut small = vec![0, 0, 0, 1, 0x02, 1, 0xd0, k | 1];
                small.extend(body(40));
                (key, small)
            }
            VideoCodec::Av1 => {
                let frame = |n: usize, out: &mut Vec<u8>| {
                    out.push(0x32);
                    rconn_core::codec::put_leb128(n as u64, out);
                    out.extend(body(n));
                };
                let mut key = vec![0x0a, 2, 0, 0];
                frame(30_000, &mut key);
                let mut small = Vec::new();
                frame(40 + usize::from(k), &mut small);
                (key, small)
            }
        }
    }

    /// HEVC and AV1 frames come out whole through the same assembly as H.264, the codec
    /// taken from the payload type; a lost packet costs its frame, and is reported.
    #[test]
    fn hevc_and_av1_frames_are_reassembled_and_losses_reported() {
        for codec in [VideoCodec::H265, VideoCodec::Av1] {
            let mut rx = VideoReceiver::default();
            let mut seq = 1000;
            let (key, _) = units_of(codec, 0);
            let mut out = Vec::new();
            for p in packets_in(codec, &key, 0, &mut seq) {
                out.extend(rx.push(p));
            }
            assert_eq!(rx.codec(), codec);
            assert_eq!(out.len(), 1, "{codec:?}: the keyframe, at once");
            assert_eq!(out[0].as_ref(), key.as_slice(), "{codec:?}: intact");
            // The next keyframe loses a packet for good; small frames follow.
            let mut lossy = packets_in(codec, &key, 3000, &mut seq);
            lossy.remove(5);
            out.clear();
            for p in lossy {
                out.extend(rx.push(p));
            }
            for k in 2..=16u32 {
                let (_, small) = units_of(codec, k as u8);
                for p in packets_in(codec, &small, k * 3000, &mut seq) {
                    out.extend(rx.push(p));
                }
            }
            assert!(
                out.iter().all(|au| au.len() < 1000),
                "{codec:?}: the broken keyframe never comes out"
            );
            assert!(
                out.len() >= 10,
                "{codec:?}: later frames came: {}",
                out.len()
            );
            assert!(rx.take_loss(), "{codec:?}: the loss is reported");
        }
    }
}
