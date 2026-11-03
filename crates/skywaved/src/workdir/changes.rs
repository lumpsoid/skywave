//! `GET /v1/workdir/changes` (the parent module's docs, "Changes"): what
//! changed in a session's workdir against `HEAD`, with a diff per file.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chrono::Utc;
use skywave_proto::{
    ChangeSet, ChangedFile, ChangesResult, DiffBody, FileChange, WorkdirQuery, limits,
};

use super::git::{GitError, Repo};
use super::{Located, READ_CHUNK, Snapshot, looks_binary};
use crate::error::Result;

/// Every patch section, and only a patch section, starts with this line
/// prefix: a patch body line starts with ` `, `+`, `-`, `\` or `@@`.
const SECTION: &[u8] = b"diff --git ";
/// A longer section header is not paired with any entry.
const HEADER_LINE_MAX: usize = 64 * 1024;
/// A longer path in git's `-z` output is left out (and not counted), like
/// one that is not UTF-8.
const PATH_MAX: usize = 32 * 1024;
/// Bytes of one `-z` record held: a numstat record is two counts and a path.
const RECORD_MAX: usize = PATH_MAX + 64;
/// Flags of both diffs: limited to the workdir's subtree and named relative
/// to it, renames found, nothing configurable runs, the repo's order file
/// and prefix settings ignored (headers must be predictable).
/// `--submodule=short` above all: the repo's `diff.submodule=diff` would run
/// an inner diff inside the submodule without `--no-ext-diff`, and `log`
/// prints lines with no `diff --git` header of their own.
const DIFF_FLAGS: [&str; 10] = [
    "--relative",
    "-M",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--ignore-submodules=dirty",
    "--submodule=short",
    "-O/dev/null",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// One changed file, before its diff is known.
struct Entry {
    file: ChangedFile,
    source: Source,
}

enum Source {
    /// From `git diff`; `binary` by its numstat. `sections`: how many its
    /// raw record gives in the full patch (two for a type change), `0` for a
    /// conflict no raw record lists.
    Tracked {
        binary: bool,
        sections: u32,
    },
    Untracked(Untracked),
}

/// An untracked file by `lstat`.
#[derive(Clone, Copy)]
enum Untracked {
    Link,
    File { size: u64, executable: bool },
}

/// A tracked entry's patch as it streams in.
#[derive(Default)]
struct Collected {
    /// Kept bytes, at most [`limits::WORKDIR_DIFF_MAX_BYTES`].
    text: Vec<u8>,
    /// All bytes seen, kept or not.
    bytes: u64,
    /// Sections paired with this entry, and the number of the last one.
    sections: u32,
    last: u64,
    /// Paired twice, not consecutively: needs the per-file call.
    broken: bool,
    /// Past [`limits::WORKDIR_DIFF_MAX_BYTES`]: only counted.
    too_large: bool,
    /// Past the snapshot's [`limits::WORKDIR_DIFFS_MAX_BYTES`]: dropped.
    omitted: bool,
}

/// Diff bytes held in memory: now, and at most at once.
#[derive(Debug, Default)]
pub(super) struct Kept {
    held: u64,
    pub(super) peak: u64,
}

impl Kept {
    /// Adds `bytes` to `c`, unless that takes it past a cap.
    fn take(&mut self, c: &mut Collected, bytes: &[u8]) {
        let n = bytes.len() as u64;
        c.bytes += n;
        if c.broken {
            return;
        }
        // Too large wins over omitted: one past the budget is still counted.
        if !c.too_large && c.bytes > limits::WORKDIR_DIFF_MAX_BYTES {
            c.too_large = true;
            self.release(c);
        }
        if c.too_large || c.omitted {
            return;
        }
        if self.held + n > limits::WORKDIR_DIFFS_MAX_BYTES {
            c.omitted = true;
            self.release(c);
        } else {
            c.text.extend_from_slice(bytes);
            self.held += n;
            self.peak = self.peak.max(self.held);
        }
    }

    /// Drops `c`'s kept bytes.
    fn release(&mut self, c: &mut Collected) {
        self.held -= c.text.len() as u64;
        c.text = Vec::new();
    }
}

/// Receives a patch split into sections.
trait Sections {
    /// A section starts with header line `line` (without its newline).
    fn header(&mut self, line: &[u8]);
    /// Bytes of the current section, its header line included.
    fn bytes(&mut self, bytes: &[u8]);
}

/// Routes sections to the entries whose expected header they carry.
struct Pairing<'c> {
    /// An entry's index, and how many sections it may take, by header.
    slots: &'c HashMap<Vec<u8>, (usize, u32)>,
    collected: &'c mut [Collected],
    kept: &'c mut Kept,
    current: Option<usize>,
    /// Sections seen so far.
    seq: u64,
}

impl Sections for Pairing<'_> {
    fn header(&mut self, line: &[u8]) {
        self.seq += 1;
        self.current = None;
        let Some(&(i, allowed)) = self.slots.get(line) else {
            return;
        };
        let c = &mut self.collected[i];
        // A type change is two consecutive sections with one header; the
        // same header again later, or once more than the entry gives (an
        // entry past the cap that shares it), cannot be trusted.
        if c.sections == allowed || (c.sections > 0 && c.last + 1 != self.seq) {
            c.broken = true;
            self.kept.release(c);
            return;
        }
        c.sections += 1;
        c.last = self.seq;
        self.current = Some(i);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        if let Some(i) = self.current {
            self.kept.take(&mut self.collected[i], bytes);
        }
    }
}

/// Takes a whole per-file patch for one entry.
/// Takes the sections of a per-file patch that carry the entry's own header
/// as git prints it (quoted or not): `:(literal)p` also matches everything
/// under a directory `p/`, whose sections are dropped.
struct Single<'c> {
    header: &'c [u8],
    collected: &'c mut Collected,
    kept: &'c mut Kept,
    current: bool,
}

impl Sections for Single<'_> {
    fn header(&mut self, line: &[u8]) {
        self.current = line == self.header;
        if self.current {
            self.collected.sections += 1;
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        if self.current {
            self.kept.take(self.collected, bytes);
        }
    }
}

impl<'a> Snapshot<'a> {
    /// `GET /v1/workdir/changes` (module docs, "Changes").
    pub fn changes(&mut self, query: &WorkdirQuery) -> Result<ChangesResult> {
        self.changes_measured(query).map(|(answer, _)| answer)
    }

    /// [`Snapshot::changes`], and the diff bytes it held at most at once.
    pub(super) fn changes_measured(
        &mut self,
        query: &WorkdirQuery,
    ) -> Result<(ChangesResult, Kept)> {
        let workdir = match self.workdir(query.session)? {
            Ok(workdir) => workdir,
            Err(Located::UnknownSession) => {
                return Ok((ChangesResult::UnknownSession, Kept::default()));
            }
            Err(_) => return Ok((ChangesResult::WorkdirGone, Kept::default())),
        };
        let mut kept = Kept::default();
        let answer = match self.collect(&workdir, &mut kept) {
            Ok(answer) => answer,
            Err(GitError::Failed(reason)) => ChangesResult::Failed { reason },
            Err(GitError::Fault(e)) => return Err(e),
        };
        Ok((answer, kept))
    }

    /// Steps 1 to 8 of the module docs, "Changes".
    fn collect(&mut self, workdir: &Path, kept: &mut Kept) -> Result<ChangesResult, GitError> {
        let at = Utc::now();
        let git = self.git;
        let mut repo = Repo::new(git, workdir, self.deadline);

        let inside = repo.output(&["rev-parse", "--is-inside-work-tree"])?;
        if inside.status.success() {
            match inside.out.trim_ascii() {
                b"true" => {}
                b"false" => return Ok(ChangesResult::NotGit),
                _ => return Err(GitError::Failed(inside.reason())),
            }
        } else if inside.stderr.starts_with(b"fatal: not a git repository") {
            return Ok(ChangesResult::NotGit);
        } else {
            return Err(GitError::Failed(inside.reason()));
        }

        // Exit 1: no filter is configured at all.
        let config =
            repo.output(&["config", "--show-scope", "-z", "--get-regexp", r"^filter\."])?;
        match config.status.code() {
            Some(0) => repo.neutralise(repo_filters(&config.out)),
            Some(1) if config.out.is_empty() => {}
            _ => return Err(GitError::Failed(config.reason())),
        }

        let branch = repo.output(&["symbolic-ref", "--short", "-q", "HEAD"])?;
        let branch = match branch.status.code() {
            Some(0) => Some(String::from_utf8_lossy(branch.out.trim_ascii()).into_owned()),
            Some(1) => None,
            _ => return Err(GitError::Failed(branch.reason())),
        };
        let head = repo.output(&["rev-parse", "--short", "-q", "--verify", "HEAD"])?;
        let head = match head.status.code() {
            Some(0) => Some(String::from_utf8_lossy(head.out.trim_ascii()).into_owned()),
            Some(1) => None,
            _ => return Err(GitError::Failed(head.reason())),
        };
        let base = match head {
            Some(_) => "HEAD".to_owned(),
            // Unborn: against the empty tree, in the repo's own hash.
            None => {
                let empty = repo
                    .output(&["hash-object", "-t", "tree", "/dev/null"])?
                    .ok()?;
                String::from_utf8_lossy(empty.trim_ascii()).into_owned()
            }
        };

        // Both listings stream: only the entries that sort first are held.
        let unmerged = repo.output(&["ls-files", "--unmerged", "-z"])?.ok()?;
        let unmerged = unmerged_paths(&unmerged);
        let mut firsts = Firsts::default();
        let mut args = vec!["diff", base.as_str()];
        args.extend(DIFF_FLAGS);
        args.extend(["-z", "--raw", "--numstat"]);
        let mut parser = TrackedParser::new(&unmerged, &mut firsts);
        repo.run(&args, |out| records(out, |r, whole| parser.feed(r, whole)))?
            .ok()?;
        let listed = parser
            .finish()
            .ok_or_else(|| GitError::Failed("unexpected git diff output".into()))?;
        // An unmerged path whose worktree equals `HEAD` (a modify/delete
        // conflict with ours kept, say) is in no raw record, but still
        // conflicted. Its diff comes from the per-file call (normally empty).
        let mut missing: Vec<&str> = unmerged
            .iter()
            .filter(|p| !listed.contains(*p))
            .filter_map(|p| std::str::from_utf8(p).ok())
            .collect();
        missing.sort_unstable();
        for path in missing {
            firsts.offer(
                Entry {
                    file: ChangedFile {
                        path: path.to_owned(),
                        from: None,
                        change: FileChange::Conflicted,
                        added: Some(0),
                        removed: Some(0),
                        diff: DiffBody::Omitted,
                    },
                    source: Source::Tracked {
                        binary: false,
                        sections: 0,
                    },
                },
                None,
            );
        }

        let others = ["ls-files", "--others", "--exclude-standard", "-z"];
        repo.run(&others, |out| {
            records(out, |path, whole| {
                // A trailing `/` is a nested repository.
                if !whole || path.len() > PATH_MAX || path.ends_with(b"/") {
                    return;
                }
                let Ok(path) = std::str::from_utf8(path) else {
                    return;
                };
                // Only an entry that will be kept is `lstat`ed.
                if !firsts.admits(path, true) {
                    firsts.count();
                    return;
                }
                let Ok(meta) = std::fs::symlink_metadata(workdir.join(path)) else {
                    return;
                };
                let kind = if meta.file_type().is_symlink() {
                    Untracked::Link
                } else if meta.is_file() {
                    Untracked::File {
                        size: meta.len(),
                        executable: meta.permissions().mode() & 0o100 != 0,
                    }
                } else {
                    return;
                };
                firsts.offer(
                    Entry {
                        file: ChangedFile {
                            path: path.to_owned(),
                            from: None,
                            change: FileChange::Untracked,
                            added: None,
                            removed: None,
                            diff: DiffBody::Omitted,
                        },
                        source: Source::Untracked(kind),
                    },
                    None,
                );
            })
        })?
        .ok()?;
        self.check_deadline()?;

        let mut total = firsts.offered;
        let entries = firsts.into_sorted();

        // Patches of the tracked entries kept, paired by exact header. An
        // entry past the cap with the same header makes one section too many.
        let mut headers: HashMap<Vec<u8>, u32> = HashMap::new();
        for e in &entries {
            if let Source::Tracked { .. } = e.source {
                *headers.entry(expected_header(&e.file)).or_default() += 1;
            }
        }
        let mut collected: Vec<Collected> = entries.iter().map(|_| Collected::default()).collect();
        let mut slots = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            let Source::Tracked {
                binary: false,
                sections,
            } = e.source
            else {
                continue;
            };
            let header = expected_header(&e.file);
            let quoted =
                needs_quote(&e.file.path) || e.file.from.as_deref().is_some_and(needs_quote);
            if sections > 0 && !quoted && headers.get(&header) == Some(&1) {
                slots.insert(header, (i, sections));
            }
        }
        let wants_patch = |e: &Entry| matches!(e.source, Source::Tracked { binary: false, .. });
        if entries.iter().any(wants_patch) {
            let mut args = vec!["diff", base.as_str()];
            args.extend(DIFF_FLAGS);
            let mut pairing = Pairing {
                slots: &slots,
                collected: &mut collected,
                kept: &mut *kept,
                current: None,
                seq: 0,
            };
            repo.run(&args, |out| split(out, &mut pairing))?.ok()?;
        }
        for (e, c) in entries.iter().zip(collected.iter_mut()) {
            if !wants_patch(e) || (c.sections > 0 && !c.broken) {
                continue;
            }
            self.check_deadline()?;
            kept.release(c);
            *c = Collected::default();
            let path = format!(":(literal){}", e.file.path);
            let from = e.file.from.as_ref().map(|from| format!(":(literal){from}"));
            let mut args = vec!["diff", base.as_str()];
            args.extend(DIFF_FLAGS);
            args.push("--");
            args.extend(from.as_deref());
            args.push(&path);
            let header = git_header(&e.file);
            let mut single = Single {
                header: &header,
                collected: c,
                kept: &mut *kept,
                current: false,
            };
            repo.run(&args, |out| split(out, &mut single))?.ok()?;
        }

        // Diffs in path order until the snapshot's budget is spent.
        let mut files = Vec::with_capacity(entries.len());
        let mut spent = 0u64;
        let mut out_of_budget = false;
        for (mut e, mut c) in entries.into_iter().zip(collected) {
            self.check_deadline()?;
            let text = match e.source {
                Source::Tracked { binary: true, .. } => {
                    e.file.diff = DiffBody::Binary;
                    None
                }
                Source::Tracked { .. } if c.too_large => {
                    e.file.diff = DiffBody::TooLarge { bytes: c.bytes };
                    None
                }
                Source::Tracked { .. } if c.omitted => {
                    e.file.diff = DiffBody::Omitted;
                    out_of_budget = true;
                    None
                }
                Source::Tracked { .. } => {
                    kept.held -= c.text.len() as u64;
                    Some(std::mem::take(&mut c.text))
                }
                Source::Untracked(_) if out_of_budget => None,
                Source::Untracked(kind) => match self.untracked(workdir, &e.file.path, kind)? {
                    Synthesised::Gone => {
                        total -= 1;
                        continue;
                    }
                    Synthesised::Binary => {
                        e.file.diff = DiffBody::Binary;
                        None
                    }
                    Synthesised::TooLarge(bytes) => {
                        e.file.diff = DiffBody::TooLarge { bytes };
                        None
                    }
                    Synthesised::Text { patch, added } => {
                        e.file.added = Some(added);
                        e.file.removed = Some(0);
                        Some(patch)
                    }
                },
            };
            if let Some(text) = text {
                let size = text.len() as u64;
                if out_of_budget || spent + size > limits::WORKDIR_DIFFS_MAX_BYTES {
                    out_of_budget = true;
                    e.file.diff = DiffBody::Omitted;
                } else {
                    spent += size;
                    e.file.diff = DiffBody::Blob(self.store(&text)?);
                }
            }
            files.push(e.file);
        }

        Ok(ChangesResult::Changes(ChangeSet {
            at,
            branch,
            head,
            files,
            total: u32::try_from(total).unwrap_or(u32::MAX),
        }))
    }

    /// The synthesised new-file diff of untracked `path` (module docs,
    /// "Changes", step 7). Never follows a link.
    fn untracked(&self, workdir: &Path, path: &str, kind: Untracked) -> Result<Synthesised> {
        let full = workdir.join(path);
        let cap = limits::WORKDIR_DIFF_MAX_BYTES;
        let (mode, content) = match kind {
            Untracked::Link => match std::fs::read_link(&full) {
                Ok(target) => ("120000", target.as_os_str().as_bytes().to_vec()),
                Err(_) => return Ok(Synthesised::Gone),
            },
            Untracked::File { size, .. } if size > cap => return Ok(Synthesised::TooLarge(size)),
            Untracked::File { executable, .. } => {
                let mut options = OpenOptions::new();
                options
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
                let Ok(mut file) = options.open(&full) else {
                    return Ok(Synthesised::Gone);
                };
                if !file.metadata().is_ok_and(|m| m.is_file()) {
                    return Ok(Synthesised::Gone);
                }
                let content = self.read_capped(&mut file, &full, cap)?;
                if content.len() as u64 > cap {
                    return Ok(Synthesised::TooLarge(content.len() as u64));
                }
                (if executable { "100755" } else { "100644" }, content)
            }
        };
        if looks_binary(&content) {
            return Ok(Synthesised::Binary);
        }
        let (patch, added) = new_file_patch(path, mode, &content);
        if patch.len() as u64 > cap {
            return Ok(Synthesised::TooLarge(patch.len() as u64));
        }
        Ok(Synthesised::Text { patch, added })
    }
}

/// An untracked file's diff.
enum Synthesised {
    Text {
        patch: Vec<u8>,
        added: u32,
    },
    Binary,
    /// At least this many bytes.
    TooLarge(u64),
    /// Gone, or no longer what `lstat` said, since it was listed.
    Gone,
}

/// Splits patch `out` at section headers into `sink`, holding at most one
/// header line (up to [`HEADER_LINE_MAX`]) and one read chunk.
fn split(out: &mut dyn Read, sink: &mut impl Sections) -> io::Result<()> {
    let mut chunk = vec![0u8; READ_CHUNK];
    // The current line while it may still be a header.
    let mut line = Vec::new();
    let mut collecting = true;
    loop {
        let n = match out.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            let (piece, ends) = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => (&rest[..=i], true),
                None => (rest, false),
            };
            rest = &rest[piece.len()..];
            if !collecting {
                sink.bytes(piece);
                collecting = ends;
                continue;
            }
            line.extend_from_slice(piece);
            let candidate = if line.len() >= SECTION.len() {
                line.starts_with(SECTION)
            } else {
                !ends && SECTION.starts_with(&line)
            };
            if !candidate {
                sink.bytes(&line);
                line.clear();
                collecting = ends;
            } else if ends {
                sink.header(&line[..line.len() - 1]);
                sink.bytes(&line);
                line.clear();
            } else if line.len() > HEADER_LINE_MAX {
                sink.header(b"");
                sink.bytes(&line);
                line.clear();
                collecting = false;
            }
        }
    }
    if !line.is_empty() {
        if line.starts_with(SECTION) {
            sink.header(&line);
        }
        sink.bytes(&line);
    }
    Ok(())
}

/// The names of the filter drivers to neutralise, from
/// `git config --show-scope -z --get-regexp '^filter\.'`: those with a
/// `clean` or `process` in `local` or `worktree` scope. An include counts as
/// the scope of the file including it.
fn repo_filters(out: &[u8]) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut fields = nul_split(out);
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        if scope != b"local" && scope != b"worktree" {
            continue;
        }
        let key = entry.split(|&b| b == b'\n').next().unwrap_or_default();
        // `filter.<name>.<key>`: the name may hold dots, the key cannot.
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        let Some(dot) = rest.iter().rposition(|&b| b == b'.') else {
            continue;
        };
        let (name, var) = (&rest[..dot], &rest[dot + 1..]);
        if (var == b"clean" || var == b"process") && !names.iter().any(|n| n == name) {
            names.push(name.to_vec());
        }
    }
    names
}

/// The paths `git ls-files --unmerged -z` lists (once per stage).
fn unmerged_paths(out: &[u8]) -> HashSet<Vec<u8>> {
    nul_split(out)
        .filter_map(|record| {
            let tab = record.iter().position(|&b| b == b'\t')?;
            Some(record[tab + 1..].to_vec())
        })
        .collect()
}

/// The entries that sort first (by path, tracked before untracked on a tie,
/// then in the order offered), at most [`limits::WORKDIR_CHANGES_MAX`], and
/// how many were offered in all. Memory is bounded by the cap, not by the
/// workdir.
#[derive(Default)]
struct Firsts {
    /// A max-heap: the entry to drop next is on top.
    heap: BinaryHeap<Ranked>,
    offered: usize,
}

/// An entry held by [`Firsts`].
struct Ranked {
    entry: Entry,
    /// Offer order, which breaks ties.
    seq: usize,
    /// The number of its raw record, to find its numstat record by.
    raw: Option<usize>,
}

impl Ranked {
    fn key(&self) -> (&str, bool, usize) {
        let untracked = matches!(self.entry.source, Source::Untracked(_));
        (&self.entry.file.path, untracked, self.seq)
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Ranked) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Ranked) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Ranked) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl Firsts {
    /// Whether an entry at `path` offered now would be held (for now).
    fn admits(&self, path: &str, untracked: bool) -> bool {
        self.heap.len() < limits::WORKDIR_CHANGES_MAX
            || self.heap.peek().is_some_and(|last| {
                let (p, u, _) = last.key();
                (path, untracked) < (p, u)
            })
    }

    /// Counts an entry [`Firsts::admits`] refused, without building it.
    fn count(&mut self) {
        self.offered += 1;
    }

    /// Counts `entry`, and holds it while it sorts among the first.
    fn offer(&mut self, entry: Entry, raw: Option<usize>) {
        let untracked = matches!(entry.source, Source::Untracked(_));
        let admitted = self.admits(&entry.file.path, untracked);
        self.offered += 1;
        if admitted {
            self.heap.push(Ranked {
                entry,
                seq: self.offered,
                raw,
            });
            if self.heap.len() > limits::WORKDIR_CHANGES_MAX {
                self.heap.pop();
            }
        }
    }

    /// The entries held, in order.
    fn into_sorted(self) -> Vec<Entry> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.entry)
            .collect()
    }
}

/// What [`TrackedParser`] expects next.
enum Expect {
    Raw,
    RawFrom {
        status: u8,
    },
    /// `from` is `None` when it was left out.
    RawPath {
        status: u8,
        from: Option<Option<String>>,
    },
    Numstat,
    NumFrom {
        counts: (Option<u32>, Option<u32>),
    },
    NumPath {
        counts: (Option<u32>, Option<u32>),
        from: Option<Vec<u8>>,
    },
}

/// `git diff -z --raw --numstat` as it streams (every raw record, then every
/// numstat record in the same order), offering each entry to [`Firsts`]. A
/// numstat record is checked against its raw record's path only for the
/// entries held: the others are not kept to compare.
struct TrackedParser<'a> {
    unmerged: &'a HashSet<Vec<u8>>,
    firsts: &'a mut Firsts,
    /// Unmerged paths some raw record lists.
    listed: HashSet<Vec<u8>>,
    expect: Expect,
    raws: usize,
    numstats: usize,
    /// Once numstat records start: the entries held, and the index among
    /// them of each held raw record number.
    numbered: Option<(Vec<Ranked>, HashMap<usize, usize>)>,
    bad: bool,
}

impl<'a> TrackedParser<'a> {
    fn new(unmerged: &'a HashSet<Vec<u8>>, firsts: &'a mut Firsts) -> TrackedParser<'a> {
        TrackedParser {
            unmerged,
            firsts,
            listed: HashSet::new(),
            expect: Expect::Raw,
            raws: 0,
            numstats: 0,
            numbered: None,
            bad: false,
        }
    }

    /// Takes the next record ([`records`]).
    fn feed(&mut self, record: &[u8], whole: bool) {
        if !self.bad && self.step(record, whole).is_none() {
            self.bad = true;
        }
    }

    /// `None` when the output is not what git prints.
    fn step(&mut self, record: &[u8], whole: bool) -> Option<()> {
        match std::mem::replace(&mut self.expect, Expect::Numstat) {
            Expect::Raw if record.starts_with(b":") => {
                if !whole {
                    return None;
                }
                let status = *record.rsplit(|&b| b == b' ').next()?.first()?;
                self.expect = match status {
                    b'R' | b'C' => Expect::RawFrom { status },
                    _ => Expect::RawPath {
                        status,
                        from: Some(None),
                    },
                };
            }
            Expect::Raw => {
                let held = std::mem::take(&mut self.firsts.heap).into_vec();
                let at = held
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| Some((r.raw?, i)))
                    .collect();
                self.numbered = Some((held, at));
                return self.numstat(record, whole);
            }
            Expect::RawFrom { status } => {
                self.expect = Expect::RawPath {
                    status,
                    from: name(record, whole).map(Some),
                };
            }
            Expect::RawPath { status, from } => {
                self.expect = Expect::Raw;
                self.raw(status, from, record, whole);
            }
            Expect::Numstat => return self.numstat(record, whole),
            Expect::NumFrom { counts } => {
                self.expect = Expect::NumPath {
                    counts,
                    from: whole.then(|| record.to_vec()),
                };
            }
            Expect::NumPath { counts, from } => {
                return self.fill(counts, from.as_deref(), record, whole);
            }
        }
        Some(())
    }

    /// Raw record number `self.raws` names `path`.
    fn raw(&mut self, status: u8, from: Option<Option<String>>, path: &[u8], whole: bool) {
        let n = self.raws;
        self.raws += 1;
        let (Some(from), Some(path)) = (from, name(path, whole)) else {
            return;
        };
        let conflicted = self.unmerged.contains(path.as_bytes());
        if conflicted {
            self.listed.insert(path.as_bytes().to_vec());
        }
        let change = match status {
            _ if conflicted => FileChange::Conflicted,
            b'A' | b'C' => FileChange::Added,
            b'D' => FileChange::Deleted,
            b'R' => FileChange::Renamed,
            b'T' => FileChange::TypeChanged,
            b'U' => FileChange::Conflicted,
            _ => FileChange::Modified,
        };
        let entry = Entry {
            file: ChangedFile {
                path,
                from: from.filter(|_| status == b'R'),
                change,
                added: None,
                removed: None,
                diff: DiffBody::Omitted,
            },
            source: Source::Tracked {
                binary: false,
                sections: if status == b'T' { 2 } else { 1 },
            },
        };
        self.firsts.offer(entry, Some(n));
    }

    /// A numstat record's first field: its counts and maybe its path.
    fn numstat(&mut self, record: &[u8], whole: bool) -> Option<()> {
        let mut parts = record.splitn(3, |&b| b == b'\t');
        let (added, removed, rest) = (parts.next()?, parts.next()?, parts.next()?);
        let count = |n: &[u8]| -> Option<u32> {
            std::str::from_utf8(n)
                .ok()?
                .parse::<u64>()
                .ok()
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        };
        let counts = (count(added), count(removed));
        if rest.is_empty() {
            self.expect = Expect::NumFrom { counts };
            return Some(());
        }
        self.expect = Expect::Numstat;
        self.fill(counts, None, rest, whole)
    }

    /// Numstat record number `self.numstats`, of `from` (a rename or copy)
    /// to `path`.
    fn fill(
        &mut self,
        counts: (Option<u32>, Option<u32>),
        from: Option<&[u8]>,
        path: &[u8],
        whole: bool,
    ) -> Option<()> {
        self.expect = Expect::Numstat;
        let n = self.numstats;
        self.numstats += 1;
        if n >= self.raws {
            return None;
        }
        let (held, at) = self.numbered.as_mut()?;
        let Some(&i) = at.get(&n) else {
            return Some(());
        };
        let e = &mut held[i].entry;
        let from_differs = match (from, &e.file.from) {
            (Some(from), Some(raw)) => from != raw.as_bytes(),
            _ => false,
        };
        if !whole || path != e.file.path.as_bytes() || from_differs {
            return None;
        }
        (e.file.added, e.file.removed) = counts;
        if let Source::Tracked { binary, .. } = &mut e.source {
            *binary = counts.0.is_none() || counts.1.is_none();
        }
        Some(())
    }

    /// The unmerged paths some raw record lists; `None` when the output was
    /// not what git prints.
    fn finish(self) -> Option<HashSet<Vec<u8>>> {
        if self.bad || !matches!(self.expect, Expect::Raw | Expect::Numstat) {
            return None;
        }
        if self.numstats != self.raws {
            return None;
        }
        if let Some((held, _)) = self.numbered {
            self.firsts.heap = BinaryHeap::from(held);
        }
        Some(self.listed)
    }
}

/// A path from a `-z` record; `None` (left out) when it was cut, is longer
/// than [`PATH_MAX`] or is not UTF-8.
fn name(record: &[u8], whole: bool) -> Option<String> {
    if !whole || record.len() > PATH_MAX {
        return None;
    }
    String::from_utf8(record.to_vec()).ok()
}

/// Feeds each record of `-z` output `out` to `each`, holding one read chunk
/// and at most [`RECORD_MAX`] bytes of one record: a longer one arrives cut
/// there, with `false`.
fn records(out: &mut dyn Read, mut each: impl FnMut(&[u8], bool)) -> io::Result<()> {
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut record = Vec::new();
    let mut whole = true;
    loop {
        let n = match out.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for piece in chunk[..n].split_inclusive(|&b| b == 0) {
            let (bytes, ends) = match piece.split_last() {
                Some((0, bytes)) => (bytes, true),
                _ => (piece, false),
            };
            let room = RECORD_MAX - record.len();
            whole &= bytes.len() <= room;
            record.extend_from_slice(&bytes[..bytes.len().min(room)]);
            if ends {
                each(&record, whole);
                record.clear();
                whole = true;
            }
        }
    }
    if !record.is_empty() || !whole {
        each(&record, whole);
    }
    Ok(())
}

/// The records of `-z` output.
fn nul_split(out: &[u8]) -> impl Iterator<Item = &[u8]> {
    out.strip_suffix(b"\0")
        .unwrap_or(out)
        .split(|&b| b == 0)
        .filter(|_| !out.is_empty())
}

/// The header git prints for `file`'s section, unquoted.
fn expected_header(file: &ChangedFile) -> Vec<u8> {
    let from = file.from.as_deref().unwrap_or(&file.path);
    format!("diff --git a/{from} b/{}", file.path).into_bytes()
}

/// The header git prints for `file`'s section, quoted where git quotes.
fn git_header(file: &ChangedFile) -> Vec<u8> {
    let from = file.from.as_deref().unwrap_or(&file.path);
    format!(
        "diff --git {} {}",
        quoted("a/", from),
        quoted("b/", &file.path)
    )
    .into_bytes()
}

/// Whether git C-quotes `path` even with `core.quotePath=false`.
fn needs_quote(path: &str) -> bool {
    path.bytes()
        .any(|b| b < 0x20 || b == b'"' || b == b'\\' || b == 0x7f)
}

/// `prefix` + `path`, C-quoted as git would.
fn quoted(prefix: &str, path: &str) -> String {
    if !needs_quote(path) {
        return format!("{prefix}{path}");
    }
    let mut out = String::from("\"");
    out.push_str(prefix);
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A new-file unified diff of `content` at `path`, and its line count.
fn new_file_patch(path: &str, mode: &str, content: &[u8]) -> (Vec<u8>, u32) {
    let mut patch = format!(
        "diff --git {} {}\nnew file mode {mode}\n",
        quoted("a/", path),
        quoted("b/", path)
    )
    .into_bytes();
    let lines: Vec<&[u8]> = content.split_inclusive(|&b| b == b'\n').collect();
    let added = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    if lines.is_empty() {
        return (patch, 0);
    }
    patch.extend_from_slice(format!("--- /dev/null\n+++ {}\n", quoted("b/", path)).as_bytes());
    let hunk = if lines.len() == 1 {
        "@@ -0,0 +1 @@\n".to_owned()
    } else {
        format!("@@ -0,0 +1,{} @@\n", lines.len())
    };
    patch.extend_from_slice(hunk.as_bytes());
    for line in &lines {
        patch.push(b'+');
        patch.extend_from_slice(line);
    }
    if !content.ends_with(b"\n") {
        patch.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
    (patch, added)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use skywave_proto::BlobRef;

    use super::super::tests::Tree;
    use super::super::{Git, WorkdirReader};
    use super::*;
    use crate::db::Db;
    use crate::error::Error;

    /// Runs the real git for test setup, immune to the user's config.
    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .output()
            .unwrap_or_else(|e| panic!("these tests need `git` on PATH: {e}"))
    }

    /// [`run_git`], which must succeed; its stdout.
    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = run_git(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn init(dir: &Path) {
        git(dir, &["init", "-q"]);
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "c"]);
    }

    fn write(path: PathBuf, content: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// The reader's git, immune to the user's config and to any repository
    /// above the tree.
    fn isolated(t: &Tree) -> Git {
        Git::default()
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_CEILING_DIRECTORIES", t.root.parent().unwrap())
    }

    fn reader(t: &Tree, git: Git) -> WorkdirReader {
        WorkdirReader::new(
            Db::open(&t.storage.dir.db_path()).unwrap(),
            t.storage.blobs.clone(),
            vec![t.root.clone()],
        )
        .with_git(git)
    }

    fn answer_with(t: &Tree, git: Git) -> (ChangesResult, Kept) {
        let query = t.query("");
        reader(t, git)
            .snapshot(Tree::later(), |s| s.changes_measured(&query))
            .unwrap()
    }

    fn answer(t: &Tree) -> ChangesResult {
        answer_with(t, isolated(t)).0
    }

    fn change_set(t: &Tree) -> ChangeSet {
        match answer(t) {
            ChangesResult::Changes(set) => set,
            other => panic!("expected changes, got {other:?}"),
        }
    }

    /// The one entry at `path` (untracked or not).
    fn entry<'s>(set: &'s ChangeSet, path: &str, untracked: bool) -> &'s ChangedFile {
        let mut found = set
            .files
            .iter()
            .filter(|f| f.path == path && (f.change == FileChange::Untracked) == untracked);
        let file = found
            .next()
            .unwrap_or_else(|| panic!("no entry {path:?}: {set:#?}"));
        assert!(found.next().is_none(), "two entries {path:?}");
        file
    }

    fn diff_text(t: &Tree, file: &ChangedFile) -> String {
        match &file.diff {
            DiffBody::Blob(blob) => String::from_utf8(t.blob_content(blob)).unwrap(),
            other => panic!("expected a diff blob for {:?}, got {other:?}", file.path),
        }
    }

    /// `(added, removed)` per path, from the real `git diff HEAD --numstat`.
    fn numstat(dir: &Path) -> HashMap<String, (Option<u32>, Option<u32>)> {
        let out = git(dir, &["diff", "HEAD", "-M", "--numstat", "-z"]);
        let mut fields = nul_split(&out);
        let mut counts = HashMap::new();
        while let Some(record) = fields.next() {
            let record = std::str::from_utf8(record).unwrap();
            let mut parts = record.splitn(3, '\t');
            let n = |s: &str| s.parse().ok();
            let (a, r) = (n(parts.next().unwrap()), n(parts.next().unwrap()));
            let mut path = parts.next().unwrap().to_owned();
            if path.is_empty() {
                fields.next();
                path = String::from_utf8(fields.next().unwrap().to_vec()).unwrap();
            }
            counts.insert(path, (a, r));
        }
        counts
    }

    /// Every blob file's content.
    fn all_blobs(t: &Tree) -> Vec<Vec<u8>> {
        fn walk(dir: &Path, out: &mut Vec<Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(std::fs::read(path).unwrap());
                }
            }
        }
        let mut out = Vec::new();
        walk(&t.storage.dir.blobs_dir(), &mut out);
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn outside_a_work_tree_is_not_git_and_other_fatals_fail() {
        let t = Tree::new();
        assert_eq!(answer(&t), ChangesResult::NotGit);

        // The workdir is a git dir: of a work tree elsewhere, and bare.
        let wd = &t.workdir;
        std::fs::remove_dir_all(wd).unwrap();
        let elsewhere = t.root.join("wt");
        std::fs::create_dir(&elsewhere).unwrap();
        git(
            &elsewhere,
            &["init", "-q", "--separate-git-dir", wd.to_str().unwrap()],
        );
        assert_eq!(answer(&t), ChangesResult::NotGit);
        std::fs::remove_dir_all(wd).unwrap();
        std::fs::create_dir(wd).unwrap();
        git(wd, &["init", "-q", "--bare"]);
        assert_eq!(answer(&t), ChangesResult::NotGit);

        // Any other fatal is git's own line.
        std::fs::remove_dir_all(wd).unwrap();
        write(wd.join(".git"), "junk\n");
        let ChangesResult::Failed { reason } = answer(&t) else {
            panic!("expected a failure");
        };
        assert!(
            reason.starts_with("fatal: invalid gitfile format"),
            "{reason}"
        );

        let missing = Git::new(t.root.join("no-such-git"));
        assert_eq!(
            answer_with(&t, missing).0,
            ChangesResult::Failed {
                reason: "git is not installed".into()
            }
        );
    }

    #[test]
    fn a_subdirectory_workdir_shows_only_its_subtree_in_its_own_terms() {
        let t = Tree::new();
        init(&t.root);
        write(t.root.join("top.txt"), "top\n");
        write(t.workdir.join("src/a.rs"), "a\n");
        write(t.workdir.join("c"), "base\n");
        commit_all(&t.root);
        git(&t.root, &["checkout", "-q", "-b", "other"]);
        write(t.workdir.join("c"), "theirs\n");
        commit_all(&t.root);
        git(&t.root, &["checkout", "-q", "main"]);
        write(t.workdir.join("c"), "ours\n");
        commit_all(&t.root);
        assert!(!run_git(&t.root, &["merge", "-q", "other"]).status.success());
        write(t.root.join("top.txt"), "changed\n");
        write(t.root.join("loose-top"), "x\n");
        write(t.workdir.join("src/a.rs"), "b\n");
        write(t.workdir.join("loose"), "y\n");

        let set = change_set(&t);
        let got: Vec<(&str, FileChange)> = set
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.change))
            .collect();
        assert_eq!(
            got,
            vec![
                ("c", FileChange::Conflicted),
                ("loose", FileChange::Untracked),
                ("src/a.rs", FileChange::Modified),
            ]
        );
        assert_eq!(set.total, 3);
        assert_eq!(set.branch.as_deref(), Some("main"));
        assert!(diff_text(&t, entry(&set, "c", false)).contains("+theirs"));
        assert!(
            diff_text(&t, entry(&set, "src/a.rs", false))
                .starts_with("diff --git a/src/a.rs b/src/a.rs\n")
        );
    }

    #[test]
    fn an_unborn_repo_shows_staged_and_untracked_files() {
        let t = Tree::new();
        init(&t.workdir);
        write(t.workdir.join("staged"), "one\ntwo\n");
        git(&t.workdir, &["add", "staged"]);
        write(t.workdir.join("loose"), "a\nb\nlast");
        write(t.workdir.join("empty"), "");

        let set = change_set(&t);
        assert_eq!(
            (set.branch.as_deref(), set.head.as_deref()),
            (Some("main"), None)
        );
        let staged = entry(&set, "staged", false);
        assert_eq!(
            (staged.change, staged.added, staged.removed),
            (FileChange::Added, Some(2), Some(0))
        );
        assert!(diff_text(&t, staged).ends_with("+one\n+two\n"));
        let loose = entry(&set, "loose", true);
        assert_eq!((loose.added, loose.removed), (Some(3), Some(0)));
        assert_eq!(
            diff_text(&t, loose),
            "diff --git a/loose b/loose\nnew file mode 100644\n--- /dev/null\n+++ b/loose\n\
             @@ -0,0 +1,3 @@\n+a\n+b\n+last\n\\ No newline at end of file\n"
        );
        let empty = entry(&set, "empty", true);
        assert_eq!(empty.added, Some(0));
        assert_eq!(
            diff_text(&t, empty),
            "diff --git a/empty b/empty\nnew file mode 100644\n"
        );
    }

    #[test]
    fn every_kind_of_change_is_one_entry_with_its_own_diff() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        write(wd.join("conflict"), "base\n");
        write(wd.join("mod"), "a\nb\nc\n");
        write(wd.join("gone"), "bye\n");
        write(wd.join("old-name"), "r1\nr2\nr3\nr4\nr5\n");
        write(wd.join("kind"), "a file\n");
        write(wd.join("bin"), b"\0\x01\x02");
        write(wd.join("twice"), "1\n");
        commit_all(wd);
        git(wd, &["checkout", "-q", "-b", "other"]);
        write(wd.join("conflict"), "theirs\n");
        commit_all(wd);
        git(wd, &["checkout", "-q", "main"]);
        write(wd.join("conflict"), "ours\n");
        commit_all(wd);
        assert!(!run_git(wd, &["merge", "-q", "other"]).status.success());

        write(wd.join("mod"), "a\nB\nc\nd\n");
        std::fs::remove_file(wd.join("gone")).unwrap();
        git(wd, &["mv", "old-name", "new-name"]);
        write(wd.join("new-name"), "r1\nr2\nr3\nr4\nr5\nr6\n");
        std::fs::remove_file(wd.join("kind")).unwrap();
        symlink("mod", wd.join("kind")).unwrap();
        write(wd.join("bin"), b"\0\x03");
        write(wd.join("twice"), "2\n");
        git(wd, &["add", "twice"]);
        write(wd.join("twice"), "3\n");

        let set = change_set(&t);
        let got: Vec<(&str, Option<&str>, FileChange)> = set
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.from.as_deref(), f.change))
            .collect();
        assert_eq!(
            got,
            vec![
                ("bin", None, FileChange::Modified),
                ("conflict", None, FileChange::Conflicted),
                ("gone", None, FileChange::Deleted),
                ("kind", None, FileChange::TypeChanged),
                ("mod", None, FileChange::Modified),
                ("new-name", Some("old-name"), FileChange::Renamed),
                ("twice", None, FileChange::Modified),
            ]
        );
        let counts = numstat(wd);
        assert_eq!(counts.len(), set.files.len());
        for file in &set.files {
            assert_eq!(
                counts[&file.path],
                (file.added, file.removed),
                "{}",
                file.path
            );
        }
        assert_eq!(entry(&set, "bin", false).diff, DiffBody::Binary);
        let kind = diff_text(&t, entry(&set, "kind", false));
        assert!(kind.contains("deleted file mode 100644"), "{kind}");
        assert!(kind.contains("new file mode 120000"), "{kind}");
        assert_eq!(kind.matches("diff --git").count(), 2);
        let twice = diff_text(&t, entry(&set, "twice", false));
        assert!(twice.contains("-1\n+3\n"), "{twice}");
        let renamed = diff_text(&t, entry(&set, "new-name", false));
        assert!(renamed.contains("rename from old-name\n") && renamed.contains("+r6\n"));
        assert!(diff_text(&t, entry(&set, "gone", false)).contains("-bye\n"));
        assert!(diff_text(&t, entry(&set, "conflict", false)).contains("+theirs\n"));
    }

    #[test]
    fn rm_cached_gives_a_deleted_and_an_untracked_entry() {
        let t = Tree::new();
        init(&t.workdir);
        write(t.workdir.join("f"), "kept\n");
        commit_all(&t.workdir);
        git(&t.workdir, &["rm", "-q", "--cached", "f"]);
        write(t.workdir.join("f"), "kept\nmore\n");

        let set = change_set(&t);
        assert_eq!(set.files.len(), 2);
        let deleted = entry(&set, "f", false);
        let untracked = entry(&set, "f", true);
        assert_eq!(deleted.change, FileChange::Deleted);
        assert!(diff_text(&t, deleted).contains("deleted file mode"));
        assert!(diff_text(&t, untracked).ends_with("+kept\n+more\n"));
        assert_ne!(deleted.diff, untracked.diff);
    }

    #[test]
    fn renames_with_colliding_headers_each_get_their_own_diff() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        let lines = |tag: &str| (0..8).map(|i| format!("{tag} {i}\n")).collect::<String>();
        write(wd.join("p b/q"), lines("first"));
        write(wd.join("p"), lines("second"));
        commit_all(wd);
        git(wd, &["mv", "p b/q", "r"]);
        std::fs::create_dir(wd.join("q b")).unwrap();
        git(wd, &["mv", "p", "q b/r"]);
        write(wd.join("r"), lines("first") + "first end\n");
        write(wd.join("q b/r"), lines("second") + "second end\n");
        git(wd, &["add", "-A"]);

        let set = change_set(&t);
        let first = entry(&set, "r", false);
        let second = entry(&set, "q b/r", false);
        assert_eq!(first.from.as_deref(), Some("p b/q"));
        assert_eq!(second.from.as_deref(), Some("p"));
        // Both print `diff --git a/p b/q b/r`.
        assert_eq!(expected_header(first), expected_header(second));
        let (first, second) = (diff_text(&t, first), diff_text(&t, second));
        assert!(
            first.contains("rename from p b/q\n") && first.contains("+first end\n"),
            "{first}"
        );
        assert!(
            second.contains("rename from p\n") && second.contains("+second end\n"),
            "{second}"
        );
        assert!(!first.contains("second") && !second.contains("first"));
    }

    #[test]
    fn awkward_names_are_paired_with_their_own_diff() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        let names = [
            "with space",
            "quote\"d",
            "new\nline",
            "caf\u{e9} \u{1f600}",
            "back\\slash",
        ];
        for name in names {
            write(wd.join(name), format!("old {name:?}\n"));
        }
        commit_all(wd);
        for name in names {
            write(wd.join(name), format!("new {name:?}\n"));
            write(wd.join(format!("{name}.u")), format!("loose {name:?}\n"));
        }

        let set = change_set(&t);
        assert_eq!(set.files.len(), 2 * names.len());
        for name in names {
            let tracked = diff_text(&t, entry(&set, name, false));
            assert!(
                tracked.contains(&format!("-old {name:?}\n+new {name:?}\n")),
                "{tracked}"
            );
            let loose = diff_text(&t, entry(&set, &format!("{name}.u"), true));
            assert!(loose.ends_with(&format!("+loose {name:?}\n")), "{loose}");
        }
        // The synthesised header quotes as git does.
        let loose = diff_text(&t, entry(&set, "new\nline.u", true));
        assert!(
            loose.starts_with("diff --git \"a/new\\nline.u\" \"b/new\\nline.u\"\n"),
            "{loose}"
        );
    }

    #[test]
    fn an_untracked_symlink_shows_its_link_text_never_its_target() {
        let t = Tree::new();
        init(&t.workdir);
        write(t.outside.join("secret"), "TOP-SECRET-BYTES\n");
        symlink(t.outside.join("secret"), t.workdir.join("link")).unwrap();
        write(t.workdir.join("plain"), "VISIBLE-BYTES\n");

        let set = change_set(&t);
        let link = diff_text(&t, entry(&set, "link", true));
        assert!(link.contains("new file mode 120000\n"));
        let target = t.outside.join("secret");
        assert!(
            link.contains(&format!("+{}\n\\ No newline", target.display())),
            "{link}"
        );
        let blobs = all_blobs(&t);
        assert!(blobs.iter().all(|b| !contains(b, b"TOP-SECRET-BYTES")));
        // Positive control: the scan sees an untracked file's content.
        assert!(blobs.iter().any(|b| contains(b, b"VISIBLE-BYTES")));
    }

    #[test]
    fn changes_are_capped_by_path_and_counted() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        for i in 0..300 {
            write(wd.join(format!("t{i:03}")), "a\n");
        }
        commit_all(wd);
        for i in 0..300 {
            write(wd.join(format!("t{i:03}")), "b\n");
            write(wd.join(format!("u{i:03}")), "c\n");
        }
        let set = change_set(&t);
        assert_eq!(set.total, 600);
        assert_eq!(set.files.len(), limits::WORKDIR_CHANGES_MAX);
        assert_eq!(set.files[0].path, "t000");
        assert_eq!(set.files[499].path, "u199");
        assert!(
            set.files
                .iter()
                .all(|f| matches!(f.diff, DiffBody::Blob(_)))
        );
    }

    #[test]
    fn untracked_files_past_the_cap_are_counted_not_kept() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        let n = limits::WORKDIR_CHANGES_MAX + 100;
        // Created in reverse, so no order but the sort's can pass.
        for i in (0..n).rev() {
            write(wd.join(format!("u{i:04}")), "c\n");
        }
        // A nested repository is listed as `nested/` and is no entry.
        write(wd.join("nested/inner"), "x\n");
        init(&wd.join("nested"));
        let set = change_set(&t);
        assert_eq!(set.total as usize, n);
        assert_eq!(set.files.len(), limits::WORKDIR_CHANGES_MAX);
        let paths: Vec<String> = set.files.iter().map(|f| f.path.clone()).collect();
        let expected: Vec<String> = (0..limits::WORKDIR_CHANGES_MAX)
            .map(|i| format!("u{i:04}"))
            .collect();
        assert_eq!(paths, expected);
        assert!(
            set.files
                .iter()
                .all(|f| f.change == FileChange::Untracked && matches!(f.diff, DiffBody::Blob(_)))
        );
    }

    #[test]
    fn tracked_entries_past_the_cap_are_counted_and_the_kept_keep_their_numstat() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        let tracked = 400;
        let loose = 300;
        for i in 0..tracked {
            write(wd.join(format!("t{i:03}")), "a\n");
        }
        let renamed: String = (0..8).map(|i| format!("r {i}\n")).collect();
        write(wd.join("old"), &renamed);
        write(wd.join("bin"), b"\0\x01");
        commit_all(wd);
        // A different line count each, so a numstat paired with the wrong
        // entry shows.
        for i in 0..tracked {
            let lines: String = (0..=i % 7).map(|l| format!("{l}\n")).collect();
            write(wd.join(format!("t{i:03}")), lines);
        }
        git(wd, &["mv", "old", "t-new"]);
        write(wd.join("t-new"), renamed + "r end\n");
        write(wd.join("bin"), b"\0\x02");
        // Untracked entries sort first, pushing tracked ones past the cap.
        for i in 0..loose {
            write(wd.join(format!("a{i:03}")), "c\n");
        }

        let set = change_set(&t);
        assert_eq!(set.total as usize, tracked + loose + 2);
        assert_eq!(set.files.len(), limits::WORKDIR_CHANGES_MAX);
        let kept_tracked = limits::WORKDIR_CHANGES_MAX - loose - 2;
        let mut expected: Vec<String> = (0..loose).map(|i| format!("a{i:03}")).collect();
        expected.push("bin".into());
        expected.push("t-new".into());
        expected.extend((0..kept_tracked).map(|i| format!("t{i:03}")));
        let paths: Vec<&str> = set.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, expected);

        let counts = numstat(wd);
        for file in set
            .files
            .iter()
            .filter(|f| f.change != FileChange::Untracked)
        {
            assert_eq!(
                counts[&file.path],
                (file.added, file.removed),
                "{}",
                file.path
            );
        }
        assert_eq!(entry(&set, "bin", false).diff, DiffBody::Binary);
        let new = entry(&set, "t-new", false);
        assert_eq!(
            (new.from.as_deref(), new.change),
            (Some("old"), FileChange::Renamed)
        );
        assert!(diff_text(&t, new).contains("+r end\n"));
        let k = kept_tracked - 1;
        let last = diff_text(&t, entry(&set, &format!("t{k:03}"), false));
        let added: String = (0..=k % 7).map(|l| format!("+{l}\n")).collect();
        assert!(last.ends_with(&format!("-a\n{added}")), "{last}");
    }

    #[test]
    fn a_header_shared_with_an_entry_past_the_cap_is_not_trusted() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        // `a-r` (renamed from `p b/q`) and `q b/a-r` (renamed from `p`) both
        // print `diff --git a/p b/q b/a-r`, in consecutive sections;
        // untracked `m…` push `q b/a-r` past the cap, so only `a-r` is kept.
        let lines = |tag: &str| (0..8).map(|i| format!("{tag} {i}\n")).collect::<String>();
        write(wd.join("p b/q"), lines("first"));
        write(wd.join("p"), lines("second"));
        commit_all(wd);
        git(wd, &["mv", "p b/q", "a-r"]);
        std::fs::create_dir(wd.join("q b")).unwrap();
        git(wd, &["mv", "p", "q b/a-r"]);
        write(wd.join("a-r"), lines("first") + "first end\n");
        write(wd.join("q b/a-r"), lines("second") + "second end\n");
        git(wd, &["add", "-A"]);
        for i in 0..limits::WORKDIR_CHANGES_MAX - 1 {
            write(wd.join(format!("m{i:03}")), "c\n");
        }

        let set = change_set(&t);
        assert_eq!(set.total as usize, limits::WORKDIR_CHANGES_MAX + 1);
        let first = entry(&set, "a-r", false);
        assert_eq!(first.from.as_deref(), Some("p b/q"));
        assert!(set.files.iter().all(|f| f.path != "q b/a-r"));
        let first = diff_text(&t, first);
        assert!(
            first.contains("rename from p b/q\n") && first.contains("+first end\n"),
            "{first}"
        );
        assert!(!first.contains("second"), "{first}");
    }

    #[test]
    fn records_split_at_nul_and_cut_a_long_one() {
        let long = vec![b'x'; RECORD_MAX + 10];
        let mut out = b"a\0".to_vec();
        out.extend_from_slice(&long);
        out.extend_from_slice(b"\0b\0last");
        for whole in [true, false] {
            let mut got: Vec<(Vec<u8>, bool)> = Vec::new();
            let mut each = |r: &[u8], w: bool| got.push((r.to_vec(), w));
            if whole {
                records(&mut &out[..], &mut each).unwrap();
            } else {
                records(&mut Trickle(&out), &mut each).unwrap();
            }
            assert_eq!(got.len(), 4);
            assert_eq!(got[0], (b"a".to_vec(), true));
            assert_eq!(got[1], (long[..RECORD_MAX].to_vec(), false));
            assert_eq!(got[2], (b"b".to_vec(), true));
            assert_eq!(got[3], (b"last".to_vec(), true));
        }
    }

    #[test]
    fn a_huge_patch_is_too_large_and_never_held_whole() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        write(wd.join("big"), "small\n");
        commit_all(wd);
        let huge: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        write(wd.join("big"), &huge);
        write(wd.join("loose-big"), &huge);
        let full = git(wd, &["diff", "HEAD", "--", "big"]).len() as u64;
        assert!(full > 2 * limits::WORKDIR_DIFF_MAX_BYTES);

        let (answer, kept) = answer_with(&t, isolated(&t));
        let ChangesResult::Changes(set) = answer else {
            panic!("expected changes");
        };
        assert_eq!(
            entry(&set, "big", false).diff,
            DiffBody::TooLarge { bytes: full }
        );
        assert_eq!(
            entry(&set, "loose-big", true).diff,
            DiffBody::TooLarge {
                bytes: huge.len() as u64
            }
        );
        assert!(
            kept.peak <= limits::WORKDIR_DIFF_MAX_BYTES,
            "held {}",
            kept.peak
        );
        assert!(kept.peak > 0, "the header was kept before the cap");
    }

    #[test]
    fn diffs_past_the_snapshot_budget_are_omitted() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        for i in 0..12 {
            write(wd.join(format!("f{i:02}")), "x\n");
        }
        write(wd.join("z-big"), "x\n");
        commit_all(wd);
        // Each patch just under the per-file cap: eight fit, not nine.
        let lines = 100_000;
        let body: String = (0..lines).map(|i| format!("l{i:07}\n")).collect();
        // A `+` per line, plus headers.
        let patch = (body.len() + lines) as u64;
        assert!(patch > limits::WORKDIR_DIFFS_MAX_BYTES / 9);
        assert!(patch + 200 < limits::WORKDIR_DIFF_MAX_BYTES);
        for i in 0..12 {
            write(wd.join(format!("f{i:02}")), &body);
        }
        // Past the per-file cap, and streamed once the budget is spent: still
        // `TooLarge`, with its size, not `Omitted`.
        write(wd.join("z-big"), body.repeat(3));
        let (answer, kept) = answer_with(&t, isolated(&t));
        let ChangesResult::Changes(set) = answer else {
            panic!("expected changes");
        };
        let bodies: Vec<bool> = set
            .files
            .iter()
            .map(|f| matches!(f.diff, DiffBody::Blob(_)))
            .collect();
        let kept_blobs = bodies.iter().take_while(|b| **b).count();
        assert_eq!(kept_blobs, 8, "{bodies:?}");
        assert!(bodies[kept_blobs..].iter().all(|b| !b), "{bodies:?}");
        let (big, rest) = set.files[kept_blobs..].split_last().unwrap();
        assert!(rest.iter().all(|f| f.diff == DiffBody::Omitted));
        assert_eq!(big.path, "z-big");
        let full = git(wd, &["diff", "HEAD", "--", "z-big"]).len() as u64;
        assert_eq!(big.diff, DiffBody::TooLarge { bytes: full });
        let stored: u64 = set
            .files
            .iter()
            .filter_map(|f| match &f.diff {
                DiffBody::Blob(BlobRef { size, .. }) => Some(*size),
                _ => None,
            })
            .sum();
        assert!(stored <= limits::WORKDIR_DIFFS_MAX_BYTES);
        assert!(
            kept.peak <= limits::WORKDIR_DIFFS_MAX_BYTES,
            "held {}",
            kept.peak
        );
    }

    /// A repo whose own config would run code on a diff, with a marker per
    /// way in, and an untouched file to diff for each.
    fn hostile(t: &Tree) -> Vec<PathBuf> {
        let wd = &t.workdir;
        init(wd);
        let mark = |name: &str| t.outside.join(name);
        let script = |name: &str| {
            let path = t.outside.join(format!("{name}.sh"));
            write(
                path.clone(),
                format!("#!/bin/sh\ntouch '{}'\ncat\n", mark(name).display()),
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.display().to_string()
        };
        for name in ["a.txt", "b.tc", "c.x", "d.inc", "e.pr"] {
            write(wd.join(name), "old\n");
        }
        commit_all(wd);
        let config = |key: &str, value: &str| git(wd, &["config", key, value]);
        config("core.fsmonitor", &script("fsmonitor"));
        config("diff.external", &script("external"));
        config("diff.tc.textconv", &script("textconv"));
        config("filter.x.clean", &script("filter"));
        config("filter.pr.process", &script("process"));
        let included = t.outside.join("included.gitconfig");
        write(
            included.clone(),
            format!("[filter \"in.c\"]\n\tclean = {}\n", script("include")),
        );
        config("include.path", included.to_str().unwrap());
        write(
            wd.join(".git/info/attributes"),
            "*.tc diff=tc\n*.x filter=x\n*.inc filter=in.c\n*.pr filter=pr\n",
        );
        for name in ["a.txt", "b.tc", "c.x", "d.inc", "e.pr"] {
            write(wd.join(name), "new\n");
        }
        [
            "fsmonitor",
            "external",
            "textconv",
            "filter",
            "include",
            "process",
        ]
        .map(mark)
        .to_vec()
    }

    #[test]
    fn repo_config_runs_nothing() {
        let t = Tree::new();
        let marks = hostile(&t);
        let set = change_set(&t);
        assert_eq!(set.files.len(), 5);
        for mark in &marks {
            assert!(!mark.exists(), "{} ran", mark.display());
        }
        // Positive control: a plain `git diff` there runs every one of them
        // (the external diff, and without it the textconv driver). The
        // `process` script speaks no protocol, so git may fail after it ran.
        run_git(&t.workdir, &["diff", "HEAD"]);
        run_git(&t.workdir, &["diff", "HEAD", "--no-ext-diff"]);
        for ran in &marks {
            assert!(ran.exists(), "{} did not run", ran.display());
        }
    }

    #[test]
    fn a_filter_from_global_config_does_run() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        write(wd.join("f.g"), "old\n");
        commit_all(wd);
        let mark = t.outside.join("global-ran");
        let global = t.outside.join("global.gitconfig");
        write(
            global.clone(),
            format!(
                "[filter \"g\"]\n\tclean = touch '{}'; cat\n",
                mark.display()
            ),
        );
        write(wd.join(".git/info/attributes"), "*.g filter=g\n");
        write(wd.join("f.g"), "new\n");
        let git = isolated(&t).with_env("GIT_CONFIG_GLOBAL", &global);
        assert!(matches!(answer_with(&t, git).0, ChangesResult::Changes(_)));
        assert!(mark.exists());
    }

    #[test]
    fn a_submodule_is_one_short_entry_and_runs_nothing_inside() {
        let t = Tree::new();
        let wd = &t.workdir;
        let source = t.outside.join("subsrc");
        std::fs::create_dir(&source).unwrap();
        init(&source);
        write(source.join("f.tc"), "a\n");
        commit_all(&source);
        init(wd);
        write(wd.join("a-file"), "old\n");
        commit_all(wd);
        git(
            wd,
            &["submodule", "add", "-q", source.to_str().unwrap(), "sub"],
        );
        git(wd, &["commit", "-q", "-m", "sub"]);
        // Inside the submodule: a diff driver that runs a command, and a
        // commit whose subject `log` mode would print.
        let mark = t.outside.join("ran-in-submodule");
        let driver = t.outside.join("driver.sh");
        write(
            driver.clone(),
            format!("#!/bin/sh\ntouch '{}'\ncat \"$2\"\n", mark.display()),
        );
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sub = wd.join("sub");
        let attributes = git(&sub, &["rev-parse", "--git-path", "info/attributes"]);
        let attributes = sub.join(String::from_utf8(attributes).unwrap().trim());
        write(attributes, "*.tc diff=tc\n");
        git(
            &sub,
            &["config", "diff.tc.command", driver.to_str().unwrap()],
        );
        write(sub.join("f.tc"), "b\n");
        git(
            &sub,
            &["commit", "-q", "-a", "-m", "SUBJECT-FROM-SUBMODULE"],
        );
        git(wd, &["config", "diff.submodule", "diff"]);
        write(wd.join("a-file"), "new\n");

        let set = change_set(&t);
        assert!(!mark.exists(), "the submodule's diff driver ran");
        let got: Vec<(&str, FileChange)> = set
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.change))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a-file", FileChange::Modified),
                ("sub", FileChange::Modified)
            ]
        );
        let sub_diff = diff_text(&t, entry(&set, "sub", false));
        assert!(
            sub_diff.starts_with("diff --git a/sub b/sub\n"),
            "{sub_diff}"
        );
        assert!(sub_diff.contains("-Subproject commit "), "{sub_diff}");
        let neighbour = diff_text(&t, entry(&set, "a-file", false));
        assert!(!neighbour.contains("Submodule") && !neighbour.contains("SUBJECT"));
        assert!(all_blobs(&t).iter().all(|b| !contains(b, b"SUBJECT-FROM")));
        // Positive control: the repo's own setting runs the driver.
        git(wd, &["diff", "HEAD", "--no-ext-diff", "--no-textconv"]);
        assert!(mark.exists(), "the plain diff did not run the driver");
    }

    #[test]
    fn a_per_file_diff_keeps_only_its_own_sections() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        // Quoted, so its diff comes from the per-file call, whose
        // `:(literal)q"x` also matches the directory `q"x/`.
        write(wd.join("q\"x"), "the file\n");
        commit_all(wd);
        git(wd, &["rm", "-q", "q\"x"]);
        write(wd.join("q\"x/evil"), "EVIL-CONTENT\n");
        git(wd, &["add", "-A"]);

        let set = change_set(&t);
        let gone = entry(&set, "q\"x", false);
        assert_eq!(gone.change, FileChange::Deleted);
        let gone = diff_text(&t, gone);
        assert!(gone.contains("-the file\n"), "{gone}");
        assert!(!gone.contains("EVIL"), "{gone}");
        let added = diff_text(&t, entry(&set, "q\"x/evil", false));
        assert!(added.contains("+EVIL-CONTENT\n"), "{added}");
    }

    #[test]
    fn a_conflict_whose_worktree_matches_head_is_still_listed() {
        let t = Tree::new();
        let wd = &t.workdir;
        init(wd);
        write(wd.join("c"), "base\n");
        write(wd.join("other"), "x\n");
        commit_all(wd);
        git(wd, &["checkout", "-q", "-b", "theirs"]);
        git(wd, &["rm", "-q", "c"]);
        git(wd, &["commit", "-q", "-m", "delete"]);
        git(wd, &["checkout", "-q", "main"]);
        write(wd.join("c"), "ours\n");
        commit_all(wd);
        assert!(!run_git(wd, &["merge", "-q", "theirs"]).status.success());
        // Positive control for the premise: `git diff HEAD` lists nothing.
        assert!(git(wd, &["diff", "HEAD", "--name-only"]).is_empty());

        let set = change_set(&t);
        assert_eq!(set.total, 1);
        let c = entry(&set, "c", false);
        assert_eq!(
            (c.change, c.added, c.removed),
            (FileChange::Conflicted, Some(0), Some(0))
        );
        assert!(matches!(c.diff, DiffBody::Blob(ref b) if b.size == 0));
    }

    /// A fake git that records each start and then sleeps for `secs`.
    fn sleeping_git(t: &Tree, secs: u32) -> (Git, PathBuf) {
        let script = t.outside.join("slow-git");
        let count = t.outside.join("slow-git.count");
        write(
            script.clone(),
            format!(
                "#!/bin/sh\necho started >> '{}'\nsleep {secs}\n",
                count.display()
            ),
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (Git::new(script), count)
    }

    fn starts(count: &Path) -> usize {
        std::fs::read_to_string(count).map_or(0, |s| s.lines().count())
    }

    #[test]
    fn git_is_killed_at_the_deadline_and_a_late_request_spawns_none() {
        let t = Tree::new();
        let (git, count) = sleeping_git(&t, 30);
        let reader = reader(&t, git);
        let query = t.query("");
        let started = Instant::now();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                reader.snapshot(Instant::now() + Duration::from_secs(1), |s| {
                    s.changes(&query)
                })
            });
            // Queue the second once the first holds the lock (its git ran).
            while starts(&count) == 0 {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "git never started"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let second = scope.spawn(|| {
                reader.snapshot(Instant::now() + Duration::from_millis(100), |s| {
                    s.changes(&query)
                })
            });
            assert_eq!(
                first.join().unwrap().unwrap(),
                ChangesResult::Failed {
                    reason: "timed out".into()
                }
            );
            assert!(matches!(
                second.join().unwrap(),
                Err(Error::WorkdirDeadline)
            ));
        });
        // Killed with its whole group: the `sleep` held stdout open.
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(starts(&count), 1);
    }

    #[test]
    fn repo_scope_filter_names_are_found_dots_and_all() {
        let out = b"global\0filter.lfs.clean\ngit-lfs clean\0\
                    local\0filter.a.b.c.clean\nx\0\
                    worktree\0filter.w.process\ny\0\
                    local\0filter.s.smudge\nz\0\
                    local\0filter.a.b.c.process\nx\0\
                    local\0filter.bare.clean\0";
        let names: Vec<String> = repo_filters(out)
            .iter()
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        assert_eq!(names, ["a.b.c", "w", "bare"]);
        assert!(repo_filters(b"").is_empty());
    }

    #[derive(Default)]
    struct Recorder {
        sections: Vec<(Vec<u8>, Vec<u8>)>,
        before: Vec<u8>,
    }

    impl Sections for Recorder {
        fn header(&mut self, line: &[u8]) {
            self.sections.push((line.to_vec(), Vec::new()));
        }

        fn bytes(&mut self, bytes: &[u8]) {
            match self.sections.last_mut() {
                Some((_, body)) => body.extend_from_slice(bytes),
                None => self.before.extend_from_slice(bytes),
            }
        }
    }

    /// Reads one byte at a time, splitting every line across reads.
    struct Trickle<'b>(&'b [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((&first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = first;
            self.0 = rest;
            Ok(1)
        }
    }

    #[test]
    fn a_patch_splits_only_at_section_headers() {
        let patch = b"diff --git a/x b/x\n+diff --git a/y b/y\n diff --git\ndiff --gi\n\
                      diff --git a/z b/z\n-last";
        for whole in [true, false] {
            let mut rec = Recorder::default();
            if whole {
                split(&mut &patch[..], &mut rec).unwrap();
            } else {
                split(&mut Trickle(patch), &mut rec).unwrap();
            }
            let headers: Vec<&[u8]> = rec.sections.iter().map(|(h, _)| &h[..]).collect();
            assert_eq!(headers, [&b"diff --git a/x b/x"[..], b"diff --git a/z b/z"]);
            let joined: Vec<u8> = rec.sections.iter().flat_map(|(_, b)| b.clone()).collect();
            assert_eq!(joined, patch);
            assert!(rec.before.is_empty());
        }
    }
}
