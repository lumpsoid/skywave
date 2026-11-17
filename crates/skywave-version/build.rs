//! Sets `SKYWAVE_COMMIT` to the 7-character hash of the checked-out commit,
//! or to nothing outside a git checkout (a source tarball).
//!
//! Re-runs only when what `HEAD` points at can have moved: `HEAD` itself,
//! the refs and `packed-refs`. Only paths that exist are named, since cargo
//! treats a missing one as always changed and would rebuild every time.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=SKYWAVE_COMMIT={commit}");

    // In a linked worktree `HEAD` lives in its own git dir and the refs in
    // the common one.
    let dirs = [
        git(&["rev-parse", "--absolute-git-dir"]),
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ];
    for dir in dirs.into_iter().flatten().map(PathBuf::from) {
        for name in ["HEAD", "refs", "packed-refs"] {
            let path = dir.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

/// `git <args>`'s trimmed output, or `None` without git or a repository.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8(output.stdout).ok()?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_owned())
}
