//! Cisco's prebuilt openh264 binaries. We must load Cisco's own build (not one we compile)
//! for Cisco's H.264 patent license to cover us, so the installer downloads the file
//! for the host's platform from Cisco and `openh264` verifies its SHA-256 on load.

/// openh264 release whose hashes are built into `openh264-sys2` 0.9.
pub const OPENH264_VERSION: &str = "2.7.0";

/// Cisco download host. Files are bzip2-compressed: `<file>.bz2`.
pub const CISCO_BASE_URL: &str = "http://ciscobinary.openh264.org";

/// File name of Cisco's binary for a Rust target triple, if Cisco ships one.
pub fn binary_name(target_os: &str, target_arch: &str) -> Option<&'static str> {
    Some(match (target_os, target_arch) {
        ("windows", "x86_64") => "openh264-2.7.0-win64.dll",
        ("windows", "aarch64") => "openh264-2.7.0-win-arm64.dll",
        ("windows", "x86") => "openh264-2.7.0-win32.dll",
        ("macos", "aarch64") => "libopenh264-2.7.0-mac-arm64.dylib",
        ("macos", "x86_64") => "libopenh264-2.7.0-mac-x64.dylib",
        ("linux", "x86_64") => "libopenh264-2.7.0-linux64.8.so",
        ("linux", "aarch64") => "libopenh264-2.7.0-linux-arm64.8.so",
        _ => return None,
    })
}

/// Binary name for the platform this crate was compiled for.
pub fn current_binary_name() -> Option<&'static str> {
    binary_name(std::env::consts::OS, std::env::consts::ARCH)
}

/// Download URL for the current platform's binary.
pub fn current_download_url() -> Option<String> {
    current_binary_name().map(|n| format!("{CISCO_BASE_URL}/{n}.bz2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipping_target_has_a_cisco_binary() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
        ] {
            let name = binary_name(os, arch).unwrap_or_else(|| panic!("no binary for {os}/{arch}"));
            assert!(name.contains(OPENH264_VERSION));
        }
        assert!(current_download_url().unwrap().ends_with(".bz2"));
    }
}
