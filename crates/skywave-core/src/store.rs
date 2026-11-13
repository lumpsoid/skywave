//! The local replica: one SQLite database in the core's data directory
//! (architecture.md "Client core", D6).
//!
//! Holds the replicated event log, projected session summaries, the outbox,
//! the pull cursor, blob rows, settings, the server's device names (from
//! `hello`), the latest answer to each workdir read and to the usage limits
//! read, and this device's
//! drafts, read
//! marks, locally deleted sessions and alerted approvals (D5: never
//! synced). Startup always reads everything back from here
//! ([`Store::load`]), so a cold start is the normal start.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use skywave_proto::{
    ApprovalId, BlobId, BlobRef, Command, CommandId, CommandKind, CommandResult, ContentHash,
    ContextId, DeviceId, DeviceInfo, Event, EventBatch, EventKind, HelloResponse, ImageType, LogId,
    LogRun, MIGRATED_SINCE, ModelChoice, PROTOCOL_VERSION, RunId, RunsSince, Seq, SessionId,
    UsageLimitsResult,
};
use uuid::Uuid;

use crate::address::{ServerAddress, ServerAddresses};
use crate::attachment::{self, AttachedImage};
use crate::projection::{BlobFate, SessionSummary, TurnRecord, blob_refs};
use crate::view::{ComposerMode, Draft, RunWhen, Snippet, SnippetId};
use crate::workdir::{WorkdirAnswer, WorkdirAsk};

/// File name of the database inside the core's data directory.
pub const DB_FILE: &str = "core.db";

/// How long a statement waits for SQLite's own lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One schema migration: SQL, or code for what SQL cannot express.
enum Migration {
    Sql(&'static str),
    Code(fn(&Transaction<'_>) -> Result<(), StoreError>),
}
use Migration::{Code, Sql};

/// Schema migrations; entry `i` moves the schema from version `i` to `i + 1`.
/// The schema version is SQLite's `user_version`.
const MIGRATIONS: &[Migration] = &[
    Sql(r#"
CREATE TABLE events (
    seq INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    event TEXT NOT NULL
);
CREATE INDEX events_by_session ON events (session_id, seq);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workdir TEXT,
    state TEXT NOT NULL,
    pending_approvals TEXT NOT NULL,
    last_activity TEXT NOT NULL,
    last_seq INTEGER NOT NULL
);

CREATE TABLE cursor (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    seq INTEGER NOT NULL
);
INSERT INTO cursor (id, seq) VALUES (0, 0);

CREATE TABLE outbox (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    command_id TEXT NOT NULL UNIQUE,
    command TEXT NOT NULL,
    sending INTEGER NOT NULL DEFAULT 0,
    result TEXT
);

CREATE TABLE blobs (
    id TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('not_downloaded', 'downloading', 'available')),
    got INTEGER,
    path TEXT
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#),
    Sql(r#"
-- `SessionSummary::stopped`, backfilled from the stored log: a session is
-- stopped if its newest stop is newer than its newest message.
ALTER TABLE sessions ADD COLUMN stopped INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET stopped = 1 WHERE
    (SELECT MAX(seq) FROM events WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'session_stopped')
    > COALESCE((SELECT MAX(seq) FROM events WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'user_message'), 0);
"#),
    Sql(r#"
-- Delivery bookkeeping: how often a command was claimed for sending, when it
-- first was, and when the server answered. Any row that predates this may
-- already have gone out, so it counts as attempted.
ALTER TABLE outbox ADD COLUMN tries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN first_sent_at TEXT;
ALTER TABLE outbox ADD COLUMN answered_at TEXT;
UPDATE outbox SET tries = 1;
"#),
    Sql(r#"
-- Unsent messages, one per session; `cursor` counts Unicode scalar values.
CREATE TABLE drafts (
    session_id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    cursor INTEGER NOT NULL
);
"#),
    Sql(r#"
-- `SessionSummary::notable`, backfilled from the stored log with the kinds
-- `projection::is_notable` counts: agent messages, approval requests, errors.
-- It ran on rows from before subagent events: none has a `parent`, so every
-- agent message there is the main agent's and counts.
ALTER TABLE sessions ADD COLUMN notable INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET notable = (SELECT COUNT(*) FROM events
    WHERE session_id = sessions.id
        AND (json_extract(event, '$.kind.type') IN ('agent_message', 'approval_requested')
            OR json_extract(event, '$.kind.type') = 'session_state_changed'
                AND json_extract(event, '$.kind.state') = 'errored'));

-- How far this device has read each session (D5: never synced). Everything
-- stored before this counts as read, so an upgrade shows nothing as new.
CREATE TABLE read_marks (
    session_id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL,
    notable INTEGER NOT NULL
);
INSERT INTO read_marks (session_id, seq, notable) SELECT id, last_seq, notable FROM sessions;
"#),
    Sql(r#"
-- `SessionSummary::name` and `first_prompt`. A replica from before this
-- holds no renames, so `name` starts empty; `first_prompt` is backfilled
-- like `projection::first_line` cuts it: the first line of the session's
-- earliest non-blank message, leading whitespace skipped, at most 200 chars.
ALTER TABLE sessions ADD COLUMN name TEXT;
ALTER TABLE sessions ADD COLUMN first_prompt TEXT;
UPDATE sessions SET first_prompt = (
    SELECT substr(rtrim(substr(t, 1, instr(t || char(10), char(10)) - 1), char(32, 9, 13)), 1, 200)
    FROM (SELECT ltrim(json_extract(event, '$.kind.text'), char(32, 9, 10, 13)) AS t, seq
        FROM events
        WHERE session_id = sessions.id
            AND json_extract(event, '$.kind.type') = 'user_message')
    WHERE t != ''
    ORDER BY seq
    LIMIT 1);
"#),
    Sql(r#"
-- `SessionSummary::archived`. A replica from before this holds no archive
-- events, so every session starts active: nothing to backfill.
ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
"#),
    Sql(r#"
-- `SessionSummary::usage` as JSON. A replica from before this holds no
-- turn usage events, so every session starts without: nothing to backfill.
ALTER TABLE sessions ADD COLUMN usage TEXT;
"#),
    Sql(r#"
-- Sessions deleted from this device only (D5, D6: they stay on the server).
-- Permanent: later events for one advance the cursor but are never stored.
CREATE TABLE deleted_sessions (
    session_id TEXT PRIMARY KEY,
    at TEXT NOT NULL
);
"#),
    Sql(r#"
-- Pending approvals this device alerted on or judged no news (D5: never
-- synced), so a restart alerts only for the rest. A row goes when its
-- approval stops pending. Everything pending before this counts as
-- alerted, so an upgrade alerts for nothing old.
CREATE TABLE alerted_approvals (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL
);
INSERT OR IGNORE INTO alerted_approvals (id, session_id)
    SELECT approval.value, sessions.id FROM sessions, json_each(sessions.pending_approvals) AS approval;
"#),
    Sql(r#"
-- The names of the server's devices, as its last `hello` listed them, so
-- the views name who answered or wrote while offline too.
CREATE TABLE devices (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
"#),
    Sql(r#"
-- `SessionSummary::agent` as JSON, and the models of the newest
-- `ModelsOffered`. A replica from before this holds no such events, so
-- every session starts without: nothing to backfill.
ALTER TABLE sessions ADD COLUMN agent TEXT;
CREATE TABLE offered_models (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    models TEXT NOT NULL
);
"#),
    Sql(r#"
-- Prompts saved to use again in any session (D5: never synced). `used`
-- orders them: higher was saved or used later.
CREATE TABLE snippets (
    id TEXT PRIMARY KEY,
    name TEXT,
    text TEXT NOT NULL,
    used INTEGER NOT NULL
);
"#),
    Sql(r#"
-- Session contexts (protocol v9). `SessionSummary::context`: a replica from
-- before this holds no context events, so every session is in its first
-- context, which has the session's id. Stored events and queued commands
-- are rewritten to the v9 shapes (no serde defaults): every event belongs
-- to its session's first context, which the server's own migration names
-- "start", and a queued message was written in it. The server's migration
-- makes that context current, so such a message is still accepted.
ALTER TABLE sessions ADD COLUMN context TEXT NOT NULL DEFAULT '';
UPDATE sessions SET context = id;
UPDATE events SET event = json_set(event, '$.context', session_id);
UPDATE events SET event = json_set(event, '$.kind.context_name', 'start')
    WHERE json_extract(event, '$.kind.type') = 'session_created';
UPDATE outbox SET command = json_set(command, '$.kind.context',
        json_extract(command, '$.kind.session_id'))
    WHERE json_extract(command, '$.kind.type') = 'send_message';
"#),
    Sql(r#"
-- `SessionSummary::work` as JSON (protocol v11). A replica from before this
-- holds no background work events, so every session starts with none:
-- nothing to backfill.
ALTER TABLE sessions ADD COLUMN work TEXT;
"#),
    Sql(r#"
-- Shell commands (protocol v14), all of it this device's (D5: never
-- synced). What each session's composer writes: a session with no row
-- writes to its agent.
CREATE TABLE composer_modes (
    session_id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK (mode IN ('agent', 'shell'))
);
-- A draft per session and composer mode; `when` is the condition a shell
-- draft runs on, `always` for an agent draft. SQLite cannot change a
-- primary key in place, so the table is rebuilt; every draft from before
-- this was written to the agent. Nothing else indexes or references it.
CREATE TABLE drafts_by_mode (
    session_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('agent', 'shell')),
    text TEXT NOT NULL,
    cursor INTEGER NOT NULL,
    "when" TEXT NOT NULL DEFAULT 'always'
        CHECK ("when" IN ('always', 'on_success', 'on_failure')),
    PRIMARY KEY (session_id, mode)
);
INSERT INTO drafts_by_mode (session_id, mode, text, cursor, "when")
    SELECT session_id, 'agent', text, cursor, 'always' FROM drafts;
DROP TABLE drafts;
ALTER TABLE drafts_by_mode RENAME TO drafts;
-- The composer a snippet is for; a text is saved once per kind. Every
-- snippet from before this was a prompt.
ALTER TABLE snippets ADD COLUMN kind TEXT NOT NULL DEFAULT 'agent'
    CHECK (kind IN ('agent', 'shell'));
"#),
    Sql(r#"
-- `TurnRecord`s: each `TurnUsage` event's figures by seq, what the usage
-- screen totals without replaying the log. `usage` is the `TurnUsage` as
-- JSON. Backfilled from the stored events.
CREATE TABLE turns (
    seq INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    at TEXT NOT NULL,
    usage TEXT NOT NULL
);
INSERT INTO turns (seq, session_id, at, usage)
    SELECT seq, session_id, json_extract(event, '$.at'),
        json_remove(json_extract(event, '$.kind'), '$.type')
    FROM events WHERE json_extract(event, '$.kind.type') = 'turn_usage';
"#),
    Sql(r#"
-- The latest answer to each workdir read (`Store::put_workdir`): `path` as
-- requested, '' for changes; `result` the `ChangesResult`, `ListResult`,
-- `FileResult` or, since the migration that allows 'log', `LogResult` as JSON.
CREATE TABLE workdir_snapshots (
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('changes', 'list', 'file')),
    path TEXT NOT NULL,
    result TEXT NOT NULL CHECK (json_valid(result)),
    PRIMARY KEY (session_id, kind, path)
);
-- The blobs each stored answer names. A blob row goes once no ref names it,
-- whatever session or read: workdir blob ids are shared across sessions.
CREATE TABLE workdir_blob_refs (
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    PRIMARY KEY (session_id, kind, path, blob_id)
);
CREATE INDEX workdir_blob_refs_by_blob ON workdir_blob_refs (blob_id);
"#),
    Sql(r#"
-- `SessionSummary::importing` (protocol v16): an adopted session whose
-- `ImportFinished` is not stored yet. A replica from before this holds no
-- events of v16, so no session is mid-import: nothing to backfill.
ALTER TABLE sessions ADD COLUMN importing INTEGER NOT NULL DEFAULT 0;
"#),
    Sql(r#"
-- `SessionSummary::adopted`: the outside conversation a session adopted, as
-- its `SessionCreated` names it (`from` of kind `agent`, protocol v12 on).
-- Backfilled from the stored log.
ALTER TABLE sessions ADD COLUMN adopted TEXT;
UPDATE sessions SET adopted = (SELECT json_extract(event, '$.kind.from.conversation')
    FROM events
    WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'session_created'
        AND json_extract(event, '$.kind.from.kind') = 'agent'
    ORDER BY seq
    LIMIT 1);
"#),
    Sql(r#"
-- The skywave_proto::PROTOCOL_VERSION whose wire JSON the replica's JSON
-- columns hold; one row, written by `migrate` when the replica is created
-- and only rewritten by `Store::reset_replica`. `IF NOT EXISTS`: the
-- migration tests rerun later migrations over a current schema.
CREATE TABLE IF NOT EXISTS protocol_stamp (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
"#),
    Sql(r#"
-- The server log the replica holds (skywave_proto::LogId), and the run of
-- the server it last took a `hello` from (RunId), which the next `hello`
-- asks about; one row once a `hello` was taken, cleared with the replica.
-- `IF NOT EXISTS` as above.
CREATE TABLE IF NOT EXISTS server_log (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    log_id TEXT NOT NULL,
    run_id TEXT NOT NULL
);
"#),
    Sql(r#"
-- The content hash a blob's download is checked against
-- (skywave_proto::BlobRef::sha256, lowercase hex): one row per `blobs` row,
-- inserted with it and deleted with it by the trigger. A table, not a
-- column, so this migration reruns over a current schema as the two above
-- do. A blob row without one (a replica from before hashes, whose events no
-- longer decode either) does not read, so the replica is reset.
CREATE TABLE IF NOT EXISTS blob_hashes (
    blob_id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS blob_hashes_go_with_blobs AFTER DELETE ON blobs
BEGIN
    DELETE FROM blob_hashes WHERE blob_id = old.id;
END;
"#),
    Sql(r#"
-- What the server did to a blob (crate::BlobFate, JSON), folded from the
-- log's `BlobDeleted` and `BlobCropped` (protocol v23), with the seq of
-- that event. Cleared with the events. A replica from before this holds no
-- such events: nothing to backfill. `IF NOT EXISTS` as above.
CREATE TABLE IF NOT EXISTS blob_fates (
    blob_id TEXT PRIMARY KEY,
    fate TEXT NOT NULL,
    seq INTEGER NOT NULL
);
"#),
    Sql(r#"
-- Why the download of a `not_downloaded` blob was given up
-- (BlobStatus::Failed), so a restart keeps it failed and `Eager` leaves it
-- until it is asked for again. Any other status written deletes the row,
-- as does the trigger with the blob's row. A table, not a state, so the
-- `blobs` CHECK stays and this reruns over a current schema; a replica
-- from before this kept failures in memory only: nothing to backfill.
CREATE TABLE IF NOT EXISTS blob_failures (
    blob_id TEXT PRIMARY KEY,
    reason TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS blob_failures_go_with_blobs AFTER DELETE ON blobs
BEGIN
    DELETE FROM blob_failures WHERE blob_id = old.id;
END;
"#),
    Sql(r#"
-- The latest answer to the usage limits read (`Store::put_usage_limits`,
-- protocol v27): one row, the `UsageLimitsResult` as JSON. Cleared with
-- the replica. A replica from before this never asked: nothing to
-- backfill. `IF NOT EXISTS` as above.
CREATE TABLE IF NOT EXISTS usage_limits (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    result TEXT NOT NULL CHECK (json_valid(result))
);
"#),
    Sql(r#"
-- `SessionSummary::last_message_seq`, the session list's order, backfilled
-- from the stored log: the session's newest message, else its first event.
ALTER TABLE sessions ADD COLUMN last_message_seq INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET last_message_seq = COALESCE(
    (SELECT MAX(seq) FROM events WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'user_message'),
    (SELECT MIN(seq) FROM events WHERE session_id = sessions.id),
    last_seq);
"#),
    Sql(r#"
-- What the server's last `hello` said of itself (crate::ServerFacts): one
-- row once a `hello` was taken, written whole by each and cleared with the
-- replica, so no fact of one server outlives a switch to another. Moved
-- from the `settings` rows that kept each fact apart; a flag stored as
-- anything but 'true' reads as false. Over a current schema those rows
-- are gone and nothing moves; `IF NOT EXISTS` as above.
CREATE TABLE IF NOT EXISTS server_facts (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    shell_enabled INTEGER NOT NULL CHECK (shell_enabled IN (0, 1)),
    bypass_allowed INTEGER NOT NULL CHECK (bypass_allowed IN (0, 1)),
    login_expires TEXT,
    agent_login_offered INTEGER NOT NULL CHECK (agent_login_offered IN (0, 1)),
    agent_logged_out INTEGER NOT NULL CHECK (agent_logged_out IN (0, 1))
);
INSERT OR REPLACE INTO server_facts (id, shell_enabled, bypass_allowed, login_expires,
        agent_login_offered, agent_logged_out)
    SELECT 1,
        EXISTS (SELECT 1 FROM settings WHERE key = 'shell_enabled' AND value = 'true'),
        EXISTS (SELECT 1 FROM settings WHERE key = 'bypass_allowed' AND value = 'true'),
        (SELECT value FROM settings WHERE key = 'login_expires'),
        EXISTS (SELECT 1 FROM settings WHERE key = 'agent_login_offered' AND value = 'true'),
        EXISTS (SELECT 1 FROM settings WHERE key = 'agent_logged_out' AND value = 'true')
    WHERE EXISTS (SELECT 1 FROM settings WHERE key IN ('shell_enabled', 'bypass_allowed',
        'login_expires', 'agent_login_offered', 'agent_logged_out'));
DELETE FROM settings WHERE key IN ('shell_enabled', 'bypass_allowed', 'login_expires',
    'agent_login_offered', 'agent_logged_out');
"#),
    // The server addresses in their canonical form (`ServerAddress`).
    Code(canonicalize_server_addresses),
    Sql(r#"
-- The images a draft holds (`Store::attach_to_draft`). `attachments` keeps
-- each image once, by its sha256, with what the composer and the model need
-- of it; its bytes are its file under the data dir (`attachment_path`).
-- `draft_attachments` keeps a session's agent draft's images in `position`
-- order. A table of its own, not a column of `drafts`: a blank text deletes
-- the `drafts` row, and the images stay. A replica from before this held no
-- images: nothing to backfill. `IF NOT EXISTS` as above.
CREATE TABLE IF NOT EXISTS attachments (
    sha256 TEXT PRIMARY KEY,
    media_type TEXT NOT NULL CHECK (media_type IN ('png', 'jpeg', 'gif', 'webp')),
    size INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS draft_attachments (
    session_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode = 'agent'),
    position INTEGER NOT NULL,
    sha256 TEXT NOT NULL REFERENCES attachments (sha256),
    PRIMARY KEY (session_id, mode, position)
);
"#),
    // The workdir commit log (`WorkdirAsk::Log`) is a fourth read kind. SQLite
    // keeps a column's CHECK as created, so `workdir_snapshots` is rebuilt
    // (create, copy, drop, rename) to allow 'log'. Its rows keep their kind
    // and path; no index, trigger or foreign key names the table.
    Sql(r#"
CREATE TABLE workdir_snapshots_with_log (
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('changes', 'list', 'file', 'log')),
    path TEXT NOT NULL,
    result TEXT NOT NULL CHECK (json_valid(result)),
    PRIMARY KEY (session_id, kind, path)
);
INSERT INTO workdir_snapshots_with_log (session_id, kind, path, result)
    SELECT session_id, kind, path, result FROM workdir_snapshots;
DROP TABLE workdir_snapshots;
ALTER TABLE workdir_snapshots_with_log RENAME TO workdir_snapshots;
"#),
    Sql(r#"
-- `SessionSummary::worktree` (protocol v32): the git worktree a session runs
-- in, as its `SessionCreated` names it. json_extract returns the object's JSON
-- text, which is what the column holds; NULL when the event has none.
-- Backfilled from the stored log.
ALTER TABLE sessions ADD COLUMN worktree TEXT;
UPDATE sessions SET worktree = (SELECT json_extract(event, '$.kind.worktree')
    FROM events
    WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'session_created'
    ORDER BY seq
    LIMIT 1);
"#),
    Sql(r#"
-- `SessionSummary::worktree_removed` (protocol v32): the session's worktree
-- was removed by `RemoveWorktree`. Backfilled from the stored log.
ALTER TABLE sessions ADD COLUMN worktree_removed INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET worktree_removed = EXISTS (SELECT 1 FROM events
    WHERE session_id = sessions.id
        AND json_extract(event, '$.kind.type') = 'worktree_removed');
"#),
];

/// Rewrites the stored server addresses to their canonical form
/// ([`ServerAddress`]'s `Display`), as the settings now read them. Before
/// this the settings took any string: one typed in `SetServerAddress` was
/// kept as typed (`MyHost`), so the same server could be stored as several
/// strings. An address that does not parse is dropped, with the reason
/// logged: connecting to it could only fail, over and over. The others
/// lose duplicates and the current address.
fn canonicalize_server_addresses(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let stored = |key: &str| -> rusqlite::Result<Option<Option<String>>> {
        tx.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            Ok(row.get_ref(0)?.as_str().ok().map(str::to_owned))
        })
        .optional()
    };
    let parse = |text: &str| {
        ServerAddress::parse(text)
            .inspect_err(|error| {
                tracing::warn!(%error, "dropping a stored server address that does not parse");
            })
            .ok()
    };
    let current = match stored(KEY_SERVER_ADDRESS)? {
        None => None,
        Some(value) => value.as_deref().and_then(parse),
    };
    let mut others: Vec<ServerAddress> = Vec::new();
    if let Some(value) = stored(KEY_OTHER_ADDRESSES)? {
        let texts = value
            .as_deref()
            .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok());
        if texts.is_none() {
            tracing::warn!("dropping stored server addresses that are not a list of strings");
        }
        for address in texts.iter().flatten().filter_map(|text| parse(text)) {
            if current.as_ref() != Some(&address) && !others.contains(&address) {
                others.push(address);
            }
        }
    }
    put_setting_in(
        tx,
        KEY_SERVER_ADDRESS,
        current.map(|a| a.to_string()).as_deref(),
    )?;
    put_setting_in(
        tx,
        KEY_OTHER_ADDRESSES,
        other_addresses_json(&others).as_deref(),
    )?;
    Ok(())
}

/// First schema version with `protocol_stamp`.
const STAMPED_SINCE: i64 = 21;

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Rewrites the wire JSON the replica holds (events, the outbox's commands,
/// ...) from one `PROTOCOL_VERSION` to the next. It runs in the open's
/// transaction, after the schema migrations, so it reads the current schema.
type ProtocolStep = fn(&Transaction<'_>) -> Result<(), StoreError>;

/// Step `i` moves the stored wire JSON from protocol version
/// `MIGRATED_SINCE + i` to the next (D43), as the server's step of the same
/// bump moves its log. Every bump adds one, a no-op saying so if nothing
/// stored changed; the length makes a bump without one fail to compile.
/// Append; never edit a released one.
const PROTOCOL_STEPS: [ProtocolStep; (PROTOCOL_VERSION - MIGRATED_SINCE) as usize] = [
    v23_to_v24, v24_to_v25, v25_to_v26, v26_to_v27, v27_to_v28, v28_to_v29, v29_to_v30, v30_to_v31,
    v31_to_v32, v32_to_v33,
];

/// v23 to v24, as the server's step of the same name. What v24 changed of
/// what is stored, and how each is moved:
/// - `Full::Inline` (a small tool output carried in its event): new, so no
///   v23 event has one; v23's `Full::Blob` and `Full::Lost` read unchanged.
/// - `full` on `AgentMessage` and on an imported `UserMessage` or
///   `AgentMessage` (a long message's whole text as a blob): a new
///   optional field, which v23 JSON lacks and reads as `None`, so no event
///   is rewritten; v23's long messages stay whole in their events.
/// - Compressed event batches and blob bodies (D45): transport only. A
///   `.part` file and its recorded progress count decoded bytes, as they
///   did, so a v23 download resumes unchanged; nothing stored changes.
fn v23_to_v24(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v24 to v25, as the server's step of the same name. What v25 changed of
/// what is stored, and how each is moved:
/// - `context_tokens` and `context_window` on `TurnUsage` (in `TurnUsage`
///   events, `turns.usage` and `sessions.usage`): new optional fields,
///   which v24 JSON lacks and reads as `None`, so nothing is rewritten.
/// - `context` on the `SessionUsage` in `sessions.usage`: an optional field
///   v24 lacks, read as `None`, the unknown fill a session shows until its
///   next turn reports one.
/// - `EventKind::SubagentUsage`: new, so no v24 event is one.
fn v24_to_v25(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v25 to v26, as the server's step of the same name. What v26 changed of
/// what is stored, and how each is moved:
/// - `EventKind::QuestionAsked` and `QuestionAnswered`: new, so no v25
///   event is one; a v25 pending approval in `sessions.pending_approvals`
///   stays a plain one.
/// - `CommandKind::AnswerQuestion` and `RejectReason::BadAnswer`: new, so
///   no v25 outbox entry holds either.
fn v25_to_v26(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v26 to v27, as the server's step of the same name. v27 added the usage
/// limits read, whose answer lives in `usage_limits`, a table the schema
/// migration before this step made empty: no v26 row holds one, and
/// nothing else stored changes.
fn v26_to_v27(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v27 to v28, as the server's step of the same name. v28 added
/// `login_expires` to `hello`, kept under its own `settings` key that no
/// v27 replica wrote: a missing key reads as no expiry until the next
/// `hello` says one, and nothing stored changes.
fn v27_to_v28(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v28 to v29, as the server's step of the same name. v29 added
/// `agent_login_offered` and `agent_logged_out` to `hello`, each kept under
/// its own `settings` key that no v28 replica wrote: a missing key reads as
/// `false` until the next `hello` says otherwise. The agent login's URL and
/// code are never stored. Nothing stored changes.
fn v28_to_v29(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v29 to v30, as the server's step of the same name. v30 added image
/// attachments: outbox `send_message` commands and replica `user_message`
/// events read as no attachments, as their JSON lacks the field. Nothing
/// stored changes.
fn v29_to_v30(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v30 to v31, as the server's step of the same name. v31 added the
/// workdir commit log, a new read; no stored workdir answer or event
/// changes shape. Nothing stored changes.
fn v30_to_v31(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v31 to v32, as the server's step of the same name. v32 added an optional
/// `worktree` to `session_created` events, which v31 JSON lacks and reads as
/// `None`. v32 also added `RemoveWorktree` and `WorktreeRemoved`, which v31
/// data cannot hold, so nothing stored changes.
fn v31_to_v32(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// v32 to v33, as the server's step of the same name. v33 added the
/// `RejectReason` `ShellRunning` and nothing else; a v32 command result cannot
/// hold it, so nothing stored changes.
fn v32_to_v33(_tx: &Transaction<'_>) -> Result<(), StoreError> {
    Ok(())
}

/// The files SQLite keeps beside [`DB_FILE`] in WAL mode.
const WAL_FILES: [&str; 2] = ["core.db-wal", "core.db-shm"];

/// Where [`Store::reset_replica`] builds a new database before renaming it
/// over [`DB_FILE`].
const FRESH_DB_FILE: &str = "core.db.new";
const FRESH_JOURNAL_FILE: &str = "core.db.new-journal";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot create data directory {path}: {source}")]
    DataDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database schema version {found} is newer than supported version {SCHEMA_VERSION}")]
    NewerSchema { found: i64 },
    /// The replica was written by a protocol version this build cannot
    /// migrate (older than `MIGRATED_SINCE`, or newer than its own), whose
    /// stored wire JSON it may not read; [`Store::reset_replica`] starts it
    /// again.
    #[error(
        "the replica was written for protocol version {found}, this build migrates versions {} to {current}",
        MIGRATED_SINCE
    )]
    ProtocolMismatch { found: u32, current: u32 },
    /// The replica breaks an invariant of its own schema.
    #[error("the replica is corrupt: {0}")]
    Corrupt(&'static str),
    /// Replacing the database file ([`Store::reset_replica`]) failed.
    #[error("cannot replace {path}: {source}")]
    Replace {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The batch starts after `cursor + 1`; applying it would leave a hole.
    #[error("event batch from {from:?} leaves a gap after cursor {cursor:?}")]
    Gap { cursor: Seq, from: Seq },
    /// Seq 0 is "before everything" and never names an event.
    #[error("event batch starts at seq 0")]
    BatchFromZero,
    #[error("value {0} does not fit an SQLite integer")]
    OutOfRange(u64),
    /// [`Store::withdraw`] found no row it may delete: the command is gone
    /// or was accepted.
    #[error("command {0:?} cannot be withdrawn")]
    NotWithdrawable(CommandId),
    #[error("blob {0:?} is not known")]
    UnknownBlob(BlobId),
    #[error("path {0} is not valid UTF-8")]
    NonUtf8Path(PathBuf),
    /// [`Store::put_workdir`] was given an answer of another kind than its ask.
    #[error("a workdir answer of another kind than its ask")]
    WorkdirMismatch,
}

impl StoreError {
    /// Whether the error says the replica itself cannot be read by this
    /// build — written by another protocol or schema version, a stored row
    /// that no longer decodes, or a damaged database file — as opposed to
    /// a failure of the moment (I/O, a busy lock) or of the call. Only
    /// [`Store::reset_replica`] gets past it.
    pub fn unreadable(&self) -> bool {
        use rusqlite::Error as Sql;
        match self {
            StoreError::ProtocolMismatch { .. }
            | StoreError::NewerSchema { .. }
            | StoreError::Corrupt(_)
            | StoreError::Json(_) => true,
            StoreError::Sqlite(err) => match err {
                Sql::FromSqlConversionFailure(..)
                | Sql::InvalidColumnType(..)
                | Sql::IntegralValueOutOfRange(..)
                | Sql::QueryReturnedNoRows => true,
                Sql::SqliteFailure(failure, _) => matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                ),
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether the write failed because the device's disk (or the user's
    /// quota) is full: SQLite's `SQLITE_FULL`, or a file the store writes
    /// failing as [`io_disk_full`] says.
    pub fn disk_full(&self) -> bool {
        match self {
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) => {
                failure.code == rusqlite::ErrorCode::DiskFull
            }
            StoreError::DataDir { source, .. } | StoreError::Replace { source, .. } => {
                io_disk_full(source)
            }
            _ => false,
        }
    }
}

/// Whether an I/O error says the device's disk is full: `ENOSPC`
/// (`StorageFull`) or a quota (`EDQUOT`, `QuotaExceeded`).
pub(crate) fn io_disk_full(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
    )
}

/// Whether blobs download alongside metadata or on explicit intent (D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlobPolicy {
    Eager,
    #[default]
    OnDemand,
}

/// User settings kept in the replica. The server addresses are not among
/// them: they belong to the pairing ([`Pairing`](crate::Pairing)).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub blob_policy: BlobPolicy,
    /// What the server's last `hello` said of itself
    /// ([`Store::set_server_facts`]); the default until one did, and again
    /// once the replica is cleared.
    pub server: ServerFacts,
}

/// What the server's `hello` says of itself, kept so it shows offline too.
/// One unit, stored as one row: each `hello` replaces all of it, and
/// clearing the replica drops all of it, so nothing one server said
/// survives pairing with another. The default is what is assumed before
/// any `hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerFacts {
    /// Whether the server runs shell commands.
    pub shell_enabled: bool,
    /// Whether the server allows bypass mode.
    pub bypass_allowed: bool,
    /// When the agent's login expires; `None` when none was reported.
    pub login_expires: Option<DateTime<Utc>>,
    /// Whether a device may log the server's agent in.
    pub agent_login_offered: bool,
    /// Whether the server's agent is logged out.
    pub agent_logged_out: bool,
}

impl ServerFacts {
    /// What `hello` says.
    pub fn of(hello: &HelloResponse) -> Self {
        ServerFacts {
            shell_enabled: hello.shell,
            bypass_allowed: hello.bypass,
            login_expires: hello.login_expires,
            agent_login_offered: hello.agent_login_offered,
            agent_logged_out: hello.agent_logged_out,
        }
    }

    /// A login from this device ended `LoggedIn`, expiring at `expires`:
    /// what a `hello` would say now.
    pub fn logged_in(&mut self, expires: Option<DateTime<Utc>>) {
        self.agent_logged_out = false;
        self.login_expires = expires;
    }
}

impl Settings {
    /// Takes in the stored row `key` = `value`; an error if this build
    /// cannot read the value. Keys written by a newer build are left alone,
    /// as are the server addresses an older build kept here, which opening
    /// the core moves into the pairing ([`Store::legacy_server_addresses`]).
    fn read(&mut self, key: &str, value: String) -> Result<(), StoreError> {
        if key == KEY_BLOB_POLICY {
            self.blob_policy = match value.as_str() {
                "eager" => BlobPolicy::Eager,
                "on_demand" => BlobPolicy::OnDemand,
                _ => {
                    return Err(bad_value(1, Type::Text, format!("blob policy {value:?}")).into());
                }
            }
        }
        Ok(())
    }

    /// Whether [`read`](Settings::read) takes the row in.
    fn readable(key: &str, value: &str) -> bool {
        Settings::default().read(key, value.to_owned()).is_ok()
    }
}

/// What [`Store::take_server_log`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogCheck {
    /// The log holds everything the replica has seen of it.
    Continues,
    /// The log lost events the replica has seen, up to `seen`.
    Lost { seen: Seq },
}

/// Why the outbox gave up on a message's image before posting it: the
/// server refused the upload, or the local file is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentRefusal {
    /// The server refused the upload as larger than it takes.
    TooLarge,
    /// The server did not read the upload as an image.
    NotAnImage,
    /// The server's hash of the upload differs from the one asked for.
    HashMismatch,
    /// The local file under the attachments directory is gone or unreadable.
    Missing,
}

/// Delivery state of a queued command.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboxStatus {
    /// Persisted locally, not yet handed to the transport.
    Queued,
    /// Handed to the transport, no result yet.
    Sending,
    /// The server's answer; final.
    Done(CommandResult),
    /// The server refused the request without an answer
    /// ([`TransportError::refuses_command`](crate::TransportError::refuses_command)),
    /// or the log shows it never applied after an unreadable reply or
    /// [`MAX_SERVER_ERRORS`](crate::MAX_SERVER_ERRORS) server errors;
    /// final, never sent again.
    Undeliverable,
    /// The message was never posted: its image could not be staged
    /// ([`AttachmentRefusal`]). Final, never sent again.
    AttachmentRefused(AttachmentRefusal),
}

/// What the outbox `result` column holds once a command is settled: the
/// server's answer, or a marker the core wrote itself. Both are objects
/// tagged `result`, so `json_extract(result, '$.result')` tells them apart.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Settled {
    Answered(CommandResult),
    Local(LocalResult),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum LocalResult {
    Undeliverable,
    AttachmentRefused { reason: AttachmentRefusal },
}

impl OutboxStatus {
    /// Whether the command has its final outcome: the server's answer, or
    /// given up on as [`Undeliverable`](OutboxStatus::Undeliverable) or
    /// [`AttachmentRefused`](OutboxStatus::AttachmentRefused). Only
    /// an unsettled command (the outbox's `result IS NULL`) is still sent,
    /// claimed ([`Store::claim_send`]) or settled ([`Store::record_result`],
    /// [`Store::record_undeliverable`], [`Store::record_attachment_refused`]).
    pub fn is_settled(&self) -> bool {
        match self {
            OutboxStatus::Queued | OutboxStatus::Sending => false,
            OutboxStatus::Done(_) | OutboxStatus::Undeliverable => true,
            OutboxStatus::AttachmentRefused(_) => true,
        }
    }
}

/// One command in the outbox. The full [`Command`] is kept so a resend uses
/// the same [`CommandId`] (D7).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub command: Command,
    pub status: OutboxStatus,
    /// How often the command was claimed for sending. 0 proves it never
    /// reached the server: a claim is durable before the request leaves.
    pub tries: u32,
    /// When the first claim was made.
    pub first_sent_at: Option<DateTime<Utc>>,
    /// When the server's answer, or its refusal of the request, was recorded.
    pub answered_at: Option<DateTime<Utc>>,
}

impl OutboxEntry {
    /// Whether the user may take the command back ([`Store::withdraw`]): a
    /// message, shell run, session creation, context move, or blob delete
    /// or crop the server has not answered, or refused. One already sent may have reached the
    /// server, and withdrawing cannot stop it there; it only stops the
    /// resends, so a command whose request keeps failing no longer holds
    /// back every later one. If the server took it, its event shows it in
    /// the log like any other, and its answer, if one still comes, settles
    /// nothing.
    pub fn discardable(&self) -> bool {
        let kind = matches!(
            self.command.kind,
            CommandKind::SendMessage { .. }
                | CommandKind::RunShell { .. }
                | CommandKind::CreateSession { .. }
                | CommandKind::MoveContext { .. }
                | CommandKind::DeleteBlob { .. }
                | CommandKind::CropBlob { .. }
        );
        kind && match &self.status {
            OutboxStatus::Done(result) => matches!(result, CommandResult::Rejected { .. }),
            OutboxStatus::Undeliverable | OutboxStatus::AttachmentRefused(_) => true,
            OutboxStatus::Queued | OutboxStatus::Sending => true,
        }
    }
}

/// Local state of one blob.
#[derive(Debug, Clone, PartialEq)]
pub enum BlobStatus {
    NotDownloaded,
    /// Not downloaded: the download was given up for `reason`. It stays
    /// so, across restarts and whatever the [`BlobPolicy`], until it is
    /// asked for again.
    Failed {
        reason: String,
    },
    /// `got` bytes of the content are in the partial file at `path`.
    Downloading {
        got: u64,
        path: PathBuf,
    },
    /// Not downloaded, nor downloading: a download nobody asked for,
    /// stopped when the policy went back to `OnDemand`. The `got` bytes it
    /// paid for stay in the partial file at `path`, so asking for the blob
    /// (or `Eager`) resumes from them; a restart under `OnDemand` leaves
    /// it (`crate::blobs`, "What gets downloaded"). Stored as
    /// `not_downloaded` with `got` and `path` set, which no other status
    /// writes, so the `blobs` CHECK and the schema stay.
    Stopped {
        got: u64,
        path: PathBuf,
    },
    /// Complete content is at `path`.
    Available {
        path: PathBuf,
    },
}

/// A blob the replica knows of, registered when an event or a stored
/// workdir answer ([`Store::put_workdir`]) references it.
#[derive(Debug, Clone, PartialEq)]
pub struct BlobRecord {
    pub id: BlobId,
    pub size: u64,
    /// What the content must hash to before it is available.
    pub sha256: ContentHash,
    pub status: BlobStatus,
    /// A stored workdir answer names it: it downloads only when asked for,
    /// whatever the [`BlobPolicy`].
    pub workdir: bool,
}

/// Result of a [`Store::put_workdir`] that wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkdirStored {
    /// Blobs the answer names that the replica did not know, now registered
    /// as [`BlobStatus::NotDownloaded`].
    pub registered: Vec<BlobId>,
    /// Blobs the previous answer named that nothing names any more, whose
    /// rows are gone and whose files the caller removes.
    pub purged: Vec<BlobId>,
}

/// Result of [`Store::apply_batch`].
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Cursor after the batch.
    pub cursor: Seq,
    /// Number of events newly stored; 0 when the batch was already applied.
    pub new_events: usize,
    /// Sessions whose summaries changed.
    pub sessions: Vec<SessionId>,
    /// Blobs first referenced by this batch, now registered as
    /// [`BlobStatus::NotDownloaded`], in log order; none the batch also
    /// purged, and none a [`local_crops`](Applied::local_crops) entry
    /// produces.
    pub blobs: Vec<BlobId>,
    /// Blobs the batch deleted or cropped away on the server whose rows are
    /// gone, and whose files (complete and partial) the caller removes.
    pub purged: Vec<BlobId>,
    /// Crops of held blobs the batch left pending, in log order: this
    /// device makes their `into` from the bytes it has.
    pub local_crops: Vec<PendingCrop>,
    /// Outbox commands still unanswered that the batch settled as accepted
    /// at the seq of their event, in log order.
    pub settled: Vec<(CommandId, Seq)>,
}

/// A server crop this device makes itself from the bytes it holds: `from`
/// (the cropped blob, `Available` here or itself a pending crop's `into`)
/// keeps its row until the device made `into` from it. Not stored on its
/// own: a `Cropped` fate whose blob still has a row is one
/// ([`Store::pending_crops`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCrop {
    pub into: BlobId,
    pub from: BlobId,
}

/// How far this device has read a session: through `seq`, when the session
/// had `notable` notable events (D5: per device, never synced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadMark {
    pub seq: Seq,
    pub notable: u64,
}

/// Everything the replica holds except the events themselves, as read on
/// startup.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub cursor: Seq,
    /// Most recently active first.
    pub sessions: Vec<SessionSummary>,
    /// Oldest first.
    pub outbox: Vec<OutboxEntry>,
    pub blobs: Vec<BlobRecord>,
    /// What the server did to blobs, by the blob it names
    /// ([`Store::blob_fates`]).
    pub fates: HashMap<BlobId, BlobFate>,
    /// Crops this device still makes itself, in log order
    /// ([`Store::pending_crops`]).
    pub pending_crops: Vec<PendingCrop>,
    pub settings: Settings,
    pub drafts: HashMap<(SessionId, ComposerMode), Draft>,
    /// The images each session's agent draft holds, in order
    /// ([`Store::draft_images`]).
    pub draft_images: HashMap<SessionId, Vec<AttachedImage>>,
    /// Every known image's metadata, by its sha256 ([`Store::attachments`]).
    pub attachments: HashMap<ContentHash, AttachedImage>,
    /// Sessions whose composer does not write to the agent
    /// ([`Store::set_composer_mode`]).
    pub composer_modes: HashMap<SessionId, ComposerMode>,
    pub read_marks: HashMap<SessionId, ReadMark>,
    /// Sessions deleted from this device ([`Store::delete_session_locally`]).
    pub deleted: HashSet<SessionId>,
    /// Pending approvals this device alerted on or judged no news
    /// ([`Store::mark_alerted`]).
    pub alerted: HashSet<ApprovalId>,
    /// The server's devices by name ([`Store::set_devices`]).
    pub devices: HashMap<DeviceId, String>,
    /// The models of the newest `ModelsOffered` in the log, in any session;
    /// empty until one comes.
    pub models: Vec<ModelChoice>,
    /// This device's saved prompts, in no order ([`Store::set_snippet`]).
    pub snippets: Vec<Snippet>,
    /// Every stored turn's usage, in log order ([`Store::turns`]).
    pub turns: Vec<TurnRecord>,
    /// The latest stored answer to each workdir read
    /// ([`Store::put_workdir`]).
    pub workdirs: HashMap<WorkdirAsk, WorkdirAnswer>,
    /// The latest stored answer to the usage limits read
    /// ([`Store::put_usage_limits`]); `None` until one is.
    pub usage_limits: Option<UsageLimitsResult>,
}

/// Handle to the replica database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the replica in `data_dir` and migrates it
    /// to the current schema.
    ///
    /// Takes no lock: the [`Core`](crate::Core) holds the data dir's
    /// ([`LOCK_FILE`](crate::lock::LOCK_FILE)). A bare store beside a live
    /// core is for inspecting the replica (tests).
    ///
    /// Commands left in `Sending` by a previous run go back to `Queued`:
    /// nothing is in flight after a restart, and the server dedups the
    /// resend (D7). Their `tries` are kept: they may have gone out.
    ///
    /// A replica stamped with an older `PROTOCOL_VERSION` since
    /// `MIGRATED_SINCE` is migrated to this build's; any other is refused
    /// with [`StoreError::ProtocolMismatch`] before anything reads it.
    ///
    /// Then collects the images nothing needs any more, and the files without
    /// a row ([`Store::collect_attachments`]). A failure there is logged and
    /// the open goes on.
    pub fn open(data_dir: &Path) -> Result<Store, StoreError> {
        let store = Store::init(connect(data_dir)?)?;
        store.collect_image_files(data_dir);
        Ok(store)
    }

    /// A replica that lives only in memory, for tests of components above
    /// the store.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Store, StoreError> {
        migrate(&mut conn, Stamp::Check)?;
        conn.execute("UPDATE outbox SET sending = 0 WHERE result IS NULL", [])?;
        Ok(Store { conn })
    }

    /// Makes the replica in `data_dir` a fresh one for the same server,
    /// for a replica this build cannot read ([`StoreError::unreadable`]):
    /// the event log, session summaries, cursor, outbox, blob rows, stored
    /// workdir answers, offered models and device names go, and the
    /// replica is stamped with this build's `PROTOCOL_VERSION`, so the
    /// next sync pulls the whole log again. What is this device's stays —
    /// settings (the server address with them), drafts, composer modes,
    /// snippets, read marks, local deletes and alerted approvals — as does
    /// the device identity, which lives beside the database.
    ///
    /// A settings row this build cannot read goes, with a warning.
    ///
    /// If even that cannot be done in place (the file is damaged, from a
    /// newer schema, or a kept row does not decode either), the database
    /// is replaced by a new one holding only the settings rows this build
    /// can read, built beside it and renamed over it, so a crash leaves
    /// either the old database or the whole new one. The caller holds the data dir's
    /// lock and removes the blob files.
    pub fn reset_replica(data_dir: &Path) -> Result<(), StoreError> {
        match clear_replica_at(data_dir) {
            Ok(()) => Ok(()),
            Err(err) if err.unreadable() => {
                tracing::warn!(%err, "cannot reset the replica in place; creating it again");
                recreate_replica(data_dir)
            }
            Err(err) => Err(err),
        }
    }

    /// Reads back the whole replica state except the event log.
    pub fn load(&self) -> Result<Snapshot, StoreError> {
        Ok(Snapshot {
            cursor: self.cursor()?,
            sessions: self.sessions()?,
            outbox: self.outbox()?,
            blobs: self.blobs()?,
            fates: self.blob_fates()?,
            pending_crops: self.pending_crops()?,
            settings: self.settings()?,
            drafts: self.drafts()?,
            draft_images: self.draft_images()?,
            attachments: self.attachments()?,
            composer_modes: self.composer_modes()?,
            read_marks: self.read_marks()?,
            deleted: read_deleted(&self.conn)?,
            alerted: self.alerted()?,
            devices: self.devices()?,
            models: self.offered_models()?,
            snippets: self.snippets()?,
            turns: self.turns()?,
            workdirs: self.workdirs()?,
            usage_limits: self.usage_limits()?,
        })
    }

    /// The models of the newest `ModelsOffered` stored; empty if none.
    pub fn offered_models(&self) -> Result<Vec<ModelChoice>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT models FROM offered_models WHERE id = 0", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default())
    }

    // ---- events, sessions, cursor -------------------------------------

    /// Seq of the newest applied event; [`Seq::ZERO`] for a fresh replica.
    pub fn cursor(&self) -> Result<Seq, StoreError> {
        read_cursor(&self.conn)
    }

    /// The run of the server the last `hello` taken came from
    /// ([`Store::take_server_log`]), to ask the next one about; `None`
    /// before the first.
    pub fn server_run(&self) -> Result<Option<RunId>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT run_id FROM server_log WHERE id = 1", [], |row| {
                Ok(RunId(uuid_col(row, 0)?))
            })
            .optional()?)
    }

    /// Takes the log a `hello` reported, with its `head`, and says whether
    /// that log still holds what this replica has seen of it: the cursor,
    /// and the seq of every command the server accepted from this device
    /// that the pull may not have reached yet. The `hello` must have asked
    /// about [`Store::server_run`]. In one transaction: when it does, the
    /// log and run are kept for the next `hello`; when it does not, nothing
    /// is written.
    ///
    /// The log lost events this replica has seen (architecture.md, "A log
    /// that went backwards") when `head` is below what was seen; when it is
    /// another log than the one the replica holds; or when its record of
    /// runs no longer has the run last seen, or a run since started below
    /// what was seen: a run starts from the head the server found on disk,
    /// and within one log only the end of it can be lost. Equal is a clean
    /// restart. An unknown run is lost history even when everything seen
    /// may still be in the log: the record went back past that run, and
    /// nothing says how far. A replica that took no `hello` yet (fresh, or
    /// reset) has seen nothing to compare, beyond `head`; nor has one that
    /// has seen nothing.
    pub fn take_server_log(&mut self, head: Seq, log: &LogRun) -> Result<LogCheck, StoreError> {
        let tx = self.conn.transaction()?;
        let cursor = read_cursor(&tx)?;
        let accepted: Option<i64> = tx.query_row(
            "SELECT MAX(json_extract(result, '$.seq')) FROM outbox
             WHERE json_extract(result, '$.result') = 'accepted'",
            [],
            |row| row.get(0),
        )?;
        let accepted = accepted.map_or(Ok(Seq::ZERO), |seq| {
            u64::try_from(seq)
                .map(Seq)
                .map_err(|_| StoreError::Corrupt("an accepted command's seq is negative"))
        })?;
        let seen = cursor.max(accepted);
        let held: Option<LogId> = tx
            .query_row("SELECT log_id FROM server_log WHERE id = 1", [], |row| {
                Ok(LogId(uuid_col(row, 0)?))
            })
            .optional()?;
        let lost = head < seen
            || (seen > Seq::ZERO
                && held.is_some_and(|held| {
                    held != log.log
                        || match log.since {
                            // Not answered about the run last seen: nothing
                            // vouches for it.
                            None | Some(RunsSince::Unknown) => true,
                            Some(RunsSince::Known { next_start }) => {
                                next_start.is_some_and(|start| start < seen)
                            }
                        }
                }));
        if lost {
            return Ok(LogCheck::Lost { seen });
        }
        tx.execute(
            "INSERT INTO server_log (id, log_id, run_id) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET log_id = excluded.log_id, run_id = excluded.run_id",
            params![log.log.0.to_string(), log.run.0.to_string()],
        )?;
        tx.commit()?;
        Ok(LogCheck::Continues)
    }

    /// Clears the replica for the same server after its log lost events
    /// this replica holds ([`Store::take_server_log`]): what
    /// [`Store::reset_replica`] clears goes, in one transaction, and what
    /// is this device's (settings, drafts, composer modes, snippets, read
    /// marks, local deletes, alerted approvals) stays. Returns the blobs
    /// that were known, whose files the caller removes.
    pub fn reset_for_lost_history(&mut self) -> Result<Vec<BlobId>, StoreError> {
        let tx = self.conn.transaction()?;
        let blobs = {
            let mut stmt = tx.prepare("SELECT id FROM blobs ORDER BY id")?;
            let rows = stmt.query_map([], |row| Ok(BlobId(uuid_col(row, 0)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        clear_replica(&tx)?;
        tx.commit()?;
        Ok(blobs)
    }

    /// Stores the batch's new events, updates the session projection and
    /// registers referenced blobs, then advances the cursor — all in one
    /// transaction.
    ///
    /// Idempotent: events at or below the cursor are skipped, so replaying a
    /// batch or an overlapping range is a no-op for the old part. A batch
    /// starting after `cursor + 1` is rejected with [`StoreError::Gap`] and
    /// changes nothing.
    ///
    /// Approvals that stop pending leave the alerted ones
    /// ([`mark_alerted`](Store::mark_alerted)) in the same transaction.
    ///
    /// The log acknowledges commands too: an event naming an outbox command
    /// still unanswered settles it as `Accepted` at the event's seq,
    /// answered at `now` — what the reply would have recorded, had it been
    /// readable ([`record_result`](Store::record_result)). Whichever comes
    /// first is kept; the other changes nothing.
    ///
    /// A `BlobDeleted` or `BlobCropped` keeps the blob's fate
    /// ([`BlobFate`]), which says what to show instead. Its content is gone
    /// from the server, so a download of it cannot finish: the blob's row
    /// is purged ([`Applied::purged`]), unless a crop finds it held — its
    /// row `available`, or itself the `into` of a pending crop (a crop of a
    /// crop). A held blob keeps its row until the device made the crop's
    /// `into` from it ([`Applied::local_crops`], [`PendingCrop`]); that
    /// `into` registers but is left out of [`Applied::blobs`], so nothing
    /// fetches it eagerly. A blob purged also purges the blob of any crop
    /// pending to produce it. Otherwise a crop's `into` registers like any
    /// referenced blob. A blob with a fate never registers again.
    ///
    /// Events of a session deleted from this device advance the cursor but
    /// are not stored, project no summary and register no blob. Contiguity
    /// is the cursor's alone and events are read per session, so the holes
    /// they leave between stored events are harmless.
    pub fn apply_batch(
        &mut self,
        batch: &EventBatch,
        now: DateTime<Utc>,
    ) -> Result<Applied, StoreError> {
        if batch.from == Seq::ZERO {
            return Err(StoreError::BatchFromZero);
        }
        let tx = self.conn.transaction()?;
        let cursor = read_cursor(&tx)?;
        if batch.from > cursor.next() {
            return Err(StoreError::Gap {
                cursor,
                from: batch.from,
            });
        }

        let deleted = read_deleted(&tx)?;
        let mut touched: BTreeMap<SessionId, SessionSummary> = BTreeMap::new();
        let mut new_cursor = cursor;
        let mut new_events = 0;
        let mut blobs = Vec::new();
        let mut purged = Vec::new();
        let mut local_crops: Vec<PendingCrop> = Vec::new();
        let mut settled = Vec::new();
        for (seq, event) in batch.iter().filter(|(seq, _)| *seq > cursor) {
            new_cursor = seq;
            if let Some(id) = event.command_id {
                let accepted = Settled::Answered(CommandResult::Accepted { seq });
                if settle_in(&tx, id, &accepted, now)? {
                    settled.push((id, seq));
                }
            }
            // Offered for every session, so kept whatever session said so.
            if let EventKind::ModelsOffered { models } = &event.kind {
                tx.execute(
                    "INSERT INTO offered_models (id, models) VALUES (0, ?1)
                     ON CONFLICT (id) DO UPDATE SET models = excluded.models",
                    [to_json(models)?],
                )?;
            }
            if deleted.contains(&event.session_id) {
                continue;
            }
            tx.execute(
                "INSERT INTO events (seq, session_id, event) VALUES (?1, ?2, ?3)",
                params![
                    seq_sql(seq)?,
                    event.session_id.0.to_string(),
                    to_json(event)?
                ],
            )?;
            match touched.get_mut(&event.session_id) {
                Some(summary) => summary.apply(seq, event),
                None => {
                    let summary = match read_session(&tx, event.session_id)? {
                        Some(mut summary) => {
                            summary.apply(seq, event);
                            summary
                        }
                        None => SessionSummary::new(seq, event),
                    };
                    touched.insert(event.session_id, summary);
                }
            }
            // Decided before a crop's `into` registers below.
            if let Some((blob, fate)) = BlobFate::of(&event.kind) {
                let into = match &fate {
                    BlobFate::Cropped { into, .. } => Some(into.id),
                    BlobFate::Deleted { .. } => None,
                };
                let held = into.is_some() && blob_held(&tx, blob)?;
                put_fate(&tx, seq, blob, &fate)?;
                if let (true, Some(into)) = (held, into) {
                    local_crops.push(PendingCrop { into, from: blob });
                } else {
                    for gone in purge_blob(&tx, blob)? {
                        blobs.retain(|id| *id != gone);
                        purged.push(gone);
                    }
                }
            }
            for blob in blob_refs(&event.kind) {
                if register_blob(&tx, blob)? {
                    blobs.push(blob.id);
                }
            }
            if let Some(turn) = TurnRecord::of(seq, event) {
                tx.execute(
                    "INSERT INTO turns (seq, session_id, at, usage) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        seq_sql(seq)?,
                        turn.session_id.0.to_string(),
                        turn.at.to_rfc3339(),
                        to_json(&turn.usage)?
                    ],
                )?;
            }
            new_events += 1;
        }
        for summary in touched.values() {
            write_session(&tx, summary)?;
            tx.execute(
                "DELETE FROM alerted_approvals WHERE session_id = ?1
                    AND id NOT IN (SELECT value FROM json_each(?2))",
                params![
                    summary.id.0.to_string(),
                    to_json(&summary.pending_approvals)?
                ],
            )?;
        }
        if new_cursor != cursor {
            tx.execute(
                "UPDATE cursor SET seq = ?1 WHERE id = 0",
                [seq_sql(new_cursor)?],
            )?;
        }
        tx.commit()?;
        // A later event of the batch may have purged a crop's `into`, and
        // with it the crop's blob.
        local_crops.retain(|crop| !purged.contains(&crop.into) && !purged.contains(&crop.from));
        blobs.retain(|id| !local_crops.iter().any(|crop| crop.into == *id));
        Ok(Applied {
            cursor: new_cursor,
            new_events,
            sessions: touched.into_keys().collect(),
            blobs,
            purged,
            local_crops,
            settled,
        })
    }

    /// All session summaries, most recently written to first.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY last_message_seq DESC"
        ))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn session(&self, id: SessionId) -> Result<Option<SessionSummary>, StoreError> {
        read_session(&self.conn, id)
    }

    /// Deletes a session from this device only, in one transaction: its
    /// events, summary, blob rows, draft and the draft's images (the image
    /// files stay), read mark and alerted approvals,
    /// its stored workdir answers, and the answered
    /// commands naming it. A tombstone keeps it out of the replica for good
    /// ([`apply_batch`](Store::apply_batch), [`put_workdir`](Store::put_workdir)).
    /// Returns the blobs its events referenced and the workdir blobs no
    /// other session's answer still names, whose files the caller removes.
    /// Whether the session may go (archived, nothing pending) is the
    /// caller's rule. An event that no longer decodes does not hold the
    /// session back: it is logged and deleted with the rest, and a blob
    /// only it named keeps its row until a reset.
    pub fn delete_session_locally(
        &mut self,
        id: SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<BlobId>, StoreError> {
        let tx = self.conn.transaction()?;
        let session = id.0.to_string();
        // The blobs table has no session column: the events say which.
        let mut blobs = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT seq, event FROM events WHERE session_id = ?1")?;
            let mut rows = stmt.query([&session])?;
            while let Some(row) = rows.next()? {
                match json_col::<Event>(row, 1) {
                    Ok(event) => blobs.extend(blob_refs(&event.kind).map(|blob| blob.id)),
                    Err(error) => {
                        let seq: i64 = row.get(0)?;
                        tracing::warn!(seq, %error, "deleting an event that does not decode");
                    }
                }
            }
        }
        for blob in &blobs {
            tx.execute("DELETE FROM blobs WHERE id = ?1", [blob.0.to_string()])?;
            // The fates of its blobs: its events named every one, a crop's
            // `into` included.
            tx.execute(
                "DELETE FROM blob_fates WHERE blob_id = ?1",
                [blob.0.to_string()],
            )?;
        }
        // Its workdir answers' blobs go only if no other session's names them.
        let workdir_blobs = {
            let mut stmt =
                tx.prepare("SELECT DISTINCT blob_id FROM workdir_blob_refs WHERE session_id = ?1")?;
            let rows = stmt.query_map([&session], |row| Ok(BlobId(uuid_col(row, 0)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        tx.execute(
            "DELETE FROM workdir_blob_refs WHERE session_id = ?1",
            [&session],
        )?;
        tx.execute(
            "DELETE FROM workdir_snapshots WHERE session_id = ?1",
            [&session],
        )?;
        blobs.extend(purge_workdir_blobs(&tx, &workdir_blobs)?);
        blobs.sort();
        blobs.dedup();
        tx.execute("DELETE FROM events WHERE session_id = ?1", [&session])?;
        tx.execute("DELETE FROM turns WHERE session_id = ?1", [&session])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [&session])?;
        tx.execute("DELETE FROM drafts WHERE session_id = ?1", [&session])?;
        tx.execute(
            "DELETE FROM draft_attachments WHERE session_id = ?1",
            [&session],
        )?;
        tx.execute(
            "DELETE FROM composer_modes WHERE session_id = ?1",
            [&session],
        )?;
        tx.execute("DELETE FROM read_marks WHERE session_id = ?1", [&session])?;
        tx.execute(
            "DELETE FROM alerted_approvals WHERE session_id = ?1",
            [&session],
        )?;
        tx.execute(
            "DELETE FROM outbox WHERE result IS NOT NULL
                AND json_extract(command, '$.kind.session_id') = ?1",
            [&session],
        )?;
        tx.execute(
            "INSERT INTO deleted_sessions (session_id, at) VALUES (?1, ?2)
             ON CONFLICT (session_id) DO NOTHING",
            params![session, now.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(blobs)
    }

    /// Makes the replica a fresh one for another server, in one
    /// transaction: the event log, session summaries, cursor, outbox,
    /// stored workdir answers and blob rows the old server produced go, as
    /// do its device names, offered models and everything its `hello` said
    /// ([`ServerFacts`]), and so does what this device kept beside them —
    /// drafts and their images, composer modes, read marks, locally deleted
    /// sessions and alerted approvals. The settings — the blob policy, a
    /// preference of this device, and keys a newer build wrote — stay, as do
    /// snippets, this device's too. Returns the blobs that were known, whose
    /// files the caller removes.
    pub fn reset_for_server(&mut self) -> Result<Vec<BlobId>, StoreError> {
        let tx = self.conn.transaction()?;
        let blobs = {
            let mut stmt = tx.prepare("SELECT id FROM blobs ORDER BY id")?;
            let rows = stmt.query_map([], |row| Ok(BlobId(uuid_col(row, 0)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        clear_replica(&tx)?;
        tx.execute_batch(
            "DELETE FROM drafts;
             DELETE FROM draft_attachments;
             DELETE FROM composer_modes;
             DELETE FROM read_marks;
             DELETE FROM deleted_sessions;
             DELETE FROM alerted_approvals;",
        )?;
        tx.commit()?;
        Ok(blobs)
    }

    /// Every stored turn's usage, in log order.
    pub fn turns(&self) -> Result<Vec<TurnRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, session_id, at, usage FROM turns ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            Ok(TurnRecord {
                seq: seq_col(row, 0)?,
                session_id: SessionId(uuid_col(row, 1)?),
                at: time_col(row, 2)?,
                usage: json_col(row, 3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The session's events in log order.
    pub fn session_events(&self, id: SessionId) -> Result<Vec<(Seq, Event)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, event FROM events WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([id.0.to_string()], |row| {
            Ok((seq_col(row, 0)?, json_col(row, 1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- outbox ---------------------------------------------------------

    /// Persists a command as `Queued`. Enqueuing a [`CommandId`] that is
    /// already in the outbox changes nothing.
    ///
    /// A message also deletes its session's agent draft and the draft's
    /// images (their rows and files stay until nothing needs them, see
    /// [`Store::collect_attachments`]), and a
    /// shell run its shell draft, in the same transaction: a crash never
    /// leaves both the queued command and the draft it was written in, which
    /// would invite sending it twice.
    pub fn enqueue(&self, command: &Command) -> Result<(), StoreError> {
        // Unchecked only because `&self`: no other transaction is open here.
        let tx = self.conn.unchecked_transaction()?;
        let written = match &command.kind {
            CommandKind::SendMessage { session_id, .. } => Some((session_id, ComposerMode::Agent)),
            CommandKind::RunShell { session_id, .. } => Some((session_id, ComposerMode::Shell)),
            _ => None,
        };
        if let Some((session_id, mode)) = written {
            tx.execute(
                "DELETE FROM drafts WHERE session_id = ?1 AND mode = ?2",
                params![session_id.0.to_string(), mode_sql(mode)],
            )?;
            if mode == ComposerMode::Agent {
                tx.execute(
                    "DELETE FROM draft_attachments WHERE session_id = ?1",
                    [session_id.0.to_string()],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO outbox (command_id, command) VALUES (?1, ?2)
             ON CONFLICT (command_id) DO NOTHING",
            params![command.id.0.to_string(), to_json(command)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every outbox entry, oldest first, including finished ones.
    pub fn outbox(&self) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox("")
    }

    /// Entries still awaiting a result, oldest first: the send order.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox("WHERE result IS NULL")
    }

    fn query_outbox(&self, filter: &str) -> Result<Vec<OutboxEntry>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT command, sending, result, tries, first_sent_at, answered_at
             FROM outbox {filter} ORDER BY position"
        ))?;
        let rows = stmt.query_map([], |row| {
            let command = json_col(row, 0)?;
            let sending: bool = row.get(1)?;
            let result: Option<Settled> = opt_json_col(row, 2)?;
            let status = match (result, sending) {
                (Some(Settled::Answered(result)), _) => OutboxStatus::Done(result),
                (Some(Settled::Local(LocalResult::Undeliverable)), _) => {
                    OutboxStatus::Undeliverable
                }
                (Some(Settled::Local(LocalResult::AttachmentRefused { reason })), _) => {
                    OutboxStatus::AttachmentRefused(reason)
                }
                (None, true) => OutboxStatus::Sending,
                (None, false) => OutboxStatus::Queued,
            };
            Ok(OutboxEntry {
                command,
                status,
                tries: row.get(3)?,
                first_sent_at: opt_time_col(row, 4)?,
                answered_at: opt_time_col(row, 5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Claims a pending command for sending at `now`: marks it `Sending`,
    /// counts the try and keeps the first send time. The claim is durable
    /// when this returns (autocommit on a `synchronous = FULL` connection),
    /// so the server never sees a command whose `tries` is 0 on disk.
    /// `false` when there is nothing to send: the command is not in the
    /// outbox or already answered.
    pub fn claim_send(&self, id: CommandId, now: DateTime<Utc>) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE outbox SET sending = 1, tries = tries + 1,
                 first_sent_at = COALESCE(first_sent_at, ?2)
             WHERE command_id = ?1 AND result IS NULL",
            params![id.0.to_string(), now.to_rfc3339()],
        )?;
        Ok(changed == 1)
    }

    /// Records the server's answer, received at `now`. The first recorded
    /// result is kept; the server returns the same one for every resend
    /// anyway. `false` when nothing changed: the command was already
    /// settled, or is not in the outbox.
    pub fn record_result(
        &self,
        id: CommandId,
        result: &CommandResult,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        settle_in(&self.conn, id, &Settled::Answered(result.clone()), now)
    }

    /// Gives up on a command the server refused without an answer, at
    /// `now` ([`OutboxStatus::Undeliverable`]). Like an answer, it is final
    /// and the first one is kept; `false` when nothing changed, as for
    /// [`record_result`](Store::record_result).
    pub fn record_undeliverable(
        &self,
        id: CommandId,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        settle_in(
            &self.conn,
            id,
            &Settled::Local(LocalResult::Undeliverable),
            now,
        )
    }

    /// Gives up on a message whose image the outbox could not stage, at
    /// `now` ([`OutboxStatus::AttachmentRefused`]). Like an answer, it is
    /// final and the first one is kept; `false` when nothing changed, as for
    /// [`record_result`](Store::record_result).
    pub fn record_attachment_refused(
        &self,
        id: CommandId,
        reason: AttachmentRefusal,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        settle_in(
            &self.conn,
            id,
            &Settled::Local(LocalResult::AttachmentRefused { reason }),
            now,
        )
    }

    /// Takes a command back: deletes it from the outbox if the server has
    /// not answered it or refused it (or its request), and keeps `draft` as
    /// its session's draft in that mode. With `images`, the session's agent
    /// draft images become these hashes, in order, too. All of it is one
    /// transaction, so a crash keeps the text and the images in exactly one
    /// place. An accepted command stays and the call fails with
    /// [`StoreError::NotWithdrawable`], writing nothing. One already sent may
    /// still reach the server ([`OutboxEntry::discardable`]).
    pub fn withdraw(
        &self,
        id: CommandId,
        draft: Option<((SessionId, ComposerMode), &Draft)>,
        images: Option<(SessionId, &[ContentHash])>,
    ) -> Result<(), StoreError> {
        // Unchecked only because `&self`: no other transaction is open here.
        let tx = self.conn.unchecked_transaction()?;
        let deleted = tx.execute(
            "DELETE FROM outbox WHERE command_id = ?1
                AND (result IS NULL
                    OR json_extract(result, '$.result')
                        IN ('rejected', 'undeliverable', 'attachment_refused'))",
            [id.0.to_string()],
        )?;
        if deleted == 0 {
            return Err(StoreError::NotWithdrawable(id));
        }
        if let Some((key, draft)) = draft {
            upsert_draft(&tx, key, draft)?;
        }
        if let Some((session_id, hashes)) = images {
            let session = session_id.0.to_string();
            tx.execute(
                "DELETE FROM draft_attachments WHERE session_id = ?1 AND mode = 'agent'",
                [&session],
            )?;
            for (position, hash) in hashes.iter().enumerate() {
                tx.execute(
                    "INSERT INTO draft_attachments (session_id, mode, position, sha256)
                     VALUES (?1, 'agent', ?2, ?3)",
                    params![session, u64_sql(position as u64)?, hash.to_hex()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- blobs ------------------------------------------------------------

    /// Every blob the replica knows of, event and workdir ones alike: this
    /// is the model's blob map, so nothing is left out.
    pub fn blobs(&self) -> Result<Vec<BlobRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {BLOB_COLUMNS} FROM blobs ORDER BY id"))?;
        let rows = stmt.query_map([], blob_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// What the server did to blobs, by the blob each fate names: folded
    /// from the log's `BlobDeleted` and `BlobCropped`
    /// ([`apply_batch`](Store::apply_batch)).
    pub fn blob_fates(&self) -> Result<HashMap<BlobId, BlobFate>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT blob_id, fate FROM blob_fates")?;
        let rows = stmt.query_map([], |row| Ok((BlobId(uuid_col(row, 0)?), json_col(row, 1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// What the server did to `blob`, if anything.
    pub fn blob_fate(&self, blob: BlobId) -> Result<Option<BlobFate>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT fate FROM blob_fates WHERE blob_id = ?1",
                [blob.0.to_string()],
                |row| json_col(row, 0),
            )
            .optional()?)
    }

    /// The crops this device still makes itself, in log order: a `Cropped`
    /// fate whose blob still has a row ([`apply_batch`](Store::apply_batch)).
    pub fn pending_crops(&self) -> Result<Vec<PendingCrop>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT from_id, into_id FROM ({PENDING_CROPS}) ORDER BY seq"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingCrop {
                from: BlobId(uuid_col(row, 0)?),
                into: BlobId(uuid_col(row, 1)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The pending crop that makes `into`, if any.
    pub fn pending_crop_into(&self, into: BlobId) -> Result<Option<PendingCrop>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT from_id FROM ({PENDING_CROPS}) WHERE into_id = ?1"),
                [into.0.to_string()],
                |row| {
                    Ok(PendingCrop {
                        into,
                        from: BlobId(uuid_col(row, 0)?),
                    })
                },
            )
            .optional()?)
    }

    /// The device made `crop`'s `into` or gave up on it: the blob it was
    /// made from goes. Returns the blobs whose rows went; none if the crop
    /// was no longer pending.
    pub fn end_local_crop(&mut self, crop: PendingCrop) -> Result<Vec<BlobId>, StoreError> {
        let tx = self.conn.transaction()?;
        let pending: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM ({PENDING_CROPS})
                     WHERE from_id = ?1 AND into_id = ?2)"
            ),
            [crop.from.0.to_string(), crop.into.0.to_string()],
            |row| row.get(0),
        )?;
        let purged = if pending {
            purge_blob(&tx, crop.from)?
        } else {
            Vec::new()
        };
        tx.commit()?;
        Ok(purged)
    }

    pub fn blob(&self, id: BlobId) -> Result<Option<BlobRecord>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE id = ?1"),
                [id.0.to_string()],
                blob_from_row,
            )
            .optional()?)
    }

    /// Writes `status` as `id`'s; a failure's reason goes to
    /// `blob_failures`, and any other status clears it, in one transaction.
    pub fn set_blob_status(&self, id: BlobId, status: &BlobStatus) -> Result<(), StoreError> {
        let (state, got, path) = match status {
            BlobStatus::NotDownloaded | BlobStatus::Failed { .. } => ("not_downloaded", None, None),
            BlobStatus::Downloading { got, path } => {
                ("downloading", Some(u64_sql(*got)?), Some(path_sql(path)?))
            }
            BlobStatus::Stopped { got, path } => (
                "not_downloaded",
                Some(u64_sql(*got)?),
                Some(path_sql(path)?),
            ),
            BlobStatus::Available { path } => ("available", None, Some(path_sql(path)?)),
        };
        let id_sql = id.0.to_string();
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE blobs SET state = ?2, got = ?3, path = ?4 WHERE id = ?1",
            params![id_sql, state, got, path],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBlob(id));
        }
        tx.execute("DELETE FROM blob_failures WHERE blob_id = ?1", [&id_sql])?;
        if let BlobStatus::Failed { reason } = status {
            tx.execute(
                "INSERT INTO blob_failures (blob_id, reason) VALUES (?1, ?2)",
                params![id_sql, reason],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Of these blobs, the ones not downloaded yet whose content is an image
    /// this device attached (its hash is an `attachments` row), with that
    /// image's hash: their bytes are on this device already.
    pub fn own_images(&self, ids: &[BlobId]) -> Result<Vec<(BlobId, ContentHash)>, StoreError> {
        let ids: Vec<String> = ids.iter().map(|id| id.0.to_string()).collect();
        let mut stmt = self.conn.prepare(
            "SELECT b.id, h.sha256 FROM blobs b
                JOIN blob_hashes h ON h.blob_id = b.id
                JOIN attachments a ON a.sha256 = h.sha256
             WHERE b.state = 'not_downloaded' AND b.id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt.query_map([to_json(&ids)?], |row| {
            let sha256: String = row.get(1)?;
            Ok((
                BlobId(uuid_col(row, 0)?),
                sha256.parse().map_err(|e| bad_value(1, Type::Text, e))?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- workdir answers ------------------------------------------------------

    /// Keeps `answer` as the latest to `ask`, in one transaction: it replaces
    /// the stored answer and the blobs it names, registers the blobs the
    /// replica did not know as [`BlobStatus::NotDownloaded`], and deletes
    /// the rows of the blobs the replaced answer named that no stored answer
    /// (any session, any read) names any more.
    ///
    /// Stores whatever it is given; which answers are worth keeping (not a
    /// `ChangesResult::Failed`) is the caller's rule. Writes nothing and
    /// returns `None` when `ask`'s session is not in the replica: no
    /// summary, or deleted from this device (an answer that was in flight
    /// across [`delete_session_locally`](Store::delete_session_locally) or
    /// [`reset_for_server`](Store::reset_for_server)).
    ///
    /// The registered blobs are not event blobs: they must not go through
    /// the blob queue's `registered`, which downloads eagerly.
    pub fn put_workdir(
        &mut self,
        ask: &WorkdirAsk,
        answer: &WorkdirAnswer,
    ) -> Result<Option<WorkdirStored>, StoreError> {
        if !answer.answers(ask) {
            return Err(StoreError::WorkdirMismatch);
        }
        let tx = self.conn.transaction()?;
        let session = ask.session().0.to_string();
        let known: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1)
                AND NOT EXISTS (SELECT 1 FROM deleted_sessions WHERE session_id = ?1)",
            [&session],
            |row| row.get(0),
        )?;
        if !known {
            return Ok(None);
        }
        let key = params![session, ask.kind(), ask.path()];
        let result = match answer {
            WorkdirAnswer::Changes(result) => to_json(result)?,
            WorkdirAnswer::List(result) => to_json(result)?,
            WorkdirAnswer::File(result) => to_json(result)?,
            WorkdirAnswer::Log(result) => to_json(result)?,
        };
        tx.execute(
            "INSERT INTO workdir_snapshots (session_id, kind, path, result)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, kind, path) DO UPDATE SET result = excluded.result",
            params![session, ask.kind(), ask.path(), result],
        )?;
        let before: HashSet<BlobId> = {
            let mut stmt = tx.prepare(
                "SELECT blob_id FROM workdir_blob_refs
                 WHERE session_id = ?1 AND kind = ?2 AND path = ?3",
            )?;
            let rows = stmt.query_map(key, |row| Ok(BlobId(uuid_col(row, 0)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        tx.execute(
            "DELETE FROM workdir_blob_refs WHERE session_id = ?1 AND kind = ?2 AND path = ?3",
            key,
        )?;
        let refs = answer.blob_refs();
        let mut stored = WorkdirStored::default();
        for blob in &refs {
            let id = blob.id.0.to_string();
            tx.execute(
                "INSERT INTO workdir_blob_refs (session_id, kind, path, blob_id)
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                params![session, ask.kind(), ask.path(), id],
            )?;
            if register_blob(&tx, blob)? && !stored.registered.contains(&blob.id) {
                stored.registered.push(blob.id);
            }
        }
        let mut dropped: Vec<BlobId> = before
            .into_iter()
            .filter(|id| !refs.iter().any(|blob| blob.id == *id))
            .collect();
        dropped.sort();
        stored.purged = purge_workdir_blobs(&tx, &dropped)?;
        tx.commit()?;
        Ok(Some(stored))
    }

    /// Keeps `result` as the latest answer to the usage limits read,
    /// replacing the one before. Stores whatever it is given; which
    /// answers are worth keeping (not a `Failed`) is the caller's rule.
    pub fn put_usage_limits(&mut self, result: &UsageLimitsResult) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO usage_limits (id, result) VALUES (0, ?1)
             ON CONFLICT (id) DO UPDATE SET result = excluded.result",
            [to_json(result)?],
        )?;
        Ok(())
    }

    /// The latest stored answer to the usage limits read; `None` until one
    /// is stored.
    pub fn usage_limits(&self) -> Result<Option<UsageLimitsResult>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT result FROM usage_limits WHERE id = 0", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json.as_deref().map(serde_json::from_str).transpose()?)
    }

    /// The latest stored answer to each workdir read.
    pub fn workdirs(&self) -> Result<HashMap<WorkdirAsk, WorkdirAnswer>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, kind, path, result FROM workdir_snapshots")?;
        let rows = stmt.query_map([], |row| {
            let session = SessionId(uuid_col(row, 0)?);
            let kind: String = row.get(1)?;
            let path: String = row.get(2)?;
            let ask = WorkdirAsk::from_parts(session, &kind, path)
                .ok_or_else(|| bad_value(1, Type::Text, format!("workdir kind {kind:?}")))?;
            let answer = match ask {
                WorkdirAsk::Changes { .. } => WorkdirAnswer::Changes(json_col(row, 3)?),
                WorkdirAsk::List { .. } => WorkdirAnswer::List(json_col(row, 3)?),
                WorkdirAsk::File { .. } => WorkdirAnswer::File(json_col(row, 3)?),
                WorkdirAsk::Log { .. } => WorkdirAnswer::Log(json_col(row, 3)?),
            };
            Ok((ask, answer))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- drafts -------------------------------------------------------------

    /// This device's unsent messages and shell commands, by session and
    /// composer mode.
    pub fn drafts(&self) -> Result<HashMap<(SessionId, ComposerMode), Draft>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(r#"SELECT session_id, mode, text, cursor, "when" FROM drafts"#)?;
        let rows = stmt.query_map([], |row| {
            let cursor: i64 = row.get(3)?;
            let draft = Draft {
                text: row.get(2)?,
                cursor: usize::try_from(cursor).map_err(|e| bad_value(3, Type::Integer, e))?,
                when: when_col(row, 4)?,
            };
            Ok(((SessionId(uuid_col(row, 0)?), mode_col(row, 1)?), draft))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps `draft` as the session's unsent text in `mode`; `None` deletes
    /// it. The images of an agent draft stay either way
    /// ([`Store::attach_to_draft`]).
    pub fn set_draft(
        &self,
        session_id: SessionId,
        mode: ComposerMode,
        draft: Option<&Draft>,
    ) -> Result<(), StoreError> {
        match draft {
            Some(draft) => upsert_draft(&self.conn, (session_id, mode), draft),
            None => {
                self.conn.execute(
                    "DELETE FROM drafts WHERE session_id = ?1 AND mode = ?2",
                    params![session_id.0.to_string(), mode_sql(mode)],
                )?;
                Ok(())
            }
        }
    }

    /// The images each session's agent draft holds, in the order they were
    /// attached.
    pub fn draft_images(&self) -> Result<HashMap<SessionId, Vec<AttachedImage>>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT d.session_id, a.sha256, a.media_type, a.size, a.width, a.height
             FROM draft_attachments d JOIN attachments a ON a.sha256 = d.sha256
             WHERE d.mode = 'agent' ORDER BY d.session_id, d.position",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((SessionId(uuid_col(row, 0)?), attached_image_cols(row, 1)?))
        })?;
        let mut images: HashMap<SessionId, Vec<AttachedImage>> = HashMap::new();
        for row in rows {
            let (session_id, image) = row?;
            images.entry(session_id).or_default().push(image);
        }
        Ok(images)
    }

    /// Every image row, by sha256. A row outlives its draft: a queued
    /// message names its images by hash, so the model can show them again.
    pub fn attachments(&self) -> Result<HashMap<ContentHash, AttachedImage>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT sha256, media_type, size, width, height FROM attachments")?;
        let rows = stmt.query_map([], |row| attached_image_cols(row, 0))?;
        let mut images = HashMap::new();
        for row in rows {
            let image = row?;
            images.insert(image.sha256, image);
        }
        Ok(images)
    }

    /// Attaches `image` to the session's agent draft, after its images, in
    /// one transaction. A draft holds an image at most once: one it already
    /// holds changes nothing. The image's row is made on first sight, at
    /// `now`; its file is the caller's to store (`attachment::store`).
    pub fn attach_to_draft(
        &self,
        session_id: SessionId,
        image: &AttachedImage,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        // Unchecked only because `&self`: no other transaction is open here.
        let tx = self.conn.unchecked_transaction()?;
        let sha256 = image.sha256.to_hex();
        tx.execute(
            "INSERT INTO attachments (sha256, media_type, size, width, height, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (sha256) DO NOTHING",
            params![
                sha256,
                image_type_sql(image.media_type),
                u64_sql(image.size)?,
                i64::from(image.width),
                i64::from(image.height),
                now.to_rfc3339()
            ],
        )?;
        let session = session_id.0.to_string();
        let held: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM draft_attachments
                WHERE session_id = ?1 AND mode = 'agent' AND sha256 = ?2)",
            params![session, sha256],
            |row| row.get(0),
        )?;
        if !held {
            tx.execute(
                "INSERT INTO draft_attachments (session_id, mode, position, sha256)
                 VALUES (?1, 'agent',
                    (SELECT COALESCE(MAX(position), -1) + 1 FROM draft_attachments
                        WHERE session_id = ?1 AND mode = 'agent'),
                    ?2)",
                params![session, sha256],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Takes the image with `sha256` off the session's agent draft. The
    /// other images keep their positions, and the image's row and file stay.
    pub fn detach_from_draft(
        &self,
        session_id: SessionId,
        sha256: ContentHash,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM draft_attachments
             WHERE session_id = ?1 AND mode = 'agent' AND sha256 = ?2",
            params![session_id.0.to_string(), sha256.to_hex()],
        )?;
        Ok(())
    }

    /// Deletes, in one transaction, the rows of the images nothing needs any
    /// more, and returns their hashes; their files are the caller's to remove.
    /// An image is still needed while one of these holds:
    /// - a draft holds it: it is one of the draft's images;
    /// - a message naming it is unsettled (`result` NULL): it may still be
    ///   posted;
    /// - a message naming it was rejected, undeliverable or refused its
    ///   attachment: Edit puts its images back into the draft;
    /// - a message naming it was accepted at a `seq` past the cursor: the
    ///   reply can settle it before the pull applies its event, and the
    ///   event's own-copy step needs the file.
    ///
    /// Nothing else keeps an image, so an image no longer needed is collected
    /// when the store opens, not by the detach or settle that freed it.
    pub fn collect_attachments(&self) -> Result<Vec<ContentHash>, StoreError> {
        // Unchecked only because `&self`: no other transaction is open here.
        let tx = self.conn.unchecked_transaction()?;
        let hashes = {
            let mut stmt = tx.prepare(
                "SELECT a.sha256 FROM attachments a
                 WHERE NOT EXISTS (SELECT 1 FROM draft_attachments d WHERE d.sha256 = a.sha256)
                   AND NOT EXISTS (
                     SELECT 1 FROM outbox o, json_each(o.command, '$.kind.attachments') j
                     WHERE j.value = a.sha256
                       AND (o.result IS NULL
                         OR json_extract(o.result, '$.result')
                             IN ('rejected', 'undeliverable', 'attachment_refused')
                         OR (json_extract(o.result, '$.result') = 'accepted'
                             AND json_extract(o.result, '$.seq')
                                 > (SELECT seq FROM cursor WHERE id = 0))))",
            )?;
            let rows = stmt.query_map([], |row| {
                let sha256: String = row.get(0)?;
                sha256.parse().map_err(|e| bad_value(0, Type::Text, e))
            })?;
            rows.collect::<rusqlite::Result<Vec<ContentHash>>>()?
        };
        for hash in &hashes {
            tx.execute("DELETE FROM attachments WHERE sha256 = ?1", [hash.to_hex()])?;
        }
        tx.commit()?;
        Ok(hashes)
    }

    /// At open, with the data dir's lock held: removes the files of the
    /// images [`Store::collect_attachments`] frees, then every file without
    /// a row left, the `.tmp-*` files included. Only at open: a row-less
    /// file may be a write whose row is still to come. A failure is logged;
    /// the sweep runs only when the rows could be read, as they are its keep
    /// set.
    fn collect_image_files(&self, data_dir: &Path) {
        match self.collect_attachments() {
            Ok(collected) => {
                for hash in collected {
                    attachment::remove(data_dir, hash);
                }
            }
            Err(error) => tracing::warn!(%error, "cannot collect the replica's images"),
        }
        match self.attachments() {
            Ok(rows) => {
                let keep = rows.into_keys().collect::<HashSet<_>>();
                attachment::sweep(data_dir, &keep);
            }
            Err(error) => tracing::warn!(%error, "cannot read the replica's images"),
        }
    }

    // ---- composer modes -------------------------------------------------------

    /// What each session's composer writes, for the sessions whose is not
    /// [`ComposerMode::Agent`].
    pub fn composer_modes(&self) -> Result<HashMap<SessionId, ComposerMode>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, mode FROM composer_modes")?;
        let rows = stmt.query_map([], |row| {
            Ok((SessionId(uuid_col(row, 0)?), mode_col(row, 1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps what the session's composer writes.
    pub fn set_composer_mode(
        &self,
        session_id: SessionId,
        mode: ComposerMode,
    ) -> Result<(), StoreError> {
        let session = session_id.0.to_string();
        match mode {
            // No row is the agent.
            ComposerMode::Agent => self.conn.execute(
                "DELETE FROM composer_modes WHERE session_id = ?1",
                [session],
            )?,
            ComposerMode::Shell => self.conn.execute(
                "INSERT INTO composer_modes (session_id, mode) VALUES (?1, ?2)
                 ON CONFLICT (session_id) DO UPDATE SET mode = excluded.mode",
                params![session, mode_sql(mode)],
            )?,
        };
        Ok(())
    }

    // ---- snippets -----------------------------------------------------------

    /// This device's saved prompts, in no order.
    pub fn snippets(&self) -> Result<Vec<Snippet>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, text, used, kind FROM snippets")?;
        let rows = stmt.query_map([], |row| {
            Ok(Snippet {
                id: SnippetId(uuid_col(row, 0)?),
                name: row.get(1)?,
                text: row.get(2)?,
                used: u64_col(row, 3)?,
                kind: mode_col(row, 4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps the snippet, replacing the one with its id.
    pub fn set_snippet(&self, snippet: &Snippet) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO snippets (id, name, text, used, kind) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET
                name = excluded.name, text = excluded.text, used = excluded.used,
                kind = excluded.kind",
            params![
                snippet.id.0.to_string(),
                snippet.name,
                snippet.text,
                u64_sql(snippet.used)?,
                mode_sql(snippet.kind)
            ],
        )?;
        Ok(())
    }

    pub fn delete_snippet(&self, id: SnippetId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM snippets WHERE id = ?1", [id.0.to_string()])?;
        Ok(())
    }

    // ---- read marks -------------------------------------------------------

    /// How far this device has read each session.
    pub fn read_marks(&self) -> Result<HashMap<SessionId, ReadMark>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, seq, notable FROM read_marks")?;
        let rows = stmt.query_map([], |row| {
            let mark = ReadMark {
                seq: seq_col(row, 1)?,
                notable: u64_col(row, 2)?,
            };
            Ok((SessionId(uuid_col(row, 0)?), mark))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps `mark` as how far the session is read.
    pub fn set_read_mark(&self, session_id: SessionId, mark: ReadMark) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO read_marks (session_id, seq, notable) VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id) DO UPDATE SET seq = excluded.seq, notable = excluded.notable",
            params![
                session_id.0.to_string(),
                seq_sql(mark.seq)?,
                u64_sql(mark.notable)?
            ],
        )?;
        Ok(())
    }

    // ---- alerted approvals ------------------------------------------------

    /// The pending approvals this device alerted on or judged no news.
    pub fn alerted(&self) -> Result<HashSet<ApprovalId>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id FROM alerted_approvals")?;
        let rows = stmt.query_map([], |row| Ok(ApprovalId(uuid_col(row, 0)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps that these approvals, pending in their sessions, need no
    /// alert any more, in one transaction. The row goes again once the
    /// approval stops pending ([`apply_batch`](Store::apply_batch)) or its
    /// session is deleted here.
    pub fn mark_alerted(
        &mut self,
        approvals: &[(SessionId, ApprovalId)],
    ) -> Result<(), StoreError> {
        if approvals.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for (session, approval) in approvals {
            tx.execute(
                "INSERT INTO alerted_approvals (id, session_id) VALUES (?1, ?2)
                 ON CONFLICT (id) DO NOTHING",
                params![approval.0.to_string(), session.0.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- device names ---------------------------------------------------------

    /// The server's devices by name.
    pub fn devices(&self) -> Result<HashMap<DeviceId, String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM devices")?;
        let rows = stmt.query_map([], |row| Ok((DeviceId(uuid_col(row, 0)?), row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Keeps the server's device names, in one transaction. A device the
    /// server no longer lists keeps its last known name: old events still
    /// name it.
    pub fn set_devices(&mut self, devices: &[DeviceInfo]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        for device in devices {
            tx.execute(
                "INSERT INTO devices (id, name) VALUES (?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name",
                params![device.id.0.to_string(), device.name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- settings -----------------------------------------------------------

    pub fn settings(&self) -> Result<Settings, StoreError> {
        let mut settings = Settings::default();
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            settings.read(&key, value)?;
        }
        settings.server = self
            .conn
            .query_row(
                "SELECT shell_enabled, bypass_allowed, login_expires, agent_login_offered,
                    agent_logged_out
                 FROM server_facts WHERE id = 1",
                [],
                |row| {
                    Ok(ServerFacts {
                        shell_enabled: row.get(0)?,
                        bypass_allowed: row.get(1)?,
                        login_expires: opt_time_col(row, 2)?,
                        agent_login_offered: row.get(3)?,
                        agent_logged_out: row.get(4)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        Ok(settings)
    }

    pub fn set_blob_policy(&self, policy: BlobPolicy) -> Result<(), StoreError> {
        let value = match policy {
            BlobPolicy::Eager => "eager",
            BlobPolicy::OnDemand => "on_demand",
        };
        self.put_setting(KEY_BLOB_POLICY, Some(value))
    }

    /// The server addresses an older build kept among the settings, before
    /// they moved into the pairing ([`Pairing`](crate::Pairing)); `None`
    /// without a current address that parses. Read leniently, as only the
    /// move to the pairing reads them: an address that does not parse is
    /// left out, with the reason logged.
    pub(crate) fn legacy_server_addresses(&self) -> Result<Option<ServerAddresses>, StoreError> {
        let stored = |key: &str| -> rusqlite::Result<Option<String>> {
            self.conn
                .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                    Ok(row.get_ref(0)?.as_str().ok().map(str::to_owned))
                })
                .optional()
                .map(Option::flatten)
        };
        let parse = |text: &str| {
            ServerAddress::parse(text)
                .inspect_err(|error| {
                    tracing::warn!(%error, "dropping a stored server address that does not parse");
                })
                .ok()
        };
        let Some(current) = stored(KEY_SERVER_ADDRESS)?.as_deref().and_then(parse) else {
            return Ok(None);
        };
        let others = stored(KEY_OTHER_ADDRESSES)?
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default();
        Ok(Some(ServerAddresses::with_others(
            current,
            others.iter().filter_map(|text| parse(text)),
        )))
    }

    /// Drops the settings rows [`legacy_server_addresses`](Store::legacy_server_addresses)
    /// reads, once the pairing holds what they said.
    pub(crate) fn drop_legacy_server_addresses(&self) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM settings WHERE key IN (?1, ?2)",
            [KEY_SERVER_ADDRESS, KEY_OTHER_ADDRESSES],
        )?;
        Ok(())
    }

    /// Keeps what the server's `hello` said of itself, replacing all it
    /// said before.
    pub fn set_server_facts(&self, facts: &ServerFacts) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO server_facts (id, shell_enabled, bypass_allowed, login_expires,
                 agent_login_offered, agent_logged_out)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET shell_enabled = excluded.shell_enabled,
                 bypass_allowed = excluded.bypass_allowed,
                 login_expires = excluded.login_expires,
                 agent_login_offered = excluded.agent_login_offered,
                 agent_logged_out = excluded.agent_logged_out",
            params![
                facts.shell_enabled,
                facts.bypass_allowed,
                facts.login_expires.map(|at| at.to_rfc3339()),
                facts.agent_login_offered,
                facts.agent_logged_out,
            ],
        )?;
        Ok(())
    }

    fn put_setting(&self, key: &str, value: Option<&str>) -> Result<(), StoreError> {
        put_setting_in(&self.conn, key, value)
    }
}

/// Writes the settings row `key`, deleting it when `value` is `None`.
fn put_setting_in(conn: &Connection, key: &str, value: Option<&str>) -> Result<(), StoreError> {
    match value {
        Some(value) => conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?,
        None => conn.execute("DELETE FROM settings WHERE key = ?1", [key])?,
    };
    Ok(())
}

/// The other addresses as an older build stored them: a JSON array of
/// canonical addresses, `None` (no row) when empty.
fn other_addresses_json(addresses: &[ServerAddress]) -> Option<String> {
    (!addresses.is_empty()).then(|| serde_json::to_string(addresses).expect("addresses serialize"))
}

const KEY_BLOB_POLICY: &str = "blob_policy";
/// Where an older build kept the current server address, before it moved
/// into the pairing ([`Store::legacy_server_addresses`]).
const KEY_SERVER_ADDRESS: &str = "server_address";
/// The server's other addresses as an older build kept them, a JSON array;
/// absent when empty.
const KEY_OTHER_ADDRESSES: &str = "other_server_addresses";

/// Opens the database in `data_dir`, creating both if needed, unmigrated.
fn connect(data_dir: &Path) -> Result<Connection, StoreError> {
    fs::create_dir_all(data_dir).map_err(|source| StoreError::DataDir {
        path: data_dir.to_path_buf(),
        source,
    })?;
    let conn = Connection::open(data_dir.join(DB_FILE))?;
    // rusqlite's default too; stated so it does not depend on that.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    // Every user action must survive power loss once dispatch returns.
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(conn)
}

/// Whether [`migrate`] migrates the stored wire JSON from the replica's
/// protocol stamp, refusing one it cannot migrate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Check,
    /// [`Store::reset_replica`], which restamps it once it is cleared.
    Ignore,
}

fn migrate(conn: &mut Connection, stamp: Stamp) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    migrate_in(&tx, stamp)?;
    tx.commit()?;
    Ok(())
}

/// [`migrate`] inside the caller's transaction, which commits it.
fn migrate_in(tx: &Transaction<'_>, stamp: Stamp) -> Result<(), StoreError> {
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema { found });
    }
    // Before any migration reads the stored JSON.
    let protocol = if stamp == Stamp::Check && found >= STAMPED_SINCE {
        match stamped_protocol(tx)? {
            Some(found) if (MIGRATED_SINCE..=PROTOCOL_VERSION).contains(&found) => Some(found),
            Some(found) => {
                return Err(StoreError::ProtocolMismatch {
                    found,
                    current: PROTOCOL_VERSION,
                });
            }
            None => return Err(StoreError::Corrupt("protocol_stamp has no row")),
        }
    } else {
        None
    };
    let start = usize::try_from(found).unwrap_or(0);
    for migration in &MIGRATIONS[start..] {
        match migration {
            Sql(script) => tx.execute_batch(script)?,
            Code(step) => step(tx)?,
        }
    }
    if let Some(protocol) = protocol {
        migrate_protocol(tx, protocol, &PROTOCOL_STEPS)?;
    }
    // A fresh replica, or one from before the stamp, gets this build's.
    tx.execute(
        "INSERT OR IGNORE INTO protocol_stamp (id, version) VALUES (1, ?1)",
        [PROTOCOL_VERSION],
    )?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Runs the `steps` from protocol version `stamp` on and restamps the
/// replica with the version they end at.
fn migrate_protocol(
    tx: &Transaction<'_>,
    stamp: u32,
    steps: &[ProtocolStep],
) -> Result<(), StoreError> {
    let first = (stamp - MIGRATED_SINCE) as usize;
    for (i, step) in steps.iter().enumerate().skip(first) {
        let from = MIGRATED_SINCE + i as u32;
        tracing::info!(from, to = from + 1, "migrating the replica's protocol data");
        step(tx)?;
    }
    tx.execute(
        "UPDATE protocol_stamp SET version = ?1 WHERE id = 1",
        [MIGRATED_SINCE + steps.len() as u32],
    )?;
    Ok(())
}

fn stamped_protocol(conn: &Connection) -> Result<Option<u32>, StoreError> {
    Ok(conn
        .query_row("SELECT version FROM protocol_stamp WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Deletes what the server's log produced: the events and what is folded
/// from them, the cursor, the outbox, blob rows, stored workdir answers,
/// the stored usage limits, offered models, the server's device names and
/// what its last `hello` said of itself ([`ServerFacts`]). What is this
/// device's alone is the caller's to keep or drop.
fn clear_replica(tx: &Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(
        "DELETE FROM events;
         DELETE FROM turns;
         DELETE FROM sessions;
         UPDATE cursor SET seq = 0 WHERE id = 0;
         DELETE FROM outbox;
         DELETE FROM blobs;
         DELETE FROM blob_fates;
         DELETE FROM devices;
         DELETE FROM offered_models;
         DELETE FROM workdir_snapshots;
         DELETE FROM workdir_blob_refs;
         DELETE FROM usage_limits;
         DELETE FROM server_log;
         DELETE FROM server_facts;",
    )?;
    Ok(())
}

/// [`Store::reset_replica`] in place: migrates the schema without checking
/// the stamp, then in one transaction clears the replica, drops the
/// settings rows this build cannot read and restamps it, then reads back
/// what was kept, so a reset that leaves it unreadable fails.
fn clear_replica_at(data_dir: &Path) -> Result<(), StoreError> {
    let mut conn = connect(data_dir)?;
    migrate(&mut conn, Stamp::Ignore)?;
    let tx = conn.transaction()?;
    clear_replica(&tx)?;
    let mut unreadable = Vec::new();
    for (rowid, setting) in setting_rows(&tx)? {
        if setting.is_none() {
            unreadable.push(rowid);
        }
    }
    for rowid in unreadable {
        tracing::warn!(rowid, "dropping a setting this build cannot read");
        tx.execute("DELETE FROM settings WHERE rowid = ?1", [rowid])?;
    }
    tx.execute(
        "INSERT INTO protocol_stamp (id, version) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET version = excluded.version",
        [PROTOCOL_VERSION],
    )?;
    tx.commit()?;
    Store::init(conn)?.load()?;
    Ok(())
}

/// A settings row's rowid, with its key and value if this build can read
/// them ([`Settings::readable`]).
type SettingRow = (i64, Option<(String, String)>);

/// Every settings row.
fn setting_rows(conn: &Connection) -> rusqlite::Result<Vec<SettingRow>> {
    let mut stmt = conn.prepare("SELECT rowid, key, value FROM settings")?;
    let rows = stmt.query_map([], |row| {
        let text = |idx| row.get_ref(idx).map(|v| v.as_str().ok().map(str::to_owned));
        let setting = match (text(1)?, text(2)?) {
            (Some(key), Some(value)) if Settings::readable(&key, &value) => Some((key, value)),
            _ => None,
        };
        Ok((row.get(0)?, setting))
    })?;
    rows.collect()
}

/// [`Store::reset_replica`] when the replica cannot be reset in place:
/// builds a new database beside it — schema, stamp and the old settings
/// this build can read, in one transaction — checks that it loads, and
/// renames it over [`DB_FILE`]. The old WAL files go just before the
/// rename, never after, so SQLite never replays old frames into the new
/// file; a crash before the rename leaves the old database, to reset again.
fn recreate_replica(data_dir: &Path) -> Result<(), StoreError> {
    let replace = |path: PathBuf| move |source| StoreError::Replace { path, source };
    let remove = |path: PathBuf| match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::Replace { path, source }),
    };
    let settings = salvage_settings(data_dir);
    let fresh = data_dir.join(FRESH_DB_FILE);
    // Left by a reset that crashed before its rename; its hot journal
    // first, which SQLite would otherwise roll back into the new file.
    remove(data_dir.join(FRESH_JOURNAL_FILE))?;
    remove(fresh.clone())?;
    {
        // The default rollback journal: the file stands alone once closed.
        let mut conn = Connection::open(&fresh)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = conn.transaction()?;
        migrate_in(&tx, Stamp::Check)?;
        for (key, value) in &settings {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Store { conn }.load()?;
    }
    for name in WAL_FILES {
        remove(data_dir.join(name))?;
    }
    let db = data_dir.join(DB_FILE);
    fs::rename(&fresh, &db).map_err(replace(db))?;
    Ok(())
}

/// The settings rows of the database in `data_dir` that this build can
/// read; the rest, or all if the table does not read, are lost.
fn salvage_settings(data_dir: &Path) -> Vec<(String, String)> {
    let read = || -> rusqlite::Result<Vec<SettingRow>> {
        setting_rows(&Connection::open(data_dir.join(DB_FILE))?)
    };
    match read() {
        Ok(rows) => {
            let lost = rows.iter().filter(|(_, s)| s.is_none()).count();
            if lost > 0 {
                tracing::warn!(lost, "dropping settings this build cannot read");
            }
            rows.into_iter()
                .filter_map(|(_, setting)| setting)
                .collect()
        }
        Err(error) => {
            tracing::warn!(%error, "cannot read the old settings; they are lost");
            Vec::new()
        }
    }
}

/// Settles the outbox command `id` as `settled` at `now`, unless it is
/// already settled; `false` when nothing changed, as also for a command not
/// in the outbox (another device's, or taken back).
fn settle_in(
    conn: &Connection,
    id: CommandId,
    settled: &Settled,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let changed = conn
        .prepare_cached(
            "UPDATE outbox SET result = ?2, sending = 0, answered_at = ?3
             WHERE command_id = ?1 AND result IS NULL",
        )?
        .execute(params![
            id.0.to_string(),
            to_json(settled)?,
            now.to_rfc3339()
        ])?;
    Ok(changed == 1)
}

fn read_cursor(conn: &Connection) -> Result<Seq, StoreError> {
    Ok(
        conn.query_row("SELECT seq FROM cursor WHERE id = 0", [], |row| {
            seq_col(row, 0)
        })?,
    )
}

fn read_deleted(conn: &Connection) -> Result<HashSet<SessionId>, StoreError> {
    let mut stmt = conn.prepare("SELECT session_id FROM deleted_sessions")?;
    let rows = stmt.query_map([], |row| Ok(SessionId(uuid_col(row, 0)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn upsert_draft(
    conn: &Connection,
    (session_id, mode): (SessionId, ComposerMode),
    draft: &Draft,
) -> Result<(), StoreError> {
    conn.execute(
        r#"INSERT INTO drafts (session_id, mode, text, cursor, "when") VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (session_id, mode) DO UPDATE SET
            text = excluded.text, cursor = excluded.cursor, "when" = excluded."when""#,
        params![
            session_id.0.to_string(),
            mode_sql(mode),
            draft.text,
            u64_sql(draft.cursor as u64)?,
            when_sql(draft.when)
        ],
    )?;
    Ok(())
}

fn mode_sql(mode: ComposerMode) -> &'static str {
    match mode {
        ComposerMode::Agent => "agent",
        ComposerMode::Shell => "shell",
    }
}

fn mode_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<ComposerMode> {
    let mode: String = row.get(idx)?;
    match mode.as_str() {
        "agent" => Ok(ComposerMode::Agent),
        "shell" => Ok(ComposerMode::Shell),
        _ => Err(bad_value(
            idx,
            Type::Text,
            format!("composer mode {mode:?}"),
        )),
    }
}

/// The serde name of `media_type`, as the `attachments` CHECK lists it.
fn image_type_sql(media_type: ImageType) -> &'static str {
    match media_type {
        ImageType::Png => "png",
        ImageType::Jpeg => "jpeg",
        ImageType::Gif => "gif",
        ImageType::Webp => "webp",
    }
}

fn image_type_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<ImageType> {
    let text: String = row.get(idx)?;
    match text.as_str() {
        "png" => Ok(ImageType::Png),
        "jpeg" => Ok(ImageType::Jpeg),
        "gif" => Ok(ImageType::Gif),
        "webp" => Ok(ImageType::Webp),
        _ => Err(bad_value(idx, Type::Text, format!("image type {text:?}"))),
    }
}

/// An [`AttachedImage`] from the five columns of `attachments` from `first`
/// on: sha256, media_type, size, width, height.
fn attached_image_cols(row: &Row<'_>, first: usize) -> rusqlite::Result<AttachedImage> {
    let sha256: String = row.get(first)?;
    Ok(AttachedImage {
        sha256: sha256
            .parse()
            .map_err(|e| bad_value(first, Type::Text, e))?,
        media_type: image_type_col(row, first + 1)?,
        size: u64_col(row, first + 2)?,
        width: u32_col(row, first + 3)?,
        height: u32_col(row, first + 4)?,
    })
}

fn when_sql(when: RunWhen) -> &'static str {
    match when {
        RunWhen::Always => "always",
        RunWhen::OnSuccess => "on_success",
        RunWhen::OnFailure => "on_failure",
    }
}

fn when_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<RunWhen> {
    let when: String = row.get(idx)?;
    match when.as_str() {
        "always" => Ok(RunWhen::Always),
        "on_success" => Ok(RunWhen::OnSuccess),
        "on_failure" => Ok(RunWhen::OnFailure),
        _ => Err(bad_value(
            idx,
            Type::Text,
            format!("run condition {when:?}"),
        )),
    }
}

const SESSION_COLUMNS: &str = "id, workdir, state, pending_approvals, last_activity, last_seq, \
    stopped, notable, name, first_prompt, archived, usage, agent, context, work, importing, adopted, \
    last_message_seq, worktree, worktree_removed";

fn read_session(conn: &Connection, id: SessionId) -> Result<Option<SessionSummary>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
            [id.0.to_string()],
            session_from_row,
        )
        .optional()?)
}

fn write_session(tx: &Transaction<'_>, summary: &SessionSummary) -> Result<(), StoreError> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
        ),
        params![
            summary.id.0.to_string(),
            summary.workdir,
            to_json(&summary.state)?,
            to_json(&summary.pending_approvals)?,
            summary.last_activity.to_rfc3339(),
            seq_sql(summary.last_seq)?,
            summary.stopped,
            u64_sql(summary.notable)?,
            summary.name,
            summary.first_prompt,
            summary.archived,
            summary.usage.as_ref().map(to_json).transpose()?,
            to_json(&summary.agent)?,
            summary.context.0.to_string(),
            to_json(&summary.work)?,
            summary.importing,
            summary.adopted,
            seq_sql(summary.last_message_seq)?,
            summary.worktree.as_ref().map(to_json).transpose()?,
            summary.worktree_removed,
        ],
    )?;
    Ok(())
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: SessionId(uuid_col(row, 0)?),
        workdir: row.get(1)?,
        state: json_col(row, 2)?,
        pending_approvals: json_col(row, 3)?,
        last_activity: time_col(row, 4)?,
        last_seq: seq_col(row, 5)?,
        stopped: row.get(6)?,
        notable: u64_col(row, 7)?,
        name: row.get(8)?,
        first_prompt: row.get(9)?,
        archived: row.get(10)?,
        usage: opt_json_col(row, 11)?,
        agent: opt_json_col(row, 12)?.unwrap_or_default(),
        context: ContextId(uuid_col(row, 13)?),
        work: opt_json_col(row, 14)?.unwrap_or_default(),
        importing: row.get(15)?,
        adopted: row.get(16)?,
        last_message_seq: seq_col(row, 17)?,
        worktree: opt_json_col(row, 18)?,
        worktree_removed: row.get(19)?,
    })
}

fn time_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let text: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|e| bad_value(idx, Type::Text, e))
}

fn opt_time_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let text: Option<String> = row.get(idx)?;
    text.map(|text| {
        DateTime::parse_from_rfc3339(&text)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|e| bad_value(idx, Type::Text, e))
    })
    .transpose()
}

/// Deletes the rows of the `candidates` no workdir answer names any more and
/// returns their ids. Not checked against the events: workdir blob ids are
/// content ids the server mints apart from event blobs' random ones (D36),
/// so no event names one.
fn purge_workdir_blobs(
    tx: &Transaction<'_>,
    candidates: &[BlobId],
) -> Result<Vec<BlobId>, StoreError> {
    let mut purged = Vec::new();
    for blob in candidates {
        let deleted = tx.execute(
            "DELETE FROM blobs WHERE id = ?1
                AND NOT EXISTS (SELECT 1 FROM workdir_blob_refs WHERE blob_id = ?1)",
            [blob.0.to_string()],
        )?;
        if deleted > 0 {
            purged.push(*blob);
        }
    }
    Ok(purged)
}

/// Keeps `fate`, decided at `seq`, as `blob`'s. The first fate stays: the
/// server decides one per blob.
fn put_fate(
    tx: &Transaction<'_>,
    seq: Seq,
    blob: BlobId,
    fate: &BlobFate,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO blob_fates (blob_id, fate, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT (blob_id) DO NOTHING",
        params![blob.0.to_string(), to_json(fate)?, seq_sql(seq)?],
    )?;
    Ok(())
}

/// The pending crops ([`PendingCrop`]), as `from_id`, `into_id` and their
/// fate's `seq`: a `Cropped` fate whose blob still has a row.
const PENDING_CROPS: &str = "SELECT f.blob_id AS from_id,
         json_extract(f.fate, '$.into.id') AS into_id, f.seq AS seq
     FROM blob_fates f JOIN blobs b ON b.id = f.blob_id
     WHERE json_extract(f.fate, '$.type') = 'cropped'";

/// Whether this device holds `blob`'s bytes, or will: its row is
/// `available`, or it is the `into` of a pending crop (whose own row is
/// there too).
fn blob_held(tx: &Transaction<'_>, blob: BlobId) -> Result<bool, StoreError> {
    Ok(tx.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM blobs WHERE id = ?1 AND state = 'available')
                 OR (EXISTS (SELECT 1 FROM blobs WHERE id = ?1)
                     AND EXISTS (SELECT 1 FROM ({PENDING_CROPS}) WHERE into_id = ?1))"
        ),
        [blob.0.to_string()],
        |row| row.get(0),
    )?)
}

/// Deletes `blob`'s row and, as nothing will produce `blob` any more, the
/// row of any blob a pending crop would have made it from (and so on down
/// a crop of a crop). Returns the blobs whose rows went.
fn purge_blob(tx: &Transaction<'_>, blob: BlobId) -> Result<Vec<BlobId>, StoreError> {
    let mut purged = Vec::new();
    let mut next = vec![blob];
    while let Some(blob) = next.pop() {
        let id = blob.0.to_string();
        let sources = {
            let mut stmt = tx.prepare(&format!(
                "SELECT from_id FROM ({PENDING_CROPS}) WHERE into_id = ?1"
            ))?;
            let rows = stmt.query_map([&id], |row| Ok(BlobId(uuid_col(row, 0)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if tx.execute("DELETE FROM blobs WHERE id = ?1", [&id])? > 0 {
            purged.push(blob);
        }
        next.extend(sources);
    }
    Ok(purged)
}

/// Registers `blob` as `NotDownloaded`, with its hash, unless a blob with
/// its id is known already or has a fate; whether it was new.
fn register_blob(tx: &Transaction<'_>, blob: &BlobRef) -> Result<bool, StoreError> {
    let id = blob.id.0.to_string();
    let inserted = tx.execute(
        "INSERT INTO blobs (id, size, state)
         SELECT ?1, ?2, 'not_downloaded'
         WHERE NOT EXISTS (SELECT 1 FROM blob_fates WHERE blob_id = ?1)
         ON CONFLICT (id) DO NOTHING",
        params![id, u64_sql(blob.size)?],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO blob_hashes (blob_id, sha256) VALUES (?1, ?2)",
        params![id, blob.sha256.to_hex()],
    )?;
    Ok(true)
}

/// Columns [`blob_from_row`] reads, from `blobs`.
const BLOB_COLUMNS: &str = "id, size, state, got, path,
    EXISTS (SELECT 1 FROM workdir_blob_refs r WHERE r.blob_id = blobs.id),
    (SELECT h.sha256 FROM blob_hashes h WHERE h.blob_id = blobs.id),
    (SELECT f.reason FROM blob_failures f WHERE f.blob_id = blobs.id)";

fn blob_from_row(row: &Row<'_>) -> rusqlite::Result<BlobRecord> {
    let state: String = row.get(2)?;
    let got: Option<i64> = row.get(3)?;
    let path: Option<String> = row.get(4)?;
    let failed: Option<String> = row.get(7)?;
    let status = match (state.as_str(), got, path) {
        // A failure clears `got` and `path`, so the two never meet.
        ("not_downloaded", Some(got), Some(path)) if failed.is_none() => BlobStatus::Stopped {
            got: u64::try_from(got).map_err(|e| bad_value(3, Type::Integer, e))?,
            path: path.into(),
        },
        ("not_downloaded", _, _) => match failed {
            Some(reason) => BlobStatus::Failed { reason },
            None => BlobStatus::NotDownloaded,
        },
        ("downloading", Some(got), Some(path)) => BlobStatus::Downloading {
            got: u64::try_from(got).map_err(|e| bad_value(3, Type::Integer, e))?,
            path: path.into(),
        },
        ("available", _, Some(path)) => BlobStatus::Available { path: path.into() },
        _ => {
            return Err(bad_value(
                2,
                Type::Text,
                format!("blob state {state:?} with missing columns"),
            ));
        }
    };
    let size: i64 = row.get(1)?;
    let sha256: String = row.get(6)?;
    Ok(BlobRecord {
        id: BlobId(uuid_col(row, 0)?),
        size: u64::try_from(size).map_err(|e| bad_value(1, Type::Integer, e))?,
        sha256: sha256.parse().map_err(|e| bad_value(6, Type::Text, e))?,
        status,
        workdir: row.get(5)?,
    })
}

// ---- column conversions ------------------------------------------------------

fn bad_value(
    idx: usize,
    ty: Type,
    err: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, ty, err.into())
}

fn uuid_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Uuid> {
    let text: String = row.get(idx)?;
    Uuid::parse_str(&text).map_err(|e| bad_value(idx, Type::Text, e))
}

fn seq_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Seq> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value)
        .map(Seq)
        .map_err(|e| bad_value(idx, Type::Integer, e))
}

fn u64_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value).map_err(|e| bad_value(idx, Type::Integer, e))
}

fn u32_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<u32> {
    let value: i64 = row.get(idx)?;
    u32::try_from(value).map_err(|e| bad_value(idx, Type::Integer, e))
}

fn json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text).map_err(|e| bad_value(idx, Type::Text, e))
}

fn opt_json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(idx)?;
    text.map(|text| serde_json::from_str(&text).map_err(|e| bad_value(idx, Type::Text, e)))
        .transpose()
}

fn to_json(value: &impl Serialize) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

fn seq_sql(seq: Seq) -> Result<i64, StoreError> {
    u64_sql(seq.0)
}

fn u64_sql(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::OutOfRange(value))
}

fn path_sql(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or_else(|| StoreError::NonUtf8Path(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steps that each record that they ran.
    const TEST_STEPS: [ProtocolStep; 3] = [
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (0)", []).map(drop)?),
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (1)", []).map(drop)?),
        |tx| Ok(tx.execute("INSERT INTO ran VALUES (2)", []).map(drop)?),
    ];

    #[test]
    fn protocol_steps_run_from_the_stamp_on() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, Stamp::Check).unwrap();
        let tx = conn.transaction().unwrap();
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
}
