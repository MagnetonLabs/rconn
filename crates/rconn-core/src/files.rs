//! File transfer: the messages on the second data channel ([`FILES_CHANNEL_PROTOCOL`]) and
//! the path rules both ends apply.
//!
//! The viewer opens the channel once the session is allowed in; the host refuses it while
//! the access check is pending or when file transfer is off (its setting or the server's
//! policy). Bulk data never shares the main channel, so input and control stay prompt.
//!
//! Flow, per request id (`req`, chosen by the viewer):
//! * [`FileMessage::List`] → one or more [`FileMessage::Entries`], the last with `last`.
//! * [`FileMessage::Get`] → [`FileMessage::Opened`], [`FileMessage::Data`] chunks,
//!   [`FileMessage::Done`] with the SHA-256 of what was sent.
//! * [`FileMessage::Put`] → [`FileMessage::Ack`] `{ received: 0 }` (go ahead), Data chunks,
//!   Done with the SHA-256 → [`FileMessage::Saved`] with where it landed. The name may
//!   carry the folder to save into (see [`put_name`]).
//! * Either side may send [`FileMessage::Cancel`]; a failed request ends with
//!   [`FileMessage::Error`]. `req` 0 means the channel itself (refused), and it closes.
//!
//! Flow control is a credit window: the side receiving data sends
//! [`FileMessage::Ack`] with the total bytes it has written; the sender keeps at most
//! [`TRANSFER_WINDOW`] bytes unacknowledged, so neither side buffers a whole file.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// WebRTC data-channel `protocol` (and label) of the file channel. A [`FileMessage`]
/// schema change bumps this, never edits v1 in place.
pub const FILES_CHANNEL_PROTOCOL: &str = "rconn-files/1";
/// Label of the file channel.
pub const FILES_CHANNEL_LABEL: &str = "rconn-files/1";

/// Bytes of file content per [`FileMessage::Data`]: just under 64 KiB, so a chunk and
/// its header fit the 64 KiB SCTP message size every WebRTC stack accepts.
pub const FILE_CHUNK_BYTES: usize = 64 * 1024 - 256;
/// Largest encoded [`FileMessage`]; anything bigger is refused before decoding.
pub const MAX_FILE_MESSAGE_BYTES: usize = 64 * 1024;
/// Unacknowledged bytes a sender may have in flight.
pub const TRANSFER_WINDOW: u64 = 4 * 1024 * 1024;
/// A receiver acknowledges at least this often (in bytes written).
pub const ACK_EVERY: u64 = 1024 * 1024;
/// Longest relative path accepted.
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest file name accepted in a [`FileMessage::Put`] (it is cleaned up and cut further).
pub const MAX_NAME_BYTES: usize = 1024;
/// Longest error text.
pub const MAX_ERROR_BYTES: usize = 1024;

/// Frozen for [`FILES_CHANNEL_PROTOCOL`] `rconn-files/1`: postcard encodes variants by
/// index, so variants and fields here (and in [`FileEntry`]) must never be reordered,
/// removed or inserted. New variants may only be appended at the end, and only where an
/// older peer that can't decode one is safe (it ignores the message); anything else goes
/// into a new protocol version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMessage {
    /// Viewer -> host: what is in this folder ("" is the root).
    List { req: u32, path: String },
    /// Host -> viewer: a page of a listing; `last` on the final page.
    Entries {
        req: u32,
        entries: Vec<FileEntry>,
        last: bool,
    },
    /// Viewer -> host: send this file.
    Get { req: u32, path: String },
    /// Host -> viewer: the file asked for is open; `size` bytes follow (as it was then).
    Opened { req: u32, size: u64 },
    /// File content, either direction. At most [`FILE_CHUNK_BYTES`].
    Data {
        req: u32,
        #[serde(with = "raw_bytes")]
        bytes: Vec<u8>,
    },
    /// End of the content, with the SHA-256 of every byte sent.
    Done { req: u32, sha256: [u8; 32] },
    /// Viewer -> host: a file of `size` bytes called `name` follows (once acknowledged).
    /// A `name` with a `/` names the folder too ([`put_name`], [`put_target`]); without,
    /// the host saves to its inbox (Downloads/Rconn).
    Put { req: u32, name: String, size: u64 },
    /// Host -> viewer: the upload was checked and saved here (relative to the root).
    Saved { req: u32, path: String },
    /// From the side receiving data: bytes written so far (0 accepts an upload).
    Ack { req: u32, received: u64 },
    /// Either side: stop this request; partial files are removed.
    Cancel { req: u32 },
    /// The request failed (`req` 0: the channel is refused and closes).
    Error { req: u32, message: String },
}

/// One file or folder in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Last modified, Unix seconds.
    pub modified: Option<u64>,
}

impl FileMessage {
    /// The request this message belongs to.
    pub fn req(&self) -> u32 {
        match self {
            Self::List { req, .. }
            | Self::Entries { req, .. }
            | Self::Get { req, .. }
            | Self::Opened { req, .. }
            | Self::Data { req, .. }
            | Self::Done { req, .. }
            | Self::Put { req, .. }
            | Self::Saved { req, .. }
            | Self::Ack { req, .. }
            | Self::Cancel { req }
            | Self::Error { req, .. } => *req,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, FileCodecError> {
        self.check_limits()?;
        let bytes = postcard::to_stdvec(self)?;
        if bytes.len() > MAX_FILE_MESSAGE_BYTES {
            return Err(FileCodecError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FileCodecError> {
        // Nothing in a message can be longer than the frame carrying it, so bounding the
        // frame bounds every allocation postcard makes.
        if bytes.len() > MAX_FILE_MESSAGE_BYTES {
            return Err(FileCodecError::TooLarge(bytes.len()));
        }
        let (msg, rest): (Self, _) = postcard::take_from_bytes(bytes)?;
        if !rest.is_empty() {
            return Err(FileCodecError::TrailingBytes(rest.len()));
        }
        msg.check_limits()?;
        Ok(msg)
    }

    fn check_limits(&self) -> Result<(), FileCodecError> {
        let over = |what: &'static str, len: usize, max: usize| {
            if len > max {
                Err(FileCodecError::FieldTooLong(what, len))
            } else {
                Ok(())
            }
        };
        match self {
            Self::List { path, .. } | Self::Get { path, .. } | Self::Saved { path, .. } => {
                over("path", path.len(), MAX_PATH_BYTES)
            }
            Self::Put { name, .. } => over("name", name.len(), MAX_NAME_BYTES),
            Self::Data { bytes, .. } => over("data", bytes.len(), FILE_CHUNK_BYTES),
            Self::Error { message, .. } => over("message", message.len(), MAX_ERROR_BYTES),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Error)]
pub enum FileCodecError {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("file message is {0} bytes, limit is {MAX_FILE_MESSAGE_BYTES}")]
    TooLarge(usize),
    #[error("{0} is {1} bytes, too long")]
    FieldTooLong(&'static str, usize),
    #[error("{0} unexpected trailing bytes")]
    TrailingBytes(usize),
}

/// `Vec<u8>` as one byte string (same wire form as a sequence of bytes in postcard, but
/// decoded in one copy instead of byte by byte).
mod raw_bytes {
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
        }
        d.deserialize_byte_buf(V)
    }
}

// ---------------------------------------------------------------------------
// Paths and names
// ---------------------------------------------------------------------------

/// Why a requested path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathRefused {
    #[error("the path contains a NUL character")]
    Nul,
    #[error("only paths inside the shared folder are allowed")]
    Absolute,
    #[error("\"..\" is not allowed in paths")]
    ParentDir,
    #[error("the path is too long")]
    TooLong,
}

/// The components of a viewer-supplied path relative to the shared root, refusing
/// anything that could name a place outside it: NUL, absolute paths (`/x`, `\x`, `C:\x`,
/// `C:x`, `\\server\share`), `..`, and `:` anywhere (drive letters, NTFS streams). `/` and
/// `\` both separate components on every platform; empty and `.` components are dropped.
/// Symlinks are the caller's to check, against the real file system.
pub fn relative_components(path: &str) -> Result<Vec<&str>, PathRefused> {
    if path.len() > MAX_PATH_BYTES {
        return Err(PathRefused::TooLong);
    }
    if path.contains('\0') {
        return Err(PathRefused::Nul);
    }
    if path.starts_with(['/', '\\']) || path.contains(':') {
        return Err(PathRefused::Absolute);
    }
    let mut out = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(PathRefused::ParentDir),
            p => out.push(p),
        }
    }
    Ok(out)
}

/// Longest file name [`sanitize_file_name`] produces, in bytes.
pub const MAX_SAVED_NAME_BYTES: usize = 200;

/// A received file name made safe to create in a folder on any OS: only the last path
/// component, no control characters, `<>:"/\|?*` replaced, no leading/trailing spaces or
/// trailing dots, Windows device names (`CON`, `COM1`, ...) defused, at most
/// [`MAX_SAVED_NAME_BYTES`] (keeping a short extension). Never empty: `"file"` if nothing is
/// left.
pub fn sanitize_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        // Control characters, and the invisible ones that reorder text ("txt.exe" shown
        // as "exe.txt").
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let mut s = cleaned.trim().trim_end_matches(['.', ' ']).to_owned();
    if s.is_empty() {
        return "file".into();
    }
    let stem = s.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit());
    if device {
        s.insert(0, '_');
    }
    if s.len() > MAX_SAVED_NAME_BYTES {
        let (base, ext) = split_extension(&s);
        let ext = if ext.len() <= 16 { ext } else { "" };
        let room = MAX_SAVED_NAME_BYTES - ext.len();
        let base = cut_to(base, room);
        s = format!("{base}{ext}");
    }
    s
}

/// The [`FileMessage::Put`] name for saving `file` into `folder` on the host (relative
/// to its shared root, "" for the root itself): `folder/file`, or `/file` for the root.
/// Hosts since 0.1.11 save there (if it is a folder inside the root); older ones keep
/// the last component, as they always did, and save to their inbox. A name too long for
/// the message is sent as `file` alone (the inbox).
pub fn put_name(folder: &str, file: &str) -> String {
    let name = format!("{}/{file}", folder.trim_end_matches(['/', '\\']));
    if name.len() > MAX_NAME_BYTES {
        file.to_owned()
    } else {
        name
    }
}

/// The host's reading of a [`FileMessage::Put`] name: `Some((folder, file))` when it
/// names a folder (split at the last `/`; the folder is "" for the root and still has to
/// pass [`relative_components`]), `None` for a plain name (the inbox).
pub fn put_target(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('/')
}

/// `name` with ` (n)` before its extension, for the n-th file of that name in a folder
/// (`n` <= 1 is the name itself): `report.pdf` → `report (2).pdf`, `.bashrc` →
/// `.bashrc (2)`.
pub fn numbered_name(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_owned();
    }
    let (base, ext) = split_extension(name);
    format!("{base} ({n}){ext}")
}

/// `("archive.tar", ".gz")`; a leading dot alone is not an extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// At most `max` bytes of `s`, cut on a character boundary.
fn cut_to(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        let msgs = [
            FileMessage::List {
                req: 1,
                path: "Documents/Reports".into(),
            },
            FileMessage::Entries {
                req: 1,
                entries: vec![
                    FileEntry {
                        name: "a.txt".into(),
                        is_dir: false,
                        size: 12,
                        modified: Some(1_700_000_000),
                    },
                    FileEntry {
                        name: "sub".into(),
                        is_dir: true,
                        size: 0,
                        modified: None,
                    },
                ],
                last: true,
            },
            FileMessage::Get {
                req: 2,
                path: "a.txt".into(),
            },
            FileMessage::Opened {
                req: 2,
                size: u64::MAX,
            },
            FileMessage::Data {
                req: 2,
                bytes: (0..=255).collect(),
            },
            FileMessage::Done {
                req: 2,
                sha256: [7; 32],
            },
            FileMessage::Put {
                req: 3,
                name: "photo.jpg".into(),
                size: 100 << 20,
            },
            FileMessage::Saved {
                req: 3,
                path: "Downloads/Rconn/photo.jpg".into(),
            },
            FileMessage::Ack {
                req: 3,
                received: 4 << 20,
            },
            FileMessage::Cancel { req: 3 },
            FileMessage::Error {
                req: 0,
                message: "file transfer is off".into(),
            },
        ];
        for m in msgs {
            let bytes = m.encode().unwrap();
            assert_eq!(FileMessage::decode(&bytes).unwrap(), m);
        }
    }

    /// The variant indexes are the wire format: they must never move.
    #[test]
    fn encoding_is_frozen() {
        assert_eq!(FileMessage::Cancel { req: 5 }.encode().unwrap(), [9, 5]);
        assert_eq!(
            FileMessage::Ack {
                req: 1,
                received: 2
            }
            .encode()
            .unwrap(),
            [8, 1, 2]
        );
        assert_eq!(
            FileMessage::Data {
                req: 1,
                bytes: vec![0xAB, 0xCD]
            }
            .encode()
            .unwrap(),
            [4, 1, 2, 0xAB, 0xCD],
            "raw bytes: same wire form as a byte sequence"
        );
        assert_eq!(
            FileMessage::List {
                req: 1,
                path: "a".into()
            }
            .encode()
            .unwrap(),
            [0, 1, 1, b'a']
        );
    }

    #[test]
    fn a_full_chunk_fits_one_sctp_message() {
        let m = FileMessage::Data {
            req: u32::MAX,
            bytes: vec![0; FILE_CHUNK_BYTES],
        };
        let bytes = m.encode().unwrap();
        assert!(bytes.len() <= MAX_FILE_MESSAGE_BYTES, "{}", bytes.len());
        assert_eq!(FileMessage::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn oversized_or_trailing_messages_are_refused() {
        let big = FileMessage::Data {
            req: 1,
            bytes: vec![0; FILE_CHUNK_BYTES + 1],
        };
        assert!(matches!(
            big.encode(),
            Err(FileCodecError::FieldTooLong("data", _))
        ));
        let raw = postcard::to_stdvec(&big).unwrap();
        assert!(FileMessage::decode(&raw).is_err());
        assert!(matches!(
            FileMessage::decode(&vec![0; MAX_FILE_MESSAGE_BYTES + 1]),
            Err(FileCodecError::TooLarge(_))
        ));
        let mut bytes = FileMessage::Cancel { req: 1 }.encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            FileMessage::decode(&bytes),
            Err(FileCodecError::TrailingBytes(1))
        ));
        let long_path = FileMessage::Get {
            req: 1,
            path: "a/".repeat(MAX_PATH_BYTES),
        };
        assert!(long_path.encode().is_err());
    }

    #[test]
    fn relative_paths_stay_inside_the_root() {
        assert_eq!(relative_components("").unwrap(), Vec::<&str>::new());
        assert_eq!(
            relative_components("Documents/./Reports//q3.pdf").unwrap(),
            ["Documents", "Reports", "q3.pdf"]
        );
        assert_eq!(relative_components("a\\b").unwrap(), ["a", "b"]);
        assert_eq!(
            relative_components("..").unwrap_err(),
            PathRefused::ParentDir
        );
        assert_eq!(
            relative_components("a/../../etc").unwrap_err(),
            PathRefused::ParentDir
        );
        assert_eq!(
            relative_components("a\\..\\b").unwrap_err(),
            PathRefused::ParentDir
        );
        for abs in [
            "/etc/passwd",
            "\\Windows",
            "C:\\Windows",
            "C:x",
            "\\\\srv\\share",
        ] {
            assert_eq!(
                relative_components(abs).unwrap_err(),
                PathRefused::Absolute,
                "{abs}"
            );
        }
        assert_eq!(
            relative_components("file.txt:stream").unwrap_err(),
            PathRefused::Absolute
        );
        assert_eq!(relative_components("a\0b").unwrap_err(), PathRefused::Nul);
        assert_eq!(
            relative_components(&"a".repeat(MAX_PATH_BYTES + 1)).unwrap_err(),
            PathRefused::TooLong
        );
        // Names that merely start with dots are ordinary.
        assert_eq!(
            relative_components("...x/.hidden").unwrap(),
            ["...x", ".hidden"]
        );
    }

    #[test]
    fn received_names_are_sanitized() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("C:\\Users\\x\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name("."), "file");
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name("dir/"), "file");
        assert_eq!(sanitize_file_name("  spaced .  "), "spaced");
        assert_eq!(sanitize_file_name("a\u{0}b\nc\u{7}.txt"), "abc.txt");
        assert_eq!(
            sanitize_file_name("invoice\u{202e}fdp.exe"),
            "invoicefdp.exe"
        );
        assert_eq!(sanitize_file_name("what?<>:\"|*.txt"), "what_______.txt");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("con.txt"), "_con.txt");
        assert_eq!(sanitize_file_name("COM1.log"), "_COM1.log");
        assert_eq!(sanitize_file_name("lpt9"), "_lpt9");
        assert_eq!(sanitize_file_name("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(sanitize_file_name("COMX"), "COMX");
        assert_eq!(sanitize_file_name(".bashrc"), ".bashrc");
        let long = format!("{}.tar.gz", "é".repeat(300));
        let s = sanitize_file_name(&long);
        assert!(s.len() <= MAX_SAVED_NAME_BYTES, "{}", s.len());
        assert!(s.ends_with(".gz"), "{s}");
        let no_ext = sanitize_file_name(&"x".repeat(500));
        assert_eq!(no_ext.len(), MAX_SAVED_NAME_BYTES);
    }

    #[test]
    fn upload_names_carry_the_folder_and_old_hosts_keep_the_file_name() {
        assert_eq!(put_name("Projects/Q3", "plan.pdf"), "Projects/Q3/plan.pdf");
        assert_eq!(put_name("Projects/", "plan.pdf"), "Projects/plan.pdf");
        assert_eq!(put_name("", "plan.pdf"), "/plan.pdf", "the root");
        assert_eq!(
            put_target("Projects/Q3/plan.pdf"),
            Some(("Projects/Q3", "plan.pdf"))
        );
        assert_eq!(put_target("/plan.pdf"), Some(("", "plan.pdf")));
        assert_eq!(put_target("plan.pdf"), None, "old viewers: the inbox");
        // What a host before 0.1.11 makes of them: the file name, in its inbox.
        assert_eq!(
            sanitize_file_name(&put_name("Projects/Q3", "plan.pdf")),
            "plan.pdf"
        );
        assert_eq!(sanitize_file_name(&put_name("", "plan.pdf")), "plan.pdf");
        // Too long for the message: the name alone.
        let deep = "d/".repeat(MAX_NAME_BYTES / 2);
        assert_eq!(put_name(&deep, "plan.pdf"), "plan.pdf");
        let fits = "d".repeat(MAX_NAME_BYTES - "/plan.pdf".len());
        assert_eq!(put_name(&fits, "plan.pdf").len(), MAX_NAME_BYTES);
        assert!(
            FileMessage::Put {
                req: 1,
                name: put_name(&fits, "plan.pdf"),
                size: 1
            }
            .encode()
            .is_ok()
        );
    }

    #[test]
    fn duplicate_names_are_numbered() {
        assert_eq!(numbered_name("report.pdf", 1), "report.pdf");
        assert_eq!(numbered_name("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered_name("archive.tar.gz", 3), "archive.tar (3).gz");
        assert_eq!(numbered_name("README", 2), "README (2)");
        assert_eq!(numbered_name(".bashrc", 2), ".bashrc (2)");
    }
}
