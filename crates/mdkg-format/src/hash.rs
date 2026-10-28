//! Algorithm-prefixed content hashes for blobs and canonical records.
//!
//! Blobs hash their raw bytes. Records hash their canonical JSON form. Every
//! hash is written `sha256:<64 lowercase hex>`, so the algorithm can change
//! later without ambiguity.

use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::canonical::{self, CanonicalError};

pub const SHA256_PREFIX: &str = "sha256:";

/// A content hash with its algorithm.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    hex: String,
}

impl Digest {
    /// Hashes raw bytes.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hex = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self { hex }
    }

    /// Hashes a record in canonical JSON form.
    pub fn of_record(value: &serde_json::Value) -> Result<Self, CanonicalError> {
        canonical::to_canonical_string(value).map(|s| Self::of_bytes(s.as_bytes()))
    }

    /// Parses `sha256:<hex>`.
    pub fn parse(value: &str) -> Option<Self> {
        let hex = value.strip_prefix(SHA256_PREFIX)?;
        (hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))).then(|| Self {
            hex: hex.to_string(),
        })
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SHA256_PREFIX}{}", self.hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Fixed test vectors: if any of these change, hashing rules changed and
    // every stored hash would break compatibility with its recorded content.
    #[test]
    fn byte_vectors() {
        assert_eq!(
            Digest::of_bytes(b"").to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Digest::of_bytes(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn record_vector() {
        // Canonical form: {"a":1,"b":[true,null,"x"]}
        let record = json!({"b": [true, null, "x"], "a": 1});
        assert_eq!(
            Digest::of_record(&record).unwrap().to_string(),
            "sha256:eca8cfb31ab74533e1eb2f4c74d2d55dfe3c79ac704787e54be8647ea7777eb1"
        );
    }

    #[test]
    fn parse_round_trips() {
        let d = Digest::of_bytes(b"x");
        assert_eq!(Digest::parse(&d.to_string()), Some(d));
        assert_eq!(Digest::parse("sha256:XYZ"), None);
        assert_eq!(Digest::parse("md5:00"), None);
    }
}
