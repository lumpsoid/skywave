//! The SQLite database (`skywave.db`) and its schema migrations.
//!
//! `serve` and the admin commands open the same database concurrently
//! (architecture.md "Server"), so connections use WAL mode and a busy timeout.
//!
//! Migrations are versioned by `PRAGMA user_version`: migration `i` (0-based)
//! moves the schema from version `i` to `i + 1`. Append new migrations; never
//! edit a released one.
//!
//! The JSON columns hold wire JSON, which a schema migration does not
//! rewrite, so the database is also stamped with the `PROTOCOL_VERSION` its
//! JSON is in (`protocol_stamp`). Opening one stamped with an older version
//! since [`MIGRATED_SINCE`] runs the [`PROTOCOL_STEPS`] from it (D43); any
//! other stamp fails with [`Error::ProtocolMismatch`] before anything reads
//! it.
//!
//! Column conventions: ids are hyphenated UUID text, timestamps are RFC 3339
//! UTC text, `JSON` columns hold the serde_json encoding of the named
//! `skywave-proto` type.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use skywave_proto::{LogId, MIGRATED_SINCE, PROTOCOL_VERSION};
use uuid::Uuid;

use crate::error::{Error, Result};

/// How long a writer waits for another connection's lock before failing.
///
/// Long, because a busy error is fatal where it matters most: `serve`'s
/// agent supervisor and shell runner stop the daemon on a failed append
/// (architecture.md "Server"). An admin command, an adopting import or a
/// workdir answer can hold the write lock for seconds; that must only
/// delay the daemon's writes, never end it. A lock held for longer than
/// this is a stuck writer, not contention.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(60);

const MIGRATIONS: &[&str] = &[
    // v1: initial schema (architecture.md "Server").
    r#"
    -- Global event log (D12). seq is assigned by the single writer: the first
    -- event is 1 and each later event is exactly one more.
    CREATE TABLE events (
        seq        INTEGER PRIMARY KEY CHECK (seq >= 1),
        session_id TEXT NOT NULL,
        at         TEXT NOT NULL,
        kind       TEXT NOT NULL CHECK (json_valid(kind))   -- EventKind
    ) STRICT;
    CREATE INDEX events_session ON events (session_id, seq);

    -- Command dedup: a known command_id returns the stored result.
    CREATE TABLE commands (
        command_id TEXT PRIMARY KEY,
        device_id  TEXT NOT NULL,
        result     TEXT NOT NULL CHECK (json_valid(result)) -- CommandResult
    ) STRICT;

    CREATE TABLE sessions (
        id          TEXT PRIMARY KEY,
        workdir     TEXT NOT NULL,
        -- The agent's opaque resume handle; NULL until the agent reports one.
        agent_state TEXT CHECK (agent_state IS NULL OR json_valid(agent_state))
    ) STRICT;

    CREATE TABLE devices (
        id               TEXT PRIMARY KEY,
        name             TEXT NOT NULL,
        cert_fingerprint TEXT NOT NULL UNIQUE,
        paired_at        TEXT NOT NULL,
        revoked_at       TEXT,
        last_seen        TEXT
    ) STRICT;

    -- One-time pairing codes, stored hashed.
    CREATE TABLE pair_codes (
        hash       TEXT PRIMARY KEY,
        expires_at TEXT NOT NULL,
        used       INTEGER NOT NULL DEFAULT 0 CHECK (used IN (0, 1))
    ) STRICT;

    -- Metadata of files in blobs/; sha256 is lowercase hex.
    CREATE TABLE blobs (
        id     TEXT PRIMARY KEY,
        size   INTEGER NOT NULL CHECK (size >= 0),
        sha256 TEXT NOT NULL
    ) STRICT;
    "#,
    // v2: projections that command validation reads (T03). They are derived
    // from `events` and updated in the transaction that appends the event.
    r#"
    -- 1 while the session's agent has a running turn: set by
    -- SessionStateChanged(AgentWorking), cleared by SessionStateChanged(Idle
    -- or Errored) and by TurnInterrupted.
    ALTER TABLE sessions ADD COLUMN
        turn_active INTEGER NOT NULL DEFAULT 0 CHECK (turn_active IN (0, 1));

    -- Approval requests seen in the log.
    CREATE TABLE approvals (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions (id),
        -- Seq of the ApprovalResolved or ApprovalCancelled that closed the
        -- request; NULL while it is pending.
        closed_seq INTEGER
    ) STRICT;
    CREATE INDEX approvals_pending ON approvals (closed_seq) WHERE closed_seq IS NULL;
    "#,
    // v3: the agent supervisor's read position in the log (T10).
    r#"
    -- Seq of the last event the agent supervisor has handled; one row. A
    -- database that predates the supervisor starts at its current head:
    -- nothing logged before had an agent to deliver to.
    CREATE TABLE supervisor_cursor (
        id  INTEGER PRIMARY KEY CHECK (id = 1),
        seq INTEGER NOT NULL CHECK (seq >= 0)
    ) STRICT;
    INSERT INTO supervisor_cursor (id, seq) SELECT 1, COALESCE(MAX(seq), 0) FROM events;
    "#,
    // v4: what `ResolveApproval.rule` is checked against (protocol v4).
    r#"
    -- How many rule suggestions the request offers; an answer's rule must
    -- index one. Requests logged before v4 offered none.
    ALTER TABLE approvals ADD COLUMN
        rules INTEGER NOT NULL DEFAULT 0 CHECK (rules >= 0);
    "#,
    // v5: `Event::command_id` (protocol v6). Events logged before carry
    // none.
    r#"
    -- The command that logged the event; NULL for agent and daemon events.
    ALTER TABLE events ADD COLUMN command_id TEXT;
    "#,
    // v6: what the supervisor gives a starting agent (protocol v7).
    r#"
    -- The session's permission mode and model as the log has them, folded
    -- with `AgentSettings::apply`; NULL until the first such event.
    ALTER TABLE sessions ADD COLUMN
        agent_settings TEXT CHECK (agent_settings IS NULL OR json_valid(agent_settings));
    "#,
    // v7: session contexts (protocol v9). A session holds an ordered list
    // of contexts, each with its own resume handle; every event and
    // approval names the context it belongs to. A session's first context
    // has the session's own id, so existing data moves into one context per
    // session. SQLite adds a NOT NULL column only with a constant default:
    // the '' defaults stay in the schema, so every INSERT must name these
    // columns.
    r#"
    -- The contexts of every session, in the order they entered the log
    -- (rowid). Names live in the log only: nothing on the server reads them.
    CREATE TABLE contexts (
        id          TEXT PRIMARY KEY,
        session_id  TEXT NOT NULL REFERENCES sessions (id),
        -- The agent's opaque resume handle for this context; NULL until the
        -- agent reports one.
        agent_state TEXT CHECK (agent_state IS NULL OR json_valid(agent_state))
    ) STRICT;
    CREATE INDEX contexts_session ON contexts (session_id);
    INSERT INTO contexts (id, session_id, agent_state)
        SELECT id, id, agent_state FROM sessions ORDER BY rowid;

    -- The context the session runs in now: set by SessionCreated,
    -- ContextStarted and ContextSwitched.
    ALTER TABLE sessions ADD COLUMN current_context TEXT NOT NULL DEFAULT '';
    UPDATE sessions SET current_context = id;
    -- The context of the turn turn_active describes: set with it by
    -- SessionStateChanged(AgentWorking), cleared with it.
    ALTER TABLE sessions ADD COLUMN turn_context TEXT;
    UPDATE sessions SET turn_context = id WHERE turn_active = 1;
    -- Opaque grants the agent keeps for the whole session, across contexts.
    ALTER TABLE sessions ADD COLUMN
        agent_grants TEXT CHECK (agent_grants IS NULL OR json_valid(agent_grants));
    ALTER TABLE sessions DROP COLUMN agent_state;

    -- The context the event belongs to (Event::context).
    ALTER TABLE events ADD COLUMN context TEXT NOT NULL DEFAULT '';
    UPDATE events SET context = session_id;
    UPDATE events SET kind = json_set(kind, '$.context_name', 'start')
        WHERE json_extract(kind, '$.type') = 'session_created';

    -- The context the approval was requested in; its answer is stamped so.
    ALTER TABLE approvals ADD COLUMN context TEXT NOT NULL DEFAULT '';
    UPDATE approvals SET context = session_id;
    "#,
    // v8: moving a context into a new session (protocol v10). Both columns
    // are NULL for every context that exists before it.
    r#"
    -- The context this one continues (SessionCreated.from): its conversation
    -- is forked on the context's first start, until it has a handle of its own.
    ALTER TABLE contexts ADD COLUMN origin TEXT REFERENCES contexts (id);
    -- The session a ContextMoved sent the context to; set, it is never current
    -- or started again.
    ALTER TABLE contexts ADD COLUMN moved_to TEXT REFERENCES sessions (id);
    "#,
    // v9: the agent's background work (protocol v11).
    r#"
    -- The session's agent's background tasks and schedules as the log has
    -- them, folded with `AgentWork::apply`; NULL until the first such event.
    ALTER TABLE sessions ADD COLUMN
        agent_work TEXT CHECK (agent_work IS NULL OR json_valid(agent_work));
    "#,
    // v10: adopting an outside agent conversation (protocol v12). A session's
    // first context continues either a context moved here or an outside
    // conversation, so v8's `origin` (a context id) becomes one JSON
    // SessionOrigin. SQLite can't drop or retype a column a foreign key
    // uses, so `contexts` is rebuilt (create, copy, drop, rename): rowids
    // are copied so the log order holds, and the index is made again. With
    // foreign keys on, DROP TABLE deletes every row first; only the old
    // table's own `origin` refers to it, so that delete breaks nothing.
    r#"
    CREATE TABLE contexts_v10 (
        id          TEXT PRIMARY KEY,
        session_id  TEXT NOT NULL REFERENCES sessions (id),
        -- The agent's opaque resume handle for this context; NULL until the
        -- agent reports one.
        agent_state TEXT CHECK (agent_state IS NULL OR json_valid(agent_state)),
        -- What this context continues (SessionCreated.from, a SessionOrigin
        -- as JSON): a context moved here, or the agent's own id of an outside
        -- conversation (not a ResumeHandle: only the adapter knows a handle's
        -- shape, D3). Its conversation is forked on the context's first
        -- start, until the context has a handle of its own. NULL for every
        -- other context. JSON, so no foreign key: the context a move continues
        -- exists because command validation moves only an existing context.
        origin      TEXT CHECK (origin IS NULL OR json_valid(origin)),
        -- The session a ContextMoved sent the context to; set, it is never
        -- current or started again.
        moved_to    TEXT REFERENCES sessions (id)
    ) STRICT;
    INSERT INTO contexts_v10 (rowid, id, session_id, agent_state, origin, moved_to)
        SELECT c.rowid, c.id, c.session_id, c.agent_state,
               CASE WHEN c.origin IS NULL THEN NULL ELSE json_object(
                   'kind', 'context',
                   'session', (SELECT o.session_id FROM contexts o WHERE o.id = c.origin),
                   'context', c.origin)
               END,
               c.moved_to
        FROM contexts c ORDER BY c.rowid;
    DROP TABLE contexts;
    ALTER TABLE contexts_v10 RENAME TO contexts;
    CREATE INDEX contexts_session ON contexts (session_id);
    "#,
    // v11: shell commands (protocol v14). Additive: no run exists before it.
    r#"
    -- One row per accepted RunShell, keyed by its command id (the run id),
    -- folded from the five shell events in their append. `when` and `end`
    -- are SQL keywords, hence `run_when` and `run_end`.
    CREATE TABLE shell_runs (
        command_id   TEXT PRIMARY KEY,
        session_id   TEXT NOT NULL REFERENCES sessions (id),
        -- Seq of the run's ShellRequested.
        seq          INTEGER NOT NULL,
        run_when     TEXT NOT NULL
                     CHECK (run_when IN ('always', 'on_success', 'on_failure')), -- RunWhen
        -- Seq of the first ShellStopRequested naming the run.
        stop_seq     INTEGER,
        -- Seq of its ShellStarted; NULL when it never started.
        started_seq  INTEGER,
        -- Seq of its ShellFinished; set, the run is over and logs nothing more.
        finished_seq INTEGER,
        run_end      TEXT CHECK (run_end IS NULL OR json_valid(run_end)), -- ShellEnd
        -- ShellFinished.cwd: where the shell ended, when known.
        cwd          TEXT
    ) STRICT;
    CREATE INDEX shell_runs_session ON shell_runs (session_id, finished_seq);
    "#,
    // v12: workdir reads (protocol v15, D36). Additive.
    r#"
    -- Workdir content stored as a blob, by content: a snapshot of unchanged
    -- content reuses its blob. Only workdir reads look here, so an event
    -- blob is never shared with a snapshot. sha256 is lowercase hex.
    CREATE TABLE workdir_blobs (
        sha256  TEXT NOT NULL,
        size    INTEGER NOT NULL CHECK (size >= 0),
        blob_id TEXT NOT NULL REFERENCES blobs (id),
        PRIMARY KEY (sha256, size)
    ) STRICT;
    "#,
    // v13: the protocol stamp. The row is written by `migrate`, which knows
    // `PROTOCOL_VERSION`; a database from before it gets this build's.
    r#"
    -- The skywave_proto::PROTOCOL_VERSION whose wire JSON the JSON columns
    -- hold; one row, set when the database is created and never changed.
    CREATE TABLE protocol_stamp (
        id      INTEGER PRIMARY KEY CHECK (id = 1),
        version INTEGER NOT NULL
    ) STRICT;
    "#,
    // v14: workdir blob GC (D36). Additive: workdir blobs registered before
    // it are named by no answer, so the first collection removes them.
    r#"
    -- The blobs the latest answer to each workdir read names, as the core
    -- keeps them: `path` as requested, '' for changes. A workdir blob no row
    -- names is deleted (workdir.rs, "Garbage").
    CREATE TABLE workdir_answer_blobs (
        session_id TEXT NOT NULL,
        kind       TEXT NOT NULL CHECK (kind IN ('changes', 'file')),
        path       TEXT NOT NULL,
        blob_id    TEXT NOT NULL REFERENCES blobs (id),
        PRIMARY KEY (session_id, kind, path, blob_id)
    ) STRICT;
    CREATE INDEX workdir_answer_blobs_by_blob ON workdir_answer_blobs (blob_id);
    "#,
    r#"
    -- The log's identity (skywave_proto::LogId); one row, written by
    -- `migrate` with the migration that creates it and never rewritten: a
    -- new database is a new log, a restored backup keeps its id. Devices
    -- compare it on `hello` (architecture.md, "A log that went backwards").
    CREATE TABLE log_identity (
        id     INTEGER PRIMARY KEY CHECK (id = 1),
        log_id TEXT NOT NULL
    ) STRICT;
    -- Every run of `serve` over this log (skywave_proto::RunId), in the
    -- order they opened it, with the head each started from. Append-only;
    -- a restored backup holds only the runs before it was taken.
    CREATE TABLE log_runs (
        position INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id   TEXT NOT NULL UNIQUE,
        start    INTEGER NOT NULL
    ) STRICT;
    "#,
    // v16: the agent supervisor's progress beside its cursor: the message
    // it last handed over, and the sessions whose events wait for their
    // agent to start or stop. Additive.
    r#"
    -- Seq of the last UserMessage the supervisor gave an agent, stored
    -- before the agent gets it (delivery is at most once).
    ALTER TABLE supervisor_cursor ADD COLUMN
        handed INTEGER NOT NULL DEFAULT 0 CHECK (handed >= 0);
    UPDATE supervisor_cursor SET handed = seq;
    -- A session whose agent was starting or stopping: its events after
    -- `seq`, up to the cursor, are not handled yet.
    CREATE TABLE supervisor_held (
        session_id TEXT PRIMARY KEY,
        seq        INTEGER NOT NULL CHECK (seq >= 0)
    ) STRICT;
    "#,
    // v17: every message handed to an agent whose turn has not started,
    // per session, replacing the one `handed` seq: a restart reports each
    // one no agent took, whichever session it was in.
    r#"
    ALTER TABLE supervisor_cursor DROP COLUMN handed;
    -- The log's last seq when the rows below were stored: the
    -- supervisor's own state events after it are not reflected in them.
    ALTER TABLE supervisor_cursor ADD COLUMN
        accounted INTEGER NOT NULL DEFAULT 0 CHECK (accounted >= 0);
    -- A UserMessage handed to its session's agent (stored before the agent
    -- gets it) whose turn has not started. `settling`: an Errored about to
    -- be logged in its context reports it.
    CREATE TABLE supervisor_owed (
        seq        INTEGER PRIMARY KEY CHECK (seq >= 0),
        session_id TEXT NOT NULL,
        context    TEXT NOT NULL,
        settling   INTEGER NOT NULL CHECK (settling IN (0, 1))
    ) STRICT;
    "#,
    // v18: restart recovery that costs what is open, not the log's length
    // (architecture.md "Event log"). Every lookup a daemon start makes per
    // session reads an index or a projection; none scans the history.
    r#"
    -- A session's newest event of one type, without reading the others.
    -- A query uses it only with the expression written exactly so.
    CREATE INDEX events_session_type ON events (session_id, json_extract(kind, '$.type'), seq);
    -- The event a command logged (`EventLog::command_seq`).
    CREATE INDEX events_command ON events (command_id) WHERE command_id IS NOT NULL;
    -- The shell runner's starting point: the runs not over yet.
    CREATE INDEX shell_runs_unfinished ON shell_runs (seq) WHERE finished_seq IS NULL;

    -- The calls still open in the log: a ToolCall naming a call adds its
    -- row, a ToolResult or CallsUnanswered naming it deletes it, in the
    -- append's transaction. Filled here from the log as `open_calls` read
    -- it before: a call is open unless its session answers it anywhere.
    CREATE TABLE open_calls (
        session_id TEXT NOT NULL,
        call       TEXT NOT NULL,
        -- The context and seq of the ToolCall that opened it.
        context    TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        PRIMARY KEY (session_id, call)
    ) STRICT;
    INSERT OR IGNORE INTO open_calls (session_id, call, context, seq)
        SELECT session_id, json_extract(kind, '$.call'), context, seq FROM events
        WHERE json_extract(kind, '$.type') = 'tool_call'
          AND json_extract(kind, '$.call') IS NOT NULL
        ORDER BY seq;
    DELETE FROM open_calls WHERE (session_id, call) IN (
        SELECT session_id, json_extract(kind, '$.call') FROM events
        WHERE json_extract(kind, '$.type') = 'tool_result'
        UNION
        SELECT events.session_id, calls.value
        FROM events, json_each(events.kind, '$.calls') AS calls
        WHERE json_extract(events.kind, '$.type') = 'calls_unanswered');
    "#,
    // v19: an owed message's turn is marked as about to start, as its
    // `Errored` was: a restart settles a message only by the event marked
    // for it, so a turn the agent started on its own settles none.
    r#"
    -- `state`: `waiting`; `starting`: its turn's AgentWorking is about to
    -- be logged in its context; `settling`: an Errored about to be logged
    -- in its context reports it.
    ALTER TABLE supervisor_owed ADD COLUMN
        state TEXT NOT NULL DEFAULT 'waiting'
        CHECK (state IN ('waiting', 'starting', 'settling'));
    UPDATE supervisor_owed SET state = 'settling' WHERE settling = 1;
    ALTER TABLE supervisor_owed DROP COLUMN settling;
    "#,
    // v20: the storage report (protocol v22). Not backfilled: a database
    // with events is stamped with an older protocol, which `open` refuses.
    r#"
    -- The event referencing each event blob (EventKind::blobs), written in
    -- the append's transaction: what the storage report sums per session.
    -- `holds` is the BlobUse. Workdir blobs have no row.
    CREATE TABLE event_blobs (
        blob_id    TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        holds      TEXT NOT NULL CHECK (holds IN
            ('tool_input', 'tool_body', 'tool_output', 'approval_input', 'shell_output'))
    ) STRICT;
    CREATE INDEX event_blobs_seq ON event_blobs (seq);
    "#,
    // v21: a long message's blob (protocol v24, `BlobUse::Message`). SQLite
    // cannot change a CHECK in place, so the table is made again, its rows
    // copied as they are.
    r#"
    CREATE TABLE event_blobs_v21 (
        blob_id    TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        holds      TEXT NOT NULL CHECK (holds IN
            ('tool_input', 'tool_body', 'tool_output', 'approval_input', 'shell_output',
             'message'))
    ) STRICT;
    INSERT INTO event_blobs_v21 (blob_id, session_id, seq, holds)
        SELECT blob_id, session_id, seq, holds FROM event_blobs;
    DROP TABLE event_blobs;
    ALTER TABLE event_blobs_v21 RENAME TO event_blobs;
    CREATE INDEX event_blobs_seq ON event_blobs (seq);
    "#,
    // v22: what `AnswerQuestion` is checked against (protocol v26).
    // Additive: no question was asked before it.
    r#"
    -- The questions of a QuestionAsked (JSON Vec<Question>); NULL for a
    -- plain approval.
    ALTER TABLE approvals ADD COLUMN
        questions TEXT CHECK (questions IS NULL OR json_valid(questions));
    "#,
    // v23: an image on a user message (protocol v30, `BlobUse::Attachment`).
    // Made again as in v21: SQLite cannot change a CHECK in place.
    r#"
    CREATE TABLE event_blobs_v23 (
        blob_id    TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        holds      TEXT NOT NULL CHECK (holds IN
            ('tool_input', 'tool_body', 'tool_output', 'approval_input', 'shell_output',
             'message', 'attachment'))
    ) STRICT;
    INSERT INTO event_blobs_v23 (blob_id, session_id, seq, holds)
        SELECT blob_id, session_id, seq, holds FROM event_blobs;
    DROP TABLE event_blobs;
    ALTER TABLE event_blobs_v23 RENAME TO event_blobs;
    CREATE INDEX event_blobs_seq ON event_blobs (seq);
    "#,
    // v24: a worktree a create is about to add, until its session is logged or
    // the create is undone; a row left at startup is a crash's orphan. `command`
    // is the creating command, kept for the sweep; `attempt` is one preparation's
    // own token: an undo deletes only the row its own attempt wrote.
    r#"
    CREATE TABLE worktree_intents (
        repo    TEXT NOT NULL,
        branch  TEXT NOT NULL,
        target  TEXT NOT NULL,
        start   TEXT NOT NULL,
        command TEXT NOT NULL,
        attempt TEXT NOT NULL,
        PRIMARY KEY (repo, branch)
    ) STRICT;
    "#,
    // v25: a download answer (protocol v36, `GET /v1/workdir/download`) names its blob too.
    // Made again as in v23: SQLite cannot change a CHECK in place.
    r#"
    CREATE TABLE workdir_answer_blobs_v25 (
        session_id TEXT NOT NULL,
        kind       TEXT NOT NULL CHECK (kind IN ('changes', 'file', 'download')),
        path       TEXT NOT NULL,
        blob_id    TEXT NOT NULL REFERENCES blobs (id),
        PRIMARY KEY (session_id, kind, path, blob_id)
    ) STRICT;
    INSERT INTO workdir_answer_blobs_v25 (session_id, kind, path, blob_id)
        SELECT session_id, kind, path, blob_id FROM workdir_answer_blobs;
    DROP TABLE workdir_answer_blobs;
    ALTER TABLE workdir_answer_blobs_v25 RENAME TO workdir_answer_blobs;
    CREATE INDEX workdir_answer_blobs_by_blob ON workdir_answer_blobs (blob_id);
    "#,
];

/// First schema version with `protocol_stamp`.
const STAMPED_SINCE: u32 = 13;

/// Schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Rewrites the wire JSON the database holds from one `PROTOCOL_VERSION` to
/// the next. It runs in the open's write transaction, after the schema
/// migrations, so it reads the current schema.
type ProtocolStep = fn(&rusqlite::Transaction<'_>) -> Result<()>;

/// Step `i` moves the stored wire JSON from protocol version
/// `MIGRATED_SINCE + i` to the next (D43). Every bump adds one, a no-op
/// saying so if nothing stored changed; the length makes a bump without one
/// fail to compile. Append; never edit a released one.
const PROTOCOL_STEPS: [ProtocolStep; (PROTOCOL_VERSION - MIGRATED_SINCE) as usize] = [
    v23_to_v24, v24_to_v25, v25_to_v26, v26_to_v27, v27_to_v28, v28_to_v29, v29_to_v30, v30_to_v31,
    v31_to_v32, v32_to_v33, v33_to_v34, v34_to_v35, v35_to_v36, v36_to_v37, v37_to_v38, v38_to_v39,
];

/// v23 to v24. What v24 changed of what is stored, and how each is moved:
/// - `Full::Inline` (a small tool output carried in its event): new, so no
///   v23 event has one; v23's `Full::Blob` and `Full::Lost` read unchanged
///   and its small output blobs stay blobs.
/// - `full` on `AgentMessage` and on an imported `UserMessage` or
///   `AgentMessage` (a long message's whole text as a blob): a new
///   optional field, which v23 JSON lacks and reads as `None`, so no event
///   is rewritten; v23's long messages stay whole in their events. Its
///   `BlobUse::Message` came with schema migration v21, which lets
///   `event_blobs.holds` say `message`.
/// - Compressed event batches and blob bodies (D45): transport only. Blobs
///   are stored as they were, uncompressed; nothing stored changes.
fn v23_to_v24(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v24 to v25. What v25 changed of what is stored, and how each is moved:
/// - `context_tokens` and `context_window` on `TurnUsage` events: new
///   optional fields, which v24 JSON lacks and reads as `None`, so no event
///   is rewritten; a v24 turn reports no fill.
/// - `EventKind::SubagentUsage`: new, so no v24 event is one.
fn v24_to_v25(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v25 to v26. What v26 changed of what is stored, and how each is moved:
/// - `EventKind::QuestionAsked` and `QuestionAnswered`: new, so no v25
///   event is one. Their `approvals.questions` came with schema migration
///   v22, NULL for every approval v25 logged: each was a plain one.
fn v25_to_v26(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v26 to v27. v27 added `GET /v1/usage-limits`, whose answer the server
/// reads from the agent when asked and never stores: nothing stored
/// changes.
fn v26_to_v27(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v27 to v28. v28 added `login_expires` to the `hello` answer, which the
/// server reads from the agent's login each time and never stores: nothing
/// stored changes.
fn v27_to_v28(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v28 to v29. v29 added `agent_login_offered` and `agent_logged_out` to
/// the `hello` answer, which the server reads live each time, and the
/// agent-login endpoints, which store nothing (the login's URL and code
/// live in memory only): nothing stored changes.
fn v28_to_v29(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v29 to v30. v30 added image attachments: `attachments` on `UserMessage`
/// and `SendMessage`, which v29 JSON lacks and reads as empty, so no event
/// or stored command result is rewritten; staged uploads are files, not
/// rows. Nothing stored changes.
fn v29_to_v30(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v30 to v31. v31 added the workdir commit log, a read the server answers
/// from git and never stores. Nothing stored changes.
fn v30_to_v31(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v31 to v32. v32 added an optional `worktree` to `CreateSession` and
/// `SessionCreated`; JSON without it reads as `None`. v32 also added
/// `RemoveWorktree` and `WorktreeRemoved`, which v31 data cannot hold, so
/// nothing stored changes.
fn v31_to_v32(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v32 to v33. v33 added the `RejectReason` `ShellRunning` and nothing else;
/// a v32 command result cannot hold it, so nothing stored changes.
fn v32_to_v33(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v33 to v34. v34 added the read-only `GET /v1/worktrees` answer and nothing
/// that is stored, so nothing stored changes.
fn v33_to_v34(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v34 to v35. v35 added `context_compacted` to `TurnUsage`, which v34 JSON
/// lacks and reads as `false`: no v34 turn is marked, so nothing stored
/// changes.
fn v34_to_v35(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v35 to v36. v36 added the route `GET /v1/workdir/download` and nothing
/// stored; staging answers that are not stored either. It also added the
/// event `FileWritten`, which data stored before v36 never holds, so no stored
/// event or command result changes.
fn v35_to_v36(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v36 to v37. v37 made staging resumable: a header on `HEAD` and `PUT`
/// `/v1/attachments/{sha256}` and the answer `OffsetMismatch`, none of
/// which is stored, so no stored event or command result changes.
fn v36_to_v37(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v37 to v38. v38 added the safety check: a command and two events, all
/// new, so no stored event or command result changes.
fn v37_to_v38(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// v38 to v39. v39 added safety check feedback (a command, an event, a reject
/// reason, all new) and an optional `shell` on `ApprovalRequested` and
/// `SafetyCheckApproved`, which v38 JSON lacks and reads as `None`; nothing
/// stored changes.
fn v38_to_v39(_tx: &rusqlite::Transaction<'_>) -> Result<()> {
    Ok(())
}

/// An open, migrated database connection.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if needed) the database at `path` and migrates it to
    /// [`SCHEMA_VERSION`]. Reopening an up-to-date database changes nothing.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "SQLite refused WAL journal mode");
        }
        // FULL, not NORMAL: in WAL mode NORMAL may lose the last commits on
        // power loss, and the log is the only record of what users did. A
        // device that pulled them would then see the log go backwards
        // (`LogRun`). The cost is an fsync of the WAL per commit.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Db { conn };
        db.migrate(path)?;
        Ok(db)
    }

    /// The underlying connection, for queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The underlying connection, for transactions.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Runs SQLite's `PRAGMA quick_check` over the whole file: fails with
    /// [`Error::DamagedDatabase`], listing the first problems it found, if
    /// the file is damaged. Reads every page, so `serve` runs it once at
    /// startup, before it serves anything from the file.
    pub fn quick_check(&self, path: &Path) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA quick_check(10)")?;
        let found = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if found == ["ok"] {
            return Ok(());
        }
        Err(Error::DamagedDatabase {
            path: path.to_owned(),
            problems: found.join("; "),
        })
    }

    /// Current `user_version` of the database.
    pub fn schema_version(&self) -> Result<u32> {
        user_version(&self.conn)
    }

    /// Every row that breaks the context stamp invariant, described; empty
    /// when it holds. The invariant: every event, approval and session
    /// names a context (never the `''` the v7 defaults leave behind an
    /// `INSERT` that forgot the column), and that context is a `contexts`
    /// row of the same session. Test support: the daemon's test fixtures
    /// check it after every test.
    #[doc(hidden)]
    pub fn context_stamp_violations(&self) -> Result<Vec<String>> {
        // Each query yields (table, row key, context) for a bad row.
        const CHECKS: [&str; 4] = [
            "SELECT 'events', CAST(seq AS TEXT), context FROM events e WHERE NOT EXISTS
               (SELECT 1 FROM contexts c WHERE c.id = e.context AND c.session_id = e.session_id)",
            "SELECT 'approvals', id, context FROM approvals a WHERE NOT EXISTS
               (SELECT 1 FROM contexts c WHERE c.id = a.context AND c.session_id = a.session_id)",
            "SELECT 'sessions.current_context', id, current_context FROM sessions s
               WHERE NOT EXISTS (SELECT 1 FROM contexts c
                 WHERE c.id = s.current_context AND c.session_id = s.id)",
            "SELECT 'sessions.turn_context', id, turn_context FROM sessions s
               WHERE turn_context IS NOT NULL AND NOT EXISTS (SELECT 1 FROM contexts c
                 WHERE c.id = s.turn_context AND c.session_id = s.id)",
        ];
        let mut found = Vec::new();
        for sql in CHECKS {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(format!(
                    "{} {}: context {:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?
                ))
            })?;
            for row in rows {
                found.push(row?);
            }
        }
        Ok(found)
    }

    /// The log this database holds (`log_identity`), minted when it was
    /// created.
    pub fn log_id(&self) -> Result<LogId> {
        let text: String = self
            .conn
            .query_row("SELECT log_id FROM log_identity WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::Corrupt("log_identity has no row".into()))?;
        Uuid::parse_str(&text)
            .map(LogId)
            .map_err(|e| Error::Corrupt(format!("log id {text:?}: {e}")))
    }

    /// The protocol version the database's wire JSON is in
    /// (`protocol_stamp`); [`Db::open`] migrates it to this build's.
    pub fn protocol_version(&self) -> Result<u32> {
        stamped_protocol(&self.conn)?
            .ok_or_else(|| Error::Corrupt("protocol_stamp has no row".into()))
    }

    /// Migrates the schema, then the stored wire JSON, checking the protocol
    /// stamp before anything reads a JSON column: a database this build
    /// cannot migrate is refused untouched, as its wire JSON may not parse.
    fn migrate(&mut self, path: &Path) -> Result<()> {
        // Fast path without a write lock, so admin commands don't contend
        // with a running `serve` once the database is current.
        let found = user_version(&self.conn)?;
        check_supported(found)?;
        if found == SCHEMA_VERSION && check_protocol(&self.conn, path)? == PROTOCOL_VERSION {
            return Ok(());
        }
        // IMMEDIATE takes the write lock up front; re-read the version under
        // it in case another process migrated in between.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = user_version(&tx)?;
        check_supported(found)?;
        let stamp = if found >= STAMPED_SINCE {
            Some(check_protocol(&tx, path)?)
        } else {
            None
        };
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            tracing::info!(from = i, to = i + 1, "migrating database schema");
            tx.execute_batch(sql)?;
        }
        if let Some(stamp) = stamp {
            migrate_protocol(&tx, stamp, &PROTOCOL_STEPS)?;
        }
        // A fresh database, or one from before the stamp, gets this build's.
        tx.execute(
            "INSERT OR IGNORE INTO protocol_stamp (id, version) VALUES (1, ?1)",
            [PROTOCOL_VERSION],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO log_identity (id, log_id) VALUES (1, ?1)",
            [LogId::new().0.hyphenated().to_string()],
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }
}

fn user_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

fn stamped_protocol(conn: &Connection) -> Result<Option<u32>> {
    Ok(conn
        .query_row("SELECT version FROM protocol_stamp WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// The database's protocol stamp, refusing one this build cannot migrate:
/// older than [`MIGRATED_SINCE`], or written by a newer build.
fn check_protocol(conn: &Connection, path: &Path) -> Result<u32> {
    match stamped_protocol(conn)? {
        Some(found) if (MIGRATED_SINCE..=PROTOCOL_VERSION).contains(&found) => Ok(found),
        Some(found) => Err(Error::ProtocolMismatch {
            path: path.to_owned(),
            found,
            current: PROTOCOL_VERSION,
        }),
        None => Err(Error::Corrupt(format!(
            "{}: protocol_stamp has no row",
            path.display()
        ))),
    }
}

/// Runs the `steps` from protocol version `stamp` on and restamps the
/// database with the version they end at.
fn migrate_protocol(
    tx: &rusqlite::Transaction<'_>,
    stamp: u32,
    steps: &[ProtocolStep],
) -> Result<()> {
    let first = (stamp - MIGRATED_SINCE) as usize;
    for (i, step) in steps.iter().enumerate().skip(first) {
        let from = MIGRATED_SINCE + i as u32;
        tracing::info!(from, to = from + 1, "migrating stored protocol data");
        step(tx)?;
    }
    tx.execute(
        "UPDATE protocol_stamp SET version = ?1 WHERE id = 1",
        [MIGRATED_SINCE + steps.len() as u32],
    )?;
    Ok(())
}

fn check_supported(found: u32) -> Result<()> {
    if found > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use skywave_proto::{ContextRef, SessionOrigin};

    use super::*;
    use crate::event_log::parse_id;

    const TABLES: [&str; 16] = [
        "approvals",
        "blobs",
        "commands",
        "contexts",
        "devices",
        "events",
        "open_calls",
        "pair_codes",
        "protocol_stamp",
        "sessions",
        "shell_runs",
        "supervisor_cursor",
        "supervisor_held",
        "supervisor_owed",
        "workdir_blobs",
        "worktree_intents",
    ];

    fn schema(db: &Db) -> Vec<(String, String)> {
        let mut stmt = db
            .conn()
            .prepare("SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_gets_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let names: Vec<String> = schema(&db).into_iter().map(|(n, _)| n).collect();
        for table in TABLES {
            assert!(names.iter().any(|n| n == table), "missing table {table}");
        }
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn reopen_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let db = Db::open(&path).unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (seq, session_id, at, kind) VALUES (1, 's', 't', '{}')",
                [],
            )
            .unwrap();
        let before = schema(&db);
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(schema(&db), before);
        let count: u32 = db
            .conn()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn quick_check_passes_a_sound_file_and_fails_a_damaged_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let db = Db::open(&path).unwrap();
        db.quick_check(&path).unwrap();
        // Closing the last connection checkpoints the WAL into the file.
        drop(db);

        // The last page is a b-tree page; an invalid page type byte (the
        // first of its header) damages it.
        let mut bytes = std::fs::read(&path).unwrap();
        let page_size = usize::from(u16::from_be_bytes([bytes[16], bytes[17]]));
        let last_page = bytes.len() - page_size;
        bytes[last_page] = 0xff;
        std::fs::write(&path, bytes).unwrap();

        let db = Db {
            conn: Connection::open(&path).unwrap(),
        };
        match db.quick_check(&path) {
            Err(Error::DamagedDatabase { problems, .. }) => assert!(!problems.is_empty()),
            other => panic!("damage not found: {other:?}"),
        }
    }

    #[test]
    fn concurrent_connections_share_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let serve = Db::open(&path).unwrap();
        let admin = Db::open(&path).unwrap();
        admin
            .conn()
            .execute(
                "INSERT INTO pair_codes (hash, expires_at) VALUES ('h', 't')",
                [],
            )
            .unwrap();
        let used: i64 = serve
            .conn()
            .query_row("SELECT used FROM pair_codes WHERE hash = 'h'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(used, 0);
    }

    #[test]
    fn rejects_newer_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            Db::open(&path),
            Err(Error::SchemaTooNew { found, .. }) if found == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn fresh_database_records_the_protocol_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        drop(db);

        // The same version reopens, keeping its stamp and its data.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let rows: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM protocol_stamp", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// Stamps the database at `path` with protocol `version`.
    fn restamp(path: &Path, version: u32) {
        Connection::open(path)
            .unwrap()
            .execute("UPDATE protocol_stamp SET version = ?1", [version])
            .unwrap();
    }

    #[test]
    fn rejects_a_database_of_a_protocol_version_it_cannot_migrate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        for other in [MIGRATED_SINCE - 1, PROTOCOL_VERSION + 1] {
            restamp(&path, other);
            let err = Db::open(&path).unwrap_err();
            assert!(
                matches!(
                    &err,
                    Error::ProtocolMismatch { path: p, found, current }
                        if *p == path && *found == other && *current == PROTOCOL_VERSION
                ),
                "{err:?}"
            );
            let message = err.to_string();
            assert!(message.contains("delete its data directory"), "{message}");
            assert!(
                message.contains(&format!("protocol version {other}")),
                "{message}"
            );
        }
        // Refused untouched: the stamp is still the other version.
        let stamp: u32 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT version FROM protocol_stamp", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stamp, PROTOCOL_VERSION + 1);
        // Positive control: restamped with this build's version, it opens.
        restamp(&path, PROTOCOL_VERSION);
        assert!(Db::open(&path).is_ok());
    }

    /// A database stamped with any version it migrates opens restamped,
    /// through the fast path's check too (the schema is current).
    #[test]
    fn opens_a_database_of_every_migrated_protocol_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        for older in MIGRATED_SINCE..PROTOCOL_VERSION {
            restamp(&path, older);
            let db = Db::open(&path).unwrap();
            assert_eq!(
                db.protocol_version().unwrap(),
                PROTOCOL_VERSION,
                "from {older}"
            );
        }
    }

    /// The tool results v23 wrote (its wire snapshot's) read unchanged once
    /// v24's step ran: a whole output stays the blob or loss it was.
    #[test]
    fn v24_reads_the_tool_results_v23_wrote() {
        use skywave_proto::{EventKind, Full, ToolResult};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        let v23 = [
            r#"{"type":"tool_result","call":"01920000-0000-7000-8000-000000000007","is_error":false,"status":"exit 0","preview":"running","lines":40,"output":{"blob":{"id":"01920000-0000-7000-8000-000000000005","size":48213,"sha256":"7b24cf3d897fd680e0258c1c7c23db50a5428581ed1785c08de505c381b4c4b5"}}}"#,
            r#"{"type":"tool_result","call":null,"is_error":false,"status":null,"preview":"running","lines":40,"output":"lost"}"#,
            r#"{"type":"tool_result","call":null,"is_error":true,"status":"exit 1","preview":"no such file","lines":1,"output":null}"#,
        ];
        {
            let conn = Connection::open(&path).unwrap();
            for (seq, kind) in (1i64..).zip(v23) {
                conn.execute(
                    "INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (?1, 's', 't', 'c', ?2)",
                    rusqlite::params![seq, kind],
                )
                .unwrap();
            }
        }
        restamp(&path, 23);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kinds: Vec<EventKind> = db
            .conn()
            .prepare("SELECT kind FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|kind| serde_json::from_str(&kind.unwrap()).unwrap())
            .collect();
        let outputs: Vec<Option<Full>> = kinds
            .into_iter()
            .map(|kind| match kind {
                EventKind::ToolResult(ToolResult { output, .. }) => output,
                other => panic!("not a result: {other:?}"),
            })
            .collect();
        assert!(matches!(outputs[0], Some(Full::Blob(ref b)) if b.size == 48213));
        assert_eq!(outputs[1..], [Some(Full::Lost), None]);
    }

    /// The messages a v23 build wrote (its wire snapshot's) read as whole
    /// messages once v24's step ran, and the v20 schema it ran on keeps its
    /// blob rows while v21 lets a message's blob have one.
    #[test]
    fn v24_reads_the_messages_v23_wrote() {
        use skywave_proto::{EventKind, ImportedItem};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v23 = [
            r#"{"type":"agent_message","text":"Running cargo test.","parent":null}"#,
            r#"{"type":"agent_message","text":"Found three call sites.","parent":"01920000-0000-7000-8000-000000000007"}"#,
            r#"{"type":"imported","at":null,"item":{"type":"user_message","text":"Where is the event log?"}}"#,
            r#"{"type":"imported","at":null,"item":{"type":"agent_message","text":"In crates/skywaved/src/event_log.rs."}}"#,
        ];
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..20] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 20).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 23)",
                [],
            )
            .unwrap();
            for (seq, kind) in (1i64..).zip(v23) {
                conn.execute(
                    "INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (?1, 's', 't', 'c', ?2)",
                    rusqlite::params![seq, kind],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                 VALUES ('b1', 's', 1, 'tool_output')",
                [],
            )
            .unwrap();
            // v20 refuses a message's blob.
            assert!(
                conn.execute(
                    "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                     VALUES ('b2', 's', 1, 'message')",
                    [],
                )
                .is_err()
            );
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let kinds: Vec<EventKind> = db
            .conn()
            .prepare("SELECT kind FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|kind| serde_json::from_str(&kind.unwrap()).unwrap())
            .collect();
        let texts: Vec<(&str, bool)> = kinds
            .iter()
            .map(|kind| match kind {
                EventKind::AgentMessage { text, full, .. }
                | EventKind::Imported {
                    item:
                        ImportedItem::UserMessage { text, full }
                        | ImportedItem::AgentMessage { text, full },
                    ..
                } => (text.as_str(), full.is_none()),
                other => panic!("not a message: {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            [
                ("Running cargo test.", true),
                ("Found three call sites.", true),
                ("Where is the event log?", true),
                ("In crates/skywaved/src/event_log.rs.", true),
            ]
        );
        let holds: Vec<(String, String)> = db
            .conn()
            .prepare("SELECT blob_id, holds FROM event_blobs ORDER BY blob_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(holds, [("b1".to_owned(), "tool_output".to_owned())]);
        db.conn()
            .execute(
                "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                 VALUES ('b2', 's', 1, 'message')",
                [],
            )
            .unwrap();
    }

    /// v23 lets an image on a user message have a blob row: the table made
    /// again keeps v22's rows and its `event_blobs_seq` index.
    #[test]
    fn v23_schema_lets_an_attachment_blob_in() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..22] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 22).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, ?1)",
                [PROTOCOL_VERSION],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                 VALUES ('m1', 's', 1, 'message')",
                [],
            )
            .unwrap();
            // v22 refuses an image's blob.
            assert!(
                conn.execute(
                    "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                     VALUES ('a1', 's', 1, 'attachment')",
                    [],
                )
                .is_err()
            );
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let rows: Vec<(String, String, i64, String)> = db
            .conn()
            .prepare("SELECT blob_id, session_id, seq, holds FROM event_blobs ORDER BY blob_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            [("m1".to_owned(), "s".to_owned(), 1, "message".to_owned())]
        );
        let index: String = db
            .conn()
            .query_row(
                "SELECT name FROM sqlite_schema WHERE type = 'index' AND name = 'event_blobs_seq'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, "event_blobs_seq");
        db.conn()
            .execute(
                "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                 VALUES ('a1', 's', 1, 'attachment')",
                [],
            )
            .unwrap();
    }

    /// v24 adds an empty `worktree_intents`, one row per repository and
    /// branch: a row with no `start`, `command` or `attempt` is refused, a second row
    /// for the same pair is refused, and the same branch in another repository
    /// is its own.
    #[test]
    fn v24_adds_an_empty_worktree_intents_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..23] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 23).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, ?1)",
                [PROTOCOL_VERSION],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM worktree_intents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // Each of `start`, `command` and `attempt` is required.
        for missing in [
            "INSERT INTO worktree_intents (repo, branch, target, command, attempt)
             VALUES ('r', 'b', 't', 'c', 'a')",
            "INSERT INTO worktree_intents (repo, branch, target, start, attempt)
             VALUES ('r', 'b', 't', 's', 'a')",
            "INSERT INTO worktree_intents (repo, branch, target, start, command)
             VALUES ('r', 'b', 't', 's', 'c')",
        ] {
            assert!(conn.execute(missing, []).is_err(), "{missing}");
        }
        let insert = |repo: &str, branch: &str| {
            conn.execute(
                "INSERT INTO worktree_intents (repo, branch, target, start, command, attempt)
                 VALUES (?1, ?2, 't', 's', 'c', 'a')",
                rusqlite::params![repo, branch],
            )
        };
        // Positive control, then the same repository and branch again.
        assert!(insert("r", "b").is_ok());
        assert!(insert("r", "b").is_err());
        assert!(insert("r2", "b").is_ok());
    }

    /// v25 lets a download answer name its blob: the table made again keeps
    /// v24's `file` row and its `workdir_answer_blobs_by_blob` index.
    #[test]
    fn v25_schema_lets_a_download_answer_in() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..24] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 24).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, ?1)",
                [PROTOCOL_VERSION],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO blobs (id, size, sha256) VALUES ('b1', 1, 'h')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO workdir_answer_blobs (session_id, kind, path, blob_id)
                 VALUES ('s', 'file', 'a.txt', 'b1')",
                [],
            )
            .unwrap();
            // v24 refuses a download's answer.
            assert!(
                conn.execute(
                    "INSERT INTO workdir_answer_blobs (session_id, kind, path, blob_id)
                     VALUES ('s', 'download', 'a.txt', 'b1')",
                    [],
                )
                .is_err()
            );
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let rows: Vec<(String, String, String, String)> = db
            .conn()
            .prepare("SELECT session_id, kind, path, blob_id FROM workdir_answer_blobs")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            [(
                "s".to_owned(),
                "file".to_owned(),
                "a.txt".to_owned(),
                "b1".to_owned()
            )]
        );
        let index: String = db
            .conn()
            .query_row(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'index' AND name = 'workdir_answer_blobs_by_blob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, "workdir_answer_blobs_by_blob");
        db.conn()
            .execute(
                "INSERT INTO workdir_answer_blobs (session_id, kind, path, blob_id)
                 VALUES ('s', 'download', 'a.txt', 'b1')",
                [],
            )
            .unwrap();
    }

    /// The turn usage v24 wrote (its wire snapshot's) reads once v25's
    /// step ran, with no context fill: a v24 turn reported none.
    #[test]
    fn v25_reads_the_turn_usage_v24_wrote() {
        use skywave_proto::{EventKind, TurnUsage};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        let v24 = [
            r#"{"type":"turn_usage","duration_ms":12400,"tokens_in":3100,"tokens_out":812,"cost_usd":0.04,"model":"model-a"}"#,
            r#"{"type":"turn_usage","duration_ms":2590,"tokens_in":null,"tokens_out":null,"cost_usd":null,"model":null}"#,
        ];
        {
            let conn = Connection::open(&path).unwrap();
            for (seq, kind) in (1i64..).zip(v24) {
                conn.execute(
                    "INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (?1, 's', 't', 'c', ?2)",
                    rusqlite::params![seq, kind],
                )
                .unwrap();
            }
        }
        restamp(&path, 24);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let usages: Vec<TurnUsage> = db
            .conn()
            .prepare("SELECT kind FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|kind| match serde_json::from_str(&kind.unwrap()).unwrap() {
                EventKind::TurnUsage(usage) => usage,
                other => panic!("not a turn's usage: {other:?}"),
            })
            .collect();
        assert_eq!(
            usages,
            [
                TurnUsage {
                    duration_ms: Some(12_400),
                    tokens_in: Some(3_100),
                    tokens_out: Some(812),
                    cost_usd: Some(0.04),
                    model: Some("model-a".into()),
                    context_tokens: None,
                    context_window: None,
                    context_compacted: false,
                },
                TurnUsage {
                    duration_ms: Some(2_590),
                    ..TurnUsage::default()
                },
            ]
        );
    }

    /// An approval a v25 build logged (its wire snapshot's request), on the
    /// v21 schema it ran on, reads as a plain approval once v26's step ran:
    /// v22 gives its row no questions.
    #[test]
    fn v26_reads_the_approval_v25_logged_as_plain() {
        use skywave_proto::{ApprovalId, EventKind};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let id = "01920000-0000-7000-8000-000000000004";
        let v25 = format!(
            r#"{{"type":"approval_requested","id":"{id}","call":null,"tool":"exec","summary":"rm -rf target","input":null,"suggestions":[]}}"#
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..21] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 21).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 25)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [&v25],
            )
            .unwrap();
            // No sessions row: only the approval's own is read.
            conn.pragma_update(None, "foreign_keys", false).unwrap();
            conn.execute(
                "INSERT INTO approvals (id, session_id, context, rules) VALUES (?1, ?2, ?2, 0)",
                [id, "01920000-0000-7000-8000-000000000001"],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::ApprovalRequested { .. }
        ));
        let id = ApprovalId(id.parse().unwrap());
        let row = crate::event_log::approval_in(db.conn(), id)
            .unwrap()
            .unwrap();
        assert_eq!(row.questions, None);
        assert_eq!(row.closed, None);
    }

    /// A database a v26 build wrote reads unchanged once v27's step ran.
    #[test]
    fn v27_reads_what_v26_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v26 = r#"{"type":"agent_message","text":"Done.","parent":null,"full":null}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 26)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v26],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v26);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::AgentMessage { .. }
        ));
    }

    /// A database a v27 build wrote reads unchanged once v28's step ran.
    #[test]
    fn v28_reads_what_v27_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v27 = r#"{"type":"turn_usage","duration_ms":1200,"tokens_in":10,"tokens_out":5,"cost_usd":null,"model":null,"context_tokens":null,"context_window":null}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 27)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v27],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v27);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::TurnUsage(_)
        ));
    }

    /// A database a v28 build wrote reads unchanged once v29's step ran.
    #[test]
    fn v29_reads_what_v28_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v28 =
            r#"{"type":"agent_message","text":"Please run /login.","parent":null,"full":null}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 28)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v28],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v28);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::AgentMessage { .. }
        ));
    }

    /// A database a v29 build wrote reads unchanged once v30's step ran.
    #[test]
    fn v30_reads_what_v29_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v29 = r#"{"type":"user_message","device_id":"0192f0a0-0000-7000-8000-000000000003","text":"look"}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 29)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v29],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v29);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::UserMessage { attachments, .. } if attachments.is_empty()
        ));
    }

    /// A database a v30 build wrote reads unchanged once v31's step ran.
    #[test]
    fn v31_reads_what_v30_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v30 = r#"{"type":"user_message","device_id":"0192f0a0-0000-7000-8000-000000000003","text":"look"}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 30)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v30],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v30);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::UserMessage { attachments, .. } if attachments.is_empty()
        ));
    }

    /// A database a v31 build wrote reads unchanged once v32's step ran: its
    /// `session_created` has no `worktree` key, and reads as `None`.
    #[test]
    fn v32_reads_what_v31_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v31 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 31)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v31],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v31);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated { worktree: None, .. }
        ));
    }

    #[test]
    fn v33_reads_what_v32_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v32 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null,"worktree":{"repo":"/srv/code/skywave","branch":"feature"}}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 32)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v32],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v32);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated {
                worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn v34_reads_what_v33_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v33 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null,"worktree":{"repo":"/srv/code/skywave","branch":"feature"}}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 33)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v33],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v33);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated {
                worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn v35_reads_what_v34_wrote() {
        use skywave_proto::{EventKind, TurnUsage};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v34 = r#"{"type":"turn_usage","duration_ms":12400,"tokens_in":3100,"tokens_out":812,"cost_usd":0.04,"model":"model-a","context_tokens":19337,"context_window":1000000}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 34)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v34],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v34);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::TurnUsage(TurnUsage {
                duration_ms: Some(12_400),
                context_compacted: false,
                ..
            })
        ));
    }

    #[test]
    fn v36_reads_what_v35_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v35 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null,"worktree":{"repo":"/srv/code/skywave","branch":"feature"}}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 35)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v35],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v35);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated {
                worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn v37_reads_what_v36_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v36 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null,"worktree":{"repo":"/srv/code/skywave","branch":"feature"}}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 36)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v36],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v36);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated {
                worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn v38_reads_what_v37_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v37 = r#"{"type":"session_created","workdir":"/srv/code/skywave","context_name":"velvet walrus","from":null,"worktree":{"repo":"/srv/code/skywave","branch":"feature"}}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 37)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v37],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v37);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::SessionCreated {
                worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn v39_reads_what_v38_wrote() {
        use skywave_proto::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let v38 = r#"{"type":"approval_requested","id":"01920000-0000-7000-8000-000000000004","call":"01920000-0000-7000-8000-000000000007","tool":"Bash","summary":"ls","input":null,"suggestions":[]}"#;
        {
            let conn = Connection::open(&path).unwrap();
            for sql in MIGRATIONS {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, 38)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (seq, session_id, at, context, kind)
                 VALUES (1, 's', 't', 'c', ?1)",
                [v38],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, v38);
        assert!(matches!(
            serde_json::from_str(&kind).unwrap(),
            EventKind::ApprovalRequested { shell: None, .. }
        ));
    }

    /// Steps for [`protocol_steps_run_from_the_stamp_on`]: each records
    /// that it ran.
    const TEST_STEPS: [ProtocolStep; 3] = [
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (0)", []).map(drop)?),
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (1)", []).map(drop)?),
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (2)", []).map(drop)?),
    ];

    #[test]
    fn protocol_steps_run_from_the_stamp_on() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        let tx = db.conn_mut().transaction().unwrap();
        tx.execute_batch("CREATE TEMP TABLE ran (step INTEGER)")
            .unwrap();
        migrate_protocol(&tx, MIGRATED_SINCE + 1, &TEST_STEPS).unwrap();
        let ran: Vec<u32> = tx
            .prepare("SELECT step FROM ran ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ran, [1, 2], "from the stamp's step on, in order");
        assert_eq!(stamped_protocol(&tx).unwrap(), Some(MIGRATED_SINCE + 3));
    }

    /// A v12 database, from before the stamp, gets this build's protocol
    /// version: nothing recorded which build wrote it.
    #[test]
    fn v13_stamps_a_database_from_before_the_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..12] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 12).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(db.protocol_version().unwrap(), PROTOCOL_VERSION);
        // One row only.
        assert!(
            db.conn()
                .execute("INSERT INTO protocol_stamp (id, version) VALUES (2, 1)", [])
                .is_err()
        );
    }

    /// A v6 database, from before session contexts, moves each session into
    /// one context with the session's id.
    #[test]
    fn v7_moves_every_session_into_its_first_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, b) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000b",
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..6] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, agent_state, turn_active)
                    VALUES ('{a}', '/w/a', '{{"session":"agent-a"}}', 1),
                           ('{b}', '/w/b', NULL, 0);
                INSERT INTO events (seq, session_id, at, command_id, kind) VALUES
                    (1, '{a}', 't', 'c1', '{{"type":"session_created","workdir":"/w/a"}}'),
                    (2, '{b}', 't', 'c2', '{{"type":"session_created","workdir":"/w/b"}}'),
                    (3, '{a}', 't', NULL, '{{"type":"session_state_changed","state":"agent_working"}}'),
                    (4, '{a}', 't', NULL, '{{"type":"approval_requested","id":"p1","tool":"x","summary":"y","input":null}}');
                INSERT INTO approvals (id, session_id, closed_seq) VALUES ('p1', '{a}', NULL);
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();
        let rows = |sql: &str| -> Vec<Vec<Option<String>>> {
            let mut stmt = conn.prepare(sql).unwrap();
            let n = stmt.column_count();
            stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let s = |v: &str| Some(v.to_owned());

        // One context per session, with its id and the handle moved there.
        assert_eq!(
            rows("SELECT id, session_id, agent_state FROM contexts ORDER BY rowid"),
            [
                vec![s(a), s(a), s(r#"{"session":"agent-a"}"#)],
                vec![s(b), s(b), None],
            ]
        );
        assert_eq!(
            rows("SELECT current_context, turn_context, agent_grants FROM sessions ORDER BY rowid"),
            [vec![s(a), s(a), None], vec![s(b), None, None]]
        );
        let columns = rows("SELECT name FROM pragma_table_info('sessions')");
        assert!(!columns.contains(&vec![s("agent_state")]), "{columns:?}");
        assert!(columns.contains(&vec![s("agent_grants")]), "{columns:?}");

        // Every event and approval is stamped with its session's id.
        assert_eq!(
            rows("SELECT context FROM events ORDER BY seq"),
            [vec![s(a)], vec![s(b)], vec![s(a)], vec![s(a)]]
        );
        assert_eq!(rows("SELECT context FROM approvals"), [vec![s(a)]]);
        // The first contexts of old sessions are named "start"; other kinds
        // are untouched.
        assert_eq!(
            rows("SELECT kind FROM events ORDER BY seq"),
            [
                vec![s(
                    r#"{"type":"session_created","workdir":"/w/a","context_name":"start"}"#
                )],
                vec![s(
                    r#"{"type":"session_created","workdir":"/w/b","context_name":"start"}"#
                )],
                vec![s(
                    r#"{"type":"session_state_changed","state":"agent_working"}"#
                )],
                vec![s(
                    r#"{"type":"approval_requested","id":"p1","tool":"x","summary":"y","input":null}"#
                )],
            ]
        );
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
    }

    /// A v7 database, from before moving contexts, gains `contexts.origin`
    /// and `contexts.moved_to`, NULL for every context it has.
    #[test]
    fn v8_adds_origin_and_moved_to_to_every_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, c) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000c",
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..7] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context) VALUES ('{a}', '/w/a', '{c}');
                INSERT INTO contexts (id, session_id, agent_state)
                    VALUES ('{a}', '{a}', '{{"session":"agent-a"}}'), ('{c}', '{a}', NULL);
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT id, session_id, agent_state, origin, moved_to FROM contexts ORDER BY rowid",
            )
            .unwrap();
        let rows: Vec<[Option<String>; 5]> = stmt
            .query_map([], |r| {
                Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            rows,
            [
                [s(a), s(a), s(r#"{"session":"agent-a"}"#), None, None],
                [s(c), s(a), None, None, None],
            ]
        );
        let current: String = db
            .conn()
            .query_row("SELECT current_context FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(current, c);
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
    }

    /// A v8 database, from before background work, gains
    /// `sessions.agent_work`, NULL for every session it has, and the column
    /// takes JSON only.
    #[test]
    fn v9_adds_agent_work_to_every_session() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let a = "01920000-0000-7000-8000-00000000000a";
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..8] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 8).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context, agent_settings)
                    VALUES ('{a}', '/w/a', '{a}', '{{}}');
                INSERT INTO contexts (id, session_id) VALUES ('{a}', '{a}');
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let row: (String, Option<String>, Option<String>) = db
            .conn()
            .query_row(
                "SELECT workdir, agent_settings, agent_work FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("/w/a".to_owned(), Some("{}".to_owned()), None));
        let set = |json: &str| {
            db.conn()
                .execute("UPDATE sessions SET agent_work = ?1", [json])
        };
        assert!(set(r#"{"tasks":[],"schedules":[]}"#).is_ok());
        assert!(set("not json").is_err());
    }

    /// A v9 database, from before adopting outside conversations, has
    /// `contexts` rebuilt around one JSON `origin`: every row, in its order,
    /// keeps what it had; a moved session's first context's origin becomes
    /// that context as a `SessionOrigin`; the table stays STRICT with its
    /// checks, references and index.
    #[test]
    fn v10_keeps_every_context_and_makes_origin_a_session_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, b, c) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000b",
            "01920000-0000-7000-8000-00000000000c",
        );
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            for sql in &MIGRATIONS[..9] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
            // Inserted out of id order, so rowid order is not id order: c
            // (plain, of a, moved on to b), b (a's handle), a (continues c).
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context)
                    VALUES ('{a}', '/w/a', '{b}'), ('{b}', '/w/a', '{a}');
                INSERT INTO contexts (id, session_id, agent_state, origin, moved_to)
                    VALUES ('{c}', '{a}', NULL, NULL, '{b}'),
                           ('{b}', '{a}', '{{"session":"agent-b"}}', NULL, NULL),
                           ('{a}', '{b}', NULL, '{c}', NULL);
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();

        let mut stmt = conn
            .prepare(
                "SELECT rowid, id, session_id, agent_state, origin, moved_to
                 FROM contexts ORDER BY rowid",
            )
            .unwrap();
        type Row = (
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let s = |v: &str| v.to_owned();
        let handle = Some(s(r#"{"session":"agent-b"}"#));
        let origin = rows[2].4.clone().expect("the moved session's origin");
        assert_eq!(
            rows,
            [
                (1, s(c), s(a), None, None, Some(s(b))),
                (2, s(b), s(a), handle, None, None),
                (3, s(a), s(b), None, Some(origin.clone()), None),
            ]
        );
        // The context it continues, with that context's session.
        let origin: SessionOrigin = serde_json::from_str(&origin).unwrap();
        assert_eq!(
            origin,
            SessionOrigin::Context(ContextRef {
                session: parse_id(a).unwrap(),
                context: parse_id(c).unwrap(),
            })
        );

        // No `adopted` column; the index, STRICT and every check stay.
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('contexts')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            columns,
            ["id", "session_id", "agent_state", "origin", "moved_to"]
        );
        let index: String = conn
            .query_row(
                "SELECT tbl_name || ':' || sql FROM sqlite_master
                 WHERE type = 'index' AND name = 'contexts_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            index,
            "contexts:CREATE INDEX contexts_session ON contexts (session_id)"
        );
        let strict: bool = conn
            .query_row(
                "SELECT strict FROM pragma_table_list('contexts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(strict);
        let insert = |sql: &str| conn.execute(sql, []);
        // A blob, as STRICT turns an integer into TEXT; a non-STRICT
        // table would take the blob.
        assert!(
            insert(&format!(
                "INSERT INTO contexts (id, session_id) VALUES (X'00', '{a}')"
            ))
            .is_err()
        );
        assert!(
            insert(&format!(
                "INSERT INTO contexts (id, session_id, agent_state) VALUES ('d', '{a}', 'not json')"
            ))
            .is_err()
        );
        assert!(
            insert(&format!(
                "INSERT INTO contexts (id, session_id, origin) VALUES ('d', '{a}', 'not json')"
            ))
            .is_err()
        );
        assert!(
            insert(&format!(
                "INSERT INTO contexts (id, session_id, moved_to) VALUES ('d', '{a}', 'nowhere')"
            ))
            .is_err()
        );
        assert!(insert("INSERT INTO contexts (id, session_id) VALUES ('d', 'nowhere')").is_err());
        // Positive control: a row that breaks nothing goes in.
        assert!(
            insert(&format!(
                r#"INSERT INTO contexts (id, session_id, origin, moved_to)
                   VALUES ('d', '{a}', '{{"kind":"agent","conversation":"x"}}', '{b}')"#
            ))
            .is_ok()
        );
        let violations = conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(violations, 0);
    }

    /// A v10 database, from before shell commands, gains an empty
    /// `shell_runs`; what it had stays, and the new table's checks hold.
    #[test]
    fn v11_adds_an_empty_shell_runs_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let a = "01920000-0000-7000-8000-00000000000a";
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..10] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 10).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context) VALUES ('{a}', '/w/a', '{a}');
                INSERT INTO contexts (id, session_id) VALUES ('{a}', '{a}');
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("sessions"), 1);
        assert_eq!(count("shell_runs"), 0);
        let insert = |when: &str, end: &str| {
            conn.execute(
                "INSERT INTO shell_runs (command_id, session_id, seq, run_when, run_end)
                 VALUES ('r', ?1, 1, ?2, ?3)",
                [a, when, end],
            )
        };
        assert!(insert("sometimes", r#"{"kind":"stopped"}"#).is_err());
        assert!(insert("always", "not json").is_err());
        // Positive control: a row that breaks nothing goes in.
        assert!(insert("on_failure", r#"{"kind":"stopped"}"#).is_ok());
    }

    /// A v11 database, from before workdir reads, gains an empty
    /// `workdir_blobs` that takes one row per content and only real blobs.
    #[test]
    fn v12_adds_an_empty_workdir_blobs_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..11] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 11).unwrap();
            conn.execute_batch("INSERT INTO blobs (id, size, sha256) VALUES ('b', 1, 'h')")
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("blobs"), 1);
        assert_eq!(count("workdir_blobs"), 0);
        let insert = |sha: &str, size: i64, blob: &str| {
            conn.execute(
                "INSERT INTO workdir_blobs (sha256, size, blob_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![sha, size, blob],
            )
        };
        assert!(insert("h", 1, "no-such-blob").is_err());
        assert!(insert("h", -1, "b").is_err());
        // Positive control, then the same content again.
        assert!(insert("h", 1, "b").is_ok());
        assert!(insert("h", 1, "b").is_err());
    }

    /// A v17 database gains `open_calls` filled from its log as the old
    /// scan read it: a call answered by a result or named unanswered in its
    /// own session is closed; one answered only in another session, or
    /// never, stays open.
    #[test]
    fn v18_fills_open_calls_from_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..17] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 17).unwrap();
            conn.execute(
                "INSERT INTO protocol_stamp (id, version) VALUES (1, ?1)",
                [PROTOCOL_VERSION],
            )
            .unwrap();
            conn.execute_batch(
                r#"
                INSERT INTO log_identity (id, log_id)
                    VALUES (1, '01920000-0000-7000-8000-000000000001');
                INSERT INTO events (seq, session_id, at, context, kind) VALUES
                    (1, 'a', 't', 'ca', '{"type":"tool_call","call":"open"}'),
                    (2, 'a', 't', 'ca', '{"type":"tool_call","call":"result"}'),
                    (3, 'a', 't', 'ca', '{"type":"tool_call","call":"named"}'),
                    (4, 'a', 't', 'ca', '{"type":"tool_call","call":null}'),
                    (5, 'b', 't', 'cb', '{"type":"tool_call","call":"elsewhere"}'),
                    (6, 'a', 't', 'cx', '{"type":"tool_result","call":"result"}'),
                    (7, 'a', 't', 'ca', '{"type":"calls_unanswered","calls":["named"]}'),
                    (8, 'a', 't', 'ca', '{"type":"tool_result","call":"elsewhere"}'),
                    (9, 'a', 't', 'ca', '{"type":"tool_call","call":"late"}'),
                    (10, 'a', 't', 'ca', '{"type":"tool_result","call":null}');
                "#,
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db
            .conn()
            .prepare("SELECT session_id, call, context, seq FROM open_calls ORDER BY seq")
            .unwrap();
        let rows: Vec<(String, String, String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let row = |s: &str, call: &str, context: &str, seq| {
            (s.to_owned(), call.to_owned(), context.to_owned(), seq)
        };
        assert_eq!(
            rows,
            [
                row("a", "open", "ca", 1),
                row("b", "elsewhere", "cb", 5),
                row("a", "late", "ca", 9),
            ]
        );
    }

    #[test]
    fn the_stamp_check_names_rows_without_a_context_of_their_session() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO sessions (id, workdir, current_context) VALUES ('a', '/', 'a'),
                                                                         ('b', '/', 'b');
                 INSERT INTO contexts (id, session_id) VALUES ('a', 'a'), ('b', 'b');
                 INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (1, 'a', 't', 'a', '{}');",
            )
            .unwrap();
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
        db.conn()
            .execute_batch(
                "INSERT INTO events (seq, session_id, at, kind) VALUES (2, 'a', 't', '{}');
                 INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (3, 'a', 't', 'b', '{}');
                 INSERT INTO approvals (id, session_id) VALUES ('p', 'a');
                 UPDATE sessions SET current_context = 'a', turn_context = 'a' WHERE id = 'b';",
            )
            .unwrap();
        assert_eq!(
            db.context_stamp_violations().unwrap(),
            [
                r#"events 2: context """#,
                r#"events 3: context "b""#,
                r#"approvals p: context """#,
                r#"sessions.current_context b: context "a""#,
                r#"sessions.turn_context b: context "a""#,
            ]
        );
    }

    #[test]
    fn json_columns_reject_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        let err = db.conn().execute(
            "INSERT INTO events (seq, session_id, at, kind) VALUES (1, 's', 't', 'not json')",
            [],
        );
        assert!(err.is_err());
    }
}
