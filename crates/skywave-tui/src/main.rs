use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use skywave_core::Core;

/// Skywave terminal client.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Where the local replica and the device identity live. Defaults to
    /// `$XDG_DATA_HOME/skywave-tui`, else `~/.local/share/skywave-tui`.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
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
    let Some(data_dir) = args.data_dir.or_else(default_data_dir) else {
        eprintln!("skywave-tui: no data directory; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let core = match Core::open(&data_dir) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("skywave-tui: opening {}: {err}", data_dir.display());
            return ExitCode::FAILURE;
        }
    };
    match skywave_tui::run(&core) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("skywave-tui: terminal: {err}");
            ExitCode::FAILURE
        }
    }
}
