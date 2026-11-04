//! Reading a session's workdir against a real `skywaved` (HTTPS API and
//! agent supervisor with the fake agent) in-process on 127.0.0.1 over real
//! TLS, on a temporary git repository: reads on request; with the engine
//! suspended what was read still shows from memory, and after a restart
//! with no server to reach it shows from the replica.
//!
//! Waits are bounded and driven by the client's `ViewState` snapshots.

mod support;

use std::path::Path;

use skywave_core::{
    Action, BlobState, ChangedFile, ChangesResult, Connector, DiffBody, EngineConfig, FileChange,
    Screen, TransportError, ViewState, WorkdirBody, WorkdirFetch, WorkdirPane, WorkdirView,
};
use skywaved::agent::fake::{FakeAgent, FakeScript};
use support::{Client, Daemon, config};

/// Runs git for test setup in `dir`, immune to the user's config.
fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"));
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn workdir(view: &ViewState) -> Option<&WorkdirView> {
    match &view.screen {
        Screen::Workdir(view) => Some(view),
        _ => None,
    }
}

/// The stored changes' entry for `a.txt`, once the read is settled.
fn changed(view: &ViewState) -> Option<ChangedFile> {
    let view = workdir(view).filter(|v| v.fetch == WorkdirFetch::Idle)?;
    match &view.body {
        WorkdirBody::Changes(ChangesResult::Changes(set)) => {
            set.files.iter().find(|f| f.path == "a.txt").cloned()
        }
        _ => None,
    }
}

fn diff_id(file: &ChangedFile) -> skywave_core::BlobId {
    match &file.diff {
        DiffBody::Blob(blob) => blob.id,
        other => panic!("expected a diff blob, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn workdir_reads_on_request_and_shows_what_it_read_while_suspended() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let dir = Path::new(&daemon.workdir()).to_owned();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&dir, &["add", "a.txt"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.join("a.txt"), "two\n").unwrap();

    let client = Client::open(config());
    daemon.pair(&client, &daemon.addr.to_string()).await;
    let session = client.create_session(&daemon).await;

    // Opening reads the changes.
    client.dispatch(Action::OpenWorkdir {
        session_id: session,
    });
    let view = client
        .wait("the changes", |_, view| changed(view).is_some())
        .await;
    let first = changed(&view).unwrap();
    assert_eq!(first.change, FileChange::Modified);

    // A refresh reads them again: new content, a new diff.
    std::fs::write(dir.join("a.txt"), "three\n").unwrap();
    client.dispatch(Action::RefreshWorkdir);
    let view = client
        .wait("the refreshed changes", |_, view| {
            changed(view).is_some_and(|f| diff_id(&f) != diff_id(&first))
        })
        .await;
    let file = changed(&view).unwrap();

    // Opening the diff downloads it.
    client.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
        path: "a.txt".into(),
        untracked: false,
    }));
    let available = |view: &ViewState| match workdir(view).map(|v| &v.body) {
        Some(WorkdirBody::Diff {
            file: Some(shown),
            blob: Some(blob),
        }) if *shown == file => match &blob.state {
            BlobState::Available { path } => Some(path.clone()),
            _ => None,
        },
        _ => None,
    };
    let view = client
        .wait("the diff downloaded", |_, view| available(view).is_some())
        .await;
    let path = available(&view).unwrap();
    let diff = std::fs::read_to_string(&path).unwrap();
    assert!(diff.contains("+three"), "{diff}");

    // Suspended, it still shows; a refresh fails at once and keeps it.
    client.core.suspend();
    client.dispatch(Action::RefreshWorkdir);
    let view = client
        .wait("the refresh refused", |_, view| {
            workdir(view).is_some_and(|v| matches!(v.fetch, WorkdirFetch::Failed(_)))
        })
        .await;
    assert_eq!(available(&view), Some(path.clone()));
    client.dispatch(Action::ShowWorkdir(WorkdirPane::Changes));
    assert_eq!(changed(&client.core.view()), None, "the refresh failed");
    let shown = client.core.view();
    let Some(WorkdirBody::Changes(ChangesResult::Changes(set))) = workdir(&shown).map(|v| &v.body)
    else {
        panic!("expected the stored changes, got {shown:?}");
    };
    assert_eq!(set.files.iter().find(|f| f.path == "a.txt"), Some(&file));

    // Restarted with no server to reach: the replica alone shows the
    // stored diff and its downloaded blob, and nothing is asked.
    let dir = client.stop();
    let client = Client::open_in(
        dir,
        EngineConfig {
            connector: Connector::new(|_, _, _, _| {
                Err(TransportError::Unreachable("no network".into()))
            }),
            ..config()
        },
    );
    client.dispatch(Action::OpenWorkdir {
        session_id: session,
    });
    client.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
        path: "a.txt".into(),
        untracked: false,
    }));
    let view = client.core.view();
    assert_eq!(workdir(&view).map(|v| &v.fetch), Some(&WorkdirFetch::Idle));
    assert_eq!(available(&view), Some(path.clone()));
    assert!(std::fs::read_to_string(&path).unwrap().contains("+three"));
}
