//! Cropping a blob to chosen lines (`CommandKind::CropBlob`), the one
//! definition of a line the server and every device share.
//!
//! A line is a run of bytes ending at `b'\n'` (kept with it) or at the end
//! of the content. A trailing `\n` does not start an empty last line, and
//! empty content has no lines. Bytes are copied verbatim: no ANSI
//! stripping, no UTF-8 decoding, `\r\n` kept as it is. Lines are counted on
//! the raw bytes, never on stripped text, since stripping an escape string
//! can swallow a `\n`.

use std::fmt;
use std::io::{self, BufWriter, ErrorKind, Read, Write};

use crate::LineSpan;
use crate::limits::CROP_SPANS_MAX;

/// What [`crop`] found in the content it cropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropCount {
    /// The content's lines, all of them, not only the kept ones.
    pub lines: u32,
}

/// Why [`crop`] failed.
#[derive(Debug)]
pub enum CropError {
    /// Reading the content or writing the kept lines failed.
    Io(io::Error),
    /// The spans do not fit the content: their shape is wrong
    /// ([`check_spans`]), one runs past the last line, or they keep every
    /// line. Also content of more lines than a `u32` counts.
    BadCrop,
}

impl fmt::Display for CropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CropError::Io(e) => write!(f, "cropping failed: {e}"),
            CropError::BadCrop => f.write_str("the kept lines do not fit the content"),
        }
    }
}

impl std::error::Error for CropError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CropError::Io(e) => Some(e),
            CropError::BadCrop => None,
        }
    }
}

impl From<io::Error> for CropError {
    fn from(e: io::Error) -> CropError {
        CropError::Io(e)
    }
}

/// Whether `keep` has the shape a crop needs, whatever the content: at
/// least one span and at most [`CROP_SPANS_MAX`], each of at least one
/// line, sorted and not overlapping. Spans that touch (`3+2`, `5+1`) are
/// allowed and kept as given.
pub fn check_spans(keep: &[LineSpan]) -> bool {
    if keep.is_empty() || keep.len() > CROP_SPANS_MAX {
        return false;
    }
    let mut next = 0u64;
    for span in keep {
        if span.count == 0 || u64::from(span.first) < next {
            return false;
        }
        next = end(span);
    }
    true
}

/// Streams `content`, writing the lines `keep` names to `out`, the kept
/// spans joined with nothing between them. Returns the content's line
/// count, or [`CropError::BadCrop`] if `keep` does not fit it: then `out`
/// has had some of the content written and is the caller's to discard.
/// Checks `keep`'s shape ([`check_spans`]) before reading anything.
pub fn crop(
    mut content: impl Read,
    out: impl Write,
    keep: &[LineSpan],
) -> Result<CropCount, CropError> {
    if !check_spans(keep) {
        return Err(CropError::BadCrop);
    }
    let mut out = BufWriter::new(out);
    let lines = walk(&mut content, |line, part| {
        if kept(keep, line) {
            out.write_all(part)?;
        }
        Ok(())
    })?;
    out.flush()?;
    let lines = u32::try_from(lines).map_err(|_| CropError::BadCrop)?;
    let last = keep.last().map(end).unwrap_or(0);
    let kept: u64 = keep.iter().map(|s| u64::from(s.count)).sum();
    if last > u64::from(lines) || kept == u64::from(lines) {
        return Err(CropError::BadCrop);
    }
    Ok(CropCount { lines })
}

/// The lines in `content`, as [`crop`] counts them.
pub fn count(mut content: impl Read) -> io::Result<u64> {
    walk(&mut content, |_, _| Ok(()))
}

/// The line after `span`'s last.
fn end(span: &LineSpan) -> u64 {
    u64::from(span.first) + u64::from(span.count)
}

/// Whether line `line` is in one of `keep`'s spans.
fn kept(keep: &[LineSpan], line: u64) -> bool {
    // Spans are few (CROP_SPANS_MAX) and sorted.
    let i = keep.partition_point(|s| end(s) <= line);
    keep.get(i).is_some_and(|s| u64::from(s.first) <= line)
}

/// Reads `content` to its end, handing each piece of a line to `each` with
/// the line's index; returns how many lines there were.
fn walk(
    content: &mut impl Read,
    mut each: impl FnMut(u64, &[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut line = 0u64;
    // Whether bytes of line `line` were read: it counts even without a
    // `\n` to end it.
    let mut open = false;
    loop {
        let n = match content.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut chunk = &buf[..n];
        while !chunk.is_empty() {
            let (part, ends) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (&chunk[..=i], true),
                None => (chunk, false),
            };
            each(line, part)?;
            if ends {
                line += 1;
                open = false;
            } else {
                open = true;
            }
            chunk = &chunk[part.len()..];
        }
    }
    Ok(if open { line + 1 } else { line })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentHash;

    fn span(first: u32, count: u32) -> LineSpan {
        LineSpan { first, count }
    }

    fn run(content: &[u8], keep: &[LineSpan]) -> Result<(Vec<u8>, u32), CropError> {
        let mut out = Vec::new();
        let counted = crop(content, &mut out, keep)?;
        // `count` agrees with `crop` on every content cropped here.
        assert_eq!(lines_in(content), u64::from(counted.lines));
        Ok((out, counted.lines))
    }

    fn lines_in(content: &[u8]) -> u64 {
        count(content).unwrap()
    }

    /// A reader handing out one byte at a time, so lines span reads.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    #[test]
    fn empty_content_has_no_lines_and_no_span_fits() {
        assert_eq!(lines_in(b""), 0);
        assert!(matches!(
            crop(&b""[..], Vec::new(), &[span(0, 1)]),
            Err(CropError::BadCrop)
        ));
    }

    #[test]
    fn a_last_line_without_a_newline_counts() {
        assert_eq!(run(b"a\nb\nc", &[span(2, 1)]).unwrap(), (b"c".to_vec(), 3));
        assert_eq!(
            run(b"a\nb\nc", &[span(0, 1)]).unwrap(),
            (b"a\n".to_vec(), 3)
        );
    }

    #[test]
    fn a_trailing_newline_starts_no_empty_line() {
        assert_eq!(
            run(b"a\nb\nc\n", &[span(2, 1)]).unwrap(),
            (b"c\n".to_vec(), 3)
        );
        // Line 3 would be the empty one; there is none.
        assert!(matches!(
            run(b"a\nb\nc\n", &[span(3, 1)]),
            Err(CropError::BadCrop)
        ));
        // Empty lines in the middle are lines.
        assert_eq!(lines_in(b"\n\n"), 2);
        assert_eq!(lines_in(b"\n\nx"), 3);
    }

    #[test]
    fn crlf_is_kept_verbatim() {
        assert_eq!(
            run(b"a\r\nb\r\nc\r\n", &[span(0, 1), span(2, 1)]).unwrap(),
            (b"a\r\nc\r\n".to_vec(), 3)
        );
    }

    #[test]
    fn invalid_utf8_and_escapes_pass_through() {
        let content = b"\xff\xfe\n\x1b]0;t\x07x\n\x1b[31mred\n";
        assert_eq!(
            run(content, &[span(0, 1), span(2, 1)]).unwrap(),
            (b"\xff\xfe\n\x1b[31mred\n".to_vec(), 3)
        );
    }

    #[test]
    fn a_span_at_the_very_end_fits_one_past_it_does_not() {
        let content = b"1\n2\n3\n4\n";
        assert_eq!(
            run(content, &[span(0, 1), span(2, 2)]).unwrap(),
            (b"1\n3\n4\n".to_vec(), 4)
        );
        assert!(matches!(
            run(content, &[span(0, 1), span(2, 3)]),
            Err(CropError::BadCrop)
        ));
        assert!(matches!(
            run(content, &[span(9, 1)]),
            Err(CropError::BadCrop)
        ));
    }

    #[test]
    fn keeping_every_line_is_a_bad_crop() {
        assert!(matches!(
            run(b"1\n2\n", &[span(0, 1), span(1, 1)]),
            Err(CropError::BadCrop)
        ));
        // Control: one line fewer is a crop.
        assert_eq!(run(b"1\n2\n", &[span(1, 1)]).unwrap(), (b"2\n".to_vec(), 2));
    }

    #[test]
    fn lines_spanning_reads_crop_the_same() {
        let content: Vec<u8> = (1..=50)
            .flat_map(|n| format!("{n}\n").into_bytes())
            .collect();
        let keep = [span(0, 3), span(47, 3)];
        let mut out = Vec::new();
        let count = crop(Trickle(&content), &mut out, &keep).unwrap();
        assert_eq!(count.lines, 50);
        assert_eq!(out, b"1\n2\n3\n48\n49\n50\n");
    }

    #[test]
    fn spans_of_the_wrong_shape_are_refused() {
        assert!(check_spans(&[span(0, 1), span(1, 2), span(5, 1)]));
        assert!(!check_spans(&[]));
        assert!(!check_spans(&[span(0, 3), span(2, 1)]), "overlapping");
        assert!(!check_spans(&[span(5, 1), span(0, 1)]), "unsorted");
        assert!(!check_spans(&[span(0, 0)]), "zero count");
        let many: Vec<_> = (0..CROP_SPANS_MAX as u32 + 1)
            .map(|i| span(i * 2, 1))
            .collect();
        assert!(!check_spans(&many), "65 spans");
        assert!(check_spans(&many[..CROP_SPANS_MAX]), "64 spans");
        // A bad shape is refused before reading anything.
        struct Unreadable;
        impl Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("read");
            }
        }
        assert!(matches!(
            crop(Unreadable, Vec::new(), &[span(1, 0)]),
            Err(CropError::BadCrop)
        ));
    }

    #[test]
    fn two_runs_hash_the_same() {
        let content: Vec<u8> = (1..=3000)
            .flat_map(|n| format!("{n}\n").into_bytes())
            .collect();
        let keep = [span(0, 10), span(2990, 10)];
        let (a, _) = run(&content, &keep).unwrap();
        let (b, _) = run(&content, &keep).unwrap();
        assert_eq!(ContentHash::of(&a), ContentHash::of(&b));
        assert_eq!(a, b);
    }
}
