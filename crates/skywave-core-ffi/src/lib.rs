//! UniFFI wrapper over `skywave-core`.
//!
//! Exports are declared with UniFFI proc macros (no UDL). The crate builds as
//! a `cdylib`/`staticlib` for apps and ships its own `uniffi-bindgen` binary
//! (behind the `bindgen` feature), so the generator always matches the
//! scaffolding version. Kotlin bindings land in package `dev.skywave.core`
//! (see `uniffi.toml`).
//!
//! Generate Kotlin with `crates/skywave-core-ffi/gen-kotlin.sh <out-dir> [profile]`,
//! which for the default `debug` profile runs:
//!
//! ```sh
//! cargo build -p skywave-core-ffi --lib
//! cargo run -p skywave-core-ffi --features bindgen --bin uniffi-bindgen -- \
//!     generate --library target/debug/libskywave_core_ffi.so \
//!     --language kotlin --out-dir <out-dir>
//! ```

uniffi::setup_scaffolding!();

/// The crate version and the wire protocol it speaks, e.g. `"0.1.0 (protocol 1)"`.
#[uniffi::export]
pub fn core_version() -> String {
    format!(
        "{} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        skywave_core::PROTOCOL_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_names_crate_and_protocol() {
        let expected = format!(
            "{} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            skywave_core::PROTOCOL_VERSION
        );
        assert_eq!(core_version(), expected);
        assert!(core_version().starts_with("0.1.0 (protocol "));
    }
}
