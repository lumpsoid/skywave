//! Agent content made safe to show: the one filter every head runs
//! displayed text through, so neither can be tricked into drawing
//! something other than what the agent sent.

/// One line of `text` made safe to show, by one rule for all content
/// (approvals included):
///
/// - a tab becomes four spaces;
/// - every other control character (Unicode `Cc`: C0, DEL, C1, line
///   breaks too) is dropped, so nothing reaches a terminal as an escape
///   sequence;
/// - a bidirectional formatting character (U+061C, U+200E, U+200F,
///   U+202A–U+202E, U+2066–U+2069) becomes a visible escape such as
///   `<U+202E>`. Left in, it reorders what follows, so an approval could
///   read differently from what will run; dropped silently, a reviewer
///   would not know it was there.
pub fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => match bidi_escape(c) {
                Some(escape) => out.push_str(&escape),
                None => out.push(c),
            },
        }
    }
    out
}

/// The visible escape a bidirectional formatting character `c` shows as
/// (`<U+202E>`), or `None` when `c` is not one.
pub(crate) fn bidi_escape(c: char) -> Option<String> {
    is_bidi_control(c).then(|| format!("<U+{:04X}>", u32::from(c)))
}

/// The first line of `text`, as [`clean`].
pub fn clean_line(text: &str) -> String {
    clean(text.split('\n').next().unwrap_or_default())
}

/// `text` with its line breaks kept (a `\r\n` as one), each line as
/// [`clean`].
pub fn clean_text(text: &str) -> String {
    text.split('\n')
        .map(|line| clean(line.strip_suffix('\r').unwrap_or(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `c` is a bidirectional formatting character: the marks
/// (ALM, LRM, RLM), embeddings and overrides (LRE–RLO, PDF) and isolates
/// (LRI–PDI).
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bidi_control_shows_as_an_escape() {
        let bidi = [
            0x061C, 0x200E, 0x200F, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066, 0x2067, 0x2068,
            0x2069,
        ];
        for code in bidi {
            let c = char::from_u32(code).unwrap();
            assert_eq!(clean(&format!("a{c}b")), format!("a<U+{code:04X}>b"));
        }
    }

    #[test]
    fn a_reordered_command_reads_as_sent() {
        // "rm -rf /" with an override that would draw the tail reversed.
        assert_eq!(
            clean("echo \u{202E}/ fr- mr\u{202C}"),
            "echo <U+202E>/ fr- mr<U+202C>"
        );
    }

    #[test]
    fn neighbouring_format_characters_stay() {
        // Zero-width joiner and non-joiner shape scripts and emoji; they
        // reorder nothing.
        assert_eq!(clean("a\u{200C}b\u{200D}c"), "a\u{200C}b\u{200D}c");
        assert_eq!(clean("مرحبا"), "مرحبا");
    }

    #[test]
    fn control_characters_go_and_tabs_widen() {
        assert_eq!(clean("a\x1b]0;x\x07b\u{9b}c\x7f\r\nd"), "a]0;xbcd");
        assert_eq!(clean("a\tb"), "a    b");
    }

    #[test]
    fn clean_line_keeps_the_first_line() {
        assert_eq!(clean_line("one\u{202E}\ntwo"), "one<U+202E>");
    }

    #[test]
    fn clean_text_keeps_line_breaks() {
        assert_eq!(clean_text("one\r\n\u{2066}two\x1b"), "one\n<U+2066>two");
    }
}
