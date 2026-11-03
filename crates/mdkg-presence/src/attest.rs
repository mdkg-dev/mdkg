//! FIDO2 attestation at enrolment (WebAuthn section 8): whether a
//! credential's hardware is vouched for by a root the approving person
//! supplied (P2, decided 2026-10-29).
//!
//! What every FIDO2 enrolment must satisfy, whatever its attestation:
//! - the authenticator data parses, and names this RP id;
//! - the "user present" flag is set, and "user verified" too when the
//!   key's policy needs it;
//! - the statement names a format; an empty one is malformed.
//!
//! Then by format:
//! - `packed` (with or without x5c) carries a signature over
//!   `authData ‖ clientDataHash`, where the client data hash is SHA-256 of
//!   the enrolment's challenge, so a packed attestation made for another
//!   enrolment never verifies here. A packed x5c statement in a signature
//!   algorithm this build doesn't verify has that signature skipped (its
//!   other checks still run), so, like `none`, only the proof of
//!   possession binds it, and it is Absent(unsupported_alg);
//! - `none` carries nothing to check, and must carry nothing;
//! - any other format (fido-u2f, tpm, android-key, apple, ...) isn't
//!   verified by this build: its statement isn't read, whatever it holds,
//!   so neither its signature nor the challenge it was made for is
//!   checked (#9152, on #9281).
//!
//! For `none` and unverified formats, nothing in the statement binds the
//! credential to this enrolment; the four-field proof of possession does
//! (#8051), and these keys are only ever machine-grade.
//!
//! An x5c's leaf must fit the packed attestation profile (section
//! 8.2.1): X.509 v3, subject OU "Authenticator Attestation", basic
//! constraints with CA false, and an id-fido-gen-ce-aaguid extension, if
//! present, that names the authenticator data's AAGUID. Every supplied
//! root must be a CA certificate, so a leaf is never a root.
//!
//! The result is [`Proof::Chained`] only for `packed` with an x5c chain
//! that leads to one of the supplied roots. `none`, self attestation
//! (`packed` without x5c), any other format, and an x5c with no roots
//! supplied give the other proofs, which never make a key verified; the
//! enrolment's grade also needs neither backup flag set, which only
//! [`crate::enrol`] decides.
//! An x5c chain that fails against roots the person did supply is refused:
//! they asked for a verified key and didn't get one.
//!
//! Residual (#9777): chain Absent versus Failed classification follows
//! webpki's first error; best effort for audit, with grade unaffected.
//!
//! Residual (#9916): statement fields the CTAP library drops are not
//! checked; verification is over the normalized subset (`alg`, `sig` and
//! `x5c` for packed; nothing for `none`). Any field received malformed is
//! still Failed (malformed): the HID adapter passes on an `alg` it got for
//! `none`, which is then refused (Terra #10099).

use std::fmt;
use std::time::Duration;

use ring::signature::{self, UnparsedPublicKey};
use rustls_pki_types::{CertificateDer, SignatureVerificationAlgorithm, TrustAnchor, UnixTime};
use sha2::{Digest, Sha256};
use webpki::{EndEntityCert, ExtendedKeyUsageValidator, KeyPurposeIdIter};

use crate::authdata::{self, AuthData, AuthDataError};
use crate::cose::{ALG_EDDSA, ALG_ES256, PublicKey};
use crate::grade::Policy;
use crate::x509;

/// RS256, which some authenticators' attestation keys use.
pub const ALG_RS256: i64 = -257;

/// The most certificates an x5c may carry, leaf included.
pub const MAX_CHAIN: usize = 5;
/// The largest certificate accepted, in bytes.
pub const MAX_CERT: usize = 4096;

/// An attestation statement as the authenticator returned it, already
/// unpacked from CBOR by the FIDO2 library on the enrolling device.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Statement {
    /// `none`, `packed`, or another WebAuthn format.
    pub fmt: String,
    /// The COSE algorithm of `sig`; absent for `none`.
    pub alg: Option<i64>,
    pub sig: Vec<u8>,
    /// The attestation certificate first, then its chain, DER.
    pub x5c: Vec<Vec<u8>>,
}

/// Why a FIDO2 enrolment was refused. Never carries key or signature bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestError {
    AuthData(AuthDataError),
    /// The credential was made for another RP id.
    RpMismatch,
    /// The "user present" flag is clear.
    NotPresent,
    /// The policy needs "user verified", and the flag is clear.
    NotVerified,
    /// A statement that doesn't hang together: `none` with a signature,
    /// `packed` without an algorithm, a chain too long or too large.
    Malformed(&'static str),
    /// The attestation signature doesn't verify.
    BadSignature,
    /// An x5c chain that doesn't lead to any supplied root.
    Untrusted,
    /// The attestation certificate fails the packed profile.
    Profile(x509::ProfileError),
}

impl fmt::Display for AttestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttestError::AuthData(e) => e.fmt(f),
            AttestError::RpMismatch => f.write_str("the credential was made for another RP id"),
            AttestError::NotPresent => f.write_str("the authenticator didn't set the user-present flag"),
            AttestError::NotVerified => f.write_str(
                "the key's policy needs user verification, and the authenticator didn't set the user-verified flag",
            ),
            AttestError::Malformed(what) => write!(f, "the attestation statement is malformed: {what}"),
            AttestError::BadSignature => {
                f.write_str("the attestation signature doesn't verify over this enrolment")
            }
            AttestError::Profile(e) => f.write_str(e.as_str()),
            AttestError::Untrusted => {
                f.write_str("the attestation certificate doesn't chain to any of the supplied roots")
            }
        }
    }
}

impl std::error::Error for AttestError {}

/// The roots an attestation may chain to, and the time to check them at.
/// Its fields are private: [`Roots::none`] and [`Roots::from_der`] are the
/// only ways to make one, so every anchor has passed the CA checks
/// (Terra #8294), once, here.
#[derive(Debug)]
pub struct Roots<'a> {
    anchors: Vec<TrustAnchor<'a>>,
    /// The roots' own DER, so a leaf is never taken for one.
    ders: Vec<&'a [u8]>,
    now: UnixTime,
}

impl<'a> Roots<'a> {
    /// No roots: every attestation is `none`.
    pub fn none(now: UnixTime) -> Self {
        Self {
            anchors: Vec::new(),
            ders: Vec::new(),
            now,
        }
    }

    /// Trust anchors from DER certificates. A certificate that isn't one,
    /// or may not sign certificates (basic constraints with CA true, and
    /// keyCertSign if it has KeyUsage), is refused rather than skipped.
    pub fn from_der(certs: &'a [CertificateDer<'a>], now: UnixTime) -> Result<Self, AttestError> {
        let mut anchors = Vec::with_capacity(certs.len());
        for c in certs {
            if !x509::may_sign_certificates(c) {
                return Err(AttestError::Malformed(
                    "a supplied root isn't a CA certificate",
                ));
            }
            anchors.push(
                webpki::anchor_from_trusted_cert(c)
                    .map_err(|_| AttestError::Malformed("a supplied root isn't a certificate"))?,
            );
        }
        let ders = certs.iter().map(|c| c.as_ref()).collect();
        Ok(Self { anchors, ders, now })
    }
}

/// `UnixTime` from milliseconds since the epoch, the store's clock unit.
pub fn unix_time_ms(ms: u64) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_millis(ms))
}

/// The RP id hash a credential must carry.
pub fn rp_id_hash(rp_id: &str) -> [u8; 32] {
    Sha256::digest(rp_id.as_bytes()).into()
}

/// The checks that don't depend on the statement's format (#9346): the
/// authenticator data parses, names `rp_id`, has UP (and UV under `uv`),
/// and the statement names a format. Returns the parsed data.
pub fn check_common(
    auth_data: &[u8],
    statement: &Statement,
    rp_id: &str,
    policy: Policy,
) -> Result<AuthData, AttestError> {
    let data = authdata::parse_attested(auth_data).map_err(AttestError::AuthData)?;
    if data.rp_id_hash != rp_id_hash(rp_id) {
        return Err(AttestError::RpMismatch);
    }
    if !data.user_present() {
        return Err(AttestError::NotPresent);
    }
    if policy.needs_verified() && !data.user_verified() {
        return Err(AttestError::NotVerified);
    }
    if statement.fmt.is_empty() {
        return Err(AttestError::Malformed("the statement names no format"));
    }
    Ok(data)
}

/// What a statement that verified proves (#9152).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// A packed x5c chain to a supplied root.
    Chained,
    /// Format `none`: no statement to check.
    NoneFormat,
    /// Packed without x5c: the credential key signed, which proves
    /// possession, not hardware.
    SelfAttested,
    /// A packed x5c, with no roots supplied to check it against.
    Unanchored,
    /// A format this build doesn't verify (fido-u2f, tpm, android-key,
    /// apple, ...): a lack of proof, not a fault (#9152, on #9281).
    UnsupportedFormat,
    /// A packed x5c statement, or its certificate chain, signed with an
    /// algorithm this build doesn't verify (#9478, #9567): like an
    /// unverified format, it proves nothing.
    UnsupportedAlg,
}

/// Checks a FIDO2 credential made at enrolment; returns its parsed
/// authenticator data and what its attestation proves. A statement in a
/// format this build doesn't verify is [`Proof::UnsupportedFormat`]: an
/// absence of proof, never mistaken for a verified one (#9152, on #9281).
pub fn verify(
    auth_data: &[u8],
    statement: &Statement,
    rp_id: &str,
    challenge: &[u8],
    policy: Policy,
    roots: &Roots<'_>,
) -> Result<(AuthData, Proof), AttestError> {
    let data = check_common(auth_data, statement, rp_id, policy)?;
    let client_data_hash: [u8; 32] = Sha256::digest(challenge).into();
    let mut signed = Vec::with_capacity(auth_data.len() + 32);
    signed.extend_from_slice(auth_data);
    signed.extend_from_slice(&client_data_hash);

    let proof = match statement.fmt.as_str() {
        "none" => {
            if statement.alg.is_some() || !statement.sig.is_empty() || !statement.x5c.is_empty() {
                return Err(AttestError::Malformed(
                    "a none statement carries a signature",
                ));
            }
            Proof::NoneFormat
        }
        "packed" => {
            let alg = statement.alg.ok_or(AttestError::Malformed(
                "a packed statement has no algorithm",
            ))?;
            // Packed always carries its signature, even in an algorithm
            // this build can't verify (Terra #10099).
            if statement.sig.is_empty() {
                return Err(AttestError::Malformed(
                    "a packed statement has no signature",
                ));
            }
            if statement.x5c.is_empty() {
                // Self attestation: the credential key signs; it proves
                // possession, not hardware.
                if alg != data.public_key.cose_alg() {
                    return Err(AttestError::Malformed(
                        "a self attestation's algorithm isn't the credential's",
                    ));
                }
                verify_with_key(&data.public_key, &signed, &statement.sig)?;
                Proof::SelfAttested
            } else {
                verify_x5c(
                    alg,
                    &statement.x5c,
                    &data.aaguid,
                    &signed,
                    &statement.sig,
                    roots,
                )?
            }
        }
        // fido-u2f, tpm, android-key, apple and the rest: this build
        // doesn't verify them, so they prove nothing either way, and the
        // statement isn't read, whatever it carries: unlike `none`, its
        // contents are deliberately left unchecked, since the outcome is
        // the same machine grade. The RP id and the presence flags above
        // were still checked.
        _ => Proof::UnsupportedFormat,
    };
    Ok((data, proof))
}

fn verify_with_key(key: &PublicKey, msg: &[u8], sig: &[u8]) -> Result<(), AttestError> {
    let alg: &dyn signature::VerificationAlgorithm = match key {
        PublicKey::P256(_) => &signature::ECDSA_P256_SHA256_ASN1,
        PublicKey::Ed25519(_) => &signature::ED25519,
    };
    UnparsedPublicKey::new(alg, key.as_bytes())
        .verify(msg, sig)
        .map_err(|_| AttestError::BadSignature)
}

/// Accepts any extended key usage: WebAuthn doesn't require one on an
/// attestation certificate, and vendors differ.
struct AnyUsage;

impl ExtendedKeyUsageValidator for AnyUsage {
    fn validate(&self, iter: KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        for purpose in iter {
            purpose?;
        }
        Ok(())
    }
}

fn verify_x5c(
    alg: i64,
    x5c: &[Vec<u8>],
    aaguid: &[u8; 16],
    signed: &[u8],
    sig: &[u8],
    roots: &Roots<'_>,
) -> Result<Proof, AttestError> {
    if x5c.len() > MAX_CHAIN {
        return Err(AttestError::Malformed("the certificate chain is too long"));
    }
    if x5c.iter().any(|c| c.len() > MAX_CERT) {
        return Err(AttestError::Malformed("a certificate is too large"));
    }
    // A statement algorithm this build can't verify skips only the
    // statement signature: every other check still runs, so evidence it
    // shows wrong is Failed, and only a statement that passes them all is
    // Absent(UnsupportedAlg), before unanchored and backup (#9478, Claude
    // #9612).
    let sig_alg: Option<&dyn SignatureVerificationAlgorithm> = match alg {
        ALG_ES256 => Some(webpki::ring::ECDSA_P256_SHA256),
        ALG_EDDSA => Some(webpki::ring::ED25519),
        ALG_RS256 => Some(webpki::ring::RSA_PKCS1_2048_8192_SHA256),
        _ => None,
    };
    let sound = |proof| {
        Ok(if sig_alg.is_some() {
            proof
        } else {
            Proof::UnsupportedAlg
        })
    };
    let leaf_der = CertificateDer::from(x5c[0].as_slice());
    let leaf = EndEntityCert::try_from(&leaf_der)
        .map_err(|_| AttestError::Malformed("the attestation certificate doesn't parse"))?;
    if let Some(sig_alg) = sig_alg {
        leaf.verify_signature(sig_alg, signed, sig)
            .map_err(|_| AttestError::BadSignature)?;
    }
    x509::check_attestation_cert(&x5c[0], aaguid).map_err(AttestError::Profile)?;
    if roots.ders.contains(&x5c[0].as_slice()) {
        return Err(AttestError::Malformed(
            "the attestation certificate is a supplied root",
        ));
    }
    // webpki ignores KeyUsage while it builds the chain, and doesn't say
    // which certificates it chose, so every intermediate the authenticator
    // sent must be able to sign certificates (Terra #8294): a certificate
    // check, so it runs with no roots too (Terra #10099). An intermediate
    // that doesn't parse can't sign certificates either.
    if !x5c[1..].iter().all(|c| x509::may_sign_certificates(c)) {
        return Err(AttestError::Untrusted);
    }
    if roots.anchors.is_empty() {
        return sound(Proof::Unanchored);
    }
    let intermediates: Vec<CertificateDer<'_>> = x5c[1..]
        .iter()
        .map(|c| CertificateDer::from(c.as_slice()))
        .collect();
    let chained = leaf.verify_for_usage(
        CHAIN_ALGS,
        &roots.anchors,
        &intermediates,
        roots.now,
        AnyUsage,
        None,
        None,
    );
    // A chain signed with an algorithm this build can't verify is a lack
    // of proof, like an unverified statement algorithm (#9567); a chain
    // that verifies and fails against the roots, or is malformed, is
    // Untrusted. Which of the two a chain that doesn't verify gets
    // follows webpki's first error (an unsupported intermediate can hide
    // a bad link nearer the leaf; a cross-family issuer key reads as an
    // unsupported algorithm): best effort for audit, with the grade
    // unaffected, since both are machine-grade (#9777 residual).
    #[allow(deprecated)]
    if let Err(
        webpki::Error::UnsupportedSignatureAlgorithm
        | webpki::Error::UnsupportedSignatureAlgorithmContext(_)
        | webpki::Error::UnsupportedSignatureAlgorithmForPublicKey
        | webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_),
    ) = chained
    {
        return Ok(Proof::UnsupportedAlg);
    }
    chained.map_err(|_| AttestError::Untrusted)?;
    sound(Proof::Chained)
}

/// The signature algorithms an attestation chain may use.
static CHAIN_ALGS: &[&dyn SignatureVerificationAlgorithm] = &[
    webpki::ring::ECDSA_P256_SHA256,
    webpki::ring::ECDSA_P384_SHA384,
    webpki::ring::ED25519,
    webpki::ring::RSA_PKCS1_2048_8192_SHA256,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authdata::FLAG_UV;
    use crate::grade::Attestation;
    use crate::testutil::{self, Fmt, UP, make_credential};

    const RP: &str = "mdkg";
    const CHALLENGE: &[u8] = b"enrolment one";

    fn run(
        made: &testutil::Made,
        policy: Policy,
        roots: &Roots<'_>,
    ) -> Result<Attestation, AttestError> {
        verify(
            &made.auth_data,
            &made.statement,
            RP,
            CHALLENGE,
            policy,
            roots,
        )
        .map(|(d, a)| {
            assert_eq!(d.public_key, made.public_key);
            assert_eq!(d.credential_id, b"credential-1");
            if a == Proof::Chained {
                Attestation::Verified
            } else {
                Attestation::None
            }
        })
    }

    fn proof(made: &testutil::Made, roots: &Roots<'_>) -> Result<Proof, AttestError> {
        verify(
            &made.auth_data,
            &made.statement,
            RP,
            CHALLENGE,
            Policy::Present,
            roots,
        )
        .map(|(_, p)| p)
    }

    #[test]
    fn each_statement_says_what_it_proves() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let none = Roots::none(testutil::now());
        let packed = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        assert_eq!(proof(&packed, &roots), Ok(Proof::Chained));
        assert_eq!(proof(&packed, &none), Ok(Proof::Unanchored));
        let selfp = make_credential(RP, CHALLENGE, UP, Fmt::SelfPacked);
        assert_eq!(proof(&selfp, &roots), Ok(Proof::SelfAttested));
        let n = make_credential(RP, CHALLENGE, UP, Fmt::None);
        assert_eq!(proof(&n, &roots), Ok(Proof::NoneFormat));
    }

    #[test]
    fn only_a_chain_to_a_supplied_root_verifies() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Ok(Attestation::Verified)
        );
        // The same chain with no roots proves nothing, and isn't refused.
        let none = Roots::none(testutil::now());
        assert_eq!(run(&made, Policy::Present, &none), Ok(Attestation::None));
        // Roots the person supplied that the chain doesn't reach: refused.
        let other = testutil::other_root();
        let other = Roots::from_der(&other, testutil::now()).expect("roots");
        assert_eq!(
            run(&made, Policy::Present, &other),
            Err(AttestError::Untrusted)
        );
    }

    #[test]
    fn none_and_self_attestation_are_machine_grade_evidence() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        for fmt in [Fmt::None, Fmt::SelfPacked] {
            let made = make_credential(RP, CHALLENGE, UP, fmt);
            assert_eq!(
                run(&made, Policy::Present, &roots),
                Ok(Attestation::None),
                "{fmt:?}"
            );
        }
        // A format this build doesn't verify proves nothing.
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.statement.fmt = "tpm".into();
        assert_eq!(run(&made, Policy::Present, &roots), Ok(Attestation::None));
        assert_eq!(proof(&made, &roots), Ok(Proof::UnsupportedFormat));
        // Its RP id and flags are still checked.
        let mut other = make_credential("example.com", CHALLENGE, UP, Fmt::Packed);
        other.statement.fmt = "tpm".into();
        assert_eq!(proof(&other, &roots), Err(AttestError::RpMismatch));
    }

    #[test]
    fn an_attestation_for_another_enrolment_never_verifies() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        for fmt in [Fmt::Packed, Fmt::SelfPacked] {
            let made = make_credential(RP, b"enrolment two", UP, fmt);
            assert_eq!(
                run(&made, Policy::Present, &roots),
                Err(AttestError::BadSignature),
                "{fmt:?}"
            );
        }
        // Authenticator data changed after signing.
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.auth_data[34] ^= 1;
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::BadSignature)
        );
    }

    #[test]
    fn checks_the_rp_and_the_presence_flags() {
        let roots = Roots::none(testutil::now());
        let made = make_credential("example.com", CHALLENGE, UP, Fmt::None);
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::RpMismatch)
        );
        let made = make_credential(RP, CHALLENGE, 0, Fmt::None);
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::NotPresent)
        );
        let made = make_credential(RP, CHALLENGE, UP, Fmt::None);
        assert_eq!(
            run(&made, Policy::Verified, &roots),
            Err(AttestError::NotVerified)
        );
        let made = make_credential(RP, CHALLENGE, UP | FLAG_UV, Fmt::None);
        assert_eq!(run(&made, Policy::Verified, &roots), Ok(Attestation::None));
    }

    #[test]
    fn an_expired_chain_is_untrusted() {
        let root = testutil::root();
        // 2200-01-01, after the certificates expire.
        let roots = Roots::from_der(&root, unix_time_ms(7_258_118_400_000)).expect("roots");
        let made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Untrusted)
        );
    }

    #[test]
    fn refuses_malformed_statements() {
        let roots = Roots::none(testutil::now());
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::None);
        made.statement.sig = vec![1];
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.statement.alg = None;
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::SelfPacked);
        made.statement.alg = Some(ALG_EDDSA);
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.statement.alg = Some(-35);
        assert_eq!(
            proof(&made, &roots),
            Ok(Proof::UnsupportedAlg),
            "ES384: unverified, not a fault"
        );
        made.statement.x5c = vec![vec![0x30, 0x00]];
        assert!(
            matches!(proof(&made, &roots), Err(AttestError::Malformed(_))),
            "an unverified algorithm still has its leaf parsed (Claude #9612)"
        );
        // A packed statement with no signature, in any algorithm, is
        // malformed (Terra #10099).
        for alg in [-35, ALG_ES256] {
            let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
            made.statement.alg = Some(alg);
            made.statement.sig.clear();
            assert!(
                matches!(proof(&made, &roots), Err(AttestError::Malformed(_))),
                "alg {alg}"
            );
        }
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.statement.x5c = vec![testutil::LEAF.to_vec(); MAX_CHAIN + 1];
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        made.statement.x5c = vec![vec![0x30; MAX_CERT + 1]];
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        made.statement.x5c = vec![vec![0x30, 0x00]];
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
        assert!(Roots::from_der(&[CertificateDer::from(&b"junk"[..])], testutil::now()).is_err());
    }

    #[test]
    fn the_leaf_must_fit_the_packed_profile() {
        use crate::x509::ProfileError;
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        // An AAGUID extension that names the authenticator data's AAGUID
        // ([0xaa; 16] in the fake) verifies.
        let made = make_credential(RP, CHALLENGE, UP, Fmt::PackedWith(testutil::LEAF_AAGUID));
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Ok(Attestation::Verified)
        );
        // Each of these chains to the root, and is refused, with or
        // without roots supplied.
        let none = Roots::none(testutil::now());
        for (cert, why) in [
            (testutil::LEAF_AAGUID_OTHER, ProfileError::AaguidMismatch),
            (testutil::LEAF_OU, ProfileError::NoAttestationOu),
            (testutil::LEAF_CA, ProfileError::NotALeaf),
        ] {
            let made = make_credential(RP, CHALLENGE, UP, Fmt::PackedWith(cert));
            for r in [&roots, &none] {
                assert_eq!(
                    run(&made, Policy::Present, r),
                    Err(AttestError::Profile(why)),
                    "{why:?}"
                );
            }
        }
        let mut made = make_credential(RP, CHALLENGE, UP, Fmt::Packed);
        made.statement.x5c = vec![testutil::leaf_v1()];
        // webpki refuses a v1 leaf before the profile is read; the profile
        // refuses it too (x509::tests::enforces_the_profile).
        assert!(matches!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Malformed(_))
        ));
    }

    #[test]
    fn a_leaf_is_never_a_root() {
        // A leaf supplied as a root isn't a CA, so it's refused as a root.
        let leaf = vec![CertificateDer::from(testutil::LEAF)];
        assert!(matches!(
            Roots::from_der(&leaf, testutil::now()),
            Err(AttestError::Malformed(_))
        ));
        let both = vec![
            CertificateDer::from(testutil::ROOT),
            CertificateDer::from(testutil::LEAF),
        ];
        assert!(Roots::from_der(&both, testutil::now()).is_err());
    }

    #[test]
    fn an_rs256_attestation_is_read() {
        // Its self-signed leaf chains to no supplied root: machine-grade
        // without roots, refused with them.
        let made = make_credential(RP, CHALLENGE, UP, Fmt::PackedRsa);
        assert_eq!(
            run(&made, Policy::Present, &Roots::none(testutil::now())),
            Ok(Attestation::None)
        );
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Untrusted)
        );
        let mut bad = make_credential(RP, CHALLENGE, UP, Fmt::PackedRsa);
        bad.statement.sig[10] ^= 1;
        assert_eq!(
            run(&bad, Policy::Present, &roots),
            Err(AttestError::BadSignature)
        );
    }

    #[test]
    fn every_intermediate_must_be_allowed_to_sign_certificates() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let mut made = make_credential(
            RP,
            CHALLENGE,
            UP,
            Fmt::PackedWith(testutil::INTER_CERTSIGN_LEAF),
        );
        made.statement.x5c.push(testutil::INTER_CERTSIGN.to_vec());
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Ok(Attestation::Verified)
        );
        // CA true, but KeyUsage only cRLSign: webpki would accept it.
        let mut made = make_credential(
            RP,
            CHALLENGE,
            UP,
            Fmt::PackedWith(testutil::INTER_CRLONLY_LEAF),
        );
        made.statement.x5c.push(testutil::INTER_CRLONLY.to_vec());
        assert_eq!(
            run(&made, Policy::Present, &roots),
            Err(AttestError::Untrusted)
        );
        // Nor may such a certificate be a root.
        let crl = vec![CertificateDer::from(testutil::INTER_CRLONLY)];
        assert!(Roots::from_der(&crl, testutil::now()).is_err());
    }

    /// A chain signed with an algorithm the chain check doesn't verify
    /// (ECDSA with SHA-512) is Absent(UnsupportedAlg), not Untrusted
    /// (#9567); the same leaf's statement signature still verifies.
    #[test]
    fn a_chain_in_an_unverified_algorithm_is_unsupported_alg() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let made = make_credential(RP, CHALLENGE, UP, Fmt::PackedWith(testutil::LEAF_SHA512));
        assert_eq!(proof(&made, &roots), Ok(Proof::UnsupportedAlg));
        // Its statement signature over another enrolment still fails.
        // Only the expected challenge changes (Terra #9777).
        let other = verify(
            &made.auth_data,
            &made.statement,
            RP,
            b"another enrolment",
            Policy::Present,
            &roots,
        );
        assert_eq!(other.map(|(_, p)| p), Err(AttestError::BadSignature));
    }

    /// An unverified statement algorithm skips only the statement
    /// signature: the profile, leaf-is-a-root and chain checks still give
    /// Failed, and a sound statement is UnsupportedAlg with roots or
    /// without (Claude #9612, Terra #9660, #9757).
    #[test]
    fn an_unverified_statement_algorithm_still_runs_every_other_check() {
        use crate::x509::ProfileError;
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let none = Roots::none(testutil::now());
        let es384 = |fmt| {
            let mut made = make_credential(RP, CHALLENGE, UP, fmt);
            made.statement.alg = Some(-35);
            made
        };
        let sound = es384(Fmt::Packed);
        assert_eq!(proof(&sound, &roots), Ok(Proof::UnsupportedAlg));
        assert_eq!(proof(&sound, &none), Ok(Proof::UnsupportedAlg));
        assert!(matches!(
            proof(&es384(Fmt::PackedWith(testutil::LEAF_OU)), &none),
            Err(AttestError::Profile(ProfileError::NoAttestationOu))
        ));
        let other = testutil::other_root();
        let other = Roots::from_der(&other, testutil::now()).expect("roots");
        assert_eq!(proof(&sound, &other), Err(AttestError::Untrusted));
        // A malformed intermediate is refused with no roots too (Terra
        // #10099).
        let mut bad_inter = es384(Fmt::Packed);
        bad_inter.statement.x5c.push(vec![0x30, 0x00]);
        assert_eq!(proof(&bad_inter, &none), Err(AttestError::Untrusted));
        // A supplied root sent as the leaf fails the profile (it has no
        // attestation OU, and is a CA) before the supplied-root check.
        let mut leaf_root = es384(Fmt::Packed);
        leaf_root.statement.x5c = vec![testutil::ROOT.to_vec()];
        assert_eq!(
            proof(&leaf_root, &roots),
            Err(AttestError::Profile(ProfileError::NoAttestationOu))
        );
    }
}
