//! File transfer end to end: a viewer and a host negotiate a real WebRTC session through a
//! live signaling server, then the viewer opens the `rconn-files/1` channel and moves
//! files both ways. The host here is the agent's file handling in miniature: it serves the
//! channel with `rconn_peer::files::serve` when its setting and the server's policy allow
//! it, and refuses it otherwise.

use bytes::BytesMut;
use rconn_core::files::FILES_CHANNEL_PROTOCOL;
use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::SessionPolicy;
use rconn_core::protocol::{Control, DataMessage};
use rconn_peer::files::{FilesClient, Progress, ServeConfig, Stopped};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{
    HostSession, SessionHandler, ViewerSession, open_viewer_session, run_host,
};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;

const SERVER_ID: &str = "wss://files.test";
const MB: u64 = 1024 * 1024;

struct Rig {
    session: ViewerSession,
    files: FilesClient,
    /// The host's shared folder (its "home").
    host_root: tempfile::TempDir,
    /// The viewer's side: files to send, and where downloads go.
    viewer: tempfile::TempDir,
    host_task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Rig {
    async fn close(self) {
        drop(self.files);
        self.session.close().await;
        self.host_task.abort();
    }
    fn inbox(&self) -> PathBuf {
        self.host_root.path().join("Downloads").join("Rconn")
    }
}

/// A session whose host allows file transfer if `setting` (its own) and the server's
/// `policy` both say so, with the file channel opened by the viewer.
async fn rig(setting: bool, policy: bool) -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: ca.org_id(),
    };
    let auth = FleetAuthenticator::new(SERVER_ID)
        .with_org_ca(ca.org_id(), ca.cert_pem())
        .with_operator_token("tok", op)
        .with_session_policy(SessionPolicy {
            show_indicator: false,
            clipboard: true,
            guest: false,
            file_transfer: policy,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router(AppState::new(Arc::new(auth))))
            .await
            .unwrap()
    });

    let host_id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(host_id, &key).unwrap();
    let opts = PeerOptions {
        udp_addrs: vec!["127.0.0.1:0".into()],
        relay_only: false,
        ..Default::default()
    };
    let host_sig = SignalingClient::connect_host(
        &url,
        &ServerTrust::WebPki,
        HostCredentials {
            server_id: SERVER_ID,
            host_id,
            key: &key,
            cert_pem: &cert,
            label: None,
            os: None,
            version: None,
            capabilities: vec![],
        },
    )
    .await
    .unwrap();
    let host_root = tempfile::tempdir().unwrap();
    let root = host_root.path().to_owned();
    let handler: SessionHandler = Arc::new(move |mut s: HostSession| {
        let root = root.clone();
        tokio::spawn(async move {
            let allowed = setting && s.policy.file_transfer;
            while let Some(dc) = s.channels.recv().await {
                let root = root.clone();
                let session = s.session_id.to_string();
                tokio::spawn(async move {
                    if dc.protocol().await.unwrap_or_default() != FILES_CHANNEL_PROTOCOL {
                        // The main channel: answers pings, to measure it during transfers.
                        while let Some(ev) = dc.poll().await {
                            if let DataChannelEvent::OnMessage(m) = ev
                                && let Ok(DataMessage::Control(Control::Ping(n))) =
                                    DataMessage::decode(&m.data)
                            {
                                let pong = DataMessage::Control(Control::Pong(n)).encode().unwrap();
                                let _ = dc.send(BytesMut::from(&pong[..])).await;
                            }
                        }
                        return;
                    }
                    if !allowed {
                        return rconn_peer::files::refuse(
                            dc,
                            "file transfer is turned off on this computer",
                        )
                        .await;
                    }
                    let cfg = ServeConfig {
                        inbox: root.join("Downloads").join("Rconn"),
                        root,
                        free_space: None,
                        session,
                    };
                    rconn_peer::files::serve(dc, cfg).await;
                });
            }
        });
    });
    let host_task = tokio::spawn(run_host(host_sig, opts.clone(), handler));

    let viewer_sig = SignalingClient::connect_operator(&url, &ServerTrust::WebPki, "tok")
        .await
        .unwrap();
    let session = open_viewer_session(viewer_sig, host_id, opts)
        .await
        .unwrap();
    // The file channel joins a connected session (the viewer opens it on demand).
    let dc = session.data.clone();
    while let Some(ev) = dc.poll().await {
        if matches!(ev, DataChannelEvent::OnOpen) {
            break;
        }
    }
    let files = FilesClient::open(&*session.peer.pc).await.unwrap();
    Rig {
        session,
        files,
        host_root,
        viewer: tempfile::tempdir().unwrap(),
        host_task,
    }
}

/// `len` bytes of noise (doesn't compress, so nothing in the path can cheat).
fn noise_file(path: &Path, len: u64) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ len;
    let mut block = vec![0u8; MB as usize];
    let mut left = len;
    while left > 0 {
        for chunk in block.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *chunk = x.to_le_bytes();
        }
        let n = left.min(MB) as usize;
        f.write_all(&block[..n]).unwrap();
        left -= n as u64;
    }
    f.flush().unwrap();
}

fn sha256_of(path: &Path) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0u8; MB as usize];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize().into()
}

fn stopped(e: &anyhow::Error) -> Option<Stopped> {
    e.downcast_ref::<Stopped>().copied()
}

/// Cancel `progress` once `after` bytes have moved.
fn cancel_after(progress: Arc<Progress>, after: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while progress.done() < after {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        progress.cancel();
    })
}

/// Ping on the main channel every 20 ms until `progress` has moved its whole file;
/// returns the round trips.
fn ping_while(
    dc: Arc<dyn webrtc::data_channel::DataChannel>,
    progress: Arc<Progress>,
) -> tokio::task::JoinHandle<Vec<Duration>> {
    tokio::spawn(async move {
        let mut rtts = Vec::new();
        for n in 0u64.. {
            if progress.total() > 0 && progress.done() >= progress.total() {
                break;
            }
            let ping = DataMessage::Control(Control::Ping(n)).encode().unwrap();
            let sent = Instant::now();
            dc.send(BytesMut::from(&ping[..])).await.unwrap();
            loop {
                match dc.poll().await {
                    Some(DataChannelEvent::OnMessage(m))
                        if DataMessage::decode(&m.data).ok()
                            == Some(DataMessage::Control(Control::Pong(n))) =>
                    {
                        break;
                    }
                    Some(_) => {}
                    None => return rtts,
                }
            }
            rtts.push(sent.elapsed());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        rtts
    })
}

/// Wait (a few seconds at most) for `cond`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_100_mb_file_round_trips_both_ways_with_matching_sha256() {
    tokio::time::timeout(Duration::from_secs(300), async {
        let rig = rig(true, true).await;
        let src = rig.viewer.path().join("big.bin");
        noise_file(&src, 100 * MB);
        let want = sha256_of(&src);

        // Viewer -> host, while the main channel keeps pinging: input must not wait
        // behind file data.
        let progress = Progress::new();
        let pinger = ping_while(rig.session.data.clone(), progress.clone());
        let t = Instant::now();
        let saved = rig.files.upload(&src, &progress).await.unwrap();
        let up = t.elapsed();
        let rtts = pinger.await.unwrap();
        assert_eq!(saved, "Downloads/Rconn/big.bin");
        assert_eq!(progress.done(), 100 * MB);
        assert_eq!(progress.total(), 100 * MB);
        assert_eq!(sha256_of(&rig.inbox().join("big.bin")), want);

        // The host lists it.
        let listed = rig.files.list("Downloads/Rconn").await.unwrap();
        assert!(
            listed
                .iter()
                .any(|e| e.name == "big.bin" && !e.is_dir && e.size == 100 * MB),
            "{listed:?}"
        );
        let root = rig.files.list("").await.unwrap();
        assert!(root.iter().any(|e| e.name == "Downloads" && e.is_dir), "{root:?}");

        // Host -> viewer.
        let dest = rig.viewer.path().join("dl");
        let progress = Progress::new();
        let t = Instant::now();
        let got = rig
            .files
            .download("Downloads/Rconn/big.bin", &dest, &progress)
            .await
            .unwrap();
        let down = t.elapsed();
        assert_eq!(got, dest.join("big.bin"));
        assert_eq!(sha256_of(&got), want);
        assert_eq!(progress.done(), 100 * MB);
        eprintln!(
            "100 MB up in {up:?} ({:.0} MB/s), down in {down:?} ({:.0} MB/s)",
            100.0 / up.as_secs_f64(),
            100.0 / down.as_secs_f64()
        );
        let mut sorted = rtts.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        let worst = *sorted.last().unwrap();
        eprintln!(
            "main-channel ping during upload: {} pings, median {median:?}, worst {worst:?}; in order (ms): {:?}",
            sorted.len(),
            rtts.iter().map(|d| d.as_millis()).collect::<Vec<_>>()
        );
        assert!(sorted.len() >= 5, "pings answered during the upload");
        assert!(
            worst < Duration::from_millis(250),
            "input waited behind file data: worst ping {worst:?}"
        );

        // Same names never overwrite: on either side, the second copy is numbered.
        let note = rig.viewer.path().join("note.txt");
        std::fs::write(&note, b"first").unwrap();
        rig.files.upload(&note, &Progress::new()).await.unwrap();
        std::fs::write(&note, b"second").unwrap();
        let second = rig.files.upload(&note, &Progress::new()).await.unwrap();
        assert_eq!(second, "Downloads/Rconn/note (2).txt");
        assert_eq!(std::fs::read(rig.inbox().join("note.txt")).unwrap(), b"first");
        let again = rig
            .files
            .download("Downloads/Rconn/big.bin", &dest, &Progress::new())
            .await
            .unwrap();
        assert_eq!(again, dest.join("big (2).bin"));
        rig.close().await;
    })
    .await
    .expect("test timed out");
}

/// The main channel's round trip (ping to pong) during a large transfer each way, and
/// how much file data a message on it could queue behind: on loopback the link is never
/// the limit, so the bytes queued are what tells how long input would wait on a slow one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_messages_stay_prompt_during_a_large_transfer() {
    tokio::time::timeout(Duration::from_secs(300), async {
        let rig = rig(true, true).await;
        let src = rig.viewer.path().join("large.bin");
        noise_file(&src, 64 * MB);
        std::fs::copy(&src, rig.host_root.path().join("large.bin")).unwrap();
        let dc = rig.session.data.clone();
        let dest = rig.viewer.path().join("dl");
        for upload in [true, false] {
            let progress = Progress::new();
            let pings = async {
                let mut out: Vec<(Duration, usize)> = Vec::new();
                for n in 0u64.. {
                    if progress.total() > 0 && progress.done() >= progress.total() {
                        break;
                    }
                    let queued = rig.files.queued().await;
                    let ping = DataMessage::Control(Control::Ping(n)).encode().unwrap();
                    let sent = Instant::now();
                    dc.send(BytesMut::from(&ping[..])).await.unwrap();
                    loop {
                        match dc.poll().await {
                            Some(DataChannelEvent::OnMessage(m))
                                if DataMessage::decode(&m.data).ok()
                                    == Some(DataMessage::Control(Control::Pong(n))) =>
                            {
                                break;
                            }
                            Some(_) => {}
                            None => return out,
                        }
                    }
                    out.push((sent.elapsed(), queued));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                out
            };
            let t = Instant::now();
            let transfer = async {
                if upload {
                    rig.files.upload(&src, &progress).await.map(|_| ())
                } else {
                    rig.files
                        .download("large.bin", &dest, &progress)
                        .await
                        .map(|_| ())
                }
            };
            let (pings, done) = tokio::join!(pings, transfer);
            done.unwrap();
            let took = t.elapsed();
            let mut rtts: Vec<Duration> = pings.iter().map(|p| p.0).collect();
            rtts.sort();
            let most_queued = pings.iter().map(|p| p.1).max().unwrap_or(0);
            let pct = |p: usize| rtts[(rtts.len() - 1) * p / 100];
            eprintln!(
                "{}: 64 MB in {took:?} ({:.0} MB/s); {} pings, median {:?}, p95 {:?}, worst {:?}; \
                 file data queued ahead of a ping: at most {} KB",
                if upload { "upload" } else { "download" },
                64.0 / took.as_secs_f64(),
                rtts.len(),
                pct(50),
                pct(95),
                rtts.last().unwrap(),
                most_queued / 1024,
            );
            assert!(rtts.len() >= 5, "pings answered during the transfer");
            assert!(
                *rtts.last().unwrap() < Duration::from_millis(100),
                "input waited behind file data: {:?}",
                rtts.last()
            );
            // What was queued, at the rate it drained: how long a message waited behind it
            // (in flight or not), within QUEUE_TIME and a little slack for measuring. (The
            // viewer sees only its own channel: during a download the queue is the host's,
            // and the ping's round trip is the measure.) A long, slow path cannot be made
            // on loopback; the budget's own tests model one.
            let rate = 64.0 * MB as f64 / took.as_secs_f64();
            let behind = Duration::from_secs_f64(most_queued as f64 / rate);
            eprintln!("  at most {behind:?} of file data ahead of a ping");
            assert!(
                behind <= rconn_peer::files::QUEUE_TIME + Duration::from_millis(15),
                "input waited {behind:?} behind file data"
            );
        }
        assert_eq!(sha256_of(&dest.join("large.bin")), sha256_of(&src));
        rig.close().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_mid_transfer_removes_the_partial_file_on_both_sides() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let rig = rig(true, true).await;
        let src = rig.viewer.path().join("movie.bin");
        noise_file(&src, 40 * MB);

        // Upload, cancelled by the viewer.
        let progress = Progress::new();
        let canceller = cancel_after(progress.clone(), 4 * MB);
        let err = rig.files.upload(&src, &progress).await.unwrap_err();
        canceller.await.unwrap();
        assert_eq!(stopped(&err), Some(Stopped::Cancelled), "{err:#}");
        assert!(progress.done() < 40 * MB, "it stopped early");
        let inbox = rig.inbox();
        eventually("the host removes the partial upload", || {
            files_in(&inbox).is_empty()
        })
        .await;

        // Download, cancelled by the viewer.
        std::fs::copy(&src, rig.host_root.path().join("movie.bin")).unwrap();
        let dest = rig.viewer.path().join("dl");
        let progress = Progress::new();
        let canceller = cancel_after(progress.clone(), 4 * MB);
        let err = rig
            .files
            .download("movie.bin", &dest, &progress)
            .await
            .unwrap_err();
        canceller.await.unwrap();
        assert_eq!(stopped(&err), Some(Stopped::Cancelled), "{err:#}");
        assert!(files_in(&dest).is_empty(), "{:?}", files_in(&dest));

        // The channel is still good afterwards.
        let progress = Progress::new();
        let got = rig
            .files
            .download("movie.bin", &dest, &progress)
            .await
            .unwrap();
        assert_eq!(sha256_of(&got), sha256_of(&src));
        rig.close().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paths_outside_the_shared_folder_are_refused() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let rig = rig(true, true).await;
        let root = rig.host_root.path();
        std::fs::write(root.join("inside.txt"), b"ok").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("file.txt"), b"deeper").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();
        let dest = rig.viewer.path().join("dl");
        let get = |p: &str| {
            let (files, dest, p) = (&rig.files, dest.clone(), p.to_owned());
            async move { files.download(&p, &dest, &Progress::new()).await }
        };

        assert_eq!(
            std::fs::read(get("inside.txt").await.unwrap()).unwrap(),
            b"ok"
        );
        assert_eq!(
            std::fs::read(get("sub\\file.txt").await.unwrap()).unwrap(),
            b"deeper",
            "backslashes separate folders too"
        );
        let secret_abs = secret.to_string_lossy().into_owned();
        for bad in [
            "../secret.txt",
            "sub/../../secret.txt",
            "..\\secret.txt",
            secret_abs.as_str(),
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "inside.txt\0.png",
        ] {
            let err = get(bad).await.unwrap_err();
            assert!(stopped(&err).is_none(), "{bad}: {err:#}");
            assert!(rig.files.list(bad).await.is_err(), "list {bad}");
        }
        assert!(rig.files.list("..").await.is_err());
        assert!(rig.files.list("/").await.is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(outside.path(), root.join("out")).unwrap();
            symlink(&secret, root.join("out.txt")).unwrap();
            symlink(root.join("sub"), root.join("in")).unwrap();
            for bad in ["out/secret.txt", "out.txt"] {
                let err = get(bad).await.unwrap_err();
                assert!(
                    format!("{err:#}").contains("inside the shared folder"),
                    "{bad}: {err:#}"
                );
            }
            assert!(rig.files.list("out").await.is_err());
            // A link that stays inside is fine.
            assert_eq!(
                std::fs::read(get("in/file.txt").await.unwrap()).unwrap(),
                b"deeper"
            );
        }
        // Nothing from outside reached the viewer.
        for name in files_in(&dest) {
            assert_ne!(
                std::fs::read(dest.join(&name)).unwrap(),
                b"top secret",
                "{name}"
            );
        }
        rig.close().await;
    })
    .await
    .expect("test timed out");
}

/// An upload goes into the folder the viewer names (the one its Files panel shows), with
/// the same inside-the-root checks as listings and downloads; a plain name (viewers
/// before 0.1.11) still goes to Downloads/Rconn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploads_land_in_the_chosen_folder_and_never_outside_the_root() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let rig = rig(true, true).await;
        let root = rig.host_root.path();
        std::fs::create_dir_all(root.join("Projects").join("Q3")).unwrap();
        std::fs::write(root.join("inside.txt"), b"ok").unwrap();
        let note = rig.viewer.path().join("plan.txt");
        std::fs::write(&note, b"the plan").unwrap();
        let up = |folder: Option<&str>| {
            let (files, note, folder) = (&rig.files, note.clone(), folder.map(str::to_owned));
            async move {
                files
                    .upload_into(&note, folder.as_deref(), &Progress::new())
                    .await
            }
        };

        // Into a subfolder, twice: never replacing a file there.
        assert_eq!(
            up(Some("Projects/Q3")).await.unwrap(),
            "Projects/Q3/plan.txt"
        );
        assert_eq!(
            up(Some("Projects\\Q3/")).await.unwrap(),
            "Projects/Q3/plan (2).txt"
        );
        assert_eq!(
            std::fs::read(root.join("Projects").join("Q3").join("plan.txt")).unwrap(),
            b"the plan"
        );
        // Into the root.
        assert_eq!(up(Some("")).await.unwrap(), "plan.txt");
        // A plain name: the inbox, as before.
        assert_eq!(up(None).await.unwrap(), "Downloads/Rconn/plan.txt");
        assert_eq!(
            rig.files.upload(&note, &Progress::new()).await.unwrap(),
            "Downloads/Rconn/plan (2).txt"
        );

        let before = files_in(root);
        for bad in [
            "..",
            "Projects/../..",
            "../outside",
            "/etc",
            "C:\\Windows",
            "\\\\server\\share",
            "missing",
            "inside.txt",
        ] {
            let err = up(Some(bad)).await.unwrap_err();
            assert!(stopped(&err).is_none(), "{bad}: {err:#}");
            assert!(
                format!("{err:#}").contains("can't save into"),
                "{bad}: {err:#}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), root.join("out")).unwrap();
            symlink(root.join("Projects"), root.join("in")).unwrap();
            let err = up(Some("out")).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("inside the shared folder"),
                "{err:#}"
            );
            assert!(
                files_in(outside.path()).is_empty(),
                "nothing written outside"
            );
            // A link that stays inside is fine; the host says where it really went.
            assert_eq!(up(Some("in")).await.unwrap(), "Projects/plan.txt");
        }
        let mut after = files_in(root);
        after.retain(|n| n != "out" && n != "in");
        assert_eq!(
            after, before,
            "nothing new at the root from the refused uploads"
        );
        rig.close().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_channel_is_refused_when_the_setting_or_the_policy_is_off() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for (setting, policy) in [(false, true), (true, false)] {
            let rig = rig(setting, policy).await;
            let err = rig.files.list("").await.unwrap_err();
            assert!(
                format!("{err:#}").contains("turned off"),
                "setting={setting} policy={policy}: {err:#}"
            );
            let note = rig.viewer.path().join("note.txt");
            std::fs::write(&note, b"hi").unwrap();
            assert!(rig.files.upload(&note, &Progress::new()).await.is_err());
            assert!(files_in(&rig.inbox()).is_empty());
            rig.close().await;
        }
    })
    .await
    .expect("test timed out");
}
