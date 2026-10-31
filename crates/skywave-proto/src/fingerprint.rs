//! Certificate fingerprints (D9, spec §6): what `skywaved pair` prints for
//! the CA, what a device pins, and the daemon's device registry key.

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

/// SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of_der(der: &[u8]) -> Fingerprint {
        Fingerprint(Sha256::digest(der).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 64 lowercase hex digits without separators: the pairing URI form,
    /// and the form the daemon stores in its database.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Uppercase hex pairs separated by colons (`AB:CD:…`), as `skywaved pair`
/// prints, for people to read and compare.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// Reads a fingerprint as typed, scanned or stored: 64 hex digits, any
/// case, with or without `:` separators and whitespace. Accepts the
/// [`Display`](fmt::Display) form and [`Fingerprint::to_hex`].
impl FromStr for Fingerprint {
    type Err = InvalidFingerprint;

    fn from_str(s: &str) -> Result<Fingerprint, InvalidFingerprint> {
        let digits: String = s
            .chars()
            .filter(|c| *c != ':' && !c.is_whitespace())
            .collect();
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&digits, &mut bytes).map_err(|_| InvalidFingerprint(s.to_owned()))?;
        Ok(Fingerprint(bytes))
    }
}

/// Text that is not a [`Fingerprint`]; holds the rejected input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidFingerprint(pub String);

impl fmt::Display for InvalidFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid fingerprint {:?}: expected 32 hex bytes", self.0)
    }
}

impl std::error::Error for InvalidFingerprint {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<Fingerprint> {
        s.parse().ok()
    }

    #[test]
    fn displays_as_uppercase_colon_separated_pairs() {
        let fp = Fingerprint::of_der(b"cert");
        let shown = fp.to_string();
        assert_eq!(shown.len(), 32 * 3 - 1);
        assert!(shown.chars().all(|c| c == ':' || c.is_ascii_hexdigit()));
        assert_eq!(shown, shown.to_uppercase());
        assert_eq!(format!("{fp:?}"), format!("Fingerprint({shown})"));
    }

    #[test]
    fn hex_is_lowercase_without_separators() {
        let fp = Fingerprint::of_der(b"cert");
        let hex = fp.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex.to_lowercase());
        assert_eq!(hex, fp.to_string().replace(':', "").to_lowercase());
    }

    #[test]
    fn input_is_lenient() {
        let fp = Fingerprint::of_der(b"ca");
        let shown = fp.to_string();
        assert_eq!(parse(&shown), Some(fp));
        assert_eq!(parse(&shown.to_lowercase()), Some(fp));
        assert_eq!(parse(&shown.replace(':', " ")), Some(fp));
        assert_eq!(parse(&shown.to_lowercase().replace(':', " ")), Some(fp));
        assert_eq!(parse(&shown.replace(':', "\t")), Some(fp));
        assert_eq!(parse(&format!(" {}\n", fp.to_hex())), Some(fp));
        assert_eq!(parse(&fp.to_hex()), Some(fp));
        assert_eq!(parse(&fp.to_hex().to_uppercase()), Some(fp));
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        let fp = Fingerprint::of_der(b"ca");
        assert_eq!(parse(&fp.to_hex()[2..]), None);
        assert_eq!(parse(&format!("{}00", fp.to_hex())), None);
        assert_eq!(parse("AB:CD"), None);
        assert_eq!(parse("zz"), None);
        assert_eq!(parse(""), None);
        assert_eq!(
            "zz".parse::<Fingerprint>(),
            Err(InvalidFingerprint("zz".into()))
        );
    }
}
