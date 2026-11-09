//! The link banner: core's [`Link`] with the queue and the approvals
//! elsewhere, as [`LinkUpdate`], and when its countdown next changes.

use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_core::{Link, LinkRejection};
use skywave_present::{link, login};

use crate::signals::{LinkState, LinkUpdate};

/// The agent's login as the server's last `hello` said it, for the
/// banner's login warnings ([`skywave_core::ViewState`]'s fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentLogin {
    /// As `ViewState::login_expires`.
    pub expires: Option<DateTime<Utc>>,
    /// As `ViewState::agent_login_offered`: the warnings say to log in
    /// from settings rather than on the server.
    pub offered: bool,
    /// As `ViewState::agent_logged_out`.
    pub logged_out: bool,
}

/// The banner for `link` at `wall_ms` (ms since the Unix epoch): the
/// retry countdown runs from it to the link's `retry_at`, and the login
/// warning from it to `login.expires`.
pub fn update(
    link: &Link,
    queued: usize,
    elsewhere: usize,
    login: AgentLogin,
    wall_ms: i64,
) -> LinkUpdate {
    let queued_part = match queued {
        0 => String::new(),
        n => format!(" · {n} queued"),
    };
    let mut retry_in_s = None;
    let mut retry = String::new();
    let (state, label) = match link {
        Link::Unpaired => (LinkState::Unpaired, String::new()),
        Link::Connecting => (LinkState::Connecting, format!("connecting…{queued_part}")),
        // Nothing to say while online with nothing waiting: the banner
        // hides. No latency: it changes on every ping and buys no action;
        // Settings shows it.
        Link::Online { .. } if queued == 0 && elsewhere == 0 => (LinkState::Online, String::new()),
        Link::Online { .. } => (LinkState::Online, format!("online{queued_part}")),
        Link::CatchingUp { have, server } => (
            LinkState::CatchingUp,
            format!("syncing · {}/{}{queued_part}", have.0, server.0),
        ),
        Link::Reconnecting {
            attempt,
            retry_at,
            reason,
            ..
        } => {
            let left = seconds_left(retry_at.timestamp_millis(), wall_ms);
            retry_in_s = Some(left);
            retry = match left {
                0 => "retrying…".to_owned(),
                // Two digits (the cap is 30s) keep the banner from shifting.
                n => format!("retry in {n:02}s"),
            };
            let reason = link::failure(*reason);
            // Two lines, always: wrapped as one, the ticking countdown
            // moved the break and the banner flipped between one and two
            // lines on a phone. The countdown goes last, so its change to
            // `retrying…` moves nothing after it.
            (
                LinkState::Reconnecting,
                format!("offline · {reason}{queued_part}\nattempt {attempt} · {retry}"),
            )
        }
        Link::Rejected(
            rejection @ (LinkRejection::Revoked
            | LinkRejection::ServerChanged
            | LinkRejection::NameMismatch
            | LinkRejection::BadIdentity),
        ) => (
            LinkState::RepairNeeded,
            format!(
                "{} · re-pair to reconnect{queued_part}",
                repair_short(*rejection)
            ),
        ),
        Link::Rejected(LinkRejection::ProtocolMismatch { client, server }) => (
            LinkState::ProtocolMismatch,
            format!(
                "protocol mismatch · client v{client}, server v{server} · update, then retry{queued_part}"
            ),
        ),
        Link::Rejected(LinkRejection::HistoryLost { have, server }) => (
            LinkState::HistoryLost,
            format!(
                "server lost history (had {}, server at {}) · sync stopped{queued_part}",
                have.0, server.0
            ),
        ),
    };
    LinkUpdate {
        state,
        label,
        connection_label: connection_label(link, &retry),
        retry_in_s,
        can_retry_now: match link {
            Link::Reconnecting { .. } => true,
            Link::Rejected(rejection) => rejection.can_retry(),
            _ => false,
        },
        rejected: matches!(link, Link::Rejected(_)),
        can_reset_replica: matches!(link, Link::Rejected(LinkRejection::HistoryLost { .. })),
        queued: count(queued),
        approvals_elsewhere: count(elsewhere),
        elsewhere_label: match elsewhere {
            0 => None,
            1 => Some("1 approval elsewhere".to_owned()),
            n => Some(format!("{n} approvals elsewhere")),
        },
        logged_out_label: login
            .logged_out
            .then(|| login::logged_out_warning(login.offered)),
        // A logged-out agent's expiry is moot: the logged-out warning says
        // all there is to do.
        login_label: DateTime::from_timestamp_millis(wall_ms)
            .filter(|_| !login.logged_out)
            .and_then(|now| login::expiry_warning_for(login.expires, now, login.offered)),
    }
}

/// The link as Settings says it, with the round trip while online;
/// `retry` is the countdown while reconnecting.
fn connection_label(link: &Link, retry: &str) -> String {
    match link {
        Link::Unpaired => "Not paired".to_owned(),
        Link::Connecting => "Connecting…".to_owned(),
        Link::Online { latency_ms } => format!("Connected · {latency_ms} ms"),
        Link::CatchingUp { have, server } => format!("Syncing · {}/{}", have.0, server.0),
        Link::Reconnecting { reason, .. } => {
            format!("Offline · {} · {retry}", link::failure(*reason))
        }
        Link::Rejected(
            rejection @ (LinkRejection::Revoked
            | LinkRejection::ServerChanged
            | LinkRejection::NameMismatch
            | LinkRejection::BadIdentity),
        ) => sentence_case(repair_short(*rejection)),
        Link::Rejected(LinkRejection::ProtocolMismatch { client, server }) => {
            format!("Protocol mismatch · client v{client}, server v{server}")
        }
        Link::Rejected(LinkRejection::HistoryLost { .. }) => "Server lost history".to_owned(),
    }
}

/// The re-pair reason as the banner says it: `server identity changed`.
fn repair_short(rejection: LinkRejection) -> &'static str {
    link::repair_notice(rejection).map_or("", |notice| notice.short)
}

/// `text` with its first letter upper case, for Settings.
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How long from `wall_ms` until the countdown of `link` shows another
/// second; `None` when nothing counts down.
pub fn next_change(link: &Link, wall_ms: i64) -> Option<Duration> {
    let Link::Reconnecting { retry_at, .. } = link else {
        return None;
    };
    let left_ms = retry_at.timestamp_millis().saturating_sub(wall_ms);
    // Rounded up, the label changes as `left_ms` crosses a whole second.
    (left_ms > 0).then(|| {
        let into = left_ms.rem_euclid(1000);
        let ms = if into == 0 { 1000 } else { into };
        Duration::from_millis(ms.unsigned_abs())
    })
}

/// Whole seconds from `now_ms` to `at_ms`, rounded up: "1s" until the
/// moment itself.
fn seconds_left(at_ms: i64, now_ms: i64) -> u32 {
    let left = at_ms.saturating_sub(now_ms).max(0).unsigned_abs();
    u32::try_from(left.div_ceil(1000)).unwrap_or(u32::MAX)
}

/// A count as the contract carries it.
pub fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_core::Seq;

    use super::*;

    #[test]
    fn every_link_state_reads() {
        let at = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let now = at.timestamp_millis() - 6_500;
        let reconnecting = Link::Reconnecting {
            attempt: 3,
            retry_at: at,
            offline_since: at,
            reason: skywave_core::LinkFailure::Unreachable,
        };
        let cases = [
            (Link::Unpaired, ""),
            (Link::Connecting, "connecting… · 2 queued"),
            (Link::Online { latency_ms: 12 }, "online · 2 queued"),
            (
                Link::CatchingUp {
                    have: Seq(4),
                    server: Seq(9),
                },
                "syncing · 4/9 · 2 queued",
            ),
            (
                reconnecting.clone(),
                "offline · server unreachable · 2 queued\nattempt 3 · retry in 07s",
            ),
            (
                Link::Rejected(LinkRejection::Revoked),
                "device revoked · re-pair to reconnect · 2 queued",
            ),
            (
                Link::Rejected(LinkRejection::ServerChanged),
                "server identity changed · re-pair to reconnect · 2 queued",
            ),
            (
                Link::Rejected(LinkRejection::NameMismatch),
                "server name mismatch · re-pair to reconnect · 2 queued",
            ),
            (
                Link::Rejected(LinkRejection::BadIdentity),
                "device identity unusable · re-pair to reconnect · 2 queued",
            ),
            (
                Link::Rejected(LinkRejection::HistoryLost {
                    have: Seq(9),
                    server: Seq(4),
                }),
                "server lost history (had 9, server at 4) · sync stopped · 2 queued",
            ),
        ];
        for (link, label) in cases {
            let banner = update(&link, 2, 0, AgentLogin::default(), now);
            assert_eq!(banner.label, label, "{link:?}");
            let lost = matches!(link, Link::Rejected(LinkRejection::HistoryLost { .. }));
            assert_eq!(banner.can_reset_replica, lost, "{link:?}");
            let repair = matches!(link, Link::Rejected(r) if r.needs_repair());
            assert_eq!(banner.state == LinkState::RepairNeeded, repair, "{link:?}");
        }
        let online = Link::Online { latency_ms: 12 };
        assert_eq!(
            update(&online, 0, 0, AgentLogin::default(), now).label,
            "",
            "nothing to say"
        );
        assert_eq!(
            update(&online, 0, 1, AgentLogin::default(), now).label,
            "online",
            "a button to show"
        );
        let banner = update(&reconnecting, 0, 1, AgentLogin::default(), now);
        assert_eq!(
            banner.connection_label,
            "Offline · server unreachable · retry in 07s"
        );
        assert_eq!(banner.retry_in_s, Some(7));
        assert!(banner.can_retry_now);
        assert_eq!(
            banner.elsewhere_label.as_deref(),
            Some("1 approval elsewhere")
        );
        // The countdown changes when 6.5s become 6s left.
        assert_eq!(
            next_change(&reconnecting, now),
            Some(Duration::from_millis(500))
        );
        let due = update(
            &reconnecting,
            0,
            0,
            AgentLogin::default(),
            at.timestamp_millis() + 10,
        );
        assert_eq!(due.retry_in_s, Some(0));
        assert!(due.label.ends_with("\nattempt 3 · retrying…"));
        assert_eq!(
            due.connection_label,
            "Offline · server unreachable · retrying…"
        );
        assert_eq!(next_change(&reconnecting, at.timestamp_millis()), None);
    }

    #[test]
    fn settings_says_the_link_with_its_latency_while_online() {
        let cases = [
            (Link::Unpaired, "Not paired"),
            (Link::Connecting, "Connecting…"),
            (Link::Online { latency_ms: 12 }, "Connected · 12 ms"),
            (
                Link::CatchingUp {
                    have: Seq(4),
                    server: Seq(9),
                },
                "Syncing · 4/9",
            ),
            (Link::Rejected(LinkRejection::Revoked), "Device revoked"),
            (
                Link::Rejected(LinkRejection::ServerChanged),
                "Server identity changed",
            ),
            (
                Link::Rejected(LinkRejection::BadIdentity),
                "Device identity unusable",
            ),
            (
                Link::Rejected(LinkRejection::ProtocolMismatch {
                    client: 3,
                    server: 4,
                }),
                "Protocol mismatch · client v3, server v4",
            ),
        ];
        for (link, label) in cases {
            // A server-side fix may clear a mismatch or a changed
            // certificate; nothing on the server mends a revoke or a
            // damaged identity file.
            let retry = matches!(
                link,
                Link::Rejected(
                    LinkRejection::ProtocolMismatch { .. } | LinkRejection::ServerChanged
                )
            );
            assert_eq!(
                update(&link, 0, 0, AgentLogin::default(), 0).can_retry_now,
                retry,
                "{link:?}"
            );
            // Said whatever is queued, and even while the banner hides.
            assert_eq!(
                update(&link, 0, 0, AgentLogin::default(), 0).connection_label,
                label,
                "{link:?}"
            );
            assert_eq!(
                update(&link, 2, 1, AgentLogin::default(), 0).connection_label,
                label,
                "{link:?}"
            );
        }
    }

    #[test]
    fn the_login_warning_shows_within_three_days_of_its_expiry_whatever_the_link() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let online = Link::Online { latency_ms: 12 };
        let in_days = |days| AgentLogin {
            expires: Some(now + chrono::TimeDelta::days(days)),
            ..AgentLogin::default()
        };
        let banner = update(&online, 0, 0, in_days(2), now.timestamp_millis());
        assert_eq!(banner.label, "", "the link has nothing to say");
        assert_eq!(
            banner.login_label.as_deref(),
            Some("agent login expires in 2 days · log in again on the server")
        );
        let offered = AgentLogin {
            offered: true,
            ..in_days(2)
        };
        assert_eq!(
            update(&online, 0, 0, offered, now.timestamp_millis())
                .login_label
                .as_deref(),
            Some("agent login expires in 2 days · log in again from settings")
        );
        let later = update(&online, 0, 0, in_days(4), now.timestamp_millis());
        assert_eq!(later.login_label, None);
        assert_eq!(
            update(&online, 0, 0, AgentLogin::default(), 0).login_label,
            None
        );
    }

    #[test]
    fn the_logged_out_warning_shows_whatever_the_link_even_online() {
        let online = Link::Online { latency_ms: 12 };
        let login = |offered, logged_out| AgentLogin {
            expires: None,
            offered,
            logged_out,
        };
        let banner = update(&online, 0, 0, login(true, true), 0);
        assert_eq!(banner.label, "", "the link has nothing to say");
        assert_eq!(
            banner.logged_out_label.as_deref(),
            Some("agent not logged in · log in from settings")
        );
        assert_eq!(
            update(&Link::Connecting, 0, 0, login(false, true), 0)
                .logged_out_label
                .as_deref(),
            Some("agent not logged in · log in on the server")
        );
        assert_eq!(
            update(&online, 0, 0, login(true, false), 0).logged_out_label,
            None
        );
    }

    #[test]
    fn no_expiry_warning_while_logged_out() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let online = Link::Online { latency_ms: 12 };
        let expiring = |logged_out| AgentLogin {
            expires: Some(now + chrono::TimeDelta::days(2)),
            offered: true,
            logged_out,
        };
        let banner = update(&online, 0, 0, expiring(true), now.timestamp_millis());
        assert_eq!(banner.login_label, None);
        assert_eq!(
            banner.logged_out_label.as_deref(),
            Some("agent not logged in · log in from settings")
        );
        assert_eq!(
            update(&online, 0, 0, expiring(false), now.timestamp_millis())
                .login_label
                .as_deref(),
            Some("agent login expires in 2 days · log in again from settings"),
            "the same expiry warns while logged in"
        );
    }
}
