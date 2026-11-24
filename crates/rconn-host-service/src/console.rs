//! Launch a process in the active console session with this process's (SYSTEM) token.
//!
//! This is how the service reaches the interactive desktop from session 0: duplicate the
//! SYSTEM token, retarget it at the console session, and `CreateProcessAsUser`. Running as
//! SYSTEM inside the console session is what lets the agent open the Winlogon desktop
//! (lock screen, login, UAC prompts). Requires SeTcbPrivilege, which LocalSystem has.
//!
//! Unsafe is confined to this module (Win32 FFI). Every handle is closed via `Owned`.
#![allow(unsafe_code)]

use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
    TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_SHARE_READ, OPEN_ALWAYS, SetFilePointerEx,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, UpdateProcThreadAttribute,
};
use windows::core::{PCWSTR, PWSTR};

/// An owned Win32 handle, closed on drop.
pub struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Quote one argument per the MSVC CRT rules.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The console session, or an error if nobody is attached (headless before first logon
/// still has session 1 with LogonUI, so this mostly fails during session transitions).
pub fn console_session() -> Result<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == u32::MAX {
        bail!("no active console session");
    }
    Ok(id)
}

/// A process started by [`launch_in_session`].
pub struct Child {
    pub pid: u32,
    process: Owned,
}

impl Child {
    /// Still running?
    pub fn alive(&self) -> bool {
        use windows::Win32::Foundation::WAIT_TIMEOUT;
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }

    pub fn kill(&self) {
        use windows::Win32::System::Threading::TerminateProcess;
        unsafe {
            let _ = TerminateProcess(self.process.0, 1);
        }
    }
}

/// Start `exe args...` as SYSTEM in `session`, on `winsta0\default`, with stdout/stderr
/// appended to `log`.
pub fn launch_in_session(session: u32, exe: &Path, args: &[String], log: &Path) -> Result<Child> {
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own)
            .context("OpenProcessToken")?;
        let own = Owned(own);
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            own.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
        .context("DuplicateTokenEx")?;
        let primary = Owned(primary);
        SetTokenInformation(
            primary.0,
            TokenSessionId,
            (&session as *const u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
        .context(
            "SetTokenInformation(TokenSessionId) — needs SeTcbPrivilege (run as LocalSystem)",
        )?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let log_w = wide(log.as_os_str());
        let log_h = CreateFileW(
            PCWSTR(log_w.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ,
            Some(&sa),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .with_context(|| format!("opening log {}", log.display()))?;
        let log_h = Owned(log_h);
        SetFilePointerEx(log_h.0, 0, None, FILE_END).context("seeking log")?;

        // Inherit exactly one handle (the log). Plain bInheritHandles=TRUE would also pass
        // every other inheritable handle we hold, such as a parent's stdout pipe, and keep
        // it open for the agent's lifetime.
        let mut attr_size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(attrs), 1, None, &mut attr_size)
            .context("InitializeProcThreadAttributeList")?;
        let inherit = [log_h.0];
        UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(inherit.as_ptr().cast()),
            std::mem::size_of_val(&inherit),
            None,
            None,
        )
        .context("UpdateProcThreadAttribute")?;

        let mut desktop = wide(std::ffi::OsStr::new("winsta0\\default"));
        let si = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                dwFlags: STARTF_USESTDHANDLES,
                hStdOutput: log_h.0,
                hStdError: log_h.0,
                ..Default::default()
            },
            lpAttributeList: attrs,
        };
        let exe_w = wide(exe.as_os_str());
        let mut cmdline: Vec<u16> = {
            let mut s = quote(&exe.to_string_lossy());
            for a in args {
                s.push(' ');
                s.push_str(&quote(a));
            }
            wide(std::ffi::OsStr::new(&s))
        };
        let mut pi = PROCESS_INFORMATION::default();
        let created = CreateProcessAsUserW(
            Some(primary.0),
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR::null(),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attrs);
        drop(attr_buf);
        created.context("CreateProcessAsUserW")?;
        let _ = Owned(pi.hThread);
        Ok(Child {
            pid: pi.dwProcessId,
            process: Owned(pi.hProcess),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoting_follows_crt_rules() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("has space"), "\"has space\"");
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(""), "\"\"");
    }
}
