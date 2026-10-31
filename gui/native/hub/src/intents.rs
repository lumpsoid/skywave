//! Plain intents as core [`Action`]s: ids parsed from the strings Dart
//! sends. A malformed id is an error for Dart to show, never a panic.

use skywave_core::{Action, BlobPolicy, Decision, Route};
use uuid::Uuid;

use crate::signals::{ActionIntent, BlobPolicyChoice, DecisionView, RouteView};

/// `intent` as a core action, or why it is not one.
pub fn action(intent: ActionIntent) -> Result<Action, String> {
    Ok(match intent {
        ActionIntent::Discard { command_id } => Action::Discard(id("command", &command_id)?),
        ActionIntent::EditQueued { command_id } => Action::EditQueued(id("command", &command_id)?),
        ActionIntent::ResolveApproval {
            approval_id,
            decision,
            rule,
        } => Action::ResolveApproval {
            id: id("approval", &approval_id)?,
            decision: match decision {
                DecisionView::Approve => Decision::Approve,
                DecisionView::Deny => Decision::Deny,
            },
            rule: rule
                .map(|rule| u8::try_from(rule).map_err(|_| format!("no rule {rule}")))
                .transpose()?,
        },
        ActionIntent::Interrupt { session_id } => Action::Interrupt {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::StopSession { session_id } => Action::StopSession {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::SetSessionArchived {
            session_id,
            archived,
        } => Action::SetSessionArchived {
            session_id: id("session", &session_id)?,
            archived,
        },
        ActionIntent::DeleteSessionLocally { session_id } => Action::DeleteSessionLocally {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::OpenSession { session_id } => {
            Action::OpenSession(id("session", &session_id)?)
        }
        ActionIntent::OpenNextApproval => Action::OpenNextApproval,
        ActionIntent::DownloadBlob { blob_id } => Action::DownloadBlob(id("blob", &blob_id)?),
        ActionIntent::SetBlobPolicy { policy } => Action::SetBlobPolicy(match policy {
            BlobPolicyChoice::Eager => BlobPolicy::Eager,
            BlobPolicyChoice::OnDemand => BlobPolicy::OnDemand,
        }),
        ActionIntent::RetryNow => Action::RetryNow,
        ActionIntent::Navigate { route } => Action::Navigate(match route {
            RouteView::SessionList => Route::SessionList,
            RouteView::ArchivedSessions => Route::ArchivedSessions,
            RouteView::Settings => Route::Settings,
            RouteView::Pairing => Route::Pairing,
        }),
        ActionIntent::Back => Action::Back,
    })
}

/// A UUID id of the kind `what`, as core's id types take it.
pub fn id<T: From<Uuid>>(what: &str, text: &str) -> Result<T, String> {
    Uuid::parse_str(text)
        .map(T::from)
        .map_err(|_| format!("not a {what} id: {text:?}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use skywave_core::SessionId;

    use super::*;

    #[test]
    fn ids_parse_and_malformed_ones_are_errors() {
        let session = SessionId::new();
        assert_eq!(
            action(ActionIntent::OpenSession {
                session_id: session.0.to_string(),
            }),
            Ok(Action::OpenSession(session))
        );
        assert_eq!(
            action(ActionIntent::Interrupt {
                session_id: "nope".into(),
            }),
            Err("not a session id: \"nope\"".to_owned())
        );
        assert!(
            action(ActionIntent::ResolveApproval {
                approval_id: uuid::Uuid::now_v7().to_string(),
                decision: DecisionView::Approve,
                rule: Some(300),
            })
            .is_err()
        );
    }
}
