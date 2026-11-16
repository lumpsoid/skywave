//! The programs a command may run. Each is read-only, so only its arguments
//! can make it unsafe, and the path rule checks those. Adding a program is
//! adding a row to `PROGRAMS`.

/// A program on the allowlist.
pub(crate) struct Program {
    /// The command word, matched exactly: `/bin/cat` is not `cat`.
    pub(crate) name: &'static str,
    /// Options that make the program unsafe, because it never ends or reads
    /// more than it looks at.
    asks: &'static [&'static str],
    /// Whether its arguments name files, so the path rule applies. `echo` and
    /// `printf` read none.
    pub(crate) reads_files: bool,
}

/// `(name, asks, reads_files)`. The asks are options that spawn another
/// program, follow symlinks out of the workdir, or never end: `tail`'s follow
/// modes (`+1f` is one too, hence the `+` letters), `printf -v` (it expands a
/// subscript in its variable name, and `-v PATH` changes PATH), `diff -l`
/// (spawns `pr`). A bundle holding an ask's letter asks too: `-nf` holds `f`.
const PROGRAMS: &[(&str, &[&str], bool)] = &[
    ("ls", &["-L", "--dereference"], true),
    ("cat", &[], true),
    ("head", &[], true),
    ("wc", &["--files0-from"], true),
    ("pwd", &[], true),
    ("basename", &[], true),
    ("dirname", &[], true),
    ("realpath", &[], true),
    ("readlink", &[], true),
    ("stat", &[], true),
    ("true", &[], true),
    ("false", &[], true),
    ("nl", &[], true),
    ("cut", &[], true),
    ("tr", &[], true),
    ("cmp", &[], true),
    ("comm", &[], true),
    ("diff", &["-l", "--paginate", "-r", "--recursive"], true),
    ("grep", &["-R", "--dereference-recursive"], true),
    ("egrep", &["-R", "--dereference-recursive"], true),
    ("fgrep", &["-R", "--dereference-recursive"], true),
    (
        "tail",
        &["-f", "-F", "--follow", "--retry", "+f", "+F"],
        true,
    ),
    ("echo", &[], false),
    ("printf", &["-v"], false),
];

/// The allowlisted program named `name`, if there is one.
pub(crate) fn find(name: &str) -> Option<Program> {
    PROGRAMS
        .iter()
        .find(|(known, _, _)| *known == name)
        .map(|&(name, asks, reads_files)| Program {
            name,
            asks,
            reads_files,
        })
}

impl Program {
    /// Whether the word `word` is one of the asks. A long option (`--...`)
    /// matches an ask by unambiguous prefix, as getopt does (`--fol` is
    /// `--follow`). A short option (`-...`) or a `+` word matches a one-letter
    /// ask anywhere in its letters (`-nf` holds `f`, `+1f` holds `f`). Any
    /// other word asks never.
    pub(crate) fn asks_on(&self, word: &str) -> bool {
        if let Some(long) = word.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            return !name.is_empty()
                && self
                    .asks
                    .iter()
                    .filter_map(|ask| ask.strip_prefix("--"))
                    .any(|full| full.starts_with(name));
        }
        if let Some(body) = word.strip_prefix('+') {
            return self.letter_asks('+', body);
        }
        word.strip_prefix('-')
            .is_some_and(|bundle| self.letter_asks('-', bundle))
    }

    /// Whether `letters` holds the letter of a one-letter ask written with
    /// `prefix` (`-f` for prefix `-`, `+F` for prefix `+`).
    fn letter_asks(&self, prefix: char, letters: &str) -> bool {
        self.asks
            .iter()
            .filter_map(|ask| ask.strip_prefix(prefix))
            .filter(|letter| letter.len() == 1)
            .any(|letter| letters.contains(letter))
    }
}
