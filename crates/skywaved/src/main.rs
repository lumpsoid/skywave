use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use skywaved::Storage;
use skywaved::config::{Config, ProcessEnv};
use skywaved::devices::{self, Revoke};
use skywaved::identity::{ServerIdentity, local_hostname, server_names};
use skywaved::pairing;

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
    /// Print a one-time pairing code, the server address and the CA
    /// fingerprint.
    Pair {
        /// How long the code stays valid (s, m or h; at most 24h).
        #[arg(long, default_value = "10m", value_parser = pairing::parse_ttl)]
        ttl: chrono::Duration,
    },
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

    let hostname = local_hostname();
    let bootstrap = || {
        let names = server_names(config.listen, hostname.as_deref(), &config.server_names);
        ServerIdentity::bootstrap(&storage.dir, &names)
    };

    match cli.command {
        Command::Serve => {
            let identity = bootstrap()?;
            tracing::info!(
                server_id = %identity.server_id().0,
                ca_fingerprint = %identity.ca_fingerprint(),
                names = ?identity.server_names(),
                "server identity ready"
            );
            not_implemented("serve");
        }
        Command::Pair { ttl } => {
            let identity = bootstrap()?;
            let issued = pairing::issue_code(storage.db.conn(), Utc::now(), ttl)?;
            let addr =
                pairing::pair_address(config.listen, hostname.as_deref(), &config.server_names);
            let fingerprint = identity.ca_fingerprint();
            let uri = pairing::pair_uri(&addr, &issued.code, fingerprint);
            println!("{}", pairing::render_qr(&uri)?);
            println!("{uri}\n");
            println!("Code:            {}", issued.code);
            println!("Server address:  {addr}");
            println!("CA fingerprint:  {fingerprint}");
            println!(
                "Expires:         {} (single use)",
                issued
                    .expires_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            );
        }
        Command::Devices {
            command: DevicesCommand::List,
        } => print!(
            "{}",
            devices::format_table(&devices::list(storage.db.conn())?)
        ),
        Command::Devices {
            command: DevicesCommand::Revoke { id },
        } => {
            let id = devices::parse_id(&id)?;
            match devices::revoke(storage.db.conn(), id, Utc::now())? {
                Revoke::Revoked(d) => println!("revoked device {} ({})", d.id.0, d.name),
                Revoke::AlreadyRevoked(d) => println!(
                    "device {} ({}) was already revoked at {}",
                    d.id.0,
                    d.name,
                    d.revoked_at.map(|t| t.to_rfc3339()).unwrap_or_default()
                ),
            }
        }
    }
    Ok(())
}

fn not_implemented(name: &str) {
    eprintln!(
        "skywaved {name}: not implemented (protocol v{})",
        skywave_proto::PROTOCOL_VERSION
    );
}
