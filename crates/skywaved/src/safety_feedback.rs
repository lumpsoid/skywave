//! The safety check feedback export (`skywaved safety-feedback`): every
//! judgment a device sent on the checker (D59), one JSON object per line,
//! oldest first, to improve the checker from.

use rusqlite::Connection;
use serde::Serialize;
use skywave_proto::{CallId, DeviceId, EventKind, SafetyJudgment};

use crate::error::{Error, Result};

/// The feedback events with their session's workdir. The session row may be
/// gone, so the join is a LEFT JOIN and the workdir may be NULL.
const FEEDBACK_SQL: &str = "SELECT e.at, e.session_id, s.workdir, e.kind FROM events e
    LEFT JOIN sessions s ON s.id = e.session_id
    WHERE json_extract(e.kind, '$.type') = 'safety_check_feedback'
    ORDER BY e.seq";

/// One exported feedback. The field order is the order of the output.
#[derive(Serialize)]
struct Line {
    at: String,
    session: String,
    workdir: Option<String>,
    call: CallId,
    shell: String,
    judgment: SafetyJudgment,
    note: Option<String>,
    checked_safe: bool,
    checked_reason: String,
    by: DeviceId,
}

/// Every safety check feedback in the log, as one JSON object per string,
/// oldest first.
pub fn lines(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(FEEDBACK_SQL)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut lines = Vec::new();
    for row in rows {
        let (at, session, workdir, kind) = row?;
        let EventKind::SafetyCheckFeedback {
            call,
            judgment,
            note,
            shell,
            checked_safe,
            checked_reason,
            by,
        } = serde_json::from_str(&kind)?
        else {
            return Err(Error::Corrupt(format!("safety check feedback row {kind}")));
        };
        let line = Line {
            at,
            session,
            workdir,
            call,
            shell,
            judgment,
            note,
            checked_safe,
            checked_reason,
            by,
        };
        lines.push(serde_json::to_string(&line)?);
    }
    Ok(lines)
}
