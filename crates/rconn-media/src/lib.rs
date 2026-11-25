//! Media building blocks. Phase 1 holds the codec loaders and the Cisco binary table;
//! capture and the encode loop arrive in phase 3.

pub mod capture;
#[cfg(not(target_os = "ios"))]
pub mod cisco;
pub mod codec;
#[cfg(windows)]
pub mod mf;
#[cfg(all(windows, target_arch = "aarch64"))]
mod opus_shim;
pub mod video;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod vt;
pub mod yuv;
