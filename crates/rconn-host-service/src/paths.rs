//! Where the host keeps settings and state (admin-only on both OSes).

use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
        base.join("rconn")
    } else {
        PathBuf::from("/Library/Application Support/rconn")
    }
}

/// User-readable status for the local app ("This computer").
#[cfg_attr(not(windows), allow(dead_code))]
pub fn status_dir() -> PathBuf {
    data_dir().with_file_name("rconn-status")
}
