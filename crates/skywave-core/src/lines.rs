//! Reading a downloaded blob's content a few lines at a time, numbered as a
//! crop numbers them ([`skywave_proto::crop`]), so the line a head shows as
//! line N is the line a crop keeping line N keeps.
//!
//! A line is a run of bytes ending at a `\n` (kept with it) or at the end
//! of the content; a trailing `\n` starts no empty line, and empty content
//! has none. Lines are raw bytes: a head shows each one by itself through
//! [`strip_ansi`](skywave_proto::strip_ansi), never the content at once,
//! since an OSC or DCS string can swallow a `\n` and shift the numbering.

use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

/// One line start in every `STRIDE` is kept: a 64 MiB blob of empty lines
/// keeps 256 Ki offsets, not 64 Mi.
const STRIDE: u64 = 256;

/// A windowed reader of a file's lines. Plain blocking I/O: [`open`]
/// reads the whole file once and [`lines`] reads from a kept line start
/// on, so a head calls both off its UI thread.
///
/// [`open`]: LineIndex::open
/// [`lines`]: LineIndex::lines
#[derive(Debug)]
pub struct LineIndex {
    file: File,
    /// The offset of every `STRIDE`th line, from line 0.
    starts: Vec<u64>,
    len: u64,
}

impl LineIndex {
    /// Indexes the file at `path`, reading it to its end once.
    pub fn open(path: &Path) -> io::Result<LineIndex> {
        let mut file = File::open(path)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut starts = Vec::new();
        let mut offset = 0u64;
        // The index of the line the next byte begins, and whether a byte of
        // it was read: it counts even without a `\n` to end it.
        let mut line = 0u64;
        let mut open = false;
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            for (i, &byte) in buf[..n].iter().enumerate() {
                if !open {
                    if line.is_multiple_of(STRIDE) {
                        starts.push(offset + i as u64);
                    }
                    open = true;
                }
                if byte == b'\n' {
                    line += 1;
                    open = false;
                }
            }
            offset += n as u64;
        }
        Ok(LineIndex {
            file,
            starts,
            len: if open { line + 1 } else { line },
        })
    }

    /// How many lines the file has, as [`skywave_proto::crop::count`]
    /// counts them.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The lines in `range` (0 the first), each with its `\n` if it has
    /// one; a range past the last line is cut short.
    pub fn lines(&self, range: Range<u64>) -> io::Result<Vec<Vec<u8>>> {
        let end = range.end.min(self.len);
        if range.start >= end {
            return Ok(Vec::new());
        }
        let stride = range.start / STRIDE;
        let Some(&from) = usize::try_from(stride)
            .ok()
            .and_then(|i| self.starts.get(i))
        else {
            return Ok(Vec::new());
        };
        let mut file = &self.file;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut line = stride * STRIDE;
        while line < end {
            let mut bytes = Vec::new();
            if reader.read_until(b'\n', &mut bytes)? == 0 {
                break;
            }
            if line >= range.start {
                lines.push(bytes);
            }
            line += 1;
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use skywave_proto::LineSpan;
    use skywave_proto::crop::{count, crop};

    use super::*;

    fn index(dir: &tempfile::TempDir, content: &[u8]) -> LineIndex {
        let path = dir.path().join("blob");
        std::fs::write(&path, content).unwrap();
        LineIndex::open(&path).unwrap()
    }

    fn cropped(content: &[u8], first: u32) -> Vec<u8> {
        let mut out = Vec::new();
        crop(content, &mut out, &[LineSpan { first, count: 1 }]).unwrap();
        out
    }

    /// Line N read through the index is what a crop keeping line N keeps,
    /// even where an OSC string spans a `\n` (which `strip_ansi` over the
    /// whole content would swallow): the numbering a head shows is the
    /// server's.
    #[test]
    fn line_index_numbers_lines_as_a_crop_does() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"one\n\x1b]0;title\nstill osc\x07two\n\nthree\r\nlast";
        let lines = index(&dir, content);
        assert_eq!(lines.len(), count(&content[..]).unwrap());
        assert_eq!(lines.len(), 6);
        let all = lines.lines(0..lines.len()).unwrap();
        for n in 0..lines.len() {
            let one = lines.lines(n..n + 1).unwrap();
            assert_eq!(one, vec![cropped(content, n as u32)], "line {n}");
            assert_eq!(one[0], all[n as usize]);
        }
        // The OSC string's `\n` ends a line, as the crop counts it.
        assert_eq!(all[1], b"\x1b]0;title\n");
        assert_eq!(all.concat(), content);
    }

    /// Windows past a kept line start, across one and past the end.
    #[test]
    fn line_index_reads_windows_past_its_stride() {
        let dir = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..1000)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        let lines = index(&dir, &content);
        assert_eq!(lines.len(), 1000);
        assert_eq!(
            lines.lines(255..258).unwrap(),
            vec![
                b"line 255\n".to_vec(),
                b"line 256\n".to_vec(),
                b"line 257\n".to_vec(),
            ]
        );
        assert_eq!(lines.lines(998..2000).unwrap().len(), 2);
        assert!(lines.lines(1000..1001).unwrap().is_empty());
        assert_eq!(lines.lines(600..601).unwrap(), vec![cropped(&content, 600)]);
    }

    #[test]
    fn line_index_of_empty_content_has_no_lines() {
        let dir = tempfile::tempdir().unwrap();
        let lines = index(&dir, b"");
        assert!(lines.is_empty());
        assert!(lines.lines(0..1).unwrap().is_empty());
        let lines = index(&dir, b"\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines.lines(0..5).unwrap(), vec![b"\n".to_vec()]);
    }
}
