//! The system notification ids Dart shows and cancels by: one approval
//! notification and one shell notice per session, so two sessions waiting
//! at once each keep theirs, and one answered anywhere can be taken down
//! without touching the other.
//!
//! An id is the session id hashed (FNV-1a, so the same in every process:
//! a notification an earlier run left up is still taken down when its
//! session is answered), moved on to the next free one if another session
//! of this run already has it. Approval ids are even, shell ids odd, and
//! all fit an Android `int` (positive `i32`).

use std::collections::HashMap;

use skywave_core::SessionId;

/// Which notification of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Approval,
    Shell,
}

/// The notification saying the hub stopped off screen: negative, apart
/// from every session's, and not -1, the background service's
/// (`StayConnectedService.NOTIFICATION_ID`).
pub const STOPPED: i32 = -2;

/// Slots are 30 bits, so a slot doubled (plus one) stays a positive `i32`.
const SLOT_MASK: i32 = (1 << 30) - 1;

/// Each session's slot, for this run. Slots are never given back, so an id
/// means one session for the hub's lifetime.
#[derive(Debug, Default)]
pub struct NotificationIds {
    slots: HashMap<SessionId, i32>,
    taken: HashMap<i32, SessionId>,
}

impl NotificationIds {
    /// `session`'s notification id of `kind`.
    pub fn id(&mut self, session: SessionId, kind: Kind) -> i32 {
        let slot = match self.slots.get(&session) {
            Some(slot) => *slot,
            None => {
                let mut slot = hash(session);
                while self.taken.contains_key(&slot) {
                    slot = (slot + 1) & SLOT_MASK;
                }
                self.slots.insert(session, slot);
                self.taken.insert(slot, session);
                slot
            }
        };
        id_of(slot, kind)
    }
}

/// The id of `kind` in `slot`.
fn id_of(slot: i32, kind: Kind) -> i32 {
    slot * 2
        + match kind {
            Kind::Approval => 0,
            Kind::Shell => 1,
        }
}

/// FNV-1a over the session id's bytes, cut to a slot.
fn hash(session: SessionId) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in session.0.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    i32::from_ne_bytes(hash.to_ne_bytes()) & SLOT_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_per_session_and_kind() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut ids = NotificationIds::default();
        let approval = ids.id(a, Kind::Approval);
        assert_eq!(approval % 2, 0);
        assert_eq!(ids.id(a, Kind::Shell), approval + 1);
        assert_eq!(ids.id(a, Kind::Approval), approval);
        assert_ne!(ids.id(b, Kind::Approval), approval);
        // The same in a later run.
        assert_eq!(NotificationIds::default().id(a, Kind::Approval), approval);
    }

    #[test]
    fn a_colliding_session_takes_the_next_free_slot() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut ids = NotificationIds::default();
        // Force `b` onto `a`'s slot as a hash collision would.
        let slot = hash(a);
        ids.taken.insert(slot, b);
        ids.slots.insert(b, slot);
        let id = ids.id(a, Kind::Approval);
        assert_eq!(id, id_of((slot + 1) & SLOT_MASK, Kind::Approval));
        assert_ne!(id, ids.id(b, Kind::Approval));
        // The last slot's ids still fit, and probing past it wraps to 0.
        assert_eq!(id_of(SLOT_MASK, Kind::Shell), i32::MAX);
        assert_eq!((SLOT_MASK + 1) & SLOT_MASK, 0);
    }
}
