use std::path::PathBuf;

/// Errors from the daemon's storage and configuration layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("config {}: {source}", path.display())]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("no config directory: neither XDG_CONFIG_HOME nor HOME is set")]
    NoConfigDir,
    #[error("no data directory: set `data_dir` in the config, XDG_DATA_HOME or HOME")]
    NoDataDir,
    #[error("database schema version {found} is newer than this skywaved supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("blob {id}: stored size {expected}, file has {actual} bytes")]
    BlobSizeMismatch {
        id: String,
        expected: u64,
        actual: u64,
    },
    #[error("stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stored value: {0}")]
    Corrupt(String),
    /// A caller tried to append an event the log does not accept from it,
    /// e.g. the supervisor appending a command-owned event kind.
    #[error("cannot append event: {0}")]
    InvalidAppend(String),
    #[error("logging: {0}")]
    Logging(String),
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("{}: {reason}", path.display())]
    BadIdentity { path: PathBuf, reason: String },
    #[error("invalid server name: {0}")]
    InvalidServerName(String),
    #[error("invalid certificate signing request: {0}")]
    InvalidCsr(#[source] rcgen::Error),
    #[error("invalid fingerprint {0:?}: expected 32 hex bytes")]
    InvalidFingerprint(String),
    #[error("invalid device id {0:?}: expected a UUID")]
    InvalidDeviceId(String),
    #[error("no device with id {}", .0.0)]
    UnknownDevice(skywave_proto::DeviceId),
    #[error("system random number generator: {0}")]
    Random(getrandom::Error),
    #[error("QR code: {0}")]
    Qr(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Attaches a human-readable context to an I/O error.
pub(crate) trait IoContext<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}
