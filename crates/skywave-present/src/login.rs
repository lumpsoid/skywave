//! The agent's login in words ([`ViewState::login_expires`]): when it
//! expires, which Settings always says, and the warning a head shows on
//! every screen as it nears, so someone logs the agent in again before it
//! stops starting; that the agent is logged out
//! ([`ViewState::agent_logged_out`]); and where a login from this device
//! stands ([`LoginView`]).
//!
//! Where to log in depends on whether the server offers a login from a
//! device ([`ViewState::agent_login_offered`]): from settings when it
//! does, on the server when it does not.
//!
//! [`ViewState::login_expires`]: skywave_core::ViewState::login_expires
//! [`ViewState::agent_logged_out`]: skywave_core::ViewState::agent_logged_out
//! [`ViewState::agent_login_offered`]: skywave_core::ViewState::agent_login_offered

use chrono::{DateTime, TimeDelta, Utc};
use skywave_core::{LoginCodeRefused, LoginFlowView, LoginNoAnswer, LoginOutcome, LoginView};

/// How long before the login expires the warning shows; the agent's own
/// terminal warns as early.
pub const WARN_BEFORE: TimeDelta = TimeDelta::days(3);

/// The steps of a login from this device, once the server opened one.
pub const LOGIN_STEPS: &str = "open this URL, sign in, paste the code shown";

/// The warning for a login that expires at `expires`, as of `now`,
/// lowercase for a banner: `agent login expires in 2 days`, counting a
/// part of a day as a day, or `agent login expired`. `None` while it is
/// further off than [`WARN_BEFORE`], and when it does not expire. Always
/// says to log in on the server; [`expiry_warning_for`] says where.
pub fn expiry_warning(expires: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    expiry_warning_for(expires, now, false)
}

/// As [`expiry_warning`], advising to log in again from settings when the
/// server `offered` a login from a device, on the server when not.
pub fn expiry_warning_for(
    expires: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    offered: bool,
) -> Option<String> {
    let left = expires? - now;
    if left > WARN_BEFORE {
        return None;
    }
    let warning = match days_left(left) {
        0 => "agent login expired".to_owned(),
        days => format!("agent login {}", expires_in(days)),
    };
    Some(format!("{warning} · log in again {}", where_to(offered)))
}

/// The warning that the server's agent is not logged in, for a banner on
/// every screen (ahead of [`expiry_warning_for`]): `agent not logged in ·
/// log in from settings` when the server `offered` a login from a device,
/// `… · log in on the server` when not.
pub fn logged_out_warning(offered: bool) -> String {
    format!("agent not logged in · log in {}", where_to(offered))
}

/// Where to log in: `from settings`, or `on the server`.
fn where_to(offered: bool) -> &'static str {
    if offered {
        "from settings"
    } else {
        "on the server"
    }
}

/// Where this device's login stands, in lines for Settings, as of `now`.
/// The URL to open is not among them: a head shows it on its own, to open
/// or copy. Empty with no login started and nothing to say.
pub fn flow_lines(login: &LoginView, now: DateTime<Utc>) -> Vec<String> {
    match &login.flow {
        LoginFlowView::Idle if login.logged_out => vec!["agent not logged in".to_owned()],
        LoginFlowView::Idle => Vec::new(),
        LoginFlowView::Starting => vec!["starting the login…".to_owned()],
        LoginFlowView::Open { refused, .. } => {
            let mut lines = vec![LOGIN_STEPS.to_owned()];
            lines.extend(refused.as_ref().map(refused_line));
            lines
        }
        LoginFlowView::Finishing { .. } => vec!["sending the code…".to_owned()],
        LoginFlowView::NoAnswer { why, .. } => vec![match why {
            LoginNoAnswer::Offline => {
                "offline · the code was not sent · retry once connected".to_owned()
            }
            LoginNoAnswer::Failed(err) => format!("the code got no answer: {err} · retry"),
        }],
        LoginFlowView::Done(outcome) => match outcome {
            LoginOutcome::LoggedIn => vec![
                "agent logged in".to_owned(),
                expiry_label(login.expires, now),
            ],
            LoginOutcome::Busy => {
                vec!["another login is under way on the server · start again".to_owned()]
            }
            LoginOutcome::Unavailable => {
                vec!["the agent cannot be logged in from here · log in on the server".to_owned()]
            }
            LoginOutcome::NoFlow => {
                vec!["the server no longer knows this login · start again".to_owned()]
            }
            LoginOutcome::Offline => {
                vec!["offline · nothing was asked · start again once connected".to_owned()]
            }
            LoginOutcome::Failed(err) => vec![format!("login failed: {err} · start again")],
        },
    }
}

/// Why a pasted code was not sent.
fn refused_line(refused: &LoginCodeRefused) -> String {
    match refused {
        LoginCodeRefused::Blank => "no code pasted · paste the code shown".to_owned(),
        LoginCodeRefused::TooLong { max_bytes } => {
            format!("code too long (over {max_bytes} bytes) · paste the code shown")
        }
    }
}

/// When the login expires, for Settings, whatever how far off:
/// `expires in 12 days · 2026-11-21`, `expired · 2026-11-08` (dates in
/// UTC), or `no expiry reported` when the server says none (a login that
/// does not expire, or one it cannot read).
pub fn expiry_label(expires: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(expires) = expires else {
        return "no expiry reported".to_owned();
    };
    let date = expires.format("%Y-%m-%d");
    match days_left(expires - now) {
        0 => format!("expired · {date}"),
        days => format!("{} · {date}", expires_in(days)),
    }
}

/// Whole days in `left`, a part of a day counting as a day; 0 once it is
/// not positive.
fn days_left(left: TimeDelta) -> u64 {
    if left <= TimeDelta::zero() {
        return 0;
    }
    // Positive here, so unsigned: signed `div_ceil` is unstable.
    let day = TimeDelta::days(1).num_milliseconds().unsigned_abs();
    left.num_milliseconds().unsigned_abs().div_ceil(day)
}

/// `expires in 1 day`, `expires in 12 days`.
fn expires_in(days: u64) -> String {
    let unit = if days == 1 { "day" } else { "days" };
    format!("expires in {days} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_000_000, 0).unwrap()
    }

    fn warning(left: TimeDelta) -> Option<String> {
        expiry_warning(Some(now() + left), now())
    }

    #[test]
    fn no_warning_further_off_than_three_days_or_without_an_expiry() {
        assert_eq!(expiry_warning(None, now()), None);
        assert_eq!(warning(WARN_BEFORE + TimeDelta::seconds(1)), None);
        assert_eq!(warning(TimeDelta::days(30)), None);
    }

    #[test]
    fn days_left_count_a_part_of_a_day_as_a_day() {
        let says = |left| warning(left).unwrap();
        assert_eq!(
            says(WARN_BEFORE),
            "agent login expires in 3 days · log in again on the server"
        );
        assert!(says(TimeDelta::hours(49)).starts_with("agent login expires in 3 days"));
        assert!(says(TimeDelta::hours(48)).starts_with("agent login expires in 2 days"));
        assert!(says(TimeDelta::hours(25)).starts_with("agent login expires in 2 days"));
        assert!(says(TimeDelta::hours(24)).starts_with("agent login expires in 1 day ·"));
        assert!(says(TimeDelta::seconds(1)).starts_with("agent login expires in 1 day ·"));
    }

    #[test]
    fn a_login_past_its_expiry_has_expired() {
        assert_eq!(
            warning(TimeDelta::zero()).unwrap(),
            "agent login expired · log in again on the server"
        );
        assert!(
            warning(-TimeDelta::days(9))
                .unwrap()
                .starts_with("agent login expired")
        );
    }

    #[test]
    fn settings_say_the_expiry_however_far_off() {
        // 2026-11-06 21:20 UTC.
        let label = |left| expiry_label(Some(now() + left), now());
        assert_eq!(
            label(TimeDelta::days(30)),
            "expires in 30 days · 2026-12-06"
        );
        assert_eq!(label(TimeDelta::hours(2)), "expires in 1 day · 2026-11-06");
        assert_eq!(label(TimeDelta::hours(3)), "expires in 1 day · 2026-11-07");
        assert_eq!(label(-TimeDelta::days(1)), "expired · 2026-11-05");
        assert_eq!(expiry_label(None, now()), "no expiry reported");
    }

    #[test]
    fn the_logged_out_warning_says_where_to_log_in() {
        assert_eq!(
            logged_out_warning(true),
            "agent not logged in · log in from settings"
        );
        assert_eq!(
            logged_out_warning(false),
            "agent not logged in · log in on the server"
        );
    }

    #[test]
    fn the_expiry_advice_says_where_to_log_in_again() {
        let soon = Some(now() + TimeDelta::days(2));
        let gone = Some(now() - TimeDelta::days(1));
        assert_eq!(
            expiry_warning_for(soon, now(), true).unwrap(),
            "agent login expires in 2 days · log in again from settings"
        );
        assert_eq!(
            expiry_warning_for(soon, now(), false).unwrap(),
            "agent login expires in 2 days · log in again on the server"
        );
        assert_eq!(
            expiry_warning_for(gone, now(), true).unwrap(),
            "agent login expired · log in again from settings"
        );
        assert_eq!(
            expiry_warning_for(gone, now(), false).unwrap(),
            "agent login expired · log in again on the server"
        );
        // Not offered, the old wording, byte for byte.
        for at in [soon, gone, None, Some(now() + TimeDelta::days(9))] {
            assert_eq!(
                expiry_warning(at, now()),
                expiry_warning_for(at, now(), false)
            );
        }
        assert_eq!(
            expiry_warning_for(Some(now() + TimeDelta::days(9)), now(), true),
            None
        );
    }

    fn lines(logged_out: bool, flow: LoginFlowView) -> Vec<String> {
        let login = LoginView {
            offered: true,
            logged_out,
            expires: Some(now() + TimeDelta::days(30)),
            flow,
        };
        flow_lines(&login, now())
    }

    #[test]
    fn every_flow_state_reads() {
        let url = || "https://example.test/login".to_owned();
        assert_eq!(lines(false, LoginFlowView::Idle), Vec::<String>::new());
        assert_eq!(lines(true, LoginFlowView::Idle), ["agent not logged in"]);
        assert_eq!(
            lines(true, LoginFlowView::Starting),
            ["starting the login…"]
        );
        assert_eq!(
            lines(
                true,
                LoginFlowView::Open {
                    url: url(),
                    refused: None
                }
            ),
            ["open this URL, sign in, paste the code shown"]
        );
        assert_eq!(
            lines(
                true,
                LoginFlowView::Open {
                    url: url(),
                    refused: Some(LoginCodeRefused::Blank)
                }
            ),
            [
                "open this URL, sign in, paste the code shown",
                "no code pasted · paste the code shown"
            ]
        );
        assert_eq!(
            lines(
                true,
                LoginFlowView::Open {
                    url: url(),
                    refused: Some(LoginCodeRefused::TooLong { max_bytes: 4096 })
                }
            )[1],
            "code too long (over 4096 bytes) · paste the code shown"
        );
        assert_eq!(
            lines(true, LoginFlowView::Finishing { url: url() }),
            ["sending the code…"]
        );
        assert_eq!(
            lines(
                true,
                LoginFlowView::NoAnswer {
                    url: url(),
                    why: LoginNoAnswer::Offline
                }
            ),
            ["offline · the code was not sent · retry once connected"]
        );
        assert_eq!(
            lines(
                true,
                LoginFlowView::NoAnswer {
                    url: url(),
                    why: LoginNoAnswer::Failed("timed out".into())
                }
            ),
            ["the code got no answer: timed out · retry"]
        );
        let done = |outcome| lines(false, LoginFlowView::Done(outcome));
        assert_eq!(
            done(LoginOutcome::LoggedIn),
            ["agent logged in", "expires in 30 days · 2026-12-06"]
        );
        assert_eq!(
            done(LoginOutcome::Busy),
            ["another login is under way on the server · start again"]
        );
        assert_eq!(
            done(LoginOutcome::Unavailable),
            ["the agent cannot be logged in from here · log in on the server"]
        );
        assert_eq!(
            done(LoginOutcome::NoFlow),
            ["the server no longer knows this login · start again"]
        );
        assert_eq!(
            done(LoginOutcome::Offline),
            ["offline · nothing was asked · start again once connected"]
        );
        assert_eq!(
            done(LoginOutcome::Failed("bad code".into())),
            ["login failed: bad code · start again"]
        );
    }
}
