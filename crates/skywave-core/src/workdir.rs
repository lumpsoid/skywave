//! Reading a session's workdir (architecture.md "Client core"): what is
//! asked of the server ([`WorkdirAsk`]), the domain answer it gave
//! ([`WorkdirAnswer`]), and how long the device waits for one
//! ([`WORKDIR_TIMEOUT`]).
//!
//! The replica keeps the latest answer per ask ([`Store::put_workdir`]),
//! so a workdir already browsed still reads offline.
//!
//! [`Store::put_workdir`]: crate::Store::put_workdir

use std::time::Duration;

use skywave_proto::{
    BlobRef, ChangesResult, DiffBody, FileBody, FileResult, ListResult, SessionId, WorkdirQuery,
};

/// Longest a workdir read may take on the device, connecting included: the
/// server's own 20 s bound on the read plus the link. Both the request
/// timeout of [`Transport::workdir_changes`](crate::Transport::workdir_changes)
/// and its siblings and the engine's outer bound on a read.
pub const WORKDIR_TIMEOUT: Duration = Duration::from_secs(30);

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
}

impl WorkdirAsk {
    pub fn session(&self) -> SessionId {
        match self {
            WorkdirAsk::Changes { session }
            | WorkdirAsk::List { session, .. }
            | WorkdirAsk::File { session, .. } => *session,
        }
    }

    /// The requested path; empty for [`WorkdirAsk::Changes`]. Not the
    /// canonical path the answer names.
    pub fn path(&self) -> &str {
        match self {
            WorkdirAsk::Changes { .. } => "",
            WorkdirAsk::List { path, .. } | WorkdirAsk::File { path, .. } => path,
        }
    }

    /// `changes`, `list` or `file`: the replica's `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            WorkdirAsk::Changes { .. } => "changes",
            WorkdirAsk::List { .. } => "list",
            WorkdirAsk::File { .. } => "file",
        }
    }

    /// The ask with `kind` (as [`kind`](WorkdirAsk::kind) names it);
    /// `None` for an unknown kind.
    pub fn from_parts(session: SessionId, kind: &str, path: String) -> Option<WorkdirAsk> {
        match kind {
            "changes" => Some(WorkdirAsk::Changes { session }),
            "list" => Some(WorkdirAsk::List { session, path }),
            "file" => Some(WorkdirAsk::File { session, path }),
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
            WorkdirAnswer::Changes(_) | WorkdirAnswer::List(_) | WorkdirAnswer::File(_) => {
                Vec::new()
            }
        }
    }

    /// Whether this answers `ask`'s kind.
    pub fn answers(&self, ask: &WorkdirAsk) -> bool {
        matches!(
            (self, ask),
            (WorkdirAnswer::Changes(_), WorkdirAsk::Changes { .. })
                | (WorkdirAnswer::List(_), WorkdirAsk::List { .. })
                | (WorkdirAnswer::File(_), WorkdirAsk::File { .. })
        )
    }
}
