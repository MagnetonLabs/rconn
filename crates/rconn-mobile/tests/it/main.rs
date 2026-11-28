//! Integration tests of rconn-mobile, built as one test program (one link instead of one
//! per file). Each file below is a module; run one with e.g.
//! `cargo nextest run -p rconn-mobile -E 'test(guest_session::)'`.

mod guest_session;
mod sound_and_files;
mod support;
