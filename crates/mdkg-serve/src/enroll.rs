//! The enrolment endpoints (S6): `POST /enroll`, `POST /enroll/{id}:reveal`
//! and `GET /enroll/{id}`, served on the socket and on TCP (R94).
//!
//! A pending machine is granted nothing: every request here is
//! authenticated as [`crate::auth::PendingAuth`], which never reaches the
//! kernel. Approval is never served over HTTP (R16); it is the host's
//! `machine approve`, a compare-and-set on the pending row this module
//! records.
//!
//! The graph's store holds the rows, behind [`EnrollStore`] (L3's
//! `serve.rs` implements it); its operations are atomic there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use serde_json::{Value, json};

use crate::auth::{EnrollKey, Gate};
use crate::b64;
use crate::http::{RequestHead, Response};
use crate::limits::{Bucket, EnrollLimiter, Limits, Peer};
use crate::random;
use crate::refusal::Refusal;

/// A pending request lives this long (S6).
pub const PENDING_MS: u64 = 15 * 60 * 1000;
/// At most this many requests pending per `serve` (S6, S21).
pub const MAX_PENDING: usize = 16;
/// Reveals and polls a minute from one source address (Terra 1 #6726).
pub const LOOKUPS_PER_MINUTE: u32 = 60;
pub const ENROLL_SCHEMA: &str = "mdkg.serve.enroll.v1";
/// The refusals of a reveal or poll that never cost its source: a
/// verified key over its own rate or on a full nonce cache, and the
/// server's own failure to observe its clocks (orchestrator #8197).
const VERIFIED_OR_OURS: [&str; 3] = ["enroll_poll", "nonce_cache_full", "boot_rotation"];

/// Machine names: what `launcher:<name>` and placement rules can carry.
/// One to 64 of lowercase letters, digits, `-`, `_` and `.`, starting
/// with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.".contains(c))
}

/// Machine ids as the enrolment routes carry them: 1 to 64 ASCII letters,
/// digits and `-` (UUIDs), so a path segment needs no decoding. A first
/// enrolment for any other id is refused, since it could never be revealed
/// or polled (#6717).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// Stands in for the key of an id the store doesn't know, so that a
/// request for it is verified, and refused, exactly as one signed by the
/// wrong key: an unauthenticated caller can't tell the two apart (#6717).
///
/// It is a valid Ed25519 public key of full (prime) order, so a strict
/// verifier runs the whole verification against it, as against a stored
/// key, instead of refusing it early as weak (Terra 1 #6982). It is
/// SHA-256 of `mdkg-serve no-key stand-in 19`, the first of that counter
/// whose bytes decode to a point of the prime-order subgroup, so nobody
/// chose it and nobody holds its private key; `tools/check_no_key.py`
/// re-derives it and checks it is canonical and of prime order, with the
/// curve's own arithmetic (Opus #7228). Should a signature ever
/// verify against it, the request is still refused at the pre-nonce
/// hook, so it takes no nonce (#6951).
const NO_KEY: [u8; 32] = [
    0x36, 0xc9, 0xfa, 0x2a, 0x56, 0x5d, 0xd6, 0x98, 0x41, 0x02, 0xed, 0x6b, 0xe8, 0x3e, 0xd2, 0x74,
    0x9a, 0x82, 0x56, 0x1d, 0x04, 0xc5, 0x06, 0x96, 0x5a, 0x69, 0x4e, 0x81, 0x87, 0x91, 0x9a, 0x49,
];

/// A new pending request, minted by `serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPending {
    pub pending_id: String,
    pub machine_id: String,
    pub public_key: [u8; 32],
    pub fingerprint: String,
    pub name: String,
    pub commitment: [u8; 32],
    pub ns: [u8; 32],
    pub requested_ms: u64,
    pub expires_ms: u64,
    /// Where it came from: `unix`, or the TCP peer's address, for
    /// `machine list --pending` to show who is squatting from where (S6).
    pub source: String,
}

/// Why the store refused a pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRefused {
    /// A `machines` row exists for the id, in any state (R78).
    Exists,
    /// An unexpired request for the id exists (R52), holding this key:
    /// read in the transaction that refused, so the caller tells only
    /// that key it is pending (Terra 3 #8179).
    Pending([u8; 32]),
    /// [`MAX_PENDING`] requests are pending.
    Full,
}

/// An unexpired pending request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRow {
    pub pending_id: String,
    pub public_key: [u8; 32],
    pub commitment: [u8; 32],
    pub nc: Option<[u8; 32]>,
    /// The `Ns` serve minted, which a signed poll returns for the worker
    /// to check against the one it pinned. A worker with no pin never
    /// takes it from a poll; it starts over (orchestrator #8343).
    pub ns: [u8; 32],
    pub expires_ms: u64,
}

/// What the graph's store knows of a machine id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enrolment {
    Pending(PendingRow),
    /// A `machines` row, with its key and state (`approved`, `revoking`,
    /// `revoked`).
    Machine {
        public_key: [u8; 32],
        state: String,
    },
}

/// The store failed; the request may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFailed {
    /// The write lock wasn't free within `serve.busy_timeout_ms`: HTTP
    /// 503, which clients retry (Terra 1 #6726).
    Busy,
    /// Anything else.
    Failed,
}

/// The graph's enrolment rows (L3). Each method is one transaction.
pub trait EnrollStore: Send + Sync {
    /// Records `p` unless a `machines` row exists for its id, an unexpired
    /// request does, or `max` are pending; expired requests are dropped
    /// first.
    fn request(
        &self,
        p: &NewPending,
        max: usize,
        now_ms: u64,
    ) -> Result<Result<(), PendingRefused>, StoreFailed>;

    /// The machine's `machines` row, else its unexpired pending request.
    fn lookup(&self, machine_id: &str, now_ms: u64) -> Result<Option<Enrolment>, StoreFailed>;

    /// Sets `nc` on the pending request `pending_id` if it is still
    /// pending and has none; true when it now holds `nc`.
    fn reveal(&self, pending_id: &str, nc: &[u8; 32], now_ms: u64) -> Result<bool, StoreFailed>;
}

/// An ephemeral event to fan out (`durable: false`).
#[derive(Debug, Clone, PartialEq)]
pub struct Ephemeral {
    pub kind: &'static str,
    pub data: Value,
}

/// What an enrolment request produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Handled {
    pub response: Response,
    pub event: Option<Ephemeral>,
}

/// The enrolment endpoints of one `serve`.
pub struct Enroller {
    gate: Arc<Gate>,
    store: Arc<dyn EnrollStore>,
    limiter: EnrollLimiter,
    /// Reveals and polls per source, before the store is read.
    lookups: EnrollLimiter,
    /// Reveals and polls: one a second per pending id, after
    /// authentication, apart from the five-a-minute limit on new requests
    /// (Decided, #6704). A worker polls every 2 s for up to 15 minutes.
    pending_rate: Mutex<HashMap<String, Bucket>>,
    limits: Limits,
}

impl std::fmt::Debug for Enroller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enroller").finish_non_exhaustive()
    }
}

fn ok(data: Value) -> Response {
    let body = json!({"schema": ENROLL_SCHEMA, "ok": true, "data": data, "diagnostics": []});
    Response {
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

fn store_failed(e: StoreFailed) -> Refusal {
    match e {
        StoreFailed::Busy => Refusal::new(Code::CoordStoreFailed, ErrorKind::Failure, "store_busy")
            .with_status(503)
            .retry_after(1),
        StoreFailed::Failed => Refusal::new(Code::CoordStoreFailed, ErrorKind::Failure, "store"),
    }
}

/// The system's randomness failed: not the store's fault.
fn no_random() -> Refusal {
    Refusal::new(Code::CoordStoreFailed, ErrorKind::Failure, "random")
}

fn changed(reason: &'static str) -> Refusal {
    Refusal::new(Code::ApiEnrollChanged, ErrorKind::Conflict, reason)
}

fn bytes32(v: &Value, key: &str) -> Option<[u8; 32]> {
    b64::decode_url(v.get(key)?.as_str()?)?.try_into().ok()
}

/// Which enrolment endpoint a request is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Request,
    Reveal(String),
    Poll(String),
}

impl Route {
    /// The enrolment route of `head`, if it is one: the router's own
    /// match, so an id sent percent-encoded is decoded once, as every path
    /// capture is (S4, Terra 4 #9403).
    pub fn of(head: &RequestHead) -> Option<Route> {
        crate::router::enroll_route(head)
    }
}

impl Enroller {
    pub fn new(gate: Arc<Gate>, store: Arc<dyn EnrollStore>, limits: Limits) -> Self {
        Self {
            gate,
            store,
            limiter: EnrollLimiter::default(),
            lookups: EnrollLimiter::new(LOOKUPS_PER_MINUTE, "enroll_lookup"),
            pending_rate: Mutex::new(HashMap::new()),
            limits,
        }
    }

    /// Refusals before the body is read: the 4 KiB cap, and the source
    /// address's rate: five new requests a minute; and for reveals and
    /// polls, which look the store up before any signature can be checked,
    /// sixty a minute that fail to authenticate, whatever the id (Terra 1
    /// #6726, orchestrator #7392). Here a source whose window is spent is
    /// refused before the store; a request is charged only once it fails
    /// to authenticate, so a verified pending or enrolled key's reveals
    /// and polls cost only its own bucket, and workers behind one address
    /// don't share a budget (orchestrator #7056). The listener calls this,
    /// then reads at most that cap, then [`Enroller::handle`].
    pub fn admit(&self, route: &Route, head: &RequestHead, peer: &Peer) -> Result<(), Refusal> {
        self.limits
            .check_head(head, Some(self.limits.enroll_body_bytes))?;
        let now = self.gate.observe_mono();
        match route {
            Route::Request => self.limiter.admit(peer, now),
            Route::Reveal(_) | Route::Poll(_) => self.lookups.peek(peer, now),
        }
    }

    /// A refusal of a reveal or a poll the listener makes before reading
    /// its body (its media type, say): charged to the source's window for
    /// failed lookups as any failed reveal or poll is, since `admit` only
    /// looked at that window (Terra 4 #10077). A first request was
    /// charged at `admit` already.
    pub fn refused_before_body(&self, route: &Route, peer: &Peer, e: Refusal) -> Refusal {
        match route {
            Route::Request => e,
            Route::Reveal(_) | Route::Poll(_) => self.charge_failed(peer, e),
        }
    }

    /// The 60 s timer.
    pub fn drop_idle(&self) -> usize {
        let now = self.gate.observe_mono();
        let mut rates = self.pending_rate.lock().unwrap_or_else(|e| e.into_inner());
        let before = rates.len();
        rates.retain(|_, b| !b.idle(now));
        self.limiter.drop_idle(now) + self.lookups.drop_idle(now) + before - rates.len()
    }

    /// One reveal or poll for `machine`. The table holds only ids whose
    /// key just signed, which are pending or enrolled, so it is bounded
    /// by the pending cap and the graph's machines.
    fn admit_pending(&self, machine: &str) -> Result<(), Refusal> {
        let now = self.gate.observe_mono();
        let mut rates = self.pending_rate.lock().unwrap_or_else(|e| e.into_inner());
        rates
            .entry(machine.to_owned())
            .or_insert_with(|| Bucket::new(1.0, 2.0, now))
            .take(now)
            .map_err(|secs| {
                Refusal::new(Code::ApiRateLimited, ErrorKind::Conflict, "enroll_poll")
                    .retry_after(secs)
            })
    }

    pub fn handle(
        &self,
        route: &Route,
        head: &RequestHead,
        body: &[u8],
        peer: &Peer,
    ) -> Result<Handled, Refusal> {
        if body.len() as u64 > self.limits.enroll_body_bytes {
            return Err(Refusal::new(Code::ApiTooLarge, ErrorKind::Usage, "body"));
        }
        match route {
            Route::Request => self.request(head, body, peer),
            Route::Reveal(id) => self.reveal(id, head, body, peer),
            Route::Poll(id) => self.poll(id, head, body, peer),
        }
    }

    fn request(&self, head: &RequestHead, body: &[u8], peer: &Peer) -> Result<Handled, Refusal> {
        // The body's form, after the origin and before the signature, the
        // store and the nonce: syntax first, so a malformed request reads
        // no store and uses no nonce (Terra 7 #8463, orchestrator #8480).
        self.gate.check_origin(head)?;
        let (v, name, commitment) = (|| {
            let v: Value = serde_json::from_slice(body)
                .map_err(|_| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "body"))?;
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if !valid_name(&name) {
                return Err(Refusal::new(
                    Code::UsageInvalidArguments,
                    ErrorKind::Usage,
                    "name",
                ));
            }
            let commitment = bytes32(&v, "commitment").ok_or_else(|| {
                Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "commitment")
            })?;
            Ok((v, name, commitment))
        })()?;
        let (
            auth,
            EnrollKey {
                machine_id,
                public_key,
            },
        ) = self.gate.authenticate_enroll(head, body, &|key| {
            // The store's own rows: the key directory can lag them (Terra
            // 6 #6939). Only the key a pending row holds learns it is
            // pending; any other key learns only that the id exists
            // (orchestrator #7853).
            let exists = || Refusal::new(Code::ApiEnrollExists, ErrorKind::Conflict, "exists");
            match self
                .store
                .lookup(&key.machine_id, self.gate.now_ms())
                .map_err(store_failed)?
            {
                None => Ok(()),
                Some(Enrolment::Machine { .. }) => Err(exists()),
                Some(Enrolment::Pending(row)) if row.public_key == key.public_key => Err(
                    Refusal::new(Code::ApiEnrollPending, ErrorKind::Conflict, "pending"),
                ),
                Some(Enrolment::Pending(_)) => Err(exists()),
            }
        })?;
        if v.get("graph_id").and_then(Value::as_str) != Some(self.gate.graph_id()) {
            return Err(Refusal::new(
                Code::ApiGraphUnknown,
                ErrorKind::NotFound,
                "graph_unknown",
            ));
        }
        let now_ms = self.gate.now_ms();
        let pending = NewPending {
            pending_id: uuid_like()?,
            machine_id: machine_id.clone(),
            public_key,
            fingerprint: mdkg_wire::keys::fingerprint(&public_key),
            name,
            commitment,
            ns: random::bytes().map_err(|_| no_random())?,
            requested_ms: now_ms,
            expires_ms: now_ms + PENDING_MS,
            source: peer.address(),
        };
        match self
            .store
            .request(&pending, MAX_PENDING, now_ms)
            .map_err(store_failed)?
        {
            Ok(()) => {}
            // A request that raced ours to the store: only its own key
            // learns it is pending (Terra 3 #8179).
            Err(PendingRefused::Exists) => {
                return Err(Refusal::new(
                    Code::ApiEnrollExists,
                    ErrorKind::Conflict,
                    "exists",
                ));
            }
            Err(PendingRefused::Pending(held)) if held != public_key => {
                return Err(Refusal::new(
                    Code::ApiEnrollExists,
                    ErrorKind::Conflict,
                    "exists",
                ));
            }
            Err(PendingRefused::Pending(_)) => {
                return Err(Refusal::new(
                    Code::ApiEnrollPending,
                    ErrorKind::Conflict,
                    "pending",
                ));
            }
            Err(PendingRefused::Full) => {
                return Err(Refusal::new(
                    Code::ApiRateLimited,
                    ErrorKind::Conflict,
                    "pending_full",
                )
                .retry_after(60));
            }
        }
        debug_assert_eq!(auth.machine(), machine_id);
        Ok(Handled {
            response: ok(json!({
                "pending_id": pending.pending_id,
                "ns": b64::encode_url(&pending.ns),
                "expires_ms": pending.expires_ms,
            })),
            event: Some(Ephemeral {
                kind: "mdkg.machine.enroll_requested",
                data: json!({
                    "machine_id": machine_id,
                    "pending_id": pending.pending_id,
                    "name": pending.name,
                    "fingerprint": pending.fingerprint,
                    // No `source`: the stream reaches every subscriber,
                    // and the peer's address is for `machine list
                    // --pending` on the host only (#7049).
                }),
            }),
        })
    }

    /// The id's row, after its key verified the request. Whatever the
    /// store holds, a caller without the key gets the same refusal: an
    /// unknown id is verified against [`NO_KEY`], and an enrolled machine
    /// is told it isn't pending only once its own key signed (#6717).
    ///
    /// `form` checks the route's body before the store is read, after the
    /// origin, and a body it refuses costs the source like any other
    /// failure (Terra 3 #8179, Opus #8132).
    fn authenticated_row<T>(
        &self,
        id: &str,
        head: &RequestHead,
        body: &[u8],
        peer: &Peer,
        form: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<(Option<Enrolment>, T), Refusal> {
        let failed = |e| self.charge_failed(peer, e);
        // The request's form before the store is read (orchestrator
        // #7278): the gate's, then the route's own.
        self.gate.precheck_pending(head).map_err(failed)?;
        let form = form().map_err(failed)?;
        let row = self
            .store
            .lookup(id, self.gate.now_ms())
            .map_err(store_failed)?;
        let key = match &row {
            None => NO_KEY,
            Some(Enrolment::Pending(r)) => r.public_key,
            Some(Enrolment::Machine { public_key, .. }) => *public_key,
        };
        let auth = self
            .gate
            .authenticate_pending(head, id, &key, &|m| {
                if row.is_none() {
                    return Err(Refusal::unauthenticated("bad_signature"));
                }
                self.admit_pending(m)
            })
            .map_err(failed)?;
        self.gate.check_pending_body(&auth, body)?;
        Ok((row, form))
    }

    /// Charges a reveal or poll that failed to authenticate to its source,
    /// whatever the row, so the limit says nothing of which ids exist
    /// (#6717, orchestrator #7392, #7770). A request whose signature
    /// verified is never charged there (orchestrator #8197, Terra 2
    /// #8164): neither over its own bucket, which it has paid (#7059), nor
    /// on a full nonce cache; nor is a refusal that is the server's own
    /// failure. A charge over the window refuses: that answer, too, is the
    /// same for every id.
    fn charge_failed(&self, peer: &Peer, e: Refusal) -> Refusal {
        if VERIFIED_OR_OURS.contains(&e.reason) {
            return e;
        }
        match self.lookups.admit(peer, self.gate.observe_mono()) {
            Ok(()) => e,
            Err(limited) => limited,
        }
    }

    fn reveal(
        &self,
        id: &str,
        head: &RequestHead,
        body: &[u8],
        peer: &Peer,
    ) -> Result<Handled, Refusal> {
        // The body's shape after the origin and before the store is read
        // (orchestrator #7278, Terra 3 #8179).
        let nc = || {
            serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| bytes32(&v, "nc"))
                .ok_or_else(|| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "nc"))
        };
        let (row, nc) = self.authenticated_row(id, head, body, peer, nc)?;
        let row = match row {
            Some(Enrolment::Pending(row)) => row,
            _ => return Err(changed("not_pending")),
        };
        if !mdkg_wire::pair::reveal_matches(&row.commitment, &nc) {
            return Err(changed("commitment"));
        }
        let revealed = |had: Option<[u8; 32]>| match had {
            Some(had) if had != nc => Err(changed("revealed")),
            _ => Ok(Handled {
                response: ok(json!({"pending_id": row.pending_id, "revealed": true})),
                event: None,
            }),
        };
        if row.nc.is_some() {
            return revealed(row.nc);
        }
        if self
            .store
            .reveal(&row.pending_id, &nc, self.gate.now_ms())
            .map_err(store_failed)?
        {
            return revealed(None);
        }
        // Another reveal won the compare-and-set: say what it set (#6717).
        match self
            .store
            .lookup(id, self.gate.now_ms())
            .map_err(store_failed)?
        {
            Some(Enrolment::Pending(now))
                if now.pending_id == row.pending_id && now.nc.is_some() =>
            {
                revealed(now.nc)
            }
            _ => Err(changed("not_pending")),
        }
    }

    fn poll(
        &self,
        id: &str,
        head: &RequestHead,
        body: &[u8],
        peer: &Peer,
    ) -> Result<Handled, Refusal> {
        let data = match self.authenticated_row(id, head, body, peer, || Ok(()))?.0 {
            // `ns` and `expires_ms` again: a worker checks them against
            // the values it pinned from the enrolment response, and one
            // that never received that response doesn't recover from them
            // but starts over (orchestrator #8343, Terra 4 #8656).
            Some(Enrolment::Pending(row)) => json!({
                "state": "pending",
                "pending_id": row.pending_id,
                "revealed": row.nc.is_some(),
                "ns": b64::encode_url(&row.ns),
                "expires_ms": row.expires_ms,
            }),
            // Approval doesn't bind the worker: it binds only once the
            // person types the host's code there too (S6).
            Some(Enrolment::Machine { state, .. }) => json!({"state": state}),
            None => return Err(Refusal::unauthenticated("bad_signature")),
        };
        Ok(Handled {
            response: ok(data),
            event: None,
        })
    }
}

/// A fresh pending id: 16 random bytes as hex.
fn uuid_like() -> Result<String, Refusal> {
    random::bytes::<16>()
        .map(|b| random::hex(&b))
        .map_err(|_| no_random())
}

#[cfg(test)]
mod tests;
