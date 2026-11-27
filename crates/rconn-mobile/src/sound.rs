//! The host's sound: its Opus track decoded and played on the device's output (cpal, which
//! is CoreAudio's RemoteIO on iOS), as the desktop viewer does. The app sets up the audio
//! session (category, activation) before turning sound on.
//!
//! Latency stays low: at most [`MAX_QUEUE_MS`] waits to be played, older sound is dropped.
//! While sound is off nothing is decoded and the output is closed.

use rconn_media::playback::{Player, SoundDecoder};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

/// The most sound waiting to be played; beyond it the oldest is dropped.
pub const MAX_QUEUE_MS: usize = 120;
/// After the output failed to open, wait this long before trying again.
const RETRY_OUTPUT: Duration = Duration::from_secs(2);

/// Tests and the simulator's headless runs: decode, but open no output device.
static NULL_OUTPUT: AtomicBool = AtomicBool::new(false);

/// Decode the sound but don't play it (no output device is opened). For tests.
#[doc(hidden)]
pub fn use_null_output() {
    NULL_OUTPUT.store(true, Ordering::Relaxed);
}

/// Whether sound is on, shared by the session and its sound task.
#[derive(Debug)]
pub struct Sound {
    on: AtomicBool,
    changed: Notify,
    /// Packets decoded (for tests and logs).
    decoded: AtomicU64,
}

impl Default for Sound {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(true),
            changed: Notify::new(),
            decoded: AtomicU64::new(0),
        }
    }
}

impl Sound {
    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Relaxed)
    }
}

/// Where decoded sound goes.
pub trait Output: Send {
    fn push(&mut self, pcm: &[f32]);
}

impl Output for Player {
    fn push(&mut self, pcm: &[f32]) {
        Player::push(self, pcm);
    }
}

/// Discards the sound ([`use_null_output`]).
struct Null;

impl Output for Null {
    fn push(&mut self, _pcm: &[f32]) {}
}

/// Opens the output on demand.
pub type OpenOutput = Box<dyn FnMut() -> anyhow::Result<Box<dyn Output>> + Send>;

/// The packet-by-packet half of the sound task, without the track: decode while on, keep
/// the output open only while on, retry a failed output now and then.
pub struct Pipe {
    sound: Arc<Sound>,
    dec: SoundDecoder,
    open: OpenOutput,
    out: Option<Box<dyn Output>>,
    failed: Option<Instant>,
}

impl Pipe {
    pub fn new(sound: Arc<Sound>, open: OpenOutput) -> anyhow::Result<Self> {
        Ok(Self {
            sound,
            dec: SoundDecoder::new()?,
            open,
            out: None,
            failed: None,
        })
    }

    /// One RTP payload from the host.
    pub fn packet(&mut self, payload: &[u8]) {
        if !self.sound.is_on() {
            self.out = None;
            return;
        }
        if self.out.is_none() && self.failed.is_none_or(|t| t.elapsed() >= RETRY_OUTPUT) {
            match (self.open)() {
                Ok(o) => {
                    self.out = Some(o);
                    self.failed = None;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "no sound output");
                    self.failed = Some(Instant::now());
                }
            }
        }
        let Some(pcm) = self.dec.decode(payload) else {
            return;
        };
        self.sound.decoded.fetch_add(1, Ordering::Relaxed);
        if let Some(out) = self.out.as_mut() {
            out.push(pcm);
        }
    }

    /// Sound was switched: close the output now if it went off.
    pub fn switched(&mut self) {
        if !self.sound.is_on() {
            self.out = None;
        }
    }

    pub fn is_open(&self) -> bool {
        self.out.is_some()
    }
}

fn open_device() -> anyhow::Result<Box<dyn Output>> {
    if NULL_OUTPUT.load(Ordering::Relaxed) {
        return Ok(Box::new(Null));
    }
    // Starting waits for the device thread; a moment, off the async workers.
    let player = tokio::task::block_in_place(|| Player::start(MAX_QUEUE_MS))?;
    Ok(Box::new(player))
}

/// Play `track` until it ends.
pub async fn play(track: Arc<dyn TrackRemote>, sound: Arc<Sound>) {
    let mut pipe = match Pipe::new(sound.clone(), Box::new(open_device)) {
        Ok(p) => p,
        Err(e) => return tracing::warn!(error = %e, "no Opus decoder"),
    };
    loop {
        tokio::select! {
            ev = track.poll() => match ev {
                Some(TrackRemoteEvent::OnRtpPacket(pkt)) => pipe.packet(&pkt.payload),
                Some(_) => {}
                None => break,
            },
            () = sound.changed.notified() => pipe.switched(),
        }
    }
    tracing::info!(packets = sound.decoded(), "sound track ended");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Spy(Arc<Mutex<Vec<usize>>>);

    impl Output for Spy {
        fn push(&mut self, pcm: &[f32]) {
            self.0.lock().unwrap().push(pcm.len());
        }
    }

    fn packet() -> Vec<u8> {
        let mut enc = rconn_media::codec::opus_encoder().unwrap();
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 0.06).sin() * 0.3)
            .collect();
        let mut out = [0u8; 4000];
        let n = enc.encode_float(&tone, &mut out).unwrap();
        out[..n].to_vec()
    }

    #[test]
    fn plays_while_on_and_closes_the_output_when_off() {
        let sound = Arc::new(Sound::default());
        let spy = Spy::default();
        let opened = Arc::new(AtomicU64::new(0));
        let (s, o) = (spy.clone(), opened.clone());
        let mut pipe = Pipe::new(
            sound.clone(),
            Box::new(move || {
                o.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(s.clone()) as Box<dyn Output>)
            }),
        )
        .unwrap();
        let p = packet();
        pipe.packet(&p);
        pipe.packet(&p);
        assert_eq!(*spy.0.lock().unwrap(), [1920, 1920], "20 ms of stereo each");
        assert_eq!(
            opened.load(Ordering::Relaxed),
            1,
            "one output for the stream"
        );

        sound.set(false);
        pipe.switched();
        assert!(!pipe.is_open(), "off closes the output at once");
        pipe.packet(&p);
        assert_eq!(spy.0.lock().unwrap().len(), 2, "nothing played while off");
        assert_eq!(sound.decoded(), 2, "nor decoded");

        sound.set(true);
        pipe.packet(&p);
        assert_eq!(spy.0.lock().unwrap().len(), 3);
        assert_eq!(opened.load(Ordering::Relaxed), 2, "reopened when back on");
    }

    #[test]
    fn a_failed_output_is_retried_later_not_per_packet() {
        let sound = Arc::new(Sound::default());
        let tries = Arc::new(AtomicU64::new(0));
        let t = tries.clone();
        let mut pipe = Pipe::new(
            sound.clone(),
            Box::new(move || {
                t.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("no device")
            }),
        )
        .unwrap();
        let p = packet();
        for _ in 0..10 {
            pipe.packet(&p);
        }
        assert_eq!(tries.load(Ordering::Relaxed), 1);
        assert_eq!(
            sound.decoded(),
            10,
            "still decoded (the stream stays in step)"
        );
        assert!(!pipe.is_open());
    }
}
