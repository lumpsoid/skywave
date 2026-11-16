//! This device's saved prompts ([`Snippet`]): what saving, using and
//! deleting one does. The model owns one [`Snippets`] and writes back the
//! snippet each change returns.

use crate::view::{ComposerMode, Snippet, SnippetId};

/// The snippets as stored, the most recently used or saved first.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snippets(Vec<Snippet>);

impl Snippets {
    pub(crate) fn new(mut stored: Vec<Snippet>) -> Snippets {
        stored.sort_by_key(|a| std::cmp::Reverse(a.used));
        Snippets(stored)
    }

    /// Most recently used or saved first.
    pub(crate) fn list(&self) -> &[Snippet] {
        &self.0
    }

    /// Saves `text` as a snippet of `kind` under `name` (normalised as a
    /// session name is; blank for none), first in the list. Saving a text
    /// saved already as that kind moves that snippet first instead, renamed
    /// if `name` is not blank. Returns the snippet to store; `None` for
    /// blank text, or nothing changed.
    pub(crate) fn save(&mut self, name: &str, text: String, kind: ComposerMode) -> Option<Snippet> {
        if text.trim().is_empty() {
            return None;
        }
        let name = skywave_proto::normalise_session_name(name);
        let used = self.next_used();
        let Some(at) = self.0.iter().position(|s| s.kind == kind && s.text == text) else {
            let snippet = Snippet {
                id: SnippetId::new(),
                name,
                text,
                kind,
                used,
            };
            self.0.insert(0, snippet.clone());
            return Some(snippet);
        };
        let old = self.0.remove(at);
        let snippet = Snippet {
            name: name.or_else(|| old.name.clone()),
            used: if at == 0 { old.used } else { used },
            ..old.clone()
        };
        self.0.insert(0, snippet.clone());
        (snippet != old).then_some(snippet)
    }

    /// Moves the snippet first, as just used. Returns it to store; `None`
    /// when unknown or first already.
    pub(crate) fn used(&mut self, id: SnippetId) -> Option<Snippet> {
        let at = self
            .0
            .iter()
            .position(|s| s.id == id)
            .filter(|at| *at > 0)?;
        let used = self.next_used();
        let mut snippet = self.0.remove(at);
        snippet.used = used;
        self.0.insert(0, snippet.clone());
        Some(snippet)
    }

    /// Gives the snippet `name` (normalised as in [`save`](Snippets::save);
    /// blank drops it) and `text`, where it lists. Another snippet of its
    /// kind with that text already goes: a text is saved once per kind. Returns the snippet to
    /// store and the one gone; `None` for blank text, an unknown snippet,
    /// or nothing changed.
    pub(crate) fn edit(
        &mut self,
        id: SnippetId,
        name: &str,
        text: String,
    ) -> Option<(Snippet, Option<SnippetId>)> {
        if text.trim().is_empty() {
            return None;
        }
        let at = self.0.iter().position(|s| s.id == id)?;
        let edited = Snippet {
            name: skywave_proto::normalise_session_name(name),
            text,
            ..self.0[at].clone()
        };
        if edited == self.0[at] {
            return None;
        }
        let twin = self
            .0
            .iter()
            .find(|s| s.id != id && s.kind == edited.kind && s.text == edited.text)
            .map(|s| s.id);
        self.0[at] = edited.clone();
        if let Some(twin) = twin {
            self.delete(twin);
        }
        Some((edited, twin))
    }

    /// Forgets the snippet; false when unknown.
    pub(crate) fn delete(&mut self, id: SnippetId) -> bool {
        let before = self.0.len();
        self.0.retain(|s| s.id != id);
        self.0.len() != before
    }

    fn next_used(&self) -> u64 {
        self.0.first().map_or(1, |s| s.used + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: ComposerMode = ComposerMode::Agent;

    fn texts(snippets: &Snippets) -> Vec<&str> {
        snippets.list().iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn saved_snippets_list_newest_first() {
        let mut snippets = Snippets::default();
        let first = snippets
            .save("", "run the tests".to_owned(), AGENT)
            .unwrap();
        let second = snippets
            .save("ship", "commit this".to_owned(), AGENT)
            .unwrap();
        assert_eq!(texts(&snippets), ["commit this", "run the tests"]);
        assert_eq!(first.name, None);
        assert_eq!(second.name.as_deref(), Some("ship"));
        assert!(second.used > first.used);
    }

    #[test]
    fn blank_text_is_not_saved_and_names_are_normalised() {
        let mut snippets = Snippets::default();
        assert_eq!(snippets.save("name", " \n\t".to_owned(), AGENT), None);
        let saved = snippets.save("  a\u{7}b  ", "x".to_owned(), AGENT).unwrap();
        assert_eq!(saved.name.as_deref(), Some("ab"));
        let blank = snippets.save("   ", "y".to_owned(), AGENT).unwrap();
        assert_eq!(blank.name, None);
    }

    #[test]
    fn saving_a_saved_text_moves_it_first_and_renames_it() {
        let mut snippets = Snippets::default();
        let first = snippets
            .save("tests", "run the tests".to_owned(), AGENT)
            .unwrap();
        snippets.save("", "commit this".to_owned(), AGENT).unwrap();

        let again = snippets
            .save("", "run the tests".to_owned(), AGENT)
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(
            again.name.as_deref(),
            Some("tests"),
            "a blank name keeps the old"
        );
        assert_eq!(texts(&snippets), ["run the tests", "commit this"]);

        let renamed = snippets
            .save("check", "run the tests".to_owned(), AGENT)
            .unwrap();
        assert_eq!(renamed.name.as_deref(), Some("check"));
        assert_eq!(renamed.used, again.used, "first already");
        assert_eq!(snippets.save("", "run the tests".to_owned(), AGENT), None);
        assert_eq!(snippets.list().len(), 2);
    }

    #[test]
    fn using_a_snippet_moves_it_first() {
        let mut snippets = Snippets::default();
        let old = snippets.save("", "a".to_owned(), AGENT).unwrap();
        let new = snippets.save("", "b".to_owned(), AGENT).unwrap();
        assert_eq!(snippets.used(new.id), None, "first already");
        let used = snippets.used(old.id).unwrap();
        assert!(used.used > new.used);
        assert_eq!(texts(&snippets), ["a", "b"]);
        assert_eq!(snippets.used(SnippetId::new()), None);
    }

    #[test]
    fn editing_changes_name_and_text_in_place() {
        let mut snippets = Snippets::default();
        let old = snippets
            .save("tests", "run the tests".to_owned(), AGENT)
            .unwrap();
        snippets.save("", "commit this".to_owned(), AGENT).unwrap();

        let (edited, gone) = snippets
            .edit(old.id, " ", "run every test".to_owned())
            .unwrap();
        assert_eq!(gone, None);
        assert_eq!((edited.id, edited.used), (old.id, old.used));
        assert_eq!(edited.name, None, "a blank name drops it");
        assert_eq!(texts(&snippets), ["commit this", "run every test"]);

        assert_eq!(snippets.edit(old.id, "", "run every test".to_owned()), None);
        assert_eq!(snippets.edit(old.id, "x", "  ".to_owned()), None);
        assert_eq!(snippets.edit(SnippetId::new(), "x", "y".to_owned()), None);
    }

    #[test]
    fn editing_into_a_saved_text_drops_the_other() {
        let mut snippets = Snippets::default();
        let kept = snippets.save("", "a".to_owned(), AGENT).unwrap();
        let twin = snippets.save("", "b".to_owned(), AGENT).unwrap();
        let (edited, gone) = snippets.edit(kept.id, "named", "b".to_owned()).unwrap();
        assert_eq!(gone, Some(twin.id));
        assert_eq!(edited.id, kept.id);
        assert_eq!(texts(&snippets), ["b"]);
    }

    #[test]
    fn a_text_is_saved_once_per_kind() {
        let mut snippets = Snippets::default();
        let prompt = snippets.save("", "make test".to_owned(), AGENT).unwrap();
        let command = snippets
            .save("", "make test".to_owned(), ComposerMode::Shell)
            .unwrap();
        assert_ne!(prompt.id, command.id, "both kinds keep the text");
        assert_eq!(command.kind, ComposerMode::Shell);
        assert_eq!(snippets.list().len(), 2);

        // Saving again finds the snippet of that kind only.
        let again = snippets.save("p", "make test".to_owned(), AGENT).unwrap();
        assert_eq!(again.id, prompt.id);
        assert_eq!(snippets.list().len(), 2);

        // Editing into a text of the other kind drops nothing.
        let other = snippets.save("", "ls".to_owned(), AGENT).unwrap();
        let (_, gone) = snippets.edit(other.id, "", "make test".to_owned()).unwrap();
        assert_eq!(gone, Some(prompt.id), "the twin of its own kind goes");
        assert!(snippets.list().iter().any(|s| s.id == command.id));
    }

    #[test]
    fn deleting_forgets_a_snippet() {
        let mut snippets = Snippets::default();
        let saved = snippets.save("", "a".to_owned(), AGENT).unwrap();
        assert!(snippets.delete(saved.id));
        assert!(!snippets.delete(saved.id));
        assert!(snippets.list().is_empty());
    }

    #[test]
    fn stored_snippets_load_by_use() {
        let snippet = |text: &str, used| Snippet {
            id: SnippetId::new(),
            name: None,
            text: text.to_owned(),
            kind: AGENT,
            used,
        };
        let snippets = Snippets::new(vec![snippet("old", 1), snippet("new", 5)]);
        assert_eq!(texts(&snippets), ["new", "old"]);
    }
}
