//! Client side of an rconn session, shared by the viewer and the host agent:
//! a signaling client ([`signaling`]) and a WebRTC peer wrapper ([`rtc`]).

pub mod clipboard;
pub mod rtc;
pub mod session;
pub mod signaling;
pub mod trust;
pub mod video;
