//! Session negotiation for both roles, over a split [`SignalingClient`].
//!
//! * Viewer: [`open_viewer_session`] requests a session, offers with a data channel, and
//!   pumps the answer and trickled candidates in the background.
//! * Host: [`run_host`] answers sessions forever, applying trickled candidates and handing
//!   each data channel the viewer opens to a callback.

use crate::rtc::{CandidateSink, Peer, PeerEvent, PeerOptions, apply_remote};
use crate::signaling::{SignalingClient, SignalingTx};
use anyhow::{Result, bail};
use rconn_core::ids::{HostId, SessionId};
use rconn_core::protocol::{
    ClientMessage, DATA_CHANNEL_PROTOCOL, SdpType, ServerMessage, SessionPolicy, SignalPayload,
};
use std::sync::Arc;
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

/// Called for each data channel a viewer opens, with that session's policy.
pub type ChannelHandler = Arc<dyn Fn(Arc<dyn DataChannel>, SessionPolicy) + Send + Sync>;

/// Serve sessions until the signaling connection drops. One active session at a time:
/// a new session replaces the previous one.
pub async fn run_host(
    sig: SignalingClient,
    opts: PeerOptions,
    on_channel: ChannelHandler,
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
                let answer = peer.answer(sdp).await?;
                tx.send(ClientMessage::Signal {
                    session_id: sid,
                    payload: answer,
                })?;
                for (_, c) in early.drain(..).filter(|(s, _)| *s == sid) {
                    if let Err(e) = peer.apply_remote(c).await {
                        tracing::warn!(error = %e, "could not apply early candidate");
                    }
                }
                let mut events =
                    std::mem::replace(&mut peer.events, tokio::sync::mpsc::unbounded_channel().1);
                let handler = on_channel.clone();
                tokio::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        match ev {
                            PeerEvent::DataChannel(dc) => handler(dc, policy),
                            PeerEvent::State(s) => {
                                tracing::info!(state = %s, "peer connection");
                                if matches!(
                                    s,
                                    RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
                                ) {
                                    break;
                                }
                            }
                        }
                    }
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
