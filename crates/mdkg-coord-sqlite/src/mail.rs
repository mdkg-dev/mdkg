//! Rooms and messaging for the harness: an A2A-shaped
//! subset kept in the coordinator store, beside the lease tables.
//!
//! - A message is immutable once sent. Its `message_id` is chosen by the
//!   sender and unique in the store: sending it again as the same request
//!   (sender, kind, destination, context, references and body) is a no-op,
//!   anything else is refused.
//! - A post to a room fans out to every member's mailbox except the
//!   sender's, the writer's (for an import) and the roles either holds, as
//!   one message with one delivery per recipient. Reading doesn't ack; each
//!   delivery is acked on its own.
//! - Bodies are kept per message with its retention: purged once every
//!   delivery is acked (the default), after a number of days, or never. A
//!   `purge_on_ack` body with a `human:*` recipient also goes once every
//!   other delivery is acked and the human hold (days after sending) is
//!   over. With an ack grace ([`Store::set_ack_grace_days`]), an acked
//!   body stays readable that long after the ack that would purge it.
//!   The metadata (ids, kind, sender, recipients, digest, size, times,
//!   acks) always stays.
//!
//! Mail never touches the lease journal or state, so lease replay is
//! unaffected; purges run inside mail operations, with no daemon.

use std::collections::BTreeSet;

use mdkg_format::Digest;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;

use crate::ledger::{ExpiryPolicy, LedgerError, SendEffects, SendFlow};
use crate::store::{Store, StoreError};

mod fault;
pub use fault::{
    CLEAR_RECORD, FAULT_SCHEMA, FAULT_SENDER, FaultCleared, FaultOut, FaultSent, FaultTally,
    fault_rows_read,
};

/// The mail tables' own version, in `meta` as `msg_schema`. "2" added
/// `msg_messages.request_digest`, "3" `msg_deliveries.acked_via`, "4"
/// `msg_deliveries.shown_ms`; "5" adds durable `msg_decisions` provenance.
/// An older store gains them on open.
pub const MAIL_SCHEMA_VERSION: &str = "5";

pub(crate) const MAIL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS msg_rooms (
    name TEXT PRIMARY KEY,
    created_by TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS msg_members (
    room TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_ms INTEGER NOT NULL,
    PRIMARY KEY (room, mailbox)
);
CREATE TABLE IF NOT EXISTS msg_messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL UNIQUE,
    context_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    sender TEXT NOT NULL,
    via TEXT,
    room TEXT,
    reference_ids TEXT NOT NULL,
    digest TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    retention TEXT NOT NULL,
    sent_ms INTEGER NOT NULL,
    purge_after_ms INTEGER
);
CREATE INDEX IF NOT EXISTS msg_messages_context ON msg_messages (context_id, seq);
CREATE INDEX IF NOT EXISTS msg_messages_room ON msg_messages (room, seq);
CREATE TABLE IF NOT EXISTS msg_bodies (
    seq INTEGER PRIMARY KEY,
    parts TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS msg_deliveries (
    seq INTEGER NOT NULL,
    mailbox TEXT NOT NULL,
    state TEXT NOT NULL,
    acked_by TEXT,
    acked_via TEXT,
    acked_ms INTEGER,
    outcome TEXT,
    shown_ms INTEGER,
    PRIMARY KEY (seq, mailbox)
);
CREATE INDEX IF NOT EXISTS msg_deliveries_mailbox ON msg_deliveries (mailbox, state, seq);
CREATE TABLE IF NOT EXISTS msg_decisions (
    seq INTEGER PRIMARY KEY,
    provenance TEXT NOT NULL
);
";

/// Brings mail tables an older build created up to this version, inside
/// the store's open transaction. "1" to "2": `msg_messages` gains
/// `request_digest` (stores from the first M1 build, a76dd2c), set to an
/// empty value that never matches. Stores from the build after it
/// (f028e82) already have the column, filled with an older digest form,
/// which never matches either. Either way an old message's id can't be
/// retried, only re-sent under a new id, and output imported by those
/// builds, and by the build after them (de035c9, already at "2"), imports
/// again as a new message: their import ids were derived differently. Only
/// development stores are affected. "2" to "3": `msg_deliveries` gains
/// `acked_via`, empty for acks made before it. "3" to "4": `msg_deliveries`
/// gains `shown_ms`, when the message was first listed to that mailbox;
/// empty for deliveries before it, which are shown again before an ack.
/// "4" to "5" adds `msg_decisions`; existing mail is never upgraded into authority.
/// The mail versions [`upgrade`] moves on to [`MAIL_SCHEMA_VERSION`]; the
/// store copies itself first for each (forward-only, additive: AGENTS.md).
pub(crate) const UPGRADES_FROM: &[&str] = &["1", "2", "3", "4"];

pub(crate) fn upgrade(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let version: String =
        tx.query_row("SELECT value FROM meta WHERE key = 'msg_schema'", [], |r| {
            r.get(0)
        })?;
    if !UPGRADES_FROM.contains(&version.as_str()) {
        return Ok(());
    }
    let has_column = |table: &str, column: &str| -> Result<bool, StoreError> {
        Ok(tx
            .prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?
            .exists(params![table, column])?)
    };
    if !has_column("msg_messages", "request_digest")? {
        tx.execute_batch(
            "ALTER TABLE msg_messages ADD COLUMN request_digest TEXT NOT NULL DEFAULT ''",
        )?;
    }
    if !has_column("msg_deliveries", "acked_via")? {
        tx.execute_batch("ALTER TABLE msg_deliveries ADD COLUMN acked_via TEXT")?;
    }
    if !has_column("msg_deliveries", "shown_ms")? {
        tx.execute_batch("ALTER TABLE msg_deliveries ADD COLUMN shown_ms INTEGER")?;
    }
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'msg_schema'",
        params![MAIL_SCHEMA_VERSION],
    )?;
    Ok(())
}

/// The message kinds this build knows. The kernel attaches no state machine
/// to them. `receipt` and `fault` are sent only by mdkg's own `mdkg:` actors
/// (A26).
pub const KINDS: &[&str] = &[
    "review_request",
    "findings",
    "resolution",
    "question",
    "decision",
    "handoff",
    "note",
    "receipt",
    "fault",
];

/// The kinds only an `mdkg:` sender may send (A26).
pub const RESERVED_KINDS: &[&str] = &["receipt", "fault"];

/// The message id prefix reserved to `mdkg:` senders: a fault's copies are
/// `fault.<fault_id>.r<k>` (A26).
pub const FAULT_ID_PREFIX: &str = "fault.";

/// How long a message's body stays, fixed at send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Deleted once every delivery is acked, or, with a `human:*`
    /// recipient, once every other delivery is acked and the human hold is
    /// over.
    PurgeOnAck,
    /// Deleted this many days after sending, acked or not.
    Ttl(u32),
    /// Kept.
    Durable,
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

impl Retention {
    /// `purge_on_ack`, `ttl:<days>` (1 or more) or `durable`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "purge_on_ack" => Some(Self::PurgeOnAck),
            "durable" => Some(Self::Durable),
            _ => s
                .strip_prefix("ttl:")
                .and_then(|d| d.parse().ok())
                .filter(|d| *d > 0)
                .map(Self::Ttl),
        }
    }

    fn purge_after(self, sent_ms: u64) -> Option<u64> {
        match self {
            Self::Ttl(days) => Some(sent_ms.saturating_add(u64::from(days) * DAY_MS)),
            _ => None,
        }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PurgeOnAck => f.write_str("purge_on_ack"),
            Self::Ttl(d) => write!(f, "ttl:{d}"),
            Self::Durable => f.write_str("durable"),
        }
    }
}

/// Where a message goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum To {
    /// Every member of the room, at send time.
    Room(String),
    /// These mailboxes.
    Mailboxes(Vec<String>),
}

/// A message to send.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub message_id: String,
    /// Defaults to the first referenced message's context when the reply
    /// goes to the same place (room to room, direct to direct), else
    /// `room:<name>` for a room post, else the message's own id.
    pub context_id: Option<String>,
    pub kind: String,
    pub sender: String,
    /// Who wrote it for the sender (an import), if not the sender.
    pub via: Option<String>,
    /// Every mailbox the sender (or the writer) reads: itself and the roles
    /// it holds. Any of them makes it a member of a room.
    pub sender_mailboxes: Vec<String>,
    pub to: To,
    pub reference_ids: Vec<String>,
    /// A JSON array of parts.
    pub parts: Value,
    pub retention: Retention,
    /// For `purge_on_ack` with a `human:*` recipient: how many days after
    /// sending the body waits for the human (once every other delivery is
    /// acked).
    pub human_hold_days: u32,
    pub now_ms: u64,
}

/// Fixed approval scopes. Configuration can never add an action.
pub const DECISION_ACTIONS: &[&str] = &["implement", "test", "commit", "design", "review"];

/// Validated operator grant supplied only by the CLI's decision path. The
/// store stamps the message identity, body digest, CLI gate assertion and time.
/// The outside-turn assertion is advisory, not OS process authentication.
#[derive(Debug, Clone)]
pub struct DecisionGrant {
    pub human: String,
    pub source_file: String,
    pub action: String,
    pub node_ref: String,
    pub node_commit: String,
    pub node_path: String,
    pub node_digest: String,
    pub graph_config_path: String,
}

impl DecisionGrant {
    fn value(&self) -> Value {
        serde_json::json!({
            "human": self.human, "sourceFile": self.source_file,
            "action": self.action, "nodeRef": self.node_ref,
            "nodeCommit": self.node_commit, "nodePath": self.node_path,
            "nodeDigest": self.node_digest,
            "graphConfigPath": self.graph_config_path,
        })
    }
}

fn is_human(mailbox: &str) -> bool {
    mailbox.starts_with("human:")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub mailbox: String,
    /// `delivered` or `acked`.
    pub state: String,
    pub acked_by: Option<String>,
    /// For a human's ack: the folder name of the worktree the command ran
    /// against. Like `acked_by`, not authenticated: a hint of where the
    /// ack came from, never proof.
    pub acked_via: Option<String>,
    pub acked_ms: Option<u64>,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub seq: u64,
    pub message_id: String,
    pub context_id: String,
    pub kind: String,
    pub sender: String,
    pub via: Option<String>,
    pub room: Option<String>,
    pub reference_ids: Vec<String>,
    /// `None` once the body is purged.
    pub parts: Option<Value>,
    pub digest: String,
    pub size: u64,
    pub retention: String,
    pub sent_ms: u64,
    pub deliveries: Vec<Delivery>,
    /// Coordinator-stamped provenance, never taken from a body part.
    pub decision: Option<Value>,
}

impl Message {
    /// Every recipient mailbox.
    pub fn to(&self) -> Vec<String> {
        self.deliveries.iter().map(|d| d.mailbox.clone()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub name: String,
    pub created_by: String,
    pub created_ms: u64,
    pub members: Vec<String>,
    /// Highest message sequence posted here, including purged bodies.
    pub last_seq: u64,
}

/// What `inbox` and `wait` look for.
#[derive(Debug, Clone, Default)]
pub struct InboxQuery {
    /// The mailboxes read; deliveries to others are left out.
    pub mailboxes: Vec<String>,
    pub room: Option<String>,
    /// Only these kinds (any of them); empty, every kind but `receipt`,
    /// including kinds this build doesn't know. Receipts are the launcher's
    /// record of a turn, kept in its retained file: a read leaves them out
    /// unless it names them (A26).
    pub kinds: Vec<String>,
    /// Room mail only of these kinds; direct mail of every kind still
    /// counts. Empty, every kind.
    pub room_kinds: Vec<String>,
    /// Only messages after this sequence number.
    pub after: Option<u64>,
    /// Acked deliveries too.
    pub all: bool,
    pub limit: Option<usize>,
    pub now_ms: u64,
}

/// A message as a listing showed it, for [`Store::mark_shown`]: the
/// number, id and body digest it had then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub seq: u64,
    pub message_id: String,
    pub digest: String,
}

impl Shown {
    pub fn of(m: &Message) -> Self {
        Shown {
            seq: m.seq,
            message_id: m.message_id.clone(),
            digest: m.digest.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inbox {
    pub messages: Vec<Message>,
    /// Head read in the same transaction as these messages.
    pub event_cursor: String,
    /// The highest sequence returned, or `after` (else 0) when none.
    pub cursor: u64,
    /// More matched than `limit` allowed.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Acked {
    /// (message id, mailbox) pairs acked by this call or already acked.
    pub acked: Vec<(String, String)>,
    /// Messages whose bodies this call purged.
    pub purged: Vec<String>,
}

/// What [`Store::ack_through`] left unacked, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub message_id: String,
    pub seq: u64,
    pub mailbox: String,
    pub kind: String,
    /// `findings` or `fault` (not asked for by kind), or `not_shown` (never
    /// listed to that mailbox, so it can't be acked unseen).
    pub reason: &'static str,
}

#[derive(Debug)]
pub enum MailError {
    /// The message id is taken by a different message.
    IdReused(String),
    UnknownRoom(String),
    UnknownMessage(String),
    UnknownContext(String),
    RoomExists(String),
    /// The sender (and the writer) isn't a member of the room, or someone
    /// who isn't a member tried to add or remove others.
    NotMember {
        room: String,
        actor: String,
    },
    /// A mailbox named to remove isn't a member.
    Absent {
        room: String,
        mailbox: String,
    },
    /// Only the human removes a `human:*` member.
    HumanMember(String),
    /// None of the given mailboxes received the message.
    NotHolder(String),
    /// A selected read mailbox is not held by this actor in the snapshot.
    MailboxNotHeld {
        role: bool,
    },
    /// The message hasn't been listed to the acking mailbox yet (an inbox,
    /// thread or wait listing): an ack never purges one nobody has seen.
    NotShown(String),
    /// The sender names a conversation, or replies to a message, it isn't
    /// part of. Uniform, and names neither.
    NotInConversation,
    NoRecipients,
    /// A sender that isn't `mdkg:` used a reserved `fault.` id, as the
    /// message id or as a new conversation (A26).
    ReservedId,
    /// A row under a fault's id that isn't that fault's copy: store damage,
    /// never taken as the alert (A26).
    Integrity(String),
    Store(StoreError),
    /// The send's flow (A23 to A25) was refused.
    Ledger(LedgerError),
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IdReused(id) => write!(f, "message id {id} is taken by a different message"),
            Self::UnknownRoom(r) => write!(f, "no channel {r}"),
            Self::UnknownMessage(id) => write!(f, "no message {id}"),
            Self::UnknownContext(c) => write!(f, "no message or context {c}"),
            Self::RoomExists(r) => write!(f, "channel {r} already exists"),
            Self::NotMember { room, actor } => {
                write!(f, "{actor} isn't a member of channel {room}")
            }
            Self::Absent { room, mailbox } => {
                write!(f, "{mailbox} isn't a member of channel {room}")
            }
            Self::HumanMember(m) => write!(f, "only {m} can remove {m} from a channel"),
            Self::MailboxNotHeld { .. } => write!(f, "you don't hold that mailbox"),
            Self::NotHolder(id) => write!(f, "none of your mailboxes received {id}"),
            Self::NotShown(id) => write!(f, "{id} hasn't been shown to you yet"),
            Self::NoRecipients => write!(f, "the message has no recipients"),
            Self::NotInConversation => write!(f, "that conversation isn't one you're part of"),
            Self::ReservedId => write!(f, "fault.* message ids are reserved to mdkg"),
            Self::Integrity(id) => {
                write!(f, "message {id} is under a fault's id but isn't that fault")
            }
            Self::Store(e) => write!(f, "{e}"),
            Self::Ledger(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MailError {}

impl From<rusqlite::Error> for MailError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::Sqlite(e))
    }
}

impl From<serde_json::Error> for MailError {
    fn from(e: serde_json::Error) -> Self {
        Self::Store(StoreError::Json(e))
    }
}

impl From<StoreError> for MailError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// The digest of a send request: everything the sender asked for, in
/// canonical JSON. A retry with the same id must ask for exactly this
/// again. The writer (`via`) isn't part of it, so a second writer importing
/// the same output for the same request is a no-op.
/// The request digest of `out` (`sha256:<hex>`), for an id that must be
/// the same whenever the same request is made, such as an import's.
pub fn request_id(out: &Outgoing) -> Result<String, MailError> {
    let (_, body, _) = body_digest(&out.parts)?;
    request_digest(out, &body)
}

fn request_digest(out: &Outgoing, body_digest: &str) -> Result<String, MailError> {
    let to = match &out.to {
        To::Room(r) => serde_json::json!({"room": r}),
        To::Mailboxes(m) => {
            let mut m = m.clone();
            m.sort();
            m.dedup();
            serde_json::json!({"to": m})
        }
    };
    let request = serde_json::json!({
        "kind": out.kind,
        "sender": out.sender,
        "to": to,
        "context": out.context_id,
        "references": out.reference_ids,
        "body": body_digest,
    });
    Digest::of_record(&request)
        .map(|d| d.to_string())
        .map_err(|e| MailError::Store(StoreError::Schema(format!("request digest: {e}"))))
}

/// What a send's flow adds to its request (A23 to A25), or `None` when it
/// adds nothing.
fn flow_request(flow: &SendFlow) -> Option<Value> {
    let empty = flow.nodes.is_empty()
        && flow.entries.is_empty()
        && flow.supersedes.is_empty()
        && flow.slot.is_none()
        && flow.expires.is_none()
        && flow.range.is_none();
    (!empty).then(|| {
        serde_json::json!({
            "nodes": flow.nodes.iter().map(|n| &n.uuid).collect::<Vec<_>>(),
            "entries": flow.entries.iter().map(|(n, e)| format!("{}#{e}", n.uuid)).collect::<Vec<_>>(),
            "supersedes": flow.supersedes,
            "slot": flow.slot,
            "expires": flow.expires.map(|e| format!("{e:?}")),
            "range": flow.range,
        })
    })
}

/// The digest and size of a message body as stored.
pub fn body_digest(parts: &Value) -> Result<(String, String, u64), serde_json::Error> {
    let text = serde_json::to_string(parts)?;
    let digest = Digest::of_bytes(text.as_bytes()).to_string();
    let size = text.len() as u64;
    Ok((text, digest, size))
}

fn ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or_default()
}

fn sql_ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Whether message `m`'s body is due for purging at `?1` (now, in ms): a
/// `ttl` body at its time; a `purge_on_ack` body past its ack grace, or
/// held for a human once its hold is over and every other delivery is
/// acked. [`purge_due`] deletes by it, and [`mask_due`] and `msg list`
/// (`mail_list`) hide by it, so a read that purges and one that doesn't
/// return the same thing.
pub(crate) const DUE: &str = "m.purge_after_ms IS NOT NULL AND m.purge_after_ms <= ?1
       AND (m.retention != 'purge_on_ack' OR NOT EXISTS (
           SELECT 1 FROM msg_deliveries d
           WHERE d.seq = m.seq AND d.state != 'acked'
             AND substr(d.mailbox, 1, 6) != 'human:'))";

/// Deletes bodies whose time is up ([`DUE`]).
fn purge_due(tx: &Transaction<'_>, now_ms: u64) -> Result<(), MailError> {
    let due: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT m.seq, m.message_id FROM msg_messages m JOIN msg_bodies b ON b.seq = m.seq
             WHERE {DUE}
             ORDER BY m.seq"
        ))?;
        let rows = stmt.query_map(params![sql_ms(now_ms)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    if due.is_empty() {
        return Ok(());
    }
    for (seq, _) in &due {
        tx.execute("DELETE FROM msg_bodies WHERE seq = ?1", params![seq])?;
    }
    // Purged bodies are a change too: recorded in the same transaction,
    // as the actor whose command found them due.
    let ids: Vec<&String> = due.iter().map(|(_, id)| id).collect();
    crate::log::append_mail(
        tx,
        crate::log::SYSTEM_ACTOR,
        "msg.purge",
        ids.iter().map(|id| format!("msg:{id}")).collect(),
        serde_json::json!({"purged": ids, "reason": "retention"}),
    )?;
    Ok(())
}

/// For a read that writes nothing: shows each listed body that
/// [`purge_due`] would delete now as purged (no parts), so the read returns
/// what it would have returned had a purge run first. The body stays in
/// the store for a writing command to purge.
fn mask_due(conn: &Connection, messages: &mut [Message], now_ms: u64) -> Result<(), MailError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT 1 FROM msg_messages m WHERE m.seq = ?2 AND {DUE}"
    ))?;
    for m in messages.iter_mut().filter(|m| m.parts.is_some()) {
        if stmt.exists(params![sql_ms(now_ms), sql_ms(m.seq)])? {
            m.parts = None;
        }
    }
    Ok(())
}

/// Whether any of `who` sent, wrote or received the message.
fn took_part(conn: &Connection, m: &Message, who: &BTreeSet<&str>) -> Result<bool, MailError> {
    if who.contains(m.sender.as_str())
        || m.via.as_deref().is_some_and(|v| who.contains(v))
        || m.deliveries
            .iter()
            .any(|d| who.contains(d.mailbox.as_str()))
    {
        return Ok(true);
    }
    // A room's posts are read by its members, whenever they joined.
    Ok(match &m.room {
        Some(room) => members_of(conn, room)?
            .iter()
            .any(|member| who.contains(member.as_str())),
        None => false,
    })
}

/// Whether a post to `room` (or a direct one) may join `context`, as the
/// sender names it: one nobody has used yet, or one of the same kind (a
/// room's posts only, or direct messages only) the sender is part of.
/// This prevents posting into someone else's thread with a human.
fn may_join(
    conn: &Connection,
    context: &str,
    room: Option<&str>,
    who: &BTreeSet<&str>,
) -> Result<bool, MailError> {
    let mixed = match room {
        Some(r) => conn
            .prepare(
                "SELECT 1 FROM msg_messages WHERE context_id = ?1 AND (room IS NULL OR room != ?2)",
            )?
            .exists(params![context, r])?,
        None => {
            context.starts_with("room:")
                || context.starts_with("channel:")
                || context_has_room_post(conn, context)?
        }
    };
    if mixed {
        return Ok(false);
    }
    // A room's posts are read by its members, and posting checks that
    // the sender is one.
    if room.is_some() {
        return Ok(true);
    }
    let mut stmt = conn.prepare(
        "SELECT 1 FROM msg_messages m WHERE m.context_id = ?1
           AND (m.sender = ?2 OR m.via = ?2
                OR EXISTS (SELECT 1 FROM msg_deliveries d WHERE d.seq = m.seq AND d.mailbox = ?2))",
    )?;
    for mailbox in who {
        if stmt.exists(params![context, mailbox])? {
            return Ok(true);
        }
    }
    Ok(!conn
        .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1")?
        .exists(params![context])?)
}

fn context_has_room_post(conn: &Connection, context: &str) -> Result<bool, MailError> {
    Ok(conn
        .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1 AND room IS NOT NULL")?
        .exists(params![context])?)
}

/// Which messages of a thread or a room to read: those after a seq, the
/// oldest `limit` of them or, with `newest`, the newest.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page {
    pub after: Option<u64>,
    pub limit: Option<usize>,
    pub newest: bool,
}

/// One page of messages whose `column` (context_id or room) is `value`,
/// oldest first.
fn page_of(
    tx: &Transaction<'_>,
    column: &str,
    value: &str,
    page: Page,
) -> Result<Vec<Message>, MailError> {
    let order = if page.newest { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM msg_messages m LEFT JOIN msg_bodies b ON b.seq = m.seq
         WHERE m.{column} = ?1 AND m.seq > ?3 ORDER BY m.seq {order} LIMIT ?2"
    );
    let mut messages = Vec::new();
    let mut stmt = tx.prepare(&sql)?;
    let cap = page
        .limit
        .map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
    let mut rows = stmt.query(params![value, cap, sql_ms(page.after.unwrap_or(0))])?;
    while let Some(r) = rows.next()? {
        messages.push(message_from_row(tx, r)?);
    }
    if page.newest {
        messages.reverse();
    }
    Ok(messages)
}

fn members_of(conn: &Connection, room: &str) -> Result<Vec<String>, MailError> {
    let mut stmt =
        conn.prepare("SELECT mailbox FROM msg_members WHERE room = ?1 ORDER BY mailbox")?;
    let rows = stmt.query_map(params![room], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn room_exists(conn: &Connection, room: &str) -> Result<bool, MailError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM msg_rooms WHERE name = ?1",
            params![room],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn deliveries_of(conn: &Connection, seq: u64) -> Result<Vec<Delivery>, MailError> {
    let mut stmt = conn.prepare(
        "SELECT mailbox, state, acked_by, acked_ms, outcome, acked_via FROM msg_deliveries
         WHERE seq = ?1 ORDER BY mailbox",
    )?;
    let rows = stmt.query_map(params![sql_ms(seq)], |r| {
        Ok(Delivery {
            mailbox: r.get(0)?,
            state: r.get(1)?,
            acked_by: r.get(2)?,
            acked_via: r.get(5)?,
            acked_ms: r.get::<_, Option<i64>>(3)?.map(ms),
            outcome: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const MESSAGE_COLUMNS: &str = "m.seq, m.message_id, m.context_id, m.kind, m.sender, m.via, m.room,
     m.reference_ids, m.digest, m.size, m.retention, m.sent_ms, b.parts";

fn message_from_row(conn: &Connection, r: &rusqlite::Row<'_>) -> Result<Message, MailError> {
    let seq = ms(r.get(0)?);
    let references: String = r.get(7)?;
    let parts: Option<String> = r.get(12)?;
    Ok(Message {
        seq,
        message_id: r.get(1)?,
        context_id: r.get(2)?,
        kind: r.get(3)?,
        sender: r.get(4)?,
        via: r.get(5)?,
        room: r.get(6)?,
        reference_ids: serde_json::from_str(&references)?,
        parts: parts.map(|p| serde_json::from_str(&p)).transpose()?,
        digest: r.get(8)?,
        size: ms(r.get(9)?),
        retention: r.get(10)?,
        sent_ms: ms(r.get(11)?),
        deliveries: deliveries_of(conn, seq)?,
        decision: decision_of(conn, seq)?,
    })
}

fn decision_of(conn: &Connection, seq: u64) -> Result<Option<Value>, MailError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT provenance FROM msg_decisions WHERE seq = ?1",
            params![sql_ms(seq)],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|v| serde_json::from_str(&v).map_err(MailError::from))
        .transpose()
}

/// Audit reads never open a writer or migrate logical contents. SQLite may
/// maintain WAL-index cache files during a read.
/// Missing stores have no decisions. Incompatible existing schemas refuse.
pub fn read_decisions(
    path: &std::path::Path,
    after: u64,
    limit: usize,
    ids: &[String],
) -> Result<Vec<Value>, MailError> {
    let present = crate::store::store_file_present(path).map_err(StoreError::from)?;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::from(e).into()),
        Ok(meta) if !meta.is_file() => {
            return Err(StoreError::Schema("decision audit needs a regular store".into()).into());
        }
        Ok(_) => (),
    }
    if !present {
        return Ok(Vec::new());
    }
    let Some(conn) = Store::open_raw_read_only(path)? else {
        return Ok(Vec::new());
    };
    crate::store::check_mail_schema(&conn)?;
    if ids.is_empty() {
        decision_rows(&conn, after, limit)
    } else {
        let mut records = Vec::new();
        for id in ids {
            if let Some(row) = decision_record(&conn, id)? {
                records.push(row);
            }
        }
        Ok(records)
    }
}

fn decision_rows(conn: &Connection, after: u64, limit: usize) -> Result<Vec<Value>, MailError> {
    let mut statement = conn.prepare(
        "SELECT seq, provenance FROM msg_decisions WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = statement.query_map(params![sql_ms(after), limit.min(1000) as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (seq, text) = row?;
        Ok(serde_json::json!({"seq": ms(seq), "decision": serde_json::from_str::<Value>(&text)?}))
    })
    .collect()
}

fn decision_record(conn: &Connection, message_id: &str) -> Result<Option<Value>, MailError> {
    let row: Option<(i64, String)> = conn.query_row(
        "SELECT d.seq, d.provenance FROM msg_decisions d JOIN msg_messages m ON m.seq=d.seq WHERE m.message_id=?1",
        params![message_id], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    row.map(|(seq, text)| {
        Ok(serde_json::json!({"seq": ms(seq), "decision": serde_json::from_str::<Value>(&text)?}))
    })
    .transpose()
}

pub(crate) fn message_by_id(conn: &Connection, id: &str) -> Result<Option<Message>, MailError> {
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM msg_messages m LEFT JOIN msg_bodies b ON b.seq = m.seq
         WHERE m.message_id = ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(r) => Ok(Some(message_from_row(conn, r)?)),
        None => Ok(None),
    }
}

/// Records that `listed` were listed to `mailboxes`: their deliveries there
/// that have no time yet. A message that no longer matches its number, id
/// and digest as listed (a restore in between) is left unshown.
fn mark_shown(
    tx: &Transaction<'_>,
    listed: &[Shown],
    mailboxes: &[String],
    now_ms: u64,
) -> Result<(), MailError> {
    let mut stmt = tx.prepare(
        "UPDATE msg_deliveries SET shown_ms = ?5
         WHERE seq = ?1 AND mailbox = ?4 AND shown_ms IS NULL
           AND EXISTS (SELECT 1 FROM msg_messages
                       WHERE seq = ?1 AND message_id = ?2 AND digest = ?3)",
    )?;
    for m in listed {
        for mailbox in mailboxes {
            stmt.execute(params![
                sql_ms(m.seq),
                m.message_id,
                m.digest,
                mailbox,
                sql_ms(now_ms)
            ])?;
        }
    }
    Ok(())
}

/// Every delivery acked, or every one but a human's whose hold is over:
/// the body of a `purge_on_ack` message goes. With an ack grace it goes
/// that long after this ack instead, when [`purge_due`] finds it due
/// (friction item 72), so a recipient can still read what it acked.
fn purge_if_done(
    tx: &Transaction<'_>,
    seq: u64,
    now_ms: u64,
    grace_ms: u64,
) -> Result<bool, MailError> {
    let (agents, humans): (i64, i64) = tx.query_row(
        "SELECT COALESCE(SUM(substr(mailbox, 1, 6) != 'human:'), 0),
                COALESCE(SUM(substr(mailbox, 1, 6) = 'human:'), 0)
         FROM msg_deliveries WHERE seq = ?1 AND state != 'acked'",
        params![sql_ms(seq)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (retention, purge_after): (String, Option<i64>) = tx.query_row(
        "SELECT retention, purge_after_ms FROM msg_messages WHERE seq = ?1",
        params![sql_ms(seq)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let hold_over = purge_after.is_some_and(|p| ms(p) <= now_ms);
    if agents > 0 || retention != Retention::PurgeOnAck.to_string() {
        return Ok(false);
    }
    if grace_ms > 0 {
        // A human hold still running ends later than the grace or not:
        // the later of the two holds.
        keep_for_grace(tx, seq, now_ms, grace_ms)?;
        return Ok(false);
    }
    if humans > 0 && !hold_over {
        return Ok(false);
    }
    Ok(tx.execute(
        "DELETE FROM msg_bodies WHERE seq = ?1",
        params![sql_ms(seq)],
    )? > 0)
}

/// Keeps an acked `purge_on_ack` body for `grace_ms` from `now_ms`:
/// `purge_after_ms` moves to the later of its value and that time, so a
/// human hold is never cut short, and [`purge_due`] purges the body then.
/// A message whose body is already gone is left alone.
pub(crate) fn keep_for_grace(
    tx: &Transaction<'_>,
    seq: u64,
    now_ms: u64,
    grace_ms: u64,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "UPDATE msg_messages SET purge_after_ms = MAX(COALESCE(purge_after_ms, 0), ?2)
         WHERE seq = ?1 AND EXISTS (SELECT 1 FROM msg_bodies WHERE seq = ?1)",
        params![sql_ms(seq), sql_ms(now_ms.saturating_add(grace_ms))],
    )?;
    Ok(())
}

/// [`Store::send`] (with a decision's grant, if any) inside a transaction
/// the caller commits. A new message's flow (A23 to A25: its nodes and
/// entries, what it supersedes, its expiry and human hold) is recorded by
/// [`crate::ledger::on_send`] in the same transaction, and its effects
/// returned; a retried one's was recorded when it was new, and it has
/// none.
#[allow(clippy::too_many_arguments)]
fn send_in(
    tx: &Transaction<'_>,
    out: &Outgoing,
    grant: Option<&DecisionGrant>,
    base_request: &str,
    text: &str,
    digest: &str,
    size: u64,
    flow: &SendFlow,
    policy: &ExpiryPolicy,
    ack_grace_ms: u64,
) -> Result<(Message, bool, SendEffects), MailError> {
    let with_grant = |grant: &DecisionGrant| {
        Digest::of_record(&serde_json::json!({"request": base_request, "grant": grant.value()}))
            .map(|digest| digest.to_string())
            .map_err(|e| MailError::Store(StoreError::Schema(e.to_string())))
    };
    // A fault's copies are `fault.<fault_id>.r<k>`, and ids and contexts are
    // one namespace, so nobody else may take one first (A26). A context
    // naming an existing message is a reply into it, which is fine.
    if !out.sender.starts_with("mdkg:") {
        let reserved = |id: &str| {
            id.get(..FAULT_ID_PREFIX.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(FAULT_ID_PREFIX))
        };
        if reserved(&out.message_id) {
            return Err(MailError::ReservedId);
        }
        if let Some(c) = &out.context_id
            && reserved(c)
            && message_by_id(tx, c)?.is_none()
        {
            return Err(MailError::ReservedId);
        }
    }
    let mut request = base_request.to_string();
    if let Some(grant) = grant {
        // Send time is stamped once; an otherwise identical retry keeps
        // that original time. Changed scope/provenance is an ID collision.
        request = with_grant(grant)?;
    }
    if let Some(existing) = message_by_id(tx, &out.message_id)? {
        if let Some(grant) = grant
            && let Some(commit) = existing
                .decision
                .as_ref()
                .and_then(|p| p["nodeCommit"].as_str())
        {
            // HEAD can advance without changing the pinned node. The CLI
            // has revalidated current delegation and committed bytes; an
            // otherwise identical retry keeps the original immutable
            // binding. Every other grant and request field still matches.
            let mut original_binding = grant.clone();
            original_binding.node_commit = commit.into();
            request = with_grant(&original_binding)?;
        }
        let same: Option<String> = tx
            .query_row(
                "SELECT request_digest FROM msg_messages WHERE message_id = ?1",
                params![out.message_id],
                |r| r.get(0),
            )
            .optional()?;
        if same.as_deref() == Some(request.as_str()) {
            // The caller commits, so what purge_due cleared stays
            // cleared (a dropped transaction would roll it back).
            return Ok((existing, true, SendEffects::default()));
        }
        return Err(MailError::IdReused(out.message_id.clone()));
    }
    // Nobody gets their own post: not the sender, not the writer, not a
    // role either holds (D1).
    let mut skip: BTreeSet<&str> = BTreeSet::from([out.sender.as_str()]);
    if let Some(via) = &out.via {
        skip.insert(via.as_str());
    }
    skip.extend(out.sender_mailboxes.iter().map(String::as_str));
    // A reply stays in the conversation it answers only when it goes to
    // the same place: a room post replying in the same room, or a direct
    // message replying to a direct one (D4, refined). A direct reply to
    // a room post starts its own conversation, so it never appears in
    // the room's.
    let replied = match out.reference_ids.first() {
        Some(first) => message_by_id(tx, first)?,
        None => None,
    };
    let here = match &out.to {
        To::Room(r) => Some(r.as_str()),
        To::Mailboxes(_) => None,
    };
    // A direct send never takes a conversation that holds a room post,
    // even one an older build gave to a direct message.
    let reply_context = match replied
        .filter(|m| m.room.as_deref() == here)
        .map(|m| m.context_id)
    {
        Some(c)
            if here.is_none()
                && (c.starts_with("room:")
                    || c.starts_with("channel:")
                    || context_has_room_post(tx, &c)?) =>
        {
            None
        }
        other => other,
    };
    let (room, recipients, default_context) = match &out.to {
        To::Room(room) => {
            if !room_exists(tx, room)? {
                return Err(MailError::UnknownRoom(room.clone()));
            }
            let members = members_of(tx, room)?;
            let writer = out.via.as_deref().unwrap_or(&out.sender);
            let member =
                |m: &String| m == &out.sender || m == writer || out.sender_mailboxes.contains(m);
            if !members.iter().any(member) {
                return Err(MailError::NotMember {
                    room: room.clone(),
                    actor: writer.to_string(),
                });
            }
            let context = reply_context
                .clone()
                .unwrap_or_else(|| format!("room:{room}"));
            (Some(room.clone()), members, context)
        }
        To::Mailboxes(list) => {
            let context = reply_context
                .clone()
                .unwrap_or_else(|| out.message_id.clone());
            (None, list.clone(), context)
        }
    };
    // Only a conversation the sender is part of: every
    // message it replies to, and a context it names. Checked after room
    // membership, so a non-member's refusal stays the uniform one.
    for id in &out.reference_ids {
        if let Some(m) = message_by_id(tx, id)?
            && !took_part(tx, &m, &skip)?
        {
            return Err(MailError::NotInConversation);
        }
    }
    if let Some(c) = &out.context_id
        && !may_join(tx, c, here, &skip)?
    {
        return Err(MailError::NotInConversation);
    }
    // Ids and contexts are one namespace, since `msg thread` reads
    // either: a new message's id can't be a context
    // someone already has, and a named context can't be another
    // conversation's message.
    if tx
        .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1")?
        .exists(params![out.message_id])?
    {
        return Err(MailError::IdReused(out.message_id.clone()));
    }
    if let Some(c) = &out.context_id
        && message_by_id(tx, c)?.is_some_and(|m| m.context_id != *c)
    {
        return Err(MailError::NotInConversation);
    }
    let recipients: BTreeSet<String> = recipients
        .into_iter()
        .filter(|m| !skip.contains(m.as_str()))
        .collect();
    if recipients.is_empty() {
        return Err(MailError::NoRecipients);
    }
    let context = out.context_id.clone().unwrap_or(default_context);
    let purge_after = match out.retention {
        Retention::PurgeOnAck if recipients.iter().any(|m| is_human(m)) => Some(
            out.now_ms
                .saturating_add(u64::from(out.human_hold_days) * DAY_MS),
        ),
        other => other.purge_after(out.now_ms),
    };
    tx.execute(
        "INSERT INTO msg_messages (message_id, context_id, kind, sender, via, room,
             reference_ids, digest, request_digest, size, retention, sent_ms, purge_after_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            out.message_id,
            context,
            out.kind,
            out.sender,
            out.via,
            room,
            serde_json::to_string(&out.reference_ids)?,
            digest,
            request,
            sql_ms(size),
            out.retention.to_string(),
            sql_ms(out.now_ms),
            purge_after.map(sql_ms),
        ],
    )?;
    let seq = ms(tx.last_insert_rowid());
    if let Some(grant) = grant {
        let mut provenance = grant.value();
        let fields = provenance.as_object_mut().expect("grant object");
        fields.insert("sender".into(), out.sender.clone().into());
        fields.insert("messageId".into(), out.message_id.clone().into());
        fields.insert("outsideAgentTurn".into(), true.into());
        fields.insert("sentAtMs".into(), out.now_ms.into());
        fields.insert("bodyDigest".into(), digest.into());
        tx.execute(
            "INSERT INTO msg_decisions (seq, provenance) VALUES (?1, ?2)",
            params![sql_ms(seq), serde_json::to_string(&provenance)?],
        )?;
    }
    tx.execute(
        "INSERT INTO msg_bodies (seq, parts) VALUES (?1, ?2)",
        params![sql_ms(seq), text],
    )?;
    for mailbox in &recipients {
        tx.execute(
            "INSERT INTO msg_deliveries (seq, mailbox, state) VALUES (?1, ?2, 'delivered')",
            params![sql_ms(seq), mailbox],
        )?;
    }
    let effects = crate::ledger::on_send(
        tx,
        seq,
        &out.message_id,
        &out.sender,
        &out.kind,
        out.now_ms,
        out.human_hold_days,
        flow,
        policy,
        out.now_ms,
        ack_grace_ms,
    )
    .map_err(MailError::Ledger)?;
    let message = message_by_id(tx, &out.message_id)?
        .ok_or_else(|| MailError::UnknownMessage(out.message_id.clone()))?;
    // The record carries the body's digest, never the body (W1).
    let mut subject = vec![format!("msg:{}", out.message_id)];
    if let Some(r) = &room {
        subject.push(format!("room:{r}"));
    }
    // A node-bound message names its nodes' bare ids, so `msg watch` on a
    // node subject sees it (A23, "Record"; format §9.1).
    for n in &flow.nodes {
        subject.push(mdkg_log::record::node_ref(&n.id));
    }
    let mut body = serde_json::json!({
        "message_id": out.message_id,
        "context_id": message.context_id,
        "kind": out.kind,
        "sender": out.sender,
        "via": out.via,
        "room": room,
        "to": recipients,
        "references": out.reference_ids,
        "digest": digest,
        "size": size,
        "retention": out.retention.to_string(),
        "decision": message.decision,
    });
    if !flow.nodes.is_empty() {
        body["nodes"] = flow.nodes.iter().map(|n| n.id.clone()).collect();
    }
    // A person's own send names its proof, none while sends are unsigned,
    // so an unsigned person send can be told apart later (DECISION 39,
    // 2026-11-04).
    if out.via.is_none() && out.sender.starts_with("human:") {
        body["person_proof"] = Value::Null;
    }
    crate::log::append_mail(
        tx,
        out.via.as_deref().unwrap_or(&out.sender),
        "msg.send",
        subject,
        body,
    )?;
    Ok((message, false, effects))
}

/// [`Store::ack`] inside a transaction the caller commits.
#[allow(clippy::too_many_arguments)]
fn ack_in(
    tx: &Transaction<'_>,
    ids: &[String],
    mailboxes: &[String],
    actor: &str,
    via: Option<&str>,
    outcome: Option<&str>,
    now_ms: u64,
    grace_ms: u64,
) -> Result<Acked, MailError> {
    let mut result = Acked::default();
    let mut newly: Vec<Value> = Vec::new();
    for id in ids {
        let message =
            message_by_id(tx, id)?.ok_or_else(|| MailError::UnknownMessage(id.clone()))?;
        let mine: Vec<&Delivery> = message
            .deliveries
            .iter()
            .filter(|d| mailboxes.contains(&d.mailbox))
            .collect();
        if mine.is_empty() {
            return Err(MailError::NotHolder(id.clone()));
        }
        // Never acked (and so never purged) before it's been listed to
        // that mailbox: refused, with nothing acked.
        for d in &mine {
            let shown: Option<Option<i64>> = tx
                .query_row(
                    "SELECT shown_ms FROM msg_deliveries
                     WHERE seq = ?1 AND mailbox = ?2 AND state != 'acked'",
                    params![sql_ms(message.seq), d.mailbox],
                    |r| r.get(0),
                )
                .optional()?;
            if matches!(shown, Some(None)) {
                return Err(MailError::NotShown(id.clone()));
            }
        }
        let mut changed_any = false;
        for d in mine {
            let changed = tx.execute(
                "UPDATE msg_deliveries SET state = 'acked', acked_by = ?3, acked_ms = ?4,
                     outcome = ?5, acked_via = ?6
                 WHERE seq = ?1 AND mailbox = ?2 AND state != 'acked'",
                params![
                    sql_ms(message.seq),
                    d.mailbox,
                    actor,
                    sql_ms(now_ms),
                    outcome,
                    via
                ],
            )?;
            if changed > 0 {
                changed_any = true;
                newly.push(serde_json::json!({"message_id": id, "mailbox": d.mailbox}));
            }
            result.acked.push((id.clone(), d.mailbox.clone()));
        }
        // A repeat ack starts no new grace: only the ack that ends the
        // last delivery sets the body's purge time, so acking it again
        // can't keep it forever. Without a grace it purges as before.
        if (changed_any || grace_ms == 0) && purge_if_done(tx, message.seq, now_ms, grace_ms)? {
            result.purged.push(id.clone());
        }
    }
    // Only what changed; the outcome as a digest, like a body (W1).
    if !newly.is_empty() || !result.purged.is_empty() {
        let mut subject: Vec<String> = Vec::new();
        for id in ids {
            let s = format!("msg:{id}");
            if !subject.contains(&s) {
                subject.push(s);
            }
        }
        crate::log::append_mail(
            tx,
            actor,
            "msg.ack",
            subject,
            serde_json::json!({
                "acked": newly,
                "by": actor,
                "via": via,
                "outcome_digest": outcome.map(crate::log::text_digest),
                "purged": result.purged,
            }),
        )?;
    }
    Ok(result)
}

impl Store {
    /// Keeps an acked `purge_on_ack` body for `days` after the ack that
    /// ends it, instead of purging it then: `coordination.messages.
    /// ack_grace_days` (friction item 72). Human holds and TTLs keep
    /// their own times; 0, the default, purges at the ack.
    pub fn set_ack_grace_days(&mut self, days: u32) {
        self.ack_grace_ms = u64::from(days) * DAY_MS;
    }

    /// Mail reads after this write nothing: no purge and no log record.
    /// Bodies due for purging read as purged ([`mask_due`]) and stay for a
    /// writing command to delete. Marking (`mark_shown`) and acks still
    /// write; callers that must not write don't call them. A store opened
    /// read-only reads this way from the start.
    pub fn set_pure_reads(&mut self) {
        self.pure_reads = true;
    }

    /// A read's transaction: with pure reads a deferred one that takes no
    /// write lock and commits nothing, else the writer's ([`Self::mail_tx`]).
    fn read_tx(&mut self) -> Result<Transaction<'_>, MailError> {
        if !self.pure_reads {
            return self.mail_tx();
        }
        let tx = self.conn.unchecked_transaction()?;
        // Its first read takes the snapshot, so a clock read after this
        // returns is read within it, as it is under the writer's lock.
        crate::store::check_mail_schema(&tx)?;
        Ok(tx)
    }

    /// Before a read: purges what's due, unless reads are pure.
    fn purge_before_read(tx: &Transaction<'_>, pure: bool, now_ms: u64) -> Result<(), MailError> {
        if pure { Ok(()) } else { purge_due(tx, now_ms) }
    }

    /// After a read: with pure reads, hides what a purge would have taken.
    fn mask_after_read(
        tx: &Transaction<'_>,
        pure: bool,
        messages: &mut [Message],
        now_ms: u64,
    ) -> Result<(), MailError> {
        if pure {
            mask_due(tx, messages, now_ms)
        } else {
            Ok(())
        }
    }

    fn mail_tx(&mut self) -> Result<Transaction<'_>, MailError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::store::check_mail_schema(&tx)?;
        Ok(tx)
    }

    /// Sends one message. Returns it, and whether it had been sent before
    /// (the same id and request: nothing new is stored).
    pub fn send(&mut self, out: &Outgoing) -> Result<(Message, bool), MailError> {
        self.send_with_decision(out, None)
    }

    /// Send and record provenance in the same transaction. Generic sends and
    /// imports cannot create decisions, or retrofit authority onto old mail.
    pub fn send_decision(
        &mut self,
        out: &Outgoing,
        grant: &DecisionGrant,
    ) -> Result<(Message, bool), MailError> {
        self.send_with_decision(out, Some(grant))
    }

    /// [`Store::send`] with the flow a send adds (A23 to A25), under
    /// `policy`, then `then`, inside the send's transaction once the
    /// message is stored or found again as the same request: a review
    /// request's round opens there, so the request and its round commit
    /// together or not at all (Terra's #6548). The flow is part of the
    /// request, so a retry with another flow is `IdReused`. Returns the
    /// flow's effects too (none for a retried message).
    pub fn send_flow(
        &mut self,
        out: &Outgoing,
        flow: &SendFlow,
        policy: &ExpiryPolicy,
        then: &mut dyn FnMut(&Transaction<'_>, &Message) -> Result<(), MailError>,
    ) -> Result<(Message, bool, SendEffects), MailError> {
        if out.kind == "decision" {
            return Err(MailError::Store(StoreError::Schema(
                "decisions require the validated outside-turn delegate send path".into(),
            )));
        }
        let (text, digest, size) = body_digest(&out.parts)?;
        let mut request = request_digest(out, &digest)?;
        if let Some(f) = flow_request(flow) {
            // Only a send that adds a flow digests it, so a plain send's
            // request, and its retries, stay as before.
            request = Digest::of_record(&serde_json::json!({"request": request, "flow": f}))
                .map(|d| d.to_string())
                .map_err(|e| MailError::Store(StoreError::Schema(e.to_string())))?;
        }
        let grace = self.ack_grace_ms;
        let tx = self.mail_tx()?;
        purge_due(&tx, out.now_ms)?;
        let sent = send_in(
            &tx, out, None, &request, &text, &digest, size, flow, policy, grace,
        )?;
        then(&tx, &sent.0)?;
        tx.commit()?;
        Ok(sent)
    }

    fn send_with_decision(
        &mut self,
        out: &Outgoing,
        grant: Option<&DecisionGrant>,
    ) -> Result<(Message, bool), MailError> {
        if (out.kind == "decision") != grant.is_some()
            || grant.is_some_and(|g| {
                !DECISION_ACTIONS.contains(&g.action.as_str()) || out.via.is_some()
            })
        {
            return Err(MailError::Store(StoreError::Schema(
                "decisions require the validated outside-turn delegate send path".into(),
            )));
        }
        let (text, digest, size) = body_digest(&out.parts)?;
        let base_request = request_digest(out, &digest)?;
        let grace = self.ack_grace_ms;
        let tx = self.mail_tx()?;
        purge_due(&tx, out.now_ms)?;
        let (message, existed, _) = send_in(
            &tx,
            out,
            grant,
            &base_request,
            &text,
            &digest,
            size,
            &SendFlow::default(),
            &ExpiryPolicy::default(),
            grace,
        )?;
        let sent = (message, existed);
        // A repeat commits too: what purge_due cleared stays cleared.
        tx.commit()?;
        Ok(sent)
    }

    /// Durable audit metadata only: no purge, mark-shown, ack or journal write.
    pub fn decisions(&self, after: u64, limit: usize) -> Result<Vec<Value>, MailError> {
        decision_rows(&self.conn, after, limit)
    }

    pub fn decision_record(&self, message_id: &str) -> Result<Option<Value>, MailError> {
        decision_record(&self.conn, message_id)
    }

    /// Messages delivered to the query's mailboxes, oldest first.
    pub fn inbox(&mut self, q: &InboxQuery) -> Result<Inbox, MailError> {
        self.inbox_from(q, &[])
    }

    /// [`Store::inbox`], only of messages from these senders (any of
    /// them); empty, every sender. The limit counts only those.
    pub fn inbox_from(&mut self, q: &InboxQuery, senders: &[String]) -> Result<Inbox, MailError> {
        self.inbox_from_resolved(q, senders, None, || q.now_ms)
            .map(|(inbox, _)| inbox)
    }

    /// Resolve role mailboxes, messages and head in one transaction.
    pub fn inbox_from_for_actor(
        &mut self,
        q: &InboxQuery,
        senders: &[String],
        actor: &str,
        mailbox: Option<&str>,
        clock: impl FnOnce() -> u64,
    ) -> Result<(Inbox, Vec<String>), MailError> {
        self.inbox_from_resolved(q, senders, Some((actor, mailbox)), clock)
    }

    fn inbox_from_resolved(
        &mut self,
        q: &InboxQuery,
        senders: &[String],
        reader: Option<(&str, Option<&str>)>,
        clock: impl FnOnce() -> u64,
    ) -> Result<(Inbox, Vec<String>), MailError> {
        let ttl = self.ttl;
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        let mut q = q.clone();
        // The lock may have waited past a role deadline. Bind time only once
        // the snapshot is acquired, never to the caller's earlier attempt.
        q.now_ms = clock();
        if let Some((actor, mailbox)) = reader {
            q.mailboxes = read_mailboxes_on(&tx, ttl, actor, mailbox, q.now_ms)?;
        }
        Self::purge_before_read(&tx, pure, q.now_ms)?;
        let after = q.after.unwrap_or(0);
        let mut seqs: Vec<u64> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT d.seq, m.kind, m.room IS NOT NULL, m.sender FROM msg_deliveries d
                 JOIN msg_messages m ON m.seq = d.seq
                 WHERE d.mailbox = ?1 AND d.seq > ?2 AND (?3 OR d.state = 'delivered')
                   AND (?4 IS NULL OR m.room = ?4)",
            )?;
            for mailbox in &q.mailboxes {
                let rows = stmt.query_map(params![mailbox, sql_ms(after), q.all, q.room], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                for row in rows {
                    let (seq, kind, in_room, sender) = row?;
                    let wanted = |kinds: &[String]| kinds.is_empty() || kinds.contains(&kind);
                    let kind_wanted = if q.kinds.is_empty() {
                        kind != "receipt"
                    } else {
                        q.kinds.contains(&kind)
                    };
                    let from = senders.is_empty() || senders.contains(&sender);
                    if from && kind_wanted && (!in_room || wanted(&q.room_kinds)) {
                        seqs.push(ms(seq));
                    }
                }
            }
        }
        seqs.sort_unstable();
        seqs.dedup();
        let more = q.limit.is_some_and(|l| seqs.len() > l);
        if let Some(limit) = q.limit {
            seqs.truncate(limit);
        }
        let mut messages = Vec::new();
        for seq in &seqs {
            let sql = format!(
                "SELECT {MESSAGE_COLUMNS} FROM msg_messages m LEFT JOIN msg_bodies b ON b.seq = m.seq
                 WHERE m.seq = ?1"
            );
            let mut stmt = tx.prepare(&sql)?;
            let mut rows = stmt.query(params![sql_ms(*seq)])?;
            if let Some(r) = rows.next()? {
                messages.push(message_from_row(&tx, r)?);
            }
        }
        Self::mask_after_read(&tx, pure, &mut messages, q.now_ms)?;
        let event_cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        let cursor = seqs.last().copied().unwrap_or(after);
        Ok((
            Inbox {
                messages,
                event_cursor,
                cursor,
                more,
            },
            q.mailboxes,
        ))
    }

    /// Acks the deliveries of `ids` to any of `mailboxes`, as `actor`.
    /// Acking again is a no-op. A message with no delivery to any of the
    /// mailboxes is refused, and nothing is acked.
    pub fn ack(
        &mut self,
        ids: &[String],
        mailboxes: &[String],
        actor: &str,
        via: Option<&str>,
        outcome: Option<&str>,
        now_ms: u64,
    ) -> Result<Acked, MailError> {
        let grace = self.ack_grace_ms;
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let result = ack_in(&tx, ids, mailboxes, actor, via, outcome, now_ms, grace)?;
        tx.commit()?;
        Ok(result)
    }

    /// Acks every unacked delivery to `mailboxes` numbered `through` or
    /// lower, as `actor`, in one transaction; with `kinds`, only those
    /// kinds. Findings and faults are acked only when `kinds` names them
    /// (a fault is never bulk-acked, A26 and R6), and a delivery never
    /// listed to its mailbox is never acked: both are returned as skipped.
    /// The rest is as [`Store::ack`].
    #[allow(clippy::too_many_arguments)]
    pub fn ack_through(
        &mut self,
        through: u64,
        kinds: &[String],
        mailboxes: &[String],
        actor: &str,
        via: Option<&str>,
        outcome: Option<&str>,
        now_ms: u64,
    ) -> Result<(Acked, Vec<Skipped>), MailError> {
        let grace = self.ack_grace_ms;
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let mut skipped = Vec::new();
        let mut result = Acked::default();
        for mailbox in mailboxes {
            let mut ids: Vec<String> = Vec::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT m.message_id, d.seq, m.kind, d.shown_ms IS NOT NULL
                     FROM msg_deliveries d JOIN msg_messages m ON m.seq = d.seq
                     WHERE d.mailbox = ?1 AND d.seq <= ?2 AND d.state = 'delivered'
                     ORDER BY d.seq",
                )?;
                let rows = stmt.query_map(params![mailbox, sql_ms(through)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })?;
                for row in rows {
                    let (message_id, seq, kind, shown) = row?;
                    if !kinds.is_empty() && !kinds.contains(&kind) {
                        continue;
                    }
                    let reason = if kind == "findings" && !kinds.contains(&kind) {
                        Some("findings")
                    } else if kind == "fault" && !kinds.contains(&kind) {
                        Some("fault")
                    } else if !shown {
                        Some("not_shown")
                    } else {
                        None
                    };
                    match reason {
                        Some(reason) => skipped.push(Skipped {
                            message_id,
                            seq: ms(seq),
                            mailbox: mailbox.clone(),
                            kind,
                            reason,
                        }),
                        None => ids.push(message_id),
                    }
                }
            }
            if ids.is_empty() {
                continue;
            }
            // One mailbox at a time: each delivery here was listed to it,
            // while the same message's delivery to another may not be.
            let one = ack_in(
                &tx,
                &ids,
                std::slice::from_ref(mailbox),
                actor,
                via,
                outcome,
                now_ms,
                grace,
            )?;
            result.acked.extend(one.acked);
            for id in one.purged {
                if !result.purged.contains(&id) {
                    result.purged.push(id);
                }
            }
        }
        tx.commit()?;
        Ok((result, skipped))
    }

    /// Sends `out`, a message that forwards `sources` (each an id and the
    /// body digest it was read with), then acks them as [`Store::ack`]
    /// does, all in one transaction: nothing is acked unless the forward
    /// is stored, and nothing is sent when a source can't be acked. Each
    /// source must be delivered to one of `mailboxes`, unacked there, and
    /// still hold the body it was read with.
    #[allow(clippy::too_many_arguments)]
    pub fn forward(
        &mut self,
        out: &Outgoing,
        sources: &[(String, String)],
        mailboxes: &[String],
        actor: &str,
        outcome: Option<&str>,
        now_ms: u64,
    ) -> Result<(Message, Acked), MailError> {
        if out.kind == "decision" {
            return Err(MailError::Store(StoreError::Schema(
                "a forward is never a decision".into(),
            )));
        }
        let (text, digest, size) = body_digest(&out.parts)?;
        let base_request = request_digest(out, &digest)?;
        let grace = self.ack_grace_ms;
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        for (id, read) in sources {
            let message =
                message_by_id(&tx, id)?.ok_or_else(|| MailError::UnknownMessage(id.clone()))?;
            let unacked = message
                .deliveries
                .iter()
                .any(|d| mailboxes.contains(&d.mailbox) && d.state != "acked");
            // Read before a purge, a restore or an ack someone else made:
            // what the forward holds is no longer what's stored.
            if !unacked || message.parts.is_none() || &message.digest != read {
                return Err(MailError::NotHolder(id.clone()));
            }
        }
        let (message, _, _) = send_in(
            &tx,
            out,
            None,
            &base_request,
            &text,
            &digest,
            size,
            &SendFlow::default(),
            &ExpiryPolicy::default(),
            grace,
        )?;
        let ids: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
        let acked = ack_in(&tx, &ids, mailboxes, actor, None, outcome, now_ms, grace)?;
        tx.commit()?;
        Ok((message, acked))
    }

    /// Records that the messages `listed` reached `mailboxes` (a listing
    /// their reader got whole), so those mailboxes may ack them. Callers
    /// mark only once the listing is out: a listing that never arrives
    /// marks nothing. A message that no longer matches what was listed (a
    /// restore in between) is left unshown.
    pub fn mark_shown(
        &mut self,
        listed: &[Shown],
        mailboxes: &[String],
        now_ms: u64,
    ) -> Result<(), MailError> {
        let tx = self.mail_tx()?;
        mark_shown(&tx, listed, mailboxes, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// The id of the message numbered `seq` (the `#N` listings show),
    /// whether a message has `id` as its id, and whether a conversation has
    /// it as its context: for reading a handle.
    pub fn handle_ids(
        &self,
        seq: Option<u64>,
        id: &str,
    ) -> Result<(Option<String>, bool, bool), MailError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let result = handle_ids_on(&tx, seq, id)?;
        tx.finish()?;
        Ok(result)
    }

    /// One message by id, with its deliveries.
    pub fn message(&mut self, id: &str, now_ms: u64) -> Result<Message, MailError> {
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        Self::purge_before_read(&tx, pure, now_ms)?;
        let mut message =
            message_by_id(&tx, id)?.ok_or_else(|| MailError::UnknownMessage(id.into()))?;
        Self::mask_after_read(&tx, pure, std::slice::from_mut(&mut message), now_ms)?;
        tx.commit()?;
        Ok(message)
    }

    /// The messages of a context after seq `after`, oldest first: the
    /// oldest `limit` of them, or with `newest` the newest `limit`.
    /// `id_or_context` names a message (its context is used) or a context.
    pub fn thread(
        &mut self,
        id_or_context: &str,
        page: Page,
        now_ms: u64,
    ) -> Result<(String, Vec<Message>), MailError> {
        self.thread_with_cursor(id_or_context, page, now_ms)
            .map(|(context, messages, _)| (context, messages))
    }

    /// A thread and its event-stream head from one transaction.
    pub fn thread_with_cursor(
        &mut self,
        id_or_context: &str,
        page: Page,
        now_ms: u64,
    ) -> Result<(String, Vec<Message>, String), MailError> {
        self.thread_handle_with_cursor(id_or_context, None, page, || now_ms)
    }

    /// Like thread_with_cursor, resolving a CLI number in the same snapshot.
    /// The bool distinguishes a forced number (#N) from an ambiguous bare N.
    pub fn thread_handle_with_cursor(
        &mut self,
        typed: &str,
        number: Option<(u64, bool)>,
        page: Page,
        clock: impl FnOnce() -> u64,
    ) -> Result<(String, Vec<Message>, String), MailError> {
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        let now_ms = clock();
        Self::purge_before_read(&tx, pure, now_ms)?;
        let resolved = match number {
            Some((seq, forced)) => {
                let (by_seq, is_id, is_context) = handle_ids_on(&tx, Some(seq), typed)?;
                if !forced && (is_id || is_context) {
                    typed.to_string()
                } else {
                    match by_seq {
                        Some(id) => id,
                        None if forced => return Err(MailError::UnknownMessage(typed.to_string())),
                        None => typed.to_string(),
                    }
                }
            }
            None => typed.to_string(),
        };
        let id_or_context = resolved.as_str();
        let context = match message_by_id(&tx, id_or_context)? {
            Some(m) => m.context_id,
            None => id_or_context.to_string(),
        };
        // Unknown only when nothing at all is in it, not when a page is empty.
        if !tx
            .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1")?
            .exists(params![context])?
        {
            return Err(MailError::UnknownContext(id_or_context.to_string()));
        }
        let mut messages = page_of(&tx, "context_id", &context, page)?;
        Self::mask_after_read(&tx, pure, &mut messages, now_ms)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((context, messages, cursor))
    }

    /// The messages posted to a room after seq `after`, oldest first: the
    /// oldest `limit` of them, or with `newest` the newest `limit`.
    pub fn room_messages(
        &mut self,
        room: &str,
        page: Page,
        now_ms: u64,
    ) -> Result<Vec<Message>, MailError> {
        self.room_messages_with_cursor(room, page, now_ms)
            .map(|(messages, _)| messages)
    }

    /// Channel messages and their stream head from one transaction.
    pub fn room_messages_with_cursor(
        &mut self,
        room: &str,
        page: Page,
        now_ms: u64,
    ) -> Result<(Vec<Message>, String), MailError> {
        self.channel_messages_with_cursor(room, page, || now_ms)
    }

    /// Channel messages with a clock sampled after acquiring the snapshot.
    pub fn channel_messages_with_cursor(
        &mut self,
        room: &str,
        page: Page,
        clock: impl FnOnce() -> u64,
    ) -> Result<(Vec<Message>, String), MailError> {
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        let now_ms = clock();
        Self::purge_before_read(&tx, pure, now_ms)?;
        if !room_exists(&tx, room)? {
            return Err(MailError::UnknownRoom(room.to_string()));
        }
        let mut messages = page_of(&tx, "room", room, page)?;
        Self::mask_after_read(&tx, pure, &mut messages, now_ms)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((messages, cursor))
    }

    /// `room_messages` for a reader who must be a member (any of
    /// `mailboxes` in the room): checked before anything is read, in the
    /// same transaction, and a room that doesn't exist is refused the same
    /// way, so the refusal doesn't reveal whether the room exists.
    pub fn member_room_messages(
        &mut self,
        room: &str,
        mailboxes: &[String],
        page: Page,
        now_ms: u64,
    ) -> Result<Vec<Message>, MailError> {
        self.member_room_messages_with_cursor(room, mailboxes, page, now_ms)
            .map(|(messages, _)| messages)
    }

    /// Channel messages and their stream head from one transaction.
    pub fn member_room_messages_with_cursor(
        &mut self,
        room: &str,
        mailboxes: &[String],
        page: Page,
        now_ms: u64,
    ) -> Result<(Vec<Message>, String), MailError> {
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        let member =
            room_exists(&tx, room)? && members_of(&tx, room)?.iter().any(|m| mailboxes.contains(m));
        if !member {
            return Err(MailError::NotMember {
                room: room.to_string(),
                actor: mailboxes.first().cloned().unwrap_or_default(),
            });
        }
        Self::purge_before_read(&tx, pure, now_ms)?;
        let mut messages = page_of(&tx, "room", room, page)?;
        Self::mask_after_read(&tx, pure, &mut messages, now_ms)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((messages, cursor))
    }

    /// A member read with role holdings and head in the same transaction.
    pub fn member_room_messages_for_actor_with_cursor(
        &mut self,
        room: &str,
        actor: &str,
        page: Page,
        clock: impl FnOnce() -> u64,
    ) -> Result<(Vec<Message>, String), MailError> {
        let ttl = self.ttl;
        let pure = self.pure_reads;
        let tx = self.read_tx()?;
        let now_ms = clock();
        let mailboxes = own_mailboxes_on(&tx, ttl, actor, now_ms)?;
        let member =
            room_exists(&tx, room)? && members_of(&tx, room)?.iter().any(|m| mailboxes.contains(m));
        if !member {
            return Err(MailError::NotMember {
                room: room.to_string(),
                actor: mailboxes.first().cloned().unwrap_or_default(),
            });
        }
        Self::purge_before_read(&tx, pure, now_ms)?;
        let mut messages = page_of(&tx, "room", room, page)?;
        Self::mask_after_read(&tx, pure, &mut messages, now_ms)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((messages, cursor))
    }

    /// Creates a room with `by` and `members` in it.
    pub fn room_create(
        &mut self,
        name: &str,
        by: &str,
        members: &[String],
        now_ms: u64,
    ) -> Result<Room, MailError> {
        let tx = self.mail_tx()?;
        if room_exists(&tx, name)? {
            return Err(MailError::RoomExists(name.to_string()));
        }
        tx.execute(
            "INSERT INTO msg_rooms (name, created_by, created_ms) VALUES (?1, ?2, ?3)",
            params![name, by, sql_ms(now_ms)],
        )?;
        let mut all: BTreeSet<&str> = members.iter().map(String::as_str).collect();
        all.insert(by);
        for m in &all {
            tx.execute(
                "INSERT OR IGNORE INTO msg_members (room, mailbox, added_by, added_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, m, by, sql_ms(now_ms)],
            )?;
        }
        crate::log::append_mail(
            &tx,
            by,
            "room.create",
            vec![format!("room:{name}")],
            serde_json::json!({"room": name, "by": by, "members": all}),
        )?;
        let room = room_on(&tx, name)?;
        tx.commit()?;
        Ok(room)
    }

    /// Adds `mailboxes` to a room; already a member is fine. `by` acts,
    /// reading `by_mailboxes` (itself and the roles it holds): any of them
    /// joins as itself, and any of them in the room makes `by` a member.
    pub fn room_join(
        &mut self,
        name: &str,
        mailboxes: &[String],
        by: &str,
        by_mailboxes: &[String],
        now_ms: u64,
    ) -> Result<Room, MailError> {
        let tx = self.mail_tx()?;
        if !room_exists(&tx, name)? {
            return Err(MailError::UnknownRoom(name.to_string()));
        }
        // Anyone joins as themselves; only a member adds others (D5).
        let members = members_of(&tx, name)?;
        let is_self = |m: &String| m == by || by_mailboxes.contains(m);
        if mailboxes.iter().any(|m| !is_self(m)) && !members.iter().any(is_self) {
            return Err(MailError::NotMember {
                room: name.to_string(),
                actor: by.to_string(),
            });
        }
        let mut added = Vec::new();
        for m in mailboxes {
            if tx.execute(
                "INSERT OR IGNORE INTO msg_members (room, mailbox, added_by, added_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, m, by, sql_ms(now_ms)],
            )? > 0
            {
                added.push(m.clone());
            }
        }
        if !added.is_empty() {
            crate::log::append_mail(
                &tx,
                by,
                "room.join",
                vec![format!("room:{name}")],
                serde_json::json!({"room": name, "by": by, "added": added}),
            )?;
        }
        let room = room_on(&tx, name)?;
        tx.commit()?;
        Ok(room)
    }

    /// Removes `mailboxes` from a room; deliveries already made stay.
    /// With `missing_ok`, mailboxes that aren't members are skipped and
    /// returned (leaving yourself again is done already); without it, any
    /// of them refuses the whole leave. Decided inside the transaction, so
    /// two overlapping leaves agree.
    pub fn room_leave(
        &mut self,
        name: &str,
        mailboxes: &[String],
        by: &str,
        by_mailboxes: &[String],
        missing_ok: bool,
    ) -> Result<(Room, Vec<String>), MailError> {
        let tx = self.mail_tx()?;
        if !room_exists(&tx, name)? {
            return Err(MailError::UnknownRoom(name.to_string()));
        }
        // Only current members leave: a mistyped name is refused rather
        // than reported as gone while the real member keeps its mail.
        let members = members_of(&tx, name)?;
        // Anyone leaves (as itself or a role it holds); only a member
        // removes others, and only the human removes a `human:*` member
        // (D5).
        let is_self = |m: &String| m == by || by_mailboxes.contains(m);
        let others: Vec<&String> = mailboxes.iter().filter(|m| !is_self(m)).collect();
        if !others.is_empty() && !members.iter().any(is_self) {
            return Err(MailError::NotMember {
                room: name.to_string(),
                actor: by.to_string(),
            });
        }
        let absent: Vec<String> = mailboxes
            .iter()
            .filter(|m| !members.contains(m))
            .cloned()
            .collect();
        if let (false, Some(m)) = (missing_ok, absent.first()) {
            return Err(MailError::Absent {
                room: name.to_string(),
                mailbox: m.clone(),
            });
        }
        if let Some(h) = others.iter().find(|m| is_human(m)) {
            return Err(MailError::HumanMember((*h).clone()));
        }
        let mut removed = Vec::new();
        for m in mailboxes {
            if tx.execute(
                "DELETE FROM msg_members WHERE room = ?1 AND mailbox = ?2",
                params![name, m],
            )? > 0
            {
                removed.push(m.clone());
            }
        }
        if !removed.is_empty() {
            crate::log::append_mail(
                &tx,
                by,
                "room.leave",
                vec![format!("room:{name}")],
                serde_json::json!({"room": name, "by": by, "removed": removed}),
            )?;
        }
        let room = room_on(&tx, name)?;
        tx.commit()?;
        Ok((room, absent))
    }

    /// Every room, by name.
    pub fn rooms(&self) -> Result<Vec<Room>, MailError> {
        crate::store::check_mail_schema(&self.conn)?;
        let names: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT name FROM msg_rooms ORDER BY name")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        names.iter().map(|n| room_on(&self.conn, n)).collect()
    }

    /// Channels, caller counts and stream head from one transaction. A
    /// message delivered to several of the caller's mailboxes counts once.
    pub fn channels_with_cursor(
        &mut self,
        mailboxes: &[String],
        now_ms: u64,
    ) -> Result<(Vec<(Room, u64)>, String), MailError> {
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let channels = channels_on(&tx, mailboxes, false)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((channels, cursor))
    }

    /// Role holdings, channel selection/counts and head share one transaction.
    pub fn channels_for_actor_with_cursor(
        &mut self,
        actor: &str,
        mine: bool,
        clock: impl FnOnce() -> u64,
    ) -> Result<(Vec<(Room, u64)>, String), MailError> {
        let ttl = self.ttl;
        let tx = self.mail_tx()?;
        let now_ms = clock();
        purge_due(&tx, now_ms)?;
        let mailboxes = own_mailboxes_on(&tx, ttl, actor, now_ms)?;
        let channels = channels_on(&tx, &mailboxes, mine)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.commit()?;
        Ok((channels, cursor))
    }

    /// Unacked deliveries per mailbox, for the mailboxes that have any.
    /// Receipts aren't counted: a plain inbox read hides them (A26), so
    /// every message counted is one the mailbox's reader can find.
    pub fn unacked(&self, mailboxes: &[String]) -> Result<Vec<(String, u64)>, MailError> {
        crate::store::check_mail_schema(&self.conn)?;
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM msg_deliveries d
             JOIN msg_messages m ON m.seq = d.seq
             WHERE d.mailbox = ?1 AND d.state = 'delivered' AND m.kind != 'receipt'",
        )?;
        for m in mailboxes {
            let n: i64 = stmt.query_row(params![m], |r| r.get(0))?;
            if n > 0 {
                out.push((m.clone(), ms(n)));
            }
        }
        Ok(out)
    }
}

fn handle_ids_on(
    conn: &Connection,
    seq: Option<u64>,
    id: &str,
) -> Result<(Option<String>, bool, bool), MailError> {
    let by_seq = match seq {
        Some(seq) => conn
            .query_row(
                "SELECT message_id FROM msg_messages WHERE seq = ?1",
                params![sql_ms(seq)],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    let is_id = conn
        .prepare("SELECT 1 FROM msg_messages WHERE message_id = ?1")?
        .exists(params![id])?;
    let is_context = conn
        .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1")?
        .exists(params![id])?;
    Ok((by_seq, is_id, is_context))
}

fn own_mailboxes_on(
    conn: &Connection,
    ttl: u64,
    actor: &str,
    now: u64,
) -> Result<Vec<String>, MailError> {
    let (_, state) = crate::store::load(conn, ttl)?;
    let mut mailboxes = vec![actor.to_string()];
    mailboxes.extend(
        state
            .leases()
            .filter(|lease| {
                lease.owner == actor
                    && lease.confirmed
                    && lease.status == mdkg_coord::Status::Active
                    && lease.deadline > now
                    && lease.scope.as_str().starts_with("role:")
            })
            .map(|lease| lease.scope.as_str().to_string()),
    );
    Ok(mailboxes)
}

fn read_mailboxes_on(
    conn: &Connection,
    ttl: u64,
    actor: &str,
    selected: Option<&str>,
    now: u64,
) -> Result<Vec<String>, MailError> {
    let own = own_mailboxes_on(conn, ttl, actor, now)?;
    match selected {
        None => Ok(own),
        Some(mailbox) if is_human(mailbox) || own.iter().any(|m| m == mailbox) => {
            Ok(vec![mailbox.to_string()])
        }
        Some(mailbox) => Err(MailError::MailboxNotHeld {
            role: mailbox.starts_with("role:"),
        }),
    }
}

fn channels_on(
    conn: &Connection,
    mailboxes: &[String],
    mine: bool,
) -> Result<Vec<(Room, u64)>, MailError> {
    let names: Vec<String> = conn
        .prepare("SELECT name FROM msg_rooms ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut channels = Vec::new();
    for name in names {
        let room = room_on(conn, &name)?;
        if mine && !room.members.iter().any(|m| mailboxes.contains(m)) {
            continue;
        }
        let mut unread = BTreeSet::new();
        let mut stmt = conn.prepare("SELECT d.seq FROM msg_deliveries d JOIN msg_messages m ON m.seq=d.seq WHERE m.room=?1 AND d.mailbox=?2 AND d.state='delivered'")?;
        for mailbox in mailboxes {
            for seq in stmt.query_map(params![name, mailbox], |r| r.get::<_, i64>(0))? {
                unread.insert(seq?);
            }
        }
        channels.push((room, unread.len() as u64));
    }
    Ok(channels)
}

pub(crate) fn room_on(conn: &Connection, name: &str) -> Result<Room, MailError> {
    let (created_by, created_ms, last_seq) = conn
        .query_row(
            "SELECT created_by, created_ms,
                (SELECT COALESCE(MAX(seq), 0) FROM msg_messages WHERE room = ?1)
             FROM msg_rooms WHERE name = ?1",
            params![name],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| MailError::UnknownRoom(name.to_string()))?;
    Ok(Room {
        name: name.to_string(),
        created_by,
        created_ms: ms(created_ms),
        members: members_of(conn, name)?,
        last_seq: ms(last_seq),
    })
}

#[cfg(test)]
#[path = "../tests/common/temp.rs"]
mod snapshot_temp;

#[cfg(test)]
mod snapshot_clock_tests {
    use super::*;
    use mdkg_coord::{Command, Mode, Scope};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use std::time::Duration;

    thread_local! {
        static LOCK_ATTEMPT: std::cell::RefCell<Option<mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
    }
    fn retry_snapshot_lock(_attempt: i32) -> bool {
        LOCK_ATTEMPT.with(|slot| {
            if let Some(sender) = slot.borrow_mut().take() {
                sender.send(()).unwrap();
            }
        });
        std::thread::sleep(Duration::from_millis(1));
        true
    }

    #[test]
    fn read_clocks_wait_for_the_snapshot_lock_across_role_and_body_deadlines() {
        for view in [
            "inbox", "selected", "channels", "member", "context", "number", "channel",
        ] {
            let dir = super::snapshot_temp::dir("mdkg-role-clock");
            let path = dir.join("coord.sqlite");
            let mut writer = Store::open_with(&path, 1000, crate::Upgrade::Allow).unwrap();
            writer
                .room_create("review", "sender", &["role:reviewer".into()], 0)
                .unwrap();
            writer
                .execute(&Command::Claim {
                    owner: "a".into(),
                    scope: Scope::new("role:reviewer"),
                    mode: Mode::Exclusive,
                    now: 0,
                    request: None,
                    node: None,
                    branch: None,
                })
                .unwrap();
            let ttl_view = matches!(view, "context" | "number" | "channel");
            let expiring = Outgoing {
                message_id: "expiring".into(),
                context_id: None,
                kind: "note".into(),
                sender: "sender".into(),
                via: None,
                sender_mailboxes: vec!["sender".into()],
                to: To::Room("review".into()),
                reference_ids: Vec::new(),
                parts: serde_json::json!([{"kind":"text","text":"expires while waiting"}]),
                retention: Retention::Ttl(1),
                human_hold_days: 7,
                now_ms: 0,
            };
            writer.send(&expiring).unwrap();
            let mut reader = Store::open_with(&path, 1000, crate::Upgrade::Refuse).unwrap();
            let tx = writer.mail_tx().unwrap();
            let now = Arc::new(AtomicU64::new(if ttl_view { 86_399_999 } else { 500 }));
            let read_now = now.clone();
            let (lock_tx, lock_rx) = mpsc::channel();
            let (clock_tx, clock_rx) = mpsc::channel();
            let read = std::thread::spawn(move || {
                let clock = || {
                    clock_tx.send(()).unwrap();
                    read_now.load(Ordering::SeqCst)
                };
                LOCK_ATTEMPT.with(|slot| *slot.borrow_mut() = Some(lock_tx));
                reader.conn.busy_handler(Some(retry_snapshot_lock)).unwrap();
                match view {
                    "inbox" => {
                        let (inbox, mailboxes) = reader
                            .inbox_from_for_actor(
                                &InboxQuery {
                                    now_ms: 500,
                                    ..InboxQuery::default()
                                },
                                &[],
                                "a",
                                None,
                                clock,
                            )
                            .unwrap();
                        assert_eq!(mailboxes, vec!["a"]);
                        assert!(inbox.messages.is_empty());
                    }
                    "selected" => assert!(matches!(
                        reader.inbox_from_for_actor(
                            &InboxQuery {
                                now_ms: 500,
                                ..InboxQuery::default()
                            },
                            &[],
                            "a",
                            Some("role:reviewer"),
                            clock
                        ),
                        Err(MailError::MailboxNotHeld { role: true })
                    )),
                    "channels" => {
                        let (channels, _) = reader
                            .channels_for_actor_with_cursor("a", true, clock)
                            .unwrap();
                        assert!(channels.is_empty());
                    }
                    "member" => assert!(matches!(
                        reader.member_room_messages_for_actor_with_cursor(
                            "review",
                            "a",
                            Page::default(),
                            clock
                        ),
                        Err(MailError::NotMember { .. })
                    )),
                    "context" | "number" | "channel" => {
                        let messages = if view == "channel" {
                            reader
                                .channel_messages_with_cursor("review", Page::default(), clock)
                                .unwrap()
                                .0
                        } else {
                            let number = (view == "number").then_some((1, true));
                            reader
                                .thread_handle_with_cursor(
                                    if view == "number" { "#1" } else { "expiring" },
                                    number,
                                    Page::default(),
                                    clock,
                                )
                                .unwrap()
                                .1
                        };
                        let message = messages
                            .iter()
                            .find(|m| m.message_id == "expiring")
                            .expect("expiring message");
                        assert!(
                            message.parts.is_none(),
                            "expired body must be purged at snapshot time"
                        );
                    }
                    _ => unreachable!(),
                }
            });
            lock_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("reader attempted the held lock");
            let early_clock = clock_rx.try_recv();
            // The lease expires while the reader is blocked. This message only
            // becomes visible after expiry, in the writer's held transaction.
            let after = if ttl_view { 86_400_001 } else { 2000 };
            now.store(after, Ordering::SeqCst);
            let outgoing = Outgoing {
                message_id: "after-expiry".into(),
                context_id: None,
                kind: "note".into(),
                sender: "sender".into(),
                via: None,
                sender_mailboxes: vec!["sender".into()],
                to: To::Room("review".into()),
                reference_ids: Vec::new(),
                parts: serde_json::json!([{"kind":"text","text":"posted after the role expired"}]),
                retention: Retention::Durable,
                human_hold_days: 7,
                now_ms: after,
            };
            let (text, digest, size) = body_digest(&outgoing.parts).unwrap();
            let request = request_digest(&outgoing, &digest).unwrap();
            send_in(
                &tx,
                &outgoing,
                None,
                &request,
                &text,
                &digest,
                size,
                &SendFlow::default(),
                &ExpiryPolicy::default(),
                0,
            )
            .unwrap();
            tx.commit().unwrap();
            let read_result = read.join();
            drop(writer);
            std::fs::remove_dir_all(dir).unwrap();
            assert_eq!(
                early_clock,
                Err(mpsc::TryRecvError::Empty),
                "the clock must not run while another writer holds the snapshot lock"
            );
            read_result.unwrap();
        }
    }
}
