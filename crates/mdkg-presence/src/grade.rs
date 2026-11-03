//! What a person key proves: its kind, the presence policy it was enrolled
//! with, whether its hardware was attested, and the grade that follows.
//!
//! A key's kind is what the enrolling device says. Only an attestation
//! verified at enrolment makes it evidence of hardware (Terra 2 #6271), so
//! presence is proven only by a FIDO2 key whose attestation verified;
//! every other key is `machine-grade` (S41, R129, #7691). wire grades each
//! signature the same way, from the enrolled record and the assertion's
//! flags.

use mdkg_ops::wire::PersonKeyKind;
use mdkg_wire::pair;

/// The kind as the store and the pairing code spell it: wire's
/// [`pair::kind_name`], serve-store's names.
pub fn kind_str(kind: PersonKeyKind) -> &'static str {
    pair::kind_name(kind)
}

pub fn parse_kind(s: &str) -> Option<PersonKeyKind> {
    match s {
        "enclave" => Some(PersonKeyKind::SecureEnclave),
        "fido2" => Some(PersonKeyKind::Fido2),
        "file" => Some(PersonKeyKind::File),
        _ => None,
    }
}

/// What every signature of the key needs from its person, by
/// serve-store's names (#7691): `up` and `uv`, where `uv` implies `up`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// `up`: the person is present. Touch ID or the password for an
    /// enclave key, a touch (the "user present" flag) for a FIDO2 key. A
    /// file key enrols with `up` too, though nothing enforces it: its grade
    /// comes from its kind, never its policy.
    Present,
    /// `uv`: a FIDO2 key whose assertions also need its PIN or biometric
    /// (the "user verified" flag).
    Verified,
}

impl Policy {
    /// The stored and pairing-code spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Present => "up",
            Policy::Verified => "uv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Policy::Present),
            "uv" => Some(Policy::Verified),
            _ => None,
        }
    }

    /// Whether assertions must carry the "user verified" flag.
    pub fn needs_verified(self) -> bool {
        self == Policy::Verified
    }

    /// The policies a kind may be enrolled with: `uv` only for FIDO2,
    /// whose authenticator reports the flag; `up` for every kind.
    pub fn allowed_for(self, kind: PersonKeyKind) -> bool {
        self == Policy::Present || kind == PersonKeyKind::Fido2
    }
}

/// Whether the key's hardware was proved at enrolment, recorded with the
/// key as `verified` or `none` (wire's type, serve-store's names). Only a
/// FIDO2 attestation chained to a root the approving person supplied is
/// `verified`.
pub use mdkg_wire::person::Attestation;

/// How a key shows in `person list`, records and receipts (wire's type):
/// `present` or `machine-grade`.
pub use mdkg_wire::person::Grade;

/// True only for a FIDO2 key whose attestation verified (#7691). A
/// backup-eligible FIDO2 credential (the BE or BS flag) is recorded with
/// attestation `none` at enrolment, since it may live on hardware the
/// attestation never covered, so it is machine-grade too (#8059). A file
/// key is never present-grade, whatever it claims, and an enclave key
/// stays machine-grade even if a record says `verified`: Apple offers no
/// attestation a coordinator can check for a command-line tool's key, so
/// hardware backing alone is never presence (P1, decided 2026-10-29).
pub fn presence_proven(kind: PersonKeyKind, attestation: Attestation) -> bool {
    kind == PersonKeyKind::Fido2 && attestation == Attestation::Verified
}

pub fn grade(kind: PersonKeyKind, attestation: Attestation) -> Grade {
    if presence_proven(kind, attestation) {
        Grade::Present
    } else {
        Grade::MachineGrade
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [PersonKeyKind; 3] = [
        PersonKeyKind::SecureEnclave,
        PersonKeyKind::Fido2,
        PersonKeyKind::File,
    ];

    #[test]
    fn only_a_verified_hardware_key_proves_presence() {
        for kind in KINDS {
            assert_eq!(grade(kind, Attestation::None), Grade::MachineGrade);
        }
        // A record that says verified for a file or enclave key still
        // doesn't make it present-grade.
        for kind in [PersonKeyKind::File, PersonKeyKind::SecureEnclave] {
            assert_eq!(grade(kind, Attestation::Verified), Grade::MachineGrade);
        }
        assert_eq!(
            grade(PersonKeyKind::Fido2, Attestation::Verified),
            Grade::Present
        );
        assert_eq!(Grade::MachineGrade.as_str(), "machine-grade");
    }

    #[test]
    fn spellings_round_trip() {
        for kind in KINDS {
            assert_eq!(parse_kind(kind_str(kind)), Some(kind));
        }
        for p in [Policy::Present, Policy::Verified] {
            assert_eq!(Policy::parse(p.as_str()), Some(p));
        }
        assert_eq!(Policy::Present.as_str(), "up");
        assert_eq!(Policy::Verified.as_str(), "uv");
        // Exactly wire's list, which is serve-store's.
        assert_eq!(
            pair::POLICIES.map(|p| Policy::parse(p).map(Policy::as_str)),
            pair::POLICIES.map(Some)
        );
        assert_eq!(
            pair::KINDS.map(|k| parse_kind(k).map(kind_str)),
            pair::KINDS.map(Some)
        );
        assert_eq!(parse_kind("Enclave"), None);
        for old in ["up+uv", "none", "UV", ""] {
            assert_eq!(Policy::parse(old), None, "{old}");
        }
    }

    #[test]
    fn each_kind_takes_only_its_policies() {
        for kind in KINDS {
            assert!(Policy::Present.allowed_for(kind));
        }
        assert!(Policy::Verified.allowed_for(PersonKeyKind::Fido2));
        assert!(!Policy::Verified.allowed_for(PersonKeyKind::File));
        assert!(!Policy::Verified.allowed_for(PersonKeyKind::SecureEnclave));
    }
}
