//! The device's pairing identity: device key, signed certificate and the
//! pinned server CA (spec §6, D9).
//!
//! The rest of the core sees only the [`Identity`] trait, so a
//! Keystore-backed signer can replace the file-backed key later without
//! touching transport or sync code. Generating the keypair and CSR is the
//! pairing client's job; this module only stores and loads the result.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use skywave_proto::DeviceId;
use uuid::Uuid;

/// A paired device's credentials.
pub trait Identity: Send + Sync {
    /// The id the server assigned to this device at pairing.
    fn device_id(&self) -> DeviceId;
    /// PEM-encoded device certificate signed by the server's CA.
    fn device_cert_pem(&self) -> &str;
    /// PEM-encoded CA certificate pinned at pairing.
    fn ca_cert_pem(&self) -> &str;
    /// How the device proves possession of its key.
    fn device_key(&self) -> DeviceKey<'_>;
}

/// Where the device's private key lives.
///
/// Non-exhaustive: a Keystore-backed signer whose key never leaves secure
/// hardware becomes another variant (D9, deferred).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum DeviceKey<'a> {
    /// PKCS#8 PEM held in app-private storage (the MVP).
    Pem(&'a str),
}

/// Credentials returned by pairing, ready to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedCredentials {
    pub device_id: DeviceId,
    pub device_key_pem: String,
    pub device_cert_pem: String,
    pub ca_cert_pem: String,
}

/// An identity stored as files in `<data_dir>/identity/`.
#[derive(Debug, Clone, PartialEq)]
pub struct FileIdentity {
    credentials: PairedCredentials,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("identity file {0} is missing; the identity directory is incomplete")]
    Incomplete(PathBuf),
    #[error("identity file {path} holds an invalid device id: {source}")]
    BadDeviceId { path: PathBuf, source: uuid::Error },
}

const DIR: &str = "identity";
const STAGING_DIR: &str = "identity.new";
const DEVICE_ID: &str = "device_id";
const DEVICE_KEY: &str = "device_key.pem";
const DEVICE_CERT: &str = "device_cert.pem";
const CA_CERT: &str = "ca_cert.pem";

impl FileIdentity {
    /// Loads the identity from `data_dir`, or `None` if the device was never
    /// paired.
    pub fn load(data_dir: &Path) -> Result<Option<Self>, IdentityError> {
        let dir = data_dir.join(DIR);
        if !dir.exists() {
            return Ok(None);
        }
        let read = |name: &str| -> Result<String, IdentityError> {
            let path = dir.join(name);
            fs::read_to_string(&path).map_err(|source| match source.kind() {
                io::ErrorKind::NotFound => IdentityError::Incomplete(path),
                _ => IdentityError::Io { path, source },
            })
        };
        let id_text = read(DEVICE_ID)?;
        let device_id = Uuid::parse_str(id_text.trim())
            .map(DeviceId)
            .map_err(|source| IdentityError::BadDeviceId {
                path: dir.join(DEVICE_ID),
                source,
            })?;
        Ok(Some(FileIdentity {
            credentials: PairedCredentials {
                device_id,
                device_key_pem: read(DEVICE_KEY)?,
                device_cert_pem: read(DEVICE_CERT)?,
                ca_cert_pem: read(CA_CERT)?,
            },
        }))
    }

    /// Persists `credentials` in `data_dir`, replacing any earlier identity
    /// (re-pairing). Files are written to a staging directory and renamed
    /// into place, so a crash never leaves a half-written identity.
    pub fn save(data_dir: &Path, credentials: PairedCredentials) -> Result<Self, IdentityError> {
        let io_err = |path: &Path| {
            let path = path.to_path_buf();
            move |source| IdentityError::Io { path, source }
        };
        let staging = data_dir.join(STAGING_DIR);
        let dir = data_dir.join(DIR);
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(io_err(&staging))?;
        }
        fs::create_dir_all(&staging).map_err(io_err(&staging))?;
        let files = [
            (DEVICE_ID, credentials.device_id.0.to_string()),
            (DEVICE_KEY, credentials.device_key_pem.clone()),
            (DEVICE_CERT, credentials.device_cert_pem.clone()),
            (CA_CERT, credentials.ca_cert_pem.clone()),
        ];
        for (name, contents) in files {
            let path = staging.join(name);
            write_private(&path, contents.as_bytes()).map_err(io_err(&path))?;
        }
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(io_err(&dir))?;
        }
        fs::rename(&staging, &dir).map_err(io_err(&dir))?;
        Ok(FileIdentity { credentials })
    }
}

/// Writes a file readable only by its owner where the platform allows it.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

impl Identity for FileIdentity {
    fn device_id(&self) -> DeviceId {
        self.credentials.device_id
    }

    fn device_cert_pem(&self) -> &str {
        &self.credentials.device_cert_pem
    }

    fn ca_cert_pem(&self) -> &str {
        &self.credentials.ca_cert_pem
    }

    fn device_key(&self) -> DeviceKey<'_> {
        DeviceKey::Pem(&self.credentials.device_key_pem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(tag: &str) -> PairedCredentials {
        PairedCredentials {
            device_id: DeviceId::new(),
            device_key_pem: format!("KEY {tag}"),
            device_cert_pem: format!("CERT {tag}"),
            ca_cert_pem: format!("CA {tag}"),
        }
    }

    #[test]
    fn unpaired_device_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(FileIdentity::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn save_then_load_round_trips_and_repair_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let first = FileIdentity::save(dir.path(), credentials("a")).unwrap();
        assert_eq!(
            FileIdentity::load(dir.path()).unwrap().as_ref(),
            Some(&first)
        );

        let second = FileIdentity::save(dir.path(), credentials("b")).unwrap();
        let loaded = FileIdentity::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, second);
        assert_eq!(loaded.device_cert_pem(), "CERT b");
        assert!(matches!(loaded.device_key(), DeviceKey::Pem("KEY b")));
        assert!(!dir.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn missing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        FileIdentity::save(dir.path(), credentials("a")).unwrap();
        fs::remove_file(dir.path().join(DIR).join(CA_CERT)).unwrap();
        let err = FileIdentity::load(dir.path()).unwrap_err();
        assert!(matches!(err, IdentityError::Incomplete(_)), "{err}");
    }
}
