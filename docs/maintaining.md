# Maintaining Skywave

The jobs that keep the project releasable, in the order you meet them.

## Fresh checkout

```sh
git clone git@github.com:lumpsoid/skywave.git && cd skywave
rustup show                                  # Rust per Cargo.toml's rust-version
cargo install rinf_cli --version 8.11.0 --locked
(cd gui && flutter pub get && rinf gen)      # Flutter 3.47.9; bindings are not committed
scripts/check.sh
```

For Android release builds, put `gui/android/key.properties` in place (see
[Signing key](#signing-key)); without it a release build is debug-signed.

## Everyday

- `scripts/check.sh` before pushing: the same gates as CI, in CI's order,
  stopping at the first failure. `--no-gui` skips the Flutter head.
- Every user-visible change adds a line to `CHANGELOG.md` under
  `[Unreleased]` (Added, Changed, Fixed, Removed).
- A wire change bumps `PROTOCOL_VERSION` once per batch and adds a
  migration step on both sides (CLAUDE.md, "Rules").

## Version

The version lives in two files that `scripts/version.sh` keeps in step:

| File | Holds |
|------|-------|
| `Cargo.toml` `[workspace.package] version` | `X.Y.Z`, every crate inherits it |
| `gui/pubspec.yaml` `version` | `X.Y.Z+N`; `N` is Android's `versionCode`, which must grow with every release |

```sh
scripts/version.sh show          # 0.1.0+1
scripts/version.sh check         # fails if the two disagree (CI runs it)
scripts/version.sh bump 0.2.0    # both files, N += 1, dates the changelog, refreshes Cargo.lock
```

Binaries and the app show `0.1.0 (2391c1d)`: the version and the commit,
from `crates/skywave-version`. Before 1.0, bump the minor version for a
release that breaks the wire protocol or stored data, the patch version
otherwise.

## Release

1. On an up-to-date, clean `main`: `scripts/version.sh bump X.Y.Z`.
2. Read the new `## [X.Y.Z]` section of `CHANGELOG.md` as a user would and
   fix it up.
3. `git commit -m "chore(release): X.Y.Z" Cargo.toml Cargo.lock gui/pubspec.yaml CHANGELOG.md`
4. `scripts/release.sh`: checks the branch, the tree, the versions, the
   changelog and that the `versionCode` is above every earlier tag's, runs
   every gate, and tags `vX.Y.Z` with the changelog section as the message.
   It never pushes.
5. `git push origin main vX.Y.Z`

The tag starts `.github/workflows/release.yml`. Its `verify` job runs
`scripts/release.sh verify vX.Y.Z`: the same release checks, plus that the
tag is annotated, names the version and is on `main`. Then it runs every CI
gate on the tagged commit (`ci.yml`, called as a reusable workflow) and,
beside them, builds:

- `skywave-X.Y.Z-x86_64-linux.tar.gz` and `skywave-X.Y.Z-aarch64-linux.tar.gz`:
  static (musl) `skywaved` and `skywave-tui`, the licenses, the README, the
  changelog and the systemd unit;
- `skywave-X.Y.Z.apk`, signed with the release key;

Only when the gates and the builds all pass does it publish them on a
GitHub release with `SHA256SUMS`, using the tag's annotation as the notes.
If the release fails, fix it on `main`, delete the tag locally and on
GitHub (`git push origin :vX.Y.Z`), and tag again. Never re-use a
published `versionCode`.

## Signing key

Android only updates an app signed with the same key, so this key is for
the life of the app.

Create it once, outside the repo:

```sh
keytool -genkeypair -v -keystore ~/keys/skywave-upload.jks -alias skywave \
  -keyalg RSA -keysize 4096 -validity 36500
chmod 600 ~/keys/skywave-upload.jks
```

Save the keystore file and both passwords in a password manager. A lost
key means no more updates for anyone who installed the app.

Local release builds read `gui/android/key.properties` (gitignored):

```properties
storeFile=/home/you/keys/skywave-upload.jks
storePassword=...
keyAlias=skywave
keyPassword=...
```

The release workflow reads the same from repository secrets, and fails
without them rather than publish a debug-signed APK:

```sh
base64 -w0 ~/keys/skywave-upload.jks | gh secret set ANDROID_KEYSTORE_BASE64 -R lumpsoid/skywave
gh secret set ANDROID_KEYSTORE_PASSWORD -R lumpsoid/skywave   # prompts
gh secret set ANDROID_KEY_ALIAS -R lumpsoid/skywave
gh secret set ANDROID_KEY_PASSWORD -R lumpsoid/skywave
```

Check a build's certificate with a single build-tools version:

```sh
$ANDROID_HOME/build-tools/<version>/apksigner verify --print-certs app-release.apk
```

It must show your CN, not `CN=Android Debug`.

## Dependency updates

Dependabot opens weekly grouped pull requests for Cargo, pub, Gradle and
GitHub Actions; CI runs on each. Merge them when green. These move by hand,
together:

- **rinf**: the crate (`Cargo.toml`, `rinf = "X"`), the Dart package
  vendored in `gui/third_party/rinf` (with its cargokit patch, see
  `gui/pubspec.yaml`), and `RINF_CLI_VERSION` in both workflows plus the
  `cargo install rinf_cli` lines in CONTRIBUTING.md and this file.
  Dependabot ignores rinf.
- **Rust**: `rust-version` in `Cargo.toml`. The workflows install that
  version, so CI follows the bump.
- **Flutter**: pinned to 3.47.9 (`flutter-version` in both workflows, and
  CONTRIBUTING.md). Not 3.50: its Linux Wayland renderer can deadlock
  (flutter/flutter#196529, see `gui/lib/boot/startup_page.dart`). Move the
  pin only after running the Linux desktop app on the new version; the
  `sdk` constraint in `gui/pubspec.yaml` must still resolve on it.
- **Claude Code**: after a `claude` upgrade, run `skywave-claude-compat
  --quick`, then `--full` (see `docs/agents/claude-code.md`).

## One-time GitHub settings

- Description and topics (`coding-agent`, `claude-code`, `remote`,
  `offline-first`, `rust`, `flutter`).
- Settings → Code security: Dependabot alerts and security updates on.
- Settings → Actions → General: workflow permissions "Read repository
  contents" (the release job asks for write itself).
- Optional: a branch rule on `main` that requires the CI checks `rust` and
  `gui` to pass.
- The four signing secrets above.
