//! Wayland capture: the desktop portal's screen stream ([`crate::portal`]), read from
//! PipeWire. Frames arrive in shared memory (MemFd/MemPtr, never DMA-BUF: no modifiers are
//! offered) as BGRx/BGRA (or RGBx/RGBA, swapped here). Only the areas the compositor's
//! `SPA_META_VideoDamage` reports are copied and converted; buffers without damage
//! metadata count as wholly changed.

use super::{CaptureError, Captured, Capturer, convert_whole};
use crate::portal::{self, Lease};
use crate::video::Frame;
use crate::yuv::{I420, Rect};
use pipewire as pw;
use pw::spa;
use spa::buffer::meta::MetaVideoDamage;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{Pod, Property, Value, serialize::PodSerializer};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long the first call waits for the consent dialog before reporting "waiting".
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// Paused this long right after a screen size change: the stream has ended.
const STOPPED_AFTER: Duration = Duration::from_millis(500);
/// More damage rectangles than this pending: treat the frame as wholly changed.
const MAX_RECTS: usize = 64;

fn backend(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("Wayland {what}: {e}"))
}

/// What changed in [`Latest::image`] since the consumer last took it.
#[derive(Debug, Clone, PartialEq, Default)]
enum Damage {
    #[default]
    None,
    Rects(Vec<Rect>),
    Whole,
}

impl Damage {
    fn add(&mut self, rects: Option<&[Rect]>) {
        let Some(rects) = rects else {
            *self = Damage::Whole;
            return;
        };
        match self {
            Damage::Whole => {}
            Damage::None => *self = Damage::Rects(rects.to_vec()),
            Damage::Rects(v) => v.extend_from_slice(rects),
        }
        if let Damage::Rects(v) = self
            && v.len() > MAX_RECTS
        {
            *self = Damage::Whole;
        }
    }
}

/// The newest picture, tightly packed BGRA (`width * 4` bytes per row).
#[derive(Default)]
struct Latest {
    image: Vec<u8>,
    width: usize,
    height: usize,
    damage: Damage,
    captured_at: Option<Instant>,
    error: Option<String>,
    frames: u64,
    /// Since when the stream is paused: briefly while it starts; for good once GNOME
    /// ends the screen cast of a monitor whose size changed.
    paused_at: Option<Instant>,
}

/// One PipeWire buffer's pixels: `height` rows of `stride` bytes, 4 bytes per pixel.
struct Source<'a> {
    bytes: &'a [u8],
    stride: usize,
    width: usize,
    height: usize,
    /// RGBx/RGBA rather than BGRx/BGRA.
    swap_rb: bool,
}

impl Latest {
    /// Copy the changed parts of a buffer (`damage` in buffer pixels; `None` = all).
    fn ingest(&mut self, src: &Source<'_>, damage: Option<&[Rect]>) -> Result<(), String> {
        let (w, h) = (src.width, src.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let row = w * 4;
        let stride = if src.stride >= row { src.stride } else { row };
        let needed = stride * (h - 1) + row;
        if src.bytes.len() < needed {
            return Err(format!(
                "buffer of {} bytes is too small for {w}x{h} with stride {stride}",
                src.bytes.len()
            ));
        }
        let resized = (w, h) != (self.width, self.height);
        if resized {
            (self.width, self.height) = (w, h);
            self.image = vec![0; row * h];
        }
        let whole = [Rect { x: 0, y: 0, w, h }];
        let clipped: Vec<Rect> = match damage {
            Some(d) if !resized => d.iter().filter_map(|r| clip(*r, w, h)).collect(),
            _ => whole.to_vec(),
        };
        for r in &clipped {
            for y in r.y..r.y + r.h {
                let s = &src.bytes[y * stride + r.x * 4..y * stride + (r.x + r.w) * 4];
                let d = &mut self.image[y * row + r.x * 4..y * row + (r.x + r.w) * 4];
                d.copy_from_slice(s);
                if src.swap_rb {
                    d.as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .for_each(|px| px.swap(0, 2));
                }
            }
        }
        if resized || damage.is_none() {
            self.damage.add(None);
        } else if !clipped.is_empty() {
            self.damage.add(Some(&clipped));
        }
        self.captured_at = Some(Instant::now());
        self.frames += 1;
        Ok(())
    }
}

/// `r` clipped to `w` x `h`; `None` if nothing is left.
fn clip(r: Rect, w: usize, h: usize) -> Option<Rect> {
    let (x1, y1) = ((r.x + r.w).min(w), (r.y + r.h).min(h));
    (x1 > r.x && y1 > r.y).then(|| Rect {
        x: r.x,
        y: r.y,
        w: x1 - r.x,
        h: y1 - r.y,
    })
}

struct Shared {
    latest: Mutex<Latest>,
    changed: Condvar,
}

impl Shared {
    fn fail(&self, msg: String) {
        let mut l = self.latest.lock().expect("latest frame");
        if l.error.is_none() {
            tracing::warn!(error = %msg, "PipeWire screen stream failed");
            l.error = Some(msg);
        }
        self.changed.notify_all();
    }
}

/// A PipeWire video stream consumed on its own thread (PipeWire objects are not `Send`).
struct PwStream {
    shared: Arc<Shared>,
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PwStream {
    /// Connect to `node` via the portal's PipeWire connection `fd` (or, with `None`, the
    /// user's default PipeWire daemon: tests).
    fn start(fd: Option<OwnedFd>, node: Option<u32>) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest::default()),
            changed: Condvar::new(),
        });
        let (quit, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rconn-pipewire".into())
            .spawn(move || {
                if let Err(e) = pw_main(fd, node, &s, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e.clone()));
                    s.fail(e);
                }
            })
            .map_err(|e| backend("PipeWire thread", e))?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(backend("PipeWire", e)),
            Err(_) => return Err(backend("PipeWire", "the stream did not connect")),
        }
        Ok(Self {
            shared,
            quit,
            thread: Some(thread),
        })
    }
}

impl Drop for PwStream {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The negotiated format.
#[derive(Default)]
struct Negotiated {
    width: usize,
    height: usize,
    swap_rb: bool,
    warned_unmapped: bool,
}

fn serialize(obj: spa::pod::Object) -> Result<Vec<u8>, String> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .map_err(|e| format!("pod: {e:?}"))
}

/// Formats we take: raw 32-bit BGR/RGB, any size, variable frame rate up to 60 fps.
fn format_pod() -> Result<Vec<u8>, String> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::utils::{Fraction, Rectangle};
    serialize(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        // Compositors send 0/1 (variable: only on change); fixed-rate sources work too.
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 360, denom: 1 }
        ),
        spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction { num: 60, denom: 1 }
        ),
    ))
}

/// Shared-memory buffers only, plus up to 16 damage rectangles per buffer.
fn buffer_pods() -> Result<[Vec<u8>; 2], String> {
    use spa::sys;
    let data_types = (1i32 << sys::SPA_DATA_MemFd) | (1i32 << sys::SPA_DATA_MemPtr);
    let buffers = serialize(spa::pod::Object {
        type_: sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(
            sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int(data_types),
        )],
    })?;
    let region = std::mem::size_of::<sys::spa_meta_region>() as i32;
    let meta = serialize(spa::pod::Object {
        type_: sys::SPA_TYPE_OBJECT_ParamMeta,
        id: sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(sys::SPA_META_VideoDamage)),
            ),
            Property::new(
                sys::SPA_PARAM_META_size,
                Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range {
                        default: region * 16,
                        min: region,
                        max: region * 16,
                    },
                ))),
            ),
        ],
    })?;
    Ok([buffers, meta])
}

fn pw_main(
    fd: Option<OwnedFd>,
    node: Option<u32>,
    shared: &Arc<Shared>,
    quit_rx: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(|e| format!("connecting to PipeWire: {e}"))?;
    let s = shared.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, msg| {
            if id == pw::core::PW_ID_CORE {
                s.fail(format!("PipeWire core error {res}: {msg}"));
            }
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "rconn-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("stream: {e}"))?;
    let [buffers, meta] = buffer_pods()?;
    let (s_state, s_frames) = (shared.clone(), shared.clone());
    let _listener = stream
        .add_local_listener_with_user_data(Negotiated::default())
        .state_changed(move |_, _, old, new| {
            tracing::debug!(?old, ?new, "PipeWire stream state");
            s_state.latest.lock().expect("latest frame").paused_at =
                matches!(new, pw::stream::StreamState::Paused).then(Instant::now);
            match new {
                pw::stream::StreamState::Error(e) => s_state.fail(format!("stream error: {e}")),
                pw::stream::StreamState::Unconnected => {
                    s_state.fail("the screen stream ended".into())
                }
                _ => {}
            }
        })
        .param_changed(move |stream, fmt, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let format = info.format();
            fmt.swap_rb = match format {
                VideoFormat::BGRx | VideoFormat::BGRA => false,
                VideoFormat::RGBx | VideoFormat::RGBA => true,
                other => {
                    tracing::warn!(?other, "unexpected screen format");
                    return;
                }
            };
            (fmt.width, fmt.height) = (info.size().width as usize, info.size().height as usize);
            tracing::info!(
                ?format,
                width = fmt.width,
                height = fmt.height,
                modifier = info.modifier(),
                "PipeWire screen format"
            );
            let (Some(b), Some(m)) = (Pod::from_bytes(&buffers), Pod::from_bytes(&meta)) else {
                return;
            };
            if let Err(e) = stream.update_params(&mut [b, m]) {
                tracing::warn!(error = %e, "PipeWire buffer parameters refused");
            }
        })
        .process(move |stream, fmt| on_process(stream, fmt, &s_frames))
        .register()
        .map_err(|e| format!("stream listener: {e}"))?;
    let format = format_pod()?;
    let mut params = [Pod::from_bytes(&format).ok_or("format pod")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            node,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connecting the stream to node {node:?}: {e}"))?;
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |()| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// Take the newest queued buffer (older ones only add their damage).
fn on_process(stream: &pw::stream::Stream, fmt: &mut Negotiated, shared: &Shared) {
    let mut rects: Vec<Rect> = Vec::new();
    let mut whole = false;
    let mut newest = None;
    while let Some(buf) = stream.dequeue_buffer() {
        match buf.find_meta::<MetaVideoDamage>() {
            Some(meta) => {
                let before = rects.len();
                rects.extend(meta.iter().map(|r| Rect {
                    x: r.position().x.max(0) as usize,
                    y: r.position().y.max(0) as usize,
                    w: r.size().width as usize,
                    h: r.size().height as usize,
                }));
                // Damage metadata without a valid region: no information, assume all.
                whole |= rects.len() == before;
            }
            None => whole = true,
        }
        newest = Some(buf); // drops (requeues) the previous one
    }
    let Some(mut buf) = newest else { return };
    let (w, h) = (fmt.width, fmt.height);
    let Some(data) = buf.datas_mut().first_mut() else {
        return;
    };
    let chunk = data.chunk();
    let (offset, size, stride) = (
        chunk.offset() as usize,
        chunk.size() as usize,
        chunk.stride().max(0) as usize,
    );
    let corrupted = chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED);
    if size == 0 || corrupted {
        return; // a cursor-only or empty update
    }
    let kind = data.type_();
    let Some(bytes) = data.data() else {
        if !fmt.warned_unmapped {
            fmt.warned_unmapped = true;
            tracing::warn!(?kind, "unmappable screen buffers (DMA-BUF?); no picture");
        }
        return;
    };
    let start = offset.min(bytes.len());
    let end = start.saturating_add(size).min(bytes.len());
    let src = Source {
        bytes: &bytes[start..end],
        stride,
        width: w,
        height: h,
        swap_rb: fmt.swap_rb,
    };
    let mut l = shared.latest.lock().expect("latest frame");
    let damage = (!whole).then_some(rects.as_slice());
    match l.ingest(&src, damage) {
        Ok(()) => shared.changed.notify_all(),
        Err(e) => tracing::debug!(error = %e, "skipped a screen buffer"),
    }
}

/// The Wayland screen via the desktop portal (or, for tests, any PipeWire video node).
pub struct PortalCapturer {
    pw: PwStream,
    /// Keeps the portal session (and its stream) alive; `None` for a plain node.
    lease: Option<Lease>,
    want_full: bool,
}

impl PortalCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        let lease = portal::acquire(ACQUIRE_TIMEOUT).map_err(|e| backend("capture", e))?;
        let stream = lease.stream();
        let fd = lease.open_pipewire().map_err(|e| backend("capture", e))?;
        let pw = PwStream::start(Some(fd), Some(stream.node_id))?;
        tracing::info!(node = stream.node_id, size = ?stream.size, "Wayland capture via the desktop portal");
        Ok(Self {
            pw,
            lease: Some(lease),
            want_full: true,
        })
    }

    /// Capture PipeWire node `node` from the default PipeWire daemon, without a portal.
    pub fn from_node(node: u32) -> Result<Self, CaptureError> {
        Ok(Self {
            pw: PwStream::start(None, Some(node))?,
            lease: None,
            want_full: true,
        })
    }

    /// Wait up to `timeout` for a change; the lock is held on return.
    fn wait(&self, timeout: Duration) -> Result<std::sync::MutexGuard<'_, Latest>, CaptureError> {
        let deadline = Instant::now() + timeout;
        let mut l = self.pw.shared.latest.lock().expect("latest frame");
        loop {
            if let Some(e) = &l.error {
                return Err(backend("capture", e));
            }
            if self.lease.as_ref().is_some_and(Lease::is_closed) {
                return Err(backend("capture", "the portal session was closed"));
            }
            // GNOME stops the screen cast of a monitor whose size changed; a fresh session
            // streams the new size.
            if let Some(lease) = &self.lease
                && l.paused_at.is_some_and(|t| t.elapsed() >= STOPPED_AFTER)
                && portal::screen_changing()
            {
                lease.renew();
                return Err(backend(
                    "capture",
                    "the screen stream stopped (new screen size)",
                ));
            }
            let have = l.width > 0 && (l.damage != Damage::None || self.want_full);
            if have {
                return Ok(l);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(l);
            }
            // Wake now and then to notice a closed session.
            let step = (deadline - now).min(Duration::from_millis(250));
            l = self
                .pw
                .shared
                .changed
                .wait_timeout(l, step)
                .expect("latest frame")
                .0;
        }
    }

    /// Pointer positions follow the screen's size (it changes when fitted to a viewer).
    fn note_size(&self, width: usize, height: usize) {
        if let Some(lease) = &self.lease {
            lease.picture_size(width, height);
        }
    }
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let want_full = self.want_full;
        let mut l = self.wait(timeout)?;
        if l.width == 0 || (l.damage == Damage::None && !want_full) {
            return Ok(None);
        }
        l.damage = Damage::None;
        self.note_size(l.width, l.height);
        let frame = Frame {
            width: l.width,
            height: l.height,
            bgra: l.image.clone(),
            captured_at: l.captured_at.unwrap_or_else(Instant::now),
        };
        drop(l);
        self.want_full = false;
        Ok(Some(frame))
    }

    fn next_into(
        &mut self,
        timeout: Duration,
        picture: &mut Option<I420>,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        let want_full = self.want_full;
        let mut l = self.wait(timeout)?;
        if l.width == 0 || (l.damage == Damage::None && !want_full) {
            return Ok(None);
        }
        let (w, h) = (l.width, l.height);
        self.note_size(w, h);
        let (ew, eh) = (w & !1, h & !1);
        if ew == 0 || eh == 0 {
            return Ok(None);
        }
        let damage = std::mem::take(&mut l.damage);
        let captured_at = l.captured_at.unwrap_or_else(Instant::now);
        if w > max.0 || h > max.1 {
            // Halved to fit: no partial updates on this path.
            let frame = Frame {
                width: w,
                height: h,
                bgra: l.image.clone(),
                captured_at,
            };
            drop(l);
            self.want_full = false;
            return Ok(convert_whole(frame, picture, max));
        }
        let whole = Rect {
            x: 0,
            y: 0,
            w: ew,
            h: eh,
        };
        let fresh = want_full || picture.as_ref().is_none_or(|p| p.dimensions() != (ew, eh));
        let rects: Vec<Rect> = match damage {
            _ if fresh => vec![whole],
            Damage::Whole => vec![whole],
            Damage::None => Vec::new(),
            Damage::Rects(v) => v
                .into_iter()
                .filter_map(|r| r.even_within(ew, eh))
                .collect(),
        };
        if rects.is_empty() {
            return Ok(None);
        }
        let pic = match picture {
            Some(p) if p.dimensions() == (ew, eh) => p,
            _ => picture.insert(I420::new(ew, eh)),
        };
        pic.fill_rects_from_bgra(&l.image, w * 4, &rects);
        drop(l);
        self.want_full = false;
        let area: usize = rects.iter().map(|r| r.w * r.h).sum();
        let top = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap_or(eh);
        Ok(Some(Captured {
            rows: (top, bottom),
            changed: (area as f32 / (ew * eh) as f32).min(1.0),
            captured_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` x `h` source with `pad` bytes after each row; pixel (x, y) = [x, y, 7, 255]
    /// in BGRA order (or that order with R/B swapped for RGB sources).
    fn padded(w: usize, h: usize, pad: usize) -> (Vec<u8>, usize) {
        let stride = w * 4 + pad;
        let mut v = vec![0xEE; stride * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * stride + x * 4;
                v[i..i + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        (v, stride)
    }

    fn px(l: &Latest, x: usize, y: usize) -> [u8; 4] {
        let i = (y * l.width + x) * 4;
        l.image[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn padded_rows_are_packed() {
        let (bytes, stride) = padded(10, 6, 24);
        let mut l = Latest::default();
        let src = Source {
            bytes: &bytes,
            stride,
            width: 10,
            height: 6,
            swap_rb: false,
        };
        l.ingest(&src, None).unwrap();
        assert_eq!(l.image.len(), 10 * 6 * 4);
        assert_eq!(px(&l, 0, 0), [0, 0, 7, 255]);
        assert_eq!(px(&l, 9, 5), [9, 5, 7, 255]);
        assert!(!l.image.contains(&0xEE), "padding leaked into the picture");
        assert_eq!(l.damage, Damage::Whole);
    }

    #[test]
    fn last_row_may_end_without_padding() {
        // Some producers size the buffer as stride * (h - 1) + w * 4.
        let (mut bytes, stride) = padded(8, 4, 16);
        bytes.truncate(stride * 3 + 8 * 4);
        let mut l = Latest::default();
        let src = Source {
            bytes: &bytes,
            stride,
            width: 8,
            height: 4,
            swap_rb: false,
        };
        l.ingest(&src, None).unwrap();
        assert_eq!(px(&l, 7, 3), [7, 3, 7, 255]);
        // One byte short is refused instead of reading past the end.
        let short = Source {
            bytes: &bytes[..bytes.len() - 1],
            ..src
        };
        assert!(l.ingest(&short, None).is_err());
    }

    #[test]
    fn zero_stride_means_packed_and_rgb_is_swapped() {
        let (bytes, _) = padded(4, 2, 0);
        let mut l = Latest::default();
        let src = Source {
            bytes: &bytes,
            stride: 0,
            width: 4,
            height: 2,
            swap_rb: true,
        };
        l.ingest(&src, None).unwrap();
        assert_eq!(px(&l, 3, 1), [7, 1, 3, 255]);
    }

    #[test]
    fn damage_copies_only_changed_areas_and_accumulates() {
        let (w, h) = (16, 8);
        let (bytes, stride) = padded(w, h, 8);
        let mut l = Latest::default();
        let src = Source {
            bytes: &bytes,
            stride,
            width: w,
            height: h,
            swap_rb: false,
        };
        // First buffer: everything, whatever the damage says (new size).
        let r = Rect {
            x: 2,
            y: 1,
            w: 3,
            h: 2,
        };
        l.ingest(&src, Some(&[r])).unwrap();
        assert_eq!(l.damage, Damage::Whole);
        l.damage = Damage::None;

        // A changed source; only the damaged rectangle is copied.
        let changed: Vec<u8> = bytes.iter().map(|b| b.wrapping_add(1)).collect();
        let src2 = Source {
            bytes: &changed,
            ..src
        };
        l.ingest(&src2, Some(&[r])).unwrap();
        assert_eq!(px(&l, 2, 1), [3, 2, 8, 0]);
        assert_eq!(px(&l, 4, 2), [5, 3, 8, 0]);
        assert_eq!(px(&l, 5, 1), [5, 1, 7, 255], "outside the damage");
        assert_eq!(px(&l, 2, 3), [2, 3, 7, 255], "outside the damage");
        // Damage past the edge is clipped; rectangles add up until taken.
        let edge = Rect {
            x: 14,
            y: 6,
            w: 10,
            h: 10,
        };
        l.ingest(&src2, Some(&[edge])).unwrap();
        let clipped = Rect {
            x: 14,
            y: 6,
            w: 2,
            h: 2,
        };
        assert_eq!(l.damage, Damage::Rects(vec![r, clipped]));
        // Too many rectangles: whole frame.
        let many = vec![r; MAX_RECTS];
        l.ingest(&src2, Some(&many)).unwrap();
        assert_eq!(l.damage, Damage::Whole);
    }

    #[test]
    fn size_change_reallocates() {
        let mut l = Latest::default();
        let (a, sa) = padded(4, 4, 0);
        let src = Source {
            bytes: &a,
            stride: sa,
            width: 4,
            height: 4,
            swap_rb: false,
        };
        l.ingest(&src, None).unwrap();
        l.damage = Damage::None;
        let (b, sb) = padded(6, 2, 4);
        let tiny = Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        let src = Source {
            bytes: &b,
            stride: sb,
            width: 6,
            height: 2,
            swap_rb: false,
        };
        l.ingest(&src, Some(&[tiny])).unwrap();
        assert_eq!((l.width, l.height, l.image.len()), (6, 2, 48));
        assert_eq!(l.damage, Damage::Whole);
        assert_eq!(px(&l, 5, 1), [5, 1, 7, 255]);
    }

    #[test]
    fn negotiation_pods_serialize() {
        let f = format_pod().unwrap();
        assert!(Pod::from_bytes(&f).is_some());
        let [b, m] = buffer_pods().unwrap();
        assert!(Pod::from_bytes(&b).is_some() && Pod::from_bytes(&m).is_some());
    }

    /// Against a real PipeWire daemon: `RCONN_PW_TEST_NODE=<id>` of a video source node
    /// (e.g. `gst-launch-1.0 videotestsrc ! video/x-raw,format=BGRx ! pipewiresink`).
    #[test]
    fn captures_a_pipewire_node() {
        let Some(node) = std::env::var("RCONN_PW_TEST_NODE")
            .ok()
            .and_then(|n| n.parse().ok())
        else {
            eprintln!("skipped: RCONN_PW_TEST_NODE not set");
            return;
        };
        let mut cap = PortalCapturer::from_node(node).expect("PipeWire stream");
        let mut picture = None;
        let first = cap
            .next_into(Duration::from_secs(5), &mut picture, (8192, 8192))
            .unwrap()
            .expect("a first picture");
        assert_eq!(first.changed, 1.0);
        let (w, h) = picture.as_ref().unwrap().dimensions();
        eprintln!("first picture {w}x{h}");
        assert!(w > 0 && h > 0);
        // A solid red source (`PATTERN=red`): the channel order survived negotiation.
        if std::env::var_os("RCONN_PW_TEST_RED").is_some() {
            let l = cap.pw.shared.latest.lock().unwrap();
            let i = (l.height / 2 * l.width + l.width / 2) * 4;
            let bgr = &l.image[i..i + 3];
            eprintln!("centre pixel (B, G, R) = {bgr:?}");
            assert!(
                bgr[0] < 40 && bgr[1] < 40 && bgr[2] > 215,
                "{bgr:?} is not red"
            );
        }
        let mut more = 0;
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if cap
                .next_into(Duration::from_millis(200), &mut picture, (8192, 8192))
                .unwrap()
                .is_some()
            {
                more += 1;
            }
        }
        eprintln!("{more} more pictures in 2 s");
        assert!(more > 0, "a moving test source should keep sending");
    }
}
