//! A person enrolment's proof of possession (S41, orchestrator #8046):
//! `POST /persons/{pending_id}:prove`.
//!
//! The candidate key signed the first `POST /persons:enroll`, and the host
//! answered with the pending id and `Ns`. The candidate then proves it
//! holds the key over what only this request has: it signs the
//! possession challenge, the label `mdkg-person-possession-v1`, the
//! pending id, `Ns`, its commitment to `Nc` and the graph id, each framed
//! as the pairing codes are. The host verifies the proof and records the
//! pending row as proved, in the one transaction that checks the row is
//! still pending; `person approve` refuses a row that isn't proved
//! (serve-store's).
//!
//! The request itself is an approved machine's ([`Authenticated`]), the
//! machine the person enrols from. The challenge and the check of the
//! candidate's signature, FIDO2 flags included, are mdkg-wire's
//! (`person::possession_challenge`, `person::proves_possession`).

use std::sync::Arc;

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_ops::wire::{MachineState, PersonKey, PersonKeyKind};
use mdkg_wire::person::{Possession, proves_possession};
use serde_json::{Value, json};

use crate::auth::Authenticated;
use crate::b64;
use crate::enroll::StoreFailed;
use crate::http::Response;
use crate::refusal::Refusal;

pub const PROVE_SCHEMA: &str = "mdkg.serve.person_prove.v1";

/// A pending person request, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonPendingRow {
    pub pending_id: String,
    /// The machine the person enrols from: only its requests may prove.
    pub machine_id: String,
    /// The `human:*` actor the key is for.
    pub person: String,
    pub kind: PersonKeyKind,
    pub public_key: Vec<u8>,
    /// The presence policy the request named, `up` or `uv` (serve-store's
    /// `policy` column): a FIDO2 key under `uv` proves with the "user
    /// verified" flag too. Read from the row at every proof, so a restart
    /// can't weaken a `uv` enrolment (Terra 6 #8275, orchestrator #8279).
    pub policy: String,
    pub ns: [u8; 32],
    pub commitment: [u8; 32],
    /// When the proof verified (serve-store's `proved_ms`); `None` until it
    /// has.
    pub proved_ms: Option<u64>,
}

/// What [`PersonPendingStore::prove`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proved {
    /// The proof verified over the row and is recorded (or already was).
    Proved,
    /// No unexpired request `pending_id` from this machine.
    NotPending,
    /// The verifier refused the proof; nothing was recorded.
    Invalid,
    /// `nc` doesn't open the row's commitment: a store that checks it
    /// before running the verifier answers this, not `NotPending`, so the
    /// endpoint's `commitment` refusal is the same whichever checks it
    /// (Terra 1 #8887). Nothing was recorded.
    WrongNc,
    /// The acting machine is no longer approved, as the transaction read
    /// it: its state, never `Approved`.
    Machine(MachineState),
}

/// The graph's pending person rows (serve-store implements it, as
/// `person_prove`).
pub trait PersonPendingStore: Send + Sync {
    /// In one transaction: reads the coordinator's clock with `now` once
    /// the transaction is held, and judges expiry by that reading (Terra
    /// 1 #8585, orchestrator #8601); checks `machine_id` is still
    /// approved; finds the unexpired request `pending_id` it made; runs
    /// `verify` on that row; and records `proved_ms` and the revealed `nc`
    /// together only if it passes (orchestrator #8788), so
    /// the row verified is the row marked, whatever happened since the
    /// request was authenticated (Terra 3 #8363). `verify` runs on every
    /// call that finds the machine's eligible row and an `nc` that opens
    /// its commitment, an already-proved row's included, and `Proved`
    /// needs it to pass; proving a proved row again records nothing (Opus
    /// #8547). A store may answer `WrongNc` or `NotPending` without
    /// running it (Terra 1 #8967).
    fn prove(
        &self,
        pending_id: &str,
        machine_id: &str,
        nc: &[u8; 32],
        now: &dyn Fn() -> u64,
        verify: &mut dyn FnMut(&PersonPendingRow) -> bool,
    ) -> Result<Proved, StoreFailed>;
}

/// The proof endpoint of one `serve`.
pub struct PersonProofs {
    graph_id: String,
    store: Arc<dyn PersonPendingStore>,
}

impl std::fmt::Debug for PersonProofs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonProofs").finish_non_exhaustive()
    }
}

fn changed(reason: &'static str) -> Refusal {
    Refusal::new(Code::ApiEnrollChanged, ErrorKind::Conflict, reason)
}

fn store_failed(e: StoreFailed) -> Refusal {
    match e {
        StoreFailed::Busy => Refusal::new(Code::CoordStoreFailed, ErrorKind::Failure, "store_busy")
            .with_status(503)
            .retry_after(1),
        StoreFailed::Failed => Refusal::new(Code::CoordStoreFailed, ErrorKind::Failure, "store"),
    }
}

impl PersonProofs {
    pub fn new(graph_id: String, store: Arc<dyn PersonPendingStore>) -> Self {
        Self { graph_id, store }
    }

    /// `POST /persons/{pending_id}:prove`, body `{alg, proof, nc}` (both
    /// base64url): the proof, and the candidate's `Nc`, revealed here
    /// because the bootstrap path has no reveal route of its own (Terra 6
    /// #8558, orchestrator #8788). `nc` must match the row's commitment,
    /// checked with the proof. From the approved machine `auth`, whose body already
    /// matched its digest. A row that doesn't exist, has expired, or
    /// belongs to another machine is refused alike (`not_pending`), so a
    /// machine learns nothing of others' requests; a machine revoked since
    /// its request was authenticated is refused as revoked.
    pub fn prove(
        &self,
        auth: &Authenticated,
        pending_id: &str,
        body: &[u8],
        now: &dyn Fn() -> u64,
    ) -> Result<Response, Refusal> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|_| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "body"))?;
        let alg = v.get("alg").and_then(Value::as_str).unwrap_or_default();
        let proof = v
            .get("proof")
            .and_then(Value::as_str)
            .and_then(b64::decode_url)
            .ok_or_else(|| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "proof"))?;
        let nc: [u8; 32] = v
            .get("nc")
            .and_then(Value::as_str)
            .and_then(b64::decode_url)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "nc"))?;
        // The proof is checked inside the store's transaction, over the
        // row it marks (Terra 3 #8363).
        let (mut unreadable, mut wrong_nc) = (false, false);
        let mut verify = |row: &PersonPendingRow| {
            if !mdkg_wire::pair::reveal_matches(&row.commitment, &nc) {
                wrong_nc = true;
                return false;
            }
            let user_verified = match row.policy.as_str() {
                "up" => false,
                "uv" => true,
                // A row this build can't read is refused, never taken as
                // the weaker policy.
                _ => {
                    unreadable = true;
                    return false;
                }
            };
            let candidate = PersonKey {
                key_id: mdkg_wire::keys::fingerprint(&row.public_key),
                person: row.person.clone(),
                kind: row.kind,
                public_key: row.public_key.clone(),
                user_verified,
                revoked: false,
            };
            let issued = Possession {
                pending_id: &row.pending_id,
                ns: &row.ns,
                commitment: &row.commitment,
                graph_id: &self.graph_id,
            };
            proves_possession(&candidate, alg, &issued, &proof)
        };
        let found = self
            .store
            .prove(pending_id, auth.machine(), &nc, now, &mut verify)
            .map_err(store_failed)?;
        match found {
            Proved::Proved => {}
            _ if unreadable => return Err(store_failed(StoreFailed::Failed)),
            // As a machine's reveal refuses a mismatch (S6).
            Proved::WrongNc => return Err(changed("commitment")),
            _ if wrong_nc => return Err(changed("commitment")),
            Proved::Invalid => return Err(Refusal::unauthenticated("bad_proof")),
            Proved::NotPending => return Err(changed("not_pending")),
            Proved::Machine(MachineState::Pending) => {
                return Err(Refusal::new(
                    Code::ApiMachinePending,
                    ErrorKind::Conflict,
                    "machine_pending",
                ));
            }
            Proved::Machine(MachineState::Revoked) => {
                return Err(Refusal::new(
                    Code::ApiMachineRevoked,
                    ErrorKind::Conflict,
                    "machine_revoked",
                ));
            }
            // A store that calls an approved machine unapproved is broken:
            // not reported as a revocation (Opus #8547).
            Proved::Machine(MachineState::Approved) => {
                return Err(store_failed(StoreFailed::Failed));
            }
        }
        let body = json!({
            "schema": PROVE_SCHEMA,
            "ok": true,
            "data": {"pending_id": pending_id, "proved": true},
            "diagnostics": [],
        });
        Ok(Response {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests;
