//! The link's troubles in words: why it is reconnecting, and the notice a
//! head shows when only pairing again (or a new address) brings it back.
//! A head adds its own key hints and the countdown.

use skywave_core::{LinkFailure, LinkRejection};

/// Why the last attempt failed, as the reconnecting banner says it:
/// `server unreachable`, `server error 503`.
pub fn failure(reason: LinkFailure) -> String {
    match reason {
        LinkFailure::Unreachable => "server unreachable".to_owned(),
        LinkFailure::TimedOut => "timed out".to_owned(),
        LinkFailure::ConnectionLost => "connection lost".to_owned(),
        LinkFailure::Handshake => "TLS handshake failed".to_owned(),
        LinkFailure::ServerRestarted => "server restarted".to_owned(),
        LinkFailure::ServerError(status) => format!("server error {status}"),
        LinkFailure::BadResponse => "unreadable server reply".to_owned(),
        LinkFailure::Storage => "local storage error".to_owned(),
    }
}

/// What a head says about a rejection that
/// [`needs_repair`](LinkRejection::needs_repair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairNotice {
    /// For the banner, lowercase, before the head's "re-pair" hint:
    /// `server identity changed`.
    pub short: &'static str,
    /// The notice's headline, a sentence.
    pub title: &'static str,
    /// What happened and what to do, in sentences.
    pub body: &'static str,
}

/// The re-pair notice for `rejection`; `None` for one that pairing again
/// would not fix (a protocol mismatch, a log that lost history).
pub fn repair_notice(rejection: LinkRejection) -> Option<RepairNotice> {
    let notice = match rejection {
        LinkRejection::Revoked => RepairNotice {
            short: "device revoked",
            title: "This device was revoked.",
            body: "The server no longer accepts this device. Local data stays readable.",
        },
        LinkRejection::ServerChanged => RepairNotice {
            short: "server identity changed",
            title: "The server's identity changed.",
            body: "The server's certificate is not from the authority this device \
                   trusted when it paired: the server's certificate authority was \
                   created anew, or something else answers at this address. Re-pair \
                   with what `skywaved pair` prints now. Local data stays readable.",
        },
        LinkRejection::NameMismatch => RepairNotice {
            short: "server name mismatch",
            title: "The server's certificate does not cover this address.",
            body: "The server's certificate no longer lists the name this device \
                   connects to: its host name changed, or the name was dropped from \
                   its configuration. Change the server address in Settings to a \
                   name the server lists, add the name back on the server, or \
                   re-pair. Local data stays readable.",
        },
        LinkRejection::BadIdentity => RepairNotice {
            short: "device identity unusable",
            title: "This device's identity is damaged.",
            body: "The certificate or key this device stored when it paired cannot \
                   be read. Re-pair to get a new one. Local data stays readable.",
        },
        LinkRejection::ProtocolMismatch { .. } | LinkRejection::HistoryLost { .. } => {
            return None;
        }
    };
    Some(notice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rejection_that_needs_a_repair_has_a_notice() {
        let all = [
            LinkRejection::Revoked,
            LinkRejection::ServerChanged,
            LinkRejection::NameMismatch,
            LinkRejection::BadIdentity,
            LinkRejection::ProtocolMismatch {
                client: 1,
                server: 2,
            },
            LinkRejection::HistoryLost {
                have: skywave_core::Seq(2),
                server: skywave_core::Seq(1),
            },
        ];
        for rejection in all {
            assert_eq!(
                repair_notice(rejection).is_some(),
                rejection.needs_repair(),
                "{rejection:?}"
            );
        }
        assert_eq!(
            repair_notice(LinkRejection::ServerChanged).map(|n| n.short),
            Some("server identity changed")
        );
    }

    #[test]
    fn a_failure_reads_short() {
        assert_eq!(failure(LinkFailure::Unreachable), "server unreachable");
        assert_eq!(failure(LinkFailure::ServerError(503)), "server error 503");
        assert_eq!(failure(LinkFailure::ServerRestarted), "server restarted");
    }
}
