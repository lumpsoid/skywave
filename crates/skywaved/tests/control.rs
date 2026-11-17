//! `skywaved stop` and `skywaved restart` against a running `serve` binary.
//! What they decide over running work is tested beside the shell runner
//! and the supervisor (`tests/shell.rs`, `tests/supervisor.rs`); here, the
//! commands themselves and the restart's new process.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// Upper bound for any wait; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(20);

struct Daemon {
    _tmp: TempDir,
    config: PathBuf,
    log_file: PathBuf,
}

impl Daemon {
    fn new() -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let log_file = tmp.path().join("skywaved.log");
        let config = tmp.path().join("skywaved.toml");
        std::fs::write(
            &config,
            format!(
                // No floor: the test machine's free space is no part of it.
                "listen = \"127.0.0.1:0\"\ndata_dir = {:?}\nroots = [{:?}]\nlog_file = {:?}\n\
                 [disk]\nmin_free = 0\n",
                tmp.path().join("data"),
                root,
                log_file
            ),
        )
        .unwrap();
        Daemon {
            _tmp: tmp,
            config,
            log_file,
        }
    }

    fn serve(&self) -> tokio::process::Child {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_skywaved"))
            .arg("--config")
            .arg(&self.config)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    /// Runs `skywaved <args>`, with no terminal on stdin.
    async fn cli(&self, args: &[&str]) -> Output {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_skywaved"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .stdin(Stdio::null())
            .output();
        tokio::time::timeout(WAIT, output)
            .await
            .expect("the command ends")
            .unwrap()
    }

    /// Waits until the log has said "listening" `count` times.
    async fn listening(&self, count: usize) {
        wait_logged(&self.log_file, " listening addr=", count).await;
    }
}

/// Waits until `needle` appears `count` times in the file at `path`.
async fn wait_logged(path: &Path, needle: &str, count: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let logged = std::fs::read_to_string(path).unwrap_or_default();
        if logged.matches(needle).count() >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {needle} x{count}; log:\n{logged}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn restart_starts_serve_again_in_its_process_and_stop_ends_it() {
    let d = Daemon::new();
    let mut serve = d.serve();
    d.listening(1).await;

    // Nothing runs: no question asked.
    let restarted = d.cli(&["restart"]).await;
    assert!(restarted.status.success(), "{restarted:?}");
    assert!(
        stderr(&restarted).contains("stopped; starting again"),
        "{}",
        stderr(&restarted)
    );
    d.listening(2).await;
    assert!(
        serve.try_wait().unwrap().is_none(),
        "the same process runs serve again"
    );

    let stopped = d.cli(&["stop"]).await;
    assert!(stopped.status.success(), "{stopped:?}");
    let status = tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("serve ends")
        .unwrap();
    assert!(status.success(), "{status:?}");

    let again = d.cli(&["stop"]).await;
    assert!(!again.status.success(), "{again:?}");
    assert!(
        stderr(&again).contains("no skywaved serve is running"),
        "{}",
        stderr(&again)
    );
}

#[tokio::test]
async fn cancel_with_no_stop_waiting_says_so() {
    let d = Daemon::new();
    let mut serve = d.serve();
    d.listening(1).await;
    let cancel = d.cli(&["stop", "--cancel"]).await;
    assert!(cancel.status.success(), "{cancel:?}");
    assert!(
        stderr(&cancel).contains("no stop was waiting"),
        "{}",
        stderr(&cancel)
    );
    assert!(d.cli(&["stop", "--after-work"]).await.status.success());
    let status = tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("serve ends")
        .unwrap();
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn status_reports_a_running_serve_and_fails_without_one() {
    let d = Daemon::new();
    let none = d.cli(&["status"]).await;
    assert!(!none.status.success(), "{none:?}");
    assert!(
        stderr(&none).contains("no skywaved serve is running"),
        "{}",
        stderr(&none)
    );

    let mut serve = d.serve();
    d.listening(1).await;
    let status = d.cli(&["status"]).await;
    assert!(status.status.success(), "{status:?}");
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains(&format!(
            "serving (protocol {})",
            skywave_proto::PROTOCOL_VERSION
        )),
        "{stdout}"
    );
    assert!(stdout.contains("nothing running"), "{stdout}");

    assert!(d.cli(&["stop"]).await.status.success());
    tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("serve ends")
        .unwrap();
}

#[tokio::test]
async fn safety_feedback_prints_nothing_without_feedback() {
    let d = Daemon::new();
    let output = d.cli(&["safety-feedback"]).await;
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn version_prints_the_build_version() {
    let d = Daemon::new();
    let output = d.cli(&["--version"]).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("skywaved {}\n", skywave_version::VERSION)
    );
}

#[tokio::test]
async fn sighup_reopens_a_moved_log_file() {
    let d = Daemon::new();
    let mut serve = d.serve();
    d.listening(1).await;
    let rotated = d.log_file.with_extension("log.1");
    std::fs::rename(&d.log_file, &rotated).unwrap();
    let pid = libc::pid_t::try_from(serve.id().unwrap()).unwrap();
    // SAFETY: signals a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGHUP) }, 0);
    wait_logged(&d.log_file, "reopened the log file", 1).await;
    assert!(
        serve.try_wait().unwrap().is_none(),
        "SIGHUP does not end serve"
    );

    assert!(d.cli(&["stop"]).await.status.success());
    tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("serve ends")
        .unwrap();
    let rotated = std::fs::read_to_string(&rotated).unwrap();
    assert!(rotated.contains(" listening addr="), "{rotated}");
    let current = std::fs::read_to_string(&d.log_file).unwrap();
    assert!(current.contains("stopped"), "{current}");
}

#[tokio::test]
async fn ctrl_c_with_nothing_running_stops_at_once() {
    let d = Daemon::new();
    let mut serve = d.serve();
    d.listening(1).await;
    let pid = libc::pid_t::try_from(serve.id().unwrap()).unwrap();
    // SAFETY: signals a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("serve ends")
        .unwrap();
    let current = std::fs::read_to_string(&d.log_file).unwrap();
    assert!(current.contains("received Ctrl-C"), "{current}");
    assert!(current.contains("stopped"), "{current}");
    assert!(!current.contains("work running"), "{current}");
}
