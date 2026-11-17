# skywave_gui

The Flutter head of skywave. Flutter renders and performs platform effects;
everything else is Rust: `native/hub` is the [rinf](https://pub.dev/packages/rinf)
bridge to `skywave-core` and `skywave-present`, and a member of the root
Cargo workspace. See `docs/architecture.md` ("Flutter head") and D23–D28 in
`docs/decisions.md`.

## Every platform

Needs the Flutter SDK, a Rust toolchain and the rinf CLI
(`cargo install rinf_cli`). The Dart bindings are generated and not
committed, so run `rinf gen` on a fresh checkout and after any change in
`native/hub/src/signals`:

```shell
cd gui
flutter pub get
rinf gen
flutter analyze
flutter test
```

`cargo test -p hub` runs the hub's tests from the repo root.

The hub crate's package name is `hub`, not `skywave-hub`: rinf 8.11's
cargokit looks for `lib<package name>.so`, while its gradle, CMake and pod
setups link `libhub`.

The rinf Dart package is vendored in `third_party/rinf` (8.11.0 minus
`example/`) and wired in through `dependency_overrides` in `pubspec.yaml`.
It carries one patch: the cargokit `gradle/plugin.gradle` compileSdk parse,
which AGP 9.3 needs (it reports `android-37.0`). On a rinf upgrade, re-vendor
the package, then re-apply the patch or drop it if rinf fixes it; otherwise
keep the copy verbatim.

## Linux (verified)

Needs the usual Flutter Linux desktop packages (clang, cmake, ninja,
pkg-config, GTK 3).

```shell
flutter build linux          # or: flutter run -d linux
build/linux/x64/release/bundle/skywave
```

Notifications go over D-Bus to the desktop's notification daemon; without
one they are skipped. Files open with `xdg-open`.

## Android (verified)

Needs the Android SDK with platform 37, an NDK, and the Rust Android
targets (`rustup target add aarch64-linux-android armv7-linux-androideabi
x86_64-linux-android i686-linux-android`); cargokit builds the hub per ABI
from Gradle.

```shell
flutter build apk --debug    # or: flutter run -d <device>
```

Gradle needs JDK 21–25: `flutter config --jdk-dir=/path/to/jdk` points
Flutter at one (Android Studio's bundled `jbr` works).

This is skywave's Android app (D28); its id is `io.github.lumpsoid.skywave`. At
launch it asks for ACCESS_LOCAL_NETWORK (API 37+, until granted) and
POST_NOTIFICATIONS (API 33+, once). The pairing screen's **Scan QR code**
opens ZXing's scanner (D20), which asks for the camera when it opens.

## macOS (scaffolded, never built)

Needs Xcode and the Rust Apple targets (`rustup target add
aarch64-apple-darwin x86_64-apple-darwin`).

```shell
flutter build macos
```

Both entitlements files carry `com.apple.security.network.client`. The
notification permission prompt comes at first launch.

## iOS (scaffolded, never built)

Needs Xcode, a signing team, and the Rust iOS targets (`rustup target add
aarch64-apple-ios aarch64-apple-ios-sim`).

```shell
flutter build ios --no-codesign
```

`Info.plist` has `NSLocalNetworkUsageDescription`; `AppDelegate.swift`
sets the notification centre delegate, and the notification permission
prompt comes at first launch.

## Windows (scaffolded, never built)

Needs Visual Studio with the C++ desktop workload and the Rust MSVC target.

```shell
flutter build windows
```

Notifications use the app user model id `io.github.lumpsoid.skywave` and a fixed
GUID (`lib/effects/notifications.dart`). An unpackaged build shows them but
cannot withdraw them; that needs MSIX packaging.

## Web

Not supported: skywave-core needs threads, TCP and bundled SQLite, none of
which wasm32 has (D26).

## Data

The app keeps its data in `<application support dir>/skywave` and pairs as
its own device (D25).
