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
    /// `agent.args` would turn bypass on behind `agent.allow_bypass` (D38).
    #[error(
        "config {}: `agent.args` has `{arg}`, which allows bypass mode; \
         set `[agent] allow_bypass = true` instead",
        path.display()
    )]
    BypassInAgentArgs { path: PathBuf, arg: String },
    #[error("no config directory: neither XDG_CONFIG_HOME nor HOME is set")]
    NoConfigDir,
    #[error("no data directory: set `data_dir` in the config, XDG_DATA_HOME or HOME")]
    NoDataDir,
    /// Another `skywaved serve` holds the data dir's serve lock.
    #[error("another skywaved serve is running on data directory {}", path.display())]
    DataDirInUse { path: PathBuf },
    #[error("database schema version {found} is newer than this skywaved supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    /// The database was written by a protocol version this build cannot
    /// migrate (older than `MIGRATED_SINCE`, or newer than its own); its
    /// stored wire JSON may not parse.
    #[error(
        "database {} was written by protocol version {found}, this skywaved \
         migrates versions {} to {current}; run a skywaved that reads it, or stop \
         skywaved, delete its data directory and pair devices again",
        path.display(),
        skywave_proto::MIGRATED_SINCE
    )]
    ProtocolMismatch {
        path: PathBuf,
        found: u32,
        current: u32,
    },
    /// `PRAGMA quick_check` found the database file damaged.
    #[error(
        "database {} is damaged ({problems}); stop skywaved and restore the data \
         directory from a backup",
        path.display()
    )]
    DamagedDatabase { path: PathBuf, problems: String },
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
    /// A command handed without the preparation it needs
    /// (`EventLog::handle_with`), e.g. a `CropBlob` without its crop.
    #[error("{0} needs its preparation")]
    Unprepared(&'static str),
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
    #[error("invalid device id {0:?}: expected a UUID")]
    InvalidDeviceId(String),
    #[error("no device with id {}", .0.0)]
    UnknownDevice(skywave_proto::DeviceId),
    #[error("system random number generator: {0}")]
    Random(getrandom::Error),
    #[error("TLS setup: {0}")]
    Tls(String),
    #[error("QR code: {0}")]
    Qr(String),
    #[error("no session context with id {}", .0.0)]
    UnknownContext(skywave_proto::ContextId),
    #[error("no session with id {}", .0.0)]
    UnknownSession(skywave_proto::SessionId),
    #[error("agent supervisor: {0}")]
    Supervisor(String),
    #[error("shell runner: {0}")]
    Shell(String),
    /// No `serve` listens on the data dir's control socket.
    #[error("no skywaved serve is running on {}", path.display())]
    NotServing { path: PathBuf },
    #[error("control socket: {0}")]
    Control(String),
    /// A workdir snapshot reached its request's deadline (`crate::workdir`);
    /// the API answers 503.
    #[error("workdir snapshot passed its deadline")]
    WorkdirDeadline,
    /// A storage report passed its deadline, waiting or reading; the API
    /// answers 503.
    #[error("storage report passed its deadline")]
    StorageDeadline,
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
