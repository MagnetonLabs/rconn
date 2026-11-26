//! Wayland capture: the desktop portal's screen stream ([`crate::portal`]), read from
//! PipeWire. Frames arrive in shared memory (MemFd/MemPtr) as BGRx/BGRA (or RGBx/RGBA,
//! swapped here). Only the areas the compositor's `SPA_META_VideoDamage` reports are
//! copied and converted; buffers without damage metadata count as wholly changed.
//!
//! Surface mode ([`Capturer::use_surfaces`], when the VA-API encoder can convert on the
//! GPU): a new stream offers DMA-BUF formats with modifiers first (and shared memory
//! after them, per PipeWire's DMA-BUF negotiation). If the compositor picks DMA-BUF,
//! each buffer is handed on as a [`Surface`] without touching its pixels; damage still
//! drives the frame pacing. If it doesn't, the stream carries on in shared memory.

use super::{CaptureError, Captured, Capturer, convert_whole};
use crate::dmabuf::{self, DmaBuf, Plane, Surface};
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
/// How long a new stream may take to agree on a format.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(3);
/// A DMA-BUF stream with no usable frame this long after it was agreed has failed.
const FIRST_SURFACE_TIMEOUT: Duration = Duration::from_secs(5);
/// DMA-BUF buffers that can't be used, in a row, before the stream counts as failed.
const MAX_BAD_DMABUFS: u32 = 10;

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

/// How the negotiated stream delivers pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Memory,
    DmaBuf,
}

/// The newest picture, tightly packed BGRA (`width * 4` bytes per row), or (DMA-BUF
/// streams) the newest buffer.
#[derive(Default)]
struct Latest {
    image: Vec<u8>,
    /// DMA-BUF streams: the newest screen buffer (`image` stays empty).
    dmabuf: Option<Arc<DmaBuf>>,
    width: usize,
    height: usize,
    /// The negotiated format: how pictures come, and their size.
    negotiated: Option<(Kind, (usize, usize))>,
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
        let resized = (w, h) != (self.width, self.height) || self.image.len() != row * h;
        if resized {
            (self.width, self.height) = (w, h);
            self.image = vec![0; row * h];
        }
        self.dmabuf = None;
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

    /// A DMA-BUF screen buffer arrived (`damage` in buffer pixels; `None` = all): keep a
    /// reference to it, and add up what changed.
    fn note_dmabuf(&mut self, frame: DmaBuf, damage: Option<&[Rect]>) {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let resized = (w, h) != (self.width, self.height) || self.dmabuf.is_none();
        (self.width, self.height) = (w, h);
        self.image = Vec::new();
        self.dmabuf = Some(Arc::new(frame));
        match damage {
            Some(d) if !resized => {
                let clipped: Vec<Rect> = d.iter().filter_map(|r| clip(*r, w, h)).collect();
                if !clipped.is_empty() {
                    self.damage.add(Some(&clipped));
                }
            }
            _ => self.damage.add(None),
        }
        self.captured_at = Some(Instant::now());
        self.frames += 1;
    }
}

/// What a surface-mode capture changed, in the sent picture (`size`, the `w` x `h`
/// screen fitted): its even row range and share of the area.
fn surface_change(
    damage: &Damage,
    whole: bool,
    (w, h): (usize, usize),
    size: (usize, usize),
) -> ((usize, usize), f32) {
    match damage {
        _ if whole => ((0, size.1), 1.0),
        Damage::Whole => ((0, size.1), 1.0),
        Damage::None => ((0, 0), 0.0),
        Damage::Rects(v) => {
            let area: usize = v.iter().map(|r| r.w * r.h).sum();
            let top = v.iter().map(|r| r.y).min().unwrap_or(0);
            let bottom = v.iter().map(|r| r.y + r.h).max().unwrap_or(h);
            (
                dmabuf::scale_rows((top, bottom), h, size.1),
                (area as f32 / (w * h).max(1) as f32).min(1.0),
            )
        }
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
    /// user's default PipeWire daemon: tests). With `dmabuf`, DMA-BUF formats are offered
    /// first.
    fn start(fd: Option<OwnedFd>, node: Option<u32>, dmabuf: bool) -> Result<Self, CaptureError> {
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
                if let Err(e) = pw_main(fd, node, dmabuf, &s, quit_rx, &ready_tx) {
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

impl PwStream {
    /// Disconnect the stream and end its thread.
    fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.quit.send(());
            let _ = t.join();
        }
    }
}

impl Drop for PwStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The negotiated format.
#[derive(Default)]
struct Negotiated {
    width: usize,
    height: usize,
    swap_rb: bool,
    /// DMA-BUF: the buffers' modifier and DRM format.
    dmabuf: Option<(u64, u32)>,
    /// DMA-BUF buffers in a row that could not be used.
    bad_dmabufs: u32,
    warned_unmapped: bool,
}

/// The DRM format of a 32-bit RGB PipeWire format (same bytes in memory).
fn drm_format(f: VideoFormat) -> Option<u32> {
    Some(match f {
        VideoFormat::BGRx => dmabuf::DRM_XRGB8888,
        VideoFormat::BGRA => dmabuf::DRM_ARGB8888,
        VideoFormat::RGBx => dmabuf::DRM_XBGR8888,
        VideoFormat::RGBA => dmabuf::DRM_ABGR8888,
        _ => return None,
    })
}

/// The modifier in a negotiated Format, if it has one (a DMA-BUF format). Should the
/// producer leave a choice, its default.
fn modifier_of(format: &Pod) -> Option<u64> {
    let obj = format.as_object().ok()?;
    let prop = obj.find_prop(spa::utils::Id(spa::sys::SPA_FORMAT_VIDEO_modifier))?;
    let value = prop.value();
    if value.is_long() {
        return value.get_long().ok().map(|m| m as u64);
    }
    let (_, v) =
        spa::pod::deserialize::PodDeserializer::deserialize_any_from(value.as_bytes()).ok()?;
    match v {
        Value::Long(m) => Some(m as u64),
        Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(_, choice))) => match choice {
            spa::utils::ChoiceEnum::None(m) => Some(m as u64),
            spa::utils::ChoiceEnum::Enum { default, .. } => Some(default as u64),
            _ => None,
        },
        _ => None,
    }
}

fn serialize(obj: spa::pod::Object) -> Result<Vec<u8>, String> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .map_err(|e| format!("pod: {e:?}"))
}

/// The size and frame-rate properties every offered format has: any size, variable
/// frame rate up to 60 fps.
fn common_properties() -> Vec<Property> {
    use spa::param::format::FormatProperties;
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};
    fn range<T: spa::pod::CanonicalFixedSizedPod>(default: T, min: T, max: T) -> ChoiceEnum<T> {
        ChoiceEnum::Range { default, min, max }
    }
    vec![
        Property::new(
            FormatProperties::VideoSize.as_raw(),
            Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                range(
                    Rectangle {
                        width: 1920,
                        height: 1080,
                    },
                    Rectangle {
                        width: 1,
                        height: 1,
                    },
                    Rectangle {
                        width: 16384,
                        height: 16384,
                    },
                ),
            ))),
        ),
        Property::new(
            FormatProperties::VideoFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                range(
                    Fraction { num: 30, denom: 1 },
                    Fraction { num: 0, denom: 1 },
                    Fraction { num: 360, denom: 1 },
                ),
            ))),
        ),
        Property::new(
            FormatProperties::VideoMaxFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                range(
                    Fraction { num: 60, denom: 1 },
                    Fraction { num: 1, denom: 1 },
                    Fraction { num: 60, denom: 1 },
                ),
            ))),
        ),
    ]
}

/// A DMA-BUF format: `format` in one of `modifiers` (mandatory, so a producer without
/// DMA-BUF skips it; not to be fixated by PipeWire: the producer picks one it can
/// allocate).
fn dmabuf_format_pod(format: VideoFormat, modifiers: &[u64]) -> Result<Vec<u8>, String> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::{ChoiceValue, PropertyFlags};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};
    let first = *modifiers.first().ok_or("no modifiers")? as i64;
    let mut properties = vec![
        Property::new(
            FormatProperties::MediaType.as_raw(),
            Value::Id(Id(MediaType::Video.as_raw())),
        ),
        Property::new(
            FormatProperties::MediaSubtype.as_raw(),
            Value::Id(Id(MediaSubtype::Raw.as_raw())),
        ),
        Property::new(
            FormatProperties::VideoFormat.as_raw(),
            Value::Id(Id(format.as_raw())),
        ),
    ];
    properties.extend(common_properties());
    properties.push(Property {
        key: spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: PropertyFlags::from_bits_retain(
            spa::sys::SPA_POD_PROP_FLAG_MANDATORY | spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE,
        ),
        value: Value::Choice(ChoiceValue::Long(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: first,
                alternatives: modifiers.iter().map(|&m| m as i64).collect(),
            },
        ))),
    });
    serialize(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties,
    })
}

/// The formats offered, in order of preference: with `dmabuf`, the 32-bit RGB formats as
/// DMA-BUFs with our modifiers, then shared memory.
fn format_pods(dmabuf: bool) -> Result<Vec<Vec<u8>>, String> {
    let mut v = Vec::new();
    if dmabuf {
        let modifiers = dmabuf::offered_modifiers();
        for f in [
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ] {
            v.push(dmabuf_format_pod(f, &modifiers)?);
        }
    }
    v.push(format_pod()?);
    Ok(v)
}

/// Formats we take in shared memory: raw 32-bit BGR/RGB, any size, variable frame rate
/// up to 60 fps.
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

/// Shared-memory buffers (or, for a DMA-BUF format, DMA-BUFs), plus up to 16 damage
/// rectangles per buffer.
fn buffer_pods(dmabuf: bool) -> Result<[Vec<u8>; 2], String> {
    use spa::sys;
    let data_types = if dmabuf {
        1i32 << sys::SPA_DATA_DmaBuf
    } else {
        (1i32 << sys::SPA_DATA_MemFd) | (1i32 << sys::SPA_DATA_MemPtr)
    };
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
    dmabuf: bool,
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
    let (s_state, s_format, s_frames) = (shared.clone(), shared.clone(), shared.clone());
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
            fmt.dmabuf = modifier_of(param).zip(drm_format(format));
            tracing::info!(
                ?format,
                width = fmt.width,
                height = fmt.height,
                dmabuf = fmt.dmabuf.is_some(),
                modifier = format_args!("{:#x}", fmt.dmabuf.map_or(0, |d| d.0)),
                "PipeWire screen format"
            );
            {
                let mut l = s_format.latest.lock().expect("latest frame");
                let kind = if fmt.dmabuf.is_some() {
                    Kind::DmaBuf
                } else {
                    Kind::Memory
                };
                l.negotiated = Some((kind, (fmt.width, fmt.height)));
                s_format.changed.notify_all();
            }
            let Ok([buffers, meta]) = buffer_pods(fmt.dmabuf.is_some()) else {
                return;
            };
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
    let formats = format_pods(dmabuf)?;
    let mut params = formats
        .iter()
        .map(|f| Pod::from_bytes(f).ok_or("format pod"))
        .collect::<Result<Vec<_>, _>>()?;
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
    if kind == spa::buffer::DataType::DmaBuf {
        let damage = (!whole).then_some(rects.as_slice());
        match dmabuf_frame(buf.datas_mut(), fmt) {
            Ok(frame) => {
                fmt.bad_dmabufs = 0;
                let mut l = shared.latest.lock().expect("latest frame");
                l.note_dmabuf(frame, damage);
                shared.changed.notify_all();
            }
            Err(e) => {
                fmt.bad_dmabufs += 1;
                if fmt.bad_dmabufs == MAX_BAD_DMABUFS {
                    // Memory capture from the next capture on.
                    let why = format!("DMA-BUF screen buffers unusable: {e}");
                    crate::vaapi::zero_copy_failed(&why);
                    shared.fail(why);
                } else {
                    tracing::debug!(error = %e, "skipped a DMA-BUF screen buffer");
                }
            }
        }
        return;
    }
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

/// Our references to a DMA-BUF buffer's planes (one per PipeWire data).
fn dmabuf_frame(datas: &[spa::buffer::Data], fmt: &Negotiated) -> Result<DmaBuf, String> {
    let (modifier, drm_format) = fmt
        .dmabuf
        .ok_or("a DMA-BUF in a stream negotiated for memory")?;
    let planes = datas
        .iter()
        .filter(|d| d.type_() == spa::buffer::DataType::DmaBuf)
        .map(|d| {
            let c = d.chunk();
            Plane::from_raw(
                d.fd() as std::os::fd::RawFd,
                c.offset(),
                c.stride().max(0) as u32,
                d.as_raw().maxsize,
            )
            .map_err(|e| format!("DMA-BUF fd: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if planes.is_empty() {
        return Err("a buffer without planes".into());
    }
    Ok(DmaBuf {
        width: fmt.width as u32,
        height: fmt.height as u32,
        drm_format,
        modifier,
        planes,
    })
}

/// The Wayland screen via the desktop portal (or, for tests, any PipeWire video node).
pub struct PortalCapturer {
    pw: PwStream,
    /// Keeps the portal session (and its stream) alive; `None` for a plain node.
    lease: Option<Lease>,
    node: u32,
    want_full: bool,
    /// Surface mode: DMA-BUF screens, sent within this size.
    surfaces: Option<(usize, usize)>,
    /// Surface mode: when it began, and whether a DMA-BUF frame has come since.
    surfaces_since: Option<Instant>,
    got_surface: bool,
}

impl PortalCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        let lease = portal::acquire(ACQUIRE_TIMEOUT).map_err(|e| backend("capture", e))?;
        let stream = lease.stream();
        let fd = lease.open_pipewire().map_err(|e| backend("capture", e))?;
        let pw = PwStream::start(Some(fd), Some(stream.node_id), false)?;
        tracing::info!(node = stream.node_id, size = ?stream.size, "Wayland capture via the desktop portal");
        Ok(Self {
            pw,
            lease: Some(lease),
            node: stream.node_id,
            want_full: true,
            surfaces: None,
            surfaces_since: None,
            got_surface: false,
        })
    }

    /// Capture PipeWire node `node` from the default PipeWire daemon, without a portal.
    pub fn from_node(node: u32) -> Result<Self, CaptureError> {
        Ok(Self {
            pw: PwStream::start(None, Some(node), false)?,
            lease: None,
            node,
            want_full: true,
            surfaces: None,
            surfaces_since: None,
            got_surface: false,
        })
    }

    /// A new stream to the same screen (offering DMA-BUFs or not).
    fn restart(&mut self, dmabuf: bool) -> Result<(), CaptureError> {
        let fd = match &self.lease {
            Some(l) => Some(l.open_pipewire().map_err(|e| backend("capture", e))?),
            None => None,
        };
        // The old stream goes first: while it is linked, the compositor would also have
        // to serve its formats.
        self.pw.stop();
        match PwStream::start(fd, Some(self.node), dmabuf) {
            Ok(pw) => self.pw = pw,
            Err(e) => {
                // No stream now: the next wait fails, and the capture restarts.
                self.pw.shared.fail(e.to_string());
                return Err(e);
            }
        }
        self.want_full = true;
        Ok(())
    }

    /// Wait up to `timeout` for the stream's format.
    fn negotiated(&self, timeout: Duration) -> Option<(Kind, (usize, usize))> {
        let deadline = Instant::now() + timeout;
        let mut l = self.pw.shared.latest.lock().expect("latest frame");
        loop {
            if l.negotiated.is_some() || l.error.is_some() {
                return l.negotiated;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            l = self
                .pw
                .shared
                .changed
                .wait_timeout(l, deadline - now)
                .expect("latest frame")
                .0;
        }
    }

    /// [`Capturer::use_surfaces`]: switch to a stream of DMA-BUFs if the video processor
    /// converts this screen's size with the right colours and the compositor offers them.
    fn try_surfaces(&mut self, max: (usize, usize)) -> Result<(), String> {
        let (_, (w, h)) = self
            .negotiated(NEGOTIATE_TIMEOUT)
            .ok_or("the screen stream has no format yet")?;
        crate::vaapi::check_zero_copy(dmabuf::fitted(w, h, max))?;
        self.restart(true).map_err(|e| e.to_string())?;
        match self.negotiated(NEGOTIATE_TIMEOUT) {
            Some((Kind::DmaBuf, _)) => Ok(()),
            Some((Kind::Memory, _)) => Err("the compositor offered no DMA-BUF".into()),
            None => {
                // No format at all: back to the plain stream.
                self.restart(false).map_err(|e| e.to_string())?;
                Err("no format agreed for DMA-BUF capture".into())
            }
        }
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

/// Memory mode met a DMA-BUF stream (it never asks for one).
fn no_pixels() -> CaptureError {
    backend("capture", "DMA-BUF screen buffers outside surface mode")
}

impl Capturer for PortalCapturer {
    fn use_surfaces(&mut self, max: (usize, usize)) -> bool {
        if !crate::vaapi::zero_copy_allowed() {
            return false;
        }
        let t = Instant::now();
        match self.try_surfaces(max) {
            Ok(()) => {
                tracing::info!(
                    setup_ms = t.elapsed().as_millis() as u64,
                    "frames stay on the GPU (DMA-BUF capture, VA-API convert and encode)"
                );
                self.surfaces = Some(max);
                self.surfaces_since = Some(Instant::now());
                self.got_surface = false;
                true
            }
            Err(e) => {
                crate::vaapi::zero_copy_failed(&e);
                false
            }
        }
    }

    fn next_surface(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(Surface, Captured)>, CaptureError> {
        let max = self
            .surfaces
            .ok_or_else(|| backend("capture", "not in surface mode"))?;
        let want_full = self.want_full;
        let first = !self.got_surface;
        let mut l = match self.wait(timeout) {
            Ok(l) => l,
            Err(e) => {
                if first {
                    // The compositor agreed to DMA-BUFs but could not deliver them.
                    crate::vaapi::zero_copy_failed(&format!("before the first DMA-BUF: {e}"));
                }
                return Err(e);
            }
        };
        if l.width == 0 || (l.damage == Damage::None && !want_full) {
            if first
                && self
                    .surfaces_since
                    .is_some_and(|t| t.elapsed() > FIRST_SURFACE_TIMEOUT)
            {
                drop(l);
                crate::vaapi::zero_copy_failed("no DMA-BUF frame arrived");
                return Err(backend("capture", "no DMA-BUF frame arrived"));
            }
            return Ok(None);
        }
        let Some(frame) = l.dmabuf.clone() else {
            // The compositor went back to shared memory (a renegotiation).
            drop(l);
            crate::vaapi::zero_copy_failed("the screen stream switched to memory buffers");
            return Err(backend(
                "capture",
                "the screen stream switched to memory buffers",
            ));
        };
        let (w, h) = (l.width, l.height);
        let size = dmabuf::fitted(w, h, max);
        if size.0 == 0 || size.1 == 0 {
            return Ok(None);
        }
        let damage = std::mem::take(&mut l.damage);
        let captured_at = l.captured_at.unwrap_or_else(Instant::now);
        drop(l);
        self.note_size(w, h);
        self.want_full = false;
        self.got_surface = true;
        let (rows, changed) = surface_change(&damage, want_full, (w, h), size);
        Ok(Some((
            Surface { frame, size },
            Captured {
                rows,
                changed,
                captured_at,
            },
        )))
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let want_full = self.want_full;
        let mut l = self.wait(timeout)?;
        if l.width == 0 || (l.damage == Damage::None && !want_full) {
            return Ok(None);
        }
        if l.image.len() < l.width * l.height * 4 {
            return Err(no_pixels());
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
        if l.image.len() < w * h * 4 {
            return Err(no_pixels());
        }
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
        for dmabuf in [false, true] {
            let [b, m] = buffer_pods(dmabuf).unwrap();
            assert!(Pod::from_bytes(&b).is_some() && Pod::from_bytes(&m).is_some());
        }
    }

    fn data_types(buffers: &[u8]) -> i32 {
        let pod = Pod::from_bytes(buffers).unwrap();
        let obj = pod.as_object().unwrap();
        let p = obj
            .find_prop(spa::utils::Id(spa::sys::SPA_PARAM_BUFFERS_dataType))
            .unwrap();
        p.value().get_int().unwrap()
    }

    #[test]
    fn buffers_follow_the_negotiated_kind() {
        use spa::sys::{SPA_DATA_DmaBuf, SPA_DATA_MemFd, SPA_DATA_MemPtr};
        let [memory, _] = buffer_pods(false).unwrap();
        assert_eq!(
            data_types(&memory),
            (1 << SPA_DATA_MemFd) | (1 << SPA_DATA_MemPtr)
        );
        let [gpu, _] = buffer_pods(true).unwrap();
        assert_eq!(data_types(&gpu), 1 << SPA_DATA_DmaBuf);
    }

    #[test]
    fn dmabuf_formats_come_first_with_mandatory_modifiers() {
        assert_eq!(format_pods(false).unwrap().len(), 1, "memory only");
        let pods = format_pods(true).unwrap();
        assert_eq!(pods.len(), 5, "four DMA-BUF formats, then memory");
        let key = spa::utils::Id(spa::sys::SPA_FORMAT_VIDEO_modifier);
        for (i, bytes) in pods.iter().enumerate() {
            let pod = Pod::from_bytes(bytes).unwrap();
            let obj = pod.as_object().unwrap();
            match obj.find_prop(key) {
                Some(p) if i < 4 => {
                    let want = spa::sys::SPA_POD_PROP_FLAG_MANDATORY
                        | spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE;
                    assert_eq!(p.flags().bits() & want, want);
                    assert_eq!(modifier_of(pod), Some(dmabuf::MOD_LINEAR));
                }
                None if i == 4 => assert_eq!(modifier_of(pod), None),
                other => panic!("format {i}: modifier {:?}", other.map(|p| p.flags())),
            }
        }
    }

    #[test]
    fn a_fixated_modifier_is_read() {
        use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        let bytes = serialize(spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::Format,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
            spa::pod::property!(FormatProperties::VideoModifier, Long, 0x0100_0000_0000_0002),
        ))
        .unwrap();
        assert_eq!(
            modifier_of(Pod::from_bytes(&bytes).unwrap()),
            Some(0x0100_0000_0000_0002)
        );
    }

    #[test]
    fn rgb_formats_map_to_drm_formats() {
        assert_eq!(drm_format(VideoFormat::BGRx), Some(dmabuf::DRM_XRGB8888));
        assert_eq!(drm_format(VideoFormat::BGRA), Some(dmabuf::DRM_ARGB8888));
        assert_eq!(drm_format(VideoFormat::RGBx), Some(dmabuf::DRM_XBGR8888));
        assert_eq!(drm_format(VideoFormat::RGBA), Some(dmabuf::DRM_ABGR8888));
        assert_eq!(drm_format(VideoFormat::NV12), None);
    }

    fn fake_dmabuf(w: u32, h: u32) -> DmaBuf {
        DmaBuf {
            width: w,
            height: h,
            drm_format: dmabuf::DRM_XRGB8888,
            modifier: dmabuf::MOD_LINEAR,
            planes: Vec::new(),
        }
    }

    #[test]
    fn dmabuf_damage_accumulates_without_copying() {
        let mut l = Latest::default();
        let r = Rect {
            x: 8,
            y: 4,
            w: 16,
            h: 8,
        };
        // First buffer: everything.
        l.note_dmabuf(fake_dmabuf(64, 32), Some(&[r]));
        assert_eq!(l.damage, Damage::Whole);
        assert!(l.image.is_empty() && l.dmabuf.is_some());
        l.damage = Damage::None;
        l.note_dmabuf(fake_dmabuf(64, 32), Some(&[r]));
        l.note_dmabuf(fake_dmabuf(64, 32), None);
        assert_eq!(l.damage, Damage::Whole, "no damage metadata: all");
        l.damage = Damage::None;
        let edge = Rect {
            x: 60,
            y: 30,
            w: 10,
            h: 10,
        };
        l.note_dmabuf(fake_dmabuf(64, 32), Some(&[r, edge]));
        let clipped = Rect {
            x: 60,
            y: 30,
            w: 4,
            h: 2,
        };
        assert_eq!(l.damage, Damage::Rects(vec![r, clipped]));
        // A new size: all.
        l.note_dmabuf(fake_dmabuf(32, 32), Some(&[r]));
        assert_eq!(l.damage, Damage::Whole);
        // A memory buffer after DMA-BUFs: the reference goes.
        let (bytes, stride) = padded(4, 4, 0);
        let src = Source {
            bytes: &bytes,
            stride,
            width: 4,
            height: 4,
            swap_rb: false,
        };
        l.ingest(&src, None).unwrap();
        assert!(l.dmabuf.is_none() && l.image.len() == 64);
    }

    #[test]
    fn surface_changes_are_in_sent_rows() {
        let r = Rect {
            x: 0,
            y: 100,
            w: 400,
            h: 101,
        };
        let full = (3840, 2160);
        let sent = (1920, 1080);
        assert_eq!(
            surface_change(&Damage::Rects(vec![r]), false, full, sent),
            ((50, 102), 400.0 * 101.0 / (3840.0 * 2160.0))
        );
        assert_eq!(
            surface_change(&Damage::None, true, full, sent),
            ((0, 1080), 1.0)
        );
        assert_eq!(
            surface_change(&Damage::Whole, false, full, sent),
            ((0, 1080), 1.0)
        );
        assert_eq!(
            surface_change(&Damage::None, false, full, sent),
            ((0, 0), 0.0)
        );
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

    /// As above, with DMA-BUF formats offered first: GStreamer's `pipewiresink` has no
    /// DMA-BUFs, so the stream must settle on shared memory and keep delivering.
    #[test]
    fn captures_a_pipewire_node_offering_dmabuf() {
        let Some(node) = std::env::var("RCONN_PW_TEST_NODE")
            .ok()
            .and_then(|n| n.parse().ok())
        else {
            eprintln!("skipped: RCONN_PW_TEST_NODE not set");
            return;
        };
        let mut cap = PortalCapturer::from_node(node).expect("PipeWire stream");
        assert_eq!(
            cap.negotiated(Duration::from_secs(5)).map(|n| n.0),
            Some(Kind::Memory)
        );
        cap.restart(true).expect("a stream offering DMA-BUF");
        let negotiated = cap.negotiated(Duration::from_secs(5));
        eprintln!("negotiated with DMA-BUF offered: {negotiated:?}");
        assert_eq!(negotiated.map(|n| n.0), Some(Kind::Memory));
        let mut picture = None;
        let first = cap
            .next_into(Duration::from_secs(5), &mut picture, (8192, 8192))
            .unwrap();
        assert!(first.is_some(), "pictures keep coming in shared memory");
    }
}
