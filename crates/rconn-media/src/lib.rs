//! Media building blocks. Phase 1 holds the codec loaders and the Cisco binary table;
//! capture and the encode loop arrive in phase 3.

#[cfg(target_os = "linux")]
pub mod audio_monitor;
pub mod caps;
pub mod capture;
#[cfg(not(target_os = "ios"))]
pub mod cisco;
pub mod codec;
pub mod copyrect;
#[cfg(windows)]
pub mod d3d11;
pub mod decode;
#[cfg(target_os = "linux")]
pub mod dmabuf;
pub mod h264;
#[cfg(windows)]
pub mod mf;
#[cfg(windows)]
pub mod mf_decode;
mod oh264;
#[cfg(all(windows, target_arch = "aarch64"))]
mod opus_shim;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod recovery;
pub mod refine;
pub mod sps;
#[cfg(target_os = "linux")]
pub mod vaapi;
pub mod video;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod vt;
#[cfg(target_os = "macos")]
pub mod vt_encode;
pub mod yuv;
