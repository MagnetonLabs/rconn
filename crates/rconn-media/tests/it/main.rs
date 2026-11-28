//! Integration tests of rconn-media, built as one test program (one link instead of one
//! per file). Each file below is a module; run one with e.g.
//! `cargo nextest run -p rconn-media -E 'test(h264_roundtrip::)'`.

mod encode_bench;
mod h264_roundtrip;
