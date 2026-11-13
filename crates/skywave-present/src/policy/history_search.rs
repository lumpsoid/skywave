//! An fzf-like filter of shell history: a query matches a command when its
//! chars occur in the command in order, not necessarily adjacent. Smart
//! case, no scoring: matches keep the order they came in.

/// One command that matches a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMatch<'a> {
    pub command: &'a str,
    /// The char indices (not bytes) of the command's chars the query matched,
    /// ascending, leftmost first.
    pub matched: Vec<usize>,
}

/// The commands `query` matches as a subsequence, in their given order, at most `limit`.
pub fn search_history<'a>(
    commands: &'a [String],
    query: &str,
    limit: usize,
) -> Vec<HistoryMatch<'a>> {
    let fold = !query.chars().any(char::is_uppercase);
    let query: Vec<char> = query
        .chars()
        .map(|c| if fold { first_lower(c) } else { c })
        .collect();
    commands
        .iter()
        .filter_map(|command| {
            let matched = subsequence(command, &query, fold)?;
            Some(HistoryMatch {
                command: command.as_str(),
                matched,
            })
        })
        .take(limit)
        .collect()
}

/// The char indices of `command` that spell `query` in order, greedily
/// leftmost, or `None` if it does not. With `fold`, `query` is already
/// lower-cased and each command char is compared lower-cased.
fn subsequence(command: &str, query: &[char], fold: bool) -> Option<Vec<usize>> {
    let mut matched = Vec::with_capacity(query.len());
    let mut wanted = query.iter();
    let mut next = wanted.next();
    for (index, c) in command.chars().enumerate() {
        let Some(&want) = next else { break };
        let c = if fold { first_lower(c) } else { c };
        if c == want {
            matched.push(index);
            next = wanted.next();
        }
    }
    next.is_none().then_some(matched)
}

/// The first char of `c`'s lower-case. A char that lower-cases to several
/// chars compares by its first, so match indices stay 1:1 with the chars.
fn first_lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(list: &[&str]) -> Vec<String> {
        list.iter().map(|&c| c.to_owned()).collect()
    }

    fn found<'a>(matches: &[HistoryMatch<'a>]) -> Vec<&'a str> {
        matches.iter().map(|m| m.command).collect()
    }

    #[test]
    fn a_subsequence_matches() {
        let list = commands(&["git status", "ls -la", "git stash"]);
        let matches = search_history(&list, "gst", 10);
        assert_eq!(found(&matches), ["git status", "git stash"]);
        assert_eq!(matches[0].matched, vec![0, 4, 5]);
    }

    #[test]
    fn order_is_kept_and_limited() {
        let list = commands(&["git a", "ls", "git b", "git c"]);
        let matches = search_history(&list, "git", 2);
        assert_eq!(found(&matches), ["git a", "git b"]);
    }

    #[test]
    fn smart_case() {
        let list = commands(&["Make", "make"]);
        assert_eq!(found(&search_history(&list, "ma", 10)), ["Make", "make"]);
        assert_eq!(found(&search_history(&list, "Ma", 10)), ["Make"]);
    }

    #[test]
    fn empty_query_matches_all() {
        let list = commands(&["ls", "pwd"]);
        let matches = search_history(&list, "", 10);
        assert_eq!(found(&matches), ["ls", "pwd"]);
        assert!(matches.iter().all(|m| m.matched.is_empty()));
    }

    #[test]
    fn no_match_is_empty() {
        let list = commands(&["ls", "pwd"]);
        assert!(search_history(&list, "zz", 10).is_empty());
    }

    #[test]
    fn indices_are_chars_not_bytes() {
        let list = commands(&["é ls"]);
        let matches = search_history(&list, "l", 10);
        assert_eq!(matches[0].matched, vec![2]);
    }
}
