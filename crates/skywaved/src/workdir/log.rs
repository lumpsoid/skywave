//! The commit log of a workdir's `HEAD`: `GET /v1/workdir/log`.

use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};
use skywave_proto::{Commit, CommitLog, LogResult, WorkdirQuery, limits};

use super::changes::{branch, work_tree};
use super::git::{GitError, Repo};
use super::{Git, Located, Snapshot};
use crate::error::Result;

impl Snapshot<'_> {
    /// `GET /v1/workdir/log` (the parent module's docs, "Log"). Recorded as
    /// nothing: the answer names no blob.
    pub fn log(&mut self, query: &WorkdirQuery) -> Result<LogResult> {
        let workdir = match self.workdir(query.session)? {
            Ok(workdir) => workdir,
            Err(Located::UnknownSession) => return Ok(LogResult::UnknownSession),
            Err(_) => return Ok(LogResult::WorkdirGone),
        };
        match read(self.git, &workdir, self.deadline) {
            Ok(answer) => Ok(answer),
            Err(GitError::Failed(reason)) => Ok(LogResult::Failed { reason }),
            Err(GitError::Fault(e)) => Err(e),
        }
    }
}

/// The git side of the parent module's docs, "Log": the answer for a
/// workdir already resolved.
fn read(git: &Git, workdir: &Path, deadline: Instant) -> Result<LogResult, GitError> {
    let at = Utc::now();
    let repo = Repo::new(git, workdir, deadline);
    if !work_tree(&repo)? {
        return Ok(LogResult::NotGit);
    }
    let branch = branch(&repo)?;
    // Unborn: no commit to list, which is an answer, not a failure.
    let Some(tip) = tip(&repo)? else {
        return Ok(LogResult::Log(CommitLog {
            at,
            branch,
            commits: Vec::new(),
            more: false,
        }));
    };
    // One past the cap, so that `more` says whether older commits exist.
    let count = format!("-n{}", limits::WORKDIR_LOG_MAX + 1);
    let out = repo
        .output(&[
            "log",
            "-z",
            "--no-show-signature",
            "--no-color",
            "--encoding=UTF-8",
            count.as_str(),
            "--format=%H%n%h%n%at%n%an%n%s",
            tip.as_str(),
            "--",
        ])?
        .ok()?;
    let mut commits = Vec::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let commit =
            parse(record).ok_or_else(|| GitError::Failed("unexpected git log record".into()))?;
        commits.push(commit);
    }
    let more = commits.len() > limits::WORKDIR_LOG_MAX;
    commits.truncate(limits::WORKDIR_LOG_MAX);
    Ok(LogResult::Log(CommitLog {
        at,
        branch,
        commits,
        more,
    }))
}

/// The commit the branch names, `None` while it names none yet (an unborn
/// branch). The branch is named by its full ref, never the bare `HEAD`: on
/// an unborn branch a ref called `HEAD` elsewhere (`refs/tags/HEAD`) would
/// answer the bare name. A detached `HEAD` is a file of its own and answers
/// it.
fn tip(repo: &Repo<'_>) -> Result<Option<String>, GitError> {
    let target = repo.output(&["symbolic-ref", "-q", "HEAD"])?;
    let target = match target.status.code() {
        Some(0) => lossy(target.out.trim_ascii()),
        Some(1) => "HEAD".to_owned(),
        _ => return Err(GitError::Failed(target.reason())),
    };
    let commit = format!("{target}^{{commit}}");
    let sha = repo.output(&["rev-parse", "-q", "--verify", commit.as_str()])?;
    match sha.status.code() {
        Some(0) => Ok(Some(lossy(sha.out.trim_ascii()))),
        Some(1) => Ok(None),
        _ => Err(GitError::Failed(sha.reason())),
    }
}

/// One `--format` record: its fields, one per line, in the order of the
/// format. An ident holds no newline and `%s` folds the subject's lines, so
/// each field is one line. A missing field is `None`; a bad author date is
/// no time, not a failure of the whole log.
fn parse(record: &[u8]) -> Option<Commit> {
    let mut fields = record.splitn(5, |&b| b == b'\n');
    let sha = lossy(fields.next()?);
    let short = lossy(fields.next()?);
    let secs = lossy(fields.next()?);
    let author = lossy(fields.next()?);
    let subject = lossy(fields.next()?);
    let time = secs
        .parse::<i64>()
        .ok()
        .and_then(|secs| DateTime::from_timestamp(secs, 0));
    Some(Commit {
        sha,
        short,
        author,
        time,
        subject,
    })
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::changes::tests::{git, init, isolated, reader, run_git, write};
    use super::super::tests::Tree;
    use super::*;

    fn answer(t: &Tree) -> LogResult {
        reader(t, isolated(t))
            .snapshot(Tree::later(), |s| s.log(&t.query("")))
            .unwrap()
    }

    fn log_of(t: &Tree) -> CommitLog {
        match answer(t) {
            LogResult::Log(log) => log,
            other => panic!("expected a log, got {other:?}"),
        }
    }

    fn commit(dir: &Path, subject: &str) {
        git(dir, &["commit", "-q", "--allow-empty", "-m", subject]);
    }

    #[test]
    fn outside_a_work_tree_is_not_git() {
        let t = Tree::new();
        assert_eq!(answer(&t), LogResult::NotGit);
    }

    #[test]
    fn an_unborn_head_is_an_empty_log() {
        let t = Tree::new();
        init(&t.workdir);
        let log = log_of(&t);
        assert_eq!(log.branch.as_deref(), Some("main"));
        assert!(log.commits.is_empty());
        assert!(!log.more);
    }

    #[test]
    fn commits_come_newest_first_with_their_fields() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        for subject in ["one", "two", "three"] {
            commit(wd, subject);
        }
        let log = log_of(&t);
        let subjects: Vec<&str> = log.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["three", "two", "one"]);
        assert!(!log.more);
        for (listed, rev) in log.commits.iter().zip(["HEAD", "HEAD~1", "HEAD~2"]) {
            let sha = String::from_utf8(git(wd, &["rev-parse", rev])).unwrap();
            assert_eq!(listed.sha, sha.trim(), "{rev}");
            assert!(listed.sha.starts_with(listed.short.as_str()), "{rev}");
            assert_eq!(listed.author, "t");
        }
        let age = Utc::now() - log.commits[0].time.unwrap();
        assert!(age.num_seconds().abs() < 600, "{age}");
    }

    #[test]
    fn a_detached_head_has_no_branch() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        commit(wd, "one");
        commit(wd, "two");
        git(wd, &["checkout", "-q", "--detach", "HEAD~1"]);
        let log = log_of(&t);
        assert_eq!(log.branch, None);
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].subject, "one");
    }

    #[test]
    fn the_log_is_capped_and_says_more() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        for i in 0..=limits::WORKDIR_LOG_MAX {
            commit(wd, &format!("c{i}"));
        }
        let log = log_of(&t);
        assert_eq!(log.commits.len(), limits::WORKDIR_LOG_MAX);
        assert!(log.more);
        assert_eq!(
            log.commits[0].subject,
            format!("c{}", limits::WORKDIR_LOG_MAX)
        );
        // The one dropped is the oldest, c0.
        assert_eq!(log.commits.last().unwrap().subject, "c1");
    }

    #[test]
    fn a_signature_check_from_repo_config_runs_nothing() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        commit(wd, "one");
        // The same commit with a signature header that nothing can verify:
        // only a signed commit makes `log --show-signature` run `gpg.program`.
        let raw = String::from_utf8(git(wd, &["cat-file", "commit", "HEAD"])).unwrap();
        let (headers, message) = raw.split_once("\n\n").unwrap();
        let signed = format!(
            "{headers}\ngpgsig -----BEGIN PGP SIGNATURE-----\n fake\n -----END PGP SIGNATURE-----\n\n{message}"
        );
        let object = t.outside.join("signed-commit");
        write(object.clone(), signed);
        let sha = String::from_utf8(git(
            wd,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                object.to_str().unwrap(),
            ],
        ))
        .unwrap();
        git(wd, &["update-ref", "refs/heads/main", sha.trim()]);
        let mark = t.outside.join("signature-ran");
        let program = t.outside.join("gpg.sh");
        write(
            program.clone(),
            format!("#!/bin/sh\ntouch '{}'\ncat\n", mark.display()),
        );
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(wd, &["config", "log.showSignature", "true"]);
        git(wd, &["config", "gpg.program", program.to_str().unwrap()]);

        let log = log_of(&t);
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].subject, "one");
        assert!(!mark.exists(), "the signature check ran");
        // Positive control: a plain `git log` there runs the program.
        run_git(wd, &["log", "-1"]);
        assert!(mark.exists(), "the signature check did not run");
    }

    #[test]
    fn an_author_with_a_unit_separator_is_read_whole() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        git(
            wd,
            &[
                "-c",
                "user.name=Ann\u{1f}Lee",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
        );
        let log = log_of(&t);
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].author, "Ann\u{1f}Lee");
        assert_eq!(log.commits[0].subject, "one");
    }

    #[test]
    fn a_commit_with_a_bad_author_date_has_no_time() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        commit(wd, "one");
        // The author's timestamp replaced by one past what chrono holds.
        let raw = String::from_utf8(git(wd, &["cat-file", "commit", "HEAD"])).unwrap();
        let (headers, message) = raw.split_once("\n\n").unwrap();
        let headers: Vec<String> = headers
            .lines()
            .map(|line| match line.strip_prefix("author ") {
                Some(author) => {
                    let (ident, zone) = author.rsplit_once(' ').unwrap();
                    let (ident, _) = ident.rsplit_once(' ').unwrap();
                    format!("author {ident} 99999999999999999 {zone}")
                }
                None => line.to_owned(),
            })
            .collect();
        let object = t.outside.join("bad-date-commit");
        write(
            object.clone(),
            format!("{}\n\n{message}", headers.join("\n")),
        );
        let sha = String::from_utf8(git(
            wd,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                object.to_str().unwrap(),
            ],
        ))
        .unwrap();
        git(wd, &["update-ref", "refs/heads/main", sha.trim()]);

        let log = log_of(&t);
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].time, None);
        assert_eq!(log.commits[0].subject, "one");
    }

    #[test]
    fn an_unborn_branch_ignores_a_ref_named_head() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        commit(wd, "one");
        git(wd, &["update-ref", "refs/tags/HEAD", "HEAD"]);
        git(wd, &["checkout", "-q", "--orphan", "fresh"]);
        let log = log_of(&t);
        assert_eq!(log.branch.as_deref(), Some("fresh"));
        assert!(log.commits.is_empty());
        assert!(!log.more);
    }
}
