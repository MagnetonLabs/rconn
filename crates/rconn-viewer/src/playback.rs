//! Plays the host's sound on the viewer's default output device.
//!
//! Decoded 48 kHz stereo goes into a bounded queue (≈200 ms). The device callback drains it,
//! filling silence on underrun; on overrun the oldest audio is dropped, so latency can't
//! build up behind a stall.

use anyhow::{Context, Result, anyhow};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const MAX_QUEUE: usize = 48_000 * 2 / 5; // 200 ms of 48 kHz stereo samples

#[derive(Clone)]
pub struct Player {
    queue: Arc<Mutex<VecDeque<f32>>>,
}

impl Player {
    /// Start playback on a dedicated thread (cpal streams are not Send everywhere).
    pub fn start() -> Result<Self> {
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let q = queue.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String>>();
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
                        std::thread::park(); // keep the stream alive for the process lifetime
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        let desc = ready_rx.recv().context("playback thread died")??;
        tracing::info!(device = %desc, "audio playback started");
        Ok(Self { queue })
    }

    /// Queue decoded 48 kHz interleaved stereo.
    pub fn push(&self, pcm: &[f32]) {
        bounded_push(&mut self.queue.lock().expect("playback queue"), pcm);
    }
}

/// Append, then drop the oldest whole stereo frames beyond `MAX_QUEUE`.
fn bounded_push(q: &mut VecDeque<f32>, pcm: &[f32]) {
    q.extend(pcm);
    let excess = q.len().saturating_sub(MAX_QUEUE);
    let excess = excess + excess % 2; // keep L/R pairs aligned
    q.drain(..excess.min(q.len()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_stays_bounded_and_keeps_the_newest_audio_aligned() {
        let mut q = VecDeque::new();
        let chunk: Vec<f32> = (0..1920).map(|i| i as f32).collect(); // 20 ms stereo
        for _ in 0..100 {
            bounded_push(&mut q, &chunk);
        }
        assert!(q.len() <= MAX_QUEUE, "{} > {MAX_QUEUE}", q.len());
        assert_eq!(q.len() % 2, 0);
        assert_eq!(*q.back().unwrap(), 1919.0, "newest sample kept");
        assert_eq!(
            *q.front().unwrap() as usize % 2,
            0,
            "front is a left sample"
        );
    }
}
