//! The link banner: core's [`Link`] with the queue and the approvals
//! elsewhere, as [`LinkUpdate`], and when its countdown next changes.

use std::time::Duration;

use skywave_core::{Link, LinkRejection};

use crate::signals::{LinkState, LinkUpdate};

/// The banner for `link` at `wall_ms` (ms since the Unix epoch): the
/// retry countdown runs from it to the link's `retry_at`.
pub fn update(link: &Link, queued: usize, elsewhere: usize, wall_ms: i64) -> LinkUpdate {
    let queued_part = match queued {
        0 => String::new(),
        n => format!(" · {n} queued"),
    };
    let mut retry_in_s = None;
    let (state, label) = match link {
        Link::Unpaired => (LinkState::Unpaired, String::new()),
        Link::Connecting => (LinkState::Connecting, format!("connecting…{queued_part}")),
        // Nothing to say while online with nothing waiting: the banner
        // hides. No latency: it changes on every ping and buys no action.
        Link::Online { .. } if queued == 0 && elsewhere == 0 => (LinkState::Online, String::new()),
        Link::Online { .. } => (LinkState::Online, format!("online{queued_part}")),
        Link::CatchingUp { have, server } => (
            LinkState::CatchingUp,
            format!("syncing · {}/{}{queued_part}", have.0, server.0),
        ),
        Link::Reconnecting {
            attempt, retry_at, ..
        } => {
            let left = seconds_left(retry_at.timestamp_millis(), wall_ms);
            retry_in_s = Some(left);
            let retry = match left {
                0 => "retrying…".to_owned(),
                // Two digits (the cap is 30s) keep the banner from shifting.
                n => format!("retry in {n:02}s"),
            };
            (
                LinkState::Reconnecting,
                format!("offline{queued_part} · {retry} (attempt {attempt})"),
            )
        }
        Link::Rejected(LinkRejection::Revoked) => (
            LinkState::Revoked,
            format!("device revoked · re-pair to reconnect{queued_part}"),
        ),
        Link::Rejected(LinkRejection::ProtocolMismatch { client, server }) => (
            LinkState::ProtocolMismatch,
            format!(
                "protocol mismatch · client v{client}, server v{server} · update to reconnect{queued_part}"
            ),
        ),
    };
    LinkUpdate {
        state,
        label,
        retry_in_s,
        can_retry_now: matches!(link, Link::Reconnecting { .. }),
        rejected: matches!(link, Link::Rejected(_)),
        queued: count(queued),
        approvals_elsewhere: count(elsewhere),
        elsewhere_label: match elsewhere {
            0 => None,
            1 => Some("1 approval elsewhere".to_owned()),
            n => Some(format!("{n} approvals elsewhere")),
        },
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
                "offline · 2 queued · retry in 07s (attempt 3)",
            ),
            (
                Link::Rejected(LinkRejection::Revoked),
                "device revoked · re-pair to reconnect · 2 queued",
            ),
        ];
        for (link, label) in cases {
            assert_eq!(update(&link, 2, 0, now).label, label, "{link:?}");
        }
        let online = Link::Online { latency_ms: 12 };
        assert_eq!(update(&online, 0, 0, now).label, "", "nothing to say");
        assert_eq!(
            update(&online, 0, 1, now).label,
            "online",
            "a button to show"
        );
        let banner = update(&reconnecting, 0, 1, now);
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
        let due = update(&reconnecting, 0, 0, at.timestamp_millis() + 10);
        assert_eq!(due.retry_in_s, Some(0));
        assert!(due.label.contains("retrying…"));
        assert_eq!(next_change(&reconnecting, at.timestamp_millis()), None);
    }
}
