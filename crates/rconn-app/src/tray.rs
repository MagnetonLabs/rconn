//! Tray icon (`rconn --tray`): shows this computer's ID and status, opens the window.
//! Runs as its own small process, started at login, so it costs nothing while idle.

use rconn_core::status::HostStatus;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

/// Held while a tray runs, so a second one exits at once.
pub const TRAY_PORT: u16 = 47_392;

struct Tray {
    status_file: PathBuf,
    icon: Option<TrayIcon>,
    id_item: MenuItem,
    state_item: MenuItem,
    open: MenuItem,
    quit: MenuItem,
    next_status: Instant,
}

pub fn run(status_file: PathBuf) -> anyhow::Result<()> {
    let Ok(_single) = std::net::TcpListener::bind(("127.0.0.1", TRAY_PORT)) else {
        return Ok(()); // already running
    };
    #[allow(unused_mut)]
    let mut builder = EventLoop::builder();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory); // menu bar only, no Dock icon
    }
    let event_loop = builder.build()?;
    let mut tray = Tray {
        status_file,
        icon: None,
        id_item: MenuItem::new("ID: not set up", false, None),
        state_item: MenuItem::new("", false, None),
        open: MenuItem::new("Open rconn", true, None),
        quit: MenuItem::new("Quit rconn", true, None),
        next_status: Instant::now(),
    };
    event_loop.run_app(&mut tray)?;
    Ok(())
}

/// Quit everything: ask an open window (and through it, its session windows) to exit.
fn quit_app() {
    use std::io::Write;
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", crate::WINDOW_PORT)) {
        let _ = s.write_all(b"quit\n");
    }
}

fn open_window() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn(); // single-instance: focuses an open window
    }
}

impl Tray {
    fn build(&mut self) -> anyhow::Result<()> {
        let menu = Menu::new();
        menu.append_items(&[
            &self.id_item,
            &self.state_item,
            &PredefinedMenuItem::separator(),
            &self.open,
            &self.quit,
        ])?;
        let mac = cfg!(target_os = "macos");
        let img = if mac {
            crate::icon::tray_template()
        } else {
            crate::icon::tray()
        };
        let icon = tray_icon::Icon::from_rgba(img.rgba, img.width, img.height)?;
        let builder = TrayIconBuilder::new();
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);
        self.icon = Some(
            builder
                .with_menu(Box::new(menu))
                // Windows: left click opens the window, right click the menu.
                // macOS: any click opens the menu, as menu-bar icons do.
                .with_menu_on_left_click(mac)
                .with_tooltip("rconn")
                .build()?,
        );
        Ok(())
    }

    fn refresh_status(&mut self) {
        let now = crate::profile::now_unix();
        let (id, state) = match HostStatus::read(&self.status_file) {
            None => (
                "ID: not set up as a host".to_owned(),
                "Connections to this computer are off",
            ),
            Some(s) if !s.is_fresh(now) => (
                format!("ID: {}", s.short_code),
                "Offline: host service not running",
            ),
            Some(s) if s.session_active => {
                (format!("ID: {}", s.short_code), "Someone is connected now")
            }
            Some(s) if s.online => (
                format!("ID: {}", s.short_code),
                "Online, ready for connections",
            ),
            Some(s) => (format!("ID: {}", s.short_code), "Can't reach the server"),
        };
        self.id_item.set_text(&id);
        self.state_item.set_text(state);
        if let Some(icon) = &self.icon {
            let _ = icon.set_tooltip(Some(format!("rconn: {id}, {}", state.to_lowercase())));
        }
    }
}

impl ApplicationHandler for Tray {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init
            && let Err(e) = self.build()
        {
            tracing::error!(error = %e, "tray icon failed");
            event_loop.exit();
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *self.open.id() {
                open_window();
            } else if ev.id == *self.quit.id() {
                quit_app();
                self.icon = None;
                event_loop.exit();
                return;
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            match ev {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => open_window(),
                _ => {}
            }
        }
        if Instant::now() >= self.next_status {
            self.refresh_status();
            self.next_status = Instant::now() + Duration::from_secs(5);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(200),
        ));
    }
}
