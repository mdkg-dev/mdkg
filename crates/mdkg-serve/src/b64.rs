//! Base64, both alphabets (RFC 4648): the standard one with padding for
//! structured-field byte sequences (RFC 8941, so `Signature` and
//! `Content-Digest`), and the URL-safe one without padding for nonces and
//! fingerprints. Decoding is strict: one canonical form per value.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_with(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.len();
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= n {
                out.push(alphabet[((v >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

fn decode_with(text: &str, alphabet: &[u8; 64], pad: bool) -> Option<Vec<u8>> {
    let raw = text.as_bytes();
    let body = if pad {
        if !raw.len().is_multiple_of(4) {
            return None;
        }
        let pads = raw.iter().rev().take_while(|&&c| c == b'=').count();
        if pads > 2 {
            return None;
        }
        &raw[..raw.len() - pads]
    } else {
        raw
    };
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    for chunk in body.chunks(4) {
        let mut v = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let d = alphabet.iter().position(|a| a == c)? as u32;
            v |= d << (18 - 6 * i);
        }
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        let n = chunk.len() - 1;
        // The unused low bits must be zero, so each value has one encoding.
        let unused = match n {
            1 => v & 0xFFFF,
            2 => v & 0xFF,
            _ => 0,
        };
        if unused != 0 {
            return None;
        }
        out.extend_from_slice(&bytes[..n]);
    }
    Some(out)
}

pub fn encode(bytes: &[u8]) -> String {
    encode_with(bytes, STD, true)
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    decode_with(text, STD, true)
}

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
    fn url_alphabet_and_strictness() {
        assert_eq!(encode_url(&[0xfb, 0xff]), "-_8");
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
        // Non-canonical trailing bits, bad padding and foreign characters.
        assert_eq!(decode("Zh=="), None);
        assert_eq!(decode("Zg="), None);
        assert_eq!(decode("Zg==="), None);
        assert_eq!(decode_url("Zg=="), None);
        assert_eq!(decode_url("Z"), None);
        assert_eq!(decode("Z!=="), None);
        assert_eq!(decode_url("-_8"), Some(vec![0xfb, 0xff]));
    }
}
