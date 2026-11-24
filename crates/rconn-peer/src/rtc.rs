//! A WebRTC peer connection with trickle ICE over our signaling, and its events on a
//! channel.

use crate::signaling::SignalingTx;
use anyhow::{Context, Result, anyhow, bail};
use rconn_core::ids::SessionId;
use rconn_core::protocol::{ClientMessage, IceServer, SdpType, SignalPayload};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCIceTransportPolicy, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState, RTCSessionDescription,
};

pub enum PeerEvent {
    State(RTCPeerConnectionState),
    /// The remote side opened a data channel.
    DataChannel(Arc<dyn DataChannel>),
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
}

pub struct Peer {
    pub pc: Arc<dyn PeerConnection>,
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
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
        let pc = PeerConnectionBuilder::new()
            .with_configuration(cfg.build())
            .with_handler(Arc::new(Handler { events: tx, sink }))
            .with_runtime(runtime)
            .with_udp_addrs(udp)
            .build()
            .await
            .context("building peer connection")?;
        Ok(Self {
            pc: Arc::new(pc),
            events,
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
