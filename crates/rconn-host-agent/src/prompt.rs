//! Asking the person at the computer whether a guest may connect: the agent runs the Rconn
//! app beside it as `rconn --ask <json>`, which shows a small Accept / Decline window in
//! this desktop session and prints the answer.
//!
//! The agent already runs in the logged-in desktop session (a LaunchAgent on macOS, a
//! systemd user unit on Linux, the console session's default desktop on Windows), so the
//! window opens there. Anything but an explicit "accept" counts as no.

use rconn_core::guest::{ANSWER_ACCEPT, ANSWER_DECLINE, ANSWER_TIMEOUT, PromptRequest};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Decline,
    TimedOut,
    /// The window couldn't be shown (no app beside the agent, no desktop).
    CannotAsk(String),
}

/// The app that shows the question: `$RCONN_PROMPT_EXE`, else `rconn` beside this program.
fn prompt_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RCONN_PROMPT_EXE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let app = exe.with_file_name(format!("rconn{}", std::env::consts::EXE_SUFFIX));
    app.exists().then_some(app)
}

/// What the app printed, as an answer.
pub fn parse(stdout: &str) -> Answer {
    // The answer is a line of its own; anything else the app printed (a library's warning
    // on a misconfigured system) is ignored. The last answer line counts.
    let answer = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| [ANSWER_ACCEPT, ANSWER_DECLINE, ANSWER_TIMEOUT].contains(l));
    match answer {
        Some(ANSWER_ACCEPT) => Answer::Accept,
        Some(ANSWER_DECLINE) => Answer::Decline,
        Some(ANSWER_TIMEOUT) => Answer::TimedOut,
        _ => Answer::CannotAsk("the question window closed without an answer".into()),
    }
}

/// Show the question for `viewer` and wait for the answer (at most `timeout`, then a
/// little grace for the window to report; past that it is closed and counts as no answer).
pub async fn ask(viewer: &str, timeout: Duration) -> Answer {
    match prompt_exe() {
        Some(exe) => ask_with(&exe, viewer, timeout).await,
        None => Answer::CannotAsk("the Rconn app isn't installed beside the host agent".into()),
    }
}

async fn ask_with(exe: &std::path::Path, viewer: &str, timeout: Duration) -> Answer {
    let request = PromptRequest {
        viewer: viewer.to_owned(),
        timeout_secs: timeout.as_secs(),
    };
    let Ok(json) = serde_json::to_string(&request) else {
        return Answer::CannotAsk("couldn't encode the question".into());
    };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--ask")
        .arg(json)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // The session ended (or the answer took too long): close the window.
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Answer::CannotAsk(format!("{}: {e}", exe.display())),
    };
    match tokio::time::timeout(timeout + Duration::from_secs(10), child.wait_with_output()).await {
        Ok(Ok(out)) => parse(&String::from_utf8_lossy(&out.stdout)),
        Ok(Err(e)) => Answer::CannotAsk(e.to_string()),
        Err(_) => Answer::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_accept_lets_anyone_in() {
        assert_eq!(parse("accept\n"), Answer::Accept);
        assert_eq!(parse("decline\n"), Answer::Decline);
        assert_eq!(parse("timeout\n"), Answer::TimedOut);
        assert!(matches!(parse(""), Answer::CannotAsk(_)));
        assert!(matches!(parse("accepted\n"), Answer::CannotAsk(_)));
        // Ubuntu: a clipboard library warned on stdout before the answer (seen live).
        assert_eq!(
            parse("WARN arboard::platform::linux: Tried to initialize ...\naccept\n"),
            Answer::Accept
        );
        assert!(matches!(
            parse("the user said accept\n"),
            Answer::CannotAsk(_)
        ));
    }

    /// A stand-in for the app that answers at once (Unix: a shell script).
    #[cfg(unix)]
    #[tokio::test]
    async fn the_answer_comes_from_the_app() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let t = Duration::from_secs(5);
        // The request arrives as `--ask <json>`.
        let accept = script(
            "accept",
            r#"[ "$1" = "--ask" ] && case "$2" in *'"viewer":"Ana"'*'"timeout_secs":5'*) echo accept;; esac"#,
        );
        assert_eq!(ask_with(&accept, "Ana", t).await, Answer::Accept);
        let decline = script("decline", "echo decline");
        assert_eq!(ask_with(&decline, "Ana", t).await, Answer::Decline);
        let crash = script("crash", "exit 3");
        assert!(matches!(
            ask_with(&crash, "Ana", t).await,
            Answer::CannotAsk(_)
        ));
        let missing = dir.path().join("missing");
        assert!(matches!(
            ask_with(&missing, "Ana", t).await,
            Answer::CannotAsk(_)
        ));
    }
}
