//! Client side of an rconn session, shared by the viewer and the host agent:
//! a signaling client ([`signaling`]) and a WebRTC peer wrapper ([`rtc`]).

pub mod rtc;
pub mod signaling;
