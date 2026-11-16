//! Table tests for the read-only check. Each `ASK` row is an absence check: the
//! checker must not approve it. Each `SAFE` row is the positive control: the
//! checker must approve it, so a checker that always asks cannot pass.

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use skywave_safety::{Verdict, check};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const SAFE: &[&str] = &[
    "ls -la src | grep foo",
    "cat a.txt",
    "head -n 5 src/a.rs",
    "cat src/../a.txt",
    "wc -l a.txt 2>/dev/null",
    "grep -rn foo src 2>&1 | head",
    "cat a.txt 2>&1",
    "cat a.txt && echo done; pwd",
    "cat <<'EOF'\nhi\nEOF",
    "grep -e '*.rs' a.txt",
    "cat \"a.txt\"",
    "echo /etc/passwd",
    "cat < a.txt",
    "cat -- a.txt",
    "tail -n 5 a.txt",
    "tail -n +5 a.txt",
];

const ASK: &[&str] = &[
    "rm a.txt",
    "cat a.txt > b.txt",
    "cat a.txt >> /dev/stdout",
    "echo $(rm -rf x)",
    "echo `id`",
    "cat $HOME/x",
    "cat ~/x",
    "cat /etc/passwd",
    "cat ../x",
    "cat out/passwd",
    // Lexical folding alone would pass this: `out/..` is `/`, not the workdir.
    "cat out/../passwd",
    "cat link",
    "cat dangling",
    "cat /dev/zero",
    "FOO=1 cat a.txt",
    "(cat a.txt)",
    "{ cat a.txt; }",
    "cat a.txt &",
    "tail -f a.txt",
    // Bundle rule: `-nf` holds `f`, so it asks.
    "tail -nf a.txt",
    "tail --fol a.txt",
    // A `+` word with `f` or `F` follows the file.
    "tail +1f a.txt",
    "ls *.rs",
    "diff <(ls a) <(ls b)",
    "echo 'unterminated",
    "if then fi",
    "",
    "/bin/cat a.txt",
    "./cat a.txt",
    "cat --file=/etc/x",
    "grep -f/etc/x a.txt",
    // Attached short-option values are paths too: `link` is a symlink out.
    "grep -flink a.txt",
    "grep -rflink a.txt",
    "diff -Xlink a.txt b.txt",
    "git status",
    "find . -delete",
    "cat {/etc/passwd,a.txt}",
    "cat @(/etc/passwd)",
    "cat a.txt >&b.txt",
    "cat a.txt <> b.txt",
    // Descriptors other than 0-2 are inherited, not ours.
    "cat a.txt >&3",
    "cat a.txt 3>&1",
    "time cat a.txt",
    "cat \\\n/etc/passwd",
    // `printf -v` expands a subscript in its name, and `-v PATH` sets PATH.
    "printf -v 'a[$(touch pwned1)]' x",
    "printf -v PATH . ; cat a.txt",
    // `diff -l` spawns `pr`.
    "diff -l a.txt b.txt",
    "diff --paginate a.txt b.txt",
    // `diff -r` and `grep -R` follow symlinks inside the tree.
    "diff -r d1 d2",
    "grep -R . sub",
    // `ls -L` follows the links it lists.
    "ls -LR sub",
    // `wc --files0-from` reads the files its list names.
    "wc --files0-from=list0",
    "printf '/etc/hostname\\0' | wc --files0-from=-",
    // A `=` value goes through the path rule as before.
    "diff --from-file=/etc/hostname a.txt",
];

/// A workdir with `src/a.rs`, `a.txt`, and symlinks: `out -> /etc` (a
/// directory out), `link -> /etc/hostname` (a file out), `dangling ->
/// /nonexistent/x`, and `sub/escape -> /etc`. Canonical, as the checker
/// requires. The `TempDir` must outlive the path.
fn workdir() -> Result<(TempDir, PathBuf), Box<dyn Error>> {
    let dir = TempDir::new()?;
    let root = fs::canonicalize(dir.path())?;
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("src/a.rs"), "fn main() {}\n")?;
    fs::write(root.join("a.txt"), "hello\n")?;
    fs::create_dir(root.join("sub"))?;
    std::os::unix::fs::symlink("/etc", root.join("out"))?;
    std::os::unix::fs::symlink("/etc/hostname", root.join("link"))?;
    std::os::unix::fs::symlink("/nonexistent/x", root.join("dangling"))?;
    std::os::unix::fs::symlink("/etc", root.join("sub/escape"))?;
    Ok((dir, root))
}

#[test]
fn safe_commands_are_approved() -> TestResult {
    let (_dir, root) = workdir()?;
    for command in SAFE {
        let verdict = check(command, &root);
        assert!(
            matches!(verdict, Verdict::Safe { .. }),
            "expected safe: {command:?}: {verdict:?}"
        );
    }
    Ok(())
}

#[test]
fn unproven_commands_ask() -> TestResult {
    let (_dir, root) = workdir()?;
    for command in ASK {
        let verdict = check(command, &root);
        assert!(
            matches!(verdict, Verdict::Ask { .. }),
            "expected ask: {command:?}: {verdict:?}"
        );
    }
    Ok(())
}

#[test]
fn absolute_path_inside_the_workdir_is_safe() -> TestResult {
    let (_dir, root) = workdir()?;
    let command = format!("cat {}/a.txt", root.display());
    let verdict = check(&command, &root);
    assert!(matches!(verdict, Verdict::Safe { .. }), "{verdict:?}");
    Ok(())
}

#[test]
fn fifo_is_asked() -> TestResult {
    let (_dir, root) = workdir()?;
    let fifo = root.join("fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|status| status.success());
    if !made {
        // No mkfifo here: this row cannot be built, so it is skipped.
        return Ok(());
    }
    let verdict = check("cat fifo", &root);
    assert!(matches!(verdict, Verdict::Ask { .. }), "{verdict:?}");
    Ok(())
}

/// `{ ` nested `depth` times around `cat a.txt`, closed by `; }` at each level.
fn nested(depth: usize) -> String {
    format!("{}cat a.txt{}", "{ ".repeat(depth), "; }".repeat(depth))
}

#[test]
fn deep_nesting_asks_without_crashing() -> TestResult {
    let (_dir, root) = workdir()?;
    // Over the length cap: refused before parsing.
    let long = nested(3000);
    assert!(long.len() > 8192);
    let verdict = check(&long, &root);
    assert!(matches!(verdict, Verdict::Ask { .. }), "{verdict:?}");
    // Under the cap, parsed on the large stack. Compound commands always ask,
    // so the verdict is known; the point is that the process survives.
    for depth in [800, 1600] {
        let command = nested(depth);
        assert!(command.len() <= 8192, "fixture too long: {}", command.len());
        let verdict = check(&command, &root);
        assert!(matches!(verdict, Verdict::Ask { .. }), "{verdict:?}");
    }
    Ok(())
}

#[test]
fn reason_lists_programs_in_first_seen_order() -> TestResult {
    let (_dir, root) = workdir()?;
    let pipe = check("ls | grep foo", &root);
    assert_eq!(
        pipe,
        Verdict::Safe {
            reason: "read-only: ls, grep".into()
        }
    );
    let list = check("cat a.txt && echo done; pwd", &root);
    assert_eq!(
        list,
        Verdict::Safe {
            reason: "read-only: cat, echo, pwd".into()
        }
    );
    Ok(())
}

#[test]
fn why_names_the_first_failure() -> TestResult {
    let (_dir, root) = workdir()?;
    let rm = check("rm a.txt", &root);
    assert!(
        matches!(&rm, Verdict::Ask { why } if why.contains("`rm`")),
        "{rm:?}"
    );
    let write = check("cat a.txt > b.txt", &root);
    assert!(
        matches!(&write, Verdict::Ask { why } if why.contains("`b.txt`")),
        "{write:?}"
    );
    Ok(())
}
