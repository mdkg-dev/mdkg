//! A software FIDO2 authenticator for tests: it makes credentials and
//! signs `packed` attestations with the test certificates in `testdata/`.

use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use rustls_pki_types::{CertificateDer, UnixTime};
use sha2::{Digest, Sha256};

use crate::attest::{self, Statement};
use crate::authdata::{self, FLAG_UP};
use crate::cose::{ALG_ES256, PublicKey};

pub const ROOT: &[u8] = include_bytes!("../testdata/root.der");
pub const OTHER_ROOT: &[u8] = include_bytes!("../testdata/other-root.der");
pub const LEAF: &[u8] = include_bytes!("../testdata/leaf.der");
pub const LEAF_AAGUID: &[u8] = include_bytes!("../testdata/leaf-aaguid.der");
pub const LEAF_AAGUID_OTHER: &[u8] = include_bytes!("../testdata/leaf-aaguid-other.der");
pub const LEAF_OU: &[u8] = include_bytes!("../testdata/leaf-ou.der");
pub const LEAF_CA: &[u8] = include_bytes!("../testdata/leaf-ca.der");
pub const LEAF_RSA: &[u8] = include_bytes!("../testdata/leaf-rsa.der");
pub const LEAF_SHA512: &[u8] = include_bytes!("../testdata/leaf-sha512.der");
pub const INTER_CERTSIGN: &[u8] = include_bytes!("../testdata/inter-certsign.der");
pub const INTER_CERTSIGN_LEAF: &[u8] = include_bytes!("../testdata/inter-certsign-leaf.der");
pub const INTER_CRLONLY: &[u8] = include_bytes!("../testdata/inter-crlonly.der");
pub const INTER_CRLONLY_LEAF: &[u8] = include_bytes!("../testdata/inter-crlonly-leaf.der");
const LEAF_KEY: &[u8] = include_bytes!("../testdata/leaf-key.pk8");
const LEAF_RSA_KEY: &[u8] = include_bytes!("../testdata/leaf-rsa-key.pk8");

/// [`LEAF`] with its version set to v1. Its signature no longer holds,
/// which the profile check doesn't read.
pub fn leaf_v1() -> Vec<u8> {
    let mut der = LEAF.to_vec();
    let at = der
        .windows(5)
        .position(|w| w == [0xa0, 0x03, 0x02, 0x01, 0x02])
        .expect("a v3 version field");
    der[at + 4] = 0;
    der
}

/// 2027-01-01, inside the test certificates' validity.
pub fn now() -> UnixTime {
    attest::unix_time_ms(1_798_761_600_000)
}

pub fn root() -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(ROOT)]
}

pub fn other_root() -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(OTHER_ROOT)]
}

/// How the fake authenticator attests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    None,
    SelfPacked,
    /// Packed, with [`LEAF`] as the attestation certificate.
    Packed,
    /// Packed, with this certificate, signed with [`LEAF`]'s key.
    PackedWith(&'static [u8]),
    /// Packed, RS256, with [`LEAF_RSA`].
    PackedRsa,
}

/// A credential the fake authenticator made.
pub struct Made {
    pub auth_data: Vec<u8>,
    pub statement: Statement,
    pub public_key: PublicKey,
}

fn p256(pkcs8: &[u8]) -> EcdsaKeyPair {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &SystemRandom::new())
        .expect("test key")
}

/// Makes a P-256 credential for `rp_id` over `challenge`, with `flags`
/// (`FLAG_UP` and `FLAG_UV`) and the attestation `fmt`.
pub fn make_credential(rp_id: &str, challenge: &[u8], flags: u8, fmt: Fmt) -> Made {
    let rng = SystemRandom::new();
    let pkcs8 =
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("generate");
    let cred = p256(pkcs8.as_ref());
    let public_key = PublicKey::from_bytes(cred.public_key().as_ref()).expect("P-256");
    let auth_data = authdata::build_attested(
        attest::rp_id_hash(rp_id),
        flags,
        0,
        [0xaa; 16],
        b"credential-1",
        &public_key,
    );
    let mut signed = auth_data.clone();
    signed.extend_from_slice(&Sha256::digest(challenge));
    let statement = match fmt {
        Fmt::None => Statement {
            fmt: "none".into(),
            ..Statement::default()
        },
        Fmt::SelfPacked => Statement {
            fmt: "packed".into(),
            alg: Some(ALG_ES256),
            sig: cred.sign(&rng, &signed).expect("sign").as_ref().to_vec(),
            x5c: Vec::new(),
        },
        Fmt::Packed | Fmt::PackedWith(_) => Statement {
            fmt: "packed".into(),
            alg: Some(ALG_ES256),
            sig: p256(LEAF_KEY)
                .sign(&rng, &signed)
                .expect("sign")
                .as_ref()
                .to_vec(),
            x5c: vec![match fmt {
                Fmt::PackedWith(cert) => cert.to_vec(),
                _ => LEAF.to_vec(),
            }],
        },
        Fmt::PackedRsa => {
            let key = ring::signature::RsaKeyPair::from_pkcs8(LEAF_RSA_KEY).expect("rsa key");
            let mut sig = vec![0; key.public().modulus_len()];
            key.sign(&ring::signature::RSA_PKCS1_SHA256, &rng, &signed, &mut sig)
                .expect("sign");
            Statement {
                fmt: "packed".into(),
                alg: Some(crate::attest::ALG_RS256),
                sig,
                x5c: vec![LEAF_RSA.to_vec()],
            }
        }
    };
    Made {
        auth_data,
        statement,
        public_key,
    }
}

/// The flags a plain touch sets.
pub const UP: u8 = FLAG_UP;

/// A temporary folder, removed when dropped.
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("mdkg-presence-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A held person-key lock in its own temporary folder.
pub fn held() -> (TempDir, crate::lock::Held) {
    let tmp = TempDir::new("lock");
    let lock = crate::lock::UserLock::at(&tmp.path().join("l")).expect("lock");
    let held = lock.hold().expect("hold");
    (tmp, held)
}
