#!/usr/bin/env sh
# Generate Kotlin bindings for skywave-core-ffi with the crate's own bindgen.
# Usage: gen-kotlin.sh <out-dir> [profile]   (profile: debug | release | custom, default debug)
# The profile only picks which host build of the cdylib the bindgen reads; the
# generated Kotlin is the same for every profile. Output is not run through
# ktlint (--no-format): it is build-time generated code, never checked in.
set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <out-dir> [profile]" >&2
    exit 2
fi
mkdir -p "$1"
out_dir=$(cd "$1" && pwd)
profile=${2:-debug}

cd "$(dirname "$0")/../.."

# The lib is built with the same `bindgen` feature as the `cargo run` below,
# so both share one build of skywave-core-ffi: with differing feature sets
# each command would recompile (and overwrite) the other's. The feature only
# pulls in the bindgen CLI; it changes nothing the library exports.
case "$profile" in
    debug | dev)
        cargo build -p skywave-core-ffi --lib --features bindgen
        profile_dir=debug
        ;;
    *)
        cargo build -p skywave-core-ffi --lib --features bindgen --profile "$profile"
        profile_dir=$profile
        ;;
esac

target_dir=$(cargo metadata --format-version 1 --no-deps |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
case "$(uname -s)" in
    Darwin) lib=libskywave_core_ffi.dylib ;;
    *) lib=libskywave_core_ffi.so ;;
esac

cargo run -p skywave-core-ffi --features bindgen --bin uniffi-bindgen -- \
    generate --library "$target_dir/$profile_dir/$lib" \
    --language kotlin --no-format --out-dir "$out_dir"
