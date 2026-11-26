//! Access passwords the user chose to remember, kept in the system's credential store
//! (macOS Keychain, Windows Credential Manager, the Secret Service on Linux), never in a
//! file. One entry per server and computer ID.

const SERVICE: &str = "Rconn access password";

fn entry(server: &str, code: &str) -> Option<keyring::Entry> {
    let account = format!("{}@{server}", code.replace(' ', ""));
    match keyring::Entry::new(SERVICE, &account) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!(error = %e, "no credential store; passwords can't be remembered");
            None
        }
    }
}

/// The remembered password for computer `code` on `server`, if any.
pub fn get(server: &str, code: &str) -> Option<String> {
    entry(server, code)?.get_password().ok()
}

/// Remember `password` for computer `code`. False if the credential store refused.
pub fn set(server: &str, code: &str, password: &str) -> bool {
    match entry(server, code).map(|e| e.set_password(password)) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            tracing::warn!(error = %e, "couldn't remember the access password");
            false
        }
        None => false,
    }
}

/// Forget the password for computer `code` (nothing remembered is fine).
pub fn forget(server: &str, code: &str) {
    if let Some(e) = entry(server, code) {
        let _ = e.delete_credential();
    }
}
