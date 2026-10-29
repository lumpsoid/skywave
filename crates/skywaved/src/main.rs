use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use skywaved::Storage;
use skywaved::config::{Config, ProcessEnv};

#[derive(Parser)]
#[command(name = "skywaved", version, about = "Skywave server daemon")]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/skywave/skywaved.toml].
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Serve,
    /// Print a one-time pairing code and the server certificate fingerprint.
    Pair,
    /// Manage paired devices.
    Devices {
        #[command(subcommand)]
        command: DevicesCommand,
    },
}

#[derive(Subcommand)]
enum DevicesCommand {
    /// List paired devices.
    List,
    /// Revoke a paired device.
    Revoke {
        /// Device id.
        id: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("skywaved: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> skywaved::Result<()> {
    let config = Config::load(cli.config.as_deref(), &ProcessEnv)?;
    skywaved::logging::init(config.log_file.as_deref())?;
    // Every subcommand works on the data dir, so open (and migrate) it first.
    let storage = Storage::open(&config.data_dir)?;
    tracing::debug!(data_dir = %storage.dir.root().display(), "opened data dir");

    let name = match cli.command {
        Command::Serve => "serve",
        Command::Pair => "pair",
        Command::Devices {
            command: DevicesCommand::List,
        } => "devices list",
        Command::Devices {
            command: DevicesCommand::Revoke { .. },
        } => "devices revoke",
    };
    eprintln!(
        "skywaved {name}: not implemented (protocol v{})",
        skywave_proto::PROTOCOL_VERSION
    );
    Ok(())
}
