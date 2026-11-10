//! The conversation picker on the session list: the server agent's
//! conversations, asked for when the picker opens, to continue one in a new
//! session (`CreateSession` with its id as `conversation`).
//!
//! Only the latest request's answer is kept; an older one, or one that
//! arrives after the picker closed, is dropped. Offline the picker is
//! unavailable at once and nothing is asked later. Everything here is in
//! memory: a listing of another machine's terminal sessions goes stale at
//! once and is cheap to ask for again.

use skywave_proto::{ConversationsResult, OutsideConversation};

use crate::projection::SessionSummary;
use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{
    ConversationItem, ConversationPicker, ConversationsStatus, ConversationsUnavailable,
};

/// The picker while the session list is on screen; the list's place holds
/// it, so leaving the list drops it. Request ids come from the model,
/// which keeps them unused across visits.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Conversations {
    /// The open picker; `None` while it is closed.
    open: Option<Listing>,
}

#[derive(Debug, Clone, PartialEq)]
struct Listing {
    /// The request this listing waits on or was answered by; `None` when
    /// nothing was asked (offline).
    req: Option<u64>,
    status: ConversationsStatus,
    items: Vec<OutsideConversation>,
}

impl Conversations {
    /// Opens (or reopens) the picker; returns the listing to ask the
    /// server for, its id taken from `last_req` (the latest id handed out,
    /// never reused). Offline (`online` false) it is unavailable at once.
    pub(crate) fn list(&mut self, online: bool, last_req: &mut u64) -> Option<Request> {
        if !online {
            self.open = Some(Listing {
                req: None,
                status: ConversationsStatus::Unavailable(ConversationsUnavailable::Offline),
                items: Vec::new(),
            });
            return None;
        }
        *last_req += 1;
        let req = *last_req;
        self.open = Some(Listing {
            req: Some(req),
            status: ConversationsStatus::Loading,
            items: Vec::new(),
        });
        Some(Request::ListConversations { req })
    }

    /// The picker closed: forget its listing.
    pub(crate) fn end(&mut self) {
        self.open = None;
    }

    /// The server's answer to request `req`; a stale one is ignored.
    pub(crate) fn listed(&mut self, req: u64, result: Result<ConversationsResult, TransportError>) {
        let Some(listing) = self.open.as_mut().filter(|l| l.req == Some(req)) else {
            return;
        };
        let unavailable = |why| (ConversationsStatus::Unavailable(why), Vec::new());
        (listing.status, listing.items) = match result {
            Ok(ConversationsResult::Listed {
                conversations,
                truncated,
            }) => (ConversationsStatus::Ready { truncated }, conversations),
            Ok(ConversationsResult::Unavailable) => {
                unavailable(ConversationsUnavailable::AgentCannotList)
            }
            // The engine's answer while the link is down, or a server that
            // could not be reached.
            Err(TransportError::Unreachable(_)) => unavailable(ConversationsUnavailable::Offline),
            Err(error) => {
                tracing::debug!(%error, "listing conversations failed");
                unavailable(ConversationsUnavailable::Failed(error.to_string()))
            }
        };
    }

    /// The open picker, its items resolved against the replica's
    /// `sessions`, most recently active first.
    pub(crate) fn view(&self, sessions: &[SessionSummary]) -> Option<ConversationPicker> {
        let listing = self.open.as_ref()?;
        let items = listing
            .items
            .iter()
            .map(|c| ConversationItem {
                id: c.id.clone(),
                workdir: c.workdir.clone(),
                title: c.title.clone(),
                first_prompt: c.first_prompt.clone(),
                last_active: c.last_active,
                messages: c.messages,
                session: c.session.filter(|id| sessions.iter().any(|s| s.id == *id)),
                continued_in: sessions
                    .iter()
                    .filter(|s| s.adopted.as_deref() == Some(c.id.as_str()))
                    .map(|s| s.id)
                    .collect(),
            })
            .collect();
        Some(ConversationPicker {
            status: listing.status.clone(),
            items,
        })
    }
}
