//! The device's pairing: its identity — device key, signed certificate and
//! the pinned server CA — together with the addresses of the server that
//! identity pairs it with (architecture.md "Pairing", D9).
//!
//! The rest of the core sees the credentials only through the [`Identity`]
//! trait, so a Keystore-backed signer can replace the file-backed key later
//! without touching transport or sync code. Generating the keypair and CSR
//! is the pairing client's job; this module only stores and loads the
//! result.

use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use skywave_proto::{DeviceId, Fingerprint};

use crate::address::{ServerAddress, ServerAddresses};
use crate::client::CoreError;
use crate::store::Store;
use crate::tls;

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

impl PairedCredentials {
    /// Whether `other` pins the same server: the first certificates of the
    /// two CA PEMs have the same [`Fingerprint`]. A CA that holds no
    /// readable certificate is compared by its text instead.
    pub fn same_server(&self, other: &PairedCredentials) -> bool {
        match (
            tls::first_cert_der(&self.ca_cert_pem),
            tls::first_cert_der(&other.ca_cert_pem),
        ) {
            (Some(a), Some(b)) => Fingerprint::of_der(&a) == Fingerprint::of_der(&b),
            (None, None) => self.ca_cert_pem.trim() == other.ca_cert_pem.trim(),
            _ => false,
        }
    }
}

impl Identity for PairedCredentials {
    fn device_id(&self) -> DeviceId {
        self.device_id
    }

    fn device_cert_pem(&self) -> &str {
        &self.device_cert_pem
    }

    fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    fn device_key(&self) -> DeviceKey<'_> {
        DeviceKey::Pem(&self.device_key_pem)
    }
}

/// The device's pairing: who it is to the server, and where that server
/// is. One record in `<data_dir>/pairing.json`, replaced whole
/// ([`Pairing::save`]), so a device is paired — credentials and an address
/// together — or not at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Pairing {
    pub credentials: PairedCredentials,
    pub addresses: ServerAddresses,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("pairing I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("pairing file {path} cannot be read: {source}")]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("identity file {0} is missing; the identity directory is incomplete")]
    Incomplete(PathBuf),
    #[error("identity file {path} holds an invalid device id: {source}")]
    BadDeviceId { path: PathBuf, source: uuid::Error },
}

const FILE: &str = "pairing.json";
/// Where [`Pairing::save`] writes before renaming over [`FILE`].
pub(crate) const TEMP_FILE: &str = "pairing.json.new";

/// [`Pairing`] as stored: canonical addresses, the current one never among
/// the others.
#[derive(Serialize, Deserialize)]
struct Record<'a> {
    device_id: DeviceId,
    device_key_pem: Cow<'a, str>,
    device_cert_pem: Cow<'a, str>,
    ca_cert_pem: Cow<'a, str>,
    address: Cow<'a, ServerAddress>,
    other_addresses: Cow<'a, [ServerAddress]>,
}

impl Pairing {
    /// A pairing whose server is known by `address` alone.
    pub fn new(credentials: PairedCredentials, address: ServerAddress) -> Pairing {
        Pairing {
            credentials,
            addresses: ServerAddresses::new(address),
        }
    }

    /// Loads the pairing from `data_dir`, or `None` if the device is not
    /// paired. A temporary file a crashed [`save`](Pairing::save) left is
    /// ignored: the record is the last save that completed.
    pub fn load(data_dir: &Path) -> Result<Option<Pairing>, IdentityError> {
        let path = data_dir.join(FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(IdentityError::Io { path, source }),
        };
        let record: Record<'_> = serde_json::from_slice(&bytes)
            .map_err(|source| IdentityError::Malformed { path, source })?;
        Ok(Some(Pairing {
            credentials: PairedCredentials {
                device_id: record.device_id,
                device_key_pem: record.device_key_pem.into_owned(),
                device_cert_pem: record.device_cert_pem.into_owned(),
                ca_cert_pem: record.ca_cert_pem.into_owned(),
            },
            addresses: ServerAddresses::with_others(
                record.address.into_owned(),
                record.other_addresses.into_owned(),
            ),
        }))
    }

    /// Stores this as the device's pairing in `data_dir`, replacing any
    /// earlier one whole: written to a temporary file readable only by its
    /// owner, flushed, renamed over the record and the rename flushed. A
    /// crash at any point leaves the old pairing or this one, never a mix
    /// of the two, nor none where there was one.
    pub(crate) fn save(&self, data_dir: &Path) -> Result<(), IdentityError> {
        let io_err = |path: &Path| {
            let path = path.to_path_buf();
            move |source| IdentityError::Io { path, source }
        };
        let record = Record {
            device_id: self.credentials.device_id,
            device_key_pem: Cow::Borrowed(&self.credentials.device_key_pem),
            device_cert_pem: Cow::Borrowed(&self.credentials.device_cert_pem),
            ca_cert_pem: Cow::Borrowed(&self.credentials.ca_cert_pem),
            address: Cow::Borrowed(self.addresses.current()),
            other_addresses: Cow::Borrowed(self.addresses.others()),
        };
        let json = serde_json::to_vec_pretty(&record).expect("a pairing serializes");
        let temp = data_dir.join(TEMP_FILE);
        // What a crashed save left, perhaps half written.
        match fs::remove_file(&temp) {
            Err(source) if source.kind() != io::ErrorKind::NotFound => {
                return Err(io_err(&temp)(source));
            }
            _ => {}
        }
        write_private(&temp, &json).map_err(io_err(&temp))?;
        let path = data_dir.join(FILE);
        fs::rename(&temp, &path).map_err(io_err(&path))?;
        sync_dir(data_dir).map_err(io_err(data_dir))
    }

    /// The pairing [`Core::open`](crate::Core::open) finds in `data_dir`:
    /// the record, or — in a data dir an older build wrote — its identity
    /// directory and the server addresses it kept among `store`'s settings,
    /// moved into one record. An identity without an address that parses
    /// (one schema 29 dropped) leaves the device unpaired, to pair again:
    /// with no server to reach, there is nothing it could connect to. What
    /// the older build kept goes once the record holds it; a crash before
    /// that redoes the move, or finds the record and only clears up.
    pub(crate) fn open(data_dir: &Path, store: &Store) -> Result<Option<Pairing>, CoreError> {
        let pairing = match Pairing::load(data_dir)? {
            Some(pairing) => Some(pairing),
            None => match legacy::credentials(data_dir)? {
                None => None,
                Some(credentials) => match store.legacy_server_addresses()? {
                    Some(addresses) => {
                        let pairing = Pairing {
                            credentials,
                            addresses,
                        };
                        pairing.save(data_dir)?;
                        Some(pairing)
                    }
                    None => {
                        tracing::warn!(
                            "the stored identity has no server address; the device must pair again"
                        );
                        None
                    }
                },
            },
        };
        store.drop_legacy_server_addresses()?;
        legacy::remove(data_dir)?;
        Ok(pairing)
    }
}

impl Identity for Pairing {
    fn device_id(&self) -> DeviceId {
        self.credentials.device_id()
    }

    fn device_cert_pem(&self) -> &str {
        self.credentials.device_cert_pem()
    }

    fn ca_cert_pem(&self) -> &str {
        self.credentials.ca_cert_pem()
    }

    fn device_key(&self) -> DeviceKey<'_> {
        self.credentials.device_key()
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

/// Makes a rename in `dir` durable, where the platform allows it.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The identity directory an older build kept: `identity/`, one file per
/// credential, replaced by writing `identity.new/`, removing `identity/`
/// and renaming `identity.new/` into its place.
mod legacy {
    use std::fs;
    use std::io;
    use std::path::Path;

    use skywave_proto::DeviceId;
    use uuid::Uuid;

    use super::{IdentityError, PairedCredentials};

    pub(super) const DIR: &str = "identity";
    pub(super) const STAGING_DIR: &str = "identity.new";
    pub(super) const DEVICE_ID: &str = "device_id";
    pub(super) const DEVICE_KEY: &str = "device_key.pem";
    pub(super) const DEVICE_CERT: &str = "device_cert.pem";
    pub(super) const CA_CERT: &str = "ca_cert.pem";

    /// The credentials in `identity/`; without it, those in a complete
    /// `identity.new/`, which a crash between removing the old directory
    /// and renaming the new one left. An incomplete `identity.new/` alone
    /// is a first pairing a crash interrupted: the device never paired.
    pub(super) fn credentials(data_dir: &Path) -> Result<Option<PairedCredentials>, IdentityError> {
        let dir = data_dir.join(DIR);
        if dir.exists() {
            return read(&dir).map(Some);
        }
        let staging = data_dir.join(STAGING_DIR);
        if !staging.exists() {
            return Ok(None);
        }
        match read(&staging) {
            Ok(credentials) => Ok(Some(credentials)),
            Err(IdentityError::Incomplete(_) | IdentityError::BadDeviceId { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read(dir: &Path) -> Result<PairedCredentials, IdentityError> {
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
        Ok(PairedCredentials {
            device_id,
            device_key_pem: read(DEVICE_KEY)?,
            device_cert_pem: read(DEVICE_CERT)?,
            ca_cert_pem: read(CA_CERT)?,
        })
    }

    /// Removes both directories, wherever they are.
    pub(super) fn remove(data_dir: &Path) -> Result<(), IdentityError> {
        for name in [STAGING_DIR, DIR] {
            let path = data_dir.join(name);
            match fs::remove_dir_all(&path) {
                Err(source) if source.kind() != io::ErrorKind::NotFound => {
                    return Err(IdentityError::Io { path, source });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Writes `credentials` to `dir_name` as the older build did.
    #[cfg(test)]
    pub(crate) fn write(data_dir: &Path, dir_name: &str, credentials: &PairedCredentials) {
        let dir = data_dir.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        for (name, contents) in [
            (DEVICE_ID, credentials.device_id.0.to_string()),
            (DEVICE_KEY, credentials.device_key_pem.clone()),
            (DEVICE_CERT, credentials.device_cert_pem.clone()),
            (CA_CERT, credentials.ca_cert_pem.clone()),
        ] {
            fs::write(dir.join(name), contents).unwrap();
        }
    }
}

/// Writes `credentials` as an older build's `identity/` directory, for
/// tests of the move into the pairing record.
#[cfg(test)]
pub(crate) fn write_legacy_identity(data_dir: &Path, credentials: &PairedCredentials) {
    legacy::write(data_dir, legacy::DIR, credentials);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{DB_FILE, SCHEMA_VERSION};

    fn credentials(tag: &str) -> PairedCredentials {
        PairedCredentials {
            device_id: DeviceId::new(),
            device_key_pem: format!("KEY {tag}"),
            device_cert_pem: format!("CERT {tag}"),
            ca_cert_pem: format!("CA {tag}"),
        }
    }

    fn addr(text: &str) -> ServerAddress {
        ServerAddress::parse(text).unwrap()
    }

    /// A data dir as the build before the pairing record left it: a
    /// schema 29 replica whose settings hold `rows`.
    fn older_build(dir: &Path, rows: &[(&str, &str)]) {
        drop(Store::open(dir).unwrap());
        let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                [key, value],
            )
            .unwrap();
        }
    }

    fn legacy_rows(dir: &Path) -> i64 {
        let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM settings
             WHERE key IN ('server_address', 'other_server_addresses')",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn open(dir: &Path) -> Option<Pairing> {
        let store = Store::open(dir).unwrap();
        Pairing::open(dir, &store).unwrap()
    }

    #[test]
    fn an_unpaired_device_has_no_pairing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Pairing::load(dir.path()).unwrap(), None);
        assert_eq!(open(dir.path()), None);
    }

    #[test]
    fn save_then_load_round_trips_and_a_save_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Pairing::new(credentials("a"), addr("home"));
        first.addresses.switch_to(&addr("vpn"));
        first.save(dir.path()).unwrap();
        assert_eq!(Pairing::load(dir.path()).unwrap().as_ref(), Some(&first));

        let second = Pairing::new(credentials("b"), addr("other"));
        second.save(dir.path()).unwrap();
        let loaded = Pairing::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, second);
        assert_eq!(loaded.device_cert_pem(), "CERT b");
        assert!(matches!(loaded.device_key(), DeviceKey::Pem("KEY b")));
        assert!(!dir.path().join(TEMP_FILE).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// A save interrupted before its rename leaves its temporary file: the
    /// pairing is the one before, and the next save goes through.
    #[test]
    fn a_save_interrupted_before_the_rename_keeps_the_old_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let old = Pairing::new(credentials("a"), addr("home"));
        old.save(dir.path()).unwrap();
        fs::write(dir.path().join(TEMP_FILE), r#"{"device_id":"#).unwrap();
        assert_eq!(Pairing::load(dir.path()).unwrap().as_ref(), Some(&old));
        assert_eq!(open(dir.path()).as_ref(), Some(&old));

        let new = Pairing::new(credentials("b"), addr("other"));
        new.save(dir.path()).unwrap();
        assert_eq!(Pairing::load(dir.path()).unwrap(), Some(new));
    }

    /// The only temporary file and no record: the first pairing's save was
    /// interrupted, so the device never paired.
    #[test]
    fn an_interrupted_first_save_leaves_the_device_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(TEMP_FILE), "{").unwrap();
        assert_eq!(Pairing::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_damaged_record_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), "{").unwrap();
        let err = Pairing::load(dir.path()).unwrap_err();
        assert!(matches!(err, IdentityError::Malformed { .. }), "{err}");
    }

    /// An older build's identity directory and stored addresses become
    /// one record, and the old copies go.
    #[test]
    fn an_older_builds_identity_and_addresses_move_into_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let creds = credentials("a");
        older_build(
            dir.path(),
            &[
                ("server_address", "home:7443"),
                ("other_server_addresses", r#"["vpn:7443","public:7443"]"#),
            ],
        );
        legacy::write(dir.path(), legacy::DIR, &creds);

        let pairing = open(dir.path()).unwrap();
        assert_eq!(pairing.credentials, creds);
        assert_eq!(pairing.addresses.current(), &addr("home"));
        assert_eq!(pairing.addresses.others(), [addr("vpn"), addr("public")]);
        assert_eq!(Pairing::load(dir.path()).unwrap().as_ref(), Some(&pairing));
        assert!(!dir.path().join(legacy::DIR).exists());
        assert_eq!(legacy_rows(dir.path()), 0);
        // Opening again finds the record.
        assert_eq!(open(dir.path()), Some(pairing));
    }

    /// A schema 28 replica: its address is made canonical first, then
    /// moves.
    #[test]
    fn an_address_from_before_schema_29_moves_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let creds = credentials("a");
        older_build(dir.path(), &[("server_address", " MyHost ")]);
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
            .unwrap();
        drop(conn);
        legacy::write(dir.path(), legacy::DIR, &creds);
        let pairing = open(dir.path()).unwrap();
        assert_eq!(pairing.addresses, ServerAddresses::new(addr("myhost:7443")));
    }

    /// The older build's save crashed after removing `identity/` and
    /// before renaming `identity.new/`: the new identity is recovered.
    #[test]
    fn a_complete_staged_identity_alone_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let creds = credentials("b");
        older_build(dir.path(), &[("server_address", "home:7443")]);
        legacy::write(dir.path(), legacy::STAGING_DIR, &creds);
        let pairing = open(dir.path()).unwrap();
        assert_eq!(pairing.credentials, creds);
        assert!(!dir.path().join(legacy::STAGING_DIR).exists());
    }

    /// The older build's save crashed while writing `identity.new/`:
    /// `identity/` still holds the identity in use.
    #[test]
    fn a_staged_identity_beside_the_old_one_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let old = credentials("a");
        older_build(dir.path(), &[("server_address", "home:7443")]);
        legacy::write(dir.path(), legacy::DIR, &old);
        legacy::write(dir.path(), legacy::STAGING_DIR, &credentials("b"));
        fs::remove_file(dir.path().join(legacy::STAGING_DIR).join(legacy::CA_CERT)).unwrap();
        assert_eq!(open(dir.path()).unwrap().credentials, old);
        assert!(!dir.path().join(legacy::STAGING_DIR).exists());
        assert!(!dir.path().join(legacy::DIR).exists());
    }

    /// A first pairing whose identity write crashed: never paired.
    #[test]
    fn an_incomplete_staged_identity_alone_is_no_pairing() {
        let dir = tempfile::tempdir().unwrap();
        older_build(dir.path(), &[("server_address", "home:7443")]);
        legacy::write(dir.path(), legacy::STAGING_DIR, &credentials("a"));
        fs::remove_file(
            dir.path()
                .join(legacy::STAGING_DIR)
                .join(legacy::DEVICE_CERT),
        )
        .unwrap();
        assert_eq!(open(dir.path()), None);
        assert!(!dir.path().join(legacy::STAGING_DIR).exists());
        assert_eq!(legacy_rows(dir.path()), 0);
    }

    /// An identity whose address schema 29 dropped (or that never had
    /// one) has no server to reach: the device pairs again.
    #[test]
    fn an_identity_without_an_address_is_no_pairing() {
        let dir = tempfile::tempdir().unwrap();
        older_build(dir.path(), &[("other_server_addresses", r#"["vpn:7443"]"#)]);
        legacy::write(dir.path(), legacy::DIR, &credentials("a"));
        assert_eq!(open(dir.path()), None);
        assert!(!dir.path().join(legacy::DIR).exists());
        assert_eq!(legacy_rows(dir.path()), 0);
        assert_eq!(open(dir.path()), None);
    }

    /// A crash after the record was written, before the old copies went:
    /// the record wins and they go.
    #[test]
    fn a_move_interrupted_after_the_record_only_clears_up() {
        let dir = tempfile::tempdir().unwrap();
        older_build(dir.path(), &[("server_address", "old:7443")]);
        legacy::write(dir.path(), legacy::DIR, &credentials("old"));
        let pairing = Pairing::new(credentials("new"), addr("new"));
        pairing.save(dir.path()).unwrap();
        assert_eq!(open(dir.path()), Some(pairing));
        assert!(!dir.path().join(legacy::DIR).exists());
        assert_eq!(legacy_rows(dir.path()), 0);
    }

    #[test]
    fn a_damaged_legacy_identity_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        older_build(dir.path(), &[("server_address", "home:7443")]);
        legacy::write(dir.path(), legacy::DIR, &credentials("a"));
        fs::remove_file(dir.path().join(legacy::DIR).join(legacy::CA_CERT)).unwrap();
        let store = Store::open(dir.path()).unwrap();
        let err = Pairing::open(dir.path(), &store).unwrap_err();
        assert!(
            matches!(err, CoreError::Identity(IdentityError::Incomplete(_))),
            "{err}"
        );
    }
}
