//! Request authentication: S5's verification order, after the size limits.
//!
//! 0. browser headers (`Origin`, `Sec-Fetch-*`) unless allowed (S21);
//! 1. parse `Signature-Input` and `Signature`, refusing anything outside
//!    the profile (`api.unauthenticated`), and the write rules: a write
//!    without `Idempotency-Key` is `api.key_required` (R8);
//! 2. look up `keyid` (unknown, revoked, pending);
//! 3. `Mdkg-Graph` against this process's graph (`api.graph_unknown`),
//!    then `Mdkg-Boot` against the current boot id (`api.replayed`);
//! 4. `created` within 60 s (`api.clock_skew`);
//! 5. verify the machine signature, then a person signature if one came
//!    (S41);
//! 6. check and insert the nonce in one step (`api.replayed`);
//!    a pending key's nonces are kept apart, in their own bounded cache
//!    keyed by its fingerprint, so a self-signed enrolment can never
//!    reserve an approved machine's nonce (Terra 2 #6713);
//! 7. once the body is read, its digest ([`Gate::check_body`]).
//!
//! A request that fails any step never reaches the kernel. Only this
//! module makes an [`Authenticated`], so a remote origin is set only by
//! verification (Decided: Terra 1 #6537).

use std::sync::Arc;

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_ops::wire::{KeyDirectory, MachineState, profile};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::b64;
use crate::boot::{Boot, Clocks, Observed};
use crate::enroll::{Route, valid_id};
use crate::http::{HeadExt, RequestHead};
use crate::limits::RateLimiter;
use crate::nonce::{self, Check, NonceCache};
use crate::refusal::Refusal;
use crate::sf::{self, Entry, Item, Member};

/// The person signature's label (S41).
pub const PERSON_LABEL: &str = "mdkg-person";
/// The header naming the person key.
pub const PERSON_HEADER: &str = "mdkg-person";

/// Covered by every machine signature.
pub const ALWAYS: [&str; 5] = ["@method", "@path", "@query", "mdkg-graph", "mdkg-boot"];
/// Covered whenever the request carries them.
pub const IF_PRESENT: [&str; 4] = [
    "mdkg-actor",
    "content-digest",
    "idempotency-key",
    "mdkg-turn",
];

/// Headers that may appear at most once: everything signed, routed on or
/// naming a key. A repeat is refused, never combined.
const SINGLETONS: [&str; 10] = [
    "mdkg-graph",
    "mdkg-boot",
    "mdkg-actor",
    "content-digest",
    "idempotency-key",
    "mdkg-turn",
    "mdkg-person",
    "signature-input",
    "signature",
    "content-length",
];

/// Ed25519 verification, behind a trait until the crypto crate is
/// approved; `mdkg-wire` implements it. It must be strict (RFC 8032 with
/// ed25519-dalek's `verify_strict` rules): a small-order public key, such
/// as the all-zero one, or a non-canonical signature never verifies
/// (#6951).
pub trait Verifier: Send + Sync {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool;
}

/// A verified person key (S41).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonKey {
    pub key_id: String,
    /// The `human:*` actor the key belongs to.
    pub human: String,
    /// A file key: shown as `machine-grade` wherever it appears.
    pub machine_grade: bool,
}

/// What a person verifier found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonCheck {
    Verified(PersonKey),
    /// Unknown or revoked: `api.person_unknown`.
    Unknown,
    /// A bad signature, or presence flags missing.
    Invalid,
}

/// Person-signature verification (L16 owns the algorithms and the
/// presence flags).
pub trait PersonVerifier: Send + Sync {
    fn verify(&self, key_id: &str, alg: &str, base: &[u8], signature: &[u8]) -> PersonCheck;
}

/// A pending id's rate, charged before its nonce is used.
pub type Admit<'a> = dyn Fn(&str) -> Result<(), Refusal> + 'a;

/// Where the key that must have signed comes from.
#[derive(Clone, Copy)]
enum KeySource<'a> {
    /// An approved machine of this graph, from the key directory.
    Enrolled,
    /// An enrolment request, signed by its own pending key (S6).
    Pending {
        machine_id: &'a str,
        public_key: &'a [u8; 32],
        /// Charged in the nonce's critical section: a reveal's or poll's
        /// per-id rate (Terra 1 #6726, #7118).
        admit: Option<&'a Admit<'a>>,
        /// Run once the nonce is used (after the signature, the replay
        /// check and the insert), outside the nonce's lock: a first
        /// enrolment's known-id refusal, which reads the store (Terra 6
        /// #6939, orchestrator #8480, Terra 1 #8723). Authentication is
        /// complete at the insert; the hook rechecks no clock (#8728).
        pre: Option<&'a Admit<'a>>,
    },
}

/// Who a verified request came from: set only here (Decided, #6537).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// An approved machine of this graph.
    Remote {
        machine: String,
        person: Option<PersonKey>,
    },
    /// A pending enrolment's key: good only for its own enrolment
    /// endpoints, never for the kernel (Decided, #6673).
    Pending { machine: String },
}

// No `Local`: a serve listener never produces it. A request signed with
// the host's own machine key that arrives over serve, on the socket or
// TCP, is the host machine acting as an approved machine, `Remote`;
// host-class operations run only in-process, through the direct host CLI
// or MCP (orchestrator #8984, revising #8901 item 5; Terra 2 #8979).

/// A request from an approved machine whose head passed every check.
/// Fields are private: the only constructor is [`Gate::authenticate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    machine: String,
    /// The machine signature's nonce, already used once by the gate.
    nonce: String,
    person: Option<PersonKey>,
    actor: Option<String>,
    turn: Option<String>,
    idempotency_key: Option<String>,
    digest: Option<[u8; 32]>,
    write: bool,
    /// Set once [`Gate::check_body`] has matched the body to its signed
    /// digest; a connection is promoted only after that (#7678).
    body_checked: bool,
}

impl Authenticated {
    /// A request authenticated as `machine`, for other modules' tests.
    #[cfg(test)]
    pub(crate) fn for_tests(machine: &str) -> Self {
        Self {
            machine: machine.to_owned(),
            nonce: "n-test".to_owned(),
            person: None,
            actor: None,
            turn: None,
            idempotency_key: None,
            digest: None,
            write: false,
            body_checked: true,
        }
    }

    /// As [`Authenticated::for_tests`], with a verified person key.
    #[cfg(test)]
    pub(crate) fn for_tests_with_person(machine: &str, person: PersonKey) -> Self {
        Self {
            person: Some(person),
            ..Self::for_tests(machine)
        }
    }

    /// As [`Authenticated::for_tests`], before its body was checked.
    #[cfg(test)]
    pub(crate) fn for_tests_head_only(machine: &str) -> Self {
        Self {
            body_checked: false,
            ..Self::for_tests(machine)
        }
    }

    /// Whether the body matched its signed digest ([`Gate::check_body`]).
    pub fn body_checked(&self) -> bool {
        self.body_checked
    }

    pub fn origin(&self) -> Origin {
        Origin::Remote {
            machine: self.machine.clone(),
            person: self.person.clone(),
        }
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    pub fn person(&self) -> Option<&PersonKey> {
        self.person.as_ref()
    }

    pub fn actor(&self) -> Option<&str> {
        self.actor.as_deref()
    }

    pub fn turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    pub fn is_write(&self) -> bool {
        self.write
    }

    /// Who the request is from, for the kernel's remote context
    /// (`mdkg_ops::Ctx::remote`, which binds a person signature to the
    /// actor). `None` until [`Gate::check_body`] has matched the body.
    /// This is the one way from a request to a `Verified`: after the
    /// gate's whole S5 pass, the boot id, window and nonce included
    /// (#10448, orchestrator #10750).
    pub fn verified(&self) -> Option<mdkg_ops::wire::Verified> {
        self.body_checked.then(|| {
            mdkg_ops::wire::Verified::new(
                self.machine.clone(),
                self.nonce.clone(),
                self.person
                    .as_ref()
                    .map(|p| (p.key_id.clone(), p.human.clone())),
            )
        })
    }
}

/// A request signed by a pending enrolment's key (S6). It is a distinct
/// type from [`Authenticated`], so no enrolment can reach the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAuth {
    machine: String,
    digest: Option<[u8; 32]>,
}

impl PendingAuth {
    pub fn origin(&self) -> Origin {
        Origin::Pending {
            machine: self.machine.clone(),
        }
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }
}

/// The key a first `POST /enroll` carries in its body, which signed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollKey {
    pub machine_id: String,
    pub public_key: [u8; 32],
}

/// What [`Gate::verify_head`] found.
struct Verified {
    keyid: String,
    nonce: String,
    person: Option<PersonKey>,
    actor: Option<String>,
    digest: Option<[u8; 32]>,
}

/// Whether a request is a write at transport: every `POST` but a `:read`
/// (S4). The kernel still decides what each operation needs.
pub fn is_write(head: &RequestHead) -> bool {
    head.method == "POST" && !head.path.ends_with(":read")
}

/// The most pending keys' nonces held at once: those of enrolments,
/// reveals and polls. Enrolment is limited per source and per pending id,
/// so this is reached only by a flood, which then refuses every request
/// on the enrolment endpoints until nonces age out: a machine part-way
/// through enrolling, and an approved machine polling its own enrolment
/// to learn it was approved, which binds later (#6911, #7147). An approved
/// machine's requests to the API itself use the other cache and are never
/// refused by this one.
pub const PENDING_NONCES: usize = 1 << 14;

/// The authentication state of one `serve` process.
pub struct Gate {
    graph_id: String,
    boot: Boot,
    clocks: Arc<dyn Clocks>,
    nonces: NonceCache,
    /// Pending keys' nonces, by key fingerprint: never `nonces`.
    pending_nonces: NonceCache,
    keys: Arc<dyn KeyDirectory>,
    verifier: Arc<dyn Verifier>,
    persons: Arc<dyn PersonVerifier>,
    allow_origins: Vec<String>,
    /// Per-machine buckets, charged after the signature verifies and
    /// before the nonce is used, so a rate-limited flood can't fill the
    /// nonce cache (Terra 3 #6700). Required: no gate runs without one
    /// (Terra 4 #6786).
    rate: Arc<RateLimiter>,
    /// The host's own machine id and public key, once checked against
    /// the store's row ([`Gate::with_host`]). It grants nothing: over
    /// serve, the host key's requests are an approved machine's (#8984).
    host: Option<(String, [u8; 32])>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("graph_id", &self.graph_id)
            .field("nonces", &self.nonces.len())
            .field("pending_nonces", &self.pending_nonces.len())
            .finish_non_exhaustive()
    }
}

/// The parsed machine signature.
struct Parsed {
    components: Vec<String>,
    params_member: Member,
    sig_member: Member,
    signature: [u8; 64],
    created: i64,
    keyid: String,
    nonce: String,
    person: Option<ParsedPerson>,
}

struct ParsedPerson {
    member: Member,
    key_id: String,
    alg: String,
    signature: Vec<u8>,
}

fn string_param(params: &sf::Params, name: &str) -> Option<String> {
    match sf::param(params, name)? {
        Item::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn only_params(params: &sf::Params, allowed: &[&str]) -> bool {
    params.iter().all(|(k, _)| allowed.contains(&k.as_str()))
        && allowed.iter().all(|k| sf::param(params, k).is_some())
}

fn bytes_member(m: &Member) -> Option<&[u8]> {
    match m {
        Member::Item(Entry {
            item: Item::Bytes(b),
            params,
        }) if params.is_empty() => Some(b),
        _ => None,
    }
}

fn member<'a>(dict: &'a sf::Dictionary, label: &str) -> Option<&'a Member> {
    dict.iter().find(|(k, _)| k == label).map(|(_, m)| m)
}

impl Gate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        graph_id: String,
        boot: Boot,
        clocks: Arc<dyn Clocks>,
        nonces: NonceCache,
        keys: Arc<dyn KeyDirectory>,
        verifier: Arc<dyn Verifier>,
        persons: Arc<dyn PersonVerifier>,
        allow_origins: Vec<String>,
        rate: Arc<RateLimiter>,
    ) -> Self {
        Self {
            graph_id,
            boot,
            clocks,
            nonces,
            pending_nonces: NonceCache::new(PENDING_NONCES),
            keys,
            verifier,
            persons,
            allow_origins,
            rate,
            host: None,
        }
    }

    /// Names the host's own machine identity, after checking it against
    /// the key directory: a start whose host-id row holds another key, or
    /// a key not approved, is refused, so the host's own requests are
    /// verified against the key it holds (Terra 3 #8892, orchestrator
    /// #8901 item 5, kept by #8984).
    pub fn with_host(mut self, machine: &str, public_key: [u8; 32]) -> Result<Self, String> {
        match self.keys.machine(machine) {
            Some(k) if k.public_key == public_key && k.state == MachineState::Approved => {}
            Some(_) => {
                return Err(format!(
                    "the store's row for host machine {machine} holds another key, or isn't approved"
                ));
            }
            None => return Err(format!("host machine {machine} has no row in the store")),
        }
        self.host = Some((machine.to_owned(), public_key));
        Ok(self)
    }

    /// The host's own machine id, once [`Gate::with_host`] checked it.
    pub fn host(&self) -> Option<&str> {
        self.host.as_ref().map(|(id, _)| id.as_str())
    }

    /// Nonces held in both caches.
    #[cfg(test)]
    pub(crate) fn nonces_held(&self) -> usize {
        self.nonces.len() + self.pending_nonces.len()
    }

    /// Replaces the pending keys' nonce cache (tests use a small one).
    pub fn with_pending_nonces(mut self, cache: NonceCache) -> Self {
        self.pending_nonces = cache;
        self
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn clocks(&self) -> &dyn Clocks {
        &*self.clocks
    }

    /// The current boot id, as hex; checks the clocks first (S7).
    pub fn boot_id(&self) -> Result<String, Refusal> {
        self.boot
            .current(&*self.clocks)
            .map(|b| b.as_hex())
            .map_err(|_| Refusal::new(Code::ApiCheckFailed, ErrorKind::Failure, "boot_rotation"))
    }

    /// The 60 s timer's nonce pruning, at one observation of the clocks
    /// that also moves the boot id's rotation mark (Terra 6 #6612).
    pub fn prune_nonces(&self) -> Result<usize, Refusal> {
        let observe = || self.boot.observe(&*self.clocks);
        let failed = |_| Refusal::new(Code::ApiCheckFailed, ErrorKind::Failure, "boot_rotation");
        Ok(self.nonces.prune(&observe).map_err(failed)?
            + self.pending_nonces.prune(&observe).map_err(failed)?)
    }

    /// The wall clock for store timestamps (expiry, `requested_ms`), read
    /// through an observation like every other wall-clock reading.
    pub fn now_ms(&self) -> u64 {
        self.observe()
            .map(|o| o.wall_ms)
            .unwrap_or_else(|_| self.clocks.wall_ms())
    }

    /// Monotonic time, for buckets and timers.
    pub fn observe_mono(&self) -> std::time::Duration {
        self.clocks.mono()
    }

    fn observe(&self) -> Result<Observed, Refusal> {
        self.boot
            .observe(&*self.clocks)
            .map_err(|_| Refusal::new(Code::ApiCheckFailed, ErrorKind::Failure, "boot_rotation"))
    }

    /// Step 0: no browser surface (S21, R44).
    pub fn check_origin(&self, head: &RequestHead) -> Result<(), Refusal> {
        let refused = || Refusal::new(Code::ApiOriginRefused, ErrorKind::Usage, "origin");
        let origins: Vec<&str> = head.all("origin").collect();
        let fetch = head
            .headers
            .iter()
            .any(|(n, _)| n.starts_with("sec-fetch-"));
        if origins.len() > 1 {
            return Err(refused());
        }
        match origins.first() {
            Some(o) if self.allow_origins.iter().any(|a| a == o) => Ok(()),
            Some(_) => Err(refused()),
            None if fetch => Err(refused()),
            None => Ok(()),
        }
    }

    /// Steps 0 to 6 for an approved machine's request, on the head alone.
    /// The nonce is used once this returns `Ok`.
    pub fn authenticate(&self, head: &RequestHead) -> Result<Authenticated, Refusal> {
        let write = is_write(head);
        let v = self.verify_head(head, KeySource::Enrolled, write)?;
        Ok(Authenticated {
            machine: v.keyid,
            nonce: v.nonce,
            person: v.person,
            actor: v.actor,
            turn: head.one("mdkg-turn").ok().flatten().map(str::to_owned),
            idempotency_key: head
                .one("idempotency-key")
                .ok()
                .flatten()
                .map(str::to_owned),
            digest: v.digest,
            write,
            body_checked: false,
        })
    }

    /// A request signed by a pending enrolment's key: the reveal and the
    /// poll (S6). `public_key` is the pending row's. The write rules don't
    /// apply, and a person signature is refused. The request's method and
    /// path must be `machine_id`'s own reveal or poll, so a pending key
    /// authenticates nothing addressed to another id (Terra 2 #6713).
    /// `admit` (the id's rate) runs after the signature verified and
    /// before the nonce is used, so a request over its rate takes no place
    /// in the cache (Terra 1 #6726).
    pub fn authenticate_pending(
        &self,
        head: &RequestHead,
        machine_id: &str,
        public_key: &[u8; 32],
        admit: &Admit<'_>,
    ) -> Result<PendingAuth, Refusal> {
        match Route::of(head) {
            Some(Route::Reveal(id) | Route::Poll(id)) if id == machine_id => {}
            _ => return Err(Refusal::unauthenticated("pending_route")),
        }
        let v = self.verify_head(
            head,
            KeySource::Pending {
                machine_id,
                public_key,
                admit: Some(admit),
                pre: None,
            },
            false,
        )?;
        Ok(PendingAuth {
            machine: v.keyid,
            digest: v.digest,
        })
    }

    /// The first `POST /enroll`, the one documented exception to S5's
    /// order (Decided, #6673): its key is in its body, so the body (at
    /// most the enrolment cap, already read) comes first. It must match
    /// its signed digest; then the request must verify under the key it
    /// carries, as proof of possession, before the nonce is used and
    /// before anything is recorded.
    ///
    /// A known machine id is refused (`api.enroll_exists`) after the
    /// origin, graph, boot, window and signature checks and after the
    /// nonce is used, so a request for another graph or from a refused
    /// origin learns nothing of this graph's ids, and a replay or a
    /// concurrent copy is refused as a replay before any lookup (#6725,
    /// orchestrator #8197, #8480). The key directory is checked first;
    /// then `known`, the caller's look at the store itself, which is
    /// authoritative when the directory lags it (Terra 6 #6939).
    pub fn authenticate_enroll(
        &self,
        head: &RequestHead,
        body: &[u8],
        known: &dyn Fn(&EnrollKey) -> Result<(), Refusal>,
    ) -> Result<(PendingAuth, EnrollKey), Refusal> {
        if Route::of(head) != Some(Route::Request) {
            return Err(Refusal::unauthenticated("pending_route"));
        }
        let digest = head
            .one("content-digest")
            .ok()
            .flatten()
            .and_then(parse_digest)
            .ok_or_else(|| Refusal::unauthenticated("digest_missing"))?;
        if Sha256::digest(body).as_slice() != digest.as_slice() {
            return Err(Refusal::unauthenticated("digest_mismatch"));
        }
        let key = enroll_key(body).ok_or_else(|| Refusal::unauthenticated("enroll_key"))?;
        // An id the enrolment routes can't carry could never be revealed
        // or polled, yet would hold a pending slot (#6717).
        if !valid_id(&key.machine_id) {
            return Err(Refusal::new(
                Code::UsageInvalidArguments,
                ErrorKind::Usage,
                "machine_id",
            ));
        }
        let unknown = |id: &str| {
            // The signed body's graph first: a request for another graph
            // learns nothing of this one's ids (Terra 2 #7204).
            let body_graph = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("graph_id")?.as_str().map(str::to_owned));
            if body_graph.as_deref() != Some(self.graph_id.as_str()) {
                return Err(Refusal::new(
                    Code::ApiGraphUnknown,
                    ErrorKind::NotFound,
                    "graph_unknown",
                ));
            }
            match self.keys.machine(id) {
                // A pending entry, whatever its key, goes to the store,
                // which is authoritative: the entry may be stale, its row
                // expired or rejected, and the store tells the key that
                // signed alone it is pending (Terra 3 #7835, #8179,
                // orchestrator #7853).
                Some(k) if k.state == MachineState::Pending => known(&key),
                // An enrolled or revoked machine: any key learns only that
                // the id exists.
                Some(_) => Err(Refusal::new(
                    Code::ApiEnrollExists,
                    ErrorKind::Conflict,
                    "exists",
                )),
                None => known(&key),
            }
        };
        let v = self.verify_head(
            head,
            KeySource::Pending {
                machine_id: &key.machine_id,
                public_key: &key.public_key,
                admit: None,
                pre: Some(&unknown),
            },
            false,
        )?;
        Ok((
            PendingAuth {
                machine: v.keyid,
                digest: v.digest,
            },
            key,
        ))
    }

    /// A reveal's or poll's form, checked before the store is read: its
    /// origin, its route, and the signature and digest headers' syntax. A
    /// request that fails here never reaches the lookup, so it can't time
    /// it (orchestrator #7278, Terra 3 #7247, Terra 1 #8339).
    pub fn precheck_pending(&self, head: &RequestHead) -> Result<(), Refusal> {
        self.check_origin(head)?;
        if !matches!(Route::of(head), Some(Route::Reveal(_) | Route::Poll(_))) {
            return Err(Refusal::unauthenticated("pending_route"));
        }
        let parsed = parse(head, false)?;
        if parsed.person.is_some() {
            return Err(Refusal::unauthenticated("profile"));
        }
        // The digest's syntax too: a malformed one never reaches the
        // lookup (Terra 1 #8339).
        // Repeated, it is refused too, never read as absent (Opus #8547).
        match head.one("content-digest") {
            Ok(None) => {}
            Ok(Some(v)) if parse_digest(v).is_some() => {}
            _ => return Err(Refusal::unauthenticated("bad_digest")),
        }
        Ok(())
    }

    /// Steps 0 to 6.
    fn verify_head(
        &self,
        head: &RequestHead,
        source: KeySource<'_>,
        write: bool,
    ) -> Result<Verified, Refusal> {
        self.check_origin(head)?;
        let parsed = parse(head, write)?;
        if matches!(source, KeySource::Pending { .. }) && parsed.person.is_some() {
            return Err(Refusal::unauthenticated("profile"));
        }

        // 2. The key.
        let public_key = match source {
            KeySource::Enrolled => match self.keys.machine(&parsed.keyid) {
                None => return Err(Refusal::unauthenticated("unknown_key")),
                Some(k) => match k.state {
                    MachineState::Approved => k.public_key,
                    MachineState::Revoked => {
                        return Err(Refusal::new(
                            Code::ApiMachineRevoked,
                            ErrorKind::Conflict,
                            "machine_revoked",
                        ));
                    }
                    MachineState::Pending => {
                        return Err(Refusal::new(
                            Code::ApiMachinePending,
                            ErrorKind::Conflict,
                            "machine_pending",
                        ));
                    }
                },
            },
            KeySource::Pending {
                machine_id,
                public_key,
                ..
            } => {
                if parsed.keyid != machine_id {
                    return Err(Refusal::unauthenticated("keyid_mismatch"));
                }
                *public_key
            }
        };

        // 3. The graph, then the boot.
        let graph = head.one("mdkg-graph").ok().flatten().unwrap_or_default();
        if graph != self.graph_id {
            return Err(Refusal::new(
                Code::ApiGraphUnknown,
                ErrorKind::NotFound,
                "graph_unknown",
            ));
        }
        // One observation judges the boot, the window and the nonce.
        let at = self.observe()?;
        let boot = at.boot.as_hex();
        if head.one("mdkg-boot").ok().flatten() != Some(boot.as_str()) {
            return Err(
                Refusal::new(Code::ApiReplayed, ErrorKind::Conflict, "boot_changed")
                    .with("boot_id", Value::String(boot)),
            );
        }

        // 4. The window.
        let now_s = (at.wall_ms / 1000) as i64;
        if (parsed.created - now_s).abs() > nonce::WINDOW_S {
            return Err(
                Refusal::new(Code::ApiClockSkew, ErrorKind::Usage, "clock_skew")
                    .with("server_time", Value::from(now_s)),
            );
        }

        // 5. The machine signature, then the person's.
        let base = machine_base(head, &parsed.components, &parsed.params_member);
        if !self
            .verifier
            .verify(&public_key, base.as_bytes(), &parsed.signature)
        {
            return Err(Refusal::unauthenticated("bad_signature"));
        }
        let actor = head.one("mdkg-actor").ok().flatten().map(str::to_owned);
        let person = match &parsed.person {
            None => None,
            Some(p) => {
                let base = person_base(&parsed, &p.member);
                match self
                    .persons
                    .verify(&p.key_id, &p.alg, base.as_bytes(), &p.signature)
                {
                    // Unknown or revoked: S5's layer-1 code, with the
                    // enrolment-state kind api.machine_revoked has.
                    PersonCheck::Unknown => {
                        return Err(Refusal::new(
                            Code::ApiPersonUnknown,
                            ErrorKind::Conflict,
                            "person_unknown",
                        ));
                    }
                    PersonCheck::Invalid => {
                        return Err(Refusal::unauthenticated("bad_person_signature"));
                    }
                    PersonCheck::Verified(key) => {
                        if actor.as_deref() != Some(key.human.as_str()) {
                            return Err(Refusal::new(
                                Code::ApiActorNotAllowed,
                                ErrorKind::Conflict,
                                "person_actor_mismatch",
                            ));
                        }
                        Some(key)
                    }
                }
            }
        };

        // The digest's form, and the machine's rate, before the nonce is
        // used: neither a malformed digest nor a request over its rate
        // takes a place in the cache (Terra 3 #6700).
        let digest = match head.one("content-digest").ok().flatten() {
            None => None,
            Some(v) => Some(parse_digest(v).ok_or_else(|| Refusal::unauthenticated("bad_digest"))?),
        };
        // A pending key's nonce goes in its own cache, under the key it
        // proved, never the id it claims (Terra 2 #6713).
        let (cache, scope) = match source {
            KeySource::Enrolled => (&self.nonces, parsed.keyid.clone()),
            KeySource::Pending { .. } => (
                &self.pending_nonces,
                mdkg_wire::keys::fingerprint(&public_key),
            ),
        };
        let replayed = || {
            Refusal::new(Code::ApiReplayed, ErrorKind::Conflict, "nonce_reused")
                .with("boot_id", Value::String(boot.clone()))
        };
        // The rate is charged inside the nonce's critical section, after
        // the replay check and before the insert: of concurrent copies of
        // one request only the first is charged, a replay never is, and a
        // request over its rate takes no nonce (#6700, #6786, #7118).
        let charge = || match source {
            KeySource::Enrolled => self.rate.admit(&parsed.keyid, write, self.clocks.mono()),
            KeySource::Pending {
                admit: Some(admit), ..
            } => admit(&parsed.keyid),
            KeySource::Pending { admit: None, .. } => Ok(()),
        };
        // 6. The nonce, only now that every signature verified.
        // The boot and the window are checked again under the nonce lock,
        // against a fresh reading (Terra 1 #6697).
        match cache.check_charge_insert(
            &scope,
            &parsed.nonce,
            parsed.created,
            at.boot,
            &|| self.boot.observe(&*self.clocks),
            &charge,
        )? {
            Check::Fresh => {}
            Check::BootChanged(now) => {
                return Err(
                    Refusal::new(Code::ApiReplayed, ErrorKind::Conflict, "boot_changed")
                        .with("boot_id", Value::String(now.as_hex())),
                );
            }
            Check::Skew(now_s) => {
                return Err(
                    Refusal::new(Code::ApiClockSkew, ErrorKind::Usage, "clock_skew")
                        .with("server_time", Value::from(now_s)),
                );
            }
            Check::Unobserved => {
                return Err(Refusal::new(
                    Code::ApiCheckFailed,
                    ErrorKind::Failure,
                    "boot_rotation",
                ));
            }
            Check::Replayed => return Err(replayed()),
            Check::Full => {
                return Err(Refusal::new(
                    Code::ApiRateLimited,
                    ErrorKind::Conflict,
                    "nonce_cache_full",
                )
                .retry_after(1));
            }
        }
        // A first enrolment's known-id check reads the store, so it runs
        // outside the nonce lock, and only once the nonce is used: of
        // concurrent copies only the first gets here, the rest are
        // replays, and a request the check refuses keeps its nonce used,
        // so a relay holding it learns nothing by sending it again (Terra
        // 3 #8179, Terra 7 #8463, orchestrator #8197, #8480).
        if let KeySource::Pending { pre: Some(pre), .. } = source {
            pre(&parsed.keyid)?;
        }

        Ok(Verified {
            keyid: parsed.keyid,
            nonce: parsed.nonce,
            person,
            actor,
            digest,
        })
    }

    /// Step 7: the body's digest. A failure leaves the nonce used, which
    /// costs the sender one retry (S5).
    pub fn check_body(&self, auth: &mut Authenticated, body: &[u8]) -> Result<(), Refusal> {
        digest_ok(&auth.digest, body)?;
        auth.body_checked = true;
        Ok(())
    }

    /// Step 7 for a reveal's body.
    pub fn check_pending_body(&self, auth: &PendingAuth, body: &[u8]) -> Result<(), Refusal> {
        digest_ok(&auth.digest, body)
    }
}

/// The enrolment body's `machine_id` and `public_key` (base64url, 32
/// bytes). The enrolment module reads the rest.
fn enroll_key(body: &[u8]) -> Option<EnrollKey> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let machine_id = v.get("machine_id")?.as_str()?.to_owned();
    let public_key = b64::decode_url(v.get("public_key")?.as_str()?)?
        .try_into()
        .ok()?;
    Some(EnrollKey {
        machine_id,
        public_key,
    })
}

fn digest_ok(digest: &Option<[u8; 32]>, body: &[u8]) -> Result<(), Refusal> {
    match digest {
        Some(d) if Sha256::digest(body).as_slice() == d.as_slice() => Ok(()),
        Some(_) => Err(Refusal::unauthenticated("digest_mismatch")),
        None if body.is_empty() => Ok(()),
        None => Err(Refusal::unauthenticated("digest_missing")),
    }
}

/// `Content-Digest: sha-256=:…:`, exactly one member, 32 bytes.
fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let dict = sf::parse_dictionary(value)?;
    if dict.len() != 1 || dict[0].0 != "sha-256" {
        return None;
    }
    bytes_member(&dict[0].1)?.try_into().ok()
}

/// Step 1: the profile and the write rules.
fn parse(head: &RequestHead, write: bool) -> Result<Parsed, Refusal> {
    for name in SINGLETONS {
        if head.one(name).is_err() {
            return Err(Refusal::unauthenticated("header_repeated"));
        }
    }
    let input = head
        .one("signature-input")
        .ok()
        .flatten()
        .ok_or_else(|| Refusal::unauthenticated("unsigned"))?;
    let sig = head
        .one("signature")
        .ok()
        .flatten()
        .ok_or_else(|| Refusal::unauthenticated("unsigned"))?;
    let profile_err = || Refusal::unauthenticated("profile");
    let inputs = sf::parse_dictionary(input).ok_or_else(profile_err)?;
    let sigs = sf::parse_dictionary(sig).ok_or_else(profile_err)?;
    let labels_ok = |d: &sf::Dictionary| {
        d.iter()
            .all(|(k, _)| k == profile::LABEL || k == PERSON_LABEL)
    };
    if !labels_ok(&inputs) || !labels_ok(&sigs) || inputs.len() != sigs.len() {
        return Err(profile_err());
    }
    let params_member = member(&inputs, profile::LABEL)
        .ok_or_else(profile_err)?
        .clone();
    let sig_member = member(&sigs, profile::LABEL)
        .ok_or_else(profile_err)?
        .clone();

    let Member::InnerList(list, params) = &params_member else {
        return Err(profile_err());
    };
    let mut components = Vec::with_capacity(list.len());
    for e in list {
        match e {
            Entry {
                item: Item::String(c),
                params,
            } if params.is_empty()
                && (ALWAYS.contains(&c.as_str()) || IF_PRESENT.contains(&c.as_str()))
                && !components.contains(c) =>
            {
                components.push(c.clone())
            }
            _ => return Err(profile_err()),
        }
    }
    if !only_params(params, &["created", "keyid", "nonce", "alg", "tag"]) {
        return Err(profile_err());
    }
    let created = match sf::param(params, "created") {
        Some(Item::Integer(n)) => *n,
        _ => return Err(profile_err()),
    };
    let keyid = string_param(params, "keyid").ok_or_else(profile_err)?;
    let nonce = string_param(params, "nonce").ok_or_else(profile_err)?;
    if string_param(params, "alg").as_deref() != Some(profile::ALG)
        || string_param(params, "tag").as_deref() != Some(profile::TAG)
        || keyid.is_empty()
        || b64::decode_url(&nonce).map(|n| n.len()) != Some(16)
    {
        return Err(profile_err());
    }
    let signature: [u8; 64] = bytes_member(&sig_member)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(profile_err)?;

    // What must be covered.
    for c in ALWAYS {
        if !components.iter().any(|x| x == c) {
            return Err(Refusal::unauthenticated("not_covered"));
        }
    }
    for c in IF_PRESENT {
        let covered = components.iter().any(|x| x == c);
        if head.has(c) != covered {
            return Err(Refusal::unauthenticated(if covered {
                "component_missing"
            } else {
                "not_covered"
            }));
        }
    }
    if !head.has("mdkg-graph") || !head.has("mdkg-boot") {
        return Err(Refusal::unauthenticated("component_missing"));
    }
    if write {
        if !head.has("idempotency-key") {
            return Err(Refusal::new(
                Code::ApiKeyRequired,
                ErrorKind::Usage,
                "key_required",
            ));
        }
        if !head.has("mdkg-actor") {
            return Err(Refusal::unauthenticated("actor_missing"));
        }
    }
    // Limits::check_head refused a bad length before this; a request that
    // skipped it is treated as carrying a body.
    let has_body = !matches!(head.body_length(), Ok(None | Some(0)));
    if has_body && !head.has("content-digest") {
        return Err(Refusal::unauthenticated("digest_missing"));
    }

    // The person signature, when one came.
    let person_header = head.one(PERSON_HEADER).ok().flatten();
    let person = match (member(&inputs, PERSON_LABEL), member(&sigs, PERSON_LABEL)) {
        (None, None) => {
            if person_header.is_some() {
                return Err(profile_err());
            }
            None
        }
        (Some(m), Some(s)) => {
            let Member::InnerList(list, params) = m else {
                return Err(profile_err());
            };
            let expect = |name: &str| Entry {
                item: Item::String(name.into()),
                params: vec![("key".into(), Item::String(profile::LABEL.into()))],
            };
            if list.as_slice() != [expect("signature"), expect("signature-input")]
                || !only_params(params, &["created", "keyid", "alg", "tag"])
                || string_param(params, "tag").as_deref() != Some(profile::TAG)
            {
                return Err(profile_err());
            }
            let key_id = string_param(params, "keyid").ok_or_else(profile_err)?;
            if person_header != Some(key_id.as_str()) {
                return Err(profile_err());
            }
            Some(ParsedPerson {
                member: m.clone(),
                key_id,
                alg: string_param(params, "alg").ok_or_else(profile_err)?,
                signature: bytes_member(s).ok_or_else(profile_err)?.to_vec(),
            })
        }
        _ => return Err(profile_err()),
    };

    Ok(Parsed {
        components,
        params_member,
        sig_member,
        signature,
        created,
        keyid,
        nonce,
        person,
    })
}

/// RFC 9421's signature base for the machine signature.
pub fn machine_base(head: &RequestHead, components: &[String], params: &Member) -> String {
    let mut out = String::new();
    for c in components {
        let value = match c.as_str() {
            "@method" => head.method.clone(),
            "@path" if head.path.is_empty() => "/".to_owned(),
            "@path" => head.path.clone(),
            "@query" => format!("?{}", head.query.as_deref().unwrap_or("")),
            name => head.one(name).ok().flatten().unwrap_or_default().to_owned(),
        };
        out.push_str(&format!("\"{c}\": {value}\n"));
    }
    out.push_str(&format!(
        "\"@signature-params\": {}",
        sf::serialize_member(params)
    ));
    out
}

/// The person signature's base: the machine signature's two members, then
/// its own parameters.
fn person_base(parsed: &Parsed, person: &Member) -> String {
    format!(
        "\"signature\";key=\"{l}\": {}\n\"signature-input\";key=\"{l}\": {}\n\"@signature-params\": {}",
        sf::serialize_member(&parsed.sig_member),
        sf::serialize_member(&parsed.params_member),
        sf::serialize_member(person),
        l = profile::LABEL,
    )
}

#[cfg(test)]
pub(crate) mod tests;
