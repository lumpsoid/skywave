//! The path rule: an argument that names a file must stay inside the workdir.
//! The check is lexical and physical, so a symlink cannot lead out of it.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Checks one positional argument, after unquoting. `Err` names the word.
pub(crate) fn positional(word: &str, workdir: &Path) -> Result<(), String> {
    if inside(&workdir.join(word), workdir) {
        Ok(())
    } else {
        Err(format!("`{word}` is outside the workdir"))
    }
}

/// Lexically, `.` and `..` resolved as text, the path stays in the workdir.
/// Physically, see `physical`.
fn inside(path: &Path, workdir: &Path) -> bool {
    normalise(path).starts_with(workdir) && physical(path, workdir)
}

/// The longest prefix of the path as written that exists, lstat-wise, must
/// resolve inside the workdir to a regular file or a directory. Resolving the
/// path as written, not after `..` is folded away, matters: with `out` a
/// symlink to `/etc`, `out/../passwd` folds to `passwd` but the kernel reads
/// `/passwd`. A dangling symlink exists by lstat but cannot resolve, so it
/// asks. A FIFO, socket or device resolves but would block or act when read.
fn physical(path: &Path, workdir: &Path) -> bool {
    let Some(existing) = path
        .ancestors()
        .find(|prefix| fs::symlink_metadata(prefix).is_ok())
    else {
        return false;
    };
    let Ok(real) = fs::canonicalize(existing) else {
        return false;
    };
    real.starts_with(workdir)
        && fs::metadata(&real).is_ok_and(|meta| meta.is_file() || meta.is_dir())
}

/// Resolves `.` and `..` as text, without touching the disk.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
