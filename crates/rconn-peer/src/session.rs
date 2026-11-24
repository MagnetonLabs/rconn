//! Session negotiation for both roles, over a split [`SignalingClient`].
//!
//! * Viewer: [`open_viewer_session`] requests a session, offers a data channel plus a
//!   recv-only video transceiver, and pumps the answer and trickled candidates.
//! * Host: [`run_host`] answers sessions forever, attaching the screen track, applying
//!   trickled candidates, and handing each session to a callback as a [`HostSession`].

use crate::rtc::{CandidateSink, Peer, PeerEvent, PeerOptions, apply_remote};
use crate::signaling::{SignalingClient, SignalingTx};
use crate::video::{TargetBitrate, VideoSender};
use anyhow::{Result, bail};
use rconn_core::ids::{HostId, SessionId};
use rconn_core::protocol::{
    ClientMessage, DATA_CHANNEL_PROTOCOL, SdpType, ServerMessage, SessionPolicy, SignalPayload,
};
use rtc::rtp_transceiver::RTCRtpTransceiverDirection;
use rtc::rtp_transceiver::RTCRtpTransceiverInit;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc::data_channel::{DataChannel, RTCDataChannelInit};
use webrtc::peer_connection::RTCPeerConnectionState;

pub struct ViewerSession {
    pub session_id: SessionId,
    pub peer: Peer,
    /// The `rconn-data/1` channel, created before the offer.
    pub data: Arc<dyn DataChannel>,
    pub tx: SignalingTx,
    pump: JoinHandle<()>,
}

impl ViewerSession {
    pub async fn close(self) {
        let _ = self.tx.send(ClientMessage::EndSession {
            session_id: self.session_id,
        });
        let _ = self.peer.pc.close().await;
        self.pump.abort();
    }
}

pub async fn open_viewer_session(
    sig: SignalingClient,
    host: HostId,
    opts: PeerOptions,
) -> Result<ViewerSession> {
    let ice = sig.ice_servers.clone();
    let (tx, mut rx) = sig.split();
    tx.send(ClientMessage::RequestSession { host_id: host })?;
    let session_id = loop {
        match rx.recv().await {
            Some(ServerMessage::SessionReady { session_id }) => break session_id,
            Some(ServerMessage::Error { code, message }) => {
                bail!("session refused: {code:?}: {message}")
            }
            Some(_) => continue,
            None => bail!("signaling closed before the session opened"),
        }
    };

    let peer = Peer::new(
        &ice,
        CandidateSink {
            tx: tx.clone(),
            session_id,
        },
        opts,
    )
    .await?;
    let init = RTCDataChannelInit {
        ordered: true,
        protocol: DATA_CHANNEL_PROTOCOL.into(),
        ..Default::default()
    };
    let data = peer.pc.create_data_channel("rconn", Some(init)).await?;
    peer.pc
        .add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await?;
    let offer = peer.offer().await?;
    tx.send(ClientMessage::Signal {
        session_id,
        payload: offer,
    })?;

    let pc = peer.pc.clone();
    let pump = tokio::spawn(async move {
        // Candidates can overtake the answer (the host's gathering starts inside
        // set_local_description, before it sends the SDP). Hold them until it lands.
        let mut early: Option<Vec<SignalPayload>> = Some(Vec::new());
        while let Some(msg) = rx.recv().await {
            match msg {
                ServerMessage::Signal {
                    session_id: sid,
                    payload,
                } if sid == session_id => {
                    let is_answer = matches!(payload, SignalPayload::Sdp { .. });
                    if let (Some(buf), false) = (early.as_mut(), is_answer) {
                        buf.push(payload);
                        continue;
                    }
                    if let Err(e) = apply_remote(&*pc, payload).await {
                        tracing::warn!(error = %e, "could not apply remote signal");
                    }
                    if is_answer {
                        for c in early.take().unwrap_or_default() {
                            if let Err(e) = apply_remote(&*pc, c).await {
                                tracing::warn!(error = %e, "could not apply early candidate");
                            }
                        }
                    }
                }
                ServerMessage::SessionEnded {
                    session_id: sid,
                    reason,
                } if sid == session_id => {
                    tracing::info!(?reason, "session ended by server");
                    let _ = pc.close().await;
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(ViewerSession {
        session_id,
        peer,
        data,
        tx,
        pump,
    })
}

/// One accepted session, as the host agent sees it.
pub struct HostSession {
    pub session_id: SessionId,
    pub policy: SessionPolicy,
    /// The screen track. Write encoded access units here.
    pub video: VideoSender,
    /// Congestion-control estimate to steer the encoder bitrate.
    pub target_bitrate: TargetBitrate,
    /// Data channels the viewer opens. Ends when the session closes.
    pub channels: mpsc::UnboundedReceiver<Arc<dyn DataChannel>>,
    /// Peer-connection states, ending with `Closed` or `Failed`.
    pub states: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
}

/// Called once per accepted session. Spawn the session's work; return quickly.
pub type SessionHandler = Arc<dyn Fn(HostSession) + Send + Sync>;

/// Serve sessions until the signaling connection drops. One active session at a time:
/// a new session replaces the previous one.
pub async fn run_host(
    sig: SignalingClient,
    opts: PeerOptions,
    on_session: SessionHandler,
) -> Result<()> {
    let ice = sig.ice_servers.clone();
    let (tx, mut rx) = sig.split();
    let mut pending: Option<(SessionId, SessionPolicy)> = None;
    // Candidates that arrived before the offer they belong to (see open_viewer_session).
    let mut early: Vec<(SessionId, SignalPayload)> = Vec::new();
    let mut current: Option<(SessionId, Peer)> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            ServerMessage::IncomingSession {
                session_id,
                operator_id,
                policy,
            } => {
                tracing::info!(%session_id, %operator_id, ?policy, "incoming session");
                if let Some((_, old)) = current.take() {
                    let _ = old.pc.close().await;
                }
                pending = Some((session_id, policy));
                early.clear();
            }
            ServerMessage::Signal {
                session_id,
                payload:
                    SignalPayload::Sdp {
                        sdp_type: SdpType::Offer,
                        sdp,
                    },
            } => {
                let Some((sid, policy)) = pending.take().filter(|(sid, _)| *sid == session_id)
                else {
                    tracing::warn!(%session_id, "offer for a session we were not told about");
                    continue;
                };
                let mut peer = Peer::new(
                    &ice,
                    CandidateSink {
                        tx: tx.clone(),
                        session_id: sid,
                    },
                    opts.clone(),
                )
                .await?;
                let (answer, video) = peer.answer_with_video(sdp).await?;
                tx.send(ClientMessage::Signal {
                    session_id: sid,
                    payload: answer,
                })?;
                for (_, c) in early.drain(..).filter(|(s, _)| *s == sid) {
                    if let Err(e) = peer.apply_remote(c).await {
                        tracing::warn!(error = %e, "could not apply early candidate");
                    }
                }

                let (ch_tx, channels) = mpsc::unbounded_channel();
                let (st_tx, states) = mpsc::unbounded_channel();
                let mut events = std::mem::replace(&mut peer.events, mpsc::unbounded_channel().1);
                tokio::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        match ev {
                            PeerEvent::DataChannel(dc) => {
                                let _ = ch_tx.send(dc);
                            }
                            PeerEvent::State(s) => {
                                tracing::info!(state = %s, "peer connection");
                                let _ = st_tx.send(s);
                                if matches!(
                                    s,
                                    RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
                                ) {
                                    break; // drops ch_tx/st_tx: the session's receivers end
                                }
                            }
                            PeerEvent::Track(_) => {} // hosts don't receive media in v1
                        }
                    }
                });
                on_session(HostSession {
                    session_id: sid,
                    policy,
                    video,
                    target_bitrate: peer.target_bitrate.clone(),
                    channels,
                    states,
                });
                current = Some((sid, peer));
            }
            ServerMessage::Signal {
                session_id,
                payload,
            } => {
                if let Some((_, peer)) = current.as_ref().filter(|(sid, _)| *sid == session_id) {
                    if let Err(e) = peer.apply_remote(payload).await {
                        tracing::warn!(error = %e, "could not apply remote signal");
                    }
                } else if pending.as_ref().is_some_and(|(sid, _)| *sid == session_id) {
                    early.push((session_id, payload));
                }
            }
            ServerMessage::SessionEnded { session_id, reason } => {
                tracing::info!(%session_id, ?reason, "session ended");
                if let Some((_, peer)) = current.take_if(|(sid, _)| *sid == session_id) {
                    let _ = peer.pc.close().await;
                }
            }
            other => tracing::debug!(?other, "ignored signaling message"),
        }
    }
    bail!("signaling connection closed")
}
