//! The HTTPS server (architecture.md "Protocol" and "Pairing"): TLS with
//! optional client certificates, HTTP/1.1, and the routes of [`crate::pairing`] and [`crate::api`].
//!
//! TLS accepts a connection with no client certificate, or with one that
//! chains to the server's CA (anything else fails the handshake). Only
//! `POST /v1/pair` answers without one; every other route checks the
//! certificate against the device registry on each request
//! ([`crate::api`]). The leaf certificate travels from the handshake to the
//! handlers as a request extension.
//!
//! Start one in-process with [`Server::bind`] (port 0 picks a free port) and
//! run it with [`Server::serve`]:
//!
//! ```no_run
//! # async fn demo(dir: skywaved::data_dir::DataDir,
//! #     identity: std::sync::Arc<skywaved::identity::ServerIdentity>,
//! #     log: skywaved::EventLog) -> skywaved::Result<()> {
//! use skywaved::server::Server;
//! let server = Server::bind("127.0.0.1:0".parse().unwrap(), &dir, identity, log).await?;
//! let addr = server.local_addr();
//! let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
//! let task = tokio::spawn(server.serve(async { let _ = stopped.await; }));
//! // ... talk to https://localhost:{addr.port()} ...
//! let _ = stop.send(());
//! task.await.unwrap()?;
//! # Ok(()) }
//! ```

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{StatusCode, header};
use hyper_util::rt::TokioIo;
use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower::Service;

use crate::EventLog;
use crate::agent::import::IMPORT_DEADLINE;
use crate::agent::{
    AgentLogin, LoginState, NoAgentLogin, NoOutsideConversations, NoUsageLimits,
    OutsideConversations, UsageLimitsSource,
};
use crate::api::{self, Api, CONVERSATIONS_TIMEOUT, LOGIN_FINISH_TIMEOUT, PeerCert};
use crate::crop::{CROP_DEADLINE, CropHook};
use crate::data_dir::DataDir;
use crate::db::Db;
use crate::error::{Error, IoContext, Result};
use crate::identity::ServerIdentity;
use crate::login::LOGIN_FLOW_MAX;
use crate::pairing::{self, PairService, RateLimit, SystemClock};
use crate::workdir::Git;

mod limits;

pub use limits::ConnectionLimits;
use limits::{Activity, Admission, Admissions, IdleIo, Responding, StalledBody, tune_socket};

/// A client gets this long to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// On shutdown, in-flight requests get this long before connections are
/// dropped. Long-polls end at once on shutdown, so this is plenty.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// What a server reads besides its data dir. The default runs git from
/// `PATH`, reads no outside conversations, no usage limits and no login
/// expiry, and imports within
/// [`IMPORT_DEADLINE`], crops within [`CROP_DEADLINE`], gives a
/// conversations listing [`CONVERSATIONS_TIMEOUT`], a login flow
/// [`LOGIN_FLOW_MAX`] and a login finish [`LOGIN_FINISH_TIMEOUT`].
pub struct ServerOptions {
    /// How workdir changes run git.
    pub git: Git,
    /// What `GET /v1/conversations` lists and where an adopting
    /// `CreateSession` reads the conversation's history.
    pub outside: Arc<dyn OutsideConversations>,
    /// What `GET /v1/usage-limits` reads.
    pub usage_limits: Arc<dyn UsageLimitsSource>,
    /// What `GET /v1/hello` reads `login_expires` and
    /// `agent_login_offered` from.
    pub login: Arc<dyn AgentLogin>,
    /// What `GET /v1/hello` reads `agent_logged_out` from: the cell the
    /// supervisor keeps (`SupervisorConfig::login`).
    pub login_state: LoginState,
    /// How long a device's open login flow waits for its code before the
    /// next look at it drops it; tests set it to zero.
    pub login_flow_max: Duration,
    /// How long a `POST /v1/agent-login/finish` waits for the agent before
    /// the reply is a 503 (the finish goes on); tests shorten it.
    pub login_finish_timeout: Duration,
    /// How long such an import may take; tests shorten it.
    pub import_deadline: Duration,
    /// How long a conversations listing may take before the reply is a
    /// 503; tests shorten it.
    pub conversations_timeout: Duration,
    /// How many connections the server holds and how long a stalled peer
    /// keeps one.
    pub limits: ConnectionLimits,
    /// Where the daemon logs, `None` for stderr: the storage report sizes it.
    pub log_file: Option<PathBuf>,
    /// How long a crop may take before the reply is a 503; tests shorten
    /// it.
    pub crop_deadline: Duration,
    /// Run in every crop before it writes and when its thread ends; tests
    /// hold, fail or follow crops with it. `None` in the daemon.
    pub crop_hook: Option<Arc<dyn CropHook>>,
    /// Run each time a request waits for another request's preparation of
    /// the same command (an adopting create or a crop), and each time a
    /// login start waits for another's; tests know a resend joined from
    /// it. `None` in the daemon.
    pub joined_hook: Option<JoinedHook>,
}

/// [`ServerOptions::joined_hook`].
pub type JoinedHook = Arc<dyn Fn() + Send + Sync>;

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            git: Git::default(),
            outside: Arc::new(NoOutsideConversations),
            usage_limits: Arc::new(NoUsageLimits),
            login: Arc::new(NoAgentLogin),
            login_state: LoginState::default(),
            login_flow_max: LOGIN_FLOW_MAX,
            login_finish_timeout: LOGIN_FINISH_TIMEOUT,
            import_deadline: IMPORT_DEADLINE,
            conversations_timeout: CONVERSATIONS_TIMEOUT,
            limits: ConnectionLimits::DEFAULT,
            log_file: None,
            crop_deadline: CROP_DEADLINE,
            crop_hook: None,
            joined_hook: None,
        }
    }
}

/// A bound, not yet serving, HTTPS server.
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: TlsAcceptor,
    app: Router,
    stop: watch::Sender<bool>,
    limits: ConnectionLimits,
}

impl Server {
    /// Builds the TLS config from `identity`, opens the API's, workdir
    /// reads', storage reports' and pairing's own connections to the database in `dir`, and
    /// binds `addr`.
    ///
    /// `log` is the process's one [`EventLog`]; other writers (the agent
    /// supervisor) share it so their appends wake long-polls.
    pub async fn bind(
        addr: SocketAddr,
        dir: &DataDir,
        identity: Arc<ServerIdentity>,
        log: EventLog,
    ) -> Result<Server> {
        Server::bind_with_git(addr, dir, identity, log, Git::default()).await
    }

    /// [`Server::bind`], with workdir changes running git as `git` says
    /// (tests point it at a fake).
    pub async fn bind_with_git(
        addr: SocketAddr,
        dir: &DataDir,
        identity: Arc<ServerIdentity>,
        log: EventLog,
        git: Git,
    ) -> Result<Server> {
        let options = ServerOptions {
            git,
            ..ServerOptions::default()
        };
        Server::bind_with(addr, dir, identity, log, options).await
    }

    /// [`Server::bind`], with what [`ServerOptions`] sets.
    pub async fn bind_with(
        addr: SocketAddr,
        dir: &DataDir,
        identity: Arc<ServerIdentity>,
        log: EventLog,
        options: ServerOptions,
    ) -> Result<Server> {
        let tls = TlsAcceptor::from(tls_config(&identity)?);
        let limits = options.limits;
        let (stop, stopped) = watch::channel(false);
        let api = Api::new(dir, log, identity.server_id(), stopped, options)?;
        let pairing = PairService::new(
            Db::open(&dir.db_path())?,
            identity,
            RateLimit::default(),
            Arc::new(SystemClock),
        );
        let app = pairing::router(Arc::new(pairing)).merge(api::router(Arc::new(api)));
        let listener = TcpListener::bind(addr)
            .await
            .context(|| format!("binding {addr}"))?;
        let local_addr = listener
            .local_addr()
            .context(|| format!("local address of {addr}"))?;
        Ok(Server {
            listener,
            local_addr,
            tls,
            app,
            stop,
            limits,
        })
    }

    /// The address actually bound (resolves port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until `shutdown` completes, then stops accepting, ends
    /// long-polls, lets in-flight requests finish (up to a short grace
    /// period) and returns.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let Server {
            listener,
            tls,
            app,
            stop,
            limits,
            ..
        } = self;
        let admissions = Admissions::new(&limits);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((tcp, peer)) => {
                        // Over a cap: dropping the socket closes it at once,
                        // before any TLS work.
                        let Some(admission) = admissions.admit(peer.ip()) else {
                            tracing::debug!(%peer, "connection over the cap; closed");
                            continue;
                        };
                        tune_socket(&tcp, peer, &limits);
                        connections.spawn(connection(
                            tcp,
                            peer,
                            admission,
                            tls.clone(),
                            app.clone(),
                            limits,
                            stop.subscribe(),
                        ));
                    }
                    Err(e) => {
                        // E.g. out of file descriptors: back off instead of spinning.
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        drop(listener);
        tracing::info!(open = connections.len(), "shutting down");
        stop.send_replace(true);
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                open = connections.len(),
                "dropping connections after grace period"
            );
            connections.shutdown().await;
        }
        Ok(())
    }
}

/// Raises this process's soft limit on open files to its hard limit, so the
/// connection cap, not the descriptor table, is what turns peers away.
/// Failing leaves the limit as it was, with a warning.
#[cfg(unix)]
pub fn raise_open_files_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid rlimit for the call to fill in or read.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == -1 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(%error, "cannot read the open files limit");
        return;
    }
    if limit.rlim_cur >= limit.rlim_max {
        return;
    }
    let wanted = libc::rlimit {
        rlim_cur: limit.rlim_max,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: as above.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &wanted) } == -1 {
        // E.g. macOS, whose hard limit may be unlimited but whose soft one
        // stops at OPEN_MAX.
        let error = std::io::Error::last_os_error();
        tracing::warn!(%error, soft = limit.rlim_cur, "cannot raise the open files limit");
    }
}

/// Serves one TCP connection: TLS handshake, then HTTP/1.1 with the peer's
/// certificate and address attached to every request. `admission` holds
/// the connection's place under the caps until it ends.
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    admission: Admission,
    tls: TlsAcceptor,
    app: Router,
    limits: ConnectionLimits,
    mut stop: watch::Receiver<bool>,
) {
    let _admission = admission;
    let activity = Arc::new(Activity::default());
    // The idle clock also runs during the handshake, which has its own
    // shorter timeout.
    let tcp = IdleIo::new(tcp, limits.idle_timeout, activity.clone());
    let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            tracing::debug!(%peer, error = %e, "TLS handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!(%peer, "TLS handshake timed out");
            return;
        }
    };
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|leaf| Arc::new(leaf.clone().into_owned()));
    let peer_cert = PeerCert(cert);
    let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
        req.extensions_mut().insert(peer_cert.clone());
        req.extensions_mut().insert(ConnectInfo(peer));
        let request = activity.start();
        let stalled = Arc::new(AtomicBool::new(false));
        let req =
            req.map(|body| Body::new(StalledBody::new(body, limits.body_stall, stalled.clone())));
        let mut app = app.clone();
        async move {
            let Ok(response) = app.call(req).await;
            // Whatever the handler made of the failed body (axum's
            // extractors answer 400, which devices take as final), a stall
            // is the peer's to retry.
            if stalled.load(Ordering::SeqCst) {
                return Ok(hyper::Response::builder()
                    .status(StatusCode::REQUEST_TIMEOUT)
                    .header(header::CONNECTION, "close")
                    .body(Body::new(Responding::new(Body::empty(), request)))
                    .expect("a valid response"));
            }
            Ok::<_, std::convert::Infallible>(
                response.map(|body| Body::new(Responding::new(body, request))),
            )
        }
    });
    // Hyper's header timeout stays off: `IdleIo` does its job without
    // cutting a response that is still being flushed.
    let conn = http1::Builder::new()
        .header_read_timeout(None)
        .serve_connection(TokioIo::new(stream), service);
    tokio::pin!(conn);
    let result = tokio::select! {
        result = conn.as_mut() => result,
        // The guard `wait_for` returns must not live across the await below.
        () = async { drop(stop.wait_for(|stop| *stop).await) } => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    };
    if let Err(e) = result {
        tracing::debug!(%peer, error = %e, "connection ended with an error");
    }
}

/// Server certificate chain and key from `identity`; client certificates
/// optional, but if present they must chain to the CA.
fn tls_config(identity: &ServerIdentity) -> Result<Arc<rustls::ServerConfig>> {
    let tls = |e: &dyn std::fmt::Display| Error::Tls(e.to_string());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(identity.ca_cert_der().to_vec()))
        .map_err(|e| tls(&e))?;
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .allow_unauthenticated()
        .build()
        .map_err(|e| tls(&e))?;
    let chain = CertificateDer::pem_slice_iter(identity.server_chain_pem().as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| tls(&e))?;
    let key =
        PrivateKeyDer::from_pem_slice(identity.server_key_pem().as_bytes()).map_err(|e| tls(&e))?;
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| tls(&e))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| tls(&e))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
