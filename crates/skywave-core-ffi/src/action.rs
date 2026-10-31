//! Mirrors of [`skywave_core::Action`] and [`skywave_core::Route`]. A head
//! builds an [`Action`] and the core takes it through `TryFrom`, which fails
//! with [`SkywaveError::InvalidId`] on an id that is not a UUID (D16). Core → FFI
//! (`From`) exists too, so actions round-trip.
//!
//! Tuple variants become named fields (`Discard { command_id }`,
//! `OpenSession { session_id }`, ...) so Kotlin reads naturally.

use skywave_core as sc;
use skywave_present::utf16::{chars_to_utf16, utf16_to_chars};
use uuid::Uuid;

use crate::error::SkywaveError;
use crate::view::{BlobPolicy, Decision, PermissionMode, id};

/// A user intent for the core.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Action {
    /// Pair with a server using what `skywaved pair` printed; also re-pairs.
    Pair {
        address: String,
        code: String,
        fingerprint: String,
    },
    /// Start a session in a server directory under a configured root.
    CreateSession {
        workdir: String,
    },
    /// Complete `input` as a workdir; sent on Tab and on every edit while
    /// candidates are shown.
    CompleteWorkdir {
        input: String,
    },
    /// The new-session prompt closed.
    EndCompletion,
    SendMessage {
        session_id: String,
        text: String,
    },
    /// Keep the session's unsent message; blank text drops it.
    /// `cursor_utf16` is the position in `text` in UTF-16 code units (a
    /// Kotlin `TextRange` offset); inside a surrogate pair rounds down, past
    /// the end clamps. Save on pause or leave, not per keystroke.
    SaveDraft {
        session_id: String,
        text: String,
        cursor_utf16: u64,
    },
    /// Take back or dismiss a `discardable` message or session creation.
    Discard {
        command_id: String,
    },
    /// Take back a `discardable` message into its session's draft.
    EditQueued {
        command_id: String,
    },
    /// Answer an approval; `rule` grants the prompt's suggestion with that
    /// index along with an approval.
    ResolveApproval {
        id: String,
        decision: Decision,
        rule: Option<u8>,
    },
    /// Stop the agent's current turn.
    Interrupt {
        session_id: String,
    },
    /// Shut down the session's agent; the next message starts it again.
    StopSession {
        session_id: String,
    },
    /// Name the session; a blank `name` restores its default title.
    RenameSession {
        session_id: String,
        name: String,
    },
    /// Move the session to the archived list (`true`) or back (`false`).
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    /// Ask the session's agent to run in `mode`; the session view's `agent`
    /// shows the switch until the agent answers.
    SetPermissionMode {
        session_id: String,
        mode: PermissionMode,
    },
    /// Ask the session's agent to run `model`: the `id` of one the view's
    /// `agent.models` offers, or a name. A blank one is a no-op.
    SetModel {
        session_id: String,
        model: String,
    },
    /// Delete a `deletable` session from this device only.
    DeleteSessionLocally {
        session_id: String,
    },
    OpenSession {
        session_id: String,
    },
    /// Open the session of the next approval outside the open session.
    OpenNextApproval,
    /// The user has seen the open session through `through`.
    MarkRead {
        session_id: String,
        through: u64,
    },
    /// Fetch a blob's content.
    DownloadBlob {
        blob_id: String,
    },
    SetBlobPolicy {
        policy: BlobPolicy,
    },
    /// While `Reconnecting`, reconnect now.
    RetryNow,
    Navigate {
        route: Route,
    },
    /// Go back from the screen shown; a no-op where `ViewState.can_go_back`
    /// is false.
    Back,
}

/// Screens reachable without a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Route {
    SessionList,
    ArchivedSessions,
    Settings,
    Pairing,
}

pub(crate) fn parse<T: From<Uuid>>(name: &str, input: String) -> Result<T, SkywaveError> {
    match Uuid::parse_str(&input) {
        Ok(uuid) => Ok(T::from(uuid)),
        Err(_) => Err(SkywaveError::InvalidId {
            name: name.to_owned(),
            input,
        }),
    }
}

impl TryFrom<Action> for sc::Action {
    type Error = SkywaveError;

    fn try_from(a: Action) -> Result<Self, SkywaveError> {
        Ok(match a {
            Action::Pair {
                address,
                code,
                fingerprint,
            } => sc::Action::Pair {
                address,
                code,
                fingerprint,
            },
            Action::CreateSession { workdir } => sc::Action::CreateSession { workdir },
            Action::CompleteWorkdir { input } => sc::Action::CompleteWorkdir { input },
            Action::EndCompletion => sc::Action::EndCompletion,
            Action::SendMessage { session_id, text } => sc::Action::SendMessage {
                session_id: parse("session_id", session_id)?,
                text,
            },
            Action::SaveDraft {
                session_id,
                text,
                cursor_utf16,
            } => sc::Action::SaveDraft {
                session_id: parse("session_id", session_id)?,
                cursor: utf16_to_chars(&text, cursor_utf16),
                text,
            },
            Action::Discard { command_id } => sc::Action::Discard(parse("command_id", command_id)?),
            Action::EditQueued { command_id } => {
                sc::Action::EditQueued(parse("command_id", command_id)?)
            }
            Action::ResolveApproval { id, decision, rule } => sc::Action::ResolveApproval {
                id: parse("id", id)?,
                decision: decision.into(),
                rule,
            },
            Action::Interrupt { session_id } => sc::Action::Interrupt {
                session_id: parse("session_id", session_id)?,
            },
            Action::StopSession { session_id } => sc::Action::StopSession {
                session_id: parse("session_id", session_id)?,
            },
            Action::RenameSession { session_id, name } => sc::Action::RenameSession {
                session_id: parse("session_id", session_id)?,
                name,
            },
            Action::SetSessionArchived {
                session_id,
                archived,
            } => sc::Action::SetSessionArchived {
                session_id: parse("session_id", session_id)?,
                archived,
            },
            Action::SetPermissionMode { session_id, mode } => sc::Action::SetPermissionMode {
                session_id: parse("session_id", session_id)?,
                mode: mode.into(),
            },
            Action::SetModel { session_id, model } => sc::Action::SetModel {
                session_id: parse("session_id", session_id)?,
                model,
            },
            Action::DeleteSessionLocally { session_id } => sc::Action::DeleteSessionLocally {
                session_id: parse("session_id", session_id)?,
            },
            Action::OpenSession { session_id } => {
                sc::Action::OpenSession(parse("session_id", session_id)?)
            }
            Action::OpenNextApproval => sc::Action::OpenNextApproval,
            Action::MarkRead {
                session_id,
                through,
            } => sc::Action::MarkRead {
                session_id: parse("session_id", session_id)?,
                through: sc::Seq(through),
            },
            Action::DownloadBlob { blob_id } => {
                sc::Action::DownloadBlob(parse("blob_id", blob_id)?)
            }
            Action::SetBlobPolicy { policy } => sc::Action::SetBlobPolicy(policy.into()),
            Action::RetryNow => sc::Action::RetryNow,
            Action::Navigate { route } => sc::Action::Navigate(route.into()),
            Action::Back => sc::Action::Back,
        })
    }
}

impl From<sc::Action> for Action {
    fn from(a: sc::Action) -> Self {
        match a {
            sc::Action::Pair {
                address,
                code,
                fingerprint,
            } => Action::Pair {
                address,
                code,
                fingerprint,
            },
            sc::Action::CreateSession { workdir } => Action::CreateSession { workdir },
            sc::Action::CompleteWorkdir { input } => Action::CompleteWorkdir { input },
            sc::Action::EndCompletion => Action::EndCompletion,
            sc::Action::SendMessage { session_id, text } => Action::SendMessage {
                session_id: id(session_id.0),
                text,
            },
            sc::Action::SaveDraft {
                session_id,
                text,
                cursor,
            } => Action::SaveDraft {
                session_id: id(session_id.0),
                cursor_utf16: chars_to_utf16(&text, cursor),
                text,
            },
            sc::Action::Discard(command_id) => Action::Discard {
                command_id: id(command_id.0),
            },
            sc::Action::EditQueued(command_id) => Action::EditQueued {
                command_id: id(command_id.0),
            },
            sc::Action::ResolveApproval {
                id: approval,
                decision,
                rule,
            } => Action::ResolveApproval {
                id: id(approval.0),
                decision: decision.into(),
                rule,
            },
            sc::Action::Interrupt { session_id } => Action::Interrupt {
                session_id: id(session_id.0),
            },
            sc::Action::StopSession { session_id } => Action::StopSession {
                session_id: id(session_id.0),
            },
            sc::Action::RenameSession { session_id, name } => Action::RenameSession {
                session_id: id(session_id.0),
                name,
            },
            sc::Action::SetSessionArchived {
                session_id,
                archived,
            } => Action::SetSessionArchived {
                session_id: id(session_id.0),
                archived,
            },
            sc::Action::SetPermissionMode { session_id, mode } => Action::SetPermissionMode {
                session_id: id(session_id.0),
                mode: mode.into(),
            },
            sc::Action::SetModel { session_id, model } => Action::SetModel {
                session_id: id(session_id.0),
                model,
            },
            sc::Action::DeleteSessionLocally { session_id } => Action::DeleteSessionLocally {
                session_id: id(session_id.0),
            },
            sc::Action::OpenSession(session_id) => Action::OpenSession {
                session_id: id(session_id.0),
            },
            sc::Action::OpenNextApproval => Action::OpenNextApproval,
            sc::Action::MarkRead {
                session_id,
                through,
            } => Action::MarkRead {
                session_id: id(session_id.0),
                through: through.0,
            },
            sc::Action::DownloadBlob(blob_id) => Action::DownloadBlob {
                blob_id: id(blob_id.0),
            },
            sc::Action::SetBlobPolicy(policy) => Action::SetBlobPolicy {
                policy: policy.into(),
            },
            sc::Action::RetryNow => Action::RetryNow,
            sc::Action::Navigate(route) => Action::Navigate {
                route: route.into(),
            },
            sc::Action::Back => Action::Back,
        }
    }
}

impl From<Route> for sc::Route {
    fn from(r: Route) -> Self {
        match r {
            Route::SessionList => sc::Route::SessionList,
            Route::ArchivedSessions => sc::Route::ArchivedSessions,
            Route::Settings => sc::Route::Settings,
            Route::Pairing => sc::Route::Pairing,
        }
    }
}

impl From<sc::Route> for Route {
    fn from(r: sc::Route) -> Self {
        match r {
            sc::Route::SessionList => Route::SessionList,
            sc::Route::ArchivedSessions => Route::ArchivedSessions,
            sc::Route::Settings => Route::Settings,
            sc::Route::Pairing => Route::Pairing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_core_action() -> Vec<sc::Action> {
        let session_id = sc::SessionId::new();
        vec![
            sc::Action::Pair {
                address: "host:1".into(),
                code: "c".into(),
                fingerprint: "f".into(),
            },
            sc::Action::CreateSession {
                workdir: "/w".into(),
            },
            sc::Action::CompleteWorkdir { input: "/w".into() },
            sc::Action::EndCompletion,
            sc::Action::SendMessage {
                session_id,
                text: "hi".into(),
            },
            sc::Action::SaveDraft {
                session_id,
                text: "héllo".into(),
                cursor: 2,
            },
            sc::Action::Discard(sc::CommandId::new()),
            sc::Action::EditQueued(sc::CommandId::new()),
            sc::Action::ResolveApproval {
                id: sc::ApprovalId::new(),
                decision: sc::Decision::Approve,
                rule: Some(1),
            },
            sc::Action::Interrupt { session_id },
            sc::Action::StopSession { session_id },
            sc::Action::RenameSession {
                session_id,
                name: "n".into(),
            },
            sc::Action::SetSessionArchived {
                session_id,
                archived: true,
            },
            sc::Action::SetPermissionMode {
                session_id,
                mode: sc::PermissionMode::Plan,
            },
            sc::Action::SetModel {
                session_id,
                model: "opus".into(),
            },
            sc::Action::DeleteSessionLocally { session_id },
            sc::Action::OpenSession(session_id),
            sc::Action::OpenNextApproval,
            sc::Action::MarkRead {
                session_id,
                through: sc::Seq(42),
            },
            sc::Action::DownloadBlob(sc::BlobId::new()),
            sc::Action::SetBlobPolicy(sc::BlobPolicy::Eager),
            sc::Action::RetryNow,
            sc::Action::Navigate(sc::Route::ArchivedSessions),
            sc::Action::Back,
        ]
    }

    #[test]
    fn every_action_round_trips() {
        for action in every_core_action() {
            let mirror = Action::from(action.clone());
            assert_eq!(sc::Action::try_from(mirror), Ok(action));
        }
    }

    #[test]
    fn a_utf16_draft_cursor_round_trips_through_core_chars() {
        let session_id = sc::SessionId::new();
        let text = "a😀é".to_owned();
        let core = sc::Action::try_from(Action::SaveDraft {
            session_id: session_id.0.to_string(),
            text: text.clone(),
            cursor_utf16: 3,
        });
        assert_eq!(
            core,
            Ok(sc::Action::SaveDraft {
                session_id,
                text: text.clone(),
                cursor: 2,
            })
        );
        let draft = crate::view::Draft::from(sc::Draft {
            text: text.clone(),
            cursor: 2,
        });
        assert_eq!(draft.cursor_utf16, 3);
        assert_eq!(
            Action::from(core.unwrap()),
            Action::SaveDraft {
                session_id: session_id.0.to_string(),
                text,
                cursor_utf16: 3,
            }
        );
    }

    #[test]
    fn a_save_draft_cursor_past_the_end_clamps() {
        let session_id = sc::SessionId::new();
        let core = sc::Action::try_from(Action::SaveDraft {
            session_id: session_id.0.to_string(),
            text: "😀".into(),
            cursor_utf16: u64::MAX,
        });
        assert_eq!(
            core,
            Ok(sc::Action::SaveDraft {
                session_id,
                text: "😀".into(),
                cursor: 1,
            })
        );
    }

    #[test]
    fn action_ids_cross_as_uuid_strings() {
        let session_id = sc::SessionId::new();
        let mirror = Action::MarkRead {
            session_id: session_id.0.to_string(),
            through: 7,
        };
        assert_eq!(
            sc::Action::try_from(mirror),
            Ok(sc::Action::MarkRead {
                session_id,
                through: sc::Seq(7),
            })
        );
        assert_eq!(
            Action::from(sc::Action::OpenSession(session_id)),
            Action::OpenSession {
                session_id: session_id.0.to_string()
            }
        );
    }

    #[test]
    fn a_bad_id_is_a_typed_error() {
        let bad = Action::ResolveApproval {
            id: "not-a-uuid".into(),
            decision: Decision::Deny,
            rule: None,
        };
        let err = sc::Action::try_from(bad).unwrap_err();
        assert_eq!(
            err,
            SkywaveError::InvalidId {
                name: "id".into(),
                input: "not-a-uuid".into(),
            }
        );
        assert_eq!(err.to_string(), "id is not a valid id: \"not-a-uuid\"");

        let empty = Action::DownloadBlob {
            blob_id: String::new(),
        };
        assert!(matches!(
            sc::Action::try_from(empty),
            Err(SkywaveError::InvalidId { name, .. }) if name == "blob_id"
        ));
    }

    #[test]
    fn route_converts_every_variant_both_ways() {
        let core = [
            sc::Route::SessionList,
            sc::Route::ArchivedSessions,
            sc::Route::Settings,
            sc::Route::Pairing,
        ];
        let mirror = core.map(Route::from);
        assert_eq!(
            mirror,
            [
                Route::SessionList,
                Route::ArchivedSessions,
                Route::Settings,
                Route::Pairing,
            ]
        );
        assert_eq!(mirror.map(sc::Route::from), core);
    }
}
