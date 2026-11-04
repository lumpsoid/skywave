//! Content hashes of blobs (architecture.md "Client core", Blobs): what a
//! [`BlobRef`](crate::BlobRef) promises its bytes hash to, so a device can
//! check a download before it trusts it.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// SHA-256 of a blob's content. On the wire: 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// The hash of `content`, whole.
    pub fn of(content: &[u8]) -> ContentHash {
        ContentHash(Sha256::digest(content).into())
    }

    /// The hash of everything `content` reads, to its end.
    pub fn read(mut content: impl Read) -> io::Result<ContentHash> {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match content.read(&mut buf) {
                Ok(0) => return Ok(ContentHash(hasher.finalize().into())),
                Ok(n) => hasher.update(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// A hash computed elsewhere (a streaming SHA-256 over the content).
    pub fn from_digest(digest: [u8; 32]) -> ContentHash {
        ContentHash(digest)
    }

    /// 64 lowercase hex digits: the wire form, and the form the daemon
    /// stores in its database.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({self})")
    }
}

/// Reads exactly the [`ContentHash::to_hex`] form: 64 lowercase hex digits.
impl FromStr for ContentHash {
    type Err = InvalidContentHash;

    fn from_str(s: &str) -> Result<ContentHash, InvalidContentHash> {
        let invalid = || InvalidContentHash(s.to_owned());
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(invalid());
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(s, &mut bytes).map_err(|_| invalid())?;
        Ok(ContentHash(bytes))
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<ContentHash, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Text that is not a [`ContentHash`]; holds the rejected input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidContentHash(pub String);

impl fmt::Display for InvalidContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid content hash {:?}: expected 64 lowercase hex digits",
            self.0
        )
    }
}

impl std::error::Error for InvalidContentHash {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_matches_sha256() {
        let hash = ContentHash::of(b"abc");
        assert_eq!(
            hash.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash.to_hex().parse(), Ok(hash));
        assert_eq!(hash.to_string(), hash.to_hex());
        let long = vec![7u8; 200 * 1024];
        assert_eq!(
            ContentHash::read(&long[..]).unwrap(),
            ContentHash::of(&long)
        );
    }

    #[test]
    fn rejects_anything_but_64_lowercase_hex_digits() {
        let hex = ContentHash::of(b"abc").to_hex();
        for bad in [
            hex.to_uppercase(),
            hex[2..].to_owned(),
            format!("{hex}00"),
            String::new(),
            "zz".repeat(32),
        ] {
            assert_eq!(
                bad.parse::<ContentHash>(),
                Err(InvalidContentHash(bad.clone()))
            );
        }
    }
}
