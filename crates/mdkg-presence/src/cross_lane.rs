//! Cross-lane checks against wire at b68267cc, as integrated on the serve
//! wave's branch (first written against d3ec20c2): its published vectors
//! hold for this crate's fingerprints and spellings, and its
//! `PersonVerifier` accepts this crate's signatures with the grade the
//! enrolled record gives.

use mdkg_ops::wire::{KeyDirectory, MachineKey, PersonKey, PersonKeyKind, Signer};
use mdkg_wire::pair::{self, PersonPairing, vectors};
use mdkg_wire::person::{self, PersonAttestations, PersonCheck, PersonVerifier};

use crate::attest::Roots;
use crate::authdata::{FLAG_UP, FLAG_UV};
use crate::cose::PublicKey;
use crate::device;
use crate::enclave::{self, EnclaveKey};
use crate::enrol::{self, Binding, FileKeyOptIn, Possession};
use crate::fido2::{self, Fido2Key};
use crate::file_key::FileKey;
use crate::grade::{self, Attestation, Grade, Policy};

#[test]
fn fingerprints_match_wires_vectors() {
    assert_eq!(
        device::fingerprint(&PublicKey::Ed25519([0; 32])),
        vectors::FP_ZERO_ED25519
    );
    let mut point = [1u8; 65];
    point[0] = 0x04;
    assert_eq!(
        device::fingerprint(&PublicKey::P256(point)),
        vectors::FP_P256_SAMPLE
    );
}

#[test]
fn the_person_code_takes_this_crates_spellings() {
    let p = PersonPairing {
        kind: grade::kind_str(PersonKeyKind::Fido2),
        policy: Policy::Verified.as_str(),
        ..vectors::PERSON
    };
    assert_eq!(pair::person_code(&p).as_deref(), Some(vectors::PERSON_CODE));
    for kind in [
        PersonKeyKind::SecureEnclave,
        PersonKeyKind::Fido2,
        PersonKeyKind::File,
    ] {
        for policy in [Policy::Present, Policy::Verified] {
            let p = PersonPairing {
                kind: grade::kind_str(kind),
                policy: policy.as_str(),
                ..vectors::PERSON
            };
            assert!(pair::person_code(&p).is_some(), "{kind:?} {policy:?}");
        }
    }
}

/// One enrolled person key and its record's attestation.
struct Dir {
    key: PersonKey,
    attestation: Attestation,
}

impl KeyDirectory for Dir {
    fn machine(&self, _: &str) -> Option<MachineKey> {
        None
    }

    fn person(&self, key_id: &str) -> Option<PersonKey> {
        (key_id == self.key.key_id).then(|| self.key.clone())
    }
}

impl PersonAttestations for Dir {
    fn attestation(&self, key_id: &str) -> Attestation {
        if key_id == self.key.key_id {
            self.attestation
        } else {
            Attestation::None
        }
    }
}

fn record(kind: PersonKeyKind, public_key: &PublicKey, policy: Policy) -> PersonKey {
    PersonKey {
        key_id: device::fingerprint(public_key),
        person: "human:nick".into(),
        kind,
        public_key: public_key.as_bytes().to_vec(),
        user_verified: policy.needs_verified(),
        revoked: false,
    }
}

/// What wire's verifier says of `signer`'s signature over `base`.
fn check(signer: &dyn Signer, key: &PersonKey, attestation: Attestation) -> PersonCheck {
    let sig = signer.sign(b"the person base").expect("sign").0;
    let v = PersonVerifier::new(Dir {
        key: key.clone(),
        attestation,
    });
    assert_eq!(
        v.verify(signer.key_id(), signer.alg(), b"another base", &sig),
        PersonCheck::Invalid,
        "a signature doesn't move to another base"
    );
    v.verify(signer.key_id(), signer.alg(), b"the person base", &sig)
}

fn grade_of(c: PersonCheck) -> Grade {
    match c {
        PersonCheck::Verified(p) => {
            assert_eq!(p.human, "human:nick");
            p.grade
        }
        other => panic!("not verified: {other:?}"),
    }
}

const COMMITMENT: [u8; 32] = [3; 32];

#[test]
fn wire_verifies_a_fido2_signature_graded_by_its_record() {
    for (policy, flags) in [
        (Policy::Present, FLAG_UP),
        (Policy::Verified, FLAG_UP | FLAG_UV),
    ] {
        let fake = fido2::tests::Fake::new(flags);
        let b = Binding {
            graph_id: "graph-1",
            machine_id: "mdkg:machine",
            human: "human:nick",
            kind: PersonKeyKind::Fido2,
            policy,
            commitment: &COMMITMENT,
        };
        let (handle, evidence) = fido2::create(&fake, &b).expect("create");
        let checked = enrol::check(
            &b,
            &evidence,
            FileKeyOptIn::NotAllowed,
            &Roots::none(crate::testutil::now()),
        )
        .expect("enrols");
        let key = record(PersonKeyKind::Fido2, &handle.public_key, policy);
        let signer = Fido2Key::new(handle, Box::new(fake));
        assert_eq!(signer.key_id(), key.key_id);
        // The record this crate made (self attestation: none) is
        // machine-grade; a record whose attestation verified is present.
        assert_eq!(
            grade_of(check(&signer, &key, checked.attestation)),
            Grade::MachineGrade
        );
        assert_eq!(
            grade_of(check(&signer, &key, Attestation::Verified)),
            Grade::Present
        );
    }
}

#[test]
fn wire_verifies_enclave_and_file_signatures_as_machine_grade() {
    let tmp = crate::testutil::TempDir::new("cross-file");
    let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
    let signer =
        EnclaveKey::open_or_create(Box::new(enclave::tests::Fake::default()), "nick", &lock)
            .expect("create");
    let key = record(
        PersonKeyKind::SecureEnclave,
        signer.public_key(),
        Policy::Present,
    );
    assert_eq!(
        grade_of(check(&signer, &key, Attestation::Verified)),
        Grade::MachineGrade
    );

    let signer = FileKey::create(tmp.path(), "nick", FileKeyOptIn::Allowed, &lock).expect("create");
    let key = record(PersonKeyKind::File, signer.public_key(), Policy::Present);
    assert_eq!(
        grade_of(check(&signer, &key, Attestation::Verified)),
        Grade::MachineGrade
    );
}

#[test]
fn the_possession_challenge_is_wires_vector() {
    let commitment = pair::commitment(&vectors::NC);
    let c = enrol::possession_challenge(
        vectors::POSSESSION_PENDING_ID,
        &vectors::NS,
        &commitment,
        vectors::PERSON.graph_id,
    );
    let hex: String = c.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, vectors::POSSESSION_CHALLENGE_HEX);
}

#[test]
fn wire_accepts_each_kinds_proof_for_its_request_only() {
    let tmp = crate::testutil::TempDir::new("cross-prove");
    const NS: [u8; 32] = [4; 32];
    let fake = fido2::tests::Fake::new(FLAG_UP | FLAG_UV);
    let b = Binding {
        graph_id: "graph-1",
        machine_id: "mdkg:machine",
        human: "human:nick",
        kind: PersonKeyKind::Fido2,
        policy: Policy::Verified,
        commitment: &COMMITMENT,
    };
    let (handle, _) = fido2::create(&fake, &b).expect("create");
    let fido2_record = record(PersonKeyKind::Fido2, &handle.public_key, Policy::Verified);
    let fido2_key = Fido2Key::new(handle, Box::new(fake));

    let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
    let enclave =
        EnclaveKey::open_or_create(Box::new(enclave::tests::Fake::default()), "nick", &lock)
            .expect("create");
    let enclave_record = record(
        PersonKeyKind::SecureEnclave,
        enclave.public_key(),
        Policy::Present,
    );

    let file = FileKey::create(tmp.path(), "nick", FileKeyOptIn::Allowed, &lock).expect("create");
    let file_record = record(PersonKeyKind::File, file.public_key(), Policy::Present);

    let signers: [(&dyn Signer, &PersonKey); 3] = [
        (&fido2_key, &fido2_record),
        (&enclave, &enclave_record),
        (&file, &file_record),
    ];
    let other = [9u8; 32];
    for (signer, key) in signers {
        let issued = Possession {
            pending_id: "pp1",
            ns: &NS,
            commitment: &COMMITMENT,
            graph_id: "graph-1",
        };
        let proof = enrol::prove(signer, &issued).expect("prove").0;
        let ok = |id: &str, ns: &[u8; 32], c: &[u8; 32], g: &str| {
            let p = Possession {
                pending_id: id,
                ns,
                commitment: c,
                graph_id: g,
            };
            person::proves_possession(key, signer.alg(), &p, &proof)
        };
        assert!(ok("pp1", &NS, &COMMITMENT, "graph-1"), "{}", signer.alg());
        assert!(
            !ok("pp2", &NS, &COMMITMENT, "graph-1"),
            "another pending id"
        );
        assert!(!ok("pp1", &other, &COMMITMENT, "graph-1"), "another Ns");
        assert!(!ok("pp1", &NS, &other, "graph-1"), "another commitment");
        assert!(!ok("pp1", &NS, &COMMITMENT, "graph-2"), "another graph");
    }
}

#[test]
fn a_fido2_proof_without_uv_under_uv_fails_on_the_device() {
    let fake = fido2::tests::Fake::new(FLAG_UP | FLAG_UV);
    let b = Binding {
        graph_id: "graph-1",
        machine_id: "mdkg:machine",
        human: "human:nick",
        kind: PersonKeyKind::Fido2,
        policy: Policy::Verified,
        commitment: &COMMITMENT,
    };
    let (handle, _) = fido2::create(&fake, &b).expect("create");
    fake.set_flags(FLAG_UP);
    let key = Fido2Key::new(handle, Box::new(fake));
    let issued = Possession {
        pending_id: "pp1",
        ns: &[4; 32],
        commitment: &COMMITMENT,
        graph_id: "graph-1",
    };
    let err = enrol::prove(&key, &issued).expect_err("no UV");
    assert_eq!(err.code, mdkg_format::Code::ApiPersonSignatureRequired);
}
