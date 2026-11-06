//! How the core talks to the server (architecture.md "Protocol", D15): the [`Transport`]
//! trait and its typed errors. [`HttpsTransport`](crate::HttpsTransport) is
//! the MVP implementation; QUIC or SSE can replace it behind this trait.
//!
//! The trait is object-safe (`Arc<dyn Transport>`), so the sync engine and
//! the link harness can wrap or inject one.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use skywave_proto::{
    BlobId, ChangesResult, Command, CommandResult, ConversationsResult, DirsResult, EventBatch,
    EventsQuery, FileResult, HelloResponse, ListResult, RunId, StorageReport, WorkdirQuery,
};

use crate::view::{LinkFailure, LinkRejection};

/// A boxed, sendable future, so [`Transport`] stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The run of the server the replica is synced with, as every request of
/// one link names it ([`RUN_HEADER`](skywave_proto::RUN_HEADER)), except a
/// `hello` asking about a run: a server in another run refuses the request
/// as [`TransportError::RunChanged`]. Unset, requests name none and nothing
/// is checked (first contact).
///
/// One per link, shared by every transport the engine builds for it (the
/// link's own, the blob downloader's and the side transport's for listings
/// and workdir reads), each reading it per request. Only the link sets it,
/// once a connection's `hello` found the log still holds the replica, so a
/// clean restart the link has carried on from reaches every transport at
/// once, and none of them is left naming a run that is gone.
#[derive(Debug, Clone, Default)]
pub struct ExpectedRun(Arc<Mutex<Option<RunId>>>);

impl ExpectedRun {
    /// From now on, every request names `run`.
    pub fn expect(&self, run: RunId) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(run);
    }

    /// The run requests name now, if any.
    pub fn get(&self) -> Option<RunId> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The v1 API as seen by a paired device.
pub trait Transport: Send + Sync {
    /// `GET /v1/hello`, asking what became of run `since` of the server
    /// ([`HelloQuery`](skywave_proto::HelloQuery)). A server speaking another
    /// protocol version is [`TransportError::ProtocolMismatch`].
    fn hello(&self, since: Option<RunId>) -> BoxFuture<'_, Result<HelloResponse, TransportError>>;

    /// `POST /v1/commands`. Resending a command id is safe (the server
    /// returns the original result).
    fn send_command<'a>(
        &'a self,
        command: &'a Command,
    ) -> BoxFuture<'a, Result<CommandResult, TransportError>>;

    /// `GET /v1/events`: a long-poll when `query.wait` is set. The request
    /// timeout always exceeds the wait.
    fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>>;

    /// `GET /v1/blobs/{id}` from byte `from` on (`Range: bytes=<from>-`
    /// when `from > 0`, so an interrupted download resumes). `from` at or
    /// past the end is [`TransportError::RangeNotSatisfiable`].
    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>>;

    /// `GET /v1/dirs?dir=`: the subdirectories of `dir` for workdir
    /// completion; an empty `dir` asks for the roots. A server without the
    /// endpoint answers [`TransportError::NotFound`], which is also what a
    /// transport that cannot list returns by default.
    fn list_dirs<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<DirsResult, TransportError>> {
        let _ = dir;
        Box::pin(async { Err(TransportError::NotFound) })
    }

    /// `GET /v1/conversations`: the conversations the server's agent keeps
    /// for workdirs under the roots, for the conversation picker. Defaults
    /// to [`TransportError::NotFound`] like
    /// [`list_dirs`](Transport::list_dirs).
    fn list_conversations(&self) -> BoxFuture<'_, Result<ConversationsResult, TransportError>> {
        Box::pin(async { Err(TransportError::NotFound) })
    }

    /// `GET /v1/storage`: what the server's data takes on its disk, for
    /// the storage screen. Defaults to [`TransportError::NotFound`] like
    /// [`list_dirs`](Transport::list_dirs).
    fn storage_report(&self) -> BoxFuture<'_, Result<StorageReport, TransportError>> {
        Box::pin(async { Err(TransportError::NotFound) })
    }

    /// `GET /v1/workdir/changes?session=`: what changed in the session's
    /// workdir. Every domain outcome is an answer; a server without the
    /// endpoint answers [`TransportError::NotFound`], as does a transport
    /// that cannot read workdirs by default.
    fn workdir_changes<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<ChangesResult, TransportError>> {
        let _ = query;
        Box::pin(async { Err(TransportError::NotFound) })
    }

    /// `GET /v1/workdir/list?session=&path=`: one directory of the
    /// session's workdir. Defaults to [`TransportError::NotFound`] like
    /// [`workdir_changes`](Transport::workdir_changes).
    fn workdir_list<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<ListResult, TransportError>> {
        let _ = query;
        Box::pin(async { Err(TransportError::NotFound) })
    }

    /// `GET /v1/workdir/file?session=&path=`: one file of the session's
    /// workdir, its content as a blob. Defaults to
    /// [`TransportError::NotFound`] like
    /// [`workdir_changes`](Transport::workdir_changes).
    fn workdir_file<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<FileResult, TransportError>> {
        let _ = query;
        Box::pin(async { Err(TransportError::NotFound) })
    }
}

/// A blob response whose body is still arriving.
pub struct BlobDownload {
    /// Offset of the first body byte. Normally the requested `from`; 0 if
    /// the server sent the whole blob instead of the range.
    pub offset: u64,
    /// Size of the whole blob.
    pub total: u64,
    pub body: Box<dyn BlobBody>,
}

impl std::fmt::Debug for BlobDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobDownload")
            .field("offset", &self.offset)
            .field("total", &self.total)
            .finish_non_exhaustive()
    }
}

/// The body of a [`BlobDownload`], chunk by chunk.
pub trait BlobBody: Send {
    /// The next chunk, or `None` at the end. A read that stalls longer
    /// than the request timeout is [`TransportError::Timeout`].
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>>;
}

/// Why a transport call failed. [`is_retryable`](Self::is_retryable) and
/// [`rejection`](Self::rejection) tell the sync engine what to do with it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// No connection: DNS, TCP connect or a refused connection.
    #[error("server unreachable: {0}")]
    Unreachable(String),
    /// The TLS handshake failed for a reason other than the server's
    /// identity, e.g. the server rejected our certificate or the
    /// connection broke mid-handshake.
    #[error("TLS handshake failed: {0}")]
    Tls(String),
    /// The server presented a certificate that does not chain to the
    /// pinned CA (rustls `UnknownIssuer`).
    #[error("the server's certificate is not from the CA this device trusts: {0}")]
    UntrustedServer(String),
    /// The server's certificate chains to the pinned CA but does not
    /// cover the address connected to (rustls `NotValidForName`).
    #[error("the server's certificate does not cover this address: {0}")]
    ServerNameMismatch(String),
    /// Connect, request or body read took too long.
    #[error("timed out")]
    Timeout,
    /// The connection broke during a request.
    #[error("connection error: {0}")]
    Io(String),
    /// 401: the server saw no client certificate.
    #[error("the server did not accept this device's certificate (401)")]
    Unauthorized,
    /// 403: the device is unknown to the server or revoked.
    #[error("this device was revoked or is unknown to the server (403)")]
    Revoked,
    #[error("protocol mismatch: this device speaks v{client}, the server v{server}")]
    ProtocolMismatch { client: u32, server: u32 },
    /// 404: e.g. an unknown blob.
    #[error("not found (404)")]
    NotFound,
    /// 416: the requested offset is past the blob's end; `size` is the
    /// blob's size when the server said it.
    #[error("range not satisfiable (416)")]
    RangeNotSatisfiable { size: Option<u64> },
    /// 412: the server is in another run than the one the request named
    /// ([`ExpectedRun`]): it restarted, or was rolled back under
    /// a connection that never closed. It did nothing with the request.
    /// The next connection's `hello` tells which.
    #[error("the server is in another run than this device expects (412)")]
    RunChanged,
    /// Any other non-2xx status.
    #[error("unexpected HTTP status {0}")]
    BadStatus(u16),
    /// A 2xx response whose body or headers could not be understood.
    #[error("invalid response: {0}")]
    Decode(String),
    /// The stored identity cannot be used for TLS (bad PEM, key mismatch).
    #[error("the device identity is unusable: {0}")]
    BadIdentity(String),
}

impl TransportError {
    /// Worth retrying with backoff (D7): the link, not the device, failed.
    /// 5xx statuses count; other statuses and decode errors do not.
    pub fn is_retryable(&self) -> bool {
        match self {
            TransportError::Unreachable(_)
            | TransportError::Tls(_)
            | TransportError::Timeout
            | TransportError::Io(_)
            | TransportError::RunChanged => true,
            TransportError::BadStatus(status) => *status >= 500,
            TransportError::Unauthorized
            | TransportError::Revoked
            | TransportError::UntrustedServer(_)
            | TransportError::ServerNameMismatch(_)
            | TransportError::ProtocolMismatch { .. }
            | TransportError::NotFound
            | TransportError::RangeNotSatisfiable { .. }
            | TransportError::Decode(_)
            | TransportError::BadIdentity(_) => false,
        }
    }

    /// What this error does to the link, decided from its kind alone: a
    /// rejection stops it until the user acts; anything else is a failure
    /// it retries, shown with the countdown.
    pub fn link_verdict(&self) -> Result<LinkFailure, LinkRejection> {
        match self {
            TransportError::Unauthorized | TransportError::Revoked => Err(LinkRejection::Revoked),
            TransportError::UntrustedServer(_) => Err(LinkRejection::ServerChanged),
            TransportError::ServerNameMismatch(_) => Err(LinkRejection::NameMismatch),
            TransportError::BadIdentity(_) => Err(LinkRejection::BadIdentity),
            TransportError::ProtocolMismatch { client, server } => {
                Err(LinkRejection::ProtocolMismatch {
                    client: *client,
                    server: *server,
                })
            }
            TransportError::Unreachable(_) => Ok(LinkFailure::Unreachable),
            TransportError::Tls(_) => Ok(LinkFailure::Handshake),
            TransportError::Timeout => Ok(LinkFailure::TimedOut),
            TransportError::Io(_) => Ok(LinkFailure::ConnectionLost),
            TransportError::RunChanged => Ok(LinkFailure::ServerRestarted),
            TransportError::NotFound => Ok(LinkFailure::ServerError(404)),
            TransportError::RangeNotSatisfiable { .. } => Ok(LinkFailure::ServerError(416)),
            TransportError::BadStatus(status) => Ok(LinkFailure::ServerError(*status)),
            TransportError::Decode(_) => Ok(LinkFailure::BadResponse),
        }
    }

    /// The server refused this one command for good, so the outbox gives
    /// up on it rather than resend it ahead of every later one: a 4xx that
    /// is about the request (not the device, 401/403, nor the endpoint,
    /// 404; 408 and 429 only ask to come back later). An unreadable
    /// response is not a refusal: the server may have taken the command,
    /// and failing it would invite the user to send it twice. A
    /// [`CommandResult`] rejection is an answer, not an error, and never
    /// gets here.
    pub fn refuses_command(&self) -> bool {
        match self {
            TransportError::BadStatus(status) => {
                (400..500).contains(status) && !matches!(status, 408 | 429)
            }
            _ => false,
        }
    }

    /// The server answered with an error of its own (5xx): it is up, and
    /// the request reached something that could not handle it. Unlike a
    /// timeout or a broken connection, this is a definitive answer, so the
    /// outbox counts it against the command
    /// ([`MAX_SERVER_ERRORS`](crate::MAX_SERVER_ERRORS)).
    pub fn is_server_error(&self) -> bool {
        matches!(self, TransportError::BadStatus(status) if (500..600).contains(status))
    }

    /// This one request failed, which says nothing yet about the link: it
    /// timed out ([`Timeout`](Self::Timeout): for a command, its upload
    /// stalled or its reply never came), its connection broke, or the
    /// server gave up waiting for its body (408). Each request has a
    /// connection of its own, so the others, the long-poll among them, may
    /// be fine; if the link is down, they fail too. Unlike a server error,
    /// it is no answer: the outbox resends without counting it against
    /// the command.
    pub fn failed_request(&self) -> bool {
        matches!(
            self,
            TransportError::Timeout | TransportError::Io(_) | TransportError::BadStatus(408)
        )
    }

    /// No retry can succeed: the link becomes
    /// [`Link::Rejected`](crate::Link::Rejected) until the user acts
    /// ([`link_verdict`](Self::link_verdict)).
    pub fn rejection(&self) -> Option<LinkRejection> {
        self.link_verdict().err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_errors_for_the_link() {
        assert!(TransportError::Timeout.is_retryable());
        assert!(TransportError::BadStatus(503).is_retryable());
        assert!(!TransportError::BadStatus(400).is_retryable());
        assert_eq!(
            TransportError::Revoked.rejection(),
            Some(LinkRejection::Revoked)
        );
        assert_eq!(
            TransportError::Unauthorized.rejection(),
            Some(LinkRejection::Revoked)
        );
        assert_eq!(
            TransportError::ProtocolMismatch {
                client: 1,
                server: 2
            }
            .rejection(),
            Some(LinkRejection::ProtocolMismatch {
                client: 1,
                server: 2
            })
        );
        assert_eq!(TransportError::Io("x".into()).rejection(), None);
        assert!(TransportError::BadStatus(400).refuses_command());
        assert!(TransportError::BadStatus(413).refuses_command());
        assert!(!TransportError::Decode("x".into()).refuses_command());
        assert!(!TransportError::BadStatus(429).refuses_command());
        assert!(!TransportError::BadStatus(500).refuses_command());
        assert!(!TransportError::NotFound.refuses_command());
        assert!(!TransportError::Revoked.refuses_command());
        assert!(!TransportError::Timeout.refuses_command());
        // Another run: the connection ends and its `hello` decides; the
        // command was not refused, nor the device.
        assert!(TransportError::RunChanged.is_retryable());
        assert!(!TransportError::RunChanged.refuses_command());
        assert!(!TransportError::RunChanged.is_server_error());
        assert_eq!(TransportError::RunChanged.rejection(), None);
        assert!(TransportError::BadStatus(500).is_server_error());
        assert!(TransportError::BadStatus(503).is_server_error());
        assert!(!TransportError::BadStatus(429).is_server_error());
        assert!(!TransportError::Timeout.is_server_error());
        assert!(!TransportError::Io("x".into()).is_server_error());
    }

    /// An identity fault is a rejection from its kind alone; a handshake
    /// that failed otherwise, and a restarted server, stay retryable and
    /// say why.
    #[test]
    fn identity_faults_reject_and_transient_failures_say_why() {
        let rejected = [
            (
                TransportError::UntrustedServer("x".into()),
                LinkRejection::ServerChanged,
            ),
            (
                TransportError::ServerNameMismatch("x".into()),
                LinkRejection::NameMismatch,
            ),
            (
                TransportError::BadIdentity("x".into()),
                LinkRejection::BadIdentity,
            ),
        ];
        for (error, rejection) in rejected {
            assert!(!error.is_retryable(), "{error:?}");
            assert_eq!(error.link_verdict(), Err(rejection), "{error:?}");
            assert!(rejection.needs_repair(), "{rejection:?}");
        }
        let retried = [
            (TransportError::Tls("x".into()), LinkFailure::Handshake),
            (TransportError::RunChanged, LinkFailure::ServerRestarted),
            (
                TransportError::Unreachable("x".into()),
                LinkFailure::Unreachable,
            ),
            (TransportError::Timeout, LinkFailure::TimedOut),
            (TransportError::Io("x".into()), LinkFailure::ConnectionLost),
            (
                TransportError::BadStatus(503),
                LinkFailure::ServerError(503),
            ),
        ];
        for (error, failure) in retried {
            assert_eq!(error.link_verdict(), Ok(failure), "{error:?}");
        }
        assert!(LinkRejection::ServerChanged.can_retry());
        assert!(LinkRejection::NameMismatch.can_retry());
        assert!(!LinkRejection::BadIdentity.can_retry());
        assert!(!LinkRejection::Revoked.can_retry());
        assert!(
            !LinkRejection::ProtocolMismatch {
                client: 1,
                server: 2
            }
            .needs_repair()
        );
    }
}
