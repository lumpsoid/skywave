//! Walks the parsed shell program. Only lists and pipelines of simple commands
//! get through, and every simple command must pass the word, redirection and
//! path rules.

use std::path::Path;
use std::thread;

use brush_parser::ast::{self, AndOr, CompoundListItem, SeparatorOperator};
use brush_parser::word::{self, WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};

use crate::paths;
use crate::programs::{self, Program};
use crate::{Rule, Verdict};

/// Why an expansion is refused: the checker does not evaluate them.
const EXPANSION: &str = "expansions are not checked yet";

/// Longest command the checker reads. The parser recurses once per nesting
/// level, and a deep enough `{ { ...` aborts the process, which no catch can
/// stop; a read-only command never needs this much nesting.
const MAX_COMMAND_BYTES: usize = 8192;

/// Stack for the parse and walk, far above what `MAX_COMMAND_BYTES` can need.
const STACK_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn check(command: &str, workdir: &Path, rules: &[Rule]) -> Verdict {
    if command.len() > MAX_COMMAND_BYTES {
        return Verdict::Ask {
            why: "too long to check".into(),
        };
    }
    // The worker needs owned data, so it gets its own copy of the rules.
    let (command, workdir, rules) = (command.to_owned(), workdir.to_owned(), rules.to_vec());
    let worker = thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || prove(&command, &workdir, &rules));
    let outcome = match worker {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| Err("the checker failed".into())),
        Err(_) => Err("the checker could not start".into()),
    };
    match outcome {
        Ok(proof) => Verdict::Safe {
            reason: proof.reason(),
        },
        Err(why) => Verdict::Ask { why },
    }
}

/// What a passing command was proven by: the table programs and the rules it
/// used, each listed once, in first-seen order.
#[derive(Default)]
struct Proof {
    programs: Vec<&'static str>,
    rules: Vec<String>,
}

impl Proof {
    fn add_program(&mut self, name: &'static str) {
        if !self.programs.contains(&name) {
            self.programs.push(name);
        }
    }

    fn add_rule(&mut self, rule: &Rule) {
        let text = rule.to_string();
        if !self.rules.contains(&text) {
            self.rules.push(text);
        }
    }

    /// `read-only: cat, grep; rules: git commit`. A part is omitted when no
    /// command was proven by that kind.
    fn reason(&self) -> String {
        let mut parts = Vec::new();
        if !self.programs.is_empty() {
            parts.push(format!("read-only: {}", self.programs.join(", ")));
        }
        if !self.rules.is_empty() {
            parts.push(format!("rules: {}", self.rules.join(", ")));
        }
        parts.join("; ")
    }
}

/// The proof of the command, if all of it is proven.
fn prove(command: &str, workdir: &Path, rules: &[Rule]) -> Result<Proof, String> {
    let mut proof = Proof::default();
    prove_into(command, workdir, rules, &mut proof)?;
    Ok(proof)
}

/// Proves the command into `proof`, which may already hold the proof of an
/// enclosing command's substitutions.
fn prove_into(
    command: &str,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<(), String> {
    let program = parse(command)?;
    let items: Vec<&CompoundListItem> = program
        .complete_commands
        .iter()
        .flat_map(|list| &list.0)
        .collect();
    if items.is_empty() {
        return Err("the command is empty".into());
    }
    for CompoundListItem(and_or, separator) in items {
        if matches!(separator, SeparatorOperator::Async) {
            return Err("background jobs (`&`) are not checked".into());
        }
        pipeline(&and_or.first, workdir, rules, proof)?;
        for next in &and_or.additional {
            match next {
                AndOr::And(later) | AndOr::Or(later) => {
                    pipeline(later, workdir, rules, proof)?;
                }
            }
        }
    }
    Ok(())
}

fn parse(command: &str) -> Result<ast::Program, String> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(command.as_bytes(), &options);
    parser
        .parse_program()
        .map_err(|_| "could not parse the command".into())
}

fn pipeline(
    pipeline: &ast::Pipeline,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<(), String> {
    if pipeline.timed.is_some() {
        return Err("`time` is not checked".into());
    }
    pipeline
        .seq
        .iter()
        .try_for_each(|command| command_check(command, workdir, rules, proof))
}

fn command_check(
    command: &ast::Command,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<(), String> {
    match command {
        ast::Command::Simple(simple) => simple_command(simple, workdir, rules, proof),
        ast::Command::Compound(..) => {
            Err("compound commands (`( )`, `{ }`, `if`, loops) are not checked".into())
        }
        ast::Command::Function(_) => Err("function definitions are not checked".into()),
        ast::Command::ExtendedTest(..) => Err("`[[ ]]` tests are not checked".into()),
    }
}

fn simple_command(
    simple: &ast::SimpleCommand,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<(), String> {
    let name_word = simple
        .word_or_name
        .as_ref()
        .ok_or("a command with no command word is not checked")?;
    let name = literal(name_word)?;
    // The table is looked up now, but an unknown program is only reported
    // after the words are read: a rule may name a program the table lacks.
    let table = programs::find(&name);
    let items = simple
        .prefix
        .iter()
        .flat_map(|prefix| &prefix.0)
        .chain(simple.suffix.iter().flat_map(|suffix| &suffix.0));
    let mut args: Vec<Option<String>> = Vec::new();
    // Every item is read, so a rule can see all the words; the first failure
    // is kept.
    let mut failure: Option<String> = None;
    for item in items {
        let outcome = match item {
            ast::CommandPrefixOrSuffixItem::Word(word) => {
                argument(word, workdir, rules, proof).map(|arg| args.push(arg))
            }
            ast::CommandPrefixOrSuffixItem::IoRedirect(io) => redirection(io, workdir),
            ast::CommandPrefixOrSuffixItem::AssignmentWord(..) => {
                Err("assignments are not checked".into())
            }
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                Err("process substitution is not checked".into())
            }
        };
        if let Err(why) = outcome {
            failure.get_or_insert(why);
        }
    }
    let rule = rules.iter().find(|rule| covers(rule, &name, &args));
    if let Some(why) = failure {
        // Without a rule, an unknown program is the error, as it always was.
        return Err(match (table, rule) {
            (None, None) => unknown(&name),
            _ => why,
        });
    }
    if let Some(rule) = rule {
        proof.add_rule(rule);
        return Ok(());
    }
    let program = table.ok_or_else(|| unknown(&name))?;
    // A table program cannot be checked against a value nobody knows.
    let words = args
        .into_iter()
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| {
            format!("`{name}` takes a word from a command substitution, which is not checked")
        })?;
    check_args(&program, &words, workdir)?;
    proof.add_program(program.name);
    Ok(())
}

fn unknown(name: &str) -> String {
    format!("`{name}` is not a known read-only program")
}

/// Whether the rule's words are the first words of the command: the program,
/// then the arguments, word for word. An unknown argument matches no rule
/// word; words after the rule's are not looked at.
fn covers(rule: &Rule, name: &str, args: &[Option<String>]) -> bool {
    let mut command = std::iter::once(Some(name)).chain(args.iter().map(Option::as_deref));
    rule.words()
        .iter()
        .all(|word| command.next().flatten() == Some(word.as_str()))
}

/// The arguments of a program. Options come first (up to `--`): each is
/// checked against the program's asks, then the path rule where it names a
/// file. Everything else is a positional: checked against the asks (`+1f`),
/// then the path rule.
fn check_args(program: &Program, args: &[String], workdir: &Path) -> Result<(), String> {
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with('-') {
            if arg != "-" {
                check_option(program, arg, workdir)?;
            }
        } else {
            check_positional(program, arg, workdir)?;
        }
    }
    Ok(())
}

fn check_positional(program: &Program, arg: &str, workdir: &Path) -> Result<(), String> {
    if program.asks_on(arg) {
        return Err(asked(program, arg));
    }
    if program.reads_files && arg != "-" {
        paths::positional(arg, workdir)?;
    }
    Ok(())
}

fn check_option(program: &Program, option: &str, workdir: &Path) -> Result<(), String> {
    if program.asks_on(option) {
        return Err(asked(program, option));
    }
    if !program.reads_files {
        return Ok(());
    }
    let (name, value) = match option.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (option, None),
    };
    if name.contains('/') {
        return Err(format!("`{option}` is outside the workdir"));
    }
    if let Some(value) = value {
        paths::positional(value, workdir)?;
    }
    attached_paths(option, workdir)
}

fn asked(program: &Program, word: &str) -> String {
    format!("`{word}` of `{}` is not checked", program.name)
}

/// A short option with attached text can name a file: `-flink` may be `-f`
/// with the file `link`. Every suffix of the attached text is checked as a
/// path (`flink`, `link`, `ink`, `nk`, `k`), since the program may read the
/// text from any position.
fn attached_paths(option: &str, workdir: &Path) -> Result<(), String> {
    let Some(body) = option.strip_prefix('-') else {
        return Ok(());
    };
    if body.starts_with('-') || body.len() < 2 {
        return Ok(());
    }
    body.char_indices()
        .try_for_each(|(at, _)| paths::positional(&body[at..], workdir))
}

fn redirection(redirect: &ast::IoRedirect, workdir: &Path) -> Result<(), String> {
    match redirect {
        ast::IoRedirect::File(fd, kind, target) => file_redirection(*fd, kind, target, workdir),
        ast::IoRedirect::HereDocument(_, doc) => here_document(doc),
        ast::IoRedirect::HereString(_, word) => {
            literal(word)?;
            Ok(())
        }
        ast::IoRedirect::OutputAndError(word, _) => null_device(&literal(word)?),
    }
}

fn file_redirection(
    fd: Option<ast::IoFd>,
    kind: &ast::IoFileRedirectKind,
    target: &ast::IoFileRedirectTarget,
    workdir: &Path,
) -> Result<(), String> {
    match target {
        ast::IoFileRedirectTarget::Filename(word) => filename(kind, &literal(word)?, workdir),
        ast::IoFileRedirectTarget::Duplicate(word) => duplicate(fd, kind, &literal(word)?),
        ast::IoFileRedirectTarget::Fd(number) => duplicate(fd, kind, &number.to_string()),
        ast::IoFileRedirectTarget::ProcessSubstitution(..) => {
            Err("process substitution is not checked".into())
        }
    }
}

fn filename(kind: &ast::IoFileRedirectKind, target: &str, workdir: &Path) -> Result<(), String> {
    use ast::IoFileRedirectKind as Kind;
    match kind {
        Kind::Read => paths::positional(target, workdir),
        Kind::Write | Kind::Append | Kind::Clobber => null_device(target),
        _ => Err(format!("writes to `{target}`")),
    }
}

/// Output may go only to `/dev/null`: nothing is written anyone can read.
fn null_device(target: &str) -> Result<(), String> {
    if target == "/dev/null" {
        Ok(())
    } else {
        Err(format!("writes to `{target}`"))
    }
}

/// `>&2`, `2>&1` and `>&-` name descriptors, not files; `>&file` writes one.
/// Only the standard descriptors 0, 1 and 2 are allowed on either side: any
/// other one is inherited from the server, and writing to it writes to
/// something the checker cannot see (`>&3`, `3>&1`).
fn duplicate(
    fd: Option<ast::IoFd>,
    kind: &ast::IoFileRedirectKind,
    target: &str,
) -> Result<(), String> {
    let default = match kind {
        ast::IoFileRedirectKind::DuplicateInput => 0,
        _ => 1,
    };
    let source = fd.unwrap_or(default);
    let target_ok = target == "-"
        || (!target.is_empty()
            && target.bytes().all(|b| b.is_ascii_digit())
            && target.parse::<ast::IoFd>().is_ok_and(is_standard));
    if is_standard(source) && target_ok {
        Ok(())
    } else {
        Err(format!("redirects fd {source} to `{target}`"))
    }
}

fn is_standard(fd: ast::IoFd) -> bool {
    (0..=2).contains(&fd)
}

/// A quoted delimiter means no expansion; an unquoted one expands `$` and
/// backquotes in the body, so those are refused.
fn here_document(doc: &ast::IoHereDocument) -> Result<(), String> {
    if doc.requires_expansion && doc.doc.value.contains(['$', '`']) {
        Err(EXPANSION.into())
    } else {
        Ok(())
    }
}

/// The pieces of a word as the parser reads them.
fn pieces(word: &ast::Word) -> Result<Vec<WordPieceWithSource>, String> {
    // A backslash-newline is removed by the shell, so the parser's view of
    // the word would differ from the shell's; refuse rather than guess.
    if word.value.contains("\\\n") {
        return Err("line continuations are not checked yet".into());
    }
    word::parse(&word.value, &ParserOptions::default())
        .map_err(|_| format!("could not read `{}`", word.value))
}

/// The literal text of a word after unquoting, or why it is not a literal.
fn literal(word: &ast::Word) -> Result<String, String> {
    let mut text = String::new();
    for piece in &pieces(word)? {
        push_piece(&piece.piece, &mut text)?;
    }
    Ok(text)
}

/// An argument word: its literal text, or `None` when it holds a command
/// substitution, whose value nobody knows. The substitution's inner command
/// is proven into `proof`, so it appears in the reason.
fn argument(
    word: &ast::Word,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<Option<String>, String> {
    let mut text = String::new();
    let mut known = true;
    for piece in &pieces(word)? {
        known &= argument_piece(&piece.piece, false, &mut text, workdir, rules, proof)?;
    }
    Ok(known.then_some(text))
}

/// Appends one piece of an argument word, and returns `false` when the piece
/// is a command substitution (its value is unknown). Inside double quotes
/// (`quoted`), a substitution is checked the same way.
fn argument_piece(
    piece: &WordPiece,
    quoted: bool,
    text: &mut String,
    workdir: &Path,
    rules: &[Rule],
    proof: &mut Proof,
) -> Result<bool, String> {
    match piece {
        WordPiece::CommandSubstitution(inner) | WordPiece::BackquotedCommandSubstitution(inner) => {
            prove_into(inner, workdir, rules, proof)?;
            Ok(false)
        }
        WordPiece::DoubleQuotedSequence(inner) if !quoted => {
            let mut known = true;
            for inner_piece in inner {
                let part = &inner_piece.piece;
                known &= argument_piece(part, true, text, workdir, rules, proof)?;
            }
            Ok(known)
        }
        _ if quoted => push_quoted(piece, text).map(|()| true),
        _ => push_piece(piece, text).map(|()| true),
    }
}

/// Appends one piece of an unquoted word. Literal text, quoted text and
/// escapes are literal; anything else is an expansion.
fn push_piece(piece: &WordPiece, text: &mut String) -> Result<(), String> {
    match piece {
        WordPiece::Text(s) => {
            unquoted_pattern(s)?;
            text.push_str(s);
            Ok(())
        }
        WordPiece::SingleQuotedText(s) => {
            text.push_str(s);
            Ok(())
        }
        WordPiece::EscapeSequence(s) => {
            push_escape(s, text);
            Ok(())
        }
        WordPiece::DoubleQuotedSequence(pieces) => pieces
            .iter()
            .try_for_each(|inner| push_quoted(&inner.piece, text)),
        _ => Err(EXPANSION.into()),
    }
}

/// Appends one piece inside double quotes, where globs are not expanded.
fn push_quoted(piece: &WordPiece, text: &mut String) -> Result<(), String> {
    match piece {
        WordPiece::Text(s) => {
            text.push_str(s);
            Ok(())
        }
        WordPiece::EscapeSequence(s) => {
            push_escape(s, text);
            Ok(())
        }
        _ => Err(EXPANSION.into()),
    }
}

/// Unquoted text the shell would expand: globs, braces and extglobs (the
/// parser enables extglob, so `@(...)` reaches here as text).
fn unquoted_pattern(text: &str) -> Result<(), String> {
    if text.contains(['*', '?', '[']) {
        return Err("globs are not checked yet".into());
    }
    if text.contains(['{', '}', '(', ')']) {
        return Err("braces and extglobs are not checked yet".into());
    }
    Ok(())
}

/// `\x` is the literal `x`.
fn push_escape(escape: &str, text: &mut String) {
    text.push_str(escape.strip_prefix('\\').unwrap_or(escape));
}
