//! Preparing a `CropBlob` (architecture.md "Server", D42): the new blob is
//! written off the database, before the command's transaction, which only
//! records it ([`EventLog::handle_with`]).
//!
//! [`prepare`] streams the blob through [`skywave_proto::crop::crop`] into a
//! new blob file ([`BlobStore::write_with`]), reading from a file opened
//! beforehand, so a delete that unlinks the blob meanwhile does not cut the
//! read short (the transaction then finds the blob gone). Spans that do not
//! fit the content (`BadCrop`) and a disk with no room for the new blob
//! (`ENOSPC`, `EDQUOT`: `DiskLow`) are outcomes, refused like any command;
//! the partial file is removed either way. Any other I/O error is the
//! caller's to answer (a 500).
//!
//! A [`Prepared`] crop owns the file it wrote: dropped before the
//! transaction that records it commits (the command was answered already,
//! refused, the transaction failed, or a caller gave up on it), it removes
//! it, like an import's ([`crate::agent::import`]).
//!
//! [`EventLog::handle_with`]: crate::EventLog::handle_with

use std::io::{self, Read};
use std::time::Duration;

use rusqlite::Connection;
use skywave_proto::crop::{self, CropError};
use skywave_proto::{BlobId, BlobRef, LineSpan, RejectReason};

use crate::blobs::{BlobMeta, BlobStore};
use crate::error::Result;
use crate::event_log;

/// Longest a crop may take, from opening the blob to its new file synced.
/// The device waits for the command's reply and gives up after its request
/// timeout (30 s), like an import's ([`IMPORT_DEADLINE`]); past it the
/// reply is a 503, nothing is handled, and the device resends.
///
/// [`IMPORT_DEADLINE`]: crate::agent::import::IMPORT_DEADLINE
pub const CROP_DEADLINE: Duration = Duration::from_secs(20);

/// Runs on a crop's blocking thread once the new blob's temp file exists,
/// before the content is copied. Tests hold a crop there to race it, or
/// fail it as a write would ([`crate::server::ServerOptions::crop_hook`]).
pub trait CropHook: Send + Sync + 'static {
    /// An `Err` fails the crop's write with it.
    fn writing(&self) -> io::Result<()>;

    /// The crop's thread is ending: its crop was handed to the request, or
    /// dropped (its file removed) for a request gone past its deadline.
    fn ended(&self) {}
}

/// A crop of blob `blob` ready to log: the new blob, written but not
/// recorded, or the refusal its preparation came to.
#[derive(Debug)]
pub struct Prepared {
    blob: BlobId,
    /// The new blob and the line count of `blob`, or the refusal.
    outcome: std::result::Result<(BlobMeta, u32), RejectReason>,
    blobs: BlobStore,
    /// Whether the new blob's file is still this crop's to remove: until
    /// [`Prepared::kept`] after it was logged.
    owns: bool,
    /// Set once its new blob is recorded and the old one's row deleted in
    /// a transaction.
    logged: bool,
}

/// Crops `content`, blob `blob`'s, to the lines `keep` names into a new
/// blob of `blobs`, not recorded. Blocking: call it off the async runtime.
/// `Err` is an I/O failure other than a full disk.
pub fn prepare(
    content: impl Read,
    blobs: BlobStore,
    blob: BlobId,
    keep: &[LineSpan],
    hook: Option<&dyn CropHook>,
) -> std::result::Result<Prepared, io::Error> {
    let written = blobs.write_with(|out| {
        if let Some(hook) = hook {
            hook.writing()?;
        }
        crop::crop(content, out, keep)
    });
    let outcome = match written {
        Ok((meta, count)) => Ok((meta, count.lines)),
        Err(CropError::BadCrop) => Err(RejectReason::BadCrop),
        Err(CropError::Io(e)) if no_space(&e) => {
            tracing::warn!(blob = %blob.0, error = %e, "no room to crop a blob");
            Err(RejectReason::DiskLow)
        }
        Err(CropError::Io(e)) => return Err(e),
    };
    Ok(Prepared {
        blob,
        owns: outcome.is_ok(),
        outcome,
        blobs,
        logged: false,
    })
}

/// Whether `e` says the file system has no room left for the daemon.
fn no_space(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
    }
    #[cfg(not(unix))]
    {
        e.kind() == io::ErrorKind::StorageFull
    }
}

impl Prepared {
    /// Inside the caller's transaction: the refusal its preparation came
    /// to, or records the new blob and deletes the old blob's row,
    /// returning the new blob and the old one's line count. The new blob
    /// stays its own until [`Prepared::kept`].
    pub(crate) fn log(
        &mut self,
        tx: &Connection,
    ) -> Result<std::result::Result<(BlobRef, u32), RejectReason>> {
        let (meta, lines) = match &self.outcome {
            Ok(cropped) => cropped,
            Err(reason) => return Ok(Err(*reason)),
        };
        self.blobs.record(tx, meta)?;
        event_log::delete_blob_row_in(tx, self.blob)?;
        self.logged = true;
        Ok(Ok((meta.blob_ref(), *lines)))
    }

    /// Called once the transaction it was logged in is committed: from now
    /// on the new blob is the log's, and the old blob's file goes. A crash
    /// before this leaves the old file without a row, which the startup
    /// sweep removes. One never logged still removes its new file.
    pub(crate) fn kept(mut self) {
        if self.logged {
            self.owns = false;
            self.blobs.discard(self.blob);
        }
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let (true, Ok((meta, _))) = (self.owns, &self.outcome) {
            self.blobs.discard(meta.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path());
        (dir, blobs)
    }

    fn files(blobs: &tempfile::TempDir) -> usize {
        std::fs::read_dir(blobs.path()).unwrap().count()
    }

    struct Full;
    impl CropHook for Full {
        fn writing(&self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
    }

    struct Broken;
    impl CropHook for Broken {
        fn writing(&self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    }

    const KEEP: &[LineSpan] = &[LineSpan { first: 1, count: 1 }];

    #[test]
    fn a_crop_dropped_unlogged_removes_its_file() {
        let (dir, blobs) = store();
        let prepared = prepare(&b"a\nb\nc\n"[..], blobs, BlobId::new(), KEEP, None).unwrap();
        assert!(matches!(&prepared.outcome, Ok((meta, 3)) if meta.size == 2));
        assert_eq!(files(&dir), 1);
        drop(prepared);
        assert_eq!(files(&dir), 0);
    }

    #[test]
    fn no_room_is_disk_low_and_another_failure_an_error() {
        let (dir, blobs) = store();
        let full = prepare(
            &b"a\nb\n"[..],
            blobs.clone(),
            BlobId::new(),
            KEEP,
            Some(&Full),
        );
        assert!(matches!(full.unwrap().outcome, Err(RejectReason::DiskLow)));
        let broken = prepare(&b"a\nb\n"[..], blobs, BlobId::new(), KEEP, Some(&Broken));
        assert_eq!(broken.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(files(&dir), 0, "no temp file left");
    }

    #[test]
    fn spans_past_the_end_leave_no_file() {
        let (dir, blobs) = store();
        let keep = [LineSpan { first: 5, count: 1 }];
        let prepared = prepare(&b"a\nb\n"[..], blobs, BlobId::new(), &keep, None).unwrap();
        assert!(matches!(prepared.outcome, Err(RejectReason::BadCrop)));
        assert_eq!(files(&dir), 0);
    }
}
