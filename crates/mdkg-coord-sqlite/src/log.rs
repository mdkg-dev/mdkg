//! The store's hash-linked record chain for coordination state changes.
//!
//! Every write that changes coordination state appends one record to the
//! `log` table in the same transaction, so a mutation and its record land
//! together or not at all: a crash or a failed write leaves neither. The
//! chain opens with a `log.genesis` record naming the store, written when
//! the store is created or upgraded to schema 3.
//!
//! Coordinator commands become one record per journal entry, carrying the
//! command and its outcome. Mail becomes `msg.*` and `room.*` records that
//! carry a body's digest, never the body.

use std::sync::OnceLock;

use mdkg_coord::{Command, Coordinator, LeaseId, Outcome};
use mdkg_log::record::{GENESIS, Record, node_ref};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

use crate::store::StoreError;

/// Who a process's records name, when the write itself doesn't (set once
/// by the CLI from `--as`, `$MDKG_ACTOR` or the worktree's name).
static ACTOR: OnceLock<String> = OnceLock::new();

/// The actor this process's records name. Only the first call counts.
pub fn set_record_actor(actor: &str) {
    ACTOR.get_or_init(|| actor.to_string());
}

/// The actor for writes that don't name their own.
pub const SYSTEM_ACTOR: &str = "mdkg";

fn actor_or(own: Option<&str>) -> String {
    ACTOR
        .get()
        .map(String::as_str)
        .or(own)
        .filter(|a| !a.is_empty())
        .unwrap_or(SYSTEM_ACTOR)
        .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn record_error(e: impl std::fmt::Display) -> StoreError {
    StoreError::Log(e.to_string())
}

/// The chain's last record, as `(seq, hash)`.
pub(crate) fn head(conn: &Connection) -> Result<Option<(u64, String)>, StoreError> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, hash FROM log ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(seq, hash)| (u64::try_from(seq).unwrap_or_default(), hash)))
}

/// Appends one record inside the caller's transaction and returns its
/// `(seq, hash)`.
pub(crate) fn append(
    conn: &Connection,
    actor: &str,
    kind: &str,
    subject: Vec<String>,
    body: Map<String, Value>,
) -> Result<(u64, String), StoreError> {
    let last = checked_head(conn)?;
    let record = Record::next(
        last.as_ref().map(|(seq, hash)| (*seq, hash.as_str())),
        now_ms(),
        actor,
        kind,
        subject,
        body,
    );
    push(conn, &record)
}

/// The chain's head, refused unless it is the head the store noted last:
/// a record dropped from the end would otherwise be papered over by the
/// next append, which moves the note.
fn checked_head(conn: &Connection) -> Result<Option<(u64, String)>, StoreError> {
    let actual = head(conn)?;
    let noted = noted_head(conn)?;
    if actual != noted {
        let show = |h: &Option<(u64, String)>| {
            h.as_ref()
                .map_or_else(|| "nothing".to_string(), |(s, h)| format!("seq {s} ({h})"))
        };
        return Err(StoreError::Log(format!(
            "the chain ends at {}, but the store last wrote {}: records were dropped from its \
             end, so nothing more is appended",
            show(&actual),
            show(&noted)
        )));
    }
    Ok(actual)
}

/// Inserts a sealed record and moves the store's own note of the chain's
/// head (`meta.log_head`) to it, in the caller's transaction. Verification
/// compares the two, so a record dropped from the end of the chain shows.
fn push(conn: &Connection, record: &Record) -> Result<(u64, String), StoreError> {
    let (text, hash) = record.seal().map_err(record_error)?;
    let seq = i64::try_from(record.seq).map_err(record_error)?;
    conn.execute(
        "INSERT INTO log (seq, hash, record) VALUES (?1, ?2, ?3)",
        params![seq, hash, text],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('log_head', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [format!("{} {hash}", record.seq)],
    )?;
    Ok((record.seq, hash))
}

/// The head the store last wrote, from `meta.log_head`.
pub(crate) fn noted_head(conn: &Connection) -> Result<Option<(u64, String)>, StoreError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'log_head'", [], |r| {
            r.get(0)
        })
        .optional()?;
    value
        .map(|v| {
            v.split_once(' ')
                .and_then(|(seq, hash)| Some((seq.parse().ok()?, hash.to_string())))
                .ok_or_else(|| StoreError::BadMeta(format!("log_head {v:?}")))
        })
        .transpose()
}

/// Opens the chain: called once, when the store is created or upgraded,
/// in that transaction. `journal_seq` is the journal entry the chain
/// starts after (entries before it predate the log).
pub(crate) fn genesis(
    conn: &Connection,
    store_id: &str,
    journal_seq: i64,
) -> Result<(), StoreError> {
    if head(conn)?.is_some() {
        return Ok(());
    }
    let mut body = Map::new();
    body.insert("store".into(), store_id.into());
    body.insert("journal_seq".into(), journal_seq.into());
    append(
        conn,
        &actor_or(None),
        GENESIS,
        vec![format!("store:{store_id}")],
        body,
    )?;
    Ok(())
}

fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".into(), other);
            map
        }
    }
}

/// A node key (`<folder>#<id>`) as the node's subject.
fn node_of_key(key: &str) -> String {
    node_ref(key.rsplit_once('#').map_or(key, |(_, id)| id))
}

/// A lease scope's subject: `role:<name>`, `run:<name>`, or a profile C
/// node's `node:<uuid>` as it is; a path as `path:<path>` (format 9.1).
fn scope_ref(scope: &str) -> String {
    let named = ["role:", "run:", "node:"];
    if named.iter().any(|p| scope.starts_with(p)) {
        scope.to_string()
    } else {
        format!("path:{scope}")
    }
}

/// Subjects for a lease: its node, when it holds one, its scope and itself.
fn lease_subjects(coordinator: &Coordinator, lease: &LeaseId, out: &mut Vec<String>) {
    if let Some(l) = coordinator.lease(lease) {
        if let Some(node) = &l.node {
            out.push(node_of_key(node));
        }
        out.push(scope_ref(l.scope.as_str()));
    }
    out.push(format!("lease:{lease}"));
}

/// The kinds that record coordinator journal entries (format 9.2). Only
/// these carry this store's `journal` numbers: any other kind's body,
/// imported ones included, stays opaque.
pub const COORDINATOR_KINDS: &[&str] = &[
    "lease.claim",
    "lease.renew",
    "lease.release",
    "lease.expire",
    "lease.reconcile",
    "leaf.register",
    "changeset.commit",
    "changeset.abandon",
    "main.observe",
    "request.prune",
    "alias.issue",
    "work.status",
];

/// The record kind for a coordinator command.
pub fn command_kind(command: &Command) -> &'static str {
    match command {
        Command::Register { .. } => "leaf.register",
        Command::Claim { .. } => "lease.claim",
        Command::Heartbeat { .. } => "lease.renew",
        Command::Release { .. } => "lease.release",
        Command::Expire { .. } | Command::ExpireDue { .. } => "lease.expire",
        Command::Commit { .. } => "changeset.commit",
        Command::Reconcile { .. } => "lease.reconcile",
        Command::ObserveMain { .. } => "main.observe",
        Command::Abandon { .. } => "changeset.abandon",
        Command::PruneRequests { .. } => "request.prune",
        Command::IssueAlias { .. } => "alias.issue",
        Command::SetWork { .. } => "work.status",
    }
}

/// What a command's record is about, read from the command, its outcome
/// and the state after it.
fn command_subjects(command: &Command, outcome: &Outcome, after: &Coordinator) -> Vec<String> {
    let mut out = Vec::new();
    match (command, outcome) {
        (Command::Register { leaf, .. }, _) | (Command::SetWork { leaf, .. }, _) => {
            out.push(scope_ref(leaf.as_str()));
        }
        (Command::Claim { scope, node, .. }, outcome) => match outcome {
            Outcome::Lease { lease } => lease_subjects(after, lease, &mut out),
            _ => {
                if let Some(node) = node {
                    out.push(node_of_key(node));
                }
                out.push(scope_ref(scope.as_str()));
            }
        },
        (Command::Heartbeat { lease, .. }, _)
        | (Command::Release { lease, .. }, _)
        | (Command::Expire { lease, .. }, _) => lease_subjects(after, lease, &mut out),
        (Command::Commit { lease, .. }, outcome) => {
            lease_subjects(after, lease, &mut out);
            if let Outcome::Committed { changeset } = outcome {
                out.push(format!("changeset:{changeset}"));
            }
        }
        (Command::Reconcile { .. }, Outcome::Reconciled { lease, .. }) => {
            lease_subjects(after, lease, &mut out);
        }
        (Command::ExpireDue { .. }, Outcome::Expired { leases }) => {
            for lease in leases {
                lease_subjects(after, lease, &mut out);
            }
        }
        (Command::Abandon { changeset }, outcome) => {
            out.push(format!("changeset:{changeset}"));
            if let Outcome::Abandoned { changesets } = outcome {
                out.extend(
                    changesets
                        .iter()
                        .filter(|c| *c != changeset)
                        .map(|c| format!("changeset:{c}")),
                );
            }
        }
        (Command::IssueAlias { node, .. }, _) => out.push(node_ref(node)),
        _ => {}
    }
    out
}

/// The acting actor a command names, if any.
fn command_actor(command: &Command) -> Option<&str> {
    match command {
        Command::Claim { owner, .. }
        | Command::Heartbeat { owner, .. }
        | Command::Release { owner, .. }
        | Command::Commit { owner, .. } => Some(owner),
        Command::SetWork { actor, .. } => Some(actor),
        _ => None,
    }
}

/// Appends the record for journal entry `journal`.
pub(crate) fn append_command(
    conn: &Connection,
    journal: i64,
    command: &Command,
    outcome: &Outcome,
    after: &Coordinator,
) -> Result<(), StoreError> {
    let mut body = Map::new();
    body.insert("journal".into(), journal.into());
    body.insert("command".into(), serde_json::to_value(command)?);
    body.insert("outcome".into(), serde_json::to_value(outcome)?);
    append(
        conn,
        &actor_or(command_actor(command)),
        command_kind(command),
        command_subjects(command, outcome, after),
        body,
    )?;
    Ok(())
}

/// Appends a mail record: `kind` with `body`, about `subject`, by `actor`
/// unless the process names one.
pub(crate) fn append_mail(
    conn: &Connection,
    actor: &str,
    kind: &str,
    subject: Vec<String>,
    body: Value,
) -> Result<(), StoreError> {
    append(conn, &actor_or(Some(actor)), kind, subject, as_object(body))?;
    Ok(())
}

/// A digest of short text (an ack's outcome), so the log carries no text
/// an agent wrote.
pub(crate) fn text_digest(text: &str) -> String {
    mdkg_format::Digest::of_bytes(text.as_bytes()).to_string()
}

/// Every record's canonical text, in seq order.
pub(crate) fn texts(conn: &Connection) -> Result<Vec<(u64, String, String)>, StoreError> {
    let mut stmt = conn.prepare("SELECT seq, hash, record FROM log ORDER BY seq")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, hash, text) = row?;
        out.push((u64::try_from(seq).unwrap_or_default(), hash, text));
    }
    Ok(out)
}

/// A record from before the log (0.6.0's `events.jsonl`, checkpoints),
/// for [`crate::Store::import_records`]: written with its own time and
/// actor.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub ts_ms: u64,
    pub actor: String,
    pub kind: String,
    pub subject: Vec<String>,
    pub body: Map<String, Value>,
}

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOutcome {
    /// Records appended, the `log.import` marker included.
    pub appended: u64,
    /// The import id was imported before: nothing was appended.
    pub already: bool,
    /// Records skipped because the store already holds one with the same
    /// `migrate_digest` (another migration of this repository imported
    /// it: main's, when an unmerged branch migrates after it).
    pub skipped: u64,
}

/// The body key that names an imported record's content across imports.
pub const MIGRATE_DIGEST: &str = "migrate_digest";

/// Every `migrate_digest` the chain holds, read in the caller's
/// transaction.
fn imported_digests(conn: &Connection) -> Result<std::collections::HashSet<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT record FROM log WHERE instr(record, ?1) > 0")?;
    let rows = stmt.query_map([format!("\"{MIGRATE_DIGEST}\":")], |r| {
        r.get::<_, String>(0)
    })?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        let (record, _) = Record::parse(&row?).map_err(record_error)?;
        if let Some(d) = record.body.get(MIGRATE_DIGEST).and_then(Value::as_str) {
            out.insert(d.to_string());
        }
    }
    Ok(out)
}

/// Appends an import inside the caller's transaction: a `log.import`
/// marker, then the records, unless `import_id` was imported before. A
/// record whose `migrate_digest` the chain already holds is skipped, judged
/// in this same transaction: a rollback that took those records away
/// before it means they're imported again.
pub(crate) fn import(
    conn: &Connection,
    import_id: &str,
    records: &[Imported],
) -> Result<ImportOutcome, StoreError> {
    let key = format!("import:{import_id}");
    let seen = conn
        .query_row("SELECT 1 FROM meta WHERE key = ?1", [&key], |_| Ok(()))
        .optional()?
        .is_some();
    if seen {
        return Ok(ImportOutcome {
            appended: 0,
            already: true,
            skipped: 0,
        });
    }
    let mut known = imported_digests(conn)?;
    let mut fresh: Vec<&Imported> = Vec::new();
    for r in records {
        match r.body.get(MIGRATE_DIGEST).and_then(Value::as_str) {
            // Already in the chain, or earlier in this same import.
            Some(d) if !known.insert(d.to_string()) => {}
            _ => fresh.push(r),
        }
    }
    let skipped = (records.len() - fresh.len()) as u64;
    let fresh_len = fresh.len() as u64;
    let mut marker = Map::new();
    marker.insert("import_id".into(), import_id.into());
    // `count` is what follows the marker (verify reads that many as the
    // import), so skipped records aren't in it.
    marker.insert("count".into(), (fresh.len() as u64).into());
    marker.insert("skipped".into(), skipped.into());
    append(conn, &actor_or(None), "log.import", Vec::new(), marker)?;
    for r in fresh {
        let last = head(conn)?;
        let record = Record::next(
            last.as_ref().map(|(seq, hash)| (*seq, hash.as_str())),
            r.ts_ms,
            &r.actor,
            &r.kind,
            r.subject.clone(),
            r.body.clone(),
        );
        push(conn, &record)?;
    }
    let (_, head_hash) = head(conn)?.unwrap_or_default();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![key, head_hash],
    )?;
    Ok(ImportOutcome {
        appended: fresh_len + 1,
        already: false,
        skipped,
    })
}

/// What a compaction summarized: the records since the last
/// `log.checkpoint`, now cited by a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// The `log.checkpoint` record's seq and hash.
    pub seq: u64,
    pub hash: String,
    /// The range it summarizes, and its first and last records' hashes.
    pub from: u64,
    pub to: u64,
    pub first: String,
    pub last: String,
    /// At most 150 words.
    pub summary: String,
}

/// The most words a checkpoint summary holds.
pub const SUMMARY_WORDS: usize = 150;

/// The summary of `records` (`from` to `to`): counts by kind, the actors,
/// and the first and last hashes, cut to [`SUMMARY_WORDS`] by dropping
/// the rarest kinds and the last actors first.
pub fn summarize(records: &[(Record, String)]) -> String {
    let (Some((first, first_hash)), Some((last, last_hash))) = (records.first(), records.last())
    else {
        return String::new();
    };
    let mut kinds: Vec<(String, usize)> = Vec::new();
    let mut actors: Vec<String> = Vec::new();
    for (r, _) in records {
        match kinds.iter_mut().find(|(k, _)| *k == r.kind) {
            Some((_, n)) => *n += 1,
            None => kinds.push((r.kind.clone(), 1)),
        }
        if !actors.contains(&r.actor) {
            actors.push(r.actor.clone());
        }
    }
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let text = |kinds: &[(String, usize)], actors: &[String], more: bool| {
        let k: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} {n}")).collect();
        format!(
            "Records {} to {} ({}) by {}{}. Kinds: {}{}. First {first_hash}, last {last_hash}.",
            first.seq,
            last.seq,
            records.len(),
            actors.join(", "),
            if more { " and others" } else { "" },
            k.join(", "),
            if more { ", and more" } else { "" },
        )
    };
    let (mut k, mut a) = (kinds.len(), actors.len());
    loop {
        let more = k < kinds.len() || a < actors.len();
        let summary = text(&kinds[..k], &actors[..a], more);
        if summary.split_whitespace().count() <= SUMMARY_WORDS || (k <= 1 && a <= 1) {
            return summary;
        }
        if k > 1 && (k >= a || a <= 1) {
            k -= 1;
        } else {
            a -= 1;
        }
    }
}

/// Appends a `log.checkpoint` summarizing every record since the last
/// one, inside the caller's transaction; none when nothing came since.
pub(crate) fn checkpoint(conn: &Connection) -> Result<Option<Checkpoint>, StoreError> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'last_checkpoint'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|v| {
            v.parse()
                .map_err(|_| StoreError::BadMeta(format!("last_checkpoint {v}")))
        })
        .transpose()?;
    let from = last.unwrap_or(0) + 1;
    let mut stmt = conn.prepare("SELECT record FROM log WHERE seq >= ?1 ORDER BY seq")?;
    let rows = stmt.query_map([from], |r| r.get::<_, String>(0))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(Record::parse(&row?).map_err(record_error)?);
    }
    let (Some((first, first_hash)), Some((to, last_hash))) = (
        records.first().map(|(r, h)| (r.seq, h.clone())),
        records.last().map(|(r, h)| (r.seq, h.clone())),
    ) else {
        return Ok(None);
    };
    let summary = summarize(&records);
    let mut body = Map::new();
    body.insert("from".into(), first.into());
    body.insert("to".into(), to.into());
    body.insert("first".into(), first_hash.clone().into());
    body.insert("last".into(), last_hash.clone().into());
    body.insert("count".into(), (records.len() as u64).into());
    body.insert("summary".into(), summary.clone().into());
    let (seq, hash) = append(conn, &actor_or(None), "log.checkpoint", Vec::new(), body)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('last_checkpoint', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [seq.to_string()],
    )?;
    Ok(Some(Checkpoint {
        seq,
        hash,
        from: first,
        to,
        first: first_hash,
        last: last_hash,
        summary,
    }))
}
