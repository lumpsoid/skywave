# skywave_gui

The Flutter head of skywave. Flutter renders and performs platform effects;
everything else is Rust: `native/hub` is the [rinf](https://pub.dev/packages/rinf)
bridge to `skywave-core`, and a member of the root Cargo workspace.

## Build

Needs the Flutter SDK, a Rust toolchain and the rinf CLI
(`cargo install rinf_cli`). The Dart bindings are generated and not
committed, so on a fresh checkout and after any change in
`native/hub/src/signals`:

```shell
rinf gen
flutter analyze
flutter test
flutter build linux          # or: flutter run -d linux
flutter build apk --debug    # needs the Android SDK and the rust android targets
```

`cargo test -p hub` runs the hub's tests from the repo root.

The hub crate's package name is `hub`, not `skywave-hub`: rinf 8.11's
cargokit looks for `lib<package name>.so`, while its gradle, CMake and pod
setups link `libhub`.

## Platforms

- **Linux, Android:** built and run.
- **macOS, iOS, Windows:** scaffolded only (platform folders, cargokit
  wiring, network entitlement and local-network usage string). Not built:
  no toolchain was at hand.
- **Web:** not supported. skywave-core needs threads, TCP and bundled
  SQLite, none of which wasm32 has.

The app keeps its data in `<application support dir>/skywave` and pairs as
its own device. On Android its id is `dev.skywave.gui`, so it installs beside
the Kotlin app (`dev.skywave.android`).
