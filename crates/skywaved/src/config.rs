//! `skywaved.toml`: where the daemon listens, keeps its data and runs agents.
//!
//! The file is found at `--config <path>`, or else at
//! `$XDG_CONFIG_HOME/skywave/skywaved.toml` (`~/.config/...` when unset).
//! A missing default file means "all defaults"; a missing `--config` file is
//! an error. Relative paths in the file resolve against the file's directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, IoContext, Result};

/// Default `listen` address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7443";
/// Default `agent.command`, looked up on `PATH`.
pub const DEFAULT_AGENT_COMMAND: &str = "claude";

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
}

/// How to start the agent process.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    /// Executable path, or a bare name looked up on `PATH`.
    pub command: PathBuf,
    pub args: Vec<String>,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    command: Option<PathBuf>,
    #[serde(default)]
    args: Vec<String>,
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
        Self::resolve(raw, path.parent(), env)
    }

    fn resolve(raw: RawConfig, base: Option<&Path>, env: &impl Env) -> Result<Config> {
        let abs = |p: PathBuf| match base {
            Some(base) if p.is_relative() => base.join(p),
            _ => p,
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
                // A bare name stays bare so it is looked up on PATH.
                command: raw
                    .agent
                    .command
                    .map(|c| {
                        if c.components().count() > 1 {
                            abs(c)
                        } else {
                            c
                        }
                    })
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_AGENT_COMMAND)),
                args: raw.agent.args,
            },
            log_file: raw.log_file.map(abs),
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

            [agent]
            command = "bin/claude"
            args = ["--verbose"]
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
        assert_eq!(
            c.log_file.as_deref(),
            Some(Path::new("/var/log/skywaved.log"))
        );
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
        assert_eq!(c.log_file, None);
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
