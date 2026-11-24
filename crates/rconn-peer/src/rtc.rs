//! A WebRTC peer connection with its events delivered on a channel.

use anyhow::{Context, Result, anyhow};
use rconn_core::protocol::{IceServer, SdpType, SignalPayload};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription,
};

pub enum PeerEvent {
    GatheringComplete,
    State(RTCPeerConnectionState),
    /// The remote side opened a data channel.
    DataChannel(Arc<dyn DataChannel>),
}

struct Handler(mpsc::UnboundedSender<PeerEvent>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.0.send(PeerEvent::GatheringComplete);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.0.send(PeerEvent::State(state));
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.0.send(PeerEvent::DataChannel(dc));
    }
}

pub struct Peer {
    pub pc: Arc<dyn PeerConnection>,
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
}

/// How long to wait for ICE gathering before sending whatever candidates exist.
pub const GATHER_TIMEOUT: Duration = Duration::from_secs(5);

impl Peer {
    /// `udp_addrs` are the local sockets to gather on, e.g. `["0.0.0.0:0"]`.
    pub async fn new(ice_servers: &[IceServer], udp_addrs: Vec<String>) -> Result<Self> {
        let servers = ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();
        let (tx, events) = mpsc::unbounded_channel();
        let runtime = webrtc::runtime::default_runtime()
            .ok_or_else(|| anyhow!("no async runtime for webrtc"))?;
        let pc = PeerConnectionBuilder::new()
            .with_configuration(
                RTCConfigurationBuilder::new()
                    .with_ice_servers(servers)
                    .build(),
            )
            .with_handler(Arc::new(Handler(tx)))
            .with_runtime(runtime)
            .with_udp_addrs(udp_addrs)
            .build()
            .await
            .context("building peer connection")?;
        Ok(Self {
            pc: Arc::new(pc),
            events,
        })
    }

    /// Create an offer and return it once ICE gathering finishes (non-trickle).
    pub async fn offer(&mut self) -> Result<SignalPayload> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer).await?;
        self.local_sdp(SdpType::Offer).await
    }

    /// Apply a remote offer and return our answer once gathering finishes.
    pub async fn answer(&mut self, offer_sdp: String) -> Result<SignalPayload> {
        self.pc
            .set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
            .await?;
        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;
        self.local_sdp(SdpType::Answer).await
    }

    pub async fn accept_answer(&self, answer_sdp: String) -> Result<()> {
        self.pc
            .set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
            .await?;
        Ok(())
    }

    async fn local_sdp(&mut self, sdp_type: SdpType) -> Result<SignalPayload> {
        let _ = tokio::time::timeout(GATHER_TIMEOUT, async {
            while let Some(ev) = self.events.recv().await {
                if matches!(ev, PeerEvent::GatheringComplete) {
                    break;
                }
            }
        })
        .await;
        let desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description"))?;
        Ok(SignalPayload::Sdp {
            sdp_type,
            sdp: desc.sdp,
        })
    }
}
