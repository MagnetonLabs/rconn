//! Headless viewer: receive and decode the host's screen, report stream health.

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DataMessage};
use rconn_media::video::{DecodedFrame, VideoDecoder};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_peer::video::{KeyframeNeed, VideoReceiver};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;
use webrtc::media_stream::track_remote::TrackRemoteEvent;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    server: &str,
    trust: &ServerTrust,
    token: &str,
    host: HostId,
    seconds: u64,
    snapshot: Option<PathBuf>,
    openh264: &Path,
    opts: PeerOptions,
    tap_key: Option<(u64, Vec<u16>)>,
    fit: Option<(u32, u32)>,
    guest: Option<crate::access::GuestLogin>,
) -> Result<()> {
    let t0 = Instant::now();
    let (sig, host) = match &guest {
        Some(g) => SignalingClient::connect_guest(server, trust, &g.id).await?,
        None => (
            SignalingClient::connect_operator(server, trust, token).await?,
            host,
        ),
    };
    let mut session = open_viewer_session(sig, host, opts).await?;
    let mut exchange = guest
        .as_ref()
        .map(|g| rconn_peer::guest::Exchange::new(g.password.clone(), &session));
    // Guests may wait for the person at the computer (up to a minute on the host's side).
    let first_video = Duration::from_secs(if guest.is_some() { 75 } else { 20 });
    let (refused_tx, mut refused_rx) = tokio::sync::oneshot::channel::<String>();
    let mut refused_tx = Some(refused_tx);

    // Ask for a keyframe as soon as the data channel opens (the first IDR may have been
    // sent before our decoder existed).
    let dc = session.data.clone();
    let (kf_tx, mut kf_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Test aid: tap a key or chord (HID usages, pressed in order, released in reverse)
    // N seconds after the channel opens.
    if let Some((after, keys)) = tap_key {
        let dc = session.data.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(after)).await;
            let presses = keys.iter().map(|&k| (k, true));
            let releases = keys.iter().rev().map(|&k| (k, false));
            for (hid_usage, pressed) in presses.chain(releases) {
                if let Ok(b) =
                    DataMessage::Input(rconn_core::protocol::InputEvent::Key { hid_usage, pressed })
                        .encode()
                {
                    let _ = dc.send(BytesMut::from(&b[..])).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            println!("tapped {keys:x?} at +{after} s");
        });
    }
    // Test aid: ask the host to fit its screen to WxH, as a viewer window would.
    if let Some((width, height)) = fit {
        let dc = session.data.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(b) = DataMessage::Control(Control::Resize { width, height }).encode() {
                let _ = dc.send(BytesMut::from(&b[..])).await;
            }
            println!("asked the host to fit {width}x{height}");
        });
    }
    let dc_task = tokio::spawn(async move {
        let send_kf = |dc: std::sync::Arc<dyn webrtc::data_channel::DataChannel>| async move {
            if let Ok(b) = DataMessage::Control(Control::RequestKeyframe).encode() {
                let _ = dc.send(BytesMut::from(&b[..])).await;
            }
        };
        loop {
            tokio::select! {
                ev = dc.poll() => match ev {
                    Some(DataChannelEvent::OnOpen) => send_kf(dc.clone()).await,
                    Some(DataChannelEvent::OnMessage(m)) => {
                        if let (Ok(DataMessage::Guest(g)), Some(ex)) =
                            (DataMessage::decode(&m.data), exchange.as_mut())
                        {
                            match ex.handle(g).await {
                                rconn_peer::guest::Step::Send(reply) => {
                                    if let Ok(b) = reply.encode() {
                                        let _ = dc.send(BytesMut::from(&b[..])).await;
                                    }
                                }
                                rconn_peer::guest::Step::Waiting(text) => println!("{text}"),
                                rconn_peer::guest::Step::Accepted => {
                                    println!("accepted");
                                    send_kf(dc.clone()).await;
                                }
                                rconn_peer::guest::Step::Refused(why) => {
                                    if let Some(tx) = refused_tx.take() {
                                        let _ = tx.send(why);
                                    }
                                }
                            }
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                },
                req = kf_rx.recv() => match req {
                    Some(()) => send_kf(dc.clone()).await,
                    None => break,
                },
            }
        }
    });

    // Two tracks may arrive in either order: video feeds the decoder below, audio gets
    // a headless meter (level + pitch), so tests can verify sound without playing it.
    let (video_tx, mut video_rx) = tokio::sync::mpsc::unbounded_channel();
    let audio_report = std::sync::Arc::new(std::sync::Mutex::new(AudioMeter::default()));
    let meter = audio_report.clone();
    let mut events = std::mem::replace(
        &mut session.peer.events,
        tokio::sync::mpsc::unbounded_channel().1,
    );
    let router = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            let PeerEvent::Track(t) = ev else { continue };
            if t.kind().await == rtc::rtp_transceiver::rtp_sender::RtpCodecKind::Audio {
                let meter = meter.clone();
                tokio::spawn(async move {
                    let Ok(mut dec) = rconn_media::codec::opus_decoder() else {
                        return;
                    };
                    let mut pcm = vec![0f32; 960 * 2];
                    while let Some(ev) = t.poll().await {
                        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
                            continue;
                        };
                        if let Ok(n) = dec.decode_float(&pkt.payload, &mut pcm, false) {
                            meter.lock().expect("meter").add(&pcm[..n * 2]);
                        }
                    }
                });
            } else {
                let _ = video_tx.send(t);
            }
        }
    });
    let track = tokio::select! {
        t = tokio::time::timeout(first_video, video_rx.recv()) => t
            .map_err(|_| anyhow!("no video from the host within {} s (is it capturing?)", first_video.as_secs()))?
            .ok_or_else(|| anyhow!("session closed before video arrived"))?,
        Ok(why) = &mut refused_rx => {
            println!("refused: {why}");
            session.close().await;
            anyhow::bail!("refused: {why}");
        }
    };
    let mut rx = VideoReceiver::default();
    let mut dec = VideoDecoder::new(openh264)?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut window = Window::default();
    let mut last: Option<DecodedFrame> = None;
    let mut first_frame = None;
    let mut total = 0u32;

    let mut need = KeyframeNeed::default();
    while Instant::now() < deadline {
        let polled = tokio::time::timeout(Duration::from_millis(250), track.poll()).await;
        if need.poll(Instant::now()) {
            tracing::warn!("no picture from the video; requesting keyframe");
            let _ = kf_tx.send(());
        }
        let Ok(Some(ev)) = polled else {
            continue;
        };
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        if !pkt.payload.is_empty() {
            need.data(Instant::now());
        }
        let aus = rx.push(pkt);
        if rx.take_loss() {
            tracing::warn!("video packets lost");
            need.lost();
        }
        if dec.codec() != rx.codec() {
            println!("video codec: {}", rx.codec().name());
            dec.set_codec(rx.codec())?;
        }
        for au in aus {
            window.bytes += au.len();
            let (au, _) = rconn_peer::frames::take(rx.codec(), &au);
            let d0 = Instant::now();
            match dec.decode(&au) {
                Ok(Some(f)) => {
                    window.decode += d0.elapsed();
                    window.frames += 1;
                    total += 1;
                    need.picture();
                    if first_frame.is_none() {
                        first_frame = Some(t0.elapsed());
                        println!(
                            "first frame {}x{} after {:.0} ms",
                            f.width,
                            f.height,
                            t0.elapsed().as_secs_f64() * 1e3
                        );
                    }
                    last = Some(f);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "decode error");
                    need.lost();
                }
            }
        }
        window.maybe_print(last.as_ref());
    }

    if let (Some(path), Some(f)) = (&snapshot, &last) {
        save_png(path, f)?;
        println!(
            "snapshot {}x{} saved to {}",
            f.width,
            f.height,
            path.display()
        );
    }
    println!("{total} frames decoded in {seconds} s");
    let (frames, rms, hz) = {
        let a = audio_report.lock().expect("meter");
        (a.frames, a.rms(), a.hz())
    };
    if frames > 0 {
        println!("audio: {frames} packets, rms {rms:.3}, ~{hz:.0} Hz");
    } else {
        println!("audio: none received");
    }
    router.abort();
    drop(kf_tx);
    dc_task.abort();
    session.close().await;
    if total == 0 {
        anyhow::bail!("no video frames decoded");
    }
    Ok(())
}

#[derive(Default)]
struct Window {
    frames: u32,
    bytes: usize,
    decode: Duration,
    since: Option<Instant>,
}

impl Window {
    fn maybe_print(&mut self, last: Option<&DecodedFrame>) {
        let since = *self.since.get_or_insert_with(Instant::now);
        let secs = since.elapsed().as_secs_f64();
        if secs < 2.0 {
            return;
        }
        let size = last
            .map(|f| format!("{}x{}", f.width, f.height))
            .unwrap_or_else(|| "-".into());
        println!(
            "{size}  {:.1} fps  {:.0} kbps  decode {:.1} ms/frame",
            f64::from(self.frames) / secs,
            self.bytes as f64 * 8.0 / secs / 1000.0,
            self.decode.as_secs_f64() * 1e3 / f64::from(self.frames.max(1))
        );
        *self = Self::default();
    }
}

pub(crate) fn save_png(path: &Path, f: &DecodedFrame) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        f.width as u32,
        f.height as u32,
    );
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&f.to_rgba())?;
    Ok(())
}

/// Headless audio meter over the left channel: RMS level and zero-crossing pitch.
#[derive(Default)]
struct AudioMeter {
    frames: u64,
    samples: u64,
    sum_sq: f64,
    crossings: u64,
    last: f32,
}

impl AudioMeter {
    fn add(&mut self, interleaved: &[f32]) {
        self.frames += 1;
        for &v in interleaved.iter().step_by(2) {
            self.samples += 1;
            self.sum_sq += f64::from(v * v);
            if self.last <= 0.0 && v > 0.0 {
                self.crossings += 1;
            }
            self.last = v;
        }
    }
    fn rms(&self) -> f64 {
        (self.sum_sq / self.samples.max(1) as f64).sqrt()
    }
    fn hz(&self) -> f64 {
        self.crossings as f64 / (self.samples.max(1) as f64 / 48_000.0)
    }
}
