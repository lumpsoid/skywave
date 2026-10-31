//! What a copy puts on the clipboard: present's copy target, read from
//! the blob's file when it is one, never just its preview. A blob not here
//! yet is downloaded first, and copied on the next try.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use skywave_core::{BlobId, BlobState};
use skywave_present::format::bytes;
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
        Content::Text(text) => {
            return Copied::Text {
                notice: format!("copied {}", bytes(text.len() as u64)),
                text,
            };
        }
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

fn copy_file(path: &Path, strip: Option<&str>, max: usize) -> Copied {
    let failed = |err: std::io::Error| {
        Copied::Failed(format!("copy: cannot read {}: {err}", path.display()))
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => return failed(err),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut content = Vec::new();
    if let Err(err) = file.take(max as u64 + 1).read_to_end(&mut content) {
        return failed(err);
    }
    if content.len() > max {
        return Copied::Refused(too_large(size.max(content.len() as u64), max, Some(path)));
    }
    if let Some(strip) = strip
        && content.starts_with(strip.as_bytes())
    {
        content.drain(..strip.len());
    }
    let text = String::from_utf8_lossy(&content).into_owned();
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
