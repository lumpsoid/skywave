use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::claude_code::limits::ClaudeUsageLimits;
use skywaved::agent::claude_code::login::ClaudeLogin;
use skywaved::agent::claude_code::transcripts::{self, ClaudeTranscripts};
use skywaved::agent::{
    AgentLogin, LoginState, NoAgentLogin, NoOutsideConversations, OutsideConversations,
};
use skywaved::config::{Config, ProcessEnv};
#[cfg(unix)]
use skywaved::control::{Client, Confirm, Control, Reply, Request, When};
use skywaved::control::{Work, active_work};
use skywaved::data_dir::CONTROL_SOCKET_FILE;
use skywaved::db::Db;
use skywaved::devices::{self, Revoke};
use skywaved::disk::{Disk, FsFreeSpace};
use skywaved::identity::{ServerIdentity, local_hostname, server_names};
use skywaved::logging::LogFile;
use skywaved::pairing;
use skywaved::safety_feedback;
use skywaved::screen::ScreenRunner;
use skywaved::server::{Server, ServerOptions};
use skywaved::shell::ShellRunner;
use skywaved::workdir::Git;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig};

#[derive(Parser)]
#[command(name = "skywaved", version = skywave_version::VERSION, about = "Skywave server daemon")]
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
    /// Print all safety check feedback as JSON lines, oldest first.
    SafetyFeedback,
    /// Stop the running serve. Refused while agent turns or shell runs are
    /// running, unless you confirm losing them.
    Stop(StopArgs),
    /// Stop the running serve as `stop` does, then start it again with the
    /// same arguments (and the binary now at its path).
    Restart(StopArgs),
    /// Report whether serve runs and what it is doing: the local health
    /// check. Fails when no serve runs, or it is stopping.
    Status,
}

#[derive(clap::Args)]
struct StopArgs {
    /// Stop once the running agent turns and shell runs have ended; new
    /// messages and shell runs are refused meanwhile.
    #[arg(long, conflicts_with_all = ["force", "cancel"])]
    after_work: bool,
    /// Stop now without asking, losing whatever runs.
    #[arg(long, conflicts_with = "cancel")]
    force: bool,
    /// Cancel a stop waiting for the running work, and take work again.
    #[arg(long)]
    cancel: bool,
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
    // A stop or restart only talks to the running serve: opening the data
    // dir would migrate it under that serve, from a binary deployed since.
    let socket = config.data_dir.join(CONTROL_SOCKET_FILE);
    match &cli.command {
        Command::Stop(args) => return request_stop(&socket, false, args),
        Command::Restart(args) => return request_stop(&socket, true, args),
        Command::Status => return request_status(&socket),
        _ => {}
    }
    let log_file = skywaved::logging::init(config.log_file.as_deref())?;
    // Every other subcommand works on the data dir, so open (and migrate)
    // it first.
    let mut storage = Storage::open(&config.data_dir)?;
    tracing::debug!(data_dir = %storage.dir.root().display(), "opened data dir");

    let hostname = local_hostname();
    let bootstrap = || {
        let names = server_names(config.listen, hostname.as_deref(), &config.server_names);
        let identity = ServerIdentity::bootstrap(&storage.dir, &names)?;
        warn_operator(&identity, &storage)?;
        Ok::<_, skywaved::Error>(identity)
    };

    match cli.command {
        Command::Serve => {
            // Held until the process exits; first, so a second `serve` on
            // this dir fails before it touches the identity or the blobs.
            let _serve_lock = storage.dir.lock_for_serve()?;
            // Every page, once, before anything is served from the file.
            storage.db.quick_check(&storage.dir.db_path())?;
            let identity = bootstrap()?;
            // Before anything can replace the binary: a restart executes
            // the one at this path then.
            let exe = std::env::current_exe().map_err(|source| skywaved::Error::Io {
                context: "finding the skywaved binary".into(),
                source,
            })?;
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
            let exit = runtime.block_on(serve(&config, &mut storage, identity, log_file))?;
            if exit == Exit::Restart {
                // Everything `serve` held goes before the new one starts:
                // its connections and tasks, the database, the serve lock.
                drop(runtime);
                drop(storage);
                drop(_serve_lock);
                return Err(restart(&exe));
            }
        }
        Command::Stop(_) | Command::Restart(_) | Command::Status => {
            unreachable!("handled before the data dir")
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
        Command::SafetyFeedback => {
            for line in safety_feedback::lines(storage.db.conn())? {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Prints, on the command's own stderr as well as the log (which may be a
/// file), what a bootstrap did that locks devices out: a new CA while
/// devices are paired, or server certificate names it dropped.
fn warn_operator(identity: &ServerIdentity, storage: &Storage) -> skywaved::Result<()> {
    if identity.created_ca() {
        let paired = devices::list(storage.db.conn())?
            .iter()
            .filter(|d| !d.is_revoked())
            .count();
        if let Some(warning) = identity.new_ca_warning(paired) {
            tracing::warn!(paired, "{warning}");
            eprintln!("skywaved: WARNING: {warning}");
        }
    }
    // Bootstrap already logged these.
    if !identity.dropped_names().is_empty() {
        eprintln!(
            "skywaved: WARNING: the server certificate no longer covers {}: devices that \
             connect by one of these names cannot connect until it is back in `server_names`, \
             their address is changed to a listed name, or they re-pair",
            identity.dropped_names().join(", ")
        );
    }
    Ok(())
}

/// How `serve` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Stop,
    /// Stopped by `skywaved restart`: start again.
    Restart,
}

/// Runs the daemon until Ctrl-C, SIGTERM or a `skywaved stop` or
/// `restart`. SIGHUP reopens `log_file`, if logs go to one.
async fn serve(
    config: &Config,
    storage: &mut Storage,
    identity: ServerIdentity,
    log_file: Option<LogFile>,
) -> skywaved::Result<Exit> {
    // Before anything else: SIGHUP's default action ends the process.
    #[cfg(unix)]
    let _hangup = reopen_on_hangup(log_file)?;
    #[cfg(not(unix))]
    let _ = log_file;
    let disk = Disk::new(
        Arc::new(FsFreeSpace::new(&config.data_dir)),
        config.disk.min_free,
    );
    // Before the first write (the run's record below): on a full disk a
    // start would fail there, and a service manager restart it in a loop.
    // Then read against the floor, so a start below it comes up paused.
    tokio::select! {
        () = disk.wait_to_start() => {}
        () = shutdown_signal() => return Ok(Exit::Stop),
    }
    // The one event log of the process: every writer shares it so appends
    // wake the API's long-polls. The shell and bypass flags and the disk
    // are set before any clone: each clone keeps its own.
    let log = EventLog::open(&storage.db, config.roots.clone())?
        .with_shell(config.shell.enabled)
        .with_screen(config.screen.enabled)
        .with_bypass(config.agent.allow_bypass)
        .with_disk(disk);
    // The agent's own conversations, read from the environment the CLI the
    // adapter spawns inherits.
    let outside: Arc<dyn OutsideConversations> = match transcripts::projects_dir() {
        Some(projects) => Arc::new(ClaudeTranscripts::new(projects)),
        None => {
            tracing::warn!("no CLAUDE_CONFIG_DIR or HOME: outside conversations cannot be read");
            Arc::new(NoOutsideConversations)
        }
    };
    // The account's usage limits, read by a process of the agent's own,
    // run where sessions run (the data dir when no root is configured).
    let limits_workdir = config.roots.first().unwrap_or(&config.data_dir).clone();
    let usage_limits = Arc::new(ClaudeUsageLimits::new(
        ClaudeCodeAdapter::new(config.agent.clone()),
        limits_workdir.clone(),
    ));
    // The agent's login, read from the credentials the CLI the adapter
    // spawns uses, so devices warn before it expires; a device logs it in
    // with a process of its own, run where a usage read's runs.
    let login: Arc<dyn AgentLogin> =
        match ClaudeLogin::from_env(ClaudeCodeAdapter::new(config.agent.clone()), limits_workdir) {
            Some(login) => Arc::new(login),
            None => {
                tracing::warn!(
                    "no CLAUDE_CONFIG_DIR or HOME: the agent's login expiry cannot be read"
                );
                Arc::new(NoAgentLogin)
            }
        };
    // What the session processes say of the agent's login: the supervisor
    // learns it, `hello` reports it.
    let login_state = LoginState::new();
    let options = ServerOptions {
        outside,
        usage_limits,
        login,
        login_state: login_state.clone(),
        log_file: config.log_file.clone(),
        download_max: config.files.download_max,
        upload_max: config.files.upload_max,
        ..ServerOptions::default()
    };
    #[cfg(unix)]
    skywaved::server::raise_open_files_limit();
    // Binding accepts nothing until `serve`, and nothing below writes blobs
    // before the startup blob pass. The serve lock `run` holds keeps a
    // second daemon on this data dir from running the pass at all.
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
    // Before the API can create worktrees: undoes any a crash left.
    storage.sweep_worktrees(&Git::default())?;
    let adapter = ClaudeCodeAdapter::new(config.agent.clone());
    let mut supervisor = Supervisor::start(
        adapter,
        config.data_dir.clone(),
        log.clone(),
        SupervisorConfig {
            login: login_state,
            ..SupervisorConfig::default()
        },
    )
    .await?;
    #[cfg(unix)]
    let mut control = Control::bind(
        storage.dir.control_socket(),
        &storage.dir.db_path(),
        log.clone(),
        supervisor.activity(),
    )?;
    // Always started: disabled, it still settles runs an enabled daemon
    // left, then ends. The screen runner is the same.
    let mut shell =
        ShellRunner::start(config.data_dir.clone(), log.clone(), config.shell.clone()).await?;
    let mut screen =
        ScreenRunner::start(config.data_dir.clone(), log, config.screen.clone()).await?;
    // Made before "listening", so a signal sent once that line is logged is never lost.
    let term = sigterm_listener();
    let presses = ctrl_c_listener();
    tracing::info!(
        addr = %server.local_addr(),
        protocol_version = skywave_proto::PROTOCOL_VERSION,
        "listening"
    );
    // A failed shell runner or agent supervisor stops the daemon: running on
    // without one would accept runs nothing starts, or messages no agent
    // hears. Its error is what `shutdown` returns, so the daemon exits
    // non-zero; the next start settles what it left.
    let exit = std::cell::Cell::new(Exit::Stop);
    #[cfg(unix)]
    let requested = async {
        let stop = control.stopped().await;
        tracing::info!(
            restart = stop.restart,
            "stop requested on the control socket"
        );
        if stop.restart {
            exit.set(Exit::Restart);
        }
    };
    #[cfg(not(unix))]
    let requested = std::future::pending::<()>();
    let db_path = storage.dir.db_path();
    let stop = async {
        tokio::select! {
            () = stop_signal(&db_path, term, presses) => {}
            () = requested => {}
            () = shell.failed() => tracing::error!("shell runner failed; stopping the daemon"),
            () = screen.failed() => tracing::error!("screen runner failed; stopping the daemon"),
            () = supervisor.failed() => {
                tracing::error!("agent supervisor failed; stopping the daemon");
            }
        }
    };
    let served = server.serve(stop).await;
    let shell_stopped = shell.shutdown().await;
    let screen_stopped = screen.shutdown().await;
    supervisor.shutdown().await?;
    shell_stopped?;
    screen_stopped?;
    served?;
    tracing::info!("stopped");
    Ok(exit.get())
}

/// Executes the binary at `exe` with this process's arguments, in its
/// place; returns only if that fails.
#[cfg(unix)]
fn restart(exe: &std::path::Path) -> skywaved::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let mut command = std::process::Command::new(exe);
    if let Some(arg0) = args.next() {
        command.arg0(arg0);
    }
    tracing::info!(exe = %exe.display(), "restarting");
    let source = command.args(args).exec();
    skywaved::Error::Io {
        context: format!("restarting {}", exe.display()),
        source,
    }
}

#[cfg(not(unix))]
fn restart(_exe: &std::path::Path) -> skywaved::Error {
    skywaved::Error::Control("restart needs Unix".into())
}

/// `skywaved stop` / `restart`: asks the running `serve` over its control
/// socket, and waits until it has stopped (or the wait is cancelled).
#[cfg(unix)]
fn request_stop(socket: &std::path::Path, restart: bool, args: &StopArgs) -> skywaved::Result<()> {
    use std::io::IsTerminal;
    let mut client = Client::connect(socket)?;
    let mut request = if args.cancel {
        Request::Cancel
    } else {
        Request::Stop {
            restart,
            when: if args.after_work {
                When::AfterWork
            } else {
                When::Now
            },
            confirm: if args.force {
                Confirm::Force
            } else {
                Confirm::Nothing
            },
        }
    };
    let gone = || skywaved::Error::Control("serve closed the connection without an answer".into());
    loop {
        client.send(&request)?;
        let reply = client.reply()?.ok_or_else(gone)?;
        match reply {
            Reply::Busy { work } => {
                eprintln!("skywaved: running now, and lost if it stops:");
                for item in &work {
                    eprintln!("  - {item}");
                }
                if !std::io::stdin().is_terminal() {
                    return Err(skywaved::Error::Control(
                        "not stopped: pass --force to stop anyway, or --after-work to stop \
                         once it has ended"
                            .into(),
                    ));
                }
                eprint!("Stop anyway? [y/N] ");
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|source| skywaved::Error::Io {
                        context: "reading the answer".into(),
                        source,
                    })?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    eprintln!("skywaved: not stopped");
                    return Ok(());
                }
                if let Request::Stop { confirm, .. } = &mut request {
                    *confirm = Confirm::Seen(work);
                }
            }
            reply => return follow_stop(&mut client, reply),
        }
    }
}

/// Prints how a stop goes from `reply` on, until `serve` has stopped.
#[cfg(unix)]
fn follow_stop(client: &mut Client, mut reply: Reply) -> skywaved::Result<()> {
    let mut hinted = false;
    loop {
        match reply {
            Reply::Waiting { work } => {
                if work.is_empty() {
                    eprintln!(
                        "skywaved: refusing new work; waiting for the agents to take up what \
                         was sent"
                    );
                } else {
                    eprintln!("skywaved: refusing new work; stopping once this has ended:");
                    for item in &work {
                        eprintln!("  - {item}");
                    }
                }
                if !hinted {
                    eprintln!(
                        "  (Ctrl-C here leaves serve waiting; `skywaved stop --cancel` takes \
                         work again)"
                    );
                    hinted = true;
                }
            }
            Reply::Stopping { restart } => {
                eprintln!("skywaved: stopping");
                // The connection closes once serve has stopped.
                while client.reply()?.is_some() {}
                eprintln!(
                    "skywaved: {}",
                    if restart {
                        "stopped; starting again"
                    } else {
                        "stopped"
                    }
                );
                return Ok(());
            }
            Reply::Cancelled => {
                eprintln!("skywaved: stop cancelled; taking work again");
                return Ok(());
            }
            Reply::NothingToCancel => {
                eprintln!("skywaved: no stop was waiting");
                return Ok(());
            }
            Reply::Failed { message } => return Err(skywaved::Error::Control(message)),
            Reply::Busy { .. } | Reply::Status { .. } => {
                return Err(skywaved::Error::Control("unexpected answer".into()));
            }
        }
        reply = match client.reply()? {
            Some(reply) => reply,
            None => {
                return Err(skywaved::Error::Control(
                    "serve closed the connection".into(),
                ));
            }
        };
    }
}

#[cfg(not(unix))]
fn request_stop(
    _socket: &std::path::Path,
    _restart: bool,
    _args: &StopArgs,
) -> skywaved::Result<()> {
    Err(skywaved::Error::Control(
        "stop and restart need Unix".into(),
    ))
}

/// Reopens `log_file` on every SIGHUP (so logrotate can move it) until the
/// returned task is dropped; with no log file, SIGHUP is ignored.
#[cfg(unix)]
fn reopen_on_hangup(log_file: Option<LogFile>) -> skywaved::Result<AbortOnDrop> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hangup = signal(SignalKind::hangup()).map_err(|source| skywaved::Error::Io {
        context: "listening for SIGHUP".into(),
        source,
    })?;
    Ok(AbortOnDrop(tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            let Some(log_file) = &log_file else { continue };
            match log_file.reopen() {
                Ok(()) => tracing::info!("received SIGHUP; reopened the log file"),
                Err(e) => {
                    tracing::error!(error = %e, "received SIGHUP; cannot reopen the log file")
                }
            }
        }
    })))
}

/// Aborts its task when dropped.
#[cfg(unix)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

#[cfg(unix)]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `skywaved status`: asks the running `serve` how it is and prints it.
#[cfg(unix)]
fn request_status(socket: &std::path::Path) -> skywaved::Result<()> {
    let mut client = Client::connect(socket)?;
    client.send(&Request::Status)?;
    let reply = client.reply()?.ok_or_else(|| {
        skywaved::Error::Control("serve closed the connection without an answer".into())
    })?;
    match reply {
        Reply::Status {
            protocol_version,
            stopping_after_work,
            disk_low,
            work,
        } => {
            println!("serving (protocol {protocol_version})");
            if stopping_after_work {
                println!("stopping once the running work has ended; refusing new work");
            }
            if disk_low {
                println!("disk low: agents paused, new work refused until space is freed");
            }
            if work.is_empty() {
                println!("nothing running");
            } else {
                println!("running:");
                for item in &work {
                    println!("  - {item}");
                }
            }
            Ok(())
        }
        Reply::Stopping { .. } => Err(skywaved::Error::Control("serve is stopping".into())),
        Reply::Failed { message } => Err(skywaved::Error::Control(message)),
        _ => Err(skywaved::Error::Control("unexpected answer".into())),
    }
}

#[cfg(not(unix))]
fn request_status(_socket: &std::path::Path) -> skywaved::Result<()> {
    Err(skywaved::Error::Control("status needs Unix".into()))
}

/// Completes on Ctrl-C or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = ctrl_c => tracing::info!("received Ctrl-C"),
        () = sigterm_received(sigterm_listener()) => tracing::info!("received SIGTERM"),
    }
}

/// Completes when `serve` should stop: at once on SIGTERM, and on Ctrl-C at
/// once when no work runs. Otherwise see `ctrl_c_stops`.
async fn stop_signal(
    db_path: &std::path::Path,
    term: std::io::Result<SigtermListener>,
    presses: std::io::Result<CtrlCListener>,
) {
    tokio::select! {
        () = sigterm_received(term) => tracing::info!("received SIGTERM"),
        () = ctrl_c_stops(db_path, presses) => {}
    }
}

/// Completes on the Ctrl-C that stops `serve`. With work running it warns
/// what would be lost, and completes on the next Ctrl-C instead.
async fn ctrl_c_stops(db_path: &std::path::Path, presses: std::io::Result<CtrlCListener>) {
    let mut presses = match presses {
        Ok(presses) => presses,
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            return std::future::pending().await;
        }
    };
    presses.recv().await;
    tracing::info!("received Ctrl-C");
    match read_running_work(db_path).await {
        Ok(work) if work.is_empty() => return,
        Ok(work) => {
            tracing::warn!(
                work = work.len(),
                "Ctrl-C with work running; a second one stops"
            );
            eprintln!("skywaved: Ctrl-C stops serve and loses what runs now:");
            for item in &work {
                eprintln!("  - {item}");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot read the running work; a second Ctrl-C stops");
            eprintln!("skywaved: Ctrl-C stops serve, and what runs now could not be read: {e}");
        }
    }
    eprintln!(
        "skywaved: press Ctrl-C again to stop anyway, or run `skywaved stop --after-work` \
         to stop once it has ended"
    );
    // The same listener: a press that lands while the warning prints is kept.
    presses.recv().await;
    tracing::info!("received a second Ctrl-C");
}

/// The work a stop now would lose, read over a connection of its own on the
/// blocking pool, so the signal wait does not hold the async thread.
async fn read_running_work(db_path: &std::path::Path) -> skywaved::Result<Vec<Work>> {
    let db_path = db_path.to_owned();
    tokio::task::spawn_blocking(move || {
        let db = Db::open(&db_path)?;
        active_work(db.conn())
    })
    .await
    .map_err(|e| skywaved::Error::Control(format!("database task failed: {e}")))?
}

/// The Ctrl-C listener: a signal stream on Unix.
#[cfg(unix)]
type CtrlCListener = tokio::signal::unix::Signal;
/// The Ctrl-C listener: a console handler on Windows.
#[cfg(not(unix))]
type CtrlCListener = tokio::signal::windows::CtrlC;

/// Makes the Ctrl-C listener. It subscribes when made, and a press it has
/// not yet been asked for is kept. `tokio::signal::ctrl_c()` subscribes only
/// on its first poll, so a press between two of its calls would be lost.
#[cfg(unix)]
fn ctrl_c_listener() -> std::io::Result<CtrlCListener> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}

/// Makes the Ctrl-C listener; see the Unix version.
#[cfg(not(unix))]
fn ctrl_c_listener() -> std::io::Result<CtrlCListener> {
    tokio::signal::windows::ctrl_c()
}

/// The SIGTERM listener: a signal stream on Unix. Elsewhere SIGTERM never
/// comes, so there is nothing to hold.
#[cfg(unix)]
type SigtermListener = tokio::signal::unix::Signal;
/// The SIGTERM listener: nothing, as SIGTERM never comes elsewhere.
#[cfg(not(unix))]
type SigtermListener = ();

/// Makes the SIGTERM listener, as `ctrl_c_listener` makes the Ctrl-C one.
#[cfg(unix)]
fn sigterm_listener() -> std::io::Result<SigtermListener> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
}

/// Makes the SIGTERM listener; see the Unix version.
#[cfg(not(unix))]
fn sigterm_listener() -> std::io::Result<SigtermListener> {
    Ok(())
}

/// Completes on SIGTERM as `listener` hears it. A listener that could not be
/// made is logged, and then nothing completes.
#[cfg(unix)]
async fn sigterm_received(listener: std::io::Result<SigtermListener>) {
    match listener {
        Ok(mut term) => {
            term.recv().await;
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for SIGTERM");
            std::future::pending::<()>().await;
        }
    }
}

/// Completes on SIGTERM as `listener` hears it; never on other platforms.
#[cfg(not(unix))]
async fn sigterm_received(_listener: std::io::Result<SigtermListener>) {
    std::future::pending::<()>().await;
}
