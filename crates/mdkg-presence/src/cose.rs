//! A strict decoder for the one CBOR item a person key needs read: the
//! COSE_Key in a FIDO2 credential's attested credential data (RFC 9052
//! section 7, RFC 9053; WebAuthn section 6.5.1).
//!
//! It reads a subset only, and refuses anything outside it:
//! - one map, of at most [`MAX_ENTRIES`] entries, whose keys are small
//!   integers and whose values are integers or byte strings of at most
//!   [`MAX_BYTES`] bytes, so the depth is fixed at one;
//! - definite lengths, in their shortest form (CTAP2's canonical CBOR);
//! - no repeated key, and exactly the parameters of an ES256 (EC2, P-256)
//!   or EdDSA (OKP, Ed25519) key.
//!
//! The decoder reports how many bytes the key took, since extension data
//! may follow it in the authenticator data.

use std::fmt;

/// The most entries a COSE_Key map may have here.
pub const MAX_ENTRIES: usize = 8;
/// The longest byte string a COSE_Key may hold here.
pub const MAX_BYTES: usize = 64;

/// A person key's public key, in the form it is fingerprinted and checked
/// in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    /// A P-256 point, SEC1 uncompressed: `0x04 ‖ x ‖ y`.
    P256([u8; 65]),
    /// An Ed25519 public key.
    Ed25519([u8; 32]),
}

impl PublicKey {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            PublicKey::P256(k) => k,
            PublicKey::Ed25519(k) => k,
        }
    }

    /// Reads the raw bytes the store keeps: 65 bytes starting `0x04` for a
    /// P-256 key, 32 for an Ed25519 one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes.len() {
            65 if bytes[0] == 0x04 => bytes.try_into().ok().map(PublicKey::P256),
            32 => bytes.try_into().ok().map(PublicKey::Ed25519),
            _ => None,
        }
    }

    /// The COSE algorithm the key signs with: -7 (ES256) or -8 (EdDSA).
    pub fn cose_alg(&self) -> i64 {
        match self {
            PublicKey::P256(_) => ALG_ES256,
            PublicKey::Ed25519(_) => ALG_EDDSA,
        }
    }
}

pub const ALG_ES256: i64 = -7;
pub const ALG_EDDSA: i64 = -8;

const KTY: i64 = 1;
const ALG: i64 = 3;
const CRV: i64 = -1;
const X: i64 = -2;
const Y: i64 = -3;
const KTY_OKP: i64 = 1;
const KTY_EC2: i64 = 2;
const CRV_P256: i64 = 1;
const CRV_ED25519: i64 = 6;

/// Why a COSE_Key was refused. Never carries the key's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoseError {
    /// The input ended inside the item.
    Truncated,
    /// A major type, or an additional-information value, outside the
    /// subset: tags, floats, text, arrays, nested maps, indefinite lengths.
    Unsupported,
    /// An integer or length not in its shortest form.
    NotCanonical,
    /// Over [`MAX_ENTRIES`] or [`MAX_BYTES`].
    TooLarge,
    DuplicateKey,
    /// Well-formed, but not exactly an ES256 P-256 or EdDSA Ed25519 key.
    NotAKey,
}

impl fmt::Display for CoseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CoseError::Truncated => "the COSE key is truncated",
            CoseError::Unsupported => "the COSE key uses CBOR outside the accepted subset",
            CoseError::NotCanonical => "the COSE key isn't in canonical CBOR",
            CoseError::TooLarge => "the COSE key is too large",
            CoseError::DuplicateKey => "the COSE key repeats a parameter",
            CoseError::NotAKey => "the COSE key isn't an ES256 P-256 or EdDSA Ed25519 key",
        })
    }
}

impl std::error::Error for CoseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value<'a> {
    Int(i64),
    Bytes(&'a [u8]),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, CoseError> {
        let b = *self.buf.get(self.pos).ok_or(CoseError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CoseError> {
        let end = self.pos.checked_add(n).ok_or(CoseError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(CoseError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    /// One head: the major type and its argument, which must be in its
    /// shortest encoding.
    fn head(&mut self) -> Result<(u8, u64), CoseError> {
        let initial = self.byte()?;
        let major = initial >> 5;
        let info = initial & 0x1f;
        // Tags, floats and simple values are outside the subset, whatever
        // their argument.
        if major >= 6 {
            return Err(CoseError::Unsupported);
        }
        let (arg, min) = match info {
            0..=23 => return Ok((major, u64::from(info))),
            24 => (u64::from(self.byte()?), 24),
            25 => (u64::from(u16::from_be_bytes(arr(self.take(2)?)?)), 0x100),
            26 => (u64::from(u32::from_be_bytes(arr(self.take(4)?)?)), 0x1_0000),
            27 => (u64::from_be_bytes(arr(self.take(8)?)?), 0x1_0000_0000),
            // 28 to 30 are reserved; 31 is an indefinite length.
            _ => return Err(CoseError::Unsupported),
        };
        if arg < min {
            return Err(CoseError::NotCanonical);
        }
        Ok((major, arg))
    }

    fn int_from(major: u8, arg: u64) -> Result<i64, CoseError> {
        let v = i64::try_from(arg).map_err(|_| CoseError::TooLarge)?;
        match major {
            0 => Ok(v),
            // A negative integer encodes -1 - arg.
            1 => Ok(-1 - v),
            _ => Err(CoseError::Unsupported),
        }
    }

    fn value(&mut self) -> Result<Value<'a>, CoseError> {
        let (major, arg) = self.head()?;
        match major {
            0 | 1 => Ok(Value::Int(Self::int_from(major, arg)?)),
            2 => {
                let n = usize::try_from(arg).map_err(|_| CoseError::TooLarge)?;
                if n > MAX_BYTES {
                    return Err(CoseError::TooLarge);
                }
                Ok(Value::Bytes(self.take(n)?))
            }
            _ => Err(CoseError::Unsupported),
        }
    }
}

fn arr<const N: usize>(s: &[u8]) -> Result<[u8; N], CoseError> {
    s.try_into().map_err(|_| CoseError::Truncated)
}

/// Decodes the COSE_Key at the start of `buf`; returns the key and the
/// number of bytes it took.
pub fn decode_key(buf: &[u8]) -> Result<(PublicKey, usize), CoseError> {
    let mut r = Reader { buf, pos: 0 };
    let (major, n) = r.head()?;
    if major != 5 {
        return Err(CoseError::Unsupported);
    }
    let n = usize::try_from(n).map_err(|_| CoseError::TooLarge)?;
    if n > MAX_ENTRIES {
        return Err(CoseError::TooLarge);
    }
    let mut entries: Vec<(i64, Value<'_>)> = Vec::with_capacity(n);
    for _ in 0..n {
        let (major, arg) = r.head()?;
        let key = Reader::int_from(major, arg)?;
        let value = r.value()?;
        if entries.iter().any(|(k, _)| *k == key) {
            return Err(CoseError::DuplicateKey);
        }
        entries.push((key, value));
    }
    let get = |k: i64| entries.iter().find(|(key, _)| *key == k).map(|(_, v)| v);
    let int = |k: i64| match get(k) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let bytes = |k: i64| match get(k) {
        Some(Value::Bytes(b)) => Some(*b),
        _ => None,
    };
    let key = match (int(KTY), int(ALG), int(CRV)) {
        (Some(KTY_EC2), Some(ALG_ES256), Some(CRV_P256)) if entries.len() == 5 => {
            let (Some(x), Some(y)) = (bytes(X), bytes(Y)) else {
                return Err(CoseError::NotAKey);
            };
            if x.len() != 32 || y.len() != 32 {
                return Err(CoseError::NotAKey);
            }
            let mut sec1 = [0u8; 65];
            sec1[0] = 0x04;
            sec1[1..33].copy_from_slice(x);
            sec1[33..].copy_from_slice(y);
            PublicKey::P256(sec1)
        }
        (Some(KTY_OKP), Some(ALG_EDDSA), Some(CRV_ED25519)) if entries.len() == 4 => {
            let x = bytes(X).ok_or(CoseError::NotAKey)?;
            PublicKey::Ed25519(x.try_into().map_err(|_| CoseError::NotAKey)?)
        }
        _ => return Err(CoseError::NotAKey),
    };
    Ok((key, r.pos))
}

/// Encodes a key as canonical COSE (CTAP2 key order), the form an
/// authenticator returns. For tests and fakes; mdkg never sends COSE.
pub fn encode_key(key: &PublicKey) -> Vec<u8> {
    let mut out = Vec::new();
    match key {
        PublicKey::P256(sec1) => {
            out.push(0xa5);
            out.extend_from_slice(&[0x01, 0x02]); // kty: EC2
            out.extend_from_slice(&[0x03, 0x26]); // alg: -7
            out.extend_from_slice(&[0x20, 0x01]); // crv: P-256
            out.extend_from_slice(&[0x21, 0x58, 0x20]); // x
            out.extend_from_slice(&sec1[1..33]);
            out.extend_from_slice(&[0x22, 0x58, 0x20]); // y
            out.extend_from_slice(&sec1[33..]);
        }
        PublicKey::Ed25519(k) => {
            out.push(0xa4);
            out.extend_from_slice(&[0x01, 0x01]); // kty: OKP
            out.extend_from_slice(&[0x03, 0x27]); // alg: -8
            out.extend_from_slice(&[0x20, 0x06]); // crv: Ed25519
            out.extend_from_slice(&[0x21, 0x58, 0x20]); // x
            out.extend_from_slice(k);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p256() -> PublicKey {
        let mut k = [7u8; 65];
        k[0] = 0x04;
        PublicKey::P256(k)
    }

    #[test]
    fn decodes_both_key_kinds_and_reports_their_length() {
        for key in [p256(), PublicKey::Ed25519([9u8; 32])] {
            let mut buf = encode_key(&key);
            let len = buf.len();
            buf.extend_from_slice(&[0xa1, 0x00]); // trailing extension data
            assert_eq!(decode_key(&buf), Ok((key, len)));
        }
    }

    #[test]
    fn refuses_indefinite_lengths_and_reserved_heads() {
        assert_eq!(decode_key(&[0xbf]), Err(CoseError::Unsupported));
        let mut buf = encode_key(&p256());
        buf[0] = 0xbf; // indefinite map
        assert_eq!(decode_key(&buf), Err(CoseError::Unsupported));
        assert_eq!(decode_key(&[0xbc]), Err(CoseError::Unsupported));
        // An indefinite byte string inside the map.
        let buf = [0xa1, 0x21, 0x5f, 0x41, 0x00, 0xff];
        assert_eq!(decode_key(&buf), Err(CoseError::Unsupported));
    }

    #[test]
    fn refuses_everything_outside_the_subset() {
        // An array, text, a tag, a float and a nested map, each as the
        // item or as a value.
        assert_eq!(decode_key(&[0x80]), Err(CoseError::Unsupported));
        assert_eq!(
            decode_key(&[0xa1, 0x01, 0x61, 0x61]),
            Err(CoseError::Unsupported)
        );
        assert_eq!(decode_key(&[0xc0, 0xa0]), Err(CoseError::Unsupported));
        assert_eq!(
            decode_key(&[0xa1, 0x01, 0xf9, 0, 0]),
            Err(CoseError::Unsupported)
        );
        assert_eq!(decode_key(&[0xa1, 0x01, 0xa0]), Err(CoseError::Unsupported));
        // A byte-string map key.
        assert_eq!(
            decode_key(&[0xa1, 0x41, 0x00, 0x01]),
            Err(CoseError::Unsupported)
        );
    }

    #[test]
    fn refuses_non_canonical_heads() {
        // kty 2 written with a one-byte argument.
        let mut buf = encode_key(&p256());
        buf.splice(2..3, [0x18, 0x02]);
        assert_eq!(decode_key(&buf), Err(CoseError::NotCanonical));
        // A map length with a two-byte argument.
        assert_eq!(
            decode_key(&[0xb9, 0x00, 0x05]),
            Err(CoseError::NotCanonical)
        );
    }

    #[test]
    fn bounds_sizes() {
        assert_eq!(decode_key(&[0xa9]), Err(CoseError::TooLarge));
        let mut buf = vec![0xa1, 0x21, 0x58, 65];
        buf.extend_from_slice(&[0; 65]);
        assert_eq!(decode_key(&buf), Err(CoseError::TooLarge));
        // A length past the end, and an eight-byte length.
        assert_eq!(
            decode_key(&[0xa1, 0x21, 0x58, 0x20, 0]),
            Err(CoseError::Truncated)
        );
        let buf = [
            0xa1, 0x21, 0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        assert_eq!(decode_key(&buf), Err(CoseError::TooLarge));
        assert_eq!(decode_key(&[]), Err(CoseError::Truncated));
    }

    #[test]
    fn refuses_repeats_and_wrong_parameters() {
        let mut buf = encode_key(&PublicKey::Ed25519([1; 32]));
        buf[0] = 0xa5;
        buf.extend_from_slice(&[0x01, 0x01]);
        assert_eq!(decode_key(&buf), Err(CoseError::DuplicateKey));
        // ES256 on the Ed25519 curve.
        let mut buf = encode_key(&PublicKey::Ed25519([1; 32]));
        buf[4] = 0x26;
        assert_eq!(decode_key(&buf), Err(CoseError::NotAKey));
        // An extra parameter.
        let mut buf = encode_key(&p256());
        buf[0] = 0xa6;
        buf.extend_from_slice(&[0x04, 0x00]);
        assert_eq!(decode_key(&buf), Err(CoseError::NotAKey));
        // A short coordinate.
        let buf = [0xa4, 0x01, 0x01, 0x03, 0x27, 0x20, 0x06, 0x21, 0x41, 0x00];
        assert_eq!(decode_key(&buf), Err(CoseError::NotAKey));
        assert_eq!(decode_key(&[0xa0]), Err(CoseError::NotAKey));
    }

    #[test]
    fn raw_bytes_round_trip() {
        for key in [p256(), PublicKey::Ed25519([3; 32])] {
            assert_eq!(PublicKey::from_bytes(key.as_bytes()), Some(key));
        }
        assert_eq!(PublicKey::from_bytes(&[0x02; 65]), None);
        assert_eq!(PublicKey::from_bytes(&[0; 33]), None);
    }
}
