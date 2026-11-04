//! The server's TLS identity (D9, architecture.md "Pairing"): a CA it owns, a server
//! certificate signed by that CA, and a stable [`ServerId`].
//!
//! ```text
//! <data_dir>/
//!   server_id        hyphenated UUID, created once
//!   ca/key.pem       CA private key (PKCS#8 PEM, ECDSA P-256), mode 0600
//!   ca/cert.pem      self-signed CA certificate
//!   server/key.pem   server private key (PKCS#8 PEM, ECDSA P-256), mode 0600
//!   server/cert.pem  server certificate, signed by the CA
//! ```
//!
//! [`ServerIdentity::bootstrap`] creates what is missing and loads the rest,
//! so a second run changes nothing. Devices pin the CA (its
//! [`Fingerprint`] is what `skywaved pair` prints), so the server certificate
//! can be reissued without re-pairing: bootstrap reissues it whenever its
//! subject alternative names differ from the requested ones or it is not
//! signed by the current CA. A device connects by the name it was paired
//! with, so a reissue that drops a name warns (and
//! [`ServerIdentity::dropped_names`] lists them): such a device sees a
//! name mismatch until the name is back or it moves to a listed one.
//! Deleting `ca/` creates a new CA, which invalidates every paired device;
//! [`ServerIdentity::new_ca_warning`] is what `serve` and `pair` print then.
//!
//! Bootstrap holds an exclusive lock on `<data_dir>/identity.lock`, so
//! `serve` and `pair` running for the first time at the same moment agree
//! on one CA.
//!
//! Every file is written atomically (temp file + rename) with mode 0600.
//! Certificates are valid from 1975 to 4096: revocation goes through the
//! device registry, not expiry.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use skywave_proto::{DeviceId, Fingerprint, ServerId};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;

use crate::data_dir::DataDir;
use crate::error::{Error, IoContext, Result};

const KEY_FILE: &str = "key.pem";
const CERT_FILE: &str = "cert.pem";
const SERVER_ID_FILE: &str = "server_id";
const LOCK_FILE: &str = "identity.lock";

/// A device certificate issued by [`ServerIdentity::sign_device_csr`].
#[derive(Debug, Clone)]
pub struct DeviceCert {
    pub cert_pem: String,
    /// Fingerprint of the certificate; the device registry key.
    pub fingerprint: Fingerprint,
}

/// The loaded CA, server certificate and server id.
#[derive(Debug)]
pub struct ServerIdentity {
    server_id: ServerId,
    ca: Issuer<'static, KeyPair>,
    ca_cert_pem: String,
    ca_cert_der: Vec<u8>,
    server_key_pem: String,
    server_cert_pem: String,
    server_cert_der: Vec<u8>,
    server_names: Vec<String>,
    dropped_names: Vec<String>,
    created_ca: bool,
}

impl ServerIdentity {
    /// Loads the identity from `dir`, creating whatever is missing. The
    /// server certificate covers `server_names` (DNS names or IP addresses,
    /// see [`server_names`]); it is reissued if it covers a different set.
    pub fn bootstrap(dir: &DataDir, server_names: &[String]) -> Result<ServerIdentity> {
        let lock_path = dir.root().join(LOCK_FILE);
        let lock = std::fs::File::create(&lock_path)
            .and_then(|f| f.lock().map(|()| f))
            .context(|| format!("locking {}", lock_path.display()))?;
        let identity = Self::bootstrap_locked(dir, server_names);
        drop(lock);
        identity
    }

    fn bootstrap_locked(dir: &DataDir, server_names: &[String]) -> Result<ServerIdentity> {
        let server_id = load_or_create_server_id(&dir.root().join(SERVER_ID_FILE))?;
        let (ca, ca_cert_pem, ca_cert_der, created_ca) =
            load_or_create_ca(&dir.ca_dir(), server_id)?;
        let names = normalize_names(server_names);

        let server_dir = dir.server_dir();
        let (key_path, cert_path) = (server_dir.join(KEY_FILE), server_dir.join(CERT_FILE));
        let mut dropped_names = Vec::new();
        let existing = match read_optional(&cert_path)? {
            Some(cert_pem) => {
                let key_pem = read_optional(&key_path)?
                    .ok_or_else(|| bad(&key_path, "missing, but the certificate exists"))?;
                KeyPair::from_pem(&key_pem).map_err(|e| bad(&key_path, e))?;
                let der = pem_to_der(&cert_path, &cert_pem)?;
                let current = server_cert_is_current(&cert_path, &der, &ca_cert_der, &names)?;
                if !current {
                    dropped_names = names_dropped(&cert_path, &der, &names)?;
                }
                current.then_some((key_pem, cert_pem, der))
            }
            None => None,
        };
        if !dropped_names.is_empty() {
            // A device connects by the name it was paired with; one the
            // new certificate lacks fails every handshake on that device.
            tracing::warn!(
                dropped = ?dropped_names,
                "the reissued server certificate no longer covers {}: devices that connect \
                 by one of these names cannot connect until it is back in `server_names` \
                 (or the host name), their address is changed to a listed name, or they \
                 re-pair",
                dropped_names.join(", ")
            );
        }
        let (server_key_pem, server_cert_pem, server_cert_der) = match existing {
            Some(found) => found,
            None => {
                tracing::info!(names = ?names, "issuing server certificate");
                let key = KeyPair::generate()?;
                let mut params = CertificateParams::new(names.clone())
                    .map_err(|e| Error::InvalidServerName(e.to_string()))?;
                params.distinguished_name = rcgen::DistinguishedName::new();
                params
                    .distinguished_name
                    .push(DnType::CommonName, "skywaved");
                params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
                params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
                params.use_authority_key_identifier_extension = true;
                let cert = params.signed_by(&key, &ca)?;
                let key_pem = key.serialize_pem();
                write_private(&key_path, key_pem.as_bytes())?;
                write_private(&cert_path, cert.pem().as_bytes())?;
                (key_pem, cert.pem(), cert.der().to_vec())
            }
        };

        Ok(ServerIdentity {
            server_id,
            ca,
            ca_cert_pem,
            ca_cert_der,
            server_key_pem,
            server_cert_pem,
            server_cert_der,
            server_names: names,
            dropped_names,
            created_ca,
        })
    }

    /// Names the previous server certificate covered that this bootstrap
    /// reissued it without, sorted; empty when nothing was reissued or no
    /// name was dropped. Bootstrap already warned about them (`tracing`).
    pub fn dropped_names(&self) -> &[String] {
        &self.dropped_names
    }

    /// Whether this bootstrap created the CA (`ca/` was missing).
    pub fn created_ca(&self) -> bool {
        self.created_ca
    }

    /// The warning for a CA this bootstrap created while `paired_devices`
    /// (active, unrevoked) devices are registered: each pinned the old CA
    /// and holds a certificate it signed, so none can connect until it
    /// re-pairs. `None` when the CA was loaded or no device is paired (a
    /// first run).
    pub fn new_ca_warning(&self, paired_devices: usize) -> Option<String> {
        (self.created_ca && paired_devices > 0).then(|| {
            format!(
                "created a new certificate authority, but {paired_devices} paired device(s) \
                 trust the old one: every one of them must re-pair (`skywaved pair`). If `ca/` \
                 was lost by mistake, stop skywaved and restore it from a backup instead"
            )
        })
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    /// The fingerprint devices pin, printed by `skywaved pair`.
    pub fn ca_fingerprint(&self) -> Fingerprint {
        Fingerprint::of_der(&self.ca_cert_der)
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn ca_cert_der(&self) -> &[u8] {
        &self.ca_cert_der
    }

    /// PKCS#8 PEM.
    pub fn server_key_pem(&self) -> &str {
        &self.server_key_pem
    }

    pub fn server_cert_pem(&self) -> &str {
        &self.server_cert_pem
    }

    pub fn server_cert_der(&self) -> &[u8] {
        &self.server_cert_der
    }

    /// Server certificate followed by the CA certificate, so a device that
    /// pinned the CA fingerprint finds the CA in the TLS handshake.
    pub fn server_chain_pem(&self) -> String {
        format!("{}{}", self.server_cert_pem, self.ca_cert_pem)
    }

    /// Normalized names the server certificate covers.
    pub fn server_names(&self) -> &[String] {
        &self.server_names
    }

    /// Verifies a device's CSR and issues a client certificate for its key.
    /// The subject is `CN=<device>`; any names or extensions the CSR
    /// requests are replaced, so a device can't ask for more than client
    /// authentication.
    pub fn sign_device_csr(&self, csr_pem: &str, device: DeviceId) -> Result<DeviceCert> {
        let mut csr =
            CertificateSigningRequestParams::from_pem(csr_pem).map_err(Error::InvalidCsr)?;
        let mut params = CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device.0.hyphenated().to_string());
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        csr.params = params;
        let cert = csr.signed_by(&self.ca)?;
        Ok(DeviceCert {
            cert_pem: cert.pem(),
            fingerprint: Fingerprint::of_der(cert.der()),
        })
    }
}

/// The names the server certificate should cover: `localhost`, the loopback
/// addresses, the `listen` IP unless it is a wildcard, `hostname` and
/// `<hostname>.local` (mDNS), plus the configured `extra` names.
pub fn server_names(listen: SocketAddr, hostname: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut names = vec![
        "localhost".to_owned(),
        Ipv4Addr::LOCALHOST.to_string(),
        Ipv6Addr::LOCALHOST.to_string(),
    ];
    if !listen.ip().is_unspecified() {
        names.push(listen.ip().to_string());
    }
    if let Some(host) = hostname.map(str::trim).filter(|h| !h.is_empty()) {
        names.push(host.to_owned());
        if !host.contains('.') {
            names.push(format!("{host}.local"));
        }
    }
    names.extend(extra.iter().cloned());
    normalize_names(&names)
}

/// This machine's host name, if the OS exposes one.
pub fn local_hostname() -> Option<String> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_owned())
        .find(|s| !s.is_empty() && s != "localhost")
}

/// Canonical IP spelling, lowercase DNS names, sorted and deduplicated.
fn normalize_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| match IpAddr::from_str(n) {
            Ok(ip) => ip.to_string(),
            Err(_) => n.to_ascii_lowercase(),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn load_or_create_server_id(path: &Path) -> Result<ServerId> {
    if let Some(text) = read_optional(path)? {
        let uuid = uuid::Uuid::parse_str(text.trim()).map_err(|e| bad(path, e))?;
        return Ok(ServerId(uuid));
    }
    let id = ServerId::new();
    write_private(path, format!("{}\n", id.0.hyphenated()).as_bytes())?;
    tracing::info!(server_id = %id.0, "created server id");
    Ok(id)
}

fn load_or_create_ca(
    ca_dir: &Path,
    server_id: ServerId,
) -> Result<(Issuer<'static, KeyPair>, String, Vec<u8>, bool)> {
    let (key_path, cert_path) = (ca_dir.join(KEY_FILE), ca_dir.join(CERT_FILE));
    // The certificate is written last, so its presence marks a complete CA.
    if let Some(cert_pem) = read_optional(&cert_path)? {
        let key_pem = read_optional(&key_path)?
            .ok_or_else(|| bad(&key_path, "missing, but the CA certificate exists"))?;
        let key = KeyPair::from_pem(&key_pem).map_err(|e| bad(&key_path, e))?;
        let der = pem_to_der(&cert_path, &cert_pem)?;
        let issuer = Issuer::from_ca_cert_der(&der.as_slice().into(), key)
            .map_err(|e| bad(&cert_path, e))?;
        return Ok((issuer, cert_pem, der, false));
    }

    tracing::info!("creating certificate authority");
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("Skywave CA {}", server_id.0.hyphenated()),
    );
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let cert = params.self_signed(&key)?;
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    write_private(&cert_path, cert.pem().as_bytes())?;
    Ok((
        Issuer::new(params, key),
        cert.pem(),
        cert.der().to_vec(),
        true,
    ))
}

/// Whether the stored server certificate is signed by the CA and covers
/// exactly `names`.
fn server_cert_is_current(
    path: &Path,
    der: &[u8],
    ca_der: &[u8],
    names: &[String],
) -> Result<bool> {
    let cert = parse_cert(path, der)?;
    let ca = parse_cert(path, ca_der)?;
    if cert.verify_signature(Some(ca.public_key())).is_err() {
        tracing::info!("server certificate is not signed by the current CA");
        return Ok(false);
    }
    let found = cert_names(&cert).map_err(|e| bad(path, e))?;
    if found != names {
        tracing::info!(?found, wanted = ?names, "server certificate names changed");
        return Ok(false);
    }
    Ok(true)
}

/// The names the stored certificate covers that `names` lacks.
fn names_dropped(path: &Path, der: &[u8], names: &[String]) -> Result<Vec<String>> {
    let cert = parse_cert(path, der)?;
    let found = cert_names(&cert).map_err(|e| bad(path, e))?;
    Ok(found.into_iter().filter(|n| !names.contains(n)).collect())
}

fn parse_cert<'a>(path: &Path, der: &'a [u8]) -> Result<X509Certificate<'a>> {
    x509_parser::parse_x509_certificate(der)
        .map(|(_, cert)| cert)
        .map_err(|e| bad(path, e))
}

/// The certificate's DNS and IP subject alternative names, normalized.
fn cert_names(cert: &X509Certificate<'_>) -> Result<Vec<String>, String> {
    let Some(san) = cert.subject_alternative_name().map_err(|e| e.to_string())? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for name in &san.value.general_names {
        match name {
            GeneralName::DNSName(dns) => names.push((*dns).to_owned()),
            GeneralName::IPAddress(bytes) => {
                let ip = match bytes.len() {
                    4 => IpAddr::from(<[u8; 4]>::try_from(*bytes).expect("length checked")),
                    16 => IpAddr::from(<[u8; 16]>::try_from(*bytes).expect("length checked")),
                    n => return Err(format!("IP address SAN of {n} bytes")),
                };
                names.push(ip.to_string());
            }
            _ => {}
        }
    }
    Ok(normalize_names(&names))
}

fn pem_to_der(path: &Path, pem: &str) -> Result<Vec<u8>> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|e| bad(path, e))?;
    if block.label != "CERTIFICATE" {
        return Err(bad(
            path,
            format!("expected a CERTIFICATE, found {}", block.label),
        ));
    }
    Ok(block.contents)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(|| format!("reading {}", path.display())),
    }
}

/// Writes `bytes` to `path` atomically, readable by the owner only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let ctx = || format!("writing {}", path.display());
    // NamedTempFile creates the file with mode 0600 on Unix.
    let mut tmp = tempfile::NamedTempFile::new_in(dir).context(ctx)?;
    tmp.write_all(bytes).context(ctx)?;
    tmp.as_file().sync_all().context(ctx)?;
    tmp.persist(path).map_err(|e| e.error).context(ctx)?;
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .context(ctx)?;
    Ok(())
}

fn bad(path: &Path, reason: impl fmt::Display) -> Error {
    Error::BadIdentity {
        path: PathBuf::from(path),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["localhost".into(), "127.0.0.1".into(), "box.lan".into()]
    }

    fn files(dir: &DataDir) -> Vec<(PathBuf, Vec<u8>)> {
        [
            dir.root().join(SERVER_ID_FILE),
            dir.ca_dir().join(KEY_FILE),
            dir.ca_dir().join(CERT_FILE),
            dir.server_dir().join(KEY_FILE),
            dir.server_dir().join(CERT_FILE),
        ]
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
    }

    fn device_csr() -> String {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["evil.example".to_owned()]).unwrap();
        params.is_ca = IsCa::NoCa;
        params.serialize_request(&key).unwrap().pem().unwrap()
    }

    #[test]
    fn bootstrap_creates_identity_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let first = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        let before = files(&dir);

        let second = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert_eq!(files(&dir), before);
        assert_eq!(second.server_id(), first.server_id());
        assert_eq!(second.ca_fingerprint(), first.ca_fingerprint());
        assert_eq!(second.server_cert_pem(), first.server_cert_pem());
        assert_eq!(second.server_key_pem(), first.server_key_pem());
    }

    #[test]
    fn concurrent_first_bootstraps_agree_on_one_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let fingerprints: Vec<Fingerprint> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| ServerIdentity::bootstrap(&dir, &names()).unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().ca_fingerprint())
                .collect()
        });
        let on_disk = ServerIdentity::bootstrap(&dir, &names())
            .unwrap()
            .ca_fingerprint();
        assert!(fingerprints.iter().all(|fp| *fp == on_disk));
    }

    #[test]
    fn server_cert_covers_names_and_chains_to_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let id = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert_eq!(id.server_names(), ["127.0.0.1", "box.lan", "localhost"]);

        let cert = parse_cert(Path::new("t"), id.server_cert_der()).unwrap();
        let ca = parse_cert(Path::new("t"), id.ca_cert_der()).unwrap();
        assert_eq!(cert_names(&cert).unwrap(), id.server_names());
        cert.verify_signature(Some(ca.public_key())).unwrap();
        assert!(ca.is_ca());
        assert!(!cert.is_ca());
        assert!(
            id.server_key_pem()
                .starts_with("-----BEGIN PRIVATE KEY-----")
        );
        assert!(id.server_chain_pem().ends_with(id.ca_cert_pem()));
    }

    #[test]
    fn changed_names_reissue_server_cert_but_keep_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let first = ServerIdentity::bootstrap(&dir, &names()).unwrap();

        let mut more = names();
        more.push("10.0.0.7".into());
        let second = ServerIdentity::bootstrap(&dir, &more).unwrap();
        assert_eq!(second.ca_fingerprint(), first.ca_fingerprint());
        assert_eq!(second.server_id(), first.server_id());
        assert_ne!(second.server_cert_pem(), first.server_cert_pem());
        assert!(second.server_names().contains(&"10.0.0.7".to_owned()));
    }

    /// Log lines a closure emits, captured.
    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            self.clone()
        }
    }

    fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        (out, text)
    }

    /// A reissue that drops a name a device may connect by says so, as a
    /// warning on the command's output; one that only adds names, or
    /// changes nothing, does not.
    #[test]
    fn a_reissue_that_drops_a_name_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        ServerIdentity::bootstrap(&dir, &names()).unwrap();

        let mut more = names();
        more.push("10.0.0.7".into());
        let (added, logs) = logged(|| ServerIdentity::bootstrap(&dir, &more).unwrap());
        assert!(added.dropped_names().is_empty());
        assert!(!logs.contains("WARN"), "{logs}");

        let renamed = vec!["localhost".into(), "127.0.0.1".into(), "box2.lan".into()];
        let (reissued, logs) = logged(|| ServerIdentity::bootstrap(&dir, &renamed).unwrap());
        assert_eq!(reissued.dropped_names(), ["10.0.0.7", "box.lan"]);
        assert_eq!(reissued.ca_fingerprint(), added.ca_fingerprint());
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("no longer covers 10.0.0.7, box.lan"),
            "{logs}"
        );

        let (same, logs) = logged(|| ServerIdentity::bootstrap(&dir, &renamed).unwrap());
        assert!(same.dropped_names().is_empty());
        assert!(!logs.contains("WARN"), "{logs}");
    }

    #[test]
    fn new_ca_reissues_server_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let first = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        std::fs::remove_file(dir.ca_dir().join(CERT_FILE)).unwrap();

        let second = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert_ne!(second.ca_fingerprint(), first.ca_fingerprint());
        let cert = parse_cert(Path::new("t"), second.server_cert_der()).unwrap();
        let ca = parse_cert(Path::new("t"), second.ca_cert_der()).unwrap();
        cert.verify_signature(Some(ca.public_key())).unwrap();
    }

    /// Recreating the CA while devices are paired warns that they must
    /// re-pair; a first run (no devices) and a loaded CA do not.
    #[test]
    fn a_new_ca_with_paired_devices_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let first = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert!(first.created_ca());
        assert_eq!(first.new_ca_warning(0), None);

        let loaded = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert!(!loaded.created_ca());
        assert_eq!(loaded.new_ca_warning(3), None);

        std::fs::remove_dir_all(dir.ca_dir()).unwrap();
        std::fs::create_dir(dir.ca_dir()).unwrap();
        let recreated = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert!(recreated.created_ca());
        assert_ne!(recreated.ca_fingerprint(), first.ca_fingerprint());
        let warning = recreated.new_ca_warning(3).expect("devices are paired");
        assert!(warning.contains("3 paired device(s)"), "{warning}");
        assert!(warning.contains("must re-pair"), "{warning}");
    }

    #[test]
    fn missing_ca_key_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        ServerIdentity::bootstrap(&dir, &names()).unwrap();
        std::fs::remove_file(dir.ca_dir().join(KEY_FILE)).unwrap();
        assert!(matches!(
            ServerIdentity::bootstrap(&dir, &names()),
            Err(Error::BadIdentity { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn identity_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        ServerIdentity::bootstrap(&dir, &names()).unwrap();
        for (path, _) in files(&dir) {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{} is {mode:o}", path.display());
        }
    }

    #[test]
    fn signs_device_csr_as_client_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let id = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        let device = DeviceId::new();
        let signed = id.sign_device_csr(&device_csr(), device).unwrap();

        let der = pem_to_der(Path::new("t"), &signed.cert_pem).unwrap();
        assert_eq!(signed.fingerprint, Fingerprint::of_der(&der));
        let cert = parse_cert(Path::new("t"), &der).unwrap();
        let ca = parse_cert(Path::new("t"), id.ca_cert_der()).unwrap();
        cert.verify_signature(Some(ca.public_key())).unwrap();
        assert!(!cert.is_ca());
        assert!(cert_names(&cert).unwrap().is_empty(), "CSR SANs dropped");
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(cn, device.0.hyphenated().to_string());
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
    }

    #[test]
    fn rejects_garbage_csr() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let id = ServerIdentity::bootstrap(&dir, &names()).unwrap();
        assert!(matches!(
            id.sign_device_csr("not a csr", DeviceId::new()),
            Err(Error::InvalidCsr(_))
        ));
    }

    #[test]
    fn derived_server_names() {
        let wildcard: SocketAddr = "0.0.0.0:7443".parse().unwrap();
        assert_eq!(
            server_names(wildcard, Some("box\n"), &["Skywave.Example".into()]),
            [
                "127.0.0.1",
                "::1",
                "box",
                "box.local",
                "localhost",
                "skywave.example"
            ]
        );
        let lan: SocketAddr = "192.168.1.5:7443".parse().unwrap();
        assert_eq!(
            server_names(lan, None, &[]),
            ["127.0.0.1", "192.168.1.5", "::1", "localhost"]
        );
    }
}
