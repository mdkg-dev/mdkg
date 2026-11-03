//! The file key: an Ed25519 key in a 0600 file, only as an explicit
//! per-person opt-in (`--allow-file-key`), on a device with neither an
//! enclave nor a FIDO2 key. It doesn't prove presence, since any process
//! running as the user can sign with it, so it is `machine-grade`
//! wherever it shows (S41, R129).

use std::fmt;
use std::path::{Path, PathBuf};

use mdkg_format::{Code, Diagnostic};
use mdkg_ops::wire::{Signature, Signer, profile};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::cose::PublicKey;
use crate::device::{self, PersonDir};
use crate::enrol::FileKeyOptIn;
use crate::lock::UserLock;

/// The file's name in the person's folder.
pub const FILE_NAME: &str = "key.ed25519";

pub struct FileKey {
    key_id: String,
    public_key: PublicKey,
    pair: Ed25519KeyPair,
}

impl fmt::Debug for FileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl FileKey {
    /// Creates `name`'s key in its folder under `state`, refusing without
    /// the opt-in and refusing to replace a key already there. The whole
    /// creation holds the person-key lock.
    pub fn create(
        state: &Path,
        name: &str,
        opt_in: FileKeyOptIn,
        lock: &UserLock,
    ) -> Result<Self, Box<Diagnostic>> {
        if opt_in != FileKeyOptIn::Allowed {
            return Err(Box::new(
                Diagnostic::error(
                    Code::ApiPersonSignatureRequired,
                    "a file key is machine-grade: any process running as you can sign with it",
                )
                .with_hint("use a FIDO2 key, or pass --allow-file-key to enrol one anyway"),
            ));
        }
        let held = lock.hold()?;
        let dir = PersonDir::open(state, name, true, &held)?;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| {
            Box::new(Diagnostic::error(
                Code::IoWriteFailed,
                "the system random source failed",
            ))
        })?;
        dir.write_new(&held, FILE_NAME, pkcs8.as_ref())?;
        Self::from_pkcs8(pkcs8.as_ref(), &dir.path().join(FILE_NAME))
    }

    /// Loads `name`'s key from its folder under `state`, holding the
    /// person-key lock, so it never sees a key mid-publication.
    pub fn load(state: &Path, name: &str, lock: &UserLock) -> Result<Self, Box<Diagnostic>> {
        let held = lock.hold()?;
        let dir = PersonDir::open(state, name, false, &held)?;
        let bytes = dir.read_private(&held, FILE_NAME)?;
        Self::from_pkcs8(&bytes, &dir.path().join(FILE_NAME))
    }

    fn from_pkcs8(pkcs8: &[u8], path: &Path) -> Result<Self, Box<Diagnostic>> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| {
            Box::new(Diagnostic::error(
                Code::IoReadFailed,
                format!("{} isn't an Ed25519 key", path.display()),
            ))
        })?;
        let raw: [u8; 32] = pair.public_key().as_ref().try_into().map_err(|_| {
            Box::new(Diagnostic::error(
                Code::IoReadFailed,
                format!("{} isn't an Ed25519 key", path.display()),
            ))
        })?;
        let public_key = PublicKey::Ed25519(raw);
        Ok(Self {
            key_id: device::fingerprint(&public_key),
            public_key,
            pair,
        })
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// Where the key of `name` lives under `state`.
    pub fn path(state: &Path, name: &str) -> PathBuf {
        device::person_dir(state, name).join(FILE_NAME)
    }
}

impl Signer for FileKey {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn alg(&self) -> &'static str {
        profile::ALG
    }

    fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>> {
        Ok(Signature(self.pair.sign(base).as_ref().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use ring::signature::{ED25519, UnparsedPublicKey};

    use super::*;

    use crate::testutil::TempDir;

    fn lock() -> (TempDir, UserLock) {
        let tmp = TempDir::new("file-lock");
        let lock = UserLock::at(&tmp.path().join("l")).expect("lock");
        (tmp, lock)
    }

    #[test]
    fn needs_the_opt_in() {
        let (_l, lock) = lock();
        let tmp = TempDir::new("file-optin");
        let state = tmp.path().join("state");
        let err =
            FileKey::create(&state, "nick", FileKeyOptIn::NotAllowed, &lock).expect_err("refused");
        assert!(err.message.contains("machine-grade"));
        assert!(!state.exists(), "nothing is written without the opt-in");
    }

    #[test]
    fn signs_and_reloads_without_leaking_the_key() {
        let (_l, lock) = lock();
        let tmp = TempDir::new("file-sign");
        let key =
            FileKey::create(tmp.path(), "nick", FileKeyOptIn::Allowed, &lock).expect("create");
        assert!(
            FileKey::create(tmp.path(), "nick", FileKeyOptIn::Allowed, &lock).is_err(),
            "never replaced"
        );
        let again = FileKey::load(tmp.path(), "nick", &lock).expect("load");
        assert_eq!(again.key_id(), key.key_id());
        assert_eq!(key.alg(), "ed25519");
        let sig = key.sign(b"base").expect("sign");
        UnparsedPublicKey::new(&ED25519, key.public_key().as_bytes())
            .verify(b"base", &sig.0)
            .expect("verifies");
        assert!(!format!("{key:?}").contains("pair"));
    }
}
