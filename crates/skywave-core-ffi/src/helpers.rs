//! Pure helpers a head needs beside the core object: reading a pairing QR
//! code, previewing a downloaded blob, and the device name to fall back on.
//! Each one delegates to `skywave-core`; none carries logic of its own.

use std::path::Path;

use skywave_core as sc;

use crate::error::SkywaveError;

/// What a pairing QR code holds, in the form `Action.Pair` takes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairUri {
    /// `host:port`.
    pub address: String,
    /// 10 symbols without `-`.
    pub code: String,
    /// Colon-separated uppercase pairs, as `skywaved pair` prints it.
    pub fingerprint: String,
}

/// Parses `skywave://pair?addr=<host:port>&code=<code>&fp=<fingerprint>`,
/// the content of a pairing QR code.
#[uniffi::export]
pub fn parse_pair_uri(uri: String) -> Result<PairUri, SkywaveError> {
    let sc::PairUri {
        address,
        code,
        fingerprint,
    } = sc::parse_pair_uri(&uri).map_err(|e| SkywaveError::InvalidPairUri {
        reason: e.to_string(),
    })?;
    Ok(PairUri {
        address,
        code,
        fingerprint,
    })
}

/// The start of a downloaded blob's file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BlobHead {
    /// The first bytes as text, invalid UTF-8 replaced.
    pub text: String,
    /// The whole file's size in bytes.
    pub total_len: u64,
}

/// Reads at most `max` bytes of the blob file at `path` (a
/// `BlobState.Available` path), never the whole of a huge one.
#[uniffi::export]
pub fn read_head(path: String, max: u64) -> Result<BlobHead, SkywaveError> {
    let (text, total_len) = sc::read_head(Path::new(&path), max).map_err(|e| SkywaveError::Io {
        reason: format!("cannot read {path}: {e}"),
    })?;
    Ok(BlobHead { text, total_len })
}

/// The core's own guess at a device name (the host name). On Android pass
/// a better one to `SkywaveCore.open` (D17).
#[uniffi::export]
pub fn default_device_name() -> String {
    sc::default_device_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89";

    #[test]
    fn parses_a_pairing_link_as_the_core_does() {
        let uri = format!("skywave://pair?addr=example.org:7443&code=abcde-12345&fp={FP}");
        let core = sc::parse_pair_uri(&uri).unwrap();
        assert_eq!(
            parse_pair_uri(uri).unwrap(),
            PairUri {
                address: core.address,
                code: core.code,
                fingerprint: core.fingerprint,
            }
        );
    }

    #[test]
    fn a_bad_pairing_link_is_a_typed_error() {
        for uri in [
            "",
            "https://pair?addr=a:1&code=ABCDE12345",
            "skywave://pair?code=ABCDE12345",
        ] {
            let err = parse_pair_uri(uri.into()).unwrap_err();
            assert!(
                matches!(&err, SkywaveError::InvalidPairUri { reason } if !reason.is_empty()),
                "{uri}: {err:?}"
            );
        }
    }

    #[test]
    fn reads_the_head_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, "hello world").unwrap();
        let path = path.display().to_string();
        assert_eq!(
            read_head(path.clone(), 5).unwrap(),
            BlobHead {
                text: "hello".into(),
                total_len: 11
            }
        );
        assert_eq!(read_head(path, 100).unwrap().text, "hello world");
        let missing = dir.path().join("missing").display().to_string();
        assert!(matches!(
            read_head(missing, 5),
            Err(SkywaveError::Io { .. })
        ));
    }

    #[test]
    fn default_device_name_is_the_cores() {
        assert_eq!(default_device_name(), sc::default_device_name());
        assert!(!default_device_name().is_empty());
    }
}
