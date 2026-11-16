//! Table tests for the read-only check. Each `ASK` row is an absence check: the
//! checker must not approve it. Each `SAFE` row is the positive control: the
//! checker must approve it, so a checker that always asks cannot pass.

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use skywave_safety::{Rule, Verdict, check, check_with, simple_commands};
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

/// One rule per string, its words split on spaces.
fn rules_from(lines: &[&str]) -> Result<Vec<Rule>, Box<dyn Error>> {
    lines
        .iter()
        .map(|line| -> Result<Rule, Box<dyn Error>> {
            Ok(Rule::new(line.split(' ').map(String::from).collect())?)
        })
        .collect()
}

/// The verdict for a program that is on neither the table nor a rule.
fn unknown(name: &str) -> Verdict {
    Verdict::Ask {
        why: format!("`{name}` is not a known read-only program"),
    }
}

#[test]
fn rule_approves_a_matching_command() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git commit"])?;
    let verdict = check_with("git commit -qam \"msg\"", &root, &rules);
    assert_eq!(
        verdict,
        Verdict::Safe {
            reason: "rules: git commit".into()
        }
    );
    Ok(())
}

#[test]
fn rule_does_not_match_other_subcommands() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git commit"])?;
    for command in ["git push", "git commitx", "git"] {
        assert_eq!(
            check_with(command, &root, &rules),
            unknown("git"),
            "{command:?}"
        );
    }
    Ok(())
}

#[test]
fn rule_longer_than_command_does_not_match() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["python3 -m unittest"])?;
    assert_eq!(check_with("python3 -m", &root, &rules), unknown("python3"));
    // Positive control: the full words are approved.
    let full = check_with("python3 -m unittest", &root, &rules);
    assert_eq!(
        full,
        Verdict::Safe {
            reason: "rules: python3 -m unittest".into()
        }
    );
    Ok(())
}

#[test]
fn every_command_in_a_line_must_pass() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git commit"])?;
    let verdict = check_with("git commit -m x && rm a.txt", &root, &rules);
    assert_eq!(verdict, unknown("rm"));
    // Positive control: a table program alongside the rule passes.
    let both = check_with("git commit -m x && cat a.txt", &root, &rules);
    assert_eq!(
        both,
        Verdict::Safe {
            reason: "read-only: cat; rules: git commit".into()
        }
    );
    Ok(())
}

#[test]
fn reason_lists_programs_then_rules() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git status", "ruff check"])?;
    let verdict = check_with(
        "echo hi && git status --short; ruff check src; git status",
        &root,
        &rules,
    );
    assert_eq!(
        verdict,
        Verdict::Safe {
            reason: "read-only: echo; rules: git status, ruff check".into()
        }
    );
    Ok(())
}

#[test]
fn rule_overrides_the_table_asks() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["tail -f"])?;
    let verdict = check_with("tail -f log.txt", &root, &rules);
    assert_eq!(
        verdict,
        Verdict::Safe {
            reason: "rules: tail -f".into()
        }
    );
    // Without the rule, the table still asks on `-f`.
    assert_eq!(
        check("tail -f log.txt", &root),
        Verdict::Ask {
            why: "`-f` of `tail` is not checked".into()
        }
    );
    Ok(())
}

#[test]
fn rule_still_checks_redirections() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git status"])?;
    assert_eq!(
        check_with("git status > out.txt", &root, &rules),
        Verdict::Ask {
            why: "writes to `out.txt`".into()
        }
    );
    // Positive control: without the redirection, the rule approves it.
    assert_eq!(
        check_with("git status", &root, &rules),
        Verdict::Safe {
            reason: "rules: git status".into()
        }
    );
    Ok(())
}

#[test]
fn rule_trusts_paths_outside_the_workdir() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git -C"])?;
    assert_eq!(
        check_with("git -C /etc status", &root, &rules),
        Verdict::Safe {
            reason: "rules: git -C".into()
        }
    );
    // Without the rule, `git` is not on the table at all.
    assert_eq!(check("git -C /etc status", &root), unknown("git"));
    Ok(())
}

#[test]
fn rule_new_rejects_empty() -> TestResult {
    assert_eq!(Rule::new(vec![]), Err("a rule needs a program".into()));
    assert_eq!(
        Rule::new(vec!["git".into(), String::new()]),
        Err("a rule word is empty".into())
    );
    Ok(())
}

#[test]
fn substitution_of_a_proven_command_passes_under_a_rule() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git -C"])?;
    assert_eq!(
        check_with("git -C \"$(pwd)\" status", &root, &rules),
        Verdict::Safe {
            reason: "read-only: pwd; rules: git -C".into()
        }
    );
    Ok(())
}

#[test]
fn backquote_substitutions_ask() -> TestResult {
    let (_dir, root) = workdir()?;
    let why = "expansions are not checked yet";
    let git_c = rules_from(&["git -C"])?;
    let git_log = rules_from(&["git log"])?;
    assert_eq!(
        check_with("git -C `pwd` status", &root, &git_c),
        Verdict::Ask { why: why.into() }
    );
    // bash un-escapes `\\` and `\$` inside backquotes before it parses them;
    // the parser does not, so the proven text would differ from what runs.
    for command in [
        "git log `echo \"\\$(touch /tmp/pwned)\"`",
        "git log `echo \\\\'$(touch p) \\\\'`",
        "git log `cat \\$HOME/.ssh/id_rsa`",
        "git log \"`pwd`\"",
    ] {
        assert_eq!(
            check_with(command, &root, &git_log),
            Verdict::Ask { why: why.into() },
            "{command}"
        );
    }
    // Positive control: the `$(...)` form of the same substitution is proven.
    assert_eq!(
        check_with("git log \"$(pwd)\"", &root, &git_log),
        Verdict::Safe {
            reason: "read-only: pwd; rules: git log".into()
        }
    );
    Ok(())
}

#[test]
fn substitution_of_an_unknown_command_asks() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git -C"])?;
    assert_eq!(
        check_with("git -C \"$(rm x)\" status", &root, &rules),
        unknown("rm")
    );
    Ok(())
}

#[test]
fn table_program_asks_on_a_substituted_word() -> TestResult {
    let (_dir, root) = workdir()?;
    let why = "`cat` takes a word from a command substitution, which is not checked";
    assert_eq!(
        check("cat \"$(pwd)\"", &root),
        Verdict::Ask { why: why.into() }
    );
    // A word that mixes literal text with a substitution is unknown too.
    assert_eq!(
        check("cat \"a$(pwd)\"", &root),
        Verdict::Ask { why: why.into() }
    );
    Ok(())
}

#[test]
fn rule_words_must_be_known() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git commit"])?;
    assert_eq!(
        check_with("git \"$(echo commit)\"", &root, &rules),
        unknown("git")
    );
    // Positive control: the literal word is approved.
    assert_eq!(
        check_with("git commit", &root, &rules),
        Verdict::Safe {
            reason: "rules: git commit".into()
        }
    );
    Ok(())
}

#[test]
fn substitution_in_the_command_name_still_asks() -> TestResult {
    let (_dir, root) = workdir()?;
    assert_eq!(
        check("$(echo ls) a.txt", &root),
        Verdict::Ask {
            why: "expansions are not checked yet".into()
        }
    );
    Ok(())
}

#[test]
fn variables_still_ask() -> TestResult {
    let (_dir, root) = workdir()?;
    let rules = rules_from(&["git -C"])?;
    assert_eq!(
        check_with("git -C \"$HOME\" status", &root, &rules),
        Verdict::Ask {
            why: "expansions are not checked yet".into()
        }
    );
    Ok(())
}

#[test]
fn nested_substitution_is_proven() -> TestResult {
    let (_dir, root) = workdir()?;
    // `echo` takes an unknown word here, so a rule must cover the inner echo.
    let rules = rules_from(&["git -C", "echo"])?;
    assert_eq!(
        check_with("git -C \"$(echo $(pwd))\" status", &root, &rules),
        Verdict::Safe {
            reason: "read-only: pwd; rules: echo, git -C".into()
        }
    );
    // Negative control: without the echo rule, the inner echo asks.
    let rules = rules_from(&["git -C"])?;
    assert_eq!(
        check_with("git -C \"$(echo $(pwd))\" status", &root, &rules),
        Verdict::Ask {
            why: "`echo` takes a word from a command substitution, which is not checked".into()
        }
    );
    Ok(())
}

#[test]
fn simple_commands_lists_literal_words() {
    let rows: &[(&str, &[&[&str]])] = &[
        ("git status", &[&["git", "status"]]),
        (
            "cargo test -p x 2>&1 | tail -n 20",
            &[&["cargo", "test", "-p", "x"], &["tail", "-n", "20"]],
        ),
        (
            "cd a && git log --oneline; ls",
            &[&["cd", "a"], &["git", "log", "--oneline"], &["ls"]],
        ),
        (
            "git commit -m \"$(cat msg)\" --amend",
            &[&["git", "commit", "-m"], &["cat", "msg"]],
        ),
        ("FOO=1 make build > out.txt", &[&["make", "build"]]),
        ("echo 'a b' $HOME c", &[&["echo", "a b"]]),
        ("$EDITOR file", &[]),
        ("echo `id`", &[&["echo"]]),
        ("(cd x && ls)", &[]),
        ("echo 'unterminated", &[]),
        ("", &[]),
        // A background job still lists its words.
        ("cat a.txt &", &[&["cat", "a.txt"]]),
        // A process substitution cuts the list.
        ("cat <(ls a)", &[&["cat"]]),
        // A substitution in the program word is listed, with no entry for it.
        ("$(echo ls) a.txt", &[&["echo", "ls"]]),
        // Nested: the outer command's entry comes before the inner ones.
        ("echo $(echo $(pwd))", &[&["echo"], &["echo"], &["pwd"]]),
    ];
    for (command, expected) in rows {
        let want: Vec<Vec<String>> = expected
            .iter()
            .map(|words| words.iter().map(|word| word.to_string()).collect())
            .collect();
        assert_eq!(simple_commands(command), want, "{command:?}");
    }
}
