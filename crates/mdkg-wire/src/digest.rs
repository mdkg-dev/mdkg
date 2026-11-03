//! `Content-Digest` (RFC 9530), SHA-256 only: `sha-256=:<base64>:`. Every
//! request with a body carries it, and the signature covers it (S5).

use sha2::{Digest, Sha256};

use crate::b64;

/// The header value for `body`.
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", b64::encode(&Sha256::digest(body)))
}

/// The SHA-256 a header value names: exactly one `sha-256` member, 32
/// bytes, nothing else. Anything else is `None`.
pub fn parse_content_digest(value: &str) -> Option<[u8; 32]> {
    let inner = value.trim().strip_prefix("sha-256=:")?.strip_suffix(':')?;
    b64::decode(inner)?.try_into().ok()
}

/// Whether `value` names `body`'s digest.
pub fn matches(value: &str, body: &[u8]) -> bool {
    parse_content_digest(value).is_some_and(|d| d.as_slice() == Sha256::digest(body).as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc9530_example() {
        // RFC 9530, section 2: the body {"hello": "world"}.
        assert_eq!(
            content_digest(b"{\"hello\": \"world\"}"),
            "sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:"
        );
    }

    #[test]
    fn round_trip_and_mismatch() {
        let v = content_digest(b"body");
        assert!(matches(&v, b"body"));
        assert!(!matches(&v, b"bodY"));
        assert!(!matches("sha-512=:AAAA:", b"body"));
        assert!(!matches("sha-256=:AAAA:", b"body"));
        assert!(!matches(&format!("{v}, sha-512=:AAAA:"), b"body"));
        assert!(
            parse_content_digest("sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE:").is_none()
        );
    }
}
