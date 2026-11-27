//! Plays the host's sound on this device's default output (the desktop viewer and the iOS
//! app share it): Opus packets in, the speakers out.
//!
//! Decoded 48 kHz stereo goes into a bounded queue. The device callback drains it, filling
//! silence on underrun; on overrun the oldest audio is dropped, so latency can't build up
//! behind a stall. Dropping the [`Player`] stops the output stream.

use anyhow::{Context, Result, anyhow};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 48 kHz stereo samples per millisecond.
const SAMPLES_PER_MS: usize = 48 * 2;
/// The most PCM one Opus packet decodes to: 120 ms of 48 kHz stereo.
const MAX_PACKET_SAMPLES: usize = 960 * 2 * 6;

pub struct Player {
    queue: Arc<Mutex<VecDeque<f32>>>,
    max_queue: usize,
    /// Dropped to stop the stream (its thread waits on the other end).
    _stop: std::sync::mpsc::Sender<()>,
}

impl Player {
    /// Start playback on a dedicated thread (cpal streams are not Send everywhere),
    /// holding at most `max_queue_ms` of sound.
    pub fn start(max_queue_ms: usize) -> Result<Self> {
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let q = queue.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String>>();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("rconn-playback".into())
            .spawn(move || {
                use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
                let built = (|| -> Result<(cpal::Stream, String)> {
                    let device = cpal::default_host()
                        .default_output_device()
                        .ok_or_else(|| anyhow!("no output device"))?;
                    let cfg = device.default_output_config().context("output config")?;
                    let (rate, channels) = (cfg.sample_rate(), usize::from(cfg.channels()));
                    let desc = format!("{rate} Hz, {channels} ch");
                    let step = 48_000.0 / f64::from(rate); // source frames per device frame
                    let mut pos = 0.0f64;
                    let stream = device.build_output_stream::<f32, _, _>(
                        cfg.config(),
                        move |out, _| {
                            let mut q = q.lock().expect("playback queue");
                            for frame in out.chunks_mut(channels) {
                                // Linear-interpolated read at the device rate.
                                let i = pos.floor() as usize;
                                let t = (pos - pos.floor()) as f32;
                                let sample = |idx: usize, ch: usize| {
                                    q.get(idx * 2 + ch).copied().unwrap_or(0.0)
                                };
                                let l = sample(i, 0) + (sample(i + 1, 0) - sample(i, 0)) * t;
                                let r = sample(i, 1) + (sample(i + 1, 1) - sample(i, 1)) * t;
                                for (c, out) in frame.iter_mut().enumerate() {
                                    *out = if c % 2 == 0 { l } else { r };
                                }
                                pos += step;
                            }
                            let consumed = (pos.floor() as usize).min(q.len() / 2);
                            q.drain(..consumed * 2);
                            pos -= pos.floor();
                        },
                        |e| tracing::warn!(error = %e, "playback error"),
                        None,
                    )?;
                    stream.play()?;
                    Ok((stream, desc))
                })();
                match built {
                    Ok((stream, desc)) => {
                        let _ = ready_tx.send(Ok(desc));
                        // Until the Player is dropped.
                        let _ = stopped.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        let desc = ready_rx.recv().context("playback thread died")??;
        tracing::info!(device = %desc, "audio playback started");
        Ok(Self {
            queue,
            max_queue: max_queue_ms * SAMPLES_PER_MS,
            _stop: stop,
        })
    }

    /// Queue decoded 48 kHz interleaved stereo.
    pub fn push(&self, pcm: &[f32]) {
        bounded_push(
            &mut self.queue.lock().expect("playback queue"),
            pcm,
            self.max_queue,
        );
    }
}

/// Decodes the host's Opus packets (48 kHz stereo) into PCM for a [`Player`].
pub struct SoundDecoder {
    dec: opus::Decoder,
    pcm: Vec<f32>,
}

impl SoundDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dec: crate::codec::opus_decoder()?,
            pcm: vec![0.0; MAX_PACKET_SAMPLES],
        })
    }

    /// One RTP payload's interleaved stereo, or None if it doesn't decode.
    pub fn decode(&mut self, packet: &[u8]) -> Option<&[f32]> {
        let n = self.dec.decode_float(packet, &mut self.pcm, false).ok()?;
        Some(&self.pcm[..n * 2])
    }
}

/// Append, then drop the oldest whole stereo frames beyond `max` samples.
pub fn bounded_push(q: &mut VecDeque<f32>, pcm: &[f32], max: usize) {
    q.extend(pcm);
    let excess = q.len().saturating_sub(max);
    let excess = excess + excess % 2; // keep L/R pairs aligned
    q.drain(..excess.min(q.len()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_stays_bounded_and_keeps_the_newest_audio_aligned() {
        let max = 200 * SAMPLES_PER_MS;
        let mut q = VecDeque::new();
        let chunk: Vec<f32> = (0..1920).map(|i| i as f32).collect(); // 20 ms stereo
        for _ in 0..100 {
            bounded_push(&mut q, &chunk, max);
        }
        assert!(q.len() <= max, "{} > {max}", q.len());
        assert_eq!(q.len() % 2, 0);
        assert_eq!(*q.back().unwrap(), 1919.0, "newest sample kept");
        assert_eq!(
            *q.front().unwrap() as usize % 2,
            0,
            "front is a left sample"
        );
    }

    #[test]
    fn decoder_turns_a_host_packet_into_20ms_of_stereo() {
        let mut enc = crate::codec::opus_encoder().unwrap();
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 0.06).sin() * 0.3)
            .collect();
        let mut packet = [0u8; 4000];
        let n = enc.encode_float(&tone, &mut packet).unwrap();
        let mut dec = SoundDecoder::new().unwrap();
        assert_eq!(dec.decode(&packet[..n]).unwrap().len(), 960 * 2);
        assert!(dec.decode(&[0xff; 3]).is_none(), "garbage is refused");
    }
}
