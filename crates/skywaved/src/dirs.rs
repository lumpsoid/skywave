//! Directory listings for workdir completion (`GET /v1/dirs`) and the
//! "canonical and under a root" check that creating a session shares (D14).
//!
//! A listing names only directories under the configured roots: the listed
//! directory is canonicalised before the check, so `..` and symlinks cannot
//! escape, and a symlinked entry is offered only if its canonical target is
//! a directory under a root too. Names that are not UTF-8 or hold control
//! characters are skipped: a client prints them. The scan stops after
//! [`SCAN_CAP`] entries and the reply holds at most [`REPLY_CAP`] names, so
//! a huge directory costs a bounded amount of work. Dotfiles are included;
//! the client filters what it shows.
//!
//! Blocking: file system calls only, never the database.

use std::path::{Path, PathBuf};

use skywave_proto::{DirsResult, RejectReason};

/// Directory entries read at most per listing.
pub const SCAN_CAP: usize = 10_000;
/// Names sent at most per listing.
pub const REPLY_CAP: usize = 500;

/// The canonical form of `dir` if it is an existing directory under one of
/// `roots`.
///
/// Canonicalizing resolves `..` and symlinks before the check, so neither
/// can escape a root. Roots are canonicalized too; a root that does not
/// exist admits nothing. A relative `dir` names no directory on the server
/// and is not allowed. A target that is not UTF-8 cannot travel on the wire
/// and is not allowed either.
pub fn canonical_under_roots(roots: &[PathBuf], dir: &str) -> Result<String, RejectReason> {
    let path = Path::new(dir);
    if !path.is_absolute() {
        return Err(RejectReason::WorkdirNotAllowed);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| RejectReason::WorkdirNotFound)?;
    if !canonical.is_dir() {
        return Err(RejectReason::WorkdirNotFound);
    }
    if !under(&canonical_roots(roots), &canonical) {
        return Err(RejectReason::WorkdirNotAllowed);
    }
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| RejectReason::WorkdirNotAllowed)
}

/// Lists the subdirectories of `dir` for completion; an empty `dir` lists
/// the roots. See the module docs.
pub fn list_dirs(roots: &[PathBuf], dir: &str) -> DirsResult {
    list_dirs_capped(roots, dir, SCAN_CAP, REPLY_CAP)
}

fn list_dirs_capped(roots: &[PathBuf], dir: &str, scan_cap: usize, reply_cap: usize) -> DirsResult {
    let canonical = canonical_roots(roots);
    let root_names = || {
        canonical
            .iter()
            .filter_map(|root| root.to_str().map(str::to_owned))
            .collect()
    };
    if dir.is_empty() {
        return DirsResult::Roots {
            roots: root_names(),
        };
    }
    let listed = match canonical_under_roots(roots, dir) {
        Ok(listed) => listed,
        Err(RejectReason::WorkdirNotFound) => return DirsResult::NotFound,
        Err(_) => {
            return DirsResult::NotAllowed {
                roots: root_names(),
            };
        }
    };
    let Ok(entries) = std::fs::read_dir(&listed) else {
        return DirsResult::NotFound;
    };
    let mut names = Vec::new();
    let mut truncated = false;
    for (scanned, entry) in entries.enumerate() {
        if scanned == scan_cap {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.chars().any(char::is_control) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let is_dir = if kind.is_symlink() {
            std::fs::canonicalize(entry.path())
                .is_ok_and(|target| target.is_dir() && under(&canonical, &target))
        } else {
            kind.is_dir()
        };
        if is_dir {
            names.push(name);
        }
    }
    names.sort_unstable();
    if names.len() > reply_cap {
        names.truncate(reply_cap);
        truncated = true;
    }
    DirsResult::Listed {
        dir: listed,
        names,
        truncated,
    }
}

/// The roots that exist, canonical, in configured order.
fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .filter(|root| root.is_dir())
        .collect()
}

fn under(canonical_roots: &[PathBuf], canonical: &Path) -> bool {
    canonical_roots
        .iter()
        .any(|root| canonical.starts_with(root))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A temp dir holding `root/` (the only root) and `outside/`.
    struct Tree {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    impl Tree {
        fn new() -> Tree {
            let tmp = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(tmp.path()).unwrap();
            let root = base.join("root");
            let outside = base.join("outside");
            std::fs::create_dir_all(root.join("project/src")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            Tree {
                _tmp: tmp,
                root,
                outside,
            }
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }

        fn list(&self, dir: &Path) -> DirsResult {
            list_dirs(&self.roots(), dir.to_str().unwrap())
        }

        fn names(&self, dir: &Path) -> Vec<String> {
            match self.list(dir) {
                DirsResult::Listed { names, .. } => names,
                other => panic!("expected a listing, got {other:?}"),
            }
        }

        fn root_names(&self) -> Vec<String> {
            vec![self.root.to_str().unwrap().to_owned()]
        }
    }

    #[test]
    fn empty_dir_lists_the_canonical_roots_that_exist() {
        let t = Tree::new();
        let roots = vec![t.root.join("project/.."), t.root.join("missing")];
        assert_eq!(
            list_dirs(&roots, ""),
            DirsResult::Roots {
                roots: t.root_names()
            }
        );
    }

    #[test]
    fn lists_only_directories_sorted_with_dotfiles() {
        let t = Tree::new();
        std::fs::create_dir(t.root.join(".hidden")).unwrap();
        std::fs::create_dir(t.root.join("alpha")).unwrap();
        std::fs::write(t.root.join("file.txt"), "x").unwrap();
        assert_eq!(
            t.list(&t.root),
            DirsResult::Listed {
                dir: t.root.to_str().unwrap().to_owned(),
                names: vec![".hidden".into(), "alpha".into(), "project".into()],
                truncated: false,
            }
        );
    }

    #[test]
    fn dot_dot_cannot_escape_a_root() {
        let t = Tree::new();
        assert_eq!(
            t.list(&t.root.join("project/../..")),
            DirsResult::NotAllowed {
                roots: t.root_names()
            }
        );
        // Inside a root, `..` is fine and the canonical dir is named.
        match t.list(&t.root.join("project/src/..")) {
            DirsResult::Listed { dir, names, .. } => {
                assert_eq!(dir, t.root.join("project").to_str().unwrap());
                assert_eq!(names, vec!["src".to_owned()]);
            }
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    #[test]
    fn a_dir_above_a_root_offers_the_roots() {
        let t = Tree::new();
        assert_eq!(
            t.list(t.root.parent().unwrap()),
            DirsResult::NotAllowed {
                roots: t.root_names()
            }
        );
        assert_eq!(
            list_dirs(&t.roots(), "relative/path"),
            DirsResult::NotAllowed {
                roots: t.root_names()
            }
        );
        assert_eq!(t.list(&t.root.join("nope")), DirsResult::NotFound);
    }

    #[test]
    fn symlinks_are_offered_only_into_a_root_and_to_directories() {
        let t = Tree::new();
        std::fs::write(t.root.join("file.txt"), "x").unwrap();
        symlink(&t.outside, t.root.join("out")).unwrap();
        symlink(t.root.join("project/src"), t.root.join("src-link")).unwrap();
        symlink(t.root.join("file.txt"), t.root.join("file-link")).unwrap();
        symlink(t.root.join("gone"), t.root.join("dangling")).unwrap();
        assert_eq!(
            t.names(&t.root),
            vec!["project".to_owned(), "src-link".to_owned()]
        );
        // Listing through the link that leaves the root is refused.
        assert_eq!(
            t.list(&t.root.join("out")),
            DirsResult::NotAllowed {
                roots: t.root_names()
            }
        );
    }

    #[test]
    fn names_with_control_characters_are_skipped() {
        let t = Tree::new();
        std::fs::create_dir(t.root.join("evil\x1b[2Jname")).unwrap();
        std::fs::create_dir(t.root.join("tab\tname")).unwrap();
        assert_eq!(t.names(&t.root), vec!["project".to_owned()]);
    }

    #[test]
    fn scan_and_reply_caps_truncate() {
        let t = Tree::new();
        for i in 0..20 {
            std::fs::create_dir(t.root.join(format!("d{i:02}"))).unwrap();
        }
        let dir = t.root.to_str().unwrap();
        match list_dirs_capped(&t.roots(), dir, 100, 5) {
            DirsResult::Listed {
                names, truncated, ..
            } => {
                assert_eq!(names, vec!["d00", "d01", "d02", "d03", "d04"]);
                assert!(truncated);
            }
            other => panic!("expected a listing, got {other:?}"),
        }
        match list_dirs_capped(&t.roots(), dir, 10, 500) {
            DirsResult::Listed {
                names, truncated, ..
            } => {
                assert!(names.len() <= 10);
                assert!(truncated);
            }
            other => panic!("expected a listing, got {other:?}"),
        }
        match list_dirs_capped(&t.roots(), dir, 100, 500) {
            DirsResult::Listed {
                names, truncated, ..
            } => {
                assert_eq!(names.len(), 21);
                assert!(!truncated);
            }
            other => panic!("expected a listing, got {other:?}"),
        }
    }
}
