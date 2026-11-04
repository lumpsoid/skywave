//! Pairing (architecture.md "Pairing", D9): `skywaved pair` issues a one-time code, and
//! `POST /v1/pair` trades a valid code plus a CSR for a device certificate.
//!
//! **Codes** are 10 symbols of Crockford base32 (50 bits from the OS CSPRNG),
//! shown as `XXXXX-XXXXX`. Input is case-insensitive, ignores `-` and
//! spaces, and reads `O` as `0` and `I`/`L` as `1`. Only the SHA-256 of the
//! canonical code is stored (`pair_codes`). A code is redeemed by a single
//! `UPDATE … WHERE hash = ? AND used = 0 AND expires_at > now`, so unknown,
//! used and expired codes take the same path and look the same to the
//! caller. The update, the device insert and the certificate signing share
//! one transaction: a bad CSR rolls back and leaves the code usable.
//!
//! **Rate limit:** sliding windows over failed code attempts ([`RateLimit`]),
//! kept in memory: one per source (default 5 per minute) and a global
//! backstop over every remote source (default 100 per minute). Every attempt
//! reserves a slot in its windows before the code is checked and gives it
//! back if the code was right, so parallel guesses can't overrun the limit.
//! While a window is full every attempt it covers, right or wrong, gets 429
//! without the code being looked at. A source is the peer's IP, an IPv6
//! address cut to its /64 (one host usually owns the whole prefix), and all
//! of loopback as one source. Loopback is outside the global window, so no
//! remote flood can lock out pairing from the server itself. The global
//! window keeps distributed guessing hopeless — 144 000 guesses a day against
//! 2^50 codes — and bounds the per-source map: an entry lives only while it
//! holds a slot, and every remote slot is also a global one.
//!
//! A full global window refuses the right code from a remote address too, so
//! a few dozen attacking addresses could keep remote pairing blocked. Issuing
//! a code clears every window: the owner gets unstuck with `skywaved pair`.
//! That may run as another process, so the server learns of the code from
//! the database: each attempt reads which code was issued last
//! (`newest_code`) and clears the windows when it changed.
//!
//! **HTTP** ([`router`]): the only route is `POST /v1/pair`; errors are bare
//! status codes (architecture.md "Protocol"): 400 malformed request or CSR, 401 wrong, used or
//! expired code, 429 rate limited with `Retry-After`, 500 server fault.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use skywave_proto::{DeviceId, Fingerprint, PairRequest, PairResponse, path};

use crate::db::Db;
use crate::devices;
use crate::error::{Error, Result};
use crate::identity::ServerIdentity;
use crate::lock;

/// `skywaved pair` default code lifetime (architecture.md "Pairing").
pub const DEFAULT_TTL: Duration = Duration::minutes(10);
/// Longest lifetime `--ttl` accepts.
pub const MAX_TTL: Duration = Duration::hours(24);
/// Longest accepted `device_name`, in characters.
pub const MAX_DEVICE_NAME: usize = 64;
/// Largest accepted `POST /v1/pair` body.
pub const MAX_BODY: usize = 64 * 1024;

/// Crockford base32: digits and letters without I, L, O, U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 10;

/// A pairing code in canonical form: [`CODE_LEN`] Crockford base32 symbols.
#[derive(Clone, PartialEq, Eq)]
pub struct PairCode(String);

impl PairCode {
    /// A fresh code from the OS CSPRNG.
    pub fn generate() -> Result<PairCode> {
        let mut bytes = [0u8; CODE_LEN];
        getrandom::fill(&mut bytes).map_err(Error::Random)?;
        // 256 is a multiple of 32, so masking keeps the symbols uniform.
        let code = bytes
            .iter()
            .map(|b| ALPHABET[usize::from(b & 31)] as char)
            .collect();
        Ok(PairCode(code))
    }

    /// Normalizes typed input; `None` if it can't be a code.
    pub fn parse(input: &str) -> Option<PairCode> {
        let mut code = String::with_capacity(CODE_LEN);
        for c in input.chars() {
            let c = match c.to_ascii_uppercase() {
                '-' | ' ' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                c if c.is_ascii() && ALPHABET.contains(&(c as u8)) => c,
                _ => return None,
            };
            if code.len() == CODE_LEN {
                return None;
            }
            code.push(c);
        }
        (code.len() == CODE_LEN).then_some(PairCode(code))
    }

    /// Canonical form without separators, as used in the pairing URI.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// What `pair_codes.hash` stores: lowercase hex SHA-256 of the
    /// canonical code.
    fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

/// `XXXXX-XXXXX`, for people to read and type.
impl fmt::Display for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (a, b) = self.0.split_at(CODE_LEN / 2);
        write!(f, "{a}-{b}")
    }
}

/// Never prints the code itself.
impl fmt::Debug for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairCode(..)")
    }
}

/// A code stored by [`issue_code`].
#[derive(Debug, Clone)]
pub struct IssuedCode {
    pub code: PairCode,
    pub expires_at: DateTime<Utc>,
}

/// Stores a fresh code valid until `now + ttl`, and deletes codes that are
/// used or expired.
pub fn issue_code(conn: &Connection, now: DateTime<Utc>, ttl: Duration) -> Result<IssuedCode> {
    let code = PairCode::generate()?;
    let expires_at = now + ttl;
    conn.execute(
        "DELETE FROM pair_codes WHERE used = 1 OR expires_at <= ?1",
        [timestamp(now)],
    )?;
    conn.execute(
        "INSERT INTO pair_codes (hash, expires_at) VALUES (?1, ?2)",
        params![code.hash(), timestamp(expires_at)],
    )?;
    Ok(IssuedCode { code, expires_at })
}

/// The hash of the code [`issue_code`] stored last, if any. A new code
/// always changes it: a row's rowid is one past the highest at insert, and
/// only a later issue deletes rows, so the newest code has the highest.
fn newest_code(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT hash FROM pair_codes ORDER BY rowid DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
}

/// Parses `--ttl`: a positive whole number with unit `s`, `m` or `h`
/// (`90s`, `10m`, `1h`), at most [`MAX_TTL`].
pub fn parse_ttl(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text.char_indices().last().map_or(0, |(i, _)| i);
    let (number, unit) = (&text[..split], &text[split..]);
    let n: i64 =
        number.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
            format!("{text:?}: expected a positive number with s, m or h, e.g. 10m")
        })?;
    let ttl = match unit {
        // Clamped first: chrono panics on durations beyond its range.
        "s" => Duration::seconds(n.min(MAX_TTL.num_seconds() + 1)),
        "m" => Duration::minutes(n.min(MAX_TTL.num_minutes() + 1)),
        "h" => Duration::hours(n.min(MAX_TTL.num_hours() + 1)),
        _ => return Err(format!("{text:?}: unit must be s, m or h, e.g. 10m")),
    };
    if ttl > MAX_TTL {
        return Err(format!("{text:?}: at most {}h", MAX_TTL.num_hours()));
    }
    Ok(ttl)
}

/// The address `skywaved pair` tells devices to connect to: the first
/// configured `server_names` entry, else a non-wildcard `listen` IP, else
/// the host name (`<host>.local` for a bare one, resolved via mDNS), else
/// `localhost`; always with the `listen` port. The server certificate covers
/// every one of these (see [`crate::identity::server_names`]).
pub fn pair_address(listen: SocketAddr, hostname: Option<&str>, extra: &[String]) -> String {
    let host = extra
        .iter()
        .map(|n| n.trim())
        .find(|n| !n.is_empty())
        .map(str::to_owned)
        .or_else(|| (!listen.ip().is_unspecified()).then(|| listen.ip().to_string()))
        .or_else(|| {
            hostname
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(|h| match h.contains('.') {
                    true => h.to_owned(),
                    false => format!("{h}.local"),
                })
        })
        .unwrap_or_else(|| "localhost".to_owned());
    match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, listen.port()).to_string(),
        Err(_) => format!("{host}:{}", listen.port()),
    }
}

/// `skywave://pair?addr=<host:port>&code=<code>&fp=<sha256 hex>`: what the
/// pairing QR code encodes (architecture.md "Pairing"). `code` is canonical (no `-`), `fp` is
/// 64 lowercase hex digits, values are percent-encoded (RFC 3986).
pub fn pair_uri(addr: &str, code: &PairCode, ca_fingerprint: Fingerprint) -> String {
    format!(
        "skywave://pair?addr={}&code={}&fp={}",
        percent_encode(addr),
        code.as_str(),
        ca_fingerprint.to_hex()
    )
}

/// `text` as a terminal QR code: two modules per character cell, with a
/// quiet zone. Dark modules are blank cells, so it reads correctly on a
/// dark-background terminal.
pub fn render_qr(text: &str) -> Result<String> {
    let qr = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L)
        .map_err(|e| Error::Qr(e.to_string()))?;
    Ok(qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn timestamp(t: DateTime<Utc>) -> String {
    // Fixed width, so SQL compares them as text in time order.
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Source of the current time, injectable for tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// At most `per_source` failed code attempts from one source, and
/// `global` from all remote sources together, in any `window` (module docs,
/// "Rate limit").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub per_source: usize,
    pub global: usize,
    pub window: Duration,
}

impl Default for RateLimit {
    fn default() -> RateLimit {
        RateLimit {
            per_source: 5,
            global: 100,
            window: Duration::minutes(1),
        }
    }
}

/// Where an attempt comes from, as far as the rate limit cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    /// Any loopback address: the server's own host.
    Loopback,
    /// An IPv4 address, or an IPv6 address with all but its /64 zeroed.
    Remote(IpAddr),
}

impl Source {
    fn of(ip: IpAddr) -> Source {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return Source::Loopback;
        }
        match ip {
            IpAddr::V4(_) => Source::Remote(ip),
            IpAddr::V6(v6) => {
                let prefix = u128::from(v6) & !((1u128 << 64) - 1);
                Source::Remote(IpAddr::V6(prefix.into()))
            }
        }
    }
}

/// One sliding window of attempt times.
#[derive(Debug, Default)]
struct Window(VecDeque<DateTime<Utc>>);

impl Window {
    /// Drops the attempts older than `window` before `now`.
    fn expire(&mut self, now: DateTime<Utc>, window: Duration) {
        // A clock that went back must not lock pairing out for the gap.
        for t in self.0.iter_mut() {
            *t = (*t).min(now);
        }
        while self.0.front().is_some_and(|t| *t + window <= now) {
            self.0.pop_front();
        }
    }

    /// How long until a slot frees up, if all `max` are taken.
    fn full(&self, max: usize, now: DateTime<Utc>, window: Duration) -> Option<Duration> {
        (self.0.len() >= max).then(|| match self.0.front() {
            Some(oldest) => *oldest + window - now,
            None => window,
        })
    }

    fn remove(&mut self, at: DateTime<Utc>) {
        if let Some(i) = self.0.iter().position(|t| *t == at) {
            self.0.remove(i);
        }
    }
}

/// A slot taken by [`Attempts::reserve`].
#[derive(Debug, Clone, Copy)]
struct Slot {
    source: Source,
    at: DateTime<Utc>,
    /// [`Attempts::cleared`] when the slot was taken.
    cleared: u64,
}

/// The rate limit's windows; an attempt holds its slots until it turns out
/// to be a success.
#[derive(Debug)]
struct Attempts {
    limit: RateLimit,
    /// Every remote attempt.
    global: Window,
    /// Only sources holding a slot: an emptied window is dropped.
    by_source: HashMap<Source, Window>,
    /// The newest issued code (its hash) these windows have seen.
    newest_code: Option<String>,
    /// How many times a new code cleared the windows; a slot taken before
    /// that is gone and is not given back.
    cleared: u64,
}

impl Attempts {
    fn new(limit: RateLimit) -> Attempts {
        Attempts {
            limit,
            global: Window::default(),
            by_source: HashMap::new(),
            newest_code: None,
            cleared: 0,
        }
    }

    /// Clears every window if `newest` (the hash of the newest issued code)
    /// is not the one seen last: issuing a code is how the owner gets
    /// pairing unstuck when a flood keeps the windows full.
    fn note_newest_code(&mut self, newest: Option<String>) {
        if newest == self.newest_code {
            return;
        }
        if self.newest_code.is_some() {
            tracing::info!("new pairing code issued, clearing the pairing rate limit");
        }
        self.newest_code = newest;
        self.global = Window::default();
        self.by_source.clear();
        self.cleared += 1;
    }

    /// Reserves a slot for `ip` at `now`, or says how long until one frees
    /// up.
    fn reserve(&mut self, ip: IpAddr, now: DateTime<Utc>) -> std::result::Result<Slot, Duration> {
        let window = self.limit.window;
        self.global.expire(now, window);
        self.by_source.retain(|_, w| {
            w.expire(now, window);
            !w.0.is_empty()
        });
        let source = Source::of(ip);
        if let Some(retry) = self
            .by_source
            .get(&source)
            .and_then(|w| w.full(self.limit.per_source, now, window))
        {
            return Err(retry);
        }
        if source != Source::Loopback {
            if let Some(retry) = self.global.full(self.limit.global, now, window) {
                return Err(retry);
            }
            self.global.0.push_back(now);
        }
        self.by_source.entry(source).or_default().0.push_back(now);
        Ok(Slot {
            source,
            at: now,
            cleared: self.cleared,
        })
    }

    /// Gives back a slot taken by [`Attempts::reserve`].
    fn release(&mut self, slot: Slot) {
        if slot.cleared != self.cleared {
            // Cleared since: giving it back would free someone else's slot.
            return;
        }
        if slot.source != Source::Loopback {
            self.global.remove(slot.at);
        }
        if let Some(w) = self.by_source.get_mut(&slot.source) {
            w.remove(slot.at);
            if w.0.is_empty() {
                self.by_source.remove(&slot.source);
            }
        }
    }
}

/// Why a pairing attempt failed; maps 1:1 to the HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// 400: the request or its CSR is malformed.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// 401: the code is wrong, used or expired (deliberately not said which).
    #[error("invalid pairing code")]
    InvalidCode,
    /// 429: too many failed attempts; retry after the given time.
    #[error("too many pairing attempts, retry in {}s", retry_after.num_seconds())]
    RateLimited { retry_after: Duration },
    /// 500.
    #[error(transparent)]
    Internal(#[from] Error),
}

impl From<rusqlite::Error> for PairError {
    fn from(e: rusqlite::Error) -> PairError {
        PairError::Internal(e.into())
    }
}

/// Redeems pairing codes. Owns its own database connection (pairing is
/// rare, and WAL lets it sit beside the server's other connections).
pub struct PairService {
    db: Mutex<Db>,
    identity: Arc<ServerIdentity>,
    attempts: Mutex<Attempts>,
    clock: Arc<dyn Clock>,
}

impl PairService {
    pub fn new(
        db: Db,
        identity: Arc<ServerIdentity>,
        limit: RateLimit,
        clock: Arc<dyn Clock>,
    ) -> PairService {
        PairService {
            db: Mutex::new(db),
            identity,
            attempts: Mutex::new(Attempts::new(limit)),
            clock,
        }
    }

    /// Checks the code, signs the CSR with the CA and records the device.
    /// `peer` is the address the request came from, for the rate limit.
    pub fn pair(
        &self,
        peer: IpAddr,
        req: &PairRequest,
    ) -> std::result::Result<PairResponse, PairError> {
        let name = req.device_name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_DEVICE_NAME
            || name.chars().any(char::is_control)
        {
            return Err(PairError::BadRequest(format!(
                "device_name must be 1 to {MAX_DEVICE_NAME} printable characters"
            )));
        }

        let now = self.clock.now();
        // `skywaved pair` may be another process: the database is how a new
        // code reaches this one.
        let newest = newest_code(lock(&self.db).conn())?;
        let slot = {
            let mut attempts = lock(&self.attempts);
            attempts.note_newest_code(newest);
            attempts.reserve(peer, now)
        }
        .map_err(|retry_after| PairError::RateLimited { retry_after })?;
        let result = self.redeem(req, name, now);
        match &result {
            Err(PairError::InvalidCode) => {
                tracing::warn!(%peer, "pairing attempt with an invalid code");
            }
            _ => lock(&self.attempts).release(slot),
        }
        result
    }

    fn redeem(
        &self,
        req: &PairRequest,
        name: &str,
        now: DateTime<Utc>,
    ) -> std::result::Result<PairResponse, PairError> {
        let code = PairCode::parse(&req.code).ok_or(PairError::InvalidCode)?;
        let mut db = lock(&self.db);
        // IMMEDIATE: two redemptions of one code serialize on the write lock.
        let tx = db
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claimed = tx.execute(
            "UPDATE pair_codes SET used = 1 WHERE hash = ?1 AND used = 0 AND expires_at > ?2",
            params![code.hash(), timestamp(now)],
        )?;
        if claimed != 1 {
            return Err(PairError::InvalidCode);
        }
        let device_id = DeviceId::new();
        let cert = self
            .identity
            .sign_device_csr(&req.csr_pem, device_id)
            .map_err(|e| match e {
                Error::InvalidCsr(e) => PairError::BadRequest(format!("csr_pem: {e}")),
                e => PairError::Internal(e),
            })?;
        devices::insert(&tx, device_id, name, cert.fingerprint, now)?;
        tx.commit()?;
        tracing::info!(device_id = %device_id.0, name, "paired device");
        Ok(PairResponse {
            device_id,
            device_cert_pem: cert.cert_pem,
            ca_cert_pem: self.identity.ca_cert_pem().to_owned(),
        })
    }
}

/// The unauthenticated part of the API: `POST /v1/pair` and nothing else.
/// Callers need no client certificate for it.
pub fn router(service: Arc<PairService>) -> Router {
    Router::new()
        .route(path::PAIR, post(pair_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(service)
}

async fn pair_handler(
    State(service): State<Arc<PairService>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: std::result::Result<Json<PairRequest>, JsonRejection>,
) -> Response {
    let peer = peer.ip();
    let req = match body {
        Ok(Json(req)) => req,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    // SQLite and signing block; keep them off the async workers.
    let result = match tokio::task::spawn_blocking(move || service.pair(peer, &req)).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = %e, "pairing task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(PairError::BadRequest(reason)) => {
            tracing::debug!(%reason, "rejected pairing request");
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(PairError::InvalidCode) => StatusCode::UNAUTHORIZED.into_response(),
        Err(PairError::RateLimited { retry_after }) => {
            // Round up, so a client that waits exactly this long gets in.
            let secs = (retry_after.num_milliseconds().max(0) + 999) / 1000;
            let mut resp = StatusCode::TOO_MANY_REQUESTS.into_response();
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
            resp
        }
        Err(PairError::Internal(e)) => {
            tracing::error!(error = %e, "pairing failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;
    use axum::body::Body;
    use axum::http::Request;
    use rcgen::{CertificateParams, KeyPair};
    use tower::ServiceExt;

    struct ManualClock(Mutex<DateTime<Utc>>);

    impl ManualClock {
        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        storage: Storage,
        identity: Arc<ServerIdentity>,
        clock: Arc<ManualClock>,
        service: Arc<PairService>,
    }

    fn start() -> DateTime<Utc> {
        "2026-10-29T12:00:00Z".parse().unwrap()
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path().join("d")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let clock = Arc::new(ManualClock(Mutex::new(start())));
        let service = Arc::new(PairService::new(
            Db::open(&storage.dir.db_path()).unwrap(),
            identity.clone(),
            RateLimit::default(),
            clock.clone(),
        ));
        Fixture {
            _tmp: tmp,
            storage,
            identity,
            clock,
            service,
        }
    }

    impl Fixture {
        fn issue(&self, ttl: Duration) -> PairCode {
            issue_code(self.storage.db.conn(), self.clock.now(), ttl)
                .unwrap()
                .code
        }

        fn devices(&self) -> Vec<devices::Device> {
            devices::list(self.storage.db.conn()).unwrap()
        }

        /// Pairs from [`REMOTE`].
        fn pair(&self, req: &PairRequest) -> std::result::Result<PairResponse, PairError> {
            self.service.pair(REMOTE, req)
        }

        /// Spends `n` failed attempts from `peer`, each answered 401.
        fn fail(&self, peer: IpAddr, n: usize, right: &PairCode) {
            for _ in 0..n {
                assert!(matches!(
                    self.service.pair(peer, &request(&wrong_code(right))),
                    Err(PairError::InvalidCode)
                ));
            }
        }
    }

    const REMOTE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7));

    fn remote(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n))
    }

    fn rate_limited(result: std::result::Result<PairResponse, PairError>) -> Duration {
        match result {
            Err(PairError::RateLimited { retry_after }) => retry_after,
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    fn csr() -> String {
        let key = KeyPair::generate().unwrap();
        CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap()
    }

    fn request(code: &str) -> PairRequest {
        PairRequest {
            code: code.to_owned(),
            csr_pem: csr(),
            device_name: "phone".to_owned(),
        }
    }

    fn wrong_code(right: &PairCode) -> String {
        // Same shape, different last symbol.
        let mut s = right.as_str().to_owned();
        let last = s.pop().unwrap();
        s.push(if last == '0' { '1' } else { '0' });
        s
    }

    #[test]
    fn valid_code_pairs_and_cert_verifies_against_ca() {
        let f = fixture();
        let code = f.issue(DEFAULT_TTL);
        // Typed loosely: lowercase, with the separator.
        let resp = f.pair(&request(&code.to_string().to_lowercase())).unwrap();

        assert_eq!(resp.ca_cert_pem, f.identity.ca_cert_pem());
        let (_, pem) = x509_parser::pem::parse_x509_pem(resp.device_cert_pem.as_bytes()).unwrap();
        let cert = pem.parse_x509().unwrap();
        let (_, ca) = x509_parser::parse_x509_certificate(f.identity.ca_cert_der()).unwrap();
        cert.verify_signature(Some(ca.public_key())).unwrap();
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);

        let devices = f.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, resp.device_id);
        assert_eq!(devices[0].name, "phone");
        assert_eq!(devices[0].paired_at, start());
        assert_eq!(
            devices[0].cert_fingerprint,
            Fingerprint::of_der(&pem.contents)
        );
    }

    #[test]
    fn code_is_single_use() {
        let f = fixture();
        let code = f.issue(DEFAULT_TTL);
        f.pair(&request(code.as_str())).unwrap();
        assert!(matches!(
            f.pair(&request(code.as_str())),
            Err(PairError::InvalidCode)
        ));
        assert_eq!(f.devices().len(), 1);
    }

    #[test]
    fn code_expires() {
        let f = fixture();
        let code = f.issue(Duration::minutes(10));
        f.clock.advance(Duration::minutes(10));
        assert!(matches!(
            f.pair(&request(code.as_str())),
            Err(PairError::InvalidCode)
        ));
        assert!(f.devices().is_empty());

        let code = f.issue(Duration::minutes(10));
        f.clock
            .advance(Duration::minutes(10) - Duration::seconds(1));
        f.pair(&request(code.as_str())).unwrap();
    }

    #[test]
    fn wrong_and_malformed_codes_are_rejected() {
        let f = fixture();
        let code = f.issue(DEFAULT_TTL);
        for attempt in [wrong_code(&code), "short".into(), "U".repeat(CODE_LEN)] {
            assert!(matches!(
                f.pair(&request(&attempt)),
                Err(PairError::InvalidCode)
            ));
        }
        assert!(f.devices().is_empty());
        f.pair(&request(code.as_str())).unwrap();
    }

    #[test]
    fn bad_csr_or_name_is_rejected_without_burning_the_code() {
        let f = fixture();
        let code = f.issue(DEFAULT_TTL);
        let mut req = request(code.as_str());
        req.csr_pem = "not a csr".into();
        assert!(matches!(f.pair(&req), Err(PairError::BadRequest(_))));
        for name in ["", "  ", "a\nb", &"x".repeat(MAX_DEVICE_NAME + 1)] {
            let mut req = request(code.as_str());
            req.device_name = name.to_owned();
            assert!(matches!(f.pair(&req), Err(PairError::BadRequest(_))));
        }
        assert!(f.devices().is_empty());
        f.pair(&request(code.as_str())).unwrap();
    }

    #[test]
    fn failed_attempts_are_rate_limited() {
        let f = fixture();
        let code = f.issue(Duration::hours(1));
        let limit = RateLimit::default();
        for _ in 0..limit.per_source {
            f.fail(REMOTE, 1, &code);
            f.clock.advance(Duration::seconds(1));
        }
        // Window full: even the right code is refused, unchecked.
        let retry_after = rate_limited(f.pair(&request(code.as_str())));
        assert_eq!(retry_after, limit.window - Duration::seconds(5));
        // The oldest failure leaves the window: one slot frees up.
        f.clock.advance(limit.window - Duration::seconds(5));
        f.pair(&request(code.as_str())).unwrap();
    }

    #[test]
    fn one_source_does_not_lock_out_another() {
        let f = fixture();
        let code = f.issue(Duration::hours(1));
        let limit = RateLimit::default();
        f.fail(REMOTE, limit.per_source, &code);
        rate_limited(f.pair(&request(code.as_str())));

        // Another address still has its code checked: wrong is 401, not 429.
        f.fail(remote(1), 1, &code);
        // Loopback, whichever address and form, is one source of its own.
        let v4_mapped: IpAddr = "::ffff:127.0.0.2".parse().unwrap();
        f.fail(v4_mapped, limit.per_source - 1, &code);
        f.fail(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 1, &code);
        rate_limited(
            f.service
                .pair("127.0.0.1".parse().unwrap(), &request(code.as_str())),
        );

        // An IPv6 host is its /64: rotating the interface id doesn't help.
        for n in 0..limit.per_source as u16 {
            let ip = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, n + 1);
            f.fail(IpAddr::V6(ip), 1, &code);
        }
        let neighbour = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0xffff, 0, 0, 9);
        rate_limited(
            f.service
                .pair(IpAddr::V6(neighbour), &request(code.as_str())),
        );
        let other_prefix = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 2, 0, 0, 0, 1);
        f.service
            .pair(IpAddr::V6(other_prefix), &request(code.as_str()))
            .unwrap();
    }

    #[test]
    fn global_cap_backstops_every_remote_source_but_spares_loopback() {
        let f = fixture();
        // Two codes up front: issuing one later would clear the windows.
        let spare = f.issue(Duration::hours(1));
        let code = f.issue(Duration::hours(1));
        let limit = RateLimit::default();
        let sources = (limit.global / limit.per_source) as u32;
        for n in 0..sources {
            f.fail(remote(n), limit.per_source, &code);
            f.clock.advance(Duration::seconds(1));
        }
        // A fresh remote address is refused, unchecked, until the oldest
        // failures leave the window.
        let fresh = remote(sources);
        let retry_after = rate_limited(f.service.pair(fresh, &request(code.as_str())));
        assert_eq!(
            retry_after,
            limit.window - Duration::seconds(sources.into())
        );
        // Loopback is outside the global window.
        f.service
            .pair("127.0.0.1".parse().unwrap(), &request(code.as_str()))
            .unwrap();

        // The per-source map holds only sources with a slot, so the global
        // cap bounds it (plus loopback, which released its slot).
        assert_eq!(lock(&f.service.attempts).by_source.len(), sources as usize);
        f.clock.advance(retry_after);
        let code = spare;
        f.service.pair(fresh, &request(code.as_str())).unwrap();
        assert_eq!(
            lock(&f.service.attempts).by_source.len(),
            sources as usize - 1
        );
        f.clock.advance(limit.window);
        f.fail(fresh, 1, &code);
        assert_eq!(lock(&f.service.attempts).by_source.len(), 1);
    }

    #[test]
    fn issuing_a_code_clears_both_windows() {
        let f = fixture();
        let old = f.issue(Duration::hours(1));
        let limit = RateLimit::default();
        // REMOTE fills its own window, and with the others the global one.
        let sources = (limit.global / limit.per_source) as u32;
        f.fail(REMOTE, limit.per_source, &old);
        for n in 1..sources {
            f.fail(remote(n), limit.per_source, &old);
        }
        let fresh = remote(sources);
        rate_limited(f.pair(&request(old.as_str())));
        rate_limited(f.service.pair(fresh, &request(old.as_str())));

        // Issued through another connection, as `skywaved pair` does from
        // its own process; the clock has not moved.
        let new = f.issue(DEFAULT_TTL);
        f.pair(&request(new.as_str())).unwrap();
        f.service.pair(fresh, &request(old.as_str())).unwrap();
        assert_eq!(f.devices().len(), 2);

        // Cleared, not lifted: the limit still counts from here.
        let code = f.issue(DEFAULT_TTL);
        f.fail(REMOTE, limit.per_source, &code);
        rate_limited(f.pair(&request(code.as_str())));
    }

    #[test]
    fn a_slot_from_before_a_clear_is_not_given_back() {
        let mut attempts = Attempts::new(RateLimit::default());
        let before = attempts.reserve(REMOTE, start()).unwrap();
        attempts.note_newest_code(Some("a".into()));
        attempts.reserve(REMOTE, start()).unwrap();
        attempts.release(before);
        assert_eq!(attempts.global.0.len(), 1);
        assert_eq!(attempts.by_source[&Source::of(REMOTE)].0.len(), 1);

        // The same code again is no news.
        attempts.note_newest_code(Some("a".into()));
        assert_eq!(attempts.global.0.len(), 1);
    }

    #[test]
    fn successes_do_not_count_against_the_limit() {
        let f = fixture();
        for _ in 0..RateLimit::default().per_source + 2 {
            let code = f.issue(DEFAULT_TTL);
            f.pair(&request(code.as_str())).unwrap();
        }
        assert_eq!(f.devices().len(), RateLimit::default().per_source + 2);
    }

    #[test]
    fn issuing_stores_only_the_hash_and_prunes_stale_codes() {
        let f = fixture();
        let used = f.issue(DEFAULT_TTL);
        f.pair(&request(used.as_str())).unwrap();
        f.issue(Duration::minutes(1));
        f.clock.advance(Duration::minutes(2));
        let live = f.issue(DEFAULT_TTL);

        let rows: Vec<String> = {
            let conn = f.storage.db.conn();
            let mut stmt = conn.prepare("SELECT hash FROM pair_codes").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(rows, [live.hash()]);
        assert_ne!(live.hash(), live.as_str());
    }

    #[test]
    fn codes_parse_and_display() {
        let code = PairCode::generate().unwrap();
        assert_eq!(code.as_str().len(), CODE_LEN);
        assert!(code.as_str().bytes().all(|b| ALPHABET.contains(&b)));
        assert_eq!(PairCode::parse(&code.to_string()), Some(code.clone()));
        assert_ne!(PairCode::generate().unwrap(), code);

        let read = PairCode::parse("abcde-fghjk").unwrap();
        assert_eq!(read.to_string(), "ABCDE-FGHJK");
        assert_eq!(
            PairCode::parse("O1IL2 34567").unwrap().as_str(),
            "0111234567"
        );
        assert_eq!(PairCode::parse("ABCDEFGHJ"), None);
        assert_eq!(PairCode::parse("ABCDEFGHJKM"), None);
        assert_eq!(PairCode::parse("ABCDEFGHJU"), None);
        assert_eq!(PairCode::parse("ABCDEFGHJ\u{e9}"), None);
        assert_eq!(format!("{read:?}"), "PairCode(..)");
    }

    #[test]
    fn ttl_parsing() {
        assert_eq!(parse_ttl("10m"), Ok(Duration::minutes(10)));
        assert_eq!(parse_ttl("90s"), Ok(Duration::seconds(90)));
        assert_eq!(parse_ttl("24h"), Ok(Duration::hours(24)));
        for bad in [
            "",
            "m",
            "10",
            "0m",
            "-5m",
            "10d",
            "25h",
            "1441m",
            "86401s",
            "99999999999999h",
            "9999999999999999999s",
            "1\u{e9}",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn pairing_address_and_uri() {
        let wildcard: SocketAddr = "0.0.0.0:7443".parse().unwrap();
        let lan: SocketAddr = "192.168.1.5:7443".parse().unwrap();
        assert_eq!(
            pair_address(wildcard, Some("box"), &["sky.example".into()]),
            "sky.example:7443"
        );
        assert_eq!(pair_address(lan, Some("box"), &[]), "192.168.1.5:7443");
        assert_eq!(pair_address(wildcard, Some("box\n"), &[]), "box.local:7443");
        assert_eq!(pair_address(wildcard, Some("box.lan"), &[]), "box.lan:7443");
        assert_eq!(pair_address(wildcard, None, &[]), "localhost:7443");
        assert_eq!(
            pair_address(wildcard, None, &["fd00::7".into()]),
            "[fd00::7]:7443"
        );

        let code = PairCode::parse("ABCDEFGHJK").unwrap();
        let fp = Fingerprint::of_der(b"ca");
        assert_eq!(
            pair_uri("[fd00::7]:7443", &code, fp),
            format!(
                "skywave://pair?addr=%5Bfd00::7%5D:7443&code=ABCDEFGHJK&fp={}",
                fp.to_hex()
            )
        );
        let qr = render_qr(&pair_uri("box.local:7443", &code, fp)).unwrap();
        assert!(qr.lines().count() > 10);
    }

    async fn post(f: &Fixture, body: Body, content_type: bool) -> Response {
        // What the server's connection loop attaches to every request.
        let mut req =
            Request::post(path::PAIR).extension(ConnectInfo(SocketAddr::new(REMOTE, 50_000)));
        if content_type {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        router(f.service.clone())
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap()
    }

    fn json(req: &PairRequest) -> Body {
        Body::from(serde_json::to_vec(req).unwrap())
    }

    #[tokio::test]
    async fn http_pairs_and_maps_errors_to_status_codes() {
        let f = fixture();
        let code = f.issue(DEFAULT_TTL);

        let resp = post(&f, json(&request(code.as_str())), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), MAX_BODY)
            .await
            .unwrap();
        let paired: PairResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(f.devices()[0].id, paired.device_id);

        // Reused code: 401 with an empty body.
        let resp = post(&f, json(&request(code.as_str())), true).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), MAX_BODY)
            .await
            .unwrap();
        assert!(bytes.is_empty());

        for body in [Body::from("{"), Body::from(r#"{"code":"x"}"#)] {
            assert_eq!(post(&f, body, true).await.status(), StatusCode::BAD_REQUEST);
        }
        let resp = post(&f, json(&request(code.as_str())), false).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = post(&f, Body::from(vec![b' '; MAX_BODY + 1]), true).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let code = f.issue(DEFAULT_TTL);
        let mut bad_csr = request(code.as_str());
        bad_csr.csr_pem = "garbage".into();
        assert_eq!(
            post(&f, json(&bad_csr), true).await.status(),
            StatusCode::BAD_REQUEST
        );

        // Issuing the code cleared the reuse's failure; fill the window,
        // then 429.
        for _ in 0..RateLimit::default().per_source {
            let resp = post(&f, json(&request(&wrong_code(&code))), true).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let resp = post(&f, json(&request(code.as_str())), true).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers()[header::RETRY_AFTER], "60");
    }

    #[tokio::test]
    async fn http_exposes_only_the_pair_route() {
        let f = fixture();
        let get = Request::get(path::PAIR).body(Body::empty()).unwrap();
        let resp = router(f.service.clone()).oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        for p in [path::HELLO, path::COMMANDS, path::EVENTS, "/"] {
            let req = Request::post(p).body(Body::empty()).unwrap();
            let resp = router(f.service.clone()).oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{p}");
        }
    }
}
