//! The "remote session active" indicator shown on the host while an operator is connected.
//! Shown by default on physical machines (someone may be at the screen), per policy.
//! OS privacy indicators are separate and never touched.

/// Shows the indicator until dropped.
pub struct Indicator {
    #[cfg(windows)]
    /// Held only so the badge closes when the session ends (its Drop does the work).
    _badge: Option<win::Badge>,
}

impl Indicator {
    pub fn show() -> Self {
        tracing::info!("session indicator shown");
        #[cfg(windows)]
        {
            Self {
                _badge: win::Badge::show()
                    .map_err(|e| tracing::warn!(error = %e, "indicator failed"))
                    .ok(),
            }
        }
        #[cfg(not(windows))]
        {
            tracing::warn!("session indicator not implemented on this OS yet");
            Self {}
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        tracing::info!("session indicator hidden");
    }
}

#[cfg(windows)]
mod win {
    //! Win32 badge window. Unsafe is confined here: window class registration, the window
    //! procedure, and GDI painting.
    #![allow(unsafe_code)]

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject,
        DrawTextW, EndPaint, FillRect, PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetSystemMetrics, HTTRANSPARENT, LWA_ALPHA, MSG, PostMessageW, PostQuitMessage,
        RegisterClassW, SM_CXSCREEN, SetLayeredWindowAttributes, TranslateMessage, WM_CLOSE,
        WM_DESTROY, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::w;

    const W: i32 = 240;
    const H: i32 = 30;

    pub struct Badge {
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Badge {
        pub fn show() -> anyhow::Result<Self> {
            let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<isize>>();
            let thread = std::thread::Builder::new()
                .name("rconn-indicator".into())
                .spawn(move || unsafe {
                    match create() {
                        Ok(hwnd) => {
                            let _ = tx.send(Ok(hwnd.0 as isize));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                        }
                    }
                })?;
            let hwnd = rx.recv()??;
            Ok(Self {
                hwnd,
                thread: Some(thread),
            })
        }
    }

    impl Drop for Badge {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    unsafe fn create() -> anyhow::Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("rconn_session_indicator");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc); // re-registering an existing class fails harmlessly
            let x = GetSystemMetrics(SM_CXSCREEN) - W - 16;
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE,
                class,
                w!("Remote session active"),
                WS_POPUP | WS_VISIBLE,
                x,
                16,
                W,
                H,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 225, LWA_ALPHA)?;
            Ok(hwnd)
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    let brush = CreateSolidBrush(COLORREF(0x0020_20C0)); // red (BGR), readable
                    let mut rect = RECT {
                        left: 0,
                        top: 0,
                        right: W,
                        bottom: H,
                    };
                    FillRect(hdc, &rect, brush);
                    let _ = DeleteObject(brush.into());
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                    let mut text: Vec<u16> =
                        "\u{25CF}  Remote session active".encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut text,
                        &mut rect,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    let _ = EndPaint(hwnd, &ps);
                    LRESULT(0)
                }
                // Click-through: the badge never takes input from the person at the machine.
                WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}
