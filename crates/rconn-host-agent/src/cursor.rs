//! Watch the host's mouse-pointer shape and report changes, so the viewer can draw the
//! pointer itself (instantly) instead of waiting for it in the video.

use rconn_core::protocol::CursorShape;
use tokio::sync::mpsc;

/// Start watching; changes arrive on the receiver. Stops when the receiver is dropped.
/// On systems without support the receiver simply never yields.
pub fn watch() -> mpsc::UnboundedReceiver<CursorShape> {
    let (tx, rx) = mpsc::unbounded_channel();
    #[cfg(windows)]
    {
        let _ = std::thread::Builder::new()
            .name("rconn-cursor".into())
            .spawn(move || windows::run(&tx));
    }
    #[cfg(not(windows))]
    drop(tx);
    rx
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: GetCursorInfo / LoadCursorW / desktop attachment
mod windows {
    use super::CursorShape;
    use std::time::{Duration, Instant};
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS,
        IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
        IDC_SIZEWE, IDC_WAIT, LoadCursorW,
    };

    const POLL: Duration = Duration::from_millis(30);

    fn attach_input_desktop() {
        const GENERIC_ALL: u32 = 0x1000_0000;
        unsafe {
            let _ = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            )
            .and_then(|d| SetThreadDesktop(d));
        }
    }

    pub fn run(tx: &tokio::sync::mpsc::UnboundedSender<CursorShape>) {
        use CursorShape as C;
        let table: Vec<(HCURSOR, CursorShape)> = [
            (IDC_ARROW, C::Arrow),
            (IDC_IBEAM, C::Text),
            (IDC_HAND, C::Hand),
            (IDC_SIZEWE, C::ResizeHorizontal),
            (IDC_SIZENS, C::ResizeVertical),
            (IDC_SIZENWSE, C::ResizeNwSe),
            (IDC_SIZENESW, C::ResizeNeSw),
            (IDC_SIZEALL, C::Move),
            (IDC_WAIT, C::Wait),
            (IDC_APPSTARTING, C::Progress),
            (IDC_NO, C::NotAllowed),
            (IDC_CROSS, C::Crosshair),
        ]
        .into_iter()
        .filter_map(|(id, shape)| unsafe { LoadCursorW(None, id) }.ok().map(|h| (h, shape)))
        .collect();
        let mut last = None;
        let mut attached_at = Instant::now() - Duration::from_secs(10);
        loop {
            // The input desktop changes (login screen, UAC); re-attach now and then.
            if attached_at.elapsed() > Duration::from_secs(1) {
                attach_input_desktop();
                attached_at = Instant::now();
            }
            let mut info = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetCursorInfo(&mut info) }.is_ok() {
                let shape = if info.flags.0 & CURSOR_SHOWING.0 == 0 {
                    C::Hidden
                } else {
                    table
                        .iter()
                        .find(|(h, _)| *h == info.hCursor)
                        .map_or(C::Arrow, |(_, s)| *s)
                };
                if last != Some(shape) {
                    if tx.send(shape).is_err() {
                        return; // session over
                    }
                    last = Some(shape);
                }
            }
            std::thread::sleep(POLL);
        }
    }
}
