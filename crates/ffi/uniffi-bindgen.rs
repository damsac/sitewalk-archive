//! Dev-only entry point for `uniffi-bindgen` (Task 9). Not part of the
//! shipped library — gated behind the `uniffi-bindgen-cli` feature so
//! `uniffi`'s `cli` feature never lands in the iOS cdylib/staticlib.
//!
//! Usage: cargo run -p ffi --features uniffi-bindgen-cli --bin uniffi-bindgen -- \
//!   generate --library <path-to-libffi.{dylib,so}> --language swift --out-dir <dir>

fn main() {
    uniffi::uniffi_bindgen_main()
}
