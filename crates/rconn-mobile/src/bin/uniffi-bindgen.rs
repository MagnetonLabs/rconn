//! `cargo run -p rconn-mobile --features bindgen --bin uniffi-bindgen -- generate ...`
//! (see ios/build-rust.sh).

fn main() {
    uniffi::uniffi_bindgen_main()
}
