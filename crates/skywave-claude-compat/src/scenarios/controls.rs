//! Quick scenarios: control requests only, no user message, so no turn and
//! no tokens.

use serde_json::{Value, json};
use skywaved::agent::claude_code::ALLOW_BYPASS_ARG;
use skywaved::agent::claude_code::protocol::{mode_name, parse_effort};
use skywaved::agent::{AdapterEvent, Effort, ModelChoice, PermissionMode};

use super::Ctx;
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup};
use crate::shape::clip;

/// How long one control request may take to be answered.
const ANSWER: u64 = 30;

/// A task id no process knows: the apprentice who went home early.
const GONE_HOME: &str = "apprentice-who-went-home";

pub fn handshake(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let checks = &mut ctx.checks;
    checks.contract(
        "the answer names claude_code_version",
        ensure(s.protocol.version().is_some(), || "no version".into()),
    );
    let mode = s.find_event(0, |e| match e {
        AdapterEvent::PermissionMode(mode) => Some(*mode),
        _ => None,
    });
    checks.contract(
        "the answer names a known current_permission_mode",
        ensure(mode.is_some(), || "none the adapter reads".into()),
    );
    let models = offered(&s);
    checks.contract(
        "the answer offers models",
        ensure(!models.is_empty(), || {
            "no `models` the adapter reads".into()
        }),
    );
    let read = s.wait_event("effort read after the handshake", ANSWER, 0, |e| match e {
        AdapterEvent::Effort(effort) => Some(*effort),
        _ => None,
    });
    checks.contract(
        "get_settings reads the effort in force",
        read.map(|_| ()).map_err(stalled),
    );
    let raw = s
        .find(0, |seen| {
            seen.msg.pointer("/response/response/models").is_some()
        })
        .map(|at| s.seen[at].msg["response"]["response"]["models"].clone());
    let unknown: Vec<String> = raw
        .iter()
        .flat_map(|models| models.as_array().into_iter().flatten())
        .flat_map(|model| {
            model["supportedEffortLevels"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_str)
        .filter(|level| parse_effort(level).is_none())
        .map(str::to_owned)
        .collect();
    checks.watch(
        "every offered effort level is one the adapter knows",
        "skywave_proto::Effort and protocol::effort_name (docs/agents/claude-code.md §3.10)",
        ensure(unknown.is_empty(), || format!("new levels {unknown:?}")),
    );
    Ok(())
}

pub fn permission_modes(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let plan = set_mode(&mut s, PermissionMode::Plan);
    ctx.checks.contract(
        "set_permission_mode plan succeeds and echoes the mode",
        expect_mode(plan, PermissionMode::Plan),
    );
    let bypass = set_mode(&mut s, PermissionMode::Bypass);
    ctx.checks.watch(
        "bypassPermissions is refused without the launch flag",
        "offering bypassPermissions (docs/agents/claude-code.md §3.6)",
        match bypass {
            Ok(Err(_)) => Ok(()),
            Ok(Ok(mode)) => Err(format!("accepted, now {}", mode_name(mode))),
            Err(stop) => Err(stalled(stop)),
        },
    );
    let id = s.control(json!({ "subtype": "set_permission_mode", "mode": "bogus" }));
    let answer = s.response(&id, ANSWER);
    ctx.checks.contract(
        "an unknown mode is refused",
        match &answer {
            Ok(r) => ensure(r["subtype"] == "error", || describe(r)),
            Err(stop) => Err(stop_text(stop)),
        },
    );
    if let Ok(answer) = &answer {
        ctx.checks.watch(
            "the CLI's modes are the adapter's",
            "skywave_proto::PermissionMode and protocol::mode_name",
            same_modes(answer),
        );
    }
    let back = set_mode(&mut s, PermissionMode::Ask);
    ctx.checks.contract(
        "set_permission_mode default succeeds",
        expect_mode(back, PermissionMode::Ask),
    );
    Ok(())
}

/// What `agent.allow_bypass` launches with: the session does not start in
/// bypass, and may switch to it (docs/agents/claude-code.md §3.6).
pub fn bypass_allowed(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup {
        args: vec![ALLOW_BYPASS_ARG.to_owned()],
        ..Setup::default()
    })?;
    let mode = s.find_event(0, |e| match e {
        AdapterEvent::PermissionMode(mode) => Some(*mode),
        _ => None,
    });
    ctx.checks.contract(
        "the allow flag does not start the session in bypass",
        match mode.map(|(_, mode)| mode) {
            Some(PermissionMode::Bypass) => Err("starts in bypassPermissions".into()),
            Some(_) => Ok(()),
            None => Err("no current_permission_mode the adapter reads".into()),
        },
    );
    let bypass = set_mode(&mut s, PermissionMode::Bypass);
    ctx.checks.contract(
        "with the allow flag, set_permission_mode bypassPermissions succeeds",
        expect_mode(bypass, PermissionMode::Bypass),
    );
    Ok(())
}

pub fn model_switch(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    let sonnet = set_model(&mut s, "sonnet");
    ctx.checks.contract(
        "set_model answers with the resolved model",
        match &sonnet {
            Ok(Ok(model)) => ensure(model.starts_with("claude-"), || model.clone()),
            Ok(Err(reason)) => Err(format!("refused: {reason}")),
            Err(stop) => Err(stop_text(stop)),
        },
    );
    if sonnet.is_ok() {
        let read = s.wait_event("effort read after set_model", ANSWER, from, |e| match e {
            AdapterEvent::Effort(effort) => Some(*effort),
            _ => None,
        });
        ctx.checks.contract(
            "get_settings answers after a model switch",
            read.map(|_| ()).map_err(stalled),
        );
    }
    let bogus = set_model(&mut s, "bogus-model");
    ctx.checks.contract(
        "an unknown model is refused",
        match bogus {
            Ok(Err(_)) => Ok(()),
            Ok(Ok(model)) => Err(format!("accepted as {model}")),
            Err(stop) => Err(stalled(stop)),
        },
    );
    let default = set_model(&mut s, "default");
    ctx.checks.contract(
        "set_model default succeeds",
        match default {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(reason)) => Err(format!("refused: {reason}")),
            Err(stop) => Err(stalled(stop)),
        },
    );
    Ok(())
}

pub fn effort_switch(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let models = offered(&s);
    let Some(rich) = models.iter().find(|m| m.efforts.len() >= 2) else {
        return Err(Stop::Inconclusive(
            "no offered model takes two effort levels".into(),
        ));
    };
    let (rich_id, levels) = (rich.id.clone(), rich.efforts.clone());
    let plain = models
        .iter()
        .find(|m| m.efforts.is_empty() && m.id != "default")
        .map(|m| m.id.clone());

    if let Err(stop) = settle_model(&mut s, &rich_id) {
        return Err(Stop::Inconclusive(format!(
            "cannot switch to {rich_id}: {}",
            stop_text(&stop)
        )));
    }
    for level in [levels[1], levels[0]] {
        let answer = set_effort(&mut s, level);
        ctx.checks.contract(
            &format!("{rich_id} reads back effort {level:?} once set"),
            match answer {
                Ok(Ok(Some(set))) if set == level => Ok(()),
                Ok(Ok(other)) => Err(format!("read {other:?}")),
                Ok(Err(reason)) => Err(format!("refused: {reason}")),
                Err(stop) => Err(stalled(stop)),
            },
        );
    }
    let Some(plain) = plain else {
        ctx.checks.watch(
            "some offered model takes no effort level",
            "the effort picker's empty state (docs/agents/claude-code.md §3.10)",
            Err("every offered model takes one".into()),
        );
        return Ok(());
    };
    let read = settle_model(&mut s, &plain);
    ctx.checks.contract(
        &format!("{plain}, offered without effort, reads effort null"),
        match read {
            Ok(None) => Ok(()),
            Ok(Some(level)) => Err(format!("read {level:?}")),
            Err(stop) => Err(stalled(stop)),
        },
    );
    let answer = set_effort(&mut s, levels[0]);
    ctx.checks.contract(
        &format!("{plain} refuses an effort level"),
        match answer {
            Ok(Err(_)) => Ok(()),
            Ok(Ok(level)) => Err(format!("read {level:?}")),
            Err(stop) => Err(stalled(stop)),
        },
    );
    Ok(())
}

pub fn stop_task_unknown(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    // The user asks to stop an apprentice who already went home.
    let out = s.protocol.stop_task(GONE_HOME.into());
    s.apply(out);
    let answer = s.wait_event("answer to stop_task", ANSWER, from, |e| match e {
        AdapterEvent::TaskStopAnswered { .. } => Some(Ok(())),
        AdapterEvent::TaskStopRefused { reason, .. } => Some(Err(reason.clone())),
        _ => None,
    });
    ctx.checks.contract(
        "stop_task on an unknown id gets one answer",
        answer.map(|_| ()).map_err(stalled),
    );
    Ok(())
}

pub fn send_task_message(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let id = s.control(json!({
        "subtype": "send_task_message",
        "task_id": GONE_HOME,
        "message": "Psst, apprentice: the croissants are ready, bring the big tray.",
    }));
    let answer = s.response(&id, ANSWER)?;
    ctx.checks.watch(
        "send_task_message is a stub",
        "D33/D34, messaging a subagent (docs/agents/claude-code.md §3.12)",
        ensure(
            answer["subtype"] == "error"
                && answer["error"] == "Unsupported control request subtype: send_task_message",
            || describe(&answer),
        ),
    );
    Ok(())
}

/// The models the handshake offered, as the adapter read them.
fn offered(s: &Session) -> Vec<ModelChoice> {
    s.find_event(0, |e| match e {
        AdapterEvent::ModelsOffered(models) => Some(models.clone()),
        _ => None,
    })
    .map(|(_, models)| models)
    .unwrap_or_default()
}

/// Asks for `mode`: the mode set, or the refusal's reason.
pub(super) fn set_mode(
    s: &mut Session,
    mode: PermissionMode,
) -> Result<Result<PermissionMode, String>, Stop> {
    let from = s.mark();
    let out = s.protocol.set_permission_mode(mode);
    s.apply(out);
    let what = format!("answer to set_permission_mode {}", mode_name(mode));
    s.wait_event(&what, ANSWER, from, |e| match e {
        AdapterEvent::PermissionModeSet(mode) => Some(Ok(*mode)),
        AdapterEvent::PermissionModeRefused { reason, .. } => Some(Err(reason.clone())),
        _ => None,
    })
    .map(|(_, answer)| answer)
}

pub(super) fn expect_mode(
    answer: Result<Result<PermissionMode, String>, Stop>,
    want: PermissionMode,
) -> Result<(), String> {
    match answer {
        Ok(Ok(mode)) => ensure(mode == want, || format!("set {}", mode_name(mode))),
        Ok(Err(reason)) => Err(format!("refused: {reason}")),
        Err(stop) => Err(stalled(stop)),
    }
}

/// Whether the modes a refusal lists ("must be one of a, b") are exactly
/// the adapter's.
fn same_modes(answer: &Value) -> Result<(), String> {
    let error = answer["error"].as_str().unwrap_or("");
    let Some((_, list)) = error.split_once("one of ") else {
        return Err(format!("no mode list in {}", describe(answer)));
    };
    let mut theirs: Vec<&str> = list.split(',').map(str::trim).collect();
    let mut ours: Vec<&str> = PermissionMode::ALL.into_iter().map(mode_name).collect();
    theirs.sort_unstable();
    ours.sort_unstable();
    ensure(theirs == ours, || format!("the CLI lists {theirs:?}"))
}

/// Asks for `model`: the model as the CLI resolved it, or the refusal's
/// reason.
fn set_model(s: &mut Session, model: &str) -> Result<Result<String, String>, Stop> {
    let from = s.mark();
    let out = s.protocol.set_model(model.to_owned());
    s.apply(out);
    s.wait_event(
        &format!("answer to set_model {model}"),
        ANSWER,
        from,
        |e| match e {
            AdapterEvent::ModelSet(model) => Some(Ok(model.clone())),
            AdapterEvent::ModelRefused { reason, .. } => Some(Err(reason.clone())),
            _ => None,
        },
    )
    .map(|(_, answer)| answer)
}

/// Switches to `model` and waits for the effort read that follows; the
/// effort in force.
fn settle_model(s: &mut Session, model: &str) -> Result<Option<Effort>, Stop> {
    let from = s.mark();
    match set_model(s, model)? {
        Ok(_) => {}
        Err(reason) => return Err(Stop::Stalled(format!("{model} refused: {reason}"))),
    }
    s.wait_event("effort read after set_model", ANSWER, from, |e| match e {
        AdapterEvent::Effort(effort) => Some(*effort),
        _ => None,
    })
    .map(|(_, effort)| effort)
}

/// Asks for `effort`: the effort read back once set, or the refusal's
/// reason.
fn set_effort(s: &mut Session, effort: Effort) -> Result<Result<Option<Effort>, String>, Stop> {
    let from = s.mark();
    let out = s.protocol.set_effort(effort);
    s.apply(out);
    s.wait_event(
        &format!("answer to effort {effort:?}"),
        ANSWER,
        from,
        |e| match e {
            AdapterEvent::EffortSet(effort) => Some(Ok(*effort)),
            AdapterEvent::EffortRefused { reason, .. } => Some(Err(reason.clone())),
            _ => None,
        },
    )
    .map(|(_, answer)| answer)
}

/// A control response, for a report.
fn describe(response: &Value) -> String {
    match response["subtype"].as_str() {
        Some("error") => format!("error {}", response["error"]),
        Some("success") => format!("success {}", clip(&response["response"].to_string())),
        _ => clip(&response.to_string()),
    }
}

pub(super) fn stop_text(stop: &Stop) -> String {
    match stop {
        Stop::Stalled(what) | Stop::Inconclusive(what) => what.clone(),
    }
}

pub(super) fn stalled(stop: Stop) -> String {
    stop_text(&stop)
}
