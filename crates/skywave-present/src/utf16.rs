//! Positions in text as UTF-16 code units, how Kotlin, Java and Dart
//! strings index, against code points (how the core counts cursors) and
//! bytes (how the core reports matches).

use std::ops::Range;

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A cursor `chars` code points into `text`, as UTF-16 code units. Past
/// the end clamps to the end.
pub fn chars_to_utf16(text: &str, chars: usize) -> u64 {
    count(text.chars().take(chars).map(char::len_utf16).sum())
}

/// A cursor `units` UTF-16 code units into `text`, as code points (how the
/// core counts). Inside a surrogate pair rounds down to before the pair;
/// past the end clamps to the end.
pub fn utf16_to_chars(text: &str, units: u64) -> usize {
    let mut seen = 0u64;
    text.chars()
        .take_while(|c| {
            seen += c.len_utf16() as u64;
            seen <= units
        })
        .count()
}

/// Byte `ranges` of `text` (on char boundaries, in order) as UTF-16 code
/// units, counted in one pass.
pub fn byte_ranges_to_utf16(text: &str, ranges: &[Range<usize>]) -> Vec<Range<u64>> {
    let (mut at, mut units) = (0usize, 0u64);
    let mut to = |byte: usize| {
        units += count(text[at..byte].encode_utf16().count());
        at = byte;
        units
    };
    ranges
        .iter()
        .map(|range| to(range.start)..to(range.end))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_crosses_in_utf16_units() {
        // (text, code points, UTF-16 units)
        for (text, chars, units) in [
            ("", 0, 0),
            ("hello", 3, 3),
            ("héllo", 2, 2),
            ("привет", 4, 4),
            ("a😀b", 1, 1),
            ("a😀b", 2, 3),
            ("a😀b", 3, 4),
            ("😀😀", 2, 4),
        ] {
            assert_eq!(chars_to_utf16(text, chars), units, "{text:?} chars {chars}");
            assert_eq!(utf16_to_chars(text, units), chars, "{text:?} units {units}");
        }
    }

    #[test]
    fn a_utf16_cursor_inside_a_surrogate_pair_rounds_down() {
        assert_eq!(utf16_to_chars("a😀b", 2), 1);
        assert_eq!(utf16_to_chars("😀", 1), 0);
    }

    #[test]
    fn a_cursor_past_the_end_clamps() {
        assert_eq!(utf16_to_chars("a😀", 99), 2);
        assert_eq!(utf16_to_chars("", u64::MAX), 0);
        assert_eq!(chars_to_utf16("a😀", 99), 3);
        assert_eq!(chars_to_utf16("", usize::MAX), 0);
    }

    #[test]
    fn byte_ranges_become_utf16_ranges() {
        let text = "héllo 👋 x";
        let wave = text.find('👋').unwrap();
        let x = text.find('x').unwrap();
        let ranges = byte_ranges_to_utf16(text, &[0..3, wave..wave + 4, x..x + 1]);
        // "é" is two bytes but one unit; the wave is four bytes, two units.
        assert_eq!(ranges, [0..2, 6..8, 9..10]);
    }
}
