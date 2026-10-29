use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "skywaved", version, about = "Skywave server daemon")]
struct Cli {
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

fn main() {
    let cli = Cli::parse();
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
}
