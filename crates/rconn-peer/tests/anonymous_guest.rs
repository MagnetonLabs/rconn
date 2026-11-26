//! No accounts anywhere (AnyDesk-style): a computer gets an ID from the server by itself,
//! then someone connects with that ID and its access password over a real WebRTC data
//! channel. The right password opens the session; a wrong one is refused.

use bytes::BytesMut;
use rconn_core::fleet::FleetCa;
use rconn_core::guest::{HostCheck, make_verifier};
use rconn_core::ids::OrgId;
use rconn_core::protocol::{DataMessage, GuestMessage, GuestRefusal};
use rconn_peer::guest::{Exchange, Step};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, open_viewer_session, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::public::{EnrollLimits, PublicHosts};
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use webrtc::data_channel::DataChannelEvent;

const LOOPBACK: &str = "127.0.0.1:0";

async fn server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    let public = PublicHosts::new(
        FleetCa::generate(OrgId::new()).unwrap(),
        Some(EnrollLimits::default()),
        None,
    )
    .unwrap();
    let auth = FleetAuthenticator::new(url.clone()).with_org_ca(public.org_id(), public.ca_pem());
    let state = AppState::new(Arc::new(auth));
    state.allow_public_hosts(public);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    url
}

/// The host side of the password check (what rconn-host-agent's access gate does).
fn password_host(verifier: String) -> SessionHandler {
    Arc::new(move |mut session: HostSession| {
        assert!(
            session.policy.guest,
            "the server must flag a password session"
        );
        let verifier = verifier.clone();
        tokio::spawn(async move {
            let Some(dc) = session.channels.recv().await else {
                return;
            };
            let send = |m: GuestMessage| {
                let dc = dc.clone();
                async move {
                    let b = DataMessage::Guest(m).encode().unwrap();
                    dc.send(BytesMut::from(&b[..])).await.unwrap();
                }
            };
            let (check, ch) = HostCheck::start(&verifier, session.session_id).unwrap();
            send(GuestMessage::Challenge {
                salt: ch.salt,
                msg: ch.msg,
            })
            .await;
            let mut check = Some(check);
            while let Some(ev) = dc.poll().await {
                if let DataChannelEvent::OnMessage(m) = ev
                    && let Ok(DataMessage::Guest(GuestMessage::Proof { msg, confirm })) =
                        DataMessage::decode(&m.data)
                    && let Some(c) = check.take()
                {
                    send(match c.finish(&msg, &confirm) {
                        Ok(confirm) => GuestMessage::Accepted { confirm },
                        Err(_) => GuestMessage::Refused(GuestRefusal::WrongPassword),
                    })
                    .await;
                }
            }
        });
    })
}

/// Connect with the ID and `password`; `Ok` once the computer accepted it.
async fn connect(url: &str, code: &str, password: &str) -> Result<(), String> {
    let opts = PeerOptions {
        udp_addrs: vec![LOOPBACK.into()],
        relay_only: false,
    };
    let (sig, host_id) = SignalingClient::connect_guest(url, &ServerTrust::WebPki, code)
        .await
        .map_err(|e| e.to_string())?;
    let session = open_viewer_session(sig, host_id, opts)
        .await
        .map_err(|e| e.to_string())?;
    let mut exchange = Exchange::new(password.into(), session.session_id);
    let dc = session.data.clone();
    let mut result = Err("channel closed".to_string());
    while let Some(ev) = dc.poll().await {
        if let DataChannelEvent::OnMessage(m) = ev
            && let Ok(DataMessage::Guest(g)) = DataMessage::decode(&m.data)
        {
            match exchange.handle(g).await {
                Step::Send(reply) => {
                    let b = reply.encode().unwrap();
                    dc.send(BytesMut::from(&b[..])).await.unwrap();
                }
                Step::Accepted => {
                    result = Ok(());
                    break;
                }
                Step::Refused(why) => {
                    result = Err(why);
                    break;
                }
            }
        }
    }
    session.close().await;
    result
}

#[tokio::test]
async fn enroll_without_an_account_then_connect_with_id_and_password() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let url = server().await;
        let dir = tempfile::tempdir().unwrap();
        let host_id =
            rconn_peer::identity::enroll_anonymous_into(dir.path(), &url, &ServerTrust::WebPki)
                .await
                .unwrap();
        assert_eq!(
            rconn_peer::identity::local_owner(dir.path()),
            rconn_peer::identity::LocalOwner::Recorded(None),
            "the computer records that nobody owns it"
        );
        let read = |f: &str| std::fs::read_to_string(dir.path().join(f)).unwrap();
        let key = rconn_core::fleet::HostKey::from_pem(&read("host.key.pem")).unwrap();
        let cert = read("host.cert.pem");
        let sig = SignalingClient::connect_host(
            &url,
            &ServerTrust::WebPki,
            HostCredentials {
                server_id: &url,
                host_id,
                key: &key,
                cert_pem: &cert,
                label: Some("LAPTOP".into()),
            },
        )
        .await
        .unwrap();
        let opts = PeerOptions {
            udp_addrs: vec![LOOPBACK.into()],
            relay_only: false,
        };
        let verifier = make_verifier("correct horse").unwrap();
        let host = tokio::spawn(run_host(sig, opts, password_host(verifier)));

        let code = host_id.short_code();
        assert_eq!(connect(&url, &code, "correct horse").await, Ok(()));
        let wrong = connect(&url, &code, "wrong guess").await;
        assert_eq!(wrong, Err("wrong access password".into()));
        host.abort();
    })
    .await
    .expect("test timed out");
}
