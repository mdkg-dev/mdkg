//! ECDSA signature encodings. The Secure Enclave returns X9.62 DER
//! (`SEQUENCE { INTEGER r, INTEGER s }`); RFC 9421's
//! `ecdsa-p256-sha256` sends `r ‖ s`, 32 bytes each (section 3.3.4),
//! which is what wire verifies.

/// The DER of a P-256 ECDSA signature as `r ‖ s`. Refuses anything but
/// strict DER: definite short lengths, minimal positive integers, nothing
/// after the sequence.
pub fn der_to_fixed(der: &[u8]) -> Option<[u8; 64]> {
    let (&tag, rest) = der.split_first()?;
    let (&len, body) = rest.split_first()?;
    if tag != 0x30 || len >= 0x80 || usize::from(len) != body.len() {
        return None;
    }
    let (r, body) = integer(body)?;
    let (s, body) = integer(body)?;
    if !body.is_empty() {
        return None;
    }
    let mut out = [0u8; 64];
    out[32 - r.len()..32].copy_from_slice(r);
    out[64 - s.len()..].copy_from_slice(s);
    Some(out)
}

/// One DER INTEGER of at most 32 bytes of magnitude, with its leading
/// zero (if any) stripped.
fn integer(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&tag, rest) = buf.split_first()?;
    let (&len, rest) = rest.split_first()?;
    let len = usize::from(len);
    if tag != 0x02 || len == 0 || len >= 0x80 || rest.len() < len {
        return None;
    }
    let (int, rest) = rest.split_at(len);
    // Negative, or a leading zero that isn't needed.
    if int[0] & 0x80 != 0 || (int.len() > 1 && int[0] == 0 && int[1] & 0x80 == 0) {
        return None;
    }
    let magnitude = if int[0] == 0 { &int[1..] } else { int };
    (magnitude.len() <= 32 && !magnitude.is_empty()).then_some((magnitude, rest))
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{
        ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED, EcdsaKeyPair, KeyPair,
        UnparsedPublicKey,
    };

    use super::*;

    #[test]
    fn converts_what_ring_signs() {
        let rng = SystemRandom::new();
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("gen");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .expect("key");
        // Enough signatures that some r or s is short or has its high bit
        // set.
        for i in 0..64u8 {
            let der = pair.sign(&rng, &[i]).expect("sign");
            let fixed = der_to_fixed(der.as_ref()).expect("strict DER");
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, pair.public_key().as_ref())
                .verify(&[i], &fixed)
                .expect("verifies");
        }
    }

    #[test]
    fn refuses_loose_der() {
        let ok = [0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02];
        let fixed = der_to_fixed(&ok).expect("ok");
        assert_eq!((fixed[31], fixed[63]), (1, 2));
        for bad in [
            &[0x31, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02][..],
            &[0x30, 0x07, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02],
            &[0x30, 0x06, 0x02, 0x01, 0x81, 0x02, 0x01, 0x02], // negative
            &[0x30, 0x07, 0x02, 0x02, 0x00, 0x01, 0x02, 0x01, 0x02], // padded
            &[0x30, 0x05, 0x02, 0x00, 0x02, 0x01, 0x02],       // empty
            &[0x30, 0x08, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02, 0x00, 0x00], // trailing
            &[0x30, 0x81, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02], // long form
            &[],
        ] {
            assert_eq!(der_to_fixed(bad), None, "{bad:02x?}");
        }
        let mut long = vec![0x30, 0x25, 0x02, 0x21, 0x01];
        long.extend_from_slice(&[0; 32]);
        long.extend_from_slice(&[0x02, 0x01, 0x01]);
        assert_eq!(der_to_fixed(&long), None, "33-byte magnitude");
    }
}
