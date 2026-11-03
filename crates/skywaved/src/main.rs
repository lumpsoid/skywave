use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::claude_code::transcripts::{self, ClaudeTranscripts};
use skywaved::agent::{NoOutsideConversations, OutsideConversations};
use skywaved::config::{Config, ProcessEnv};
use skywaved::devices::{self, Revoke};
use skywaved::identity::{ServerIdentity, local_hostname, server_names};
use skywaved::pairing;
use skywaved::server::{Server, ServerOptions};
use skywaved::shell::ShellRunner;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig};

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
    // A shell run's host (see `skywaved::shell`): before clap, so no help
    // lists it and nothing of the daemon starts.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|arg| arg == skywaved::shell::RUN_HOST)
        {
            return skywaved::shell::host::run(args);
        }
    }
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
    let mut storage = Storage::open(&config.data_dir)?;
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
            runtime.block_on(serve(&config, &mut storage, identity))?;
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
    storage: &mut Storage,
    identity: ServerIdentity,
) -> skywaved::Result<()> {
    // The one event log of the process: every writer shares it so appends
    // wake the API's long-polls. The shell and bypass flags are set before
    // any clone: each clone keeps its own.
    let log = EventLog::open(&storage.db, config.roots.clone())?
        .with_shell(config.shell.enabled)
        .with_bypass(config.agent.allow_bypass);
    // The agent's own conversations, read from the environment the CLI the
    // adapter spawns inherits.
    let outside: Arc<dyn OutsideConversations> = match transcripts::projects_dir() {
        Some(projects) => Arc::new(ClaudeTranscripts::new(projects)),
        None => {
            tracing::warn!("no CLAUDE_CONFIG_DIR or HOME: outside conversations cannot be read");
            Arc::new(NoOutsideConversations)
        }
    };
    let options = ServerOptions {
        outside,
        ..ServerOptions::default()
    };
    #[cfg(unix)]
    skywaved::server::raise_open_files_limit();
    // Bound first, so a second `serve` on the same address fails here,
    // before its startup blob pass could delete what a running daemon is
    // writing. Binding accepts nothing until `serve`, and nothing below
    // writes blobs before the pass. This narrows the risk but is no data
    // dir lock: a second daemon listening elsewhere still runs the pass.
    let server = Server::bind_with(
        config.listen,
        &storage.dir,
        Arc::new(identity),
        log.clone(),
        options,
    )
    .await?;
    // Before the supervisor, shell runner and API can write blobs.
    storage.collect_blobs()?;
    let adapter = ClaudeCodeAdapter::new(config.agent.clone());
    let mut supervisor = Supervisor::start(
        adapter,
        config.data_dir.clone(),
        log.clone(),
        SupervisorConfig::default(),
    )
    .await?;
    // Always started: disabled, it still settles runs an enabled daemon
    // left, then ends.
    let mut shell = ShellRunner::start(config.data_dir.clone(), log, config.shell.clone()).await?;
    tracing::info!(
        addr = %server.local_addr(),
        protocol_version = skywave_proto::PROTOCOL_VERSION,
        "listening"
    );
    // A failed shell runner or agent supervisor stops the daemon: running on
    // without one would accept runs nothing starts, or messages no agent
    // hears. Its error is what `shutdown` returns, so the daemon exits
    // non-zero; the next start settles what it left.
    let stop = async {
        tokio::select! {
            () = shutdown_signal() => {}
            () = shell.failed() => tracing::error!("shell runner failed; stopping the daemon"),
            () = supervisor.failed() => {
                tracing::error!("agent supervisor failed; stopping the daemon");
            }
        }
    };
    let served = server.serve(stop).await;
    let shell_stopped = shell.shutdown().await;
    supervisor.shutdown().await?;
    shell_stopped?;
    served?;
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
