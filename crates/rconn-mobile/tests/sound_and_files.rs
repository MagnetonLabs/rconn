//! The iOS session's sound and file transfer against the in-process test host: the host's
//! tone is decoded (no output device is opened), and the app's file calls browse, fetch
//! and send through the host's real `rconn-files/1` server.

mod support;

use rconn_mobile::{FileError, SessionListener, Transfer, VideoFrame, connect_guest};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::HostSetup;

const PASSWORD: &str = "harbor-violet-27";

#[derive(Default)]
struct App {
    pictures: Mutex<usize>,
    closed: Mutex<Option<String>>,
}

impl SessionListener for App {
    fn on_status(&self, _text: String) {}
    fn on_video(&self, _frame: VideoFrame) {
        *self.pictures.lock().unwrap() += 1;
    }
    fn on_clipboard(&self, _text: String) {}
    fn on_host_name(&self, _name: String) {}
    fn on_closed(&self, reason: String) {
        *self.closed.lock().unwrap() = Some(reason);
    }
}

async fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A host sharing `root`, and a connected session that has seen a picture.
async fn connect(
    root: Option<&Path>,
) -> (Arc<rconn_mobile::Session>, Arc<App>, Arc<support::HostLog>) {
    rconn_mobile::sound::use_null_output();
    let (url, id, host) = support::start(HostSetup {
        password: PASSWORD.into(),
        width: 320,
        height: 240,
        listen: "127.0.0.1:0".into(),
        files_root: root.map(Path::to_path_buf),
    })
    .await;
    let app = Arc::new(App::default());
    let session = connect_guest(url, None, id, PASSWORD.into(), app.clone());
    wait_for("a picture", Duration::from_secs(20), || {
        *app.pictures.lock().unwrap() > 0
    })
    .await;
    (session, app, host)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_computers_sound_is_decoded_while_on_and_not_while_off() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (session, _app, host) = connect(None).await;
        assert!(session.is_sound_on(), "sound starts on");
        wait_for("sound packets", Duration::from_secs(10), || {
            session.sound_packets() >= 25
        })
        .await;
        assert!(host.sound_packets_sent.load(Ordering::Relaxed) >= 25);

        session.set_sound(false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let off = session.sound_packets();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(session.sound_packets(), off, "nothing decoded while off");

        session.set_sound(true);
        wait_for("sound again", Duration::from_secs(5), || {
            session.sound_packets() >= off + 10
        })
        .await;
        session.disconnect();
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_are_browsed_fetched_and_sent_checked_end_to_end() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let home = tempfile::tempdir().unwrap();
        let downloads = home.path().join("Downloads");
        std::fs::create_dir_all(downloads.join("Rconn")).unwrap();
        std::fs::create_dir_all(home.path().join("Projects")).unwrap();
        let content: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(downloads.join("photo.jpg"), &content).unwrap();
        std::fs::write(downloads.join("a.txt"), b"hello").unwrap();
        let phone = tempfile::tempdir().unwrap();
        let (session, _app, _host) = connect(Some(home.path())).await;

        // Browse: folders first, then files by name; the root and a subfolder.
        let listed = session.list_folder("Downloads".into()).await.unwrap();
        let names: Vec<_> = listed.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(
            names,
            [("Rconn", true), ("a.txt", false), ("photo.jpg", false)]
        );
        assert_eq!(listed[2].size, content.len() as u64);
        let root = session.list_folder(String::new()).await.unwrap();
        assert!(root.iter().any(|e| e.name == "Projects" && e.is_dir));

        // Paths outside the shared folder are refused by the computer.
        for outside in ["..", "../..", "/etc", "Downloads/../../x"] {
            let err = session.list_folder(outside.into()).await.unwrap_err();
            assert!(
                matches!(&err, FileError::Failed { message } if !message.is_empty()),
                "{outside}: {err:?}"
            );
        }
        let t = Transfer::new();
        let err = session
            .download("../secret".into(), phone.path().display().to_string(), t)
            .await
            .unwrap_err();
        assert!(matches!(err, FileError::Failed { .. }), "{err:?}");

        // Download: progress to the full size, the same bytes, never replacing a file.
        let t = Transfer::new();
        let saved = session
            .download(
                "Downloads/photo.jpg".into(),
                phone.path().display().to_string(),
                t.clone(),
            )
            .await
            .unwrap();
        assert_eq!(Path::new(&saved), phone.path().join("photo.jpg"));
        assert_eq!(std::fs::read(&saved).unwrap(), content);
        assert_eq!(t.done_bytes(), content.len() as u64);
        assert_eq!(t.total_bytes(), content.len() as u64);
        let again = session
            .download(
                "Downloads/photo.jpg".into(),
                phone.path().display().to_string(),
                Transfer::new(),
            )
            .await
            .unwrap();
        assert_eq!(Path::new(&again), phone.path().join("photo (2).jpg"));

        // Upload into the folder shown.
        let local = phone.path().join("from phone.txt");
        std::fs::write(&local, b"sent from the phone").unwrap();
        let t = Transfer::new();
        let at = session
            .upload(local.display().to_string(), "Projects".into(), t.clone())
            .await
            .unwrap();
        assert!(at.contains("from phone.txt"), "{at}");
        assert_eq!(
            std::fs::read(home.path().join("Projects").join("from phone.txt")).unwrap(),
            b"sent from the phone"
        );
        assert_eq!(t.done_bytes(), 19);

        // Cancel: the request ends as cancelled and no partial file stays on the phone.
        let t = Transfer::new();
        t.cancel();
        let err = session
            .download(
                "Downloads/photo.jpg".into(),
                phone.path().display().to_string(),
                t,
            )
            .await
            .unwrap_err();
        assert_eq!(err, FileError::Cancelled);
        assert!(!phone.path().join("photo (3).jpg").exists());

        session.disconnect();
        wait_for("the end", Duration::from_secs(5), || {
            _app.closed.lock().unwrap().is_some()
        })
        .await;
        let err = session.list_folder(String::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "the session is over");
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_without_file_transfer_says_why() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (session, _app, _host) = connect(None).await;
        let err = session.list_folder(String::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "file transfer is off");
        session.disconnect();
    })
    .await
    .expect("test timed out");
}
