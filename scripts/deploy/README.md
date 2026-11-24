# Deploying skywaved

`scripts/deploy/deploy.sh` builds `skywaved` as a static musl binary for the
device's CPU, copies it and the systemd user unit over ssh, and runs
`install.sh` on the device. The device runs the server as a user service, as
you, so the agent finds your `claude` login. Run the script on your own
machine, from anywhere in the repo. With `--local` the device is the machine
you build on.

## Before the first deploy

On this machine, install `cargo-zigbuild` and `zig`, and add the Rust targets
for the devices you deploy to. `setup-build.sh` does all three, as you and
without sudo:

```sh
scripts/deploy/setup-build.sh
scripts/deploy/setup-build.sh --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl
```

- `zig`: unless one is already on `PATH`, it installs the latest stable release
  from ziglang.org, checks its SHA-256 against the release index, unpacks it
  into `~/.local/opt` and links `~/.local/bin/zig` to it. `--zig <version>`
  installs that version even if a zig is already installed, which also
  upgrades one the script installed before.
- `cargo-zigbuild`: `cargo install --locked cargo-zigbuild`, unless it is
  installed already.
- Rust targets: `rustup target add` for each `--target`. Without `--target`,
  only the musl target for this machine's CPU, which is what `--local`
  needs.

Running it again only does what is missing.

On the device, once:

1. Install the `claude` CLI and log in to it as the user that will run the
   service. Have `git` installed too.
2. Let the user manager run while nobody is logged in:

   ```sh
   loginctl enable-linger "$USER"
   ```

   Without this, the service stops when your last ssh session ends. The deploy
   script does not do it for you.
3. Write `~/.config/skywave/skywaved.toml`. Every key is optional, but without
   `listen` the server binds `127.0.0.1:7443`, which other machines cannot
   reach:

   ```toml
   listen = "0.0.0.0:7443"
   roots = ["/home/me/code"]
   server_names = ["192.168.1.20"]
   ```

   `roots` is where sessions may run. `skywaved pair` advertises the first
   entry of `server_names`, so put the address your devices dial first.
4. Open TCP port 7443 in the device's firewall.

## Deploy

```sh
scripts/deploy/deploy.sh <ssh-host>
```

`<ssh-host>` is any ssh destination, including an alias from `~/.ssh/config`.

The script probes the device's CPU with `uname -m` (`x86_64`, or `aarch64` or
`arm64`), builds with `cargo zigbuild --release --locked -p skywaved`, copies
the binary to `~/.local/bin/skywaved` and the unit to
`~/.config/systemd/user/skywaved.service`, then runs `install.sh` on the
device. `install.sh` first runs the new binary with `--version`, so a binary
for the wrong CPU stops the deploy before anything is replaced.

- The first run starts the service.
- A later run restarts it after running work finishes: `install.sh` runs
  `skywaved restart --after-work`, which waits for running agent turns and
  shell runs to finish, then re-execs the new binary in the same process.
- If the unit file changed, the run restarts the service right away, as
  `--now` does.

Flags:

- `--now`: restart right away, without waiting for running work. Running work
  is lost.
- `--target <triple>`: build for this triple and skip the `uname -m` probe.
  Pass it when the probe cannot name the device's CPU.
- `--build-only`: build, print the path of the binary and stop. With
  `--target` it needs no host, so `scripts/deploy/deploy.sh --build-only
  --target x86_64-unknown-linux-musl` builds without touching a device.
- `--local`: deploy to this machine instead of an ssh host. See below.

## Deploy to this machine

When the machine that builds `skywaved` also runs it:

```sh
scripts/deploy/deploy.sh --local
```

It works like an ssh deploy without ssh: it reads the CPU from the local
`uname -m`, copies the binary and unit into your own `~/.local/bin` and
`~/.config/systemd/user`, and runs `install.sh` here. `--now` and `--target`
work the same way. The one-time setup in "Before the first deploy" still
applies, both the build tools and the device steps. Afterwards, run
`~/.local/bin/skywaved status` and `journalctl --user -u skywaved` directly.

## Pair

After the first deploy, pair a device as in the main README:

```sh
ssh -t <ssh-host> .local/bin/skywaved pair
```

The path is relative to your home directory, because a non-login ssh shell
may not have `~/.local/bin` on `PATH`. Then follow
[step 2 of the main README](../../README.md#2-pair-a-device) on your device.

## After deploying

```sh
ssh <ssh-host> .local/bin/skywaved status
ssh <ssh-host> journalctl --user -u skywaved
```

`status` asks the running daemon for its state. It exits non-zero when no
daemon answers or the daemon is stopping. Logs go to the journal unless
`log_file` is set in the config.

## Moving from the old unit

The old unit ran `~/.cargo/bin/skywaved`. The first deploy moves the binary to
`~/.local/bin` and changes the unit, so it restarts the daemon right away
rather than after work. Running work is lost. Afterwards,
`~/.cargo/bin/skywaved` is no longer used and can be removed.
