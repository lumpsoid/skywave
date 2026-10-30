//! UniFFI wrapper over `skywave-core`.
//!
//! Exports are declared with UniFFI proc macros (no UDL). The crate builds as
//! a `cdylib`/`staticlib` for apps and ships its own `uniffi-bindgen` binary
//! (behind the `bindgen` feature), so the generator always matches the
//! scaffolding version. Kotlin bindings land in package `dev.skywave.core`
//! (see `uniffi.toml`).
//!
//! [`view`] mirrors `ViewState` and everything reachable from it (core →
//! FFI); [`action`] mirrors `Action` and `Route` (FFI → core, failing on a
//! bad id). Both follow the type mapping of D16. A completion's `fill` and
//! a session's `prompts` are computed by the core during the conversion, so
//! the head holds no logic of its own (D2).
//!
//! [`client::SkywaveCore`] is the core itself: `open` it once per process
//! with the device name, `subscribe` a [`client::ViewListener`] for its
//! snapshots, `dispatch` actions, `suspend`/`resume` with the app, and
//! `shutdown` when done (D17). [`helpers`] holds the few pure functions a
//! head needs besides; every failure is a [`error::SkywaveError`].
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
//!
//! Build the Android libraries with
//! `crates/skywave-core-ffi/build-android.sh <jniLibs-dir> [profile]`, which
//! for the default size-optimised `release-android` profile runs:
//!
//! ```sh
//! cargo ndk --platform 26 -t arm64-v8a -t x86_64 -o <jniLibs-dir> \
//!     build -p skywave-core-ffi --lib --profile release-android
//! ```
//!
//! Call [`logging::init_logging`] once at startup: on Android the core's
//! `tracing` output then shows in logcat under tag `skywave`, elsewhere on
//! stderr.

uniffi::setup_scaffolding!();

pub mod action;
pub mod client;
pub mod error;
pub mod helpers;
pub mod logging;
pub mod view;

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
