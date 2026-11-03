//! The pipeline's verifiers over mdkg-wire's (orchestrator #9489): strict
//! Ed25519 for machine signatures, and wire's person verifier, whose
//! grade becomes the gate's `machine_grade`. And the one adapter from
//! person-keys' attestation outcome to serve-store's approval check
//! ([`attestation_check`], #10686 item 4).

use mdkg_coord_sqlite::serve::{
    AbsentReason as StoreAbsent, AttestationCheck, FailedReason as StoreFailed, VerifierError,
};
use mdkg_ops::wire::KeyDirectory;
use mdkg_presence::enrol::{AbsentReason, Outcome};
use mdkg_wire::keys::Ed25519;
use mdkg_wire::person::{self, Grade, PersonAttestations};

use crate::auth::{PersonCheck, PersonKey, PersonVerifier, Verifier};

impl Verifier for Ed25519 {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        Ed25519::verify(self, public_key, base, signature)
    }
}

impl<D> PersonVerifier for person::PersonVerifier<D>
where
    D: KeyDirectory + PersonAttestations + Send + Sync,
{
    fn verify(&self, key_id: &str, alg: &str, base: &[u8], signature: &[u8]) -> PersonCheck {
        match person::PersonVerifier::verify(self, key_id, alg, base, signature) {
            person::PersonCheck::Verified(v) => PersonCheck::Verified(PersonKey {
                key_id: v.key_id,
                human: v.human,
                // Anything but Present is machine grade (S41).
                machine_grade: v.grade != Grade::Present,
            }),
            person::PersonCheck::Unknown => PersonCheck::Unknown,
            person::PersonCheck::Invalid => PersonCheck::Invalid,
        }
    }
}

/// person-keys' attestation outcome as serve-store's approval check
/// (S41): the one mapping between them (#10686 item 4).
/// - Verified is Verified, and each Absent reason is its store word; a
///   key with no attestation object and FIDO2's format `none` are both
///   the store's `none`.
/// - A Failed outcome is Failed with its closed word. A word the store
///   doesn't know is `malformed`, so no verifier's text reaches the store.
/// - An error about the request rather than its evidence (another kind
///   than the binding names, a policy the kind can't have, a file key
///   without the opt-in) isn't an outcome. It is `Err`, which approval
///   propagates and never records as Failed (S41, Terra 3 #9913).
pub fn attestation_check(outcome: &Outcome) -> Result<AttestationCheck, VerifierError> {
    Ok(match outcome {
        Outcome::Verified => AttestationCheck::Verified,
        Outcome::Absent(r) => AttestationCheck::Absent(match r {
            AbsentReason::NoObject | AbsentReason::NoneFormat => StoreAbsent::None,
            AbsentReason::SelfAttested => StoreAbsent::SelfAttested,
            AbsentReason::NoRoots => StoreAbsent::Unanchored,
            AbsentReason::BackupEligible => StoreAbsent::Backup,
            AbsentReason::UnsupportedFormat => StoreAbsent::UnsupportedFormat,
            AbsentReason::UnsupportedAlg => StoreAbsent::UnsupportedAlg,
        }),
        Outcome::Failed(e) => match e.failed_reason() {
            Some(r) => AttestationCheck::Failed(
                StoreFailed::parse(r.as_str()).unwrap_or(StoreFailed::Malformed),
            ),
            None => return Err(VerifierError(e.to_string())),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mdkg_ops::wire::{MachineKey, PersonKeyKind, Signer, profile};
    use mdkg_wire::keys::MachineKey as Key;
    use mdkg_wire::person::Attestation;

    use super::*;

    #[test]
    fn machine_signatures_verify_through_wire_s_strict_ed25519() {
        let key = Key::from_parts("m-1", [7; 32]);
        let sig = key.sign(b"base").expect("sign");
        let sig: [u8; 64] = sig.0.as_slice().try_into().expect("64 bytes");
        let v: &dyn Verifier = &Ed25519;
        assert!(v.verify(&key.public_key(), b"base", &sig));
        assert!(!v.verify(&key.public_key(), b"other", &sig));
        // The all-zero key is small-order: never verifies (#6951).
        assert!(!v.verify(&[0; 32], b"base", &sig));
    }

    /// Person keys by id, with what their enrolment recorded.
    struct Keys(HashMap<String, (mdkg_ops::wire::PersonKey, Attestation)>);

    impl KeyDirectory for Keys {
        fn machine(&self, _: &str) -> Option<MachineKey> {
            None
        }
        fn person(&self, key_id: &str) -> Option<mdkg_ops::wire::PersonKey> {
            self.0.get(key_id).map(|(k, _)| k.clone())
        }
    }

    impl PersonAttestations for Keys {
        fn attestation(&self, key_id: &str) -> Attestation {
            self.0.get(key_id).map_or(Attestation::None, |(_, a)| *a)
        }
    }

    #[test]
    fn a_person_key_s_grade_maps_to_machine_grade() {
        // A file key verifies, at machine grade whatever its record says;
        // an unknown key and a bad signature map across unchanged.
        let file = Key::from_parts("pk-file", [9; 32]);
        let record = mdkg_ops::wire::PersonKey {
            key_id: "pk-file".into(),
            person: "human:ann".into(),
            kind: PersonKeyKind::File,
            public_key: file.public_key().to_vec(),
            user_verified: false,
            revoked: false,
        };
        let keys = Keys(HashMap::from([(
            "pk-file".to_owned(),
            (record, Attestation::Verified),
        )]));
        let v: &dyn PersonVerifier = &person::PersonVerifier::new(keys);
        let sig = file.sign(b"base").expect("sign").0;
        assert_eq!(
            v.verify("pk-file", profile::ALG, b"base", &sig),
            PersonCheck::Verified(PersonKey {
                key_id: "pk-file".into(),
                human: "human:ann".into(),
                machine_grade: true,
            })
        );
        assert_eq!(
            v.verify("pk-file", profile::ALG, b"other", &sig),
            PersonCheck::Invalid
        );
        assert_eq!(
            v.verify("pk-none", profile::ALG, b"base", &sig),
            PersonCheck::Unknown
        );
    }

    /// Machine keys and person keys, with what each person key's
    /// enrolment recorded.
    struct Directory {
        machines: HashMap<String, MachineKey>,
        persons: Keys,
    }

    impl KeyDirectory for Directory {
        fn machine(&self, machine: &str) -> Option<MachineKey> {
            self.machines.get(machine).cloned()
        }
        fn person(&self, key_id: &str) -> Option<mdkg_ops::wire::PersonKey> {
            self.persons.person(key_id)
        }
    }

    impl PersonAttestations for Directory {
        fn attestation(&self, key_id: &str) -> Attestation {
            self.persons.attestation(key_id)
        }
    }

    /// What the codec hands the Gate for a request wire's client built:
    /// names lowercased, values as sent, and the length the client sends
    /// with a body.
    fn head_of(req: &mdkg_wire::sign::Outgoing) -> crate::http::RequestHead {
        let mut headers: Vec<(String, String)> = req
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .collect();
        if !req.body.is_empty() {
            headers.push(("content-length".into(), req.body.len().to_string()));
        }
        crate::http::RequestHead {
            method: req.method.clone(),
            path: req.path.clone(),
            query: req.query.clone(),
            headers,
        }
    }

    #[test]
    fn the_gate_verifies_what_wire_s_client_signs() {
        // #10448's vector: the signature base wire's client signs
        // (`sign::sign_machine`, `sign::sign_person`) is the one the Gate's
        // own RFC 9421 parser rebuilds, byte for byte, or the real Ed25519
        // checks below would fail. Every covered component is exercised:
        // an encoded path, a query, the actor, the digest, the idempotency
        // key and the turn, then a person signature over all of it.
        use crate::auth::Gate;
        use crate::boot::Boot;
        use crate::boot::test_clocks::ManualClocks;
        use crate::nonce::NonceCache;
        use mdkg_wire::sign;
        use std::sync::Arc;

        const T0: i64 = 1_793_290_515_000;
        let machine = Key::from_parts("m-wire", [5; 32]);
        let file = Key::from_parts("pk-file", [9; 32]);
        // Wire's person verifier takes its directory by value.
        let directory = || Directory {
            machines: HashMap::from([(
                "m-wire".to_owned(),
                MachineKey {
                    machine: "m-wire".into(),
                    public_key: machine.public_key(),
                    state: mdkg_ops::wire::MachineState::Approved,
                },
            )]),
            persons: Keys(HashMap::from([(
                "pk-file".to_owned(),
                (
                    mdkg_ops::wire::PersonKey {
                        key_id: "pk-file".into(),
                        person: "human:ann".into(),
                        kind: PersonKeyKind::File,
                        public_key: file.public_key().to_vec(),
                        user_verified: false,
                        revoked: false,
                    },
                    Attestation::None,
                ),
            )])),
        };
        let clocks = Arc::new(ManualClocks::at(T0));
        let gate = Gate::new(
            "graph-1".into(),
            Boot::start(&*clocks).expect("boot"),
            clocks.clone(),
            NonceCache::default(),
            Arc::new(directory()),
            Arc::new(Ed25519),
            Arc::new(person::PersonVerifier::new(directory())),
            vec![],
            Arc::new(crate::limits::RateLimiter::new(
                crate::limits::Rate::default(),
                "m-wire".into(),
            )),
        );
        let boot = gate.boot_id().expect("boot id");
        let request = |nonce: u8| {
            let mut req = sign::Outgoing::new(
                "POST",
                "/v1/nodes/task%3A1:claim",
                Some("dry=1&after=s1%3A7".into()),
            );
            req.header("Mdkg-Graph", "graph-1").expect("graph");
            req.header("Mdkg-Boot", boot.clone()).expect("boot");
            req.header("Mdkg-Actor", "human:ann").expect("actor");
            req.header("Idempotency-Key", "01a188cc-0000-7000-8000-000000000001")
                .expect("key");
            req.header("Mdkg-Turn", "turn-1").expect("turn");
            req.body(br#"{"lease":"task-1"}"#.to_vec()).expect("body");
            sign::sign_machine(&mut req, &machine, T0 / 1000, [nonce; 16]).expect("machine");
            req
        };

        let req = request(1);
        let mut auth = gate
            .authenticate(&head_of(&req))
            .expect("machine signature");
        assert_eq!(auth.machine(), "m-wire");
        assert_eq!(auth.person(), None);
        gate.check_body(&mut auth, &req.body).expect("digest");

        let mut req = request(2);
        sign::sign_person(&mut req, &file, T0 / 1000).expect("person");
        let mut auth = gate.authenticate(&head_of(&req)).expect("both signatures");
        assert_eq!(
            auth.person(),
            Some(&PersonKey {
                key_id: "pk-file".into(),
                human: "human:ann".into(),
                machine_grade: true,
            })
        );
        gate.check_body(&mut auth, &req.body).expect("digest");

        // The same bytes with any covered component changed don't verify.
        let req = request(3);
        let mut tampered = head_of(&req);
        tampered.query = Some("dry=0&after=s1%3A7".into());
        assert_eq!(
            gate.authenticate(&tampered).map(|_| ()).map_err(|r| r.code),
            Err(mdkg_format::Code::ApiUnauthenticated)
        );
    }

    #[test]
    fn person_keys_outcomes_map_one_to_one_onto_the_store_s_check() {
        use mdkg_presence::attest::AttestError;
        use mdkg_presence::authdata::AuthDataError;
        use mdkg_presence::enrol::EnrolError;
        use mdkg_presence::x509::ProfileError;

        assert_eq!(
            attestation_check(&Outcome::Verified),
            Ok(AttestationCheck::Verified)
        );
        for (r, word) in [
            (AbsentReason::NoObject, "none"),
            (AbsentReason::NoneFormat, "none"),
            (AbsentReason::SelfAttested, "self"),
            (AbsentReason::NoRoots, "unanchored"),
            (AbsentReason::BackupEligible, "backup"),
            (AbsentReason::UnsupportedFormat, "unsupported_format"),
            (AbsentReason::UnsupportedAlg, "unsupported_alg"),
        ] {
            match attestation_check(&Outcome::Absent(r)) {
                Ok(AttestationCheck::Absent(a)) => assert_eq!(a.as_str(), word, "{r:?}"),
                other => panic!("{r:?}: {other:?}"),
            }
        }
        // Every Failed error keeps its own word: none falls back to
        // malformed unless it is malformed.
        for (e, word) in [
            (EnrolError::BadKey, "malformed"),
            (EnrolError::KeyMismatch, "key_mismatch"),
            (
                EnrolError::Attest(AttestError::AuthData(AuthDataError::Truncated)),
                "malformed",
            ),
            (EnrolError::Attest(AttestError::Malformed("x")), "malformed"),
            (EnrolError::Attest(AttestError::RpMismatch), "rp_mismatch"),
            (EnrolError::Attest(AttestError::NotPresent), "no_up"),
            (EnrolError::Attest(AttestError::NotVerified), "no_uv"),
            (
                EnrolError::Attest(AttestError::BadSignature),
                "bad_signature",
            ),
            (EnrolError::Attest(AttestError::Untrusted), "chain_failed"),
            (
                EnrolError::Attest(AttestError::Profile(ProfileError::NoAttestationOu)),
                "profile",
            ),
        ] {
            match attestation_check(&Outcome::Failed(e.clone())) {
                Ok(AttestationCheck::Failed(f)) => assert_eq!(f.as_str(), word, "{e:?}"),
                other => panic!("{e:?}: {other:?}"),
            }
        }
        // Errors about the request, not its evidence, are no outcome.
        for e in [
            EnrolError::KindMismatch,
            EnrolError::PolicyNotAllowed,
            EnrolError::FileKeyNotAllowed,
            EnrolError::MachineGradeNotAcknowledged,
        ] {
            assert!(
                attestation_check(&Outcome::Failed(e.clone())).is_err(),
                "{e:?}"
            );
        }
    }

    #[test]
    fn every_failed_word_of_person_keys_is_one_the_store_knows() {
        // So `malformed` is never a fallback hiding a word the two crates
        // spell differently: each of person-keys' words parses as the
        // store's.
        use mdkg_presence::enrol::FailedReason;
        for r in [
            FailedReason::Malformed,
            FailedReason::RpMismatch,
            FailedReason::NoUp,
            FailedReason::NoUv,
            FailedReason::KeyMismatch,
            FailedReason::BadSignature,
            FailedReason::ChainFailed,
            FailedReason::Profile,
        ] {
            assert!(StoreFailed::parse(r.as_str()).is_some(), "{r:?}");
        }
        assert_eq!(StoreFailed::ALL.len(), 8, "the same closed set");
    }
}
