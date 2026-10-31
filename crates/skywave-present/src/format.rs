//! Neutral wording: sizes, durations, token counts, costs, and how a
//! command went out or why it failed. Plain strings, no key hints and no
//! styling; a head adds its own around them.

use std::time::Duration;

use skywave_core::{
    BlobState, Decision, Delivery, FailReason, Outgoing, RejectReason, RuleScope, RuleSuggestion,
};

/// How an own command went out, all by this device's clock. Once answered,
/// what each step took from the queueing: `queued 14:02:11 · sent +3.2s ·
/// 2 tries · answered +8.1s`. Until then, how long it has waited and where
/// it is: `queued 2m10s ago · sending, try 3`, or `… · sent once, waiting
/// for the server` (sent, so it can no longer be taken back). `wall_ms` is
/// now, in milliseconds since the Unix epoch.
pub fn delivery_label(delivery: &Delivery, outgoing: &Outgoing, wall_ms: i64) -> String {
    let queued = delivery.queued_at;
    // From the queueing to `ms`; never negative, should the clock step back.
    let since =
        |ms: i64| Duration::from_millis(u64::try_from(ms - queued.timestamp_millis()).unwrap_or(0));
    let tries = delivery.tries;
    let mut parts = Vec::new();
    match delivery.answered_at {
        Some(answered) => {
            parts.push(format!("queued {}", queued.format("%H:%M:%S")));
            match delivery.first_sent_at {
                Some(sent) => {
                    parts.push(format!(
                        "sent +{}",
                        took_label(since(sent.timestamp_millis()))
                    ));
                }
                // Sent before send times were recorded.
                None if tries > 0 => parts.push("sent".to_owned()),
                None => {}
            }
            if tries > 1 {
                parts.push(format!("{tries} tries"));
            }
            let verb = match outgoing {
                Outgoing::Failed(_) => "refused",
                _ => "answered",
            };
            parts.push(format!(
                "{verb} +{}",
                took_label(since(answered.timestamp_millis()))
            ));
        }
        None => {
            parts.push(format!("queued {} ago", seconds(since(wall_ms))));
            match (outgoing, tries) {
                (Outgoing::Sending, 0 | 1) => parts.push("sending".to_owned()),
                (Outgoing::Sending, n) => parts.push(format!("sending, try {n}")),
                (Outgoing::Queued, 0) => {}
                (Outgoing::Queued, 1) => parts.push("sent once, waiting for the server".to_owned()),
                (Outgoing::Queued, n) => {
                    parts.push(format!("sent {n} times, waiting for the server"));
                }
                (Outgoing::Accepted(_) | Outgoing::Failed(_), _) => {}
            }
        }
    }
    parts.join(" · ")
}

/// A call's duration: tenths below ten seconds.
pub fn took_label(took: Duration) -> String {
    if took < Duration::from_secs(10) {
        format!("{:.1}s", took.as_secs_f64())
    } else {
        seconds(took)
    }
}

/// Whole seconds, minutes or hours: "7s", "2m05s", "1h02m".
pub fn seconds(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// A blob's size while it is not here; a downloaded blob is its file.
pub fn blob_size(state: &BlobState) -> Option<u64> {
    match state {
        BlobState::NotDownloaded { size }
        | BlobState::Requested { size }
        | BlobState::Downloading { size, .. }
        | BlobState::Failed { size, .. } => Some(*size),
        BlobState::Available { .. } => None,
    }
}

/// `got` of `size` as a whole percentage, at most 100; an empty size is
/// done.
pub fn percent(got: u64, size: u64) -> u64 {
    (got.saturating_mul(100))
        .checked_div(size)
        .unwrap_or(100)
        .min(100)
}

/// A byte count in words: "812 B", "12.3 KiB", "4.0 MiB".
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / KIB),
        _ => format!("{:.1} MiB", n as f64 / (KIB * KIB)),
    }
}

/// A turn's length: "0.8s", "12.4s", "2m 05s", "1h 02m".
pub fn turn_time(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// A token count, compact: "812", "3.1k", "153k", "1.2M".
pub fn tokens(n: u64) -> String {
    let (value, unit) = match n {
        0..1000 => return n.to_string(),
        1000..999_500 => (n as f64 / 1e3, "k"),
        _ => (n as f64 / 1e6, "M"),
    };
    if value < 9.95 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.0}{unit}")
    }
}

/// A cost in US dollars to 2–3 significant digits: "$0.0014", "$0.04",
/// "$0.87", "$1.27", "$12.3", "$140".
pub fn dollars(usd: f64) -> String {
    if usd.is_nan() || usd <= 0.0 {
        return "$0".to_owned();
    }
    if usd >= 1.0 {
        let decimals = if usd < 9.995 {
            2
        } else if usd < 99.95 {
            1
        } else {
            0
        };
        return format!("${usd:.decimals$}");
    }
    // Two significant digits, at least cents, without trailing zeros.
    let decimals = (1 - usd.log10().floor() as i32).max(2) as usize;
    let text = format!("{usd:.decimals$}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let fraction = fraction.trim_end_matches('0');
    format!("${whole}.{fraction:0<2}")
}

/// A command's delivery state in words, with why it failed.
pub fn outgoing_label(outgoing: &Outgoing) -> String {
    match outgoing {
        Outgoing::Queued => "queued".to_owned(),
        Outgoing::Sending => "sending".to_owned(),
        Outgoing::Accepted(_) => "accepted".to_owned(),
        Outgoing::Failed(reason) => format!("failed: {}", fail_reason(*reason)),
    }
}

/// Why a command failed, in words.
pub fn fail_reason(reason: FailReason) -> &'static str {
    match reason {
        FailReason::Rejected(reason) => reject_reason(reason),
        FailReason::Undeliverable => "the server could not take it",
    }
}

/// Why the server refused a command, in words.
pub fn reject_reason(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::WorkdirNotAllowed => "not under a configured root on the server",
        RejectReason::WorkdirNotFound => "no such directory on the server",
        RejectReason::UnknownSession => "the server does not know this session",
        RejectReason::UnknownApproval => "the server does not know this approval",
        RejectReason::ApprovalAlreadyResolved => "already answered on another device",
        RejectReason::ApprovalCancelled => "the approval was cancelled first",
        RejectReason::NoActiveTurn => "the agent was not working",
        RejectReason::UnknownRule => "the agent did not offer that rule",
    }
}

/// A decision in words: "approved", "denied".
pub fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approved",
        Decision::Deny => "denied",
    }
}

/// An answer in words: `approved always` when it granted a rule.
pub fn answer_words(decision: Decision, rule: Option<&RuleSuggestion>) -> &'static str {
    match rule {
        Some(_) => "approved always",
        None => decision_label(decision),
    }
}

/// How far a rule reaches, in words.
pub fn scope_label(scope: RuleScope) -> &'static str {
    match scope {
        RuleScope::Session => "this session",
        RuleScope::Project => "project",
        RuleScope::User => "user",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_figures_are_compact() {
        let times = [(800, "0.8s"), (12_400, "12.4s"), (125_000, "2m 05s")];
        for (ms, text) in times.into_iter().chain([(3_720_000, "1h 02m")]) {
            assert_eq!(turn_time(ms), text);
        }
        for (n, text) in [
            (812, "812"),
            (3_100, "3.1k"),
            (19_337, "19k"),
            (153_400, "153k"),
            (999_999, "1.0M"),
            (1_234_567, "1.2M"),
            (45_000_000, "45M"),
        ] {
            assert_eq!(tokens(n), text, "{n}");
        }
        for (usd, text) in [
            (0.0, "$0"),
            (0.00142969, "$0.0014"),
            (0.04, "$0.04"),
            (0.1, "$0.10"),
            (0.86724, "$0.87"),
            (1.27, "$1.27"),
            (12.34, "$12.3"),
            (140.2, "$140"),
        ] {
            assert_eq!(dollars(usd), text, "{usd}");
        }
    }
}
