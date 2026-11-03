//! The traits and wire constants shared across the crate boundaries of
//! serve: `mdkg-ops` (the kernel), `mdkg-wire` (framing, signing, the
//! remote backend) and `mdkg-serve` (the server).
//!
//! - [`Apply`]: one served operation's `apply`, inside a transaction the
//!   kernel opened.
//! - [`Backend`]: where `apply` runs. The local store implements it here;
//!   `mdkg-wire`'s client implements it as one HTTP request.
//! - [`Signer`]: a key that signs requests. `mdkg-wire` implements it over
//!   the machine key file; the turn signing proxy (S36) holds one.
//! - [`Verified`]: who a request is from, what a remote [`Ctx`] is built
//!   from (`Ctx::remote`). Built only by `mdkg-serve`'s
//!   `Authenticated::verified()`, after the Gate's whole S5 pass
//!   (signatures, graph, boot id, window, nonce). There is no verifier
//!   trait offering a weaker path (#10448, orchestrator #10750).
//!   [`Verified::new`] is public, so this is enforced by a test that finds
//!   every caller, not by the type (Terra 3 #10813).
//! - [`KeyDirectory`]: the enrolled machine and person keys verification
//!   looks up, backed by the graph's store.
//! - [`EventStream`]: events in cursor order, from SSE or the local log.
//! - [`PushRunner`]: a thin trait over g-core's `mdkg-git` for pushes to
//!   the bound destination (S38).

use std::fmt;
use std::path::Path;

use mdkg_format::Diagnostic;
use mdkg_graph::ErrorKind;
use serde_json::Value;

use crate::ctx::Ctx;
use crate::registry::OpSpec;

/// An operation's canonical request: nothing local in it (repository-
/// relative scopes, node UUIDs, message ids), so any coordinator can apply
/// it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub op: &'static str,
    pub body: Value,
}

/// What `apply` returns: the operation's canonical result, or its refusal.
/// Each adapter builds its envelope from it (A12). Served operations
/// return JSON, the default `T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Report<T = Value> {
    pub data: Option<T>,
    pub diagnostics: Vec<Diagnostic>,
    /// Set exactly when the operation failed; gives the exit code.
    pub failure: Option<ErrorKind>,
    /// The store stayed busy past its timeout (`serve.busy_timeout_ms`), so
    /// nothing was decided and the request may be sent again as it is:
    /// HTTP 503, which clients retry.
    pub busy: bool,
}

impl<T> Report<T> {
    pub fn ok(data: T) -> Self {
        Self {
            data: Some(data),
            diagnostics: Vec::new(),
            failure: None,
            busy: false,
        }
    }

    pub fn refused(kind: ErrorKind, d: Diagnostic) -> Self {
        Self {
            data: None,
            diagnostics: vec![d],
            failure: Some(kind),
            busy: false,
        }
    }

    /// The store was busy past its timeout; `d` says so.
    pub fn busy(d: Diagnostic) -> Self {
        Self {
            busy: true,
            ..Self::refused(ErrorKind::Failure, d)
        }
    }
}

/// The store transaction a served write applies in. Named here once, so
/// that only this alias changes when mdkg-coord-sqlite re-exports it.
pub type Transaction<'a> = rusqlite::Transaction<'a>;

/// One served operation's `apply` (S16). The kernel opens `tx`, checks the
/// acting machine's row in it, calls this, writes the idempotency row and
/// the record's machine, and commits once, so a write and its key land
/// together or not at all. `apply` never commits or rolls back `tx`
/// itself; a refusal it returns rolls the whole transaction back.
pub trait Apply: Send + Sync {
    fn apply(&self, ctx: &Ctx, tx: &Transaction<'_>, request: &Request) -> Report;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// This process opens the store.
    Local,
    /// A bound coordinator, over the network.
    Remote,
}

/// Where `apply` runs. The local backend opens the store transaction and
/// passes it through to the operation's [`Apply`]; the remote backend
/// sends the request, and the coordinator's local backend does the same
/// there. A remote write that may or may not have landed is reported in
/// its result (`data.outcome: "unknown"`, S10), never retried under a new
/// key.
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;

    fn apply(&self, ctx: &Ctx, op: &'static OpSpec, request: &Request) -> Report;

    /// Opens a stream operation (`events`, turn output, `msg wait`) from
    /// the request's cursor: locally by tailing the store's log, remotely
    /// as SSE. A refusal before the stream starts is an ordinary report
    /// (A12).
    fn stream(
        &self,
        ctx: &Ctx,
        op: &'static OpSpec,
        request: &Request,
    ) -> Result<Box<dyn EventStream>, Report>;
}

/// A signature's bytes: 64 for Ed25519; DER for ECDSA P-256; a WebAuthn
/// assertion (with its authenticator data) for a FIDO2 key.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature(pub Vec<u8>);

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signature(..)")
    }
}

/// A key that signs requests (S5, S41). The private key never leaves the
/// implementation: not in a log, an error or `Debug` output.
pub trait Signer: Send + Sync {
    /// RFC 9421's `keyid`: the machine id, or the person key id.
    fn key_id(&self) -> &str;

    /// RFC 9421's `alg` for this key.
    fn alg(&self) -> &'static str;

    /// Signs RFC 9421's signature base.
    fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>>;
}

/// A machine's enrolment state in one graph (S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    Pending,
    Approved,
    Revoked,
}

/// An enrolled machine's public key and state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineKey {
    pub machine: String,
    pub public_key: [u8; 32],
    pub state: MachineState,
}

/// The kind of a person's key (S41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonKeyKind {
    /// A P-256 key in the Secure Enclave; `ecdsa-p256-sha256`.
    SecureEnclave,
    /// A FIDO2 credential (ES256 or Ed25519), as a WebAuthn assertion.
    Fido2,
    /// An Ed25519 file key, the explicit opt-in: `machine-grade`, since it
    /// doesn't prove presence.
    File,
}

/// An enrolled person key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonKey {
    pub key_id: String,
    /// The `human:*` actor it belongs to.
    pub person: String,
    pub kind: PersonKeyKind,
    pub public_key: Vec<u8>,
    /// A FIDO2 key whose policy needs the "user verified" flag too.
    pub user_verified: bool,
    pub revoked: bool,
}

/// The enrolled keys of the graph `serve` serves.
pub trait KeyDirectory: Send + Sync {
    /// Unknown gives `api.unauthenticated`, revoked `api.machine_revoked`,
    /// pending `api.machine_pending`.
    fn machine(&self, machine: &str) -> Option<MachineKey>;

    /// Unknown or revoked gives `api.person_unknown`.
    fn person(&self, key_id: &str) -> Option<PersonKey>;
}

/// Who a verified request is from. `mdkg-serve`'s Gate builds it after
/// its whole S5 pass, the boot id, window and nonce included. Only
/// [`Verified::new`] makes one, so every producer is a call a grep finds; `mdkg-serve`'s
/// `only_the_gate_makes_a_verified` test fails on any other.
/// Its fields are private, read through getters, so a holder can't
/// rewrite what the Gate checked either (Terra 2 #10838).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    machine: String,
    nonce: String,
    person: Option<(String, String)>,
}

impl Verified {
    /// For `mdkg-serve`'s `Authenticated::verified()` alone, once its
    /// whole S5 pass and the body's digest have succeeded.
    pub fn new(machine: String, nonce: String, person: Option<(String, String)>) -> Self {
        Self {
            machine,
            nonce,
            person,
        }
    }

    /// The kernel's own tests' contexts, without a Gate.
    #[cfg(test)]
    pub(crate) fn for_tests(machine: &str, nonce: &str, person: Option<(String, String)>) -> Self {
        Self::new(machine.to_owned(), nonce.to_owned(), person)
    }

    /// The approved machine that signed it.
    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// The machine signature's `nonce`, which the Gate's replay check has
    /// already used once (S7).
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// When a person signature (label `mdkg-person`) over the machine
    /// signature verified: the person key id and its `human:*` person.
    pub fn person(&self) -> Option<(&str, &str)> {
        self.person.as_ref().map(|(k, h)| (k.as_str(), h.as_str()))
    }
}

/// One event: its cursor (SSE's `id:`) and the compact JSON line the CLI
/// prints for it (SSE's `data:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub data: String,
}

/// Events in cursor order. A stream that ends in trouble returns its
/// diagnostics (`mdkg.stream.error`'s); a normal close is `Ok(None)`.
pub trait EventStream: Send {
    fn next_event(&mut self) -> Result<Option<Event>, Vec<Diagnostic>>;
}

/// Pushes a branch from a checkout to the clone's bound git destination,
/// and nowhere else (S38). Never forced; the receiving side's hooks decide.
/// A thin layer over g-core's `mdkg-git`, so git runs as mdkg always runs
/// it.
pub trait PushRunner: Send + Sync {
    /// Pushes `commit` to `branch` on the bound destination. A refusal by
    /// the receiving side comes back as `run.push_refused`.
    fn push(&self, checkout: &Path, branch: &str, commit: &str) -> Result<(), Box<Diagnostic>>;
}

/// Request and response header names (S4, S5, S41).
pub mod headers {
    pub const GRAPH: &str = "Mdkg-Graph";
    pub const BOOT: &str = "Mdkg-Boot";
    pub const ACTOR: &str = "Mdkg-Actor";
    pub const TURN: &str = "Mdkg-Turn";
    pub const PERSON: &str = "Mdkg-Person";
    pub const API: &str = "Mdkg-Api";
    pub const BUILD: &str = "Mdkg-Build";
    pub const REQUEST_ID: &str = "Mdkg-Request-Id";
    pub const IDEMPOTENCY_KEY: &str = "Idempotency-Key";
    pub const CONTENT_DIGEST: &str = "Content-Digest";
    pub const SIGNATURE_INPUT: &str = "Signature-Input";
    pub const SIGNATURE: &str = "Signature";
}

/// The signing profile's fixed values (S5, S41).
pub mod profile {
    /// The machine signature's RFC 9421 label.
    pub const LABEL: &str = "mdkg";
    /// The person signature's label. It covers the machine signature's
    /// `"signature";key="mdkg"` and `"signature-input";key="mdkg"`.
    pub const PERSON_LABEL: &str = "mdkg-person";
    /// `tag`, so another application's signature never verifies here.
    pub const TAG: &str = "mdkg-v1";
    /// The machine signature's `alg`.
    pub const ALG: &str = "ed25519";
    /// A Secure Enclave person key's `alg`; a file key uses [`ALG`].
    pub const ALG_P256: &str = "ecdsa-p256-sha256";
    /// The API version every request names in `Mdkg-Api`.
    pub const API: &str = "v1";
}
