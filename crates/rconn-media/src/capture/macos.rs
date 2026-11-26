//! ScreenCaptureKit capture of the main display.
//!
//! Unsafe is confined to this module: it is Objective-C/CoreFoundation FFI into
//! ScreenCaptureKit, CoreMedia and CoreVideo. objc2 wraps objects in ref-counted
//! `Retained`/`CFRetained` types, every fallible call is checked, and the only raw memory
//! access is the locked CVPixelBuffer, read strictly within `bytesPerRow * height`.
//!
//! SCStream delivers frames on its own dispatch queue to [`StreamOutput`], which copies
//! each complete frame into a one-slot mailbox; [`MacCapturer::next_frame`] waits on it.
//! In surface mode ([`Capturer::use_surfaces`], for the VideoToolbox encoder) nothing is
//! copied: ScreenCaptureKit converts to NV12 and scales on the GPU, and the mailbox holds
//! its IOSurface-backed pixel buffer, which the encoder reads directly.
//! The process needs the Screen Recording permission (TCC); without it ScreenCaptureKit
//! returns an error, reported as [`CaptureError::Backend`].
#![allow(unsafe_code)]

use super::{CaptureError, Captured, Capturer};
use crate::video::Frame;
use crate::yuv::I420;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID,
    kCGDisplayStreamYCbCrMatrix_ITU_R_601_4,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
#[cfg(test)]
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

/// How long to wait for ScreenCaptureKit's completion handlers. The first call can show a
/// permission prompt, which returns an error right away rather than blocking.
const SCK_TIMEOUT: Duration = Duration::from_secs(10);

const PERMISSION_HINT: &str = "grant Screen Recording permission in System Settings > \
     Privacy & Security > Screen & System Audio Recording, then restart the agent";

/// Latest captured frame (or surface), or the error that stopped the stream.
#[derive(Default)]
struct Slot {
    frame: Option<Frame>,
    surface: Option<Surface>,
    stopped: Option<String>,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
    /// Hand over ScreenCaptureKit's pixel buffers instead of copies.
    surfaces: AtomicBool,
}

/// A captured picture still on the GPU: ScreenCaptureKit's IOSurface-backed pixel buffer
/// (NV12 or BGRA), handed to a hardware encoder without a copy.
pub struct Surface {
    pb: CFRetained<CVPixelBuffer>,
    width: usize,
    height: usize,
    pub captured_at: Instant,
}

// SAFETY: a CVPixelBuffer may be retained, released and read from any thread; nothing
// writes to it once ScreenCaptureKit has delivered it.
unsafe impl Send for Surface {}

impl Surface {
    /// The sample's pixel buffer, if it is one of the formats we read.
    fn from_sample(sample: &CMSampleBuffer) -> Option<Surface> {
        // SAFETY: `image_buffer` returns a retained pixel buffer; we read its properties.
        unsafe {
            let pb = sample.image_buffer()?;
            let format = CVPixelBufferGetPixelFormatType(&pb);
            if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                && format != kCVPixelFormatType_32BGRA
            {
                return None;
            }
            let (width, height) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
            (width >= 2 && height >= 2).then(|| Surface {
                pb,
                width,
                height,
                captured_at: Instant::now(),
            })
        }
    }

    /// Wrap a pixel buffer (NV12 video range or BGRA), retaining it. For tests.
    ///
    /// # Safety
    /// `pb` must be a valid `CVPixelBufferRef`.
    #[cfg(test)]
    pub(crate) unsafe fn from_pixel_buffer(pb: NonNull<std::ffi::c_void>) -> Surface {
        // SAFETY: the caller hands us a valid pixel buffer; we take our own reference.
        let pb = unsafe { CFRetained::retain(pb.cast::<CVPixelBuffer>()) };
        let (width, height) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
        Surface {
            pb,
            width,
            height,
            captured_at: Instant::now(),
        }
    }

    /// The pixel size; H.264 uses the even part of it.
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The `CVPixelBufferRef`, valid while `self` is.
    pub(crate) fn as_ptr(&self) -> *const std::ffi::c_void {
        CFRetained::as_ptr(&self.pb).as_ptr().cast_const().cast()
    }

    /// Convert on the CPU (for a software encoder) into `out`, (re)allocated at the even
    /// size. `false` if the buffer could not be read.
    pub(crate) fn to_i420(&self, out: &mut Option<I420>) -> bool {
        let (w, h) = (self.width & !1, self.height & !1);
        let pic = match out {
            Some(p) if p.dimensions() == (w, h) => p,
            _ => out.insert(I420::new(w, h)),
        };
        let pb = &*self.pb;
        // SAFETY: the buffer is locked read-only while we read each plane within
        // `bytes_per_row * height`.
        unsafe {
            if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly)
                != kCVReturnSuccess
            {
                return false;
            }
            let ok = if CVPixelBufferIsPlanar(pb) {
                let (y, uv) = (
                    CVPixelBufferGetBaseAddressOfPlane(pb, 0).cast::<u8>(),
                    CVPixelBufferGetBaseAddressOfPlane(pb, 1).cast::<u8>(),
                );
                let (ys, uvs) = (
                    CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
                    CVPixelBufferGetBytesPerRowOfPlane(pb, 1),
                );
                let ok = !y.is_null() && !uv.is_null() && ys >= w && uvs >= w;
                if ok {
                    pic.fill_from_nv12(
                        (
                            std::slice::from_raw_parts(y, ys * self.height),
                            std::slice::from_raw_parts(uv, uvs * self.height.div_ceil(2)),
                        ),
                        (ys, uvs),
                    );
                }
                ok
            } else {
                let base = CVPixelBufferGetBaseAddress(pb).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRow(pb);
                let ok = !base.is_null() && stride >= w * 4 && stride.is_multiple_of(4);
                if ok {
                    let src = std::slice::from_raw_parts(base, stride * self.height);
                    pic.fill_from_bgra(src, stride / 4);
                }
                ok
            };
            CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
            ok
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        // A poisoned lock only means a previous holder panicked; the slot is still valid.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and StreamOutput does not
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "RconnStreamOutput"]
    #[ivars = Arc<Shared>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen || !is_complete(sample) {
                return;
            }
            let shared = self.ivars();
            if shared.surfaces.load(Ordering::Relaxed) {
                if let Some(surface) = Surface::from_sample(sample) {
                    shared.lock().surface = Some(surface);
                    shared.ready.notify_all();
                }
                return;
            }
            // Reuse the buffer of a frame the consumer never took.
            let reuse = shared.lock().frame.take().map(|f| f.bgra);
            if let Some(frame) = copy_frame(sample, reuse.unwrap_or_default()) {
                shared.lock().frame = Some(frame);
                shared.ready.notify_all();
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let shared = self.ivars();
            shared.lock().stopped = Some(format!("capture stopped: {}", describe(error)));
            shared.ready.notify_all();
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: plain NSObject init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// `true` if the sample's SCStreamFrameInfoStatus is Complete (new content). Idle and
/// blank frames, and samples without the attachment, are skipped.
fn is_complete(sample: &CMSampleBuffer) -> bool {
    // SAFETY: the attachments array belongs to `sample` and holds CFDictionaries; we only
    // read it while `sample` is alive. The status value is a CFNumber (NSNumber).
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else {
            return false;
        };
        if attachments.count() < 1 {
            return false;
        }
        let dict = attachments.value_at_index(0).cast::<CFDictionary>();
        let Some(dict) = dict.as_ref() else {
            return false;
        };
        let key: &NSString = SCStreamFrameInfoStatus;
        let value = dict.value((key as *const NSString).cast());
        let Some(status) = value.cast::<CFNumber>().as_ref() else {
            return false;
        };
        status.as_isize() == Some(SCFrameStatus::Complete.0)
    }
}

/// Copy the sample's BGRA pixel buffer into a tightly packed frame, dropping row padding.
fn copy_frame(sample: &CMSampleBuffer, mut bgra: Vec<u8>) -> Option<Frame> {
    // SAFETY: `image_buffer` returns a retained CVPixelBuffer; we lock it read-only and
    // read only `bytes_per_row * (height - 1) + width * 4` bytes from its base address.
    unsafe {
        let pb = sample.image_buffer()?;
        if CVPixelBufferGetPixelFormatType(&pb) != kCVPixelFormatType_32BGRA
            || CVPixelBufferIsPlanar(&pb)
        {
            return None;
        }
        if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) != kCVReturnSuccess {
            return None;
        }
        let (w, h) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
        let stride = CVPixelBufferGetBytesPerRow(&pb);
        let base = CVPixelBufferGetBaseAddress(&pb).cast::<u8>();
        let row = w * 4;
        let ok = !base.is_null() && w > 0 && h > 0 && stride >= row;
        if ok {
            let src = std::slice::from_raw_parts(base, stride * (h - 1) + row);
            bgra.clear();
            bgra.reserve(row * h);
            for y in 0..h {
                bgra.extend_from_slice(&src[y * stride..y * stride + row]);
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly);
        ok.then(|| Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        })
    }
}

fn describe(e: &NSError) -> String {
    format!("{} ({} {})", e.localizedDescription(), e.domain(), e.code())
}

/// Error from a ScreenCaptureKit call that usually fails for lack of permission.
fn sck_error(what: &str, detail: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("{what}: {detail}; {PERMISSION_HINT}"))
}

/// A value handed from a ScreenCaptureKit completion handler to the waiting thread.
struct Handoff<T>(T);
// SAFETY: used only for SCShareableContent, an immutable snapshot ScreenCaptureKit itself
// hands across queues; the sending block never touches it after sending.
unsafe impl<T> Send for Handoff<T> {}

fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: the pointers are valid (or null) for the duration of the callback;
            // retaining keeps the content alive after it returns.
            let result = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(Handoff(c)),
                None => Err(unsafe { error.as_ref() }
                    .map(describe)
                    .unwrap_or_else(|| "no content and no error".into())),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the handler matches the declared block signature and is 'static.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(Ok(Handoff(c))) => Ok(c),
        Ok(Err(e)) => Err(sck_error("ScreenCaptureKit shareable content", e)),
        Err(_) => Err(sck_error(
            "ScreenCaptureKit shareable content",
            "timed out waiting for ScreenCaptureKit",
        )),
    }
}

/// Run a ScreenCaptureKit call that reports completion through an `NSError*` block.
fn wait_completion(
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the error pointer is valid or null for the duration of the callback.
        let r = match unsafe { error.as_ref() } {
            Some(e) => Err(describe(e)),
            None => Ok(()),
        };
        let _ = tx.send(r);
    });
    call(&handler);
    rx.recv_timeout(SCK_TIMEOUT)
        .unwrap_or_else(|_| Err("timed out waiting for ScreenCaptureKit".into()))
}

/// The display to capture: the main display, else the first one ScreenCaptureKit lists.
fn main_display(content: &SCShareableContent) -> Option<Retained<SCDisplay>> {
    // SAFETY: plain property reads on a valid SCShareableContent / SCDisplay.
    unsafe {
        let displays = content.displays();
        let main = CGMainDisplayID();
        let all: Vec<_> = (0..displays.count())
            .map(|i| displays.objectAtIndex(i))
            .collect();
        let pick = all.iter().position(|d| d.displayID() == main).unwrap_or(0);
        all.into_iter().nth(pick)
    }
}

/// The backing size of display `id`'s current mode (2x points on a Retina panel).
fn mode_pixels(id: u32) -> Option<(usize, usize)> {
    let mode = CGDisplayCopyDisplayMode(id);
    let (w, h) = (
        CGDisplayMode::pixel_width(mode.as_deref()),
        CGDisplayMode::pixel_height(mode.as_deref()),
    );
    (w > 0 && h > 0).then_some((w, h))
}

/// Native pixel size of `display`, falling back to its size in points.
fn pixel_size(display: &SCDisplay) -> (usize, usize) {
    // SAFETY: property reads on a valid SCDisplay.
    unsafe {
        mode_pixels(display.displayID()).unwrap_or_else(|| {
            (
                usize::try_from(display.width()).unwrap_or(0),
                usize::try_from(display.height()).unwrap_or(0),
            )
        })
    }
}

/// The size `Frame::fit_within(max)` gives (halving), made even: what the CPU path sends.
fn fit(size: (usize, usize), max: (usize, usize)) -> (usize, usize) {
    let (mut w, mut h) = size;
    while (w > max.0 || h > max.1) && w >= 4 && h >= 4 {
        (w, h) = (w / 2, h / 2);
    }
    (w & !1, h & !1)
}

/// Surface mode keeps this many buffers in ScreenCaptureKit's queue: the encoder holds the
/// latest picture (for keyframe repeats) and the mailbox one more.
const SURFACE_QUEUE_DEPTH: isize = 6;

/// How often to look for a display mode change (the host fitting the screen to the
/// viewer, or someone changing the resolution here).
const MODE_CHECK: Duration = Duration::from_millis(250);

pub struct MacCapturer {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    config: Retained<SCStreamConfiguration>,
    display: u32,
    size: (usize, usize),
    checked: Instant,
    /// Surface mode: the largest size to stream at (ScreenCaptureKit scales).
    surface_max: Option<(usize, usize)>,
}

// SCStream may be driven from any thread (its callbacks arrive on our dispatch queue);
// the capturer is used by one thread at a time.
unsafe impl Send for MacCapturer {}

impl MacCapturer {
    /// Start streaming the main display at its native pixel size, up to 60 fps.
    pub fn new() -> Result<Self, CaptureError> {
        let content = shareable_content()?;
        let display = main_display(&content).ok_or(CaptureError::NoOutput)?;
        let (width, height) = pixel_size(&display);
        if width == 0 || height == 0 {
            return Err(CaptureError::NoOutput);
        }

        let shared = Arc::new(Shared::default());
        let output = StreamOutput::new(shared.clone());
        let queue = DispatchQueue::new("rconn.capture", None);

        // SAFETY: Objective-C calls on freshly created, valid objects with the argument
        // types the bindings declare. CMTime is built field by field (1/60 s, valid).
        let (stream, config) = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width);
            config.setHeight(height);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setShowsCursor(true);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| sck_error("SCStream addStreamOutput", describe(&e)))?;
            (stream, config)
        };

        // SAFETY: the handler matches the declared block signature.
        wait_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) })
            .map_err(|e| sck_error("SCStream startCapture", e))?;
        tracing::info!(width, height, "ScreenCaptureKit capture started");

        Ok(Self {
            stream,
            output,
            _queue: queue,
            shared,
            config,
            // SAFETY: property read on a valid SCDisplay.
            display: unsafe { display.displayID() },
            size: (width, height),
            checked: Instant::now(),
            surface_max: None,
        })
    }

    /// The size ScreenCaptureKit should deliver for a screen of `size` pixels.
    fn output_size(&self, size: (usize, usize)) -> (usize, usize) {
        match self.surface_max {
            Some(max) => fit(size, max),
            None => size,
        }
    }

    fn update_configuration(&self) -> Result<(), String> {
        // SAFETY: an update of our running stream with a handler of the declared block
        // signature.
        wait_completion(|h| unsafe {
            self.stream
                .updateConfiguration_completionHandler(&self.config, Some(h))
        })
    }

    /// After a display mode change, stream at the new size (ScreenCaptureKit would
    /// otherwise keep scaling the screen to the old one).
    fn follow_mode_change(&mut self) -> Result<(), CaptureError> {
        if self.checked.elapsed() < MODE_CHECK {
            return Ok(());
        }
        self.checked = Instant::now();
        let Some(size) = mode_pixels(self.display).filter(|&s| s != self.size) else {
            return Ok(());
        };
        tracing::info!(
            width = size.0,
            height = size.1,
            "display size changed; capturing at the new size"
        );
        let out = self.output_size(size);
        // SAFETY: setters on our configuration.
        unsafe {
            self.config.setWidth(out.0);
            self.config.setHeight(out.1);
        }
        self.update_configuration()
            .map_err(|e| CaptureError::Backend(format!("SCStream updateConfiguration: {e}")))?;
        self.size = size;
        Ok(())
    }
}

impl Capturer for MacCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        self.follow_mode_change()?;
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.lock();
        loop {
            if let Some(e) = &slot.stopped {
                return Err(CaptureError::Backend(format!("{e}; {PERMISSION_HINT}")));
            }
            if let Some(frame) = slot.frame.take() {
                return Ok(Some(frame));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            slot = match self.shared.ready.wait_timeout(slot, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
    }

    fn use_surfaces(&mut self, max: (usize, usize)) -> bool {
        if self.surface_max.is_some() {
            return true;
        }
        self.surface_max = Some(max);
        let out = self.output_size(self.size);
        // SAFETY: setters on our configuration. The matrix constant is static.
        unsafe {
            self.config
                .setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            // BT.601, as the CPU conversion and both viewer decoders use.
            self.config
                .setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_601_4);
            self.config.setQueueDepth(SURFACE_QUEUE_DEPTH);
            self.config.setWidth(out.0);
            self.config.setHeight(out.1);
        }
        match self.update_configuration() {
            Ok(()) => {
                self.shared.surfaces.store(true, Ordering::Relaxed);
                self.shared.lock().frame = None;
                tracing::info!(
                    width = out.0,
                    height = out.1,
                    "ScreenCaptureKit hands NV12 surfaces to the encoder"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "ScreenCaptureKit surface mode unavailable");
                self.surface_max = None;
                // SAFETY: setters on our configuration, back to the copying mode.
                unsafe {
                    self.config.setPixelFormat(kCVPixelFormatType_32BGRA);
                    self.config.setWidth(self.size.0);
                    self.config.setHeight(self.size.1);
                }
                let _ = self.update_configuration();
                false
            }
        }
    }

    fn next_surface(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(Surface, Captured)>, CaptureError> {
        self.follow_mode_change()?;
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.lock();
        loop {
            if let Some(e) = &slot.stopped {
                return Err(CaptureError::Backend(format!("{e}; {PERMISSION_HINT}")));
            }
            if let Some(s) = slot.surface.take() {
                let captured = Captured {
                    rows: (0, s.height & !1),
                    changed: 1.0,
                    captured_at: s.captured_at,
                };
                return Ok(Some((s, captured)));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            slot = match self.shared.ready.wait_timeout(slot, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
    }
}

impl Drop for MacCapturer {
    fn drop(&mut self) {
        // Stop and wait, so no callback runs once the output is released.
        // SAFETY: the handler matches the declared block signature.
        if let Err(e) =
            wait_completion(|h| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(h)) })
        {
            tracing::debug!("SCStream stopCapture: {e}");
        }
        // SAFETY: removing the output we added, on the same stream.
        let removed = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                SCStreamOutputType::Screen,
            )
        };
        if let Err(e) = removed {
            tracing::debug!("SCStream removeStreamOutput: {}", describe(&e));
        }
    }
}
