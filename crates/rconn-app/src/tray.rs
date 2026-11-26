//! Tray icon (`rconn --tray`): shows this computer's ID and status, opens the window.
//! Runs as its own small process, started at login, so it costs nothing while idle.
//! macOS and Windows drive it with winit; Linux with GTK (AppIndicator, which Ubuntu's
//! desktop shows in the top bar).

use rconn_core::status::HostStatus;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
#[cfg(not(target_os = "linux"))]
use winit::application::ApplicationHandler;
#[cfg(not(target_os = "linux"))]
use winit::event::{StartCause, WindowEvent};
#[cfg(not(target_os = "linux"))]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(not(target_os = "linux"))]
use winit::window::WindowId;

/// How often the tray handles clicks (and, every 5 s, re-reads the status).
const TICK: Duration = Duration::from_millis(200);

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
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", TRAY_PORT)) else {
        return Ok(()); // already running
    };
    // "quit" from the window (the app was quit with Cmd+Q / Quit): remove the icon too.
    std::thread::spawn(move || {
        use std::io::BufRead;
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            if std::io::BufReader::new(conn).read_line(&mut line).is_ok() && line.trim() == "quit" {
                std::process::exit(0);
            }
        }
    });
    let tray = Tray {
        status_file,
        icon: None,
        id_item: MenuItem::new("ID: not set up", false, None),
        state_item: MenuItem::new("", false, None),
        open: MenuItem::new("Open Rconn", true, None),
        quit: MenuItem::new("Quit Rconn", true, None),
        next_status: Instant::now(),
    };
    run_loop(tray)
}

#[cfg(not(target_os = "linux"))]
fn run_loop(mut tray: Tray) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut builder = EventLoop::builder();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory); // menu bar only, no Dock icon
    }
    let event_loop = builder.build()?;
    event_loop.run_app(&mut tray)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_loop(mut tray: Tray) -> anyhow::Result<()> {
    gtk::init()?;
    tray.build()?;
    let mut tray = Some(tray);
    gtk::glib::timeout_add_local(TICK, move || {
        if tray.as_mut().is_some_and(Tray::tick) {
            gtk::glib::ControlFlow::Continue
        } else {
            tray = None; // removes the icon
            gtk::main_quit();
            gtk::glib::ControlFlow::Break
        }
    });
    gtk::main();
    Ok(())
}

/// Quit everything: ask an open window (and through it, its session windows) to exit.
fn quit_app() {
    use std::io::Write;
    if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", crate::WINDOW_PORT)) {
        let _ = s.write_all(b"quit\n");
    }
}

/// Open (or bring forward) the main window.
fn open_window() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // macOS: this runs inside rconn.app/Contents/Helpers/rconn-tray.app; let macOS open
    // the outer app, which also brings an already-open window forward.
    if let Some(app) = exe.ancestors().find(|p| {
        p.extension().is_some_and(|e| e == "app")
            && p.file_name().is_some_and(|n| n != "rconn-tray.app")
    }) {
        let _ = std::process::Command::new("open").arg(app).spawn();
        return;
    }
    // Windows / unpackaged: the window binary sits next to this one.
    let is_tray_binary = exe.file_stem().is_some_and(|s| s == "rconn-tray");
    let window = if is_tray_binary {
        exe.with_file_name(format!("rconn{}", std::env::consts::EXE_SUFFIX))
    } else {
        exe
    };
    let _ = std::process::Command::new(window).spawn(); // single-instance: focuses an open window
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
                .with_tooltip("Rconn")
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
            let _ = icon.set_tooltip(Some(format!("Rconn: {id}, {}", state.to_lowercase())));
        }
    }
}

impl Tray {
    /// Handle clicks and refresh the status. `false` once the user chose Quit.
    fn tick(&mut self) -> bool {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *self.open.id() {
                open_window();
            } else if ev.id == *self.quit.id() {
                quit_app();
                return false;
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
        true
    }
}

#[cfg(not(target_os = "linux"))]
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
        if !self.tick() {
            self.icon = None;
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    }
}
