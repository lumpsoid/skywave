use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

/// The search through the open session's timeline. Sent whole whenever it
/// changes; Dart replaces. A match that shows only expanded is expanded
/// by the hub: its row arrives expanded on the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub struct SearchUpdate {
    /// A search is open (`SearchTyped`) and not closed (`SearchClose`).
    pub open: bool,
    pub query: String,
    /// `3 of 17`, `17 matches`, `no matches`.
    pub count_label: String,
    /// The last step went round an end of the timeline; say so until the
    /// next step or edit.
    pub wrapped: bool,
    /// What the search could not look through: `3 outputs not downloaded`.
    pub partial_label: Option<String>,
    /// The row of the current match.
    pub current_key: Option<String>,
    /// For a current match inside a downloaded blob: the line of the
    /// blob's content it is on, from 0.
    pub current_line: Option<u32>,
    /// Where the query matches, per matching row, in the row's own
    /// strings as sent on the timeline.
    pub hits: Vec<SearchHit>,
}

/// Where the query matches in one timeline row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SearchHit {
    pub key: String,
    /// In the row's `text`.
    pub text: Vec<Utf16Range>,
    /// In its tool facts' `title`.
    pub title: Vec<Utf16Range>,
    /// In its tool facts' `detail`.
    pub detail: Vec<Utf16Range>,
    /// In its sections' lines.
    pub lines: Vec<LineHit>,
}

/// Matches in one section line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct LineHit {
    pub section: u32,
    pub line: u32,
    pub ranges: Vec<Utf16Range>,
}

/// `start..end` in UTF-16 code units, as Dart strings index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub struct Utf16Range {
    pub start: u32,
    pub end: u32,
}
