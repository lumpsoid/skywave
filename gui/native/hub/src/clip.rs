//! What a copy puts on the clipboard: present's copy target, read from
//! the blob's file when it is one, never just its preview. A blob not here
//! yet is downloaded first, and copied on the next try.

use std::fs::File;
use std::path::Path;

use skywave_core::{BlobId, BlobState, ShellRun};
use skywave_present::clip::{ClipError, file_clip, output_clip};
use skywave_present::format::bytes;
use skywave_present::output::{WholeOutput, whole_output};
use skywave_present::sections::Content;

/// Most bytes put on the clipboard; a copy past it is refused, naming the
/// file, rather than cut.
pub const COPY_MAX: usize = 1024 * 1024;

/// What a copy came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// Put `text` on the clipboard; `notice` says how much.
    Text { text: String, notice: String },
    /// The blob is not here: download it (`fetch`, unless it is on its way
    /// already) and say to copy again.
    Fetch {
        fetch: Option<BlobId>,
        notice: String,
    },
    /// Nothing copied, for the reason given.
    Refused(String),
    /// The file could not be read.
    Failed(String),
}

/// Copies `content`, at most `max` bytes of it.
pub fn copy(content: Content, max: usize) -> Copied {
    let (blob, strip) = match content {
        Content::Text(text) if text.len() > max => {
            return Copied::Refused(too_large(text.len() as u64, max, None));
        }
        Content::Text(text) => return text_copied(text),
        Content::Blob { blob, strip } => (blob, strip),
    };
    let (size, fetch) = match blob.state {
        BlobState::Available { path } => return copy_file(&path, strip, max),
        BlobState::NotDownloaded { size } | BlobState::Failed { size, .. } => (size, Some(blob.id)),
        BlobState::Requested { size } | BlobState::Downloading { size, .. } => (size, None),
    };
    Copied::Fetch {
        fetch,
        notice: format!("downloading {} · copy again when it is here", bytes(size)),
    }
}

/// Copies `run`'s whole output ([`whole_output`]), at most `max` bytes of
/// it: its blob ANSI stripped once downloaded, else the run's own text,
/// stripped already. A copy of only part of it says so
/// ([`WholeOutput::copy_note`]).
pub fn copy_output(run: &ShellRun, max: usize) -> Copied {
    let whole = whole_output(run);
    match &whole {
        WholeOutput::Here(text) | WholeOutput::Partial { text, .. } => {
            match (
                copy(Content::Text((*text).to_owned()), max),
                whole.copy_note(),
            ) {
                (Copied::Text { text, notice }, Some(note)) => Copied::Text {
                    text,
                    notice: format!("{notice} · {note}"),
                },
                (copied, _) => copied,
            }
        }
        WholeOutput::Blob(blob) => match &blob.state {
            BlobState::Available { path } => {
                copied(path, max, |file, size| output_clip(file, size, max))
            }
            _ => copy(
                Content::Blob {
                    blob: (*blob).clone(),
                    strip: None,
                },
                max,
            ),
        },
    }
}

fn copy_file(path: &Path, strip: Option<&str>, max: usize) -> Copied {
    copied(path, max, |file, size| {
        file_clip(file, size, strip, max).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    })
}

/// Copies what `read` makes of the file at `path` (present's [`clip`]
/// rules, given the file and its size), or says why not.
///
/// [`clip`]: skywave_present::clip
fn copied(
    path: &Path,
    max: usize,
    read: impl FnOnce(File, u64) -> Result<String, ClipError>,
) -> Copied {
    let failed = |err: std::io::Error| {
        Copied::Failed(format!("copy: cannot read {}: {err}", path.display()))
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => return failed(err),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    match read(file, size) {
        Ok(text) => text_copied(text),
        Err(ClipError::TooLarge { size }) => Copied::Refused(too_large(size, max, Some(path))),
        Err(ClipError::Read(err)) => failed(err),
    }
}

fn text_copied(text: String) -> Copied {
    Copied::Text {
        notice: format!("copied {}", bytes(text.len() as u64)),
        text,
    }
}

fn too_large(size: u64, max: usize, file: Option<&Path>) -> String {
    let file = file.map_or_else(String::new, |path| format!(" · file: {}", path.display()));
    format!(
        "too large to copy ({} > {}){file}",
        bytes(size),
        bytes(max as u64)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use skywave_core::BlobView;

    use super::*;

    #[test]
    fn a_file_is_copied_without_its_prompt_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, "$ cargo test").unwrap();
        let blob = |state| Content::Blob {
            blob: BlobView {
                id: BlobId::new(),
                state,
                change: None,
                pending: None,
            },
            strip: Some("$ "),
        };
        let here = || blob(BlobState::Available { path: path.clone() });
        assert_eq!(
            copy(here(), 100),
            Copied::Text {
                text: "cargo test".into(),
                notice: "copied 10 B".into(),
            }
        );
        assert!(matches!(copy(here(), 4), Copied::Refused(n) if n.contains("file:")));
        assert!(matches!(
            copy(blob(BlobState::NotDownloaded { size: 9 }), 100),
            Copied::Fetch { fetch: Some(_), .. }
        ));
        assert!(matches!(
            copy(blob(BlobState::Requested { size: 9 }), 100),
            Copied::Fetch { fetch: None, .. }
        ));
    }
}
