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

/// A rule a person wrote: a program and the literal words that must follow it.
/// A simple command whose words start with these is approved without proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    words: Vec<String>,
}

impl Rule {
    /// Builds a rule from its words, the program first. At least one word is
    /// needed, and no word may be empty.
    pub fn new(words: Vec<String>) -> Result<Rule, String> {
        if words.is_empty() {
            return Err("a rule needs a program".into());
        }
        if words.iter().any(String::is_empty) {
            return Err("a rule word is empty".into());
        }
        Ok(Rule { words })
    }

    /// The rule's words, the program first.
    pub fn words(&self) -> &[String] {
        &self.words
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.words.join(" "))
    }
}

/// Checks `command`, which runs with its working directory at `workdir`
/// (absolute and canonical).
pub fn check(command: &str, workdir: &Path) -> Verdict {
    check_with(command, workdir, &[])
}

/// Like `check`, and also approves a simple command that starts with the
/// words of one of `rules`. Redirections are still checked, and every simple
/// command in the line must pass by a rule or by the built-in table.
pub fn check_with(command: &str, workdir: &Path, rules: &[Rule]) -> Verdict {
    shell::check(command, workdir, rules)
}
