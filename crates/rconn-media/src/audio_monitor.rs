//! Linux: what the computer is playing, from PipeWire (the default output's monitor).
//!
//! ALSA has no loopback: "record the output device" opens the default *input*, the
//! microphone. PipeWire can capture a sink instead (`stream.capture.sink`), which is what
//! the speakers get, and never touches a microphone.

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Pod, Value, serialize::PodSerializer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Sample rate and channel count delivered (PipeWire converts to these).
pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

/// Capture the default output's sound as interleaved 48 kHz stereo `f32`, calling `on_samples`
/// from PipeWire's thread until `stop` is set. Returns once the stream is connected.
pub fn spawn(
    on_samples: impl FnMut(&[f32]) + Send + 'static,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("rconn-audio".into())
        .spawn(move || {
            if let Err(e) = run(on_samples, &stop, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("PipeWire didn't answer".into()),
    }
}

fn format_pod() -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .map_err(|e| format!("audio format: {e:?}"))
}

fn run(
    mut on_samples: impl FnMut(&[f32]) + 'static,
    stop: &Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connecting to PipeWire: {e}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "rconn-host-sound",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            // Record a sink (what is played), never a source (a microphone).
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .map_err(|e| format!("stream: {e}"))?;
    let mut samples: Vec<f32> = Vec::new();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            while let Some(mut buf) = stream.dequeue_buffer() {
                let Some(data) = buf.datas_mut().first_mut() else {
                    continue;
                };
                let chunk = data.chunk();
                let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
                let Some(bytes) = data.data() else { continue };
                let start = offset.min(bytes.len());
                let end = start.saturating_add(size).min(bytes.len());
                samples.clear();
                samples.extend(
                    bytes[start..end]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b)),
                );
                if !samples.is_empty() {
                    on_samples(&samples);
                }
            }
        })
        .register()
        .map_err(|e| format!("stream listener: {e}"))?;
    let format = format_pod()?;
    let mut params = [Pod::from_bytes(&format).ok_or("format pod")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connecting the sound stream: {e}"))?;
    // Quit when asked: a small watcher turns the stop flag into a loop wake-up.
    let (quit, quit_rx) = pw::channel::channel::<()>();
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |()| ml.quit());
    let watch = stop.clone();
    std::thread::spawn(move || {
        while !watch.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = quit.send(());
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}
