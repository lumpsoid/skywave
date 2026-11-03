use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use skywave_core::{Core, CoreError};
use skywave_tui::Notify;
use skywave_tui::notify::VAR as NOTIFY_VAR;

/// Skywave terminal client.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Where the local replica and the device identity live. Defaults to
    /// `$XDG_DATA_HOME/skywave-tui`, else `~/.local/share/skywave-tui`.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// How to alert when an agent asks for an approval: `off`, or any of
    /// `bell`, `osc9`, `osc777` (desktop notifications) separated by
    /// commas. Defaults to `$SKYWAVE_NOTIFY`, else `bell`. Inside tmux, OSC
    /// 9/777 need `set -g allow-passthrough on`.
    #[arg(long, value_name = "HOW", value_parser = Notify::parse)]
    notify: Option<Notify>,
    /// Before starting, drop the local replica (sessions, events, queued
    /// commands, downloaded blobs) so it syncs again from the server. The
    /// pairing, settings, drafts and snippets stay. For local data this
    /// build cannot read, e.g. after an upgrade.
    #[arg(long)]
    reset_replica: bool,
}

fn default_data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("skywave-tui"))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let notify = match args.notify {
        Some(notify) => notify,
        None => match std::env::var(NOTIFY_VAR) {
            Ok(var) if !var.trim().is_empty() => match Notify::parse(&var) {
                Ok(notify) => notify,
                Err(err) => {
                    eprintln!("skywave-tui: {NOTIFY_VAR}: {err}");
                    return ExitCode::FAILURE;
                }
            },
            _ => Notify::default(),
        },
    };
    let Some(data_dir) = args.data_dir.or_else(default_data_dir) else {
        eprintln!("skywave-tui: no data directory; pass --data-dir");
        return ExitCode::FAILURE;
    };
    if args.reset_replica {
        if let Err(err) = Core::reset_replica(&data_dir) {
            eprintln!("skywave-tui: resetting {}: {err}", data_dir.display());
            return ExitCode::FAILURE;
        }
        eprintln!(
            "skywave-tui: reset the local replica in {}",
            data_dir.display()
        );
    }
    let core = match Core::open(&data_dir) {
        Ok(core) => core,
        Err(err @ CoreError::ReplicaUnreadable { .. }) => {
            eprintln!("skywave-tui: opening {}: {err}", data_dir.display());
            eprintln!(
                "skywave-tui: run again with --reset-replica to sync it again from the server \
                 (keeps the pairing, settings, drafts and snippets; unsent messages are lost)"
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("skywave-tui: opening {}: {err}", data_dir.display());
            return ExitCode::FAILURE;
        }
    };
    match skywave_tui::run(&core, notify) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("skywave-tui: terminal: {err}");
            ExitCode::FAILURE
        }
    }
}
