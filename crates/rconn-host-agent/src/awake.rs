//! Keep the host's display on during a session, and wake it when one starts.
//!
//! Screen capture gets no frames while the display is asleep (Windows turns it off after
//! the idle timeout), so a viewer connecting to an idle machine would see nothing.

/// Held for the session; dropping it lets the display sleep again.
pub struct KeepAwake {
    #[cfg(windows)]
    stop: Option<std::sync::mpsc::Sender<()>>,
}

pub fn start() -> KeepAwake {
    #[cfg(windows)]
    {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let _ = std::thread::Builder::new()
            .name("rconn-awake".into())
            .spawn(move || windows::hold(&rx));
        KeepAwake { stop: Some(tx) }
    }
    #[cfg(not(windows))]
    KeepAwake {}
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        drop(self.stop.take()); // ends the thread, which releases the request
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: SetThreadExecutionState, SendInput, desktop attachment
mod windows {
    use std::time::Duration;
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
    };

    /// A 1-pixel mouse move and back: wakes a sleeping display without moving the pointer.
    fn nudge() {
        unsafe {
            let _ = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(0x1000_0000),
            )
            .and_then(|d| SetThreadDesktop(d));
            for dx in [1, -1] {
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }

    pub fn hold(stop: &std::sync::mpsc::Receiver<()>) {
        nudge();
        // The request belongs to this thread; refresh it now and then for good measure.
        loop {
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            }
            match stop.recv_timeout(Duration::from_secs(30)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                _ => break,
            }
        }
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}
