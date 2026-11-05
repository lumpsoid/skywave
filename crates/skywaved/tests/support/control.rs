//! A control-socket client for tests: [`Client`]'s blocking calls on a
//! blocking thread, each bounded.

use std::path::Path;
use std::time::Duration;

use skywaved::control::{Client, Confirm, Reply, Request, When};

/// Upper bound for one answer; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(20);

pub struct TestClient(Option<Client>);

impl TestClient {
    pub async fn connect(path: &Path) -> TestClient {
        let path = path.to_owned();
        let client = tokio::task::spawn_blocking(move || Client::connect(&path).unwrap())
            .await
            .unwrap();
        TestClient(Some(client))
    }

    /// Sends `request` and returns the first reply.
    pub async fn ask(&mut self, request: Request) -> Reply {
        self.exchange(Some(request)).await
    }

    /// The next reply to what was asked before.
    pub async fn next(&mut self) -> Reply {
        self.exchange(None).await
    }

    /// Skips the [`Reply::Waiting`] updates until the stop, and returns
    /// whether it restarts.
    pub async fn stopping(&mut self) -> bool {
        loop {
            match self.next().await {
                Reply::Waiting { .. } => {}
                Reply::Stopping { restart } => return restart,
                other => panic!("expected the stop, got {other:?}"),
            }
        }
    }

    async fn exchange(&mut self, request: Option<Request>) -> Reply {
        let mut client = self.0.take().expect("a client");
        let (client, reply) = tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || {
                if let Some(request) = request {
                    client.send(&request).unwrap();
                }
                let reply = client.reply().unwrap().expect("a reply");
                (client, reply)
            }),
        )
        .await
        .expect("serve answers")
        .unwrap();
        self.0 = Some(client);
        reply
    }
}

pub fn stop_now(confirm: Confirm) -> Request {
    Request::Stop {
        restart: false,
        when: When::Now,
        confirm,
    }
}

pub fn stop_after_work(restart: bool) -> Request {
    Request::Stop {
        restart,
        when: When::AfterWork,
        confirm: Confirm::Nothing,
    }
}
