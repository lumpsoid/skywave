use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use skywaved::config::{Config, ProcessEnv};
use skywaved::devices::{self, Revoke};
use skywaved::identity::{ServerIdentity, local_hostname, server_names};
use skywaved::pairing;
use skywaved::server::Server;
use skywaved::{EventLog, Storage};

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
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|source| skywaved::Error::Io {
                    context: "starting the async runtime".into(),
                    source,
                })?;
            runtime.block_on(serve(&config, &storage, identity))?;
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

/// Runs the daemon until Ctrl-C or SIGTERM.
async fn serve(
    config: &Config,
    storage: &Storage,
    identity: ServerIdentity,
) -> skywaved::Result<()> {
    // The one event log of the process: every writer shares it so appends
    // wake the API's long-polls.
    let log = EventLog::open(&storage.db, config.roots.clone())?;
    let server = Server::bind(config.listen, &storage.dir, Arc::new(identity), log).await?;
    tracing::info!(
        addr = %server.local_addr(),
        protocol_version = skywave_proto::PROTOCOL_VERSION,
        "listening"
    );
    server.serve(shutdown_signal()).await?;
    tracing::info!("stopped");
    Ok(())
}

/// Completes on Ctrl-C or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => tracing::info!("received Ctrl-C"),
        () = term => tracing::info!("received SIGTERM"),
    }
}
