//! `skywaved.toml`: where the daemon listens, keeps its data and runs agents.
//!
//! The file is found at `--config <path>`, or else at
//! `$XDG_CONFIG_HOME/skywave/skywaved.toml` (`~/.config/...` when unset).
//! A missing default file means "all defaults"; a missing `--config` file is
//! an error. Relative paths in the file resolve against the file's directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::attachments::STAGING_MAX_BYTES;
use crate::disk::DEFAULT_MIN_FREE;
use crate::error::{Error, IoContext, Result};
use crate::screen::{DEFAULT_ADB, ScreenConfig};
use crate::shell::{DEFAULT_OUTPUT_MAX, DEFAULT_PROGRAM, ShellConfig};
use crate::workdir::DEFAULT_DOWNLOAD_MAX;

/// Default `listen` address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7443";
/// Default `agent.command`, looked up on `PATH`.
pub const DEFAULT_AGENT_COMMAND: &str = "claude";
/// Default `[files] upload_max`: the largest file a `PUT` stages (D55).
pub const DEFAULT_UPLOAD_MAX: u64 = 64 << 20;

/// The daemon configuration, with every default and relative path resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Address the HTTPS API binds to.
    pub listen: SocketAddr,
    /// Root of the data directory (see [`crate::data_dir`]).
    pub data_dir: PathBuf,
    /// Directories sessions may run in (D14). Empty allows none.
    pub roots: Vec<PathBuf>,
    pub agent: AgentConfig,
    /// Log to this file instead of stderr.
    pub log_file: Option<PathBuf>,
    /// Extra DNS names or IP addresses for the server certificate, on top of
    /// the derived ones (see [`crate::identity::server_names`]).
    pub server_names: Vec<String>,
    /// Shell commands from devices (`[shell]`); off by default.
    pub shell: ShellConfig,
    /// `[screen]`: the screen runner (D65).
    pub screen: ScreenConfig,
    /// The data dir's disk (`[disk]`).
    pub disk: DiskConfig,
    /// Files moving between devices and the server (`[files]`).
    pub files: FilesConfig,
}

/// How much of the data dir's disk the daemon keeps free ([`crate::disk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskConfig {
    /// Below this many free bytes the disk is low: agents are paused and
    /// new work is refused until space is freed (`min_free`, default
    /// [`DEFAULT_MIN_FREE`]). 0 turns the pause off.
    pub min_free: u64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig {
            min_free: DEFAULT_MIN_FREE,
        }
    }
}

/// Files moving between devices and the server (`[files]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesConfig {
    /// Largest workdir file `GET /v1/workdir/download` serves, in bytes (D54).
    pub download_max: u64,
    /// Largest file `PUT /v1/attachments` stages, in bytes (D55); at most
    /// [`crate::attachments::STAGING_MAX_BYTES`].
    pub upload_max: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        FilesConfig {
            download_max: DEFAULT_DOWNLOAD_MAX,
            upload_max: DEFAULT_UPLOAD_MAX,
        }
    }
}

/// How to start the agent process.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    /// Executable path, or a bare name looked up on `PATH`.
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Whether a device may switch a session to bypass mode, where every
    /// tool runs without asking (`allow_bypass`); off by default (D38).
    pub allow_bypass: bool,
}

/// The file as written; every field optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<SocketAddr>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default)]
    agent: RawAgent,
    log_file: Option<PathBuf>,
    #[serde(default)]
    server_names: Vec<String>,
    #[serde(default)]
    shell: RawShell,
    #[serde(default)]
    screen: RawScreen,
    #[serde(default)]
    disk: RawDisk,
    #[serde(default)]
    files: RawFiles,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisk {
    #[serde(default, deserialize_with = "size")]
    min_free: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFiles {
    #[serde(default, deserialize_with = "size")]
    download_max: Option<u64>,
    #[serde(default, deserialize_with = "size")]
    upload_max: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    command: Option<PathBuf>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    allow_bypass: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShell {
    #[serde(default)]
    enabled: bool,
    program: Option<PathBuf>,
    rc: Option<PathBuf>,
    #[serde(default, deserialize_with = "size")]
    output_max: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScreen {
    #[serde(default)]
    enabled: bool,
    adb: Option<PathBuf>,
}

/// A size as `parse_size` reads it, or a bare integer of bytes.
fn size<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bytes(u64),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Bytes(bytes) => Ok(Some(bytes)),
        Raw::Text(text) => parse_size(&text)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Parses a size such as `"64MiB"`: a whole number, optionally followed by
/// `B`, `KiB`, `MiB` or `GiB` (binary multiples), spaces allowed between.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("size {text:?}: expected a number of bytes, e.g. \"64MiB\""))?;
    let shift = match unit.trim_start() {
        "" | "B" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        other => {
            return Err(format!(
                "size {text:?}: unknown unit {other:?} (use B, KiB, MiB or GiB)"
            ));
        }
    };
    number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("size {text:?} is too large"))
}

/// Environment lookups the config depends on; injectable for tests.
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

/// The process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// `$XDG_CONFIG_HOME/skywave/skywaved.toml`, falling back to `~/.config`.
pub fn default_config_path(env: &impl Env) -> Result<PathBuf> {
    xdg_dir(env, "XDG_CONFIG_HOME", ".config")
        .map(|d| d.join("skywave").join("skywaved.toml"))
        .ok_or(Error::NoConfigDir)
}

/// `$XDG_DATA_HOME/skywave`, falling back to `~/.local/share`.
pub fn default_data_dir(env: &impl Env) -> Result<PathBuf> {
    xdg_dir(env, "XDG_DATA_HOME", ".local/share")
        .map(|d| d.join("skywave"))
        .ok_or(Error::NoDataDir)
}

/// XDG base dirs must be absolute; relative values are ignored per the spec.
fn xdg_dir(env: &impl Env, var: &str, home_fallback: &str) -> Option<PathBuf> {
    env.var(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env.var("HOME")
                .map(|h| PathBuf::from(h).join(home_fallback))
        })
}

impl Config {
    /// Loads the config from `explicit` (`--config`) or the XDG default.
    pub fn load(explicit: Option<&Path>, env: &impl Env) -> Result<Config> {
        match explicit {
            Some(path) => Self::from_file(path, env),
            None => {
                let path = default_config_path(env)?;
                if path.exists() {
                    Self::from_file(&path, env)
                } else {
                    Self::resolve(RawConfig::default(), None, env)
                }
            }
        }
    }

    /// Reads and parses one config file.
    pub fn from_file(path: &Path, env: &impl Env) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text, path, env)
    }

    /// Parses config text; relative paths resolve against `path`'s directory.
    pub fn parse(text: &str, path: &Path, env: &impl Env) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| Error::ConfigParse {
            path: path.to_owned(),
            source,
        })?;
        // Bypass has one switch, `allow_bypass`, which `hello` reports:
        // agent flags that enable it behind that switch are refused (D38).
        if let Some(arg) = crate::agent::claude_code::bypass_arg(&raw.agent.args) {
            return Err(Error::BypassInAgentArgs {
                path: path.to_owned(),
                arg,
            });
        }
        // A staged file is evicted to fit the staging cap, so an upload cap
        // above it could never be kept whole (D55).
        if let Some(max) = raw.files.upload_max.filter(|&max| max > STAGING_MAX_BYTES) {
            return Err(Error::UploadMaxTooLarge {
                path: path.to_owned(),
                max,
            });
        }
        Self::resolve(raw, path.parent(), env)
    }

    fn resolve(raw: RawConfig, base: Option<&Path>, env: &impl Env) -> Result<Config> {
        let abs = |p: PathBuf| match base {
            Some(base) if p.is_relative() => base.join(p),
            _ => p,
        };
        // A bare name stays bare so it is looked up on PATH.
        let program = |c: PathBuf| {
            if c.components().count() > 1 {
                abs(c)
            } else {
                c
            }
        };
        let data_dir = match raw.data_dir {
            Some(dir) => abs(dir),
            None => default_data_dir(env)?,
        };
        Ok(Config {
            listen: raw
                .listen
                .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("valid default address")),
            data_dir,
            roots: raw.roots.into_iter().map(abs).collect(),
            agent: AgentConfig {
                command: raw
                    .agent
                    .command
                    .map(program)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_AGENT_COMMAND)),
                args: raw.agent.args,
                allow_bypass: raw.agent.allow_bypass,
            },
            log_file: raw.log_file.map(abs),
            server_names: raw.server_names,
            shell: ShellConfig {
                enabled: raw.shell.enabled,
                program: raw
                    .shell
                    .program
                    .map(program)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_PROGRAM)),
                rc: raw.shell.rc.map(abs),
                output_max: raw.shell.output_max.unwrap_or(DEFAULT_OUTPUT_MAX),
                ..ShellConfig::default()
            },
            screen: ScreenConfig {
                enabled: raw.screen.enabled,
                adb: raw
                    .screen
                    .adb
                    .map(program)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_ADB)),
            },
            disk: DiskConfig {
                min_free: raw.disk.min_free.unwrap_or(DEFAULT_MIN_FREE),
            },
            files: FilesConfig {
                download_max: raw.files.download_max.unwrap_or(DEFAULT_DOWNLOAD_MAX),
                upload_max: raw.files.upload_max.unwrap_or(DEFAULT_UPLOAD_MAX),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct FakeEnv(HashMap<String, String>);

    impl Env for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn env(vars: &[(&str, &str)]) -> FakeEnv {
        FakeEnv(
            vars.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn default_paths_follow_xdg() {
        let e = env(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/data"),
        ]);
        assert_eq!(
            default_config_path(&e).unwrap(),
            Path::new("/cfg/skywave/skywaved.toml")
        );
        assert_eq!(default_data_dir(&e).unwrap(), Path::new("/data/skywave"));
    }

    #[test]
    fn default_paths_fall_back_to_home() {
        let e = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "relative")]);
        assert_eq!(
            default_config_path(&e).unwrap(),
            Path::new("/home/u/.config/skywave/skywaved.toml")
        );
        assert_eq!(
            default_data_dir(&e).unwrap(),
            Path::new("/home/u/.local/share/skywave")
        );
        assert!(matches!(
            default_config_path(&env(&[])),
            Err(Error::NoConfigDir)
        ));
    }

    #[test]
    fn parses_full_config_and_resolves_relative_paths() {
        let text = r#"
            listen = "0.0.0.0:9000"
            data_dir = "state"
            roots = ["/srv/code", "projects"]
            log_file = "/var/log/skywaved.log"
            server_names = ["skywave.example", "10.0.0.7"]

            [agent]
            command = "bin/claude"
            args = ["--verbose"]
            allow_bypass = true

            [shell]
            enabled = true
            program = "bin/bash"
            rc = "shellrc"
            output_max = "8 KiB"

            [disk]
            min_free = "2GiB"
        "#;
        let c = Config::parse(text, Path::new("/etc/skywave/skywaved.toml"), &env(&[])).unwrap();
        assert_eq!(c.listen, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(c.data_dir, Path::new("/etc/skywave/state"));
        assert_eq!(
            c.roots,
            [Path::new("/srv/code"), Path::new("/etc/skywave/projects")]
        );
        assert_eq!(c.agent.command, Path::new("/etc/skywave/bin/claude"));
        assert_eq!(c.agent.args, ["--verbose"]);
        assert!(c.agent.allow_bypass);
        assert_eq!(
            c.log_file.as_deref(),
            Some(Path::new("/var/log/skywaved.log"))
        );
        assert_eq!(c.server_names, ["skywave.example", "10.0.0.7"]);
        assert!(c.shell.enabled);
        assert_eq!(c.shell.program, Path::new("/etc/skywave/bin/bash"));
        assert_eq!(
            c.shell.rc.as_deref(),
            Some(Path::new("/etc/skywave/shellrc"))
        );
        assert_eq!(c.shell.output_max, 8 * 1024);
        assert_eq!(c.disk.min_free, 2 << 30);
    }

    #[test]
    fn the_disk_floor_takes_a_size_and_zero_turns_it_off() {
        let parse = |text: &str| Config::parse(text, Path::new("/c.toml"), &env(&[("HOME", "/h")]));
        assert_eq!(parse("[disk]\nmin_free = 0").unwrap().disk.min_free, 0);
        assert_eq!(
            parse("[disk]\nmin_free = \"512 MiB\"")
                .unwrap()
                .disk
                .min_free,
            512 << 20
        );
        for bad in ["[disk]\nmin_free = \"1GB\"", "[disk]\nfloor = 1"] {
            assert!(
                matches!(parse(bad), Err(Error::ConfigParse { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_download_cap_takes_a_size() {
        let parse = |text: &str| Config::parse(text, Path::new("/c.toml"), &env(&[("HOME", "/h")]));
        assert_eq!(
            parse("[files]\ndownload_max = \"256 MiB\"")
                .unwrap()
                .files
                .download_max,
            256 << 20
        );
        assert_eq!(
            parse("[files]\ndownload_max = 1000")
                .unwrap()
                .files
                .download_max,
            1000
        );
        for bad in ["[files]\ndownload_max = \"1GB\"", "[files]\nupload = 1"] {
            assert!(
                matches!(parse(bad), Err(Error::ConfigParse { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_upload_cap_takes_a_size_up_to_the_staging_cap() {
        let parse = |text: &str| Config::parse(text, Path::new("/c.toml"), &env(&[("HOME", "/h")]));
        assert_eq!(
            parse("[files]\nupload_max = \"1 MiB\"")
                .unwrap()
                .files
                .upload_max,
            1 << 20
        );
        assert_eq!(
            parse("[files]\nupload_max = \"256 MiB\"")
                .unwrap()
                .files
                .upload_max,
            STAGING_MAX_BYTES,
            "the staging cap itself is allowed"
        );
        assert!(
            matches!(
                parse("[files]\nupload_max = \"512 MiB\""),
                Err(Error::UploadMaxTooLarge { max, .. }) if max == 512 << 20
            ),
            "over the staging cap is refused at load"
        );
    }

    #[test]
    fn shell_takes_a_bare_program_and_a_byte_count() {
        let text = "[shell]\nprogram = \"bash\"\noutput_max = 4096";
        let e = env(&[("HOME", "/home/u")]);
        let c = Config::parse(text, Path::new("/etc/skywaved.toml"), &e).unwrap();
        assert!(!c.shell.enabled);
        assert_eq!(c.shell.program, Path::new("bash"), "looked up on PATH");
        assert_eq!(c.shell.output_max, 4096);
        for bad in [
            "[shell]\noutput_max = \"64MB\"",
            "[shell]\noutput_max = \"lots\"",
            "[shell]\nshell = true",
        ] {
            let err = Config::parse(bad, Path::new("/c.toml"), &env(&[])).unwrap_err();
            assert!(matches!(err, Error::ConfigParse { .. }), "{bad}");
        }
    }

    #[test]
    fn screen_is_off_by_default_and_takes_an_adb_path() {
        let e = env(&[("HOME", "/home/u")]);
        let c = Config::parse("", Path::new("/etc/skywaved.toml"), &e).unwrap();
        assert!(!c.screen.enabled, "screen is opt-in");
        let text = "[screen]\nenabled = true\nadb = \"/opt/android/adb\"";
        let c = Config::parse(text, Path::new("/etc/skywaved.toml"), &e).unwrap();
        assert!(c.screen.enabled);
        assert_eq!(c.screen.adb, Path::new("/opt/android/adb"));
    }

    #[test]
    fn bypass_is_allowed_only_by_a_true_flag() {
        let parse = |text: &str| Config::parse(text, Path::new("/c.toml"), &env(&[("HOME", "/h")]));
        assert!(
            parse("[agent]\nallow_bypass = true")
                .unwrap()
                .agent
                .allow_bypass
        );
        assert!(
            !parse("[agent]\nallow_bypass = false")
                .unwrap()
                .agent
                .allow_bypass
        );
        assert!(!parse("[agent]\nargs = []").unwrap().agent.allow_bypass);
        for bad in [
            "[agent]\nallow_bypass = \"yes\"",
            "[agent]\nallow_bypass = 1",
            "allow_bypass = true",
        ] {
            assert!(
                matches!(parse(bad), Err(Error::ConfigParse { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn agent_args_that_allow_bypass_are_refused() {
        let parse = |args: &str| {
            let text = format!("[agent]\nallow_bypass = true\nargs = [{args}]");
            Config::parse(&text, Path::new("/c.toml"), &env(&[("HOME", "/h")]))
        };
        for (args, named) in [
            (
                r#""--dangerously-skip-permissions""#,
                "--dangerously-skip-permissions",
            ),
            (
                r#""--dangerously-skip-permissions=true""#,
                "--dangerously-skip-permissions=true",
            ),
            (
                r#""--allow-dangerously-skip-permissions""#,
                "--allow-dangerously-skip-permissions",
            ),
            (
                r#""--verbose", "--permission-mode", "bypassPermissions""#,
                "--permission-mode bypassPermissions",
            ),
            (
                r#""--permission-mode=bypassPermissions""#,
                "--permission-mode=bypassPermissions",
            ),
        ] {
            let err = parse(args).unwrap_err();
            let Error::BypassInAgentArgs { arg, .. } = &err else {
                panic!("{args}: {err:?}");
            };
            assert_eq!(arg, named);
            let text = err.to_string();
            assert!(text.contains("[agent] allow_bypass"), "{text}");
        }
        // Positive controls: other modes, and look-alikes, are fine.
        for args in [
            r#""--permission-mode", "acceptEdits""#,
            r#""--permission-mode=plan""#,
            r#""bypassPermissions""#,
            r#""--dangerously-skip-permissionsX""#,
            r#""--model", "haiku""#,
        ] {
            assert!(parse(args).is_ok(), "{args}");
        }
    }

    #[test]
    fn sizes_take_binary_units() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("17"), Ok(17));
        assert_eq!(parse_size("17B"), Ok(17));
        assert_eq!(parse_size("2KiB"), Ok(2048));
        assert_eq!(parse_size(" 64MiB "), Ok(64 << 20));
        assert_eq!(parse_size("64 MiB"), Ok(64 << 20));
        assert_eq!(parse_size("3GiB"), Ok(3 << 30));
        for bad in [
            "", "MiB", "1.5MiB", "-1", "64MB", "64mib", "1KB", "64 MiB x",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
        assert!(parse_size("18446744073709551615GiB").is_err(), "overflow");
    }

    #[test]
    fn empty_config_uses_defaults() {
        let e = env(&[("HOME", "/home/u")]);
        let c = Config::parse("", Path::new("/etc/skywaved.toml"), &e).unwrap();
        assert_eq!(c.listen, DEFAULT_LISTEN.parse().unwrap());
        assert_eq!(c.data_dir, Path::new("/home/u/.local/share/skywave"));
        assert!(c.roots.is_empty());
        assert_eq!(c.agent.command, Path::new(DEFAULT_AGENT_COMMAND));
        assert!(c.agent.args.is_empty());
        assert!(!c.agent.allow_bypass, "bypass is opt-in");
        assert_eq!(c.log_file, None);
        assert!(c.server_names.is_empty());
        assert_eq!(c.shell, ShellConfig::default());
        assert!(!c.shell.enabled);
        assert_eq!(c.shell.program, Path::new("/bin/sh"));
        assert_eq!(c.shell.rc, None);
        assert_eq!(c.shell.output_max, 64 << 20);
        assert_eq!(c.disk.min_free, 1 << 30);
        assert_eq!(c.files.download_max, 64 << 20);
        assert_eq!(c.files.upload_max, 64 << 20);
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Config::parse("listne = \"x\"", Path::new("/c.toml"), &env(&[])).unwrap_err();
        assert!(matches!(err, Error::ConfigParse { .. }));
    }

    #[test]
    fn load_missing_default_file_uses_defaults_but_explicit_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_home = tmp.path().to_str().unwrap();
        let e = env(&[("XDG_CONFIG_HOME", cfg_home), ("XDG_DATA_HOME", "/data")]);
        let c = Config::load(None, &e).unwrap();
        assert_eq!(c.data_dir, Path::new("/data/skywave"));

        let missing = tmp.path().join("nope.toml");
        assert!(matches!(
            Config::load(Some(&missing), &e),
            Err(Error::Io { .. })
        ));

        let file = tmp.path().join("skywave/skywaved.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "data_dir = \"d\"").unwrap();
        let c = Config::load(None, &e).unwrap();
        assert_eq!(c.data_dir, tmp.path().join("skywave/d"));
    }
}
