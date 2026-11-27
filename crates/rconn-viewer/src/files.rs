//! File transfer in the session window: the Files panel (this computer | the remote one),
//! files dropped on the window, and the transfers with their progress.
//!
//! The UI sends [`Cmd`]s to [`run`] on the session's runtime and reads [`State`] back. The
//! file channel ([`FilesClient`]) opens on the first command, once the session is allowed
//! in; downloads go to this computer's Downloads folder, uploads to the remote
//! Downloads/Rconn. Both ends check every file's SHA-256.

use egui::{Color32, RichText};
use rconn_core::files::FileEntry;
use rconn_peer::files::{FilesClient, Progress, Stopped};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use webrtc::peer_connection::PeerConnection;

/// What the Files panel asks the session for.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Show this remote folder ("" is the shared root).
    List(String),
    /// Copy this remote file into the local Downloads folder.
    Download(String),
    /// Copy this local file to the remote computer.
    Upload(PathBuf),
}

/// One transfer, as the panel shows it.
pub struct Transfer {
    pub upload: bool,
    pub name: String,
    pub progress: Arc<Progress>,
    /// `Ok(where it was saved)` or `Err(why not)`, once finished.
    pub outcome: Option<Result<String, String>>,
}

/// The session's side of the panel.
#[derive(Default)]
pub struct State {
    /// The remote folder shown, and what is in it (None while loading).
    pub remote_dir: String,
    pub entries: Option<Vec<FileEntry>>,
    pub listing_error: Option<String>,
    /// Why file transfer can't be used with this computer, once that is known.
    pub unavailable: Option<String>,
    pub transfers: Vec<Transfer>,
}

pub type Shared = Arc<Mutex<State>>;

/// Where downloads go on this computer.
pub fn downloads_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| {
            d.download_dir()
                .map(Path::to_path_buf)
                .or_else(|| Some(d.home_dir().join("Downloads")))
        })
        .unwrap_or_else(std::env::temp_dir)
}

/// Serve the panel's commands for one session. `allowed` turns true once the session is
/// let in (the host refuses the file channel before that).
pub async fn run(
    pc: Arc<dyn PeerConnection>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    state: Shared,
    mut allowed: watch::Receiver<bool>,
    repaint: impl Fn() + Clone + Send + Sync + 'static,
) {
    let Some(first) = cmds.recv().await else {
        return;
    };
    if allowed.wait_for(|a| *a).await.is_err() {
        return;
    }
    let client = match FilesClient::open(&*pc).await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            state.lock().expect("files").unavailable = Some(format!("{e:#}"));
            repaint();
            return;
        }
    };
    let mut next = Some(first);
    loop {
        let cmd = match next.take() {
            Some(c) => c,
            None => match cmds.recv().await {
                Some(c) => c,
                None => return,
            },
        };
        let (client, state, done) = (client.clone(), state.clone(), repaint.clone());
        match cmd {
            Cmd::List(dir) => {
                {
                    let mut s = state.lock().expect("files");
                    s.remote_dir.clone_from(&dir);
                    s.entries = None;
                    s.listing_error = None;
                }
                tokio::spawn(async move {
                    let result = client.list(&dir).await;
                    let mut s = state.lock().expect("files");
                    if s.remote_dir == dir {
                        match result {
                            Ok(entries) => s.entries = Some(entries),
                            Err(e) => {
                                s.entries = Some(Vec::new());
                                s.listing_error = Some(format!("{e:#}"));
                            }
                        }
                        if let Some(why) = client.refused() {
                            s.unavailable = Some(why);
                        }
                    }
                    drop(s);
                    done();
                });
            }
            Cmd::Download(path) => {
                let name = path.rsplit(['/', '\\']).next().unwrap_or(&path).to_owned();
                let progress = start(&state, false, name);
                tokio::spawn(async move {
                    let result = client
                        .download(&path, &downloads_dir(), &progress)
                        .await
                        .map(|p| p.display().to_string());
                    finish(&state, &progress, result, &client);
                    done();
                });
            }
            Cmd::Upload(local) => {
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let progress = start(&state, true, name);
                tokio::spawn(async move {
                    let result = client.upload(&local, &progress).await;
                    finish(&state, &progress, result, &client);
                    done();
                });
            }
        }
        repaint();
    }
}

fn start(state: &Shared, upload: bool, name: String) -> Arc<Progress> {
    let progress = Progress::new();
    state.lock().expect("files").transfers.push(Transfer {
        upload,
        name,
        progress: progress.clone(),
        outcome: None,
    });
    progress
}

fn finish(
    state: &Shared,
    progress: &Arc<Progress>,
    result: anyhow::Result<String>,
    client: &FilesClient,
) {
    let mut s = state.lock().expect("files");
    let outcome = result.map_err(|e| match e.downcast_ref::<Stopped>() {
        Some(Stopped::Cancelled) => "cancelled".to_owned(),
        _ => format!("{e:#}"),
    });
    if let Some(t) = s
        .transfers
        .iter_mut()
        .find(|t| Arc::ptr_eq(&t.progress, progress))
    {
        t.outcome = Some(outcome);
    }
    if let Some(why) = client.refused() {
        s.unavailable = Some(why);
    }
}

/// Sizes as people read them.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// The remote folder above `dir` ("" stays "").
pub fn parent(dir: &str) -> String {
    match dir.trim_end_matches('/').rsplit_once('/') {
        Some((up, _)) => up.to_owned(),
        None => String::new(),
    }
}

/// `dir/name` as a remote path.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// A local folder entry: name, is a folder, size.
type LocalEntry = (String, bool, u64);

/// A local folder being read on its own thread (a network share or a folder of 100 000
/// files must not stall the window, and with it the remote screen and input).
struct LocalListing {
    dir: PathBuf,
    result: Arc<Mutex<Option<Vec<LocalEntry>>>>,
}

impl LocalListing {
    fn start(dir: PathBuf, ctx: &egui::Context) -> Self {
        let result = Arc::new(Mutex::new(None));
        let (slot, ctx, path) = (result.clone(), ctx.clone(), dir.clone());
        let spawned = std::thread::Builder::new()
            .name("files-local-list".into())
            .spawn(move || {
                let listed = read_dir_sorted(&path);
                *slot.lock().expect("local listing") = Some(listed);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            *result.lock().expect("local listing") = Some(Vec::new());
        }
        Self { dir, result }
    }
}

/// `dir`'s entries, folders first, then by name.
fn read_dir_sorted(dir: &Path) -> Vec<LocalEntry> {
    let mut v: Vec<LocalEntry> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let m = std::fs::metadata(e.path()).ok()?;
                    Some((
                        e.file_name().to_string_lossy().into_owned(),
                        m.is_dir(),
                        m.len(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    v
}

/// The panel's UI side, one per tab.
pub struct Panel {
    pub open: bool,
    cmds: mpsc::UnboundedSender<Cmd>,
    pub state: Shared,
    local_dir: PathBuf,
    /// The local folder's contents, once read (off the UI thread: [`LocalListing`]).
    local: Option<Vec<LocalEntry>>,
    local_listing: Option<LocalListing>,
    local_selected: std::collections::BTreeSet<String>,
    remote_selected: std::collections::BTreeSet<String>,
    /// Listed at least once since the panel opened.
    listed: bool,
}

impl Panel {
    pub fn new(cmds: mpsc::UnboundedSender<Cmd>, state: Shared) -> Self {
        Self {
            open: false,
            cmds,
            state,
            local_dir: directories::UserDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .unwrap_or_else(std::env::temp_dir),
            local: None,
            local_listing: None,
            local_selected: Default::default(),
            remote_selected: Default::default(),
            listed: false,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    /// Transfers still running.
    pub fn active(&self) -> usize {
        self.state
            .lock()
            .expect("files")
            .transfers
            .iter()
            .filter(|t| t.outcome.is_none())
            .count()
    }

    /// Send files dropped on the window; returns how many.
    pub fn upload(&mut self, paths: Vec<PathBuf>) -> usize {
        let n = paths.len();
        for p in paths {
            self.send(Cmd::Upload(p));
        }
        if n > 0 {
            self.open = true;
        }
        n
    }

    fn list_remote(&mut self, dir: String) {
        self.remote_selected.clear();
        self.listed = true;
        self.send(Cmd::List(dir));
    }

    /// The local folder's contents; `None` while they are being read.
    fn read_local(&mut self, ctx: &egui::Context) -> Option<&[LocalEntry]> {
        if self.local.is_none() {
            match &self.local_listing {
                Some(l) if l.dir == self.local_dir => {
                    let done = l.result.lock().expect("local listing").take();
                    if done.is_some() {
                        self.local = done;
                        self.local_listing = None;
                    }
                }
                _ => self.local_listing = Some(LocalListing::start(self.local_dir.clone(), ctx)),
            }
        }
        self.local.as_deref()
    }

    fn set_local_dir(&mut self, dir: PathBuf) {
        self.local_dir = dir;
        self.local = None;
        self.local_selected.clear();
    }

    /// The floating panel; returns its rectangle (pointer input there stays local).
    pub fn show(&mut self, ctx: &egui::Context, computer: &str) -> Option<egui::Rect> {
        if !self.open {
            return None;
        }
        if !self.listed {
            let dir = self.state.lock().expect("files").remote_dir.clone();
            self.list_remote(dir);
        }
        let mut open = true;
        let shown = egui::Window::new(format!("Files — {computer}"))
            .id(egui::Id::unique("files-panel"))
            .open(&mut open)
            .default_size([760.0, 460.0])
            .collapsible(false)
            .show(ctx, |ui| self.contents(ui, computer));
        self.open = open;
        if !open {
            self.listed = false;
        }
        shown.map(|r| r.response.rect)
    }

    fn contents(&mut self, ui: &mut egui::Ui, computer: &str) {
        let unavailable = self.state.lock().expect("files").unavailable.clone();
        if let Some(why) = &unavailable {
            ui.colored_label(
                Color32::from_rgb(220, 120, 60),
                format!("File transfer unavailable: {why}"),
            );
            ui.separator();
        }
        ui.label(
            RichText::new(format!(
                "Drop files on the window to send them to {computer} (they land in Downloads/Rconn there). Downloads go to {}.",
                downloads_dir().display()
            ))
            .color(Color32::GRAY)
            .small(),
        );
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            self.local_pane(&mut cols[0]);
            self.remote_pane(&mut cols[1], computer, unavailable.is_some());
        });
        ui.separator();
        self.transfers(ui);
    }

    fn local_pane(&mut self, ui: &mut egui::Ui) {
        ui.strong("💻 This computer");
        ui.horizontal(|ui| {
            if ui
                .small_button("⬆")
                .on_hover_text("Up one folder")
                .clicked()
                && let Some(up) = self.local_dir.parent().map(Path::to_path_buf)
            {
                self.set_local_dir(up);
            }
            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                self.local = None;
            }
            ui.label(
                RichText::new(self.local_dir.display().to_string())
                    .monospace()
                    .small(),
            );
        });
        let mut enter = None;
        let entries = self.read_local(ui.ctx()).map(<[_]>::to_vec);
        egui::ScrollArea::vertical()
            .id_salt("local-files")
            .max_height(220.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some(entries) = &entries else {
                    ui.spinner();
                    return;
                };
                for (name, is_dir, size) in entries {
                    ui.horizontal(|ui| {
                        if *is_dir {
                            if ui.selectable_label(false, format!("📁 {name}")).clicked() {
                                enter = Some(name.clone());
                            }
                        } else {
                            let mut on = self.local_selected.contains(name);
                            if ui.checkbox(&mut on, format!("📄 {name}")).changed() {
                                if on {
                                    self.local_selected.insert(name.clone());
                                } else {
                                    self.local_selected.remove(name);
                                }
                            }
                            ui.label(RichText::new(human(*size)).color(Color32::GRAY).small());
                        }
                    });
                }
            });
        if let Some(name) = enter {
            let dir = self.local_dir.join(name);
            self.set_local_dir(dir);
        }
        let n = self.local_selected.len();
        if ui
            .add_enabled(n > 0, egui::Button::new(format!("Send {n} ➡")))
            .on_hover_text("Copy the selected files to the remote computer's Downloads/Rconn")
            .clicked()
        {
            let paths: Vec<PathBuf> = std::mem::take(&mut self.local_selected)
                .into_iter()
                .map(|n| self.local_dir.join(n))
                .collect();
            self.upload(paths);
        }
    }

    fn remote_pane(&mut self, ui: &mut egui::Ui, computer: &str, unavailable: bool) {
        let (dir, entries, error) = {
            let s = self.state.lock().expect("files");
            (
                s.remote_dir.clone(),
                s.entries.clone(),
                s.listing_error.clone(),
            )
        };
        ui.strong(format!("🖥 {computer}"));
        ui.horizontal(|ui| {
            if ui
                .small_button("⬆")
                .on_hover_text("Up one folder")
                .clicked()
                && !dir.is_empty()
            {
                self.list_remote(parent(&dir));
            }
            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                self.list_remote(dir.clone());
            }
            let shown = if dir.is_empty() {
                "~".to_owned()
            } else {
                format!("~/{dir}")
            };
            ui.label(RichText::new(shown).monospace().small());
        });
        let mut enter = None;
        egui::ScrollArea::vertical()
            .id_salt("remote-files")
            .max_height(220.0)
            .auto_shrink([false, false])
            .show(ui, |ui| match &entries {
                None if !unavailable => {
                    ui.spinner();
                }
                None => {}
                Some(list) => {
                    if let Some(e) = &error {
                        ui.colored_label(Color32::from_rgb(220, 90, 70), e);
                    }
                    for e in list {
                        ui.horizontal(|ui| {
                            if e.is_dir {
                                if ui
                                    .selectable_label(false, format!("📁 {}", e.name))
                                    .clicked()
                                {
                                    enter = Some(e.name.clone());
                                }
                            } else {
                                let mut on = self.remote_selected.contains(&e.name);
                                if ui.checkbox(&mut on, format!("📄 {}", e.name)).changed() {
                                    if on {
                                        self.remote_selected.insert(e.name.clone());
                                    } else {
                                        self.remote_selected.remove(&e.name);
                                    }
                                }
                                ui.label(RichText::new(human(e.size)).color(Color32::GRAY).small());
                            }
                        });
                    }
                }
            });
        if let Some(name) = enter {
            self.list_remote(join(&dir, &name));
        }
        let n = self.remote_selected.len();
        if ui
            .add_enabled(n > 0, egui::Button::new(format!("⬅ Download {n}")))
            .on_hover_text("Copy the selected files to this computer's Downloads folder")
            .clicked()
        {
            for name in std::mem::take(&mut self.remote_selected) {
                self.send(Cmd::Download(join(&dir, &name)));
            }
        }
    }

    fn transfers(&mut self, ui: &mut egui::Ui) {
        let mut s = self.state.lock().expect("files");
        ui.horizontal(|ui| {
            ui.strong("Transfers");
            if s.transfers.iter().any(|t| t.outcome.is_some())
                && ui.small_button("Clear finished").clicked()
            {
                s.transfers.retain(|t| t.outcome.is_none());
            }
        });
        if s.transfers.is_empty() {
            ui.label(RichText::new("None yet").color(Color32::GRAY));
            return;
        }
        let mut running = false;
        egui::ScrollArea::vertical()
            .id_salt("transfers")
            .max_height(120.0)
            .show(ui, |ui| {
                for t in s.transfers.iter().rev() {
                    ui.horizontal(|ui| {
                        ui.label(if t.upload { "➡" } else { "⬅" });
                        ui.label(&t.name);
                        let (done, total) = (t.progress.done(), t.progress.total());
                        match &t.outcome {
                            None => {
                                running = true;
                                let frac = if total > 0 {
                                    done as f32 / total as f32
                                } else {
                                    0.0
                                };
                                ui.add(
                                    egui::ProgressBar::new(frac)
                                        .desired_width(200.0)
                                        .text(format!("{} / {}", human(done), human(total))),
                                );
                                if ui.small_button("Cancel").clicked() {
                                    t.progress.cancel();
                                }
                            }
                            Some(Ok(saved)) => {
                                ui.colored_label(
                                    Color32::from_rgb(120, 200, 140),
                                    format!("✔ {} — saved as {saved}", human(done)),
                                );
                            }
                            Some(Err(why)) => {
                                ui.colored_label(
                                    Color32::from_rgb(220, 90, 70),
                                    format!("✖ {why}"),
                                );
                            }
                        }
                    });
                }
            });
        if running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_remote_paths() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_500_000), "1.5 MB");
        assert_eq!(human(100 * 1024 * 1024), "104.9 MB");
        assert_eq!(parent("Documents/Reports"), "Documents");
        assert_eq!(parent("Documents"), "");
        assert_eq!(parent(""), "");
        assert_eq!(join("", "a.txt"), "a.txt");
        assert_eq!(join("Documents", "a.txt"), "Documents/a.txt");
    }

    /// The local pane's folder is read on its own thread: the first frame shows a
    /// spinner instead of waiting for the disk, a later one the entries.
    #[test]
    fn the_local_folder_is_read_off_the_ui_thread() {
        let dir = std::env::temp_dir().join(format!("rconn-local-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("b.txt"), b"bb").unwrap();
        std::fs::write(dir.join("A.txt"), b"a").unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut panel = Panel::new(tx, Shared::default());
        panel.set_local_dir(dir.clone());
        let ctx = egui::Context::default();
        let started = std::time::Instant::now();
        let mut listed = panel.read_local(&ctx).map(<[_]>::to_vec);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        while listed.is_none() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "never listed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            listed = panel.read_local(&ctx).map(<[_]>::to_vec);
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Folders first, then by name; a folder's size is whatever the OS says.
        let got: Vec<(String, bool, Option<u64>)> = listed
            .unwrap()
            .into_iter()
            .map(|(n, d, s)| (n, d, (!d).then_some(s)))
            .collect();
        assert_eq!(
            got,
            [
                ("Sub".to_owned(), true, None),
                ("A.txt".to_owned(), false, Some(1)),
                ("b.txt".to_owned(), false, Some(2)),
            ]
        );
    }
}
