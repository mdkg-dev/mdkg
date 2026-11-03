//! The FIDO2 signer: a credential on a security key, made for the RP id
//! [`RP_ID`], whose every assertion needs a touch (the "user present"
//! flag) and, under the policy `uv`, the key's PIN or biometric too.
//!
//! The device keeps each graph's credential handle in the person's folder
//! (`fido2-<graph_id>.json`: its id, public key, policy, enrolment
//! evidence and binding, none of it secret). The private key never leaves
//! the security key.
//!
//! **The signature** is the one wire verifies: the authenticator signs
//! `authenticatorData ‖ SHA-256(base)`, and the signature's bytes are
//! `len ‖ authenticatorData ‖ sig`, `len` being the authenticator data's
//! length as two big-endian bytes, and `sig` DER for ES256 or 64 bytes for
//! Ed25519. Its `alg` is `fido2-es256` or `fido2-ed25519`.
//!
//! Before it returns a signature, the signer checks what wire will: the RP
//! id hash, the flags the policy needs, and the signature itself, so a
//! touch that didn't count fails here with a reason, not at the
//! coordinator.

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mdkg_format::{Code, Diagnostic};
use mdkg_ops::wire::{PersonKeyKind, Signature, Signer};
use mdkg_wire::person;
use ring::signature::{self, UnparsedPublicKey};
use rustls_pki_types::UnixTime;
use serde::{Deserialize, Serialize};

use crate::attest::{self, AttestError, Roots, Statement};
use crate::authdata::{FLAG_UP, FLAG_UV};
use crate::cose::PublicKey;
use crate::device;
use crate::enrol::{self, Binding, Evidence, RP_ID};
use crate::grade::Policy;
use crate::lock::UserLock;

/// wire's `alg` for an Ed25519 FIDO2 assertion.
pub use mdkg_wire::sign::ALG_FIDO2_ED25519 as ALG_ED25519;
/// wire's `alg` for an ES256 FIDO2 assertion.
pub use mdkg_wire::sign::ALG_FIDO2_ES256 as ALG_ES256;

/// The single handle file of an older, never-shipped layout; refused.
pub const LEGACY_FILE_NAME: &str = "fido2.json";

/// The handle's file name for a graph, in the person's folder:
/// `fido2-<graph_id>.json` (#9579, #9743 call). A FIDO2 credential is
/// bound to one graph through enrol::challenge, so each graph has its own
/// credential, attestation and handle; a security key holds many. The
/// graph id must be a plain name (lowercase letters, digits, `.`, `_`,
/// `-`): lowercase only, so two ids that differ in case never share a file
/// on a case-insensitive volume (Claude #9899); graph ids are lowercase
/// UUIDs.
pub fn handle_file(graph_id: &str) -> Result<String, Box<Diagnostic>> {
    let plain = !graph_id.is_empty()
        && graph_id.len() <= 128
        && !graph_id.starts_with('.')
        && graph_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if plain {
        Ok(format!("fido2-{graph_id}.json"))
    } else {
        Err(Box::new(Diagnostic::error(
            Code::IoWriteFailed,
            format!("{graph_id:?} isn't a graph id a handle file can be named for"),
        )))
    }
}

/// Refuses the older single-handle layout (never shipped), whose file
/// would otherwise be ignored.
fn refuse_legacy(dir: &device::PersonDir) -> Result<(), Box<Diagnostic>> {
    let path = dir.path().join(LEGACY_FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(_) => Err(Box::new(
            Diagnostic::error(
                Code::IoReadFailed,
                format!("{} is a FIDO2 handle in an older format", path.display()),
            )
            .with_hint("remove it and enrol this key again"),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(device::io_error(
            Code::IoReadFailed,
            "can't check",
            &path,
            &e,
        )),
    }
}

/// rpIdHash (32), flags (1), signCount (4).
const AUTH_DATA_MIN: usize = 37;

/// Why the security key couldn't do what was asked. Never carries a PIN,
/// key bytes or a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fido2Error {
    /// This build has no FIDO2 transport on this platform.
    Unsupported,
    /// No security key is plugged in.
    NoDevice,
    /// More than one is, and mdkg won't guess which.
    MultipleDevices,
    /// The key is there, but this user may not open it: on Linux, its
    /// hidraw device needs a udev rule or group access.
    NoAccess,
    /// The person didn't touch it in time, or declined.
    Cancelled,
    /// The key needs its PIN, or the PIN was wrong or is blocked.
    Pin,
    /// The key doesn't hold this credential.
    NoCredential,
    /// Anything else the key or the library reported.
    Failed(String),
}

impl fmt::Display for Fido2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fido2Error::Unsupported => {
                f.write_str("this mdkg build has no FIDO2 support on this platform")
            }
            Fido2Error::NoDevice => f.write_str("no FIDO2 security key is plugged in"),
            Fido2Error::MultipleDevices => {
                f.write_str("more than one FIDO2 security key is plugged in")
            }
            Fido2Error::NoAccess => f.write_str("this user may not open the FIDO2 security key"),
            Fido2Error::Cancelled => f.write_str("the security key wasn't touched, or declined"),
            Fido2Error::Pin => {
                f.write_str("the security key needs its PIN, or the PIN was refused")
            }
            Fido2Error::NoCredential => {
                f.write_str("this security key doesn't hold the person's credential")
            }
            Fido2Error::Failed(m) => write!(f, "the security key failed: {m}"),
        }
    }
}

impl std::error::Error for Fido2Error {}

/// What to do instead on a build without a FIDO2 transport. A Linux musl
/// build (the static binaries and musllinux wheels) has none: hidapi's
/// hidraw backend needs libudev, which has no static build for musl.
#[cfg(all(target_os = "linux", target_env = "musl"))]
const UNSUPPORTED_HINT: &str = "this build has no FIDO2 support: musl builds can't use FIDO2 \
     person keys; use a gnu (glibc) build, Homebrew or the macOS build, or a machine-grade \
     file key with --allow-file-key";
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
const UNSUPPORTED_HINT: &str =
    "enrol a Secure Enclave key (macOS), or a machine-grade file key with --allow-file-key";

/// The diagnostic for a FIDO2 error.
pub fn diagnostic(e: &Fido2Error) -> Box<Diagnostic> {
    let d = match e {
        Fido2Error::Unsupported => {
            Diagnostic::error(Code::ApiUnsupported, e.to_string()).with_hint(UNSUPPORTED_HINT)
        }
        Fido2Error::NoDevice | Fido2Error::MultipleDevices => {
            Diagnostic::error(Code::ApiPersonSignatureRequired, e.to_string())
                .with_hint("plug in exactly one security key: the one you enrolled")
        }
        Fido2Error::NoAccess => Diagnostic::error(Code::ApiPersonSignatureRequired, e.to_string())
            .with_hint(
                "on Linux, your user needs the key's FIDO interface: systemd 244 and later \
                 grant it through 60-fido-id.rules, which tags only hidraw interfaces whose \
                 HID descriptor is FIDO (ID_FIDO_TOKEN). On an older system add those two \
                 rules under /etc/udev/rules.d/: SUBSYSTEM==\"hidraw\", \
                 IMPORT{builtin}=\"fido_id\" and SUBSYSTEM==\"hidraw\", \
                 ENV{ID_FIDO_TOKEN}==\"1\", TAG+=\"uaccess\". Don't match by USB ids: a \
                 key's keyboard interface shares them",
            ),
        Fido2Error::Cancelled | Fido2Error::Pin => {
            Diagnostic::error(Code::ApiPersonSignatureRequired, e.to_string())
        }
        Fido2Error::NoCredential => Diagnostic::error(Code::ApiPersonUnknown, e.to_string())
            .with_hint(format!(
                "plug in the key you enrolled, or enrol this one with `{} person enroll`",
                mdkg_format::program::BIN
            )),
        Fido2Error::Failed(_) => Diagnostic::error(Code::IoReadFailed, e.to_string()),
    };
    Box::new(d)
}

/// What `makeCredential` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeCredential {
    pub auth_data: Vec<u8>,
    pub statement: Statement,
}

/// What `getAssertion` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub auth_data: Vec<u8>,
    pub signature: Vec<u8>,
}

/// The transport under the signer: USB HID on macOS and Linux, or a software fake
/// in tests. `challenge` is hashed into the client data hash by the
/// transport (`SHA-256(challenge)`).
pub trait Fido2Backend: Send + Sync {
    /// Makes a credential for `rp_id`; `uv` asks for user verification.
    fn make_credential(
        &self,
        rp_id: &str,
        challenge: &[u8],
        uv: bool,
    ) -> Result<MadeCredential, Fido2Error>;

    /// Asserts `credential_id` for `rp_id` over `challenge`, always with
    /// user presence, and with user verification when `uv`.
    fn get_assertion(
        &self,
        rp_id: &str,
        challenge: &[u8],
        credential_id: &[u8],
        uv: bool,
    ) -> Result<Assertion, Fido2Error>;
}

/// A credential's handle on the person's device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub credential_id: Vec<u8>,
    pub public_key: PublicKey,
    pub policy: Policy,
}

/// serve-store's limit on an enrolment's attestation object, as its
/// canonical CBOR (`MAX_ATTESTATION_BYTES`, #8984): a handle never keeps
/// evidence the coordinator would refuse (Terra #9354).
pub const MAX_ATTESTATION_BYTES: usize = 16 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandleFile {
    credential_id: String,
    public_key: String,
    policy: String,
    /// The makeCredential evidence, so an enrolment interrupted after the
    /// save resends the same attested request (#9160, #9162). Absent only
    /// in the older format, which is refused.
    #[serde(default)]
    evidence: Option<EvidenceFile>,
    /// What the credential was made for, compared on resume: `none` and
    /// unverified formats carry nothing else that binds them (Terra
    /// #9354, #9368).
    #[serde(default)]
    binding: Option<BindingFile>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BindingFile {
    graph_id: String,
    machine_id: String,
    human: String,
    kind: String,
    policy: String,
    commitment: String,
}

impl BindingFile {
    fn of(b: &Binding<'_>) -> Self {
        Self {
            graph_id: b.graph_id.to_owned(),
            machine_id: b.machine_id.to_owned(),
            human: b.human.to_owned(),
            kind: crate::grade::kind_str(b.kind).to_owned(),
            policy: b.policy.as_str().to_owned(),
            commitment: URL_SAFE_NO_PAD.encode(b.commitment),
        }
    }
}

/// A credential's enrolment evidence as the handle stores it. None of it
/// is secret.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFile {
    auth_data: String,
    fmt: String,
    alg: Option<i64>,
    sig: String,
    x5c: Vec<String>,
}

/// A CBOR head: major type `major` with argument `n`, shortest form.
fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend_from_slice(&[m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

fn cbor_tstr(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn cbor_bstr(out: &mut Vec<u8>, b: &[u8]) {
    cbor_head(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}

/// The enrolment evidence exactly as it is sent and stored (#9743 call):
/// the canonical CBOR attestation object `{"fmt": tstr, "attStmt":
/// {"alg": int, "sig": bstr, "x5c": [bstr]}, "authData": bstr}`, keys in
/// CTAP2's canonical order, with only the statement members present.
///
/// The statement holds the fields this build understands: complete for
/// `packed` and `none`. For formats it doesn't verify (tpm, apple, ...) it
/// holds what ctap-hid-fido2 keeps, which is lossy: such a statement isn't
/// kept as the original object. That doesn't matter, since those formats
/// are Absent(UnsupportedFormat) and never verified later.
pub fn attestation_object(auth_data: &[u8], statement: &Statement) -> Vec<u8> {
    let mut stmt = Vec::new();
    let mut members = 0u64;
    if let Some(alg) = statement.alg {
        cbor_tstr(&mut stmt, "alg");
        if alg < 0 {
            cbor_head(&mut stmt, 1, (-1 - alg) as u64);
        } else {
            cbor_head(&mut stmt, 0, alg as u64);
        }
        members += 1;
    }
    if !statement.sig.is_empty() {
        cbor_tstr(&mut stmt, "sig");
        cbor_bstr(&mut stmt, &statement.sig);
        members += 1;
    }
    if !statement.x5c.is_empty() {
        cbor_tstr(&mut stmt, "x5c");
        cbor_head(&mut stmt, 4, statement.x5c.len() as u64);
        for c in &statement.x5c {
            cbor_bstr(&mut stmt, c);
        }
        members += 1;
    }
    let mut out = Vec::with_capacity(stmt.len() + auth_data.len() + 32);
    cbor_head(&mut out, 5, 3);
    cbor_tstr(&mut out, "fmt");
    cbor_tstr(&mut out, &statement.fmt);
    cbor_tstr(&mut out, "attStmt");
    cbor_head(&mut out, 5, members);
    out.extend_from_slice(&stmt);
    cbor_tstr(&mut out, "authData");
    cbor_bstr(&mut out, auth_data);
    out
}

/// The length of [`attestation_object`]: the exact bytes the 16 KiB cap
/// measures.
pub fn attestation_object_len(auth_data: &[u8], statement: &Statement) -> usize {
    attestation_object(auth_data, statement).len()
}

impl Handle {
    /// Writes `name`'s handle, with the credential's enrolment `evidence`
    /// and the `binding` it was made for, into its folder under `state`,
    /// refusing to replace one, holding the person-key lock. Handle,
    /// evidence and binding are one file, published whole, so a crash
    /// can't leave a handle without them. [`enrol_credential`] makes the
    /// credential and saves it under one hold of the lock.
    pub fn save(
        &self,
        evidence: &Evidence,
        binding: &Binding<'_>,
        state: &Path,
        name: &str,
        lock: &UserLock,
    ) -> Result<(), Box<Diagnostic>> {
        let json = self.file_bytes(evidence, binding)?;
        let file = handle_file(binding.graph_id)?;
        let held = lock.hold()?;
        let dir = device::PersonDir::open(state, name, true, &held)?;
        refuse_legacy(&dir)?;
        dir.write_new(&held, &file, &json)
    }

    fn file_bytes(
        &self,
        evidence: &Evidence,
        binding: &Binding<'_>,
    ) -> Result<Vec<u8>, Box<Diagnostic>> {
        let Evidence::Fido2 {
            auth_data,
            statement,
        } = evidence
        else {
            return Err(Box::new(Diagnostic::error(
                Code::IoWriteFailed,
                "a FIDO2 handle holds FIDO2 evidence only",
            )));
        };
        if attestation_object_len(auth_data, statement) > MAX_ATTESTATION_BYTES {
            return Err(Box::new(Diagnostic::error(
                Code::IoWriteFailed,
                "the credential's attestation object is over 16 KiB",
            )));
        }
        let file = HandleFile {
            credential_id: URL_SAFE_NO_PAD.encode(&self.credential_id),
            public_key: URL_SAFE_NO_PAD.encode(self.public_key.as_bytes()),
            policy: self.policy.as_str().to_owned(),
            evidence: Some(EvidenceFile {
                auth_data: URL_SAFE_NO_PAD.encode(auth_data),
                fmt: statement.fmt.clone(),
                alg: statement.alg,
                sig: URL_SAFE_NO_PAD.encode(&statement.sig),
                x5c: statement
                    .x5c
                    .iter()
                    .map(|c| URL_SAFE_NO_PAD.encode(c))
                    .collect(),
            }),
            binding: Some(BindingFile::of(binding)),
        };
        serde_json::to_vec_pretty(&file)
            .map_err(|e| Box::new(Diagnostic::error(Code::IoWriteFailed, e.to_string())))
    }

    /// Reads `name`'s handle for `graph_id` from its folder under
    /// `state`, holding the person-key lock.
    pub fn load(
        state: &Path,
        name: &str,
        graph_id: &str,
        lock: &UserLock,
    ) -> Result<Self, Box<Diagnostic>> {
        read_handle(state, name, graph_id, lock).map(|(h, _, _)| h)
    }
}

/// Reads a handle, its evidence and the binding it was saved for.
fn read_handle(
    state: &Path,
    name: &str,
    graph_id: &str,
    lock: &UserLock,
) -> Result<(Handle, Evidence, BindingFile), Box<Diagnostic>> {
    let file = handle_file(graph_id)?;
    let held = lock.hold()?;
    let dir = device::PersonDir::open(state, name, false, &held)?;
    refuse_legacy(&dir)?;
    let path = dir.path().join(&file);
    let bytes = dir.read_private(&held, &file)?;
    drop(held);
    let bad = || {
        Box::new(
            Diagnostic::error(
                Code::IoReadFailed,
                format!("{} isn't a FIDO2 credential handle", path.display()),
            )
            .with_hint("remove it, revoke the key if it was approved, and enrol again"),
        )
    };
    let b64 = |s: &str| URL_SAFE_NO_PAD.decode(s).map_err(|_| bad());
    let file: HandleFile = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let credential_id = b64(&file.credential_id)?;
    let public_key = PublicKey::from_bytes(&b64(&file.public_key)?).ok_or_else(bad)?;
    let policy = Policy::parse(&file.policy).ok_or_else(bad)?;
    if credential_id.is_empty() {
        return Err(bad());
    }
    let (Some(ev), Some(saved_for)) = (file.evidence, file.binding) else {
        return Err(Box::new(
            Diagnostic::error(
                Code::IoReadFailed,
                format!(
                    "{} has no enrolment evidence or binding (an older format)",
                    path.display()
                ),
            )
            .with_hint("remove it and enrol this key again"),
        ));
    };
    // The file's name is only an index: the graph inside it must be the
    // one asked for, so a handle renamed or restored under another
    // graph's name is never that graph's (#9748, Terra #9916).
    if saved_for.graph_id != graph_id {
        return Err(Box::new(
            Diagnostic::error(
                Code::IoReadFailed,
                format!(
                    "{} holds another graph's FIDO2 credential handle",
                    path.display()
                ),
            )
            .with_hint(format!(
                "if this graph had a FIDO2 key enrolled, its handle was overwritten and that key can't sign here any more: have it revoked with `{} person revoke` (signed by another of your keys or an approver), then remove this file (only it) and enrol this graph's key again",
                mdkg_format::program::BIN
            )),
        ));
    }
    let auth_data = b64(&ev.auth_data)?;
    let statement = Statement {
        fmt: ev.fmt,
        alg: ev.alg,
        sig: b64(&ev.sig)?,
        x5c: ev.x5c.iter().map(|c| b64(c)).collect::<Result<_, _>>()?,
    };
    if attestation_object_len(&auth_data, &statement) > MAX_ATTESTATION_BYTES {
        return Err(bad());
    }
    let handle = Handle {
        credential_id,
        public_key,
        policy,
    };
    Ok((
        handle,
        Evidence::Fido2 {
            auth_data,
            statement,
        },
        saved_for,
    ))
}

/// Resumes an enrolment whose request never went out: loads `name`'s
/// handle and returns it with the same evidence, for an identical resend
/// through the enrolment flow's existing pins (#9162). The binding the
/// handle was saved for must be exactly `binding` (graph, machine,
/// person, kind, policy and commitment), whatever the attestation format
/// (Terra #9354, #9368); the evidence is rechecked too, so a packed one
/// must also sign [`enrol::challenge`] of this binding, and its credential
/// must be the handle's. A handle saved for another enrolment is refused.
pub fn resume(
    state: &Path,
    name: &str,
    binding: &Binding<'_>,
    lock: &UserLock,
) -> Result<(Handle, Evidence), Box<Diagnostic>> {
    let (handle, evidence, saved_for) = read_handle(state, name, binding.graph_id, lock)?;
    let Evidence::Fido2 {
        auth_data,
        statement,
    } = &evidence
    else {
        unreachable!("read_handle returns FIDO2 evidence");
    };
    let other = || {
        Box::new(
            Diagnostic::error(
                Code::IoReadFailed,
                "this FIDO2 handle was saved for another enrolment",
            )
            .with_hint("remove it and enrol this key again"),
        )
    };
    if binding.kind != PersonKeyKind::Fido2
        || binding.policy != handle.policy
        || saved_for != BindingFile::of(binding)
    {
        return Err(other());
    }
    let (data, _) = attest::verify(
        auth_data,
        statement,
        RP_ID,
        &enrol::challenge(binding),
        binding.policy,
        &Roots::none(now()),
    )
    .map_err(|_| other())?;
    if data.credential_id != handle.credential_id || data.public_key != handle.public_key {
        return Err(other());
    }
    Ok((handle, evidence))
}

fn now() -> UnixTime {
    UnixTime::since_unix_epoch(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

/// Makes `name`'s credential for an enrolment on the plugged-in key and
/// saves its handle, all under one hold of the person-key lock (Terra
/// #9347), so two processes never create credentials at once. Refuses if
/// a handle is already there: resume it ([`resume`]) or remove it.
pub fn enrol_credential(
    backend: &dyn Fido2Backend,
    binding: &Binding<'_>,
    state: &Path,
    name: &str,
    lock: &UserLock,
) -> Result<(Handle, Evidence), Box<Diagnostic>> {
    // A binding this crate could never resume or check is refused before
    // the person is asked to touch anything (Terra #9579).
    if binding.kind != PersonKeyKind::Fido2 || !binding.policy.allowed_for(binding.kind) {
        return Err(Box::new(Diagnostic::error(
            Code::IoWriteFailed,
            "a FIDO2 credential needs a FIDO2 binding",
        )));
    }
    let file = handle_file(binding.graph_id)?;
    let held = lock.hold()?;
    let dir = device::PersonDir::open(state, name, true, &held)?;
    refuse_legacy(&dir)?;
    // Only this graph's handle blocks creation (#9743 call).
    let path = dir.path().join(&file);
    // Only "not found" means no handle; any other error is returned before
    // the person is asked to touch the key (Claude #9518).
    match handle_lookup(&path) {
        Ok(_) => {
            return Err(Box::new(
                Diagnostic::error(
                    Code::IoWriteFailed,
                    format!(
                        "{} already holds this graph's FIDO2 credential handle",
                        path.display()
                    ),
                )
                .with_hint(
                    "resume that enrolment, or remove this graph's handle (only it) and enrol again",
                ),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(device::io_error(
                Code::IoReadFailed,
                "can't check",
                &path,
                &e,
            ));
        }
    }
    let (handle, evidence) = create(backend, binding)?;
    let json = handle.file_bytes(&evidence, binding)?;
    dir.write_new(&held, &file, &json)?;
    Ok((handle, evidence))
}

#[cfg(test)]
thread_local! {
    /// An error the handle lookup returns, for tests.
    static FAIL_LOOKUP: std::cell::Cell<Option<std::io::ErrorKind>> = const { std::cell::Cell::new(None) };
}

/// Whether this graph's handle exists, by `symlink_metadata`.
fn handle_lookup(path: &Path) -> std::io::Result<fs::Metadata> {
    #[cfg(test)]
    if let Some(kind) = FAIL_LOOKUP.with(|f| f.get()) {
        return Err(std::io::Error::from(kind));
    }
    fs::symlink_metadata(path)
}

/// Runs a backend call, turning a panic into an authenticator-data error:
/// ctap-hid-fido2 3.6.0's `parse_cbor_authdata` slices the first 32 bytes
/// of a device's authData without checking its length, so a device that
/// returns a short authData would otherwise abort the process (Terra
/// #9579; to be reported upstream). The workspace doesn't build with
/// `panic = "abort"`, so this holds in release builds too.
fn contained<T>(f: impl FnOnce() -> T) -> Result<T, Box<Diagnostic>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|_| {
        Box::new(Diagnostic::error(
            Code::ApiPersonSignatureRequired,
            AttestError::AuthData(crate::authdata::AuthDataError::Truncated).to_string(),
        ))
    })
}

/// Makes the credential for an enrolment on the plugged-in key; returns
/// its handle and the evidence the enrolment request carries. The
/// coordinator checks that evidence again ([`enrol::check`]); checking it
/// here first means a key that didn't set its flags fails on the person's
/// screen. Not under the lock: [`enrol_credential`] is the public way.
pub(crate) fn create(
    backend: &dyn Fido2Backend,
    binding: &Binding<'_>,
) -> Result<(Handle, Evidence), Box<Diagnostic>> {
    let made = contained(|| {
        backend.make_credential(
            RP_ID,
            &enrol::challenge(binding),
            binding.policy.needs_verified(),
        )
    })
    .map_err(|e| {
        Box::new((*e).with_hint(
            "the security key may now hold a credential mdkg didn't save; remove it with the key's own manager",
        ))
    })?
    .map_err(|e| diagnostic(&e))?;
    let (data, _) = attest::verify(
        &made.auth_data,
        &made.statement,
        RP_ID,
        &enrol::challenge(binding),
        binding.policy,
        &Roots::none(now()),
    )
    .map_err(|e| {
        Box::new(Diagnostic::error(
            Code::ApiPersonSignatureRequired,
            e.to_string(),
        ))
    })?;
    let handle = Handle {
        credential_id: data.credential_id,
        public_key: data.public_key,
        policy: binding.policy,
    };
    let evidence = Evidence::Fido2 {
        auth_data: made.auth_data,
        statement: made.statement,
    };
    Ok((handle, evidence))
}

/// A person's FIDO2 credential, ready to sign.
pub struct Fido2Key {
    key_id: String,
    handle: Handle,
    backend: Box<dyn Fido2Backend>,
}

impl fmt::Debug for Fido2Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fido2Key")
            .field("key_id", &self.key_id)
            .field("policy", &self.handle.policy)
            .finish_non_exhaustive()
    }
}

impl Fido2Key {
    pub fn new(handle: Handle, backend: Box<dyn Fido2Backend>) -> Self {
        Self {
            key_id: device::fingerprint(&handle.public_key),
            handle,
            backend,
        }
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    fn refused(&self, why: &str) -> Box<Diagnostic> {
        Box::new(Diagnostic::error(
            Code::ApiPersonSignatureRequired,
            format!("the security key's assertion {why}"),
        ))
    }
}

impl Signer for Fido2Key {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn alg(&self) -> &'static str {
        match self.handle.public_key {
            PublicKey::P256(_) => ALG_ES256,
            PublicKey::Ed25519(_) => ALG_ED25519,
        }
    }

    fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>> {
        let uv = self.handle.policy.needs_verified();
        let a = contained(|| {
            self.backend
                .get_assertion(RP_ID, base, &self.handle.credential_id, uv)
        })?
        .map_err(|e| diagnostic(&e))?;
        if a.auth_data.len() < AUTH_DATA_MIN {
            return Err(self.refused("is truncated"));
        }
        if a.auth_data[..32] != attest::rp_id_hash(RP_ID) {
            return Err(self.refused("is for another RP id"));
        }
        let flags = a.auth_data[32];
        if flags & FLAG_UP == 0 {
            return Err(self.refused("doesn't show the key was touched"));
        }
        if uv && flags & FLAG_UV == 0 {
            return Err(self.refused("doesn't show the PIN or biometric the key's policy needs"));
        }
        let signed = person::assertion_message(&a.auth_data, base);
        let alg: &dyn signature::VerificationAlgorithm = match self.handle.public_key {
            PublicKey::P256(_) => &signature::ECDSA_P256_SHA256_ASN1,
            PublicKey::Ed25519(_) => &signature::ED25519,
        };
        UnparsedPublicKey::new(alg, self.handle.public_key.as_bytes())
            .verify(&signed, &a.signature)
            .map_err(|_| self.refused("doesn't verify with the enrolled key"))?;
        person::encode_assertion(&a.auth_data, &a.signature)
            .ok_or_else(|| self.refused("is too large"))
    }
}

/// How a request to the key verifies its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(
        target_os = "macos",
        all(target_os = "linux", not(target_env = "musl"))
    )),
    allow(dead_code)
)]
enum Verification<'a> {
    /// Presence only.
    Skip,
    /// The PIN the person gave.
    Pin(&'a str),
    /// The key's own biometric or PIN entry.
    BuiltIn,
}

/// A supplied PIN is always used: `up` is a minimum, and a key that asks
/// for its PIN on every operation (alwaysUv, or PIN-protected creation)
/// refuses without it (Terra #8056 9). Without a PIN, `uv` asks the key
/// for its built-in verification, and `up` asks for presence alone.
#[cfg_attr(
    not(any(
        target_os = "macos",
        all(target_os = "linux", not(target_env = "musl"))
    )),
    allow(dead_code)
)]
fn verification(uv: bool, pin: Option<&str>) -> Verification<'_> {
    match (uv, pin) {
        (_, Some(pin)) => Verification::Pin(pin),
        (true, None) => Verification::BuiltIn,
        (false, None) => Verification::Skip,
    }
}

/// The platform's FIDO2 transport: USB HID on macOS and Linux gnu (where
/// opening the key needs a udev rule or group access to its hidraw
/// device), and [`Fido2Error::Unsupported`] elsewhere, Linux musl included. `pin` is the key's
/// PIN, for a `uv` policy on a key without a biometric; it is held in
/// memory only.
pub fn platform(pin: Option<String>) -> Box<dyn Fido2Backend> {
    Box::new(hid::Hid::new(pin))
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", not(target_env = "musl"))
))]
mod hid {
    use ctap_hid_fido2::fidokey::{
        CredentialSupportedKeyType, GetAssertionArgsBuilder, MakeCredentialArgsBuilder,
    };
    use ctap_hid_fido2::{FidoKeyHid, FidoKeyHidFactory, LibCfg};

    use super::{Assertion, Fido2Backend, Fido2Error, MadeCredential, Verification, verification};
    use crate::attest::Statement;

    pub(super) struct Hid {
        pin: Option<String>,
    }

    impl Hid {
        pub(super) fn new(pin: Option<String>) -> Self {
            Self { pin }
        }
    }

    /// The library's errors are text; sort them by the CTAP status names
    /// and messages it uses.
    fn classify(e: &impl std::fmt::Display) -> Fido2Error {
        let m = e.to_string();
        if m.contains("FIDO device not found") {
            Fido2Error::NoDevice
        } else if m.contains("Multiple FIDO devices") {
            Fido2Error::MultipleDevices
        } else if m.contains("Permission denied")
            || m.contains("Failed to open device")
            || m.contains("failed to open device")
        {
            Fido2Error::NoAccess
        } else if m.contains("PIN") || m.contains("PUAT") {
            Fido2Error::Pin
        } else if m.contains("NO_CREDENTIALS") {
            Fido2Error::NoCredential
        } else if m.contains("OPERATION_DENIED")
            || m.contains("KEEPALIVE_CANCEL")
            || m.contains("USER_ACTION_TIMEOUT")
            || m.contains("ACTION_TIMEOUT")
        {
            Fido2Error::Cancelled
        } else {
            Fido2Error::Failed(m.chars().take(200).collect())
        }
    }

    /// The key types offered at creation, most preferred first: wire
    /// verifies both (Terra #8056 10).
    const KEY_TYPES: [CredentialSupportedKeyType; 2] = [
        CredentialSupportedKeyType::Ecdsa256,
        CredentialSupportedKeyType::Ed25519,
    ];

    fn device() -> Result<FidoKeyHid, Fido2Error> {
        // The touch prompt goes to stderr: stdout carries JSON.
        let cfg = LibCfg::init().with_keep_alive_msg_to_stderr(true);
        FidoKeyHidFactory::create(&cfg).map_err(|e| classify(&e))
    }

    impl Fido2Backend for Hid {
        fn make_credential(
            &self,
            rp_id: &str,
            challenge: &[u8],
            uv: bool,
        ) -> Result<MadeCredential, Fido2Error> {
            let key = device()?;
            let mut args = MakeCredentialArgsBuilder::new(rp_id, challenge);
            for t in KEY_TYPES {
                args = args.key_type(t);
            }
            args = match verification(uv, self.pin.as_deref()) {
                Verification::Skip => args.without_pin_and_uv(),
                Verification::Pin(pin) => args.pin(pin),
                Verification::BuiltIn => args,
            };
            let att = key
                .make_credential_with_args(&args.build())
                .map_err(|e| classify(&e))?;
            // The library reports a missing alg as 0; any other value is
            // passed on, for `none` too, where the checker refuses it
            // (Terra #10099).
            let alg = (att.attstmt_alg != 0).then_some(i64::from(att.attstmt_alg));
            Ok(MadeCredential {
                auth_data: att.auth_data,
                statement: Statement {
                    fmt: att.fmt,
                    alg,
                    sig: att.attstmt_sig,
                    x5c: att.attstmt_x5c,
                },
            })
        }

        fn get_assertion(
            &self,
            rp_id: &str,
            challenge: &[u8],
            credential_id: &[u8],
            uv: bool,
        ) -> Result<Assertion, Fido2Error> {
            let key = device()?;
            let mut args =
                GetAssertionArgsBuilder::new(rp_id, challenge).credential_id(credential_id);
            args = match verification(uv, self.pin.as_deref()) {
                Verification::Skip => args.without_pin_and_uv(),
                Verification::Pin(pin) => args.pin(pin),
                Verification::BuiltIn => args,
            };
            let mut all = key
                .get_assertion_with_args(&args.build())
                .map_err(|e| classify(&e))?;
            // One credential id was asked for, so one assertion answers.
            if all.len() != 1 {
                return Err(Fido2Error::Failed(
                    "the key returned more than one assertion".into(),
                ));
            }
            let a = all.remove(0);
            Ok(Assertion {
                auth_data: a.auth_data,
                signature: a.signature,
            })
        }
    }
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", not(target_env = "musl"))
)))]
mod hid {
    use super::{Assertion, Fido2Backend, Fido2Error, MadeCredential};

    pub(super) struct Hid;

    impl Hid {
        pub(super) fn new(_pin: Option<String>) -> Self {
            Self
        }
    }

    impl Fido2Backend for Hid {
        fn make_credential(
            &self,
            _: &str,
            _: &[u8],
            _: bool,
        ) -> Result<MadeCredential, Fido2Error> {
            Err(Fido2Error::Unsupported)
        }

        fn get_assertion(
            &self,
            _: &str,
            _: &[u8],
            _: &[u8],
            _: bool,
        ) -> Result<Assertion, Fido2Error> {
            Err(Fido2Error::Unsupported)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use mdkg_ops::wire::PersonKeyKind;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::authdata;
    use crate::cose::ALG_ES256 as COSE_ES256;
    use crate::enrol::FileKeyOptIn;
    use crate::grade::{Attestation, Grade};

    /// A software security key with one P-256 credential, whose flags and
    /// RP id each test sets.
    pub(crate) struct Fake {
        pair: EcdsaKeyPair,
        flags: Mutex<u8>,
        rp_id: &'static str,
    }

    impl Fake {
        pub(crate) fn set_flags(&self, flags: u8) {
            *self.flags.lock().expect("lock") = flags;
        }

        pub(crate) fn new(flags: u8) -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("gen");
            Self {
                pair: EcdsaKeyPair::from_pkcs8(
                    &ECDSA_P256_SHA256_ASN1_SIGNING,
                    pkcs8.as_ref(),
                    &rng,
                )
                .expect("key"),
                flags: Mutex::new(flags),
                rp_id: RP_ID,
            }
        }

        fn public(&self) -> PublicKey {
            PublicKey::from_bytes(self.pair.public_key().as_ref()).expect("P-256")
        }

        fn sign(&self, auth_data: &[u8], challenge: &[u8]) -> Vec<u8> {
            let mut signed = auth_data.to_vec();
            signed.extend_from_slice(&Sha256::digest(challenge));
            self.pair
                .sign(&SystemRandom::new(), &signed)
                .expect("sign")
                .as_ref()
                .to_vec()
        }
    }

    impl Fido2Backend for Fake {
        fn make_credential(
            &self,
            rp_id: &str,
            challenge: &[u8],
            _uv: bool,
        ) -> Result<MadeCredential, Fido2Error> {
            let flags = *self.flags.lock().expect("lock");
            let auth_data = authdata::build_attested(
                attest::rp_id_hash(rp_id),
                flags,
                0,
                [0; 16],
                b"cred",
                &self.public(),
            );
            let sig = self.sign(&auth_data, challenge);
            Ok(MadeCredential {
                auth_data,
                statement: Statement {
                    fmt: "packed".into(),
                    alg: Some(COSE_ES256),
                    sig,
                    x5c: Vec::new(),
                },
            })
        }

        fn get_assertion(
            &self,
            _rp_id: &str,
            challenge: &[u8],
            credential_id: &[u8],
            _uv: bool,
        ) -> Result<Assertion, Fido2Error> {
            if credential_id != b"cred" {
                return Err(Fido2Error::NoCredential);
            }
            let flags = *self.flags.lock().expect("lock");
            let mut auth_data = attest::rp_id_hash(self.rp_id).to_vec();
            auth_data.push(flags);
            auth_data.extend_from_slice(&1u32.to_be_bytes());
            let signature = self.sign(&auth_data, challenge);
            Ok(Assertion {
                auth_data,
                signature,
            })
        }
    }

    const COMMITMENT: [u8; 32] = [1; 32];

    fn binding(policy: Policy) -> Binding<'static> {
        Binding {
            graph_id: "graph-1",
            machine_id: "mdkg:machine",
            human: "human:nick",
            kind: PersonKeyKind::Fido2,
            policy,
            commitment: &COMMITMENT,
        }
    }

    #[test]
    fn enrols_and_signs_in_wires_format() {
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, evidence) = create(&fake, &b).expect("create");
        // The coordinator's check passes; self attestation is machine-grade.
        let checked = enrol::check(
            &b,
            &evidence,
            FileKeyOptIn::NotAllowed,
            &Roots::none(UnixTime::now()),
        )
        .expect("checks");
        assert_eq!(
            (checked.attestation, checked.grade),
            (Attestation::None, Grade::MachineGrade)
        );
        assert_eq!(checked.public_key, handle.public_key);

        let key = Fido2Key::new(handle, Box::new(fake));
        assert_eq!(key.alg(), "fido2-es256");
        assert_eq!(key.key_id(), device::fingerprint(&checked.public_key));
        let sig = key.sign(b"the base").expect("sign").0;
        // len ‖ authenticatorData ‖ DER sig, over authData ‖ SHA-256(base).
        let len = usize::from(u16::from_be_bytes([sig[0], sig[1]]));
        let (auth, der) = sig[2..].split_at(len);
        assert_eq!(&auth[..32], &attest::rp_id_hash("mdkg"));
        let mut signed = auth.to_vec();
        signed.extend_from_slice(&Sha256::digest(b"the base"));
        UnparsedPublicKey::new(
            &signature::ECDSA_P256_SHA256_ASN1,
            key.handle().public_key.as_bytes(),
        )
        .verify(&signed, der)
        .expect("verifies as wire would");
    }

    #[test]
    fn refuses_an_assertion_without_the_flags_its_policy_needs() {
        let fake = Fake::new(FLAG_UP | FLAG_UV);
        let (handle, _) = create(&fake, &binding(Policy::Verified)).expect("create");
        *fake.flags.lock().expect("lock") = FLAG_UP;
        let key = Fido2Key::new(handle.clone(), Box::new(fake));
        let err = key.sign(b"base").expect_err("no UV");
        assert_eq!(err.code, Code::ApiPersonSignatureRequired);

        let fake = Fake::new(0);
        let key = Fido2Key::new(
            Handle {
                policy: Policy::Present,
                public_key: fake.public(),
                ..handle.clone()
            },
            Box::new(fake),
        );
        assert_eq!(
            key.sign(b"base").expect_err("no UP").code,
            Code::ApiPersonSignatureRequired
        );

        let mut fake = Fake::new(FLAG_UP);
        fake.rp_id = "example.com";
        let key = Fido2Key::new(
            Handle {
                policy: Policy::Present,
                public_key: fake.public(),
                ..handle.clone()
            },
            Box::new(fake),
        );
        assert!(
            key.sign(b"base")
                .expect_err("other RP")
                .message
                .contains("RP id")
        );

        // A key that isn't the enrolled one.
        let fake = Fake::new(FLAG_UP);
        let key = Fido2Key::new(
            Handle {
                policy: Policy::Present,
                ..handle
            },
            Box::new(fake),
        );
        assert!(
            key.sign(b"base")
                .expect_err("other key")
                .message
                .contains("enrolled key")
        );
    }

    #[test]
    fn enrolment_fails_on_screen_without_the_flags() {
        let fake = Fake::new(FLAG_UP);
        let err = create(&fake, &binding(Policy::Verified)).expect_err("no UV");
        assert_eq!(err.code, Code::ApiPersonSignatureRequired);
    }

    #[test]
    fn handles_round_trip_and_are_never_replaced() {
        let tmp = crate::testutil::TempDir::new("fido2-handle");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP | FLAG_UV);
        let b = binding(Policy::Verified);
        let (handle, evidence) = create(&fake, &b).expect("create");
        handle
            .save(&evidence, &b, tmp.path(), "nick", &lock)
            .expect("save");
        assert!(
            handle
                .save(&evidence, &b, tmp.path(), "nick", &lock)
                .is_err(),
            "never replaced"
        );
        assert_eq!(
            Handle::load(tmp.path(), "nick", "graph-1", &lock).expect("load"),
            handle
        );
        let path =
            device::person_dir(tmp.path(), "nick").join(handle_file("graph-1").expect("file"));
        let text = std::fs::read_to_string(path).expect("read");
        assert!(text.contains("\"policy\": \"uv\""));
        assert!(text.contains("\"evidence\""));
    }

    /// The process saves the handle, then crashes before the enrolment
    /// request goes out. The next run resumes it and sends the identical
    /// request, which the coordinator's check accepts (#9162).
    #[test]
    fn a_crash_after_save_resumes_the_identical_request() {
        let tmp = crate::testutil::TempDir::new("fido2-resume");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, evidence) = create(&fake, &b).expect("create");
        handle
            .save(&evidence, &b, tmp.path(), "nick", &lock)
            .expect("save");
        drop((handle, evidence.clone()));
        let (again, resent) = resume(tmp.path(), "nick", &b, &lock).expect("resume");
        assert_eq!(resent, evidence, "the same evidence, byte for byte");
        let checked = enrol::check(
            &b,
            &resent,
            FileKeyOptIn::NotAllowed,
            &Roots::none(UnixTime::now()),
        )
        .expect("the coordinator accepts it");
        assert_eq!(checked.public_key, again.public_key);
    }

    #[test]
    fn a_handle_for_another_binding_is_refused() {
        let tmp = crate::testutil::TempDir::new("fido2-other");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, evidence) = create(&fake, &b).expect("create");
        handle
            .save(&evidence, &b, tmp.path(), "nick", &lock)
            .expect("save");
        let other_commitment = [9u8; 32];
        for other in [
            Binding {
                graph_id: "graph-2",
                ..b
            },
            Binding {
                human: "human:eve",
                ..b
            },
            Binding {
                commitment: &other_commitment,
                ..b
            },
            Binding {
                policy: Policy::Verified,
                ..b
            },
        ] {
            let err = resume(tmp.path(), "nick", &other, &lock).expect_err("refused");
            // Another graph has no handle of its own here; the rest find
            // this graph's handle and refuse it.
            let want = if other.graph_id != "graph-1" {
                "can't read"
            } else {
                "another enrolment"
            };
            assert!(err.message.contains(want), "{other:?}: {}", err.message);
        }
    }

    #[test]
    fn a_handle_without_evidence_is_refused_with_the_hint() {
        let tmp = crate::testutil::TempDir::new("fido2-old");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let old = format!(
            "{{\"credential_id\":\"{}\",\"public_key\":\"{}\",\"policy\":\"up\"}}",
            URL_SAFE_NO_PAD.encode(b"cred"),
            URL_SAFE_NO_PAD.encode(fake.public().as_bytes())
        );
        {
            let held = lock.hold().expect("hold");
            let dir = device::PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
            dir.write_new(
                &held,
                &handle_file("graph-1").expect("file"),
                old.as_bytes(),
            )
            .expect("write");
        }
        for err in [
            Handle::load(tmp.path(), "nick", "graph-1", &lock).expect_err("refused"),
            resume(tmp.path(), "nick", &binding(Policy::Present), &lock).expect_err("refused"),
        ] {
            assert!(
                err.message.contains("no enrolment evidence"),
                "{}",
                err.message
            );
            assert!(err.hint.as_deref().is_some_and(|h| h.contains("remove it")));
        }
    }

    #[test]
    fn a_supplied_pin_is_always_used() {
        assert_eq!(verification(false, Some("1234")), Verification::Pin("1234"));
        assert_eq!(verification(true, Some("1234")), Verification::Pin("1234"));
        assert_eq!(verification(true, None), Verification::BuiltIn);
        assert_eq!(verification(false, None), Verification::Skip);
    }

    /// A musl build has no FIDO2 transport: the platform backend refuses
    /// with the stable api.unsupported code and a hint naming the builds
    /// that have one.
    #[cfg(all(target_os = "linux", target_env = "musl"))]
    #[test]
    fn musl_build_refuses_fido2_with_a_hint() {
        let hid = platform(None);
        assert_eq!(
            hid.make_credential(RP_ID, b"challenge", true),
            Err(Fido2Error::Unsupported)
        );
        assert_eq!(
            hid.get_assertion(RP_ID, b"challenge", b"credential", true),
            Err(Fido2Error::Unsupported)
        );
        let d = diagnostic(&Fido2Error::Unsupported);
        assert_eq!(d.code, Code::ApiUnsupported);
        let hint = d.hint.as_deref().expect("hint");
        assert!(hint.contains("musl") && hint.contains("gnu"), "{hint}");
    }

    #[test]
    fn errors_map_to_api_codes_without_secrets() {
        let d = diagnostic(&Fido2Error::NoAccess);
        assert_eq!(d.code, Code::ApiPersonSignatureRequired);
        let hint = d.hint.as_deref().expect("hint");
        assert!(
            hint.contains("ID_FIDO_TOKEN") && !hint.contains("ATTRS{idVendor}"),
            "the FIDO interface alone, not every interface with the key's USB ids"
        );
        assert_eq!(
            diagnostic(&Fido2Error::Unsupported).code,
            Code::ApiUnsupported
        );
        assert_eq!(
            diagnostic(&Fido2Error::Cancelled).code,
            Code::ApiPersonSignatureRequired
        );
        assert_eq!(
            diagnostic(&Fido2Error::NoCredential).code,
            Code::ApiPersonUnknown
        );
    }

    /// A `none` handle has no signature binding it, so its saved binding
    /// is what refuses another enrolment (Terra #9354, #9368).
    #[test]
    fn a_none_format_handle_for_another_binding_is_refused() {
        let tmp = crate::testutil::TempDir::new("fido2-none");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, evidence) = create(&fake, &b).expect("create");
        let Evidence::Fido2 { auth_data, .. } = evidence else {
            panic!("fido2");
        };
        let none = Evidence::Fido2 {
            auth_data,
            statement: Statement {
                fmt: "none".into(),
                ..Statement::default()
            },
        };
        handle
            .save(&none, &b, tmp.path(), "nick", &lock)
            .expect("save");
        resume(tmp.path(), "nick", &b, &lock).expect("its own binding resumes");
        let other_commitment = [9u8; 32];
        for other in [
            Binding {
                graph_id: "graph-2",
                ..b
            },
            Binding {
                machine_id: "mdkg:other",
                ..b
            },
            Binding {
                human: "human:eve",
                ..b
            },
            Binding {
                commitment: &other_commitment,
                ..b
            },
        ] {
            let err = resume(tmp.path(), "nick", &other, &lock).expect_err("refused");
            let want = if other.graph_id != "graph-1" {
                "can't read"
            } else {
                "another enrolment"
            };
            assert!(err.message.contains(want), "{other:?}: {}", err.message);
        }
    }

    /// Credential creation holds the lock and refuses a second credential
    /// while a handle exists (Terra #9347).
    #[test]
    fn enrol_credential_makes_one_credential_and_saves_it() {
        let tmp = crate::testutil::TempDir::new("fido2-enrol");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, _) = enrol_credential(&fake, &b, tmp.path(), "nick", &lock).expect("enrols");
        assert_eq!(
            Handle::load(tmp.path(), "nick", "graph-1", &lock).expect("saved"),
            handle
        );
        let err = enrol_credential(&fake, &b, tmp.path(), "nick", &lock).expect_err("refused");
        assert!(err.message.contains("already holds"), "{}", err.message);
    }

    /// The cap is the attestation object's CBOR size, at serve-store's
    /// 16 KiB (Terra #9354).
    #[test]
    fn the_evidence_cap_is_the_encoded_object_at_16_kib() {
        let tmp = crate::testutil::TempDir::new("fido2-cap");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        let (handle, evidence) = create(&fake, &b).expect("create");
        let Evidence::Fido2 {
            auth_data,
            statement,
        } = evidence
        else {
            panic!("fido2");
        };
        let sized = |n: usize| Statement {
            sig: vec![0; n],
            ..statement.clone()
        };
        let fit = (0..MAX_ATTESTATION_BYTES)
            .rev()
            .find(|&n| attestation_object_len(&auth_data, &sized(n)) <= MAX_ATTESTATION_BYTES)
            .expect("a size that fits");
        assert_eq!(
            attestation_object_len(&auth_data, &sized(fit)),
            MAX_ATTESTATION_BYTES
        );
        let at = |n: usize| Evidence::Fido2 {
            auth_data: auth_data.clone(),
            statement: sized(n),
        };
        assert!(
            handle
                .save(&at(fit + 1), &b, tmp.path(), "nick", &lock)
                .is_err(),
            "one over"
        );
        handle
            .save(&at(fit), &b, tmp.path(), "nick", &lock)
            .expect("exactly at the cap");
    }

    #[test]
    fn the_cbor_length_matches_an_encoding() {
        // {"fmt": "none", "attStmt": {}, "authData": h'00'}
        let want = [
            &[0xa3, 0x63][..],
            b"fmt",
            &[0x64],
            b"none",
            &[0x67],
            b"attStmt",
            &[0xa0, 0x68],
            b"authData",
            &[0x41, 0x00],
        ]
        .concat();
        let none = Statement {
            fmt: "none".into(),
            ..Statement::default()
        };
        assert_eq!(attestation_object(&[0], &none), want, "the exact bytes");
        assert_eq!(attestation_object_len(&[0], &none), want.len());
        // {"alg": -7, "sig": h'01'} as attStmt: 1+3+1 + 1+3+2.
        let packed = Statement {
            fmt: "none".into(),
            alg: Some(-7),
            sig: vec![1],
            x5c: Vec::new(),
        };
        assert_eq!(
            attestation_object_len(&[0], &packed),
            want.len() + 4 + 1 + 4 + 2
        );
    }

    /// The person-key lock is held while the credential is made: another
    /// holder is locked out through makeCredential (Claude #9518, Terra #9347).
    #[test]
    fn the_lock_is_held_while_the_credential_is_made() {
        // Observed directly, by a non-blocking attempt during the creation:
        // no thread, no timing (Terra #9679, #10099).
        struct Watching {
            inner: Fake,
            lock: crate::lock::UserLock,
            held: std::sync::Mutex<Option<bool>>,
        }
        impl Fido2Backend for Watching {
            fn make_credential(
                &self,
                rp: &str,
                c: &[u8],
                uv: bool,
            ) -> Result<MadeCredential, Fido2Error> {
                *self.held.lock().expect("lock") = Some(self.lock.is_held());
                self.inner.make_credential(rp, c, uv)
            }
            fn get_assertion(
                &self,
                rp: &str,
                c: &[u8],
                id: &[u8],
                uv: bool,
            ) -> Result<Assertion, Fido2Error> {
                self.inner.get_assertion(rp, c, id, uv)
            }
        }
        let tmp = crate::testutil::TempDir::new("fido2-held");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let backend = Watching {
            inner: Fake::new(FLAG_UP),
            lock: lock.clone(),
            held: std::sync::Mutex::new(None),
        };
        enrol_credential(
            &backend,
            &binding(Policy::Present),
            tmp.path(),
            "nick",
            &lock,
        )
        .expect("enrols");
        assert_eq!(
            *backend.held.lock().expect("lock"),
            Some(true),
            "the lock was held while the credential was made"
        );
        assert!(!lock.is_held(), "and released after");
    }

    /// Longer CBOR heads and an x5c array, against a hand encoding
    /// (Claude #9518).
    #[test]
    fn the_cbor_length_matches_a_long_encoding() {
        let auth_data = vec![7u8; 300]; // a two-byte bstr head
        let sig = vec![1u8; 256]; // a two-byte head
        let c1 = vec![2u8; 70_000]; // a four-byte head
        let c2 = vec![3u8; 30]; // a two-byte head
        let statement = Statement {
            fmt: "packed".into(),
            alg: Some(-257), // a two-byte negative argument
            sig: sig.clone(),
            x5c: vec![c1.clone(), c2.clone()],
        };
        let mut want = vec![0xa3, 0x63];
        want.extend_from_slice(b"fmt");
        want.push(0x66);
        want.extend_from_slice(b"packed");
        want.push(0x67);
        want.extend_from_slice(b"attStmt");
        want.push(0xa3);
        want.push(0x63);
        want.extend_from_slice(b"alg");
        want.extend_from_slice(&[0x39, 0x01, 0x00]); // -257
        want.push(0x63);
        want.extend_from_slice(b"sig");
        want.extend_from_slice(&[0x59, 0x01, 0x00]);
        want.extend_from_slice(&sig);
        want.push(0x63);
        want.extend_from_slice(b"x5c");
        want.push(0x82);
        want.extend_from_slice(&[0x5a, 0x00, 0x01, 0x11, 0x70]); // 70_000
        want.extend_from_slice(&c1);
        want.extend_from_slice(&[0x58, 30]); // 30 > 23: a two-byte head
        want.extend_from_slice(&c2);
        want.push(0x68);
        want.extend_from_slice(b"authData");
        want.extend_from_slice(&[0x59, 0x01, 0x2c]);
        want.extend_from_slice(&auth_data);
        assert_eq!(
            attestation_object(&auth_data, &statement),
            want,
            "the exact bytes"
        );
        assert_eq!(attestation_object_len(&auth_data, &statement), want.len());
    }

    /// A binding that isn't FIDO2 is refused before the hardware is
    /// touched: no credential and no handle (Terra #9579).
    /// A backend that counts make-credential calls.
    struct Counting(Fake, std::sync::atomic::AtomicUsize);

    impl Fido2Backend for Counting {
        fn make_credential(
            &self,
            rp: &str,
            c: &[u8],
            uv: bool,
        ) -> Result<MadeCredential, Fido2Error> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.make_credential(rp, c, uv)
        }
        fn get_assertion(
            &self,
            rp: &str,
            c: &[u8],
            id: &[u8],
            uv: bool,
        ) -> Result<Assertion, Fido2Error> {
            self.0.get_assertion(rp, c, id, uv)
        }
    }

    /// A handle lookup that fails other than "not found" (EIO, EACCES) is
    /// returned before the person is asked to touch the key (Terra #9679).
    #[test]
    fn a_failed_handle_lookup_touches_nothing() {
        let tmp = crate::testutil::TempDir::new("fido2-lookup");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let backend = Counting(Fake::new(FLAG_UP), std::sync::atomic::AtomicUsize::new(0));
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            FAIL_LOOKUP.with(|f| f.set(Some(kind)));
            let err = enrol_credential(
                &backend,
                &binding(Policy::Present),
                tmp.path(),
                "nick",
                &lock,
            )
            .expect_err("refused");
            FAIL_LOOKUP.with(|f| f.set(None));
            assert_eq!(err.code, Code::IoReadFailed, "{}", err.message);
        }
        assert_eq!(
            backend.1.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no touch"
        );
        // And with the lookup working, the same call enrols.
        enrol_credential(
            &backend,
            &binding(Policy::Present),
            tmp.path(),
            "nick",
            &lock,
        )
        .expect("enrols");
    }

    #[test]
    fn a_non_fido2_binding_touches_nothing() {
        let tmp = crate::testutil::TempDir::new("fido2-kind");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let backend = Counting(Fake::new(FLAG_UP), std::sync::atomic::AtomicUsize::new(0));
        let b = Binding {
            kind: PersonKeyKind::File,
            ..binding(Policy::Present)
        };
        assert!(enrol_credential(&backend, &b, tmp.path(), "nick", &lock).is_err());
        assert_eq!(
            backend.1.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no credential"
        );
        assert!(
            !device::person_dir(tmp.path(), "nick")
                .join(handle_file("graph-1").expect("file"))
                .exists(),
            "no handle"
        );
    }

    /// Each graph has its own credential and handle; one graph's handle
    /// blocks only that graph (#9743 call).
    #[test]
    fn each_graph_has_its_own_handle() {
        let tmp = crate::testutil::TempDir::new("fido2-graphs");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        // Two authenticators' worth of credentials, so the handles differ
        // (Terra #9916).
        let fake = Fake::new(FLAG_UP);
        let other = Fake::new(FLAG_UP);
        let a = binding(Policy::Present);
        let b = Binding {
            graph_id: "graph-2",
            ..a
        };
        let (ha, _) = enrol_credential(&fake, &a, tmp.path(), "nick", &lock).expect("graph A");
        let (hb, _) = enrol_credential(&other, &b, tmp.path(), "nick", &lock).expect("graph B too");
        assert_ne!(ha.public_key, hb.public_key, "distinct credentials");
        assert_eq!(
            Handle::load(tmp.path(), "nick", "graph-1", &lock).expect("A"),
            ha
        );
        assert_eq!(
            Handle::load(tmp.path(), "nick", "graph-2", &lock).expect("B"),
            hb
        );
        resume(tmp.path(), "nick", &a, &lock).expect("A resumes");
        resume(tmp.path(), "nick", &b, &lock).expect("B resumes");
        let err = enrol_credential(&fake, &a, tmp.path(), "nick", &lock).expect_err("A again");
        assert!(err.message.contains("this graph's"), "{}", err.message);
        assert!(handle_file("../x").is_err() && handle_file("").is_err());
        assert!(
            handle_file("Graph-A").is_err(),
            "lowercase only (Claude #9899)"
        );
        // Graph A's handle copied under graph 3's name isn't graph 3's.
        let dir = device::person_dir(tmp.path(), "nick");
        let copied = dir.join(handle_file("graph-3").expect("file"));
        fs::copy(dir.join(handle_file("graph-1").expect("file")), &copied).expect("copy");
        let err = Handle::load(tmp.path(), "nick", "graph-3", &lock).expect_err("refused");
        assert!(err.message.contains("another graph's"), "{}", err.message);
        // Over an enrolled graph's handle: refused, and the hint says to
        // revoke that graph's old key first (Terra #10099).
        let b_file = dir.join(handle_file("graph-2").expect("file"));
        fs::remove_file(&b_file).expect("rm");
        fs::copy(dir.join(handle_file("graph-1").expect("file")), &b_file).expect("copy over B");
        let err = Handle::load(tmp.path(), "nick", "graph-2", &lock).expect_err("refused");
        let hint = err.hint.as_deref().expect("hint");
        // Both must be there: None < Some would pass a dropped step
        // (Terra #10276).
        let at = |s: &str| hint.find(s).unwrap_or_else(|| panic!("{s} in {hint}"));
        assert!(at("revoke") < at("remove this file"), "{hint}");
    }

    /// The older single fido2.json is refused with the hint.
    #[test]
    fn the_legacy_handle_file_is_refused() {
        let tmp = crate::testutil::TempDir::new("fido2-legacy");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        {
            let held = lock.hold().expect("hold");
            let dir = device::PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
            dir.write_new(&held, LEGACY_FILE_NAME, b"{}")
                .expect("write");
        }
        let fake = Fake::new(FLAG_UP);
        let b = binding(Policy::Present);
        for err in [
            enrol_credential(&fake, &b, tmp.path(), "nick", &lock)
                .map(|_| ())
                .expect_err("enrol"),
            resume(tmp.path(), "nick", &b, &lock)
                .map(|_| ())
                .expect_err("resume"),
        ] {
            assert!(err.message.contains("older format"), "{}", err.message);
            assert!(err.hint.as_deref().is_some_and(|h| h.contains("remove it")));
        }
    }

    /// A backend that panics (as ctap-hid-fido2 does on a short authData)
    /// gives an authenticator-data error with the remove-credential hint,
    /// and no handle (#9743 call 7).
    #[test]
    fn a_panicking_backend_is_contained() {
        struct Panicking;
        impl Fido2Backend for Panicking {
            fn make_credential(
                &self,
                _: &str,
                _: &[u8],
                _: bool,
            ) -> Result<MadeCredential, Fido2Error> {
                let short: &[u8] = &[1, 2, 3];
                let _ = &short[..32]; // the library's unchecked slice
                unreachable!()
            }
            fn get_assertion(
                &self,
                _: &str,
                _: &[u8],
                _: &[u8],
                _: bool,
            ) -> Result<Assertion, Fido2Error> {
                let short: &[u8] = &[1, 2, 3];
                let _ = &short[..32];
                unreachable!()
            }
        }
        let tmp = crate::testutil::TempDir::new("fido2-panic");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let err = enrol_credential(
            &Panicking,
            &binding(Policy::Present),
            tmp.path(),
            "nick",
            &lock,
        )
        .expect_err("contained");
        assert!(
            err.message.contains("authenticator data"),
            "{}",
            err.message
        );
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("may now hold a credential"))
        );
        assert!(
            !device::person_dir(tmp.path(), "nick")
                .join(handle_file("graph-1").expect("f"))
                .exists()
        );
        let fake = Fake::new(FLAG_UP);
        let key = Fido2Key::new(
            Handle {
                credential_id: b"cred".to_vec(),
                public_key: fake.public(),
                policy: Policy::Present,
            },
            Box::new(Panicking),
        );
        assert!(
            key.sign(b"base").is_err(),
            "an assertion panic is contained too"
        );
    }

    /// Two creators at once, each blocking in makeCredential: exactly one
    /// credential is made, one creator wins, the other is refused (Terra
    /// #9579 #6).
    #[test]
    fn concurrent_creators_make_one_credential() {
        struct Slow(Fake, std::sync::atomic::AtomicUsize);
        impl Fido2Backend for Slow {
            fn make_credential(
                &self,
                rp: &str,
                c: &[u8],
                uv: bool,
            ) -> Result<MadeCredential, Fido2Error> {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(100));
                self.0.make_credential(rp, c, uv)
            }
            fn get_assertion(
                &self,
                rp: &str,
                c: &[u8],
                id: &[u8],
                uv: bool,
            ) -> Result<Assertion, Fido2Error> {
                self.0.get_assertion(rp, c, id, uv)
            }
        }
        let tmp = crate::testutil::TempDir::new("fido2-race");
        let lock = crate::lock::UserLock::at(&tmp.path().join("locks")).expect("lock");
        let backend = std::sync::Arc::new(Slow(
            Fake::new(FLAG_UP),
            std::sync::atomic::AtomicUsize::new(0),
        ));
        let state = tmp.path().to_path_buf();
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (backend, lock, state) = (backend.clone(), lock.clone(), state.clone());
                std::thread::spawn(move || {
                    enrol_credential(
                        backend.as_ref(),
                        &binding(Policy::Present),
                        &state,
                        "nick",
                        &lock,
                    )
                    .is_ok()
                })
            })
            .collect();
        let won = threads
            .into_iter()
            .map(|t| t.join().expect("join"))
            .filter(|ok| *ok)
            .count();
        assert_eq!(won, 1, "one creator wins");
        assert_eq!(
            backend.1.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one credential made"
        );
    }
}
