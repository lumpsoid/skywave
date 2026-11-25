//! Plain intents as core [`Action`]s: ids parsed from the strings Dart
//! sends. A malformed id is an error for Dart to show, never a panic.

use std::path::PathBuf;

use skywave_core::{
    Action, BlobPolicy, BlobUse, ContentHash, Decision, Effort, LineSpan, LoginCode,
    PermissionMode, QuestionAnswer, Route, SafetyJudgment, ServerAddress, TaskId, WorkdirPane,
};
use skywave_present::crop::normalise;
use uuid::Uuid;

use crate::signals::{
    ActionIntent, BlobPolicyChoice, BlobUseView, DecisionView, EffortView, LineSpanView,
    PastedCode, PermissionModeView, QuestionAnswerView, RouteView, SafetyJudgmentView,
    WorkdirPaneKind,
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
        ActionIntent::AnswerQuestion {
            approval_id,
            answers,
        } => Action::AnswerQuestion {
            id: id("approval", &approval_id)?,
            answers: answers
                .into_iter()
                .map(|answer: QuestionAnswerView| {
                    let picked = answer
                        .picked
                        .into_iter()
                        .map(|i| u8::try_from(i).map_err(|_| format!("no option {i}")))
                        .collect::<Result<_, _>>()?;
                    Ok(QuestionAnswer {
                        picked,
                        other: answer.text,
                    })
                })
                .collect::<Result<_, String>>()?,
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
        ActionIntent::ViewSubagent { session_id, call } => Action::ViewSubagent {
            session_id: id("session", &session_id)?,
            call: call.map(|call| id("call", &call)).transpose()?,
        },
        ActionIntent::SetPermissionMode { session_id, mode } => Action::SetPermissionMode {
            session_id: id("session", &session_id)?,
            mode: permission_mode(mode),
        },
        ActionIntent::SetSafetyCheck { session_id, on } => Action::SetSafetyCheck {
            session_id: id("session", &session_id)?,
            on,
        },
        ActionIntent::SafetyCheckFeedback {
            session_id,
            call,
            judgment,
            note,
        } => Action::SafetyCheckFeedback {
            session_id: id("session", &session_id)?,
            call: id("call", &call)?,
            judgment: safety_judgment(judgment),
            note,
        },
        ActionIntent::SetSafetyRule {
            session_id,
            words,
            on,
        } => Action::SetSafetyRule {
            session_id: id("session", &session_id)?,
            words,
            on,
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
        ActionIntent::DeleteSessionEverywhere { session_id } => Action::DeleteSessionEverywhere {
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
        ActionIntent::OpenSearch { session_id } => Action::OpenSearch(
            session_id
                .as_deref()
                .map(|text| id("session", text))
                .transpose()?,
        ),
        ActionIntent::OpenWorkdir { session_id } => Action::OpenWorkdir {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::OpenSessionInfo { session_id } => Action::OpenSessionInfo {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::ShowWorkdir {
            pane,
            path,
            untracked,
        } => Action::ShowWorkdir(match pane {
            WorkdirPaneKind::Changes => WorkdirPane::Changes,
            WorkdirPaneKind::Commits => WorkdirPane::Commits,
            WorkdirPaneKind::Diff => WorkdirPane::Diff { path, untracked },
            WorkdirPaneKind::Dir => WorkdirPane::Dir { path },
            WorkdirPaneKind::File => WorkdirPane::File { path },
        }),
        ActionIntent::RefreshWorkdir => Action::RefreshWorkdir,
        ActionIntent::OpenNextApproval => Action::OpenNextApproval,
        ActionIntent::DownloadBlob { blob_id } => Action::DownloadBlob(id("blob", &blob_id)?),
        ActionIntent::DownloadWorkdirFile { session_id, path } => Action::DownloadWorkdirFile {
            session_id: id("session", &session_id)?,
            path,
        },
        ActionIntent::PushWorkdirFile {
            session_id,
            source,
            path,
            replace,
        } => {
            let replace: Option<ContentHash> = replace
                .map(|sha256| sha256.parse().map_err(|_| "bad file hash".to_owned()))
                .transpose()?;
            Action::PushWorkdirFile {
                session_id: id("session", &session_id)?,
                source: PathBuf::from(source),
                path,
                replace,
            }
        }
        ActionIntent::CancelWorkdirDownload { session_id, path } => Action::CancelWorkdirDownload {
            session_id: id("session", &session_id)?,
            path,
        },
        ActionIntent::CancelWorkdirPush { session_id, path } => Action::CancelWorkdirPush {
            session_id: id("session", &session_id)?,
            path,
        },
        ActionIntent::DeleteBlob { blob_id } => Action::DeleteBlob(id("blob", &blob_id)?),
        ActionIntent::CropBlob { blob_id, keep } => Action::CropBlob {
            blob: id("blob", &blob_id)?,
            keep: spans(&keep),
        },
        ActionIntent::OpenCrop {
            blob_id,
            session_id,
            holds,
            title,
        } => Action::OpenCrop {
            blob: id("blob", &blob_id)?,
            session: id("session", &session_id)?,
            holds: blob_use(holds),
            title,
        },
        ActionIntent::SetCropKeep { keep } => Action::SetCropKeep(normalise(&spans(&keep))),
        ActionIntent::SetBlobPolicy { policy } => Action::SetBlobPolicy(match policy {
            BlobPolicyChoice::Eager => BlobPolicy::Eager,
            BlobPolicyChoice::OnDemand => BlobPolicy::OnDemand,
        }),
        ActionIntent::SetDefaultMode { mode } => Action::SetDefaultMode(mode.map(permission_mode)),
        ActionIntent::SetDefaultSafetyCheck { on } => Action::SetDefaultSafetyCheck(on),
        ActionIntent::RetryNow => Action::RetryNow,
        ActionIntent::ResetReplica => Action::ResetReplica,
        ActionIntent::RefreshStorage => Action::RefreshStorage,
        ActionIntent::ReclaimServerSpace => Action::ReclaimServerSpace,
        ActionIntent::ReclaimDeviceSpace => Action::ReclaimDeviceSpace,
        ActionIntent::RefreshWorktrees => Action::RefreshWorktrees,
        ActionIntent::RefreshUsageLimits => Action::RefreshUsageLimits,
        // Checked here too so a typo is shown, not silently dropped.
        ActionIntent::SetServerAddress { address } => {
            ServerAddress::parse(&address).map_err(|err| err.to_string())?;
            Action::SetServerAddress { address }
        }
        ActionIntent::ForgetServerAddress { address } => Action::ForgetServerAddress { address },
        ActionIntent::Navigate { route } => Action::Navigate(match route {
            RouteView::SessionList => Route::SessionList,
            RouteView::ArchivedSessions => Route::ArchivedSessions,
            RouteView::AllSessions => Route::AllSessions,
            RouteView::Settings => Route::Settings,
            RouteView::ServerAddresses => Route::ServerAddresses,
            RouteView::Pairing => Route::Pairing,
            RouteView::Usage => Route::Usage,
            RouteView::Storage => Route::Storage,
            RouteView::Worktrees => Route::Worktrees,
            RouteView::UsageLimits => Route::UsageLimits,
            RouteView::Approvals => Route::Approvals,
        }),
        ActionIntent::Back => Action::Back,
        ActionIntent::StartAgentLogin => Action::StartAgentLogin,
        ActionIntent::FinishAgentLogin {
            code: PastedCode { text },
        } => Action::FinishAgentLogin {
            code: LoginCode::new(text),
        },
        ActionIntent::RetryAgentLogin => Action::RetryAgentLogin,
        ActionIntent::DismissAgentLogin => Action::DismissAgentLogin,
        ActionIntent::RemoveWorktree { session_id } => Action::RemoveWorktree {
            session_id: id("session", &session_id)?,
        },
        ActionIntent::ResendReplacing { command_id } => {
            Action::ResendReplacing(id("command", &command_id)?)
        }
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

fn safety_judgment(judgment: SafetyJudgmentView) -> SafetyJudgment {
    match judgment {
        SafetyJudgmentView::ShouldApprove => SafetyJudgment::ShouldApprove,
        SafetyJudgmentView::ShouldAsk => SafetyJudgment::ShouldAsk,
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

/// Spans as Dart sent them, as core takes them.
pub fn spans(keep: &[LineSpanView]) -> Vec<LineSpan> {
    keep.iter()
        .map(|s| LineSpan {
            first: s.first,
            count: s.count,
        })
        .collect()
}

fn blob_use(holds: BlobUseView) -> BlobUse {
    match holds {
        BlobUseView::ToolInput => BlobUse::ToolInput,
        BlobUseView::ToolBody => BlobUse::ToolBody,
        BlobUseView::ToolOutput => BlobUse::ToolOutput,
        BlobUseView::ApprovalInput => BlobUse::ApprovalInput,
        BlobUseView::ShellOutput => BlobUse::ShellOutput,
        BlobUseView::Message => BlobUse::Message,
        BlobUseView::Attachment => BlobUse::Attachment,
        BlobUseView::ScreenCapture => BlobUse::ScreenCapture,
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
    use skywave_core::{CallId, SessionId};

    use super::*;

    #[test]
    fn safety_check_feedback_maps_its_ids_and_judgment() {
        let (session, call) = (SessionId::new(), CallId::new());
        assert_eq!(
            action(ActionIntent::SafetyCheckFeedback {
                session_id: session.0.to_string(),
                call: call.0.to_string(),
                judgment: SafetyJudgmentView::ShouldAsk,
                note: Some("x".into()),
            }),
            Ok(Action::SafetyCheckFeedback {
                session_id: session,
                call,
                judgment: SafetyJudgment::ShouldAsk,
                note: Some("x".into()),
            })
        );
        assert_eq!(
            action(ActionIntent::SafetyCheckFeedback {
                session_id: session.0.to_string(),
                call: "nope".into(),
                judgment: SafetyJudgmentView::ShouldAsk,
                note: None,
            }),
            Err("not a call id: \"nope\"".to_owned())
        );
    }

    #[test]
    fn set_safety_rule_maps_its_session() {
        let session = SessionId::new();
        let words = vec!["git".to_owned(), "commit".to_owned()];
        assert_eq!(
            action(ActionIntent::SetSafetyRule {
                session_id: session.0.to_string(),
                words: words.clone(),
                on: true,
            }),
            Ok(Action::SetSafetyRule {
                session_id: session,
                words,
                on: true,
            })
        );
        assert_eq!(
            action(ActionIntent::SetSafetyRule {
                session_id: "nope".into(),
                words: Vec::new(),
                on: false,
            }),
            Err("not a session id: \"nope\"".to_owned())
        );
    }

    #[test]
    fn set_default_mode_and_safety_check_map() {
        assert_eq!(
            action(ActionIntent::SetDefaultMode {
                mode: Some(PermissionModeView::AcceptEdits),
            }),
            Ok(Action::SetDefaultMode(Some(PermissionMode::AcceptEdits)))
        );
        assert_eq!(
            action(ActionIntent::SetDefaultMode { mode: None }),
            Ok(Action::SetDefaultMode(None))
        );
        assert_eq!(
            action(ActionIntent::SetDefaultSafetyCheck { on: true }),
            Ok(Action::SetDefaultSafetyCheck(true))
        );
    }

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
            action(ActionIntent::SetSafetyCheck {
                session_id: session.0.to_string(),
                on: true,
            }),
            Ok(Action::SetSafetyCheck {
                session_id: session,
                on: true,
            })
        );
        assert_eq!(
            action(ActionIntent::SetSafetyCheck {
                session_id: "nope".into(),
                on: false,
            }),
            Err("not a session id: \"nope\"".to_owned())
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
            action(ActionIntent::ForgetServerAddress {
                address: "10.0.0.7:7443".into(),
            }),
            Ok(Action::ForgetServerAddress {
                address: "10.0.0.7:7443".into(),
            })
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
        let call = skywave_core::CallId::new();
        assert_eq!(
            action(ActionIntent::ViewSubagent {
                session_id: session.0.to_string(),
                call: Some(call.0.to_string()),
            }),
            Ok(Action::ViewSubagent {
                session_id: session,
                call: Some(call)
            })
        );
        assert_eq!(
            action(ActionIntent::ViewSubagent {
                session_id: session.0.to_string(),
                call: None,
            }),
            Ok(Action::ViewSubagent {
                session_id: session,
                call: None
            })
        );
        assert!(
            action(ActionIntent::ViewSubagent {
                session_id: session.0.to_string(),
                call: Some("nope".into()),
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
        let question = uuid::Uuid::now_v7();
        let answer = |picked: Vec<u32>, text: Option<&str>| QuestionAnswerView {
            picked,
            text: text.map(str::to_owned),
        };
        assert_eq!(
            action(ActionIntent::AnswerQuestion {
                approval_id: question.to_string(),
                answers: vec![answer(vec![0, 2], Some("salt"))],
            }),
            Ok(Action::AnswerQuestion {
                id: skywave_core::ApprovalId(question),
                answers: vec![QuestionAnswer {
                    picked: vec![0, 2],
                    other: Some("salt".into()),
                }],
            })
        );
        assert!(
            action(ActionIntent::AnswerQuestion {
                approval_id: question.to_string(),
                answers: vec![answer(vec![300], None)],
            })
            .is_err()
        );
    }

    #[test]
    fn resending_replacing_names_the_refused_command() {
        let command = skywave_core::CommandId::new();
        assert_eq!(
            action(ActionIntent::ResendReplacing {
                command_id: command.0.to_string(),
            }),
            Ok(Action::ResendReplacing(command))
        );
        assert_eq!(
            action(ActionIntent::ResendReplacing {
                command_id: "nope".into(),
            }),
            Err("not a command id: \"nope\"".to_owned())
        );
    }
}
