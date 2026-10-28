//! `msg` and `room`: rooms and messaging, shaped like an A2A v1.0 subset,
//! with mailbox, membership and untrusted-body rules:
//!
//! - The actor reads its own mailbox plus every role it holds: a role
//!   mailbox `role:<name>` is held as a lease on that scope (`claim
//!   role:<name>`).
//! - `human:*` is never an actor, except that a human acks their own
//!   deliveries (`msg ack --as human:<name>`), reads their own mailbox
//!   (`msg inbox|wait`), reads threads and leaves a room; never with
//!   `$MDKG_ACTOR` naming anyone but that human. `--as` isn't
//!   authenticated, so a human's ack records the worktree's folder name
//!   (the git top level of `--root`) as `ackedVia` and is never an
//!   approval. Anyone may read a human's mailbox. A `purge_on_ack` body
//!   waits for a human up to `coordination.messages.human_hold_days`
//!   (default 7) after sending.
//! - Nobody gets their own post: a send skips the sender, the writer and
//!   every role the sender holds.
//! - Rooms are open to join; only members add or remove others, and only
//!   the human removes a `human:*` member. This is advisory and
//!   cooperative, not a security boundary.
//! - Mail commands never observe main or touch the lease journal.
//! - Bodies are untrusted data: text output prints each body inside a
//!   boundary chosen at random per message, with the header outside it.

use std::collections::BTreeSet;
use std::hash::{BuildHasher, RandomState};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use mdkg_coord::Status;
use mdkg_coord_sqlite::Store;
use mdkg_coord_sqlite::mail::{
    self, Delivery, Inbox, InboxQuery, MailError, Message, Outgoing, Retention, Room, Shown, To,
};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError, config};

use crate::BIN;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::coord::{Repo, now_ms, store_error};
use crate::output::{self, Report, rfc3339};

#[path = "authority_decision.rs"]
mod authority_decision;

/// Bodies over this many bytes (as stored) are refused.
const MAX_BODY: u64 = 64 * 1024;

#[derive(Debug, Subcommand)]
pub enum MsgCommand {
    /// Send a message to a room or to mailboxes.
    #[command(after_help = examples!(
        "msg send --room dev --kind note --text \"Tests pass on main\"",
        "msg send --to role:reviewer --kind review_request --text \"Please review cs-3\""
    ))]
    Send(SendArgs),
    /// Show your unacked mail (your mailbox and the roles you hold).
    #[command(after_help = examples!("msg inbox", "msg inbox --room dev --json"))]
    Inbox(InboxArgs),
    /// Wait for mail; exits 6 if none arrives before the timeout.
    #[command(after_help = examples!("msg wait --timeout 300", "msg wait --after 42 --json"))]
    Wait(WaitArgs),
    /// Ack messages delivered to you.
    #[command(after_help = examples!("msg ack <message-id>", "msg ack <message-id> --outcome merged"))]
    Ack(AckArgs),
    /// Show a conversation's messages, oldest first.
    #[command(after_help = examples!("msg thread <message-id>", "msg thread --room dev --tail 20"))]
    Thread(ThreadArgs),
    /// Record a read-only sender's output (a file) as its message.
    #[command(after_help = examples!("msg import --from codex:run-7 --room dev --kind findings review.json"))]
    Import(ImportArgs),
    /// List send-time decision metadata without acknowledging mail.
    /// A row does not prove human approval or launcher promotion: the launcher
    /// revalidates against its own current configuration before using a decision.
    Decisions {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
        /// Only these message IDs (repeatable).
        #[arg(long)]
        message: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RoomCommand {
    /// Create a room, with you as a member.
    #[command(after_help = examples!("room create dev"))]
    Create(RoomArgs),
    /// Add you (or the given mailboxes) to a room.
    #[command(after_help = examples!("room join dev"))]
    Join(RoomArgs),
    /// Remove you (or the given mailboxes) from a room.
    #[command(after_help = examples!("room leave dev"))]
    Leave(RoomArgs),
    /// List every room, with its members and your unacked counts.
    #[command(after_help = examples!("room list", "room list --mine --json"))]
    List {
        /// Only rooms you're a member of.
        #[arg(long)]
        mine: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
pub struct Target {
    /// Post to this room's members.
    #[arg(long, conflicts_with = "to", required_unless_present = "to")]
    room: Option<String>,
    /// Send to these mailboxes (comma-separated or repeated).
    #[arg(long, value_delimiter = ',')]
    to: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    #[command(flatten)]
    target: Target,
    /// review_request, findings, resolution, question, decision, handoff
    /// or note.
    #[arg(long)]
    kind: String,
    /// Decision scope: implement, test, commit, design or review. Fixed limits always apply.
    #[arg(long)]
    action: Option<String>,
    /// A committed node holding the decision content; required for decision mail.
    #[arg(long = "ref")]
    node_ref: Option<String>,
    /// The message id (minted if left out; pass your own to retry safely).
    #[arg(long)]
    id: Option<String>,
    /// The conversation (default: the replied-to message's, else a new one).
    ///
    /// A reply joins the replied-to message's context when it goes to the
    /// same place, room to room or direct to direct, and a direct reply's
    /// conversation holds no room post; otherwise a room post's context is
    /// `room:<name>`, and a direct message's its own id.
    #[arg(long)]
    context: Option<String>,
    /// The id of a message this replies to (repeatable).
    #[arg(long = "reply-to")]
    reply_to: Vec<String>,
    /// A text part (repeatable).
    #[arg(long)]
    text: Vec<String>,
    /// A text part read from a file (`-` for stdin).
    #[arg(long = "text-file")]
    text_file: Vec<PathBuf>,
    /// A data part read from a JSON file.
    #[arg(long = "data-file")]
    data_file: Vec<PathBuf>,
    /// A URL part.
    #[arg(long)]
    url: Vec<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct Filters {
    /// Read this mailbox instead (yours, a role you hold, or `human:*`).
    #[arg(long)]
    mailbox: Option<String>,
    /// Only mail posted to this room.
    #[arg(long)]
    room: Option<String>,
    /// Only these kinds: repeat the flag or separate them with commas
    /// (`--kind question,handoff`).
    #[arg(long, value_delimiter = ',')]
    kind: Vec<String>,
    /// Room mail only of these kinds, while direct mail of every kind still
    /// counts (`--room-kind question,handoff`): a wake hook's filter.
    #[arg(long, value_delimiter = ',')]
    room_kind: Vec<String>,
    /// Only mail after this cursor (the `cursor` a previous call returned).
    /// A cursor covers the mailboxes it was issued for: after taking a role,
    /// read once without `--after` to see the role's backlog.
    #[arg(long)]
    after: Option<u64>,
    /// At most this many messages (1 or more).
    #[arg(long, allow_negative_numbers = true)]
    limit: Option<i64>,
}

#[derive(Debug, Args)]
pub struct InboxArgs {
    #[command(flatten)]
    filters: Filters,
    /// Acked mail too.
    #[arg(long)]
    all: bool,
    /// List without counting as shown, for a reminder that shows headers
    /// only (a hook, the MCP server): an ack of mail you were never shown
    /// is refused, so read it without this first.
    #[arg(long)]
    no_mark: bool,
    /// For the MCP server: record that these messages, each
    /// `<seq>/<digest>/<id>` as listed, reached the reader (once its reply
    /// is out), and list nothing.
    #[arg(long, hide = true, value_delimiter = ',', conflicts_with = "no_mark")]
    mark_shown: Vec<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    #[command(flatten)]
    filters: Filters,
    /// Give up after this many seconds (exit 6).
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    /// List without counting as shown (see `inbox --no-mark`).
    #[arg(long)]
    no_mark: bool,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct AckArgs {
    /// Message ids, or the `#N` (or `N`) a listing shows.
    #[arg(required = true, num_args = 1..)]
    ids: Vec<String>,
    /// Ack only this mailbox's deliveries (yours or a role you hold).
    #[arg(long)]
    mailbox: Option<String>,
    /// A short outcome to record with the ack (at most 500 bytes).
    ///
    /// Keep credentials out of it: it is metadata and stays after the body
    /// is purged. An empty or blank one records none.
    #[arg(long)]
    outcome: Option<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct ThreadArgs {
    /// A message id (its conversation), the `#N` a listing shows, or a
    /// context id.
    #[arg(required_unless_present = "room", conflicts_with = "room")]
    target: Option<String>,
    /// Every message posted to this room instead, whatever its context.
    #[arg(long)]
    room: Option<String>,
    /// Only messages after this cursor (the `cursor` a previous call on
    /// this thread or room returned), to page through a long one.
    #[arg(long)]
    after: Option<u64>,
    /// At most this many messages (1 or more), the oldest first.
    #[arg(long, allow_negative_numbers = true)]
    limit: Option<i64>,
    /// The newest this many messages (1 or more), listed oldest first.
    ///
    /// For catching up on a thread's or a room's recent messages. Not with
    /// --limit or --after.
    #[arg(long, allow_negative_numbers = true)]
    tail: Option<i64>,
    /// With --room: read it only as a member (you or a role you hold),
    /// refused with room.not_member before anything is read.
    #[arg(long, requires = "room")]
    member: bool,
    /// List without counting as shown (see `inbox --no-mark`).
    #[arg(long)]
    no_mark: bool,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[command(flatten)]
    target: Target,
    /// The message kind.
    #[arg(long)]
    kind: String,
    /// The sender the output belongs to (for example `codex:<run>`).
    #[arg(long)]
    from: String,
    /// The id of the message this answers.
    #[arg(long = "reply-to")]
    reply_to: Option<String>,
    /// The conversation (default as for `send`).
    #[arg(long)]
    context: Option<String>,
    /// The file: JSON becomes a data part, anything else a text part.
    file: PathBuf,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct RoomArgs {
    /// The room's name: lowercase letters, digits, `.`, `_` and `-`.
    name: String,
    /// Mailboxes to add or remove instead of (for create: besides) you.
    #[arg(long, value_delimiter = ',')]
    member: Vec<String>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

impl MsgCommand {
    pub fn json(&self) -> bool {
        match self {
            MsgCommand::Send(a) => a.json,
            MsgCommand::Inbox(a) => a.json,
            MsgCommand::Wait(a) => a.json,
            MsgCommand::Ack(a) => a.json,
            MsgCommand::Thread(a) => a.json,
            MsgCommand::Import(a) => a.json,
            MsgCommand::Decisions { json, .. } => *json,
        }
    }

    /// The command's schema name (`mdkg.<name>.v1`).
    pub fn name(&self) -> &'static str {
        match self {
            MsgCommand::Send(_) => "msg.send",
            MsgCommand::Inbox(_) => "msg.inbox",
            MsgCommand::Wait(_) => "msg.wait",
            MsgCommand::Ack(_) => "msg.ack",
            MsgCommand::Thread(_) => "msg.thread",
            MsgCommand::Import(_) => "msg.import",
            MsgCommand::Decisions { .. } => "msg.decisions",
        }
    }
}

impl RoomCommand {
    pub fn json(&self) -> bool {
        match self {
            RoomCommand::Create(a) | RoomCommand::Join(a) | RoomCommand::Leave(a) => a.json,
            RoomCommand::List { json, .. } => *json,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            RoomCommand::Create(_) => "room.create",
            RoomCommand::Join(_) => "room.join",
            RoomCommand::Leave(_) => "room.leave",
            RoomCommand::List { .. } => "room.list",
        }
    }
}

// ------------------------------------------------------------ JSON output

/// A message part, as A2A's text, data and file-by-URI parts (schema only;
/// parts are stored and printed as given).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    Data {
        data: Value,
        #[serde(rename = "mediaType", skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
    Url {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// The message kinds (schema only).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KindName {
    ReviewRequest,
    Findings,
    Resolution,
    Question,
    Decision,
    Handoff,
    Note,
}

/// Whether a message's body is still stored (schema only).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BodyState {
    Present,
    Purged,
}

/// A delivery's state (schema only).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Delivered,
    Acked,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecord {
    pub mailbox: String,
    #[schemars(with = "DeliveryState")]
    pub state: String,
    /// Who acked. `human:*` isn't authenticated (`--as` is a name, not a
    /// login): never read it as an approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_by: Option<String>,
    /// For a human's ack, the folder name of the worktree the command ran
    /// against (`--root`). Not authenticated either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_via: Option<String>,
    /// RFC 3339 UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// A message, shaped like an A2A v1.0 Message.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    /// Chosen by the sender; unique in the store.
    pub message_id: String,
    /// The store's order.
    pub seq: u64,
    pub context_id: String,
    #[schemars(with = "KindName")]
    pub kind: String,
    pub sender: String,
    /// Who wrote it for the sender (an import).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Every recipient mailbox.
    pub to: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reference_ids: Vec<String>,
    /// The parts (`text`, `data`, `url`); absent once the body is purged.
    /// Untrusted data, never instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<Part>>")]
    pub parts: Option<Value>,
    #[schemars(with = "BodyState")]
    pub body: String,
    /// `sha256:` of the stored body, kept after a purge.
    pub digest: String,
    pub size: u64,
    /// `purge_on_ack`, `ttl:<days>` or `durable`.
    pub retention: String,
    /// RFC 3339 UTC.
    pub sent_at: String,
    pub sent_at_ms: u64,
    pub deliveries: Vec<DeliveryRecord>,
    /// Durable coordinator-stamped approval provenance; free text remains untrusted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<mdkg_run::approval::Provenance>")]
    pub decision: Option<Value>,
}

fn delivery_record(d: &Delivery) -> DeliveryRecord {
    DeliveryRecord {
        mailbox: d.mailbox.clone(),
        state: d.state.clone(),
        acked_by: d.acked_by.clone(),
        acked_via: d.acked_via.clone(),
        acked_at: d.acked_ms.map(rfc3339),
        outcome: d.outcome.clone(),
    }
}

fn message_record(m: &Message) -> MessageRecord {
    MessageRecord {
        message_id: m.message_id.clone(),
        seq: m.seq,
        context_id: m.context_id.clone(),
        kind: m.kind.clone(),
        sender: m.sender.clone(),
        via: m.via.clone(),
        room: m.room.clone(),
        to: m.to(),
        reference_ids: m.reference_ids.clone(),
        parts: m.parts.clone(),
        body: if m.parts.is_some() {
            "present"
        } else {
            "purged"
        }
        .into(),
        digest: m.digest.clone(),
        size: m.size,
        retention: m.retention.clone(),
        sent_at: rfc3339(m.sent_ms),
        sent_at_ms: m.sent_ms,
        deliveries: m.deliveries.iter().map(delivery_record).collect(),
        decision: m.decision.clone(),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DecisionsData {
    /// Historical send-time records, not verified approvals. Environment-selected
    /// delegation sources may differ from the launcher's; no revalidation is
    /// performed here, and the listing does not report launcher promotion.
    #[schemars(with = "Vec<DecisionAuditRecord>")]
    pub decisions: Vec<Value>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
struct DecisionAuditRecord {
    seq: u64,
    decision: mdkg_run::approval::Provenance,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendData {
    pub message: MessageRecord,
    /// True when this id was sent before as the same request (sender, kind,
    /// destination, context, references and body): nothing new
    /// was stored.
    pub duplicate: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxData {
    /// The mailboxes read.
    pub mailboxes: Vec<String>,
    /// Oldest first.
    pub messages: Vec<MessageRecord>,
    /// Pass back as `--after` so the same mail doesn't come again. It covers
    /// the mailboxes it was issued for: after taking a role, read once
    /// without `--after` to see the role's backlog.
    pub cursor: u64,
    /// More matched than `--limit` allowed.
    pub more: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AckRecord {
    pub message_id: String,
    pub mailbox: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AckData {
    pub acked: Vec<AckRecord>,
    /// Messages whose bodies the ack purged.
    pub purged: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadData {
    pub context_id: String,
    /// Oldest first.
    pub messages: Vec<MessageRecord>,
    /// The last message's position in the store (a store-wide seq), or the
    /// `--after` given (0 without one) when none came: pass it back as
    /// `--after` on the same thread or room for what came next.
    pub cursor: u64,
    /// More messages than `--limit` allowed came after these. Always
    /// false with `--tail`, which reads up to the newest.
    pub more: bool,
    /// Older messages came before these (a newest page: `--tail`, or no
    /// page asked for); read them from the start with `--after 0`.
    pub earlier: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoomRecord {
    pub name: String,
    pub created_by: String,
    /// RFC 3339 UTC.
    pub created_at: String,
    pub members: Vec<String>,
    /// Unacked deliveries of this room's mail to your mailboxes (in `room
    /// list`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unacked: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoomData {
    pub room: RoomRecord,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoomListData {
    pub rooms: Vec<RoomRecord>,
}

fn room_record(r: &Room, unacked: Option<u64>) -> RoomRecord {
    RoomRecord {
        name: r.name.clone(),
        created_by: r.created_by.clone(),
        created_at: rfc3339(r.created_ms),
        members: r.members.clone(),
        unacked,
    }
}

// ---------------------------------------------------------------- helpers

fn usage(code: Code, message: impl Into<String>) -> GraphError {
    GraphError::usage(code, message)
}

fn mail_error(e: MailError) -> GraphError {
    // What was typed isn't quoted: an id, a context or a
    // room name never reaches the message, whatever the store says.
    let message = match &e {
        MailError::IdReused(_) => "that id is taken by another message or conversation".to_string(),
        MailError::RoomExists(_) => "a room has that name already".to_string(),
        MailError::HumanMember(_) => "only that human removes themselves from a room".to_string(),
        MailError::UnknownRoom(_) => "no room has that name".to_string(),
        MailError::UnknownMessage(_) => "no message has that id".to_string(),
        MailError::UnknownContext(_) => "no message or context has that id".to_string(),
        MailError::NotMember { .. } => "you aren't a member of that room".to_string(),
        MailError::Absent { .. } => "that mailbox isn't a member of the room".to_string(),
        MailError::NotHolder(_) => "none of your mailboxes received that message".to_string(),
        MailError::NotShown(_) => {
            "that message hasn't been shown to you yet, so it isn't acked".to_string()
        }
        other => other.to_string(),
    };
    match e {
        MailError::IdReused(_) => GraphError::conflict(Code::MsgIdReused, message)
            .with_hint(
                "a retry must repeat the message exactly (kind, destination, context, references, body), and an id can't name a conversation in use; otherwise use a new id",
            ),
        MailError::UnknownRoom(_) => GraphError::not_found(Code::RoomUnknown, message)
            .with_hint(format!("see rooms with `{BIN} room list`")),
        MailError::UnknownMessage(_) => GraphError::not_found(Code::MsgUnknownMessage, message)
            .with_hint(format!("see your mail, acked too, with `{BIN} msg inbox --all`")),
        MailError::UnknownContext(_) => GraphError::not_found(Code::MsgUnknownContext, message)
            .with_hint(format!(
                "give a message id, or a contextId from `{BIN} msg inbox --json`"
            )),
        MailError::RoomExists(_) => GraphError::conflict(Code::RoomExists, message)
            .with_hint(format!("join it with `{BIN} room join <room>`")),
        MailError::NotMember { .. } => GraphError::conflict(Code::RoomNotMember, message)
            .with_hint(format!(
                "only members post, add or remove others: `{BIN} room join <room>`"
            )),
        MailError::Absent { .. } => GraphError::conflict(Code::RoomNotMember, message)
            .with_hint(format!(
                "check the name: see the room's members with `{BIN} room list`"
            )),
        MailError::NotHolder(_) => GraphError::conflict(Code::MsgNotHolder, message)
            .with_hint("ack only mail delivered to you or to a role you hold"),
        MailError::NotShown(_) => GraphError::conflict(Code::MsgNotShown, message).with_hint(
            format!(
                "read it first (`{BIN} msg inbox`, or `{BIN} msg thread <id>`), then ack; nothing \
                 was acked"
            ),
        ),
        MailError::NoRecipients => GraphError::usage(Code::MsgNoRecipients, message)
            .with_hint("a send skips its sender, its writer and the roles they hold"),
        MailError::NotInConversation => GraphError::conflict(Code::MsgUnknownContext, message)
            .with_hint("reply to mail you sent or received, or start a new conversation"),
        MailError::HumanMember(_) => GraphError::conflict(Code::RoomHumanMember, message)
            .with_hint("ask the human to leave the room themselves"),
        MailError::Store(e) => store_error(e),
    }
}

fn is_name(s: &str, max: usize, allowed: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(allowed)
}

/// `import:` ids belong to imports, which name them by their request's
/// digest: nobody can take one (as an id or a context) ahead of the import
/// it's for. Replies and acks still name them.
fn not_an_import(id: &str) -> Result<(), GraphError> {
    if has_prefix(id, "import:") {
        return Err(usage(
            Code::MsgBadId,
            "a message id or context can't start with `import:`",
        )
        .with_hint("`msg import` names its messages itself"));
    }
    Ok(())
}

/// The number in a message handle typed as `#N` or `N` (the `#N` that
/// listings show), and whether it had the `#`.
fn number_handle(typed: &str) -> Option<(u64, bool)> {
    let (digits, hashed) = match typed.strip_prefix('#') {
        Some(rest) => (rest, true),
        None => (typed, false),
    };
    (!digits.is_empty() && digits.len() <= 19 && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .map(|n| (n, hashed))
}

/// A message as typed: its id, or its number as `#N` (always the number)
/// or `N`. A bare `N` is a message's id when one has it (or, with
/// `contexts`, a conversation's context when one has it), as before
/// numbers were handles, so callers that pass ids keep working; otherwise
/// it's the number.
fn message_handle(store: &Store, typed: &str, contexts: bool) -> Result<String, GraphError> {
    let Some((seq, hashed)) = number_handle(typed) else {
        return Ok(typed.to_string());
    };
    let (by_number, is_id, is_context) = store.handle_ids(Some(seq), typed).map_err(mail_error)?;
    if !hashed && (is_id || (contexts && is_context)) {
        return Ok(typed.to_string());
    }
    match by_number {
        Some(id) => Ok(id),
        None if hashed => Err(GraphError::not_found(
            Code::MsgUnknownMessage,
            "no message has that number",
        )
        .with_hint(format!("see your mail with `{BIN} msg inbox --all`"))),
        None => Ok(typed.to_string()),
    }
}

fn check_message_id(id: &str) -> Result<(), GraphError> {
    // `room:` belongs to room conversations, so no id can stand for one.
    if has_prefix(id, "room:") {
        return Err(usage(
            Code::MsgBadId,
            "a message id or context can't start with `room:`",
        )
        .with_hint("post to the room with --room; its conversation is room:<name>"));
    }
    if is_name(id, 128, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
    }) {
        // Ids and contexts are stored and shown like bodies, so scan them too.
        if looks_secret(id) {
            return Err(usage(
                Code::MsgSecretRefused,
                "a message id or context looks like it holds a credential; refused",
            )
            .with_hint("never send keys or tokens; refer to where they're kept instead"));
        }
        Ok(())
    } else {
        Err(usage(Code::MsgBadId, "that isn't a valid message id")
            .with_hint("a message id is 1 to 128 of A-Z, a-z, 0-9, `.`, `_`, `:` and `-`"))
    }
}

/// A role or human mailbox's name: lowercase, so it reads the same as the
/// role's lease, whose scope ignores case.
pub(crate) fn is_lower_name(name: &str) -> bool {
    is_name(name, 120, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | ':')
    })
}

/// A mailbox: an actor, `role:<name>` or `human:<name>` (lowercase), of
/// printable ASCII `A-Z a-z 0-9 . _ : @ -` (no spaces or commas). `run:*` is
/// a turn, not a mailbox.
pub(crate) fn check_mailbox(m: &str) -> Result<(), GraphError> {
    // Printable ASCII only, with no comma: a name can't hide characters
    // (blank-looking or right-to-left ones) or a second name in a label.
    if !is_name(m, 128, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '-')
    }) {
        return Err(usage(Code::MsgBadId, "that isn't a valid mailbox name")
            .with_hint("a mailbox is 1 to 128 of A-Z, a-z, 0-9, `.`, `_`, `:`, `@` and `-`"));
    }
    if has_prefix(m, "run:") {
        return Err(usage(Code::MsgBadId, "run:* is a turn, not a mailbox")
            .with_hint("send to the agent's own mailbox, or to a role it holds"));
    }
    for prefix in ["role:", "human:"] {
        if has_prefix(m, prefix) && !(m.starts_with(prefix) && is_lower_name(&m[prefix.len()..])) {
            return Err(
                usage(Code::MsgBadId, "that isn't a valid mailbox name").with_hint(format!(
                    "a {prefix}<name> mailbox is lowercase a-z, 0-9, `.`, `_`, `-` and `:`"
                )),
            );
        }
    }
    Ok(())
}

fn check_room(name: &str) -> Result<(), GraphError> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if first_ok
        && is_name(name, 64, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
        })
    {
        Ok(())
    } else {
        Err(usage(Code::MsgBadId, "that isn't a valid room name").with_hint(
            "a room name is 1 to 64 of a-z, 0-9, `.`, `_` and `-`, starting with a letter or digit",
        ))
    }
}

fn check_kind(kind: &str) -> Result<(), GraphError> {
    if mail::KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(usage(Code::MsgBadKind, "unknown message kind")
            .with_hint(format!("use one of: {}", mail::KINDS.join(", "))))
    }
}

fn has_prefix(m: &str, prefix: &str) -> bool {
    m.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

fn is_human(m: &str) -> bool {
    has_prefix(m, "human:")
}

/// Whether an actor name is a human's, in any case.
pub(crate) fn is_human_name(m: &str) -> bool {
    is_human(m)
}

/// Refuses an actor named like a human, a role or a turn (in any case):
/// a role is read only by holding its lease, and nobody acts as a human.
pub(crate) fn check_actor(actor: &str) -> Result<(), GraphError> {
    if is_human(actor) {
        return Err(usage(
            Code::MsgHumanActor,
            "this command can't run as a human:* mailbox",
        )
        .with_hint(
            "an agent acts as itself, and a human's answer goes in a `decision` it sends; \
             a human may only ack, read (`msg inbox|wait|thread`) and leave a room",
        ));
    }
    if has_prefix(actor, "role:") || has_prefix(actor, "run:") {
        return Err(usage(
            Code::MsgReservedActor,
            "an actor can't be named role:* or run:*",
        )
        .with_hint(format!(
            "act as yourself, and take a role with `{BIN} claim role:<name>`"
        )));
    }
    Ok(())
}

/// The actor, refusing a reserved name.
fn agent(repo: &Repo, flag: Option<String>) -> Result<String, GraphError> {
    let named = crate::coord::named_actor(flag.as_ref());
    let actor = repo.actor(flag);
    check_actor(&actor)?;
    check_actor_mailbox(&actor, named)?;
    Ok(actor)
}

/// A human acting for themselves, where that's allowed: acking their own
/// mail, reading their own mailbox, `msg thread`, and leaving a room
/// (orchestrator, 2026-10-21, D2 and the answers to round 1 of 585b125).
/// Returns the human's mailbox and the worktree's folder name (the git top
/// level of `--root`), recorded on an ack since `--as` isn't
/// authenticated; `None` for an agent. `--as human:*` with `$MDKG_ACTOR`
/// naming anyone but that human is refused: that's someone else speaking
/// for a human.
fn human_actor(repo: &Repo, flag: &Option<String>) -> Result<Option<(String, String)>, GraphError> {
    let resolved = repo.actor(flag.clone());
    if !is_human(&resolved) {
        return Ok(None);
    }
    check_actor_mailbox(&resolved, crate::coord::named_actor(flag.as_ref()))?;
    if flag.is_some()
        && let Ok(env) = std::env::var("MDKG_ACTOR")
        && !env.trim().is_empty()
        && env != resolved
    {
        let who = if is_human(&env) {
            "another human"
        } else {
            "the agent"
        };
        return Err(usage(
            Code::MsgHumanActor,
            format!("$MDKG_ACTOR names {who}, which can't act as that human"),
        )
        .with_hint("only the human runs commands as human:<name>, from their own shell"));
    }
    let via = repo
        .top
        .file_name()
        .map_or("agent".into(), |n| n.to_string_lossy().into_owned());
    Ok(Some((resolved, via)))
}

/// Who reads mail: an agent, or a human reading their own mailbox
/// (orchestrator, 2026-10-21).
fn reader(repo: &Repo, actor: Option<String>, mailbox: Option<&str>) -> Result<String, GraphError> {
    match human_actor(repo, &actor)? {
        Some((human, _)) => {
            own_human_mailbox(&human, mailbox, "reads")?;
            Ok(human)
        }
        None => agent(repo, actor),
    }
}

/// A human reads, or acks, only their own mailbox.
fn own_human_mailbox(human: &str, mailbox: Option<&str>, verb: &str) -> Result<(), GraphError> {
    match mailbox {
        Some(m) if m != human => Err(GraphError::conflict(
            Code::MsgNotHolder,
            format!("a human {verb} only their own mailbox"),
        )
        .with_hint("drop --mailbox, or pass your own human:<name>")),
        _ => Ok(()),
    }
}

/// Checks that the actor's name is a valid mailbox, and says so about the
/// actor (and, when nobody named it, the worktree folder it came from).
pub(crate) fn check_actor_mailbox(actor: &str, named: bool) -> Result<(), GraphError> {
    check_mailbox(actor).map_err(|e| {
        let mut d = *e.diagnostic;
        // The refused name isn't quoted: it is untrusted input.
        if named {
            // The hint already names the rule it broke; a turn's name is
            // the one refusal with its reason in the message.
            if d.message.starts_with("run:") {
                d.message = format!("the actor isn't a valid mailbox name: {}", d.message);
                d.hint = Some("use a name of A-Z, a-z, 0-9, `.`, `_`, `:`, `@` and `-`".into());
            } else {
                d.message = "the actor isn't a valid mailbox name".to_string();
            }
        } else {
            d.message = "the actor defaults to this worktree's folder name, which isn't a valid \
                         mailbox"
                .to_string();
            d.hint = Some("pass --as <name> or set $MDKG_ACTOR".into());
        }
        GraphError::new(e.kind, d)
    })
}

/// The mailboxes the actor reads: its own and every role it holds.
fn own_mailboxes(store: &Store, actor: &str, now: u64) -> Result<Vec<String>, GraphError> {
    let state = store.coordinator().map_err(store_error)?;
    let mut out = vec![actor.to_string()];
    out.extend(
        state
            .leases()
            .filter(|l| {
                l.owner == actor
                    && l.confirmed
                    && l.status == Status::Active
                    && l.deadline > now
                    && l.scope.as_str().starts_with("role:")
            })
            .map(|l| l.scope.as_str().to_string()),
    );
    Ok(out)
}

/// The mailboxes a read covers: `--mailbox` (one of yours, or a human's,
/// which anyone may read), else all of yours.
fn read_mailboxes(
    store: &Store,
    actor: &str,
    mailbox: Option<&str>,
    now: u64,
) -> Result<Vec<String>, GraphError> {
    if let Some(m) = mailbox {
        check_mailbox(m)?;
    }
    let own = own_mailboxes(store, actor, now)?;
    match mailbox {
        None => Ok(own),
        Some(m) if is_human(m) || own.iter().any(|o| o == m) => Ok(vec![m.to_string()]),
        // Neither the mailbox nor the actor (a mailbox too) is quoted:
        // both were typed and may contain secrets.
        Some(m) => Err(
            GraphError::conflict(Code::MsgNotHolder, "you don't hold that mailbox").with_hint(
                if m.starts_with("role:") {
                    format!("take the role with `{BIN} claim role:<name>`")
                } else {
                    "read your own mailbox, a role you hold, or a human's".to_string()
                },
            ),
        ),
    }
}

/// The retention new messages get: `coordination.messages.retention` in
/// the config, default `purge_on_ack`.
fn retention(root: &Path) -> Result<Retention, GraphError> {
    let path = root.join(config::CONFIG_PATH);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match value.pointer("/coordination/messages/retention") {
        None => Ok(Retention::PurgeOnAck),
        Some(v) => v.as_str().and_then(Retention::parse).ok_or_else(|| {
            GraphError::failure(
                Code::ConfigInvalid,
                "coordination.messages.retention must be \"purge_on_ack\", \"ttl:<days>\" or \"durable\"",
            )
            .at(config::CONFIG_PATH, None)
        }),
    }
}

/// How many days after sending a `purge_on_ack` body waits for a human
/// recipient (once every other delivery is acked):
/// `coordination.messages.human_hold_days` in the config, default 7
/// (orchestrator, 2026-10-21, D2).
fn human_hold_days(root: &Path) -> Result<u32, GraphError> {
    let path = root.join(config::CONFIG_PATH);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match value.pointer("/coordination/messages/human_hold_days") {
        None => Ok(7),
        Some(v) => v
            .as_u64()
            .filter(|d| (1..=3650).contains(d))
            .and_then(|d| u32::try_from(d).ok())
            .ok_or_else(|| {
                GraphError::failure(
                    Code::ConfigInvalid,
                    "coordination.messages.human_hold_days must be a whole number of days from 1 to 3650",
                )
                .at(config::CONFIG_PATH, None)
            }),
    }
}

/// A process-local random number: ids and text boundaries, not secrets.
fn random_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    RandomState::new().hash_one((n, std::process::id(), now_ms()))
}

fn mint_id(now: u64) -> String {
    let mut random = [0u8; 10];
    random[..8].copy_from_slice(&random_u64().to_le_bytes());
    random[8..].copy_from_slice(&random_u64().to_le_bytes()[..2]);
    mdkg_format::uuid7::uuid_v7(now, random)
}

/// A `--limit` is 1 or more: zero would read nothing (and `wait` would
/// never return). The same code as `search --limit 0` or `-1`.
fn check_limit(limit: Option<i64>) -> Result<Option<usize>, GraphError> {
    check_limit_named(limit, "--limit")
}

fn check_limit_named(limit: Option<i64>, flag: &str) -> Result<Option<usize>, GraphError> {
    match limit {
        None => Ok(None),
        Some(n) if n >= 1 => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
        Some(_) => Err(usage(
            Code::UsageBadLimit,
            format!("{flag} must be a positive integer"),
        )
        .with_hint("pass 1 or more")),
    }
}

/// A thread's page: after a cursor, the oldest `ask`, or the newest `tail`.
fn page(after: Option<u64>, ask: Option<usize>, tail: Option<usize>) -> mail::Page {
    match tail {
        Some(n) => mail::Page {
            after,
            limit: Some(n),
            newest: true,
        },
        None => mail::Page {
            after,
            limit: ask,
            newest: false,
        },
    }
}

/// A thread read's page when none is asked for, and the most one holds.
const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 500;

/// The most an ack's outcome holds: a short word or line, not a body.
const MAX_OUTCOME: usize = 500;

/// Checks an ack's outcome: short, and no credentials, like a body.
fn check_outcome(outcome: &str) -> Result<(), GraphError> {
    if outcome.len() > MAX_OUTCOME {
        return Err(usage(
            Code::MsgTooLarge,
            format!(
                "the outcome is {} bytes, over the {MAX_OUTCOME}-byte cap",
                outcome.len()
            ),
        )
        .with_hint("keep the outcome short; send the details as a message"));
    }
    if looks_secret(outcome) {
        return Err(usage(
            Code::MsgSecretRefused,
            "the outcome looks like it holds a credential; nothing was acked",
        )
        .with_hint("never send keys or tokens; refer to where they're kept instead"));
    }
    Ok(())
}

/// High-confidence credential shapes: a private key block, a known token
/// prefix followed by a long token, a JWT, or a URL with a password.
pub(crate) fn looks_secret(text: &str) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    if has_jwt(text) || has_url_password(text) || has_exact_token(text) {
        return true;
    }
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-ant-", 20),
        ("sk-proj-", 20),
        ("ghp_", 30),
        ("gho_", 30),
        ("ghs_", 30),
        ("github_pat_", 30),
        ("xoxb-", 20),
        ("xoxp-", 20),
        ("AIza", 30),
        ("glpat-", 20),
    ];
    let token = |s: &str| {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count()
    };
    PREFIXES.iter().any(|(prefix, min)| {
        text.match_indices(prefix).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let boundary = before.is_none_or(|c| !c.is_ascii_alphanumeric());
            boundary && token(&text[i + prefix.len()..]) >= *min
        })
    })
}

/// Whether `c` can be in a token: base64url's alphabet.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Tokens with an exact shape, so an ordinary identifier that merely
/// starts like one (`ASIA_PACIFIC_…`, `npm_config_…`,
/// `is_sk_live_mode`) isn't one: an AWS key id (`AKIA`/`ASIA` and exactly
/// 16 of A-Z, 0-9), an npm token (`npm_` and exactly 36 of A-Z, a-z,
/// 0-9) or a Stripe live key (`sk_live_` and 24 or more of them), with
/// no letter or digit right before or after.
fn has_exact_token(text: &str) -> bool {
    type Shape = (&'static str, usize, bool, fn(char) -> bool);
    const SHAPES: &[Shape] = &[
        ("AKIA", 16, true, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }),
        ("ASIA", 16, true, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }),
        ("npm_", 36, true, |c| c.is_ascii_alphanumeric()),
        ("sk_live_", 24, false, |c| c.is_ascii_alphanumeric()),
    ];
    SHAPES.iter().any(|(prefix, len, exact, allowed)| {
        text.match_indices(prefix).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let rest = &text[i + prefix.len()..];
            let n = rest.chars().take_while(|&c| allowed(c)).count();
            let after = rest.chars().nth(n);
            before.is_none_or(|c| !c.is_ascii_alphanumeric())
                && if *exact { n == *len } else { n >= *len }
                && after.is_none_or(|c| !c.is_ascii_alphanumeric())
        })
    })
}

/// A JWT: three dot-separated base64url parts of at least 10 characters,
/// the first a JSON header (`eyJ`). One pass over the text's words.
fn has_jwt(text: &str) -> bool {
    text.split(|c: char| !(token_char(c) || c == '.'))
        .any(|word| {
            let parts: Vec<&str> = word.split('.').collect();
            parts
                .windows(3)
                .any(|w| w[0].starts_with("eyJ") && w.iter().all(|p| p.len() >= 10))
        })
}

/// A URL with a password in it: `scheme://user:password@host` (the user
/// may be empty, `@` written either way). Placeholders (`$VAR`, `<pw>`,
/// `{pw}`) and a password that only repeats the user (`guest:guest`, a
/// local default) aren't credentials.
fn has_url_password(text: &str) -> bool {
    // What a URL's user and password can hold, unencoded (RFC 3986).
    let userinfo = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.' | '_' | '~' | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+'
            )
            || matches!(c, ',' | ';' | '=' | ':' | '%' | '@' | '\u{FF20}')
    };
    // `host:8080,` or `host:8000'`: a port that prose or code goes on
    // from, not a password.
    // Only after a host (`localhost`, a dotted name or address).
    let port_then_text = |user: &str, p: &str| {
        let host = user == "localhost" || user.contains('.');
        let digits = p.chars().take_while(char::is_ascii_digit).count();
        host && digits > 0 && matches!(p[digits..].chars().next(), Some(',' | ';' | '\''))
    };
    // A variable, not a password: `$NAME` (an environment variable), or
    // `$name` when the user is empty or a variable too, as code templates
    // write them (PHP, Perl, shell). `$hunter2` after a plain user, or
    // `$Zx9k…`, may well be a password.
    let ident = |r: &str| {
        r.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let variable = |user: &str, p: &str| {
        p.strip_prefix('$').is_some_and(|r| {
            let upper = ident(r) && r.chars().all(|c| !c.is_ascii_lowercase());
            let lower = ident(r)
                && r.chars().all(|c| !c.is_ascii_uppercase())
                && (user.is_empty() || user.strip_prefix('$').is_some_and(ident));
            upper || lower
        })
    };
    // A mask or a stand-in, as docs and log redaction write them.
    let stand_in = |p: &str| {
        p.chars().all(|c| c == '*')
            || p.chars().all(|c| c == 'x' || c == 'X')
            || [
                "password", "pass", "passwd", "pwd", "secret", "redacted", "changeme",
            ]
            .contains(&p.to_ascii_lowercase().as_str())
    };
    text.match_indices("://").any(|(i, _)| {
        let authority: String = text[i + 3..].chars().take_while(|&c| userinfo(c)).collect();
        let Some(at) = authority.rfind(['@', '\u{FF20}']) else {
            return false;
        };
        authority[..at]
            .split_once(':')
            .is_some_and(|(user, password)| {
                !password.is_empty()
                    && !variable(user, password)
                    && !stand_in(password)
                    && !port_then_text(user, password)
                    && password != user
            })
    })
}

/// Every string in a body, unescaped: text, URLs and the strings inside
/// data, so a credential at a line start or after a tab is seen as typed.
fn strings_in<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| strings_in(x, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| {
            out.push(k);
            strings_in(x, out);
        }),
        _ => {}
    }
}

/// Checks a body: at least one part, under the size cap, no credentials.
fn check_body(parts: &Value) -> Result<(), GraphError> {
    if parts.as_array().is_none_or(Vec::is_empty) {
        return Err(usage(Code::MsgNoParts, "a message needs at least one part")
            .with_hint("add --text, --text-file, --data-file or --url"));
    }
    let (_, _, size) = mail::body_digest(parts)
        .map_err(|e| GraphError::failure(Code::CoordStoreFailed, e.to_string()))?;
    if size > MAX_BODY {
        return Err(usage(
            Code::MsgTooLarge,
            format!("the body is {size} bytes, over the {MAX_BODY}-byte cap"),
        )
        .with_hint("send a URL or a path instead of the content"));
    }
    let mut strings = Vec::new();
    strings_in(parts, &mut strings);
    if strings.iter().any(|s| looks_secret(s)) {
        return Err(usage(
            Code::MsgSecretRefused,
            "the body looks like it holds a credential; nothing was stored",
        )
        .with_hint("never send keys or tokens; refer to where they're kept instead"));
    }
    Ok(())
}

/// Reads a file the caller named with `flag` (`-`: stdin). The path was
/// typed, so it isn't echoed in an error; the flag is named instead.
fn read_input(path: &Path, flag: &str) -> Result<String, GraphError> {
    let io = |e: std::io::Error| {
        GraphError::failure(
            Code::IoReadFailed,
            format!("can't read the {flag} file: {e}"),
        )
    };
    if path == Path::new("-") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(io)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).map_err(io)
    }
}

fn parts_from(a: &SendArgs) -> Result<Value, GraphError> {
    let mut parts = Vec::new();
    for t in &a.text {
        parts.push(json!({"kind": "text", "text": t}));
    }
    for f in &a.text_file {
        parts.push(json!({"kind": "text", "text": read_input(f, "--text-file")?}));
    }
    for f in &a.data_file {
        let text = read_input(f, "--data-file")?;
        let data: Value = serde_json::from_str(&text).map_err(|e| {
            usage(
                Code::UsageInvalidArguments,
                format!("a --data-file file isn't JSON: {e}"),
            )
        })?;
        parts.push(json!({"kind": "data", "data": data, "mediaType": "application/json"}));
    }
    for u in &a.url {
        parts.push(json!({"kind": "url", "url": u}));
    }
    Ok(Value::Array(parts))
}

fn destination(t: &Target) -> Result<To, GraphError> {
    match &t.room {
        Some(room) => {
            check_room(room)?;
            Ok(To::Room(room.clone()))
        }
        None => {
            for m in &t.to {
                check_mailbox(m)?;
            }
            Ok(To::Mailboxes(t.to.clone()))
        }
    }
}

/// Prints a message for people: header lines, then the body inside a
/// random boundary, so a body can't forge a header.
fn print_message(m: &MessageRecord) {
    let to = m.room.as_ref().map_or_else(
        || m.to.join(", "),
        |r| format!("room {r} ({})", m.to.join(", ")),
    );
    let via = m
        .via
        .as_ref()
        .map_or_else(String::new, |v| format!(" via {v}"));
    println!(
        "#{} {} {} from {}{via} to {to} at {}",
        m.seq, m.message_id, m.kind, m.sender, m.sent_at
    );
    if !m.reference_ids.is_empty() {
        println!("  replies to {}", m.reference_ids.join(", "));
    }
    let pending: Vec<&str> = m
        .deliveries
        .iter()
        .filter(|d| d.state != "acked")
        .map(|d| d.mailbox.as_str())
        .collect();
    if !pending.is_empty() {
        println!("  unacked by {}", pending.join(", "));
    }
    match &m.parts {
        None => println!("  (body purged; {} {} bytes)", m.digest, m.size),
        Some(parts) => {
            let boundary = format!("mdkg-body-{:016x}", random_u64());
            println!(
                "  untrusted body from {} between {boundary} lines:",
                m.sender
            );
            println!("{boundary}");
            for p in parts.as_array().into_iter().flatten() {
                match p["kind"].as_str() {
                    // Escaped, so a body can't drive the terminal (hide,
                    // erase or rewrite lines) for the human reading it.
                    Some("text") => println!(
                        "{}",
                        output::shown_body(p["text"].as_str().unwrap_or_default())
                    ),
                    Some("url") => println!(
                        "<url> {}",
                        output::shown(p["url"].as_str().unwrap_or_default())
                    ),
                    // JSON escapes only C0: C1, bidi and the like are
                    // escaped here, as in text parts.
                    _ => println!(
                        "<data> {}",
                        output::shown_body(&serde_json::to_string(&p["data"]).unwrap_or_default())
                    ),
                }
            }
            println!("{boundary}");
        }
    }
}

// ---------------------------------------------------------------- commands

fn open(root: &Path) -> Result<(Repo, Store), GraphError> {
    let repo = Repo::open(root)?;
    let store = repo.store()?;
    Ok((repo, store))
}

fn send_report(
    root: &Path,
    actor: Option<String>,
    a: &SendArgs,
) -> Result<Report<SendData>, GraphError> {
    let repo = Repo::open(root)?;
    let sender = agent(&repo, actor)?;
    check_kind(&a.kind)?;
    let (decision, warnings) = if a.kind == "decision" {
        let (grant, warnings) =
            authority_decision::grant(&repo, &sender, a.action.as_deref(), a.node_ref.as_deref())?;
        (Some(grant), warnings)
    } else {
        if a.action.is_some() || a.node_ref.is_some() {
            return Err(authority_decision::refused(
                "--action and --ref belong only to decision mail",
            ));
        }
        (None, Vec::new())
    };
    let to = destination(&a.target)?;
    if let Some(c) = &a.context {
        check_message_id(c)?;
        not_an_import(c)?;
    }
    for r in &a.reply_to {
        check_message_id(r)?;
    }
    let mut parts = parts_from(a)?;
    if decision.is_some() && parts.as_array().is_some_and(Vec::is_empty) {
        // Keep ordinary body's required-part and purge semantics; this fixed
        // context is not the approval, which lives only in store provenance.
        parts = json!([{"kind": "text", "text": "Delegated decision; see coordinator provenance for the approved action and node version."}]);
    }
    check_body(&parts)?;
    let now = now_ms();
    let message_id = match &a.id {
        Some(id) => {
            check_message_id(id)?;
            not_an_import(id)?;
            id.clone()
        }
        None => mint_id(now),
    };
    let mut store = repo.store()?;
    let out = Outgoing {
        message_id,
        context_id: a.context.clone(),
        kind: a.kind.clone(),
        sender_mailboxes: own_mailboxes(&store, &sender, now)?,
        sender,
        via: None,
        to,
        reference_ids: a.reply_to.clone(),
        parts,
        retention: retention(root)?,
        human_hold_days: human_hold_days(root)?,
        now_ms: now,
    };
    let (message, duplicate) = match decision {
        Some(grant) => store.send_decision(&out, &grant),
        None => store.send(&out),
    }
    .map_err(mail_error)?;
    Ok(Report::ok(SendData {
        message: message_record(&message),
        duplicate,
    })
    .with(warnings))
}

fn print_sent(d: &SendData) {
    let again = if d.duplicate { " (already sent)" } else { "" };
    println!(
        "sent {} #{} to {}{again}",
        d.message.message_id,
        d.message.seq,
        d.message.to.join(", ")
    );
}

fn inbox_query(
    store: &Store,
    actor: &str,
    f: &Filters,
    limit: Option<usize>,
    all: bool,
    now: u64,
) -> Result<InboxQuery, GraphError> {
    if let Some(r) = &f.room {
        check_room(r)?;
    }
    for k in f.kind.iter().chain(&f.room_kind) {
        check_kind(k)?;
    }
    Ok(InboxQuery {
        mailboxes: read_mailboxes(store, actor, f.mailbox.as_deref(), now)?,
        room: f.room.clone(),
        kinds: f.kind.clone(),
        room_kinds: f.room_kind.clone(),
        after: f.after,
        all,
        limit,
        now_ms: now,
    })
}

fn inbox_data(q: &InboxQuery, inbox: &Inbox) -> InboxData {
    InboxData {
        mailboxes: q.mailboxes.clone(),
        messages: inbox.messages.iter().map(message_record).collect(),
        cursor: inbox.cursor,
        more: inbox.more,
    }
}

fn print_inbox(d: &InboxData) {
    if d.messages.is_empty() {
        println!("no mail for {}", d.mailboxes.join(", "));
    }
    for m in &d.messages {
        print_message(m);
    }
    let more = if d.more { " (more waiting)" } else { "" };
    println!("cursor {}{more}", d.cursor);
}

fn inbox_report(
    root: &Path,
    actor: Option<String>,
    a: &InboxArgs,
) -> Result<Report<InboxData>, GraphError> {
    let limit = check_limit(a.filters.limit)?;
    let (repo, mut store) = open(root)?;
    let actor = reader(&repo, actor, a.filters.mailbox.as_deref())?;
    let q = inbox_query(&store, &actor, &a.filters, limit, a.all, now_ms())?;
    if !a.mark_shown.is_empty() {
        let listed = a
            .mark_shown
            .iter()
            .map(|listed| {
                shown_of(listed).ok_or_else(|| {
                    usage(
                        Code::UsageInvalidArguments,
                        "--mark-shown takes <seq>/<digest>/<id>",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        store
            .mark_shown(&listed, &q.mailboxes, q.now_ms)
            .map_err(mail_error)?;
        return Ok(Report::ok(InboxData {
            mailboxes: q.mailboxes.clone(),
            messages: Vec::new(),
            cursor: q.after.unwrap_or(0),
            more: false,
        }));
    }
    let inbox = store.inbox(&q).map_err(mail_error)?;
    Ok(Report::ok(inbox_data(&q, &inbox)))
}

/// How often `wait` looks: `$MDKG_WAIT_POLL_MS`, else once a second.
fn poll_interval() -> std::time::Duration {
    let ms = std::env::var("MDKG_WAIT_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000u64);
    std::time::Duration::from_millis(ms.max(10))
}

fn wait_report(
    root: &Path,
    actor: Option<String>,
    a: &WaitArgs,
) -> Result<Report<InboxData>, GraphError> {
    let most = check_limit(a.filters.limit)?;
    let (repo, mut store) = open(root)?;
    let actor = reader(&repo, actor, a.filters.mailbox.as_deref())?;
    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(a.timeout);
    loop {
        // Role holdings and the clock are read afresh on every look, so a
        // role taken while waiting counts for mail newer than the cursor. A
        // cursor covers the mailboxes it was issued for: after taking a
        // role, run `inbox` without `--after` to see its backlog.
        let q = inbox_query(&store, &actor, &a.filters, most, false, now_ms())?;
        let inbox = store.inbox(&q).map_err(mail_error)?;
        if !inbox.messages.is_empty() {
            return Ok(Report::ok(inbox_data(&q, &inbox)));
        }
        if started.elapsed() >= limit {
            return Err(GraphError::new(
                ErrorKind::Timeout,
                Diagnostic::error(
                    Code::MsgWaitTimeout,
                    format!("no mail in {}s", a.timeout),
                ),
            )
            .with_hint(
                "wait again with the same --after; if you took a role since, read once without --after",
            ));
        }
        std::thread::sleep(poll_interval().min(limit.saturating_sub(started.elapsed())));
    }
}

fn ack_report(
    root: &Path,
    actor: Option<String>,
    a: &AckArgs,
) -> Result<Report<AckData>, GraphError> {
    // An empty outcome says nothing: recorded as none.
    let outcome = a.outcome.as_deref().filter(|o| !o.trim().is_empty());
    if let Some(outcome) = outcome {
        check_outcome(outcome)?;
    }
    // Validate before opening the store, and never echo the input. A number
    // (`#N` or `N`, as listings show) is read once the store is open.
    for id in &a.ids {
        if number_handle(id).is_none() {
            check_message_id(id)?;
        }
    }
    let (repo, mut store) = open(root)?;
    let now = now_ms();
    if let Some(m) = a.mailbox.as_deref() {
        check_mailbox(m)?;
    }
    // A human acks their own deliveries, and only those; nobody else acks
    // a human's mailbox (orchestrator, 2026-10-21, D2).
    let human = human_actor(&repo, &actor)?;
    let via = human.as_ref().map(|(_, via)| via.clone());
    let (actor, mailboxes) = if let Some((human, _)) = human {
        own_human_mailbox(&human, a.mailbox.as_deref(), "acks")?;
        let mailboxes = vec![human.clone()];
        (human, mailboxes)
    } else {
        let actor = agent(&repo, actor)?;
        if a.mailbox.as_deref().is_some_and(is_human) {
            return Err(GraphError::conflict(
                Code::MsgNotHolder,
                "only the human acks a human:* mailbox",
            )
            .with_hint("a human's mail stays until they ack it with `--as human:<name>`"));
        }
        let mailboxes = read_mailboxes(&store, &actor, a.mailbox.as_deref(), now)?;
        (actor, mailboxes)
    };
    let ids = a
        .ids
        .iter()
        .map(|id| message_handle(&store, id, false))
        .collect::<Result<Vec<_>, _>>()?;
    let acked = store
        .ack(&ids, &mailboxes, &actor, via.as_deref(), outcome, now)
        .map_err(mail_error)?;
    Ok(Report::ok(AckData {
        acked: acked
            .acked
            .into_iter()
            .map(|(message_id, mailbox)| AckRecord {
                message_id,
                mailbox,
            })
            .collect(),
        purged: acked.purged,
    }))
}

fn thread_report(
    root: &Path,
    actor: Option<String>,
    a: &ThreadArgs,
) -> Result<Report<ThreadData>, GraphError> {
    // Checked here, not by clap: its `conflicts_with` on the target makes it
    // skip `requires` and `conflicts_with` between these.
    if a.tail.is_some() && (a.limit.is_some() || a.after.is_some()) {
        return Err(usage(
            Code::UsageInvalidArguments,
            "--tail reads the newest messages: it doesn't combine with --limit or --after",
        )
        .with_hint("page from the start with --limit and --after, or catch up with --tail alone"));
    }
    let limit = check_limit_named(a.limit, "--limit")?;
    let tail = check_limit_named(a.tail, "--tail")?;
    // Pages are bounded, so one read can't materialize a whole long
    // thread or room in memory.
    if limit.or(tail).is_some_and(|n| n > MAX_PAGE) {
        return Err(usage(
            Code::UsageBadLimit,
            format!("a page holds at most {MAX_PAGE} messages"),
        )
        .with_hint("page through with --limit and the returned cursor as --after"));
    }
    // `--member` reads only a room (clap skips its `requires` here, as
    // above): with a message or context it would fail open.
    if a.member && a.room.is_none() {
        return Err(usage(
            Code::UsageInvalidArguments,
            "--member reads a room: give --room",
        ));
    }
    // Unasked, a page is the newest messages, so a fresh read sees the
    // latest post; paging from a cursor reads forward.
    let tail = tail.or_else(|| (limit.is_none() && a.after.is_none()).then_some(DEFAULT_PAGE));
    let limit = limit.or_else(|| tail.is_none().then_some(DEFAULT_PAGE));
    let tail_ask = tail.map(|t| t.saturating_add(1));
    let (repo, mut store) = open(root)?;
    // Anyone reads threads; a human must still be one (main.rs lets them
    // through to here).
    human_actor(&repo, &actor)?;
    let readers = if a.member {
        let me = repo.actor(actor.clone());
        Some(own_mailboxes(&store, &me, now_ms())?)
    } else {
        None
    };
    // One more than the limit tells whether more came after.
    let ask = limit.map(|l| l.saturating_add(1));
    let (context_id, mut messages) = match (&a.target, &a.room) {
        (_, Some(room)) => {
            check_room(room)?;
            let messages = match &readers {
                Some(mine) => {
                    store.member_room_messages(room, mine, page(a.after, ask, tail_ask), now_ms())
                }
                None => store.room_messages(room, page(a.after, ask, tail_ask), now_ms()),
            }
            .map_err(mail_error)?;
            (format!("room:{room}"), messages)
        }
        // Don't echo typed text, which may contain secrets.
        (Some(target), None) if has_prefix(target, "room:") => {
            return Err(usage(
                Code::UsageInvalidArguments,
                "that names a room, not a message or a context",
            )
            .with_hint(format!("read a room with `{BIN} msg thread --room <room>`")));
        }
        (Some(target), None) => {
            let target = message_handle(&store, target, true)?;
            store
                .thread(&target, page(a.after, ask, tail_ask), now_ms())
                .map_err(mail_error)?
        }
        (None, None) => {
            return Err(usage(
                Code::UsageInvalidArguments,
                "name a message, a context or --room",
            ));
        }
    };
    let more = limit.is_some_and(|l| messages.len() > l);
    if let Some(l) = limit {
        messages.truncate(l);
    }
    // One more than the tail tells whether older messages came before.
    let earlier = tail.is_some_and(|t| messages.len() > t);
    if earlier {
        messages.remove(0);
    }
    let cursor = messages.last().map_or(a.after.unwrap_or(0), |m| m.seq);
    Ok(Report::ok(ThreadData {
        context_id,
        messages: messages.iter().map(message_record).collect(),
        cursor,
        more,
        earlier,
    }))
}

fn import_report(
    root: &Path,
    actor: Option<String>,
    a: &ImportArgs,
) -> Result<Report<SendData>, GraphError> {
    let (repo, mut store) = open(root)?;
    let writer = agent(&repo, actor)?;
    check_kind(&a.kind)?;
    check_mailbox(&a.from)?;
    if is_human(&a.from) {
        return Err(
            usage(Code::MsgHumanActor, "`--from` can't name a human:* sender")
                .with_hint("a human's answer goes in a `decision` you send as yourself"),
        );
    }
    check_actor(&a.from)?;
    if a.kind == "decision" {
        return Err(authority_decision::refused(
            "decision mail cannot be imported; use the outside-turn delegate send path",
        ));
    }
    if let Some(c) = &a.context {
        check_message_id(c)?;
        not_an_import(c)?;
    }
    if let Some(r) = &a.reply_to {
        check_message_id(r)?;
    }
    let to = destination(&a.target)?;
    let text = read_input(&a.file, "import")?;
    let part = match serde_json::from_str::<Value>(&text) {
        Ok(data) => json!({"kind": "data", "data": data, "mediaType": "application/json"}),
        Err(_) => json!({"kind": "text", "text": text}),
    };
    let parts = Value::Array(vec![part]);
    check_body(&parts)?;
    let mut out = Outgoing {
        message_id: String::new(),
        context_id: a.context.clone(),
        kind: a.kind.clone(),
        sender: a.from.clone(),
        // The writer's mailboxes and the roles the sender holds: neither
        // gets a copy (D1), and either makes the import a room member.
        sender_mailboxes: {
            let mut own = own_mailboxes(&store, &writer, now_ms())?;
            own.extend(
                own_mailboxes(&store, &a.from, now_ms())?
                    .into_iter()
                    .filter(|m| m.starts_with("role:")),
            );
            own
        },
        via: Some(writer),
        to,
        reference_ids: a.reply_to.iter().cloned().collect(),
        parts,
        retention: retention(root)?,
        human_hold_days: human_hold_days(root)?,
        now_ms: now_ms(),
    };
    // The id is the request's digest, so re-importing the same output for
    // the same request is a no-op, whoever writes it.
    let digest = mail::request_id(&out).map_err(mail_error)?;
    let hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
    out.message_id = format!("import:{}", &hex[..32]);
    let (message, duplicate) = store.send(&out).map_err(mail_error)?;
    Ok(Report::ok(SendData {
        message: message_record(&message),
        duplicate,
    }))
}

pub fn run(root: &Path, actor: Option<String>, cmd: &MsgCommand) -> u8 {
    let json = cmd.json();
    let name = cmd.name();
    match cmd {
        MsgCommand::Decisions {
            after,
            limit,
            message,
            ..
        } => {
            let r = (|| {
                let repo = Repo::open(root)?;
                if message.len() > 1000 {
                    return Err(authority_decision::refused("at most 1000 decision IDs"));
                }
                for id in message {
                    check_message_id(id)?;
                }
                let path = mdkg_coord_sqlite::git::store_path(&repo.top)
                    .map_err(|_| authority_decision::refused("cannot locate decision store"))?;
                let decisions = mail::read_decisions(&path, *after, usize::from(*limit), message)
                    .map_err(mail_error)?;
                Ok(Report::ok(DecisionsData { decisions }))
            })()
            .unwrap_or_else(Report::failed);
            output::emit(name, json, r, |d| {
                println!(
                    "Send-time decision records only; human approval and launcher promotion are not verified here."
                );
                for row in &d.decisions {
                    println!("{}", serde_json::to_string(row).unwrap_or_default());
                }
            })
        }
        MsgCommand::Send(a) => {
            let r = send_report(root, actor, a).unwrap_or_else(Report::failed);
            output::emit(name, json, r, print_sent)
        }
        MsgCommand::Import(a) => {
            let r = import_report(root, actor, a).unwrap_or_else(Report::failed);
            output::emit(name, json, r, print_sent)
        }
        MsgCommand::Inbox(a) => {
            let r = inbox_report(root, actor, a).unwrap_or_else(Report::failed);
            let listed = listed_in(&r);
            let code = output::emit(name, json, r, print_inbox);
            if !a.no_mark && a.mark_shown.is_empty() {
                mark_listed(root, listed);
            }
            code
        }
        MsgCommand::Wait(a) => {
            let r = wait_report(root, actor, a).unwrap_or_else(Report::failed);
            let listed = listed_in(&r);
            let code = output::emit(name, json, r, print_inbox);
            if !a.no_mark {
                mark_listed(root, listed);
            }
            code
        }
        MsgCommand::Ack(a) => {
            let r = ack_report(root, actor, a).unwrap_or_else(Report::failed);
            output::emit(name, json, r, |d| {
                for x in &d.acked {
                    println!("acked {} for {}", x.message_id, x.mailbox);
                }
                if !d.purged.is_empty() {
                    println!("bodies purged: {}", d.purged.join(", "));
                }
            })
        }
        MsgCommand::Thread(a) => {
            let r = thread_report(root, actor.clone(), a).unwrap_or_else(Report::failed);
            // Marked for the reader's own mailboxes once the listing is out.
            let listed: Vec<Shown> = r
                .data
                .as_ref()
                .map(|d| d.messages.iter().map(shown).collect())
                .unwrap_or_default();
            let no_mark = a.no_mark;
            let after = move || {
                if !no_mark && !listed.is_empty() {
                    mark_read(root, actor, listed);
                }
            };
            let code = output::emit(name, json, r, |d| {
                println!("context {}", d.context_id);
                for m in &d.messages {
                    print_message(m);
                }
                if d.more {
                    println!("more after cursor {} (pass --after {})", d.cursor, d.cursor);
                }
                if d.earlier {
                    println!("earlier messages came before these (read from --after 0)");
                }
            });
            after();
            code
        }
    }
}

/// Messages a listing showed (number and id), and the mailboxes it read.
type Listed = (Vec<Shown>, Vec<String>);

/// A listed message as [`Store::mark_shown`] checks it.
fn shown(m: &MessageRecord) -> Shown {
    Shown {
        seq: m.seq,
        message_id: m.message_id.clone(),
        digest: m.digest.clone(),
    }
}

/// `<seq>/<digest>/<id>` (neither digests nor ids hold a `/`).
fn shown_of(listed: &str) -> Option<Shown> {
    let mut parts = listed.splitn(3, '/');
    let seq = parts.next()?.parse().ok()?;
    let digest = parts.next()?.to_string();
    let message_id = parts.next()?.to_string();
    (!digest.is_empty() && !message_id.is_empty()).then_some(Shown {
        seq,
        message_id,
        digest,
    })
}

/// What an inbox or wait report lists, and to which mailboxes.
fn listed_in(r: &Report<InboxData>) -> Option<Listed> {
    let d = r.data.as_ref()?;
    (!d.messages.is_empty()).then(|| (d.messages.iter().map(shown).collect(), d.mailboxes.clone()))
}

/// Records what a listing showed, once it's out (printing failed would
/// have stopped the command before this). Best effort: a mark that fails
/// leaves the mail unshown, so its ack is refused until it's listed
/// again, never the other way round.
fn mark_listed(root: &Path, listed: Option<Listed>) {
    let Some((seqs, mailboxes)) = listed else {
        return;
    };
    if let Ok((_, mut store)) = open(root) {
        store.mark_shown(&seqs, &mailboxes, now_ms()).ok();
    }
}

/// [`mark_listed`] for a thread or room read: the reader's own mailboxes
/// (a human's own, or an agent's and the roles it holds).
fn mark_read(root: &Path, actor: Option<String>, seqs: Vec<Shown>) {
    let Ok((repo, mut store)) = open(root) else {
        return;
    };
    let now = now_ms();
    let mailboxes = match human_actor(&repo, &actor) {
        Ok(Some((human, _))) => vec![human],
        Ok(None) => match own_mailboxes(&store, &repo.actor(actor), now) {
            Ok(own) => own,
            Err(_) => return,
        },
        Err(_) => return,
    };
    store.mark_shown(&seqs, &mailboxes, now).ok();
}

fn room_members(
    repo: &Repo,
    actor: Option<String>,
    a: &RoomArgs,
) -> Result<(String, Vec<String>), GraphError> {
    let actor = agent(repo, actor)?;
    check_room(&a.name)?;
    let members = if a.member.is_empty() {
        vec![actor.clone()]
    } else {
        a.member.clone()
    };
    Ok((actor, members))
}

pub fn room(root: &Path, actor: Option<String>, cmd: &RoomCommand) -> u8 {
    let json = cmd.json();
    let name = cmd.name();
    let report = (|| -> Result<Report<Value>, GraphError> {
        let (repo, mut store) = open(root)?;
        let now = now_ms();
        let mut warnings = Vec::new();
        let data = match cmd {
            RoomCommand::Create(a) => {
                let (actor, _) = room_members(&repo, actor, a)?;
                a.member.iter().try_for_each(|m| check_mailbox(m))?;
                let room = store
                    .room_create(&a.name, &actor, &a.member, now)
                    .map_err(mail_error)?;
                serde_json::to_value(RoomData {
                    room: room_record(&room, None),
                })
            }
            RoomCommand::Join(a) => {
                let (actor, members) = room_members(&repo, actor, a)?;
                // Names are checked on the way in; `leave` takes any current
                // member, so a name an older build allowed can still go.
                a.member.iter().try_for_each(|m| check_mailbox(m))?;
                let own = own_mailboxes(&store, &actor, now)?;
                let room = store
                    .room_join(&a.name, &members, &actor, &own, now)
                    .map_err(mail_error)?;
                serde_json::to_value(RoomData {
                    room: room_record(&room, None),
                })
            }
            RoomCommand::Leave(a) => {
                // A human leaves a room themselves, and removes nobody else.
                let (actor, members) = match human_actor(&repo, &actor)? {
                    Some((human, _)) => {
                        check_room(&a.name)?;
                        if a.member.iter().any(|m| *m != human) {
                            return Err(usage(
                                Code::MsgHumanActor,
                                "a human leaves a room only as themselves",
                            )
                            .with_hint("drop --member"));
                        }
                        (human.clone(), vec![human])
                    }
                    None => room_members(&repo, actor, a)?,
                };
                // Leaving yourself again is done already (as releasing an
                // ended lease is): a warning. An explicit --member that
                // isn't a member is refused.
                let leaving_self = a.member.is_empty();
                let own = own_mailboxes(&store, &actor, now)?;
                let (room, absent) = store
                    .room_leave(&a.name, &members, &actor, &own, leaving_self)
                    .map_err(mail_error)?;
                if !absent.is_empty() {
                    let hint = if is_human(&actor) {
                        "nothing to do: you aren't in this room".to_string()
                    } else {
                        format!(
                            "to leave as a role or another mailbox, pass --member <mailbox>; see the members with `{BIN} room list`"
                        )
                    };
                    warnings.push(
                        Diagnostic::warning(
                            Code::RoomNotMember,
                            "you aren't a member of that room; nothing to leave",
                        )
                        .with_hint(hint),
                    );
                }
                serde_json::to_value(RoomData {
                    room: room_record(&room, None),
                })
            }
            RoomCommand::List { mine, .. } => {
                let actor = agent(&repo, actor)?;
                let own = own_mailboxes(&store, &actor, now)?;
                let own_set: BTreeSet<&String> = own.iter().collect();
                let mut rooms = Vec::new();
                for r in store.rooms().map_err(mail_error)? {
                    if *mine && !r.members.iter().any(|m| own_set.contains(m)) {
                        continue;
                    }
                    let unread = store
                        .inbox(&InboxQuery {
                            mailboxes: own.clone(),
                            room: Some(r.name.clone()),
                            now_ms: now,
                            ..InboxQuery::default()
                        })
                        .map_err(mail_error)?
                        .messages
                        .len() as u64;
                    rooms.push(room_record(&r, Some(unread)));
                }
                serde_json::to_value(RoomListData { rooms })
            }
        }
        .map_err(|e| GraphError::failure(Code::CoordStoreFailed, e.to_string()))?;
        Ok(Report::ok(data).with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit(name, json, report, |d| {
        let rooms: Vec<&Value> = match d.get("rooms") {
            Some(Value::Array(rooms)) => rooms.iter().collect(),
            _ => d.get("room").into_iter().collect(),
        };
        if rooms.is_empty() {
            println!("no rooms");
        }
        for r in rooms {
            let members: Vec<&str> = r["members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let unacked = r["unacked"]
                .as_u64()
                .map_or_else(String::new, |n| format!(", {n} unacked"));
            println!(
                "room {}: {}{unacked}",
                r["name"].as_str().unwrap_or_default(),
                members.join(", ")
            );
        }
    })
}

/// Unacked mail for the actor, for `status`.
pub fn unacked(store: &Store, actor: &str, now: u64) -> Vec<(String, u64)> {
    own_mailboxes(store, actor, now)
        .ok()
        .and_then(|own| store.unacked(&own).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_recognised_without_tripping_on_ordinary_text() {
        assert!(looks_secret(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----"
        ));
        assert!(looks_secret(
            "token: ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        assert!(looks_secret("key=AKIAIOSFODNN7EXAMPLE"));
        // Task-36: more prefixes, JWTs and URLs with a password.
        for secret in [
            "ASIAIOSFODNN7EXAMPLE",
            concat!("sk_", "live_", "abcdefghijklmnopqrstuvwx"),
            "npm_abcdefghijklmnopqrstuvwxyz0123456789",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abcdefghijklmnop",
            "DATABASE_URL=postgres://app:hunter2@db.internal:5432/x",
            "postgres://app:hunter2\u{FF20}db.internal",
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        assert!(looks_secret("redis://:hunter2@cache:6379"));
        assert!(looks_secret(&format!("x {}", ".eyJabcdefghij".repeat(3))));
        // Review round 2 of audit-core.
        for secret in [
            "key_AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE_backup",
            "postgres://app:$Zx9kq2LmQ7@db.prod",
            concat!("sk_", "live_", "abcdefghijklmnopqrstuvwx"),
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        for ordinary in [
            "postgresql://scott:***@db/x",
            "amqp://guest:**@localhost:5672//",
            "redis://:****@cache",
            "postgres://user:password@localhost/x",
            "USER:PASSWORD@HOST via mysql://USER:PASSWORD@HOST/db",
            "postgres://app:REDACTED@db",
            "postgres://app:xxxxx@db",
            "{'url':'http://localhost:8000','user':'bot@example.com'}",
            "see http://localhost:8080,ops@example.com",
            "is_sk_live_mode_enabled_for_this_account",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        // Review round 4 of audit-core.
        for secret in [
            "postgres://app:$ecretsauce@db.prod",
            "postgres://app:$hunter2@db.prod",
            "postgres://app:2024;Spring!@db.prod",
            "redis://:1;Zx9kq2Lm@cache",
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        assert!(!looks_secret("see http://example.com:8080;admin@corp"));
        assert_eq!(
            crate::output::shown_body("a\r\nb\rc\u{9b}2K\u{202e}"),
            "a\r\nb\\u{d}c\\u{9b}2K\\u{202e}"
        );
        // Review round 3 of audit-core.
        for secret in [
            "postgres://app:pa;ss9Zk2@db.prod",
            "postgres://app:Zx9,kq2Lm@db.prod",
            "redis://:a1b2;c3d4@cache",
        ] {
            assert!(looks_secret(secret), "{secret}");
        }
        for ordinary in [
            r#"$dsn = "mysql://$user:$pass@$host/$db";"#,
            "DATABASE_URL=postgres://$db_user:$db_password@$db_host:5432/app",
            "redis://:$redis_pass@cache:6379",
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        assert!(not_an_import("import:abc").is_err());
        // Review round 1 of audit-core: ordinary text stays sendable.
        for ordinary in [
            "ssh://git@github.com/x/y",
            "https://example.com/a:b@c",
            "see eyJ for the header",
            "http://user:@host",
            "AWS_REGION_ASIA_PACIFIC_SOUTHEAST_1",
            "ASIA_PACIFIC_NORTHEAST",
            "npm_config_strict_peer_dependencies=false",
            "npm_package_dependencies_typescript_version",
            "postgres://postgres:postgres@localhost:5432/app",
            "postgres://$POSTGRES_USER:$POSTGRES_PASSWORD@db:5432/app",
            "mongodb://<username>:<password>@cluster0.example.net",
            "amqp://guest:guest@localhost:5672/",
            r#"{"url":"https://example.com","user":"bot@example.com"}"#,
        ] {
            assert!(!looks_secret(ordinary), "{ordinary}");
        }
        assert!(
            check_message_id("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abcdefghijklmnop").is_err()
        );
        assert!(!looks_secret("the ghp_ prefix marks GitHub tokens"));
        assert!(!looks_secret("secret = os.environ['X']"));
        assert!(!looks_secret("MASKIAIOSFODNN7EXAMPLEX is not an AWS key"));
        // Scanned as typed, not as escaped JSON (review of a76dd2c).
        let mut strings = Vec::new();
        let parts = json!([
            {"kind": "text", "text": "here:\nghp_abcdefghijklmnopqrstuvwxyz0123456789"},
            {"kind": "data", "data": {"k": "\tAKIAIOSFODNN7EXAMPLE"}}
        ]);
        strings_in(&parts, &mut strings);
        assert_eq!(strings.iter().filter(|s| looks_secret(s)).count(), 2);
    }

    #[test]
    fn names_are_checked() {
        assert!(check_message_id("01J9-abc:1.x_y").is_ok());
        assert!(check_message_id("has space").is_err());
        assert!(check_room("review-1").is_ok());
        assert!(check_room("Review").is_err());
        assert!(check_room("-x").is_err());
        assert!(check_mailbox("codex:pr19-4226245").is_ok());
        assert!(check_mailbox("a b").is_err());
    }
}
