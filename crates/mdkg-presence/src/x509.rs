//! The packed attestation certificate profile (WebAuthn section 8.2.1),
//! read with a strict DER walker over the few fields it needs: the
//! version, the subject's organizational units, basic constraints and
//! the id-fido-gen-ce-aaguid extension. rustls-webpki checks the
//! signatures and the chain; it exposes none of these.
//!
//! The walker reads definite, minimal lengths of at most two bytes (a
//! certificate here is at most 4 KiB), single-byte tags, and nothing past
//! the end of an element.

/// `2.5.4.11`, organizationalUnitName.
const OID_OU: &[u8] = &[0x55, 0x04, 0x0b];
/// `2.5.29.15`, keyUsage.
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// `2.5.29.19`, basicConstraints.
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// `1.3.6.1.4.1.45724.1.1.4`, id-fido-gen-ce-aaguid.
const OID_FIDO_AAGUID: &[u8] = &[
    0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0xe5, 0x1c, 0x01, 0x01, 0x04,
];

/// The OU a packed attestation certificate's subject must have.
pub const ATTESTATION_OU: &str = "Authenticator Attestation";

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const BOOLEAN: u8 = 0x01;
const OCTET_STRING: u8 = 0x04;
const BIT_STRING: u8 = 0x03;
const OID: u8 = 0x06;
const UTF8_STRING: u8 = 0x0c;
const PRINTABLE_STRING: u8 = 0x13;
const TELETEX_STRING: u8 = 0x14;
const UNIVERSAL_STRING: u8 = 0x1c;
const BMP_STRING: u8 = 0x1e;
const VERSION: u8 = 0xa0;
const ISSUER_UID: u8 = 0x81;
const SUBJECT_UID: u8 = 0x82;
const EXTENSIONS: u8 = 0xa3;

/// One element: its tag, its contents and what follows it.
fn tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = buf.split_first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 2 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(n)?;
        if bytes[0] == 0 {
            return None;
        }
        let len = bytes.iter().fold(0usize, |a, &b| a << 8 | usize::from(b));
        if len < 0x80 {
            return None;
        }
        (len, rest)
    };
    let (contents, rest) = rest.split_at_checked(len)?;
    Some((tag, contents, rest))
}

/// The element `tag` at the start of `buf`.
fn expect(buf: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (t, c, rest) = tlv(buf)?;
    (t == tag).then_some((c, rest))
}

/// Every element in `buf`, which must be wholly elements.
fn elements(mut buf: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let (t, c, rest) = tlv(buf)?;
        out.push((t, c));
        buf = rest;
    }
    Some(out)
}

/// What the profile reads from a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile<'a> {
    /// 0 for v1, 2 for v3.
    pub version: u8,
    /// The subject's organizational units, decoded.
    pub subject_ous: Vec<String>,
    /// The basic constraints' `cA`, or `None` without the extension.
    pub ca: Option<bool>,
    /// The id-fido-gen-ce-aaguid extension's value and its critical flag.
    pub aaguid: Option<(&'a [u8], bool)>,
    /// The KeyUsage extension's keyCertSign bit, or `None` without the
    /// extension.
    pub key_cert_sign: Option<bool>,
}

/// Reads a certificate's profile fields, or `None` if its DER isn't what
/// the walker accepts.
pub fn read(der: &[u8]) -> Option<Profile<'_>> {
    let (cert, rest) = expect(der, SEQUENCE)?;
    if !rest.is_empty() {
        return None;
    }
    let (tbs, _) = expect(cert, SEQUENCE)?;
    let mut fields = elements(tbs)?.into_iter().peekable();
    let version = match fields.peek() {
        Some(&(VERSION, c)) => {
            fields.next();
            let (v, rest) = expect(c, INTEGER)?;
            if !rest.is_empty() || v.len() != 1 || v[0] > 2 {
                return None;
            }
            v[0]
        }
        _ => 0,
    };
    // serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo.
    let tags = [INTEGER, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE];
    let mut subject = None;
    for (i, want) in tags.into_iter().enumerate() {
        let (t, c) = fields.next()?;
        if t != want {
            return None;
        }
        if i == 4 {
            subject = Some(c);
        }
    }
    let mut extensions = None;
    for (t, c) in fields {
        match t {
            ISSUER_UID | SUBJECT_UID if extensions.is_none() => {}
            EXTENSIONS if extensions.is_none() => {
                let (list, rest) = expect(c, SEQUENCE)?;
                if !rest.is_empty() {
                    return None;
                }
                extensions = Some(list);
            }
            _ => return None,
        }
    }
    let subject_ous = ous(subject?)?;
    let mut profile = Profile {
        version,
        subject_ous,
        ca: None,
        aaguid: None,
        key_cert_sign: None,
    };
    let mut seen: Vec<&[u8]> = Vec::new();
    for (t, ext) in elements(extensions.unwrap_or(&[]))? {
        if t != SEQUENCE {
            return None;
        }
        let (oid, rest) = expect(ext, OID)?;
        let (critical, rest) = match tlv(rest)? {
            (BOOLEAN, [b], rest) => (*b == 0xff, rest),
            _ => (false, rest),
        };
        let (value, rest) = expect(rest, OCTET_STRING)?;
        if !rest.is_empty() || seen.contains(&oid) {
            return None;
        }
        seen.push(oid);
        if oid == OID_BASIC_CONSTRAINTS {
            profile.ca = Some(basic_constraints_ca(value)?);
        } else if oid == OID_KEY_USAGE {
            let (bits, rest) = expect(value, BIT_STRING)?;
            if !rest.is_empty() {
                return None;
            }
            profile.key_cert_sign = Some(key_cert_sign(bits)?);
        } else if oid == OID_FIDO_AAGUID {
            let (inner, rest) = expect(value, OCTET_STRING)?;
            if !rest.is_empty() {
                return None;
            }
            profile.aaguid = Some((inner, critical));
        }
    }
    Some(profile)
}

/// KeyUsage ::= BIT STRING; keyCertSign is bit 5, `0x04` of the first
/// byte after the unused-bits count. A DER BIT STRING's unused bits are
/// zero, and a bit counts only if it is inside the declared length
/// (Terra #8624): `03 02 03 04` declares three unused bits and sets one
/// of them, which is refused, not read as keyCertSign.
fn key_cert_sign(bits: &[u8]) -> Option<bool> {
    let (&unused, data) = bits.split_first()?;
    if unused > 7 || (data.is_empty() && unused != 0) {
        return None;
    }
    if data
        .last()
        .is_some_and(|&last| last & ((1u8 << unused) - 1) != 0)
    {
        return None;
    }
    let len = data.len() * 8 - usize::from(unused);
    Some(len > 5 && data[0] & 0x04 != 0)
}

/// A DirectoryString's value, decoded (Terra #8624): PrintableString,
/// UTF8String, BMPString (UTF-16BE) and UniversalString (UTF-32BE).
/// TeletexString has no one decoding, so it yields `Some(None)`: valid,
/// but never equal to an attestation OU. Anything else is `None`.
fn directory_string(tag: u8, value: &[u8]) -> Option<Option<String>> {
    match tag {
        PRINTABLE_STRING => {
            let printable = |b: &u8| b.is_ascii_alphanumeric() || b" '()+,-./:=?".contains(b);
            value
                .iter()
                .all(printable)
                .then(|| Some(String::from_utf8_lossy(value).into_owned()))
        }
        UTF8_STRING => std::str::from_utf8(value).ok().map(|s| Some(s.to_owned())),
        BMP_STRING => {
            if !value.len().is_multiple_of(2) {
                return None;
            }
            let units = value
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]));
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .ok()
                .map(Some)
        }
        UNIVERSAL_STRING => {
            if !value.len().is_multiple_of(4) {
                return None;
            }
            value
                .chunks_exact(4)
                .map(|c| char::from_u32(u32::from_be_bytes([c[0], c[1], c[2], c[3]])))
                .collect::<Option<String>>()
                .map(Some)
        }
        TELETEX_STRING => Some(None),
        _ => None,
    }
}

/// The organizational units of a Name, decoded.
fn ous(name: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for (t, rdn) in elements(name)? {
        if t != SET {
            return None;
        }
        for (t, atv) in elements(rdn)? {
            if t != SEQUENCE {
                return None;
            }
            let (oid, rest) = expect(atv, OID)?;
            let (vt, value, rest) = tlv(rest)?;
            if !rest.is_empty() {
                return None;
            }
            if oid == OID_OU {
                // Any DirectoryString is a valid OU (Terra #8294), and
                // its decoded value is what's compared (#8624, #8634).
                if let Some(decoded) = directory_string(vt, value)? {
                    out.push(decoded);
                }
            }
        }
    }
    Some(out)
}

/// The `cA` of a BasicConstraints value: FALSE when left out.
fn basic_constraints_ca(value: &[u8]) -> Option<bool> {
    let (bc, rest) = expect(value, SEQUENCE)?;
    if !rest.is_empty() {
        return None;
    }
    match tlv(bc) {
        None if bc.is_empty() => Some(false),
        Some((BOOLEAN, [b], _)) => Some(*b == 0xff),
        Some((INTEGER, _, _)) => Some(false),
        _ => None,
    }
}

/// Why a certificate fails the packed attestation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    Unreadable,
    NotV3,
    NoAttestationOu,
    /// Basic constraints absent, or `cA` true.
    NotALeaf,
    /// The AAGUID extension is marked critical, or isn't 16 bytes.
    BadAaguidExtension,
    /// The AAGUID extension names another authenticator model.
    AaguidMismatch,
}

impl ProfileError {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileError::Unreadable => "the attestation certificate doesn't parse",
            ProfileError::NotV3 => "the attestation certificate isn't X.509 v3",
            ProfileError::NoAttestationOu => {
                "the attestation certificate's subject OU isn't \"Authenticator Attestation\""
            }
            ProfileError::NotALeaf => {
                "the attestation certificate doesn't have basic constraints with CA false"
            }
            ProfileError::BadAaguidExtension => {
                "the attestation certificate's AAGUID extension is malformed or critical"
            }
            ProfileError::AaguidMismatch => {
                "the attestation certificate's AAGUID isn't the authenticator data's"
            }
        }
    }
}

/// Checks a packed attestation certificate against the profile, its
/// AAGUID extension (if any) against `aaguid` from the authenticator data.
pub fn check_attestation_cert(der: &[u8], aaguid: &[u8; 16]) -> Result<(), ProfileError> {
    let p = read(der).ok_or(ProfileError::Unreadable)?;
    if p.version != 2 {
        return Err(ProfileError::NotV3);
    }
    if !p.subject_ous.iter().any(|ou| ou == ATTESTATION_OU) {
        return Err(ProfileError::NoAttestationOu);
    }
    if p.ca != Some(false) {
        return Err(ProfileError::NotALeaf);
    }
    if let Some((value, critical)) = p.aaguid {
        if critical || value.len() != 16 {
            return Err(ProfileError::BadAaguidExtension);
        }
        if value != aaguid {
            return Err(ProfileError::AaguidMismatch);
        }
    }
    Ok(())
}

/// Whether a certificate may sign certificates: basic constraints with
/// `cA` true, and keyCertSign set if it has a KeyUsage extension. Every
/// root and intermediate an attestation chains through must be one;
/// webpki ignores KeyUsage while it builds the chain (Terra #8294).
pub fn may_sign_certificates(der: &[u8]) -> bool {
    read(der).is_some_and(|p| p.ca == Some(true) && p.key_cert_sign != Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    const AAGUID: [u8; 16] = [0xaa; 16];

    #[test]
    fn reads_the_test_certificates() {
        let leaf = read(testutil::LEAF).expect("leaf");
        assert_eq!(leaf.version, 2);
        assert_eq!(leaf.subject_ous, vec![ATTESTATION_OU.to_owned()]);
        assert_eq!((leaf.ca, leaf.aaguid), (Some(false), None));
        assert!(may_sign_certificates(testutil::ROOT));
        assert!(!may_sign_certificates(testutil::LEAF));
        assert!(may_sign_certificates(testutil::INTER_CERTSIGN));
        assert!(
            !may_sign_certificates(testutil::INTER_CRLONLY),
            "CA, but no keyCertSign"
        );
        assert_eq!(
            read(testutil::INTER_CRLONLY).expect("read").key_cert_sign,
            Some(false)
        );
        let with = read(testutil::LEAF_AAGUID).expect("aaguid");
        assert_eq!(with.aaguid, Some((&AAGUID[..], false)));
    }

    #[test]
    fn enforces_the_profile() {
        assert_eq!(check_attestation_cert(testutil::LEAF, &AAGUID), Ok(()));
        assert_eq!(
            check_attestation_cert(testutil::LEAF_AAGUID, &AAGUID),
            Ok(())
        );
        assert_eq!(
            check_attestation_cert(testutil::LEAF_AAGUID_OTHER, &AAGUID),
            Err(ProfileError::AaguidMismatch)
        );
        assert_eq!(
            check_attestation_cert(testutil::LEAF_AAGUID, &[0xbb; 16]),
            Err(ProfileError::AaguidMismatch)
        );
        assert_eq!(
            check_attestation_cert(testutil::LEAF_OU, &AAGUID),
            Err(ProfileError::NoAttestationOu)
        );
        assert_eq!(
            check_attestation_cert(testutil::LEAF_CA, &AAGUID),
            Err(ProfileError::NotALeaf)
        );
        assert_eq!(
            check_attestation_cert(testutil::ROOT, &AAGUID),
            Err(ProfileError::NoAttestationOu)
        );
        assert_eq!(
            check_attestation_cert(&testutil::leaf_v1(), &AAGUID),
            Err(ProfileError::NotV3)
        );
        assert_eq!(
            check_attestation_cert(b"junk", &AAGUID),
            Err(ProfileError::Unreadable)
        );
    }

    #[test]
    fn refuses_loose_der() {
        // Long-form lengths that fit the short form, three-byte lengths,
        // high tags, and trailing bytes.
        assert_eq!(tlv(&[0x30, 0x81, 0x01, 0x00]), None);
        assert_eq!(tlv(&[0x30, 0x83, 0x01, 0x00, 0x00]), None);
        assert_eq!(tlv(&[0x1f, 0x01, 0x00]), None);
        assert_eq!(tlv(&[0x30, 0x02, 0x00]), None);
        let mut der = testutil::LEAF.to_vec();
        der.push(0);
        assert_eq!(read(&der), None);
    }

    /// A Name holding one OU, `tag` and `value`.
    fn name(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut atv = vec![0x06, 0x03, 0x55, 0x04, 0x0b, tag, value.len() as u8];
        atv.extend_from_slice(value);
        let mut set = vec![SEQUENCE, atv.len() as u8];
        set.extend(atv);
        let mut name = vec![SET, set.len() as u8];
        name.extend(set);
        let mut out = vec![SEQUENCE, name.len() as u8];
        out.extend(name);
        out
    }

    fn ous_of(der: &[u8]) -> Option<Vec<String>> {
        let (n, _) = expect(der, SEQUENCE)?;
        ous(n)
    }

    #[test]
    fn the_ou_is_compared_decoded() {
        let want = Some(vec![ATTESTATION_OU.to_owned()]);
        let ou = ATTESTATION_OU.as_bytes();
        assert_eq!(ous_of(&name(PRINTABLE_STRING, ou)), want, "PrintableString");
        assert_eq!(ous_of(&name(UTF8_STRING, ou)), want, "UTF8String");
        let bmp: Vec<u8> = ATTESTATION_OU
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(ous_of(&name(BMP_STRING, &bmp)), want, "BMPString");
        let universal: Vec<u8> = ATTESTATION_OU
            .chars()
            .flat_map(|c| (c as u32).to_be_bytes())
            .collect();
        assert_eq!(
            ous_of(&name(UNIVERSAL_STRING, &universal)),
            want,
            "UniversalString"
        );
        // Teletex reads, but never matches.
        assert_eq!(ous_of(&name(TELETEX_STRING, ou)), Some(vec![]));
        // Not DirectoryStrings, or not well formed: refused.
        assert_eq!(ous_of(&name(0x16, ou)), None, "IA5String");
        assert_eq!(ous_of(&name(BMP_STRING, &bmp[1..])), None, "odd BMP length");
        assert_eq!(
            ous_of(&name(PRINTABLE_STRING, b"a*b")),
            None,
            "* isn't printable"
        );
        assert_eq!(ous_of(&name(UTF8_STRING, &[0xff])), None);
    }

    #[test]
    fn key_usage_padding_must_be_zero() {
        // 03 02 03 04: three unused bits, one of them set: refused.
        assert_eq!(key_cert_sign(&[0x03, 0x04]), None);
        assert_eq!(
            key_cert_sign(&[0x02, 0x04]),
            Some(true),
            "two unused, bit 5 in range"
        );
        assert_eq!(key_cert_sign(&[0x01, 0x02]), Some(false));
        assert_eq!(key_cert_sign(&[0x00, 0x86]), Some(true));
        // Bit 5 outside the declared length isn't set.
        assert_eq!(key_cert_sign(&[0x03, 0x00]), Some(false));
        assert_eq!(key_cert_sign(&[0x08, 0x00]), None, "more than seven unused");
        assert_eq!(key_cert_sign(&[0x01]), None, "unused bits with no data");
        assert_eq!(key_cert_sign(&[0x00]), Some(false));
        assert_eq!(key_cert_sign(&[]), None);
    }
}
