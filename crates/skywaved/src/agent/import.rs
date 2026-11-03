//! Preparing the import of an outside conversation's history, for the
//! adopting `CreateSession` that logs it (architecture.md "Server", D37).
//!
//! [`prepare`] reads the history from an [`OutsideConversations`] source
//! and bounds it into `Imported` items exactly as the live mapper bounds an
//! agent's events, with the same helpers: what an event leaves out goes to
//! a blob. Its blobs are written but not recorded, so preparing touches no
//! database and holds no lock; [`EventLog::handle_with`] records them in the
//! command's transaction. Each imported call gets a fresh [`CallId`], and a
//! result links to the call the agent's tool-use id names, the newer call
//! when the agent reused an id, like the mapper.
//!
//! A [`Prepared`] import owns its written blobs: dropped without its events
//! having been committed (the command was answered already, refused, the
//! transaction failed, or a caller gave up on it), it removes their files,
//! so no path leaves an orphan.
//!
//! [`EventLog::handle_with`]: crate::EventLog::handle_with

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use skywave_proto::limits::REFUSAL_MAX_CHARS;
use skywave_proto::{
    BlobRef, CallId, ContextId, EventKind, ImportOutcome, ImportedItem, SessionId, ToolCall,
};

use super::mapper::{call_display, cut_line, tool_result};
use super::{AdapterEvent, AgentCallId, HistoryItem, HistoryRead, OutsideConversations};
use crate::blobs::{BlobMeta, BlobStore};
use crate::error::Result;
use crate::event_log;

/// Longest an import may take, the read and the blob writes together.
/// The creating device waits for the command's reply and gives up on it
/// after its request timeout (30 s), treating that as a dead link; this
/// leaves the reply well inside it. Past it the outcome is
/// `Failed("timed out")` and the session is still created.
pub const IMPORT_DEADLINE: Duration = Duration::from_secs(20);

/// The reason an import past its deadline fails with.
pub const TIMED_OUT: &str = "timed out";

/// The reason an import fails with when the workdir canonicalised
/// differently when the command was handled than when it was read.
pub const WORKDIR_CHANGED: &str = "workdir changed";

/// Items bounded between two deadline checks.
pub const DEADLINE_EVERY: usize = 64;

/// One imported item and when the agent recorded it.
type Item = (Option<DateTime<Utc>>, ImportedItem);

/// An import ready to log: its items, or the outcome that replaces them,
/// and the blobs its items refer to, written but not recorded.
#[derive(Debug)]
pub struct Prepared {
    /// The canonical workdir the history was read for; `None` when the
    /// workdir was refused then, so nothing was read.
    workdir: Option<String>,
    read: std::result::Result<Vec<Item>, ImportOutcome>,
    blobs: BlobStore,
    metas: Vec<BlobMeta>,
    /// Set once its blob rows and events are appended to a transaction.
    logged: bool,
}

/// Reads conversation `id` of the canonical `workdir` from `source` and
/// bounds it, writing what the events leave out to `blobs`. Blocking: call
/// it off the async runtime. The source gets `deadline`; bounding checks
/// it every [`DEADLINE_EVERY`] items and past it stops with the outcome
/// `Failed("timed out")` and no blobs left behind. Those checks only bound
/// the work a late import wastes: whether a finished import came too late
/// is the caller's to decide (the API stops waiting at the deadline and
/// drops what comes after, which removes its blobs).
pub fn prepare(
    source: &dyn OutsideConversations,
    blobs: BlobStore,
    workdir: &str,
    id: &str,
    deadline: Instant,
) -> Prepared {
    let mut prepared = Prepared {
        workdir: Some(workdir.to_owned()),
        read: Ok(Vec::new()),
        blobs,
        metas: Vec::new(),
        logged: false,
    };
    let history = match source.history(Path::new(workdir), id, deadline) {
        HistoryRead::Read { items } => items,
        HistoryRead::NotFound => return prepared.ended(ImportOutcome::NotFound),
        HistoryRead::Failed(reason) => return prepared.failed(&reason),
        HistoryRead::Unavailable => return prepared.ended(ImportOutcome::Unavailable),
    };
    let mut items = Vec::with_capacity(history.len());
    let mut calls: HashMap<AgentCallId, CallId> = HashMap::new();
    for (n, item) in history.into_iter().enumerate() {
        if n > 0 && n % DEADLINE_EVERY == 0 && Instant::now() >= deadline {
            return prepared.failed(TIMED_OUT);
        }
        match prepared.bound(item, &mut calls) {
            Ok(Some(item)) => items.push(item),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "storing an imported conversation's blobs failed");
                return prepared.failed("could not store the history");
            }
        }
    }
    prepared.read = Ok(items);
    prepared
}

impl Prepared {
    /// An import of the canonical `workdir` that ended with `outcome` and
    /// brought nothing.
    pub fn without_items(blobs: BlobStore, workdir: &str, outcome: ImportOutcome) -> Prepared {
        Prepared {
            workdir: Some(workdir.to_owned()),
            read: Err(outcome),
            blobs,
            metas: Vec::new(),
            logged: false,
        }
    }

    /// No import: the workdir was refused when the history was to be read.
    /// A create that then accepts it anyway (the directory appeared in
    /// between) logs `Failed("workdir changed")`, as for any workdir that
    /// canonicalises otherwise than when read; never `Unavailable`, which
    /// would claim the agent cannot read its conversations.
    pub fn workdir_refused(blobs: BlobStore) -> Prepared {
        Prepared {
            workdir: None,
            read: Err(ImportOutcome::Failed {
                reason: WORKDIR_CHANGED.to_owned(),
            }),
            blobs,
            metas: Vec::new(),
            logged: false,
        }
    }

    /// An import of `workdir` that its caller stopped waiting for at the
    /// deadline: `Failed("timed out")`.
    pub fn timed_out(blobs: BlobStore, workdir: &str) -> Prepared {
        let outcome = ImportOutcome::Failed {
            reason: TIMED_OUT.to_owned(),
        };
        Prepared::without_items(blobs, workdir, outcome)
    }

    /// The canonical workdir the history was read for, `None` if refused.
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }

    /// The outcome its `ImportFinished` will carry.
    pub fn outcome(&self) -> ImportOutcome {
        match &self.read {
            Ok(items) => ImportOutcome::Imported {
                items: u32::try_from(items.len()).unwrap_or(u32::MAX),
            },
            Err(outcome) => outcome.clone(),
        }
    }

    /// Records its blob rows and appends an `Imported` for each item to
    /// session `session_id`, stamped with its first `context`, inside the
    /// caller's transaction; returns the outcome its `ImportFinished`
    /// carries. The blobs stay its own until [`Prepared::kept`].
    pub(crate) fn log(
        &mut self,
        tx: &Connection,
        session_id: SessionId,
        context: ContextId,
    ) -> Result<ImportOutcome> {
        for meta in &self.metas {
            self.blobs.record(tx, meta)?;
        }
        if let Ok(items) = &self.read {
            for (at, item) in items {
                let kind = EventKind::Imported {
                    at: *at,
                    item: item.clone(),
                };
                event_log::append_in(tx, session_id, context, None, &kind)?;
            }
        }
        self.logged = true;
        Ok(self.outcome())
    }

    /// Called once the transaction it was logged in is committed: from now
    /// on its blobs are the log's. One never logged still removes them.
    pub(crate) fn kept(mut self) {
        if self.logged {
            self.metas.clear();
        }
    }

    /// Ends the import with `outcome`, removing what it wrote.
    fn ended(mut self, outcome: ImportOutcome) -> Prepared {
        self.discard();
        self.read = Err(outcome);
        self
    }

    /// Ends the import as failed for `reason`, bounded.
    fn failed(self, reason: &str) -> Prepared {
        let reason = cut_line(reason, REFUSAL_MAX_CHARS).0;
        self.ended(ImportOutcome::Failed { reason })
    }

    /// `item` bounded, or `None` for an agent event history never shows.
    fn bound(
        &mut self,
        item: HistoryItem,
        calls: &mut HashMap<AgentCallId, CallId>,
    ) -> Result<Option<Item>> {
        let Prepared { blobs, metas, .. } = self;
        let mut put = |bytes: &[u8]| -> Result<BlobRef> {
            let meta = blobs.write(bytes)?;
            let blob = meta.blob_ref();
            metas.push(meta);
            Ok(blob)
        };
        let (at, event) = match item {
            HistoryItem::User { at, text } => {
                return Ok(Some((at, ImportedItem::UserMessage { text })));
            }
            HistoryItem::Agent { at, event } => (at, event),
        };
        let item = match event {
            AdapterEvent::Text { text, .. } => ImportedItem::AgentMessage { text },
            AdapterEvent::ToolCall {
                call,
                name,
                input,
                display,
                ..
            } => {
                let id = CallId::new();
                let (display, input) = call_display(&mut put, &input, display)?;
                if let Some(call) = call {
                    calls.insert(call, id);
                }
                ImportedItem::ToolCall(ToolCall {
                    parent: None,
                    call: Some(id),
                    name,
                    display,
                    input,
                })
            }
            AdapterEvent::ToolResult { call, result } => {
                let call = call.and_then(|call| calls.get(&call).copied());
                ImportedItem::ToolResult(tool_result(&mut put, call, result)?)
            }
            other => {
                tracing::debug!(event = ?other, "not importing an agent event");
                return Ok(None);
            }
        };
        Ok(Some((at, item)))
    }

    fn discard(&mut self) {
        for meta in self.metas.drain(..) {
            self.blobs.discard(meta.id);
        }
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        self.discard();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake::FakeOutside;
    use crate::agent::{ResultDisplay, ToolDisplay, ToolKind};

    fn files(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    fn long(n: usize) -> String {
        "line\n".repeat(n)
    }

    fn history() -> Vec<HistoryItem> {
        let call = |id: &str, command: String| HistoryItem::Agent {
            at: None,
            event: AdapterEvent::ToolCall {
                call: Some(AgentCallId::from(id)),
                parent: None,
                name: "Bash".into(),
                input: serde_json::json!({ "command": command }),
                display: Some(ToolDisplay {
                    kind: ToolKind::Shell,
                    title: command,
                    detail: None,
                    body: None,
                    shows_all_input: true,
                }),
            },
        };
        let result = |id: &str, output: String| HistoryItem::Agent {
            at: None,
            event: AdapterEvent::ToolResult {
                call: Some(AgentCallId::from(id)),
                result: ResultDisplay {
                    is_error: false,
                    status: None,
                    preview: None,
                    output,
                },
            },
        };
        vec![
            HistoryItem::User {
                at: Some("2026-11-01T10:00:00Z".parse().unwrap()),
                text: "hi".into(),
            },
            call("t1", "ls".into()),
            result("t1", long(500)),
            HistoryItem::Agent {
                at: None,
                event: AdapterEvent::TurnStarted,
            },
            HistoryItem::Agent {
                at: None,
                event: AdapterEvent::Text {
                    text: "done".into(),
                    parent: None,
                },
            },
            result("unknown", "x".into()),
        ]
    }

    #[test]
    fn items_are_bounded_and_results_linked_to_fresh_call_ids() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeOutside::new();
        source.conversation(dir.path(), "c", HistoryRead::Read { items: history() });
        let workdir = dir.path().to_str().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        std::fs::create_dir(dir.path().join("blobs")).unwrap();
        let prepared = prepare(
            &source,
            blobs,
            workdir,
            "c",
            Instant::now() + IMPORT_DEADLINE,
        );
        let items = prepared.read.as_ref().unwrap();
        assert_eq!(items.len(), 5, "TurnStarted is not imported");
        assert_eq!(prepared.outcome(), ImportOutcome::Imported { items: 5 });
        assert_eq!(items[0].0, Some("2026-11-01T10:00:00Z".parse().unwrap()));
        let ImportedItem::ToolCall(call) = &items[1].1 else {
            panic!("{:?}", items[1]);
        };
        let ImportedItem::ToolResult(result) = &items[2].1 else {
            panic!("{:?}", items[2]);
        };
        assert!(call.call.is_some());
        assert_eq!(result.call, call.call);
        assert!(result.output.is_some(), "a long output goes to a blob");
        let ImportedItem::ToolResult(unlinked) = &items[4].1 else {
            panic!("{:?}", items[4]);
        };
        assert_eq!(unlinked.call, None);
        assert_eq!(files(&dir.path().join("blobs")), 1);
        // Dropped unlogged, its blob goes too.
        drop(prepared);
        assert_eq!(files(&dir.path().join("blobs")), 0);
    }

    #[test]
    fn past_the_deadline_it_fails_and_leaves_no_blobs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blobs")).unwrap();
        let source = FakeOutside::new();
        // A blob is written before the first check, then removed by it.
        let mut items = history();
        items.extend((0..DEADLINE_EVERY).map(|n| HistoryItem::User {
            at: None,
            text: format!("prompt {n}"),
        }));
        source.conversation(dir.path(), "c", HistoryRead::Read { items });
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let workdir = dir.path().to_str().unwrap();
        let prepared = prepare(&source, blobs, workdir, "c", Instant::now());
        assert_eq!(
            prepared.outcome(),
            ImportOutcome::Failed {
                reason: TIMED_OUT.into()
            }
        );
        assert_eq!(files(&dir.path().join("blobs")), 0);
    }

    #[test]
    fn a_failed_read_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeOutside::new();
        source.conversation(
            dir.path(),
            "c",
            HistoryRead::Failed("x".repeat(REFUSAL_MAX_CHARS * 2)),
        );
        let blobs = BlobStore::new(dir.path());
        let workdir = dir.path().to_str().unwrap();
        let prepared = prepare(
            &source,
            blobs,
            workdir,
            "c",
            Instant::now() + IMPORT_DEADLINE,
        );
        let ImportOutcome::Failed { reason } = prepared.outcome() else {
            panic!();
        };
        assert!(reason.chars().count() <= REFUSAL_MAX_CHARS);
        let missing = prepare(
            &source,
            BlobStore::new(dir.path()),
            workdir,
            "other",
            Instant::now() + IMPORT_DEADLINE,
        );
        assert_eq!(missing.outcome(), ImportOutcome::NotFound);
    }
}
