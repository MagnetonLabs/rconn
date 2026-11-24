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
