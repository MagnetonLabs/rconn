//! Host sound: capture what the machine is playing (loopback), encode Opus, send.
//!
//! Windows: WASAPI loopback (cpal opens an *input* stream on the default *output*
//! device). Linux: PipeWire's capture of the default output (never a microphone, which is
//! what cpal's "input on the output device" opens there). Audio is converted to 48 kHz stereo, cut into 20 ms frames and Opus-encoded.
//! A machine without an output device (soundcard-less cloud VM) simply has no audio;
//! the session carries on without it.

use anyhow::{Result, anyhow};
use rconn_peer::video::AudioSender;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 960; // samples per channel in 20 ms
const FRAME_DUR: Duration = Duration::from_millis(20);

pub enum Source {
    /// What the host is playing.
    Loopback,
    /// A 440 Hz tone, for tests and setups without sound.
    TestTone,
}

/// Stream until `stop` is set or the track stops accepting packets.
pub async fn run(source: Source, audio: AudioSender, stop: Arc<AtomicBool>) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let capture = match source {
        Source::Loopback => Some(spawn_loopback(tx, stop.clone())?),
        Source::TestTone => {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut n = 0usize;
                let mut tick = tokio::time::interval(FRAME_DUR);
                while !stop.load(Ordering::Relaxed) {
                    tick.tick().await;
                    let mut pcm = Vec::with_capacity(FRAME * 2);
                    for _ in 0..FRAME {
                        let v = (2.0 * std::f32::consts::PI * 440.0 * n as f32 / RATE as f32).sin()
                            * 0.3;
                        pcm.extend([v, v]);
                        n += 1;
                    }
                    if tx.send(pcm).is_err() {
                        break;
                    }
                }
            });
            None
        }
    };

    let mut enc = rconn_media::codec::opus_encoder()?;
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut out = vec![0u8; 4000];
    // WASAPI loopback delivers nothing while the host is silent. Fill gaps with silence so
    // the viewer's audio track is established (and timing continuous) from the start;
    // Opus codes a silent 20 ms frame in a few bytes.
    let mut tick = tokio::time::interval(FRAME_DUR);
    let mut last_data = tokio::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { break };
                last_data = tokio::time::Instant::now();
                pending.extend(chunk);
            }
            _ = tick.tick() => {
                if last_data.elapsed() > FRAME_DUR * 5 && pending.len() < FRAME * 2 {
                    pending.resize(FRAME * 2, 0.0);
                }
            }
        }
        while pending.len() >= FRAME * 2 {
            let frame: Vec<f32> = pending.drain(..FRAME * 2).collect();
            let len = enc.encode_float(&frame, &mut out)?;
            if audio.write(out[..len].to_vec(), FRAME_DUR).await.is_err() {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(t) = capture {
        let _ = t.join();
    }
    Ok(())
}

/// Linear resampler + channel mapper to 48 kHz stereo, streaming across callbacks.
pub struct ToStereo48k {
    channels: usize,
    step: f64, // input frames per output frame
    pos: f64,  // fractional position into the pending input
    prev: [f32; 2],
}

impl ToStereo48k {
    pub fn new(in_rate: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            step: f64::from(in_rate) / f64::from(RATE),
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    fn stereo(&self, frame: &[f32]) -> [f32; 2] {
        match self.channels {
            1 => [frame[0], frame[0]],
            2 => [frame[0], frame[1]],
            // Surround: front left/right carry most program material.
            _ => [frame[0], frame[1]],
        }
    }

    /// Convert one interleaved input chunk to interleaved 48 kHz stereo.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let frames: Vec<[f32; 2]> = input
            .chunks_exact(self.channels)
            .map(|f| self.stereo(f))
            .collect();
        if frames.is_empty() {
            return Vec::new();
        }
        if (self.step - 1.0).abs() < f64::EPSILON {
            return frames.iter().flat_map(|f| [f[0], f[1]]).collect();
        }
        let mut out = Vec::with_capacity((frames.len() as f64 / self.step) as usize * 2 + 4);
        // Positions are relative to `prev` at -1 and frames[0] at 0.
        while self.pos < frames.len() as f64 - 1.0 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let a = if i < 0.0 {
                self.prev
            } else {
                frames[i as usize]
            };
            let b = frames[(i + 1.0) as usize];
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.pos -= frames.len() as f64;
        self.prev = *frames.last().expect("non-empty");
        out
    }
}

#[cfg(target_os = "linux")]
fn spawn_loopback(
    tx: mpsc::UnboundedSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    use rconn_media::audio_monitor::{CHANNELS, RATE as PW_RATE, spawn};
    let mut conv = ToStereo48k::new(PW_RATE, CHANNELS as usize);
    let thread = spawn(
        move |data| {
            let _ = tx.send(conv.process(data));
        },
        stop,
    )
    .map_err(|e| anyhow!("PipeWire sound capture: {e}"))?;
    tracing::info!("host audio: capturing the default output through PipeWire");
    Ok(thread)
}

#[cfg(not(target_os = "linux"))]
fn spawn_loopback(
    tx: mpsc::UnboundedSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    use anyhow::Context as _;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String>>();
    // cpal streams are not Send everywhere: build and own the stream on this thread.
    let thread = std::thread::Builder::new()
        .name("rconn-audio".into())
        .spawn(move || {
            let built = (|| -> Result<(cpal::Stream, String)> {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| anyhow!("no audio output device"))?;
                let cfg = device
                    .default_output_config()
                    .context("output device config")?;
                let desc = format!(
                    "{} Hz, {} ch, {:?}",
                    cfg.sample_rate(),
                    cfg.channels(),
                    cfg.sample_format()
                );
                let conv = std::sync::Mutex::new(ToStereo48k::new(
                    cfg.sample_rate(),
                    usize::from(cfg.channels()),
                ));
                let err = |e: cpal::Error| tracing::warn!(error = %e, "audio capture error");
                let stream = match cfg.sample_format() {
                    cpal::SampleFormat::F32 => {
                        let tx = tx.clone();
                        device.build_input_stream::<f32, _, _>(
                            cfg.config(),
                            move |data, _| {
                                let _ = tx.send(conv.lock().expect("resampler").process(data));
                            },
                            err,
                            None,
                        )?
                    }
                    cpal::SampleFormat::I16 => {
                        let tx = tx.clone();
                        device.build_input_stream::<i16, _, _>(
                            cfg.config(),
                            move |data, _| {
                                let f: Vec<f32> =
                                    data.iter().map(|s| f32::from(*s) / 32768.0).collect();
                                let _ = tx.send(conv.lock().expect("resampler").process(&f));
                            },
                            err,
                            None,
                        )?
                    }
                    other => return Err(anyhow!("unsupported sample format {other:?}")),
                };
                stream.play()?;
                Ok((stream, desc))
            })();
            match built {
                Ok((stream, desc)) => {
                    let _ = ready_tx.send(Ok(desc));
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
    let desc = ready_rx.recv().context("audio thread died")??;
    tracing::info!(format = %desc, "host audio loopback started");
    Ok(thread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_preserves_pitch_and_length() {
        // 1 s of 440 Hz mono at 44.1 kHz, fed in uneven chunks.
        let input: Vec<f32> = (0..44_100)
            .map(|n| (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 44_100.0).sin())
            .collect();
        let mut r = ToStereo48k::new(44_100, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(441 * 3 + 7) {
            out.extend(r.process(chunk));
        }
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(
            (left.len() as i64 - 48_000).abs() < 10,
            "got {} samples",
            left.len()
        );
        let crossings = left
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        assert!(
            (crossings as i64 - 440).abs() <= 2,
            "pitch drifted: {crossings} cycles"
        );
    }

    #[test]
    fn stereo_and_surround_map_to_two_channels() {
        let mut r = ToStereo48k::new(48_000, 6);
        let out = r.process(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        assert_eq!(out, vec![0.1, 0.2]);
    }
}
