//! File transfer for the app: browse the computer's folders, download into a local
//! folder (the app's Documents), upload into the folder shown. The `rconn-files/1` channel
//! and its protocol are `rconn_peer::files::FilesClient`'s, as in the desktop viewer: both
//! ends check every file's SHA-256, a download never replaces a local file, and the host
//! refuses paths outside its shared folder.
//!
//! The channel opens on the first request, once the computer has let the session in.

use rconn_core::files::FileEntry;
use rconn_peer::files::{FilesClient, Progress, Stopped};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use webrtc::peer_connection::PeerConnection;

/// The remote folder the app opens at (the root if the computer has none).
pub const START_FOLDER: &str = "Downloads";
/// How long a listing may take: a computer too old for file transfer opens the channel
/// but never answers.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a request waits for the session to be let in.
const ALLOWED_TIMEOUT: Duration = Duration::from_secs(90);

/// One entry of a remote folder.
#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Last modified, Unix seconds.
    pub modified: Option<u64>,
}

impl From<FileEntry> for RemoteEntry {
    fn from(e: FileEntry) -> Self {
        Self {
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
            modified: e.modified,
        }
    }
}

#[derive(uniffi::Error, thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    #[error("cancelled")]
    Cancelled,
    #[error("{message}")]
    Failed { message: String },
}

impl FileError {
    fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for FileError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<Stopped>() {
            Some(Stopped::Cancelled) => Self::Cancelled,
            _ => Self::failed(format!("{e:#}")),
        }
    }
}

/// One download or upload: its progress, and how to cancel it. Make one per transfer and
/// pass it in; read the progress from any thread.
#[derive(uniffi::Object, Debug, Default)]
pub struct Transfer {
    progress: Arc<Progress>,
}

#[uniffi::export]
impl Transfer {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Bytes moved so far.
    pub fn done_bytes(&self) -> u64 {
        self.progress.done()
    }

    /// The file's size, once known (0 before).
    pub fn total_bytes(&self) -> u64 {
        self.progress.total()
    }

    /// Stop it; the partial file is removed on both sides.
    pub fn cancel(&self) {
        self.progress.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.progress.is_cancelled()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// Not yet let in.
    Waiting,
    Yes,
    /// The session ended.
    Over,
}

/// The session's file side, shared with its run loop.
pub struct Files {
    pc: OnceLock<Arc<dyn PeerConnection>>,
    allowed: watch::Sender<Allowed>,
    client: Mutex<Option<Arc<FilesClient>>>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            pc: OnceLock::new(),
            allowed: watch::Sender::new(Allowed::Waiting),
            client: Mutex::default(),
        }
    }
}

impl Files {
    /// The session's connection, once it has one.
    pub fn connected(&self, pc: Arc<dyn PeerConnection>) {
        let _ = self.pc.set(pc);
    }

    /// The computer let the session in.
    pub fn allow(&self) {
        self.allowed.send_replace(Allowed::Yes);
    }

    /// The session is over: fail what waits, close the channel (call on the runtime).
    pub async fn end(&self) {
        self.allowed.send_replace(Allowed::Over);
        self.client.lock().await.take();
    }

    /// The open file channel, opening it on first use.
    async fn client(&self) -> Result<Arc<FilesClient>, FileError> {
        let mut allowed = self.allowed.subscribe();
        let state = tokio::time::timeout(
            ALLOWED_TIMEOUT,
            allowed.wait_for(|a| *a != Allowed::Waiting),
        )
        .await
        .map_err(|_| FileError::failed("the computer has not let this session in"))?
        .map(|a| *a)
        .unwrap_or(Allowed::Over);
        if state == Allowed::Over {
            return Err(FileError::failed("the session is over"));
        }
        let mut client = self.client.lock().await;
        if let Some(c) = client.as_ref() {
            return match c.refused() {
                Some(why) => Err(FileError::failed(why)),
                None => Ok(c.clone()),
            };
        }
        let pc = self
            .pc
            .get()
            .ok_or_else(|| FileError::failed("not connected"))?;
        let c = Arc::new(FilesClient::open(&**pc).await?);
        *client = Some(c.clone());
        Ok(c)
    }

    /// What is in `folder` on the computer ("" is its shared folder).
    pub async fn list(&self, folder: &str) -> Result<Vec<RemoteEntry>, FileError> {
        let client = self.client().await?;
        let entries = tokio::time::timeout(LIST_TIMEOUT, client.list(folder))
            .await
            .map_err(|_| {
                FileError::failed(
                    "the computer did not answer (it may be too old for file transfer)",
                )
            })??;
        let mut out: Vec<RemoteEntry> = entries.into_iter().map(Into::into).collect();
        // Folders first, then by name as people sort.
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Copy `remote` into `dest_dir`; returns the local path it was saved as.
    pub async fn download(
        &self,
        remote: &str,
        dest_dir: &Path,
        transfer: &Transfer,
    ) -> Result<String, FileError> {
        let client = self.client().await?;
        let path = client
            .download(remote, dest_dir, &transfer.progress)
            .await?;
        Ok(path.display().to_string())
    }

    /// Copy `local` into `folder` on the computer; returns where it saved it.
    pub async fn upload(
        &self,
        local: &Path,
        folder: &str,
        transfer: &Transfer,
    ) -> Result<String, FileError> {
        let client = self.client().await?;
        Ok(client
            .upload_into(local, Some(folder), &transfer.progress)
            .await?)
    }
}

/// The remote folder the app opens at.
#[uniffi::export]
pub fn remote_start_folder() -> String {
    START_FOLDER.into()
}

/// `folder/name` as a remote path, if `name` is one plain entry name (as a folder listing
/// gives them): None for anything that could step outside `folder` (`..`, separators,
/// drive letters, NUL).
#[uniffi::export]
pub fn remote_child(folder: String, name: String) -> Option<String> {
    let plain =
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0']);
    if !plain {
        return None;
    }
    let folder = folder.trim_end_matches(['/', '\\']);
    Some(if folder.is_empty() {
        name
    } else {
        format!("{folder}/{name}")
    })
}

/// The remote folder above `folder` ("" stays "").
#[uniffi::export]
pub fn remote_parent(folder: String) -> String {
    match folder
        .trim_end_matches(['/', '\\'])
        .rsplit_once(['/', '\\'])
    {
        Some((up, _)) => up.to_owned(),
        None => String::new(),
    }
}

/// How the app names a remote folder: `~` for the shared folder, `~/Downloads` below it.
#[uniffi::export]
pub fn remote_folder_title(folder: String) -> String {
    let f = folder.trim_matches(['/', '\\']);
    if f.is_empty() {
        "~".into()
    } else {
        format!("~/{f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_paths_refuse_anything_that_steps_outside() {
        let c = |f: &str, n: &str| remote_child(f.into(), n.into());
        assert_eq!(c("", "a.txt").as_deref(), Some("a.txt"));
        assert_eq!(c("Downloads", "a.txt").as_deref(), Some("Downloads/a.txt"));
        assert_eq!(c("Downloads/", "b").as_deref(), Some("Downloads/b"));
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "nul\0"] {
            assert_eq!(c("Downloads", bad), None, "{bad:?}");
        }
        // Whatever it builds passes the host's own check.
        let ok = c("Projects/rconn", "notes.md").unwrap();
        assert!(rconn_core::files::relative_components(&ok).is_ok());
    }

    #[test]
    fn parents_and_titles() {
        assert_eq!(remote_parent("Downloads/Rconn".into()), "Downloads");
        assert_eq!(remote_parent("Downloads/".into()), "");
        assert_eq!(remote_parent("".into()), "");
        assert_eq!(remote_folder_title("".into()), "~");
        assert_eq!(remote_folder_title("Downloads".into()), "~/Downloads");
        assert_eq!(remote_start_folder(), "Downloads");
    }

    #[test]
    fn cancelling_is_its_own_error() {
        let e: FileError = anyhow::Error::from(Stopped::Cancelled).into();
        assert_eq!(e, FileError::Cancelled);
        let e: FileError = anyhow::anyhow!("disk full").into();
        assert_eq!(e.to_string(), "disk full");
    }

    #[tokio::test]
    async fn requests_fail_cleanly_once_the_session_is_over() {
        let files = Files::default();
        files.end().await;
        let err = files.list("").await.unwrap_err();
        assert_eq!(err.to_string(), "the session is over");
        let t = Transfer::new();
        let err = files
            .download("a.txt", Path::new("/nonexistent"), &t)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "the session is over");
    }
}
