//! Base64, both alphabets, strict on decode: RFC 8941 byte sequences use
//! the standard alphabet with padding; nonces and fingerprints use
//! base64url without padding (S5). A decoder that accepted two spellings
//! of one value would let a signed value be re-spelled, so anything but the
//! canonical form is refused.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_with(bytes: &[u8], table: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]),
            2 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8),
            _ => u32::from(chunk[0]) << 16,
        };
        let sextets = chunk.len() + 1;
        for i in 0..4 {
            if i < sextets {
                out.push(char::from(table[((n >> (18 - 6 * i)) & 63) as usize]));
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

fn decode_with(text: &str, table: &[u8; 64], pad: bool) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let body = if pad {
        if !bytes.len().is_multiple_of(4) {
            return None;
        }
        let trailing = bytes.iter().rev().take_while(|b| **b == b'=').count();
        if trailing > 2 {
            return None;
        }
        &bytes[..bytes.len() - trailing]
    } else {
        if bytes.len() % 4 == 1 {
            return None;
        }
        bytes
    };
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in body {
        let v = table.iter().position(|t| *t == b)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero, so each value has one spelling.
    if acc != 0 {
        return None;
    }
    if pad && encode_with(&out, table, true).len() != bytes.len() {
        return None;
    }
    Some(out)
}

/// The standard alphabet, padded (RFC 8941 byte sequences).
pub fn encode(bytes: &[u8]) -> String {
    encode_with(bytes, STD, true)
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    decode_with(text, STD, true)
}

/// base64url without padding (nonces, fingerprints, enrolment bodies).
pub fn encode_url(bytes: &[u8]) -> String {
    encode_with(bytes, URL, false)
}

pub fn decode_url(text: &str) -> Option<Vec<u8>> {
    decode_with(text, URL, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, coded) in cases {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).as_deref(), Some(plain.as_bytes()));
            let url = coded.trim_end_matches('=');
            assert_eq!(encode_url(plain.as_bytes()), url);
            assert_eq!(decode_url(url).as_deref(), Some(plain.as_bytes()));
        }
    }

    #[test]
    fn alphabets_differ() {
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
        assert_eq!(encode_url(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode("-_8="), None);
        assert_eq!(decode_url("+/8"), None);
    }

    #[test]
    fn only_canonical_spellings() {
        // Non-zero leftover bits.
        assert_eq!(decode("Zh=="), None);
        assert_eq!(decode_url("Zh"), None);
        // Missing or extra padding.
        assert_eq!(decode("Zg"), None);
        assert_eq!(decode("Zg==="), None);
        assert_eq!(decode("Zm9v===="), None);
        assert_eq!(decode_url("Zg=="), None);
        // Padding inside.
        assert_eq!(decode("Zg==Zg=="), None);
        // Impossible length.
        assert_eq!(decode_url("Z"), None);
        assert_eq!(decode("Zm9 v"), None);
    }

    #[test]
    fn round_trips_every_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..data.len() {
            let d = &data[..n];
            assert_eq!(decode(&encode(d)).as_deref(), Some(d));
            assert_eq!(decode_url(&encode_url(d)).as_deref(), Some(d));
        }
    }
}
