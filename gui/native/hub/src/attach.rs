//! Preparing an image the composer attaches: reading it, shrinking and
//! re-encoding it, and storing it under the core's data dir. It takes
//! 50-400 ms of CPU, so `run` does it on a blocking thread, never on the
//! hub task; the head only asks for it ([`crate::head::Out::Prepare`]).

use std::fmt;
use std::path::{Path, PathBuf};

use skywave_core::attachment::{self, AttachedImage};

/// Where an attached image's bytes come from.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ImageSource {
    /// A file the picker or a drop named.
    File(PathBuf),
    /// Bytes a paste or a keyboard insert carried.
    Bytes(Vec<u8>),
}

/// The bytes are not printed: a picture can be megabytes.
impl fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageSource::File(path) => write!(f, "File({})", path.display()),
            ImageSource::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
        }
    }
}

/// Prepares `source` and stores it under `data_dir`. What the draft keeps of
/// it, or the words the composer shows for why not.
pub(crate) fn prepare_image(data_dir: &Path, source: ImageSource) -> Result<AttachedImage, String> {
    let bytes = match source {
        ImageSource::File(path) => {
            std::fs::read(&path).map_err(|error| format!("the image could not be read: {error}"))?
        }
        ImageSource::Bytes(bytes) => bytes,
    };
    let prepared = attachment::prepare(&bytes).map_err(|refusal| refusal.to_string())?;
    attachment::store(data_dir, &prepared)
        .map_err(|error| format!("the image could not be saved: {error}"))?;
    Ok(prepared.attached())
}
