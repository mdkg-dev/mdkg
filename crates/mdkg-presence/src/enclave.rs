//! The Secure Enclave signer, on macOS: a P-256 key that never leaves the
//! enclave, created with access control `userPresence`, so every signature
//! needs Touch ID or the login password. That is established for the keys
//! mdkg makes and records; a key found under the label is trusted only
//! with mdkg's record of making it, since the platform can't read its
//! access control back (#9521 D1, #9571 A).
//!
//! What it doesn't prove (P1, decided 2026-10-29): Apple offers no
//! attestation a coordinator can check for a key a command-line tool made,
//! so an enclave key enrols with attestation `none` and is
//! `machine-grade`. It still beats a file key: the key can't be copied off
//! the device, and the person is asked for each signature.
//!
//! A lasting enclave key needs the data-protection keychain, which only a
//! binary signed with a keychain-access-groups entitlement may use. A build
//! without it (`cargo install` makes one) gets [`EnclaveError::NotEntitled`]
//! when it creates the key, and `person enroll` refuses the enclave kind
//! with [`unavailable`]'s diagnostic.

use std::fmt;

use mdkg_format::{Code, Diagnostic};
use mdkg_ops::wire::{Signature, Signer, profile};

use crate::cose::PublicKey;
use crate::device;
use crate::device::PersonDir;
use crate::lock::{Held, UserLock};
use crate::sig;

/// Why the enclave couldn't do what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnclaveError {
    /// Not macOS, or a Mac without a Secure Enclave.
    Unavailable,
    /// This binary lacks the keychain entitlement a lasting enclave key
    /// needs (`errSecMissingEntitlement`).
    NotEntitled,
    /// The person cancelled Touch ID or the password prompt, or failed it.
    Cancelled,
    /// No enclave key with that label on this device.
    NotFound,
    /// Anything else the Security framework reported, by its code.
    Failed(i64),
    /// The person-key lock couldn't be taken.
    Lock(String),
    /// The creation record or its intent couldn't be read or written; the
    /// cause is in the message (Terra #9855 4).
    Record(String),
    /// The key under this label can't be shown to be a Secure Enclave key
    /// (#9521 D1): its token isn't the enclave's, or can't be read.
    NotSecureEnclave(String),
    /// A Secure Enclave key under this label that mdkg has no record of
    /// making (#9571 A).
    Unrecorded(String),
    /// A Secure Enclave key under this label that isn't the one mdkg
    /// recorded (#9571 A).
    NotRecorded(String),
    /// The Secure Enclave returned a signature that doesn't verify against
    /// this signer's key (Terra #9840).
    BadSignature,
    /// A key under this label, with the intent of an enrolment that was
    /// interrupted before mdkg recorded its key. Which key it is can't be
    /// known (that enrolment's, another program's, a replacement), so it
    /// is refused and never deleted automatically (#10133, Terra #10099).
    Interrupted(String),
}

impl fmt::Display for EnclaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnclaveError::Unavailable => f.write_str("this device has no Secure Enclave"),
            EnclaveError::NotEntitled => f.write_str(
                "this mdkg build isn't signed with the keychain entitlement a Secure Enclave key needs",
            ),
            EnclaveError::Cancelled => f.write_str("Touch ID or the password was cancelled"),
            EnclaveError::NotFound => f.write_str("no Secure Enclave key for this person on this device"),
            EnclaveError::Failed(code) => write!(f, "the Security framework failed (OSStatus {code})"),
            EnclaveError::Lock(why) => write!(f, "can't take the person-key lock: {why}"),
            EnclaveError::Record(why) => write!(f, "the enclave key's record: {why}"),
            EnclaveError::NotSecureEnclave(label) => write!(
                f,
                "a key labelled {label} is in the keychain, but mdkg can't establish that it is a Secure Enclave key, so it won't use it"
            ),
            EnclaveError::Unrecorded(label) => write!(
                f,
                "a Secure Enclave key labelled {label} is in the keychain, but mdkg has no record of making it, so it won't use it"
            ),
            EnclaveError::NotRecorded(label) => write!(
                f,
                "the Secure Enclave key labelled {label} isn't the one mdkg recorded making, so it won't use it"
            ),
            EnclaveError::BadSignature => f.write_str(
                "the Secure Enclave returned a signature that doesn't verify against this key",
            ),
            EnclaveError::Interrupted(label) => write!(
                f,
                "an enrolment on this device was interrupted before mdkg recorded its key, and a key labelled {label} is in the keychain that mdkg can't vouch for, so it won't use or delete it"
            ),
        }
    }
}

impl std::error::Error for EnclaveError {}

/// The diagnostic for an enclave error. `NotEntitled` and `Unavailable`
/// are `api.unsupported`, and name the kinds that remain.
pub fn unavailable(e: &EnclaveError) -> Box<Diagnostic> {
    let d = match e {
        EnclaveError::Unavailable | EnclaveError::NotEntitled => Diagnostic::error(
            Code::ApiUnsupported,
            e.to_string(),
        )
        .with_hint(
            "enrol a FIDO2 key (--kind fido2), or a machine-grade file key with --allow-file-key",
        ),
        EnclaveError::Cancelled => {
            Diagnostic::error(Code::ApiPersonSignatureRequired, e.to_string())
        }
        EnclaveError::NotFound => Diagnostic::error(Code::ApiPersonUnknown, e.to_string())
            .with_hint(format!(
                "enrol a key on this device with `{} person enroll`",
                mdkg_format::program::BIN
            )),
        EnclaveError::Interrupted(label) => {
            // The one explicit recovery: nothing is deleted automatically
            // (#10133, on Terra #10099). The person as S41 takes it
            // (Terra #10085).
            let name = person_of(label);
            Diagnostic::error(Code::IoWriteFailed, e.to_string()).with_hint(format!(
                "nothing was deleted. If an enrolment may use this key (mdkg can't tell which key it is), first have each one revoked with `{bin} person revoke`, signed by another of your enrolled keys or by an approver. Then run `{bin} person remove-device-key human:{name}` to remove the key under this label and the interrupted enrolment's marker, and `{bin} person enroll human:{name}` again",
                bin = mdkg_format::program::BIN
            ))
        }
        EnclaveError::Failed(_) | EnclaveError::Record(_) | EnclaveError::BadSignature => {
            Diagnostic::error(Code::IoReadFailed, e.to_string())
        }
        EnclaveError::Lock(_) => Diagnostic::error(Code::IoWriteFailed, e.to_string()),
        EnclaveError::NotSecureEnclave(label)
        | EnclaveError::Unrecorded(label)
        | EnclaveError::NotRecorded(label) => {
            let name = person_of(label);
            // Revocation comes before removal: once the key is gone, nothing
            // on this device can sign for it (Terra #9628).
            Diagnostic::error(Code::IoWriteFailed, e.to_string()).with_hint(format!(
                "nothing was changed. This one label holds this person's key for every graph it is enrolled in on this device. If an enrolment may use it, first have each one revoked with `{bin} person revoke`, signed by another of your enrolled keys or by an approver, since mdkg won't sign with this key; then remove it with `{bin} person remove-device-key human:{name}` and enrol again. If no enrolment used it, removing it is safe",
                bin = mdkg_format::program::BIN
            ))
        }
    };
    Box::new(d)
}

/// The platform under the signer: the Security framework on macOS, or a
/// software fake in tests. A key is found by one lookup, which returns a
/// reference ([`EnclaveRef`]); its token, its public key and every
/// signature come from that reference, and the label is never searched a
/// second time, so no swap between lookups can pair one key's checks with
/// another's signature (#9736, on Terra #9727).
pub trait EnclaveBackend: Send + Sync {
    /// Creates a key under `label`, refusing if one exists.
    fn create(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError>;

    /// The one key under `label`, by one lookup.
    fn find(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError>;

    /// Deletes whatever key is under `label`.
    fn delete(&self, label: &str) -> Result<(), EnclaveError>;
}

/// One key, as one lookup found it.
pub trait EnclaveRef: Send + Sync {
    /// Its public key, SEC1 uncompressed, derived from this reference.
    fn public_key(&self) -> Result<[u8; 65], EnclaveError>;

    /// Its token is the Secure Enclave, from this reference's own
    /// attributes; `false` when they can't say.
    fn secure_enclave(&self) -> bool;

    /// Signs SHA-256 of `msg` (ECDSA P-256) with this key, asking the
    /// person first; returns the X9.62 DER signature, which the signer
    /// converts.
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, EnclaveError>;
}

/// The creation record of `name`'s enclave key: its SEC1 public key, in
/// the person-key lock's folder, the per-user scope the label has (#9571
/// A, #9869 D2). The key is trusted only with it, so it must survive: a
/// lost record leaves the enrolled key refused ([`EnclaveError::Unrecorded`])
/// until it is revoked, removed and enrolled again (Claude #9797 3). Back
/// up that folder with the keychain.
pub fn record_file(name: &str) -> String {
    format!("enclave-{name}.pub")
}

/// The intent written before a key is created, and cleared once it is
/// recorded (#9869 D1).
pub fn intent_file(name: &str) -> String {
    format!("enclave-{name}.creating")
}

/// The one check on every path that turns a stored key into a signer
/// (`open`, `open_or_create`; #9521 D1, #9571 A): the key found must have
/// the Secure Enclave token, and its public key, from the same reference,
/// must be the one mdkg recorded when it made the key; otherwise it is
/// refused, fail closed, and never used. mdkg makes every key with
/// userPresence in the same protocol that records it, so a recorded key
/// needs the person. The residual: a program with mdkg's keychain access
/// group and write access to this user's own 0700 lock folder could plant
/// both a key and its record, which is within the same user's trust
/// boundary.
fn trusted(
    key: &dyn EnclaveRef,
    label: &str,
    recorded: Option<[u8; 65]>,
) -> Result<[u8; 65], EnclaveError> {
    if !key.secure_enclave() {
        return Err(EnclaveError::NotSecureEnclave(label.to_owned()));
    }
    let raw = key.public_key()?;
    match recorded {
        None => Err(EnclaveError::Unrecorded(label.to_owned())),
        Some(r) if r == raw => Ok(raw),
        Some(_) => Err(EnclaveError::NotRecorded(label.to_owned())),
    }
}

fn record_error(d: &Diagnostic) -> EnclaveError {
    EnclaveError::Record(d.message.clone())
}

/// The lock folder's records, under the held lock.
struct Records<'a> {
    dir: PersonDir,
    held: &'a Held,
    record: String,
    intent: String,
}

impl<'a> Records<'a> {
    fn open(lock: &UserLock, held: &'a Held, name: &str) -> Result<Self, EnclaveError> {
        Ok(Self {
            dir: PersonDir::lock_folder(lock, held).map_err(|d| record_error(&d))?,
            held,
            record: record_file(name),
            intent: intent_file(name),
        })
    }

    fn recorded(&self) -> Result<Option<[u8; 65]>, EnclaveError> {
        if !self
            .dir
            .exists(self.held, &self.record)
            .map_err(|d| record_error(&d))?
        {
            return Ok(None);
        }
        let bytes = self
            .dir
            .read_private(self.held, &self.record)
            .map_err(|d| record_error(&d))?;
        bytes.try_into().map(Some).map_err(|_| {
            EnclaveError::Record(format!(
                "{} isn't a P-256 public key",
                self.dir.path().join(&self.record).display()
            ))
        })
    }

    fn intended(&self) -> Result<bool, EnclaveError> {
        self.dir
            .exists(self.held, &self.intent)
            .map_err(|d| record_error(&d))
    }

    fn remove(&self, file: &str) -> Result<(), EnclaveError> {
        self.dir
            .remove(self.held, file)
            .map_err(|d| record_error(&d))
    }

    fn write(&self, file: &str, bytes: &[u8]) -> Result<(), EnclaveError> {
        self.dir
            .write_new(self.held, file, bytes)
            .map_err(|d| record_error(&d))
    }
}

fn hold(lock: &UserLock) -> Result<Held, EnclaveError> {
    lock.hold()
        .map_err(|d| EnclaveError::Lock(d.message.clone()))
}

/// A person's enclave key on this device.
pub struct EnclaveKey {
    key_id: String,
    public_key: PublicKey,
    key: Box<dyn EnclaveRef>,
}

impl fmt::Debug for EnclaveKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnclaveKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The person a label names, for hints.
fn person_of(label: &str) -> &str {
    label
        .strip_prefix(device::ENCLAVE_LABEL_PREFIX)
        .unwrap_or(label)
}

/// The keychain label of a valid person name.
fn label_of(name: &str) -> Result<String, EnclaveError> {
    device::person_name(&format!("human:{name}"))
        .map(device::enclave_label)
        .ok_or(EnclaveError::NotFound)
}

impl EnclaveKey {
    /// `name`'s key on this device: a new one if none is under its label,
    /// or the existing one if it passes the shared check ([`trusted`]).
    /// An existing key that doesn't is refused and left as it is;
    /// removing it is [`EnclaveKey::remove`]. This is the path for
    /// enrolment.
    ///
    /// Creation is a durable protocol (#9869 D1): (a) the intent is
    /// written and synced, (b) the key is created, (c) its public key is
    /// recorded and synced, then the intent is cleared. A key found with
    /// an intent and no record is refused as
    /// [`EnclaveError::Interrupted`], and the intent kept: that key may be
    /// the interrupted creation's, another program's, or a replacement,
    /// so nothing deletes it but the person's explicit
    /// [`EnclaveKey::remove`] (#10133, on Terra #10099). A key with
    /// neither intent nor record is refused too, as
    /// [`EnclaveError::Unrecorded`]. With no key under the label, a
    /// leftover record or intent is stale and is replaced: a creation
    /// interrupted before (b) left no key (Claude #9797 2, Terra #9855 1).
    ///
    /// The whole look-then-create holds the person-key lock
    /// ([`UserLock`], one per OS user, #8674), as does every [`open`]: no
    /// two creators race, so the label never holds two keys, and no opener
    /// sees a key whose creation hasn't finished.
    ///
    /// [`open`]: EnclaveKey::open
    pub fn open_or_create(
        backend: Box<dyn EnclaveBackend>,
        name: &str,
        lock: &UserLock,
    ) -> Result<Self, EnclaveError> {
        let label = label_of(name)?;
        let held = hold(lock)?;
        let records = Records::open(lock, &held, name)?;
        let intended = records.intended()?;
        match backend.find(&label) {
            Ok(key) => {
                let recorded = records.recorded()?;
                if recorded.is_none() && intended {
                    return Err(EnclaveError::Interrupted(label));
                }
                let raw = trusted(key.as_ref(), &label, recorded)?;
                if intended {
                    // Interrupted after (c): the record stands.
                    records.remove(&records.intent)?;
                }
                return Ok(Self::new(key, raw));
            }
            Err(EnclaveError::NotFound) => {}
            Err(e) => return Err(e),
        }
        // No key under the label now: any record is stale.
        records.remove(&records.record)?;
        records.remove(&records.intent)?;
        records.write(&records.intent, label.as_bytes())?;
        let key = backend.create(&label)?;
        // A failure from here leaves the intent, so the next
        // open_or_create refuses this key until the person removes it.
        if !key.secure_enclave() {
            return Err(EnclaveError::NotSecureEnclave(label));
        }
        let raw = key.public_key()?;
        records.write(&records.record, &raw)?;
        records.remove(&records.intent)?;
        Ok(Self::new(key, raw))
    }

    /// Deletes `name`'s key from this device's keychain, then its record
    /// and any intent, holding the person-key lock: the way out of a
    /// refused key. Its enrolments are revoked separately, and first (see
    /// [`unavailable`]'s hint).
    pub fn remove(
        backend: &dyn EnclaveBackend,
        name: &str,
        lock: &UserLock,
    ) -> Result<(), EnclaveError> {
        let label = label_of(name)?;
        let held = hold(lock)?;
        let records = Records::open(lock, &held, name)?;
        match backend.delete(&label) {
            Ok(()) | Err(EnclaveError::NotFound) => {}
            Err(e) => return Err(e),
        }
        records.remove(&records.record)?;
        records.remove(&records.intent)
    }

    /// Opens `name`'s existing key for signing, holding the person-key
    /// lock, after the same check as [`open_or_create`](Self::open_or_create)
    /// (a Secure Enclave token and mdkg's record of the key, fail closed).
    /// For signing with an enrolled key only, never for enrolment:
    /// enrolment goes through `open_or_create`. It creates nothing.
    pub fn open(
        backend: Box<dyn EnclaveBackend>,
        name: &str,
        lock: &UserLock,
    ) -> Result<Self, EnclaveError> {
        let label = label_of(name)?;
        let held = hold(lock)?;
        let records = Records::open(lock, &held, name)?;
        let key = backend.find(&label)?;
        let recorded = records.recorded()?;
        if recorded.is_none() && records.intended()? {
            return Err(EnclaveError::Interrupted(label));
        }
        let raw = trusted(key.as_ref(), &label, recorded)?;
        Ok(Self::new(key, raw))
    }

    fn new(key: Box<dyn EnclaveRef>, raw: [u8; 65]) -> Self {
        let public_key = PublicKey::P256(raw);
        Self {
            key_id: device::fingerprint(&public_key),
            public_key,
            key,
        }
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

impl Signer for EnclaveKey {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn alg(&self) -> &'static str {
        profile::ALG_P256
    }

    /// Signs as RFC 9421's `ecdsa-p256-sha256`: `r ‖ s`, 64 bytes. It
    /// signs with the reference that passed the check, never a key found
    /// again by label (#9736), so a label removed and given a new key
    /// since can't put the new key's signature under this key's id
    /// (Terra #9466, #9628).
    fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>> {
        let PublicKey::P256(mine) = self.public_key else {
            unreachable!("an enclave key is P-256");
        };
        let der = self.key.sign(base).map_err(|e| unavailable(&e))?;
        let fixed = sig::der_to_fixed(&der).ok_or_else(|| {
            Box::new(Diagnostic::error(
                Code::IoReadFailed,
                "the Secure Enclave returned a signature that isn't strict DER",
            ))
        })?;
        // A signature that doesn't verify against the pinned key is never
        // returned (Claude #9586).
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &mine)
            .verify(base, &fixed)
            .map_err(|_| unavailable(&EnclaveError::BadSignature))?;
        Ok(Signature(fixed.to_vec()))
    }
}

/// The platform's enclave: the Security framework on macOS, and
/// [`EnclaveError::Unavailable`] elsewhere.
pub fn platform() -> Box<dyn EnclaveBackend> {
    Box::new(apple::Apple)
}

#[cfg(target_os = "macos")]
mod apple {
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    use security_framework::item::{
        ItemClass, ItemSearchOptions, KeyClass, Location, Reference, SearchResult,
    };
    use security_framework::key::{Algorithm, GenerateKeyOptions, KeyType, SecKey, Token};

    use super::{EnclaveBackend, EnclaveError, EnclaveRef};

    /// `kSecAccessControlPrivateKeyUsage | kSecAccessControlUserPresence`:
    /// the key signs only inside the enclave, and only after Touch ID or
    /// the password.
    const FLAGS: usize = (1 << 30) | 1;

    /// `errSecMissingEntitlement`.
    const MISSING_ENTITLEMENT: i64 = -34018;
    /// `errSecUserCanceled`, `errSecAuthFailed`, and LocalAuthentication's
    /// cancel and failure codes.
    const CANCELLED: [i64; 5] = [-128, -25293, -2, -3, -4];
    /// `errSecItemNotFound`.
    const NOT_FOUND: i64 = -25300;
    /// `errSecDuplicateItem`.
    const DUPLICATE: i64 = -25299;

    #[derive(Debug)]
    pub(super) struct Apple;

    fn classify(code: i64) -> EnclaveError {
        match code {
            MISSING_ENTITLEMENT => EnclaveError::NotEntitled,
            NOT_FOUND => EnclaveError::NotFound,
            c if CANCELLED.contains(&c) => EnclaveError::Cancelled,
            c => EnclaveError::Failed(c),
        }
    }

    fn find(label: &str) -> Result<SecKey, EnclaveError> {
        let results = ItemSearchOptions::new()
            .class(ItemClass::key())
            .key_class(KeyClass::private())
            .label(label)
            .ignore_legacy_keychains()
            .load_refs(true)
            .limit(2)
            .search()
            .map_err(|e| classify(i64::from(e.code())))?;
        let mut keys = results.into_iter().filter_map(|r| match r {
            SearchResult::Ref(Reference::Key(k)) => Some(k),
            _ => None,
        });
        match (keys.next(), keys.next()) {
            (Some(k), None) => Ok(k),
            (None, _) => Err(EnclaveError::NotFound),
            // Two keys under one label: refuse rather than pick one.
            (Some(_), Some(_)) => Err(EnclaveError::Failed(DUPLICATE)),
        }
    }

    /// One key reference; everything about it comes from the reference.
    struct AppleKey(SecKey);

    impl EnclaveRef for AppleKey {
        /// SecKeyCopyPublicKey on this reference.
        fn public_key(&self) -> Result<[u8; 65], EnclaveError> {
            let public = self.0.public_key().ok_or(EnclaveError::Failed(0))?;
            let data = public
                .external_representation()
                .ok_or(EnclaveError::Failed(0))?;
            data.bytes().try_into().map_err(|_| EnclaveError::Failed(0))
        }

        /// `kSecAttrTokenID` from SecKeyCopyAttributes on this reference.
        /// The SDK declares that result nullable, and security-framework
        /// wraps it unchecked, so a null panics in core-foundation; that is
        /// contained here and read as "can't establish" (Terra #10099).
        /// The default panic hook still prints the panic to stderr first;
        /// the process-wide hook isn't swapped for it, since other threads
        /// share it (Terra #10276).
        fn secure_enclave(&self) -> bool {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                SearchResult::Dict(self.0.attributes())
                    .simplify_dict()
                    .and_then(|d| d.get("tkid").cloned())
            }))
            .ok()
            .flatten()
            .as_deref()
                == Some("com.apple.setoken")
        }

        fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, EnclaveError> {
            self.0
                .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, msg)
                .map_err(|e| classify(e.code() as i64))
        }
    }

    impl EnclaveBackend for Apple {
        fn create(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            match find(label) {
                Ok(_) => return Err(EnclaveError::Failed(DUPLICATE)),
                Err(EnclaveError::NotFound) => {}
                Err(e) => return Err(e),
            }
            let access = SecAccessControl::create_with_protection(
                Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
                FLAGS as _,
            )
            .map_err(|e| classify(i64::from(e.code())))?;
            let mut opts = GenerateKeyOptions::default();
            opts.set_key_type(KeyType::ec_sec_prime_random())
                .set_size_in_bits(256)
                .set_label(label)
                .set_token(Token::SecureEnclave)
                .set_location(Location::DataProtectionKeychain)
                .set_access_control(access);
            // Callers hold the person-key lock, so no other mdkg process
            // creates under this label meanwhile.
            let key = SecKey::new(&opts).map_err(|e| classify(e.code() as i64))?;
            Ok(Box::new(AppleKey(key)))
        }

        fn find(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            Ok(Box::new(AppleKey(find(label)?)))
        }

        fn delete(&self, label: &str) -> Result<(), EnclaveError> {
            find(label)?
                .delete()
                .map_err(|e| EnclaveError::Failed(i64::from(e.code())))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod apple {
    use super::{EnclaveBackend, EnclaveError, EnclaveRef};

    #[derive(Debug)]
    pub(super) struct Apple;

    impl EnclaveBackend for Apple {
        fn create(&self, _: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            Err(EnclaveError::Unavailable)
        }

        fn find(&self, _: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            Err(EnclaveError::Unavailable)
        }

        fn delete(&self, _: &str) -> Result<(), EnclaveError> {
            Err(EnclaveError::Unavailable)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use ring::rand::SystemRandom;
    use ring::signature::{
        ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED, EcdsaKeyPair, KeyPair,
        UnparsedPublicKey,
    };

    use super::*;
    use crate::testutil::TempDir;

    const LABEL: &str = "dev.mdkg.person.nick";

    /// A person-key lock in a temporary folder, removed with the guard.
    pub(crate) fn lock() -> (TempDir, UserLock) {
        let tmp = TempDir::new("enclave-lock");
        let lock = UserLock::at(&tmp.path().join("l")).expect("lock");
        (tmp, lock)
    }

    fn pair(pkcs8: &[u8]) -> EcdsaKeyPair {
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &SystemRandom::new())
            .expect("key")
    }

    fn generate() -> Vec<u8> {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
            .expect("generate")
            .as_ref()
            .to_vec()
    }

    fn sec1(pkcs8: &[u8]) -> [u8; 65] {
        pair(pkcs8).public_key().as_ref().try_into().expect("65")
    }

    /// A key a fake lookup found: a copy of its key and what the platform
    /// says, so it keeps signing with that key whatever the label holds
    /// later, as a retained SecKey does.
    struct FakeRef {
        pkcs8: Vec<u8>,
        enclave: bool,
        refuse: bool,
    }

    type Keys = Mutex<HashMap<String, (Vec<u8>, bool)>>;

    impl EnclaveRef for FakeRef {
        fn public_key(&self) -> Result<[u8; 65], EnclaveError> {
            Ok(sec1(&self.pkcs8))
        }

        fn secure_enclave(&self) -> bool {
            self.enclave
        }

        fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, EnclaveError> {
            if self.refuse {
                return Err(EnclaveError::Cancelled);
            }
            Ok(pair(&self.pkcs8)
                .sign(&SystemRandom::new(), msg)
                .expect("sign")
                .as_ref()
                .to_vec())
        }
    }

    /// A software enclave: keys by label (with whether each is an enclave
    /// key), an answer to each prompt, and a count of label searches.
    #[derive(Default)]
    pub(crate) struct Fake {
        keys: Keys,
        refuse: bool,
        searches: Mutex<usize>,
    }

    impl Fake {
        /// Plants a software P-256 key under `label`, as another program
        /// with the keychain group could.
        fn plant_software(&self, label: &str) {
            self.keys
                .lock()
                .expect("lock")
                .insert(label.to_owned(), (generate(), false));
        }

        /// The public key under `label` now, without a search.
        fn key_of(&self, label: &str) -> Option<[u8; 65]> {
            self.keys
                .lock()
                .expect("lock")
                .get(label)
                .map(|(k, _)| sec1(k))
        }

        fn searches(&self) -> usize {
            *self.searches.lock().expect("lock")
        }
    }

    impl EnclaveBackend for Fake {
        fn create(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            let mut keys = self.keys.lock().expect("lock");
            if keys.contains_key(label) {
                return Err(EnclaveError::Failed(-25299));
            }
            let pkcs8 = generate();
            keys.insert(label.to_owned(), (pkcs8.clone(), true));
            Ok(Box::new(FakeRef {
                pkcs8,
                enclave: true,
                refuse: self.refuse,
            }))
        }

        fn find(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            *self.searches.lock().expect("lock") += 1;
            let keys = self.keys.lock().expect("lock");
            let (pkcs8, enclave) = keys.get(label).ok_or(EnclaveError::NotFound)?;
            Ok(Box::new(FakeRef {
                pkcs8: pkcs8.clone(),
                enclave: *enclave,
                refuse: self.refuse,
            }))
        }

        fn delete(&self, label: &str) -> Result<(), EnclaveError> {
            self.keys
                .lock()
                .expect("lock")
                .remove(label)
                .map(|_| ())
                .ok_or(EnclaveError::NotFound)
        }
    }

    /// Shares one fake between several signers, as several processes share
    /// one keychain.
    struct Shared(Arc<Fake>);

    impl EnclaveBackend for Shared {
        fn create(&self, l: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            self.0.create(l)
        }
        fn find(&self, l: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            self.0.find(l)
        }
        fn delete(&self, l: &str) -> Result<(), EnclaveError> {
            self.0.delete(l)
        }
    }

    fn shared(fake: &Arc<Fake>) -> Box<dyn EnclaveBackend> {
        Box::new(Shared(fake.clone()))
    }

    /// The lock folder's records, as a test writes them by hand.
    fn records_at(lock: &UserLock) -> std::path::PathBuf {
        lock.dir()
    }

    /// An enclave whose look-then-create races, as the platform's does:
    /// the look and the create are apart, and nothing in it stops a
    /// second key under a label.
    type Labelled = Vec<(String, Vec<u8>)>;

    #[derive(Clone, Default)]
    struct Racy {
        keys: Arc<Mutex<Labelled>>,
    }

    impl EnclaveBackend for Racy {
        fn create(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let pkcs8 = generate();
            self.keys
                .lock()
                .expect("lock")
                .push((label.to_owned(), pkcs8.clone()));
            Ok(Box::new(FakeRef {
                pkcs8,
                enclave: true,
                refuse: false,
            }))
        }

        fn find(&self, label: &str) -> Result<Box<dyn EnclaveRef>, EnclaveError> {
            let keys = self.keys.lock().expect("lock");
            let mut found = keys.iter().filter(|(l, _)| l == label);
            match (found.next(), found.next()) {
                (Some((_, k)), None) => Ok(Box::new(FakeRef {
                    pkcs8: k.clone(),
                    enclave: true,
                    refuse: false,
                })),
                (None, _) => Err(EnclaveError::NotFound),
                (Some(_), Some(_)) => Err(EnclaveError::Failed(-25299)),
            }
        }

        fn delete(&self, label: &str) -> Result<(), EnclaveError> {
            self.keys.lock().expect("lock").retain(|(l, _)| l != label);
            Ok(())
        }
    }

    /// Two creators and an opener, as three processes of one user sharing
    /// the one lock and the one record beside it: one key is made, and
    /// every one of them ends with that key (Terra #8666, #8765).
    #[test]
    fn concurrent_creators_and_openers_share_one_key() {
        let (_st, lock) = lock();
        let racy = Racy::default();
        let creators: Vec<_> = (0..2)
            .map(|_| {
                let (racy, lock) = (racy.clone(), lock.clone());
                std::thread::spawn(move || {
                    EnclaveKey::open_or_create(Box::new(racy), "nick", &lock)
                        .map(|k| k.public_key().clone())
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let opener = {
            let (racy, lock) = (racy.clone(), lock.clone());
            std::thread::spawn(move || {
                EnclaveKey::open(Box::new(racy), "nick", &lock).map(|k| k.public_key().clone())
            })
        };
        let keys: Vec<PublicKey> = creators
            .into_iter()
            .map(|t| t.join().expect("join").expect("a key"))
            .collect();
        assert_eq!(keys[0], keys[1], "both creators have the one key");
        assert_eq!(
            racy.keys.lock().expect("lock").len(),
            1,
            "one key under the label"
        );
        // The opener either waited for the creation or ran first and found
        // none; it never saw a key mid-creation.
        match opener.join().expect("join") {
            Ok(k) => assert_eq!(k, keys[0]),
            Err(e) => assert_eq!(e, EnclaveError::NotFound),
        }
    }

    #[test]
    fn signs_as_ecdsa_p256_under_the_keys_fingerprint() {
        let (_st, lock) = lock();
        let key =
            EnclaveKey::open_or_create(Box::new(Fake::default()), "nick", &lock).expect("create");
        assert_eq!(key.alg(), "ecdsa-p256-sha256");
        assert_eq!(key.key_id(), device::fingerprint(key.public_key()));
        let sig = key.sign(b"base").expect("sign");
        assert_eq!(sig.0.len(), 64, "r ‖ s, as wire verifies");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_key().as_bytes())
            .verify(b"base", &sig.0)
            .expect("verifies");
        assert!(!format!("{key:?}").contains("pkcs8"));
        // The record and no intent, beside the lock (#9869 D2).
        let dir = records_at(&lock);
        assert_eq!(
            std::fs::read(dir.join(record_file("nick"))).expect("record"),
            key.public_key().as_bytes()
        );
        assert!(!dir.join(intent_file("nick")).exists());
    }

    #[test]
    fn a_cancelled_prompt_is_a_missing_person_signature() {
        let (_st, lock) = lock();
        let fake = Fake {
            refuse: true,
            ..Fake::default()
        };
        let key = EnclaveKey::open_or_create(Box::new(fake), "nick", &lock).expect("create");
        let err = key.sign(b"base").expect_err("cancelled");
        assert_eq!(err.code, Code::ApiPersonSignatureRequired);
        let err = EnclaveKey::open(Box::new(Fake::default()), "nick", &lock).expect_err("none");
        assert_eq!(unavailable(&err).code, Code::ApiPersonUnknown);
        assert_eq!(
            unavailable(&EnclaveError::NotEntitled).code,
            Code::ApiUnsupported
        );
        assert!(EnclaveKey::open(Box::new(Fake::default()), "../x", &lock).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_that_cant_be_taken_is_a_lock_error() {
        let tmp = TempDir::new("enclave-fifo");
        let lock = UserLock::at(&tmp.path().join("l")).expect("lock");
        let made = std::process::Command::new("mkfifo")
            .arg(lock.dir().join(crate::lock::LOCK_FILE))
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let err = EnclaveKey::open_or_create(Box::new(Fake::default()), "nick", &lock)
            .expect_err("refused");
        assert!(matches!(err, EnclaveError::Lock(_)));
    }

    /// A software P-256 key planted under the label is refused, and left
    /// as it is; it is never taken for an enclave key (Terra #9305).
    #[test]
    fn a_planted_software_key_is_never_reused() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        fake.plant_software(LABEL);
        let planted = fake.key_of(LABEL).expect("planted");
        let err = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect_err("refused");
        assert_eq!(err, EnclaveError::NotSecureEnclave(LABEL.into()));
        let d = unavailable(&err);
        assert!(d.message.contains(LABEL) && d.message.contains("can't establish"));
        let hint = d.hint.as_deref().expect("hint");
        assert!(
            hint.contains("remove-device-key human:nick")
                && hint.contains("every graph")
                && hint.contains("mdkg person revoke")
        );
        // Revocation is advised before removal (Terra #9628).
        let at = |s: &str| hint.find(s).unwrap_or_else(|| panic!("{s} in {hint}"));
        assert!(at("revoke") < at("remove-device-key"));
        assert_eq!(fake.key_of(LABEL), Some(planted), "nothing changed");
        // Removed, then a fresh key is made.
        EnclaveKey::remove(&Shared(fake.clone()), "nick", &lock).expect("removed");
        let key = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("created");
        assert_ne!(key.public_key().as_bytes(), &planted[..]);
    }

    #[test]
    fn a_key_mdkg_recorded_is_reused_and_opened() {
        // Its userPresence can't be read back (as on macOS); mdkg made it
        // with userPresence and recorded it, so it is trusted (#9571 A).
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        let made =
            EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("created and recorded");
        let again = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("reused");
        assert_eq!(again.public_key(), made.public_key());
        let opened = EnclaveKey::open(shared(&fake), "nick", &lock).expect("opened");
        assert_eq!(opened.public_key(), made.public_key());
    }

    /// A Secure Enclave key mdkg didn't record, with no intent (another
    /// program made it under the label) is refused by open and
    /// open_or_create, and never removed automatically (#9571 A, #9869).
    #[test]
    fn an_unrecorded_secure_enclave_key_is_refused() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        fake.create(LABEL).expect("made by someone else");
        let theirs = fake.key_of(LABEL);
        for err in [
            EnclaveKey::open(shared(&fake), "nick", &lock).map(|_| ()),
            EnclaveKey::open_or_create(shared(&fake), "nick", &lock).map(|_| ()),
        ] {
            let err = err.expect_err("refused");
            assert_eq!(err, EnclaveError::Unrecorded(LABEL.into()));
            assert!(unavailable(&err).message.contains("no record"));
        }
        assert_eq!(fake.key_of(LABEL), theirs, "left as it is");
    }

    /// A key swapped in under the label after mdkg recorded another is
    /// refused: the record doesn't match (#9571 A).
    #[test]
    fn a_swapped_key_doesnt_match_the_record() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("recorded");
        fake.delete(LABEL).expect("swap out");
        fake.create(LABEL).expect("swap in");
        let err = EnclaveKey::open(shared(&fake), "nick", &lock).expect_err("refused");
        assert_eq!(err, EnclaveError::NotRecorded(LABEL.into()));
    }

    /// open runs the same check: a planted software key is refused, fail
    /// closed (#9521 D1).
    #[test]
    fn open_refuses_a_planted_software_key() {
        let (_st, lock) = lock();
        let fake = Fake::default();
        fake.plant_software(LABEL);
        let err = EnclaveKey::open(Box::new(fake), "nick", &lock).expect_err("refused");
        assert_eq!(err, EnclaveError::NotSecureEnclave(LABEL.into()));
    }

    /// One lookup per open, and signing uses the reference it checked: a
    /// keychain that swaps the label's key after that lookup gets no
    /// signature by the new key under the old id (#9736, Terra #9727,
    /// #9466).
    #[test]
    fn a_signer_uses_only_the_key_it_checked() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("create");
        let before = fake.searches();
        let signer = EnclaveKey::open(shared(&fake), "nick", &lock).expect("open");
        assert_eq!(fake.searches(), before + 1, "one lookup");
        // The label is swapped after the lookup.
        fake.delete(LABEL).expect("swap out");
        fake.create(LABEL).expect("swap in");
        let sig = signer.sign(b"base").expect("signs with the checked key");
        assert_eq!(fake.searches(), before + 1, "signing never searches");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, signer.public_key().as_bytes())
            .verify(b"base", &sig.0)
            .expect("by the checked key");
    }

    /// A signature that doesn't verify against the pinned key (here, over
    /// another message by the same key) is refused as a bad signature,
    /// not reported as a replaced key (Claude #9586, Terra #9840).
    #[test]
    fn a_signature_that_doesnt_verify_is_refused() {
        struct OtherMessage(FakeRef);
        impl EnclaveRef for OtherMessage {
            fn public_key(&self) -> Result<[u8; 65], EnclaveError> {
                self.0.public_key()
            }
            fn secure_enclave(&self) -> bool {
                true
            }
            fn sign(&self, _: &[u8]) -> Result<Vec<u8>, EnclaveError> {
                self.0.sign(b"another message")
            }
        }
        let pkcs8 = generate();
        let raw = sec1(&pkcs8);
        let signer = EnclaveKey::new(
            Box::new(OtherMessage(FakeRef {
                pkcs8,
                enclave: true,
                refuse: false,
            })),
            raw,
        );
        let err = signer.sign(b"base").expect_err("refused");
        assert_eq!(err.code, Code::IoReadFailed);
        assert!(err.message.contains("doesn't verify"), "{}", err.message);
    }

    /// A crash between creating the key (b) and recording it (c) leaves
    /// the intent and an unrecorded key. Both open and open_or_create
    /// refuse it, delete nothing and keep the intent; the explicit
    /// removal clears both, and the next enrolment makes a fresh key
    /// (#10133, Terra #10099, Claude #10035).
    #[test]
    fn an_interrupted_creation_waits_for_explicit_removal() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        let dir = records_at(&lock);
        // (a) and (b), then the crash; or a key appearing under the label.
        std::fs::write(dir.join(intent_file("nick")), LABEL).expect("intent");
        fake.create(LABEL).expect("created");
        let orphan = fake.key_of(LABEL).expect("orphan");
        for err in [
            EnclaveKey::open(shared(&fake), "nick", &lock).map(|_| ()),
            EnclaveKey::open_or_create(shared(&fake), "nick", &lock).map(|_| ()),
        ] {
            let err = err.expect_err("refused");
            assert_eq!(err, EnclaveError::Interrupted(LABEL.into()));
            let hint = unavailable(&err).hint.expect("hint");
            // Revoke first, as for an unrecorded key: mdkg can't tell the
            // key was never enrolled (Terra #10276).
            let at = |s: &str| hint.find(s).unwrap_or_else(|| panic!("{s} in {hint}"));
            assert!(at("mdkg person revoke") < at("mdkg person remove-device-key human:nick"));
            assert!(at("remove-device-key") < at("mdkg person enroll human:nick"));
        }
        assert_eq!(fake.key_of(LABEL), Some(orphan), "nothing deleted");
        assert!(dir.join(intent_file("nick")).exists(), "the intent stays");
        EnclaveKey::remove(&Shared(fake.clone()), "nick", &lock).expect("removed");
        assert!(!dir.join(intent_file("nick")).exists());
        let key = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("fresh");
        assert_ne!(key.public_key().as_bytes(), &orphan[..]);
        EnclaveKey::open(shared(&fake), "nick", &lock).expect("recorded now");
    }

    /// An intent and no key: a crash before (b) left nothing under the
    /// label, so the key is made.
    #[test]
    fn an_intent_with_no_key_is_stale() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        std::fs::write(records_at(&lock).join(intent_file("nick")), LABEL).expect("intent");
        EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("made");
        assert!(!records_at(&lock).join(intent_file("nick")).exists());
        EnclaveKey::open(shared(&fake), "nick", &lock).expect("recorded");
    }

    /// A crash after the record (c) but before the intent is cleared: the
    /// record stands, so the key is reused and the intent cleared; a
    /// published record whose folder sync failed is the same case (Terra
    /// #9855 1).
    #[test]
    fn a_creation_interrupted_after_its_record_keeps_the_key() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        let made = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("create");
        let dir = records_at(&lock);
        std::fs::write(dir.join(intent_file("nick")), LABEL).expect("intent");
        let again = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("reused");
        assert_eq!(again.public_key(), made.public_key());
        assert!(!dir.join(intent_file("nick")).exists());
    }

    /// A record left by a removal that stopped halfway, with the keychain
    /// empty, is stale: a new key is made and recorded (Claude #9797 2).
    #[test]
    fn a_stale_record_doesnt_block_a_new_key() {
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        let old = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("create");
        fake.delete(LABEL).expect("the keychain half of a removal");
        let new = EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("a new key");
        assert_ne!(new.public_key(), old.public_key());
        EnclaveKey::open(shared(&fake), "nick", &lock).expect("recorded");
    }

    /// A record that can't be read is reported as the record's error,
    /// with its cause, not as a key that isn't an enclave key (Terra
    /// #9855 4).
    #[cfg(unix)]
    #[test]
    fn an_unreadable_record_is_a_record_error() {
        use std::os::unix::fs::PermissionsExt;
        let (_st, lock) = lock();
        let fake = Arc::new(Fake::default());
        EnclaveKey::open_or_create(shared(&fake), "nick", &lock).expect("create");
        let record = records_at(&lock).join(record_file("nick"));
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = EnclaveKey::open(shared(&fake), "nick", &lock).expect_err("refused");
        assert!(
            matches!(&err, EnclaveError::Record(why) if why.contains("readable by others")),
            "{err}"
        );
        assert_eq!(unavailable(&err).code, Code::IoReadFailed);
    }
}
