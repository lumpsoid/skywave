//! Preparing what the composer attaches: an image is read, shrunk and
//! re-encoded, and stored under the core's data dir; any other file is copied
//! there as it is. An image takes 50-400 ms of CPU, so `run` does this on a
//! blocking thread, never on the hub task; the head only asks for it
//! ([`crate::head::Out::Prepare`]).

use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use skywave_core::attachment::{self, AttachedFile, AttachedImage};
use skywave_proto::ImageType;

/// Where an attachment's bytes come from.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AttachSource {
    /// A file the picker or a drop named: an image or any other file.
    File(PathBuf),
    /// Bytes a paste or a keyboard insert carried: always an image.
    Bytes(Vec<u8>),
}

/// The bytes are not printed: a picture can be megabytes.
impl fmt::Debug for AttachSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachSource::File(path) => write!(f, "File({})", path.display()),
            AttachSource::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
        }
    }
}

/// What an attachment became: an image of the draft's images, or a file of
/// its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prepared {
    Image(AttachedImage),
    File(AttachedFile),
}

/// Most bytes read from a file to tell an image by its header.
const HEAD_LEN: u64 = 16;

/// Prepares `source` and stores it under `data_dir`. What the draft keeps of
/// it, or the words the composer shows for why not. A file that sniffs as an
/// image is prepared as one; any other is copied as it is, under its name.
pub(crate) fn prepare_attachment(
    data_dir: &Path,
    source: AttachSource,
) -> Result<Prepared, String> {
    let path = match source {
        AttachSource::File(path) => path,
        AttachSource::Bytes(bytes) => {
            return prepare_image(data_dir, AttachSource::Bytes(bytes)).map(Prepared::Image);
        }
    };
    let head = file_head(&path).map_err(|error| format!("the file could not be read: {error}"))?;
    if ImageType::sniff(&head).is_some() {
        return prepare_image(data_dir, AttachSource::File(path)).map(Prepared::Image);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the file has no usable name".to_owned())?
        .to_owned();
    let (sha256, size) = attachment::store_file(data_dir, &path)
        .map_err(|error| format!("the file could not be saved: {error}"))?;
    Ok(Prepared::File(AttachedFile {
        sha256,
        name,
        size,
        replace: None,
    }))
}

/// The first bytes of the file at `path`, at most [`HEAD_LEN`] of them.
fn file_head(path: &Path) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    std::fs::File::open(path)?
        .take(HEAD_LEN)
        .read_to_end(&mut head)?;
    Ok(head)
}

/// Prepares the image `source` and stores it under `data_dir`. What the draft
/// keeps of it, or the words for why not.
pub(crate) fn prepare_image(
    data_dir: &Path,
    source: AttachSource,
) -> Result<AttachedImage, String> {
    let bytes = match source {
        AttachSource::File(path) => {
            std::fs::read(&path).map_err(|error| format!("the image could not be read: {error}"))?
        }
        AttachSource::Bytes(bytes) => bytes,
    };
    let prepared = attachment::prepare(&bytes).map_err(|refusal| refusal.to_string())?;
    attachment::store(data_dir, &prepared)
        .map_err(|error| format!("the image could not be saved: {error}"))?;
    Ok(prepared.attached())
}
