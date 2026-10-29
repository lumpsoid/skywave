//! The pairing client (spec §6, D9): turns what `skywaved pair` printed —
//! address, code, CA fingerprint — into stored credentials.
//!
//! [`pair`] generates an ECDSA P-256 key and CSR, connects without a client
//! certificate and trusts the server only if it presents the CA with the
//! entered fingerprint (checked before the code is sent), then redeems the
//! code via `POST /v1/pair`. [`pair_and_report`] runs it for a
//! [`Request::Pair`](crate::Request::Pair) and reports to the [`Seam`].
//!
//! Also the input helpers heads share: [`normalize_code`] and
//! [`parse_pair_uri`] for the QR code's `skywave://pair?...` URI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use hyper::{Method, StatusCode, header};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use skywave_proto::{PairRequest, PairResponse, path};

use crate::address::{AddressError, ServerAddress};
use crate::https::{Http, TransportConfig};
use crate::identity::PairedCredentials;
use crate::seam::Seam;
use crate::tls::{self, Fingerprint};
use crate::transport::TransportError;

/// Crockford base32: digits and letters without I, L, O, U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 10;

/// Why pairing failed. The `Display` text is shown on the pairing screen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    #[error(transparent)]
    InvalidAddress(#[from] AddressError),
    #[error("the pairing code must be 10 letters and digits, e.g. ABCDE-12345")]
    InvalidCode,
    #[error("the fingerprint must be 64 hex digits (32 colon-separated pairs)")]
    InvalidFingerprint,
    #[error("not a pairing link: {0}")]
    InvalidUri(String),
    #[error("could not reach the server: {0}")]
    Unreachable(String),
    /// The server's CA is not the one the user was shown: wrong address,
    /// mistyped fingerprint, or someone in between. The code was not sent.
    #[error(
        "the server's certificate does not match the fingerprint; check the address and the fingerprint"
    )]
    FingerprintMismatch,
    /// The CA matched, but the server certificate is not valid for the
    /// address (or not signed by it, or expired).
    #[error("the server's certificate is not valid for this address: {0}")]
    BadServerCertificate(String),
    #[error("the pairing code was not accepted: it is wrong, already used or expired")]
    CodeRejected,
    #[error("too many failed pairing attempts; try again in {}", retry_text(.retry_after_secs))]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("the server refused the pairing request as malformed (400)")]
    BadRequest,
    #[error("the server answered with HTTP {0}")]
    BadStatus(u16),
    #[error("the server's answer could not be understood: {0}")]
    Decode(String),
    /// The CA in the response is not the one the handshake was pinned to.
    #[error("the server returned a CA certificate that does not match the fingerprint")]
    CaMismatch,
    #[error("could not create the device key: {0}")]
    Key(String),
}

fn retry_text(secs: &Option<u64>) -> String {
    match secs {
        Some(s) => format!("{s}s"),
        None => "a minute".into(),
    }
}

/// The canonical form of a typed pairing code: 10 Crockford base32 symbols,
/// uppercase, without `-`. Input is case-insensitive, ignores `-` and
/// spaces, and reads `O` as `0` and `I`/`L` as `1` (spec §6).
pub fn normalize_code(input: &str) -> Result<String, PairError> {
    let mut code = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c if c.is_ascii() && ALPHABET.contains(&(c as u8)) => c,
            _ => return Err(PairError::InvalidCode),
        };
        if code.len() == CODE_LEN {
            return Err(PairError::InvalidCode);
        }
        code.push(c);
    }
    if code.len() != CODE_LEN {
        return Err(PairError::InvalidCode);
    }
    Ok(code)
}

/// The fingerprint as typed (any case, `:` or spaces between pairs).
pub fn parse_fingerprint(input: &str) -> Result<Fingerprint, PairError> {
    Fingerprint::parse(input).ok_or(PairError::InvalidFingerprint)
}

/// What a pairing QR code holds, validated and in canonical form, ready
/// for [`Action::Pair`](crate::Action::Pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairUri {
    /// `host:port`.
    pub address: String,
    /// 10 symbols without `-`.
    pub code: String,
    /// Colon-separated uppercase pairs, as `skywaved pair` prints it.
    pub fingerprint: String,
}

/// Parses `skywave://pair?addr=<host:port>&code=<code>&fp=<fingerprint>`
/// (spec §6). Values are percent-decoded; unknown parameters are ignored.
pub fn parse_pair_uri(uri: &str) -> Result<PairUri, PairError> {
    let uri = uri.trim();
    let bad = |why: &str| PairError::InvalidUri(why.to_owned());
    let (scheme, rest) = uri.split_once("://").ok_or_else(|| bad("missing scheme"))?;
    if !scheme.eq_ignore_ascii_case("skywave") {
        return Err(bad("expected skywave://pair?..."));
    }
    let (target, query) = rest.split_once('?').ok_or_else(|| bad("missing query"))?;
    if !target.trim_end_matches('/').eq_ignore_ascii_case("pair") {
        return Err(bad("expected skywave://pair?..."));
    }
    let (mut addr, mut code, mut fp) = (None, None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value).ok_or_else(|| bad("bad percent-encoding"))?;
        match key {
            "addr" => addr = Some(value),
            "code" => code = Some(value),
            "fp" => fp = Some(value),
            _ => {}
        }
    }
    let address = ServerAddress::parse(&addr.ok_or_else(|| bad("missing addr"))?)?;
    let code = normalize_code(&code.ok_or_else(|| bad("missing code"))?)?;
    let fingerprint = parse_fingerprint(&fp.ok_or_else(|| bad("missing fp"))?)?;
    Ok(PairUri {
        address: address.to_string(),
        code,
        fingerprint: fingerprint.to_string(),
    })
}

/// RFC 3986 percent-decoding (`+` stays `+`); `None` for a broken escape
/// or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A successful pairing, not yet stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    /// Canonical `host:port`, to store as the server address.
    pub address: String,
    pub credentials: PairedCredentials,
}

/// Pairs with the server at `address` using the one-time `code`, trusting
/// it only if its CA has `fingerprint`. Inputs are normalised and checked
/// before anything touches the network. Nothing is stored; see
/// [`pair_and_report`].
pub async fn pair(
    address: &str,
    code: &str,
    fingerprint: &str,
    device_name: &str,
    config: &TransportConfig,
) -> Result<Paired, PairError> {
    let address = ServerAddress::parse(address)?;
    let code = normalize_code(code)?;
    let expected = parse_fingerprint(fingerprint)?;

    let key = KeyPair::generate().map_err(|e| PairError::Key(e.to_string()))?;
    let mut params = CertificateParams::default();
    // The server replaces the subject with the device id.
    params.distinguished_name = DistinguishedName::new();
    let csr_pem = params
        .serialize_request(&key)
        .and_then(|csr| csr.pem())
        .map_err(|e| PairError::Key(e.to_string()))?;
    let body = serde_json::to_vec(&PairRequest {
        code,
        csr_pem,
        device_name: device_name.to_owned(),
    })
    .map_err(|e| PairError::Key(e.to_string()))?;

    let mismatch = Arc::new(AtomicBool::new(false));
    let http = Http::new(
        address.clone(),
        tls::pairing_config(expected, mismatch.clone()),
        config.clone(),
    );
    let reply = http
        .request(
            Method::POST,
            path::PAIR,
            Bytes::from(body),
            config.request_timeout,
        )
        .await
        .map_err(|e| match e {
            _ if mismatch.load(Ordering::SeqCst) => PairError::FingerprintMismatch,
            TransportError::Tls(reason) => PairError::BadServerCertificate(reason),
            TransportError::Timeout => PairError::Unreachable("timed out".into()),
            other => PairError::Unreachable(other.to_string()),
        })?;
    match reply.status {
        StatusCode::OK => {}
        StatusCode::BAD_REQUEST => return Err(PairError::BadRequest),
        StatusCode::UNAUTHORIZED => return Err(PairError::CodeRejected),
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after_secs = reply
                .headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            return Err(PairError::RateLimited { retry_after_secs });
        }
        other => return Err(PairError::BadStatus(other.as_u16())),
    }
    let resp: PairResponse = reply.json().map_err(|e| PairError::Decode(e.to_string()))?;
    let ca = tls::first_cert_der(&resp.ca_cert_pem)
        .ok_or_else(|| PairError::Decode("ca_cert_pem holds no certificate".into()))?;
    if Fingerprint::of_der(&ca) != expected {
        return Err(PairError::CaMismatch);
    }
    if tls::first_cert_der(&resp.device_cert_pem).is_none() {
        return Err(PairError::Decode(
            "device_cert_pem holds no certificate".into(),
        ));
    }
    Ok(Paired {
        address: address.to_string(),
        credentials: PairedCredentials {
            device_id: resp.device_id,
            device_key_pem: key.serialize_pem(),
            device_cert_pem: resp.device_cert_pem,
            ca_cert_pem: resp.ca_cert_pem,
        },
    })
}

/// Handles a [`Request::Pair`](crate::Request::Pair): runs [`pair`] and
/// reports the outcome through `seam` — on success the identity is saved
/// ([`Seam::pairing_succeeded`], which also asks for a connection), on
/// failure the pairing screen shows the reason.
pub async fn pair_and_report(
    seam: &Seam,
    address: &str,
    code: &str,
    fingerprint: &str,
    device_name: &str,
    config: &TransportConfig,
) {
    match pair(address, code, fingerprint, device_name, config).await {
        Ok(paired) => {
            if let Err(e) = seam.pairing_succeeded(&paired.address, paired.credentials) {
                seam.pairing_failed(format!("paired, but could not save the identity: {e}"));
            }
        }
        Err(e) => seam.pairing_failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_normalise_like_the_server() {
        assert_eq!(normalize_code("abcde-12345").unwrap(), "ABCDE12345");
        assert_eq!(normalize_code(" oIl00 xyz9 9 ").unwrap(), "01100XYZ99");
        for bad in ["", "ABCDE1234", "ABCDE123456", "ABCDE1234U", "ABCDE1234é"] {
            assert_eq!(normalize_code(bad), Err(PairError::InvalidCode), "{bad:?}");
        }
    }

    #[test]
    fn pair_uri_round_trips_the_server_format() {
        let fp = Fingerprint::of_der(b"ca");
        let uri = format!(
            "skywave://pair?addr=%5Bfd00::7%5D:7443&code=ABCDE12345&fp={}",
            fp.to_hex()
        );
        assert_eq!(
            parse_pair_uri(&uri).unwrap(),
            PairUri {
                address: "[fd00::7]:7443".into(),
                code: "ABCDE12345".into(),
                fingerprint: fp.to_string(),
            }
        );
        let plain = format!(
            "SKYWAVE://pair?fp={}&code=abcde-12345&x=1&addr=host.local%3A9",
            fp.to_hex()
        );
        assert_eq!(parse_pair_uri(&plain).unwrap().address, "host.local:9");
    }

    #[test]
    fn bad_pair_uris_are_refused() {
        let fp = Fingerprint::of_der(b"ca").to_hex();
        for bad in [
            "https://pair?addr=h:1&code=ABCDE12345".to_owned(),
            "skywave://other?addr=h:1".to_owned(),
            format!("skywave://pair?code=ABCDE12345&fp={fp}"),
            format!("skywave://pair?addr=h:1&code=ABCDE12345&fp={fp}%"),
            "skywave://pair?addr=h:1&code=ABCDE12345&fp=00".to_owned(),
        ] {
            assert!(parse_pair_uri(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rate_limit_reason_names_the_wait() {
        let e = PairError::RateLimited {
            retry_after_secs: Some(42),
        };
        assert!(e.to_string().contains("42s"), "{e}");
    }
}
