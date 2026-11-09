//! The agent's login in words ([`ViewState::login_expires`]): when it
//! expires, which Settings always says, and the warning a head shows on
//! every screen as it nears, so someone logs the agent in again on the
//! server before it stops starting.
//!
//! [`ViewState::login_expires`]: skywave_core::ViewState::login_expires

use chrono::{DateTime, TimeDelta, Utc};

/// How long before the login expires the warning shows; the agent's own
/// terminal warns as early.
pub const WARN_BEFORE: TimeDelta = TimeDelta::days(3);

/// The warning for a login that expires at `expires`, as of `now`,
/// lowercase for a banner: `agent login expires in 2 days`, counting a
/// part of a day as a day, or `agent login expired`. `None` while it is
/// further off than [`WARN_BEFORE`], and when it does not expire.
pub fn expiry_warning(expires: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    let left = expires? - now;
    if left > WARN_BEFORE {
        return None;
    }
    let warning = match days_left(left) {
        0 => "agent login expired".to_owned(),
        days => format!("agent login {}", expires_in(days)),
    };
    Some(format!("{warning} · log in again on the server"))
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
}
