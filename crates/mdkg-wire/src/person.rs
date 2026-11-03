//! Person signatures (S41): verifying the `mdkg-person` signature over
//! a person signature's base, by the enrolled key's kind.
//!
//! | Kind | `alg` | Public key | Signature |
//! |---|---|---|---|
//! | file key | `ed25519` | 32 bytes | 64 bytes |
//! | Secure Enclave | `ecdsa-p256-sha256` | SEC1 uncompressed point, 65 bytes | `r ‖ s`, 64 bytes (RFC 9421 section 3.3.4) |
//! | FIDO2, ES256 | `fido2-es256` | SEC1 uncompressed point, 65 bytes | an assertion, below |
//! | FIDO2, Ed25519 | `fido2-ed25519` | 32 bytes | an assertion, below |
//!
//! **A FIDO2 assertion** is WebAuthn's: the authenticator signs
//! `authenticatorData ‖ clientDataHash`, and here `clientDataHash` is the
//! SHA-256 of the person signature's base. The signature's bytes are
//! `len ‖ authenticatorData ‖ sig`, where `len` is the authenticator
//! data's length as two big-endian bytes, and `sig` is DER for ES256 or 64
//! bytes for Ed25519. The authenticator data must name the relying party
//! [`RP_ID`] (its SHA-256 in the first 32 bytes), and carry the "user
//! present" flag, and "user verified" too under the policy "uv". An
//! assertion missing a flag its policy needs is refused (R129, SC68).
//!
//! **What a signature proves** is its [`Grade`], decided per signature
//! (orchestrator's calls on Terra #6769, #7636, #7836 and #9478): an
//! operation takes the grade of the request's own verified signature,
//! never one cached from the key, since one key's assertions can grade
//! differently (#9534). [`Grade::Present`] needs both a FIDO2 key whose
//! enrolled record says its hardware attestation was verified against the
//! supplied roots ([`PersonAttestations`]), never the kind a key claims,
//! since any software key can build a FIDO2-shaped assertion; and this
//! assertion carrying neither the backup-eligible nor the backup-state
//! flag. Every other signature that verifies is
//! [`Grade::MachineGrade`]: file keys, Secure Enclave keys (enrolled
//! without attestation; a signature carries no presence flag), unattested
//! FIDO2 keys, and any assertion with the backup-eligible or backup-state
//! flag set, whose credential can be or was copied off its authenticator
//! (Terra 2 #9478).

use mdkg_ops::wire::{KeyDirectory, PersonKey, PersonKeyKind, Signature, profile};
use rustls::pki_types::{SignatureVerificationAlgorithm, alg_id};
use sha2::{Digest, Sha256};

use crate::keys::Ed25519;
use crate::pair::{Nonce, encode};
use crate::sign::{ALG_FIDO2_ED25519, ALG_FIDO2_ES256};

/// The relying party id FIDO2 person keys are created and asserted for.
pub const RP_ID: &str = "mdkg";

/// WebAuthn's authenticator data flags.
const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
/// Backup eligible and backup state: a credential that can be, or has
/// been, copied off its authenticator (a synced passkey).
const FLAG_BE: u8 = 0x08;
const FLAG_BS: u8 = 0x10;
/// rpIdHash (32), flags (1), signCount (4).
const AUTH_DATA_MIN: usize = 37;

/// What a verified person signature proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// A FIDO2 key with a verified hardware attestation, and an assertion
    /// with neither BE nor BS set (#9524). An accepted assertion carried
    /// "user present", and "user verified" under the policy "uv"; one
    /// without them is refused, so the person was there.
    Present,
    /// It doesn't prove the person was there: at most that some holder of
    /// the key signed (a copied credential doesn't even name one device).
    /// Shown as `machine-grade` wherever it appears.
    MachineGrade,
}

impl Grade {
    /// The name serve-store and the CLI show.
    pub fn as_str(self) -> &'static str {
        match self {
            Grade::Present => "present",
            Grade::MachineGrade => "machine-grade",
        }
    }
}

/// What enrolment recorded about a person key's hardware attestation:
/// evidence about the key, not a grade (#7906). serve-store's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attestation {
    /// Checked against the supplied roots at enrolment.
    Verified,
    /// Not checked, or not checkable.
    None,
}

impl Attestation {
    /// The string serve-store stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Attestation::Verified => "verified",
            Attestation::None => "none",
        }
    }
}

/// The attestation each enrolled person key's record holds; an unknown key
/// is [`Attestation::None`]. Backed by the store. Only
/// [`PersonVerifier::verify`] turns it into a [`Grade`]: Present for a
/// FIDO2 key alone, whose assertion carried the flags its policy needs
/// (#7636, #7836).
pub trait PersonAttestations {
    fn attestation(&self, key_id: &str) -> Attestation;
}

/// A person key that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPerson {
    pub key_id: String,
    /// The `human:*` actor the key belongs to.
    pub human: String,
    /// This signature's grade: Present only for an attested FIDO2 key's
    /// assertion with neither BE nor BS set. Not a property of the key:
    /// don't cache it (#9534).
    pub grade: Grade,
}

/// What a person check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonCheck {
    Verified(VerifiedPerson),
    /// Unknown or revoked: `api.person_unknown`.
    Unknown,
    /// A bad signature, an `alg` that isn't the key's, another relying
    /// party, or a FIDO2 assertion missing a flag its policy needs (SC68).
    Invalid,
}

/// Verifies person signatures against a graph's enrolled person keys.
pub struct PersonVerifier<D> {
    keys: D,
}

impl<D> std::fmt::Debug for PersonVerifier<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonVerifier").finish_non_exhaustive()
    }
}

impl<D: KeyDirectory + PersonAttestations> PersonVerifier<D> {
    pub fn new(keys: D) -> Self {
        Self { keys }
    }

    /// Checks the person signature `signature`, labelled `alg`, over
    /// `base`, by the key `key_id`.
    pub fn verify(&self, key_id: &str, alg: &str, base: &[u8], signature: &[u8]) -> PersonCheck {
        let Some(key) = self.keys.person(key_id) else {
            return PersonCheck::Unknown;
        };
        if key.revoked || key.key_id != key_id {
            return PersonCheck::Unknown;
        }
        if !verify_key(&key, alg, base, signature) {
            return PersonCheck::Invalid;
        }
        // An assertion whose credential can be or was copied off its
        // authenticator (BE or BS) proves no single device, so it is
        // machine-grade whatever enrolment recorded (Terra 2 #9478).
        let present = key.kind == PersonKeyKind::Fido2
            && self.keys.attestation(&key.key_id) == Attestation::Verified
            && !backed_up(signature);
        PersonCheck::Verified(VerifiedPerson {
            grade: if present {
                Grade::Present
            } else {
                Grade::MachineGrade
            },
            key_id: key.key_id,
            human: key.person,
        })
    }
}

/// The label of [`possession_challenge`]'s input.
pub const POSSESSION_LABEL: &str = "mdkg-person-possession-v1";

/// What a candidate person key signs to prove possession at enrolment,
/// after the host answered its first request (orchestrator's enrolment
/// contract, #8036): [`POSSESSION_LABEL`], the pending request's id, its
/// fresh `Ns`, the candidate's commitment to `Nc`, and the graph id, each
/// (the label too) framed as the pairing codes are
/// ([`crate::pair::encode`]). The server values bind each proof to one
/// enrolment, so a proof seen once can't enrol the same key under another
/// request (#7985). [`crate::pair::vectors::POSSESSION_CHALLENGE_HEX`] is
/// the published vector.
pub fn possession_challenge(
    pending_id: &str,
    ns: &Nonce,
    commitment: &[u8; 32],
    graph_id: &str,
) -> Vec<u8> {
    encode(
        POSSESSION_LABEL,
        &[pending_id.as_bytes(), ns, commitment, graph_id.as_bytes()],
    )
}

/// The values a proof of possession binds, as the host issued them.
#[derive(Debug, Clone, Copy)]
pub struct Possession<'a> {
    pub pending_id: &'a str,
    pub ns: &'a Nonce,
    /// The candidate's commitment to its `Nc`.
    pub commitment: &'a [u8; 32],
    pub graph_id: &'a str,
}

impl Possession<'_> {
    pub fn challenge(&self) -> Vec<u8> {
        possession_challenge(self.pending_id, self.ns, self.commitment, self.graph_id)
    }
}

/// Whether `signature` by a candidate key, not yet enrolled, verifies
/// over the [`possession_challenge`] of this pending request, FIDO2 flags
/// included: enrolment's proof of possession (#7906, #7985, #8036). A yes
/// or no, never a grade; it looks nothing up, so revoked or unknown keys
/// are the caller's to refuse.
pub fn proves_possession(
    candidate: &PersonKey,
    alg: &str,
    issued: &Possession<'_>,
    signature: &[u8],
) -> bool {
    verify_key(candidate, alg, &issued.challenge(), signature)
}

/// Checks one signature by one enrolled key, by its kind (the table
/// above), FIDO2 flags included. The `alg` a request names must be the
/// key's own. This is not the grade: [`PersonVerifier`] grades each
/// signature from the enrolled record and the assertion's own flags, and
/// is the only public way to get one (#7818, #9534).
pub(crate) fn verify_key(key: &PersonKey, alg: &str, base: &[u8], signature: &[u8]) -> bool {
    match (key.kind, alg) {
        (PersonKeyKind::File, a) if a == profile::ALG => ed25519(&key.public_key, base, signature),
        (PersonKeyKind::SecureEnclave, a) if a == profile::ALG_P256 => {
            let Ok(fixed) = <[u8; 64]>::try_from(signature) else {
                return false;
            };
            p256(&key.public_key, base, &fixed_to_der(&fixed))
        }
        (PersonKeyKind::Fido2, ALG_FIDO2_ES256) if key.public_key.len() == 65 => {
            fido2(key, base, signature, |msg, sig| {
                p256(&key.public_key, msg, sig)
            })
        }
        (PersonKeyKind::Fido2, ALG_FIDO2_ED25519) if key.public_key.len() == 32 => {
            fido2(key, base, signature, |msg, sig| {
                ed25519(&key.public_key, msg, sig)
            })
        }
        _ => false,
    }
}

fn ed25519(public_key: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    match (<[u8; 32]>::try_from(public_key), <[u8; 64]>::try_from(sig)) {
        (Ok(pk), Ok(sig)) => Ed25519.verify(&pk, msg, &sig),
        _ => false,
    }
}

/// ECDSA P-256 with SHA-256, DER signature, through the ring provider's
/// verifier that TLS uses too.
fn p256(public_key: &[u8], msg: &[u8], der: &[u8]) -> bool {
    if public_key.len() != 65 || public_key[0] != 0x04 {
        return false;
    }
    let provider = rustls::crypto::ring::default_provider();
    let alg: Option<&dyn SignatureVerificationAlgorithm> = provider
        .signature_verification_algorithms
        .all
        .iter()
        .copied()
        .find(|a| {
            a.public_key_alg_id() == alg_id::ECDSA_P256
                && a.signature_alg_id() == alg_id::ECDSA_SHA256
        });
    alg.is_some_and(|a| a.verify_signature(public_key, msg, der).is_ok())
}

/// RFC 9421's fixed `r ‖ s` as the DER `SEQUENCE { INTEGER r, INTEGER s }`
/// the verifier takes.
fn fixed_to_der(fixed: &[u8; 64]) -> Vec<u8> {
    fn int(bytes: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
        if v.first().is_none_or(|b| b & 0x80 != 0) {
            v.insert(0, 0);
        }
        let mut out = vec![0x02, v.len() as u8];
        out.extend(v);
        out
    }
    let mut body = int(&fixed[..32]);
    body.extend(int(&fixed[32..]));
    let mut out = vec![0x30, body.len() as u8];
    out.extend(body);
    out
}

/// What a FIDO2 authenticator signs for a person signature over `base`:
/// `authenticatorData ‖ SHA-256(base)` (WebAuthn's `clientDataHash` is the
/// base's hash here).
pub fn assertion_message(auth_data: &[u8], base: &[u8]) -> Vec<u8> {
    let mut msg = auth_data.to_vec();
    msg.extend_from_slice(&Sha256::digest(base));
    msg
}

/// A FIDO2 assertion as a person signature's bytes: `len ‖
/// authenticatorData ‖ sig`, `len` two big-endian bytes. `None` when the
/// authenticator data is shorter than WebAuthn's 37 bytes or longer than
/// two bytes can say, or `sig` is empty.
pub fn encode_assertion(auth_data: &[u8], sig: &[u8]) -> Option<Signature> {
    let len = u16::try_from(auth_data.len()).ok()?;
    if auth_data.len() < AUTH_DATA_MIN || sig.is_empty() {
        return None;
    }
    let mut out = len.to_be_bytes().to_vec();
    out.extend_from_slice(auth_data);
    out.extend_from_slice(sig);
    Some(Signature(out))
}

/// [`encode_assertion`]'s inverse: the authenticator data and the
/// signature, or `None` when the bytes aren't one.
pub fn decode_assertion(signature: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = signature.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_be_bytes(*len));
    if len < AUTH_DATA_MIN || rest.len() <= len {
        return None;
    }
    Some(rest.split_at(len))
}

/// Whether a (verified) FIDO2 assertion carries the backup-eligible or
/// backup-state flag.
fn backed_up(signature: &[u8]) -> bool {
    decode_assertion(signature).is_some_and(|(auth, _)| auth[32] & (FLAG_BE | FLAG_BS) != 0)
}

/// A FIDO2 assertion over `base` (the module's format), checked with
/// `check(message, signature)`.
fn fido2(
    key: &PersonKey,
    base: &[u8],
    signature: &[u8],
    check: impl Fn(&[u8], &[u8]) -> bool,
) -> bool {
    let Some((auth_data, sig)) = decode_assertion(signature) else {
        return false;
    };
    // The flags only after the signature, so a forged assertion learns
    // nothing from which check refused it.
    if auth_data[..32] != Sha256::digest(RP_ID.as_bytes())[..]
        || !check(&assertion_message(auth_data, base), sig)
    {
        return false;
    }
    let flags = auth_data[32];
    flags & FLAG_UP != 0 && (!key.user_verified || flags & FLAG_UV != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MachineKey;
    use mdkg_ops::wire::{MachineKey as Enrolled, Signer};
    use std::collections::HashMap;

    /// Person keys, and the ids enrolled with a verified attestation.
    #[derive(Default)]
    struct Keys(HashMap<String, PersonKey>, Vec<String>);
    impl PersonAttestations for Keys {
        fn attestation(&self, key_id: &str) -> Attestation {
            if self.1.iter().any(|k| k == key_id) {
                Attestation::Verified
            } else {
                Attestation::None
            }
        }
    }
    impl KeyDirectory for Keys {
        fn machine(&self, _: &str) -> Option<Enrolled> {
            None
        }
        fn person(&self, key_id: &str) -> Option<PersonKey> {
            self.0.get(key_id).cloned()
        }
    }

    fn file_key(id: &str, seed: u8, revoked: bool) -> (PersonKey, MachineKey) {
        let signer = MachineKey::from_parts(id, [seed; 32]);
        (
            PersonKey {
                key_id: id.into(),
                person: "human:nick".into(),
                kind: PersonKeyKind::File,
                public_key: signer.public_key().to_vec(),
                user_verified: false,
                revoked,
            },
            signer,
        )
    }

    fn verifier(keys: Vec<PersonKey>) -> PersonVerifier<Keys> {
        attested(keys, &[])
    }

    fn attested(keys: Vec<PersonKey>, ids: &[&str]) -> PersonVerifier<Keys> {
        PersonVerifier::new(Keys(
            keys.into_iter().map(|k| (k.key_id.clone(), k)).collect(),
            ids.iter().map(|s| s.to_string()).collect(),
        ))
    }

    #[test]
    fn a_file_key_verifies_and_is_machine_grade() {
        let (k, s) = file_key("pk-1", 1, false);
        let v = verifier(vec![k]);
        let sig = s.sign(b"base").unwrap().0;
        assert_eq!(
            v.verify("pk-1", "ed25519", b"base", &sig),
            PersonCheck::Verified(VerifiedPerson {
                key_id: "pk-1".into(),
                human: "human:nick".into(),
                grade: Grade::MachineGrade,
            })
        );
        assert_eq!(
            v.verify("pk-1", "ed25519", b"basE", &sig),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("pk-1", "ecdsa-p256-sha256", b"base", &sig),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("pk-1", "fido2-ed25519", b"base", &sig),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("pk-2", "ed25519", b"base", &sig),
            PersonCheck::Unknown
        );
    }

    #[test]
    fn a_file_key_is_machine_grade_even_if_recorded_present() {
        let (k, s) = file_key("pk-1", 1, false);
        let sig = s.sign(b"base").unwrap().0;
        assert!(matches!(
            attested(vec![k], &["pk-1"]).verify("pk-1", "ed25519", b"base", &sig),
            PersonCheck::Verified(VerifiedPerson {
                grade: Grade::MachineGrade,
                ..
            })
        ));
    }

    #[test]
    fn a_candidate_proves_possession_without_being_enrolled() {
        // #7906: enrolment checks a key before any directory holds it.
        // #7985, #8036: the proof is bound to one pending request.
        let ns = [9u8; 32];
        let c = crate::pair::commitment(&[2u8; 32]);
        let issued = Possession {
            pending_id: "pending-1",
            ns: &ns,
            commitment: &c,
            graph_id: "graph-1",
        };
        let challenge = issued.challenge();
        let (file, s) = file_key("new", 6, false);
        let sig = s.sign(&challenge).unwrap().0;
        assert!(proves_possession(&file, "ed25519", &issued, &sig));
        // Replayed for another request, Ns, commitment or graph: refused.
        let ns2 = [8u8; 32];
        let c2 = crate::pair::commitment(&[3u8; 32]);
        for other in [
            Possession {
                pending_id: "pending-2",
                ..issued
            },
            Possession { ns: &ns2, ..issued },
            Possession {
                commitment: &c2,
                ..issued
            },
            Possession {
                graph_id: "graph-2",
                ..issued
            },
        ] {
            assert!(!proves_possession(&file, "ed25519", &other, &sig));
        }
        let signer = MachineKey::from_parts("x", [7u8; 32]);
        let fido2 = PersonKey {
            key_id: "new-f".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: signer.public_key().to_vec(),
            user_verified: true,
            revoked: false,
        };
        let both = assertion(&signer, &challenge, RP_ID, FLAG_UP | FLAG_UV);
        assert!(proves_possession(&fido2, ALG_FIDO2_ED25519, &issued, &both));
        let other = Possession {
            pending_id: "pending-2",
            ..issued
        };
        assert!(!proves_possession(&fido2, ALG_FIDO2_ED25519, &other, &both));
        // The flags its policy needs are checked here too.
        let up = assertion(&signer, &challenge, RP_ID, FLAG_UP);
        assert!(!proves_possession(&fido2, ALG_FIDO2_ED25519, &issued, &up));
        assert!(!proves_possession(&fido2, ALG_FIDO2_ES256, &issued, &both));
    }

    #[test]
    fn the_possession_challenge_matches_the_published_vector() {
        use crate::pair::vectors::{
            NC, NS, PERSON, POSSESSION_CHALLENGE_HEX, POSSESSION_PENDING_ID,
        };
        let c = crate::pair::commitment(&NC);
        let got = possession_challenge(POSSESSION_PENDING_ID, &NS, &c, PERSON.graph_id);
        let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, POSSESSION_CHALLENGE_HEX);
        // Framed by hand: every item, the label too, length-prefixed.
        let mut want = Vec::new();
        for item in [
            POSSESSION_LABEL.as_bytes(),
            POSSESSION_PENDING_ID.as_bytes(),
            &NS,
            &c,
            PERSON.graph_id.as_bytes(),
        ] {
            want.extend_from_slice(&(item.len() as u32).to_be_bytes());
            want.extend_from_slice(item);
        }
        assert_eq!(got, want);
    }

    #[test]
    fn a_revoked_key_is_unknown() {
        let (k, s) = file_key("pk-1", 1, true);
        let v = verifier(vec![k]);
        let sig = s.sign(b"base").unwrap().0;
        assert_eq!(
            v.verify("pk-1", "ed25519", b"base", &sig),
            PersonCheck::Unknown
        );
    }

    fn assertion(signer: &MachineKey, base: &[u8], rp: &str, flags: u8) -> Vec<u8> {
        let mut auth = Sha256::digest(rp.as_bytes()).to_vec();
        auth.push(flags);
        auth.extend_from_slice(&7u32.to_be_bytes());
        let mut msg = auth.clone();
        msg.extend_from_slice(&Sha256::digest(base));
        let sig = signer.sign(&msg).unwrap().0;
        let mut out = (auth.len() as u16).to_be_bytes().to_vec();
        out.extend(auth);
        out.extend(sig);
        out
    }

    #[test]
    fn the_assertion_encoding_round_trips_and_matches_the_helper() {
        let signer = MachineKey::from_parts("x", [4u8; 32]);
        let raw = assertion(&signer, b"base", RP_ID, FLAG_UP);
        let (auth, sig) = decode_assertion(&raw).unwrap();
        assert_eq!(auth.len(), AUTH_DATA_MIN);
        assert_eq!(sig.len(), 64);
        assert_eq!(encode_assertion(auth, sig).unwrap().0, raw);
        assert_eq!(
            signer.sign(&assertion_message(auth, b"base")).unwrap().0,
            sig
        );
        // Too-short authenticator data, an empty signature, a length past
        // the end, and data too long for two bytes are all refused.
        assert!(encode_assertion(&auth[..36], sig).is_none());
        assert!(encode_assertion(auth, &[]).is_none());
        assert!(encode_assertion(&vec![0u8; 65_536], sig).is_none());
        assert!(decode_assertion(&raw[..2 + AUTH_DATA_MIN]).is_none());
        assert!(decode_assertion(&[0x00]).is_none());
        let mut short = raw.clone();
        short[1] = 36;
        assert!(decode_assertion(&short).is_none());
    }

    #[test]
    fn a_fido2_assertion_needs_presence_and_the_relying_party() {
        let signer = MachineKey::from_parts("x", [4u8; 32]);
        let mut key = PersonKey {
            key_id: "fk-1".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: signer.public_key().to_vec(),
            user_verified: false,
            revoked: false,
        };
        // A software key builds a FIDO2-shaped assertion with "user
        // present" set. Enrolled without a verified attestation, it is
        // machine-grade, whatever kind it claimed (Terra #6769).
        let v = verifier(vec![key.clone()]);
        let ok = assertion(&signer, b"base", RP_ID, FLAG_UP);
        assert_eq!(
            v.verify("fk-1", ALG_FIDO2_ED25519, b"base", &ok),
            PersonCheck::Verified(VerifiedPerson {
                key_id: "fk-1".into(),
                human: "human:nick".into(),
                grade: Grade::MachineGrade,
            })
        );
        // Only the enrolled record's verified attestation makes it present.
        assert!(matches!(
            attested(vec![key.clone()], &["fk-1"]).verify("fk-1", ALG_FIDO2_ED25519, b"base", &ok),
            PersonCheck::Verified(VerifiedPerson {
                grade: Grade::Present,
                ..
            })
        ));
        assert_eq!(
            v.verify("fk-1", ALG_FIDO2_ED25519, b"other", &ok),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("fk-1", ALG_FIDO2_ES256, b"base", &ok),
            PersonCheck::Invalid
        );
        // SC68: without "user present", even an attested key's assertion
        // is refused.
        let absent = assertion(&signer, b"base", RP_ID, 0);
        assert_eq!(
            attested(vec![key.clone()], &["fk-1"]).verify(
                "fk-1",
                ALG_FIDO2_ED25519,
                b"base",
                &absent
            ),
            PersonCheck::Invalid
        );
        let elsewhere = assertion(&signer, b"base", "example.com", FLAG_UP);
        assert_eq!(
            v.verify("fk-1", ALG_FIDO2_ED25519, b"base", &elsewhere),
            PersonCheck::Invalid
        );
        // A key whose policy needs user verification: "user present"
        // alone is refused, both flags present.
        key.user_verified = true;
        let v = attested(vec![key], &["fk-1"]);
        assert_eq!(
            v.verify("fk-1", ALG_FIDO2_ED25519, b"base", &ok),
            PersonCheck::Invalid
        );
        let uv = assertion(&signer, b"base", RP_ID, FLAG_UP | FLAG_UV);
        assert!(matches!(
            v.verify("fk-1", ALG_FIDO2_ED25519, b"base", &uv),
            PersonCheck::Verified(VerifiedPerson {
                grade: Grade::Present,
                ..
            })
        ));
        // Truncated forms.
        for cut in [0, 1, 2, 30, 39, uv.len() - 64] {
            assert_eq!(
                v.verify("fk-1", ALG_FIDO2_ED25519, b"base", &uv[..cut]),
                PersonCheck::Invalid
            );
        }
    }

    #[test]
    fn present_needs_an_attested_fido2_key_and_its_flags() {
        // Orchestrator's calls on Terra #7636 and #7836, case by case.
        let signer = MachineKey::from_parts("x", [5u8; 32]);
        let fido2 = |uv: bool| PersonKey {
            key_id: "fk".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: signer.public_key().to_vec(),
            user_verified: uv,
            revoked: false,
        };
        let check = |key: PersonKey, ids: &[&str], flags: u8| {
            let sig = assertion(&signer, b"base", RP_ID, flags);
            attested(vec![key], ids).verify("fk", ALG_FIDO2_ED25519, b"base", &sig)
        };
        let grade = |key: PersonKey, ids: &[&str], flags: u8| match check(key, ids, flags) {
            PersonCheck::Verified(v) => Some(v.grade),
            other => panic!("expected a verified signature, got {other:?}"),
        };
        // Attested, with the flags its policy needs: present.
        assert_eq!(grade(fido2(false), &["fk"], FLAG_UP), Some(Grade::Present));
        assert_eq!(
            grade(fido2(true), &["fk"], FLAG_UP | FLAG_UV),
            Some(Grade::Present)
        );
        // Unattested, with the flags its policy needs: machine-grade.
        assert_eq!(
            grade(fido2(true), &[], FLAG_UP | FLAG_UV),
            Some(Grade::MachineGrade)
        );
        // A flag its policy needs is missing: refused, attested or not
        // (SC68): only an assertion with its policy's flags is graded.
        // Exactly Invalid, never Unknown: the key is enrolled (#7958).
        for ids in [&["fk"][..], &[]] {
            assert_eq!(check(fido2(false), ids, 0), PersonCheck::Invalid);
            assert_eq!(check(fido2(true), ids, FLAG_UP), PersonCheck::Invalid);
            assert_eq!(check(fido2(true), ids, FLAG_UV), PersonCheck::Invalid);
        }
        // A signature that doesn't verify, or names another relying
        // party, is refused whatever the record says.
        let mut bad = assertion(&signer, b"base", RP_ID, FLAG_UP);
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(
            attested(vec![fido2(false)], &["fk"]).verify("fk", ALG_FIDO2_ED25519, b"base", &bad),
            PersonCheck::Invalid
        );
        let elsewhere = assertion(&signer, b"base", "example.com", FLAG_UP);
        assert_eq!(
            attested(vec![fido2(false)], &["fk"]).verify(
                "fk",
                ALG_FIDO2_ED25519,
                b"base",
                &elsewhere
            ),
            PersonCheck::Invalid
        );
    }

    #[test]
    fn a_backed_up_credential_is_never_present() {
        // Terra 2 #9478: the key was enrolled (attested) with BE=BS=0; a
        // later assertion with BS (or BE) set is machine-grade, whatever
        // the enrolment record says.
        let signer = MachineKey::from_parts("x", [8u8; 32]);
        let key = PersonKey {
            key_id: "fb".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: signer.public_key().to_vec(),
            user_verified: false,
            revoked: false,
        };
        let v = attested(vec![key], &["fb"]);
        let grade = |flags: u8| match v.verify(
            "fb",
            ALG_FIDO2_ED25519,
            b"base",
            &assertion(&signer, b"base", RP_ID, flags),
        ) {
            PersonCheck::Verified(p) => p.grade,
            other => panic!("expected verified, got {other:?}"),
        };
        assert_eq!(grade(FLAG_UP), Grade::Present);
        assert_eq!(grade(FLAG_UP | FLAG_BS), Grade::MachineGrade);
        assert_eq!(grade(FLAG_UP | FLAG_BE), Grade::MachineGrade);
        assert_eq!(grade(FLAG_UP | FLAG_BE | FLAG_BS), Grade::MachineGrade);
    }

    #[test]
    fn a_forged_backed_up_assertion_is_invalid_not_machine_grade() {
        // Terra 1 #9534: the backup flags are read only after the
        // assertion verifies, so a forged one carrying them is refused.
        let signer = MachineKey::from_parts("x", [9u8; 32]);
        let key = PersonKey {
            key_id: "fz".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: signer.public_key().to_vec(),
            user_verified: false,
            revoked: false,
        };
        let v = attested(vec![key], &["fz"]);
        for flags in [
            FLAG_UP | FLAG_BS,
            FLAG_UP | FLAG_BE,
            FLAG_UP | FLAG_BE | FLAG_BS,
        ] {
            let mut sig = assertion(&signer, b"base", RP_ID, flags);
            let last = sig.len() - 1;
            sig[last] ^= 1;
            assert_eq!(
                v.verify("fz", ALG_FIDO2_ED25519, b"base", &sig),
                PersonCheck::Invalid
            );
        }
        // And a valid one without UP, backup flags or not, is refused too.
        let no_up = assertion(&signer, b"base", RP_ID, FLAG_BS);
        assert_eq!(
            v.verify("fz", ALG_FIDO2_ED25519, b"base", &no_up),
            PersonCheck::Invalid
        );
    }

    #[test]
    fn fixed_ecdsa_signatures_become_der() {
        let mut fixed = [0u8; 64];
        fixed[0] = 0x80; // r needs a leading zero
        fixed[63] = 1; // s is 1
        let der = fixed_to_der(&fixed);
        assert_eq!(&der[..4], &[0x30, 38, 0x02, 33]);
        assert_eq!(der[4], 0);
        assert_eq!(&der[der.len() - 3..], &[0x02, 1, 1]);
        assert_eq!(fixed_to_der(&[0u8; 64]), vec![0x30, 6, 2, 1, 0, 2, 1, 0]);
    }

    // RFC 6979 appendix A.2.5: P-256, SHA-256, the message "sample".
    const P256_X: &str = "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6";
    const P256_Y: &str = "7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299";
    const P256_R: &str = "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716";
    const P256_S: &str = "f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8";
    /// The same appendix's private key, a published test value.
    const P256_D: &str = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";

    /// RFC 6979's public key as a SEC1 point.
    fn p256_point() -> Vec<u8> {
        let mut pk = vec![4u8];
        pk.extend(hex(P256_X));
        pk.extend(hex(P256_Y));
        pk
    }

    /// Signs `msg` with RFC 6979's P-256 key through the ring provider's
    /// signer; DER, as an ES256 authenticator returns it.
    fn p256_sign(msg: &[u8]) -> Vec<u8> {
        use rustls::SignatureScheme;
        use rustls::pki_types::{PrivateKeyDer, PrivateSec1KeyDer};
        // SEC1 ECPrivateKey: version 1, the key, prime256v1, the point.
        let mut der = vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20];
        der.extend(hex(P256_D));
        der.extend([
            0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07,
        ]);
        der.extend([0xa1, 0x44, 0x03, 0x42, 0x00]);
        der.extend(p256_point());
        let key = rustls::crypto::ring::sign::any_ecdsa_type(&PrivateKeyDer::Sec1(
            PrivateSec1KeyDer::from(der),
        ))
        .expect("the RFC 6979 key loads");
        key.choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .expect("ES256")
            .sign(msg)
            .expect("signs")
    }

    #[test]
    fn a_fido2_es256_assertion_verifies() {
        // Terra #7775: a real ES256 assertion, DER signature and all.
        let key = PersonKey {
            key_id: "fe".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::Fido2,
            public_key: p256_point(),
            user_verified: true,
            revoked: false,
        };
        let mut auth = Sha256::digest(RP_ID.as_bytes()).to_vec();
        auth.push(FLAG_UP | FLAG_UV);
        auth.extend_from_slice(&9u32.to_be_bytes());
        let sig = p256_sign(&assertion_message(&auth, b"base"));
        let wire = encode_assertion(&auth, &sig).unwrap().0;
        let v = attested(vec![key.clone()], &["fe"]);
        assert_eq!(
            v.verify("fe", ALG_FIDO2_ES256, b"base", &wire),
            PersonCheck::Verified(VerifiedPerson {
                key_id: "fe".into(),
                human: "human:nick".into(),
                grade: Grade::Present,
            })
        );
        assert!(verify_key(&key, ALG_FIDO2_ES256, b"base", &wire));
        // Another base, the Ed25519 alg, or a flipped signature byte fail.
        assert_eq!(
            v.verify("fe", ALG_FIDO2_ES256, b"other", &wire),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("fe", ALG_FIDO2_ED25519, b"base", &wire),
            PersonCheck::Invalid
        );
        let mut bad = wire.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(
            v.verify("fe", ALG_FIDO2_ES256, b"base", &bad),
            PersonCheck::Invalid
        );
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_secure_enclave_signature_verifies_rfc6979() {
        let mut pk = vec![4u8];
        pk.extend(hex(P256_X));
        pk.extend(hex(P256_Y));
        let mut sig = hex(P256_R);
        sig.extend(hex(P256_S));
        let key = PersonKey {
            key_id: "se-1".into(),
            person: "human:nick".into(),
            kind: PersonKeyKind::SecureEnclave,
            public_key: pk,
            user_verified: false,
            revoked: false,
        };
        assert!(matches!(
            verifier(vec![key.clone()]).verify("se-1", "ecdsa-p256-sha256", b"sample", &sig),
            PersonCheck::Verified(VerifiedPerson {
                grade: Grade::MachineGrade,
                ..
            })
        ));
        // An enclave key is machine-grade even if a record claimed it
        // attested: its signature carries no presence flag (#7636).
        let v = attested(vec![key], &["se-1"]);
        assert!(matches!(
            v.verify("se-1", "ecdsa-p256-sha256", b"sample", &sig),
            PersonCheck::Verified(VerifiedPerson {
                grade: Grade::MachineGrade,
                ..
            })
        ));
        assert_eq!(
            v.verify("se-1", "ecdsa-p256-sha256", b"samplf", &sig),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("se-1", "ed25519", b"sample", &sig),
            PersonCheck::Invalid
        );
        // DER is not the profile's form.
        let der = fixed_to_der(&sig.clone().try_into().unwrap());
        assert_eq!(
            v.verify("se-1", "ecdsa-p256-sha256", b"sample", &der),
            PersonCheck::Invalid
        );
    }
}
