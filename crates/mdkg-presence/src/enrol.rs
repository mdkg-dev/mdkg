//! A person key's enrolment, as the coordinator checks it (S41).
//!
//! `person enroll` on the person's device creates the key and sends it with
//! its kind, its policy and, for a FIDO2 key, the credential's attestation.
//! The device's machine and the new key both sign that request (wire's
//! signatures; the new key's is the proof of possession), and the person
//! then compares wire's person pairing code on both screens. This module
//! checks what only the key's own evidence can show:
//! - the kind and its policy go together, and the key is the kind's type;
//! - a FIDO2 credential with a packed attestation was made for this
//!   enrolment: its client data hash is SHA-256 of [`challenge`], which
//!   binds the graph, the device's machine, the person, the kind, the
//!   policy and the commitment to the device's random value, so its
//!   attestation can't be lifted from another enrolment. A `none` or
//!   unverified-format statement, or a packed one in an unverified
//!   signature algorithm (whose signature isn't checked), carries nothing
//!   that binds it; the four-field proof of possession does (#8051), and
//!   such keys are only machine-grade;
//! - the attestation, recorded as `verified` or `none`, and the grade.
//!
//! The order of the checks (#9346): everything that doesn't depend on
//! the format runs first, and any failure there is Failed whatever the
//! format: the kind and policy, the authenticator data parsing, the RP id,
//! UP (and UV under `uv`), a named format, and, at approval, the attested
//! key being the pending key. Only then does the format decide: packed
//! signatures (which carry the challenge), then the Absent reasons in a
//! fixed order, the first that applies winning (#9346, #9478): `none`,
//! self attestation, an unverified format, an unverified signature
//! algorithm, no roots (unanchored), and last the
//! backup flags, which keep a verified chain from being Verified. Failed
//! dominates every Absent reason.

use mdkg_format::Diagnostic;
use mdkg_ops::wire::{PersonKeyKind, Signature, Signer};

use crate::attest::{self, AttestError, Proof, Roots, Statement};
use crate::cose::PublicKey;
use crate::grade::{self, Attestation, Grade, Policy};

/// The RP id of every mdkg FIDO2 credential, at attestation and assertion
/// alike: wire's.
pub use mdkg_wire::person::RP_ID;

/// The domain of [`challenge`], so it never collides with another mdkg
/// hash input.
pub const CHALLENGE_DOMAIN: &str = "mdkg-person-enroll-v1";

/// What an enrolment is for: every value [`challenge`] binds.
#[derive(Debug, Clone, Copy)]
pub struct Binding<'a> {
    pub graph_id: &'a str,
    /// The enrolling device's machine id.
    pub machine_id: &'a str,
    /// The `human:*` person the key is for.
    pub human: &'a str,
    pub kind: PersonKeyKind,
    pub policy: Policy,
    /// SHA-256 of the device's random value `Nc`, as sent in the request.
    pub commitment: &'a [u8; 32],
}

/// The FIDO2 `makeCredential` challenge for an enrolment. Each field is
/// length-prefixed, so no two bindings give the same bytes.
pub fn challenge(b: &Binding<'_>) -> Vec<u8> {
    let fields: [&[u8]; 7] = [
        CHALLENGE_DOMAIN.as_bytes(),
        b.graph_id.as_bytes(),
        b.machine_id.as_bytes(),
        b.human.as_bytes(),
        grade::kind_str(b.kind).as_bytes(),
        b.policy.as_str().as_bytes(),
        b.commitment,
    ];
    let mut out = Vec::new();
    for f in fields {
        let len = u32::try_from(f.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// What a candidate key signs to prove possession before approval
/// (orchestrator, #8051): wire's challenge over the pending request's id,
/// the host's `Ns`, the commitment to the device's `Nc`, and the graph id
/// (`pair::vectors::POSSESSION_CHALLENGE_HEX` is its vector). Together
/// they bind a proof to one pending request, the `Nc` the person compared
/// and this graph. They don't make it single-use: the same proof verifies
/// for that request as often as it is checked, so the host must record
/// the request as proved in the transaction that checks it, and approve
/// only a proved request (serve-store's and serve-core's part).
pub use mdkg_wire::person::{POSSESSION_LABEL, Possession, possession_challenge};

/// The candidate key's proof of possession for one pending request, as
/// the host issued it: its signature over [`possession_challenge`]. An
/// enclave key asks for Touch ID or the password; a FIDO2 key needs a
/// touch, and its PIN or biometric under `uv`, and the signer refuses an
/// assertion missing a flag before it returns; a file key just signs.
pub fn prove(signer: &dyn Signer, issued: &Possession<'_>) -> Result<Signature, Box<Diagnostic>> {
    signer.sign(&issued.challenge())
}

/// The key's evidence, as the enrolling device sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// An enclave key's public key, SEC1 uncompressed. Apple offers no
    /// attestation a coordinator can check for a command-line tool's key.
    Enclave { public_key: Vec<u8> },
    /// A file key's Ed25519 public key.
    File { public_key: Vec<u8> },
    /// A FIDO2 credential: `makeCredential`'s authenticator data and
    /// attestation statement.
    Fido2 {
        auth_data: Vec<u8>,
        statement: Statement,
    },
}

impl Evidence {
    pub fn kind(&self) -> PersonKeyKind {
        match self {
            Evidence::Enclave { .. } => PersonKeyKind::SecureEnclave,
            Evidence::File { .. } => PersonKeyKind::File,
            Evidence::Fido2 { .. } => PersonKeyKind::Fido2,
        }
    }
}

/// An enrolment that passed: what the pending request and the key's row
/// record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub kind: PersonKeyKind,
    pub policy: Policy,
    pub public_key: PublicKey,
    /// A FIDO2 credential's id, which its device needs for assertions.
    pub credential_id: Option<Vec<u8>>,
    pub attestation: Attestation,
    pub grade: Grade,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrolError {
    /// The evidence is for another kind than the binding names.
    KindMismatch,
    /// A policy the kind can't have: `uv` for anything but FIDO2.
    PolicyNotAllowed,
    /// The public key isn't the kind's key type, or isn't well formed.
    BadKey,
    /// A file key without the person's explicit opt-in.
    FileKeyNotAllowed,
    /// A machine-grade key approved without `--machine-grade`.
    MachineGradeNotAcknowledged,
    /// The attested credential's key isn't the pending request's key.
    KeyMismatch,
    Attest(AttestError),
}

impl std::fmt::Display for EnrolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnrolError::KindMismatch => f.write_str("the key's evidence is for another kind"),
            EnrolError::PolicyNotAllowed => f.write_str("the key's kind can't have that policy"),
            EnrolError::BadKey => f.write_str("the public key isn't a valid key of its kind"),
            EnrolError::FileKeyNotAllowed => f.write_str(
                "a file key is machine-grade and enrols only with --allow-file-key for that person",
            ),
            EnrolError::MachineGradeNotAcknowledged => f.write_str(
                "this key is machine-grade: its signatures prove the device, not the person; approve it with --machine-grade",
            ),
            EnrolError::KeyMismatch => {
                f.write_str("the attested credential's key isn't the pending request's key")
            }
            EnrolError::Attest(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for EnrolError {}

/// Why evidence is Failed, as a closed set of stable words (#9632, #9828):
/// the event and the refusal use [`FailedReason::as_str`], never free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedReason {
    /// The evidence or its CBOR is unreadable, the format is missing or
    /// empty, or a leaf certificate is itself a supplied root.
    Malformed,
    RpMismatch,
    NoUp,
    /// UV clear under the `uv` policy.
    NoUv,
    KeyMismatch,
    /// A verified statement's signature fails, a wrong challenge for
    /// packed included.
    BadSignature,
    /// A chain that does verify fails against the supplied roots.
    ChainFailed,
    /// A leaf whose signature verifies but that doesn't fit the packed
    /// attestation profile.
    Profile,
}

impl FailedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FailedReason::Malformed => "malformed",
            FailedReason::RpMismatch => "rp_mismatch",
            FailedReason::NoUp => "no_up",
            FailedReason::NoUv => "no_uv",
            FailedReason::KeyMismatch => "key_mismatch",
            FailedReason::BadSignature => "bad_signature",
            FailedReason::ChainFailed => "chain_failed",
            FailedReason::Profile => "profile",
        }
    }
}

impl EnrolError {
    /// The closed Failed word for this error (#9632, #9828). `None` for
    /// the errors that are about the request, not its evidence: another
    /// kind than the binding names, a policy the kind can't have, a file
    /// key without the opt-in, and a machine-grade key approved without
    /// `--machine-grade` (which [`approval`] returns, never an outcome).
    pub fn failed_reason(&self) -> Option<FailedReason> {
        Some(match self {
            EnrolError::KindMismatch
            | EnrolError::PolicyNotAllowed
            | EnrolError::FileKeyNotAllowed
            | EnrolError::MachineGradeNotAcknowledged => return None,
            // An enclave or file key that isn't a valid key of its type:
            // its evidence is unreadable.
            EnrolError::BadKey => FailedReason::Malformed,
            EnrolError::KeyMismatch => FailedReason::KeyMismatch,
            EnrolError::Attest(e) => match e {
                AttestError::AuthData(_) | AttestError::Malformed(_) => FailedReason::Malformed,
                AttestError::RpMismatch => FailedReason::RpMismatch,
                AttestError::NotPresent => FailedReason::NoUp,
                AttestError::NotVerified => FailedReason::NoUv,
                AttestError::BadSignature => FailedReason::BadSignature,
                AttestError::Untrusted => FailedReason::ChainFailed,
                AttestError::Profile(_) => FailedReason::Profile,
            },
        })
    }
}

/// The approver's acknowledgement that a key is machine-grade
/// (`person approve --machine-grade`; orchestrator, #8059).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineGradeAck {
    Acknowledged,
    NotAcknowledged,
}

/// Whether `person approve` may commit this enrolment: a machine-grade
/// key (an unattested or backup-eligible FIDO2 key, any enclave key, any
/// file key) needs the approver's `--machine-grade`. Returns the grade,
/// which the output and the approval record show.
pub fn approval(checked: &Checked, ack: MachineGradeAck) -> Result<Grade, EnrolError> {
    match (checked.grade, ack) {
        (Grade::MachineGrade, MachineGradeAck::NotAcknowledged) => {
            Err(EnrolError::MachineGradeNotAcknowledged)
        }
        (grade, _) => Ok(grade),
    }
}

/// The person's explicit opt-in to a file key (`--allow-file-key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKeyOptIn {
    Allowed,
    NotAllowed,
}

/// Why an enrolment's attestation proves no hardware (#9152). Each is
/// a fact about the evidence, never a swallowed error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentReason {
    /// An enclave or file key: no attestation object.
    NoObject,
    /// A FIDO2 statement in format `none`.
    NoneFormat,
    /// Packed self attestation: possession, not hardware.
    SelfAttested,
    /// A chain, with no roots supplied to check it against.
    NoRoots,
    /// The BE or BS flag: the credential may live on hardware the
    /// attestation never covered, so it is machine-grade whatever the
    /// chain says (#8059, #9152).
    BackupEligible,
    /// A FIDO2 attestation in a format this build doesn't verify
    /// (fido-u2f, tpm, android-key, apple, ...): no proof either way
    /// (#9152, on #9281).
    UnsupportedFormat,
    /// A packed statement, or its x5c certificate chain, signed with an
    /// algorithm this build doesn't verify (#9478, #9567). Every other
    /// check of the statement passed (Claude #9612).
    UnsupportedAlg,
}

/// An enrolment's attestation, as serve-store's approval maps it (#9152):
/// exactly one of three outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A FIDO2 chain to a supplied root, over this enrolment's challenge,
    /// for the pending key, with neither backup flag.
    Verified,
    /// No evidence of hardware; the key is machine-grade.
    Absent(AbsentReason),
    /// The evidence doesn't hold: a chain that fails against the supplied
    /// roots, malformed evidence, the wrong challenge (a signature over
    /// another enrolment), or an attested key that differs from the
    /// pending one. A format this build doesn't verify is Absent, not
    /// Failed. [`EnrolError::failed_reason`] gives its closed word.
    Failed(EnrolError),
}

impl Outcome {
    /// The record's attestation, or the failure: a Failed outcome never
    /// flattens into `none` (Terra #9354).
    pub fn attestation(&self) -> Result<Attestation, &EnrolError> {
        match self {
            Outcome::Verified => Ok(Attestation::Verified),
            Outcome::Absent(_) => Ok(Attestation::None),
            Outcome::Failed(e) => Err(e),
        }
    }
}

/// The attestation outcome of an enrolment whose pending request named
/// `pending_key`: what serve-store's approval maps (#9152).
pub fn attestation(
    binding: &Binding<'_>,
    evidence: &Evidence,
    pending_key: &PublicKey,
    file_key: FileKeyOptIn,
    roots: &Roots<'_>,
) -> Outcome {
    // Every format-independent check, the pending key included, runs
    // before any format-specific one (#9346, Terra #9562).
    match format_independent(binding, evidence, file_key) {
        Err(e) => return Outcome::Failed(e),
        Ok(key) if &key != pending_key => return Outcome::Failed(EnrolError::KeyMismatch),
        Ok(_) => {}
    }
    match evaluate(binding, evidence, file_key, roots) {
        Err(e) => Outcome::Failed(e),
        Ok((_, _, outcome)) => outcome,
    }
}

/// The checks that don't depend on the attestation format: the kind and
/// its policy, the key's type, the file-key opt-in, and for FIDO2
/// [`attest::check_common`]. Returns the evidence's public key.
fn format_independent(
    binding: &Binding<'_>,
    evidence: &Evidence,
    file_key: FileKeyOptIn,
) -> Result<PublicKey, EnrolError> {
    if evidence.kind() != binding.kind {
        return Err(EnrolError::KindMismatch);
    }
    if !binding.policy.allowed_for(binding.kind) {
        return Err(EnrolError::PolicyNotAllowed);
    }
    match evidence {
        Evidence::Enclave { public_key } => match PublicKey::from_bytes(public_key) {
            Some(k @ PublicKey::P256(_)) => Ok(k),
            _ => Err(EnrolError::BadKey),
        },
        Evidence::File { public_key } => {
            if file_key != FileKeyOptIn::Allowed {
                return Err(EnrolError::FileKeyNotAllowed);
            }
            match PublicKey::from_bytes(public_key) {
                Some(k @ PublicKey::Ed25519(_)) => Ok(k),
                _ => Err(EnrolError::BadKey),
            }
        }
        Evidence::Fido2 {
            auth_data,
            statement,
        } => attest::check_common(auth_data, statement, RP_ID, binding.policy)
            .map(|d| d.public_key)
            .map_err(EnrolError::Attest),
    }
}

/// Checks an enrolment's evidence against what it is for; a failed
/// attestation is an error, and anything else gives the record.
pub fn check(
    binding: &Binding<'_>,
    evidence: &Evidence,
    file_key: FileKeyOptIn,
    roots: &Roots<'_>,
) -> Result<Checked, EnrolError> {
    let (public_key, credential_id, outcome) = evaluate(binding, evidence, file_key, roots)?;
    let attestation = outcome.attestation().map_err(Clone::clone)?;
    Ok(Checked {
        kind: binding.kind,
        policy: binding.policy,
        public_key,
        credential_id,
        attestation,
        grade: grade::grade(binding.kind, attestation),
    })
}

/// The evidence's key, credential id and outcome; `Err` is the failure.
fn evaluate(
    binding: &Binding<'_>,
    evidence: &Evidence,
    file_key: FileKeyOptIn,
    roots: &Roots<'_>,
) -> Result<(PublicKey, Option<Vec<u8>>, Outcome), EnrolError> {
    let key = format_independent(binding, evidence, file_key)?;
    let absent = Outcome::Absent(AbsentReason::NoObject);
    match evidence {
        Evidence::Enclave { .. } | Evidence::File { .. } => Ok((key, None, absent)),
        Evidence::Fido2 {
            auth_data,
            statement,
        } => {
            let (data, proof) = attest::verify(
                auth_data,
                statement,
                RP_ID,
                &challenge(binding),
                binding.policy,
                roots,
            )
            .map_err(EnrolError::Attest)?;
            // The Absent reasons in their fixed order, the first that
            // applies winning (#9346, #9478): none, self, unsupported
            // format, unsupported algorithm, unanchored, backup. So the backup flags decide only for a
            // chain that verified, which they keep from being Verified.
            let outcome = match proof {
                Proof::NoneFormat => Outcome::Absent(AbsentReason::NoneFormat),
                Proof::SelfAttested => Outcome::Absent(AbsentReason::SelfAttested),
                Proof::UnsupportedFormat => Outcome::Absent(AbsentReason::UnsupportedFormat),
                Proof::UnsupportedAlg => Outcome::Absent(AbsentReason::UnsupportedAlg),
                Proof::Unanchored => Outcome::Absent(AbsentReason::NoRoots),
                Proof::Chained if data.backed_up_or_eligible() => {
                    Outcome::Absent(AbsentReason::BackupEligible)
                }
                Proof::Chained => Outcome::Verified,
            };
            Ok((data.public_key, Some(data.credential_id), outcome))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authdata::FLAG_UV;
    use crate::testutil::{self, Fmt, UP, make_credential};

    const COMMITMENT: [u8; 32] = [7; 32];

    fn binding(kind: PersonKeyKind, policy: Policy) -> Binding<'static> {
        Binding {
            graph_id: "graph-1",
            machine_id: "mdkg:machine",
            human: "human:nick",
            kind,
            policy,
            commitment: &COMMITMENT,
        }
    }

    fn fido2(b: &Binding<'_>, flags: u8, fmt: Fmt) -> Evidence {
        let made = make_credential(RP_ID, &challenge(b), flags, fmt);
        Evidence::Fido2 {
            auth_data: made.auth_data,
            statement: made.statement,
        }
    }

    fn p256_bytes() -> Vec<u8> {
        let mut k = vec![9u8; 65];
        k[0] = 0x04;
        k
    }

    #[test]
    fn only_an_attested_fido2_key_is_present_grade() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let b = binding(PersonKeyKind::Fido2, Policy::Verified);
        let c = check(
            &b,
            &fido2(&b, UP | FLAG_UV, Fmt::Packed),
            FileKeyOptIn::NotAllowed,
            &roots,
        )
        .expect("enrols");
        assert_eq!(
            (c.attestation, c.grade),
            (Attestation::Verified, Grade::Present)
        );
        assert_eq!(c.credential_id.as_deref(), Some(&b"credential-1"[..]));
        let c = check(
            &b,
            &fido2(&b, UP | FLAG_UV, Fmt::None),
            FileKeyOptIn::NotAllowed,
            &roots,
        )
        .expect("enrols");
        assert_eq!(
            (c.attestation, c.grade),
            (Attestation::None, Grade::MachineGrade)
        );

        let b = binding(PersonKeyKind::SecureEnclave, Policy::Present);
        let e = Evidence::Enclave {
            public_key: p256_bytes(),
        };
        let c = check(&b, &e, FileKeyOptIn::NotAllowed, &roots).expect("enrols");
        assert_eq!(
            (c.attestation, c.grade),
            (Attestation::None, Grade::MachineGrade)
        );

        let b = binding(PersonKeyKind::File, Policy::Present);
        let e = Evidence::File {
            public_key: vec![3; 32],
        };
        let c = check(&b, &e, FileKeyOptIn::Allowed, &roots).expect("enrols");
        assert_eq!(
            (c.attestation, c.grade),
            (Attestation::None, Grade::MachineGrade)
        );
        assert_eq!(c.grade.as_str(), "machine-grade");
    }

    #[test]
    fn a_file_key_needs_the_opt_in() {
        let roots = Roots::none(testutil::now());
        let b = binding(PersonKeyKind::File, Policy::Present);
        let e = Evidence::File {
            public_key: vec![3; 32],
        };
        assert_eq!(
            check(&b, &e, FileKeyOptIn::NotAllowed, &roots),
            Err(EnrolError::FileKeyNotAllowed)
        );
    }

    #[test]
    fn refuses_a_mismatched_kind_policy_or_key() {
        let roots = Roots::none(testutil::now());
        let enclave = Evidence::Enclave {
            public_key: p256_bytes(),
        };
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        assert_eq!(
            check(&b, &enclave, FileKeyOptIn::Allowed, &roots),
            Err(EnrolError::KindMismatch)
        );
        let b = binding(PersonKeyKind::SecureEnclave, Policy::Verified);
        assert_eq!(
            check(&b, &enclave, FileKeyOptIn::Allowed, &roots),
            Err(EnrolError::PolicyNotAllowed)
        );
        let b = binding(PersonKeyKind::File, Policy::Verified);
        let file = Evidence::File {
            public_key: vec![3; 32],
        };
        assert_eq!(
            check(&b, &file, FileKeyOptIn::Allowed, &roots),
            Err(EnrolError::PolicyNotAllowed)
        );
        // An enclave key must be P-256, a file key Ed25519.
        let b = binding(PersonKeyKind::SecureEnclave, Policy::Present);
        let e = Evidence::Enclave {
            public_key: vec![3; 32],
        };
        assert_eq!(
            check(&b, &e, FileKeyOptIn::Allowed, &roots),
            Err(EnrolError::BadKey)
        );
        let b = binding(PersonKeyKind::File, Policy::Present);
        let e = Evidence::File {
            public_key: p256_bytes(),
        };
        assert_eq!(
            check(&b, &e, FileKeyOptIn::Allowed, &roots),
            Err(EnrolError::BadKey)
        );
    }

    #[test]
    fn a_credential_made_for_another_binding_is_refused() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let made_for = binding(PersonKeyKind::Fido2, Policy::Present);
        let other_commitment = [8u8; 32];
        let variants = [
            Binding {
                graph_id: "graph-2",
                ..made_for
            },
            Binding {
                machine_id: "mdkg:other",
                ..made_for
            },
            Binding {
                human: "human:eve",
                ..made_for
            },
            Binding {
                commitment: &other_commitment,
                ..made_for
            },
        ];
        for fmt in [Fmt::Packed, Fmt::SelfPacked] {
            let e = fido2(&made_for, UP | FLAG_UV, fmt);
            for b in &variants {
                assert_eq!(
                    check(b, &e, FileKeyOptIn::NotAllowed, &roots),
                    Err(EnrolError::Attest(AttestError::BadSignature)),
                    "{fmt:?} {b:?}"
                );
            }
            // The policy is bound too: a credential made for "up" doesn't
            // enrol as "uv", even with the flag set.
            let b = Binding {
                policy: Policy::Verified,
                ..made_for
            };
            assert_eq!(
                check(&b, &e, FileKeyOptIn::NotAllowed, &roots),
                Err(EnrolError::Attest(AttestError::BadSignature))
            );
        }
    }

    #[test]
    fn the_challenge_is_length_prefixed() {
        let a = Binding {
            graph_id: "ab",
            machine_id: "c",
            ..binding(PersonKeyKind::Fido2, Policy::Present)
        };
        let b = Binding {
            graph_id: "a",
            machine_id: "bc",
            ..a
        };
        assert_ne!(challenge(&a), challenge(&b));
        let c = challenge(&a);
        assert_eq!(&c[..4], &(CHALLENGE_DOMAIN.len() as u32).to_be_bytes());
        assert!(c.ends_with(&COMMITMENT));
    }

    #[test]
    fn a_backup_eligible_credential_is_machine_grade() {
        use crate::authdata::{FLAG_BE, FLAG_BS};
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        for flag in [FLAG_BE, FLAG_BE | FLAG_BS] {
            let c = check(
                &b,
                &fido2(&b, UP | flag, Fmt::Packed),
                FileKeyOptIn::NotAllowed,
                &roots,
            )
            .expect("enrols");
            assert_eq!(
                (c.attestation, c.grade),
                (Attestation::None, Grade::MachineGrade)
            );
        }
    }

    #[test]
    fn approving_a_machine_grade_key_needs_the_acknowledgement() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let present = check(
            &b,
            &fido2(&b, UP, Fmt::Packed),
            FileKeyOptIn::NotAllowed,
            &roots,
        )
        .expect("enrols");
        let unattested = check(
            &b,
            &fido2(&b, UP, Fmt::None),
            FileKeyOptIn::NotAllowed,
            &roots,
        )
        .expect("enrols");
        let eb = binding(PersonKeyKind::SecureEnclave, Policy::Present);
        let enclave = check(
            &eb,
            &Evidence::Enclave {
                public_key: p256_bytes(),
            },
            FileKeyOptIn::NotAllowed,
            &roots,
        )
        .expect("enrols");
        let fb = binding(PersonKeyKind::File, Policy::Present);
        let file = check(
            &fb,
            &Evidence::File {
                public_key: vec![3; 32],
            },
            FileKeyOptIn::Allowed,
            &roots,
        )
        .expect("enrols");
        for ack in [
            MachineGradeAck::Acknowledged,
            MachineGradeAck::NotAcknowledged,
        ] {
            assert_eq!(approval(&present, ack), Ok(Grade::Present));
        }
        for c in [&unattested, &enclave, &file] {
            assert_eq!(
                approval(c, MachineGradeAck::NotAcknowledged),
                Err(EnrolError::MachineGradeNotAcknowledged),
                "{:?}",
                c.kind
            );
            assert_eq!(
                approval(c, MachineGradeAck::Acknowledged),
                Ok(Grade::MachineGrade)
            );
        }
    }

    #[test]
    fn the_attestation_outcome_is_one_of_three() {
        use crate::authdata::FLAG_BE;
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let none = Roots::none(testutil::now());
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let out = |ev: &Evidence, roots: &Roots<'_>| {
            let key = match check(&b, ev, FileKeyOptIn::NotAllowed, &none) {
                Ok(c) => c.public_key,
                Err(_) => PublicKey::Ed25519([0; 32]),
            };
            attestation(&b, ev, &key, FileKeyOptIn::NotAllowed, roots)
        };
        assert_eq!(out(&fido2(&b, UP, Fmt::Packed), &roots), Outcome::Verified);
        assert_eq!(
            out(&fido2(&b, UP, Fmt::None), &roots),
            Outcome::Absent(AbsentReason::NoneFormat)
        );
        assert_eq!(
            out(&fido2(&b, UP, Fmt::SelfPacked), &roots),
            Outcome::Absent(AbsentReason::SelfAttested)
        );
        assert_eq!(
            out(&fido2(&b, UP, Fmt::Packed), &none),
            Outcome::Absent(AbsentReason::NoRoots)
        );
        // A valid chain with BE set is never Verified.
        assert_eq!(
            out(&fido2(&b, UP | FLAG_BE, Fmt::Packed), &roots),
            Outcome::Absent(AbsentReason::BackupEligible)
        );
        let eb = binding(PersonKeyKind::SecureEnclave, Policy::Present);
        let enclave = Evidence::Enclave {
            public_key: p256_bytes(),
        };
        let key = PublicKey::from_bytes(&p256_bytes()).expect("key");
        assert_eq!(
            attestation(&eb, &enclave, &key, FileKeyOptIn::NotAllowed, &roots),
            Outcome::Absent(AbsentReason::NoObject)
        );
        // Failures carry a typed reason.
        let other = testutil::other_root();
        let other = Roots::from_der(&other, testutil::now()).expect("roots");
        assert_eq!(
            out(&fido2(&b, UP, Fmt::Packed), &other),
            Outcome::Failed(EnrolError::Attest(AttestError::Untrusted))
        );
        let lifted = Binding {
            graph_id: "graph-2",
            ..b
        };
        let lifted_ev = fido2(&lifted, UP, Fmt::Packed);
        let lifted_key = match &lifted_ev {
            Evidence::Fido2 { auth_data, .. } => {
                crate::authdata::parse_attested(auth_data)
                    .expect("parses")
                    .public_key
            }
            _ => unreachable!(),
        };
        assert_eq!(
            attestation(
                &b,
                &lifted_ev,
                &lifted_key,
                FileKeyOptIn::NotAllowed,
                &roots
            ),
            Outcome::Failed(EnrolError::Attest(AttestError::BadSignature)),
            "the wrong challenge, for the pending key"
        );
        let mut tpm = fido2(&b, UP, Fmt::Packed);
        if let Evidence::Fido2 { statement, .. } = &mut tpm {
            statement.fmt = "tpm".into();
        }
        assert_eq!(
            out(&tpm, &roots),
            Outcome::Absent(AbsentReason::UnsupportedFormat)
        );
        let ev = fido2(&b, UP, Fmt::Packed);
        assert_eq!(
            attestation(
                &b,
                &ev,
                &PublicKey::Ed25519([1; 32]),
                FileKeyOptIn::NotAllowed,
                &roots
            ),
            Outcome::Failed(EnrolError::KeyMismatch),
            "an attested key that isn't the pending key"
        );
        let mut bad = fido2(&b, UP, Fmt::Packed);
        if let Evidence::Fido2 { auth_data, .. } = &mut bad {
            auth_data.truncate(20);
        }
        assert!(matches!(
            out(&bad, &roots),
            Outcome::Failed(EnrolError::Attest(AttestError::AuthData(_)))
        ));
    }

    /// A TPM-attested key (or any format this build doesn't verify)
    /// enrols, at machine grade (#9152, on #9281).
    #[test]
    fn an_unverified_format_enrols_machine_grade() {
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        for fmt in ["tpm", "apple", "android-key", "fido-u2f"] {
            let mut ev = fido2(&b, UP, Fmt::Packed);
            if let Evidence::Fido2 { statement, .. } = &mut ev {
                statement.fmt = fmt.into();
            }
            let c = check(&b, &ev, FileKeyOptIn::NotAllowed, &roots).expect("enrols");
            assert_eq!(
                (c.attestation, c.grade),
                (Attestation::None, Grade::MachineGrade),
                "{fmt}"
            );
            assert_eq!(
                approval(&c, MachineGradeAck::NotAcknowledged),
                Err(EnrolError::MachineGradeNotAcknowledged),
                "approved only with --machine-grade"
            );
            assert_eq!(
                approval(&c, MachineGradeAck::Acknowledged),
                Ok(Grade::MachineGrade)
            );
        }
    }

    /// An unverified format still meets every format-independent check:
    /// each failure there is Failed, not Absent (#9346).
    #[test]
    fn an_unverified_format_still_fails_the_format_independent_checks() {
        use crate::authdata::FLAG_UV;
        let roots = Roots::none(testutil::now());
        let tpm = |b: &Binding<'_>, rp: &str, flags: u8, fmt: &str| {
            let made = make_credential(rp, &challenge(b), flags, Fmt::Packed);
            let mut statement = made.statement;
            statement.fmt = fmt.into();
            (
                Evidence::Fido2 {
                    auth_data: made.auth_data,
                    statement,
                },
                made.public_key,
            )
        };
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let outcome = |b: &Binding<'_>, ev: &Evidence, key: &PublicKey| {
            attestation(b, ev, key, FileKeyOptIn::NotAllowed, &roots)
        };
        let (ev, key) = tpm(&b, RP_ID, UP, "tpm");
        assert_eq!(
            outcome(&b, &ev, &key),
            Outcome::Absent(AbsentReason::UnsupportedFormat)
        );
        let (ev, key) = tpm(&b, "example.com", UP, "tpm");
        assert_eq!(
            outcome(&b, &ev, &key),
            Outcome::Failed(EnrolError::Attest(AttestError::RpMismatch))
        );
        let (ev, key) = tpm(&b, RP_ID, 0, "tpm");
        assert_eq!(
            outcome(&b, &ev, &key),
            Outcome::Failed(EnrolError::Attest(AttestError::NotPresent))
        );
        let uv = binding(PersonKeyKind::Fido2, Policy::Verified);
        let (ev, key) = tpm(&uv, RP_ID, UP, "tpm");
        assert_eq!(
            outcome(&uv, &ev, &key),
            Outcome::Failed(EnrolError::Attest(AttestError::NotVerified))
        );
        let (ev, key) = tpm(&uv, RP_ID, UP | FLAG_UV, "tpm");
        assert_eq!(
            outcome(&uv, &ev, &key),
            Outcome::Absent(AbsentReason::UnsupportedFormat)
        );
        let (ev, _) = tpm(&b, RP_ID, UP, "tpm");
        assert_eq!(
            outcome(&b, &ev, &PublicKey::Ed25519([1; 32])),
            Outcome::Failed(EnrolError::KeyMismatch)
        );
        let (ev, key) = tpm(&b, RP_ID, UP, "");
        assert!(matches!(
            outcome(&b, &ev, &key),
            Outcome::Failed(EnrolError::Attest(AttestError::Malformed(_)))
        ));
        assert!(
            Outcome::Failed(EnrolError::KeyMismatch)
                .attestation()
                .is_err()
        );
    }

    /// The Absent reasons' fixed order: the first that applies wins, and
    /// the backup flags come last (#9346).
    #[test]
    fn the_absent_reasons_have_a_fixed_order() {
        use crate::authdata::FLAG_BE;
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let none = Roots::none(testutil::now());
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let reason = |ev: &Evidence, r: &Roots<'_>| {
            let key = check(&b, ev, FileKeyOptIn::NotAllowed, &none)
                .expect("checks")
                .public_key;
            attestation(&b, ev, &key, FileKeyOptIn::NotAllowed, r)
        };
        let be = UP | FLAG_BE;
        assert_eq!(
            reason(&fido2(&b, be, Fmt::None), &roots),
            Outcome::Absent(AbsentReason::NoneFormat)
        );
        assert_eq!(
            reason(&fido2(&b, be, Fmt::SelfPacked), &roots),
            Outcome::Absent(AbsentReason::SelfAttested)
        );
        let mut tpm = fido2(&b, be, Fmt::Packed);
        if let Evidence::Fido2 { statement, .. } = &mut tpm {
            statement.fmt = "tpm".into();
        }
        assert_eq!(
            reason(&tpm, &roots),
            Outcome::Absent(AbsentReason::UnsupportedFormat)
        );
        let mut es384 = fido2(&b, be, Fmt::Packed);
        if let Evidence::Fido2 { statement, .. } = &mut es384 {
            statement.alg = Some(-35);
        }
        assert_eq!(
            reason(&es384, &roots),
            Outcome::Absent(AbsentReason::UnsupportedAlg)
        );
        assert_eq!(
            reason(&es384, &none),
            Outcome::Absent(AbsentReason::UnsupportedAlg),
            "before unanchored"
        );
        assert_eq!(
            reason(&fido2(&b, be, Fmt::Packed), &none),
            Outcome::Absent(AbsentReason::NoRoots)
        );
        assert_eq!(
            reason(&fido2(&b, be, Fmt::Packed), &roots),
            Outcome::Absent(AbsentReason::BackupEligible)
        );
        assert_eq!(
            reason(&fido2(&b, UP, Fmt::Packed), &roots),
            Outcome::Verified
        );
    }

    /// A key mismatch is reported before a format-specific failure: a
    /// self-packed statement for another key with a bad signature is
    /// KeyMismatch (Terra #9562).
    #[test]
    fn a_key_mismatch_is_reported_before_a_bad_signature() {
        let roots = Roots::none(testutil::now());
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let mut ev = fido2(&b, UP, Fmt::SelfPacked);
        if let Evidence::Fido2 { statement, .. } = &mut ev {
            statement.sig[5] ^= 1;
        }
        assert_eq!(
            attestation(
                &b,
                &ev,
                &PublicKey::Ed25519([1; 32]),
                FileKeyOptIn::NotAllowed,
                &roots
            ),
            Outcome::Failed(EnrolError::KeyMismatch)
        );
        // Another credential's P-256 key, same type as this one's, so a
        // check of only the key's type would miss it (Terra #9757); with a
        // bad signature, and with an unverified statement algorithm, whose
        // Absent comes only after every format-independent check (#9752).
        let other = fido2(&b, UP, Fmt::Packed);
        let other_key = check(&b, &other, FileKeyOptIn::NotAllowed, &roots)
            .expect("checks")
            .public_key;
        assert!(matches!(other_key, PublicKey::P256(_)));
        let mut es384 = fido2(&b, UP, Fmt::Packed);
        if let Evidence::Fido2 { statement, .. } = &mut es384 {
            statement.alg = Some(-35);
        }
        for ev in [&ev, &es384] {
            assert_eq!(
                attestation(&b, ev, &other_key, FileKeyOptIn::NotAllowed, &roots),
                Outcome::Failed(EnrolError::KeyMismatch)
            );
        }
    }

    /// Every Failed error maps to one closed word (#9632, #9828); the
    /// request errors map to none.
    #[test]
    fn each_failure_has_one_closed_word() {
        use crate::authdata::AuthDataError;
        use crate::x509::ProfileError;
        let cases = [
            (EnrolError::BadKey, Some("malformed")),
            (EnrolError::KeyMismatch, Some("key_mismatch")),
            (
                EnrolError::Attest(AttestError::AuthData(AuthDataError::Truncated)),
                Some("malformed"),
            ),
            (
                EnrolError::Attest(AttestError::Malformed("x")),
                Some("malformed"),
            ),
            (
                EnrolError::Attest(AttestError::RpMismatch),
                Some("rp_mismatch"),
            ),
            (EnrolError::Attest(AttestError::NotPresent), Some("no_up")),
            (EnrolError::Attest(AttestError::NotVerified), Some("no_uv")),
            (
                EnrolError::Attest(AttestError::BadSignature),
                Some("bad_signature"),
            ),
            (
                EnrolError::Attest(AttestError::Untrusted),
                Some("chain_failed"),
            ),
            (
                EnrolError::Attest(AttestError::Profile(ProfileError::NotALeaf)),
                Some("profile"),
            ),
            (EnrolError::KindMismatch, None),
            (EnrolError::PolicyNotAllowed, None),
            (EnrolError::FileKeyNotAllowed, None),
            (EnrolError::MachineGradeNotAcknowledged, None),
        ];
        for (e, word) in cases {
            assert_eq!(e.failed_reason().map(FailedReason::as_str), word, "{e}");
        }
    }

    /// BS alone (backed up, BE clear) is still backup, never Verified; and
    /// a failure with backup flags set is Failed, not Absent (Terra #9562).
    #[test]
    fn backup_state_alone_and_failures_with_backup_flags() {
        use crate::authdata::{FLAG_BE, FLAG_BS};
        let root = testutil::root();
        let roots = Roots::from_der(&root, testutil::now()).expect("roots");
        let none = Roots::none(testutil::now());
        let b = binding(PersonKeyKind::Fido2, Policy::Present);
        let key_of = |ev: &Evidence| {
            check(&b, ev, FileKeyOptIn::NotAllowed, &none)
                .expect("checks")
                .public_key
        };
        let bs = fido2(&b, UP | FLAG_BS, Fmt::Packed);
        assert_eq!(
            attestation(&b, &bs, &key_of(&bs), FileKeyOptIn::NotAllowed, &roots),
            Outcome::Absent(AbsentReason::BackupEligible)
        );
        // A chain failing against other roots, with BE and BS set: Failed.
        let other = testutil::other_root();
        let other = Roots::from_der(&other, testutil::now()).expect("roots");
        let both = fido2(&b, UP | FLAG_BE | FLAG_BS, Fmt::Packed);
        assert_eq!(
            attestation(&b, &both, &key_of(&both), FileKeyOptIn::NotAllowed, &other),
            Outcome::Failed(EnrolError::Attest(AttestError::Untrusted))
        );
        // A bad signature with BE set: Failed.
        let mut bad = fido2(&b, UP | FLAG_BE, Fmt::Packed);
        let key = key_of(&bad);
        if let Evidence::Fido2 { statement, .. } = &mut bad {
            statement.sig[5] ^= 1;
        }
        assert_eq!(
            attestation(&b, &bad, &key, FileKeyOptIn::NotAllowed, &roots),
            Outcome::Failed(EnrolError::Attest(AttestError::BadSignature))
        );
    }
}
