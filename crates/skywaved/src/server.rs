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
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
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
use crate::api::{self, Api, PeerCert};
use crate::blobs::BlobStore;
use crate::data_dir::DataDir;
use crate::db::Db;
use crate::error::{Error, IoContext, Result};
use crate::identity::ServerIdentity;
use crate::pairing::{self, PairService, RateLimit, SystemClock};

/// A client gets this long to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// And this long to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// On shutdown, in-flight requests get this long before connections are
/// dropped. Long-polls end at once on shutdown, so this is plenty.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// A bound, not yet serving, HTTPS server.
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: TlsAcceptor,
    app: Router,
    stop: watch::Sender<bool>,
}

impl Server {
    /// Builds the TLS config from `identity`, opens the API's and pairing's
    /// own connections to the database in `dir`, and binds `addr`.
    ///
    /// `log` is the process's one [`EventLog`]; other writers (the agent
    /// supervisor) share it so their appends wake long-polls.
    pub async fn bind(
        addr: SocketAddr,
        dir: &DataDir,
        identity: Arc<ServerIdentity>,
        log: EventLog,
    ) -> Result<Server> {
        let tls = TlsAcceptor::from(tls_config(&identity)?);
        let (stop, stopped) = watch::channel(false);
        let api = Api::new(
            Db::open(&dir.db_path())?,
            log,
            BlobStore::new(dir.blobs_dir()),
            identity.server_id(),
            stopped,
        );
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
            ..
        } = self;
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((tcp, peer)) => {
                        connections.spawn(connection(
                            tcp,
                            peer,
                            tls.clone(),
                            app.clone(),
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

/// Serves one TCP connection: TLS handshake, then HTTP/1.1 with the peer's
/// certificate attached to every request.
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: TlsAcceptor,
    app: Router,
    mut stop: watch::Receiver<bool>,
) {
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
        let mut app = app.clone();
        async move { app.call(req.map(Body::new)).await }
    });
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
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
