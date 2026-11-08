//! What a copy reads of a downloaded blob: the file whole or not at all,
//! never past a head's cap. The head opens the file and hands it in as a
//! reader; the cap, the prompt a copy drops and how a shell output becomes
//! text are here.

use std::io::{self, Read};

use crate::output::output_text;

/// Why a copy read nothing.
#[derive(Debug)]
pub enum ClipError {
    /// Over the cap: the content is `size` bytes, as far as is known
    /// without reading it whole.
    TooLarge { size: u64 },
    /// The file could not be read.
    Read(io::Error),
}

/// The content of `file`, if it holds at most `max` bytes. It is read no
/// further than `max + 1`, so a huge file is never read whole; `size` is
/// its length as its metadata says, what [`ClipError::TooLarge`] reports
/// unless more was read.
pub fn read_capped(file: impl Read, size: u64, max: usize) -> Result<Vec<u8>, ClipError> {
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(ClipError::Read)?;
    if bytes.len() > max {
        return Err(ClipError::TooLarge {
            size: size.max(bytes.len() as u64),
        });
    }
    Ok(bytes)
}

/// A blob's file to copy ([`read_capped`]), with `strip` dropped from its
/// start if it begins with it (a shell call's `$ `). Invalid UTF-8 stays
/// as it is.
pub fn file_clip(
    file: impl Read,
    size: u64,
    strip: Option<&str>,
    max: usize,
) -> Result<Vec<u8>, ClipError> {
    let mut bytes = read_capped(file, size, max)?;
    if let Some(strip) = strip
        && bytes.starts_with(strip.as_bytes())
    {
        bytes.drain(..strip.len());
    }
    Ok(bytes)
}

/// A shell run's output blob to copy, which holds the raw output: its text
/// as every head shows it ([`output_text`]). The cap holds for the raw
/// file, so a huge output is never read whole; stripping only shrinks it,
/// bar invalid UTF-8 decoded to replacement characters, hence the second
/// check.
pub fn output_clip(file: impl Read, size: u64, max: usize) -> Result<String, ClipError> {
    let text = output_text(&read_capped(file, size, max)?, false);
    if text.len() > max {
        return Err(ClipError::TooLarge {
            size: text.len() as u64,
        });
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn too_large<T: std::fmt::Debug>(read: Result<T, ClipError>) -> u64 {
        match read {
            Err(ClipError::TooLarge { size }) => size,
            other => panic!("expected too large, got {other:?}"),
        }
    }

    #[test]
    fn a_file_is_read_whole_or_not_at_all() {
        let file: &[u8] = b"$ ls\n\xff";
        assert_eq!(read_capped(file, 6, 6).unwrap(), file);
        // Over the cap: the metadata's size, or what was read if more.
        assert_eq!(too_large(read_capped(file, 6, 5)), 6);
        assert_eq!(too_large(read_capped(file, 0, 4)), 5);
        // Never read past the cap.
        let mut huge = io::repeat(b'a');
        assert_eq!(too_large(read_capped(&mut huge, 1 << 40, 8)), 1 << 40);
    }

    #[test]
    fn a_read_error_is_not_a_size() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("gone"))
            }
        }
        assert!(matches!(read_capped(Broken, 0, 8), Err(ClipError::Read(_))));
    }

    #[test]
    fn a_file_copy_drops_its_prompt_only_when_it_starts_with_it() {
        let file: &[u8] = b"$ ls\n\xff";
        assert_eq!(file_clip(file, 6, Some("$ "), 6).unwrap(), b"ls\n\xff");
        assert_eq!(file_clip(file, 6, Some("# "), 6).unwrap(), file);
        assert_eq!(file_clip(file, 6, None, 6).unwrap(), file);
        // The cap holds for the file, before the prompt goes.
        assert_eq!(too_large(file_clip(file, 6, Some("$ "), 5)), 6);
    }

    #[test]
    fn an_output_copy_is_stripped_and_capped_on_both_sides() {
        let raw: &[u8] = b"\x1b[31mred\x1b[0m\n";
        assert_eq!(output_clip(raw, 13, 13).unwrap(), "red\n");
        // Over the cap as raw bytes, though its text would fit.
        assert_eq!(too_large(output_clip(raw, 13, 12)), 13);
        // Within it raw, over it once invalid UTF-8 is decoded: each
        // stray byte becomes three.
        let raw: &[u8] = b"\xff\xff";
        assert_eq!(too_large(output_clip(raw, 2, 4)), 6);
    }
}
