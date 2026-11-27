//! A local signaling server plus a host streaming a synthetic test pattern, for trying the
//! iOS app in the simulator without a real computer:
//!
//!   cargo run -p rconn-mobile --example test_host -- <access password> [WxH] [listen addr] [shared folder]
//!
//! Prints the server URL and computer ID to enter in the app, then a line per 5 s with the
//! frames sent and the input received. It also sends a quiet test beep, and shares the
//! given folder (default: a new temporary one with a few files) on the file channel.
//! Nothing here touches this Mac's screen or input.

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(password) = args.next() else {
        eprintln!("usage: test_host <access password> [WxH] [listen addr]");
        std::process::exit(2);
    };
    let (width, height) = args
        .next()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280, 720));
    let listen = args.next().unwrap_or_else(|| "127.0.0.1:8787".into());
    let files_root = match args.next() {
        Some(dir) => std::path::PathBuf::from(dir),
        None => sample_folder(),
    };
    let (url, id, log) = support::start(support::HostSetup {
        password,
        width,
        height,
        listen,
        files_root: Some(files_root.clone()),
    })
    .await;
    println!("server {url}");
    println!("computer ID {id}");
    println!("shared folder {}", files_root.display());
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        println!(
            "frames sent {}, sound packets {}, input events {}, resizes {:?}, keyframe requests {}",
            log.frames_sent.load(Ordering::Relaxed),
            log.sound_packets_sent.load(Ordering::Relaxed),
            log.input.lock().unwrap().len(),
            log.resizes.lock().unwrap(),
            log.keyframe_requests.load(Ordering::Relaxed),
        );
    }
}

/// A temporary home folder with a few things to browse and fetch.
fn sample_folder() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("rconn-test-host-{}", std::process::id()));
    let docs = root.join("Documents");
    std::fs::create_dir_all(root.join("Downloads")).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        root.join("Downloads").join("notes.txt"),
        "Hello from the test host.\n",
    )
    .unwrap();
    std::fs::write(docs.join("report.csv"), "a,b\n1,2\n").unwrap();
    let big: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("Downloads").join("big.bin"), big).unwrap();
    root
}
