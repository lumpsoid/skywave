//! Copying to the terminal's clipboard with OSC 52, which works over SSH
//! and through tmux (`set -g set-clipboard on`). Plain OSC 52 only: a
//! DCS-wrapped copy for tmux could set the clipboard twice and needs
//! `allow-passthrough`. The terminal never confirms, so a copy is only
//! "sent".
//!
//! [`App`](crate::App) decides what to copy ([`Clip`]); [`send`] does the
//! I/O for [`run`](crate::run).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossterm::execute;
use crossterm::style::Print;
use skywave_proto::strip_ansi;

/// Most raw bytes sent: about 100 KB once base64-encoded, which common
/// terminals accept. [`MAX_VAR`] overrides it.
pub const MAX: usize = 74 * 1024;

/// Environment variable overriding [`MAX`], in bytes.
pub const MAX_VAR: &str = "SKYWAVE_OSC52_MAX";

/// What to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clip {
    Text(String),
    /// A downloaded blob's whole content, with `strip` dropped from its
    /// start if it begins with it.
    File {
        path: PathBuf,
        strip: Option<&'static str>,
    },
    /// A shell run's downloaded output blob, which holds the raw output:
    /// copied with its ANSI sequences stripped, as every head shows it.
    Output {
        path: PathBuf,
    },
}

/// Why nothing was copied.
#[derive(Debug)]
pub enum CopyError {
    /// Over the cap; `file` names the blob's file, if it was one.
    TooLarge {
        size: u64,
        max: usize,
        file: Option<PathBuf>,
    },
    /// The blob's file could not be read, e.g. it was deleted meanwhile.
    Read { file: PathBuf, error: io::Error },
    /// The terminal could not be written to.
    Write(io::Error),
}

/// The cap: [`MAX_VAR`]'s value when it is a byte count, else [`MAX`].
pub fn max() -> usize {
    max_from(std::env::var(MAX_VAR).ok().as_deref())
}

fn max_from(var: Option<&str>) -> usize {
    var.and_then(|v| v.trim().parse().ok()).unwrap_or(MAX)
}

/// The OSC 52 sequence setting the clipboard to `bytes`.
pub fn osc52(bytes: &[u8]) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(bytes))
}

/// The bytes `clip` stands for, at most `max` of them: a file is read no
/// further than `max + 1` bytes. Invalid UTF-8 stays as it is.
pub fn load(clip: &Clip, max: usize) -> Result<Vec<u8>, CopyError> {
    match clip {
        Clip::Text(text) if text.len() > max => Err(CopyError::TooLarge {
            size: text.len() as u64,
            max,
            file: None,
        }),
        Clip::Text(text) => Ok(text.as_bytes().to_vec()),
        Clip::File { path, strip } => {
            let mut bytes = read_capped(path, max)?;
            if let Some(strip) = strip
                && bytes.starts_with(strip.as_bytes())
            {
                bytes.drain(..strip.len());
            }
            Ok(bytes)
        }
        // The cap holds for the raw file, so a huge output is never read
        // whole; stripping only shrinks it, bar invalid UTF-8 decoded to
        // replacement characters, hence the second check.
        Clip::Output { path } => {
            let bytes = read_capped(path, max)?;
            let text = strip_ansi(&String::from_utf8_lossy(&bytes));
            if text.len() > max {
                return Err(CopyError::TooLarge {
                    size: text.len() as u64,
                    max,
                    file: Some(path.clone()),
                });
            }
            Ok(text.into_bytes())
        }
    }
}

/// The file at `path`, if it holds at most `max` bytes: it is read no
/// further than `max + 1`.
fn read_capped(path: &Path, max: usize) -> Result<Vec<u8>, CopyError> {
    let read = |error| CopyError::Read {
        file: path.to_owned(),
        error,
    };
    let file = File::open(path).map_err(read)?;
    let size = file.metadata().map_err(read)?.len();
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(read)?;
    if bytes.len() > max {
        return Err(CopyError::TooLarge {
            size: size.max(bytes.len() as u64),
            max,
            file: Some(path.to_owned()),
        });
    }
    Ok(bytes)
}

/// Sends `clip` to the terminal's clipboard through `out`; returns how many
/// bytes were copied.
pub fn send(out: &mut impl Write, clip: &Clip, max: usize) -> Result<usize, CopyError> {
    let bytes = load(clip, max)?;
    execute!(out, Print(osc52(&bytes))).map_err(CopyError::Write)?;
    Ok(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_is_base64_between_the_markers() {
        assert_eq!(osc52(b"hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52(b""), "\x1b]52;c;\x07");
        // Raw bytes, not UTF-8.
        assert_eq!(osc52(b"\xff\xfe"), "\x1b]52;c;//4=\x07");
    }

    #[test]
    fn the_cap_is_overridable() {
        assert_eq!(max_from(None), 74 * 1024);
        assert_eq!(max_from(Some("1000")), 1000);
        assert_eq!(max_from(Some("lots")), MAX);
    }

    #[test]
    fn text_over_the_cap_is_refused() {
        assert_eq!(load(&Clip::Text("abcd".into()), 4).unwrap(), b"abcd");
        assert!(matches!(
            load(&Clip::Text("abcde".into()), 4),
            Err(CopyError::TooLarge {
                size: 5,
                max: 4,
                file: None
            })
        ));
    }

    #[test]
    fn a_file_is_copied_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"$ ls\n\xff").unwrap();
        let file = |strip| Clip::File {
            path: path.clone(),
            strip,
        };
        assert_eq!(load(&file(None), 6).unwrap(), b"$ ls\n\xff");
        assert_eq!(load(&file(Some("$ ")), 6).unwrap(), b"ls\n\xff");
        assert_eq!(load(&file(Some("# ")), 6).unwrap(), b"$ ls\n\xff");
        match load(&file(None), 5) {
            Err(CopyError::TooLarge { size, max, file }) => {
                assert_eq!((size, max, file), (6, 5, Some(path.clone())));
            }
            other => panic!("expected too large, got {other:?}"),
        }
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(load(&file(None), 6), Err(CopyError::Read { .. })));
    }

    #[test]
    fn send_writes_the_sequence() {
        let mut out = Vec::new();
        assert_eq!(send(&mut out, &Clip::Text("hi".into()), MAX).unwrap(), 2);
        assert_eq!(out, b"\x1b]52;c;aGk=\x07");
        let mut out = Vec::new();
        assert!(send(&mut out, &Clip::Text("hi".into()), 1).is_err());
        assert!(out.is_empty(), "nothing is sent over the cap");
    }
}
