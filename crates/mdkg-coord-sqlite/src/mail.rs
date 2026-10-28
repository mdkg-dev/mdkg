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
//!   over.
//!   The metadata (ids, kind, sender, recipients, digest, size, times,
//!   acks) always stays.
//!
//! Mail never touches the lease journal or state, so lease replay is
//! unaffected; purges run inside mail operations, with no daemon.

use std::collections::BTreeSet;

use mdkg_format::Digest;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;

use crate::store::{Store, StoreError};

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

/// The message kinds v0.1 knows. The kernel attaches no state machine to
/// them.
pub const KINDS: &[&str] = &[
    "review_request",
    "findings",
    "resolution",
    "question",
    "decision",
    "handoff",
    "note",
];

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
}

/// What `inbox` and `wait` look for.
#[derive(Debug, Clone, Default)]
pub struct InboxQuery {
    /// The mailboxes read; deliveries to others are left out.
    pub mailboxes: Vec<String>,
    pub room: Option<String>,
    /// Only these kinds (any of them); empty, every kind.
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
    /// The message hasn't been listed to the acking mailbox yet (an inbox,
    /// thread or wait listing): an ack never purges one nobody has seen.
    NotShown(String),
    /// The sender names a conversation, or replies to a message, it isn't
    /// part of. Uniform, and names neither.
    NotInConversation,
    NoRecipients,
    Store(StoreError),
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IdReused(id) => write!(f, "message id {id} is taken by a different message"),
            Self::UnknownRoom(r) => write!(f, "no room {r}"),
            Self::UnknownMessage(id) => write!(f, "no message {id}"),
            Self::UnknownContext(c) => write!(f, "no message or context {c}"),
            Self::RoomExists(r) => write!(f, "room {r} already exists"),
            Self::NotMember { room, actor } => write!(f, "{actor} isn't a member of room {room}"),
            Self::Absent { room, mailbox } => write!(f, "{mailbox} isn't a member of room {room}"),
            Self::HumanMember(m) => write!(f, "only {m} can remove {m} from a room"),
            Self::NotHolder(id) => write!(f, "none of your mailboxes received {id}"),
            Self::NotShown(id) => write!(f, "{id} hasn't been shown to you yet"),
            Self::NoRecipients => write!(f, "the message has no recipients"),
            Self::NotInConversation => write!(f, "that conversation isn't one you're part of"),
            Self::Store(e) => write!(f, "{e}"),
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

/// Deletes bodies whose time is up.
fn purge_due(tx: &Transaction<'_>, now_ms: u64) -> Result<(), MailError> {
    // A `ttl` body at its time; a `purge_on_ack` body held for a human
    // once its hold is over and every other delivery is acked.
    let due: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT m.seq, m.message_id FROM msg_messages m JOIN msg_bodies b ON b.seq = m.seq
             WHERE m.purge_after_ms IS NOT NULL AND m.purge_after_ms <= ?1
               AND (m.retention != 'purge_on_ack' OR NOT EXISTS (
                   SELECT 1 FROM msg_deliveries d
                   WHERE d.seq = m.seq AND d.state != 'acked'
                     AND substr(d.mailbox, 1, 6) != 'human:'))
             ORDER BY m.seq",
        )?;
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
        None => context.starts_with("room:") || context_has_room_post(conn, context)?,
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

/// Audit reads never open a writer or perform store migration/maintenance.
/// Missing stores have no decisions. Incompatible existing schemas refuse.
pub fn read_decisions(
    path: &std::path::Path,
    after: u64,
    limit: usize,
    ids: &[String],
) -> Result<Vec<Value>, MailError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::from(e).into()),
        Ok(meta) if !meta.is_file() => {
            return Err(StoreError::Schema("decision audit needs a regular store".into()).into());
        }
        Ok(_) => (),
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
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

fn message_by_id(conn: &Connection, id: &str) -> Result<Option<Message>, MailError> {
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
/// the body of a `purge_on_ack` message goes.
fn purge_if_done(tx: &Transaction<'_>, seq: u64, now_ms: u64) -> Result<bool, MailError> {
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
    if agents > 0 || (humans > 0 && !hold_over) || retention != Retention::PurgeOnAck.to_string() {
        return Ok(false);
    }
    Ok(tx.execute(
        "DELETE FROM msg_bodies WHERE seq = ?1",
        params![sql_ms(seq)],
    )? > 0)
}

impl Store {
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
        let with_grant = |grant: &DecisionGrant| {
            Digest::of_record(&serde_json::json!({"request": base_request, "grant": grant.value()}))
                .map(|digest| digest.to_string())
                .map_err(|e| MailError::Store(StoreError::Schema(e.to_string())))
        };
        let mut request = base_request.clone();
        if let Some(grant) = grant {
            // Send time is stamped once; an otherwise identical retry keeps
            // that original time. Changed scope/provenance is an ID collision.
            request = with_grant(grant)?;
        }
        let tx = self.mail_tx()?;
        purge_due(&tx, out.now_ms)?;
        if let Some(existing) = message_by_id(&tx, &out.message_id)? {
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
                // What purge_due cleared stays cleared: the transaction is
                // committed, not dropped (which would roll the purge back).
                tx.commit()?;
                return Ok((existing, true));
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
            Some(first) => message_by_id(&tx, first)?,
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
                    && (c.starts_with("room:") || context_has_room_post(&tx, &c)?) =>
            {
                None
            }
            other => other,
        };
        let (room, recipients, default_context) = match &out.to {
            To::Room(room) => {
                if !room_exists(&tx, room)? {
                    return Err(MailError::UnknownRoom(room.clone()));
                }
                let members = members_of(&tx, room)?;
                let writer = out.via.as_deref().unwrap_or(&out.sender);
                let member = |m: &String| {
                    m == &out.sender || m == writer || out.sender_mailboxes.contains(m)
                };
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
            if let Some(m) = message_by_id(&tx, id)?
                && !took_part(&tx, &m, &skip)?
            {
                return Err(MailError::NotInConversation);
            }
        }
        if let Some(c) = &out.context_id
            && !may_join(&tx, c, here, &skip)?
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
            && message_by_id(&tx, c)?.is_some_and(|m| m.context_id != *c)
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
            fields.insert("bodyDigest".into(), digest.clone().into());
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
        let message = message_by_id(&tx, &out.message_id)?
            .ok_or_else(|| MailError::UnknownMessage(out.message_id.clone()))?;
        // The record carries the body's digest, never the body (W1).
        let mut subject = vec![format!("msg:{}", out.message_id)];
        if let Some(r) = &room {
            subject.push(format!("room:{r}"));
        }
        crate::log::append_mail(
            &tx,
            out.via.as_deref().unwrap_or(&out.sender),
            "msg.send",
            subject,
            serde_json::json!({
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
            }),
        )?;
        tx.commit()?;
        Ok((message, false))
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
        let tx = self.mail_tx()?;
        purge_due(&tx, q.now_ms)?;
        let after = q.after.unwrap_or(0);
        let mut seqs: Vec<u64> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT d.seq, m.kind, m.room IS NOT NULL FROM msg_deliveries d
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
                    ))
                })?;
                for row in rows {
                    let (seq, kind, in_room) = row?;
                    let wanted = |kinds: &[String]| kinds.is_empty() || kinds.contains(&kind);
                    if wanted(&q.kinds) && (!in_room || wanted(&q.room_kinds)) {
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
        tx.commit()?;
        let cursor = seqs.last().copied().unwrap_or(after);
        Ok(Inbox {
            messages,
            cursor,
            more,
        })
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
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let mut result = Acked::default();
        let mut newly: Vec<Value> = Vec::new();
        for id in ids {
            let message =
                message_by_id(&tx, id)?.ok_or_else(|| MailError::UnknownMessage(id.clone()))?;
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
                    newly.push(serde_json::json!({"message_id": id, "mailbox": d.mailbox}));
                }
                result.acked.push((id.clone(), d.mailbox.clone()));
            }
            if purge_if_done(&tx, message.seq, now_ms)? {
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
                &tx,
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
        tx.commit()?;
        Ok(result)
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
        let by_seq = match seq {
            Some(seq) => tx
                .query_row(
                    "SELECT message_id FROM msg_messages WHERE seq = ?1",
                    params![sql_ms(seq)],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        let is_id = tx
            .prepare("SELECT 1 FROM msg_messages WHERE message_id = ?1")?
            .exists(params![id])?;
        let is_context = tx
            .prepare("SELECT 1 FROM msg_messages WHERE context_id = ?1")?
            .exists(params![id])?;
        tx.finish()?;
        Ok((by_seq, is_id, is_context))
    }

    /// One message by id, with its deliveries.
    pub fn message(&mut self, id: &str, now_ms: u64) -> Result<Message, MailError> {
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        let message =
            message_by_id(&tx, id)?.ok_or_else(|| MailError::UnknownMessage(id.into()))?;
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
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
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
        let messages = page_of(&tx, "context_id", &context, page)?;
        tx.commit()?;
        Ok((context, messages))
    }

    /// The messages posted to a room after seq `after`, oldest first: the
    /// oldest `limit` of them, or with `newest` the newest `limit`.
    pub fn room_messages(
        &mut self,
        room: &str,
        page: Page,
        now_ms: u64,
    ) -> Result<Vec<Message>, MailError> {
        let tx = self.mail_tx()?;
        purge_due(&tx, now_ms)?;
        if !room_exists(&tx, room)? {
            return Err(MailError::UnknownRoom(room.to_string()));
        }
        let messages = page_of(&tx, "room", room, page)?;
        tx.commit()?;
        Ok(messages)
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
        let tx = self.mail_tx()?;
        let member =
            room_exists(&tx, room)? && members_of(&tx, room)?.iter().any(|m| mailboxes.contains(m));
        if !member {
            return Err(MailError::NotMember {
                room: room.to_string(),
                actor: mailboxes.first().cloned().unwrap_or_default(),
            });
        }
        purge_due(&tx, now_ms)?;
        let messages = page_of(&tx, "room", room, page)?;
        tx.commit()?;
        Ok(messages)
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

    /// Unacked deliveries per mailbox, for the mailboxes that have any.
    pub fn unacked(&self, mailboxes: &[String]) -> Result<Vec<(String, u64)>, MailError> {
        crate::store::check_mail_schema(&self.conn)?;
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM msg_deliveries WHERE mailbox = ?1 AND state = 'delivered'",
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

fn room_on(conn: &Connection, name: &str) -> Result<Room, MailError> {
    let (created_by, created_ms) = conn
        .query_row(
            "SELECT created_by, created_ms FROM msg_rooms WHERE name = ?1",
            params![name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| MailError::UnknownRoom(name.to_string()))?;
    Ok(Room {
        name: name.to_string(),
        created_by,
        created_ms: ms(created_ms),
        members: members_of(conn, name)?,
    })
}
