//! The session view's multi-line message editor: a text buffer with a
//! cursor, and its layout into rows of a given width. Pure text editing; what
//! a message may contain is the core's call.

/// Spaces a tab is expanded to; a terminal cell grid has no tab stops here.
const TAB: &str = "    ";

/// A draft message and the cursor in it (a byte offset on a char boundary).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
    /// Enter was pressed; the draft is cleared once the core takes the
    /// message and kept if dispatching fails.
    pub(crate) submitted: bool,
    /// Edited since the head last told its
    /// [`DraftSaver`](skywave_present::policy::DraftSaver), which keeps
    /// when.
    pub(crate) edited: bool,
}

/// One screen row of the composer: a slice of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

impl Composer {
    /// A composer holding a saved draft, the cursor `cursor_chars` chars in
    /// (clamped to the text).
    pub fn from_draft(text: String, cursor_chars: usize) -> Composer {
        let cursor = text
            .char_indices()
            .nth(cursor_chars)
            .map_or(text.len(), |(i, _)| i);
        Composer {
            text,
            cursor,
            submitted: false,
            edited: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor counted in chars, as the core keeps it.
    pub fn cursor_chars(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    /// Marks an edit (the cursor counts: it is saved too).
    fn touch(&mut self) {
        self.edited = true;
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Inserts typed or pasted text at the cursor. Line breaks of any
    /// convention become `\n`, tabs become spaces, other control
    /// characters are dropped.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut clean = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                '\n' => clean.push('\n'),
                '\t' => clean.push_str(TAB),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.touch();
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Replaces the whole text, the cursor at its end; an edit like any.
    pub fn replace(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
        self.touch();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.touch();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
            self.touch();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
            self.touch();
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.touch();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
            self.touch();
        }
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
        self.touch();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
        self.touch();
    }

    /// Moves to the same column of the previous line (`up`) or the next;
    /// `false` if the cursor is already on the first (last) line. Lines are
    /// logical: a wrapped line is one line.
    pub fn vertical(&mut self, up: bool) -> bool {
        let start = self.line_start(self.cursor);
        let column = self.text[start..self.cursor].chars().count();
        let target = if up {
            if start == 0 {
                return false;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end == self.text.len() {
                return false;
            }
            end + 1
        };
        let target_end = self.line_end(target);
        self.cursor = self.text[target..target_end]
            .char_indices()
            .nth(column)
            .map_or(target_end, |(i, _)| target + i);
        self.touch();
        true
    }

    /// Splits the text into rows of at most `width` characters: one or more
    /// per line, an empty line being one empty row. A line that fills its
    /// last row exactly gets an empty row after it, where the cursor goes.
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut start = line_start;
            let mut count = 0;
            for (i, _) in line.char_indices() {
                if count == width {
                    rows.push(Row {
                        start,
                        end: line_start + i,
                    });
                    start = line_start + i;
                    count = 0;
                }
                count += 1;
            }
            let end = line_start + line.len();
            rows.push(Row { start, end });
            if count == width {
                rows.push(Row { start: end, end });
            }
            line_start = end + 1;
        }
        rows
    }

    /// The cursor's (row, column) in [`Composer::rows`] for `width`.
    pub fn cursor_position(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        for (index, row) in rows.iter().enumerate() {
            let continued = rows
                .get(index + 1)
                .is_some_and(|next| next.start == row.end);
            let inside = self.cursor >= row.start
                && (self.cursor < row.end || (self.cursor == row.end && !continued));
            if inside {
                return (index, self.text[row.start..self.cursor].chars().count());
            }
        }
        (rows.len().saturating_sub(1), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        let mut c = Composer::default();
        c.insert(text);
        c
    }

    #[test]
    fn insert_normalises_line_breaks_and_drops_controls() {
        let c = composer("a\r\nb\rc\td\x07é");
        assert_eq!(c.text(), "a\nb\nc    dé");
        assert_eq!(c.cursor(), c.text().len());
    }

    #[test]
    fn editing_at_the_cursor() {
        let mut c = composer("héllo");
        c.left();
        c.left();
        c.insert("X");
        assert_eq!(c.text(), "hélXlo");
        c.backspace();
        c.backspace();
        assert_eq!(c.text(), "hélo");
        let mut c = composer("abc");
        c.home();
        c.delete();
        assert_eq!(c.text(), "bc");
        c.end();
        c.backspace();
        assert_eq!(c.text(), "b");
        c.clear();
        assert!(c.is_blank());
    }

    #[test]
    fn vertical_moves_keep_the_column() {
        let mut c = composer("abcd\nx\nwxyz");
        c.vertical(true);
        assert_eq!(c.cursor(), 6, "end of the short line");
        c.vertical(true);
        assert_eq!(c.cursor(), 1);
        c.vertical(true);
        assert_eq!(c.cursor(), 1, "first line stays");
        c.vertical(false);
        c.vertical(false);
        assert_eq!(c.cursor(), 8);
    }

    #[test]
    fn vertical_reports_when_it_hits_an_edge() {
        let mut c = composer("ab\ncd");
        assert!(!c.vertical(false), "already on the last line");
        assert!(c.vertical(true));
        assert!(!c.vertical(true), "already on the first line");
        assert_eq!(c.cursor(), 2);
        let mut c = composer("a long line that wraps");
        c.edited = false;
        assert!(!c.vertical(true), "a wrapped line is one line");
        assert!(!c.vertical(false));
        assert!(!c.edited, "nothing moved");
    }

    #[test]
    fn replace_puts_the_cursor_at_the_end() {
        let mut c = Composer::from_draft("old".into(), 0);
        c.replace("néw");
        assert_eq!((c.text(), c.cursor()), ("néw", "néw".len()));
        assert!(c.edited);
    }

    #[test]
    fn from_draft_clamps_cursor_to_chars() {
        let c = Composer::from_draft("héllo wörld".into(), 8);
        assert_eq!(c.cursor(), "héllo wö".len());
        assert_eq!(c.cursor_chars(), 8);
        assert!(!c.edited, "the core has it already");
        let c = Composer::from_draft("ñ".into(), 5);
        assert_eq!((c.cursor(), c.cursor_chars()), (2, 1));
        let mut c = Composer::from_draft(String::new(), 3);
        assert_eq!(c.cursor(), 0);
        c.left();
        assert!(!c.edited, "nothing moved");
        c.insert("x");
        assert!(c.edited);
    }

    #[test]
    fn rows_wrap_and_place_the_cursor() {
        let c = composer("abcdef\n\ngh");
        let rows: Vec<&str> = c
            .rows(4)
            .iter()
            .map(|r| &c.text()[r.start..r.end])
            .collect();
        assert_eq!(rows, ["abcd", "ef", "", "gh"]);
        assert_eq!(c.cursor_position(4), (3, 2));
        let c = composer("abcd");
        assert_eq!(c.cursor_position(4), (1, 0), "after a full row");
        let mut c = composer("abcdef");
        for _ in 0..2 {
            c.left();
        }
        assert_eq!(c.cursor_position(4), (1, 0));
        c.left();
        assert_eq!(c.cursor_position(4), (0, 3));
        assert_eq!(Composer::default().cursor_position(4), (0, 0));
    }
}
