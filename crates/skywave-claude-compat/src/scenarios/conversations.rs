//! Quick scenarios on conversations: resume, fork, an unknown id, and the
//! transcript the CLI writes (docs/agents/claude-code.md §5, §6c).
//!
//! The CLI keeps no conversation until a message arrives, so each scene
//! sends one: `/rename`, a local command that never calls the model (a turn
//! with `num_turns: 0` and no cost) yet is kept in the transcript as a
//! `<command-name>` user line. Resume and fork are then read back through
//! the adapter's own transcript reader ([`ClaudeTranscripts`]), so a check
//! holds only if skywaved would see the same.
//!
//! Not covered: `ai-title`, which `-p` never writes, and a model turn's
//! lines in a transcript (the stored-transcript fixtures cover those).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use skywaved::agent::claude_code::protocol::parse_resume_handle;
use skywaved::agent::claude_code::transcripts::{ClaudeTranscripts, projects_dir};
use skywaved::agent::claude_code::{CliSession, UNKNOWN_SESSION};
use skywaved::agent::{AdapterEvent, HistoryItem, HistoryRead, OutsideConversations, TurnOutcome};
use uuid::Uuid;

use super::Ctx;
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup, Workdir};
use crate::shape::clip;

/// How long a local command's turn may take.
const LOCAL: u64 = 30;

/// How long reading the transcripts back may take.
const READ: Duration = Duration::from_secs(10);

/// A cwd under the scratch dir long enough that its project dir name is
/// cut and hashed, with characters the name rule turns into `-` (`_`, `.`,
/// and `è`, `û`, `é` one UTF-16 unit each).
const LONG_SUBDIR: &str = concat!(
    "crème_brûlée.batch/crème_brûlée.batch/crème_brûlée.batch/",
    "crème_brûlée.batch/crème_brûlée.batch/crème_brûlée.batch/",
    "crème_brûlée.batch/crème_brûlée.batch/crème_brûlée.batch/",
    "crème_brûlée.batch",
);

pub fn resume(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(None)?;
    let id = Uuid::new_v4();
    begin(ctx, &workdir, id, "Morning bake")?;

    let mut s = ctx.session(Setup {
        session: Some(CliSession::Resume(id)),
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    let handles = local(&mut s, "/rename Second batch")?;
    ctx.checks.contract(
        "the resumed process runs under the same session id",
        same_id(&s, id),
    );
    // The adapter would report one only if the CLI ran under another id.
    ctx.checks.contract(
        "a resumed process reports no new resume handle",
        ensure(handles.is_empty(), || format!("reported {handles:?}")),
    );
    drop(s);

    let history = transcripts()?.history(workdir.cwd(), &id.to_string(), deadline());
    ctx.checks.contract(
        "the transcript's active branch holds both processes' messages, in order",
        in_order(history, &["Morning bake", "Second batch"]),
    );
    Ok(())
}

pub fn fork(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(None)?;
    let parent = Uuid::new_v4();
    begin(ctx, &workdir, parent, "Morning bake")?;
    let parent_file = workdir
        .project_dir()
        .map(|dir| dir.join(format!("{parent}.jsonl")));
    let before = parent_file.as_ref().and_then(|f| std::fs::read(f).ok());

    let id = Uuid::new_v4();
    let mut s = ctx.session(Setup {
        session: Some(CliSession::Fork { parent, id }),
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    let handles = local(&mut s, "/rename Wedding cake")?;
    ctx.checks.contract(
        "the fork runs under the session id we chose",
        same_id(&s, id),
    );
    ctx.checks.contract(
        "the fork reports one resume handle, naming its own id",
        ensure(handles == [id], || format!("reported {handles:?}")),
    );
    drop(s);

    let transcripts = transcripts()?;
    let forked = transcripts.history(workdir.cwd(), &id.to_string(), deadline());
    ctx.checks.contract(
        "the fork's transcript holds the parent's history, then its own",
        in_order(forked, &["Morning bake", "Wedding cake"]),
    );
    let after = parent_file.as_ref().and_then(|f| std::fs::read(f).ok());
    ctx.checks.contract(
        "the parent's transcript is untouched",
        ensure(before.is_some() && before == after, || {
            match (before, after) {
                (None, _) => "the parent had no transcript where project_dir puts it".into(),
                (Some(b), Some(a)) => format!("{} bytes before, {} after", b.len(), a.len()),
                (Some(_), None) => "it is gone".into(),
            }
        }),
    );
    Ok(())
}

pub fn resume_unknown(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(None)?;
    let resumed = ctx.session_or_refusal(Setup {
        session: Some(CliSession::Resume(Uuid::new_v4())),
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    ctx.checks.contract(
        "--resume of an unknown id fails before the handshake ends, saying UNKNOWN_SESSION",
        refused(resumed),
    );

    let id = Uuid::new_v4();
    let forked = ctx.session_or_refusal(Setup {
        session: Some(CliSession::Fork {
            parent: Uuid::new_v4(),
            id,
        }),
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    ctx.checks.contract(
        "a fork of an unknown id fails before the handshake ends, saying UNKNOWN_SESSION",
        refused(forked),
    );
    let left = workdir
        .project_dir()
        .map(|dir| dir.join(format!("{id}.jsonl")))
        .filter(|file| file.exists());
    ctx.checks.contract(
        "the failed fork leaves no transcript",
        ensure(left.is_none(), || format!("{left:?}")),
    );
    Ok(())
}

pub fn transcript(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(Some(LONG_SUBDIR))?;
    let id = Uuid::new_v4();
    begin(ctx, &workdir, id, "Morning bake")?;
    let cwd = workdir
        .cwd()
        .to_str()
        .ok_or_else(|| Stop::Stalled("the scratch dir is not UTF-8".into()))?;
    let name = format!("{id}.jsonl");
    let path = workdir.project_dir().map(|dir| dir.join(&name));
    let Some(path) = path.filter(|path| path.is_file()) else {
        ctx.checks.contract(
            "the transcript is at <projects>/<project_dir(cwd)>/<id>.jsonl (a long cwd cut and hashed)",
            Err(match find(&name) {
                Some(dir) => format!("it is in {dir}"),
                None => "no transcript under the projects dir".into(),
            }),
        );
        return Ok(());
    };
    ctx.checks.contract(
        "the transcript is at <projects>/<project_dir(cwd)>/<id>.jsonl (a long cwd cut and hashed)",
        Ok(()),
    );

    let text = std::fs::read_to_string(&path).map_err(|e| Stop::Stalled(e.to_string()))?;
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let first_user = lines
        .iter()
        .find(|line| line["type"] == "user" && line["isSidechain"] == false);
    ctx.checks.contract(
        "its first main `user` line carries the cwd",
        match first_user {
            Some(line) => ensure(line["cwd"] == cwd, || format!("cwd {}", line["cwd"])),
            None => Err("no user line with isSidechain: false".into()),
        },
    );
    let chained: Vec<&Value> = lines
        .iter()
        .filter(|line| matches!(line["type"].as_str(), Some("user" | "assistant" | "system")))
        .collect();
    let unlinked: Vec<String> = chained
        .iter()
        .filter(|line| {
            !line["uuid"].is_string()
                || !(line["parentUuid"].is_string() || line["parentUuid"].is_null())
                || line.get("parentUuid").is_none()
                || !line["isSidechain"].is_boolean()
        })
        .map(|line| clip(&line.to_string()))
        .collect();
    ctx.checks.contract(
        "every user, assistant and system line carries uuid, parentUuid and isSidechain",
        ensure(!chained.is_empty() && unlinked.is_empty(), || {
            if chained.is_empty() {
                "no such line".into()
            } else {
                unlinked.join("; ")
            }
        }),
    );
    let dangling: Vec<&str> = chained
        .iter()
        .filter_map(|line| line["parentUuid"].as_str())
        .filter(|parent| !lines.iter().any(|line| line["uuid"] == *parent))
        .collect();
    ctx.checks.contract(
        "every parentUuid names a line of the transcript",
        ensure(dangling.is_empty(), || format!("missing {dangling:?}")),
    );

    let transcripts = transcripts()?;
    let history = transcripts.history(workdir.cwd(), &id.to_string(), deadline());
    ctx.checks.contract(
        "history reads the message back, with its timestamp",
        match history {
            HistoryRead::Read { items } => ensure(
                items.iter().any(|item| {
                    matches!(item, HistoryItem::User { at: Some(_), text } if text.contains("Morning bake"))
                }),
                || format!("{items:?}"),
            ),
            other => Err(format!("{other:?}")),
        },
    );
    let listed = transcripts
        .list(
            &[workdir.root().to_owned()],
            &HashMap::new(),
            usize::MAX,
            deadline(),
        )
        .map_err(|e| e.to_string())
        .and_then(|listed| {
            listed
                .conversations
                .into_iter()
                .find(|c| c.id == id.to_string())
                .ok_or_else(|| "not listed".to_owned())
        });
    ctx.checks.contract(
        "the listing names it, under its cwd, with its first prompt",
        listed.and_then(|c| {
            ensure(
                c.workdir == cwd && c.first_prompt.is_some() && c.messages >= 1,
                || format!("{c:?}"),
            )
        }),
    );
    Ok(())
}

/// Starts conversation `id` in `workdir` with `/rename <title>`, then ends
/// its process.
fn begin(ctx: &mut Ctx, workdir: &std::rc::Rc<Workdir>, id: Uuid, title: &str) -> Result<(), Stop> {
    let mut s = ctx.session(Setup {
        session: Some(CliSession::New(id)),
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    local(&mut s, &format!("/rename {title}")).map(drop)
}

/// Sends local command `command` and waits for its turn, which must
/// complete; the session ids of the resume handles the adapter reported
/// meanwhile. The first is reported as the message is sent, so it is in
/// the send's events rather than in [`Session::seen`].
fn local(s: &mut Session, command: &str) -> Result<Vec<Uuid>, Stop> {
    let from = s.mark();
    let out = s.protocol.send_message(command.to_owned());
    let mut events = out.events.clone();
    s.apply(out);
    let (_, outcome) = s.turn_end(from, LOCAL)?;
    if outcome != TurnOutcome::Completed {
        return Err(Stop::Stalled(format!("{command} ended {outcome:?}")));
    }
    events.extend(s.seen[from..].iter().flat_map(|m| m.events.iter().cloned()));
    Ok(events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ResumeHandle(handle) => parse_resume_handle(handle).ok(),
            _ => None,
        })
        .collect())
}

/// Whether the `system/init` names session `id`.
fn same_id(s: &Session, id: Uuid) -> Result<(), String> {
    let init = s.find(0, |m| m.is("system/init"));
    let named = init.and_then(|at| s.seen[at].str("/session_id"));
    ensure(named == Some(id.to_string().as_str()), || {
        format!("system/init names {named:?}")
    })
}

/// Whether the history's user messages mention each of `titles`, in that
/// order.
fn in_order(history: HistoryRead, titles: &[&str]) -> Result<(), String> {
    let HistoryRead::Read { items } = history else {
        return Err(format!("{history:?}"));
    };
    let texts: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            HistoryItem::User { text, .. } => Some(text.as_str()),
            HistoryItem::Agent { .. } => None,
        })
        .collect();
    let mut rest = texts.iter();
    let found = titles
        .iter()
        .all(|title| rest.any(|text| text.contains(title)));
    ensure(found, || format!("user messages {texts:?}"))
}

/// Whether a start was refused with the text the adapter reads as a resume
/// error.
fn refused(start: Result<Session, String>) -> Result<(), String> {
    match start {
        Ok(_) => {
            Err("the handshake succeeded: the adapter would see the error only as an exit".into())
        }
        Err(text) => ensure(text.contains(UNKNOWN_SESSION), || {
            format!("\"{}\"", clip(&text))
        }),
    }
}

fn transcripts() -> Result<ClaudeTranscripts, Stop> {
    projects_dir()
        .map(ClaudeTranscripts::new)
        .ok_or_else(|| Stop::Stalled("neither CLAUDE_CONFIG_DIR nor HOME is set".into()))
}

fn deadline() -> Instant {
    Instant::now() + READ
}

/// The project dir holding transcript `name`, wherever the CLI put it.
fn find(name: &str) -> Option<String> {
    let projects = projects_dir()?;
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .find(|dir| dir.path().join(name).is_file())
        .map(|dir| dir.file_name().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use skywaved::agent::claude_code::transcripts::project_dir;

    use super::*;

    /// The transcript scenario's cwd takes the cut-and-hashed branch of
    /// the name rule even under the shortest scratch dir.
    #[test]
    fn the_long_subdir_is_hashed() {
        let short = "/tmp/skywave-bakery-aaaaaa/crème_brûlée.batch";
        assert_eq!(project_dir(short).len(), short.encode_utf16().count());

        let cwd = format!("/tmp/skywave-bakery-aaaaaa/{LONG_SUBDIR}");
        assert!(cwd.encode_utf16().count() > 200);
        let name = project_dir(&cwd);
        assert_eq!(name.as_bytes()[200], b'-');
        let hash = &name[201..];
        assert!(!hash.is_empty() && hash.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(
            name[..200]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
    }
}
