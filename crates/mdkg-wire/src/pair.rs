//! The commitment-based pairing code (S6, S41).
//!
//! The new machine picks a random 32-byte `Nc` and sends its commitment,
//! `SHA-256(Nc)`, before it sees the coordinator's random `Ns`. It reveals
//! `Nc` after, and the coordinator checks it against the commitment. Both
//! sides then compute the code from what each saw, and a person compares
//! the two. An attacker relaying the exchange with its own TLS key would
//! need its code to match the real one, but it committed before it saw
//! `Ns`, so it has nothing to grind: each try matches with probability
//! 10^-8.
//!
//! **The encoding** (shared with serve-core and person-keys, decided
//! 2026-10-29, #6732). The hash input is the label, then each field, in a
//! fixed order, each of them (the label too) as its length (a `u32`,
//! big-endian) followed by its bytes. Strings are UTF-8; fingerprints are
//! their string form ([`crate::keys::fingerprint`]); `Ns` and `Nc` are
//! their raw 32 bytes. Everything is length-prefixed, so no two different
//! sets of inputs hash the same bytes, and the labels keep a machine's
//! hash input apart from a person's. The eight-digit codes are not kept
//! apart: inputs chosen to collide can give equal codes, of the same kind
//! or not. What the comparison rests on is narrower: an attacker who
//! committed before seeing `Ns` gets one independent try per pairing,
//! which matches with probability about 10^-8. Two sides that saw the
//! same values agree by construction.
//! - **A machine** (S6), label `mdkg-pair-machine-v1`: `serve_fp`,
//!   `machine_fp`, `graph_id`, `Ns`, `Nc`.
//! - **A person key** (S41), label `mdkg-pair-person-v1`: `serve_fp`, the
//!   machine id, `graph_id`, the `human:*` actor, the key's kind, its
//!   fingerprint, its policy, `Ns`, `Nc`. These are every field of the
//!   pending request approval compares, so a party holding the machine's
//!   signing key can't swap in its own person key, or another person,
//!   behind a code the person approved (Terra #6271). Kind and policy are
//!   the exact strings serve-store stores ([`KINDS`], [`POLICIES`]).
//!
//! The code shown is SHA-256 over that input: its first eight bytes, read
//! as a big-endian `u64`, modulo 10^8, written as eight decimal digits
//! with leading zeros. [`vectors`] holds the published test vectors.

use mdkg_ops::wire::PersonKeyKind;
use sha2::{Digest, Sha256};

/// What [`commitment`] commits to: 32 random bytes.
pub type Nonce = [u8; 32];

pub const MACHINE_LABEL: &str = "mdkg-pair-machine-v1";
pub const PERSON_LABEL: &str = "mdkg-pair-person-v1";
const DIGITS: u64 = 100_000_000;

/// The person key kinds, as serve-store stores them.
pub const KINDS: [&str; 3] = ["enclave", "fido2", "file"];

/// The presence policies, as serve-store stores them: "user present",
/// and "user verified", which implies user present (orchestrator,
/// 2026-10-29).
pub const POLICIES: [&str; 2] = ["up", "uv"];

/// A kind's stored name.
pub fn kind_name(kind: PersonKeyKind) -> &'static str {
    match kind {
        PersonKeyKind::SecureEnclave => "enclave",
        PersonKeyKind::Fido2 => "fido2",
        PersonKeyKind::File => "file",
    }
}

/// The new side's commitment to `nc`: `SHA-256(Nc)` (S6).
pub fn commitment(nc: &Nonce) -> [u8; 32] {
    Sha256::digest(nc).into()
}

/// Whether the revealed `nc` is the one `commitment` committed to. Compares
/// every byte, whatever differs first.
pub fn reveal_matches(commitment_value: &[u8; 32], nc: &Nonce) -> bool {
    constant_eq(&commitment(nc), commitment_value)
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The hash input: the label, then each field, each one (the label too)
/// preceded by its length.
pub fn encode(label: &str, fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in std::iter::once(label.as_bytes()).chain(fields.iter().copied()) {
        let len = u32::try_from(f.len()).expect("a pairing field is under 4 GiB");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// The eight digits shown for a hash input.
pub fn code_of(input: &[u8]) -> String {
    let hash = Sha256::digest(input);
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash[..8]);
    format!("{:08}", u64::from_be_bytes(first) % DIGITS)
}

/// The values a machine pairing binds, as each side saw them (S6).
#[derive(Debug, Clone, Copy)]
pub struct Pairing<'a> {
    /// The coordinator's TLS key fingerprint: the one the worker pinned,
    /// or the host's own.
    pub serve_fp: &'a str,
    /// The enrolling machine's fingerprint.
    pub machine_fp: &'a str,
    pub graph_id: &'a str,
    pub ns: &'a Nonce,
    pub nc: &'a Nonce,
}

impl Pairing<'_> {
    pub fn input(&self) -> Vec<u8> {
        encode(
            MACHINE_LABEL,
            &[
                self.serve_fp.as_bytes(),
                self.machine_fp.as_bytes(),
                self.graph_id.as_bytes(),
                self.ns,
                self.nc,
            ],
        )
    }
}

/// A machine's pairing code (S6).
pub fn machine_code(p: &Pairing<'_>) -> String {
    code_of(&p.input())
}

/// The values a person-key pairing binds, as each side saw them (S41).
#[derive(Debug, Clone, Copy)]
pub struct PersonPairing<'a> {
    /// The coordinator's TLS key fingerprint.
    pub serve_fp: &'a str,
    /// The id of the machine the key is enrolled from.
    pub machine_id: &'a str,
    pub graph_id: &'a str,
    /// The `human:*` actor the key is for.
    pub human: &'a str,
    /// Its kind, one of [`KINDS`].
    pub kind: &'a str,
    /// The candidate person key's fingerprint.
    pub key_fp: &'a str,
    /// Its presence policy, one of [`POLICIES`].
    pub policy: &'a str,
    pub ns: &'a Nonce,
    pub nc: &'a Nonce,
}

impl PersonPairing<'_> {
    /// The hash input, or `None` for a kind or policy serve-store doesn't
    /// name.
    pub fn input(&self) -> Option<Vec<u8>> {
        (KINDS.contains(&self.kind) && POLICIES.contains(&self.policy)).then(|| {
            encode(
                PERSON_LABEL,
                &[
                    self.serve_fp.as_bytes(),
                    self.machine_id.as_bytes(),
                    self.graph_id.as_bytes(),
                    self.human.as_bytes(),
                    self.kind.as_bytes(),
                    self.key_fp.as_bytes(),
                    self.policy.as_bytes(),
                    self.ns,
                    self.nc,
                ],
            )
        })
    }
}

/// A person key's pairing code (S41), or `None` for a kind or policy
/// serve-store doesn't name.
pub fn person_code(p: &PersonPairing<'_>) -> Option<String> {
    p.input().map(|i| code_of(&i))
}

/// Whether the code a person typed is `expected`: exactly eight digits,
/// compared without stopping at the first difference.
pub fn code_matches(expected: &str, typed: &str) -> bool {
    let typed = typed.trim();
    typed.len() == 8
        && typed.bytes().all(|b| b.is_ascii_digit())
        && constant_eq(expected.as_bytes(), typed.as_bytes())
}

/// Published test vectors: wire, serve-core and person-keys each test
/// against these.
pub mod vectors {
    use super::{Nonce, Pairing, PersonPairing};

    /// `Ns`: bytes 0x00 to 0x1f.
    pub const NS: Nonce = {
        let mut n = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            n[i] = i as u8;
            i += 1;
        }
        n
    };
    /// `Nc`: 32 bytes of 0xa5.
    pub const NC: Nonce = [0xa5; 32];
    /// The fingerprint of the Ed25519 public key of 32 zero bytes.
    pub const FP_ZERO_ED25519: &str = "mdkg:Zmh6rfhivXdsj8GLjp-OIAiXFIVu4jOzkCpZHQ1fKSU";
    /// The fingerprint of the 65-byte SEC1 point `04` then 64 bytes of
    /// 0x01 (not on the curve; the fingerprint doesn't care).
    pub const FP_P256_SAMPLE: &str = "mdkg:taPq3iOv_j8YnZbhoTGKE_gTOpxk3V2LtkgsmuKUQxE";

    pub const MACHINE: Pairing<'static> = Pairing {
        serve_fp: "mdkg:serve-fp",
        machine_fp: "mdkg:machine-fp",
        graph_id: "01a18800-0000-7000-8000-000000000001",
        ns: &NS,
        nc: &NC,
    };
    /// [`MACHINE`]'s hash input, hex.
    pub const MACHINE_INPUT_HEX: &str = "000000146d646b672d706169722d6d616368696e652d76310000000d6d646b673a73657276652d66700000000f6d646b673a6d616368696e652d66700000002430316131383830302d303030302d373030302d383030302d30303030303030303030303100000020000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f00000020a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5";
    pub const MACHINE_CODE: &str = "25933147";

    /// serve-core's S6 vector (`enroll::pairing_code`'s test):
    /// `serve_fp` "mdkg:serve", `machine_fp` "mdkg:machine", `graph_id`
    /// "graph-1", `Ns` 32 bytes of 1, `Nc` 32 bytes of 2.
    pub const S6_CODE: &str = "97560245";

    pub const PERSON: PersonPairing<'static> = PersonPairing {
        serve_fp: "mdkg:serve-fp",
        machine_id: "01a18800-0000-7000-8000-000000000002",
        graph_id: "01a18800-0000-7000-8000-000000000001",
        human: "human:nick",
        kind: "fido2",
        key_fp: "mdkg:person-fp",
        policy: "uv",
        ns: &NS,
        nc: &NC,
    };
    /// [`PERSON`]'s hash input, hex.
    pub const PERSON_INPUT_HEX: &str = "000000136d646b672d706169722d706572736f6e2d76310000000d6d646b673a73657276652d66700000002430316131383830302d303030302d373030302d383030302d3030303030303030303030320000002430316131383830302d303030302d373030302d383030302d3030303030303030303030310000000a68756d616e3a6e69636b000000056669646f320000000e6d646b673a706572736f6e2d667000000002757600000020000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f00000020a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5";
    pub const PERSON_CODE: &str = "04942458";

    /// A person enrolment's proof of possession
    /// (`person::possession_challenge`): this pending id, [`NS`], the
    /// commitment to [`NC`] and [`PERSON`]'s graph id.
    pub const POSSESSION_PENDING_ID: &str = "01a18800-0000-7000-8000-000000000003";
    /// The challenge's bytes, hex.
    pub const POSSESSION_CHALLENGE_HEX: &str = "000000196d646b672d706572736f6e2d706f7373657373696f6e2d76310000002430316131383830302d303030302d373030302d383030302d30303030303030303030303300000020000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f00000020fc8b64001c5fdd0f2f40fb67dae4a865a2c5bd17836676d6d5b58b7917e337170000002430316131383830302d303030302d373030302d383030302d303030303030303030303031";
}

#[cfg(test)]
mod tests {
    use super::vectors::*;
    use super::*;
    use crate::keys::fingerprint;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_published_vectors_hold() {
        assert_eq!(fingerprint(&[0u8; 32]), FP_ZERO_ED25519);
        let mut point = [1u8; 65];
        point[0] = 0x04;
        assert_eq!(fingerprint(&point), FP_P256_SAMPLE);
        assert_eq!(hex(&MACHINE.input()), MACHINE_INPUT_HEX);
        assert_eq!(machine_code(&MACHINE), MACHINE_CODE);
        assert_eq!(hex(&PERSON.input().unwrap()), PERSON_INPUT_HEX);
        assert_eq!(person_code(&PERSON).unwrap(), PERSON_CODE);
        // The vector serve-core tests too, so the lanes can't drift.
        let s6 = Pairing {
            serve_fp: "mdkg:serve",
            machine_fp: "mdkg:machine",
            graph_id: "graph-1",
            ns: &[1; 32],
            nc: &[2; 32],
        };
        assert_eq!(machine_code(&s6), S6_CODE);
        assert_eq!(S6_CODE, "97560245");
    }

    #[test]
    fn kinds_are_serve_stores_names() {
        assert_eq!(kind_name(PersonKeyKind::SecureEnclave), "enclave");
        assert_eq!(kind_name(PersonKeyKind::Fido2), "fido2");
        assert_eq!(kind_name(PersonKeyKind::File), "file");
        for bad in ["secure-enclave", "FIDO2", "", "fido2 "] {
            assert_eq!(
                person_code(&PersonPairing {
                    kind: bad,
                    ..PERSON
                }),
                None
            );
        }
        for good in POLICIES {
            assert!(
                person_code(&PersonPairing {
                    policy: good,
                    ..PERSON
                })
                .is_some()
            );
        }
        for bad in ["up+uv", "UV", "", "uv ", "presence"] {
            assert_eq!(
                person_code(&PersonPairing {
                    policy: bad,
                    ..PERSON
                }),
                None
            );
        }
    }

    #[test]
    fn the_commitment_is_sha256_of_nc() {
        let c = commitment(&NC);
        assert_eq!(c.as_slice(), Sha256::digest(NC).as_slice());
        assert!(reveal_matches(&c, &NC));
        assert!(!reveal_matches(&c, &NS));
    }

    #[test]
    fn codes_are_eight_digits() {
        let a = machine_code(&MACHINE);
        assert!(code_matches(&a, &a));
        assert!(code_matches(&a, &format!(" {a}\n")));
        assert!(!code_matches(&a, &a[..7]));
        assert!(!code_matches(&a, &format!("{a}0")));
        // A hash whose first eight bytes are small still shows eight digits.
        assert_eq!(code_of(b"").len(), 8);
    }

    #[test]
    fn every_machine_field_changes_the_code() {
        let ns2 = [3u8; 32];
        let nc2 = [4u8; 32];
        let a = machine_code(&MACHINE);
        for v in [
            Pairing {
                serve_fp: "mdkg:other",
                ..MACHINE
            },
            Pairing {
                machine_fp: "mdkg:other",
                ..MACHINE
            },
            Pairing {
                graph_id: "graph-2",
                ..MACHINE
            },
            Pairing {
                ns: &ns2,
                ..MACHINE
            },
            Pairing {
                nc: &nc2,
                ..MACHINE
            },
        ] {
            assert_ne!(machine_code(&v), a);
        }
    }

    #[test]
    fn every_person_field_changes_the_code() {
        let ns2 = [3u8; 32];
        let nc2 = [4u8; 32];
        let a = person_code(&PERSON).unwrap();
        for v in [
            PersonPairing {
                serve_fp: "mdkg:other",
                ..PERSON
            },
            PersonPairing {
                machine_id: "m2",
                ..PERSON
            },
            PersonPairing {
                graph_id: "graph-2",
                ..PERSON
            },
            PersonPairing {
                human: "human:eve",
                ..PERSON
            },
            PersonPairing {
                kind: "file",
                ..PERSON
            },
            PersonPairing {
                key_fp: "mdkg:pk2",
                ..PERSON
            },
            PersonPairing {
                policy: "up",
                ..PERSON
            },
            PersonPairing { ns: &ns2, ..PERSON },
            PersonPairing { nc: &nc2, ..PERSON },
        ] {
            assert_ne!(person_code(&v).unwrap(), a);
        }
    }

    #[test]
    fn fields_are_framed() {
        // Moving a byte across a field boundary changes the input.
        let a = Pairing {
            serve_fp: "ab",
            machine_fp: "c",
            ..MACHINE
        };
        let b = Pairing {
            serve_fp: "a",
            machine_fp: "bc",
            ..MACHINE
        };
        assert_ne!(a.input(), b.input());
    }

    #[test]
    fn a_relay_that_grinds_its_tls_key_finds_no_match() {
        // SC48: an attacker in the path presents its own TLS key to the
        // worker. The worker's code uses that key's fingerprint; the
        // host's uses its own. Grinding fingerprints after Ns and Nc are
        // fixed matches only by chance, at 10^-8 a try.
        let host = machine_code(&MACHINE);
        let mut hits = 0;
        for i in 0..200_000u32 {
            let fp = format!("mdkg:attacker-{i}");
            let worker = machine_code(&Pairing {
                serve_fp: &fp,
                ..MACHINE
            });
            if code_matches(&host, &worker) {
                hits += 1;
            }
        }
        assert!(hits <= 1, "{hits} matches in 200000 tries");
    }
}
