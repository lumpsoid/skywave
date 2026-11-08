//! The crop page's rules ([`skywave_core::Screen::Crop`]): the selections a
//! head offers ready-made, how a selection is put in the shape a crop
//! needs, how it reads, and when it may be sent.
//!
//! A head keeps its selection in the core ([`Action::SetCropKeep`]), passed
//! through [`normalise`] first, and sends [`Action::CropBlob`] after asking
//! [`crop_confirmation`](crate::blob_actions::crop_confirmation).
//!
//! [`Action::SetCropKeep`]: skywave_core::Action::SetCropKeep
//! [`Action::CropBlob`]: skywave_core::Action::CropBlob

use skywave_core::{CropContent, CropView, LineSpan};
use skywave_proto::crop::check_spans;

/// The line counts the presets keep, each at most the content's lines.
pub const PRESET_SIZES: [u32; 3] = [50, 200, 1000];

/// A ready-made selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// The first `n` lines.
    First(u32),
    /// The last `n` lines.
    Last(u32),
    /// The first `n` lines and the last `n`.
    FirstAndLast(u32),
}

impl Preset {
    /// The lines it keeps of content of `lines` lines, normalised; `n`
    /// capped to `lines`, so it may keep them all.
    pub fn keep(self, lines: u32) -> Vec<LineSpan> {
        let span = |first, count| LineSpan { first, count };
        normalise(&match self {
            Preset::First(n) => vec![span(0, n.min(lines))],
            Preset::Last(n) => {
                let n = n.min(lines);
                vec![span(lines - n, n)]
            }
            Preset::FirstAndLast(n) => {
                let n = n.min(lines);
                vec![span(0, n), span(lines - n, n)]
            }
        })
    }

    pub fn label(self) -> String {
        match self {
            Preset::First(n) => format!("First {n}"),
            Preset::Last(n) => format!("Last {n}"),
            Preset::FirstAndLast(n) => format!("First and last {n}"),
        }
    }
}

/// The presets worth offering for content of `lines` lines: each of
/// [`PRESET_SIZES`] as first, last, and first and last, where it keeps
/// fewer lines than there are (keeping them all is no crop).
pub fn presets(lines: u32) -> Vec<Preset> {
    PRESET_SIZES
        .iter()
        .flat_map(|&n| [Preset::First(n), Preset::Last(n), Preset::FirstAndLast(n)])
        .filter(|preset| kept(&preset.keep(lines)) < u64::from(lines))
        .collect()
}

/// `spans` in the shape a crop needs: empty ones dropped, sorted, and
/// those that overlap or touch merged.
pub fn normalise(spans: &[LineSpan]) -> Vec<LineSpan> {
    let mut sorted: Vec<&LineSpan> = spans.iter().filter(|s| s.count > 0).collect();
    sorted.sort_by_key(|s| s.first);
    let mut merged: Vec<LineSpan> = Vec::with_capacity(sorted.len());
    for span in sorted {
        match merged.last_mut() {
            Some(last) if u64::from(span.first) <= end(last) => {
                let to = end(span).max(end(last));
                last.count = u32::try_from(to - u64::from(last.first)).unwrap_or(u32::MAX);
            }
            _ => merged.push(*span),
        }
    }
    merged
}

/// `keep` with lines `first..=last` toggled, given in either order: taken
/// out when all are kept, else added; normalised. A head's range toggle
/// (the TUI's `v`, the GUI's drag) sends this as its selection.
pub fn toggle(keep: &[LineSpan], first: u32, last: u32) -> Vec<LineSpan> {
    let (first, last) = (first.min(last), first.max(last));
    let keep = normalise(keep);
    let to = u64::from(last) + 1;
    let span = |from: u64, to: u64| LineSpan {
        first: u32::try_from(from).unwrap_or(u32::MAX),
        count: u32::try_from(to.saturating_sub(from)).unwrap_or(u32::MAX),
    };
    // Normalised spans merge when they touch, so lines all kept lie in one.
    let out: Vec<LineSpan> = if keep.iter().any(|s| s.first <= first && end(s) >= to) {
        keep.iter()
            .flat_map(|s| {
                [
                    span(u64::from(s.first), end(s).min(u64::from(first))),
                    span(u64::from(s.first).max(to), end(s)),
                ]
            })
            .collect()
    } else {
        keep.into_iter()
            .chain([span(u64::from(first), to)])
            .collect()
    };
    normalise(&out)
}

/// How many lines `spans` keep, counting overlaps twice: normalise first.
pub fn kept(spans: &[LineSpan]) -> u64 {
    spans.iter().map(|s| u64::from(s.count)).sum()
}

/// A selection in words: "keeps K of M lines".
pub fn keeps_label(kept: u64, of: u32) -> String {
    format!("keeps {kept} of {of} lines")
}

/// Whether the page's crop may be sent: the content is counted here, the
/// selection has a crop's shape, lies within the content and keeps fewer
/// lines than it has, and no delete or crop of this device's is pending.
pub fn crop_enabled(view: &CropView) -> bool {
    let CropContent::Ready { lines, .. } = view.content else {
        return false;
    };
    view.pending.is_none()
        && check_spans(&view.keep)
        && view.keep.last().is_some_and(|s| end(s) <= u64::from(lines))
        && kept(&view.keep) < u64::from(lines)
}

/// The line after `span`'s last.
fn end(span: &LineSpan) -> u64 {
    u64::from(span.first) + u64::from(span.count)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use skywave_core::{
        BlobCommand, BlobId, BlobPending, BlobState, BlobUse, CommandId, Outgoing, SessionId,
    };

    use super::*;

    fn span(first: u32, count: u32) -> LineSpan {
        LineSpan { first, count }
    }

    fn page(content: CropContent, keep: Vec<LineSpan>) -> CropView {
        CropView {
            blob: BlobId::new(),
            session: SessionId::new(),
            holds: BlobUse::ShellOutput,
            title: "make".into(),
            content,
            keep,
            pending: None,
        }
    }

    fn ready(lines: u32) -> CropContent {
        CropContent::Ready {
            path: PathBuf::from("blob"),
            lines,
        }
    }

    #[test]
    fn presets_keep_n_lines_below_at_and_above_n() {
        // Above: every kind keeps what it says.
        assert_eq!(Preset::First(50).keep(120), [span(0, 50)]);
        assert_eq!(Preset::Last(50).keep(120), [span(70, 50)]);
        assert_eq!(
            Preset::FirstAndLast(50).keep(120),
            [span(0, 50), span(70, 50)]
        );
        // First and last meet: one span of them all.
        assert_eq!(Preset::FirstAndLast(50).keep(100), [span(0, 100)]);
        assert_eq!(Preset::FirstAndLast(50).keep(70), [span(0, 70)]);
        // At and below: capped to the lines there are.
        for lines in [50, 30] {
            assert_eq!(Preset::First(50).keep(lines), [span(0, lines)]);
            assert_eq!(Preset::Last(50).keep(lines), [span(0, lines)]);
            assert_eq!(Preset::FirstAndLast(50).keep(lines), [span(0, lines)]);
        }
        assert!(Preset::First(50).keep(0).is_empty());
        assert_eq!(Preset::FirstAndLast(1000).label(), "First and last 1000");
    }

    #[test]
    fn presets_offer_only_what_keeps_fewer_lines_than_there_are() {
        assert_eq!(presets(0), []);
        assert_eq!(presets(50), []);
        assert_eq!(
            presets(51),
            [Preset::First(50), Preset::Last(50)],
            "first and last 50 keep all 51"
        );
        assert_eq!(
            presets(300),
            [
                Preset::First(50),
                Preset::Last(50),
                Preset::FirstAndLast(50),
                Preset::First(200),
                Preset::Last(200),
            ]
        );
        assert_eq!(presets(5000).len(), 9);
        for preset in presets(5000) {
            assert!(check_spans(&preset.keep(5000)), "{preset:?}");
        }
    }

    #[test]
    fn normalise_sorts_and_merges_overlapping_and_adjacent_spans() {
        assert_eq!(
            normalise(&[
                span(10, 5),
                span(0, 3),
                span(12, 10),
                span(3, 2),
                span(30, 0)
            ]),
            [span(0, 5), span(10, 12)]
        );
        // Kept apart by a line, they stay two.
        assert_eq!(
            normalise(&[span(4, 2), span(0, 3)]),
            [span(0, 3), span(4, 2)]
        );
        // One inside another.
        assert_eq!(normalise(&[span(0, 10), span(2, 3)]), [span(0, 10)]);
        assert!(normalise(&[]).is_empty());
        assert!(check_spans(&normalise(&[span(5, 1), span(0, 6)])));
    }

    #[test]
    fn toggle_adds_lines_not_all_kept_and_takes_out_lines_all_kept() {
        assert_eq!(toggle(&[], 3, 5), [span(3, 3)]);
        assert_eq!(toggle(&[], 5, 3), [span(3, 3)], "either order");
        // Partly kept: the whole range is added.
        assert_eq!(toggle(&[span(0, 4)], 3, 5), [span(0, 6)]);
        // All kept: taken out, splitting the span.
        assert_eq!(toggle(&[span(0, 10)], 3, 5), [span(0, 3), span(6, 4)]);
        assert_eq!(toggle(&[span(2, 3)], 2, 4), []);
        assert_eq!(toggle(&[span(4, 1)], 4, 4), []);
        // Kept across two spans that touch is all kept.
        assert_eq!(
            toggle(&[span(0, 3), span(3, 3)], 1, 4),
            [span(0, 1), span(5, 1)]
        );
        // Spans apart from the range are left as they are.
        assert_eq!(
            toggle(&[span(0, 2), span(10, 2)], 4, 5),
            [span(0, 2), span(4, 2), span(10, 2)]
        );
        assert_eq!(
            toggle(&[span(0, 2), span(4, 2), span(10, 2)], 4, 5),
            [span(0, 2), span(10, 2)]
        );
        // The last line there can be.
        assert_eq!(toggle(&[], u32::MAX, u32::MAX), [span(u32::MAX, 1)]);
    }

    #[test]
    fn crop_is_enabled_for_a_counted_selection_that_keeps_fewer_lines() {
        assert!(crop_enabled(&page(ready(10), vec![span(0, 3)])));
        // Keeping them all is no crop.
        assert!(!crop_enabled(&page(ready(10), vec![span(0, 10)])));
        assert!(!crop_enabled(&page(
            ready(10),
            vec![span(0, 4), span(4, 6)]
        )));
        // Nothing chosen, past the end, or not in a crop's shape.
        assert!(!crop_enabled(&page(ready(10), Vec::new())));
        assert!(!crop_enabled(&page(ready(10), vec![span(8, 3)])));
        assert!(!crop_enabled(&page(
            ready(10),
            vec![span(4, 2), span(0, 2)]
        )));
        // Not counted here.
        for content in [
            CropContent::NotHere(BlobState::NotDownloaded { size: 9 }),
            CropContent::Counting,
            CropContent::Gone,
        ] {
            assert!(!crop_enabled(&page(content, vec![span(0, 3)])));
        }
        // This device's own crop of it on its way.
        let mut sent = page(ready(10), vec![span(0, 3)]);
        sent.pending = Some(BlobPending {
            command: CommandId::new(),
            kind: BlobCommand::Crop,
            outgoing: Outgoing::Queued,
            discardable: true,
        });
        assert!(!crop_enabled(&sent));
        assert_eq!(keeps_label(3, 10), "keeps 3 of 10 lines");
    }
}
