//! An agent message as a head draws it: flat blocks of styled spans,
//! parsed as CommonMark with GitHub's tables, strikethrough and task lists.
//!
//! Flat, not a tree: a list item's or a quote's content is blocks like any
//! other, carrying how deep it sits and the item's marker. A head lays
//! them out in one column.
//!
//! Every piece of text is [`clean`]ed after parsing, since an entity such
//! as `&#x202E;` decodes to the character cleaning escapes. Nothing is
//! fetched: an image is its alt text and a link to it. Raw HTML shows as
//! the text it is.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::text::{clean, clean_text};

/// One block of a message, in reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// How many lists it sits in: 0 outside any.
    pub depth: u8,
    /// How many quotes it sits in.
    pub quote: u8,
    /// The marker of the list item it starts: `•`, `3.`, `☐` or `☑`.
    pub marker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph(Vec<Span>),
    /// `level` 1 to 6.
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    /// Lines joined by `\n`, without the last line break. `lang` is the
    /// first word of a fence's info string, when it has one.
    Code {
        lang: Option<String>,
        text: String,
    },
    Rule,
    /// Every row has one cell per column of `align` or fewer.
    Table {
        align: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A run of text in one style. Neighbours never share both style and link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// May hold `\n`: a hard line break.
    pub text: String,
    pub style: Style,
    /// Where it links to, when that is a link a head may open ([`openable`]).
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    /// Inline code.
    pub code: bool,
    pub strike: bool,
}

/// `text` as blocks. Never fails: whatever does not parse as markdown is
/// a paragraph's text. A message cut short (a long one's head) ends any
/// block left open where it stops, a code fence too.
pub fn blocks(text: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    builder.finish_inline();
    builder.blocks
}

/// Whether a head may open `url` from a message: a web or mail link, with
/// nothing in it that cleaning would change. Anything else (`file:`,
/// `javascript:`, a relative path) stays plain text.
pub fn openable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len())
        && clean(url) == url
        && !url.contains(char::is_whitespace)
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    bold: u32,
    italic: u32,
    strike: u32,
    /// Inside a link: where it goes, if a head may open it.
    link: Option<Option<String>>,
    /// Inside an image: its alt text so far and where it is.
    image: Option<(String, Option<String>)>,
    /// The lists it is in, innermost last: the next item's number for an
    /// ordered one.
    lists: Vec<Option<u64>>,
    quote: u8,
    /// The marker of the item whose first block has not ended yet.
    marker: Option<String>,
    /// The paragraph or heading being filled: its level, if a heading.
    inline: Option<(Option<u8>, Vec<Span>)>,
    code: Option<(Option<String>, String)>,
    table: Option<Table>,
}

struct Table {
    align: Vec<Align>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    /// The cell being filled.
    cell: Option<Vec<Span>>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                let style = Style {
                    code: true,
                    ..self.style()
                };
                self.span(clean(&text), style);
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                if self.inline.is_none() && self.table.is_none() {
                    self.inline = Some((None, Vec::new()));
                }
                self.span(clean_text(&text), self.style());
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.span("\n".to_owned(), self.style()),
            Event::Rule => {
                self.finish_inline();
                self.push(BlockKind::Rule);
            }
            Event::TaskListMarker(checked) => {
                self.marker = Some(if checked { "☑" } else { "☐" }.to_owned());
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => {
                self.finish_inline();
                self.inline = Some((None, Vec::new()));
            }
            Tag::Heading { level, .. } => {
                self.finish_inline();
                self.inline = Some((Some(heading_level(level)), Vec::new()));
            }
            Tag::BlockQuote(_) => {
                self.finish_inline();
                self.quote = self.quote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.finish_inline();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .map(clean)
                        .filter(|lang| !lang.is_empty()),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.finish_inline();
                self.lists.push(start);
            }
            Tag::Item => {
                self.finish_inline();
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}.");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "•".to_owned(),
                });
            }
            Tag::Table(align) => {
                self.finish_inline();
                self.table = Some(Table {
                    align: align
                        .into_iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::Left | Alignment::None => Align::Left,
                        })
                        .collect(),
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: None,
                });
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell = Some(Vec::new());
                }
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some(openable(&dest_url).then(|| dest_url.to_string()));
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some((
                    String::new(),
                    openable(&dest_url).then(|| dest_url.to_string()),
                ));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::HtmlBlock => self.finish_inline(),
            TagEnd::Item => {
                self.finish_inline();
                // An empty item still shows its marker.
                if self.marker.is_some() {
                    self.push(BlockKind::Paragraph(Vec::new()));
                }
            }
            TagEnd::BlockQuote(_) => {
                self.finish_inline();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let text = text.strip_suffix('\n').unwrap_or(&text);
                    self.push(BlockKind::Code {
                        lang,
                        text: clean_text(text),
                    });
                }
            }
            TagEnd::List(_) => {
                self.finish_inline();
                self.lists.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.cell.take()
                {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push(BlockKind::Table {
                        align: table.align,
                        head: table.head,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = None,
            TagEnd::Image => {
                if let Some((alt, url)) = self.image.take() {
                    let text = if alt.is_empty() {
                        "[image]".to_owned()
                    } else {
                        format!("[image: {alt}]")
                    };
                    let style = Style {
                        italic: true,
                        ..self.style()
                    };
                    self.push_span(Span {
                        text,
                        style,
                        link: url,
                    });
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else if let Some((alt, _)) = &mut self.image {
            alt.push_str(&clean(text));
        } else {
            self.span(clean(text), self.style());
        }
    }

    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            code: false,
            strike: self.strike > 0,
        }
    }

    fn span(&mut self, text: String, style: Style) {
        let link = self.link.clone().flatten();
        self.push_span(Span { text, style, link });
    }

    /// Adds `span` to the cell or paragraph being filled, opening a
    /// paragraph if none is (a tight list item's text comes bare).
    fn push_span(&mut self, span: Span) {
        if span.text.is_empty() {
            return;
        }
        let spans = match &mut self.table {
            Some(Table {
                cell: Some(cell), ..
            }) => cell,
            Some(_) => return,
            None => &mut self.inline.get_or_insert_with(|| (None, Vec::new())).1,
        };
        match spans.last_mut() {
            Some(last) if last.style == span.style && last.link == span.link => {
                last.text.push_str(&span.text);
            }
            _ => spans.push(span),
        }
    }

    /// Ends the paragraph or heading being filled, if any.
    fn finish_inline(&mut self) {
        let Some((level, mut spans)) = self.inline.take() else {
            return;
        };
        if let Some(last) = spans.last_mut() {
            let kept = last.text.trim_end_matches('\n').len();
            last.text.truncate(kept);
        }
        spans.retain(|span| !span.text.is_empty());
        if spans.is_empty() && self.marker.is_none() {
            return;
        }
        self.push(match level {
            Some(level) => BlockKind::Heading { level, spans },
            None => BlockKind::Paragraph(spans),
        });
    }

    /// Adds a block where the parser stands; the first in an item takes
    /// its marker.
    fn push(&mut self, kind: BlockKind) {
        self.blocks.push(Block {
            kind,
            depth: u8::try_from(self.lists.len()).unwrap_or(u8::MAX),
            quote: self.quote,
            marker: self.marker.take(),
        });
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Span {
        Span {
            text: text.to_owned(),
            style: Style::default(),
            link: None,
        }
    }

    fn styled(text: &str, style: Style) -> Span {
        Span {
            style,
            ..plain(text)
        }
    }

    fn block(kind: BlockKind) -> Block {
        Block {
            kind,
            depth: 0,
            quote: 0,
            marker: None,
        }
    }

    fn item(depth: u8, marker: &str, text: &str) -> Block {
        Block {
            depth,
            marker: Some(marker.to_owned()),
            ..block(BlockKind::Paragraph(vec![plain(text)]))
        }
    }

    const BOLD: Style = Style {
        bold: true,
        italic: false,
        code: false,
        strike: false,
    };

    #[test]
    fn plain_text_is_one_paragraph_with_soft_breaks_as_spaces() {
        assert_eq!(
            blocks("one\ntwo"),
            [block(BlockKind::Paragraph(vec![plain("one two")]))]
        );
    }

    #[test]
    fn headings_and_inline_styles() {
        let italic_bold = Style {
            italic: true,
            ..BOLD
        };
        let code = Style {
            code: true,
            ..Style::default()
        };
        assert_eq!(
            blocks("## Plan\n\nrun **`cargo` *now*** please"),
            [
                block(BlockKind::Heading {
                    level: 2,
                    spans: vec![plain("Plan")]
                }),
                block(BlockKind::Paragraph(vec![
                    plain("run "),
                    styled("cargo", Style { bold: true, ..code }),
                    styled(" ", BOLD),
                    styled("now", italic_bold),
                    plain(" please"),
                ])),
            ]
        );
    }

    #[test]
    fn hard_break_keeps_a_line_break() {
        assert_eq!(
            blocks("one  \ntwo"),
            [block(BlockKind::Paragraph(vec![plain("one\ntwo")]))]
        );
    }

    #[test]
    fn nested_and_ordered_lists_carry_depth_and_markers() {
        assert_eq!(
            blocks("- a\n  - b\n- c\n\n3. x\n4. y"),
            [
                item(1, "•", "a"),
                item(2, "•", "b"),
                item(1, "•", "c"),
                item(1, "3.", "x"),
                item(1, "4.", "y"),
            ]
        );
    }

    #[test]
    fn a_loose_items_later_paragraphs_have_no_marker() {
        let mut later = item(1, "•", "more");
        later.marker = None;
        assert_eq!(
            blocks("- a\n\n  more\n- b"),
            [item(1, "•", "a"), later, item(1, "•", "b"),]
        );
    }

    #[test]
    fn task_items_show_their_box() {
        assert_eq!(
            blocks("- [ ] todo\n- [x] done"),
            [item(1, "☐", "todo"), item(1, "☑", "done")]
        );
    }

    #[test]
    fn quotes_count_their_depth() {
        assert_eq!(
            blocks("> a\n>> b"),
            [
                Block {
                    quote: 1,
                    ..block(BlockKind::Paragraph(vec![plain("a")]))
                },
                Block {
                    quote: 2,
                    ..block(BlockKind::Paragraph(vec![plain("b")]))
                },
            ]
        );
    }

    #[test]
    fn code_blocks_keep_lines_and_language() {
        assert_eq!(
            blocks("```rust ignore\nfn a() {\n\tb\n}\n```\n\n    indented"),
            [
                block(BlockKind::Code {
                    lang: Some("rust".to_owned()),
                    text: "fn a() {\n    b\n}".to_owned()
                }),
                block(BlockKind::Code {
                    lang: None,
                    text: "indented".to_owned()
                }),
            ]
        );
    }

    #[test]
    fn a_head_cut_inside_a_fence_ends_it() {
        assert_eq!(
            blocks("see:\n```\nline one\nline"),
            [
                block(BlockKind::Paragraph(vec![plain("see:")])),
                block(BlockKind::Code {
                    lang: None,
                    text: "line one\nline".to_owned()
                }),
            ]
        );
    }

    #[test]
    fn tables_keep_head_rows_and_alignment() {
        assert_eq!(
            blocks("| a | **b** |\n|:-:|--:|\n| 1 | 2 |\n| 3 |"),
            [block(BlockKind::Table {
                align: vec![Align::Center, Align::Right],
                head: vec![vec![plain("a")], vec![styled("b", BOLD)]],
                rows: vec![
                    vec![vec![plain("1")], vec![plain("2")]],
                    vec![vec![plain("3")], vec![]],
                ],
            })]
        );
    }

    #[test]
    fn only_web_and_mail_links_open() {
        let link = |text: &str, to: Option<&str>| Span {
            link: to.map(str::to_owned),
            ..plain(text)
        };
        assert_eq!(
            blocks(
                "[a](https://x.org/p) [b](javascript:alert(1)) [c](file:///etc) [d](mailto:me@x.org)"
            ),
            [block(BlockKind::Paragraph(vec![
                link("a", Some("https://x.org/p")),
                plain(" b c "),
                link("d", Some("mailto:me@x.org")),
            ]))]
        );
        assert!(!openable("https://"));
        assert!(!openable("https://x.org/\u{202E}gpj.exe"));
    }

    #[test]
    fn images_are_their_alt_text_never_fetched() {
        let alt = Style {
            italic: true,
            ..Style::default()
        };
        assert_eq!(
            blocks("![a chart](https://x.org/c.png) ![](c.png)"),
            [block(BlockKind::Paragraph(vec![
                Span {
                    link: Some("https://x.org/c.png".to_owned()),
                    ..styled("[image: a chart]", alt)
                },
                plain(" "),
                styled("[image]", alt),
            ]))]
        );
    }

    #[test]
    fn html_shows_as_text() {
        assert_eq!(
            blocks("<div>\nhi\n</div>\n\na <b>c</b>"),
            [
                block(BlockKind::Paragraph(vec![plain("<div>\nhi\n</div>")])),
                block(BlockKind::Paragraph(vec![plain("a <b>c</b>")])),
            ]
        );
    }

    #[test]
    fn entities_cannot_smuggle_bidi_controls() {
        assert_eq!(
            blocks("a &#x202E; b"),
            [block(BlockKind::Paragraph(vec![plain("a <U+202E> b")]))]
        );
    }

    #[test]
    fn rules_and_empty_input() {
        assert_eq!(blocks(""), []);
        assert_eq!(
            blocks("a\n\n---\n\nb"),
            [
                block(BlockKind::Paragraph(vec![plain("a")])),
                block(BlockKind::Rule),
                block(BlockKind::Paragraph(vec![plain("b")])),
            ]
        );
    }
}
