//! A session's info page: what the session is, how its agent is set, what
//! its turns took and by which models, and every subagent it ran, worded
//! once for every head. Its usage limits read as the usage limits page's
//! ([`limits`](crate::limits)).
//!
//! A figure the agent did not report is left out, never shown as 0. Names,
//! directories, models and subagent titles are text from the server: heads
//! sanitise them before writing them anywhere raw.

use std::collections::BTreeMap;

use skywave_core::{AgentView, SessionInfoView, SubagentHeader};

use crate::format::{
    agent_effort_label, agent_mode_label, agent_model_label, context_fill, count, dollars,
    subagent_status, tokens, trail, turn_time,
};
use crate::usage::{NO_MODEL, Totals};

/// One labelled figure: `Tokens in` `153k`.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub label: String,
    pub value: String,
}

/// A titled group of fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: &'static str,
    pub fields: Vec<Field>,
}

/// A subagent as a row: what it was, where it ran and how it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentRow {
    pub title: String,
    /// The kind of subagent, as its call says (e.g. `Explore`).
    pub detail: Option<String>,
    /// The subagents it ran under, outermost first (`outer › inner`);
    /// empty for one the main agent launched.
    pub trail: String,
    /// As its row in the timeline: `finished · 14 steps · 4.3s · ctx 11.9k`.
    pub status: String,
    pub live: bool,
}

fn field(label: &str, value: impl Into<String>) -> Field {
    Field {
        label: label.to_owned(),
        value: value.into(),
    }
}

/// The page's sections, in order: the session, its usage, its agent, and
/// its models (left out while no turn reported usage).
pub fn sections(view: &SessionInfoView) -> Vec<Section> {
    let mut sections = vec![
        Section {
            title: "Session",
            fields: vec![
                field("Name", view.title.clone()),
                field(
                    "Workdir",
                    view.workdir.clone().unwrap_or_else(|| "unknown".to_owned()),
                ),
            ],
        },
        Section {
            title: "Usage",
            fields: usage(view),
        },
        Section {
            title: "Agent",
            fields: agent(&view.agent),
        },
    ];
    let models = models(view);
    if !models.is_empty() {
        sections.push(Section {
            title: "Models",
            fields: models,
        });
    }
    sections
}

/// The agent's model, effort and mode, a switch on its way as `ask → plan`.
fn agent(agent: &AgentView) -> Vec<Field> {
    // Its own label says `effort high`, for a line with no labels.
    let effort = agent_effort_label(agent).map(|label| {
        label
            .strip_prefix("effort ")
            .map_or(label.clone(), str::to_owned)
    });
    vec![
        field(
            "Model",
            agent_model_label(agent).unwrap_or_else(|| "unknown".to_owned()),
        ),
        field("Effort", effort.unwrap_or_else(|| "unknown".to_owned())),
        field("Mode", agent_mode_label(agent)),
    ]
}

/// What the session's turns took, summed, and how full its context is.
fn usage(view: &SessionInfoView) -> Vec<Field> {
    let Some(usage) = &view.usage else {
        return vec![field("Turns", "none reported yet")];
    };
    let total = &usage.total;
    let mut fields = Vec::new();
    if let Some(n) = total.tokens_in {
        fields.push(field("Tokens in", tokens(n)));
    }
    if let Some(n) = total.tokens_out {
        fields.push(field("Tokens out", tokens(n)));
    }
    fields.push(field(
        "Context",
        usage
            .context
            .map_or_else(|| "unknown".to_owned(), context_fill),
    ));
    if let Some(cost) = total.cost_usd {
        fields.push(field("Cost", dollars(cost)));
    }
    if let Some(ms) = total.duration_ms {
        fields.push(field("Time", turn_time(ms)));
    }
    fields.push(field("Turns", count(view.turns.len(), "turn", "turns")));
    fields
}

/// Each model the session's turns ran, with what its turns took, in the
/// order first used.
fn models(view: &SessionInfoView) -> Vec<Field> {
    let mut order: Vec<&str> = Vec::new();
    let mut totals: BTreeMap<&str, Totals> = BTreeMap::new();
    for turn in &view.turns {
        let model = turn.usage.model.as_deref().unwrap_or(NO_MODEL);
        if !totals.contains_key(model) {
            order.push(model);
        }
        totals.entry(model).or_default().add(&turn.usage);
    }
    order
        .into_iter()
        .map(|model| field(model, totals[model].line()))
        .collect()
}

/// The session's subagents as rows, in the order they were launched.
pub fn subagents(view: &SessionInfoView) -> Vec<SubagentRow> {
    view.subagents.iter().map(subagent_row).collect()
}

fn subagent_row(header: &SubagentHeader) -> SubagentRow {
    SubagentRow {
        title: header.title.clone(),
        detail: header.detail.clone(),
        trail: trail(&header.trail),
        status: subagent_status(&header.status),
        live: header.status.live,
    }
}

/// What the subagents section says when the session ran none.
pub const NO_SUBAGENTS: &str = "No subagents ran in this session.";

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use skywave_core::{
        CallId, Seq, SessionId, SessionUsage, SubagentStatus, SubagentUsage, TurnRecord, TurnUsage,
        UsageLimitsStatus, UsageLimitsView,
    };

    use super::*;

    fn view() -> SessionInfoView {
        SessionInfoView {
            id: SessionId::new(),
            title: "fix the flaky test".into(),
            workdir: Some("/srv/app".into()),
            usage: None,
            turns: Vec::new(),
            agent: AgentView::default(),
            subagents: Vec::new(),
            limits: UsageLimitsView {
                status: UsageLimitsStatus::Idle,
                answer: None,
            },
        }
    }

    fn turn(model: Option<&str>, cost: f64) -> TurnRecord {
        TurnRecord {
            seq: Seq(1),
            session_id: SessionId::new(),
            at: Utc::now(),
            usage: TurnUsage {
                tokens_in: Some(1_000),
                tokens_out: Some(200),
                cost_usd: Some(cost),
                model: model.map(str::to_owned),
                ..TurnUsage::default()
            },
        }
    }

    fn values(section: &Section) -> Vec<(&str, &str)> {
        section
            .fields
            .iter()
            .map(|f| (f.label.as_str(), f.value.as_str()))
            .collect()
    }

    #[test]
    fn a_session_without_usage_says_so_and_lists_no_models() {
        let sections = sections(&view());
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        assert_eq!(titles, ["Session", "Usage", "Agent"]);
        assert_eq!(
            values(&sections[0]),
            [("Name", "fix the flaky test"), ("Workdir", "/srv/app")]
        );
        assert_eq!(values(&sections[1]), [("Turns", "none reported yet")]);
        assert_eq!(
            values(&sections[2]),
            [
                ("Model", "unknown"),
                ("Effort", "unknown"),
                ("Mode", "unknown")
            ]
        );
    }

    #[test]
    fn usage_sums_the_turns_and_models_split_them_in_first_use_order() {
        let turns = vec![
            turn(Some("opus"), 0.5),
            turn(None, 0.1),
            turn(Some("opus"), 0.25),
        ];
        let mut usage = SessionUsage::new(&turns[0].usage);
        usage.add(&turns[1].usage);
        usage.add(&TurnUsage {
            context_tokens: Some(19_300),
            context_window: Some(1_000_000),
            ..turns[2].usage.clone()
        });
        let view = SessionInfoView {
            usage: Some(usage),
            turns,
            ..view()
        };
        let sections = sections(&view);
        assert_eq!(
            values(&sections[1]),
            [
                ("Tokens in", "3.0k"),
                ("Tokens out", "600"),
                ("Context", "19k/1M · 2%"),
                ("Cost", "$0.85"),
                ("Turns", "3 turns"),
            ]
        );
        assert_eq!(
            values(&sections[3]),
            [
                ("opus", "$0.75 · 2.0k in · 400 out · 2 turns"),
                ("unknown model", "$0.10 · 1.0k in · 200 out · 1 turn"),
            ]
        );
    }

    #[test]
    fn a_subagent_row_reads_as_in_the_timeline() {
        let (outer, inner) = (CallId::new(), CallId::new());
        let header = SubagentHeader {
            call: inner,
            title: "find the bug".into(),
            detail: Some("Explore".into()),
            prompt: None,
            status: SubagentStatus {
                task: None,
                live: false,
                steps: 3,
                last: None,
                usage: Some(SubagentUsage {
                    call: inner,
                    duration_ms: Some(4_300),
                    tool_uses: Some(14),
                    context_tokens: Some(11_900),
                }),
            },
            trail: vec![(outer, "outer".into())],
        };
        let view = SessionInfoView {
            subagents: vec![header],
            ..view()
        };
        assert_eq!(
            subagents(&view),
            [SubagentRow {
                title: "find the bug".into(),
                detail: Some("Explore".into()),
                trail: "outer".into(),
                status: "finished · 14 steps · 4.3s · ctx 12k".into(),
                live: false,
            }]
        );
    }
}
