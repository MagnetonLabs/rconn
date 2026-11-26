//! A WebRTC peer connection with trickle ICE over our signaling, and its events on a
//! channel.

use crate::signaling::SignalingTx;
use crate::video::{AudioSender, TargetBitrate, VideoCodec, VideoSender, media_stack};
use anyhow::{Context, Result, anyhow, bail};
use rconn_core::ids::SessionId;
use rconn_core::protocol::{ClientMessage, IceServer, SdpType, SignalPayload};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCIceTransportPolicy, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState, RTCSessionDescription,
};

pub enum PeerEvent {
    State(RTCPeerConnectionState),
    /// The remote side opened a data channel.
    DataChannel(Arc<dyn DataChannel>),
    /// The remote side is sending media on this track.
    Track(Arc<dyn TrackRemote>),
}

/// Where local ICE candidates go as they are found (trickle ICE).
#[derive(Clone)]
pub struct CandidateSink {
    pub tx: SignalingTx,
    pub session_id: SessionId,
}

#[derive(Clone, Default)]
pub struct PeerOptions {
    /// Local UDP sockets to gather on. Empty means `0.0.0.0:0`.
    pub udp_addrs: Vec<String>,
    /// Force every path through TURN (to test the fallback, or for hosts whose policy
    /// forbids revealing local addresses).
    pub relay_only: bool,
    /// Video codecs besides H.264: a viewer, those it decodes (offered); a host, those it
    /// encodes (answered when offered). Empty: H.264 only, as peers before 0.1.7.
    pub video_codecs: Vec<VideoCodec>,
}

struct Handler {
    events: mpsc::UnboundedSender<PeerEvent>,
    sink: CandidateSink,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let payload = SignalPayload::Ice {
                    candidate: init.candidate,
                    sdp_mid: init.sdp_mid,
                    sdp_mline_index: init.sdp_mline_index,
                };
                let _ = self.sink.tx.send(ClientMessage::Signal {
                    session_id: self.sink.session_id,
                    payload,
                });
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize local candidate"),
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.events.send(PeerEvent::State(state));
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.events.send(PeerEvent::DataChannel(dc));
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.events.send(PeerEvent::Track(track));
    }
}

pub struct Peer {
    pub pc: Arc<dyn PeerConnection>,
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
    /// Send-side congestion-control estimate (meaningful on the host, which sends video).
    pub target_bitrate: TargetBitrate,
    /// Video codecs besides H.264 this side has (see [`PeerOptions::video_codecs`]).
    video_codecs: Vec<VideoCodec>,
}

impl Peer {
    pub async fn new(
        ice_servers: &[IceServer],
        sink: CandidateSink,
        opts: PeerOptions,
    ) -> Result<Self> {
        let servers = ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();
        let mut cfg = RTCConfigurationBuilder::new().with_ice_servers(servers);
        if opts.relay_only {
            cfg = cfg.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
        }
        let udp = if opts.udp_addrs.is_empty() {
            vec!["0.0.0.0:0".to_string()]
        } else {
            opts.udp_addrs
        };
        let (tx, events) = mpsc::unbounded_channel();
        let runtime = webrtc::runtime::default_runtime()
            .ok_or_else(|| anyhow!("no async runtime for webrtc"))?;
        let video_codecs = opts.video_codecs;
        let (engine, registry, target_bitrate) = media_stack(&video_codecs)?;
        let pc = PeerConnectionBuilder::new()
            .with_configuration(cfg.build())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { events: tx, sink }))
            .with_runtime(runtime)
            .with_udp_addrs(udp)
            .build()
            .await
            .context("building peer connection")?;
        Ok(Self {
            pc: Arc::new(pc),
            events,
            target_bitrate,
            video_codecs,
        })
    }

    /// Create and apply a local offer. Candidates follow through the sink.
    pub async fn offer(&self) -> Result<SignalPayload> {
        let offer = self.pc.create_offer(None).await?;
        let sdp = offer.sdp.clone();
        self.pc.set_local_description(offer).await?;
        Ok(SignalPayload::Sdp {
            sdp_type: SdpType::Offer,
            sdp,
        })
    }

    /// Apply a remote offer and return our answer. Candidates follow through the sink.
    pub async fn answer(&self, offer_sdp: String) -> Result<SignalPayload> {
        self.pc
            .set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
            .await?;
        let answer = self.pc.create_answer(None).await?;
        let sdp = answer.sdp.clone();
        self.pc.set_local_description(answer).await?;
        Ok(SignalPayload::Sdp {
            sdp_type: SdpType::Answer,
            sdp,
        })
    }

    /// Host side: apply the viewer's offer, attach the screen track to its recv-only video
    /// transceiver, and return our answer with the sender.
    pub async fn answer_with_video(
        &self,
        offer_sdp: String,
    ) -> Result<(SignalPayload, VideoSender, Option<AudioSender>)> {
        // Audio only if the viewer offered to receive it (older viewers may not).
        let wants_audio = offer_sdp.contains("m=audio");
        // The best codec the viewer decodes and this host encodes; H.264 for viewers
        // before 0.1.7, which offer nothing else.
        let offered = crate::video::sdp_video_codecs(&offer_sdp);
        let mut encodable = self.video_codecs.clone();
        encodable.push(VideoCodec::H264);
        let codec = rconn_core::codec::choose(&offered, &encodable, VideoCodec::from_env());
        tracing::info!(codec = codec.name(), ?offered, "video codec");
        self.pc
            .set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
            .await?;
        let video = VideoSender::attach(&*self.pc, codec).await?;
        let audio = if wants_audio {
            Some(AudioSender::attach(&*self.pc).await?)
        } else {
            None
        };
        let answer = self.pc.create_answer(None).await?;
        let sdp = answer.sdp.clone();
        self.pc.set_local_description(answer).await?;
        Ok((
            SignalPayload::Sdp {
                sdp_type: SdpType::Answer,
                sdp,
            },
            video,
            audio,
        ))
    }

    /// Apply a remote answer or candidate. Offers are handled by [`answer`](Self::answer).
    pub async fn apply_remote(&self, payload: SignalPayload) -> Result<()> {
        apply_remote(&*self.pc, payload).await
    }
}

/// Apply a remote answer or trickled candidate to `pc`.
pub async fn apply_remote(pc: &dyn PeerConnection, payload: SignalPayload) -> Result<()> {
    match payload {
        SignalPayload::Sdp {
            sdp_type: SdpType::Answer,
            sdp,
        } => {
            pc.set_remote_description(RTCSessionDescription::answer(sdp)?)
                .await?;
        }
        SignalPayload::Sdp {
            sdp_type: SdpType::Offer,
            ..
        } => bail!("unexpected renegotiation offer"),
        SignalPayload::Ice {
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            if candidate.is_empty() {
                return Ok(()); // end-of-candidates marker
            }
            pc.add_ice_candidate(RTCIceCandidateInit {
                candidate,
                sdp_mid,
                sdp_mline_index,
                username_fragment: None,
                url: None,
            })
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use crate::signaling::SignalingTx;
    use rconn_core::ids::SessionId;
    use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
    use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};

    async fn peer(video_codecs: &[VideoCodec]) -> Peer {
        let (tx, _rx) = SignalingTx::channel();
        let sink = CandidateSink {
            tx,
            session_id: SessionId::new(),
        };
        let opts = PeerOptions {
            udp_addrs: vec!["127.0.0.1:0".into()],
            relay_only: false,
            video_codecs: video_codecs.to_vec(),
        };
        Peer::new(&[], sink, opts).await.expect("peer")
    }

    /// Offer from a viewer decoding `viewer`, answer from a host encoding `host`: the
    /// codec the host sends, and the codecs of its answer.
    async fn negotiate(
        viewer: &[VideoCodec],
        host: &[VideoCodec],
    ) -> (VideoCodec, Vec<VideoCodec>) {
        let v = peer(viewer).await;
        v.pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let SignalPayload::Sdp { sdp: offer, .. } = v.offer().await.unwrap() else {
            unreachable!()
        };
        assert_eq!(
            crate::video::sdp_video_codecs(&offer).first(),
            rconn_core::codec::offer(viewer, None).first(),
            "best first"
        );
        let h = peer(host).await;
        let (answer, video, _) = h.answer_with_video(offer).await.unwrap();
        let SignalPayload::Sdp { sdp, .. } = &answer else {
            unreachable!()
        };
        let answered = crate::video::sdp_video_codecs(sdp);
        v.apply_remote(answer)
            .await
            .expect("the viewer takes the answer");
        let _ = (v.pc.close().await, h.pc.close().await);
        (video.codec(), answered)
    }

    #[tokio::test]
    async fn codecs_are_negotiated_and_old_peers_get_h264() {
        if VideoCodec::from_env().is_some() {
            return; // RCONN_CODEC changes the pick
        }
        use VideoCodec::*;
        // A viewer before 0.1.7 offers H.264 only.
        assert_eq!(negotiate(&[], &[H265, Av1]).await.0, H264);
        // A host before 0.1.7 registers H.264 only: its answer has nothing else.
        assert_eq!(negotiate(&[H265, Av1], &[]).await, (H264, vec![H264]));
        // Both sides: the best shared codec.
        assert_eq!(negotiate(&[H265], &[H265]).await.0, H265);
        assert_eq!(negotiate(&[H265, Av1], &[H265]).await.0, H265);
        assert_eq!(negotiate(&[H265, Av1], &[H265, Av1]).await.0, Av1);
        assert_eq!(negotiate(&[Av1], &[H265]).await.0, H264);
    }
}
