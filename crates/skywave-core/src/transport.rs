//! How the core talks to the server (architecture.md "Protocol", D15): the [`Transport`]
//! trait and its typed errors. [`HttpsTransport`](crate::HttpsTransport) is
//! the MVP implementation; QUIC or SSE can replace it behind this trait.
//!
//! The trait is object-safe (`Arc<dyn Transport>`), so the sync engine and
//! the link harness can wrap or inject one.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use skywave_proto::{
    BlobId, ChangesResult, Command, CommandResult, DirsResult, EventBatch, EventsQuery, FileResult,
    HelloResponse, ListResult, WorkdirQuery,
};

use crate::view::LinkRejection;

/// A boxed, sendable future, so [`Transport`] stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The v1 API as seen by a paired device.
pub trait Transport: Send + Sync {
    /// `GET /v1/hello`. A server speaking another protocol version is
    /// [`TransportError::ProtocolMismatch`].
    fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>>;

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
    /// The TLS handshake failed, e.g. the server presented a certificate
    /// that does not chain to the pinned CA, or rejected ours.
    #[error("TLS handshake failed: {0}")]
    Tls(String),
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
            | TransportError::Io(_) => true,
            TransportError::BadStatus(status) => *status >= 500,
            TransportError::Unauthorized
            | TransportError::Revoked
            | TransportError::ProtocolMismatch { .. }
            | TransportError::NotFound
            | TransportError::RangeNotSatisfiable { .. }
            | TransportError::Decode(_)
            | TransportError::BadIdentity(_) => false,
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

    /// The server refuses this device for good: the link becomes
    /// [`Link::Rejected`](crate::Link::Rejected) until the user re-pairs.
    pub fn rejection(&self) -> Option<LinkRejection> {
        match self {
            TransportError::Unauthorized | TransportError::Revoked => Some(LinkRejection::Revoked),
            TransportError::ProtocolMismatch { client, server } => {
                Some(LinkRejection::ProtocolMismatch {
                    client: *client,
                    server: *server,
                })
            }
            _ => None,
        }
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
    }
}
