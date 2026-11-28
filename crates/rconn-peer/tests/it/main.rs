//! Integration tests of rconn-peer, built as one test program (one link instead of one
//! per file). Each file below is a module; run one with e.g.
//! `cargo nextest run -p rconn-peer -E 'test(file_transfer::)'`.

mod anonymous_guest;
mod audio_e2e;
mod codec_ladder;
mod datachannel_echo;
mod file_transfer;
mod login;
mod loss_repair;
mod resize_stream;
mod tls_pinning;
mod video_e2e;
