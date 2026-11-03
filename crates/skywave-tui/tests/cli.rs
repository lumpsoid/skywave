//! The `skywave-tui` binary's start-up, before it takes the terminal.

use std::process::{Command, Stdio};

use skywave_core::{PROTOCOL_VERSION, Store};

#[test]
fn unreadable_local_data_names_the_reset_flag_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).unwrap());
    rusqlite::Connection::open(dir.path().join(skywave_core::store::DB_FILE))
        .unwrap()
        .execute(
            "UPDATE protocol_stamp SET version = ?1",
            [PROTOCOL_VERSION + 1],
        )
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_skywave-tui"))
        .arg("--data-dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("cannot be read"), "{stderr}");
    assert!(stderr.contains("--reset-replica"), "{stderr}");
}

#[test]
fn the_reset_flag_is_in_the_usage() {
    let out = Command::new(env!("CARGO_BIN_EXE_skywave-tui"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--reset-replica"));
}
