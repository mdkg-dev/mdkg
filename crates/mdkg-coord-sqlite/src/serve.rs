//! The coordinator tables `serve` needs (the serve edd's Storage, S23):
//! machines and their pending enrolments, person keys, placements,
//! supervisors, turns with their candidates, shown sets and kept refusals,
//! and the idempotency rows of remote writes.
//!
//! - [`create`] makes the tables with `CREATE TABLE IF NOT EXISTS`, inside
//!   the caller's transaction. Nothing here changes `meta.schema`: a store
//!   that never meets serve never gets a row in these tables. **Pending,
//!   lane L8:** the store's open doesn't call `create` yet, and the
//!   required-features check (S23) that must keep older builds off a store
//!   holding serve rows isn't built; the two land together (#10769).
//! - Every write is a function over the caller's open transaction, so a
//!   served write's changes, its idempotency row, the acting machine's
//!   recheck and its records commit together (S16, R119). Reads take a
//!   [`Connection`], which a [`Transaction`] derefs to.
//! - Policy that needs the graph (placement rules, agent definitions) or
//!   the network (signatures, the pairing code) stays with the caller,
//!   which passes the facts in. What the store can decide against its own
//!   rows, it decides here, in the transaction: a check made before the
//!   transaction could be stale by the time it commits.
//! - Faults and handoff mail are sent by the caller in the same
//!   transaction, through the mail store; the functions here return what
//!   those messages need.
//!
//! Additions to the edd's table list, all additive, each closing a review
//! finding: `requests.intent` (a stored success whose graph publish is
//! still pending), `machines.revoke_by` and `revoke_started_ms`
//! (resumable revocation), `persons.machine_id`, `policy`, `attestation`,
//! `presence_capable` (#9864) and the revocation columns (presence and
//! person binding),
//! `person_pending.fingerprint` and `policy` (the compared code binds the
//! key), `person_pending.proved_ms` (the candidate key's challenge proof,
//! orchestrator #8047), `person_pending.attestation_object` (bounded
//! attestation evidence, #8984), `placements.retire_by`, `retire_started_ms`, `handoff_ms` and
//! `handoff_id` (resumable retirement and its handoff), and
//! `turns.ended_ms` (retention).
//!
//! No migration: these tables have never shipped in a release or on main,
//! so a store a pre-integration build made isn't kept, and `create` defines
//! every column from the start; there is no `ALTER TABLE` (orchestrator
//! #9151). One forward step, for development stores (#10686 item 5): a
//! `persons` table from before `presence_capable` and its CHECK is
//! rebuilt in `create`, its rows kept, `presence_capable` set from
//! `attestation` in the same step.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mdkg_format::{Code, Digest};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};

use crate::store::StoreError;

pub const SERVE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    machine TEXT NOT NULL,
    key TEXT NOT NULL,
    op TEXT NOT NULL,
    digest TEXT NOT NULL,
    result TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    intent TEXT,
    PRIMARY KEY (machine, key)
);
CREATE INDEX IF NOT EXISTS requests_expiry ON requests (expires_ms);
CREATE TABLE IF NOT EXISTS machines (
    machine_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    public_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    state TEXT NOT NULL,
    local INTEGER NOT NULL DEFAULT 0,
    groups TEXT NOT NULL,
    launcher TEXT NOT NULL,
    report TEXT,
    enrolled_ms INTEGER,
    enrolled_by TEXT,
    seen_ms INTEGER,
    lost_seq INTEGER,
    revoke_by TEXT,
    revoke_started_ms INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS machines_name ON machines (name)
    WHERE state IN ('approved', 'revoking');
CREATE UNIQUE INDEX IF NOT EXISTS machines_launcher ON machines (launcher);
CREATE TABLE IF NOT EXISTS pending (
    pending_id TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL UNIQUE,
    public_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    name TEXT NOT NULL,
    commitment TEXT NOT NULL,
    ns TEXT NOT NULL,
    nc TEXT,
    requested_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS persons (
    key_id TEXT PRIMARY KEY,
    human TEXT NOT NULL,
    kind TEXT NOT NULL,
    public_key TEXT NOT NULL,
    policy TEXT NOT NULL,
    state TEXT NOT NULL,
    enrolled_ms INTEGER,
    enrolled_by TEXT,
    machine_id TEXT,
    -- 'verified' only for a FIDO2 key with neither backup flag (BE, BS)
    -- whose kept attestation verified against the approver's roots; every
    -- other key is 'none'. wire's verifier trusts this value (#7692,
    -- #8902, #8984).
    attestation TEXT NOT NULL DEFAULT 'none',
    -- Stored capability, set at approval from a Verified outcome only: its
    -- assertions can grade Present, each by its own flags. Never
    -- authorization (orchestrator #9717, #9864).
    presence_capable INTEGER NOT NULL DEFAULT 0,
    revoked_ms INTEGER,
    revoked_by TEXT,
    -- The two can never disagree (orchestrator, answering #9971).
    CHECK (presence_capable = (attestation = 'verified'))
);
CREATE INDEX IF NOT EXISTS persons_human ON persons (human, state);
CREATE TABLE IF NOT EXISTS person_pending (
    pending_id TEXT PRIMARY KEY,
    human TEXT NOT NULL,
    kind TEXT NOT NULL,
    public_key TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    commitment TEXT NOT NULL,
    ns TEXT NOT NULL,
    nc TEXT,
    requested_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    policy TEXT NOT NULL,
    proved_ms INTEGER,
    -- The encoded attestation evidence (CBOR {fmt, authData, attStmt}, the
    -- fields this build understands), at most MAX_ATTESTATION_BYTES, kept only
    -- until approval deletes the row or prune_pending sweeps it after
    -- expiry (#8984, Terra 4 #9145).
    attestation_object BLOB
);
CREATE TABLE IF NOT EXISTS placements (
    agent TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL,
    checkout TEXT,
    source TEXT NOT NULL,
    state TEXT NOT NULL,
    since_ms INTEGER NOT NULL,
    retired_ms INTEGER,
    retire_by TEXT,
    retire_started_ms INTEGER,
    handoff_ms INTEGER,
    handoff_id TEXT
);
CREATE INDEX IF NOT EXISTS placements_machine ON placements (machine_id, state);
CREATE TABLE IF NOT EXISTS supervisors (
    supervisor_id TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL,
    pid INTEGER,
    build TEXT,
    started_ms INTEGER NOT NULL,
    heartbeat_ms INTEGER NOT NULL,
    state TEXT NOT NULL,
    agents TEXT NOT NULL,
    control TEXT
);
CREATE INDEX IF NOT EXISTS supervisors_machine ON supervisors (machine_id, state);
CREATE TABLE IF NOT EXISTS turns (
    turn_id TEXT PRIMARY KEY,
    agent TEXT NOT NULL,
    number INTEGER NOT NULL,
    machine_id TEXT NOT NULL,
    supervisor_id TEXT NOT NULL,
    ended_by TEXT,
    shape TEXT NOT NULL,
    nonce_digest TEXT NOT NULL,
    start_seq INTEGER NOT NULL,
    shown_seq INTEGER,
    end_seq INTEGER,
    outcome TEXT,
    ended_ms INTEGER,
    UNIQUE (agent, number)
);
CREATE INDEX IF NOT EXISTS turns_open ON turns (agent, end_seq);
CREATE TABLE IF NOT EXISTS turn_candidates (
    turn_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    flag TEXT,
    PRIMARY KEY (turn_id, kind, ref)
);
CREATE TABLE IF NOT EXISTS turn_shown (
    turn_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    flag TEXT,
    PRIMARY KEY (turn_id, kind, ref)
);
CREATE TABLE IF NOT EXISTS turn_refusals (
    turn_id TEXT NOT NULL,
    step TEXT NOT NULL,
    code TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    PRIMARY KEY (turn_id, step)
);
";

/// Creates the serve tables if they are missing, in the caller's
/// transaction. Safe on every open. A [`Transaction`], not a connection,
/// so the forward step's rebuild of `persons` can't run in autocommit,
/// where a failure part way would lose the table (Terra 2 #10817).
pub fn create(tx: &Transaction<'_>) -> std::result::Result<(), StoreError> {
    forward_persons(tx)?;
    tx.execute_batch(SERVE_SCHEMA)?;
    Ok(())
}

/// What `persons` must hold for its two attestation columns never to
/// disagree (orchestrator, answering #9971).
const PERSONS_CHECK: &str = "CHECK (presence_capable = (attestation = 'verified'))";

/// The development stores' forward step (#10686 item 5): a `persons` table
/// made before `presence_capable` or its CHECK is rebuilt from
/// [`SERVE_SCHEMA`], in the caller's transaction. Every column both shapes
/// have is kept, and `presence_capable` is set where `attestation` is
/// `verified`, in the same step, so the CHECK holds for every row. No
/// `ALTER TABLE`: the rows go to a temporary table and back, all inside
/// `tx`, so a failure part way leaves the old table as it was.
fn forward_persons(tx: &Transaction<'_>) -> std::result::Result<(), StoreError> {
    let conn: &Connection = tx;
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'persons'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match sql {
        None => return Ok(()),
        Some(sql) if sql.contains(PERSONS_CHECK) => return Ok(()),
        Some(_) => {}
    }
    let columns = |table: &str| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
        stmt.query_map([], |r| r.get::<_, String>(0))?.collect()
    };
    let old = columns("persons")?;
    conn.execute_batch(
        "CREATE TEMP TABLE persons_forward AS SELECT * FROM main.persons;
         DROP TABLE main.persons;",
    )?;
    conn.execute_batch(SERVE_SCHEMA)?;
    let kept: Vec<String> = columns("persons")?
        .into_iter()
        .filter(|c| c != "presence_capable" && old.contains(c))
        .collect();
    let capable = if old.iter().any(|c| c == "attestation") {
        "attestation = 'verified'"
    } else {
        "0"
    };
    let kept = kept.join(", ");
    conn.execute_batch(&format!(
        "INSERT INTO main.persons ({kept}, presence_capable)
             SELECT {kept}, {capable} FROM temp.persons_forward;
         DROP TABLE temp.persons_forward;"
    ))?;
    Ok(())
}

const MINUTE_MS: u64 = 60 * 1000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// How long a request row answers a retry (A13, S10).
pub const REQUEST_WINDOW_MS: u64 = DAY_MS;
/// A key older than this, by the server's clock, is refused (R90).
pub const KEY_MAX_AGE_MS: u64 = 23 * HOUR_MS;
/// A key whose time is further than this from the server's clock at
/// first use is refused (R90).
pub const KEY_MAX_SKEW_MS: u64 = 5 * MINUTE_MS;
/// A pending enrolment's life (S6).
pub const PENDING_MS: u64 = 15 * MINUTE_MS;
/// At most this many machine enrolments, and as many person enrolments,
/// are pending at once (S6, S21).
pub const MAX_PENDING: usize = 16;

/// The largest encoded attestation evidence a pending person request keeps,
/// measured on exactly the bytes stored (#8984, #9600).
pub const MAX_ATTESTATION_BYTES: usize = 16 * 1024;
/// A stored supervisor heartbeat counts as fresh for this long (S11).
pub const FRESH_HEARTBEAT_MS: u64 = 180 * 1000;
/// Candidates and shown sets are kept this long after their turn's end
/// (S29, R58).
pub const PROVENANCE_KEEP_MS: u64 = 30 * DAY_MS;

/// The actor `serve`'s timers write machine records and faults as.
pub const SERVE_ACTOR: &str = "mdkg:serve";
/// The prefix of the launcher actors the kernel mints (S27).
pub const LAUNCHER_PREFIX: &str = "launcher:";

/// Why a serve write was refused. Each maps to one code; the caller builds
/// the diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The machine id already has a row, in any state (R78).
    EnrollExists,
    /// The machine id, or the person key, already has a pending request
    /// (R52).
    EnrollPending,
    /// Too many enrolments are pending.
    EnrollFull,
    /// The pending request isn't the one named, or its revealed value
    /// doesn't match its commitment (R53).
    EnrollChanged,
    /// A machine or group name isn't actor syntax.
    BadName,
    /// The name is, or was, another machine's.
    NameTaken {
        machine: String,
    },
    MachineUnknown,
    MachineRevoked,
    PersonUnknown,
    PersonExists,
    /// A file key without the person's explicit opt-in (S41).
    FileKeyNotAllowed,
    /// The candidate key hasn't proved itself over the pending request's
    /// challenge yet, so it can't be approved (#8047).
    PersonUnproved,
    /// The candidate key's challenge proof didn't verify.
    ProofInvalid,
    /// `person_prove`: no pending request by that id for this device
    /// (gone, expired, approved or another device's).
    NotPending,
    /// `person_prove`: the `Nc` given doesn't open the request's
    /// commitment, or isn't the one already revealed. Reason `commitment`.
    CommitmentMismatch,
    /// The attestation object is over [`MAX_ATTESTATION_BYTES`].
    TooLarge,
    /// A keyless approval once the graph has an enrolled person: the
    /// approver must sign with their key (S41, R131; Terra 3 #9913).
    PersonSignatureRequired,
    /// The kept attestation evidence didn't verify (#9151 outcome 3). The
    /// approver may repeat the approval with the machine-grade
    /// acknowledgement.
    AttestationFailed {
        reason: FailedReason,
    },
    /// The key would be machine-grade and the approval didn't acknowledge
    /// it (`--machine-grade`, S41): code `api.machine_grade_required`,
    /// with the Absent reason (orchestrator #9400).
    MachineGradeRequired {
        reason: AbsentReason,
    },
    /// The key's kind can't carry the policy: `uv` on an enclave or file
    /// key (#8119).
    PolicyUnsupported,
    ActorNotAllowed {
        reason: &'static str,
    },
    /// The agent is placed on another machine, or this machine may not
    /// take it (R32).
    NotPlaced {
        placed_on: Option<String>,
    },
    /// The agent is retiring or retired (R110).
    Retired,
    NothingToDo,
    ShownMismatch,
    TurnUnknown,
    /// The turn already has an end, with this outcome.
    TurnEnded {
        outcome: String,
    },
    /// The nonce doesn't match the turn's digest.
    NonceMismatch,
    /// The idempotency key was used for a different request.
    KeyReused,
    KeySkewed,
    KeyExpired,
    /// The idempotency key isn't a UUIDv7.
    KeyInvalid,
}

impl Refusal {
    /// The refusal's diagnostic code: a registered [`Code`], so the
    /// compiler refuses one `mdkg-format` doesn't have (#10686 item 2).
    pub fn code(&self) -> Code {
        match self {
            Refusal::EnrollExists | Refusal::PersonExists => Code::ApiEnrollExists,
            Refusal::EnrollPending | Refusal::PersonUnproved => Code::ApiEnrollPending,
            Refusal::EnrollFull => Code::ApiRateLimited,
            Refusal::EnrollChanged | Refusal::NotPending | Refusal::CommitmentMismatch => {
                Code::ApiEnrollChanged
            }
            Refusal::BadName | Refusal::ActorNotAllowed { .. } => Code::ApiActorNotAllowed,
            Refusal::NameTaken { .. } => Code::ApiEnrollExists,
            Refusal::MachineUnknown | Refusal::ProofInvalid => Code::ApiUnauthenticated,
            Refusal::MachineRevoked => Code::ApiMachineRevoked,
            Refusal::PersonUnknown => Code::ApiPersonUnknown,
            Refusal::FileKeyNotAllowed | Refusal::PersonSignatureRequired => {
                Code::ApiPersonSignatureRequired
            }
            Refusal::PolicyUnsupported => Code::ApiUnsupported,
            Refusal::TooLarge => Code::ApiTooLarge,
            Refusal::AttestationFailed { .. } => Code::ApiAttestationFailed,
            Refusal::MachineGradeRequired { .. } => Code::ApiMachineGradeRequired,
            Refusal::NotPlaced { .. } | Refusal::Retired => Code::RunNotPlaced,
            Refusal::NothingToDo => Code::RunNothingToDo,
            Refusal::ShownMismatch => Code::ApiShownMismatch,
            Refusal::TurnUnknown => Code::ApiUnknownResource,
            Refusal::TurnEnded { .. } => Code::RunTurnEnded,
            Refusal::NonceMismatch => Code::LedgerBlockInvalid,
            Refusal::KeyReused => Code::CoordKeyReused,
            Refusal::KeySkewed => Code::ApiKeySkewed,
            Refusal::KeyExpired => Code::ApiKeyExpired,
            Refusal::KeyInvalid => Code::ApiKeyRequired,
        }
    }

    /// A refusal's reason where callers report one beside the code.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Refusal::CommitmentMismatch => Some("commitment"),
            Refusal::MachineGradeRequired { reason } => Some(reason.as_str()),
            Refusal::ActorNotAllowed { reason } => Some(reason),
            Refusal::AttestationFailed { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ServeError {
    Store(StoreError),
    Refused(Refusal),
    /// The attestation verifier couldn't give an outcome. Propagated, never
    /// flattened into Failed: the approval writes nothing, whatever the
    /// acknowledgement (S41, Terra 3 #9913).
    Verifier(VerifierError),
}

/// An attestation verifier's own error: no outcome at all, as opposed to
/// evidence it judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierError(pub String);

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeError::Store(e) => e.fmt(f),
            ServeError::Refused(r) => write!(f, "refused: {} ({r:?})", r.code().as_str()),
            ServeError::Verifier(e) => write!(f, "the attestation verifier failed: {}", e.0),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<StoreError> for ServeError {
    fn from(e: StoreError) -> Self {
        ServeError::Store(e)
    }
}

impl From<rusqlite::Error> for ServeError {
    fn from(e: rusqlite::Error) -> Self {
        ServeError::Store(StoreError::Sqlite(e))
    }
}

impl From<serde_json::Error> for ServeError {
    fn from(e: serde_json::Error) -> Self {
        ServeError::Store(StoreError::Json(e))
    }
}

pub type Result<T> = std::result::Result<T, ServeError>;

fn refuse<T>(r: Refusal) -> Result<T> {
    Err(ServeError::Refused(r))
}

fn sql_ms(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

fn ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or_default()
}

fn opt_ms(v: Option<i64>) -> Option<u64> {
    v.map(ms)
}

/// Appends one record in the caller's transaction and returns its seq.
fn record(
    conn: &Connection,
    actor: &str,
    kind: &str,
    subject: Vec<String>,
    body: Value,
) -> Result<u64> {
    let body = match body {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    let (seq, _) = crate::log::append(conn, actor, kind, subject, body)?;
    Ok(seq)
}

/// A machine or group name: lowercase actor syntax, 1 to 63 characters
/// of `a-z`, `0-9` and `-`, not starting or ending with `-`. So
/// `launcher:<name>` is always a valid actor (Opus r5 m4).
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
}

/// An actor the store will write into the hash-chained log: 1 to 128
/// printable ASCII characters, no whitespace or control characters. The
/// router validates actor syntax first (#6560); this keeps a malformed
/// string out of the log even if a caller skipped it (Claude 2 #10171).
fn loggable_actor(actor: &str) -> bool {
    !actor.is_empty() && actor.len() <= 128 && actor.bytes().all(|b| b.is_ascii_graphic())
}

fn is_human(actor: &str) -> bool {
    actor
        .strip_prefix("human:")
        .is_some_and(|name| !name.is_empty())
}

/// The launcher actor the kernel mints for a machine name (S27).
pub fn launcher_for(name: &str) -> String {
    format!("{LAUNCHER_PREFIX}{name}")
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    // `from_str_radix` would take a sign ("+a"), so check the digits first.
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// 32 bytes written as 64 lowercase hex digits, the form [`Digest`] and
/// the wire use; anything else is `None`.
fn lower_hex32(s: &str) -> Option<[u8; 32]> {
    if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    hex_bytes(s)?.try_into().ok()
}

/// Whether two hex texts are the same bytes: `Nc` is compared decoded, so
/// its case doesn't matter (Terra 4 #8861).
fn same_hex(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) && hex_bytes(a).is_some()
}

/// Whether `value`, given as hex, is the preimage of `commitment`, a
/// [`Digest`]'s text.
fn opens(commitment: &str, value_hex: &str) -> bool {
    hex_bytes(value_hex).is_some_and(|bytes| Digest::of_bytes(&bytes).to_string() == commitment)
}

/// The commitment to a random value, given as hex: the [`Digest`] text of
/// its bytes. `None` if the value isn't hex.
pub fn commitment_of(value_hex: &str) -> Option<String> {
    hex_bytes(value_hex).map(|bytes| Digest::of_bytes(&bytes).to_string())
}

fn strings(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

// ---------------------------------------------------------------------
// Idempotency rows (A13, S10, R6, R7, R90, R91)

/// The milliseconds a UUIDv7's timestamp holds, or `None` if `key` isn't a
/// UUIDv7.
pub fn key_time_ms(key: &str) -> Option<u64> {
    let parts: Vec<&str> = key.split('-').collect();
    let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    if lens != [8, 4, 4, 4, 12]
        || !parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
        || !parts[2].starts_with('7')
        || !matches!(
            parts[3].as_bytes().first(),
            Some(b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
        )
    {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", parts[0], parts[1]), 16).ok()
}

/// What a keyed write's earlier attempt left.
#[derive(Debug, Clone, PartialEq)]
pub enum Prior {
    /// No attempt committed: run the write and [`record_request`] it.
    Fresh,
    /// The first attempt committed this result. When `intent` is set, its
    /// graph publish (graph.ref's intent journal) hadn't been seen through
    /// when it was stored: the caller replays or finishes that intent before
    /// it returns the result, so a duplicate never reports a success the
    /// graph doesn't show (Terra 2 #6271).
    Duplicate {
        result: Value,
        intent: Option<String>,
    },
}

/// Checks a keyed write before it runs, inside its transaction. `digest`
/// is the canonical request's digest; `op` its operation.
pub fn check_request(
    conn: &Connection,
    machine: &str,
    key: &str,
    op: &str,
    digest: &str,
    now: u64,
) -> Result<Prior> {
    let Some(minted) = key_time_ms(key) else {
        return refuse(Refusal::KeyInvalid);
    };
    if now.saturating_sub(minted) > KEY_MAX_AGE_MS {
        return refuse(Refusal::KeyExpired);
    }
    let row: Option<(String, String, String, i64, Option<String>)> = conn
        .query_row(
            "SELECT op, digest, result, expires_ms, intent FROM requests
             WHERE machine = ?1 AND key = ?2",
            params![machine, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    match row {
        Some((row_op, row_digest, result, expires, intent)) if ms(expires) > now => {
            if row_op != op || row_digest != digest {
                return refuse(Refusal::KeyReused);
            }
            Ok(Prior::Duplicate {
                result: serde_json::from_str(&result)?,
                intent,
            })
        }
        _ => {
            if minted.abs_diff(now) > KEY_MAX_SKEW_MS {
                return refuse(Refusal::KeySkewed);
            }
            Ok(Prior::Fresh)
        }
    }
}

/// Stores a committed write's result under its key, in the write's own
/// transaction. Only a write that applied is recorded: a refusal writes no
/// row (R91). `intent` names a graph intent still to publish.
#[allow(clippy::too_many_arguments)]
pub fn record_request(
    tx: &Transaction<'_>,
    machine: &str,
    key: &str,
    op: &str,
    digest: &str,
    result: &Value,
    intent: Option<&str>,
    now: u64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO requests (machine, key, op, digest, result, created_ms, expires_ms, intent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (machine, key) DO UPDATE SET
            op = excluded.op, digest = excluded.digest, result = excluded.result,
            created_ms = excluded.created_ms, expires_ms = excluded.expires_ms,
            intent = excluded.intent
         WHERE requests.expires_ms <= excluded.created_ms",
        params![
            machine,
            key,
            op,
            digest,
            serde_json::to_string(result)?,
            sql_ms(now),
            sql_ms(now.saturating_add(REQUEST_WINDOW_MS)),
            intent
        ],
    )?;
    Ok(())
}

/// Marks a request's graph intent as published.
pub fn finish_request_intent(tx: &Transaction<'_>, machine: &str, key: &str) -> Result<()> {
    tx.execute(
        "UPDATE requests SET intent = NULL WHERE machine = ?1 AND key = ?2",
        params![machine, key],
    )?;
    Ok(())
}

/// Deletes request rows past their window; returns how many.
pub fn prune_requests(tx: &Transaction<'_>, now: u64) -> Result<usize> {
    Ok(tx.execute(
        "DELETE FROM requests WHERE expires_ms <= ?1 AND intent IS NULL",
        params![sql_ms(now)],
    )?)
}

// ---------------------------------------------------------------------
// Machines and their enrolment (S6, S11, S13, S27)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    Approved,
    /// Revocation's first phase committed; the second hasn't (S6).
    Revoking,
    Revoked,
}

impl MachineState {
    pub fn as_str(self) -> &'static str {
        match self {
            MachineState::Approved => "approved",
            MachineState::Revoking => "revoking",
            MachineState::Revoked => "revoked",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "approved" => MachineState::Approved,
            "revoking" => MachineState::Revoking,
            // Anything unknown is treated as the most restrictive state.
            _ => MachineState::Revoked,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub machine_id: String,
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub state: MachineState,
    /// The coordinator's own host.
    pub local: bool,
    pub groups: Vec<String>,
    pub launcher: String,
    pub report: Option<Value>,
    pub enrolled_ms: Option<u64>,
    pub enrolled_by: Option<String>,
    pub seen_ms: Option<u64>,
    /// The `machine.lost` record's seq while the machine is lost.
    pub lost_seq: Option<u64>,
    pub revoke_by: Option<String>,
    pub revoke_started_ms: Option<u64>,
}

const MACHINE_COLS: &str = "machine_id, name, public_key, fingerprint, state, local, groups, \
    launcher, report, enrolled_ms, enrolled_by, seen_ms, lost_seq, revoke_by, revoke_started_ms";

fn machine_row(r: &Row<'_>) -> rusqlite::Result<Machine> {
    let state: String = r.get(4)?;
    let groups: String = r.get(6)?;
    let report: Option<String> = r.get(8)?;
    Ok(Machine {
        machine_id: r.get(0)?,
        name: r.get(1)?,
        public_key: r.get(2)?,
        fingerprint: r.get(3)?,
        state: MachineState::parse(&state),
        local: r.get::<_, i64>(5)? != 0,
        groups: strings(&groups),
        launcher: r.get(7)?,
        report: report.and_then(|t| serde_json::from_str(&t).ok()),
        enrolled_ms: opt_ms(r.get(9)?),
        enrolled_by: r.get(10)?,
        seen_ms: opt_ms(r.get(11)?),
        lost_seq: opt_ms(r.get(12)?),
        revoke_by: r.get(13)?,
        revoke_started_ms: opt_ms(r.get(14)?),
    })
}

pub fn machine(conn: &Connection, machine_id: &str) -> Result<Option<Machine>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MACHINE_COLS} FROM machines WHERE machine_id = ?1"),
            params![machine_id],
            machine_row,
        )
        .optional()?)
}

pub fn machines(conn: &Connection) -> Result<Vec<Machine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MACHINE_COLS} FROM machines ORDER BY name, machine_id"
    ))?;
    let rows = stmt.query_map([], machine_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The acting machine, refused unless it is approved. Every served write
/// calls this inside its own transaction, so no write from a machine
/// commits after its revocation's first phase (S6, R17, R27).
pub fn require_approved(conn: &Connection, machine_id: &str) -> Result<Machine> {
    match machine(conn, machine_id)? {
        None => refuse(Refusal::MachineUnknown),
        Some(m) if m.state != MachineState::Approved => refuse(Refusal::MachineRevoked),
        Some(m) => Ok(m),
    }
}

/// Whether any remote machine is approved: until one is, no placement row
/// is written (R136).
pub fn has_remote_machine(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM machines WHERE local = 0 AND state = 'approved' LIMIT 1")?
        .exists([])?)
}

/// The machine that holds `name`, in any state. Names are never reused in
/// a graph's store, so a launcher actor names one machine for good.
fn name_holder(conn: &Connection, name: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT machine_id FROM machines WHERE name = ?1 ORDER BY machine_id LIMIT 1",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

/// A machine's enrolment request (S6), as `POST /enroll` gives it.
/// `pending_id` and `ns` are fresh random values the caller minted;
/// `commitment` is the [`Digest`] text of the worker's `Nc`.
#[derive(Debug, Clone)]
pub struct NewPending<'a> {
    pub pending_id: &'a str,
    pub machine_id: &'a str,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub name: &'a str,
    pub commitment: &'a str,
    pub ns: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub pending_id: String,
    pub machine_id: String,
    pub public_key: String,
    pub fingerprint: String,
    pub name: String,
    pub commitment: String,
    pub ns: String,
    pub nc: Option<String>,
    pub requested_ms: u64,
    pub expires_ms: u64,
    /// The machine that already has this name, if any (S6's `name_taken`).
    pub name_taken: Option<String>,
}

fn drop_expired_pending(tx: &Transaction<'_>, now: u64) -> Result<()> {
    prune_pending(tx, now).map(|_| ())
}

/// Deletes expired machine and person enrolment requests, a person
/// request's attestation evidence with it; returns how many rows went.
/// Writes on these tables drop expired rows too, but a write that is then
/// refused rolls that back. So callers run this in its own committed
/// transaction at store open and before each enroll, prove and approve,
/// where a later failure can't undo it (Terra 4 #9145, orchestrator
/// #9151).
pub fn prune_pending(tx: &Transaction<'_>, now: u64) -> Result<usize> {
    let mut n = tx.execute(
        "DELETE FROM pending WHERE expires_ms <= ?1",
        params![sql_ms(now)],
    )?;
    n += tx.execute(
        "DELETE FROM person_pending WHERE expires_ms <= ?1",
        params![sql_ms(now)],
    )?;
    Ok(n)
}

/// Records a pending machine enrolment. It grants nothing.
pub fn enroll(tx: &Transaction<'_>, req: &NewPending<'_>, now: u64) -> Result<()> {
    if !valid_name(req.name) {
        return refuse(Refusal::BadName);
    }
    if machine(tx, req.machine_id)?.is_some() {
        return refuse(Refusal::EnrollExists);
    }
    drop_expired_pending(tx, now)?;
    if tx
        .prepare("SELECT 1 FROM pending WHERE machine_id = ?1")?
        .exists(params![req.machine_id])?
    {
        return refuse(Refusal::EnrollPending);
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM pending", [], |r| r.get(0))?;
    if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_PENDING {
        return refuse(Refusal::EnrollFull);
    }
    tx.execute(
        "INSERT INTO pending (pending_id, machine_id, public_key, fingerprint, name, commitment,
            ns, nc, requested_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9)",
        params![
            req.pending_id,
            req.machine_id,
            req.public_key,
            req.fingerprint,
            req.name,
            req.commitment,
            req.ns,
            sql_ms(now),
            sql_ms(now.saturating_add(PENDING_MS)),
        ],
    )?;
    Ok(())
}

/// The worker reveals `Nc` (hex). It must open the commitment; a second
/// reveal must be the same value.
pub fn reveal(tx: &Transaction<'_>, machine_id: &str, nc: &str, now: u64) -> Result<Pending> {
    drop_expired_pending(tx, now)?;
    let Some(p) = pending_for(tx, machine_id)? else {
        return refuse(Refusal::EnrollChanged);
    };
    match &p.nc {
        Some(old) if same_hex(old, nc) => return Ok(p),
        Some(_) => return refuse(Refusal::EnrollChanged),
        None => {}
    }
    if !opens(&p.commitment, nc) {
        return refuse(Refusal::EnrollChanged);
    }
    // Stored lowercase, the form every later comparison expects.
    let nc = nc.to_ascii_lowercase();
    tx.execute(
        "UPDATE pending SET nc = ?1 WHERE machine_id = ?2",
        params![nc, machine_id],
    )?;
    Ok(Pending {
        nc: Some(nc.to_string()),
        ..p
    })
}

const PENDING_COLS: &str = "pending_id, machine_id, public_key, fingerprint, name, commitment, \
    ns, nc, requested_ms, expires_ms";

fn pending_row(conn: &Connection, r: &Row<'_>) -> rusqlite::Result<Pending> {
    let name: String = r.get(4)?;
    let machine_id: String = r.get(1)?;
    let taken = name_holder(conn, &name)
        .ok()
        .flatten()
        .filter(|m| *m != machine_id);
    Ok(Pending {
        pending_id: r.get(0)?,
        machine_id,
        public_key: r.get(2)?,
        fingerprint: r.get(3)?,
        name,
        commitment: r.get(5)?,
        ns: r.get(6)?,
        nc: r.get(7)?,
        requested_ms: ms(r.get(8)?),
        expires_ms: ms(r.get(9)?),
        name_taken: taken,
    })
}

fn pending_for(conn: &Connection, machine_id: &str) -> Result<Option<Pending>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PENDING_COLS} FROM pending WHERE machine_id = ?1"),
            params![machine_id],
            |r| pending_row(conn, r),
        )
        .optional()?)
}

/// The pending machine enrolments that haven't expired.
pub fn pending(conn: &Connection, now: u64) -> Result<Vec<Pending>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PENDING_COLS} FROM pending WHERE expires_ms > ?1 ORDER BY requested_ms"
    ))?;
    let rows = stmt.query_map(params![sql_ms(now)], |r| pending_row(conn, r))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `machine reject`: drops a pending request. Returns whether one existed.
pub fn reject(tx: &Transaction<'_>, machine_id: &str) -> Result<bool> {
    Ok(tx.execute(
        "DELETE FROM pending WHERE machine_id = ?1",
        params![machine_id],
    )? > 0)
}

/// What `machine approve` names: the exact pending request it showed.
/// The caller has already checked the typed pairing code against the code
/// it computed from this request's row.
#[derive(Debug, Clone)]
pub struct Approval<'a> {
    pub machine_id: &'a str,
    pub pending_id: &'a str,
    pub fingerprint: &'a str,
    pub human: &'a str,
    pub groups: &'a [String],
}

fn insert_machine(tx: &Transaction<'_>, m: &Machine) -> Result<()> {
    tx.execute(
        "INSERT INTO machines (machine_id, name, public_key, fingerprint, state, local, groups,
            launcher, enrolled_ms, enrolled_by)
         VALUES (?1, ?2, ?3, ?4, 'approved', ?5, ?6, ?7, ?8, ?9)",
        params![
            m.machine_id,
            m.name,
            m.public_key,
            m.fingerprint,
            i64::from(m.local),
            serde_json::to_string(&m.groups)?,
            m.launcher,
            m.enrolled_ms.map(sql_ms),
            m.enrolled_by,
        ],
    )?;
    Ok(())
}

fn check_new_machine(conn: &Connection, name: &str, groups: &[String], human: &str) -> Result<()> {
    if !is_human(human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "approval names a human:* person",
        });
    }
    if !valid_name(name) || !groups.iter().all(|g| valid_name(g)) {
        return refuse(Refusal::BadName);
    }
    if let Some(machine) = name_holder(conn, name)? {
        return refuse(Refusal::NameTaken { machine });
    }
    Ok(())
}

/// `machine approve`: a compare-and-set on the exact pending request
/// (S6, R16, R53). It writes the machine's row and a `machine.enroll`
/// record naming the person, or nothing.
pub fn approve(tx: &Transaction<'_>, a: &Approval<'_>, now: u64) -> Result<Machine> {
    drop_expired_pending(tx, now)?;
    let Some(p) = pending_for(tx, a.machine_id)? else {
        return refuse(Refusal::EnrollChanged);
    };
    if p.pending_id != a.pending_id
        || p.fingerprint != a.fingerprint
        || p.nc.is_none()
        || machine(tx, a.machine_id)?.is_some()
    {
        return refuse(Refusal::EnrollChanged);
    }
    check_new_machine(tx, &p.name, a.groups, a.human)?;
    let m = Machine {
        machine_id: p.machine_id.clone(),
        name: p.name.clone(),
        public_key: p.public_key,
        fingerprint: p.fingerprint,
        state: MachineState::Approved,
        local: false,
        groups: a.groups.to_vec(),
        launcher: launcher_for(&p.name),
        report: None,
        enrolled_ms: Some(now),
        enrolled_by: Some(a.human.to_string()),
        seen_ms: None,
        lost_seq: None,
        revoke_by: None,
        revoke_started_ms: None,
    };
    insert_machine(tx, &m)?;
    tx.execute(
        "DELETE FROM pending WHERE machine_id = ?1",
        params![a.machine_id],
    )?;
    enroll_record(tx, &m, a.human, false)?;
    Ok(m)
}

fn enroll_record(conn: &Connection, m: &Machine, human: &str, offline: bool) -> Result<()> {
    record(
        conn,
        human,
        "machine.enroll",
        vec![format!("machine:{}", m.machine_id)],
        json!({
            "machine": m.machine_id,
            "name": m.name,
            "fingerprint": m.fingerprint,
            "groups": m.groups,
            "launcher": m.launcher,
            "local": m.local,
            "offline": offline,
            "human": human,
        }),
    )?;
    Ok(())
}

/// `machine approve --offline`, and the host's own machine (`local`): the
/// person compared the fingerprint itself, so there is no pending request.
#[derive(Debug, Clone)]
pub struct OfflineApproval<'a> {
    pub machine_id: &'a str,
    pub name: &'a str,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub human: &'a str,
    pub groups: &'a [String],
    pub local: bool,
}

pub fn approve_offline(tx: &Transaction<'_>, a: &OfflineApproval<'_>, now: u64) -> Result<Machine> {
    drop_expired_pending(tx, now)?;
    if machine(tx, a.machine_id)?.is_some() {
        return refuse(Refusal::EnrollExists);
    }
    if pending_for(tx, a.machine_id)?.is_some() {
        return refuse(Refusal::EnrollPending);
    }
    if a.local
        && tx
            .prepare("SELECT 1 FROM machines WHERE local = 1 AND state != 'revoked'")?
            .exists([])?
    {
        return refuse(Refusal::EnrollExists);
    }
    check_new_machine(tx, a.name, a.groups, a.human)?;
    let m = Machine {
        machine_id: a.machine_id.to_string(),
        name: a.name.to_string(),
        public_key: a.public_key.to_string(),
        fingerprint: a.fingerprint.to_string(),
        state: MachineState::Approved,
        local: a.local,
        groups: a.groups.to_vec(),
        launcher: launcher_for(a.name),
        report: None,
        enrolled_ms: Some(now),
        enrolled_by: Some(a.human.to_string()),
        seen_ms: None,
        lost_seq: None,
        revoke_by: None,
        revoke_started_ms: None,
    };
    insert_machine(tx, &m)?;
    enroll_record(tx, &m, a.human, true)?;
    Ok(m)
}

/// `machine group --add|--remove` (S6). Only a person sets groups.
pub fn set_groups(
    tx: &Transaction<'_>,
    machine_id: &str,
    add: &[String],
    remove: &[String],
    human: &str,
) -> Result<Machine> {
    if !is_human(human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "groups are set by a human:* person",
        });
    }
    if !add.iter().all(|g| valid_name(g)) {
        return refuse(Refusal::BadName);
    }
    let mut m = require_approved(tx, machine_id)?;
    let mut groups: BTreeSet<String> = m.groups.iter().cloned().collect();
    groups.extend(add.iter().cloned());
    for g in remove {
        groups.remove(g);
    }
    m.groups = groups.into_iter().collect();
    tx.execute(
        "UPDATE machines SET groups = ?1 WHERE machine_id = ?2",
        params![serde_json::to_string(&m.groups)?, machine_id],
    )?;
    record(
        tx,
        human,
        "machine.group",
        vec![format!("machine:{machine_id}")],
        json!({"machine": machine_id, "add": add, "remove": remove, "groups": m.groups, "human": human}),
    )?;
    Ok(m)
}

/// Where a revocation stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Revocation {
    /// Phase 1 is committed (now or by an earlier run): take the git fence,
    /// then [`finish_revoke`].
    Revoking(Machine),
    /// Already revoked; nothing to do.
    Revoked(Machine),
}

/// Revocation's first phase (S6, R97): marks the machine `revoking` and
/// records who asked, so a rerun after a crash resumes at phase 2 with the
/// same person (Opus r5 m3, Terra 2's recovery finding). A rerun on a
/// `revoking` machine changes nothing.
pub fn begin_revoke(
    tx: &Transaction<'_>,
    machine_id: &str,
    human: &str,
    now: u64,
) -> Result<Revocation> {
    if !is_human(human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "revocation names a human:* person",
        });
    }
    let Some(mut m) = machine(tx, machine_id)? else {
        return refuse(Refusal::MachineUnknown);
    };
    match m.state {
        MachineState::Revoked => Ok(Revocation::Revoked(m)),
        MachineState::Revoking => Ok(Revocation::Revoking(m)),
        MachineState::Approved => {
            tx.execute(
                "UPDATE machines SET state = 'revoking', revoke_by = ?1, revoke_started_ms = ?2
                 WHERE machine_id = ?3",
                params![human, sql_ms(now), machine_id],
            )?;
            record(
                tx,
                human,
                "machine.revoking",
                vec![format!("machine:{machine_id}")],
                json!({"machine": machine_id, "human": human}),
            )?;
            m.state = MachineState::Revoking;
            m.revoke_by = Some(human.to_string());
            m.revoke_started_ms = Some(now);
            Ok(Revocation::Revoking(m))
        }
    }
}

/// An agent placed on a machine, for the faults a revoke or a loss sends
/// to its `reports_to` in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedAgent {
    pub agent: String,
    pub state: PlacementState,
}

/// Revocation's second phase, under the machine's git fence (S6, R88,
/// R97): marks it `revoked` and writes `machine.revoke` naming the person
/// who began it. Returns the agents placed there, for the caller's
/// `run.machine_revoked` faults in this transaction, and `true` if this
/// call did the revoke (a rerun on a revoked machine returns `false`).
pub fn finish_revoke(tx: &Transaction<'_>, machine_id: &str) -> Result<(Vec<PlacedAgent>, bool)> {
    let Some(m) = machine(tx, machine_id)? else {
        return refuse(Refusal::MachineUnknown);
    };
    match m.state {
        MachineState::Revoked => Ok((placed_on(tx, machine_id)?, false)),
        MachineState::Approved => refuse(Refusal::ActorNotAllowed {
            reason: "revocation's first phase hasn't committed",
        }),
        MachineState::Revoking => {
            tx.execute(
                "UPDATE machines SET state = 'revoked' WHERE machine_id = ?1",
                params![machine_id],
            )?;
            let human = m.revoke_by.clone().unwrap_or_else(|| SERVE_ACTOR.into());
            record(
                tx,
                &human,
                "machine.revoke",
                vec![format!("machine:{machine_id}")],
                json!({"machine": machine_id, "human": human}),
            )?;
            Ok((placed_on(tx, machine_id)?, true))
        }
    }
}

/// Machines whose revocation began more than `older_than_ms` ago and
/// hasn't finished, for `serve status` and `doctor` (Opus r5 m3).
pub fn stalled_revocations(
    conn: &Connection,
    now: u64,
    older_than_ms: u64,
) -> Result<Vec<Machine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MACHINE_COLS} FROM machines
         WHERE state = 'revoking' AND revoke_started_ms <= ?1 ORDER BY revoke_started_ms"
    ))?;
    let rows = stmt.query_map(
        params![sql_ms(now.saturating_sub(older_than_ms))],
        machine_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn logged_in_by_harness(report: Option<&Value>) -> BTreeMap<String, Value> {
    report
        .and_then(|r| r.get("harnesses"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some((
                h.get("harness")?.as_str()?.to_string(),
                h.get("logged_in").cloned().unwrap_or(Value::Null),
            ))
        })
        .collect()
}

/// `machine.report` (S11): replaces the stored report. Writes a
/// `machine.report` record only when some harness's `logged_in` changed,
/// and returns whether it did.
pub fn set_report(tx: &Transaction<'_>, machine_id: &str, report: &Value) -> Result<bool> {
    let m = require_approved(tx, machine_id)?;
    let before = logged_in_by_harness(m.report.as_ref());
    let after = logged_in_by_harness(Some(report));
    tx.execute(
        "UPDATE machines SET report = ?1 WHERE machine_id = ?2",
        params![serde_json::to_string(report)?, machine_id],
    )?;
    let changed = before != after;
    if changed {
        record(
            tx,
            &m.launcher,
            "machine.report",
            vec![format!("machine:{machine_id}")],
            json!({"machine": machine_id, "logged_in": after}),
        )?;
    }
    Ok(changed)
}

/// The 60-second timer's write of each machine's last arrival.
pub fn set_seen(tx: &Transaction<'_>, seen: &[(String, u64)]) -> Result<()> {
    let mut stmt = tx.prepare(
        "UPDATE machines SET seen_ms = MAX(COALESCE(seen_ms, 0), ?1) WHERE machine_id = ?2",
    )?;
    for (machine_id, at) in seen {
        stmt.execute(params![sql_ms(*at), machine_id])?;
    }
    Ok(())
}

/// Marks a machine lost (S13), once per loss: returns the `machine.lost`
/// record's seq, for the faults' deterministic ids in this transaction, or
/// `None` if the machine is already lost or isn't approved. `serve`
/// decided the loss from its in-memory arrivals.
pub fn mark_lost(
    tx: &Transaction<'_>,
    machine_id: &str,
) -> Result<Option<(u64, Vec<PlacedAgent>)>> {
    let Some(m) = machine(tx, machine_id)? else {
        return Ok(None);
    };
    if m.state != MachineState::Approved || m.lost_seq.is_some() {
        return Ok(None);
    }
    let seq = record(
        tx,
        SERVE_ACTOR,
        "machine.lost",
        vec![format!("machine:{machine_id}")],
        json!({"machine": machine_id, "seen_ms": m.seen_ms}),
    )?;
    tx.execute(
        "UPDATE machines SET lost_seq = ?1 WHERE machine_id = ?2",
        params![sql_ms(seq), machine_id],
    )?;
    Ok(Some((seq, placed_on(tx, machine_id)?)))
}

/// A lost machine's next heartbeat: clears the loss and writes
/// `machine.returned`. Returns the loss's seq, whose faults the caller
/// clears in this transaction, or `None` if it wasn't lost.
pub fn mark_returned(tx: &Transaction<'_>, machine_id: &str) -> Result<Option<u64>> {
    let m = require_approved(tx, machine_id)?;
    let Some(lost) = m.lost_seq else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE machines SET lost_seq = NULL WHERE machine_id = ?1",
        params![machine_id],
    )?;
    record(
        tx,
        SERVE_ACTOR,
        "machine.returned",
        vec![format!("machine:{machine_id}")],
        json!({"machine": machine_id, "lost_seq": lost}),
    )?;
    Ok(Some(lost))
}

/// `meta.serve_seen_ms`, the serve heartbeat (S13, R67).
pub fn set_serve_seen(tx: &Transaction<'_>, now: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('serve_seen_ms', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![now.to_string()],
    )?;
    Ok(())
}

pub fn serve_seen(conn: &Connection) -> Result<Option<u64>> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'serve_seen_ms'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|v| v.parse().ok()))
}

// ---------------------------------------------------------------------
// Person keys (S41)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonKind {
    Enclave,
    Fido2,
    File,
}

impl PersonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PersonKind::Enclave => "enclave",
            PersonKind::Fido2 => "fido2",
            PersonKind::File => "file",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enclave" => Some(PersonKind::Enclave),
            "fido2" => Some(PersonKind::Fido2),
            "file" => Some(PersonKind::File),
            _ => None,
        }
    }
}

/// What a person key's assertions must carry: `up`, user presence in
/// each, or `uv`, user verification as well (which implies presence).
/// These are the policy names every lane uses (orchestrator #7692).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonPolicy {
    Up,
    Uv,
}

impl PersonPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            PersonPolicy::Up => "up",
            PersonPolicy::Uv => "uv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(PersonPolicy::Up),
            "uv" => Some(PersonPolicy::Uv),
            _ => None,
        }
    }

    /// Whether a key of `kind` can carry this policy: `uv` only a FIDO2
    /// key, whose assertions show verification; enclave and file keys take
    /// `up` only (orchestrator #8119).
    pub fn allowed_for(self, kind: PersonKind) -> bool {
        self == PersonPolicy::Up || kind == PersonKind::Fido2
    }
}

/// The evidence column: whether enrolment verified the key's attestation.
/// In 0.7.1 `Verified` is only a FIDO2 key whose attestation verified
/// against the approver's roots with neither BE nor BS; hardware backing
/// alone, a Secure Enclave key's included, is not (Terra 2 #6271, Terra
/// #7636, orchestrator #7692). It says nothing about any one signature:
/// each assertion is graded by its own flags, and one with BE or BS set is
/// machine grade even from a verified key (S41, Terra 3 #9913).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attestation {
    Verified,
    None,
}

impl Attestation {
    pub fn as_str(self) -> &'static str {
        match self {
            Attestation::Verified => "verified",
            Attestation::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonKey {
    /// The key's fingerprint, sent as `Mdkg-Person`.
    pub key_id: String,
    pub human: String,
    pub kind: String,
    pub public_key: String,
    pub policy: String,
    pub revoked: bool,
    pub enrolled_ms: Option<u64>,
    pub enrolled_by: Option<String>,
    /// The device's machine it was enrolled from.
    pub machine_id: Option<String>,
    pub attestation: String,
    /// Stored at approval: the key's assertions *can* grade Present. This
    /// is capability, never authorization: `Present` is only ever the grade
    /// of one verified assertion, with its own flags (S41, orchestrator
    /// #9717, #9864). Every other key, an enclave or file key included, is
    /// machine grade (R129).
    pub presence_capable: bool,
}

impl PersonKey {
    /// The stored capability's name in the approval record:
    /// `presence-capable` or `machine-grade` (S41).
    pub fn capability(&self) -> &'static str {
        if self.presence_capable {
            "presence-capable"
        } else {
            "machine-grade"
        }
    }
}

const PERSON_COLS: &str = "key_id, human, kind, public_key, policy, state, enrolled_ms, \
    enrolled_by, machine_id, attestation, presence_capable";

fn person_row(r: &Row<'_>) -> rusqlite::Result<PersonKey> {
    let state: String = r.get(5)?;
    Ok(PersonKey {
        key_id: r.get(0)?,
        human: r.get(1)?,
        kind: r.get(2)?,
        public_key: r.get(3)?,
        policy: r.get(4)?,
        revoked: state != "approved",
        enrolled_ms: opt_ms(r.get(6)?),
        enrolled_by: r.get(7)?,
        machine_id: r.get(8)?,
        attestation: r.get(9)?,
        presence_capable: r.get::<_, i64>(10)? != 0,
    })
}

pub fn person_key(conn: &Connection, key_id: &str) -> Result<Option<PersonKey>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PERSON_COLS} FROM persons WHERE key_id = ?1"),
            params![key_id],
            person_row,
        )
        .optional()?)
}

pub fn person_keys(conn: &Connection) -> Result<Vec<PersonKey>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PERSON_COLS} FROM persons ORDER BY human, key_id"
    ))?;
    let rows = stmt.query_map([], person_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether `human` has an approved key: if so, their `host` and
/// `operator` operations need its signature (R131).
pub fn has_person_key(conn: &Connection, human: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM persons WHERE human = ?1 AND state = 'approved' LIMIT 1")?
        .exists(params![human])?)
}

/// Whether the graph has an enrolled person: any approved, unrevoked key.
/// Without one, a keyless host approval is the bootstrap (or, all keys
/// revoked, the recovery) path (S41).
pub fn has_any_person_key(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM persons WHERE state = 'approved' LIMIT 1")?
        .exists([])?)
}

/// The person key a request was signed with, rechecked inside the write's
/// transaction: unknown or revoked is `api.person_unknown`, another
/// person's is `api.actor_not_allowed` (R132). Transport verified it
/// earlier; this closes the race with a revoke in between (Terra 2's
/// person-key revocation finding).
pub fn require_person(conn: &Connection, key_id: &str, human: &str) -> Result<PersonKey> {
    match person_key(conn, key_id)? {
        None => refuse(Refusal::PersonUnknown),
        Some(k) if k.revoked => refuse(Refusal::PersonUnknown),
        Some(k) if k.human != human => refuse(Refusal::ActorNotAllowed {
            reason: "the person key belongs to another person",
        }),
        Some(k) => Ok(k),
    }
}

/// A person key's enrolment request, signed by the device's machine and
/// by the new key. Every field here is an input of the person pairing
/// code, and approval compares each one, so a relay that swaps the key,
/// the person, the device or the policy changes the code and is refused
/// (Terra 2's person-code finding).
#[derive(Debug, Clone)]
pub struct NewPersonPending<'a> {
    pub pending_id: &'a str,
    pub human: &'a str,
    pub kind: PersonKind,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub machine_id: &'a str,
    pub policy: PersonPolicy,
    pub commitment: &'a str,
    pub ns: &'a str,
    /// The key's encoded attestation evidence, if the device sent one: the
    /// CBOR encoding of {fmt, authData, attStmt} with the attStmt fields
    /// this build understands (#9600): complete for packed; for none and the
    /// unverified formats it keeps only the fields this build parses, so it
    /// may not hold every field the authenticator sent (Terra 5 #9674); evidence
    /// that approval verifies against the approver's roots (#8984).
    pub attestation_object: Option<&'a [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonPending {
    pub pending_id: String,
    pub human: String,
    pub kind: String,
    pub public_key: String,
    pub fingerprint: String,
    pub machine_id: String,
    pub policy: String,
    pub commitment: String,
    pub ns: String,
    pub nc: Option<String>,
    pub requested_ms: u64,
    pub expires_ms: u64,
    /// When the candidate key's challenge proof verified, if it has.
    pub proved_ms: Option<u64>,
    /// The encoded attestation evidence the request carried, if any.
    pub attestation_object: Option<Vec<u8>>,
}

impl PersonPending {
    /// The bytes wire's `Possession` binds besides the pending id and the
    /// graph id: `Ns`, stored as hex, and the commitment to `Nc`, stored as
    /// a [`Digest`]'s text (`sha256:` and the hex of SHA-256(Nc), wire's
    /// `pair::commitment`). `None` if either isn't 32 bytes of that form,
    /// which a verifier treats as a failed proof.
    pub fn possession_bytes(&self) -> Option<([u8; 32], [u8; 32])> {
        let ns = lower_hex32(&self.ns)?;
        // Only the canonical text `person_reveal` compares against
        // (Terra 1 #8520).
        let commitment = lower_hex32(Digest::parse(&self.commitment)?.hex())?;
        Some((ns, commitment))
    }
}

const PERSON_PENDING_COLS: &str = "pending_id, human, kind, public_key, fingerprint, machine_id, \
    policy, commitment, ns, nc, requested_ms, expires_ms, proved_ms, attestation_object";

fn person_pending_row(r: &Row<'_>) -> rusqlite::Result<PersonPending> {
    Ok(PersonPending {
        pending_id: r.get(0)?,
        human: r.get(1)?,
        kind: r.get(2)?,
        public_key: r.get(3)?,
        fingerprint: r.get(4)?,
        machine_id: r.get(5)?,
        policy: r.get(6)?,
        commitment: r.get(7)?,
        ns: r.get(8)?,
        nc: r.get(9)?,
        requested_ms: ms(r.get(10)?),
        expires_ms: ms(r.get(11)?),
        proved_ms: opt_ms(r.get(12)?),
        attestation_object: r.get(13)?,
    })
}

fn person_pending_by_id(conn: &Connection, pending_id: &str) -> Result<Option<PersonPending>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PERSON_PENDING_COLS} FROM person_pending WHERE pending_id = ?1"),
            params![pending_id],
            person_pending_row,
        )
        .optional()?)
}

pub fn person_pending(conn: &Connection, now: u64) -> Result<Vec<PersonPending>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PERSON_PENDING_COLS} FROM person_pending WHERE expires_ms > ?1
         ORDER BY requested_ms"
    ))?;
    let rows = stmt.query_map(params![sql_ms(now)], person_pending_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records a pending person-key enrolment from an approved device.
pub fn person_enroll(tx: &Transaction<'_>, req: &NewPersonPending<'_>, now: u64) -> Result<()> {
    if !is_human(req.human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "a person key belongs to a human:* person",
        });
    }
    // The acting device first, so a revoked one hears api.machine_revoked
    // whatever else is wrong with the request (Terra 1 #8520).
    require_approved(tx, req.machine_id)?;
    if !req.policy.allowed_for(req.kind) {
        return refuse(Refusal::PolicyUnsupported);
    }
    if req
        .attestation_object
        .is_some_and(|a| a.len() > MAX_ATTESTATION_BYTES)
    {
        return refuse(Refusal::TooLarge);
    }
    drop_expired_pending(tx, now)?;
    if person_key(tx, req.fingerprint)?.is_some() {
        return refuse(Refusal::PersonExists);
    }
    if tx
        .prepare("SELECT 1 FROM person_pending WHERE fingerprint = ?1 OR pending_id = ?2")?
        .exists(params![req.fingerprint, req.pending_id])?
    {
        return refuse(Refusal::EnrollPending);
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM person_pending", [], |r| r.get(0))?;
    if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_PENDING {
        return refuse(Refusal::EnrollFull);
    }
    tx.execute(
        "INSERT INTO person_pending (pending_id, human, kind, public_key, fingerprint, machine_id,
            policy, commitment, ns, nc, requested_ms, expires_ms, attestation_object)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, ?12)",
        params![
            req.pending_id,
            req.human,
            req.kind.as_str(),
            req.public_key,
            req.fingerprint,
            req.machine_id,
            req.policy.as_str(),
            req.commitment,
            req.ns,
            sql_ms(now),
            sql_ms(now.saturating_add(PENDING_MS)),
            req.attestation_object,
        ],
    )?;
    Ok(())
}

/// The device reveals its random value (hex) for a person enrolment.
pub fn person_reveal(
    tx: &Transaction<'_>,
    pending_id: &str,
    machine_id: &str,
    nc: &str,
    now: u64,
) -> Result<PersonPending> {
    drop_expired_pending(tx, now)?;
    let Some(p) = person_pending_by_id(tx, pending_id)? else {
        return refuse(Refusal::EnrollChanged);
    };
    if p.machine_id != machine_id {
        return refuse(Refusal::EnrollChanged);
    }
    match &p.nc {
        Some(old) if same_hex(old, nc) => return Ok(p),
        Some(_) => return refuse(Refusal::EnrollChanged),
        None => {}
    }
    if !opens(&p.commitment, nc) {
        return refuse(Refusal::EnrollChanged);
    }
    let nc = nc.to_ascii_lowercase();
    tx.execute(
        "UPDATE person_pending SET nc = ?1 WHERE pending_id = ?2",
        params![nc, pending_id],
    )?;
    Ok(PersonPending {
        nc: Some(nc.to_string()),
        ..p
    })
}

/// The candidate key's proof over the pending request's challenge (pending
/// id, Ns, Nc's commitment and graph id; #8047), with the device's `Nc`
/// (hex): a person enrolment has no separate reveal (serve-core #8784).
/// The caller checks wire's `reveal_matches` and the proof; this checks
/// again that `nc` opens the row's commitment, then runs `verify` on the
/// stored row (its public key, Ns, commitment and policy), and records
/// `nc` and `proved_ms` together only if both pass, in this same
/// transaction, so the row it checked is the row it marks. The acting
/// device must be the one that asked, and still approved. Proving a row
/// already proved, with the same `Nc` and a proof that verifies, returns
/// the row and records nothing, so a worker whose response was lost can
/// retry (S41, serve-core #8851); a wrong `Nc` or a failed proof is still
/// refused.
pub fn person_prove(
    tx: &Transaction<'_>,
    pending_id: &str,
    machine_id: &str,
    nc: &str,
    now: u64,
    verify: impl FnOnce(&PersonPending) -> bool,
) -> Result<PersonPending> {
    require_approved(tx, machine_id)?;
    drop_expired_pending(tx, now)?;
    let Some(p) = person_pending_by_id(tx, pending_id)? else {
        return refuse(Refusal::NotPending);
    };
    if p.machine_id != machine_id {
        return refuse(Refusal::NotPending);
    }
    if !opens(&p.commitment, nc) || p.nc.as_deref().is_some_and(|old| !same_hex(old, nc)) {
        return refuse(Refusal::CommitmentMismatch);
    }
    let nc = nc.to_ascii_lowercase();
    if !verify(&p) {
        return refuse(Refusal::ProofInvalid);
    }
    if p.proved_ms.is_some() {
        return Ok(p);
    }
    tx.execute(
        "UPDATE person_pending SET nc = ?1, proved_ms = ?2 WHERE pending_id = ?3",
        params![nc, sql_ms(now), pending_id],
    )?;
    Ok(PersonPending {
        nc: Some(nc),
        proved_ms: Some(now),
        ..p
    })
}

/// Why evidence proves nothing either way: the key is machine-grade, and
/// nothing is wrong with it (orchestrator #9151, #9281, #9299). The names
/// are person-keys' `Outcome::Absent` reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentReason {
    /// No attestation object, format "none", or a key kind that has none.
    None,
    /// Self-attestation.
    SelfAttested,
    /// No roots supplied to check the chain against.
    Unanchored,
    /// Backup-eligible or backed up (BE or BS): valid evidence, but the key
    /// can leave the authenticator it describes (#8902, Terra 4 #8994).
    Backup,
    /// A format this build doesn't verify (fido-u2f, tpm, android-key,
    /// apple, any other).
    UnsupportedFormat,
    /// A signature algorithm this build doesn't verify (Terra 2 #9478).
    UnsupportedAlg,
}

impl AbsentReason {
    pub fn as_str(self) -> &'static str {
        match self {
            AbsentReason::None => "none",
            AbsentReason::SelfAttested => "self",
            AbsentReason::Unanchored => "unanchored",
            AbsentReason::Backup => "backup",
            AbsentReason::UnsupportedFormat => "unsupported_format",
            AbsentReason::UnsupportedAlg => "unsupported_alg",
        }
    }
}

/// What approval's attestation check found in a pending row's evidence,
/// person-keys' `Outcome` one to one (orchestrator #9151, #9299). These are
/// judgements of the evidence: `Failed` is evidence shown to be wrong, which
/// the machine-grade acknowledgement can accept. A verifier that can't
/// judge returns `Err`, which propagates as [`ServeError::Verifier`] and is
/// never accepted (Terra 3 #9913, #9999).
///
/// Precedence (orchestrator #9345 and #9455; person-keys owns the order):
/// the format-independent checks run first, and any failure there is
/// `Failed` whatever the format: malformed CBOR or evidence, or an
/// attested key that isn't the pending key. The challenge is checked only
/// where a verified signature covers it (packed); for a format this build
/// doesn't verify, the proof of possession binds the key instead. Only
/// evidence that passes these can be `Absent` or `Verified`, and the first
/// applicable Absent reason wins, in this order: none, self,
/// unsupported_format, unsupported_alg, unanchored, backup (#9491).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationCheck {
    /// The chain verifies against the approver-supplied roots, the attested
    /// key is the pending key whose possession was proved, it is FIDO2, and
    /// neither BE nor BS is set. The only outcome recorded "verified".
    Verified,
    /// No proof either way, from evidence that passed the
    /// format-independent checks: machine-grade, approved only with the
    /// acknowledgement. Without it, refused `api.machine_grade_required`
    /// with this reason (#9400).
    Absent(AbsentReason),
    /// The evidence shows something wrong, for one of a closed set of
    /// reasons.
    Failed(FailedReason),
}

/// Why evidence failed: a closed set of stable words, so the store never
/// records or returns a verifier's free text (the serve edd's S41, Opus
/// #9424; Claude 2 #9621; the words are orchestrator #9633's and #9829's). A verifier
/// error that maps to none of them is the adapter's to refuse; it can't
/// reach the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedReason {
    /// The evidence or its CBOR is unreadable, or `fmt` is missing or empty.
    Malformed,
    /// The authData's RP id hash isn't this graph's.
    RpMismatch,
    /// The authData lacks user presence.
    NoUp,
    /// The authData lacks user verification under the `uv` policy.
    NoUv,
    /// The attested key isn't the pending key whose possession was proved.
    KeyMismatch,
    /// A verified statement's signature fails; for packed this covers a
    /// wrong challenge.
    BadSignature,
    /// A chain that does verify fails against the supplied roots.
    ChainFailed,
    /// A leaf whose signature verifies but that doesn't fit the packed
    /// attestation profile: wrong OU, an AAGUID extension that doesn't match
    /// authData, a leaf that is a CA (#9829). A leaf that is itself one of
    /// the supplied roots is `malformed`.
    Profile,
}

impl FailedReason {
    pub const ALL: [FailedReason; 8] = [
        FailedReason::Malformed,
        FailedReason::RpMismatch,
        FailedReason::NoUp,
        FailedReason::NoUv,
        FailedReason::KeyMismatch,
        FailedReason::BadSignature,
        FailedReason::ChainFailed,
        FailedReason::Profile,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FailedReason::Malformed => "malformed",
            FailedReason::RpMismatch => "rp_mismatch",
            FailedReason::NoUp => "no_up",
            FailedReason::NoUv => "no_uv",
            FailedReason::KeyMismatch => "key_mismatch",
            FailedReason::BadSignature => "bad_signature",
            FailedReason::ChainFailed => "chain_failed",
            FailedReason::Profile => "profile",
        }
    }

    /// The reason a stable word names; `None` for any other text.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// Approval's attestation check over a pending row and its stored
/// evidence: an outcome, or the verifier's own error.
/// The lifetime lets an adapter's closure borrow request-scoped roots
/// (Terra 3 #9999).
pub type VerifyAttestation<'a> =
    dyn Fn(&PersonPending, &[u8]) -> std::result::Result<AttestationCheck, VerifierError> + 'a;

/// The caller is the host's own in-process command, the direct host CLI
/// or its stdio MCP, never `serve`'s listener: the only context in which a
/// keyless host approval is allowed (S41; orchestrator #10206, Terra 7
/// #10200). It can't be built from request data; only
/// [`DirectHost::in_process_host_command`] makes one, and the serve paths
/// must never call it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectHost {
    _private: (),
}

impl DirectHost {
    /// For the in-process host CLI and MCP only.
    pub fn in_process_host_command() -> Self {
        DirectHost { _private: () }
    }
}

/// What `person approve` names: every input of the code the approver
/// compared. The caller checked the typed code against the code computed
/// from these values.
#[derive(Clone)]
pub struct PersonApproval<'a> {
    pub pending_id: &'a str,
    pub human: &'a str,
    pub kind: PersonKind,
    pub fingerprint: &'a str,
    pub machine_id: &'a str,
    pub policy: PersonPolicy,
    /// The approving actor: a `human:*` person for a signed approval; for
    /// a keyless host approval, whatever actor the host command ran as
    /// (orchestrator #10046).
    pub approver: &'a str,
    /// The authenticated writer's machine: the machine the approval came
    /// from, not the candidate's (S22; orchestrator #9717, Terra 2 #9703).
    /// Rechecked as approved in the transaction.
    pub approver_machine: &'a str,
    /// The person key the approver signed with, if any; rechecked as the
    /// approver's live key in the transaction.
    pub approver_person_key: Option<&'a str>,
    /// Set only by the direct host CLI or MCP; a keyless approval without
    /// it is refused (#10206).
    pub direct_host: Option<DirectHost>,
    /// `--allow-file-key`: the explicit opt-in a file key needs.
    pub allow_file_key: bool,
    /// `--machine-grade`: the approver accepts a key that will be
    /// machine-grade. Without it, an Absent or Failed check, or a
    /// non-FIDO2 key, refuses and changes nothing (S41, #9151).
    pub machine_grade_ack: bool,
    /// Checks the row's stored attestation object against the approval's
    /// roots, inside the approval's transaction, so the evidence checked
    /// is the evidence of the row approved (#8984). It must bind the
    /// object to the stored row (its key and enrolment challenge), never
    /// to values in the request. Called only for a FIDO2 row that kept an
    /// object; any other row is Absent.
    /// An `Err` is the verifier's own failure, propagated as
    /// [`ServeError::Verifier`].
    pub verify_attestation: &'a VerifyAttestation<'a>,
}

impl fmt::Debug for PersonApproval<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersonApproval")
            .field("pending_id", &self.pending_id)
            .field("human", &self.human)
            .field("kind", &self.kind)
            .field("fingerprint", &self.fingerprint)
            .field("machine_id", &self.machine_id)
            .field("policy", &self.policy)
            .field("approver", &self.approver)
            .field("approver_machine", &self.approver_machine)
            .field("approver_person_key", &self.approver_person_key)
            .field("direct_host", &self.direct_host)
            .field("allow_file_key", &self.allow_file_key)
            .field("machine_grade_ack", &self.machine_grade_ack)
            .finish_non_exhaustive()
    }
}

/// `person approve`: a compare-and-set on the exact pending request
/// (S41, R130), which the candidate key must have proved (#8047). Writes
/// the key and `person.enroll`, or nothing. A key that would be
/// machine-grade needs `machine_grade_ack`, checked here in the same
/// transaction: Failed evidence is refused `api.attestation_failed` with
/// its reason, Absent `api.machine_grade_required` with its reason (#9151,
/// #9299, #9400). With the acknowledgement it is recorded
/// attestation "none", and the event carries the Absent reason as
/// `machine_grade_reason` or the Failed one as `attestation_failed`; the
/// evidence's claims are never used.
pub fn approve_person(tx: &Transaction<'_>, a: &PersonApproval<'_>, now: u64) -> Result<PersonKey> {
    if !loggable_actor(a.approver) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the approver isn't a valid actor",
        });
    }
    require_approved(tx, a.approver_machine)?;
    match a.approver_person_key {
        Some(key) => {
            if !is_human(a.approver) {
                return refuse(Refusal::ActorNotAllowed {
                    reason: "a signed approval names a human:* person",
                });
            }
            require_person(tx, key, a.approver)?;
        }
        // Keyless only while the graph has no enrolled person, rechecked
        // here, so of two keyless bootstrap approvals only the first
        // commits (S41, Terra 3 #9913).
        None if has_any_person_key(tx)? => return refuse(Refusal::PersonSignatureRequired),
        // And only from the host itself: the in-process host command, on
        // the stored host machine, never a worker or the serve path
        // (orchestrator #10206, Terra 7 #10200).
        None if a.direct_host.is_none() => return refuse(Refusal::PersonSignatureRequired),
        None if !machine(tx, a.approver_machine)?.is_some_and(|m| m.local) => {
            return refuse(Refusal::ActorNotAllowed {
                reason: "a keyless approval comes from the host's own machine",
            });
        }
        None => {}
    }
    if !a.policy.allowed_for(a.kind) {
        return refuse(Refusal::PolicyUnsupported);
    }
    drop_expired_pending(tx, now)?;
    let Some(p) = person_pending_by_id(tx, a.pending_id)? else {
        return refuse(Refusal::EnrollChanged);
    };
    if p.human != a.human
        || p.kind != a.kind.as_str()
        || p.fingerprint != a.fingerprint
        || p.machine_id != a.machine_id
        || p.policy != a.policy.as_str()
        || person_key(tx, &p.fingerprint)?.is_some()
    {
        return refuse(Refusal::EnrollChanged);
    }
    // person_prove records Nc with the proof, so an unproved row may have
    // neither.
    if p.proved_ms.is_none() {
        return refuse(Refusal::PersonUnproved);
    }
    if p.nc.is_none() {
        return refuse(Refusal::EnrollChanged);
    }
    if a.kind == PersonKind::File && !a.allow_file_key {
        return refuse(Refusal::FileKeyNotAllowed);
    }
    // Only a FIDO2 key whose kept attestation is Verified is evidence of
    // presence; every other key is machine-grade (#7692, #8902, #8984,
    // #9151, #9299).
    let check = match &p.attestation_object {
        Some(object) if a.kind == PersonKind::Fido2 => {
            (a.verify_attestation)(&p, object).map_err(ServeError::Verifier)?
        }
        _ => AttestationCheck::Absent(AbsentReason::None),
    };
    // Exactly one is set for a machine-grade approval, neither for Present.
    let (attestation, machine_grade_reason, failure) = match check {
        AttestationCheck::Verified => (Attestation::Verified, None, None),
        AttestationCheck::Failed(reason) if !a.machine_grade_ack => {
            return refuse(Refusal::AttestationFailed { reason });
        }
        AttestationCheck::Absent(reason) if !a.machine_grade_ack => {
            return refuse(Refusal::MachineGradeRequired { reason });
        }
        AttestationCheck::Absent(reason) => (Attestation::None, Some(reason.as_str()), None),
        AttestationCheck::Failed(reason) => (Attestation::None, None, Some(reason.as_str())),
    };
    tx.execute(
        "INSERT INTO persons (key_id, human, kind, public_key, policy, state, enrolled_ms,
            enrolled_by, machine_id, attestation, presence_capable)
         VALUES (?1, ?2, ?3, ?4, ?5, 'approved', ?6, ?7, ?8, ?9, ?10)",
        params![
            p.fingerprint,
            p.human,
            p.kind,
            p.public_key,
            p.policy,
            sql_ms(now),
            a.approver,
            p.machine_id,
            attestation.as_str(),
            // Only a Verified outcome, which only a FIDO2 key reaches.
            i64::from(attestation == Attestation::Verified),
        ],
    )?;
    tx.execute(
        "DELETE FROM person_pending WHERE pending_id = ?1",
        params![a.pending_id],
    )?;
    let key = person_key(tx, &p.fingerprint)?.ok_or(ServeError::Refused(Refusal::PersonUnknown))?;
    record(
        tx,
        a.approver,
        "person.enroll",
        vec![p.human.clone(), format!("person_key:{}", key.key_id)],
        // `human`, `machine` and `person_key` are S22's: the authenticated
        // person, their machine and the key they signed with. The new key,
        // its person and its device are the candidate's (orchestrator
        // #9717, Terra 3 #9913).
        json!({
            // A keyless host approval names its human only if the host
            // command ran as one (orchestrator #10046).
            "human": is_human(a.approver).then_some(a.approver),
            "machine": a.approver_machine,
            "person_key": a.approver_person_key,
            "approval_mode": if a.approver_person_key.is_some() {
                "person_signed"
            } else {
                "keyless_host"
            },
            "candidate_human": p.human,
            "candidate_machine": key.machine_id,
            "candidate_person_key": key.key_id,
            "kind": key.kind,
            "policy": key.policy,
            "attestation": key.attestation,
            // The edd's `capability` field: the stored capability by name,
            // never `present` (S41, orchestrator #9718).
            "capability": key.capability(),
            "machine_grade_reason": machine_grade_reason,
            "attestation_failed": failure,
            "approved_by": a.approver,
        }),
    )?;
    Ok(key)
}

/// `person revoke`: ends a key. Returns whether this call revoked it.
pub fn revoke_person(tx: &Transaction<'_>, key_id: &str, human: &str, now: u64) -> Result<bool> {
    if !is_human(human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "revocation names a human:* person",
        });
    }
    let Some(key) = person_key(tx, key_id)? else {
        return refuse(Refusal::PersonUnknown);
    };
    if key.revoked {
        return Ok(false);
    }
    tx.execute(
        "UPDATE persons SET state = 'revoked', revoked_ms = ?1, revoked_by = ?2 WHERE key_id = ?3",
        params![sql_ms(now), human, key_id],
    )?;
    record(
        tx,
        human,
        "person.revoke",
        vec![key.human.clone(), format!("person_key:{key_id}")],
        json!({"human": key.human, "person_key": key_id, "revoked_by": human}),
    )?;
    Ok(true)
}

// ---------------------------------------------------------------------
// Placement (S12)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementState {
    Placed,
    /// Retirement's first phase committed (R110).
    Retiring,
    Retired,
}

impl PlacementState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlacementState::Placed => "placed",
            PlacementState::Retiring => "retiring",
            PlacementState::Retired => "retired",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "placed" => PlacementState::Placed,
            "retiring" => PlacementState::Retiring,
            _ => PlacementState::Retired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub agent: String,
    /// Empty for an agent retired before it was ever placed.
    pub machine_id: String,
    pub checkout: Option<String>,
    /// `claimed`, `local` or, for a never-placed retirement, `retired`.
    pub source: String,
    pub state: PlacementState,
    pub since_ms: u64,
    pub retired_ms: Option<u64>,
    pub retire_by: Option<String>,
    pub retire_started_ms: Option<u64>,
    /// When the retirement's handoff was sent; `None` while it is due.
    pub handoff_ms: Option<u64>,
    pub handoff_id: Option<String>,
}

const PLACEMENT_COLS: &str = "agent, machine_id, checkout, source, state, since_ms, retired_ms, \
    retire_by, retire_started_ms, handoff_ms, handoff_id";

fn placement_row(r: &Row<'_>) -> rusqlite::Result<Placement> {
    let state: String = r.get(4)?;
    Ok(Placement {
        agent: r.get(0)?,
        machine_id: r.get(1)?,
        checkout: r.get(2)?,
        source: r.get(3)?,
        state: PlacementState::parse(&state),
        since_ms: ms(r.get(5)?),
        retired_ms: opt_ms(r.get(6)?),
        retire_by: r.get(7)?,
        retire_started_ms: opt_ms(r.get(8)?),
        handoff_ms: opt_ms(r.get(9)?),
        handoff_id: r.get(10)?,
    })
}

pub fn placement(conn: &Connection, agent: &str) -> Result<Option<Placement>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PLACEMENT_COLS} FROM placements WHERE agent = ?1"),
            params![agent],
            placement_row,
        )
        .optional()?)
}

fn placed_on(conn: &Connection, machine_id: &str) -> Result<Vec<PlacedAgent>> {
    let mut stmt = conn.prepare(
        "SELECT agent, state FROM placements WHERE machine_id = ?1 AND state != 'retired'
         ORDER BY agent",
    )?;
    let rows = stmt.query_map(params![machine_id], |r| {
        let state: String = r.get(1)?;
        Ok(PlacedAgent {
            agent: r.get(0)?,
            state: PlacementState::parse(&state),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An agent's placement rule, read by the caller from the definition at
/// the graph ref's tip (S39, R87). `None` there means "the host only".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    /// Machine names or ids.
    pub machines: Vec<String>,
    pub groups: Vec<String>,
}

impl Rule {
    fn names(&self, m: &Machine) -> bool {
        self.machines
            .iter()
            .any(|x| *x == m.name || *x == m.machine_id)
            || self.groups.iter().any(|g| m.groups.contains(g))
    }
}

/// A `run:<agent>` claim's placement facts, checked in the claim's own
/// transaction (lane L8, R32, R33).
#[derive(Debug, Clone)]
pub struct PlacementClaim<'a> {
    pub agent: &'a str,
    pub machine_id: &'a str,
    pub rule: Option<&'a Rule>,
    /// The agent's harness, whose report must show it installed and not
    /// logged out (S12's eligibility).
    pub harness: Option<&'a str>,
    pub checkout: Option<&'a str>,
    /// Who claims, for the record.
    pub actor: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// No remote machine is approved: no row, 0.7.0's behaviour (R136).
    Unplaced,
    /// Already placed on this machine.
    Existing(Placement),
    /// Placed by this claim.
    New(Placement),
}

fn harness_ok(m: &Machine, harness: Option<&str>) -> bool {
    let Some(harness) = harness else {
        return true;
    };
    let Some(report) = &m.report else {
        // The host can run without having reported; a remote machine has
        // reported before it claims anything.
        return m.local;
    };
    report
        .get("harnesses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|h| h.get("harness").and_then(Value::as_str) == Some(harness))
        .is_some_and(|h| {
            h.get("installed").and_then(Value::as_bool) == Some(true)
                && h.get("logged_in").and_then(Value::as_bool) != Some(false)
        })
}

/// The placement half of a `run:<agent>` claim (S12): places an unplaced
/// agent on an eligible machine, accepts the placed machine, and refuses
/// every other machine with `run.not_placed`. Placement never moves
/// (R38).
pub fn claim_placement(tx: &Transaction<'_>, c: &PlacementClaim<'_>, now: u64) -> Result<Placed> {
    let m = require_approved(tx, c.machine_id)?;
    if let Some(p) = placement(tx, c.agent)? {
        return match p.state {
            PlacementState::Retiring | PlacementState::Retired => refuse(Refusal::Retired),
            PlacementState::Placed if p.machine_id != c.machine_id => refuse(Refusal::NotPlaced {
                placed_on: Some(p.machine_id),
            }),
            PlacementState::Placed => Ok(Placed::Existing(p)),
        };
    }
    if !has_remote_machine(tx)? {
        return if m.local {
            Ok(Placed::Unplaced)
        } else {
            refuse(Refusal::NotPlaced { placed_on: None })
        };
    }
    let eligible = match c.rule {
        None => m.local,
        Some(rule) => rule.names(&m),
    } && harness_ok(&m, c.harness);
    if !eligible {
        return refuse(Refusal::NotPlaced { placed_on: None });
    }
    let source = if m.local && c.rule.is_none() {
        "local"
    } else {
        "claimed"
    };
    tx.execute(
        "INSERT INTO placements (agent, machine_id, checkout, source, state, since_ms)
         VALUES (?1, ?2, ?3, ?4, 'placed', ?5)",
        params![c.agent, c.machine_id, c.checkout, source, sql_ms(now)],
    )?;
    record(
        tx,
        c.actor,
        "placement.claim",
        vec![
            format!("agent:{}", c.agent),
            format!("machine:{}", c.machine_id),
        ],
        json!({"agent": c.agent, "machine": c.machine_id, "checkout": c.checkout, "source": source}),
    )?;
    let p = placement(tx, c.agent)?.ok_or(ServeError::Refused(Refusal::Retired))?;
    Ok(Placed::New(p))
}

/// Refuses a remote request naming `agent` unless it comes from the
/// agent's placed machine and the agent isn't retiring or retired (R74).
/// An unplaced agent passes only on the host while no remote machine is
/// approved.
pub fn require_placed_here(
    conn: &Connection,
    agent: &str,
    machine_id: &str,
) -> Result<Option<Placement>> {
    match placement(conn, agent)? {
        Some(p) if p.state != PlacementState::Placed => refuse(Refusal::Retired),
        Some(p) if p.machine_id != machine_id => refuse(Refusal::NotPlaced {
            placed_on: Some(p.machine_id),
        }),
        Some(p) => Ok(Some(p)),
        None => {
            let local = machine(conn, machine_id)?.is_some_and(|m| m.local);
            if local && !has_remote_machine(conn)? {
                Ok(None)
            } else {
                refuse(Refusal::NotPlaced { placed_on: None })
            }
        }
    }
}

/// Retirement's first phase (S12, R110): marks the placement `retiring`
/// and records who asked, so a rerun resumes. An agent never placed gets
/// a `retiring` row with no machine, so its name can't be claimed again.
/// Returns the placement; its `machine_id`, when not empty, is the machine
/// whose git fence the caller takes before [`finish_retire`].
pub fn begin_retire(tx: &Transaction<'_>, agent: &str, human: &str, now: u64) -> Result<Placement> {
    if !is_human(human) {
        return refuse(Refusal::ActorNotAllowed {
            reason: "retirement names a human:* person",
        });
    }
    match placement(tx, agent)? {
        Some(p) if p.state != PlacementState::Placed => return Ok(p),
        Some(_) => {
            tx.execute(
                "UPDATE placements SET state = 'retiring', retire_by = ?1, retire_started_ms = ?2
                 WHERE agent = ?3",
                params![human, sql_ms(now), agent],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO placements (agent, machine_id, source, state, since_ms, retire_by,
                    retire_started_ms)
                 VALUES (?1, '', 'retired', 'retiring', ?2, ?3, ?2)",
                params![agent, sql_ms(now), human],
            )?;
        }
    }
    let p = placement(tx, agent)?.ok_or(ServeError::Refused(Refusal::Retired))?;
    record(
        tx,
        human,
        "placement.retiring",
        vec![format!("agent:{agent}")],
        json!({"agent": agent, "machine": p.machine_id, "human": human}),
    )?;
    Ok(p)
}

/// Retirement's commit, under the machine's git fence: marks the
/// placement `retired` and writes `placement.release` naming the person
/// who began it. The handoff is then due until [`mark_handoff`] records
/// it, so a crash after this commit can't lose it (Terra 2's retirement
/// finding). The `agent.define` with `retired: true` is the caller's graph
/// publish.
pub fn finish_retire(tx: &Transaction<'_>, agent: &str, now: u64) -> Result<Placement> {
    let Some(p) = placement(tx, agent)? else {
        return refuse(Refusal::NotPlaced { placed_on: None });
    };
    match p.state {
        PlacementState::Retired => Ok(p),
        PlacementState::Placed => refuse(Refusal::ActorNotAllowed {
            reason: "retirement's first phase hasn't committed",
        }),
        PlacementState::Retiring => {
            tx.execute(
                "UPDATE placements SET state = 'retired', retired_ms = ?1 WHERE agent = ?2",
                params![sql_ms(now), agent],
            )?;
            let human = p.retire_by.clone().unwrap_or_else(|| SERVE_ACTOR.into());
            record(
                tx,
                &human,
                "placement.release",
                vec![format!("agent:{agent}")],
                json!({"agent": agent, "machine": p.machine_id, "human": human}),
            )?;
            placement(tx, agent)?.ok_or(ServeError::Refused(Refusal::Retired))
        }
    }
}

/// Retired agents whose handoff hasn't been sent.
pub fn handoffs_due(conn: &Connection) -> Result<Vec<Placement>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLACEMENT_COLS} FROM placements
         WHERE state = 'retired' AND handoff_ms IS NULL ORDER BY retired_ms, agent"
    ))?;
    let rows = stmt.query_map([], placement_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records the handoff message, in the transaction that sends it.
pub fn mark_handoff(tx: &Transaction<'_>, agent: &str, message_id: &str, now: u64) -> Result<()> {
    tx.execute(
        "UPDATE placements SET handoff_ms = ?1, handoff_id = ?2
         WHERE agent = ?3 AND state = 'retired' AND handoff_ms IS NULL",
        params![sql_ms(now), message_id, agent],
    )?;
    Ok(())
}

/// Retirements whose first phase is older than `older_than_ms` and that
/// haven't committed.
pub fn stalled_retirements(
    conn: &Connection,
    now: u64,
    older_than_ms: u64,
) -> Result<Vec<Placement>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PLACEMENT_COLS} FROM placements
         WHERE state = 'retiring' AND retire_started_ms <= ?1 ORDER BY retire_started_ms"
    ))?;
    let rows = stmt.query_map(
        params![sql_ms(now.saturating_sub(older_than_ms))],
        placement_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---------------------------------------------------------------------
// Supervisors (S11, S27)

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorAgent {
    pub name: String,
    /// Registered only to close turns admitted before the agent was
    /// retired: it may finish those turns, and admit nothing (Terra 2's
    /// recovery-registration finding).
    pub recovery_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supervisor {
    pub supervisor_id: String,
    pub machine_id: String,
    pub pid: Option<u32>,
    pub build: Option<String>,
    pub started_ms: u64,
    pub heartbeat_ms: u64,
    /// `running`, `stale` or `stopped`.
    pub state: String,
    pub agents: Vec<SupervisorAgent>,
    /// Agents with a pending stop request.
    pub stop: Vec<String>,
}

const SUPERVISOR_COLS: &str =
    "supervisor_id, machine_id, pid, build, started_ms, heartbeat_ms, state, agents, control";

fn supervisor_row(r: &Row<'_>) -> rusqlite::Result<Supervisor> {
    let agents: String = r.get(7)?;
    let control: Option<String> = r.get(8)?;
    let agents: Vec<Value> = serde_json::from_str(&agents).unwrap_or_default();
    Ok(Supervisor {
        supervisor_id: r.get(0)?,
        machine_id: r.get(1)?,
        pid: r
            .get::<_, Option<i64>>(2)?
            .and_then(|p| u32::try_from(p).ok()),
        build: r.get(3)?,
        started_ms: ms(r.get(4)?),
        heartbeat_ms: ms(r.get(5)?),
        state: r.get(6)?,
        agents: agents
            .iter()
            .filter_map(|a| {
                Some(SupervisorAgent {
                    name: a.get("name")?.as_str()?.to_string(),
                    recovery_only: a.get("recovery_only").and_then(Value::as_bool) == Some(true),
                })
            })
            .collect(),
        stop: control.map(|c| strings(&c)).unwrap_or_default(),
    })
}

pub fn supervisor(conn: &Connection, supervisor_id: &str) -> Result<Option<Supervisor>> {
    Ok(conn
        .query_row(
            &format!("SELECT {SUPERVISOR_COLS} FROM supervisors WHERE supervisor_id = ?1"),
            params![supervisor_id],
            supervisor_row,
        )
        .optional()?)
}

pub fn supervisors_on(conn: &Connection, machine_id: &str) -> Result<Vec<Supervisor>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SUPERVISOR_COLS} FROM supervisors WHERE machine_id = ?1 ORDER BY started_ms"
    ))?;
    let rows = stmt.query_map(params![machine_id], supervisor_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn has_open_turn_on(conn: &Connection, agent: &str, machine_id: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM turns WHERE agent = ?1 AND machine_id = ?2 AND end_seq IS NULL")?
        .exists(params![agent, machine_id])?)
}

#[derive(Debug, Clone)]
pub struct SupervisorStart<'a> {
    pub supervisor_id: &'a str,
    pub machine_id: &'a str,
    pub pid: Option<u32>,
    pub build: Option<&'a str>,
    pub agents: &'a [String],
}

/// `supervisor.start` (S27): registers a supervisor for agents placed on
/// its machine, or not yet placed. A retired or retiring agent is
/// registered recovery-only, and only while it has a turn with no end on
/// this machine. A resend with the same id from the same machine returns
/// the row.
pub fn start_supervisor(
    tx: &Transaction<'_>,
    s: &SupervisorStart<'_>,
    now: u64,
) -> Result<Supervisor> {
    let m = require_approved(tx, s.machine_id)?;
    if let Some(existing) = supervisor(tx, s.supervisor_id)? {
        if existing.machine_id != s.machine_id {
            return refuse(Refusal::ActorNotAllowed {
                reason: "the supervisor id belongs to another machine",
            });
        }
        return Ok(existing);
    }
    let mut agents = Vec::new();
    for name in s.agents {
        let recovery_only = match placement(tx, name)? {
            Some(p) if !p.machine_id.is_empty() && p.machine_id != s.machine_id => {
                return refuse(Refusal::NotPlaced {
                    placed_on: Some(p.machine_id),
                });
            }
            Some(p) if p.state != PlacementState::Placed => {
                if !has_open_turn_on(tx, name, s.machine_id)? {
                    return refuse(Refusal::Retired);
                }
                true
            }
            _ => false,
        };
        agents.push(json!({"name": name, "recovery_only": recovery_only}));
    }
    tx.execute(
        "INSERT INTO supervisors (supervisor_id, machine_id, pid, build, started_ms, heartbeat_ms,
            state, agents, control)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'running', ?6, NULL)",
        params![
            s.supervisor_id,
            s.machine_id,
            s.pid.map(i64::from),
            s.build,
            sql_ms(now),
            serde_json::to_string(&agents)?,
        ],
    )?;
    record(
        tx,
        &m.launcher,
        "supervisor.start",
        vec![
            format!("supervisor:{}", s.supervisor_id),
            format!("machine:{}", s.machine_id),
        ],
        json!({
            "supervisor_id": s.supervisor_id,
            "machine": s.machine_id,
            "pid": s.pid,
            "build": s.build,
            "agents": agents,
        }),
    )?;
    supervisor(tx, s.supervisor_id)?.ok_or(ServeError::Refused(Refusal::ActorNotAllowed {
        reason: "the supervisor wasn't stored",
    }))
}

/// What a heartbeat's response carries (S11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Control {
    pub stop: Vec<String>,
    /// Its agents that are retiring or retired.
    pub retired: Vec<String>,
}

/// The control a supervisor must act on, read without writing.
pub fn control(conn: &Connection, supervisor_id: &str) -> Result<Control> {
    let Some(s) = supervisor(conn, supervisor_id)? else {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the supervisor isn't registered",
        });
    };
    let mut retired = Vec::new();
    for a in &s.agents {
        if placement(conn, &a.name)?.is_some_and(|p| p.state != PlacementState::Placed) {
            retired.push(a.name.clone());
        }
    }
    Ok(Control {
        stop: s.stop,
        retired,
    })
}

/// A heartbeat whose content changed, or a stale supervisor's first:
/// stores it, and writes `supervisor.resume` if the supervisor was stale.
pub fn heartbeat(
    tx: &Transaction<'_>,
    supervisor_id: &str,
    machine_id: &str,
    now: u64,
) -> Result<Control> {
    let m = require_approved(tx, machine_id)?;
    let Some(s) = supervisor(tx, supervisor_id)? else {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the supervisor isn't registered",
        });
    };
    if s.machine_id != machine_id || s.state == "stopped" {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the supervisor isn't running on this machine",
        });
    }
    tx.execute(
        "UPDATE supervisors SET heartbeat_ms = MAX(heartbeat_ms, ?1), state = 'running'
         WHERE supervisor_id = ?2",
        params![sql_ms(now), supervisor_id],
    )?;
    if s.state == "stale" {
        record(
            tx,
            &m.launcher,
            "supervisor.resume",
            vec![format!("supervisor:{supervisor_id}")],
            json!({"supervisor_id": supervisor_id, "machine": machine_id}),
        )?;
    }
    control(tx, supervisor_id)
}

/// The 60-second timer's write of each supervisor's last arrival.
pub fn set_heartbeats(tx: &Transaction<'_>, beats: &[(String, u64)]) -> Result<()> {
    let mut stmt = tx.prepare(
        "UPDATE supervisors SET heartbeat_ms = MAX(heartbeat_ms, ?1)
         WHERE supervisor_id = ?2 AND state != 'stopped'",
    )?;
    for (id, at) in beats {
        stmt.execute(params![sql_ms(*at), id])?;
    }
    Ok(())
}

/// Marks a running supervisor stale (the timer, S13). Returns whether it
/// changed; the caller raises `run.supervisor_exited` in this transaction.
pub fn mark_stale(tx: &Transaction<'_>, supervisor_id: &str) -> Result<bool> {
    Ok(tx.execute(
        "UPDATE supervisors SET state = 'stale' WHERE supervisor_id = ?1 AND state = 'running'",
        params![supervisor_id],
    )? > 0)
}

/// A drain request for one of a supervisor's agents (A18).
pub fn request_stop(tx: &Transaction<'_>, supervisor_id: &str, agent: &str) -> Result<()> {
    let Some(s) = supervisor(tx, supervisor_id)? else {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the supervisor isn't registered",
        });
    };
    let mut stop: BTreeSet<String> = s.stop.into_iter().collect();
    stop.insert(agent.to_string());
    tx.execute(
        "UPDATE supervisors SET control = ?1 WHERE supervisor_id = ?2",
        params![
            serde_json::to_string(&stop.into_iter().collect::<Vec<_>>())?,
            supervisor_id
        ],
    )?;
    Ok(())
}

/// `supervisor.stop`.
pub fn stop_supervisor(
    tx: &Transaction<'_>,
    supervisor_id: &str,
    machine_id: &str,
) -> Result<bool> {
    let Some(m) = machine(tx, machine_id)? else {
        return refuse(Refusal::MachineUnknown);
    };
    let changed = tx.execute(
        "UPDATE supervisors SET state = 'stopped'
         WHERE supervisor_id = ?1 AND machine_id = ?2 AND state != 'stopped'",
        params![supervisor_id, machine_id],
    )? > 0;
    if changed {
        record(
            tx,
            &m.launcher,
            "supervisor.stop",
            vec![format!("supervisor:{supervisor_id}")],
            json!({"supervisor_id": supervisor_id, "machine": machine_id}),
        )?;
    }
    Ok(changed)
}

/// Whether `supervisor_id` is a live supervisor on `machine_id`
/// registered for `agent`: the binding a turn's recovery steps need
/// (S10, R121). Live means not stopped, with a stored heartbeat within
/// [`FRESH_HEARTBEAT_MS`].
pub fn registered_for(
    conn: &Connection,
    supervisor_id: &str,
    machine_id: &str,
    agent: &str,
    now: u64,
) -> Result<Option<SupervisorAgent>> {
    let Some(s) = supervisor(conn, supervisor_id)? else {
        return Ok(None);
    };
    if s.machine_id != machine_id
        || s.state == "stopped"
        || now.saturating_sub(s.heartbeat_ms) > FRESH_HEARTBEAT_MS
    {
        return Ok(None);
    }
    Ok(s.agents.into_iter().find(|a| a.name == agent))
}

// ---------------------------------------------------------------------
// Turns and their provenance (S27, S29)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Digest,
    Batch,
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Digest => "digest",
            Shape::Batch => "batch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateKind {
    Node,
    Entry,
    Message,
    /// A replay fingerprint of a message body: `ref` is
    /// `<message id> <fingerprint>`.
    Fingerprint,
    /// A message body past a fingerprint cap: `ref` is the message id. A
    /// block from a turn shown it is refused whole (R123).
    Capped,
}

impl CandidateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Node => "node",
            CandidateKind::Entry => "entry",
            CandidateKind::Message => "message",
            CandidateKind::Fingerprint => "fingerprint",
            CandidateKind::Capped => "capped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(CandidateKind::Node),
            "entry" => Some(CandidateKind::Entry),
            "message" => Some(CandidateKind::Message),
            "fingerprint" => Some(CandidateKind::Fingerprint),
            "capped" => Some(CandidateKind::Capped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub reference: String,
    /// For a shown message: `body` or `header`.
    pub flag: Option<String>,
}

impl Candidate {
    pub fn new(kind: CandidateKind, reference: impl Into<String>) -> Self {
        Self {
            kind,
            reference: reference.into(),
            flag: None,
        }
    }

    /// A fingerprint of `message`'s body.
    pub fn fingerprint(message: &str, fingerprint: &str) -> Self {
        Self::new(
            CandidateKind::Fingerprint,
            format!("{message} {fingerprint}"),
        )
    }

    fn message_of(&self) -> Option<&str> {
        match self.kind {
            CandidateKind::Fingerprint => self.reference.split(' ').next(),
            CandidateKind::Capped => Some(&self.reference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub turn_id: String,
    pub agent: String,
    pub number: u64,
    pub machine_id: String,
    pub supervisor_id: String,
    pub ended_by: Option<String>,
    pub shape: String,
    pub nonce_digest: String,
    pub start_seq: u64,
    pub shown_seq: Option<u64>,
    pub end_seq: Option<u64>,
    pub outcome: Option<String>,
    pub ended_ms: Option<u64>,
}

impl Turn {
    /// A turn is running while it has no end, `lost` included (R114).
    pub fn running(&self) -> bool {
        self.end_seq.is_none()
    }

    /// Whether `nonce` is the one the turn was admitted with.
    pub fn nonce_matches(&self, nonce: &str) -> bool {
        Digest::of_bytes(nonce.as_bytes()).to_string() == self.nonce_digest
    }
}

const TURN_COLS: &str = "turn_id, agent, number, machine_id, supervisor_id, ended_by, shape, \
    nonce_digest, start_seq, shown_seq, end_seq, outcome, ended_ms";

fn turn_row(r: &Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        turn_id: r.get(0)?,
        agent: r.get(1)?,
        number: ms(r.get(2)?),
        machine_id: r.get(3)?,
        supervisor_id: r.get(4)?,
        ended_by: r.get(5)?,
        shape: r.get(6)?,
        nonce_digest: r.get(7)?,
        start_seq: ms(r.get(8)?),
        shown_seq: opt_ms(r.get(9)?),
        end_seq: opt_ms(r.get(10)?),
        outcome: r.get(11)?,
        ended_ms: opt_ms(r.get(12)?),
    })
}

pub fn turn(conn: &Connection, turn_id: &str) -> Result<Option<Turn>> {
    Ok(conn
        .query_row(
            &format!("SELECT {TURN_COLS} FROM turns WHERE turn_id = ?1"),
            params![turn_id],
            turn_row,
        )
        .optional()?)
}

/// An agent's turns with no end, in number order (R114).
pub fn running_turns(conn: &Connection, agent: &str) -> Result<Vec<Turn>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLS} FROM turns WHERE agent = ?1 AND end_seq IS NULL ORDER BY number"
    ))?;
    let rows = stmt.query_map(params![agent], turn_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether a running turn reads `lost`: its machine is lost (S13).
pub fn turn_lost(conn: &Connection, t: &Turn) -> Result<bool> {
    Ok(t.running() && machine(conn, &t.machine_id)?.is_some_and(|m| m.lost_seq.is_some()))
}

fn rows_of(conn: &Connection, table: &str, turn_id: &str) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT kind, ref, flag FROM {table} WHERE turn_id = ?1 ORDER BY kind, ref"
    ))?;
    let rows = stmt.query_map(params![turn_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, reference, flag) = row?;
        if let Some(kind) = CandidateKind::parse(&kind) {
            out.push(Candidate {
                kind,
                reference,
                flag,
            });
        }
    }
    out.sort();
    Ok(out)
}

pub fn candidates(conn: &Connection, turn_id: &str) -> Result<Vec<Candidate>> {
    rows_of(conn, "turn_candidates", turn_id)
}

#[derive(Debug, Clone)]
pub struct Admission<'a> {
    pub turn_id: &'a str,
    pub agent: &'a str,
    pub machine_id: &'a str,
    pub supervisor_id: &'a str,
    pub shape: Shape,
    /// SHA-256 of the turn's nonce, as a [`Digest`]'s text.
    pub nonce_digest: &'a str,
    /// Built by the caller from the coordinator's state, with every
    /// candidate body's fingerprints already computed (a-flow P5's
    /// function).
    pub candidates: &'a [Candidate],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub turn: Turn,
    pub candidates: Vec<Candidate>,
    pub duplicate: bool,
}

/// `turn.admit` (S29): idempotent by turn id for good. A resend checks
/// only the machine and returns the stored turn and candidates (R92,
/// R134). A new admission needs the agent placed on this machine and
/// not retiring, a supervisor there registered for it (not
/// recovery-only), and at least one candidate; with none it writes
/// nothing (R124). The `run:` lease holder check is the caller's, in
/// this transaction.
pub fn admit(tx: &Transaction<'_>, a: &Admission<'_>, now: u64) -> Result<Admitted> {
    let m = require_approved(tx, a.machine_id)?;
    if let Some(t) = turn(tx, a.turn_id)? {
        if t.machine_id != a.machine_id || t.agent != a.agent {
            return refuse(Refusal::ActorNotAllowed {
                reason: "the turn was admitted for another machine or agent",
            });
        }
        let candidates = candidates(tx, a.turn_id)?;
        return Ok(Admitted {
            turn: t,
            candidates,
            duplicate: true,
        });
    }
    require_placed_here(tx, a.agent, a.machine_id)?;
    match registered_for(tx, a.supervisor_id, a.machine_id, a.agent, now)? {
        Some(r) if !r.recovery_only => {}
        _ => {
            return refuse(Refusal::ActorNotAllowed {
                reason: "no running supervisor on this machine is registered for the agent",
            });
        }
    }
    let messages: BTreeSet<&str> = a
        .candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::Message)
        .map(|c| c.reference.as_str())
        .collect();
    if a.candidates
        .iter()
        .filter_map(Candidate::message_of)
        .any(|msg| !messages.contains(msg))
    {
        return refuse(Refusal::ShownMismatch);
    }
    if !a.candidates.iter().any(|c| {
        matches!(
            c.kind,
            CandidateKind::Node | CandidateKind::Entry | CandidateKind::Message
        )
    }) {
        return refuse(Refusal::NothingToDo);
    }
    let number: i64 = tx.query_row(
        "SELECT COALESCE(MAX(number), 0) + 1 FROM turns WHERE agent = ?1",
        params![a.agent],
        |r| r.get(0),
    )?;
    let seq = record(
        tx,
        a.agent,
        "turn.start",
        vec![format!("agent:{}", a.agent), format!("turn:{}", a.turn_id)],
        json!({
            "turn_id": a.turn_id,
            "number": number,
            "machine": a.machine_id,
            "launcher": m.launcher,
            "supervisor_id": a.supervisor_id,
            "shape": a.shape.as_str(),
            "nonce_digest": a.nonce_digest,
        }),
    )?;
    tx.execute(
        "INSERT INTO turns (turn_id, agent, number, machine_id, supervisor_id, shape, nonce_digest,
            start_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            a.turn_id,
            a.agent,
            number,
            a.machine_id,
            a.supervisor_id,
            a.shape.as_str(),
            a.nonce_digest,
            sql_ms(seq)
        ],
    )?;
    insert_rows(tx, "turn_candidates", a.turn_id, a.candidates)?;
    let t = turn(tx, a.turn_id)?.ok_or(ServeError::Refused(Refusal::TurnUnknown))?;
    Ok(Admitted {
        turn: t,
        candidates: candidates(tx, a.turn_id)?,
        duplicate: false,
    })
}

fn insert_rows(tx: &Transaction<'_>, table: &str, turn_id: &str, rows: &[Candidate]) -> Result<()> {
    let mut stmt = tx.prepare(&format!(
        "INSERT OR IGNORE INTO {table} (turn_id, kind, ref, flag) VALUES (?1, ?2, ?3, ?4)"
    ))?;
    for c in rows {
        stmt.execute(params![turn_id, c.kind.as_str(), c.reference, c.flag])?;
    }
    Ok(())
}

fn running_turn_from(conn: &Connection, turn_id: &str, machine_id: &str) -> Result<Turn> {
    let Some(t) = turn(conn, turn_id)? else {
        return refuse(Refusal::TurnUnknown);
    };
    if t.machine_id != machine_id {
        return refuse(Refusal::ActorNotAllowed {
            reason: "the turn was started on another machine",
        });
    }
    if let Some(outcome) = &t.outcome {
        return refuse(Refusal::TurnEnded {
            outcome: outcome.clone(),
        });
    }
    Ok(t)
}

/// `:shown` (S29): the set the launcher rendered, which must be within
/// the candidates. Messages carry `body` or `header`. The fingerprints and
/// caps of every shown message are copied from the candidates, the
/// header-only ones included, since a turn can read a header-only body
/// later with `msg show` (Terra 2's fingerprint finding). Idempotent by
/// turn id: the same set again is a duplicate, another set is refused.
/// Returns the stored shown set and whether this was a resend.
pub fn shown(
    tx: &Transaction<'_>,
    turn_id: &str,
    machine_id: &str,
    set: &[Candidate],
) -> Result<(Vec<Candidate>, bool)> {
    require_approved(tx, machine_id)?;
    let t = running_turn_from(tx, turn_id, machine_id)?;
    let mut wanted: Vec<Candidate> = set.to_vec();
    wanted.sort();
    wanted.dedup();
    if wanted.iter().any(|c| {
        !matches!(
            c.kind,
            CandidateKind::Node | CandidateKind::Entry | CandidateKind::Message
        ) || (c.kind == CandidateKind::Message
            && !matches!(c.flag.as_deref(), Some("body" | "header")))
            || (c.kind != CandidateKind::Message && c.flag.is_some())
    }) {
        return refuse(Refusal::ShownMismatch);
    }
    if t.shown_seq.is_some() {
        let stored: Vec<Candidate> = shown_rows(tx, turn_id)?
            .into_iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    CandidateKind::Node | CandidateKind::Entry | CandidateKind::Message
                )
            })
            .collect();
        if stored != wanted {
            return refuse(Refusal::ShownMismatch);
        }
        return Ok((shown_rows(tx, turn_id)?, true));
    }
    let cands = candidates(tx, turn_id)?;
    let known: BTreeSet<(CandidateKind, &str)> = cands
        .iter()
        .map(|c| (c.kind, c.reference.as_str()))
        .collect();
    if wanted
        .iter()
        .any(|c| !known.contains(&(c.kind, c.reference.as_str())))
    {
        return refuse(Refusal::ShownMismatch);
    }
    let shown_messages: BTreeSet<&str> = wanted
        .iter()
        .filter(|c| c.kind == CandidateKind::Message)
        .map(|c| c.reference.as_str())
        .collect();
    let mut rows = wanted.clone();
    rows.extend(
        cands
            .iter()
            .filter(|c| c.message_of().is_some_and(|m| shown_messages.contains(m)))
            .cloned(),
    );
    let seq = record(
        tx,
        &t.agent,
        "turn.shown",
        vec![format!("agent:{}", t.agent), format!("turn:{turn_id}")],
        json!({
            "turn_id": turn_id,
            "machine": machine_id,
            "nodes": wanted.iter().filter(|c| c.kind == CandidateKind::Node).count(),
            "entries": wanted.iter().filter(|c| c.kind == CandidateKind::Entry).count(),
            "messages": shown_messages.len(),
        }),
    )?;
    insert_rows(tx, "turn_shown", turn_id, &rows)?;
    tx.execute(
        "UPDATE turns SET shown_seq = ?1 WHERE turn_id = ?2",
        params![sql_ms(seq), turn_id],
    )?;
    Ok((shown_rows(tx, turn_id)?, false))
}

fn shown_rows(conn: &Connection, turn_id: &str) -> Result<Vec<Candidate>> {
    rows_of(conn, "turn_shown", turn_id)
}

/// A body the running turn read after its shown set committed (`msg show`
/// inside the turn): its fingerprints join the turn's shown set, so the
/// import's replay check covers it (Terra 2's fingerprint finding). The
/// caller computes the fingerprints with a-flow P5's function.
pub fn record_body_read(
    tx: &Transaction<'_>,
    turn_id: &str,
    machine_id: &str,
    message_id: &str,
    fingerprints: &[String],
    capped: bool,
) -> Result<()> {
    running_turn_from(tx, turn_id, machine_id)?;
    let mut rows: Vec<Candidate> = fingerprints
        .iter()
        .map(|f| Candidate::fingerprint(message_id, f))
        .collect();
    if capped {
        rows.push(Candidate::new(CandidateKind::Capped, message_id));
    }
    insert_rows(tx, "turn_shown", turn_id, &rows)
}

/// What an import is checked against (S29): the turn's stored shown set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShownSet {
    pub nodes: BTreeSet<String>,
    pub entries: BTreeSet<String>,
    pub messages: BTreeSet<String>,
    pub fingerprints: BTreeSet<String>,
    /// Any shown body past a fingerprint cap: the whole block is refused
    /// (R123).
    pub capped: bool,
}

pub fn shown_set(conn: &Connection, turn_id: &str) -> Result<ShownSet> {
    let mut set = ShownSet::default();
    for c in shown_rows(conn, turn_id)? {
        match c.kind {
            CandidateKind::Node => {
                set.nodes.insert(c.reference);
            }
            CandidateKind::Entry => {
                set.entries.insert(c.reference);
            }
            CandidateKind::Message => {
                set.messages.insert(c.reference);
            }
            CandidateKind::Fingerprint => {
                if let Some((_, f)) = c.reference.split_once(' ') {
                    set.fingerprints.insert(f.to_string());
                }
            }
            CandidateKind::Capped => set.capped = true,
        }
    }
    Ok(set)
}

/// The store's half of `:import`'s checks (S29, S10): the turn exists,
/// has no end, was started on this machine, and its shown set committed;
/// the supervisor is live there and registered for the agent; the nonce
/// opens the turn's digest; and the agent isn't retiring or retired, so
/// a turn pushed before its agent's retirement can't apply its block after
/// it (Terra 2's retired-import finding; see the open question to the
/// orchestrator). Scope, the block parse and the replay check are the
/// caller's, against [`shown_set`].
pub fn check_import(
    conn: &Connection,
    turn_id: &str,
    machine_id: &str,
    supervisor_id: &str,
    nonce: &str,
    now: u64,
) -> Result<(Turn, ShownSet)> {
    require_approved(conn, machine_id)?;
    let t = running_turn_from(conn, turn_id, machine_id)?;
    if registered_for(conn, supervisor_id, machine_id, &t.agent, now)?.is_none() {
        return refuse(Refusal::ActorNotAllowed {
            reason: "no live supervisor on the turn's machine is registered for its agent",
        });
    }
    if t.shown_seq.is_none() {
        return refuse(Refusal::ShownMismatch);
    }
    if !t.nonce_matches(nonce) {
        return refuse(Refusal::NonceMismatch);
    }
    if placement(conn, &t.agent)?.is_some_and(|p| p.state != PlacementState::Placed) {
        return refuse(Refusal::Retired);
    }
    let set = shown_set(conn, turn_id)?;
    Ok((t, set))
}

/// Who ends a turn.
#[derive(Debug, Clone)]
pub enum EndBy<'a> {
    /// A live supervisor on the turn's machine registered for its agent,
    /// recovery-only included (S10, R121).
    Supervisor {
        machine_id: &'a str,
        supervisor_id: &'a str,
    },
    /// `agents resume --abandon-turn` (S13), by a person on the host.
    Operator { human: &'a str },
}

/// `turn.end` (S27): one end per turn, for good. The same outcome again
/// returns the first end as a duplicate; another is refused with
/// `run.turn_ended`. Returns the turn and whether this was a resend.
pub fn end_turn(
    tx: &Transaction<'_>,
    turn_id: &str,
    outcome: &str,
    by: &EndBy<'_>,
    now: u64,
) -> Result<(Turn, bool)> {
    let Some(t) = turn(tx, turn_id)? else {
        return refuse(Refusal::TurnUnknown);
    };
    let (actor, ended_by) = match by {
        EndBy::Supervisor {
            machine_id,
            supervisor_id,
        } => {
            let m = require_approved(tx, machine_id)?;
            if t.machine_id != *machine_id {
                return refuse(Refusal::ActorNotAllowed {
                    reason: "the turn was started on another machine",
                });
            }
            if t.outcome.is_none()
                && registered_for(tx, supervisor_id, machine_id, &t.agent, now)?.is_none()
            {
                return refuse(Refusal::ActorNotAllowed {
                    reason: "no live supervisor on the turn's machine is registered for its agent",
                });
            }
            (m.launcher, (*supervisor_id).to_string())
        }
        EndBy::Operator { human } => {
            if !is_human(human) {
                return refuse(Refusal::ActorNotAllowed {
                    reason: "abandoning a turn names a human:* person",
                });
            }
            ((*human).to_string(), (*human).to_string())
        }
    };
    if let Some(first) = &t.outcome {
        if first == outcome {
            return Ok((t, true));
        }
        return refuse(Refusal::TurnEnded {
            outcome: first.clone(),
        });
    }
    let seq = record(
        tx,
        &actor,
        "turn.end",
        vec![format!("agent:{}", t.agent), format!("turn:{turn_id}")],
        json!({
            "turn_id": turn_id,
            "agent": t.agent,
            "machine": t.machine_id,
            "outcome": outcome,
            "ended_by": ended_by,
        }),
    )?;
    tx.execute(
        "UPDATE turns SET end_seq = ?1, outcome = ?2, ended_by = ?3, ended_ms = ?4
         WHERE turn_id = ?5",
        params![sql_ms(seq), outcome, ended_by, sql_ms(now), turn_id],
    )?;
    let t = turn(tx, turn_id)?.ok_or(ServeError::Refused(Refusal::TurnUnknown))?;
    Ok((t, false))
}

/// Keeps a launcher step's content refusal for good (S10, R91), in a
/// transaction of its own, and returns the code kept: the first one, if
/// the step was refused before.
pub fn keep_refusal(
    tx: &Transaction<'_>,
    turn_id: &str,
    step: &str,
    code: &str,
    now: u64,
) -> Result<String> {
    tx.execute(
        "INSERT OR IGNORE INTO turn_refusals (turn_id, step, code, at_ms) VALUES (?1, ?2, ?3, ?4)",
        params![turn_id, step, code, sql_ms(now)],
    )?;
    Ok(refusal(tx, turn_id, step)?.unwrap_or_else(|| code.to_string()))
}

/// The kept refusal of a turn's step, if any.
pub fn refusal(conn: &Connection, turn_id: &str, step: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT code FROM turn_refusals WHERE turn_id = ?1 AND step = ?2",
            params![turn_id, step],
            |r| r.get(0),
        )
        .optional()?)
}

/// Deletes candidates and shown sets 30 days after their turn's end, and
/// never while it has none (S29, R58). Returns how many rows went.
pub fn prune_provenance(tx: &Transaction<'_>, now: u64) -> Result<usize> {
    let cutoff = sql_ms(now.saturating_sub(PROVENANCE_KEEP_MS));
    let mut n = 0;
    for table in ["turn_candidates", "turn_shown"] {
        n += tx.execute(
            &format!(
                "DELETE FROM {table} WHERE turn_id IN
                    (SELECT turn_id FROM turns WHERE end_seq IS NOT NULL AND ended_ms <= ?1)"
            ),
            params![cutoff],
        )?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::{Store, Upgrade};

    const NOW: u64 = 1_793_000_000_000;

    struct Fixture {
        _dir: std::path::PathBuf,
        store: Store,
    }

    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mdkg-serve-store-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let mut store = Store::open_with(&dir.join("coord.sqlite"), 1_000, Upgrade::Allow).unwrap();
        let tx = store.conn.transaction().unwrap();
        create(&tx).unwrap();
        create(&tx).unwrap();
        tx.commit().unwrap();
        Fixture { _dir: dir, store }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
        }
    }

    impl Fixture {
        /// Runs `f` in a transaction, committing only on success, as a
        /// served write does.
        fn tx<T>(&mut self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
            let tx = self.store.conn.transaction().unwrap();
            let out = f(&tx);
            if out.is_ok() {
                tx.commit().unwrap();
            }
            out
        }

        fn records(&self, kind: &str) -> Vec<Value> {
            let mut stmt = self
                .store
                .conn
                .prepare("SELECT record FROM log ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|t| serde_json::from_str::<Value>(&t.unwrap()).unwrap())
                .filter(|r| r.get("kind").and_then(Value::as_str) == Some(kind))
                .collect()
        }
    }

    fn refused<T: fmt::Debug>(r: Result<T>) -> Refusal {
        match r {
            Err(ServeError::Refused(r)) => r,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    const NC: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn host(f: &mut Fixture) -> Machine {
        f.tx(|tx| {
            approve_offline(
                tx,
                &OfflineApproval {
                    machine_id: "m-host",
                    name: "host",
                    public_key: "pk-host",
                    fingerprint: "fp-host",
                    human: "human:nick",
                    groups: &[],
                    local: true,
                },
                NOW,
            )
        })
        .unwrap()
    }

    fn enrol(f: &mut Fixture, id: &str, name: &str, groups: &[String]) -> Machine {
        let commitment = commitment_of(NC).unwrap();
        let pending_id = format!("p-{id}");
        let fp = format!("fp-{id}");
        f.tx(|tx| {
            enroll(
                tx,
                &NewPending {
                    pending_id: &pending_id,
                    machine_id: id,
                    public_key: "pk",
                    fingerprint: &fp,
                    name,
                    commitment: &commitment,
                    ns: "ns",
                },
                NOW,
            )
        })
        .unwrap();
        f.tx(|tx| reveal(tx, id, NC, NOW)).unwrap();
        f.tx(|tx| {
            approve(
                tx,
                &Approval {
                    machine_id: id,
                    pending_id: &pending_id,
                    fingerprint: &fp,
                    human: "human:nick",
                    groups,
                },
                NOW,
            )
        })
        .unwrap()
    }

    #[test]
    fn enrolment_is_a_compare_and_set_on_the_revealed_request() {
        let mut f = fixture();
        let commitment = commitment_of(NC).unwrap();
        let req = NewPending {
            pending_id: "p1",
            machine_id: "m1",
            public_key: "pk",
            fingerprint: "fp1",
            name: "build-box",
            commitment: &commitment,
            ns: "ns",
        };
        f.tx(|tx| enroll(tx, &req, NOW)).unwrap();
        assert_eq!(
            refused(f.tx(|tx| enroll(tx, &req, NOW))),
            Refusal::EnrollPending
        );
        // Approval before the reveal, or naming another request, writes
        // nothing.
        let approval = Approval {
            machine_id: "m1",
            pending_id: "p1",
            fingerprint: "fp1",
            human: "human:nick",
            groups: &[],
        };
        assert_eq!(
            refused(f.tx(|tx| approve(tx, &approval, NOW))),
            Refusal::EnrollChanged
        );
        assert_eq!(
            refused(f.tx(|tx| reveal(tx, "m1", &NC.replace('0', "1"), NOW))),
            Refusal::EnrollChanged
        );
        // Nc is stored lowercase and compared as bytes (Terra 4 #8861).
        let p = f
            .tx(|tx| reveal(tx, "m1", &NC.to_uppercase(), NOW))
            .unwrap();
        assert_eq!(p.nc.as_deref(), Some(NC));
        f.tx(|tx| reveal(tx, "m1", NC, NOW)).unwrap();
        let wrong_fp = Approval {
            fingerprint: "fp-other",
            ..approval.clone()
        };
        assert_eq!(
            refused(f.tx(|tx| approve(tx, &wrong_fp, NOW))),
            Refusal::EnrollChanged
        );
        assert!(machine(&f.store.conn, "m1").unwrap().is_none());
        let m = f.tx(|tx| approve(tx, &approval, NOW)).unwrap();
        assert_eq!(m.launcher, "launcher:build-box");
        assert_eq!(m.state, MachineState::Approved);
        assert!(pending(&f.store.conn, NOW).unwrap().is_empty());
        // A known id never enrols again (R78), in any state.
        assert_eq!(
            refused(f.tx(|tx| enroll(tx, &req, NOW))),
            Refusal::EnrollExists
        );
        let records = f.records("machine.enroll");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["actor"], "human:nick");
        assert_eq!(records[0]["body"]["machine"], "m1");
    }

    #[test]
    fn names_are_actor_syntax_and_never_reused() {
        let mut f = fixture();
        let commitment = commitment_of(NC).unwrap();
        let bad = NewPending {
            pending_id: "p1",
            machine_id: "m1",
            public_key: "pk",
            fingerprint: "fp",
            name: "Build Box",
            commitment: &commitment,
            ns: "ns",
        };
        assert_eq!(refused(f.tx(|tx| enroll(tx, &bad, NOW))), Refusal::BadName);
        enrol(&mut f, "m1", "build-box", &[]);
        f.tx(|tx| begin_revoke(tx, "m1", "human:nick", NOW))
            .unwrap();
        f.tx(|tx| finish_revoke(tx, "m1")).unwrap();
        // A revoked machine's name stays its own, so `launcher:build-box`
        // names one machine for good.
        let again = NewPending {
            pending_id: "p2",
            machine_id: "m2",
            name: "build-box",
            fingerprint: "fp2",
            ..bad
        };
        f.tx(|tx| enroll(tx, &again, NOW)).unwrap();
        assert_eq!(
            pending(&f.store.conn, NOW).unwrap()[0]
                .name_taken
                .as_deref(),
            Some("m1")
        );
        f.tx(|tx| reveal(tx, "m2", NC, NOW)).unwrap();
        let r = refused(f.tx(|tx| {
            approve(
                tx,
                &Approval {
                    machine_id: "m2",
                    pending_id: "p2",
                    fingerprint: "fp2",
                    human: "human:nick",
                    groups: &[],
                },
                NOW,
            )
        }));
        assert_eq!(
            r,
            Refusal::NameTaken {
                machine: "m1".into()
            }
        );
    }

    #[test]
    fn pending_requests_expire_and_are_capped() {
        let mut f = fixture();
        let commitment = commitment_of(NC).unwrap();
        for i in 0..MAX_PENDING {
            let (p, m) = (format!("p{i}"), format!("m{i}"));
            f.tx(|tx| {
                enroll(
                    tx,
                    &NewPending {
                        pending_id: &p,
                        machine_id: &m,
                        public_key: "pk",
                        fingerprint: "fp",
                        name: "w",
                        commitment: &commitment,
                        ns: "ns",
                    },
                    NOW,
                )
            })
            .unwrap();
        }
        let one_more = NewPending {
            pending_id: "px",
            machine_id: "mx",
            public_key: "pk",
            fingerprint: "fp",
            name: "w",
            commitment: &commitment,
            ns: "ns",
        };
        assert_eq!(
            refused(f.tx(|tx| enroll(tx, &one_more, NOW))),
            Refusal::EnrollFull
        );
        f.tx(|tx| enroll(tx, &one_more, NOW + PENDING_MS)).unwrap();
        assert_eq!(pending(&f.store.conn, NOW + PENDING_MS).unwrap().len(), 1);
        assert!(f.tx(|tx| reject(tx, "mx")).unwrap());
    }

    #[test]
    fn revocation_is_two_phase_resumable_and_rechecked_in_the_transaction() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "build-box", &["macs".into()]);
        f.tx(|tx| {
            claim_placement(
                tx,
                &PlacementClaim {
                    agent: "lane-a",
                    machine_id: "m1",
                    rule: Some(&Rule {
                        machines: vec![],
                        groups: vec!["macs".into()],
                    }),
                    harness: None,
                    checkout: None,
                    actor: "launcher:build-box",
                },
                NOW,
            )
        })
        .unwrap();
        assert!(require_approved(&f.store.conn, "m1").is_ok());
        let r = f
            .tx(|tx| begin_revoke(tx, "m1", "human:nick", NOW))
            .unwrap();
        assert!(matches!(r, Revocation::Revoking(_)));
        // From phase 1, every write from the machine is refused.
        assert_eq!(
            refused(require_approved(&f.store.conn, "m1")),
            Refusal::MachineRevoked
        );
        // A crash here leaves it revoking; a rerun resumes, keeping the
        // first person, and `doctor` sees it stalled.
        let again = f
            .tx(|tx| begin_revoke(tx, "m1", "human:other", NOW + 5))
            .unwrap();
        let Revocation::Revoking(m) = again else {
            panic!("expected revoking");
        };
        assert_eq!(m.revoke_by.as_deref(), Some("human:nick"));
        assert_eq!(
            stalled_revocations(&f.store.conn, NOW + 120_000, 60_000)
                .unwrap()
                .len(),
            1
        );
        let (agents, did) = f.tx(|tx| finish_revoke(tx, "m1")).unwrap();
        assert!(did);
        assert_eq!(agents[0].agent, "lane-a");
        let (_, did) = f.tx(|tx| finish_revoke(tx, "m1")).unwrap();
        assert!(!did);
        assert_eq!(f.records("machine.revoke")[0]["actor"], "human:nick");
        assert_eq!(f.records("machine.revoking").len(), 1);
        assert!(matches!(
            f.tx(|tx| begin_revoke(tx, "m1", "human:nick", NOW))
                .unwrap(),
            Revocation::Revoked(_)
        ));
    }

    fn person_req<'a>(commitment: &'a str) -> NewPersonPending<'a> {
        NewPersonPending {
            pending_id: "pp1",
            human: "human:nick",
            kind: PersonKind::Fido2,
            public_key: "ppk",
            fingerprint: "pfp",
            machine_id: "m1",
            policy: PersonPolicy::Up,
            commitment,
            ns: "ns",
            attestation_object: None,
        }
    }

    const ATT: &[u8] = b"attestation-object";

    type Checked = std::result::Result<AttestationCheck, VerifierError>;

    fn unverified(_: &PersonPending, _: &[u8]) -> Checked {
        Ok(AttestationCheck::Failed(FailedReason::ChainFailed))
    }

    fn verified(p: &PersonPending, object: &[u8]) -> Checked {
        Ok(if object == ATT && p.public_key == "ppk" {
            AttestationCheck::Verified
        } else {
            AttestationCheck::Failed(FailedReason::KeyMismatch)
        })
    }

    fn verified_backup_eligible(p: &PersonPending, object: &[u8]) -> Checked {
        Ok(match verified(p, object)? {
            AttestationCheck::Verified => AttestationCheck::Absent(AbsentReason::Backup),
            other => other,
        })
    }

    fn person_approval() -> PersonApproval<'static> {
        PersonApproval {
            pending_id: "pp1",
            human: "human:nick",
            kind: PersonKind::Fido2,
            fingerprint: "pfp",
            machine_id: "m1",
            policy: PersonPolicy::Up,
            approver: "human:nick",
            approver_machine: "m-host",
            approver_person_key: None,
            direct_host: Some(DirectHost::in_process_host_command()),
            allow_file_key: false,
            // Most tests approve machine-grade keys; the acknowledgement's
            // own tests turn it off.
            machine_grade_ack: true,
            verify_attestation: &unverified,
        }
    }

    #[test]
    fn person_approval_binds_every_code_input_and_rechecks_in_the_transaction() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
            .unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        // A relay that swapped the person, the key, the device or the policy
        // is refused, and writes nothing.
        for swapped in [
            PersonApproval {
                human: "human:mallory",
                ..person_approval()
            },
            PersonApproval {
                fingerprint: "other",
                ..person_approval()
            },
            PersonApproval {
                machine_id: "m2",
                ..person_approval()
            },
            PersonApproval {
                policy: PersonPolicy::Uv,
                ..person_approval()
            },
            PersonApproval {
                kind: PersonKind::Enclave,
                ..person_approval()
            },
        ] {
            assert_eq!(
                refused(f.tx(|tx| approve_person(tx, &swapped, NOW))),
                Refusal::EnrollChanged
            );
        }
        assert!(person_key(&f.store.conn, "pfp").unwrap().is_none());
        let key = f
            .tx(|tx| approve_person(tx, &person_approval(), NOW))
            .unwrap();
        // No verified attestation: it proves the device, not the person.
        assert!(!key.presence_capable);
        assert!(has_person_key(&f.store.conn, "human:nick").unwrap());
        assert!(require_person(&f.store.conn, "pfp", "human:nick").is_ok());
        assert!(matches!(
            refused(require_person(&f.store.conn, "pfp", "human:other")),
            Refusal::ActorNotAllowed { .. }
        ));
        // A revoke between transport and the write's transaction refuses
        // the write.
        assert!(
            f.tx(|tx| revoke_person(tx, "pfp", "human:nick", NOW))
                .unwrap()
        );
        assert_eq!(
            refused(require_person(&f.store.conn, "pfp", "human:nick")),
            Refusal::PersonUnknown
        );
        assert_eq!(
            f.records("person.enroll")[0]["body"]["capability"],
            "machine-grade"
        );
        assert_eq!(f.records("person.revoke").len(), 1);
    }

    #[test]
    fn file_keys_need_the_opt_in_and_never_prove_presence() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            kind: PersonKind::File,
            policy: PersonPolicy::Up,
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            kind: PersonKind::File,
            policy: PersonPolicy::Up,
            verify_attestation: &verified,
            ..person_approval()
        };
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::FileKeyNotAllowed
        );
        let a = PersonApproval {
            allow_file_key: true,
            ..a
        };
        // The file-key opt-in doesn't stand in for the machine-grade
        // acknowledgement (Terra 6 #9359).
        let a = PersonApproval {
            allow_file_key: true,
            machine_grade_ack: false,
            ..a
        };
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::MachineGradeRequired {
                reason: AbsentReason::None
            }
        );
        assert_eq!(person_pending(&f.store.conn, NOW).unwrap().len(), 1);
        assert!(person_key(&f.store.conn, "pfp").unwrap().is_none());
        assert!(f.records("person.enroll").is_empty());
        let a = PersonApproval {
            machine_grade_ack: true,
            ..a
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert!(!key.presence_capable);
        assert!(!key.presence_capable);
    }

    /// A stand-in for person-keys' evaluation order (#9345, #9455, #9491):
    /// the attested key first, for every format; the challenge only inside
    /// packed's signed data; then the format. `object` is "<fmt>|<key>",
    /// or "packed|<key>|<signed challenge>": an unverified format carries
    /// no challenge the store could read.
    fn by_precedence(p: &PersonPending, object: &[u8]) -> Checked {
        let text = std::str::from_utf8(object).unwrap_or("");
        let mut parts = text.splitn(3, '|');
        let (Some(fmt), Some(key)) = (parts.next(), parts.next()) else {
            return Ok(AttestationCheck::Failed(FailedReason::Malformed));
        };
        if key != p.public_key {
            return Ok(AttestationCheck::Failed(FailedReason::KeyMismatch));
        }
        Ok(match (fmt, parts.next()) {
            ("packed", Some(signed)) if signed == p.commitment => AttestationCheck::Verified,
            ("packed", _) => AttestationCheck::Failed(FailedReason::BadSignature),
            ("none", _) => AttestationCheck::Absent(AbsentReason::None),
            _ => AttestationCheck::Absent(AbsentReason::UnsupportedFormat),
        })
    }

    #[test]
    fn a_tpm_object_with_a_mismatched_key_is_failed_not_absent() {
        let commitment = commitment_of(NC).unwrap();
        let approve = |object: String| {
            let mut f = fixture();
            host(&mut f);
            enrol(&mut f, "m1", "laptop", &[]);
            let req = NewPersonPending {
                attestation_object: Some(object.as_bytes()),
                ..person_req(&commitment)
            };
            f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
            f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
                .unwrap();
            let a = PersonApproval {
                verify_attestation: &by_precedence,
                machine_grade_ack: false,
                ..person_approval()
            };
            let r = f.tx(|tx| approve_person(tx, &a, NOW));
            let a = PersonApproval {
                machine_grade_ack: true,
                ..a
            };
            let body = r.is_err().then(|| {
                f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
                f.records("person.enroll")[0]["body"].clone()
            });
            (r, body)
        };
        // Another key: Failed, whatever the format.
        let (r, body) = approve("tpm|other".to_string());
        assert_eq!(
            refused(r),
            Refusal::AttestationFailed {
                reason: FailedReason::KeyMismatch
            }
        );
        let body = body.unwrap();
        assert_eq!(body["attestation_failed"], "key_mismatch");
        assert_eq!(body["machine_grade_reason"], Value::Null);
        // A packed object's signed challenge must match.
        let (r, _) = approve("packed|ppk|other".to_string());
        assert_eq!(
            refused(r),
            Refusal::AttestationFailed {
                reason: FailedReason::BadSignature
            }
        );
        // Passing the common checks, an unsupported format alone is Absent.
        let (r, body) = approve("tpm|ppk".to_string());
        assert_eq!(
            refused(r),
            Refusal::MachineGradeRequired {
                reason: AbsentReason::UnsupportedFormat
            }
        );
        let body = body.unwrap();
        assert_eq!(body["machine_grade_reason"], "unsupported_format");
        assert_eq!(body["attestation_failed"], Value::Null);
        // And a supported one verifies.
        let (r, body) = approve(format!("packed|ppk|{commitment}"));
        assert!(r.unwrap().presence_capable);
        assert!(body.is_none());
    }

    #[test]
    fn approval_needs_the_candidate_keys_proof_checked_in_the_same_transaction() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        enrol(&mut f, "m2", "desk", &[]);
        let commitment = commitment_of(NC).unwrap();
        f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
            .unwrap();
        // Unproved: refused, and nothing is written.
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &person_approval(), NOW))),
            Refusal::PersonUnproved
        );
        assert_eq!(
            Refusal::PersonUnproved.code().as_str(),
            "api.enroll_pending"
        );
        // Another device can't prove it, and a proof that fails isn't kept.
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m2", NC, NOW, |_| true))),
            Refusal::NotPending
        );
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| false))),
            Refusal::ProofInvalid
        );
        assert_eq!(Refusal::ProofInvalid.code().as_str(), "api.unauthenticated");
        // An Nc that doesn't open the commitment is refused before the
        // verifier runs.
        let other_nc = "ff".repeat(32);
        assert_eq!(
            refused(
                f.tx(|tx| person_prove(tx, "pp1", "m1", &other_nc, NOW, |_| {
                    panic!("verified before the commitment check")
                }))
            ),
            Refusal::CommitmentMismatch
        );
        assert_eq!(
            Refusal::CommitmentMismatch.code().as_str(),
            "api.enroll_changed"
        );
        assert_eq!(Refusal::CommitmentMismatch.reason(), Some("commitment"));
        assert_eq!(Refusal::NotPending.code().as_str(), "api.enroll_changed");
        assert_eq!(Refusal::NotPending.reason(), None);
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "nope", "m1", NC, NOW, |_| true))),
            Refusal::NotPending
        );
        let row = &person_pending(&f.store.conn, NOW).unwrap()[0];
        assert!(row.proved_ms.is_none() && row.nc.is_none());
        // The verifier sees the stored row: the key, the challenge and the
        // policy it must check against.
        let p = f
            .tx(|tx| {
                person_prove(tx, "pp1", "m1", NC, NOW + 1, |p| {
                    p.public_key == "ppk"
                        && p.ns == "ns"
                        && p.commitment == commitment
                        && p.policy == "up"
                })
            })
            .unwrap();
        assert_eq!(p.proved_ms, Some(NOW + 1));
        // Nc and the proof are recorded together.
        let row = &person_pending(&f.store.conn, NOW).unwrap()[0];
        assert_eq!(row.nc.as_deref(), Some(NC));
        assert_eq!(row.proved_ms, Some(NOW + 1));
        // A retry after a lost response returns the proved row and records
        // nothing; a wrong Nc or a failed proof is still refused.
        let again = f
            .tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW + 2, |_| true))
            .unwrap();
        assert_eq!(again.proved_ms, Some(NOW + 1));
        assert_eq!(again.nc.as_deref(), Some(NC));
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", &other_nc, NOW + 2, |_| true))),
            Refusal::CommitmentMismatch
        );
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW + 2, |_| false))),
            Refusal::ProofInvalid
        );
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m2", NC, NOW + 2, |_| true))),
            Refusal::NotPending
        );
        let row = &person_pending(&f.store.conn, NOW).unwrap()[0];
        assert_eq!(row.proved_ms, Some(NOW + 1));
        assert_eq!(row.nc.as_deref(), Some(NC));
        f.tx(|tx| approve_person(tx, &person_approval(), NOW + 3))
            .unwrap();
        assert!(person_key(&f.store.conn, "pfp").unwrap().is_some());
    }

    #[test]
    fn a_revealed_nc_must_be_the_one_the_proof_carries() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
            .unwrap();
        // Revealed in uppercase, stored lowercase; Nc is compared as bytes,
        // so either case proves it, and a different value doesn't.
        let upper = NC.to_uppercase();
        let r = f
            .tx(|tx| person_reveal(tx, "pp1", "m1", &upper, NOW))
            .unwrap();
        assert_eq!(r.nc.as_deref(), Some(NC));
        assert!(f.tx(|tx| person_reveal(tx, "pp1", "m1", NC, NOW)).is_ok());
        let other = "ff".repeat(32);
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", &other, NOW, |_| true))),
            Refusal::CommitmentMismatch
        );
        let p = f
            .tx(|tx| person_prove(tx, "pp1", "m1", &upper, NOW, |_| true))
            .unwrap();
        assert_eq!(p.nc.as_deref(), Some(NC));
        assert_eq!(
            person_pending(&f.store.conn, NOW).unwrap()[0].nc.as_deref(),
            Some(NC)
        );
        f.tx(|tx| approve_person(tx, &person_approval(), NOW))
            .unwrap();
    }

    #[test]
    fn a_pending_rows_possession_bytes_match_wires_published_vector() {
        // wire's pair::vectors: Ns is bytes 0x00 to 0x1f, Nc 32 bytes of
        // 0xa5, and POSSESSION_CHALLENGE_HEX carries SHA-256(Nc).
        let ns_hex: String = (0u8..32).map(|b| format!("{b:02x}")).collect();
        let nc_hex = "a5".repeat(32);
        let commitment = commitment_of(&nc_hex).unwrap();
        let mut f = fixture();
        enrol(&mut f, "m1", "laptop", &[]);
        let req = NewPersonPending {
            ns: &ns_hex,
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        let p = f
            .tx(|tx| {
                person_prove(tx, "pp1", "m1", &nc_hex, NOW, |p| {
                    p.possession_bytes().is_some()
                })
            })
            .unwrap();
        let (ns, c) = p.possession_bytes().unwrap();
        assert_eq!(ns.to_vec(), (0u8..32).collect::<Vec<_>>());
        let c_hex: String = c.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            c_hex,
            "fc8b64001c5fdd0f2f40fb67dae4a865a2c5bd17836676d6d5b58b7917e33717"
        );
        // Text that isn't 32 bytes of hex fails the proof.
        let bad = PersonPending {
            ns: "ns".into(),
            ..p.clone()
        };
        assert!(bad.possession_bytes().is_none());
        let bad = PersonPending {
            commitment: c_hex.clone(),
            ..p.clone()
        };
        assert!(bad.possession_bytes().is_none());
        // Only canonical text: no uppercase digest or nonce, no signed
        // pairs, no wrong length.
        let upper_ns = ns_hex.to_uppercase();
        let plus_ns = format!("+a{}", &ns_hex[2..]);
        for (ns, commitment) in [
            (ns_hex.as_str(), format!("sha256:{}", c_hex.to_uppercase())),
            (upper_ns.as_str(), format!("sha256:{c_hex}")),
            (plus_ns.as_str(), format!("sha256:{c_hex}")),
            (&ns_hex[2..], format!("sha256:{c_hex}")),
            (ns_hex.as_str(), format!("SHA256:{c_hex}")),
        ] {
            let bad = PersonPending {
                ns: ns.into(),
                commitment,
                ..p.clone()
            };
            assert!(
                bad.possession_bytes().is_none(),
                "{} {}",
                bad.ns,
                bad.commitment
            );
        }
        assert!(hex_bytes("+a").is_none());
        assert!(commitment_of("+a").is_none());
    }

    #[test]
    fn a_revoked_device_cant_prove_a_person_key() {
        let mut f = fixture();
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
            .unwrap();
        f.tx(|tx| {
            tx.execute(
                "UPDATE machines SET state = 'revoked' WHERE machine_id = 'm1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))),
            Refusal::MachineRevoked
        );
    }

    #[test]
    fn the_approval_record_names_the_approver_as_writer_and_the_candidate_apart() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        enrol(&mut f, "m2", "desk", &[]);
        let commitment = commitment_of(NC).unwrap();
        // The approver's own key, from m2.
        let first = NewPersonPending {
            machine_id: "m2",
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &first, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m2", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            machine_id: "m2",
            ..person_approval()
        };
        f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        // Bob's new key on m1, approved by human:nick from m2 with that person's key.
        let second = NewPersonPending {
            pending_id: "pp2",
            fingerprint: "pfp2",
            human: "human:bob",
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &second, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp2", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            pending_id: "pp2",
            fingerprint: "pfp2",
            human: "human:bob",
            approver_machine: "m2",
            approver_person_key: Some("pfp"),
            ..person_approval()
        };
        // Another person's key, an unknown key or an unknown machine is
        // refused before anything is read or written.
        for bad in [
            PersonApproval {
                approver: "human:other",
                ..a.clone()
            },
            PersonApproval {
                approver_person_key: Some("nope"),
                ..a.clone()
            },
            PersonApproval {
                approver_machine: "m9",
                ..a.clone()
            },
        ] {
            assert!(f.tx(|tx| approve_person(tx, &bad, NOW)).is_err());
        }
        assert!(person_key(&f.store.conn, "pfp2").unwrap().is_none());
        f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        let body = &f.records("person.enroll")[1]["body"];
        // The authenticated person, machine and key are the approver's; the
        // candidate's are apart (Terra 3 #9913).
        assert_eq!(body["human"], "human:nick");
        assert_eq!(body["candidate_human"], "human:bob");
        // The subjects name the resource that changed: Bob and his key.
        let rec = &f.records("person.enroll")[1];
        assert_eq!(rec["actor"], "human:nick");
        assert_eq!(rec["subject"], json!(["human:bob", "person_key:pfp2"]));
        assert_eq!(body["machine"], "m2");
        assert_eq!(body["person_key"], "pfp");
        assert_eq!(body["approval_mode"], "person_signed");
        assert_eq!(body["candidate_machine"], "m1");
        assert_eq!(body["candidate_person_key"], "pfp2");
        assert_eq!(body["capability"], "machine-grade");
        assert_eq!(body["attestation"], "none");
        // A revoked approver key is refused in the transaction.
        f.tx(|tx| revoke_person(tx, "pfp", "human:nick", NOW))
            .unwrap();
        let third = NewPersonPending {
            pending_id: "pp3",
            fingerprint: "pfp3",
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &third, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp3", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            pending_id: "pp3",
            fingerprint: "pfp3",
            ..a
        };
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::PersonUnknown
        );
    }

    #[test]
    fn a_verifier_error_propagates_and_writes_nothing_even_acknowledged() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            verify_attestation: &|_, _| Err(VerifierError("the verifier broke".into())),
            machine_grade_ack: true,
            ..person_approval()
        };
        match f.tx(|tx| approve_person(tx, &a, NOW)) {
            Err(ServeError::Verifier(e)) => assert_eq!(e.0, "the verifier broke"),
            other => panic!("expected the verifier's error, got {other:?}"),
        }
        assert_eq!(person_pending(&f.store.conn, NOW).unwrap().len(), 1);
        assert!(person_key(&f.store.conn, "pfp").unwrap().is_none());
        assert!(f.records("person.enroll").is_empty());
    }

    #[test]
    fn keyless_approval_is_only_the_bootstrap_rechecked_in_the_transaction() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        // Two keyless bootstrap approvals admitted while the graph has no
        // person: only the first commits.
        for (id, fp) in [("pp1", "pfp"), ("pp2", "pfp2")] {
            let req = NewPersonPending {
                pending_id: id,
                fingerprint: fp,
                ..person_req(&commitment)
            };
            f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
            f.tx(|tx| person_prove(tx, id, "m1", NC, NOW, |_| true))
                .unwrap();
        }
        assert!(!has_any_person_key(&f.store.conn).unwrap());
        f.tx(|tx| approve_person(tx, &person_approval(), NOW))
            .unwrap();
        let second = PersonApproval {
            pending_id: "pp2",
            fingerprint: "pfp2",
            ..person_approval()
        };
        let r = refused(f.tx(|tx| approve_person(tx, &second, NOW)));
        assert_eq!(r, Refusal::PersonSignatureRequired);
        assert_eq!(r.code().as_str(), "api.person_signature_required");
        assert!(person_key(&f.store.conn, "pfp2").unwrap().is_none());
        // Signed with the enrolled key, it passes.
        let signed = PersonApproval {
            approver_person_key: Some("pfp"),
            ..second
        };
        f.tx(|tx| approve_person(tx, &signed, NOW)).unwrap();
        // Every key revoked: back to bootstrap, keyless allowed again.
        for k in ["pfp", "pfp2"] {
            f.tx(|tx| revoke_person(tx, k, "human:nick", NOW)).unwrap();
        }
        assert!(!has_any_person_key(&f.store.conn).unwrap());
        let third = NewPersonPending {
            pending_id: "pp3",
            fingerprint: "pfp3",
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &third, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp3", "m1", NC, NOW, |_| true))
            .unwrap();
        let recovery = PersonApproval {
            pending_id: "pp3",
            fingerprint: "pfp3",
            ..person_approval()
        };
        f.tx(|tx| approve_person(tx, &recovery, NOW)).unwrap();
    }

    #[test]
    fn presence_capable_and_attestation_can_never_disagree() {
        let f = fixture();
        let insert = |key: &str, attestation: &str, capable: i64| {
            f.store.conn.execute(
                "INSERT INTO persons (key_id, human, kind, public_key, policy, state,
                    attestation, presence_capable)
                 VALUES (?1, 'human:nick', 'fido2', 'pk', 'up', 'approved', ?2, ?3)",
                params![key, attestation, capable],
            )
        };
        // Inconsistent rows are refused by the CHECK constraint.
        assert!(insert("k1", "none", 1).is_err());
        assert!(insert("k2", "verified", 0).is_err());
        // Consistent ones go in.
        insert("k3", "verified", 1).unwrap();
        insert("k4", "none", 0).unwrap();
        // And an update can't split them either.
        assert!(
            f.store
                .conn
                .execute(
                    "UPDATE persons SET presence_capable = 0 WHERE key_id = 'k3'",
                    []
                )
                .is_err()
        );
        assert!(
            person_key(&f.store.conn, "k3")
                .unwrap()
                .unwrap()
                .presence_capable
        );
    }

    #[test]
    fn a_development_store_s_persons_table_is_carried_forward_with_its_check() {
        // #10686 item 5: a `persons` table from before presence_capable and
        // its CHECK is rebuilt by `create`, every row kept, presence_capable
        // set from attestation in the same step; a second `create` changes
        // nothing.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE persons (
                key_id TEXT PRIMARY KEY,
                human TEXT NOT NULL,
                kind TEXT NOT NULL,
                public_key TEXT NOT NULL,
                policy TEXT NOT NULL,
                state TEXT NOT NULL,
                enrolled_ms INTEGER,
                enrolled_by TEXT,
                machine_id TEXT,
                attestation TEXT NOT NULL DEFAULT 'none',
                revoked_ms INTEGER,
                revoked_by TEXT
            );
            CREATE INDEX persons_human ON persons (human, state);
            INSERT INTO persons (key_id, human, kind, public_key, policy, state,
                enrolled_ms, attestation)
            VALUES ('k-attested', 'human:nick', 'fido2', 'pk1', 'up', 'approved', 7, 'verified'),
                   ('k-plain', 'human:ana', 'file', 'pk2', 'up', 'approved', 8, 'none');",
        )
        .unwrap();
        // Rolled back, the rebuild leaves the old table and its rows as
        // they were: it runs only inside the caller's transaction.
        let tx = conn.transaction().unwrap();
        create(&tx).unwrap();
        drop(tx);
        let capable_column: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('persons') WHERE name = 'presence_capable'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(capable_column, 0, "rolled back: the old shape");
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM persons", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 2);
        let tx = conn.transaction().unwrap();
        create(&tx).unwrap();
        create(&tx).unwrap();
        tx.commit().unwrap();
        let rows: Vec<(String, String, i64, i64)> = conn
            .prepare(
                "SELECT key_id, attestation, presence_capable, enrolled_ms
                 FROM persons ORDER BY key_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("k-attested".into(), "verified".into(), 1, 7),
                ("k-plain".into(), "none".into(), 0, 8),
            ]
        );
        assert!(
            person_key(&conn, "k-attested")
                .unwrap()
                .unwrap()
                .presence_capable
        );
        // The CHECK is there now, and so is the index.
        assert!(
            conn.execute(
                "UPDATE persons SET presence_capable = 1 WHERE key_id = 'k-plain'",
                []
            )
            .is_err()
        );
        let index: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'persons_human'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
        let temp: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_temp_master WHERE name = 'persons_forward'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(temp, 0, "the temporary copy is gone");
    }

    #[test]
    fn a_keyless_host_approval_records_exactly_its_fields() {
        let commitment = commitment_of(NC).unwrap();
        for (approver, human) in [
            ("human:nick", json!("human:nick")),
            ("nick-host", Value::Null),
        ] {
            let mut f = fixture();
            host(&mut f);
            enrol(&mut f, "m1", "laptop", &[]);
            f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
                .unwrap();
            f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
                .unwrap();
            let a = PersonApproval {
                approver,
                approver_person_key: None,
                ..person_approval()
            };
            f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
            let body = &f.records("person.enroll")[0]["body"];
            assert_eq!(body["machine"], "m-host");
            assert_eq!(body["person_key"], Value::Null);
            assert_eq!(body["human"], human, "{approver}");
            assert_eq!(body["approval_mode"], "keyless_host");
            assert_eq!(body["candidate_human"], "human:nick");
            assert_eq!(body["candidate_machine"], "m1");
            assert_eq!(body["candidate_person_key"], "pfp");
        }
        // A keyless approver must still be a valid actor (Claude 2 #10171).
        for bad in ["", "bad\nactor", "with space", "tab\there"] {
            let mut f = fixture();
            host(&mut f);
            enrol(&mut f, "m1", "laptop", &[]);
            let a = PersonApproval {
                approver: bad,
                ..person_approval()
            };
            assert!(
                matches!(
                    refused(f.tx(|tx| approve_person(tx, &a, NOW))),
                    Refusal::ActorNotAllowed { .. }
                ),
                "{bad:?}"
            );
            assert!(f.records("person.enroll").is_empty());
        }
        // A signed approval still names a human:* person.
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let a = PersonApproval {
            approver: "nick-host",
            approver_person_key: Some("pfp"),
            ..person_approval()
        };
        assert!(matches!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::ActorNotAllowed { .. }
        ));
    }

    #[test]
    fn a_keyless_approval_needs_the_host_machine_and_the_direct_host_context() {
        let commitment = commitment_of(NC).unwrap();
        let setup = || {
            let mut f = fixture();
            host(&mut f);
            enrol(&mut f, "m1", "laptop", &[]);
            f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
                .unwrap();
            f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
                .unwrap();
            f
        };
        let untouched = |f: &Fixture| {
            assert_eq!(person_pending(&f.store.conn, NOW).unwrap().len(), 1);
            assert!(person_key(&f.store.conn, "pfp").unwrap().is_none());
            assert!(f.records("person.enroll").is_empty());
        };
        // A worker's machine id passed as the approver's: refused, even from
        // the host command (Terra 7 #10200).
        let mut f = setup();
        let a = PersonApproval {
            approver_machine: "m1",
            ..person_approval()
        };
        assert!(matches!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::ActorNotAllowed { .. }
        ));
        untouched(&f);
        // The host's machine, but not the in-process host command (the serve
        // path has no DirectHost): refused as needing a person signature.
        let a = PersonApproval {
            direct_host: None,
            ..person_approval()
        };
        let r = refused(f.tx(|tx| approve_person(tx, &a, NOW)));
        assert_eq!(r, Refusal::PersonSignatureRequired);
        assert_eq!(r.code().as_str(), "api.person_signature_required");
        untouched(&f);
        // Both: the bootstrap approval commits.
        f.tx(|tx| approve_person(tx, &person_approval(), NOW))
            .unwrap();
        assert_eq!(f.records("person.enroll")[0]["body"]["machine"], "m-host");
    }

    #[test]
    fn a_verifier_may_borrow_request_scoped_roots() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        // Roots that live only as long as this request (Terra 3 #9999).
        let roots: Vec<u8> = b"request roots".to_vec();
        let check = |_: &PersonPending, object: &[u8]| -> Checked {
            Ok(if object == ATT && !roots.is_empty() {
                AttestationCheck::Verified
            } else {
                AttestationCheck::Failed(FailedReason::ChainFailed)
            })
        };
        let a = PersonApproval {
            verify_attestation: &check,
            ..person_approval()
        };
        assert!(
            f.tx(|tx| approve_person(tx, &a, NOW))
                .unwrap()
                .presence_capable
        );
        drop(roots);
    }

    #[test]
    fn policy_names_are_up_and_uv_only() {
        for p in [PersonPolicy::Up, PersonPolicy::Uv] {
            assert_eq!(PersonPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(PersonPolicy::Up.as_str(), "up");
        assert_eq!(PersonPolicy::Uv.as_str(), "uv");
        for other in ["up+uv", "UV", "", "none"] {
            assert_eq!(PersonPolicy::parse(other), None, "{other}");
        }
    }

    #[test]
    fn a_revoked_device_hears_revoked_before_a_policy_refusal() {
        let mut f = fixture();
        enrol(&mut f, "m1", "laptop", &[]);
        f.tx(|tx| {
            tx.execute(
                "UPDATE machines SET state = 'revoked' WHERE machine_id = 'm1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            kind: PersonKind::Enclave,
            policy: PersonPolicy::Uv,
            ..person_req(&commitment)
        };
        assert_eq!(
            refused(f.tx(|tx| person_enroll(tx, &req, NOW))),
            Refusal::MachineRevoked
        );
    }

    #[test]
    fn uv_is_refused_for_enclave_and_file_keys() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        for kind in [PersonKind::Enclave, PersonKind::File] {
            let req = NewPersonPending {
                kind,
                policy: PersonPolicy::Uv,
                ..person_req(&commitment)
            };
            assert_eq!(
                refused(f.tx(|tx| person_enroll(tx, &req, NOW))),
                Refusal::PolicyUnsupported,
                "{kind:?}"
            );
            assert!(!PersonPolicy::Uv.allowed_for(kind));
            assert!(PersonPolicy::Up.allowed_for(kind));
        }
        assert_eq!(
            Refusal::PolicyUnsupported.code().as_str(),
            "api.unsupported"
        );
        assert!(person_pending(&f.store.conn, NOW).unwrap().is_empty());
        // Approval refuses the combination too, before it reads the row.
        let a = PersonApproval {
            kind: PersonKind::Enclave,
            policy: PersonPolicy::Uv,
            ..person_approval()
        };
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::PolicyUnsupported
        );
        // A FIDO2 key takes either.
        for policy in [PersonPolicy::Up, PersonPolicy::Uv] {
            assert!(policy.allowed_for(PersonKind::Fido2));
        }
    }

    #[test]
    fn failed_reasons_are_a_closed_set_of_stable_words() {
        let words: Vec<_> = FailedReason::ALL
            .into_iter()
            .map(FailedReason::as_str)
            .collect();
        assert_eq!(
            words,
            [
                "malformed",
                "rp_mismatch",
                "no_up",
                "no_uv",
                "key_mismatch",
                "bad_signature",
                "chain_failed",
                "profile"
            ]
        );
        for r in FailedReason::ALL {
            assert_eq!(FailedReason::parse(r.as_str()), Some(r));
        }
        // Free text from a verifier never maps to a reason, so it can't be
        // stored or returned (Claude 2 #9621).
        for text in [
            "chain: bad signature at depth 2",
            "Malformed",
            "",
            "backup",
            "roots",
        ] {
            assert_eq!(FailedReason::parse(text), None, "{text}");
        }
    }

    #[test]
    fn absent_reasons_are_person_keys_names() {
        let names: Vec<_> = [
            AbsentReason::None,
            AbsentReason::SelfAttested,
            AbsentReason::Unanchored,
            AbsentReason::Backup,
            AbsentReason::UnsupportedFormat,
            AbsentReason::UnsupportedAlg,
        ]
        .into_iter()
        .map(AbsentReason::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "none",
                "self",
                "unanchored",
                "backup",
                "unsupported_format",
                "unsupported_alg"
            ]
        );
    }

    #[test]
    fn a_backed_up_key_is_absent_not_failed() {
        // BE or BS: nothing wrong with the evidence, it just doesn't bind
        // the key to one device (#9299). Refused machine_grade without the
        // acknowledgement; recorded "none" with the reason in the event.
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            verify_attestation: &verified_backup_eligible,
            machine_grade_ack: false,
            ..person_approval()
        };
        let r = refused(f.tx(|tx| approve_person(tx, &a, NOW)));
        assert_eq!(
            r,
            Refusal::MachineGradeRequired {
                reason: AbsentReason::Backup
            }
        );
        assert_eq!(r.code().as_str(), "api.machine_grade_required");
        assert_eq!(r.reason(), Some("backup"));
        let a = PersonApproval {
            machine_grade_ack: true,
            ..a
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert_eq!(key.attestation, "none");
        assert!(!key.presence_capable);
        let body = &f.records("person.enroll")[0]["body"];
        assert_eq!(body["machine_grade_reason"], "backup");
        assert_eq!(body["attestation_failed"], Value::Null);
    }

    #[test]
    fn a_machine_grade_key_needs_the_acknowledgement_in_the_same_transaction() {
        let commitment = commitment_of(NC).unwrap();
        let setup = |object: Option<&'static [u8]>, kind: PersonKind| {
            let mut f = fixture();
            host(&mut f);
            enrol(&mut f, "m1", "laptop", &[]);
            let req = NewPersonPending {
                kind,
                attestation_object: object,
                ..person_req(&commitment)
            };
            f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
            f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
                .unwrap();
            f
        };
        let untouched = |f: &Fixture| {
            assert_eq!(person_pending(&f.store.conn, NOW).unwrap().len(), 1);
            assert!(person_key(&f.store.conn, "pfp").unwrap().is_none());
            assert!(f.records("person.enroll").is_empty());
        };

        // Outcome 1: verified, no acknowledgement needed.
        let mut f = setup(Some(ATT), PersonKind::Fido2);
        let a = PersonApproval {
            verify_attestation: &verified,
            machine_grade_ack: false,
            ..person_approval()
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert!(key.presence_capable);
        let body = &f.records("person.enroll")[0]["body"];
        assert_eq!(body["attestation_failed"], Value::Null);
        assert_eq!(body["machine_grade_reason"], Value::Null);

        // Outcome 2: absent. Refused without the acknowledgement, the
        // check never called; recorded machine-grade with it.
        let mut f = setup(None, PersonKind::Fido2);
        let a = PersonApproval {
            verify_attestation: &|_, _| panic!("checked without evidence"),
            machine_grade_ack: false,
            ..person_approval()
        };
        let r = refused(f.tx(|tx| approve_person(tx, &a, NOW)));
        assert_eq!(
            r,
            Refusal::MachineGradeRequired {
                reason: AbsentReason::None
            }
        );
        assert_eq!(r.code().as_str(), "api.machine_grade_required");
        assert_eq!(r.reason(), Some("none"));
        untouched(&f);
        let a = PersonApproval {
            machine_grade_ack: true,
            ..a
        };
        assert!(
            !f.tx(|tx| approve_person(tx, &a, NOW))
                .unwrap()
                .presence_capable
        );
        let body = &f.records("person.enroll")[0]["body"];
        assert_eq!(body["machine_grade_reason"], "none");
        assert_eq!(body["attestation_failed"], Value::Null);

        // Outcome 3: failed. Refused with its reason; with the
        // acknowledgement, machine-grade and the reason in the event.
        let mut f = setup(Some(ATT), PersonKind::Fido2);
        let a = PersonApproval {
            verify_attestation: &unverified,
            machine_grade_ack: false,
            ..person_approval()
        };
        let r = refused(f.tx(|tx| approve_person(tx, &a, NOW)));
        assert_eq!(
            r,
            Refusal::AttestationFailed {
                reason: FailedReason::ChainFailed
            }
        );
        assert_eq!(r.code().as_str(), "api.attestation_failed");
        assert_eq!(r.reason(), Some("chain_failed"));
        untouched(&f);
        let a = PersonApproval {
            machine_grade_ack: true,
            ..a
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert_eq!(key.attestation, "none");
        let body = &f.records("person.enroll")[0]["body"];
        assert_eq!(body["attestation_failed"], "chain_failed");
        assert_eq!(body["machine_grade_reason"], Value::Null);

        // An enclave key is machine-grade: it needs the acknowledgement too,
        // and its evidence is never checked.
        let mut f = setup(Some(ATT), PersonKind::Enclave);
        let a = PersonApproval {
            kind: PersonKind::Enclave,
            verify_attestation: &|_, _| panic!("an enclave key's evidence was checked"),
            machine_grade_ack: false,
            ..person_approval()
        };
        assert_eq!(
            refused(f.tx(|tx| approve_person(tx, &a, NOW))),
            Refusal::MachineGradeRequired {
                reason: AbsentReason::None
            }
        );
        untouched(&f);
    }

    #[test]
    fn a_backup_eligible_fido2_key_is_recorded_unattested() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        // A syncable key's verified attestation describes an authenticator
        // the key can leave: stored as "none", machine-grade.
        let a = PersonApproval {
            verify_attestation: &verified_backup_eligible,
            ..person_approval()
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert_eq!(key.attestation, "none");
        assert_eq!(
            person_key(&f.store.conn, "pfp")
                .unwrap()
                .unwrap()
                .attestation,
            "none"
        );
        assert!(!key.presence_capable);
        assert_eq!(
            f.records("person.enroll")[0]["body"]["capability"],
            "machine-grade"
        );
    }

    #[test]
    fn a_verified_enclave_key_stays_machine_grade() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        let req = NewPersonPending {
            kind: PersonKind::Enclave,
            policy: PersonPolicy::Up,
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        // Hardware backing doesn't show the key needs the person to sign:
        // the store records it unattested and machine-grade.
        let a = PersonApproval {
            kind: PersonKind::Enclave,
            policy: PersonPolicy::Up,
            verify_attestation: &verified,
            ..person_approval()
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert_eq!(key.attestation, "none");
        assert!(!key.presence_capable);
        assert!(!key.presence_capable);
        assert_eq!(
            f.records("person.enroll")[0]["body"]["capability"],
            "machine-grade"
        );
    }

    #[test]
    fn a_verified_fido2_key_proves_presence() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        // A FIDO2 key may carry "uv".
        let req = NewPersonPending {
            policy: PersonPolicy::Uv,
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            policy: PersonPolicy::Uv,
            verify_attestation: &verified,
            ..person_approval()
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert!(key.presence_capable);
        assert_eq!(key.policy, "uv");
        assert_eq!(
            f.records("person.enroll")[0]["body"]["capability"],
            "presence-capable"
        );
        // Approval deletes the row, and its evidence with it.
        assert!(person_pending(&f.store.conn, NOW).unwrap().is_empty());
    }

    #[test]
    fn attestation_evidence_is_bounded_checked_at_approval_and_dropped_on_expiry() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let commitment = commitment_of(NC).unwrap();
        // Over the cap: refused, nothing kept. At the cap: kept.
        let big = vec![7u8; MAX_ATTESTATION_BYTES + 1];
        let req = NewPersonPending {
            attestation_object: Some(&big),
            ..person_req(&commitment)
        };
        assert_eq!(
            refused(f.tx(|tx| person_enroll(tx, &req, NOW))),
            Refusal::TooLarge
        );
        assert_eq!(Refusal::TooLarge.code().as_str(), "api.too_large");
        assert!(person_pending(&f.store.conn, NOW).unwrap().is_empty());
        let req = NewPersonPending {
            attestation_object: Some(&big[..MAX_ATTESTATION_BYTES]),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        assert_eq!(
            person_pending(&f.store.conn, NOW).unwrap()[0]
                .attestation_object
                .as_deref()
                .map(<[u8]>::len),
            Some(MAX_ATTESTATION_BYTES)
        );
        // The check sees the stored object and row; one that doesn't verify
        // is recorded "none".
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            verify_attestation: &verified,
            ..person_approval()
        };
        let key = f.tx(|tx| approve_person(tx, &a, NOW)).unwrap();
        assert_eq!(key.attestation, "none");

        // No object: the check isn't called, and the key is unattested.
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        f.tx(|tx| person_enroll(tx, &person_req(&commitment), NOW))
            .unwrap();
        f.tx(|tx| person_prove(tx, "pp1", "m1", NC, NOW, |_| true))
            .unwrap();
        let a = PersonApproval {
            verify_attestation: &|_, _| panic!("checked without evidence"),
            ..person_approval()
        };
        assert_eq!(
            f.tx(|tx| approve_person(tx, &a, NOW)).unwrap().attestation,
            "none"
        );

        // Expiry deletes the row with its evidence.
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "laptop", &[]);
        let req = NewPersonPending {
            attestation_object: Some(ATT),
            ..person_req(&commitment)
        };
        f.tx(|tx| person_enroll(tx, &req, NOW)).unwrap();
        let later = NOW + PENDING_MS;
        // A refused write after expiry rolls its own cleanup back...
        assert_eq!(
            refused(f.tx(|tx| person_prove(tx, "pp1", "m1", NC, later, |_| true))),
            Refusal::NotPending
        );
        let raw = |f: &Fixture| -> i64 {
            f.store
                .conn
                .query_row("SELECT COUNT(*) FROM person_pending", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(raw(&f), 1);
        // ...so the sweep, committed on its own, is what removes it.
        assert_eq!(f.tx(|tx| prune_pending(tx, later - 1)).unwrap(), 0);
        assert_eq!(f.tx(|tx| prune_pending(tx, later)).unwrap(), 1);
        assert_eq!(raw(&f), 0);
        let kept: i64 = f
            .store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM person_pending WHERE attestation_object IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 0);
    }

    fn claim<'a>(
        agent: &'a str,
        machine_id: &'a str,
        rule: Option<&'a Rule>,
    ) -> PlacementClaim<'a> {
        PlacementClaim {
            agent,
            machine_id,
            rule,
            harness: None,
            checkout: None,
            actor: "launcher:x",
        }
    }

    #[test]
    fn placement_is_fixed_by_the_first_eligible_claim() {
        let mut f = fixture();
        host(&mut f);
        // No remote machine: no row, as 0.7.0 (R136).
        assert_eq!(
            f.tx(|tx| claim_placement(tx, &claim("lane-a", "m-host", None), NOW))
                .unwrap(),
            Placed::Unplaced
        );
        assert!(placement(&f.store.conn, "lane-a").unwrap().is_none());
        enrol(&mut f, "m1", "build-box", &["macs".into()]);
        enrol(&mut f, "m2", "other", &[]);
        let rule = Rule {
            machines: vec![],
            groups: vec!["macs".into()],
        };
        // A machine the rule doesn't name can't take it by being first.
        assert_eq!(
            refused(f.tx(|tx| claim_placement(tx, &claim("lane-b", "m2", Some(&rule)), NOW))),
            Refusal::NotPlaced { placed_on: None }
        );
        let Placed::New(p) = f
            .tx(|tx| claim_placement(tx, &claim("lane-b", "m1", Some(&rule)), NOW))
            .unwrap()
        else {
            panic!("expected a new placement");
        };
        assert_eq!(p.source, "claimed");
        assert!(matches!(
            f.tx(|tx| claim_placement(tx, &claim("lane-b", "m1", Some(&rule)), NOW))
                .unwrap(),
            Placed::Existing(_)
        ));
        // Even with a rule that names it, another machine never takes it.
        let both = Rule {
            machines: vec!["other".into(), "build-box".into()],
            groups: vec![],
        };
        assert_eq!(
            refused(f.tx(|tx| claim_placement(tx, &claim("lane-b", "m2", Some(&both)), NOW))),
            Refusal::NotPlaced {
                placed_on: Some("m1".into())
            }
        );
        // No rule: the host only (R34), placed `local` once a remote exists.
        assert!(matches!(
            refused(f.tx(|tx| claim_placement(tx, &claim("lane-c", "m1", None), NOW))),
            Refusal::NotPlaced { .. }
        ));
        let Placed::New(p) = f
            .tx(|tx| claim_placement(tx, &claim("lane-c", "m-host", None), NOW))
            .unwrap()
        else {
            panic!("expected a new placement");
        };
        assert_eq!(p.source, "local");
        assert_eq!(f.records("placement.claim").len(), 2);
    }

    #[test]
    fn a_logged_out_harness_makes_a_machine_ineligible() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "build-box", &[]);
        let rule = Rule {
            machines: vec!["build-box".into()],
            groups: vec![],
        };
        let with_harness = PlacementClaim {
            harness: Some("claude"),
            ..claim("lane-a", "m1", Some(&rule))
        };
        // No report yet: not eligible.
        assert!(matches!(
            refused(f.tx(|tx| claim_placement(tx, &with_harness, NOW))),
            Refusal::NotPlaced { .. }
        ));
        let out =
            json!({"harnesses": [{"harness": "claude", "installed": true, "logged_in": false}]});
        assert!(f.tx(|tx| set_report(tx, "m1", &out)).unwrap());
        assert!(matches!(
            refused(f.tx(|tx| claim_placement(tx, &with_harness, NOW))),
            Refusal::NotPlaced { .. }
        ));
        let unknown =
            json!({"harnesses": [{"harness": "claude", "installed": true, "logged_in": null}]});
        assert!(f.tx(|tx| set_report(tx, "m1", &unknown)).unwrap());
        assert!(!f.tx(|tx| set_report(tx, "m1", &unknown)).unwrap());
        assert!(matches!(
            f.tx(|tx| claim_placement(tx, &with_harness, NOW)).unwrap(),
            Placed::New(_)
        ));
        assert_eq!(f.records("machine.report").len(), 2);
    }

    #[test]
    fn retirement_is_resumable_and_its_handoff_stays_due_until_sent() {
        let mut f = fixture();
        host(&mut f);
        enrol(&mut f, "m1", "build-box", &[]);
        let rule = Rule {
            machines: vec!["m1".into()],
            groups: vec![],
        };
        f.tx(|tx| claim_placement(tx, &claim("lane-a", "m1", Some(&rule)), NOW))
            .unwrap();
        let p = f
            .tx(|tx| begin_retire(tx, "lane-a", "human:nick", NOW))
            .unwrap();
        assert_eq!(
            (p.state, p.machine_id.as_str()),
            (PlacementState::Retiring, "m1")
        );
        // From the first phase, its turn lease can't be claimed.
        assert_eq!(
            refused(f.tx(|tx| claim_placement(tx, &claim("lane-a", "m1", Some(&rule)), NOW))),
            Refusal::Retired
        );
        assert_eq!(
            refused(require_placed_here(&f.store.conn, "lane-a", "m1")),
            Refusal::Retired
        );
        // A rerun resumes.
        let again = f
            .tx(|tx| begin_retire(tx, "lane-a", "human:x", NOW + 1))
            .unwrap();
        assert_eq!(again.retire_by.as_deref(), Some("human:nick"));
        assert_eq!(
            stalled_retirements(&f.store.conn, NOW + 10, 5)
                .unwrap()
                .len(),
            1
        );
        let p = f.tx(|tx| finish_retire(tx, "lane-a", NOW + 2)).unwrap();
        assert_eq!(p.state, PlacementState::Retired);
        assert_eq!(f.tx(|tx| finish_retire(tx, "lane-a", NOW + 3)).unwrap(), p);
        // A crash after the commit keeps the handoff due.
        assert_eq!(handoffs_due(&f.store.conn).unwrap().len(), 1);
        f.tx(|tx| mark_handoff(tx, "lane-a", "msg-1", NOW + 4))
            .unwrap();
        assert!(handoffs_due(&f.store.conn).unwrap().is_empty());
        assert_eq!(f.records("placement.release")[0]["actor"], "human:nick");
        // An agent never placed is retired too, with no machine to fence.
        let p = f
            .tx(|tx| begin_retire(tx, "never", "human:nick", NOW))
            .unwrap();
        assert_eq!(p.machine_id, "");
        assert_eq!(
            refused(f.tx(|tx| claim_placement(tx, &claim("never", "m-host", None), NOW))),
            Refusal::Retired
        );
    }

    fn placed_worker(f: &mut Fixture) {
        host(f);
        enrol(f, "m1", "build-box", &[]);
        let rule = Rule {
            machines: vec!["build-box".into()],
            groups: vec![],
        };
        f.tx(|tx| claim_placement(tx, &claim("lane-a", "m1", Some(&rule)), NOW))
            .unwrap();
        f.tx(|tx| {
            start_supervisor(
                tx,
                &SupervisorStart {
                    supervisor_id: "s1",
                    machine_id: "m1",
                    pid: Some(42),
                    build: Some("0.7.1"),
                    agents: &["lane-a".into()],
                },
                NOW,
            )
        })
        .unwrap();
    }

    fn cands() -> Vec<Candidate> {
        vec![
            Candidate::new(CandidateKind::Node, "task-1"),
            Candidate::new(CandidateKind::Entry, "e-1"),
            Candidate::new(CandidateKind::Message, "msg-1"),
            Candidate::fingerprint("msg-1", "fp-a"),
            Candidate::new(CandidateKind::Message, "msg-2"),
            Candidate::fingerprint("msg-2", "fp-b"),
            Candidate::new(CandidateKind::Message, "msg-3"),
            Candidate::fingerprint("msg-3", "fp-c"),
        ]
    }

    fn nonce_digest() -> String {
        Digest::of_bytes(b"the-nonce").to_string()
    }

    fn admit_one(f: &mut Fixture, turn_id: &str, candidates: &[Candidate]) -> Result<Admitted> {
        let nd = nonce_digest();
        f.tx(|tx| {
            admit(
                tx,
                &Admission {
                    turn_id,
                    agent: "lane-a",
                    machine_id: "m1",
                    supervisor_id: "s1",
                    shape: Shape::Digest,
                    nonce_digest: &nd,
                    candidates,
                },
                NOW,
            )
        })
    }

    fn message(id: &str, flag: &str) -> Candidate {
        Candidate {
            flag: Some(flag.into()),
            ..Candidate::new(CandidateKind::Message, id)
        }
    }

    #[test]
    fn admission_is_idempotent_by_turn_id_and_writes_nothing_without_candidates() {
        let mut f = fixture();
        placed_worker(&mut f);
        let before = f.records("turn.start").len();
        assert_eq!(refused(admit_one(&mut f, "t0", &[])), Refusal::NothingToDo);
        assert_eq!(f.records("turn.start").len(), before);
        let first = admit_one(&mut f, "t1", &cands()).unwrap();
        assert_eq!((first.turn.number, first.duplicate), (1, false));
        // A resend, however late, returns the same turn and the stored
        // candidates, whatever it sends now.
        let again = admit_one(&mut f, "t1", &[]).unwrap();
        assert!(again.duplicate);
        assert_eq!(again.turn, first.turn);
        assert_eq!(again.candidates, first.candidates);
        assert_eq!(admit_one(&mut f, "t2", &cands()).unwrap().turn.number, 2);
        // A fingerprint of a message that isn't a candidate is refused.
        let stray = vec![
            Candidate::new(CandidateKind::Node, "task-1"),
            Candidate::fingerprint("msg-9", "x"),
        ];
        assert_eq!(
            refused(admit_one(&mut f, "t3", &stray)),
            Refusal::ShownMismatch
        );
        let start = &f.records("turn.start")[0];
        assert_eq!(start["actor"], "lane-a");
        assert_eq!(start["body"]["nonce_digest"], nonce_digest());
        assert!(start["body"].get("nonce").is_none());
    }

    #[test]
    fn admission_needs_the_placed_machine_and_a_registered_supervisor() {
        let mut f = fixture();
        placed_worker(&mut f);
        enrol(&mut f, "m2", "other", &[]);
        let nd = nonce_digest();
        let c = cands();
        let r = f.tx(|tx| {
            admit(
                tx,
                &Admission {
                    turn_id: "t1",
                    agent: "lane-a",
                    machine_id: "m2",
                    supervisor_id: "s1",
                    shape: Shape::Digest,
                    nonce_digest: &nd,
                    candidates: &c,
                },
                NOW,
            )
        });
        assert_eq!(
            refused(r),
            Refusal::NotPlaced {
                placed_on: Some("m1".into())
            }
        );
        let r = f.tx(|tx| {
            admit(
                tx,
                &Admission {
                    turn_id: "t1",
                    agent: "lane-a",
                    machine_id: "m1",
                    supervisor_id: "s-unknown",
                    shape: Shape::Digest,
                    nonce_digest: &nd,
                    candidates: &c,
                },
                NOW,
            )
        });
        assert!(matches!(refused(r), Refusal::ActorNotAllowed { .. }));
    }

    #[test]
    fn the_shown_set_is_within_the_candidates_and_carries_every_shown_bodys_fingerprints() {
        let mut f = fixture();
        placed_worker(&mut f);
        admit_one(&mut f, "t1", &cands()).unwrap();
        let outside = vec![Candidate::new(CandidateKind::Node, "task-9")];
        assert_eq!(
            refused(f.tx(|tx| shown(tx, "t1", "m1", &outside))),
            Refusal::ShownMismatch
        );
        let set = vec![
            Candidate::new(CandidateKind::Node, "task-1"),
            message("msg-1", "body"),
            message("msg-2", "header"),
        ];
        let (_, dup) = f.tx(|tx| shown(tx, "t1", "m1", &set)).unwrap();
        assert!(!dup);
        let s = shown_set(&f.store.conn, "t1").unwrap();
        // The header-only body's fingerprints are kept too; the message
        // never shown isn't.
        assert_eq!(
            s.fingerprints,
            ["fp-a", "fp-b"].iter().map(|s| s.to_string()).collect()
        );
        assert!(!s.capped);
        let (_, dup) = f.tx(|tx| shown(tx, "t1", "m1", &set)).unwrap();
        assert!(dup);
        assert_eq!(
            refused(f.tx(|tx| shown(tx, "t1", "m1", &set[..1]))),
            Refusal::ShownMismatch
        );
        // A body read later in the turn joins the set.
        f.tx(|tx| record_body_read(tx, "t1", "m1", "msg-7", &["fp-z".into()], true))
            .unwrap();
        let s = shown_set(&f.store.conn, "t1").unwrap();
        assert!(s.fingerprints.contains("fp-z"));
        assert!(s.capped);
        assert_eq!(f.records("turn.shown").len(), 1);
    }

    #[test]
    fn an_import_is_checked_against_the_stored_turn() {
        let mut f = fixture();
        placed_worker(&mut f);
        admit_one(&mut f, "t1", &cands()).unwrap();
        // Before the shown set, nothing imports.
        assert_eq!(
            refused(check_import(
                &f.store.conn,
                "t1",
                "m1",
                "s1",
                "the-nonce",
                NOW
            )),
            Refusal::ShownMismatch
        );
        let set = vec![Candidate::new(CandidateKind::Node, "task-1")];
        f.tx(|tx| shown(tx, "t1", "m1", &set)).unwrap();
        assert_eq!(
            refused(check_import(&f.store.conn, "t1", "m1", "s1", "forged", NOW)),
            Refusal::NonceMismatch
        );
        let (t, s) = check_import(&f.store.conn, "t1", "m1", "s1", "the-nonce", NOW).unwrap();
        assert_eq!(t.agent, "lane-a");
        assert!(s.nodes.contains("task-1"));
        // A stale supervisor's record doesn't bind.
        assert!(matches!(
            refused(check_import(
                &f.store.conn,
                "t1",
                "m1",
                "s1",
                "the-nonce",
                NOW + FRESH_HEARTBEAT_MS + 1
            )),
            Refusal::ActorNotAllowed { .. }
        ));
        // After retirement begins, an already pushed turn's block can't
        // apply.
        f.tx(|tx| begin_retire(tx, "lane-a", "human:nick", NOW))
            .unwrap();
        assert_eq!(
            refused(check_import(
                &f.store.conn,
                "t1",
                "m1",
                "s1",
                "the-nonce",
                NOW
            )),
            Refusal::Retired
        );
    }

    #[test]
    fn a_turn_ends_once_and_a_retired_agent_can_still_close_it() {
        let mut f = fixture();
        placed_worker(&mut f);
        admit_one(&mut f, "t1", &cands()).unwrap();
        f.tx(|tx| begin_retire(tx, "lane-a", "human:nick", NOW))
            .unwrap();
        f.tx(|tx| finish_retire(tx, "lane-a", NOW)).unwrap();
        assert_eq!(
            control(&f.store.conn, "s1").unwrap().retired,
            vec!["lane-a"]
        );
        // The worker restarts: a new supervisor registers recovery-only,
        // because the turn has no end.
        let s2 = f
            .tx(|tx| {
                start_supervisor(
                    tx,
                    &SupervisorStart {
                        supervisor_id: "s2",
                        machine_id: "m1",
                        pid: None,
                        build: None,
                        agents: &["lane-a".into()],
                    },
                    NOW,
                )
            })
            .unwrap();
        assert!(s2.agents[0].recovery_only);
        // Recovery-only can't admit.
        let nd = nonce_digest();
        let c = cands();
        let r = f.tx(|tx| {
            admit(
                tx,
                &Admission {
                    turn_id: "t9",
                    agent: "lane-a",
                    machine_id: "m1",
                    supervisor_id: "s2",
                    shape: Shape::Batch,
                    nonce_digest: &nd,
                    candidates: &c,
                },
                NOW,
            )
        });
        assert_eq!(refused(r), Refusal::Retired);
        let by = EndBy::Supervisor {
            machine_id: "m1",
            supervisor_id: "s2",
        };
        let (t, dup) = f.tx(|tx| end_turn(tx, "t1", "failed", &by, NOW)).unwrap();
        assert!(!dup && !t.running());
        assert_eq!(t.ended_by.as_deref(), Some("s2"));
        let (_, dup) = f.tx(|tx| end_turn(tx, "t1", "failed", &by, NOW)).unwrap();
        assert!(dup);
        assert_eq!(
            refused(f.tx(|tx| end_turn(tx, "t1", "ok", &by, NOW))),
            Refusal::TurnEnded {
                outcome: "failed".into()
            }
        );
        // With no open turn left, a retired agent can't register at all.
        let r = f.tx(|tx| {
            start_supervisor(
                tx,
                &SupervisorStart {
                    supervisor_id: "s3",
                    machine_id: "m1",
                    pid: None,
                    build: None,
                    agents: &["lane-a".into()],
                },
                NOW,
            )
        });
        assert_eq!(refused(r), Refusal::Retired);
        assert_eq!(f.records("turn.end")[0]["actor"], "launcher:build-box");
    }

    #[test]
    fn an_operator_abandons_a_lost_turn_and_loss_is_written_once() {
        let mut f = fixture();
        placed_worker(&mut f);
        admit_one(&mut f, "t1", &cands()).unwrap();
        let (seq, agents) = f.tx(|tx| mark_lost(tx, "m1")).unwrap().unwrap();
        assert_eq!(agents[0].agent, "lane-a");
        assert!(f.tx(|tx| mark_lost(tx, "m1")).unwrap().is_none());
        let t = turn(&f.store.conn, "t1").unwrap().unwrap();
        assert!(turn_lost(&f.store.conn, &t).unwrap());
        assert_eq!(running_turns(&f.store.conn, "lane-a").unwrap().len(), 1);
        let (t, _) = f
            .tx(|tx| {
                end_turn(
                    tx,
                    "t1",
                    "interrupted",
                    &EndBy::Operator {
                        human: "human:nick",
                    },
                    NOW,
                )
            })
            .unwrap();
        assert!(!turn_lost(&f.store.conn, &t).unwrap());
        assert_eq!(f.tx(|tx| mark_returned(tx, "m1")).unwrap(), Some(seq));
        assert_eq!(f.tx(|tx| mark_returned(tx, "m1")).unwrap(), None);
        assert_eq!(f.records("machine.returned").len(), 1);
    }

    #[test]
    fn refusals_are_kept_for_good_and_provenance_is_pruned_only_after_the_end() {
        let mut f = fixture();
        placed_worker(&mut f);
        admit_one(&mut f, "t1", &cands()).unwrap();
        assert_eq!(
            f.tx(|tx| keep_refusal(tx, "t1", "import", "msg.secret_refused", NOW))
                .unwrap(),
            "msg.secret_refused"
        );
        assert_eq!(
            f.tx(|tx| keep_refusal(tx, "t1", "import", "msg.too_large", NOW))
                .unwrap(),
            "msg.secret_refused"
        );
        let far = NOW + 2 * PROVENANCE_KEEP_MS;
        assert_eq!(f.tx(|tx| prune_provenance(tx, far)).unwrap(), 0);
        let by = EndBy::Supervisor {
            machine_id: "m1",
            supervisor_id: "s1",
        };
        f.tx(|tx| end_turn(tx, "t1", "ok", &by, NOW)).unwrap();
        assert_eq!(f.tx(|tx| prune_provenance(tx, NOW + DAY_MS)).unwrap(), 0);
        assert!(f.tx(|tx| prune_provenance(tx, far)).unwrap() > 0);
        assert!(candidates(&f.store.conn, "t1").unwrap().is_empty());
        assert!(turn(&f.store.conn, "t1").unwrap().is_some());
        assert_eq!(
            refusal(&f.store.conn, "t1", "import").unwrap().as_deref(),
            Some("msg.secret_refused")
        );
    }

    #[test]
    fn supervisors_heartbeat_resume_and_stop() {
        let mut f = fixture();
        placed_worker(&mut f);
        assert!(
            registered_for(&f.store.conn, "s1", "m1", "lane-a", NOW)
                .unwrap()
                .is_some()
        );
        assert!(
            registered_for(&f.store.conn, "s1", "m2", "lane-a", NOW)
                .unwrap()
                .is_none()
        );
        assert!(f.tx(|tx| mark_stale(tx, "s1")).unwrap());
        f.tx(|tx| request_stop(tx, "s1", "lane-a")).unwrap();
        let c = f.tx(|tx| heartbeat(tx, "s1", "m1", NOW + 10)).unwrap();
        assert_eq!(c.stop, vec!["lane-a"]);
        assert_eq!(f.records("supervisor.resume").len(), 1);
        f.tx(|tx| set_heartbeats(tx, &[("s1".into(), NOW + 20)]))
            .unwrap();
        assert_eq!(
            supervisor(&f.store.conn, "s1")
                .unwrap()
                .unwrap()
                .heartbeat_ms,
            NOW + 20
        );
        assert!(f.tx(|tx| stop_supervisor(tx, "s1", "m1")).unwrap());
        assert!(
            registered_for(&f.store.conn, "s1", "m1", "lane-a", NOW + 20)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            refused(f.tx(|tx| heartbeat(tx, "s1", "m1", NOW + 30))),
            Refusal::ActorNotAllowed { .. }
        ));
        f.tx(|tx| set_seen(tx, &[("m1".into(), NOW + 30)])).unwrap();
        assert_eq!(
            machine(&f.store.conn, "m1").unwrap().unwrap().seen_ms,
            Some(NOW + 30)
        );
        f.tx(|tx| set_serve_seen(tx, NOW)).unwrap();
        assert_eq!(serve_seen(&f.store.conn).unwrap(), Some(NOW));
    }

    fn key_at(ms: u64) -> String {
        let hex = format!("{ms:012x}");
        format!("{}-{}-7000-8000-000000000000", &hex[..8], &hex[8..])
    }

    #[test]
    fn keyed_writes_answer_retries_and_refuse_reuse_skew_and_age() {
        let mut f = fixture();
        let key = key_at(NOW);
        assert_eq!(key_time_ms(&key), Some(NOW));
        assert_eq!(
            refused(check_request(
                &f.store.conn,
                "m1",
                "not-a-uuid",
                "op",
                "d",
                NOW
            )),
            Refusal::KeyInvalid
        );
        assert_eq!(
            check_request(&f.store.conn, "m1", &key, "op", "d", NOW).unwrap(),
            Prior::Fresh
        );
        let result = json!({"ok": true});
        f.tx(|tx| record_request(tx, "m1", &key, "op", "d", &result, Some("intent-1"), NOW))
            .unwrap();
        assert_eq!(
            check_request(&f.store.conn, "m1", &key, "op", "d", NOW + 1).unwrap(),
            Prior::Duplicate {
                result: result.clone(),
                intent: Some("intent-1".into())
            }
        );
        // Keys are scoped by machine.
        assert_eq!(
            check_request(&f.store.conn, "m2", &key, "op", "d", NOW).unwrap(),
            Prior::Fresh
        );
        assert_eq!(
            refused(check_request(&f.store.conn, "m1", &key, "op", "other", NOW)),
            Refusal::KeyReused
        );
        assert_eq!(
            refused(check_request(
                &f.store.conn,
                "m1",
                &key,
                "op",
                "d",
                NOW + KEY_MAX_AGE_MS + 1
            )),
            Refusal::KeyExpired
        );
        let skewed = key_at(NOW + KEY_MAX_SKEW_MS + 1);
        assert_eq!(
            refused(check_request(&f.store.conn, "m1", &skewed, "op", "d", NOW)),
            Refusal::KeySkewed
        );
        // An unfinished intent keeps its row past the window.
        assert_eq!(
            f.tx(|tx| prune_requests(tx, NOW + REQUEST_WINDOW_MS))
                .unwrap(),
            0
        );
        f.tx(|tx| finish_request_intent(tx, "m1", &key)).unwrap();
        assert_eq!(
            f.tx(|tx| prune_requests(tx, NOW + REQUEST_WINDOW_MS))
                .unwrap(),
            1
        );
    }

    #[test]
    fn valid_names() {
        for ok in ["a", "build-box", "mac2", "0x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a b", "a:b", "a.g1x", &"a".repeat(64)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
