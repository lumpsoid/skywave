#!/usr/bin/env sh
# Cross-compile skywave-core-ffi for Android into a jniLibs tree:
#   <jniLibs-dir>/arm64-v8a/libskywave_core_ffi.so
#   <jniLibs-dir>/x86_64/libskywave_core_ffi.so
# Usage: build-android.sh <jniLibs-dir> [profile]   (default release-android)
# Needs cargo-ndk (`cargo install cargo-ndk`), the Rust targets
# aarch64-linux-android and x86_64-linux-android, and an NDK that cargo-ndk
# finds (ANDROID_NDK_HOME, or the newest under $ANDROID_HOME/ndk).
# ABIs and API level follow docs/decisions.md D19 (minSdk 26).
set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <jniLibs-dir> [profile]" >&2
    exit 2
fi
mkdir -p "$1"
out_dir=$(cd "$1" && pwd)
profile=${2:-release-android}

cd "$(dirname "$0")/../.."

cargo ndk --platform 26 -t arm64-v8a -t x86_64 -o "$out_dir" \
    build -p skywave-core-ffi --lib --profile "$profile"
