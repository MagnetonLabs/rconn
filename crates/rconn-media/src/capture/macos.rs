//! ScreenCaptureKit capture of the main display.
//!
//! Unsafe is confined to this module: it is Objective-C/CoreFoundation FFI into
//! ScreenCaptureKit, CoreMedia and CoreVideo. objc2 wraps objects in ref-counted
//! `Retained`/`CFRetained` types, every fallible call is checked, and the only raw memory
//! access is the locked CVPixelBuffer, read strictly within `bytesPerRow * height`.
//!
//! SCStream delivers frames on its own dispatch queue to [`StreamOutput`], which copies
//! each complete frame into a one-slot mailbox; [`MacCapturer::next_frame`] waits on it.
//! The process needs the Screen Recording permission (TCC); without it ScreenCaptureKit
//! returns an error, reported as [`CaptureError::Backend`].
#![allow(unsafe_code)]

use super::{CaptureError, Capturer};
use crate::video::Frame;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

/// How long to wait for ScreenCaptureKit's completion handlers. The first call can show a
/// permission prompt, which returns an error right away rather than blocking.
const SCK_TIMEOUT: Duration = Duration::from_secs(10);

const PERMISSION_HINT: &str = "grant Screen Recording permission in System Settings > \
     Privacy & Security > Screen & System Audio Recording, then restart the agent";

/// Latest captured frame, or the error that stopped the stream.
#[derive(Default)]
struct Slot {
    frame: Option<Frame>,
    stopped: Option<String>,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
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

/// Native pixel size of `display`: the backing size of its current mode (2x points on a
/// Retina panel), falling back to its size in points.
fn pixel_size(display: &SCDisplay) -> (usize, usize) {
    // SAFETY: property reads on a valid SCDisplay; the display mode is a retained copy.
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display.displayID());
        let (w, h) = (
            CGDisplayMode::pixel_width(mode.as_deref()),
            CGDisplayMode::pixel_height(mode.as_deref()),
        );
        if w > 0 && h > 0 {
            (w, h)
        } else {
            (
                usize::try_from(display.width()).unwrap_or(0),
                usize::try_from(display.height()).unwrap_or(0),
            )
        }
    }
}

pub struct MacCapturer {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
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
        let stream = unsafe {
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
            stream
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
        })
    }
}

impl Capturer for MacCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
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
