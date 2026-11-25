//! Media building blocks. Phase 1 holds the codec loaders and the Cisco binary table;
//! capture and the encode loop arrive in phase 3.

pub mod capture;
pub mod cisco;
pub mod codec;
#[cfg(windows)]
pub mod mf;
#[cfg(all(windows, target_arch = "aarch64"))]
mod opus_shim;
pub mod video;
pub mod yuv;
