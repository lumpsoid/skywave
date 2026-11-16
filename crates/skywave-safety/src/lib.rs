//! Proves a shell command read-only and confined to the session's workdir.
//!
//! The checker answers only "safe" or "ask". Every unknown, unparsed or
//! unsupported construct is "ask": an expansion, a glob, a write, a path
//! outside the workdir, a program off the allowlist. It never denies; a
//! human decides anything it cannot prove.

mod paths;
mod programs;
mod shell;

use std::path::Path;

/// The checker's answer. It never denies: whatever is not proven is `Ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Proven read-only and inside the workdir. `reason` is shown to the
    /// user, e.g. "read-only: cat, grep".
    Safe { reason: String },
    /// Not proven safe; a human decides. `why` names the first thing
    /// that failed, e.g. "`rm` is not a known read-only program".
    Ask { why: String },
}

/// Checks `command`, which runs with its working directory at `workdir`
/// (absolute and canonical).
pub fn check(command: &str, workdir: &Path) -> Verdict {
    shell::check(command, workdir)
}
