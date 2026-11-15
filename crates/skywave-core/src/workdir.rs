//! Reading a session's workdir (architecture.md "Client core"): what is
//! asked of the server ([`WorkdirAsk`]), the domain answer it gave
//! ([`WorkdirAnswer`]), and how long the device waits for one
//! ([`WORKDIR_TIMEOUT`]).
//!
//! The replica keeps the latest answer per ask ([`Store::put_workdir`]),
//! so a workdir already browsed still reads offline.
//!
//! [`Store::put_workdir`]: crate::Store::put_workdir

use std::collections::HashMap;
use std::time::Duration;

use skywave_proto::{
    BlobRef, ChangesResult, DiffBody, FileBody, FileResult, ListResult, LogResult, SessionId,
    WorkdirQuery,
};

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::WorkdirFetch;

/// Longest a workdir read may take on the device, connecting included: the
/// server's own 20 s bound on the read plus the link. Both the request
/// timeout of [`Transport::workdir_changes`](crate::Transport::workdir_changes)
/// and its siblings and the engine's outer bound on a read.
pub const WORKDIR_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a write into the workdir may take on the device, connecting
/// included: the server's own 120 s bound on the write plus the link, as
/// [`WORKDIR_TIMEOUT`] allows for a read. The server gives up first, with 503.
pub const WORKDIR_WRITE_TIMEOUT: Duration = Duration::from_secs(150);

/// One read of a session's workdir, and the key its stored answer is kept
/// under: a refresh of the same ask replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkdirAsk {
    /// What changed against `HEAD`.
    Changes { session: SessionId },
    /// One directory; `path` as requested, relative to the workdir, empty
    /// for the workdir itself.
    List { session: SessionId, path: String },
    /// One file; `path` as requested, relative to the workdir.
    File { session: SessionId, path: String },
    /// One file to save on the device, text or binary
    /// (`GET /v1/workdir/download`); answered by [`WorkdirAnswer::File`].
    Download { session: SessionId, path: String },
    /// The newest commits on `HEAD`.
    Log { session: SessionId },
}

impl WorkdirAsk {
    pub fn session(&self) -> SessionId {
        match self {
            WorkdirAsk::Changes { session }
            | WorkdirAsk::Log { session }
            | WorkdirAsk::List { session, .. }
            | WorkdirAsk::File { session, .. }
            | WorkdirAsk::Download { session, .. } => *session,
        }
    }

    /// The requested path; empty for [`WorkdirAsk::Changes`] and
    /// [`WorkdirAsk::Log`]. Not the canonical path the answer names.
    pub fn path(&self) -> &str {
        match self {
            WorkdirAsk::Changes { .. } | WorkdirAsk::Log { .. } => "",
            WorkdirAsk::List { path, .. }
            | WorkdirAsk::File { path, .. }
            | WorkdirAsk::Download { path, .. } => path,
        }
    }

    /// `changes`, `list`, `file`, `log` or `download`: the replica's `kind`
    /// column.
    pub fn kind(&self) -> &'static str {
        match self {
            WorkdirAsk::Changes { .. } => "changes",
            WorkdirAsk::List { .. } => "list",
            WorkdirAsk::File { .. } => "file",
            WorkdirAsk::Log { .. } => "log",
            WorkdirAsk::Download { .. } => "download",
        }
    }

    /// The ask with `kind` (as [`kind`](WorkdirAsk::kind) names it);
    /// `None` for an unknown kind.
    pub fn from_parts(session: SessionId, kind: &str, path: String) -> Option<WorkdirAsk> {
        match kind {
            "changes" => Some(WorkdirAsk::Changes { session }),
            "list" => Some(WorkdirAsk::List { session, path }),
            "file" => Some(WorkdirAsk::File { session, path }),
            "log" => Some(WorkdirAsk::Log { session }),
            "download" => Some(WorkdirAsk::Download { session, path }),
            _ => None,
        }
    }

    /// The query string the server reads.
    pub fn query(&self) -> WorkdirQuery {
        WorkdirQuery {
            session: self.session(),
            path: self.path().to_owned(),
        }
    }
}

/// The server's domain answer to a [`WorkdirAsk`] of the same kind.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkdirAnswer {
    Changes(ChangesResult),
    List(ListResult),
    File(FileResult),
    Log(LogResult),
}

impl WorkdirAnswer {
    /// The blobs the answer names: one per diff, or the file's content.
    pub fn blob_refs(&self) -> Vec<BlobRef> {
        match self {
            WorkdirAnswer::Changes(ChangesResult::Changes(set)) => set
                .files
                .iter()
                .filter_map(|file| match &file.diff {
                    DiffBody::Blob(blob) => Some(blob.clone()),
                    DiffBody::Binary | DiffBody::TooLarge { .. } | DiffBody::Omitted => None,
                })
                .collect(),
            WorkdirAnswer::File(FileResult::File {
                content: FileBody::Blob(blob),
                ..
            }) => vec![blob.clone()],
            WorkdirAnswer::Changes(_)
            | WorkdirAnswer::List(_)
            | WorkdirAnswer::File(_)
            | WorkdirAnswer::Log(_) => Vec::new(),
        }
    }

    /// Whether this answers `ask`'s kind.
    pub fn answers(&self, ask: &WorkdirAsk) -> bool {
        matches!(
            (self, ask),
            (WorkdirAnswer::Changes(_), WorkdirAsk::Changes { .. })
                | (WorkdirAnswer::List(_), WorkdirAsk::List { .. })
                | (WorkdirAnswer::File(_), WorkdirAsk::File { .. })
                | (WorkdirAnswer::File(_), WorkdirAsk::Download { .. })
                | (WorkdirAnswer::Log(_), WorkdirAsk::Log { .. })
        )
    }
}

/// The model's workdir reads: the latest stored answer per ask, as the
/// replica keeps it, and in memory the asks in flight and the last failure
/// of each (architecture.md "Client core", "Workdir snapshots").
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Workdirs {
    /// As stored ([`Store::put_workdir`](crate::Store::put_workdir)).
    pub(crate) stored: HashMap<WorkdirAsk, WorkdirAnswer>,
    /// Id of the latest [`Request::Workdir`]; never reused, not even across
    /// a replica reset, so an answer to an old one can never pass for new.
    req: u64,
    /// The request each ask waits on; an answer to any other is stale.
    pending: HashMap<WorkdirAsk, u64>,
    /// Why the last ask failed, until it is asked again or answered.
    failed: HashMap<WorkdirAsk, String>,
}

/// Why a side request (a workdir read, a listing) fails at once while
/// the link is down, unpaired, suspended or rejected: in the engine and,
/// for a workdir read the model does not send, in the model.
pub(crate) fn not_connected() -> TransportError {
    TransportError::Unreachable("not connected".into())
}

impl Workdirs {
    /// Asks the server for `ask`, or, offline, fails it at once: nothing is
    /// queued for later.
    pub(crate) fn ask(&mut self, ask: WorkdirAsk, online: bool) -> Option<Request> {
        if !online {
            self.pending.remove(&ask);
            self.failed.insert(ask, not_connected().to_string());
            return None;
        }
        self.req += 1;
        self.failed.remove(&ask);
        self.pending.insert(ask.clone(), self.req);
        Some(Request::Workdir { req: self.req, ask })
    }

    /// Whether `ask` has neither a stored answer nor one on its way: the
    /// first show of its pane asks.
    pub(crate) fn unread(&self, ask: &WorkdirAsk) -> bool {
        !self.stored.contains_key(ask) && !self.pending.contains_key(ask)
    }

    /// Whether `req` is the request `ask` still waits on.
    pub(crate) fn awaited(&self, req: u64, ask: &WorkdirAsk) -> bool {
        self.pending.get(ask) == Some(&req)
    }

    /// The awaited answer is kept as the latest (already stored).
    pub(crate) fn answered(&mut self, ask: WorkdirAsk, answer: WorkdirAnswer) {
        self.pending.remove(&ask);
        self.failed.remove(&ask);
        self.stored.insert(ask, answer);
    }

    /// The awaited answer failed; the stored one stays.
    pub(crate) fn failed(&mut self, ask: &WorkdirAsk, reason: String) {
        self.pending.remove(ask);
        self.failed.insert(ask.clone(), reason);
    }

    /// The awaited answer was dropped (its session left the replica).
    pub(crate) fn dropped(&mut self, ask: &WorkdirAsk) {
        self.pending.remove(ask);
    }

    /// The session left the replica: everything read of it goes.
    pub(crate) fn forget(&mut self, session: SessionId) {
        self.stored.retain(|ask, _| ask.session() != session);
        self.pending.retain(|ask, _| ask.session() != session);
        self.failed.retain(|ask, _| ask.session() != session);
    }

    /// The replica was reset: nothing asked before is awaited any more.
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.failed.clear();
    }

    pub(crate) fn fetch(&self, ask: &WorkdirAsk) -> WorkdirFetch {
        if self.pending.contains_key(ask) {
            WorkdirFetch::Loading
        } else if let Some(reason) = self.failed.get(ask) {
            WorkdirFetch::Failed(reason.clone())
        } else {
            WorkdirFetch::Idle
        }
    }

    /// The paths of `session`'s download asks that are stored, in flight or
    /// failed: each once, sorted.
    pub(crate) fn download_paths(&self, session: SessionId) -> Vec<String> {
        let mut paths: Vec<String> = self
            .stored
            .keys()
            .chain(self.pending.keys())
            .chain(self.failed.keys())
            .filter(|ask| ask.session() == session)
            .filter_map(|ask| match ask {
                WorkdirAsk::Download { path, .. } => Some(path.clone()),
                WorkdirAsk::Changes { .. }
                | WorkdirAsk::List { .. }
                | WorkdirAsk::File { .. }
                | WorkdirAsk::Log { .. } => None,
            })
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// The parent of a workdir path; empty for a top-level one.
pub(crate) fn parent(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent)
}
