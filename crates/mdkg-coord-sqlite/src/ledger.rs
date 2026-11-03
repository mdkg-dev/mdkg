//! The flow tables (edd-01a1878e, "mdkg flow"): which nodes a message is
//! about, how it supersedes older mail and when it expires (A23 to A25),
//! and the ledger's entries (A27).
//!
//! - They arrive with store schema 4 (decided 2026-10-29): one bump for all
//!   of the flow work, so a build that predates it refuses the store
//!   instead of writing mail or finishing work around the flow rules.
//!   Opening a schema 3 store upgrades it, copying it first.
//! - Supersession and expiry are acks by `mdkg` (`via` `supersede` or
//!   `expiry`), so delivery states stay `delivered` and `acked`. A system
//!   ack needs no listing first: nobody was shown what it retires.
//! - Every change is made inside the caller's mail transaction, with its
//!   log record, so a send and what it supersedes land together or not at
//!   all.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::store::{Store, StoreError};

/// Part of store schema 4, created on every open (`CREATE TABLE IF NOT
/// EXISTS`), like the mail tables. `entries_round_request` is unique: one
/// round per review request (A27 rule 8); it replaces the branch-only
/// plain `entries_request`, dropped first since schema 4 is unreleased (a
/// development store holding two rounds for one request then refuses to
/// open, and is recreated). `msg_links` binds a message to nodes (by
/// UUID, so a retype keeps it) and, with `entry`, to ledger entries;
/// `msg_meta` holds what a send superseded, its slot, its expiry, when its
/// human hold ends (`hold_ms`) and when its expiry notice went
/// (`notified_ms`); `msg_deferred` holds system acks waiting for a human
/// hold to end; `entries` and `entry_events` are the ledger.
pub(crate) const LEDGER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS msg_links (
    seq INTEGER NOT NULL,
    node_uuid TEXT NOT NULL,
    node_id TEXT NOT NULL,
    entry TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (seq, node_uuid, entry)
);
CREATE INDEX IF NOT EXISTS msg_links_node ON msg_links (node_uuid, seq);
CREATE TABLE IF NOT EXISTS msg_meta (
    seq INTEGER PRIMARY KEY,
    supersedes TEXT,
    slot TEXT,
    slot_node TEXT,
    expires_ms INTEGER,
    hold_ms INTEGER,
    notified_ms INTEGER
);
CREATE INDEX IF NOT EXISTS msg_meta_slot ON msg_meta (slot, slot_node);
CREATE INDEX IF NOT EXISTS msg_meta_expiry ON msg_meta (expires_ms);
CREATE TABLE IF NOT EXISTS msg_deferred (
    seq INTEGER NOT NULL,
    mailbox TEXT NOT NULL,
    via TEXT NOT NULL,
    outcome TEXT NOT NULL,
    due_ms INTEGER NOT NULL,
    PRIMARY KEY (seq, mailbox)
);
CREATE INDEX IF NOT EXISTS msg_deferred_due ON msg_deferred (due_ms);
CREATE TABLE IF NOT EXISTS entries (
    node_uuid TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    severity TEXT,
    slot TEXT,
    blocking INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL,
    text TEXT,
    data TEXT NOT NULL,
    opened_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (node_uuid, entry_id)
);
CREATE TABLE IF NOT EXISTS entry_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    node_uuid TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    op TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT NOT NULL,
    actor TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    sha TEXT,
    message_id TEXT,
    turn_id TEXT,
    note TEXT,
    log_seq INTEGER
);
CREATE INDEX IF NOT EXISTS entry_events_entry ON entry_events (node_uuid, entry_id, seq);
DROP INDEX IF EXISTS entries_request;
CREATE UNIQUE INDEX IF NOT EXISTS entries_round_request ON entries (json_extract(data, '$.request'))
    WHERE kind = 'review';
CREATE INDEX IF NOT EXISTS entries_open_round ON entries (json_extract(data, '$.request'))
    WHERE kind = 'review' AND state != 'closed';
";

/// Creates the flow tables inside the store's open transaction: for a new
/// store, and for one this open upgrades to schema 4 (after its copy).
pub(crate) fn create(tx: &Transaction<'_>) -> Result<(), StoreError> {
    tx.execute_batch(LEDGER_SCHEMA)?;
    Ok(())
}

/// Why a flow read or write failed.
#[derive(Debug)]
pub enum LedgerError {
    Store(StoreError),
    /// Not a node id with a UUID (`<type>-<uuid>`).
    BadNode(String),
    /// Not an entry id (`F3`, `Q1`, `R2`).
    BadEntry(String),
    /// Not a slot name (`[a-z0-9._:-]{1,200}`), or one reserved to mdkg.
    BadSlot(String),
    /// Not an expiry (`30m`, `12h`, `3d` or `never`, at most 3650 days).
    BadExpiry(String),
    /// No message has this id.
    UnknownMessage(String),
    /// A send may supersede only its own sender's messages.
    NotSender(String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::BadNode(_) => f.write_str("not a node id with a UUID"),
            Self::BadEntry(_) => f.write_str("not an entry id (a letter and a number)"),
            Self::BadSlot(_) => f.write_str(
                "not a slot name: a-z, 0-9, ., _, : and -, up to 200; `fault:` is mdkg's",
            ),
            Self::BadExpiry(_) => {
                f.write_str("not an expiry: 30m, 12h, 3d or never, at most 3650 days")
            }
            Self::UnknownMessage(id) => write!(f, "no message {id}"),
            Self::NotSender(id) => write!(f, "message {id} is another sender's"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<StoreError> for LedgerError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

fn ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or_default()
}

fn sql_ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

const MINUTE: u64 = 60 * 1000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const MAX_EXPIRY: u64 = 3650 * DAY;

/// How long a delivery stays unacked before mdkg acks it (A25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// This many milliseconds after sending.
    After(u64),
    Never,
}

impl Expiry {
    /// `<n>m`, `<n>h`, `<n>d` (n from 1, no leading zero, at most 3650
    /// days) or `never`.
    pub fn parse(s: &str) -> Option<Self> {
        if s == "never" {
            return Some(Self::Never);
        }
        let unit = match s.as_bytes().last()? {
            b'm' => MINUTE,
            b'h' => HOUR,
            b'd' => DAY,
            _ => return None,
        };
        let digits = &s[..s.len() - 1];
        if digits.is_empty()
            || digits.starts_with('0')
            || digits.len() > 7
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let after = digits.parse::<u64>().ok()?.checked_mul(unit)?;
        (after <= MAX_EXPIRY).then_some(Self::After(after))
    }
}

impl std::fmt::Display for Expiry {
    /// The largest unit that divides it: `3d`, `36h`, `90m`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Never => f.write_str("never"),
            Self::After(d) if d > 0 && d % DAY == 0 => write!(f, "{}d", d / DAY),
            Self::After(d) if d > 0 && d % HOUR == 0 => write!(f, "{}h", d / HOUR),
            Self::After(d) => write!(f, "{}m", d.div_ceil(MINUTE)),
        }
    }
}

/// A kind's default (A25's table). Kinds whose mail an entry or a fault's
/// clearing settles never expire by time, and so does any kind this build
/// doesn't know.
pub fn default_expiry(kind: &str) -> Expiry {
    match kind {
        "receipt" => Expiry::After(DAY),
        "note" => Expiry::After(3 * DAY),
        "handoff" | "review_request" => Expiry::After(7 * DAY),
        _ => Expiry::Never,
    }
}

/// Defaults per kind, with the settings layer's `mail.expiry.<kind>` over
/// them. A fault never expires by time, whatever the settings say: only the
/// agent's next successful turn clears it (A26).
#[derive(Debug, Clone, Default)]
pub struct ExpiryPolicy {
    pub overrides: BTreeMap<String, Expiry>,
}

impl ExpiryPolicy {
    pub fn for_kind(&self, kind: &str) -> Expiry {
        if kind == "fault" {
            return Expiry::Never;
        }
        self.overrides
            .get(kind)
            .copied()
            .unwrap_or_else(|| default_expiry(kind))
    }

    /// When a message of `kind` sent at `sent_ms` expires: the sender's own
    /// expiry if it gave one (never for a fault), else the kind's.
    pub fn expires_ms(&self, kind: &str, sent_ms: u64, explicit: Option<Expiry>) -> Option<u64> {
        let chosen = match explicit {
            Some(e) if kind != "fault" => e,
            _ => self.for_kind(kind),
        };
        match chosen {
            Expiry::Never => None,
            Expiry::After(d) => Some(sent_ms.saturating_add(d)),
        }
    }
}

/// A node a message names: its UUID (the key, as format §9.1 matches node
/// subjects) and its id when the message was sent (for display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeLink {
    pub uuid: String,
    pub id: String,
}

impl NodeLink {
    /// From a format 0.7 node id, `<type>-<uuid>`.
    pub fn of(id: &str) -> Result<Self, LedgerError> {
        let parsed = mdkg_format::NodeId::parse(id).map_err(|_| LedgerError::BadNode(id.into()))?;
        Ok(Self {
            uuid: parsed.uuid().to_string(),
            id: parsed.to_string(),
        })
    }
}

/// An entry id: one of the ledger's letters, then a number from 1.
pub fn check_entry_id(id: &str) -> Result<(), LedgerError> {
    let bad = || LedgerError::BadEntry(id.into());
    let (letter, number) = id.split_at_checked(1).ok_or_else(bad)?;
    if !matches!(letter, "Q" | "D" | "F" | "I" | "V" | "R")
        || number.is_empty()
        || number.len() > 9
        || number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    Ok(())
}

/// A slot name, long enough for mdkg's `fault:<agent>:<code>` with any
/// agent name. Slots under `fault:` are mdkg's own (A26), so only an
/// `mdkg:` sender may use one.
pub fn check_slot(slot: &str, sender: &str) -> Result<(), LedgerError> {
    let ok = (1..=200).contains(&slot.len())
        && slot
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'-'));
    if !ok || (slot.starts_with("fault:") && !sender.starts_with("mdkg:")) {
        return Err(LedgerError::BadSlot(slot.into()));
    }
    Ok(())
}

/// What a send adds to a message (A23 to A25).
#[derive(Debug, Clone, Default)]
pub struct SendFlow {
    /// The nodes it's about; the first is its primary node.
    pub nodes: Vec<NodeLink>,
    /// Entries it opens, answers or updates, each on its node.
    pub entries: Vec<(NodeLink, String)>,
    /// Messages it replaces outright (its sender's own).
    pub supersedes: Vec<String>,
    /// Replaces its sender's earlier messages with the same slot and the
    /// same primary node.
    pub slot: Option<String>,
    /// The sender's own expiry; the kind's when `None`.
    pub expires: Option<Expiry>,
    /// A review request's round range, `(base, head)` as full commit ids:
    /// part of the request, so a retry naming another range is another
    /// request (Terra's #11150). The round records it, not this flow.
    pub range: Option<(String, String)>,
}

impl SendFlow {
    /// Refuses a malformed flow before any transaction: a bad slot or
    /// entry id. Nodes are checked when they're made ([`NodeLink::of`]).
    pub fn check(&self, sender: &str) -> Result<(), LedgerError> {
        if let Some(slot) = &self.slot {
            check_slot(slot, sender)?;
        }
        for (_, entry) in &self.entries {
            check_entry_id(entry)?;
        }
        Ok(())
    }

    /// The primary node's UUID, the key a slot is scoped by.
    fn primary(&self) -> Option<&str> {
        self.nodes.first().map(|n| n.uuid.as_str())
    }
}

/// What [`on_send`] did: the messages it superseded (those with a delivery
/// it acked), when the new message expires, and its acks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendEffects {
    pub superseded: Vec<String>,
    pub expires_ms: Option<u64>,
    pub acks: Vec<SystemAck>,
}

/// One delivery mdkg acked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemAck {
    pub seq: u64,
    pub message_id: String,
    pub mailbox: String,
    /// `supersede` or `expiry`.
    pub via: String,
    pub outcome: String,
}

/// A delivery to ack and why.
struct Target {
    seq: u64,
    message_id: String,
    mailbox: String,
    outcome: String,
}

/// Records the flow of a message just stored as `seq`, inside the mail
/// transaction that stored it, whose schema check covers these tables:
/// its node and entry links, then what it supersedes, acked by mdkg, then
/// its expiry and its human hold. Called only for a new message, never a
/// retried one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_send(
    tx: &Transaction<'_>,
    seq: u64,
    message_id: &str,
    sender: &str,
    kind: &str,
    sent_ms: u64,
    human_hold_days: u32,
    flow: &SendFlow,
    policy: &ExpiryPolicy,
    now_ms: u64,
    ack_grace_ms: u64,
) -> Result<SendEffects, LedgerError> {
    flow.check(sender)?;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let links = flow
        .nodes
        .iter()
        .map(|n| (n, String::new()))
        .chain(flow.entries.iter().map(|(n, e)| (n, e.clone())));
    for (node, entry) in links {
        if seen.insert((node.uuid.clone(), entry.clone())) {
            tx.execute(
                "INSERT INTO msg_links (seq, node_uuid, node_id, entry) VALUES (?1, ?2, ?3, ?4)",
                params![sql_ms(seq), node.uuid, node.id, entry],
            )?;
        }
    }
    // Only where the new message goes: a recipient it doesn't reach keeps
    // the old one (review of 2362c0be, finding 7). mdkg's own fault slots
    // are the exception: one fault stays open per agent and code even
    // when its `reports_to` changes (A24).
    let everywhere = sender.starts_with("mdkg:")
        && flow
            .slot
            .as_deref()
            .is_some_and(|s| s.starts_with("fault:"));
    // Each replaced delivery: `(seq, message id, mailbox, hold_ms)`, where
    // `hold_ms` is `None` when the message recorded none.
    let mut rows: Vec<(i64, String, String, Option<i64>)> = Vec::new();
    // Named messages, which must be the sender's: one lookup per name the
    // send gave, never per message in a slot's history.
    let mut named: BTreeSet<i64> = BTreeSet::new();
    for id in &flow.supersedes {
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, sender FROM msg_messages WHERE message_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (old, from) = row.ok_or_else(|| LedgerError::UnknownMessage(id.clone()))?;
        if from != sender {
            return Err(LedgerError::NotSender(id.clone()));
        }
        if ms(old) != seq {
            named.insert(old);
        }
    }
    // The deliveries to replace, in one query that joins the replaced
    // messages (the named ones, then the slot's earlier ones from this
    // sender) to their unacked deliveries, kept to the mailboxes the new
    // message reaches unless it supersedes everywhere (Terra's review of
    // e821ea6e..a1412008, finding 7).
    let named_json = serde_json::to_string(&named)?;
    {
        let mut stmt = tx.prepare(SUPERSEDED_DELIVERIES)?;
        let mapped = stmt.query_map(
            params![
                flow.slot,
                flow.primary(),
                sender,
                sql_ms(seq),
                named_json,
                everywhere
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        for row in mapped {
            rows.push(row?);
        }
    }
    let reaches: BTreeSet<String> = {
        let mut stmt = tx.prepare("SELECT mailbox FROM msg_deliveries WHERE seq = ?1")?;
        let rows = stmt.query_map(params![sql_ms(seq)], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let outcome = format!("superseded by {message_id}");
    let mut targets = Vec::new();
    for (old, old_id, mailbox, hold) in rows {
        let target = Target {
            seq: ms(old),
            message_id: old_id,
            mailbox,
            outcome: outcome.clone(),
        };
        // A message with no hold recorded (sent before 0.7.1) counts as
        // held, as `in_hold` reads it.
        if is_human(&target.mailbox) && hold.is_none_or(|h| ms(h) > now_ms) {
            defer(tx, &target, "supersede")?;
            continue;
        }
        targets.push(target);
    }
    let acks = system_ack_in(tx, targets, "supersede", now_ms, ack_grace_ms)?;
    let mut superseded: Vec<String> = Vec::new();
    for a in &acks {
        if !superseded.contains(&a.message_id) {
            superseded.push(a.message_id.clone());
        }
    }
    let expires_ms = policy.expires_ms(kind, sent_ms, flow.expires);
    // The human hold runs from the send, whatever the body's retention
    // (A25), so it is kept here rather than read from `purge_after_ms`.
    let hold_ms = reaches
        .iter()
        .any(|m| is_human(m))
        .then(|| sent_ms.saturating_add(u64::from(human_hold_days) * DAY));
    if !superseded.is_empty() || flow.slot.is_some() || expires_ms.is_some() || hold_ms.is_some() {
        tx.execute(
            "INSERT INTO msg_meta (seq, supersedes, slot, slot_node, expires_ms, hold_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                sql_ms(seq),
                (!superseded.is_empty())
                    .then(|| serde_json::to_string(&superseded))
                    .transpose()?,
                flow.slot,
                flow.slot.as_ref().and(flow.primary()),
                expires_ms.map(sql_ms),
                hold_ms.map(sql_ms),
            ],
        )?;
    }
    Ok(SendEffects {
        superseded,
        expires_ms,
        acks,
    })
}

/// The unacked deliveries a send supersedes (A24): those of the messages
/// it names (`?5`, a JSON array of seqs) and of its sender's earlier
/// messages in the same slot and primary node, joined in one query and
/// kept to the mailboxes the new message (`?4`) reaches unless `?6`.
const SUPERSEDED_DELIVERIES: &str = "SELECT d.seq, m.message_id, d.mailbox, mm.hold_ms
         FROM msg_deliveries d
         JOIN msg_messages m ON m.seq = d.seq
         LEFT JOIN msg_meta mm ON mm.seq = d.seq
         WHERE d.state != 'acked' AND d.seq < ?4
           AND d.seq IN (
               SELECT value FROM json_each(?5)
               UNION
               SELECT s.seq FROM msg_meta s JOIN msg_messages sm ON sm.seq = s.seq
               WHERE ?1 IS NOT NULL AND s.slot = ?1 AND s.slot_node IS ?2
                 AND sm.sender = ?3 AND s.seq < ?4)
           AND (?6 OR d.mailbox IN (SELECT mailbox FROM msg_deliveries WHERE seq = ?4))
         ORDER BY d.seq, d.mailbox";

fn is_human(mailbox: &str) -> bool {
    mailbox.starts_with("human:")
}

/// When message `seq`'s human hold ends, if one was recorded at send.
fn hold_of(conn: &Connection, seq: u64) -> Result<Option<u64>, LedgerError> {
    let hold: Option<Option<i64>> = conn
        .query_row(
            "SELECT hold_ms FROM msg_meta WHERE seq = ?1",
            params![sql_ms(seq)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(hold.flatten().map(ms))
}

/// Records a system ack that waits for a human hold to end (A25,
/// "Deferred acks"). A delivery with no recorded hold (mail from before
/// 0.7.1) is left to its human: nothing is deferred for it.
fn defer(tx: &Transaction<'_>, t: &Target, via: &str) -> Result<(), LedgerError> {
    if let Some(due) = hold_of(tx, t.seq)? {
        tx.execute(
            "INSERT INTO msg_deferred (seq, mailbox, via, outcome, due_ms)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (seq, mailbox) DO NOTHING",
            params![sql_ms(t.seq), t.mailbox, via, t.outcome, sql_ms(due)],
        )?;
    }
    Ok(())
}

/// Applies the deferred acks that are due, still unacked, with their
/// own `via` and outcome; a delivery its human acked first is skipped.
/// Every due row goes, applied or not.
fn apply_deferred_in(
    tx: &Transaction<'_>,
    now_ms: u64,
    ack_grace_ms: u64,
) -> Result<Vec<SystemAck>, LedgerError> {
    let rows: Vec<(i64, String, String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT f.seq, m.message_id, f.mailbox, f.via, f.outcome FROM msg_deferred f
             JOIN msg_messages m ON m.seq = f.seq
             WHERE f.due_ms <= ?1 ORDER BY f.seq, f.mailbox",
        )?;
        let mapped = stmt.query_map(params![sql_ms(now_ms)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    let mut acks = Vec::new();
    for via in ["supersede", "expiry"] {
        let targets: Vec<Target> = rows
            .iter()
            .filter(|r| r.3 == via)
            .map(|(seq, id, mailbox, _, outcome)| Target {
                seq: ms(*seq),
                message_id: id.clone(),
                mailbox: mailbox.clone(),
                outcome: outcome.clone(),
            })
            .collect();
        acks.extend(system_ack_in(tx, targets, via, now_ms, ack_grace_ms)?);
    }
    tx.execute(
        "DELETE FROM msg_deferred WHERE due_ms <= ?1",
        params![sql_ms(now_ms)],
    )?;
    Ok(acks)
}

/// Whether message `seq`'s human hold is still running: until its
/// `hold_ms`, and always for a message with none recorded (mail sent
/// before 0.7.1), so a system ack never takes a human's mail early.
fn in_hold(conn: &Connection, seq: u64, now_ms: u64) -> Result<bool, LedgerError> {
    Ok(hold_of(conn, seq)?.is_none_or(|h| h > now_ms))
}

/// Acks `targets` as mdkg, `via` the given mechanism, without the listing
/// an actor's ack needs. Callers never pass a human delivery whose hold is
/// running. A body goes only once every delivery is acked (A25). One
/// `msg.ack` record per distinct outcome, in the record format an ack
/// writes (`by: "mdkg"`), so readers see an ordinary ack. With an ack
/// grace, a body that would go is kept that long, as after an actor's ack.
fn system_ack_in(
    tx: &Transaction<'_>,
    targets: Vec<Target>,
    via: &str,
    now_ms: u64,
    ack_grace_ms: u64,
) -> Result<Vec<SystemAck>, LedgerError> {
    let mut acks: Vec<SystemAck> = Vec::new();
    for t in targets {
        let changed = tx.execute(
            "UPDATE msg_deliveries SET state = 'acked', acked_by = 'mdkg', acked_ms = ?3,
                 outcome = ?4, acked_via = ?5
             WHERE seq = ?1 AND mailbox = ?2 AND state != 'acked'",
            params![sql_ms(t.seq), t.mailbox, sql_ms(now_ms), t.outcome, via],
        )?;
        if changed > 0 {
            acks.push(SystemAck {
                seq: t.seq,
                message_id: t.message_id,
                mailbox: t.mailbox,
                via: via.to_string(),
                outcome: t.outcome,
            });
        }
    }
    let mut purged: BTreeSet<String> = BTreeSet::new();
    let seqs: BTreeSet<(u64, &str)> = acks
        .iter()
        .map(|a| (a.seq, a.message_id.as_str()))
        .collect();
    for (seq, id) in seqs {
        if purge_if_done(tx, seq, now_ms, ack_grace_ms)? {
            purged.insert(id.to_string());
        }
    }
    // Grouped by outcome, in the order first seen.
    let mut groups: Vec<(&str, Vec<&SystemAck>)> = Vec::new();
    for a in &acks {
        match groups.iter_mut().find(|(o, _)| *o == a.outcome) {
            Some((_, group)) => group.push(a),
            None => groups.push((a.outcome.as_str(), vec![a])),
        }
    }
    for (outcome, group) in groups {
        let mut subject: Vec<String> = Vec::new();
        let mut group_purged: Vec<&str> = Vec::new();
        for a in &group {
            let s = format!("msg:{}", a.message_id);
            if !subject.contains(&s) {
                subject.push(s);
            }
            if purged.contains(&a.message_id) && !group_purged.contains(&a.message_id.as_str()) {
                group_purged.push(&a.message_id);
            }
        }
        let acked: Vec<Value> = group
            .iter()
            .map(|a| json!({"message_id": a.message_id, "mailbox": a.mailbox}))
            .collect();
        crate::log::append_mail(
            tx,
            crate::log::SYSTEM_ACTOR,
            "msg.ack",
            subject,
            json!({
                "acked": acked,
                "by": crate::log::SYSTEM_ACTOR,
                "via": via,
                "outcome_digest": crate::log::text_digest(outcome),
                "purged": group_purged,
            }),
        )?;
    }
    Ok(acks)
}

/// Deletes the body of message `seq` when a system ack ends it: its
/// retention is `purge_on_ack`, **every** delivery is acked, and any human
/// hold is over. Stricter than an actor's ack, which may leave a human's
/// delivery unacked past its hold (review of 4e8bee7e..1afc3d3a, finding
/// 1): a system ack never purges a body someone hasn't acked. With an ack
/// grace, the body is kept that long instead, and `purge_due` purges it
/// (friction item 72).
fn purge_if_done(
    tx: &Transaction<'_>,
    seq: u64,
    now_ms: u64,
    grace_ms: u64,
) -> Result<bool, LedgerError> {
    let unacked: i64 = tx.query_row(
        "SELECT COUNT(*) FROM msg_deliveries WHERE seq = ?1 AND state != 'acked'",
        params![sql_ms(seq)],
        |r| r.get(0),
    )?;
    let retention: String = tx.query_row(
        "SELECT retention FROM msg_messages WHERE seq = ?1",
        params![sql_ms(seq)],
        |r| r.get(0),
    )?;
    let humans: i64 = tx.query_row(
        "SELECT COUNT(*) FROM msg_deliveries WHERE seq = ?1 AND substr(mailbox, 1, 6) = 'human:'",
        params![sql_ms(seq)],
        |r| r.get(0),
    )?;
    if unacked > 0
        || retention != crate::mail::Retention::PurgeOnAck.to_string()
        || (humans > 0 && in_hold(tx, seq, now_ms)?)
    {
        return Ok(false);
    }
    if grace_ms > 0 {
        crate::mail::keep_for_grace(tx, seq, now_ms, grace_ms)?;
        return Ok(false);
    }
    Ok(tx.execute(
        "DELETE FROM msg_bodies WHERE seq = ?1",
        params![sql_ms(seq)],
    )? > 0)
}

/// A sender to tell that its question or review request expired
/// unanswered (A25). The mail layer sends the note in the transaction
/// that expired it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryNotice {
    pub sender: String,
    pub message_id: String,
    pub kind: String,
    pub nodes: Vec<NodeLink>,
}

/// What [`expire_due_in`] acked, and whom it told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expired {
    pub acks: Vec<SystemAck>,
    pub notices: Vec<ExpiryNotice>,
}

/// Acks, as mdkg `via: "expiry"`, every delivery whose expiry has passed;
/// a `human:*` delivery only once its hold is also over. For each
/// question or review request that expired unanswered and hasn't been
/// noticed, calls `notify` in this same transaction and marks it noticed,
/// so a crash or a failed notice loses the ack too, and a later delivery's
/// expiry sends no second notice (A25, R40).
// Only tests call this yet. It lands in `purge_due` (mail.rs), which every
// mail transaction runs, when a-flow wires the rest of P2's call sites
// (still in 0.7.1's scope); until then kind expiry, deferred supersessions
// and A25's expiry notices don't run (Opus's final pass #11366, 1).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn expire_due_in(
    tx: &Transaction<'_>,
    now_ms: u64,
    ack_grace_ms: u64,
    notify: &mut dyn FnMut(&Transaction<'_>, &ExpiryNotice) -> Result<(), LedgerError>,
) -> Result<Expired, LedgerError> {
    struct Row {
        seq: u64,
        message_id: String,
        mailbox: String,
        expires: u64,
        sent: u64,
        kind: String,
        sender: String,
        hold: Option<u64>,
        notified: bool,
    }
    let rows: Vec<Row> = {
        let mut stmt = tx.prepare(
            "SELECT d.seq, m.message_id, d.mailbox, mm.expires_ms, m.sent_ms, m.kind, m.sender,
                    mm.hold_ms, mm.notified_ms IS NOT NULL
             FROM msg_deliveries d
             JOIN msg_messages m ON m.seq = d.seq
             JOIN msg_meta mm ON mm.seq = d.seq
             WHERE d.state != 'acked' AND mm.expires_ms IS NOT NULL AND mm.expires_ms <= ?1
             ORDER BY d.seq, d.mailbox",
        )?;
        let mapped = stmt.query_map(params![sql_ms(now_ms)], |r| {
            Ok(Row {
                seq: ms(r.get(0)?),
                message_id: r.get(1)?,
                mailbox: r.get(2)?,
                expires: ms(r.get(3)?),
                sent: ms(r.get(4)?),
                kind: r.get(5)?,
                sender: r.get(6)?,
                hold: r.get::<_, Option<i64>>(7)?.map(ms),
                notified: r.get(8)?,
            })
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    let mut targets = Vec::new();
    let mut asked: Vec<(u64, String, String, String)> = Vec::new();
    for row in rows {
        // No recorded hold means none was known at send: a human's copy
        // then waits, as `in_hold` has it.
        if is_human(&row.mailbox) && row.hold.is_none_or(|h| h > now_ms) {
            continue;
        }
        let after = Expiry::After(row.expires.saturating_sub(row.sent));
        if matches!(row.kind.as_str(), "question" | "review_request")
            && !row.notified
            && !asked.iter().any(|(s, ..)| *s == row.seq)
        {
            asked.push((
                row.seq,
                row.message_id.clone(),
                row.sender.clone(),
                row.kind.clone(),
            ));
        }
        targets.push(Target {
            seq: row.seq,
            message_id: row.message_id,
            mailbox: row.mailbox,
            outcome: format!("expired after {after}"),
        });
    }
    let mut acks = apply_deferred_in(tx, now_ms, ack_grace_ms)?;
    acks.extend(system_ack_in(tx, targets, "expiry", now_ms, ack_grace_ms)?);
    let mut notices = Vec::new();
    for (seq, message_id, sender, kind) in asked {
        // Only an expiry ack is an expiry: a deferred supersession that
        // falls due in the same pass never sends an "expired unanswered"
        // notice (Terra's review of e821ea6e..a1412008, finding 8).
        if !acks.iter().any(|a| a.seq == seq && a.via == "expiry") {
            continue;
        }
        // Noticed once, answered or not, so a later expiry of the same
        // message never sends a second note: the marker is taken only
        // while it is unset, and only its taker notifies.
        if !take_marker(tx, seq, now_ms)? || answered(tx, seq, &message_id)? {
            continue;
        }
        let notice = ExpiryNotice {
            sender,
            message_id,
            kind,
            nodes: nodes_in(tx, seq)?,
        };
        notify(tx, &notice)?;
        notices.push(notice);
    }
    // A round outlives its request's deliveries (A25): an open round whose
    // request's effective expiry has passed notifies its requester once,
    // under the request's `notified_ms`, the only notice marker, whatever
    // happened to the deliveries (finding 5).
    // One request opens one round (`open_review_from_mail_in`); the
    // query still aggregates per request, and the marker is taken
    // conditionally, so no request notifies twice whatever its rounds.
    // It reads only open rounds, through `entries_open_round`, so its cost
    // doesn't grow with closed history.
    let rounds: Vec<(i64, String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT m.seq, m.message_id, m.sender, m.kind FROM entries e
             JOIN msg_messages m ON m.message_id = json_extract(e.data, '$.request')
             JOIN msg_meta mm ON mm.seq = m.seq
             WHERE e.kind = 'review' AND e.state != 'closed'
               AND mm.expires_ms IS NOT NULL AND mm.expires_ms <= ?1
               AND mm.notified_ms IS NULL
             ORDER BY m.seq",
        )?;
        let mapped = stmt.query_map(params![sql_ms(now_ms)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    for (seq, message_id, sender, kind) in rounds {
        let seq = ms(seq);
        if !take_marker(tx, seq, now_ms)? {
            continue;
        }
        let notice = ExpiryNotice {
            sender,
            message_id,
            kind,
            nodes: nodes_in(tx, seq)?,
        };
        notify(tx, &notice)?;
        notices.push(notice);
    }
    Ok(Expired { acks, notices })
}

/// Sets message `seq`'s notice marker if it is unset; true only for the
/// call that set it, so each message is noticed once.
fn take_marker(tx: &Transaction<'_>, seq: u64, now_ms: u64) -> Result<bool, LedgerError> {
    Ok(tx.execute(
        "UPDATE msg_meta SET notified_ms = ?2 WHERE seq = ?1 AND notified_ms IS NULL",
        params![sql_ms(seq), sql_ms(now_ms)],
    )? == 1)
}

/// Whether one of message `seq`'s recipients answered it: a reply that
/// references it, or, for a question bound to a node, its entry no longer
/// open.
// Only tests call this yet. It lands in `purge_due` (mail.rs), which every
// mail transaction runs, when a-flow wires the rest of P2's call sites
// (still in 0.7.1's scope); until then kind expiry, deferred supersessions
// and A25's expiry notices don't run (Opus's final pass #11366, 1).
#[cfg_attr(not(test), allow(dead_code))]
fn answered(conn: &Connection, seq: u64, message_id: &str) -> Result<bool, LedgerError> {
    // A review request with a round is answered only by the round closing:
    // no reply, ack or reference completes a review (A25, A27 rule 8).
    // Every round the request opened is counted, so one open round keeps
    // it unanswered whatever the others are.
    let (rounds, open): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(state != 'closed'), 0) FROM entries
         WHERE kind = 'review' AND json_extract(data, '$.request') = ?1",
        params![message_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if rounds > 0 {
        return Ok(open == 0);
    }
    let replied: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM msg_messages r, json_each(r.reference_ids) j
             WHERE j.value = ?2 AND r.seq > ?1
               AND r.sender IN (SELECT mailbox FROM msg_deliveries WHERE seq = ?1))",
        params![sql_ms(seq), message_id],
        |r| r.get(0),
    )?;
    if replied {
        return Ok(true);
    }
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM entries
             WHERE kind = 'question' AND state != 'open'
               AND json_extract(data, '$.opened.message') = ?1)",
        params![message_id],
        |r| r.get(0),
    )?)
}

/// The nodes message `seq` names, primary first.
fn nodes_in(conn: &Connection, seq: u64) -> Result<Vec<NodeLink>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT node_uuid, node_id FROM msg_links WHERE seq = ?1 AND entry = '' ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![sql_ms(seq)], |r| {
        Ok(NodeLink {
            uuid: r.get(0)?,
            id: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A message's flow, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFlow {
    pub nodes: Vec<NodeLink>,
    pub entries: Vec<(NodeLink, String)>,
    pub supersedes: Vec<String>,
    pub slot: Option<String>,
    pub expires_ms: Option<u64>,
}

fn seq_of(conn: &Connection, message_id: &str) -> Result<u64, LedgerError> {
    conn.query_row(
        "SELECT seq FROM msg_messages WHERE message_id = ?1",
        params![message_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    .map(ms)
    .ok_or_else(|| LedgerError::UnknownMessage(message_id.into()))
}

fn flow_of(conn: &Connection, seq: u64) -> Result<MessageFlow, LedgerError> {
    let nodes = nodes_in(conn, seq)?;
    let entries = {
        let mut stmt = conn.prepare(
            "SELECT node_uuid, node_id, entry FROM msg_links WHERE seq = ?1 AND entry != ''
             ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![sql_ms(seq)], |r| {
            Ok((
                NodeLink {
                    uuid: r.get(0)?,
                    id: r.get(1)?,
                },
                r.get(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let meta: Option<(Option<String>, Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT supersedes, slot, expires_ms FROM msg_meta WHERE seq = ?1",
            params![sql_ms(seq)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (supersedes, slot, expires) = meta.unwrap_or_default();
    Ok(MessageFlow {
        nodes,
        entries,
        supersedes: supersedes
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default(),
        slot,
        expires_ms: expires.map(ms),
    })
}

impl Store {
    #[cfg(test)]
    fn flow_tx(&mut self) -> Result<Transaction<'_>, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::store::check_mail_schema(&tx)?;
        Ok(tx)
    }

    /// The nodes, entries, supersession, slot and expiry a message carries.
    pub fn message_flow(&self, message_id: &str) -> Result<MessageFlow, LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let seq = seq_of(&tx, message_id)?;
        flow_of(&tx, seq)
    }

    /// The `seq` of every message bound to the node with this UUID, after
    /// `after`, oldest first.
    pub fn bound_to(&self, node_uuid: &str, after: u64) -> Result<Vec<u64>, LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let mut stmt = tx.prepare(
            "SELECT DISTINCT seq FROM msg_links WHERE node_uuid = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![node_uuid, sql_ms(after)], |r| r.get::<_, i64>(0))?;
        Ok(rows.map(|r| r.map(ms)).collect::<Result<Vec<u64>, _>>()?)
    }

    /// Acks every delivery whose expiry has passed, in one transaction,
    /// sending no notices: `purge_due` will send them in its own
    /// transaction once P2's call sites land (A25), so only tests call
    /// this now.
    #[cfg(test)]
    pub(crate) fn expire_due(&mut self, now_ms: u64) -> Result<Expired, LedgerError> {
        let grace = self.ack_grace_ms;
        let tx = self.flow_tx()?;
        let expired = expire_due_in(&tx, now_ms, grace, &mut |_, _| Ok(()))?;
        tx.commit()?;
        Ok(expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{InboxQuery, Outgoing, Retention, Shown, To};
    use crate::store::Upgrade;

    const NODE_A: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e3";
    const NODE_B: &str = "bug-01a1878e-3e6f-710b-9d01-68d002aed3e4";

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("mdkg-ledger-{name}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn store(&self) -> Store {
            Store::open_with(
                &self.0.join("mdkg").join("coord.sqlite"),
                1_000,
                Upgrade::Allow,
            )
            .expect("store")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn out(id: &str, kind: &str, sender: &str, to: &[&str], now_ms: u64) -> Outgoing {
        Outgoing {
            message_id: id.into(),
            context_id: None,
            kind: kind.into(),
            sender: sender.into(),
            via: None,
            sender_mailboxes: vec![sender.into()],
            to: To::Mailboxes(to.iter().map(|m| m.to_string()).collect()),
            reference_ids: vec![],
            parts: json!([{"kind": "text", "text": format!("body of {id}")}]),
            retention: Retention::PurgeOnAck,
            human_hold_days: 2,
            now_ms,
        }
    }

    /// Sends `o` with `flow` through the send path, which records the flow
    /// in the send's own transaction.
    fn send(s: &mut Store, o: &Outgoing, flow: &SendFlow) -> Result<SendEffects, LedgerError> {
        match s.send_flow(o, flow, &ExpiryPolicy::default(), &mut |_, _| Ok(())) {
            Ok((_, _, effects)) => Ok(effects),
            Err(crate::mail::MailError::Ledger(e)) => Err(e),
            Err(e) => panic!("send {}: {e}", o.message_id),
        }
    }

    fn deliveries(
        s: &mut Store,
        id: &str,
    ) -> Vec<(String, String, Option<String>, Option<String>)> {
        let m = s.message(id, 0).expect("message");
        m.deliveries
            .into_iter()
            .map(|d| (d.mailbox, d.state, d.acked_by, d.acked_via))
            .collect()
    }

    fn has_body(s: &mut Store, id: &str) -> bool {
        s.message(id, 0).expect("message").parts.is_some()
    }

    fn node(id: &str) -> NodeLink {
        NodeLink::of(id).expect("node id")
    }

    #[test]
    fn expiries_parse_and_print_in_the_largest_unit() {
        assert_eq!(Expiry::parse("30m"), Some(Expiry::After(30 * MINUTE)));
        assert_eq!(Expiry::parse("12h"), Some(Expiry::After(12 * HOUR)));
        assert_eq!(Expiry::parse("3d"), Some(Expiry::After(3 * DAY)));
        assert_eq!(Expiry::parse("never"), Some(Expiry::Never));
        assert_eq!(Expiry::parse("3650d"), Some(Expiry::After(3650 * DAY)));
        for bad in [
            "",
            "d",
            "0d",
            "03d",
            "3",
            "3w",
            "-1d",
            "3.5d",
            "3651d",
            "99999999m",
            " 3d",
        ] {
            assert_eq!(Expiry::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Expiry::After(3 * DAY).to_string(), "3d");
        assert_eq!(Expiry::After(36 * HOUR).to_string(), "36h");
        assert_eq!(Expiry::After(90 * MINUTE).to_string(), "90m");
        assert_eq!(Expiry::Never.to_string(), "never");
    }

    #[test]
    fn kinds_default_as_the_table_says_and_a_fault_never_expires() {
        let p = ExpiryPolicy::default();
        assert_eq!(p.expires_ms("receipt", 10, None), Some(10 + DAY));
        assert_eq!(p.expires_ms("note", 10, None), Some(10 + 3 * DAY));
        assert_eq!(p.expires_ms("handoff", 10, None), Some(10 + 7 * DAY));
        assert_eq!(p.expires_ms("review_request", 10, None), Some(10 + 7 * DAY));
        for kind in [
            "question",
            "findings",
            "decision",
            "resolution",
            "fault",
            "x-new",
        ] {
            assert_eq!(p.expires_ms(kind, 10, None), None, "{kind}");
        }
        assert_eq!(p.expires_ms("note", 10, Some(Expiry::Never)), None);
        assert_eq!(
            p.expires_ms("question", 10, Some(Expiry::After(HOUR))),
            Some(10 + HOUR)
        );
        let mut p = ExpiryPolicy::default();
        p.overrides.insert("note".into(), Expiry::After(HOUR));
        p.overrides.insert("fault".into(), Expiry::After(HOUR));
        assert_eq!(p.expires_ms("note", 10, None), Some(10 + HOUR));
        assert_eq!(p.expires_ms("fault", 10, None), None);
        assert_eq!(p.expires_ms("fault", 10, Some(Expiry::After(HOUR))), None);
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(node(NODE_A).uuid, "01a1878e-3e6f-710b-9d01-68d002aed3e3");
        assert!(matches!(
            NodeLink::of("task-12"),
            Err(LedgerError::BadNode(_))
        ));
        for ok in ["F3", "Q1", "D10", "I2", "V1", "R7"] {
            check_entry_id(ok).expect(ok);
        }
        for bad in ["", "F", "F0", "F03", "f3", "X3", "F3a", "F1234567890"] {
            assert!(check_entry_id(bad).is_err(), "{bad:?}");
        }
        check_slot("brief", "orchestrator").expect("slot");
        check_slot("fault:run.agent_failed", "mdkg:supervisor").expect("mdkg's slot");
        let long = format!("fault:{}:msg.secret_refused", "a".repeat(120));
        check_slot(&long, "mdkg:supervisor").expect("any agent name fits");
        for bad in ["", "Brief", "a b", &"x".repeat(201)] {
            assert!(check_slot(bad, "orchestrator").is_err(), "{bad:?}");
        }
        assert!(check_slot("fault:x", "orchestrator").is_err());
    }

    #[test]
    fn a_send_binds_its_nodes_and_entries_once_each_primary_first() {
        let dir = Scratch::new("bind");
        let mut s = dir.store();
        let flow = SendFlow {
            nodes: vec![node(NODE_B), node(NODE_A), node(NODE_B)],
            entries: vec![(node(NODE_A), "F3".into()), (node(NODE_A), "F3".into())],
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("m1", "note", "orchestrator", &["a-flow"], 1_000),
            &flow,
        )
        .expect("flow");
        let got = s.message_flow("m1").expect("flow");
        assert_eq!(got.nodes, vec![node(NODE_B), node(NODE_A)]);
        assert_eq!(got.entries, vec![(node(NODE_A), "F3".to_string())]);
        assert_eq!(got.expires_ms, Some(1_000 + 3 * DAY));
        let uuid = node(NODE_A).uuid;
        let seq = s.message("m1", 0).expect("m1").seq;
        assert_eq!(s.bound_to(&uuid, 0).expect("bound"), vec![seq]);
        assert!(s.bound_to(&uuid, seq).expect("after").is_empty());
        // A retyped node is the same UUID.
        let retyped = node("feat-01a1878e-3e6f-710b-9d01-68d002aed3e3");
        assert_eq!(s.bound_to(&retyped.uuid, 0).expect("bound"), vec![seq]);
    }

    #[test]
    fn supersedes_acks_the_senders_own_unacked_mail_and_refuses_anothers() {
        let dir = Scratch::new("supersede");
        let mut s = dir.store();
        let none = SendFlow::default();
        send(
            &mut s,
            &out("old", "note", "orchestrator", &["a-flow", "a-mail"], 1_000),
            &none,
        )
        .expect("old");
        send(
            &mut s,
            &out("theirs", "note", "a-mail", &["a-flow"], 1_100),
            &none,
        )
        .expect("theirs");
        // a-mail already acked its copy: only a-flow's is superseded.
        let inbox = s
            .inbox(&InboxQuery {
                mailboxes: vec!["a-mail".into()],
                now_ms: 1_200,
                ..InboxQuery::default()
            })
            .expect("inbox");
        let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
        s.mark_shown(&listed, &["a-mail".into()], 1_200)
            .expect("shown");
        s.ack(
            &["old".into()],
            &["a-mail".into()],
            "a-mail",
            None,
            None,
            1_200,
        )
        .expect("ack");
        let refused = send(
            &mut s,
            &out("new-bad", "note", "orchestrator", &["a-flow"], 2_000),
            &SendFlow {
                supersedes: vec!["theirs".into()],
                ..SendFlow::default()
            },
        );
        assert!(matches!(refused, Err(LedgerError::NotSender(ref id)) if id == "theirs"));
        let unknown = send(
            &mut s,
            &out("new-unknown", "note", "orchestrator", &["a-flow"], 2_000),
            &SendFlow {
                supersedes: vec!["nope".into()],
                ..SendFlow::default()
            },
        );
        assert!(matches!(unknown, Err(LedgerError::UnknownMessage(_))));
        let effects = send(
            &mut s,
            &out("new", "note", "orchestrator", &["a-flow"], 3_000),
            &SendFlow {
                supersedes: vec!["old".into()],
                ..SendFlow::default()
            },
        )
        .expect("supersede");
        assert_eq!(effects.superseded, vec!["old".to_string()]);
        assert_eq!(
            deliveries(&mut s, "old"),
            vec![
                (
                    "a-flow".into(),
                    "acked".into(),
                    Some("mdkg".into()),
                    Some("supersede".into())
                ),
                ("a-mail".into(), "acked".into(), Some("a-mail".into()), None),
            ]
        );
        assert_eq!(
            s.message("old", 0).expect("old").deliveries[0]
                .outcome
                .as_deref(),
            Some("superseded by new")
        );
        // Every delivery acked: its purge_on_ack body goes, as an ack's would.
        assert!(!has_body(&mut s, "old"));
        assert_eq!(
            s.message_flow("new").expect("flow").supersedes,
            vec!["old".to_string()]
        );
        // Superseding again changes nothing and says so.
        let again = send(
            &mut s,
            &out("newer", "note", "orchestrator", &["a-flow"], 4_000),
            &SendFlow {
                supersedes: vec!["old".into()],
                ..SendFlow::default()
            },
        )
        .expect("again");
        assert!(again.superseded.is_empty());
        s.verify_log().expect("the chain holds");
    }

    #[test]
    fn supersession_never_takes_mail_from_a_recipient_the_new_one_misses() {
        let dir = Scratch::new("subset");
        let mut s = dir.store();
        let none = SendFlow::default();
        send(
            &mut s,
            &out(
                "brief",
                "note",
                "orchestrator",
                &["a-flow", "a-mail", "human:nick"],
                1,
            ),
            &none,
        )
        .expect("brief");
        let effects = send(
            &mut s,
            &out(
                "brief2",
                "note",
                "orchestrator",
                &["a-flow", "human:nick"],
                2,
            ),
            &SendFlow {
                supersedes: vec!["brief".into()],
                ..SendFlow::default()
            },
        )
        .expect("brief2");
        assert_eq!(effects.superseded, vec!["brief".to_string()]);
        let states: Vec<(String, String)> = deliveries(&mut s, "brief")
            .into_iter()
            .map(|(m, state, ..)| (m, state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("a-flow".into(), "acked".into()),
                ("a-mail".into(), "delivered".into()),
                // Inside its two-day hold: a system ack leaves it (R41).
                ("human:nick".into(), "delivered".into()),
            ]
        );
        // a-mail still holds it, so its body stays.
        assert!(has_body(&mut s, "brief"));
        // A replacement reaching nobody who has it supersedes nothing.
        let effects = send(
            &mut s,
            &out("brief3", "note", "orchestrator", &["a-ux"], 3),
            &SendFlow {
                supersedes: vec!["brief".into()],
                ..SendFlow::default()
            },
        )
        .expect("brief3");
        assert!(effects.superseded.is_empty());
    }

    #[test]
    fn slot_supersession_reads_the_slot_through_its_index_in_one_query() {
        let dir = Scratch::new("slot-plan");
        let s = dir.store();
        let mut stmt = s
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SUPERSEDED_DELIVERIES}"))
            .expect("plan");
        let plan: Vec<String> = stmt
            .query_map(
                params!["brief", NODE_A, "orchestrator", 9, "[]", false],
                |r| r.get::<_, String>(3),
            )
            .expect("plan")
            .collect::<Result<_, _>>()
            .expect("plan");
        let plan = plan.join("\n");
        assert!(plan.contains("msg_meta_slot"), "{plan}");
        assert!(
            !plan
                .lines()
                .any(|l| l.starts_with("SCAN d") || l.starts_with("SCAN msg_deliveries")),
            "{plan}"
        );
    }

    /// With an ack grace (friction item 72), a supersession that acks
    /// every delivery keeps the body as an actor's ack does: readable
    /// until the grace from the supersession is over, then purged.
    #[test]
    fn a_superseded_body_is_kept_for_the_ack_grace() {
        let dir = Scratch::new("supersede-grace");
        let mut s = dir.store();
        s.set_ack_grace_days(1);
        send(
            &mut s,
            &out("old", "note", "orchestrator", &["a-flow"], 1_000),
            &SendFlow::default(),
        )
        .expect("old");
        send(
            &mut s,
            &out("new", "note", "orchestrator", &["a-flow"], 2_000),
            &SendFlow {
                supersedes: vec!["old".into()],
                ..SendFlow::default()
            },
        )
        .expect("new");
        assert_eq!(deliveries(&mut s, "old")[0].3.as_deref(), Some("supersede"));
        assert!(has_body(&mut s, "old"), "kept at the supersession");
        assert!(
            s.message("old", 2_000 + DAY - 1)
                .expect("message")
                .parts
                .is_some()
        );
        assert!(
            s.message("old", 2_000 + DAY)
                .expect("message")
                .parts
                .is_none(),
            "purged once the grace is over"
        );
        // Without a grace, the supersession purges it at once.
        s.set_ack_grace_days(0);
        send(
            &mut s,
            &out("newer", "note", "orchestrator", &["a-flow"], 3_000),
            &SendFlow {
                supersedes: vec!["new".into()],
                ..SendFlow::default()
            },
        )
        .expect("newer");
        assert!(!has_body(&mut s, "new"));
    }

    #[test]
    fn a_slot_supersedes_per_sender_and_primary_node() {
        let dir = Scratch::new("slot");
        let mut s = dir.store();
        let slot = |nodes: Vec<NodeLink>| SendFlow {
            nodes,
            slot: Some("brief".into()),
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("a1", "note", "orchestrator", &["a-flow"], 1),
            &slot(vec![node(NODE_A)]),
        )
        .expect("a1");
        send(
            &mut s,
            &out("b1", "note", "orchestrator", &["a-flow"], 2),
            &slot(vec![node(NODE_B)]),
        )
        .expect("b1");
        send(
            &mut s,
            &out("x1", "note", "orchestrator", &["a-flow"], 3),
            &slot(vec![]),
        )
        .expect("x1");
        send(
            &mut s,
            &out("o1", "note", "a-mail", &["a-flow"], 4),
            &slot(vec![node(NODE_A)]),
        )
        .expect("o1");
        let effects = send(
            &mut s,
            &out("a2", "note", "orchestrator", &["a-flow"], 5),
            &slot(vec![node(NODE_A), node(NODE_B)]),
        )
        .expect("a2");
        assert_eq!(effects.superseded, vec!["a1".to_string()]);
        let effects = send(
            &mut s,
            &out("x2", "note", "orchestrator", &["a-flow"], 6),
            &slot(vec![]),
        )
        .expect("x2");
        assert_eq!(effects.superseded, vec!["x1".to_string()]);
        for kept in ["b1", "o1", "a2", "x2"] {
            assert_eq!(deliveries(&mut s, kept)[0].1, "delivered", "{kept}");
        }
        let refused = send(
            &mut s,
            &out("f", "note", "orchestrator", &["a-flow"], 7),
            &SendFlow {
                slot: Some("fault:run.agent_failed".into()),
                ..SendFlow::default()
            },
        );
        assert!(matches!(refused, Err(LedgerError::BadSlot(_))));
    }

    #[test]
    fn due_deliveries_expire_and_unanswered_asks_name_their_sender() {
        let dir = Scratch::new("expire");
        let mut s = dir.store();
        let none = SendFlow::default();
        let bound = SendFlow {
            nodes: vec![node(NODE_A)],
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("note", "note", "orchestrator", &["a-flow"], 0),
            &none,
        )
        .expect("note");
        send(
            &mut s,
            &out("ask", "review_request", "orchestrator", &["rev"], 0),
            &bound,
        )
        .expect("ask");
        send(
            &mut s,
            &out("q", "question", "a-flow", &["orchestrator"], 0),
            &none,
        )
        .expect("q");
        send(
            &mut s,
            &out("held", "note", "orchestrator", &["human:nick", "a-mail"], 0),
            &none,
        )
        .expect("held");
        // Before any expiry: nothing.
        assert_eq!(
            s.expire_due(3 * DAY - 1).expect("early"),
            Expired::default()
        );
        let expired = s.expire_due(3 * DAY).expect("notes");
        let acked: Vec<(&str, &str)> = expired
            .acks
            .iter()
            .map(|a| (a.message_id.as_str(), a.mailbox.as_str()))
            .collect();
        // The human's copy waits for its two-day hold, already over at
        // three days, so both copies of `held` go.
        assert_eq!(
            acked,
            vec![
                ("note", "a-flow"),
                ("held", "a-mail"),
                ("held", "human:nick")
            ]
        );
        assert!(
            expired
                .acks
                .iter()
                .all(|a| a.via == "expiry" && a.outcome == "expired after 3d")
        );
        assert!(expired.notices.is_empty());
        let expired = s.expire_due(7 * DAY).expect("review request");
        assert_eq!(expired.acks.len(), 1);
        assert_eq!(
            expired.notices,
            vec![ExpiryNotice {
                sender: "orchestrator".into(),
                message_id: "ask".into(),
                kind: "review_request".into(),
                nodes: vec![node(NODE_A)],
            }]
        );
        // A question never expires by default; it is still delivered.
        assert_eq!(deliveries(&mut s, "q")[0].1, "delivered");
        // Expired again: nothing new, no second record.
        assert_eq!(s.expire_due(30 * DAY).expect("again"), Expired::default());
        s.verify_log().expect("the chain holds");
    }

    #[test]
    fn a_human_delivery_waits_for_its_hold() {
        let dir = Scratch::new("hold");
        let mut s = dir.store();
        let flow = SendFlow {
            expires: Some(Expiry::After(HOUR)),
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("m", "note", "orchestrator", &["human:nick", "a-flow"], 0),
            &flow,
        )
        .expect("m");
        let first = s.expire_due(HOUR).expect("agent copy");
        assert_eq!(first.acks.len(), 1);
        assert_eq!(first.acks[0].mailbox, "a-flow");
        assert!(has_body(&mut s, "m"), "the human's hold keeps the body");
        assert!(s.expire_due(2 * DAY - 1).expect("held").acks.is_empty());
        let second = s.expire_due(2 * DAY).expect("hold over");
        assert_eq!(second.acks[0].mailbox, "human:nick");
        assert!(!has_body(&mut s, "m"));
    }

    #[test]
    fn a_system_ack_leaves_a_human_in_hold_and_purges_only_when_all_are_acked() {
        let dir = Scratch::new("hold-supersede");
        let mut s = dir.store();
        send(
            &mut s,
            &out("old", "note", "orchestrator", &["a-flow", "human:nick"], 0),
            &SendFlow::default(),
        )
        .expect("old");
        let replace = |id: &str, at: u64| {
            (
                out(id, "note", "orchestrator", &["a-flow", "human:nick"], at),
                SendFlow {
                    supersedes: vec!["old".into()],
                    ..SendFlow::default()
                },
            )
        };
        let (o, f) = replace("new", HOUR);
        let effects = send(&mut s, &o, &f).expect("new");
        assert_eq!(effects.acks.len(), 1, "only the agent's copy");
        assert_eq!(deliveries(&mut s, "old")[1].1, "delivered");
        assert!(has_body(&mut s, "old"), "a human in hold keeps the body");
        // The supersession waits as a deferred ack, applied by the first
        // expiry pass after the hold, with its own via; the body goes once
        // every delivery is acked.
        assert!(s.expire_due(2 * DAY - 1).expect("held").acks.is_empty());
        let late = s.expire_due(2 * DAY).expect("hold over");
        assert_eq!(late.acks.len(), 1);
        assert_eq!(late.acks[0].via, "supersede");
        assert_eq!(late.acks[0].outcome, "superseded by new");
        assert_eq!(
            deliveries(&mut s, "old")[1],
            (
                "human:nick".into(),
                "acked".into(),
                Some("mdkg".into()),
                Some("supersede".into())
            )
        );
        assert!(!has_body(&mut s, "old"));
        // A human who acks first makes the deferred ack a no-op.
        send(
            &mut s,
            &out("old2", "note", "orchestrator", &["human:nick"], 0),
            &SendFlow::default(),
        )
        .expect("old2");
        send(
            &mut s,
            &out("new2", "note", "orchestrator", &["human:nick"], 1),
            &SendFlow {
                supersedes: vec!["old2".into()],
                ..SendFlow::default()
            },
        )
        .expect("new2");
        let inbox = s
            .inbox(&InboxQuery {
                mailboxes: vec!["human:nick".into()],
                now_ms: 2,
                ..InboxQuery::default()
            })
            .expect("inbox");
        let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
        s.mark_shown(&listed, &["human:nick".into()], 2)
            .expect("shown");
        s.ack(
            &["old2".into()],
            &["human:nick".into()],
            "human:nick",
            None,
            None,
            2,
        )
        .expect("ack");
        let late = s.expire_due(3 * DAY).expect("after");
        assert!(late.acks.iter().all(|a| a.message_id != "old2"));
        s.verify_log().expect("the chain holds");
    }

    #[test]
    fn a_durable_human_delivery_waits_for_its_hold_whatever_its_expiry() {
        let dir = Scratch::new("hold-durable");
        let mut s = dir.store();
        let mut o = out("d", "note", "orchestrator", &["human:nick"], 0);
        o.retention = Retention::Durable;
        o.human_hold_days = 7;
        send(
            &mut s,
            &o,
            &SendFlow {
                expires: Some(Expiry::After(HOUR)),
                ..SendFlow::default()
            },
        )
        .expect("d");
        assert!(s.expire_due(HOUR).expect("early").acks.is_empty());
        assert!(s.expire_due(7 * DAY - 1).expect("held").acks.is_empty());
        assert_eq!(s.expire_due(7 * DAY).expect("over").acks.len(), 1);
    }

    #[test]
    fn an_open_round_notifies_once_from_the_round_and_a_reply_never_answers_it() {
        let dir = Scratch::new("round-notice");
        let mut s = dir.store();
        let bound = |expires| SendFlow {
            nodes: vec![node(NODE_A)],
            expires,
            ..SendFlow::default()
        };
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        // Two review requests with rounds; the reviewer replies to the first
        // ("couldn't read the checkout") and acks the second unreviewed.
        for id in ["rr1", "rr2"] {
            send(
                &mut s,
                &out(id, "review_request", "a-flow", &["rev"], 0),
                &bound(Some(Expiry::After(HOUR))),
            )
            .expect("request");
            let tx = s.conn.transaction().expect("tx");
            crate::entries::open_review_from_mail_in(
                &tx,
                &node(NODE_A),
                "review",
                (&base, &head),
                &["rev".to_string()],
                "a-flow",
                id,
                0,
                None,
            )
            .expect("round");
            tx.commit().expect("commit");
        }
        // A request sent `--expires never` has no round timer.
        send(
            &mut s,
            &out("rr3", "review_request", "a-flow", &["rev"], 0),
            &bound(Some(Expiry::Never)),
        )
        .expect("never");
        let mut reply = out("couldnt", "note", "rev", &["a-flow"], 10);
        reply.reference_ids = vec!["rr1".into()];
        send(&mut s, &reply, &SendFlow::default()).expect("reply");
        let inbox = s
            .inbox(&InboxQuery {
                mailboxes: vec!["rev".into()],
                now_ms: 20,
                ..InboxQuery::default()
            })
            .expect("inbox");
        let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
        s.mark_shown(&listed, &["rev".into()], 20).expect("shown");
        s.ack(&["rr2".into()], &["rev".into()], "rev", None, None, 20)
            .expect("ack");
        let mut sent = Vec::new();
        let tx = s.conn.transaction().expect("tx");
        expire_due_in(&tx, HOUR, 0, &mut |_, n| {
            sent.push(n.message_id.clone());
            Ok(())
        })
        .expect("expire");
        tx.commit().expect("commit");
        sent.sort();
        // rr1's reply doesn't answer it; rr2 notifies from its round though
        // its delivery was acked; rr3 never expires.
        assert_eq!(sent, vec!["rr1".to_string(), "rr2".to_string()]);
        // One marker: a later pass sends nothing more.
        let tx = s.conn.transaction().expect("tx");
        expire_due_in(&tx, 30 * DAY, 0, &mut |_, n| {
            panic!("a second notice for {}", n.message_id)
        })
        .expect("later");
        tx.commit().expect("commit");
    }

    /// Sends review request `id` bound to NODE_A and opens its round;
    /// returns whether the round opened.
    fn request_with_round(s: &mut Store, id: &str, expires: Option<Expiry>) -> bool {
        send(
            s,
            &out(id, "review_request", "a-flow", &["rev"], 0),
            &SendFlow {
                nodes: vec![node(NODE_A)],
                expires,
                // A slot gives even a never-expiring request its msg_meta row.
                slot: Some(format!("brief.{id}")),
                ..SendFlow::default()
            },
        )
        .expect("request");
        let tx = s.conn.transaction().expect("tx");
        let opened = crate::entries::open_review_from_mail_in(
            &tx,
            &node(NODE_A),
            "review",
            (&"a".repeat(40), &"b".repeat(40)),
            &["rev".to_string()],
            "a-flow",
            id,
            0,
            None,
        )
        .expect("round")
        .is_some();
        tx.commit().expect("commit");
        opened
    }

    #[test]
    fn one_request_opens_one_round_and_notifies_once() {
        let dir = Scratch::new("round-once");
        let mut s = dir.store();
        assert!(request_with_round(&mut s, "rr", Some(Expiry::After(HOUR))));
        // A second node of the same request, or a resend, opens nothing.
        let tx = s.conn.transaction().expect("tx");
        let again = crate::entries::open_review_from_mail_in(
            &tx,
            &node(NODE_B),
            "review",
            (&"a".repeat(40), &"b".repeat(40)),
            &["rev".to_string()],
            "a-flow",
            "rr",
            0,
            None,
        )
        .expect("again");
        tx.commit().expect("commit");
        assert!(again.is_none());
        let rounds: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = 'review'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(rounds, 1);
        let mut sent = Vec::new();
        let tx = s.conn.transaction().expect("tx");
        expire_due_in(&tx, HOUR, 0, &mut |_, n| {
            sent.push(n.message_id.clone());
            Ok(())
        })
        .expect("expire");
        tx.commit().expect("commit");
        // The delivery's expiry and the round's timer share one marker.
        assert_eq!(sent, vec!["rr".to_string()]);
    }

    #[test]
    fn a_never_expiring_request_with_an_open_round_never_notifies() {
        let dir = Scratch::new("round-never");
        let mut s = dir.store();
        assert!(request_with_round(&mut s, "rn", Some(Expiry::Never)));
        let meta: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM msg_meta mm JOIN msg_messages m ON m.seq = mm.seq
                 WHERE m.message_id = 'rn' AND mm.expires_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("meta");
        assert_eq!(meta, 1, "a msg_meta row with no expiry");
        let tx = s.conn.transaction().expect("tx");
        expire_due_in(&tx, 365 * DAY, 0, &mut |_, n| {
            panic!("a notice for {}", n.message_id)
        })
        .expect("expire");
        tx.commit().expect("commit");
    }

    #[test]
    fn a_failed_round_notice_rolls_back_and_a_retry_notifies_once() {
        let dir = Scratch::new("round-fail");
        let mut s = dir.store();
        assert!(request_with_round(&mut s, "rf", Some(Expiry::After(HOUR))));
        // Acked by the reviewer unreviewed: only the round can notify.
        let inbox = s
            .inbox(&InboxQuery {
                mailboxes: vec!["rev".into()],
                now_ms: 5,
                ..InboxQuery::default()
            })
            .expect("inbox");
        let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
        s.mark_shown(&listed, &["rev".into()], 5).expect("shown");
        s.ack(&["rf".into()], &["rev".into()], "rev", None, None, 10)
            .expect("ack");
        {
            let tx = s.conn.transaction().expect("tx");
            let failed = expire_due_in(&tx, HOUR, 0, &mut |_, _| {
                Err(LedgerError::Store(StoreError::Log("injected".into())))
            });
            assert!(failed.is_err());
            // Dropped uncommitted: the marker rolls back with the notice.
        }
        let marker: Option<i64> = s
            .conn
            .query_row(
                "SELECT mm.notified_ms FROM msg_meta mm JOIN msg_messages m ON m.seq = mm.seq
                 WHERE m.message_id = 'rf'",
                [],
                |r| r.get(0),
            )
            .expect("marker");
        assert_eq!(marker, None);
        let mut sent = Vec::new();
        for _ in 0..2 {
            let tx = s.conn.transaction().expect("tx");
            expire_due_in(&tx, HOUR, 0, &mut |_, n| {
                sent.push(n.message_id.clone());
                Ok(())
            })
            .expect("retry");
            tx.commit().expect("commit");
        }
        assert_eq!(sent, vec!["rf".to_string()]);
    }

    /// Terra's code review #6120: the duplicate-round states a store made
    /// before `entries_round_request` was unique (the index is dropped to
    /// make them) notify exactly once: one closed and one open round, in
    /// both orders, and two open ones.
    #[test]
    fn duplicate_rounds_of_one_request_notify_once() {
        for (n, (first, second)) in [("closed", "open"), ("open", "closed"), ("open", "open")]
            .into_iter()
            .enumerate()
        {
            let dir = Scratch::new(&format!("round-dup-{n}"));
            let mut s = dir.store();
            assert!(request_with_round(&mut s, "rd", Some(Expiry::After(HOUR))));
            // Acked by the reviewer unreviewed: only a round can notify.
            let inbox = s
                .inbox(&InboxQuery {
                    mailboxes: vec!["rev".into()],
                    now_ms: 5,
                    ..InboxQuery::default()
                })
                .expect("inbox");
            let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
            s.mark_shown(&listed, &["rev".into()], 5).expect("shown");
            s.ack(&["rd".into()], &["rev".into()], "rev", None, None, 10)
                .expect("ack");
            s.conn
                .execute_batch(&format!(
                    "DROP INDEX entries_round_request;
                     INSERT INTO entries (node_uuid, entry_id, kind, state, severity, slot,
                                          blocking, title, text, data, opened_ms, updated_ms)
                     SELECT node_uuid, 'V2', kind, '{second}', severity, slot, blocking, title,
                            text, data, opened_ms, updated_ms
                     FROM entries WHERE kind = 'review';
                     UPDATE entries SET state = '{first}' WHERE kind = 'review' AND entry_id = 'V1';"
                ))
                .expect("duplicate round");
            let mut sent = Vec::new();
            for _ in 0..2 {
                let tx = s.conn.transaction().expect("tx");
                expire_due_in(&tx, HOUR, 0, &mut |_, n| {
                    sent.push(n.message_id.clone());
                    Ok(())
                })
                .expect("expire");
                tx.commit().expect("commit");
            }
            assert_eq!(sent, vec!["rd".to_string()], "{first} then {second}");
        }
    }

    #[test]
    fn the_round_timer_reads_open_rounds_through_an_index() {
        let dir = Scratch::new("round-plan");
        let s = dir.store();
        let plan: Vec<String> = {
            let mut stmt = s
                .conn
                .prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT DISTINCT m.seq FROM entries e
                     JOIN msg_messages m ON m.message_id = json_extract(e.data, '$.request')
                     JOIN msg_meta mm ON mm.seq = m.seq
                     WHERE e.kind = 'review' AND e.state != 'closed'
                       AND mm.expires_ms IS NOT NULL AND mm.expires_ms <= 1
                       AND mm.notified_ms IS NULL",
                )
                .expect("plan");
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).expect("rows");
            rows.collect::<Result<_, _>>().expect("collect")
        };
        assert!(
            plan.iter().any(|l| l.contains("entries_open_round")),
            "{plan:?}"
        );
        let answered: Vec<String> = {
            let mut stmt = s
                .conn
                .prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT COUNT(*) FROM entries
                     WHERE kind = 'review' AND json_extract(data, '$.request') = 'x'",
                )
                .expect("plan");
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).expect("rows");
            rows.collect::<Result<_, _>>().expect("collect")
        };
        assert!(
            answered.iter().any(|l| l.contains("entries_round_request")),
            "{answered:?}"
        );
    }

    #[test]
    fn a_deferred_supersession_never_sends_an_expiry_notice() {
        let dir = Scratch::new("deferred-notice");
        let mut s = dir.store();
        let mut ask = out("q1", "question", "a-flow", &["human:nick"], 0);
        ask.human_hold_days = 1;
        send(
            &mut s,
            &ask,
            &SendFlow {
                slot: Some("ask".into()),
                expires: Some(Expiry::After(HOUR)),
                ..SendFlow::default()
            },
        )
        .expect("q1");
        // Superseded inside the hold: deferred until the hold ends.
        let mut again = out("q2", "question", "a-flow", &["human:nick"], 10);
        again.human_hold_days = 1;
        send(
            &mut s,
            &again,
            &SendFlow {
                slot: Some("ask".into()),
                ..SendFlow::default()
            },
        )
        .expect("q2");
        // After the hold, q1 is both expired and due its deferred
        // supersession; the deferred ack lands first, and no expiry notice
        // goes for a message that was superseded.
        let tx = s.conn.transaction().expect("tx");
        let expired = expire_due_in(&tx, 2 * DAY, 0, &mut |_, n| {
            panic!("an expiry notice for superseded {}", n.message_id)
        })
        .expect("expire");
        tx.commit().expect("commit");
        assert!(
            expired
                .acks
                .iter()
                .any(|a| a.message_id == "q1" && a.via == "supersede")
        );
    }

    #[test]
    fn an_expiry_notice_is_sent_in_the_acks_transaction_once_and_only_unanswered() {
        let dir = Scratch::new("notice");
        let mut s = dir.store();
        let mut ask = out(
            "ask",
            "question",
            "a-flow",
            &["orchestrator", "human:nick"],
            0,
        );
        ask.human_hold_days = 10;
        let expiring = SendFlow {
            expires: Some(Expiry::After(HOUR)),
            ..SendFlow::default()
        };
        send(&mut s, &ask, &expiring).expect("ask");
        send(
            &mut s,
            &out("asked", "question", "a-flow", &["orchestrator"], 0),
            &expiring,
        )
        .expect("asked");
        let mut reply = out("reply", "note", "orchestrator", &["a-flow"], 10);
        reply.reference_ids = vec!["asked".into()];
        send(&mut s, &reply, &SendFlow::default()).expect("reply");
        // A notice that fails to send takes its ack with it.
        {
            let tx = s.conn.transaction().expect("tx");
            let failed = expire_due_in(&tx, HOUR, 0, &mut |_, _| {
                Err(LedgerError::UnknownMessage("injected".into()))
            });
            assert!(failed.is_err());
        }
        assert_eq!(deliveries(&mut s, "ask")[1].1, "delivered");
        let mut sent = Vec::new();
        let tx = s.conn.transaction().expect("tx");
        let expired = expire_due_in(&tx, HOUR, 0, &mut |_, n| {
            sent.push(n.message_id.clone());
            Ok(())
        })
        .expect("expire");
        tx.commit().expect("commit");
        // `asked` was answered by a reply from its recipient: no notice.
        assert_eq!(sent, vec!["ask".to_string()]);
        assert_eq!(expired.acks.len(), 2);
        // The human's copy expires after its hold: no second notice.
        let tx = s.conn.transaction().expect("tx");
        let later = expire_due_in(&tx, 10 * DAY, 0, &mut |_, _| {
            panic!("a second notice");
        })
        .expect("later");
        tx.commit().expect("commit");
        let human: Vec<(&str, &str)> = later
            .acks
            .iter()
            .map(|a| (a.message_id.as_str(), a.mailbox.as_str()))
            .filter(|(id, _)| *id == "ask")
            .collect();
        assert_eq!(human, vec![("ask", "human:nick")]);
        s.verify_log().expect("the chain holds");
    }

    #[test]
    fn an_mdkg_fault_slot_supersedes_at_every_mailbox_it_reached() {
        let dir = Scratch::new("fault-slot");
        let mut s = dir.store();
        let fault = SendFlow {
            slot: Some("fault:a-flow:run.agent_failed".into()),
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("f1", "fault", "mdkg:supervisor", &["orchestrator"], 0),
            &fault,
        )
        .expect("f1");
        // reports_to changed: the old recipient's copy is superseded too.
        let effects = send(
            &mut s,
            &out("f2", "fault", "mdkg:supervisor", &["lead"], 1),
            &fault,
        )
        .expect("f2");
        assert_eq!(effects.superseded, vec!["f1".to_string()]);
        // Anyone else's slot stays recipient-scoped.
        let brief = SendFlow {
            slot: Some("brief".into()),
            ..SendFlow::default()
        };
        send(
            &mut s,
            &out("b1", "note", "orchestrator", &["a"], 2),
            &brief,
        )
        .expect("b1");
        let effects = send(
            &mut s,
            &out("b2", "note", "orchestrator", &["b"], 3),
            &brief,
        )
        .expect("b2");
        assert!(effects.superseded.is_empty());
    }
}
