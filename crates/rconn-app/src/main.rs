//! `rconn`: the desktop app. `rconn --tray` runs only the tray icon; `rconn --ask <json>`
//! shows the host agent's "allow this connection?" question (see `rconn_app::ask`).
#![cfg_attr(windows, windows_subsystem = "windows")] // no console window behind the GUI

use rconn_app::home::{self, Home};
use rconn_app::profile::Profile;
use rconn_app::worker::{self, Paths};
use std::io::Write;
use std::sync::{Arc, Mutex};

use rconn_app::WINDOW_PORT;

struct App {
    shared: worker::Shared,
    tx: tokio::sync::mpsc::UnboundedSender<home::Request>,
    /// The window's close button was used (keep the tray icon); otherwise the whole app
    /// was quit (Cmd+Q, Quit menu) and the icon goes too.
    closed_by_window: bool,
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        if !self.closed_by_window {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            if let Ok(mut s) =
                std::net::TcpStream::connect(("127.0.0.1", rconn_app::tray::TRAY_PORT))
            {
                let _ = s.write_all(b"quit\n");
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|i| i.viewport().close_requested()) {
            self.closed_by_window = true;
        }
        let requests = home::draw(ui, &mut self.shared.lock().expect("home state"));
        for r in requests {
            let _ = self.tx.send(r);
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Logs go to stderr: stdout carries `--ask`'s answer to the host agent.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The host agent asking the person here about a connection (own window, own process).
    if args.first().map(String::as_str) == Some("--ask") {
        return rconn_app::ask::run(args.get(1).map_or("", String::as_str));
    }
    if args.iter().any(|a| a == "--tray") {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        return rconn_app::tray::run(rconn_app::worker::Paths::installed().status_file);
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        return Ok(());
    }

    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", WINDOW_PORT)) else {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", WINDOW_PORT)) {
            let _ = s.write_all(b"show\n");
        }
        return Ok(());
    };
    // Make sure the tray icon is running (a duplicate exits immediately).
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    if !args.iter().any(|a| a == "--no-tray")
        && let Ok(exe) = std::env::current_exe()
    {
        // macOS: the helper app inside the bundle (its own app, so macOS doesn't mistake
        // the menu-bar icon for an open rconn window).
        let helper = exe
            .parent()
            .and_then(|d| d.parent())
            .map(|c| c.join("Helpers/rconn-tray.app/Contents/MacOS/rconn-tray"));
        let _ = match helper.filter(|h| h.exists()) {
            Some(h) => std::process::Command::new(h).spawn(),
            None => std::process::Command::new(exe).arg("--tray").spawn(),
        };
    }

    let paths = Paths::installed();
    let shared: worker::Shared = Arc::new(Mutex::new(Home::new(Profile::load(&paths.config))));
    let icon = rconn_app::icon::app();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("Rconn")
            .with_icon(egui::IconData {
                rgba: icon.rgba,
                width: icon.width,
                height: icon.height,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "Rconn",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let front = ctx.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for conn in listener.incoming().flatten() {
                    let mut line = String::new();
                    let _ = std::io::BufReader::new(conn).read_line(&mut line);
                    if line.trim() == "quit" {
                        // Tray "Quit rconn": close session windows too, then exit.
                        worker::end_all_sessions();
                        std::process::exit(0);
                    }
                    front.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    front.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    front.send_viewport_cmd(egui::ViewportCommand::Focus);
                    front.request_repaint();
                }
            });
            let tx = worker::spawn(shared.clone(), ctx, paths);
            Ok(Box::new(App {
                shared,
                tx,
                closed_by_window: false,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
