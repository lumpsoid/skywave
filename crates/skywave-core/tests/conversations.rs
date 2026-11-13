//! The conversation picker against a real `skywaved` (HTTPS API with a fake
//! conversation source, agent supervisor with the fake agent) in-process on
//! 127.0.0.1 over real TLS: the engine asks on the side transport, and a
//! conversation continued from the picker shows as continued once its
//! session is in the replica.
//!
//! Waits are bounded and driven by the client's `ViewState` snapshots.

mod support;

use chrono::{TimeZone, Utc};
use skywave_core::{Action, ConversationPicker, ConversationsStatus, Screen, ViewState};
use skywaved::agent::fake::{FakeAgent, FakeOutside, FakeScript};
use skywaved::agent::{HistoryRead, Listed, OutsideConversation};
use support::{Client, Daemon, adopted_history, config};

fn picker(view: &ViewState) -> Option<&ConversationPicker> {
    match &view.screen {
        Screen::SessionList(list) => list.conversations.as_ref(),
        _ => None,
    }
}

fn ready(view: &ViewState) -> Option<&ConversationPicker> {
    picker(view).filter(|p| matches!(p.status, ConversationsStatus::Ready { .. }))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_picked_and_continued_shows_as_continued() {
    let outside = FakeOutside::new();
    let daemon =
        Daemon::start_with_outside(FakeAgent::new(FakeScript::new()), outside.clone()).await;
    let workdir = daemon.canonical_workdir();
    outside.listing(Ok(Listed {
        conversations: vec![OutsideConversation {
            id: "conv-1".into(),
            workdir: workdir.display().to_string(),
            title: Some("Fix the build".into()),
            first_prompt: None,
            last_active: Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap(),
            messages: 3,
            session: None,
        }],
        truncated: false,
    }));
    outside.conversation(
        &workdir,
        "conv-1",
        HistoryRead::Read {
            items: adopted_history(1, 10),
        },
    );
    let client = Client::open(config());
    daemon.pair(&client, &daemon.addr.to_string()).await;

    client.dispatch(Action::ListConversations);
    let view = client
        .wait("the listing", |_, view| ready(view).is_some())
        .await;
    let listed = ready(&view).unwrap();
    assert_eq!(
        listed.status,
        ConversationsStatus::Ready { truncated: false }
    );
    let [pick] = listed.items.as_slice() else {
        panic!("expected one conversation, got {:?}", listed.items);
    };
    assert_eq!(pick.continued_in, Vec::new());
    assert_eq!(outside.lists(), 1);

    client.dispatch(Action::CreateSession {
        workdir: pick.workdir.clone(),
        conversation: Some(pick.id.clone()),
        worktree: None,
    });
    let view = client
        .wait("the adopted session", |client, view| {
            picker(view).is_none() && !client.sessions().is_empty()
        })
        .await;
    assert_eq!(picker(&view), None, "continuing ends the picker");
    let session = client.sessions()[0];

    client.dispatch(Action::ListConversations);
    let view = client
        .wait("the listing again", |_, view| ready(view).is_some())
        .await;
    assert_eq!(ready(&view).unwrap().items[0].continued_in, vec![session]);
    assert_eq!(outside.lists(), 2, "asked again on reopening");
}
