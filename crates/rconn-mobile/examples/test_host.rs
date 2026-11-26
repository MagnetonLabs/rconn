//! A local signaling server plus a host streaming a synthetic test pattern, for trying the
//! iOS app in the simulator without a real computer:
//!
//!   cargo run -p rconn-mobile --example test_host -- <access password> [WxH] [listen addr]
//!
//! Prints the server URL and computer ID to enter in the app, then a line per 5 s with the
//! frames sent and the input received. Nothing here touches this Mac's screen or input.

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
    let (url, id, log) = support::start(support::HostSetup {
        password,
        width,
        height,
        listen,
    })
    .await;
    println!("server {url}");
    println!("computer ID {id}");
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        println!(
            "frames sent {}, input events {}, resizes {:?}, keyframe requests {}",
            log.frames_sent.load(Ordering::Relaxed),
            log.input.lock().unwrap().len(),
            log.resizes.lock().unwrap(),
            log.keyframe_requests.load(Ordering::Relaxed),
        );
    }
}
