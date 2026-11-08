//! The usage limits page: the limits of the account the server's agent runs
//! as, one row per limit, worded once for every head.
//!
//! A figure the agent did not report is left out, never shown as 0. A
//! scope is text from the server: heads sanitise it before writing it
//! anywhere raw.

use chrono::{DateTime, Utc};
use skywave_core::{
    LimitSeverity, LimitWindow, UsageLimit, UsageLimitsResult, UsageLimitsStatus,
    UsageLimitsUnavailable, UsageLimitsView,
};

use crate::format;

/// One limit, as a row: a label, how much is used, a bar and when it
/// starts over.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitRow {
    /// `Session`, `Weekly`, or `Weekly · Fable` for one model's limit.
    pub label: String,
    /// `75%`; `None` when the agent did not say.
    pub percent: Option<String>,
    /// How full the bar is, 0.0 to 1.0; `None` without a percent.
    pub fill: Option<f64>,
    /// The agent warns that this limit is close.
    pub warning: bool,
    /// `resets in 2 h 13 min`, `resets in 6 d 4 h`, `reset since`; `None`
    /// when the agent did not say.
    pub resets: Option<String>,
}

/// What the page shows: when the limits were read and their rows, or why
/// there are none.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// `Read 12 min ago` and one row per limit, in the agent's order.
    Limits { read: String, rows: Vec<LimitRow> },
    /// Nothing to list, in a sentence.
    Empty(String),
}

/// The page's body for the stored `answer` (a view's `answer`) at `now`.
pub fn body(answer: Option<&UsageLimitsResult>, now: DateTime<Utc>) -> Body {
    match answer {
        None => Body::Empty("No usage limits read yet.".to_owned()),
        Some(UsageLimitsResult::Unavailable) => {
            Body::Empty("This server's agent does not report usage limits.".to_owned())
        }
        // Never stored; worded anyway rather than shown as no limits.
        Some(UsageLimitsResult::Failed { reason }) => {
            Body::Empty(format!("Could not read the usage limits: {reason}"))
        }
        Some(UsageLimitsResult::Read { limits, .. }) if limits.is_empty() => {
            Body::Empty("The agent's account has no usage limits.".to_owned())
        }
        Some(UsageLimitsResult::Read { at, limits }) => Body::Limits {
            read: format!("Read {}", format::ago(*at, now)),
            rows: limits.iter().map(|limit| row(limit, now)).collect(),
        },
    }
}

/// One limit as a row at `now`.
pub fn row(limit: &UsageLimit, now: DateTime<Utc>) -> LimitRow {
    let window = match limit.window {
        LimitWindow::Session => "Session",
        LimitWindow::Weekly => "Weekly",
    };
    let label = match &limit.scope {
        Some(scope) => format!("{window} · {scope}"),
        None => window.to_owned(),
    };
    LimitRow {
        label,
        percent: limit.percent.map(|p| format!("{p}%")),
        fill: limit.percent.map(|p| (f64::from(p) / 100.0).min(1.0)),
        warning: limit.severity == Some(LimitSeverity::Warning),
        resets: limit.resets_at.map(|at| resets(at, now)),
    }
}

/// When a limit starts over, from `now`: to the minute within a day, to
/// the hour past it.
fn resets(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (at - now).num_minutes();
    if at <= now {
        return "reset since".to_owned();
    }
    let (days, hours, mins) = (mins / 1_440, mins / 60 % 24, mins % 60);
    match (days, hours) {
        (0, 0) => format!("resets in {} min", mins.max(1)),
        (0, _) => format!("resets in {hours} h {mins} min"),
        (_, 0) => format!("resets in {days} d"),
        _ => format!("resets in {days} d {hours} h"),
    }
}

/// A line above the rows while a read is on its way or the last one got no
/// answer; `None` otherwise.
pub fn status_line(view: &UsageLimitsView) -> Option<String> {
    let have = view.answer.is_some();
    match &view.status {
        UsageLimitsStatus::Idle => None,
        UsageLimitsStatus::Loading if have => Some("Refreshing…".to_owned()),
        UsageLimitsStatus::Loading => Some("Asking the agent…".to_owned()),
        UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Offline) => Some(if have {
            "Offline: these limits may be out of date.".to_owned()
        } else {
            "Offline: usage limits are read only when connected.".to_owned()
        }),
        UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Failed(reason)) => Some(if have {
            format!("Refresh failed ({reason}); these limits may be out of date.")
        } else {
            format!("Could not read the usage limits: {reason}")
        }),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-08T12:00:00Z".parse().unwrap()
    }

    fn limit(window: LimitWindow) -> UsageLimit {
        UsageLimit {
            window,
            scope: None,
            percent: None,
            severity: None,
            resets_at: None,
        }
    }

    #[test]
    fn a_row_shows_what_the_agent_reported_and_leaves_out_the_rest() {
        let full = UsageLimit {
            scope: Some("Fable".into()),
            percent: Some(75),
            severity: Some(LimitSeverity::Warning),
            resets_at: Some(now() + TimeDelta::minutes(133)),
            ..limit(LimitWindow::Weekly)
        };
        assert_eq!(
            row(&full, now()),
            LimitRow {
                label: "Weekly · Fable".into(),
                percent: Some("75%".into()),
                fill: Some(0.75),
                warning: true,
                resets: Some("resets in 2 h 13 min".into()),
            }
        );
        assert_eq!(
            row(&limit(LimitWindow::Session), now()),
            LimitRow {
                label: "Session".into(),
                percent: None,
                fill: None,
                warning: false,
                resets: None,
            }
        );
        let over = UsageLimit {
            percent: Some(130),
            ..limit(LimitWindow::Session)
        };
        assert_eq!(row(&over, now()).fill, Some(1.0));
    }

    #[test]
    fn a_reset_reads_to_the_minute_within_a_day_and_to_the_hour_past_it() {
        let at = |mins| resets(now() + TimeDelta::minutes(mins), now());
        assert_eq!(at(0), "reset since");
        assert_eq!(at(-5), "reset since");
        assert_eq!(at(1), "resets in 1 min");
        assert_eq!(at(59), "resets in 59 min");
        assert_eq!(at(60), "resets in 1 h 0 min");
        assert_eq!(at(1_440), "resets in 1 d");
        assert_eq!(at(6 * 1_440 + 4 * 60 + 30), "resets in 6 d 4 h");
        assert_eq!(
            resets(now() + TimeDelta::seconds(20), now()),
            "resets in 1 min"
        );
    }

    #[test]
    fn the_body_says_why_there_is_nothing_to_list() {
        let empty = |answer| match body(answer, now()) {
            Body::Empty(text) => text,
            other => panic!("expected no rows, got {other:?}"),
        };
        assert_eq!(empty(None), "No usage limits read yet.");
        assert!(empty(Some(&UsageLimitsResult::Unavailable)).contains("does not report"));
        let none = UsageLimitsResult::Read {
            at: now(),
            limits: Vec::new(),
        };
        assert!(empty(Some(&none)).contains("no usage limits"));
        let read = UsageLimitsResult::Read {
            at: now() - TimeDelta::minutes(12),
            limits: vec![limit(LimitWindow::Session)],
        };
        let Body::Limits { read, rows } = body(Some(&read), now()) else {
            panic!("expected rows");
        };
        assert_eq!(read, "Read 12 min ago");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn the_status_line_keeps_the_stored_limits_in_view() {
        let view = |status, answer: Option<UsageLimitsResult>| UsageLimitsView { status, answer };
        let stored = Some(UsageLimitsResult::Unavailable);
        assert_eq!(status_line(&view(UsageLimitsStatus::Idle, None)), None);
        assert_eq!(
            status_line(&view(UsageLimitsStatus::Loading, stored.clone())).as_deref(),
            Some("Refreshing…")
        );
        let offline = UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Offline);
        assert!(
            status_line(&view(offline, stored))
                .unwrap()
                .contains("out of date")
        );
        let failed =
            UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Failed("timed out".into()));
        assert_eq!(
            status_line(&view(failed, None)).as_deref(),
            Some("Could not read the usage limits: timed out")
        );
    }
}
