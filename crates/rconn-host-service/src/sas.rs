//! Ctrl+Alt+Delete for the agent.
//!
//! Windows ignores Ctrl+Alt+Delete pressed with `SendInput`; `SendSAS` (sas.dll) makes
//! Winlogon act on it, but only when called from session 0. The agent runs as SYSTEM in
//! the console session, so it asks this service over a named pipe that only SYSTEM can
//! open; the service answers each request with `SendSAS`.
#![allow(unsafe_code)]

use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::{s, w};

/// The pipe the agent writes `S` to (see `rconn-host-agent`'s Windows input).
pub const PIPE: &str = r"\\.\pipe\rconn-host-sas";

/// Answer Ctrl+Alt+Delete requests until the process ends (run on its own thread).
pub fn serve() {
    let mut last: Option<Instant> = None;
    loop {
        if let Err(e) = serve_one(&mut last) {
            tracing::warn!(error = %e, "Ctrl+Alt+Delete pipe");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn serve_one(last: &mut Option<Instant>) -> Result<()> {
    let pipe = Pipe::create()?;
    match unsafe { ConnectNamedPipe(pipe.0, None) } {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
        Err(e) => return Err(e).context("ConnectNamedPipe"),
    }
    let mut buf = [0u8; 1];
    let mut read = 0u32;
    unsafe { ReadFile(pipe.0, Some(&mut buf), Some(&mut read), None) }.context("ReadFile")?;
    let mut answer = b"0";
    // At most one per second: a stuck key can't flood Winlogon.
    let due = last.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
    if read == 1 && buf[0] == b'S' && due {
        *last = Some(Instant::now());
        if send_sas() {
            answer = b"1";
        }
    }
    let _ = unsafe { WriteFile(pipe.0, Some(answer), None, None) };
    let _ = unsafe { DisconnectNamedPipe(pipe.0) };
    Ok(())
}

struct Pipe(HANDLE);

impl Pipe {
    fn create() -> Result<Self> {
        // Protected DACL: full access for SYSTEM only (the agent runs as SYSTEM).
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;SY)"),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("pipe security descriptor")?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let name: Vec<u16> = PIPE.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                16,
                16,
                0,
                Some(&sa),
            )
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if handle == INVALID_HANDLE_VALUE {
            return Err(windows::core::Error::from_thread()).context("CreateNamedPipeW");
        }
        Ok(Self(handle))
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// `SendSAS(FALSE)`, after making sure the SoftwareSASGeneration policy lets services
/// send it. Returns whether it was sent.
fn send_sas() -> bool {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    if let Err(e) = allow_software_sas() {
        tracing::warn!(error = %e, "can't allow Ctrl+Alt+Delete from software");
    }
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else {
            tracing::warn!("sas.dll not found; Ctrl+Alt+Delete not sent");
            return false;
        };
        let Some(f) = GetProcAddress(lib, s!("SendSAS")) else {
            tracing::warn!("SendSAS missing from sas.dll");
            return false;
        };
        // SendSAS(BOOL AsUser): FALSE, sent as a service.
        let send: unsafe extern "system" fn(i32) = std::mem::transmute(f);
        send(0);
    }
    tracing::info!("Ctrl+Alt+Delete sent to Windows");
    true
}

/// SendSAS needs HKLM\...\Policies\System\SoftwareSASGeneration to include services
/// (1 = services, 3 = services and Ease of Access apps). Add services if it doesn't.
fn allow_software_sas() -> windows::core::Result<()> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RegGetValueW, RegSetKeyValueW,
    };
    let key = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
    let name = w!("SoftwareSASGeneration");
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    if read.is_ok() && value & 1 == 1 {
        return Ok(());
    }
    let wanted: u32 = if read.is_ok() { value | 1 } else { 1 };
    unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            key,
            name,
            REG_DWORD.0,
            Some((&raw const wanted).cast()),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .ok()?;
    tracing::info!(
        wanted,
        "allowed services to send Ctrl+Alt+Delete (SoftwareSASGeneration)"
    );
    Ok(())
}
