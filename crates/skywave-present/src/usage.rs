//! The usage screen: cost and tokens totalled over a span of days, grouped
//! by day, session, workdir or model, from the turns in
//! [`UsageView`]. Every head shows these numbers, so they are summed here
//! once.
//!
//! A turn that reported no cost adds to [`Totals::unpriced`], never $0;
//! one that reported no tokens to [`Totals::uncounted`].

use std::collections::HashMap;

use chrono::{DateTime, Days, NaiveDate, TimeZone};
use skywave_core::{SessionId, TurnUsage, UsageView};

use crate::format;

/// The days a report covers, ending today in the head's time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Span {
    Today,
    #[default]
    Week,
    Month,
    All,
}

impl Span {
    /// In the order a head offers them.
    pub const EVERY: [Span; 4] = [Span::Today, Span::Week, Span::Month, Span::All];

    pub fn label(self) -> &'static str {
        match self {
            Span::Today => "today",
            Span::Week => "7 days",
            Span::Month => "30 days",
            Span::All => "all",
        }
    }

    /// The span after this one, wrapping.
    pub fn next(self) -> Span {
        let at = Span::EVERY.iter().position(|s| *s == self).unwrap_or(0);
        Span::EVERY[(at + 1) % Span::EVERY.len()]
    }

    /// First day counted, or `None` for every day.
    fn first_day(self, today: NaiveDate) -> Option<NaiveDate> {
        let back = match self {
            Span::Today => 0,
            Span::Week => 6,
            Span::Month => 29,
            Span::All => return None,
        };
        today.checked_sub_days(Days::new(back))
    }
}

/// What a report's rows are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    Day,
    Session,
    Workdir,
    Model,
}

impl Grouping {
    /// In the order a head offers them.
    pub const EVERY: [Grouping; 4] = [
        Grouping::Day,
        Grouping::Session,
        Grouping::Workdir,
        Grouping::Model,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Grouping::Day => "by day",
            Grouping::Session => "by session",
            Grouping::Workdir => "by directory",
            Grouping::Model => "by model",
        }
    }

    /// The grouping after this one, wrapping.
    pub fn next(self) -> Grouping {
        let at = Grouping::EVERY.iter().position(|g| *g == self).unwrap_or(0);
        Grouping::EVERY[(at + 1) % Grouping::EVERY.len()]
    }
}

/// Figures summed over some turns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub turns: u64,
    /// Over the turns that reported a cost.
    pub cost_usd: f64,
    /// Turns that reported no cost: unknown, not free.
    pub unpriced: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Turns that reported neither token count.
    pub uncounted: u64,
}

impl Totals {
    fn add(&mut self, usage: &TurnUsage) {
        self.turns += 1;
        match usage.cost_usd {
            Some(cost) => self.cost_usd += cost,
            None => self.unpriced += 1,
        }
        if usage.tokens_in.is_none() && usage.tokens_out.is_none() {
            self.uncounted += 1;
        }
        self.tokens_in += usage.tokens_in.unwrap_or(0);
        self.tokens_out += usage.tokens_out.unwrap_or(0);
    }

    /// "$1.27", "$1.27 + 2 unpriced", or "cost unknown" when no turn
    /// reported one.
    pub fn cost_label(&self) -> String {
        if self.turns > 0 && self.unpriced == self.turns {
            return "cost unknown".to_owned();
        }
        match self.unpriced {
            0 => format::dollars(self.cost_usd),
            n => format!("{} + {n} unpriced", format::dollars(self.cost_usd)),
        }
    }

    /// "3.1k in · 812 out", or "tokens unknown" when no turn reported any.
    pub fn tokens_label(&self) -> String {
        if self.turns > 0 && self.uncounted == self.turns {
            return "tokens unknown".to_owned();
        }
        format!(
            "{} in · {} out",
            format::tokens(self.tokens_in),
            format::tokens(self.tokens_out)
        )
    }

    /// "1 turn", "7 turns".
    pub fn turns_label(&self) -> String {
        let n = self.turns;
        format!("{n} {}", if n == 1 { "turn" } else { "turns" })
    }

    /// Cost, tokens and turns on one line.
    pub fn line(&self) -> String {
        format!(
            "{} · {} · {}",
            self.cost_label(),
            self.tokens_label(),
            self.turns_label()
        )
    }
}

/// One row of a report.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The day, session title, directory or model. A session title is text
    /// from the server: heads sanitise it before writing it anywhere raw.
    pub label: String,
    /// The row's session is archived (only `by session`).
    pub archived: bool,
    pub totals: Totals,
}

/// Usage over a span, every grouping at once so a head switches between
/// them without summing again.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub span: Span,
    pub total: Totals,
    /// Newest day first; days without turns are left out.
    pub days: Vec<Row>,
    /// The rest costliest first, then by tokens, then by label.
    pub sessions: Vec<Row>,
    pub workdirs: Vec<Row>,
    pub models: Vec<Row>,
    /// What the totals leave out, for the foot of the screen.
    pub note: String,
}

/// A day's label: "2026-11-02 Mon".
fn day_label(day: NaiveDate) -> String {
    day.format("%Y-%m-%d %a").to_string()
}

const NO_WORKDIR: &str = "unknown directory";
const NO_MODEL: &str = "unknown model";

impl Report {
    /// Totals the turns of `view` over `span`, ending on `now`'s day.
    /// `now`'s time zone draws the day lines.
    pub fn new<Tz: TimeZone>(view: &UsageView, span: Span, now: &DateTime<Tz>) -> Report {
        let zone = now.timezone();
        let first = span.first_day(now.date_naive());
        let sessions: HashMap<SessionId, _> = view.sessions.iter().map(|s| (s.id, s)).collect();

        let mut total = Totals::default();
        let mut days: HashMap<NaiveDate, Totals> = HashMap::new();
        let mut by_session: HashMap<SessionId, Totals> = HashMap::new();
        let mut workdirs: HashMap<Option<&str>, Totals> = HashMap::new();
        let mut models: HashMap<Option<&str>, Totals> = HashMap::new();
        for turn in &view.turns {
            let day = turn.at.with_timezone(&zone).date_naive();
            if first.is_some_and(|first| day < first) {
                continue;
            }
            let usage = &turn.usage;
            total.add(usage);
            days.entry(day).or_default().add(usage);
            by_session.entry(turn.session_id).or_default().add(usage);
            let workdir = sessions
                .get(&turn.session_id)
                .and_then(|s| s.workdir.as_deref());
            workdirs.entry(workdir).or_default().add(usage);
            models.entry(usage.model.as_deref()).or_default().add(usage);
        }

        let mut days: Vec<(NaiveDate, Totals)> = days.into_iter().collect();
        days.sort_by(|a, b| b.0.cmp(&a.0));
        let days = days
            .into_iter()
            .map(|(day, totals)| Row {
                label: day_label(day),
                archived: false,
                totals,
            })
            .collect();
        let sessions = ranked(by_session.into_iter().map(|(id, totals)| {
            let session = sessions.get(&id);
            Row {
                label: session
                    .map_or_else(|| format!("session {}", short(id)), |s| s.title.clone()),
                archived: session.is_some_and(|s| s.archived),
                totals,
            }
        }));
        let named = |label: Option<&str>, none: &str, totals| Row {
            label: label.unwrap_or(none).to_owned(),
            archived: false,
            totals,
        };
        let workdirs = ranked(workdirs.into_iter().map(|(w, t)| named(w, NO_WORKDIR, t)));
        let models = ranked(models.into_iter().map(|(m, t)| named(m, NO_MODEL, t)));

        Report {
            span,
            total,
            days,
            sessions,
            workdirs,
            models,
            note: scope_note(view.deleted),
        }
    }

    /// The rows of `grouping`.
    pub fn rows(&self, grouping: Grouping) -> &[Row] {
        match grouping {
            Grouping::Day => &self.days,
            Grouping::Session => &self.sessions,
            Grouping::Workdir => &self.workdirs,
            Grouping::Model => &self.models,
        }
    }
}

fn short(id: SessionId) -> String {
    let text = id.0.simple().to_string();
    text[text.len().saturating_sub(8)..].to_owned()
}

/// Costliest first, then most tokens, then by label.
fn ranked(rows: impl Iterator<Item = Row>) -> Vec<Row> {
    let mut rows: Vec<Row> = rows.collect();
    rows.sort_by(|a, b| {
        let tokens = |r: &Row| r.totals.tokens_in + r.totals.tokens_out;
        b.totals
            .cost_usd
            .total_cmp(&a.totals.cost_usd)
            .then_with(|| tokens(b).cmp(&tokens(a)))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

/// What the totals cover: archived sessions count; sessions deleted from
/// this device took their turns with them, and `deleted` says how many.
pub fn scope_note(deleted: usize) -> String {
    let counted = "Totals come from this device's replica: archived sessions count,";
    match deleted {
        0 => format!("{counted} sessions deleted from this device do not."),
        1 => format!("{counted} the 1 session deleted from this device does not."),
        n => format!("{counted} the {n} sessions deleted from this device do not."),
    }
}

/// What the screen says when the span holds no turn.
pub const NO_TURNS: &str = "No turn reported usage in this span.";

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Utc};
    use skywave_core::{Seq, TurnRecord, UsageSession};
    use uuid::Uuid;

    use super::*;

    fn session(n: u128, title: &str, workdir: Option<&str>, archived: bool) -> UsageSession {
        UsageSession {
            id: SessionId(Uuid::from_u128(n)),
            title: title.to_owned(),
            workdir: workdir.map(str::to_owned),
            archived,
        }
    }

    fn turn(
        seq: u64,
        session: u128,
        at: &str,
        cost: Option<f64>,
        model: Option<&str>,
    ) -> TurnRecord {
        TurnRecord {
            seq: Seq(seq),
            session_id: SessionId(Uuid::from_u128(session)),
            at: DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&Utc),
            usage: TurnUsage {
                duration_ms: Some(1000),
                tokens_in: Some(1000),
                tokens_out: Some(100),
                cost_usd: cost,
                model: model.map(str::to_owned),
            },
        }
    }

    fn view() -> UsageView {
        UsageView {
            turns: vec![
                // 40 days back: only in `All`.
                turn(1, 1, "2026-09-23T12:00:00Z", Some(5.0), Some("model-a")),
                turn(2, 1, "2026-10-30T12:00:00Z", Some(0.5), Some("model-a")),
                turn(3, 2, "2026-11-01T23:30:00Z", None, Some("model-b")),
                turn(4, 2, "2026-11-02T08:00:00Z", Some(0.25), None),
            ],
            sessions: vec![
                session(1, "first", Some("/srv/a"), false),
                session(2, "second", None, true),
            ],
            deleted: 2,
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_span_counts_whole_local_days_back_from_today() {
        let view = view();
        let all = Report::new(&view, Span::All, &now());
        assert_eq!(all.total.turns, 4);
        assert_eq!(all.total.cost_usd, 5.75);
        let week = Report::new(&view, Span::Week, &now());
        assert_eq!(week.total.turns, 3);
        let today = Report::new(&view, Span::Today, &now());
        assert_eq!(today.total.turns, 1);
        // An hour east, the 23:30 turn falls on today too.
        let east = now().with_timezone(&FixedOffset::east_opt(3600).unwrap());
        assert_eq!(Report::new(&view, Span::Today, &east).total.turns, 2);
    }

    #[test]
    fn a_turn_without_cost_is_unpriced_not_free() {
        let week = Report::new(&view(), Span::Week, &now());
        assert_eq!(week.total.unpriced, 1);
        assert_eq!(week.total.cost_label(), "$0.75 + 1 unpriced");
        let day = week
            .days
            .iter()
            .find(|r| r.label.starts_with("2026-11-01"))
            .unwrap();
        assert_eq!(day.totals.cost_label(), "cost unknown");
    }

    #[test]
    fn rows_group_by_day_session_workdir_and_model() {
        let week = Report::new(&view(), Span::Week, &now());
        let labels = |g| -> Vec<String> { week.rows(g).iter().map(|r| r.label.clone()).collect() };
        assert_eq!(
            labels(Grouping::Day),
            ["2026-11-02 Mon", "2026-11-01 Sun", "2026-10-30 Fri"]
        );
        assert_eq!(labels(Grouping::Session), ["first", "second"]);
        assert!(week.sessions[1].archived);
        assert_eq!(labels(Grouping::Workdir), ["/srv/a", NO_WORKDIR]);
        assert_eq!(labels(Grouping::Model), ["model-a", NO_MODEL, "model-b"]);
    }

    #[test]
    fn the_note_says_deleted_sessions_do_not_count() {
        let week = Report::new(&view(), Span::Week, &now());
        assert!(
            week.note
                .ends_with("the 2 sessions deleted from this device do not.")
        );
        assert!(scope_note(0).ends_with("sessions deleted from this device do not."));
    }

    #[test]
    fn labels_read_as_figures() {
        let mut totals = Totals::default();
        totals.add(&TurnUsage {
            tokens_in: Some(3100),
            tokens_out: Some(812),
            cost_usd: Some(1.27),
            ..TurnUsage::default()
        });
        assert_eq!(totals.line(), "$1.27 · 3.1k in · 812 out · 1 turn");
        totals.add(&TurnUsage::default());
        assert_eq!(totals.tokens_label(), "3.1k in · 812 out");
        assert_eq!(Totals::default().line(), "$0 · 0 in · 0 out · 0 turns");
    }

    #[test]
    fn spans_and_groupings_cycle() {
        assert_eq!(Span::All.next(), Span::Today);
        assert_eq!(Grouping::Model.next(), Grouping::Day);
    }
}
