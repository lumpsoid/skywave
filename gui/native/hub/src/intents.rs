//! Plain intents as core [`Action`]s: ids parsed from the strings Dart
//! sends. A malformed id is an error for Dart to show, never a panic.

use skywave_core::{
    Action, BlobPolicy, Decision, Effort, PermissionMode, Route, ServerAddress, TaskId,
};
use uuid::Uuid;

use crate::signals::{
    ActionIntent, BlobPolicyChoice, DecisionView, EffortView, PermissionModeView, RouteView,
};

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
        ActionIntent::CreateSessionLike { session_id } => {
            Action::CreateSessionLike(id("session", &session_id)?)
        }
        ActionIntent::ClearContext { session_id } => Action::ClearContext {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::StopTask {
            session_id,
            task_id,
        } => Action::StopTask {
            session_id: id("session", &session_id)?,
            // The agent's own opaque id; only an empty one is no task.
            task: match task_id.as_str() {
                "" => return Err("no task id".to_owned()),
                _ => TaskId(task_id),
            },
        },
        ActionIntent::SwitchContext {
            session_id,
            context_id,
        } => Action::SwitchContext {
            session_id: id("session", &session_id)?,
            context: id("context", &context_id)?,
        },
        ActionIntent::MoveContext {
            session_id,
            context_id,
        } => Action::MoveContext {
            session_id: id("session", &session_id)?,
            context: id("context", &context_id)?,
        },
        ActionIntent::ViewContext {
            session_id,
            context_id,
        } => Action::ViewContext {
            session_id: id("session", &session_id)?,
            context: context_id
                .map(|context| id("context", &context))
                .transpose()?,
        },
        ActionIntent::SetPermissionMode { session_id, mode } => Action::SetPermissionMode {
            session_id: id("session", &session_id)?,
            mode: permission_mode(mode),
        },
        ActionIntent::SetModel { session_id, model } => Action::SetModel {
            session_id: id("session", &session_id)?,
            model,
        },
        ActionIntent::SetEffort {
            session_id,
            effort: e,
        } => Action::SetEffort {
            session_id: id("session", &session_id)?,
            effort: effort(e),
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
        ActionIntent::EditSnippet {
            snippet_id,
            name,
            text,
        } => Action::EditSnippet {
            id: id("snippet", &snippet_id)?,
            name,
            text,
        },
        ActionIntent::DeleteSnippet { snippet_id } => {
            Action::DeleteSnippet(id("snippet", &snippet_id)?)
        }
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
        // Checked here too so a typo is shown, not silently dropped.
        ActionIntent::SetServerAddress { address } => {
            ServerAddress::parse(&address).map_err(|err| err.to_string())?;
            Action::SetServerAddress { address }
        }
        ActionIntent::Navigate { route } => Action::Navigate(match route {
            RouteView::SessionList => Route::SessionList,
            RouteView::ArchivedSessions => Route::ArchivedSessions,
            RouteView::Settings => Route::Settings,
            RouteView::Pairing => Route::Pairing,
        }),
        ActionIntent::Back => Action::Back,
    })
}

fn permission_mode(mode: PermissionModeView) -> PermissionMode {
    match mode {
        PermissionModeView::Ask => PermissionMode::Ask,
        PermissionModeView::AcceptEdits => PermissionMode::AcceptEdits,
        PermissionModeView::Plan => PermissionMode::Plan,
        PermissionModeView::Auto => PermissionMode::Auto,
        PermissionModeView::DontAsk => PermissionMode::DontAsk,
        PermissionModeView::Bypass => PermissionMode::Bypass,
    }
}

fn effort(effort: EffortView) -> Effort {
    match effort {
        EffortView::Low => Effort::Low,
        EffortView::Medium => Effort::Medium,
        EffortView::High => Effort::High,
        EffortView::XHigh => Effort::XHigh,
        EffortView::Max => Effort::Max,
    }
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
        assert_eq!(
            action(ActionIntent::SetPermissionMode {
                session_id: session.0.to_string(),
                mode: PermissionModeView::DontAsk,
            }),
            Ok(Action::SetPermissionMode {
                session_id: session,
                mode: PermissionMode::DontAsk,
            })
        );
        assert_eq!(
            action(ActionIntent::SetModel {
                session_id: session.0.to_string(),
                model: "opus".into(),
            }),
            Ok(Action::SetModel {
                session_id: session,
                model: "opus".into(),
            })
        );
        assert_eq!(
            action(ActionIntent::SetEffort {
                session_id: session.0.to_string(),
                effort: EffortView::XHigh,
            }),
            Ok(Action::SetEffort {
                session_id: session,
                effort: Effort::XHigh,
            })
        );
        let context = skywave_core::ContextId::new();
        assert_eq!(
            action(ActionIntent::SwitchContext {
                session_id: session.0.to_string(),
                context_id: context.0.to_string(),
            }),
            Ok(Action::SwitchContext {
                session_id: session,
                context
            })
        );
        assert_eq!(
            action(ActionIntent::StopTask {
                session_id: session.0.to_string(),
                task_id: "bg-1".into(),
            }),
            Ok(Action::StopTask {
                session_id: session,
                task: skywave_core::TaskId("bg-1".into()),
            })
        );
        assert_eq!(
            action(ActionIntent::StopTask {
                session_id: "nope".into(),
                task_id: "bg-1".into(),
            }),
            Err("not a session id: \"nope\"".to_owned())
        );
        assert_eq!(
            action(ActionIntent::StopTask {
                session_id: session.0.to_string(),
                task_id: String::new(),
            }),
            Err("no task id".to_owned())
        );
        assert_eq!(
            action(ActionIntent::SetServerAddress {
                address: "10.0.0.7:7443".into(),
            }),
            Ok(Action::SetServerAddress {
                address: "10.0.0.7:7443".into(),
            })
        );
        assert!(
            action(ActionIntent::SetServerAddress {
                address: "host:port".into(),
            })
            .unwrap_err()
            .contains("invalid server address")
        );
        assert_eq!(
            action(ActionIntent::ViewContext {
                session_id: session.0.to_string(),
                context_id: None,
            }),
            Ok(Action::ViewContext {
                session_id: session,
                context: None
            })
        );
        assert!(
            action(ActionIntent::ViewContext {
                session_id: session.0.to_string(),
                context_id: Some("nope".into()),
            })
            .is_err()
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
