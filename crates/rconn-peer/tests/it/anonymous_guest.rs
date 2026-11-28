//! No accounts anywhere (AnyDesk-style): a computer gets an ID from the server by itself,
//! then someone connects with that ID over a real WebRTC data channel, either with its
//! access password (bound to the connection's DTLS keys) or, with none set, by asking the
//! person at the computer.

use bytes::BytesMut;
use rconn_core::fleet::FleetCa;
use rconn_core::guest::{HostCheck, make_verifier};
use rconn_core::ids::OrgId;
use rconn_core::protocol::{DATA_CHANNEL_LABEL, DataMessage, GuestMessage, GuestRefusal};
use rconn_peer::guest::{Exchange, Step, WAITING_TEXT};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, open_viewer_session, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::public::{EnrollLimits, PublicHosts};
use rconn_signaling::{AppState, router};
use std::sync::{Arc, Mutex};
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

/// How the test computer behaves (what rconn-host-agent's access gate does, in miniature).
#[derive(Clone)]
enum Host {
    /// Has this access password; `tamper` makes it see another viewer certificate than the
    /// real one, as a signaling server in the middle would cause.
    Password { verifier: String, tamper: bool },
    /// No password: the person at the computer gives this answer.
    Ask(Option<GuestRefusal>),
}

/// What the host saw: the label of the viewer's channel, and who asked.
#[derive(Default)]
struct Seen {
    label: Option<String>,
    asked_by: Option<String>,
}

fn host(mode: Arc<Mutex<Host>>, seen: Arc<Mutex<Seen>>) -> SessionHandler {
    Arc::new(move |mut session: HostSession| {
        assert!(
            session.policy.guest,
            "the server must flag a password session"
        );
        let mode = mode.lock().unwrap().clone();
        let seen = seen.clone();
        tokio::spawn(async move {
            let Some(dc) = session.channels.recv().await else {
                return;
            };
            seen.lock().unwrap().label = dc.label().await.ok();
            let send = |m: GuestMessage| {
                let dc = dc.clone();
                async move {
                    let b = DataMessage::Guest(m).encode().unwrap();
                    dc.send(BytesMut::from(&b[..])).await.unwrap();
                }
            };
            let mut check = None;
            match &mode {
                Host::Password { verifier, tamper } => {
                    let mut binding = session.binding.clone();
                    if *tamper {
                        binding.viewer_dtls = "sha-256 00:11:22:33".into();
                    }
                    let (c, ch) = HostCheck::start(verifier, binding).unwrap();
                    check = Some(c);
                    send(GuestMessage::Challenge {
                        salt: ch.salt,
                        msg: ch.msg,
                    })
                    .await;
                }
                Host::Ask(_) => send(GuestMessage::ApprovalNeeded).await,
            }
            while let Some(ev) = dc.poll().await {
                let DataChannelEvent::OnMessage(m) = ev else {
                    continue;
                };
                match (DataMessage::decode(&m.data), &mode) {
                    (Ok(DataMessage::Guest(GuestMessage::Proof { msg, confirm })), _) => {
                        if let Some(c) = check.take() {
                            send(match c.finish(&msg, &confirm) {
                                Ok(confirm) => GuestMessage::Accepted { confirm },
                                Err(_) => GuestMessage::Refused(GuestRefusal::WrongPassword),
                            })
                            .await;
                        }
                    }
                    (Ok(DataMessage::Guest(GuestMessage::AskUser { name })), Host::Ask(answer)) => {
                        seen.lock().unwrap().asked_by = Some(name);
                        send(GuestMessage::AskingUser { timeout_secs: 30 }).await;
                        tokio::time::sleep(Duration::from_millis(100)).await; // deciding
                        send(match answer {
                            None => GuestMessage::Approved,
                            Some(why) => GuestMessage::Refused(*why),
                        })
                        .await;
                    }
                    _ => {}
                }
            }
        });
    })
}

/// Connect with the ID and `password` (empty: ask); `Ok(waited)` once the computer let us
/// in, `waited` saying whether the viewer was told to wait for the person there.
async fn connect(url: &str, code: &str, password: &str) -> Result<bool, String> {
    let opts = PeerOptions {
        udp_addrs: vec![LOOPBACK.into()],
        relay_only: false,
        ..Default::default()
    };
    let (sig, host_id) = SignalingClient::connect_guest(url, &ServerTrust::WebPki, code)
        .await
        .map_err(|e| e.to_string())?;
    let session = open_viewer_session(sig, host_id, opts)
        .await
        .map_err(|e| e.to_string())?;
    let mut exchange = Exchange::new(password.into(), &session).named("Ana on LAPTOP");
    let dc = session.data.clone();
    let mut waited = false;
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
                Step::Waiting(text) => {
                    assert_eq!(text, WAITING_TEXT);
                    waited = true;
                }
                Step::Accepted => {
                    result = Ok(waited);
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
async fn enroll_without_an_account_then_connect_by_password_or_by_asking() {
    tokio::time::timeout(Duration::from_secs(60), async {
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
                os: None,
                version: None,
                capabilities: vec![],
            },
        )
        .await
        .unwrap();
        let opts = PeerOptions {
            udp_addrs: vec![LOOPBACK.into()],
            relay_only: false,
            ..Default::default()
        };
        let verifier = make_verifier("correct horse").unwrap();
        let mode = Arc::new(Mutex::new(Host::Password {
            verifier: verifier.clone(),
            tamper: false,
        }));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let host = tokio::spawn(run_host(sig, opts, host(mode.clone(), seen.clone())));
        let set = |m: Host| *mode.lock().unwrap() = m;

        // The password, bound to this connection's DTLS keys.
        let code = host_id.short_code();
        assert_eq!(connect(&url, &code, "correct horse").await, Ok(false));
        assert_eq!(
            seen.lock().unwrap().label.as_deref(),
            Some(DATA_CHANNEL_LABEL),
            "the viewer says it binds the proof to the connection"
        );
        let wrong = connect(&url, &code, "wrong guess").await;
        assert_eq!(wrong, Err("wrong access password".into()));

        // A host that sees another viewer certificate (a server in the middle swapped
        // it): the right password no longer opens anything.
        set(Host::Password {
            verifier,
            tamper: true,
        });
        let mitm = connect(&url, &code, "correct horse").await;
        assert_eq!(mitm, Err("wrong access password".into()));

        // No password set: the person at the computer accepts or declines.
        set(Host::Ask(None));
        assert_eq!(connect(&url, &code, "").await, Ok(true), "waited, then in");
        assert_eq!(
            seen.lock().unwrap().asked_by.as_deref(),
            Some("Ana on LAPTOP")
        );
        set(Host::Ask(Some(GuestRefusal::Declined)));
        assert_eq!(
            connect(&url, &code, "").await,
            Err("the person at the computer declined".into())
        );
        set(Host::Ask(Some(GuestRefusal::NoAnswer)));
        assert_eq!(
            connect(&url, &code, "").await,
            Err("nobody at the computer answered in time".into())
        );
        // A typed password never quietly becomes "just ask".
        set(Host::Ask(None));
        let typed = connect(&url, &code, "correct horse").await;
        assert!(
            typed
                .as_ref()
                .is_err_and(|e| e.contains("no access password")),
            "{typed:?}"
        );
        host.abort();
    })
    .await
    .expect("test timed out");
}
