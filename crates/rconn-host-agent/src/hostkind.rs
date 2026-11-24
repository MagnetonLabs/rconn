//! Detect whether this host is a VM or a physical machine (drives the indicator default).

use rconn_core::settings::{HostKind, classify_host};
use std::process::Command;

pub fn detect() -> HostKind {
    let (manufacturer, product, hv) = signals();
    let kind = classify_host(&manufacturer, &product, hv);
    tracing::info!(%manufacturer, %product, hypervisor = hv, ?kind, "host type detected");
    kind
}

#[cfg(windows)]
fn signals() -> (String, String, bool) {
    let read = |name: &str| -> String {
        Command::new("reg")
            .args([
                "query",
                r"HKLM\HARDWARE\DESCRIPTION\System\BIOS",
                "/v",
                name,
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|out| {
                out.lines()
                    .find(|l| l.contains("REG_SZ"))
                    .map(|l| l.split("REG_SZ").nth(1).unwrap_or("").trim().to_owned())
            })
            .unwrap_or_default()
    };
    (read("SystemManufacturer"), read("SystemProductName"), false)
}

#[cfg(target_os = "macos")]
fn signals() -> (String, String, bool) {
    let sysctl = |name: &str| -> String {
        Command::new("sysctl")
            .args(["-n", name])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    (
        "Apple Inc.".into(),
        sysctl("hw.model"),
        sysctl("kern.hv_vmm_present") == "1",
    )
}

#[cfg(not(any(windows, target_os = "macos")))]
fn signals() -> (String, String, bool) {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    (
        read("/sys/class/dmi/id/sys_vendor"),
        read("/sys/class/dmi/id/product_name"),
        false,
    )
}
