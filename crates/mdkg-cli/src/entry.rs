//! `mdkg entry`: the ledger's commands (edd-01a1878e, A27 and A29).
//!
//! - Every op subcommand builds one op object and hands it to the same
//!   parser ledger blocks and the MCP `entry_update` tool use
//!   ([`mdkg_coord_sqlite::entries::Op::from_object`]), so the CLI can't
//!   accept a field a block would refuse, or the reverse.
//! - The actor's class comes from the agent definitions in this graph's
//!   `.mdkg/agents` (A27, rule 4): believed names, guard rails only.
//! - SHAs are checked against the repository's common directory, never
//!   an agent's checkout (rule 5).
//! - Titles and texts are untrusted data and pass the mail secret filter
//!   before any op reaches the store. Diagnostics never echo them.

use std::io::Read;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use mdkg_coord::Scope;
use mdkg_coord_sqlite::entries::{
    self, AgentFacts, Applied, Change, Class, Entry, EntryError, Env, Filter, Kind, NodeFolder, Op,
};
use mdkg_coord_sqlite::{Store, git, ledger::NodeLink};
use mdkg_format::Code;
use mdkg_graph::{GraphError, IndexNode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::coord::{Repo, store_error};
use crate::credentials::looks_secret;
use crate::output::{Report, emit};

/// The ledger's commands.
#[derive(Debug, Subcommand)]
pub enum EntryCommand {
    /// List a node's entries.
    List(ListArgs),
    /// Show one entry and its history.
    Show(ShowArgs),
    /// Check the node's gate: what `done` would refuse for (A29).
    Gate(GateArgs),
    /// Apply op objects, one JSON object per line, from a file or `-`.
    Update(UpdateArgs),
    /// Open a finding, an issue, a question or a decision.
    Add(OpArgs),
    /// Ask a question on a node.
    Ask(OpArgs),
    /// Answer a question asked of you, or your own.
    Answer(OpArgs),
    /// Record an accepted decision, or accept a proposed one (`<entry>`).
    Decide(OpArgs),
    /// Propose a decision.
    Propose(OpArgs),
    /// Complete a review round you were asked for (`review.complete`).
    Complete(OpArgs),
    /// Mark a finding fixed at a SHA.
    Fix(OpArgs),
    /// Verify a finding's fix at a reviewed SHA.
    Verify(OpArgs),
    /// Reopen a fixed finding (`--at` the reviewed SHA), or a disputed one.
    Reopen(OpArgs),
    /// Reject a finding, or a proposed decision, with a reason.
    Reject(OpArgs),
    /// Dispute a rejected finding.
    Dispute(OpArgs),
    /// Accept a failure as residual (a person or a coordinator).
    Residual(OpArgs),
    /// Mark a finding a duplicate of another (`--of`).
    Duplicate(OpArgs),
    /// Mark a finding outdated.
    Outdated(OpArgs),
    /// Withdraw your own open question.
    Withdraw(OpArgs),
    /// Close an answered question you asked, or a rejected finding.
    Close(OpArgs),
    /// Resolve an issue.
    Resolve(OpArgs),
    /// Move an open question or issue to another node (`--to`).
    Move(OpArgs),
    /// Set a requirement's state: met, unmet or waived (`--state`).
    Req(OpArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// The node: an id, a qid or a unique prefix.
    pub node: String,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub state: Option<String>,
    #[arg(long)]
    pub severity: Option<String>,
    #[arg(long)]
    pub asked_of: Option<String>,
    #[arg(long)]
    pub blocking: Option<bool>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub node: String,
    /// The entry: `F3`.
    pub entry: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GateArgs {
    pub node: String,
    /// The head to check at (default: this checkout's HEAD).
    #[arg(long)]
    pub at: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// The node ops act on when they name none.
    pub node: String,
    /// Op objects, one per line; `-` reads stdin.
    #[arg(long)]
    pub op_file: PathBuf,
    #[arg(long)]
    pub json: bool,
}

/// The fields of any op. Each subcommand passes only what it was given;
/// the shared parser refuses a field its op doesn't take.
#[derive(Debug, Args)]
pub struct OpArgs {
    /// The node: an id, a qid or a unique prefix.
    pub node: String,
    /// The entry the op acts on (`F3`; `R2` for `req`; `V1` for
    /// `complete`).
    pub entry: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    /// The text: a reason, an answer, a note.
    #[arg(long, alias = "reason")]
    pub text: Option<String>,
    #[arg(long)]
    pub severity: Option<String>,
    /// `path` or `path:line`.
    #[arg(long)]
    pub anchor: Option<String>,
    #[arg(long)]
    pub requirement: Vec<String>,
    #[arg(long)]
    pub asked_of: Option<String>,
    #[arg(long)]
    pub blocking: bool,
    #[arg(long)]
    pub slot: Option<String>,
    #[arg(long = "ref")]
    pub refs: Vec<String>,
    #[arg(long)]
    pub supersedes: Vec<String>,
    #[arg(long)]
    pub sha: Option<String>,
    /// For `fix`: another commit of the fix, besides `--sha` (repeat it).
    #[arg(long = "commit")]
    pub commits: Vec<String>,
    /// For `fix` on a reopened finding, by one of its fixers: these commits
    /// replace every one declared before (recorded as `supersede`).
    #[arg(long)]
    pub supersede: bool,
    #[arg(long)]
    pub at: Option<String>,
    #[arg(long)]
    pub of: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    /// For `req`: met, unmet or waived.
    #[arg(long)]
    pub state: Option<String>,
    /// For `complete`: `<base>..<sha>`.
    #[arg(long)]
    pub range: Option<String>,
    /// For `complete`: clean or findings.
    #[arg(long)]
    pub verdict: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl EntryCommand {
    pub fn json(&self) -> bool {
        match self {
            Self::List(a) => a.json,
            Self::Show(a) => a.json,
            Self::Gate(a) => a.json,
            Self::Update(a) => a.json,
            Self::Add(a)
            | Self::Ask(a)
            | Self::Answer(a)
            | Self::Decide(a)
            | Self::Propose(a)
            | Self::Complete(a)
            | Self::Fix(a)
            | Self::Verify(a)
            | Self::Reopen(a)
            | Self::Reject(a)
            | Self::Dispute(a)
            | Self::Residual(a)
            | Self::Duplicate(a)
            | Self::Outdated(a)
            | Self::Withdraw(a)
            | Self::Close(a)
            | Self::Resolve(a)
            | Self::Move(a)
            | Self::Req(a) => a.json,
        }
    }

    /// The command's schema name (`mdkg.<name>.v1`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::List(_) => "entry.list",
            Self::Show(_) => "entry.show",
            Self::Gate(_) => "entry.gate",
            Self::Update(_) => "entry.update",
            Self::Add(_) => "entry.add",
            Self::Ask(_) => "entry.ask",
            Self::Answer(_) => "entry.answer",
            Self::Decide(_) => "entry.decide",
            Self::Propose(_) => "entry.propose",
            Self::Complete(_) => "entry.complete",
            Self::Fix(_) => "entry.fix",
            Self::Verify(_) => "entry.verify",
            Self::Reopen(_) => "entry.reopen",
            Self::Reject(_) => "entry.reject",
            Self::Dispute(_) => "entry.dispute",
            Self::Residual(_) => "entry.residual",
            Self::Duplicate(_) => "entry.duplicate",
            Self::Outdated(_) => "entry.outdated",
            Self::Withdraw(_) => "entry.withdraw",
            Self::Close(_) => "entry.close",
            Self::Resolve(_) => "entry.resolve",
            Self::Move(_) => "entry.move",
            Self::Req(_) => "entry.req",
        }
    }

    /// The op an op subcommand makes, and its arguments.
    fn op(&self) -> Option<(&'static str, &OpArgs)> {
        Some(match self {
            Self::List(_) | Self::Show(_) | Self::Gate(_) | Self::Update(_) => return None,
            Self::Add(a) => ("add", a),
            Self::Ask(a) => ("ask", a),
            Self::Answer(a) => ("answer", a),
            Self::Decide(a) => ("decide", a),
            Self::Propose(a) => ("propose", a),
            Self::Complete(a) => ("review.complete", a),
            Self::Fix(a) => ("fix", a),
            Self::Verify(a) => ("verify", a),
            Self::Reopen(a) => ("reopen", a),
            Self::Reject(a) => ("reject", a),
            Self::Dispute(a) => ("dispute", a),
            Self::Residual(a) => ("residual", a),
            Self::Duplicate(a) => ("duplicate", a),
            Self::Outdated(a) => ("outdated", a),
            Self::Withdraw(a) => ("withdraw", a),
            Self::Close(a) => ("close", a),
            Self::Resolve(a) => ("resolve", a),
            Self::Move(a) => ("move", a),
            Self::Req(a) => ("req", a),
        })
    }
}

/// The op object an op subcommand stands for: only the fields given.
fn object_of(op: &str, a: &OpArgs) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("op".into(), op.into());
    let mut put = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            m.insert(k.into(), v.clone().into());
        }
    };
    let entry_key = match op {
        "req" => "req",
        "review.complete" => "round",
        _ => "entry",
    };
    put(entry_key, &a.entry);
    put("kind", &a.kind);
    put("title", &a.title);
    put("text", &a.text);
    put("severity", &a.severity);
    put("anchor", &a.anchor);
    put("asked_of", &a.asked_of);
    put("slot", &a.slot);
    put("sha", &a.sha);
    put("at", &a.at);
    put("of", &a.of);
    put("to", &a.to);
    put("state", &a.state);
    put("range", &a.range);
    put("verdict", &a.verdict);
    let mut list = |k: &str, v: &[String]| {
        if !v.is_empty() {
            m.insert(k.into(), json!(v));
        }
    };
    list("requirement", &a.requirement);
    list("refs", &a.refs);
    list("supersedes", &a.supersedes);
    list("commits", &a.commits);
    if a.blocking {
        m.insert("blocking".into(), true.into());
    }
    if a.supersede {
        m.insert("supersede".into(), true.into());
    }
    m
}

/// An op's or a list's output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UpdateData {
    pub node: String,
    /// Per op line (`entry update`), or the one op: what it changed, or
    /// why it was refused (a code, never the line's content).
    pub results: Vec<OpResult>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OpResult {
    pub line: usize,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub changed: Vec<ChangedOut>,
    /// For an `add` that repeats an open finding: that finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing: Option<String>,
    /// The refusal in full, for a one-op command, whose refusal is the
    /// command's (its message and hint); never printed per line.
    #[serde(skip)]
    #[schemars(skip)]
    pub refusal: Option<GraphError>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ChangedOut {
    pub entry: String,
    pub op: String,
    pub from: Option<String>,
    pub to: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListData {
    pub node: String,
    pub entries: Vec<EntryRecord>,
    pub input_required: Vec<String>,
    pub decisions_in_force: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShowData {
    pub entry: EntryRecord,
    /// Its transitions, oldest first.
    pub history: Vec<EntryEvent>,
}

/// One entry as `list` and `show` print it (A27, "Fields"): the names and
/// values [`Entry::to_json`] gives, typed. Each context key is present
/// only when the entry holds it. A stored value that doesn't read back
/// as its type here byte for byte stays in `other` as stored, so the
/// output never loses or changes one. Such a value prints under its own
/// key and doesn't fit that key's schema; no current writer stores one,
/// and the sample-ledger test in tests/entry.rs fails on a new one.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EntryRecord {
    /// The node's id.
    pub node: String,
    /// `Q1`, `D2`, `F3`, `I4`, `V5`, `R6`: kind letter and number.
    pub id: String,
    /// `question`, `decision`, `finding`, `issue`, `review` or `req`.
    pub kind: String,
    pub state: String,
    pub severity: Option<String>,
    pub slot: Option<String>,
    pub blocking: bool,
    pub title: String,
    pub text: Option<String>,
    /// Who opened it, when, and the message and turn it came in.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Stamp")]
    pub opened: Option<Stamp>,
    /// A finding's place.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EntryAnchor")]
    pub anchor: Option<EntryAnchor>,
    /// The requirement ids it is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub requirement: Option<Vec<String>>,
    /// The actor a question was asked of.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub asked_of: Option<String>,
    /// The entries it was opened from (`<node>#<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub refs: Option<Vec<String>>,
    /// A review round's range.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EntryRange")]
    pub range: Option<EntryRange>,
    /// A review round's reviewers.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub reviewers: Option<Vec<String>>,
    /// The reviewers that completed a round, in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<Completion>")]
    pub completed: Option<Vec<Completion>>,
    /// The review request's message id that opened a round.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub request: Option<String>,
    /// The ref a round anchored on, or null for one that didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchored: Option<Option<String>>,
    /// A question's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Answer")]
    pub answer: Option<Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub answered_by: Option<Vec<String>>,
    /// A finding's latest fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Fix")]
    pub fix: Option<Fix>,
    /// Every fix commit declared, across reopens.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub fix_commits: Option<Vec<String>>,
    /// Every actor that recorded a fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub fixers: Option<Vec<String>>,
    /// Fix commits a later fix superseded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub superseded_commits: Option<Vec<String>>,
    /// A verified finding's or a met requirement's verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Verified")]
    pub verified: Option<Verified>,
    /// A requirement verdict relayed by an outside-turn coordinator.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ReviewReceipt")]
    pub review_receipt: Option<ReviewReceipt>,
    /// A duplicate finding's original (`<node>#<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub duplicate_of: Option<String>,
    /// The decision that superseded this one (`<node>#<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub superseded_by: Option<String>,
    /// A proposed decision's ids that it supersedes once accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub supersedes: Option<Vec<String>>,
    /// A moved entry's copy (`<node>#<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub moved_to: Option<String>,
    /// A residual's waiver of a verified finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Waives")]
    pub waives: Option<Waives>,
    /// A requirement's graph folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub graph_folder: Option<String>,
    /// Reserved in the ledger's records; no op writes it yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notified: Option<Value>,
    /// Any other key the entry holds, as stored.
    #[serde(flatten)]
    pub other: Map<String, Value>,
    pub opened_ms: u64,
    pub updated_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Stamp {
    pub by: String,
    pub at: u64,
    pub message: Option<String>,
    pub turn: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryAnchor {
    pub path: String,
    pub line: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryRange {
    pub base: String,
    pub sha: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Completion {
    pub reviewer: String,
    pub by: String,
    /// `clean` or `findings`.
    pub verdict: String,
    pub at: u64,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Answer {
    pub by: String,
    pub at: u64,
    pub message: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Fix {
    pub sha: String,
    pub commits: Vec<String>,
    pub by: String,
    pub at: u64,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewReceipt {
    pub round: String,
    pub reviewer: String,
    pub recorded_by: String,
    pub message: String,
    pub sha: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Verified {
    pub sha: String,
    pub by: String,
    pub at: u64,
    /// A finding's: the round it was verified in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub review: Option<String>,
    /// A finding's: the fix commits the verify covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub fix_commits: Option<Vec<String>>,
    /// A finding's: the paths they changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>")]
    pub fix_paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Waives {
    pub entry: String,
    pub verified: String,
    pub head: String,
    pub base: String,
}

/// One transition in an entry's history ([`entries::Event::to_json`]).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EntryEvent {
    /// The ledger event's seq: the change marker.
    pub seq: u64,
    pub op: String,
    pub from: Option<String>,
    pub to: String,
    pub by: String,
    pub at_ms: u64,
    pub sha: Option<String>,
    pub message: Option<String>,
    pub turn: Option<String>,
    pub note: Option<String>,
}

impl From<&Entry> for EntryRecord {
    fn from(e: &Entry) -> Self {
        let mut other = e.data.clone();
        // The columns' names: `node` is in every entry's data, and no
        // writer stores the others, which would print twice beside the
        // columns (Terra's review of bfc039d0, nit).
        for column in COLUMNS {
            other.shift_remove(*column);
        }
        let rest = &mut other;
        EntryRecord {
            node: e.node.id.clone(),
            id: e.id.clone(),
            kind: e.kind.as_str().to_string(),
            state: e.state.clone(),
            severity: e.severity.clone(),
            slot: e.slot.clone(),
            blocking: e.blocking,
            title: e.title.clone(),
            text: e.text.clone(),
            opened: take(rest, "opened"),
            anchor: take(rest, "anchor"),
            requirement: take(rest, "requirement"),
            asked_of: take(rest, "asked_of"),
            refs: take(rest, "refs"),
            range: take(rest, "range"),
            reviewers: take(rest, "reviewers"),
            completed: take(rest, "completed"),
            request: take(rest, "request"),
            anchored: take(rest, "anchored"),
            answer: take(rest, "answer"),
            answered_by: take(rest, "answered_by"),
            fix: take(rest, "fix"),
            fix_commits: take(rest, "fix_commits"),
            fixers: take(rest, "fixers"),
            superseded_commits: take(rest, "superseded_commits"),
            verified: take(rest, "verified"),
            review_receipt: take(rest, "review_receipt"),
            duplicate_of: take(rest, "duplicate_of"),
            superseded_by: take(rest, "superseded_by"),
            supersedes: take(rest, "supersedes"),
            moved_to: take(rest, "moved_to"),
            waives: take(rest, "waives"),
            graph_folder: take(rest, "graph_folder"),
            notified: take(rest, "notified"),
            other,
            opened_ms: e.opened_ms,
            updated_ms: e.updated_ms,
        }
    }
}

/// The keys [`EntryRecord`] prints from the entry's columns.
const COLUMNS: &[&str] = &[
    "node",
    "id",
    "kind",
    "state",
    "severity",
    "slot",
    "blocking",
    "title",
    "text",
    "opened_ms",
    "updated_ms",
];

/// `key`'s value as `T`, taken out of `rest`, when it reads back as
/// exactly the stored value; otherwise it stays in `rest`.
fn take<T: Serialize + DeserializeOwned>(rest: &mut Map<String, Value>, key: &str) -> Option<T> {
    let stored = rest.get(key)?;
    let typed = T::deserialize(stored).ok()?;
    if serde_json::to_value(&typed).ok().as_ref() != Some(stored) {
        return None;
    }
    rest.shift_remove(key);
    Some(typed)
}

impl From<&entries::Event> for EntryEvent {
    fn from(e: &entries::Event) -> Self {
        EntryEvent {
            seq: e.seq,
            op: e.op.clone(),
            from: e.from.clone(),
            to: e.to.clone(),
            by: e.by.clone(),
            at_ms: e.at_ms,
            sha: e.sha.clone(),
            message: e.message.clone(),
            turn: e.turn.clone(),
            note: e.note.clone(),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GateData {
    pub node: String,
    pub at: String,
    pub ok: bool,
    /// `S`: the node's highest ledger event seq when it was read.
    pub upto: u64,
    pub failures: Vec<Value>,
    pub warnings: Vec<Value>,
}

/// The graph and repository behind entry ops.
pub(crate) struct RepoEnv<'a> {
    repo: &'a Repo,
    common: PathBuf,
    store: Option<&'a Store>,
    /// The launcher's folder (`<common>/mdkg/run`): saved specs and D6
    /// turn records.
    run_dir: PathBuf,
    /// The graph's id, which D6 records are keyed on, or why it can't be
    /// read: a running turn can't be ruled out then (Terra's code review
    /// #6278, finding 6).
    graph: Result<uuid::Uuid, String>,
    /// The graph lock, shared, held from before the op's store write
    /// transaction until this is dropped after it commits; none for an env
    /// that reads only commits and trees ([`RepoEnv::git_only`]), which
    /// then reads no class and no node folder.
    lock: Option<GraphReadLock>,
    /// Git answers read through this env, by their immutable inputs.
    memo: GitMemo,
    /// The graph commit this env's class and folder reads use, pinned on
    /// first use for its life ([`graph_commit`]), or why there's none.
    tip: std::cell::OnceCell<Result<String, String>>,
}

/// The git answers a [`RepoEnv`] has read, keyed by inputs git answers the
/// same whatever moves: full commit and object ids, and paths. `done`
/// evaluates the gate once at the commit it settles before the settling
/// transaction opens ([`Settling`]'s `warm`), and the evaluation inside
/// the transaction, which holds the coordinator's write lock while every
/// other writer waits on `BUSY_WAIT`, reads these answers instead of
/// spawning git again (Terra's code review #10357, minor 2). Only entries
/// read in the transaction can differ, so a miss there is rare. Refs (the
/// main ref, the graph ref) and abbreviated ids are never kept; neither
/// are errors.
#[derive(Default)]
struct GitMemo {
    contains: Memo<(String, String), bool>,
    heads: Memo<(Vec<String>, String), Vec<String>>,
    changed: Memo<(String, String), std::collections::BTreeSet<String>>,
    commit_paths: Memo<String, (std::collections::BTreeSet<String>, Option<String>)>,
    entries: Memo<(String, String), Option<String>>,
    blobs: Memo<String, Vec<u8>>,
}

struct Memo<K, V>(std::cell::RefCell<std::collections::HashMap<K, V>>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(std::cell::RefCell::new(std::collections::HashMap::new()))
    }
}

impl<K: Eq + std::hash::Hash, V: Clone> Memo<K, V> {
    /// `key`'s answer, read once: `None` keeps nothing.
    fn read(
        &self,
        key: Option<K>,
        read: impl FnOnce() -> Result<V, EntryError>,
    ) -> Result<V, EntryError> {
        if let Some(k) = &key
            && let Some(v) = self.0.borrow().get(k)
        {
            return Ok(v.clone());
        }
        let v = read()?;
        if let Some(k) = key {
            self.0.borrow_mut().insert(k, v.clone());
        }
        Ok(v)
    }
}

/// Whether each of `ids` is a full commit or object id (40 or 64
/// lowercase hex digits), which no ref update can make name another.
fn full_ids(ids: &[&str]) -> bool {
    ids.iter().all(|id| {
        matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The graph lock, shared, for reading agent classes from the graph ref
/// (A27 rule 4, "When it's judged"; Terra's code review #6278, finding 3,
/// with the orchestrator's call): the OS lock g-core's ref writers hold
/// exclusive while they publish (`<common>/mdkg/graph/lock`, its
/// `refwrite::GraphLock`), taken before the op's store write transaction
/// and held until it commits (G22: the graph lock first, then the store).
/// So no definition can be published between the class read and the
/// op's commit. g-core's own lock, `GraphLock::shared(common,
/// LockMode::Local)`, since graph.ref merged (the stub's swap, #6520
/// option A): its file checks, and its refusal of a lock on a network
/// filesystem, are g-core's.
pub(crate) struct GraphReadLock {
    _graph: mdkg_graph::refwrite::GraphLock,
    /// `<common>/mdkg/graph-worktree.lock`, shared, which today's graph
    /// writers (`agents edit` and the graph checkout's writes) hold
    /// exclusive: taken after the graph lock, in that fixed order, until
    /// g-writers moves them to the graph lock (the orchestrator on Terra's
    /// #6919, call 1).
    _writers: std::fs::File,
}

/// How many commits back from `--at` the final round's heads are looked
/// for: the walk limit's size.
const HISTORY_LIMIT: usize = 10_000;

/// How long an entry read waits for the graph writers' lock.
const WRITERS_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

impl GraphReadLock {
    pub(crate) fn shared(common: &Path) -> Result<Self, GraphError> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let fail = |e: std::io::Error| {
            GraphError::failure(Code::EntryGit, format!("can't take the graph lock: {e}"))
        };
        // Held for the env's whole life: nothing holding a `RepoEnv` may
        // take the exclusive graph lock in this process, which would wait
        // on this one (a second open can't upgrade it; Opus's #6834,
        // minor 2). It also makes `<common>/mdkg`, private, for the
        // writers' lock below.
        let graph =
            mdkg_graph::refwrite::GraphLock::shared(common, mdkg_graph::refwrite::LockMode::Local)
                .map_err(|e| fail(std::io::Error::other(e.to_string())))?;
        let writers = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(common.join("mdkg").join("graph-worktree.lock"))
            .map_err(fail)?;
        let meta = writers.metadata().map_err(fail)?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(fail(std::io::Error::other(
                "the graph writers' lock must be a regular file with one link",
            )));
        }
        // Bounded like the writers' own wait on it (60 s, polled every
        // 50 ms), so a stuck writer, or this process's own exclusive
        // guard, refuses with a hint instead of hanging (Opus's code
        // review #7552, minor 1).
        let started = std::time::Instant::now();
        loop {
            match writers.try_lock_shared() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => (),
                Err(std::fs::TryLockError::Error(e)) => return Err(fail(e)),
            }
            if started.elapsed() >= WRITERS_WAIT {
                return Err(GraphError::failure(
                    Code::EntryGit,
                    format!(
                        "graph checkout busy: timed out waiting for {}",
                        common.join("mdkg/graph-worktree.lock").display()
                    ),
                )
                .with_hint(
                    "Let the active graph writer finish, then retry. Do not remove the lock file.",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(Self {
            _graph: graph,
            _writers: writers,
        })
    }
}

impl std::fmt::Debug for RepoEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoEnv")
            .field("common", &self.common)
            .finish_non_exhaustive()
    }
}

impl<'a> RepoEnv<'a> {
    pub(crate) fn new(repo: &'a Repo) -> Result<Self, GraphError> {
        let common = git::common_dir(&repo.top).map_err(|e| {
            GraphError::failure(
                Code::EntryGit,
                format!("can't find the git common dir: {e}"),
            )
        })?;
        let run_dir = common.join("mdkg").join("run");
        let graph = match mdkg_graph::config::load(&repo.root) {
            Ok(c) => c
                .graph_id
                .ok_or_else(|| "the graph has no graph_id".to_string())
                .and_then(|id| {
                    uuid::Uuid::parse_str(&id.to_string()).map_err(|e| format!("graph_id: {e}"))
                }),
            Err(e) => Err(format!("its config can't be read: {e}")),
        };
        // Before any store transaction this env takes part in.
        let lock = Some(GraphReadLock::shared(&common)?);
        Ok(Self {
            repo,
            common,
            store: None,
            run_dir,
            graph,
            lock,
            memo: GitMemo::default(),
            tip: std::cell::OnceCell::new(),
        })
    }

    /// An env for the gate alone, which reads only commits, trees and
    /// blobs in the common directory, never the graph ref's classes or
    /// node folders: it takes no graph lock, so `done` run by a graph
    /// writer that holds the writers' lock never waits on itself. Its
    /// class and folder reads refuse.
    pub(crate) fn git_only(repo: &'a Repo) -> Result<Self, GraphError> {
        let common = git::common_dir(&repo.top).map_err(|e| {
            GraphError::failure(
                Code::EntryGit,
                format!("can't find the git common dir: {e}"),
            )
        })?;
        Ok(Self {
            repo,
            run_dir: common.join("mdkg").join("run"),
            common,
            store: None,
            graph: Err("the gate's env reads no graph".into()),
            lock: None,
            memo: GitMemo::default(),
            tip: std::cell::OnceCell::new(),
        })
    }

    /// The graph commit [`graph_commit`] pins, read once: the graph lock
    /// this env holds keeps a ref publish from moving it meanwhile. With
    /// none, classes and node folders refuse ([`NO_GRAPH`]).
    fn graph_tip(&self) -> Result<String, EntryError> {
        self.tip
            .get_or_init(|| graph_commit(self.repo)?.ok_or_else(|| NO_GRAPH.to_string()))
            .clone()
            .map_err(unclassed)
    }

    fn held(&self) -> Result<&GraphReadLock, EntryError> {
        self.lock
            .as_ref()
            .ok_or_else(|| unclassed("the gate's env reads no classes or node folders"))
    }
}

fn git_error(e: impl std::fmt::Display) -> EntryError {
    EntryError::Git(e.to_string())
}

impl Env for RepoEnv<'_> {
    fn node(&self, handle: &str) -> Result<NodeLink, EntryError> {
        let node = crate::write::resolve_node(self.repo, handle)
            .map_err(|_| EntryError::NodeNotFound(handle.into()))?;
        NodeLink::of(&node.id).map_err(|_| EntryError::NodeNotFound(handle.into()))
    }

    /// Resolves object ids only, never refs: a branch or tag named like a
    /// hex prefix (`deadbeef`) is no SHA (Terra's re-review of
    /// 070dc919..0bf4a12a, finding 5). The prefix's objects are listed
    /// with `--disambiguate`; exactly one of them must be a commit, or a
    /// tag that peels to one.
    fn commit(&self, sha: &str) -> Result<String, EntryError> {
        let unknown = || EntryError::UnknownCommit(sha.into());
        let hex = (4..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            return Err(unknown());
        }
        let prefix = sha.to_ascii_lowercase();
        let listed = git::common_command(&self.common)
            .arg("rev-parse")
            .arg(format!("--disambiguate={prefix}"))
            .run(None)
            .map_err(git_error)?;
        if !listed.status.success() {
            return Err(unknown());
        }
        let mut found: Vec<String> = Vec::new();
        for oid in String::from_utf8_lossy(&listed.stdout).lines() {
            let oid = oid.trim();
            if oid.len() < 40 || !oid.starts_with(&prefix) {
                continue;
            }
            // `<full oid>^{commit}` names that object alone, so no ref can
            // stand in for it; a blob or tree doesn't peel and is skipped.
            let peeled = git::common_command(&self.common)
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("{oid}^{{commit}}"))
                .run(None)
                .map_err(git_error)?;
            let commit = String::from_utf8_lossy(&peeled.stdout).trim().to_string();
            if peeled.status.success() && commit.len() >= 40 && !found.contains(&commit) {
                found.push(commit);
            }
        }
        match found.as_slice() {
            [one] => Ok(one.clone()),
            _ => Err(unknown()),
        }
    }

    fn contains(&self, ancestor: &str, head: &str) -> Result<bool, EntryError> {
        self.memo.contains.read(
            full_ids(&[ancestor, head]).then(|| (ancestor.to_string(), head.to_string())),
            || git::is_ancestor_in_common(&self.common, ancestor, head).map_err(git_error),
        )
    }

    /// One `rev-list` of `at`'s history, then one `merge-base
    /// --independent` over the heads it holds (Terra's code review #9504,
    /// major 5).
    fn latest_heads(&self, heads: &[String], at: &str) -> Result<Vec<String>, EntryError> {
        self.memo.heads.read(
            (full_ids(&[at]) && heads.iter().all(|h| full_ids(&[h])))
                .then(|| (heads.to_vec(), at.to_string())),
            || {
                let wanted: std::collections::BTreeSet<String> =
                    heads.iter().map(|h| h.to_ascii_lowercase()).collect();
                if wanted.is_empty() {
                    return Ok(Vec::new());
                }
                let at = object_at(&self.common, &format!("{at}^{{commit}}"))?
                    .ok_or_else(|| EntryError::UnknownCommit(at.into()))?;
                // `at` itself a head: every other head in its history is its
                // ancestor, so it alone is latest, with no history read (Terra's
                // #10220, major 6).
                if wanted.contains(&at) {
                    return Ok(vec![at]);
                }
                // Bounded like the walk (Opus #10156, 2): a head further back
                // than that from `at` is in no history here, so it ends no walk,
                // and the gate fails closed with the hint to review the gap.
                let limit = format!("--max-count={HISTORY_LIMIT}");
                let listed =
                    git_read(&self.common, &["rev-list", &limit, "--end-of-options", &at])?
                        .ok_or_else(|| git_error(format!("can't list {at}'s history")))?;
                let mut held: Vec<String> = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for line in String::from_utf8_lossy(&listed).lines() {
                    if wanted.contains(line) && seen.insert(line) {
                        held.push(line.to_string());
                    }
                }
                if held.len() <= 1 {
                    return Ok(held);
                }
                let out = git::common_command(&self.common)
                    .args(["merge-base", "--independent"])
                    .args(&held)
                    .run(None)
                    .map_err(git_error)?;
                if !out.status.success() {
                    return Err(git_error("can't order the review rounds' heads"));
                }
                Ok(String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect())
            },
        )
    }

    fn holds(&self, actor: &str, role: &str) -> bool {
        let Some(store) = self.store else {
            return false;
        };
        let now = crate::coord::now_ms();
        store.coordinator().is_ok_and(|state| {
            state
                .holders(&Scope::new(role))
                .any(|l| l.owner == actor && l.deadline > now)
        })
    }

    /// Read from the graph ref's objects each time it's called, which the
    /// store does inside the op's write transaction (A27 rule 4, "When
    /// it's judged"). A person needs no definitions.
    fn definitions(&self, actor: &str) -> Result<Option<(Class, Vec<String>)>, EntryError> {
        if is_person(actor) {
            return Ok(Some((Class::Person, Vec::new())));
        }
        let tip = self.graph_tip()?;
        let agents = committed_agents(self.held()?, &self.common, &self.repo.prefix, &tip)?;
        let subordinates = Class::subordinates(actor, &agents);
        let relied: Vec<&str> = std::iter::once(actor)
            .chain(subordinates.iter().map(String::as_str))
            .collect();
        let mismatched = spec_mismatches(&self.run_dir, &agents, &relied)?;
        if !mismatched.is_empty() {
            return Err(unclassed(format!(
                "an agent this actor's class relies on runs with another role or reports_to                  than the graph ref commits ({})",
                mismatched.join(", ")
            )));
        }
        Ok(Some((Class::of(actor, &agents), subordinates)))
    }

    /// The main ref is [`anchoring_main_ref`]'s, from config only; never
    /// `$MDKG_MAIN_REF`, which the launcher exports into every agent's
    /// environment and which moves only `main_ref_base` (Opus's code
    /// review #6482, finding 1; graph.ref C4b).
    fn main_tips(&self) -> Result<Option<Vec<String>>, EntryError> {
        let Some(full) = anchoring_main_ref(&self.repo.top, &self.common)? else {
            return Ok(None);
        };
        // Only a proven absence is no main ref (the orchestrator on Terra's
        // #6919, call 4): `show-ref --verify` on the validated name says
        // absent; a ref that exists must then peel to a commit.
        let Some(tip) = existing_commit(&self.common, &full)? else {
            return Ok(None);
        };
        let mut tips = vec![tip];
        // Main's upstream from the repository's own config file only, never
        // `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` or a worktree's config
        // (the orchestrator on Terra's #6919, call 3).
        // A remote-tracking main has no upstream of its own.
        if let Some(branch) = full.strip_prefix("refs/heads/")
            && let Some(upstream) = configured_upstream(&self.common, branch)?
            && let Some(up) = existing_commit(&self.common, &upstream)?
            && !tips.contains(&up)
        {
            tips.push(up);
        }
        Ok(Some(tips))
    }

    fn changed_paths(
        &self,
        base: &str,
        head: &str,
    ) -> Result<std::collections::BTreeSet<String>, EntryError> {
        self.memo.changed.read(
            full_ids(&[base, head]).then(|| (base.to_string(), head.to_string())),
            || {
                let range = format!("{base}..{head}");
                let mut out = nul_paths(git_read(
                    &self.common,
                    // Merges against their first parent only (the orchestrator on
                    // Terra's #6548); no signature output in the path stream, and
                    // gitlink changes whatever `diff.ignoreSubmodules` says
                    // (Terra's #6548).
                    &[
                        "log",
                        "--no-show-signature",
                        "--diff-merges=first-parent",
                        "--ignore-submodules=none",
                        "--no-renames",
                        "--name-only",
                        "-z",
                        "--format=",
                        &range,
                    ],
                )?)?;
                out.extend(nul_paths(git_read(
                    &self.common,
                    &[
                        "diff",
                        "--ignore-submodules=none",
                        "--name-only",
                        "--no-renames",
                        "-z",
                        base,
                        head,
                    ],
                )?)?);
                Ok(out)
            },
        )
    }

    fn commit_paths(
        &self,
        c: &str,
    ) -> Result<(std::collections::BTreeSet<String>, Option<String>), EntryError> {
        self.memo
            .commit_paths
            .read(full_ids(&[c]).then(|| c.to_string()), || {
                // Against its first parent only, as A27 rule 3 says: a merge's
                // other parents bring in paths it didn't change (Opus's code review
                // #6482, finding 2).
                let parent = object_at(&self.common, &format!("{c}^1"))?;
                let mut args = vec![
                    "diff-tree",
                    "--ignore-submodules=none",
                    "-r",
                    "--no-commit-id",
                    "--no-renames",
                    "--name-only",
                    "-z",
                ];
                let first;
                match &parent {
                    Some(p) => {
                        first = p.clone();
                        args.extend([first.as_str(), c]);
                    }
                    None => args.extend(["--root", c]),
                }
                let paths = nul_paths(git_read(&self.common, &args)?)?;
                Ok((paths, parent))
            })
    }

    fn tree_entry(&self, at: &str, path: &str) -> Result<Option<String>, EntryError> {
        self.memo.entries.read(
            full_ids(&[at]).then(|| (at.to_string(), path.to_string())),
            || {
                let out = git_read(
                    &self.common,
                    &[
                        "--literal-pathspecs",
                        "ls-tree",
                        "-z",
                        "--full-tree",
                        at,
                        "--",
                        path,
                    ],
                )?
                .ok_or_else(|| git_error(format!("can't read the tree of {at}")))?;
                // The whole record: mode, type, object id and the name git
                // returned, so a different name counts as a difference.
                Ok(out
                    .split(|b| *b == 0)
                    .find(|r| !r.is_empty())
                    .map(|r| String::from_utf8_lossy(r).into_owned()))
            },
        )
    }

    /// A blob's bytes from the common directory, capped before they're
    /// read; anything but a blob refuses.
    fn blob(&self, oid: &str) -> Result<Vec<u8>, EntryError> {
        self.memo
            .blobs
            .read(full_ids(&[oid]).then(|| oid.to_string()), || {
                const CAP: usize = 4 << 20;
                let kind = git_read(&self.common, &["cat-file", "-t", oid])?.unwrap_or_default();
                if String::from_utf8_lossy(&kind).trim() != "blob" {
                    return Err(git_error(format!("{oid} isn't a file")));
                }
                let size = git_read(&self.common, &["cat-file", "-s", oid])?
                    .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<usize>().ok())
                    .ok_or_else(|| git_error(format!("can't read {oid}'s size")))?;
                if size > CAP {
                    return Err(git_error(format!("{oid} is too large to compare")));
                }
                git_read(&self.common, &["cat-file", "blob", oid])?
                    .ok_or_else(|| git_error(format!("can't read {oid}")))
            })
    }

    /// A process with a live turn mark is in a turn whatever actor it
    /// names (`--as` or `MDKG_ACTOR`): a turn can't act as its coordinator
    /// through the CLI or MCP (Terra's code review #6278, finding 1, with
    /// the orchestrator's call).
    fn in_turn(&self, actor: &str) -> Result<bool, EntryError> {
        if std::env::var_os("MDKG_RUN_TURN").is_some_and(|v| !v.is_empty()) {
            return Ok(true);
        }
        let named = if actor.starts_with("human:") {
            match std::env::var("MDKG_ACTOR").ok().filter(|a| !a.is_empty()) {
                Some(a) => a,
                None => return Ok(false),
            }
        } else {
            actor.to_string()
        };
        let graph = self.graph.as_ref().map_err(|why| {
            unclassed(format!(
                "the graph's id can't be read ({why}), so a running turn can't be ruled out"
            ))
        })?;
        turn_running(&self.run_dir, *graph, &named)
    }

    /// From the graph ref, never the invoking checkout (the orchestrator
    /// on Terra's #6919, call 2): the folder of the one node file the
    /// graph ref's tip holds with this id, among the node folders its
    /// committed config names. A local node (in the store's work folder,
    /// which no git ref holds) is keyed in this graph's own folder, as
    /// [`Repo::node_key`] keys it; any other node the tip doesn't hold
    /// has no lease on it, so no worker and no coordinator.
    fn node_folder(&self, node: &NodeLink) -> Result<NodeFolder, EntryError> {
        let tip = self.graph_tip()?;
        if let Some(folder) =
            graph_ref_folder(self.held()?, &self.common, &self.repo.prefix, &tip, node)?
        {
            return Ok(NodeFolder::In(folder));
        }
        let local = crate::write::resolve_node(self.repo, &node.id)
            .ok()
            .is_some_and(|found| self.repo.store_scope(&found.path).is_some());
        Ok(if local {
            NodeFolder::In(self.repo.graph_scope().as_str().to_lowercase())
        } else {
            NodeFolder::Nowhere
        })
    }
}

/// The node folder (lowercase, as lease keys spell it) of the one node the
/// graph ref's tip holds with `node`'s UUID, among the graph's document
/// folders as the indexer discovers them: `core`, `design` and `work`
/// under each workspace folder its committed config enables
/// (`<prefix>.mdkg` alone without one). A candidate is a node only by the
/// indexer's own rules: its file rule (`discovery::is_node_document`: a
/// `.md` file, never a core page or one past the depth limit) and its
/// parser with the graph's options (`index::parse_options`), with an id
/// naming that UUID (a retype keeps it); a match anywhere else,
/// templates, fixtures, skills or an archive under `.mdkg` included, is
/// never a node and confers nothing (Terra's code reviews #8466, #8791
/// and #9504, the blockers). The folder returned is the lease-key rule's
/// ([`crate::coord::innermost_node_folder`]), the one lease creation
/// keys the file in (Terra's #10220, blocker 1). A file over the graph's size limit is
/// skipped, as the indexer refuses it (Opus's #8780). `None` when the tip
/// holds no such node, or no format 0.7 config; two, or any git failure,
/// refuse.
fn graph_ref_folder(
    _held: &GraphReadLock,
    common: &Path,
    prefix: &str,
    tip: &str,
    node: &NodeLink,
) -> Result<Option<String>, EntryError> {
    let uuid = node.uuid.to_ascii_lowercase();
    if uuid.is_empty() || !uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(unclassed(format!(
            "{:?} isn't a node UUID the graph ref can be searched for",
            node.uuid
        )));
    }
    let own = format!("{prefix}.mdkg");
    // A graph ref without a config holds no graph, and a graph in an
    // older format parses with templates read from a checkout: neither
    // names a node here (fail closed).
    let Some(config) = graph_ref_config(common, tip, prefix)? else {
        return Ok(None);
    };
    // Format 0.7's templates are its node schema, read from no folder.
    let schemas = mdkg_graph::templates::load(Path::new(""), &config)
        .map_err(|_| unclassed("can't load the node schema"))?
        .schemas;
    let opts = mdkg_graph::index::parse_options(&config, &schemas);
    let cap = usize::try_from(config.limits.max_file_bytes).unwrap_or(usize::MAX);
    // The enabled workspaces' folders discover the node, as the indexer
    // does; its lease folder is then the lease-key rule's over every
    // workspace's folder (Terra's #10220, blocker 1).
    let folders: Vec<String> = config
        .workspaces
        .iter()
        .filter(|w| w.enabled)
        .map(|w| format!("{prefix}{}", crate::coord::workspace_folder(w)))
        .collect();
    if folders.is_empty() {
        return Ok(None);
    }
    let lease_folders: Vec<Scope> = std::iter::once(own.clone())
        .chain(
            config
                .workspaces
                .iter()
                .map(|w| format!("{prefix}{}", crate::coord::workspace_folder(w))),
        )
        .map(Scope::new)
        .collect();
    // The node grammar allows space around the key; the value is the
    // typed id, which a retype changes but whose UUID it keeps.
    let pattern = format!("^[[:space:]]*id[[:space:]]*:.*{uuid}");
    let mut grep = git::common_command(common).args([
        "--literal-pathspecs",
        "grep",
        "-l",
        "-z",
        "-I",
        "-i",
        "-E",
        "-e",
        &pattern,
        tip,
        "--",
    ]);
    for f in &folders {
        for docs in mdkg_graph::discovery::DOC_FOLDERS {
            grep = grep.arg(format!("{f}/{docs}"));
        }
    }
    let out = grep.run(None).map_err(git_error)?;
    let listed = match out.status.code() {
        Some(0) => out.stdout,
        Some(1) => return Ok(None),
        _ => return Err(unclassed("can't search the graph ref for the node's file")),
    };
    let lead = format!("{tip}:");
    let mut found: Vec<(String, String)> = Vec::new();
    for hit in listed.split(|b| *b == 0).filter(|h| !h.is_empty()) {
        let hit = String::from_utf8(hit.to_vec())
            .map_err(|_| unclassed("a node file's path on the graph ref isn't UTF-8"))?;
        let path = hit
            .strip_prefix(&lead)
            .ok_or_else(|| unclassed("can't read the graph ref's search results"))?;
        // The indexer's own file rule, under a folder whose document it
        // is: never a `.MD` file, a core page or one past the depth limit
        // (Terra's code review #9504, blocker 2).
        let discovered = folders.iter().any(|f| {
            path.strip_prefix(f.as_str())
                .and_then(|r| r.strip_prefix('/'))
                .is_some_and(|rel| {
                    mdkg_graph::discovery::is_node_document(rel, config.limits.max_depth)
                })
        });
        if !discovered {
            continue;
        }
        // Its folder by the lease-key rule, the innermost node folder
        // holding the file, never the one that discovered it: with
        // workspaces nested in each other's folders the two differ, and
        // the gate would read another lease as the node's (Terra's
        // #10220, blocker 1).
        let Some(folder) = crate::coord::innermost_node_folder(&lease_folders, &Scope::new(path))
        else {
            continue;
        };
        let Some((mode, _, oid)) = tree_item(common, tip, path)? else {
            continue;
        };
        if mode != "100644" && mode != "100755" {
            continue;
        }
        let size = git_read(common, &["cat-file", "-s", &oid])?
            .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<usize>().ok())
            .ok_or_else(|| unclassed(format!("can't read {path}'s size on the graph ref")))?;
        if size > cap {
            continue;
        }
        let bytes = blob_capped(common, &oid, path, cap)?;
        // The indexer's own node rules: a document it would refuse, or one
        // bearing a v2 identity, is never a node.
        let text = String::from_utf8_lossy(&bytes);
        let named = mdkg_format::node::parse_node(&text, path, &opts)
            .ok()
            .filter(|n| n.identity.is_none())
            .and_then(|n| NodeLink::of(&n.id).ok())
            .is_some_and(|l| l.uuid.eq_ignore_ascii_case(&uuid));
        if named {
            found.push((path.to_string(), folder.as_str().to_lowercase()));
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [(_, folder)] => Ok(Some(folder.clone())),
        _ => Err(unclassed(format!(
            "the graph ref holds node {} in more than one file ({})",
            node.id,
            found
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The graph's committed config at the graph ref's commit `tip`
/// (`<prefix>.mdkg/config.json`), validated: `None` when the tip holds
/// none, or one in a format before 0.7; a link or folder there, a file
/// that doesn't validate, or any git failure, refuses.
fn graph_ref_config(
    common: &Path,
    tip: &str,
    prefix: &str,
) -> Result<Option<mdkg_graph::config::Config>, EntryError> {
    let Some((mode, _, oid)) = tree_item(common, tip, &format!("{prefix}.mdkg/config.json"))?
    else {
        return Ok(None);
    };
    if mode != "100644" && mode != "100755" {
        return Err(unclassed("config.json on the graph ref isn't a file"));
    }
    let config = mdkg_graph::config::from_snapshot(json_of(
        &blob(common, &oid, "config.json")?,
        "config.json",
    )?)
    .map_err(|_| unclassed("config.json on the graph ref doesn't validate"))?;
    Ok((config.format == mdkg_format::node::FormatVersion::V07).then_some(config))
}

/// The workspaces of the graph at `repo` as the graph ref commits them,
/// the lease-key input ([`Repo::node_key`]'s folders): the list the gate's
/// [`graph_ref_folder`] reads, so a checkout's own workspace edit,
/// committed or not, never splits a node's lease key from the folder the
/// gate reads its leases in (Terra's code review #10490, the major, with
/// the orchestrator's call; #6919 call 2), at the commit
/// [`graph_commit`] pins. `None` when no graph-ref config resolves: no
/// graph ref and no registered graph checkout, no format 0.7 config at
/// `<prefix>.mdkg` there, or a read that fails. Only then does lease
/// creation fall back to the checkout's config, a residual the edd names;
/// the gate confers no coordinator class in any of those cases (it finds
/// no node, or refuses).
///
/// Read without the graph lock: the pinned commit's objects never change,
/// so the read is one consistent snapshot. The lock orders a class read
/// before the op's commit; a lease key is a stored name, and the gate reads
/// the folders again under the lock in the op's own transaction. Taking it
/// here would also make every claim wait on a graph writer, or on its own
/// process's exclusive hold. The config is read once per commit, prefix
/// and repository in a process (a command opens its repository several
/// times).
pub(crate) fn graph_ref_workspaces(repo: &Repo) -> Option<Vec<mdkg_graph::config::Workspace>> {
    type Read = Option<Vec<mdkg_graph::config::Workspace>>;
    static READ: std::sync::Mutex<std::collections::BTreeMap<(PathBuf, String, String), Read>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let tip = graph_commit(repo).ok()??;
    let key = (repo.top.clone(), repo.prefix.clone(), tip);
    if let Some(read) = READ.lock().ok().and_then(|r| r.get(&key).cloned()) {
        return read;
    }
    let read = git::common_dir(&repo.top)
        .ok()
        .and_then(|common| graph_ref_config(&common, &key.2, &repo.prefix).ok())
        .flatten()
        .map(|c| c.workspaces);
    if let Ok(mut r) = READ.lock() {
        r.insert(key, read.clone());
    }
    read
}

/// P4's requirement-state reader for the graph at `root`, opened through
/// the repository's own checks (`Repo::open`: a nested repository git
/// skipped never reads the outer store; Terra's code review #8445), then
/// read-only. Any failure is the reader's typed `coord.store_failed`.
// P4's `req list` and `pack` call it once they merge.
#[allow(clippy::result_large_err, dead_code)]
pub(crate) fn requirement_reader(
    root: &Path,
) -> Result<entries::RequirementStateReader, mdkg_format::Diagnostic> {
    let unavailable = |e: &dyn std::fmt::Display| {
        mdkg_format::Diagnostic::error(
            Code::CoordStoreFailed,
            format!("the coordinator store is unavailable, so requirement states are unknown: {e}"),
        )
    };
    let repo = Repo::open(root).map_err(|e| unavailable(&e))?;
    let path = git::store_path(&repo.top).map_err(|e| unavailable(&e))?;
    crate::coord::unreachable_store(&path).map_err(|e| unavailable(&e))?;
    entries::RequirementStateReader::open_at(&path)
}

/// Why classes and node folders fail closed in checkout mode with no graph
/// checkout recorded.
pub(crate) const NO_GRAPH: &str = "no graph ref or registered graph checkout";

/// The graph commit agent classes, node folders and lease keys are read at
/// (A27 rule 4), pinned through g-core's authority (graph.ref edd row 29):
/// in ref mode (`mdkg.graph.ref`) the graph ref's tip, and in checkout mode
/// the HEAD of the registered graph checkout (`mdkg.graph.<id>.worktree`),
/// with g-core's identity and root checks. Read as git objects from the
/// common directory, never a working tree and never main. `Ok(None)` in
/// checkout mode with no checkout record, where g-core would read the
/// invoking checkout's HEAD: an agent could grant itself a class on its
/// own branch, so classes and node folders fail closed instead ([`NO_GRAPH`])
/// and lease keys use the checkout's workspaces (the orchestrator on
/// #10737, option 1). Any other failure is the error, as its message.
pub(crate) fn graph_commit(repo: &Repo) -> Result<Option<String>, String> {
    // Neither a graph ref nor a checkout record: settled without g-core's
    // lookup, which checks the worktree again through its own git.
    if !graph_recorded(repo)? {
        return Ok(None);
    }
    let found = mdkg_graph::graphref::authority_in(&repo.top, repo.prefix.trim_end_matches('/'))
        .map_err(|e| format!("the graph's authority can't be read: {e}"))?;
    // g-core names the invoking HEAD this way only with no checkout record.
    if found.graph_ref.is_none() && found.source.starts_with("checkout:HEAD@") {
        return Ok(None);
    }
    Ok(Some(found.commit))
}

/// Whether the common config sets a graph ref (`mdkg.graph.ref`) or
/// records a graph checkout (`mdkg.graph.<id>.worktree`), read from that
/// file alone, without includes, through the worktree's git, whose checks
/// `Repo::open` made.
fn graph_recorded(repo: &Repo) -> Result<bool, String> {
    let common = git::common_dir(&repo.top).map_err(|e| e.to_string())?;
    let out = git::command(&repo.top)
        .args(["config", "--no-includes", "--file"])
        .arg(common.join("config"))
        .args([
            "--name-only",
            "--get-regexp",
            r"^mdkg\.graph\.(ref|.*\.worktree)$",
        ])
        .run(None)
        .map_err(|e| e.to_string())?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("the clone's graph checkout records can't be read".into()),
    }
}

/// The most a committed definition or config may be.
const DEFINITION_CAP: usize = 1 << 20;

/// Whether the ledger classes `actor` as a person: the exact lowercase
/// `human:` and a well-formed mailbox name after it, the test main.rs
/// admits a person by (Terra's #11173 and #11205). Every class decision
/// here makes it itself, so none depends on main.rs's check running first
/// (Opus's final pass #11368, finding 1). Any other spelling is no person.
pub(crate) fn is_person(actor: &str) -> bool {
    mdkg_coord_sqlite::entries::is_person(actor)
}

fn unclassed(why: impl Into<String>) -> EntryError {
    EntryError::Unclassed(why.into())
}

/// `git <args>` against the common directory: its stdout, or `None` when
/// git says no (an absent object or path).
fn git_read(common: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, EntryError> {
    let out = git::common_command(common)
        .args(args)
        .run(None)
        .map_err(git_error)?;
    Ok(out.status.success().then_some(out.stdout))
}

/// The object `rev` names, if any: `None` only when git says it names
/// none (`rev-parse --verify --quiet` exits 1); git failing any other way
/// (killed, a broken repository) is an error, never an absent object
/// (Terra's code review #6278, finding 5).
fn object_at(common: &Path, rev: &str) -> Result<Option<String>, EntryError> {
    let out = git::common_command(common)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .run(None)
        .map_err(git_error)?;
    match out.status.code() {
        Some(0) => {
            let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if oid.is_empty() {
                return Err(git_error(format!("git named no object for {rev}")));
            }
            Ok(Some(oid))
        }
        Some(1) => Ok(None),
        _ => Err(git_error(format!("git couldn't read {rev}"))),
    }
}

/// `path`'s entry in commit `tip`'s tree as `(mode, type, oid)`: `None`
/// only when git lists the tree and the path isn't in it; any failure of
/// git is an error (Terra's code review #6278, finding 5).
fn tree_item(
    common: &Path,
    tip: &str,
    path: &str,
) -> Result<Option<(String, String, String)>, EntryError> {
    let out = git_read(
        common,
        &[
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            "--full-tree",
            tip,
            "--",
            path,
        ],
    )?
    .ok_or_else(|| unclassed(format!("can't list {path} on the graph ref")))?;
    let Some(record) = out.split(|b| *b == 0).find(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let record = String::from_utf8_lossy(record);
    let parsed = record.split_once('\t').and_then(|(meta, name)| {
        let mut meta = meta.split(' ');
        match (meta.next(), meta.next(), meta.next(), name == path) {
            (Some(mode), Some(kind), Some(oid), true) => {
                Some((mode.to_string(), kind.to_string(), oid.to_string()))
            }
            _ => None,
        }
    });
    parsed
        .map(Some)
        .ok_or_else(|| unclassed(format!("can't read {path}'s entry on the graph ref")))
}

/// The commit the ref `full` points at: `None` only when `show-ref
/// --verify` proves the ref absent (exit 1); a ref that exists and doesn't
/// peel to a commit, or any other failure, is an error.
fn existing_commit(common: &Path, full: &str) -> Result<Option<String>, EntryError> {
    let out = git::common_command(common)
        .args(["show-ref", "--verify", "--quiet", full])
        .run(None)
        .map_err(git_error)?;
    match out.status.code() {
        Some(0) => object_at(common, &format!("{full}^{{commit}}"))?
            .map(Some)
            .ok_or_else(|| git_error(format!("{full} doesn't name a commit"))),
        Some(1) => Ok(None),
        _ => Err(git_error(format!("git couldn't read {full}"))),
    }
}

/// A value from the repository's own config file only.
fn own_config(common: &Path, key: &str) -> Result<Option<String>, EntryError> {
    let out = git::common_command(common)
        .args(["config", "--file"])
        .arg(common.join("config"))
        .args(["--get", key])
        .run(None)
        .map_err(git_error)?;
    match out.status.code() {
        Some(0) => Ok(
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|v| !v.is_empty()),
        ),
        Some(1) => Ok(None),
        _ => Err(git_error(format!("can't read {key}"))),
    }
}

/// `branch`'s upstream ref, as git maps it through the remote's fetch
/// refspecs (`for-each-ref --format=%(upstream)`), from the repository's
/// own config only, never a global or system file or an environment
/// override (Terra's code review #7598, finding 2, with the orchestrator's
/// call; #6919, call 3). `None` when it has none; read errors refuse.
fn configured_upstream(common: &Path, branch: &str) -> Result<Option<String>, EntryError> {
    let full = format!("refs/heads/{branch}");
    let out = git::common_command(common)
        .own_config_only()
        .args(["for-each-ref", "--format=%(refname)%00%(upstream)", &full])
        .run(None)
        .map_err(git_error)?;
    if !out.status.success() {
        return Err(git_error(format!("can't read {branch}'s upstream")));
    }
    let listed = String::from_utf8(out.stdout)
        .map_err(|_| git_error(format!("{branch}'s upstream isn't UTF-8")))?;
    let upstream = listed
        .lines()
        .filter_map(|l| l.split_once('\0'))
        .find(|(name, _)| *name == full)
        .map(|(_, up)| up.to_string())
        .filter(|up| !up.is_empty());
    let Some(upstream) = upstream else {
        return Ok(None);
    };
    check_literal_ref(common, &upstream)
        .map_err(|_| git_error(format!("{branch}'s upstream {upstream:?} isn't a ref name")))?;
    Ok(Some(upstream))
}

/// Whether `full` is a literal ref name, checked without expansion: no
/// `@{…}` or other revision syntax, which `check-ref-format --branch`
/// would expand (Terra's code review #7598, finding 3).
fn check_literal_ref(common: &Path, full: &str) -> Result<(), EntryError> {
    let bad = || git_error(format!("{full:?} isn't a literal ref name"));
    if !full.starts_with("refs/") || full.contains("@{") || full.contains("..") {
        return Err(bad());
    }
    let checked = git::common_command(common)
        .args(["check-ref-format", full])
        .run(None)
        .map_err(git_error)?;
    if checked.status.success() {
        Ok(())
    } else {
        Err(bad())
    }
}

/// The main ref anchoring reads (A27 rule 8), as a full ref, per
/// graph.ref's amendment to that rule (the orchestrator on #10737): in ref
/// mode (`mdkg.graph.ref` set) g-core's `resolve().main_ref`, the config
/// value only, `None` when it's unset (no code base); in checkout mode the
/// repository's own `mdkg.mainRef` (its common config file, never a
/// worktree's or the environment's), else `main`, normalized as g-core
/// does: a full name as is; a bare name a local branch when one exists,
/// else a remote-tracking ref when that does, else the local branch. A
/// remote-tracking ref is allowed. A value that isn't a local branch or a
/// remote-tracking ref is a bad config, refused, never an absent main ref
/// (the orchestrator on Terra's #6548 and #7598, finding 3).
fn anchoring_main_ref(top: &Path, common: &Path) -> Result<Option<String>, EntryError> {
    let full = if own_config(common, "mdkg.graph.ref")?.is_some() {
        let graph = mdkg_graph::graphref::resolve(top)
            .map_err(|e| git_error(format!("the graph ref can't be resolved: {e}")))?
            .ok_or_else(|| git_error("the graph ref can't be resolved"))?;
        match graph.main_ref {
            Some(full) => full,
            None => return Ok(None),
        }
    } else {
        let configured = own_config(common, "mdkg.mainRef")?.unwrap_or_default();
        let value = if configured.is_empty() {
            "main"
        } else {
            configured.as_str()
        };
        if value.starts_with('-') {
            return Err(git_error(format!(
                "mdkg.mainRef {configured:?} isn't a branch name"
            )));
        }
        if value.starts_with("refs/") {
            value.to_string()
        } else {
            let local = format!("refs/heads/{value}");
            let remote = format!("refs/remotes/{value}");
            check_literal_ref(common, &local).map_err(|_| {
                git_error(format!("mdkg.mainRef {configured:?} isn't a branch name"))
            })?;
            if existing_commit(common, &local)?.is_none()
                && existing_commit(common, &remote)?.is_some()
            {
                remote
            } else {
                local
            }
        }
    };
    let branch = full.starts_with("refs/heads/") || full.starts_with("refs/remotes/");
    if !branch || check_literal_ref(common, &full).is_err() {
        return Err(git_error(format!(
            "the main ref {full:?} isn't a local branch or a remote-tracking ref"
        )));
    }
    Ok(Some(full))
}

/// NUL-delimited paths from git's `-z` output; a command git refused
/// (an unknown commit) is an error, never an empty set.
fn nul_paths(out: Option<Vec<u8>>) -> Result<std::collections::BTreeSet<String>, EntryError> {
    let out = out.ok_or_else(|| git_error("can't list the changed paths"))?;
    // A path that isn't UTF-8 is refused, never compared under another
    // spelling (Terra's code review #6548).
    out.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            String::from_utf8(p.to_vec())
                .map_err(|_| git_error("a changed path isn't UTF-8, so it can't be checked"))
        })
        .collect()
}

/// A blob's bytes, capped before they're read (Terra's code review #6278,
/// finding 7); anything else (a tree, a link) refuses.
fn blob(common: &Path, oid: &str, what: &str) -> Result<Vec<u8>, EntryError> {
    blob_capped(common, oid, what, DEFINITION_CAP)
}

/// [`blob`] with its own cap: a node file takes the graph's index limit
/// (Terra's code review #8466, minor).
fn blob_capped(common: &Path, oid: &str, what: &str, cap: usize) -> Result<Vec<u8>, EntryError> {
    let kind = git_read(common, &["cat-file", "-t", oid])?.unwrap_or_default();
    if String::from_utf8_lossy(&kind).trim() != "blob" {
        return Err(unclassed(format!("{what} on the graph ref isn't a file")));
    }
    let size = git_read(common, &["cat-file", "-s", oid])?
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<usize>().ok())
        .ok_or_else(|| unclassed(format!("can't read {what}'s size on the graph ref")))?;
    if size > cap {
        return Err(unclassed(format!("{what} on the graph ref is too large")));
    }
    let bytes = git_read(common, &["cat-file", "blob", oid])?
        .ok_or_else(|| unclassed(format!("can't read {what} on the graph ref")))?;
    if bytes.len() > cap {
        return Err(unclassed(format!("{what} on the graph ref is too large")));
    }
    Ok(bytes)
}

fn json_of(bytes: &[u8], what: &str) -> Result<Value, EntryError> {
    serde_json::from_slice(bytes)
        .map_err(|_| unclassed(format!("{what} on the graph ref isn't JSON")))
}

/// Every agent's name, role and `reports_to` as the graph ref commits them
/// (A27 rule 4): each `.mdkg/agents/<name>.json` blob through the
/// launcher's [`Definition::parse`](mdkg_run::orchestration::agent::Definition::parse),
/// and each legacy inline agent of the committed `.mdkg/config.json`
/// without a file of its own (the launcher's precedence) through its
/// execution rules ([`check_execution`](mdkg_run::orchestration::agent::check_execution)).
/// Fails closed: a missing graph ref, a definition that doesn't validate,
/// a link or folder where a definition should be, or an agent without a
/// role refuses every class (Terra's code review #5973, findings 1-4).
pub(crate) fn committed_agents(
    _held: &GraphReadLock,
    common: &Path,
    prefix: &str,
    tip: &str,
) -> Result<Vec<AgentFacts>, EntryError> {
    use mdkg_run::orchestration::agent;
    let mut facts: Vec<AgentFacts> = Vec::new();
    if let Some((_, kind, dir)) = tree_item(common, tip, &format!("{prefix}.mdkg/agents"))? {
        if kind != "tree" {
            return Err(unclassed("the graph ref's .mdkg/agents isn't a folder"));
        }
        let listing = git_read(common, &["ls-tree", "-z", &dir])?
            .ok_or_else(|| unclassed("the graph ref's .mdkg/agents isn't a folder"))?;
        for item in listing.split(|b| *b == 0).filter(|i| !i.is_empty()) {
            let item = String::from_utf8_lossy(item);
            let Some((meta, file)) = item.split_once('\t') else {
                return Err(unclassed("can't list the graph ref's agent definitions"));
            };
            let Some(name) = file.strip_suffix(".json") else {
                continue;
            };
            let mut meta = meta.split(' ');
            let (mode, oid) = (meta.next().unwrap_or(""), meta.nth(1).unwrap_or(""));
            if mode != "100644" && mode != "100755" {
                return Err(unclassed(format!(
                    "agent {name}'s definition on the graph ref isn't a file"
                )));
            }
            let what = format!("agent {name}'s definition");
            let value = json_of(&blob(common, oid, &what)?, &what)?;
            let definition = agent::Definition::parse(name, value)
                .map_err(|_| unclassed(format!("{what} on the graph ref doesn't validate")))?;
            let role = definition
                .role
                .ok_or_else(|| unclassed(format!("agent {name} has no role on the graph ref")))?;
            facts.push(AgentFacts {
                name: name.to_string(),
                role: Some(role.as_str().to_string()),
                reports_to: Some(definition.reports_to),
            });
        }
    }
    if let Some((mode, _, oid)) = tree_item(common, tip, &format!("{prefix}.mdkg/config.json"))? {
        // A regular file, never a link whose target reads as JSON (Terra's
        // code review #6919, finding 8).
        if mode != "100644" && mode != "100755" {
            return Err(unclassed("config.json on the graph ref isn't a file"));
        }
        let config = json_of(&blob(common, &oid, "config.json")?, "config.json")?;
        // A config the launcher would reject grants no class (Terra's code
        // review #6278, finding 4).
        if !config.is_object() {
            return Err(unclassed("config.json on the graph ref isn't an object"));
        }
        let inline = match config.get("agents") {
            // `null` is no inline agents, as the launcher reads it (Terra's
            // code review #6919, minor 12).
            None | Some(Value::Null) => None,
            Some(Value::Object(agents)) => Some(agents),
            Some(_) => {
                return Err(unclassed(
                    "config.json's agents on the graph ref isn't an object",
                ));
            }
        };
        for (name, entry) in inline.into_iter().flatten() {
            if name == "defaults" || facts.iter().any(|f| &f.name == name) {
                continue;
            }
            let refuse = || {
                unclassed(format!(
                    "inline agent {name} on the graph ref doesn't validate"
                ))
            };
            agent::check_name(name).map_err(|_| refuse())?;
            agent::check_execution(&config, name).map_err(|_| refuse())?;
            let role = entry
                .get("role")
                .and_then(Value::as_str)
                .and_then(mdkg_run::Role::parse)
                .ok_or_else(refuse)?;
            let reports_to = match entry.get("reports_to") {
                None | Some(Value::Null) => "orchestrator".to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(refuse()),
            };
            mdkg_run::state::check_reports_to(name, &reports_to).map_err(|_| refuse())?;
            facts.push(AgentFacts {
                name: name.clone(),
                role: Some(role.as_str().to_string()),
                reports_to: Some(reports_to),
            });
        }
    }
    Ok(facts)
}

/// The agents among `names` whose saved launcher spec (`state::load`)
/// runs with another `role` or `reports_to` than `agents` commits, as
/// `<agent>.<field>`: an op relying on one is refused rather than classed
/// either way (A27 rule 4; #5973, finding 4).
fn spec_mismatches(
    run_dir: &Path,
    agents: &[AgentFacts],
    names: &[&str],
) -> Result<Vec<String>, EntryError> {
    let mut out = Vec::new();
    for name in names {
        let Some(committed) = agents.iter().find(|a| a.name == *name) else {
            continue;
        };
        let saved = mdkg_run::state::load(run_dir, name)
            .map_err(|_| unclassed(format!("agent {name}'s launcher state can't be read")))?;
        let Some(saved) = saved else {
            continue;
        };
        if committed.role.as_deref() != Some(saved.spec.role.as_str()) {
            out.push(format!("{name}.role"));
        }
        if committed.reports_to.as_deref() != Some(saved.spec.reports_to.as_str()) {
            out.push(format!("{name}.reports_to"));
        }
    }
    Ok(out)
}

/// Whether agent `name`'s D6 progress record holds a turn that started and
/// hasn't ended. A record that can't be read refuses (fails closed).
fn turn_running(run_dir: &Path, graph: uuid::Uuid, name: &str) -> Result<bool, EntryError> {
    if name.starts_with("human:") || mdkg_run::state::check_name(name).is_err() {
        return Ok(false);
    }
    match mdkg_run::supervise::progress::inspect(run_dir, graph, name) {
        Ok(record) => Ok(record.is_some_and(|r| r.current.is_some())),
        Err(_) => Err(unclassed(format!(
            "agent {name}'s turn record can't be read"
        ))),
    }
}

/// The `R<n>` ids in a node body's requirements, as P4's parser reads them
/// (A28), so the gate and `req list` agree: retired items left out, and
/// each id once.
pub(crate) fn requirement_ids(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in mdkg_graph::requirements::parse(body).requirements {
        if !r.retired && !out.contains(&r.id) {
            out.push(r.id);
        }
    }
    out
}

pub(crate) fn entry_failure(e: EntryError) -> GraphError {
    let message = e.to_string();
    let Some(code) = e.code().and_then(Code::parse) else {
        return match e {
            EntryError::Store(s) => store_error(s),
            _ => GraphError::failure(Code::EntryGit, message),
        };
    };
    match e {
        EntryError::NotFound(_) | EntryError::NodeNotFound(_) => {
            GraphError::not_found(code, message)
        }
        EntryError::Unclassed(why) if why == NO_GRAPH => GraphError::conflict(code, message)
            .with_hint(format!(
                "agent classes come from the graph ref, or a registered graph checkout, never \
                 this checkout's HEAD: run `{} install`, or act as a person (`--as human:<name>`) \
                 outside a turn",
                crate::BIN
            )),
        EntryError::Unclassed(_) => GraphError::conflict(code, message).with_hint(
            "classes come from the agent definitions committed on the graph ref (or the \
             registered graph checkout), as git objects: commit them there, keep each running \
             agent's launcher spec in step with them, and retry",
        ),
        // Refusals on the ledger's current state, never a misused command
        // (Terra's #11125: verdict_unbacked and fix_absent among them now
        // that their codes parse).
        // How to go on (e2e UX): who may act, as the class table says.
        EntryError::NotAllowed { .. } => GraphError::conflict(code, message).with_hint(format!(
            "a person acts with `--as human:<name>`, outside an agent's turn; an agent acts \
             with the class its committed definition gives it (`{} agents status`)",
            crate::BIN
        )),
        EntryError::BadTransition { .. }
        | EntryError::FixAfterRange { .. }
        | EntryError::NotCovered { .. }
        | EntryError::RangeMismatch { .. }
        | EntryError::VerdictUnbacked { .. }
        | EntryError::FixAbsent { .. }
        | EntryError::WalkLimit(_) => GraphError::conflict(code, message),
        EntryError::Git(_) => GraphError::failure(code, message),
        // A missing or bad field names where it's given first (e2e N5):
        // its flag, or its argument; a field no command takes, only the
        // op file's key (review of 6baee0b9).
        EntryError::BadOp(field) => {
            let key = crate::output::shown_path(&field);
            let give = match bad_op_flag(&field) {
                Some(Given::Flag(flag)) => format!("check `--{flag}` (`{key}` in an op file); "),
                Some(Given::Argument) => {
                    // The help's name for the position (review of a7a7c7c3).
                    format!("check the [ENTRY] argument (`{key}` in an op file); ")
                }
                None if field == "op" => String::new(),
                None => format!("check `{key}` in the op file; "),
            };
            GraphError::usage(code, message).with_hint(format!(
                "{give}`{} entry <op> --help` lists each op's flags; `--kind` takes question, \
                 decision, finding or issue",
                crate::BIN
            ))
        }
        _ => GraphError::usage(code, message),
    }
}

/// Where a command takes an op field.
#[derive(Debug, PartialEq)]
enum Given {
    Flag(&'static str),
    Argument,
}

/// The `entry` flag or argument that gives an op field, if any: the op
/// file's keys mostly match the flags, a few are renamed, and `entry`,
/// `round` and `req` are positional (review of 6baee0b9).
fn bad_op_flag(field: &str) -> Option<Given> {
    Some(match field {
        "refs" => Given::Flag("ref"),
        "commits" => Given::Flag("commit"),
        "asked_of" => Given::Flag("asked-of"),
        "kind" => Given::Flag("kind"),
        "title" => Given::Flag("title"),
        "text" => Given::Flag("text"),
        "severity" => Given::Flag("severity"),
        "anchor" => Given::Flag("anchor"),
        "requirement" => Given::Flag("requirement"),
        "blocking" => Given::Flag("blocking"),
        "slot" => Given::Flag("slot"),
        "supersedes" => Given::Flag("supersedes"),
        "supersede" => Given::Flag("supersede"),
        "sha" => Given::Flag("sha"),
        "at" => Given::Flag("at"),
        "of" => Given::Flag("of"),
        "to" => Given::Flag("to"),
        "state" => Given::Flag("state"),
        "range" => Given::Flag("range"),
        "verdict" => Given::Flag("verdict"),
        "entry" | "round" | "req" => Given::Argument,
        _ => return None,
    })
}

/// Every title and text in an op object, through the mail secret filter.
fn check_secrets(obj: &Map<String, Value>) -> Result<(), GraphError> {
    let hit = obj
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "title" | "text"))
        .filter_map(|(_, v)| v.as_str())
        .any(looks_secret);
    if hit {
        return Err(GraphError::usage(
            Code::MsgSecretRefused,
            "an entry's title or text looks like it holds a credential; refused",
        )
        .with_hint("never put keys or tokens in entries; refer to where they're kept instead"));
    }
    Ok(())
}

fn node_link(node: &IndexNode) -> Result<NodeLink, GraphError> {
    NodeLink::of(&node.id).map_err(|_| {
        GraphError::usage(
            Code::NodeNotFound,
            "the ledger takes format 0.7 nodes (an id with a UUID)",
        )
    })
}

fn changed_out(a: &Applied) -> Vec<ChangedOut> {
    a.changed
        .iter()
        .map(|c| ChangedOut {
            entry: c.path(),
            op: c.op.clone(),
            from: c.from.clone(),
            to: c.to.clone(),
        })
        .collect()
}

/// Runs an `entry` command.
pub fn run(root: &Path, actor: Option<String>, cmd: &EntryCommand) -> u8 {
    let name = cmd.name();
    let json = cmd.json();
    match cmd {
        EntryCommand::List(a) => emit(name, json, report(list(root, a)), |d| {
            for e in &d.entries {
                println!("{} {} {}: {}", e.id, e.kind, e.state, e.title);
            }
        }),
        // Text reads as lines, not JSON (e2e UX); `--json` has the record.
        EntryCommand::Show(a) => emit(name, json, report(show(root, a)), |d| {
            print_fields("", &serde_json::to_value(&d.entry).unwrap_or_default());
            // A transition's note carries a residual's reason (review of
            // 662cc79c).
            for e in &d.history {
                let note = e.note.as_deref().map_or_else(String::new, |n| {
                    format!(": {}", crate::output::shown_path(n))
                });
                println!(
                    "  {} {} {} -> {} by {}{note}",
                    e.seq,
                    e.op,
                    e.from.as_deref().unwrap_or("-"),
                    e.to,
                    crate::output::shown_path(&e.by)
                );
            }
        }),
        EntryCommand::Gate(a) => emit(name, json, gate_report(gate(root, a)), |d| {
            println!("gate {} at {}", if d.ok { "ok" } else { "fails" }, d.at);
            for (what, list) in [("fails", &d.failures), ("warns", &d.warnings)] {
                for f in list {
                    print_condition(what, f);
                }
            }
        }),
        EntryCommand::Update(a) => {
            let lines = read_ops(&a.op_file);
            emit(
                name,
                json,
                report(lines.and_then(|l| apply(root, actor, &a.node, l))),
                print_update,
            )
        }
        other => {
            let Some((op, a)) = other.op() else {
                return 1;
            };
            let obj = object_of(op, a);
            emit(
                name,
                json,
                report(apply(root, actor, &a.node, vec![Ok(obj)]).and_then(|d| {
                    // One op: its refusal is the command's.
                    match d.results.into_iter().next() {
                        Some(mut r) if !r.ok => {
                            let full = r.refusal.take();
                            Err(full.unwrap_or_else(|| single_refusal(&r)))
                        }
                        r => Ok(UpdateData {
                            node: d.node,
                            results: r.into_iter().collect(),
                        }),
                    }
                })),
                print_update,
            )
        }
    }
}

fn single_refusal(r: &OpResult) -> GraphError {
    let code = r
        .code
        .as_deref()
        .and_then(Code::parse)
        .unwrap_or(Code::EntryBadOp);
    GraphError::conflict(code, format!("refused: {}", code.as_str()))
}

/// A JSON value as text: strings as they are (escaped), lists joined with
/// commas, objects as `{key: value, ...}`, braced so a nested object's
/// fields read as its own (Opus final on a6ba7012) yet stay text, not
/// JSON (review of 4d8a588f).
/// Keys are escaped as values are, since a definition can name them
/// (review of 662cc79c), and a null prints as `null`: it can mean
/// something, such as a reset default.
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => crate::output::shown_path(s),
        Value::Array(a) => a.iter().map(value_text).collect::<Vec<_>>().join(", "),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", crate::output::shown_path(k), value_text(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

/// An object's fields as text lines, one `key: value` each; a nested
/// object's fields follow its `key:` one indent deeper, so each value
/// reads under the field it belongs to (Opus final on a6ba7012).
pub(crate) fn field_lines(indent: &str, v: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Value::Object(o) = v {
        for (k, v) in o {
            let k = crate::output::shown_path(k);
            match v {
                Value::Object(inner) if !inner.is_empty() => {
                    lines.push(format!("{indent}{k}:"));
                    lines.extend(field_lines(&format!("{indent}  "), v));
                }
                // A list prints one `- ` item a line, an object item's
                // fields under it (review of 4d8a588f), so an item holding
                // `, ` never reads as two (review of 40ae015b).
                Value::Array(items) if !items.is_empty() => {
                    lines.push(format!("{indent}{k}:"));
                    lines.extend(item_lines(&format!("{indent}  "), items));
                }
                _ => lines.push(format!("{indent}{k}: {}", value_text(v))),
            }
        }
    }
    lines
}

/// A list's items as `- ` lines; an object item's first field follows the
/// dash and the rest align under it.
fn item_lines(indent: &str, items: &[Value]) -> Vec<String> {
    let mut lines = Vec::new();
    let under = format!("{indent}  ");
    for item in items {
        match item {
            Value::Object(o) if !o.is_empty() => {
                for (i, line) in field_lines(&under, item).into_iter().enumerate() {
                    if i == 0 {
                        lines.push(format!("{indent}- {}", &line[under.len()..]));
                    } else {
                        lines.push(line);
                    }
                }
            }
            _ => lines.push(format!("{indent}- {}", value_text(item))),
        }
    }
    lines
}

/// An object's fields, as [`field_lines`] gives them.
fn print_fields(indent: &str, v: &Value) {
    for line in field_lines(indent, v) {
        println!("{line}");
    }
}

/// One gate condition: its number, reason and items, then its hint.
fn print_condition(what: &str, f: &Value) {
    let items = value_text(&f["items"]);
    let items = if items.is_empty() || items == "null" {
        String::new()
    } else {
        format!(" ({items})")
    };
    println!(
        "  {what}: condition {} {}{items}",
        value_text(&f["condition"]),
        value_text(&f["reason"])
    );
    // Its facts too: a changed path may be named only there (review of
    // 662cc79c).
    if let Some(data) = f.get("data").filter(|d| !d.is_null()) {
        // Fields a line, as everywhere else in text output (review of
        // 4d8a588f).
        if data.as_object().is_some_and(|o| !o.is_empty()) {
            println!("    data:");
            print_fields("      ", data);
        } else {
            println!("    data: {}", value_text(data));
        }
    }
    if let Some(hint) = f["hint"].as_str().filter(|h| !h.is_empty()) {
        println!("    hint: {}", crate::output::shown_path(hint));
    }
}

fn report<T>(r: Result<T, GraphError>) -> Report<T> {
    r.map_or_else(Report::failed, Report::ok)
}

/// A failed gate is a check that found errors (exit 2), with its data:
/// the failures are the answer (e2e UX; orchestrator on #17713).
fn gate_report(r: Result<GateData, GraphError>) -> Report<GateData> {
    match r {
        Ok(d) if !d.ok => {
            let failed = mdkg_format::Diagnostic::error(
                Code::EntryGateFailed,
                format!(
                    "{}'s gate fails at {}: {} failing condition{}",
                    d.node,
                    d.at,
                    d.failures.len(),
                    if d.failures.len() == 1 { "" } else { "s" }
                ),
            )
            .with_hint("settle each listed failure, then run `done` (or the gate) again");
            Report {
                data: Some(d),
                diagnostics: vec![failed],
                failure: Some(mdkg_graph::ErrorKind::Validation),
            }
        }
        r => report(r),
    }
}

fn print_update(d: &UpdateData) {
    for r in &d.results {
        if let Some(code) = &r.code {
            println!("line {}: refused ({code})", r.line);
        }
        if let Some(existing) = &r.existing {
            println!("line {}: already open as {existing}", r.line);
        }
        for c in &r.changed {
            println!(
                "{} {} {} -> {}",
                c.entry,
                c.op,
                c.from.as_deref().unwrap_or("-"),
                c.to
            );
        }
    }
}

/// One op line: its object, or the code it was refused with.
type OpLine = Result<Map<String, Value>, String>;

/// Op lines from a file or stdin: each non-empty line a JSON object, or
/// why it isn't one.
fn read_ops(path: &Path) -> Result<Vec<OpLine>, GraphError> {
    let mut text = String::new();
    let read = if path == Path::new("-") {
        std::io::stdin().take(1 << 20).read_to_string(&mut text)
    } else {
        std::fs::File::open(path).and_then(|f| f.take(1 << 20).read_to_string(&mut text))
    };
    read.map_err(|e| {
        GraphError::usage(Code::IoReadFailed, format!("can't read the op file: {e}"))
    })?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match serde_json::from_str::<Value>(l) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err("entry.bad_op".to_string()),
        })
        .collect())
}

/// Applies op objects on `node_handle` as the actor, each in its own
/// savepoint of one transaction: a refused line doesn't stop the others.
fn apply(
    root: &Path,
    actor: Option<String>,
    node_handle: &str,
    lines: Vec<OpLine>,
) -> Result<UpdateData, GraphError> {
    let repo = Repo::open(root)?;
    let actor = repo.actor(actor);
    let node = crate::write::resolve_node(&repo, node_handle)?;
    let link = node_link(&node)?;
    // The store classes the actor inside the op's transaction
    // ([`Env::definitions`]); this is only the starting value.
    let class = if is_person(&actor) {
        Class::Person
    } else {
        Class::Other
    };
    let mut store = repo.store()?;
    let now = crate::coord::now_ms();
    let mut results: Vec<OpResult> = Vec::new();
    let mut ops: Vec<(usize, Op)> = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        let parsed = line.map_err(|code| (code, None)).and_then(|obj| {
            check_secrets(&obj).map_err(|e| (e.code().as_str().to_string(), Some(e)))?;
            Op::from_object(&obj).map_err(|e| {
                let code = e.code().unwrap_or("entry.bad_op").to_string();
                (code, Some(entry_failure(e)))
            })
        });
        match parsed {
            Ok(op) => ops.push((i + 1, op)),
            Err((code, refusal)) => results.push(OpResult {
                line: i + 1,
                ok: false,
                code: Some(code),
                changed: Vec::new(),
                existing: None,
                refusal,
            }),
        }
    }
    let changes: Vec<Change<'_>> = ops
        .iter()
        .map(|(_, op)| Change {
            op,
            actor: &actor,
            class,
            node: Some(&link),
            message: None,
            turn: None,
            now_ms: now,
        })
        .collect();
    let outcomes = {
        let held = repo.store()?;
        let mut env = RepoEnv::new(&repo)?;
        env.store = Some(&held);
        store
            .entry_apply_each(&changes, &env)
            .map_err(entry_failure)?
    };
    for ((line, _), outcome) in ops.iter().zip(outcomes) {
        results.push(match outcome {
            Ok(a) => OpResult {
                line: *line,
                ok: true,
                code: None,
                changed: changed_out(&a),
                existing: a.existing.clone(),
                refusal: None,
            },
            Err(e) => {
                if e.code().is_none() {
                    return Err(entry_failure(e));
                }
                OpResult {
                    line: *line,
                    ok: false,
                    code: e.code().map(str::to_string),
                    changed: Vec::new(),
                    existing: None,
                    refusal: Some(entry_failure(e)),
                }
            }
        });
    }
    results.sort_by_key(|r| r.line);
    Ok(UpdateData {
        node: link.id,
        results,
    })
}

fn list(root: &Path, a: &ListArgs) -> Result<ListData, GraphError> {
    let repo = Repo::open(root)?;
    let node = crate::write::resolve_node(&repo, &a.node)?;
    let link = node_link(&node)?;
    let store = repo.store()?;
    let kind = match &a.kind {
        Some(k) => Some(Kind::parse(k).ok_or_else(|| {
            GraphError::usage(Code::EntryBadOp, "not an entry kind")
                .with_hint("question, decision, finding, issue, review or req")
        })?),
        None => None,
    };
    let all = store
        .entries(&link.uuid, &Filter::default())
        .map_err(entry_failure)?;
    let filter = Filter {
        kind,
        state: a.state.clone(),
        severity: a.severity.clone(),
        asked_of: a.asked_of.clone(),
        blocking: a.blocking,
    };
    let shown: Vec<&Entry> = all
        .iter()
        .filter(|e| {
            filter.kind.is_none_or(|k| k == e.kind)
                && filter.state.as_deref().is_none_or(|s| s == e.state)
                && filter
                    .severity
                    .as_deref()
                    .is_none_or(|s| e.severity.as_deref() == Some(s))
                && filter
                    .asked_of
                    .as_deref()
                    .is_none_or(|w| e.asked_of() == Some(w))
                && filter.blocking.is_none_or(|b| b == e.blocking)
        })
        .collect();
    Ok(ListData {
        node: link.id,
        entries: shown.iter().map(|e| EntryRecord::from(*e)).collect(),
        input_required: entries::input_required(&all)
            .iter()
            .map(|e| e.id.clone())
            .collect(),
        decisions_in_force: entries::decisions_in_force(&all)
            .iter()
            .map(|e| e.id.clone())
            .collect(),
    })
}

fn show(root: &Path, a: &ShowArgs) -> Result<ShowData, GraphError> {
    let repo = Repo::open(root)?;
    let node = crate::write::resolve_node(&repo, &a.node)?;
    let link = node_link(&node)?;
    let store = repo.store()?;
    let entry = store
        .entry(&link.uuid, &a.entry)
        .map_err(entry_failure)?
        .ok_or_else(|| entry_failure(EntryError::NotFound(format!("{}#{}", link.id, a.entry))))?;
    let history = store
        .entry_history(&link.uuid, &a.entry)
        .map_err(entry_failure)?;
    Ok(ShowData {
        entry: EntryRecord::from(&entry),
        history: history.iter().map(EntryEvent::from).collect(),
    })
}

/// The gate's conditions 1 to 4 for `node` at `at` (A29). Condition 5,
/// design edits checked by content, comes with P4's requirement parser.
pub(crate) fn gate_of(
    repo: &Repo,
    node: &IndexNode,
    at: Option<&str>,
) -> Result<(String, entries::Gate), GraphError> {
    let link = node_link(node)?;
    let store = repo.store()?;
    let mut env = RepoEnv::new(repo)?;
    env.store = Some(&store);
    // `--at` names a SHA, checked like every other (never a fallback to
    // HEAD on a typo or an unfetched commit); without it, this checkout's
    // HEAD.
    let at = match at {
        Some(sha) => env.commit(sha),
        None => git::commit_of(&repo.top, "HEAD").map_err(git_error),
    }
    .map_err(entry_failure)?;
    // The node file as git holds it, from the repository's top; a local
    // node's file is in no tree.
    let node_path = repo
        .store_scope(&node.path)
        .is_none()
        .then(|| format!("{}{}", repo.prefix, node.path));
    let gate = store
        .entry_gate(
            &link.uuid,
            &requirement_ids(&node.body),
            &at,
            node_path.as_deref(),
            &env,
        )
        .map_err(entry_failure)?;
    Ok((at, gate))
}

/// `done`'s gate and settle (A29, "Completion steps"): the node, its
/// requirements and the env its git reads go through. The gate reads only
/// commits, trees and blobs, so the env takes no graph lock
/// ([`RepoEnv::git_only`]): a graph writer holding the writers' lock can
/// run `done` without waiting on itself. The g-writers
/// lanes settle through [`Settling::settle`], which calls
/// [`entries::settle_in`] inside the settling transaction, and nothing
/// else stands in for it.
pub(crate) struct Settling<'a> {
    /// The env and node, or none for a node without a UUID (a format
    /// 0.6 graph's), which has no ledger and settles as in 0.7.0.
    ledger: Option<(RepoEnv<'a>, NodeLink)>,
    repo: &'a Repo,
    requirements: Vec<String>,
    /// The node file's path from the repository's top; none for a local
    /// node, whose file is in no tree.
    node_path: Option<String>,
    actor: String,
    /// Why the settling transaction refused, for `done` to report.
    refused: Option<entries::SettleError>,
    /// The commit a git-work settle checked, which stays on the branch
    /// when the settle refuses.
    committed: Option<String>,
}

impl<'a> Settling<'a> {
    pub(crate) fn new(repo: &'a Repo, node: &IndexNode, actor: &str) -> Result<Self, GraphError> {
        let ledger = match NodeLink::of(&node.id) {
            Ok(link) => Some((RepoEnv::git_only(repo)?, link)),
            Err(_) => None,
        };
        let node_path = repo
            .store_scope(&node.path)
            .is_none()
            .then(|| format!("{}{}", repo.prefix, node.path));
        Ok(Self {
            ledger,
            repo,
            requirements: requirement_ids(&node.body),
            node_path,
            actor: actor.to_string(),
            refused: None,
            committed: None,
        })
    }

    /// Step 2, the pre-check, in one read at this checkout's HEAD: a
    /// failing gate refuses here, with nothing written. It only saves
    /// work; the settle evaluates the gate again. A HEAD that can't be
    /// read fails only a node whose gate reads git.
    pub(crate) fn precheck(&self, store: &Store) -> Result<(), GraphError> {
        if self.ledger.is_none() {
            return Ok(());
        }
        let at = git::commit_of(&self.repo.top, "HEAD").unwrap_or_default();
        self.precheck_at(store, &at)
    }

    /// The node file `text` with its `# Ledger` section (A31), for git
    /// work's `done` to commit with `status: done`: every entry and
    /// requirement as the store holds them now, `upto` this read's `S`.
    /// `None` for a node without a UUID, a local node, or a node with no
    /// ledger, as in 0.7.0. A file that can't take a section the strip
    /// removes again ([`entries::with_ledger_section`]) gets none, with a
    /// warning: the section is display only, and the ledger settles all
    /// the same.
    pub(crate) fn ledger_text(
        &self,
        store: &Store,
        text: &str,
    ) -> Result<(Option<String>, Option<mdkg_format::Diagnostic>), GraphError> {
        let Some((_, link)) = &self.ledger else {
            return Ok((None, None));
        };
        if self.node_path.is_none() {
            return Ok((None, None));
        }
        let Some(section) = store
            .ledger_section(link, &self.requirements)
            .map_err(entry_failure)?
        else {
            return Ok((None, None));
        };
        // No new section: an earlier one of the writer's goes too, so an
        // out-of-date ledger never stays in the body (Terra's #11158).
        let unwritten = |why: &str| {
            let old = entries::without_ledger_section(text.as_bytes())
                .and_then(|b| String::from_utf8(b).ok());
            let removed = if old.is_some() {
                "; its earlier section was removed"
            } else {
                ""
            };
            let warning = mdkg_format::Diagnostic::warning(
                Code::EntryLedgerUnwritten,
                format!("{} got no # Ledger section: {why}{removed}", link.id),
            )
            .with_hint(format!(
                "the ledger settles all the same; `{} entry list` shows it",
                crate::BIN
            ));
            (old, Some(warning))
        };
        let Some(t) = entries::with_ledger_section(text.as_bytes(), &section)
            .and_then(|b| String::from_utf8(b).ok())
        else {
            return Ok(unwritten(
                "its file can't take one that compares equal without it (an unclosed fence, \
                 another `# Ledger` heading, or no final newline)",
            ));
        };
        // A file over the graph's per-file limit fails every index read,
        // so the section never pushes one over (Terra's #11075, minor 1).
        let limit = mdkg_graph::config::load(&self.repo.root)?
            .limits
            .max_file_bytes;
        if t.len() as u64 > limit {
            return Ok(unwritten(&format!(
                "with it the file would be over index.limits.max_file_bytes ({limit})"
            )));
        }
        Ok((Some(t), None))
    }

    /// The pre-check at `at`, a commit the caller already pinned
    /// (`already_done`'s HEAD; Terra's #10948, minor 2).
    pub(crate) fn precheck_at(&self, store: &Store, at: &str) -> Result<(), GraphError> {
        let Some((env, link)) = &self.ledger else {
            return Ok(());
        };
        let gate = store
            .entry_gate(
                &link.uuid,
                &self.requirements,
                at,
                self.node_path.as_deref(),
                env,
            )
            .map_err(entry_failure)?;
        if gate.ok() {
            Ok(())
        } else {
            Err(gate_failed(link, &gate, None))
        }
    }

    /// The settling transaction's check in git work: the gate at `at`,
    /// the commit just made, and the `ledger.settle` record for the
    /// changeset the mark settles, whose `revision` is the node file's
    /// text at `at` ([`entries::node_revision_text`]). `false` refuses,
    /// which rolls the transaction back.
    pub(crate) fn settle_git(
        &mut self,
        tx: &rusqlite::Connection,
        at: &str,
        changeset: Option<&str>,
    ) -> Result<bool, mdkg_coord_sqlite::StoreError> {
        if self.ledger.is_none() {
            return Ok(true);
        }
        self.committed = Some(at.to_string());
        let revision = match self.revision_at(at) {
            Ok(r) => r,
            Err(e) => {
                self.refused = Some(entries::SettleError::Entry(e));
                return Ok(false);
            }
        };
        self.settle(tx, at, &revision, changeset)
    }

    /// The same in local work, `complete_local`'s transaction: the node
    /// file as written, `bytes`, at this checkout's HEAD.
    pub(crate) fn settle_local(
        &mut self,
        tx: &rusqlite::Connection,
        bytes: &[u8],
    ) -> Result<bool, mdkg_coord_sqlite::StoreError> {
        if self.ledger.is_none() {
            return Ok(true);
        }
        let at = git::commit_of(&self.repo.top, "HEAD").unwrap_or_default();
        let revision =
            mdkg_format::Digest::of_bytes(&entries::node_revision_text(bytes)).to_string();
        self.settle(tx, &at, &revision, None)
    }

    fn settle(
        &mut self,
        tx: &rusqlite::Connection,
        at: &str,
        revision: &str,
        changeset: Option<&str>,
    ) -> Result<bool, mdkg_coord_sqlite::StoreError> {
        let Some((env, link)) = &self.ledger else {
            return Ok(true);
        };
        // A settle refused once its node is written, for the recovery
        // tests of a local node, whose gate ignores its file so no real
        // failure can be arranged (Terra's code review #10357, minor 1):
        // debug builds only, as `write::fault` injects a failed write.
        if cfg!(debug_assertions)
            && std::env::var("MDKG_FAIL_AFTER").ok().as_deref() == Some("settle")
        {
            self.refused = Some(entries::SettleError::Entry(EntryError::Git(
                "the settle was refused (injected)".into(),
            )));
            return Ok(false);
        }
        // A done as an earlier build made it, with no settle record, for
        // the test of `already_done` settling a legacy mark: debug builds
        // only. The gate is still evaluated, as the settle would, and only
        // the record is rolled back, so the injection never passes a done
        // whose gate fails (Terra's #10948, minor 1).
        let unsettled = cfg!(debug_assertions)
            && std::env::var("MDKG_FAIL_AFTER").ok().as_deref() == Some("unsettled");
        let settle = entries::Settle {
            node: link,
            requirements: &self.requirements,
            at,
            node_path: self.node_path.as_deref(),
            revision,
            changeset,
        };
        if unsettled {
            tx.execute_batch("SAVEPOINT injected_unsettled")?;
        }
        let settled = entries::settle_in(tx, &self.actor, &settle, env);
        if unsettled {
            tx.execute_batch("ROLLBACK TO injected_unsettled; RELEASE injected_unsettled")?;
        }
        match settled {
            Ok(_) => Ok(true),
            Err(e) => {
                self.refused = Some(e);
                Ok(false)
            }
        }
    }

    /// Whether the node has a ledger and no `ledger.settle` record holds
    /// its file's revision at `at`: a done commit whose settle refused, or
    /// never ran, which `done`'s rerun settles.
    pub(crate) fn unsettled_at(&self, store: &Store, at: &str) -> Result<bool, GraphError> {
        let Some((_, link)) = &self.ledger else {
            return Ok(false);
        };
        let revision = self.revision_at(at).map_err(entry_failure)?;
        let settles = store.ledger_settles(link).map_err(entry_failure)?;
        Ok(!settles
            .iter()
            .any(|r| r.get("revision").and_then(Value::as_str) == Some(revision.as_str())))
    }

    /// The node file's revision digest at `at`.
    fn revision_at(&self, at: &str) -> Result<String, EntryError> {
        let path = self
            .node_path
            .as_deref()
            .ok_or_else(|| git_error("a local node has no file in git"))?;
        let Some((env, _)) = &self.ledger else {
            return Err(git_error("a node without a UUID has no ledger"));
        };
        let entry = env
            .tree_entry(at, path)?
            .ok_or_else(|| git_error(format!("{at} holds no node file")))?;
        let oid = entry
            .split('\t')
            .next()
            .and_then(|head| head.split(' ').nth(2))
            .ok_or_else(|| git_error("can't read the node file's tree entry"))?;
        let bytes = env.blob(oid)?;
        Ok(mdkg_format::Digest::of_bytes(&entries::node_revision_text(&bytes)).to_string())
    }

    /// Fills the env's git answers for a settle at `at` before its
    /// transaction opens: the node file's revision there and the gate.
    fn warm_at(&self, store: &Store, at: &str) {
        let Some((env, link)) = &self.ledger else {
            return;
        };
        if self.node_path.is_some() {
            let _ = self.revision_at(at);
        }
        let _ = store.entry_gate(
            &link.uuid,
            &self.requirements,
            at,
            self.node_path.as_deref(),
            env,
        );
    }

    /// Why the settle refused, as `done` reports it: `entry.gate_failed`
    /// (exit 5) for a failing gate, saying so when its commit stays on
    /// the branch unsettled.
    pub(crate) fn refusal(&mut self) -> Option<GraphError> {
        let committed = self.committed.clone();
        let link = self.ledger.as_ref().map(|(_, l)| l.clone())?;
        self.refused.take().map(|e| match e {
            entries::SettleError::Gate(g) => gate_failed(&link, &g, committed.as_deref()),
            entries::SettleError::Entry(e) => entry_failure(e),
        })
    }
}

impl crate::write::SettleCheck for Settling<'_> {
    fn warm(&mut self, store: &Store, at: &str) {
        self.warm_at(store, at);
    }

    fn check(
        &mut self,
        tx: &rusqlite::Connection,
        at: &str,
        changeset: Option<&str>,
    ) -> Result<bool, mdkg_coord_sqlite::StoreError> {
        self.settle_git(tx, at, changeset)
    }
}

/// `done --residual "<reason>"` (A29, "Residuals", and completion step
/// 1): a residual for every failure of the gate at HEAD that a residual
/// may record, in one store transaction, all or none, before the
/// pre-check. A residual is true whether or not `done` finishes. Refused
/// whole, with nothing recorded, when a failure can't take one
/// (condition 2's out-of-range verified findings: `entry.gate_failed`,
/// saying how to fix them), or when the actor may not record residuals
/// (`entry.not_allowed`: only a person or a coordinator). Returns the
/// entries it recorded residuals on; none for a node without a ledger, or
/// one whose gate passes. The gate is read before the residuals'
/// transaction, so a fix or verify landing between them can leave a
/// residual that's no longer needed: each op is still judged by the store
/// in its transaction, and `done`'s settle evaluates the gate again
/// (Terra's #11173, minor 2: a stated residual).
pub(crate) fn record_residuals(
    repo: &Repo,
    node: &IndexNode,
    actor: &str,
    reason: &str,
) -> Result<Vec<String>, GraphError> {
    // A person's `done --residual` never runs inside a turn, even where
    // the gate passes and nothing would be recorded: person-class
    // operations are refused there (decided 2026-10-29; the orchestrator on
    // #11043).
    // Any spelling of `human:` (Terra's #11173): main.rs admits only the
    // lowercase one, and this never lets another past the turn rule.
    let human = actor
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("human:"));
    if human && RepoEnv::new(repo)?.in_turn(actor).map_err(entry_failure)? {
        return Err(entry_failure(EntryError::NotAllowed {
            class: Class::Person,
            op: "done --residual".into(),
        }));
    }
    let Ok(link) = NodeLink::of(&node.id) else {
        return Ok(Vec::new());
    };
    let mut text = Map::new();
    text.insert("text".into(), reason.into());
    check_secrets(&text)?;
    let (at, gate) = gate_of(repo, node, None)?;
    if gate.ok() {
        return Ok(Vec::new());
    }
    let never: Vec<entries::GateFailure> = gate
        .failures
        .iter()
        .filter(|f| !f.residual)
        .cloned()
        .collect();
    if !never.is_empty() {
        let only = entries::Gate {
            failures: never,
            ..gate.clone()
        };
        return Err(gate_failed(&link, &only, None).with_hint(
            "--residual can't accept these: review the range that holds each verified fix (or \
             fetch the main that has it), then run `done` again",
        ));
    }
    let mut items: Vec<&str> = Vec::new();
    for f in &gate.failures {
        for i in &f.items {
            if !items.contains(&i.as_str()) {
                items.push(i);
            }
        }
    }
    // Failures that name no entry give a residual nothing to accept:
    // refused here, saying so, rather than by the pre-check (Terra's
    // #11173, minor 3).
    if items.is_empty() {
        return Err(gate_failed(&link, &gate, None).with_hint(
            "--residual found nothing it may accept: these failures name no entry; fix them, \
             then run `done` again",
        ));
    }
    let ops = items
        .iter()
        .map(|id| {
            let mut obj = Map::new();
            obj.insert("op".into(), "residual".into());
            obj.insert("entry".into(), (*id).into());
            obj.insert("text".into(), reason.into());
            obj.insert("at".into(), at.clone().into());
            Op::from_object(&obj)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(entry_failure)?;
    // The store classes the actor inside the transaction, as for any op.
    let class = if is_person(actor) {
        Class::Person
    } else {
        Class::Other
    };
    let now = crate::coord::now_ms();
    let changes: Vec<Change<'_>> = ops
        .iter()
        .map(|op| Change {
            op,
            actor,
            class,
            node: Some(&link),
            message: None,
            turn: None,
            now_ms: now,
        })
        .collect();
    let mut store = repo.store()?;
    let held = repo.store()?;
    let mut env = RepoEnv::new(repo)?;
    env.store = Some(&held);
    store
        .entry_apply_all(&changes, &env)
        .map_err(entry_failure)?;
    Ok(items.iter().map(|i| format!("{}#{i}", link.id)).collect())
}

/// The nodes a `msg send --node` names (A23), each resolved like `show`'s
/// handle against this checkout's index and kept by its UUID, once each,
/// in the order given. A handle that doesn't resolve, or a node without a
/// UUID (a format 0.6 graph's), is `node.not_found`.
pub(crate) fn send_nodes(repo: &Repo, handles: &[String]) -> Result<Vec<NodeLink>, GraphError> {
    let mut out: Vec<NodeLink> = Vec::new();
    for handle in handles {
        let node = crate::write::resolve_node(repo, handle)?;
        let link = node_link(&node)?;
        if !out.iter().any(|n| n.uuid == link.uuid) {
            out.push(link);
        }
    }
    Ok(out)
}

/// The review round a `msg send --kind review_request --node <node>
/// --range <base>..<sha>` opens (A27 rule 8), inside the send's own
/// transaction, so the request and its round commit together (Terra's
/// #6548). Its range is resolved to full commit ids before anything is
/// sent (`entry.unknown_commit` otherwise), since a round stores only
/// those.
pub(crate) struct SendRound<'a> {
    env: RepoEnv<'a>,
    node: NodeLink,
    base: String,
    head: String,
    /// A graph range: a design node's text, whose rounds never anchor on
    /// main ([`graph_range`]).
    graph: bool,
    title: String,
    refused: Option<EntryError>,
    opened: Option<entries::RoundOpened>,
}

impl<'a> SendRound<'a> {
    pub(crate) fn new(
        repo: &'a Repo,
        node: NodeLink,
        range: &str,
        title: &str,
    ) -> Result<Self, GraphError> {
        let (base, head) = range
            .split_once("..")
            .filter(|(b, h)| !b.is_empty() && !h.is_empty() && !h.starts_with('.'))
            .ok_or_else(|| {
                GraphError::usage(Code::UsageInvalidArguments, "--range takes <base>..<sha>")
            })?;
        let env = RepoEnv::git_only(repo)?;
        let full = |sha: &str| {
            env.commit(sha)
                .map(|c| c.to_ascii_lowercase())
                .map_err(entry_failure)
        };
        let (base, head) = (full(base)?, full(head)?);
        let graph = graph_range(repo, &env, &head).map_err(entry_failure)?;
        // A one-line title, from the request's first text line.
        let title = title
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("review")
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>();
        Ok(Self {
            env,
            node,
            base,
            head,
            graph,
            title,
            refused: None,
            opened: None,
        })
    }

    /// The send's hook: opens the round in the send's transaction, its
    /// reviewers the message's recipients. A refusal rolls the send back,
    /// so nothing is sent; [`SendRound::refusal`] says why.
    pub(crate) fn open_in(
        &mut self,
        tx: &rusqlite::Transaction<'_>,
        message: &mdkg_coord_sqlite::mail::Message,
    ) -> Result<(), mdkg_coord_sqlite::mail::MailError> {
        match entries::open_round_in(
            tx,
            &self.node,
            &self.title,
            (&self.base, &self.head),
            &message.to(),
            &message.sender,
            &message.message_id,
            message.sent_ms,
            self.graph,
            &self.env,
        ) {
            Ok(opened) => {
                self.opened = opened;
                Ok(())
            }
            Err(e) => {
                self.refused = Some(e);
                Err(mdkg_coord_sqlite::mail::MailError::Store(
                    mdkg_coord_sqlite::StoreError::Schema("the review round was refused".into()),
                ))
            }
        }
    }

    /// The range, resolved: part of the send's request, so a retry with
    /// the same id and another range is `msg.id_reused` (Terra's #11150).
    pub(crate) fn range(&self) -> (String, String) {
        (self.base.clone(), self.head.clone())
    }

    /// Why the round refused, when it did.
    pub(crate) fn refusal(&mut self) -> Option<GraphError> {
        self.refused.take().map(entry_failure)
    }

    /// The round this send opened (none on a retried send, whose request
    /// opened it before): its entry path and its anchoring.
    pub(crate) fn opened(&self) -> Option<(String, Option<&'static str>)> {
        self.opened.as_ref().map(|r| (r.changed.path(), r.anchored))
    }
}

/// Whether a round's head is on a graph range (graph.ref, "Design nodes
/// live on the graph ref": `base..sha` in the graph ref's history): in the
/// pinned graph commit's history and in no main tip's. A commit main also
/// holds is code history the graph shares, never a graph range. With no
/// graph commit, nothing is.
fn graph_range(repo: &Repo, env: &RepoEnv<'_>, head: &str) -> Result<bool, EntryError> {
    let Some(tip) = graph_commit(repo).map_err(git_error)? else {
        return Ok(false);
    };
    if !env.contains(head, &tip)? {
        return Ok(false);
    }
    for t in env.main_tips()?.unwrap_or_default() {
        if env.contains(head, &t)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How a read reports a node whose effective status is `done` (A29, "A
/// done file that didn't settle reads as unsettled"). A node without a
/// ledger reads as in 0.7.0.
pub(crate) const UNSETTLED: &str = "done (unsettled)";
/// Without the coordinator: a git-work file whose `status: done` comes
/// with a `# Ledger` section, since only the store can say it settled.
pub(crate) const UNKNOWN: &str = "done (unknown)";

/// Settled reads (A29, "A done file that didn't settle reads as
/// unsettled", with the orchestrator's call on #10251): for each node
/// whose effective status is `done`, `None` when it reads as done (no
/// ledger, or settled), else [`UNSETTLED`], or [`UNKNOWN`] when git
/// can't answer.
///
/// A revision is settled when a `ledger.settle` record has its revision
/// digest and, in git work when the record has a range, the revision
/// sits on a tree holding the reviewed content:
/// - fast: the record's `at` is in HEAD's history, where the settle's own
///   gate passed on that revision;
/// - otherwise the content check for the record's range runs at the
///   commit where the done revision arrived: the newest in HEAD's
///   first-parent history whose file says done with this revision while
///   its first parent's doesn't (`status: done` is stripped from the
///   revision, so a revision alone would arrive before the work did). So
///   later commits elsewhere never unsettle a node, and a squash or
///   cherry-pick onto a tree without the reviewed content does;
/// - a node done by the coordinator's mark alone (the file here isn't
///   done) is settled when a record settled the mark's changeset.
///
/// Both histories are read once per command, bounded at 10,000 commits.
/// The arrival search's bound is shared: one path-limited log for every
/// node the command reads lists at most 10,000 commits touching any of
/// them, so with many nodes a node's arrival can fall past it sooner.
/// Past the bound a revision that matches reads settled (a residual:
/// reads aren't authority, and `done`'s gate re-checks content; Terra's
/// #10508, 2). Results are cached per record range, commit and path.
pub(crate) fn unsettled(
    repo: &Repo,
    reads: &entries::LedgerReads,
    nodes: &[(&IndexNode, Option<String>)],
) -> Vec<Option<&'static str>> {
    let mut out: Vec<Option<&'static str>> = vec![None; nodes.len()];
    // Each git-work node still to judge: its index, top-relative path,
    // revision and the matching records with a range.
    let mut pending: Vec<(usize, String, String, Vec<&Value>)> = Vec::new();
    for (i, (node, mark)) in nodes.iter().enumerate() {
        let Ok(link) = NodeLink::of(&node.id) else {
            continue;
        };
        if !reads.has_ledger(&link.uuid) {
            continue;
        }
        let Ok(bytes) = std::fs::read(repo.root.join(&node.path)) else {
            out[i] = Some(UNKNOWN);
            continue;
        };
        // Done by the coordinator's mark alone (the file here isn't, as
        // in another worktree before the merge): settled when a record
        // settled that mark's changeset, as `done` writes them together.
        if !file_says_done(&bytes) {
            let settled = mark.as_deref().is_some_and(|m| {
                reads
                    .settles(&link.uuid)
                    .iter()
                    .any(|r| r.get("changeset").and_then(Value::as_str) == Some(m))
            });
            if !settled {
                out[i] = Some(UNSETTLED);
            }
            continue;
        }
        let revision =
            mdkg_format::Digest::of_bytes(&entries::node_revision_text(&bytes)).to_string();
        let matching: Vec<&Value> = reads
            .settles(&link.uuid)
            .iter()
            .filter(|r| r.get("revision").and_then(Value::as_str) == Some(revision.as_str()))
            .collect();
        if matching.is_empty() {
            out[i] = Some(UNSETTLED);
            continue;
        }
        let local = repo.store_scope(&node.path).is_some();
        let ranged: Vec<&Value> = matching
            .iter()
            .copied()
            .filter(|r| record_range(r).is_some())
            .collect();
        if local || ranged.len() < matching.len() {
            continue;
        }
        pending.push((i, format!("{}{}", repo.prefix, node.path), revision, ranged));
    }
    if pending.is_empty() {
        return out;
    }
    let judged = (|| -> Result<Vec<(usize, bool)>, EntryError> {
        let env = RepoEnv::git_only(repo).map_err(|e| git_error(e.diagnostic.message))?;
        let head = git::commit_of(&repo.top, "HEAD").map_err(git_error)?;
        let limit = format!("--max-count={HISTORY_LIMIT}");
        let history: std::collections::HashSet<String> = String::from_utf8_lossy(
            &git_read(
                &env.common,
                &["rev-list", &limit, "--end-of-options", &head],
            )?
            .ok_or_else(|| git_error("can't list HEAD's history"))?,
        )
        .lines()
        .map(str::to_string)
        .collect();
        let mut judged = Vec::new();
        let mut slow = Vec::new();
        for (i, path, revision, records) in &pending {
            let fast = records.iter().any(|r| {
                r.get("at")
                    .and_then(Value::as_str)
                    .is_some_and(|at| history.contains(&at.to_ascii_lowercase()))
            });
            if fast {
                judged.push((*i, true));
            } else {
                slow.push((*i, path, revision, records));
            }
        }
        if slow.is_empty() {
            return Ok(judged);
        }
        // One first-parent history read for every node still to judge:
        // the commits touching each path, newest first.
        let mut args = vec![
            "--literal-pathspecs".to_string(),
            "log".into(),
            "--first-parent".into(),
            "--no-renames".into(),
            "--ignore-submodules=none".into(),
            "--no-show-signature".into(),
            limit.clone(),
            "--format=%x01%H".into(),
            "-z".into(),
            "--name-only".into(),
            head.clone(),
            "--".into(),
        ];
        args.extend(slow.iter().map(|(_, p, _, _)| (*p).clone()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let listed = git_read(&env.common, &args)?
            .ok_or_else(|| git_error("can't read HEAD's first-parent history"))?;
        let mut touching: std::collections::HashMap<String, Vec<String>> = Default::default();
        let mut commits = 0usize;
        for chunk in listed.split(|b| *b == 1).filter(|c| !c.is_empty()) {
            let mut parts = chunk.split(|b| *b == 0);
            let sha = String::from_utf8_lossy(parts.next().unwrap_or_default()).to_string();
            commits += 1;
            for p in parts {
                let p = String::from_utf8_lossy(p);
                let p = p.trim_start_matches('\n');
                if !p.is_empty() {
                    touching.entry(p.to_string()).or_default().push(sha.clone());
                }
            }
        }
        let bounded = commits >= HISTORY_LIMIT;
        // By record range, commit and node path: the node's own file is
        // compared by its revision text (Terra's #10508, 1).
        let mut checked: std::collections::HashMap<(String, String, String, String), bool> =
            Default::default();
        // The node file's revision at `c` when it says done there.
        let revision_at = |c: &str, path: &str| -> Result<Option<String>, EntryError> {
            let Some(entry) = env.tree_entry(c, path)? else {
                return Ok(None);
            };
            let Some(oid) = entry
                .split('\t')
                .next()
                .and_then(|head| head.split(' ').nth(2))
            else {
                return Ok(None);
            };
            let bytes = env.blob(oid)?;
            Ok(file_says_done(&bytes).then(|| {
                mdkg_format::Digest::of_bytes(&entries::node_revision_text(&bytes)).to_string()
            }))
        };
        for (i, path, revision, records) in slow {
            let list = touching.get(path.as_str()).cloned().unwrap_or_default();
            // Where the done revision arrived: the oldest of the newest
            // run of commits whose file says done with this revision (a
            // `status` or `updated` change keeps the run; the commit
            // before it had another revision or wasn't done). None when
            // HEAD's file isn't it (a working-tree revision), which is
            // checked at HEAD.
            let mut arrived: Option<&str> = None;
            let mut whole_run = true;
            for c in &list {
                if revision_at(c, path)?.as_deref() == Some(revision.as_str()) {
                    arrived = Some(c);
                } else {
                    whole_run = false;
                    break;
                }
            }
            if whole_run && bounded {
                // The arrival lies past the bound: revision equality
                // alone (a residual).
                judged.push((i, true));
                continue;
            }
            let at = arrived.unwrap_or(&head).to_string();
            let mut ok = false;
            for r in records {
                let Some((base, f)) = record_range(r) else {
                    continue;
                };
                let key = (
                    base.to_string(),
                    f.to_string(),
                    at.clone(),
                    path.to_string(),
                );
                let clean = match checked.get(&key) {
                    Some(c) => *c,
                    None => {
                        let c = entries::walk_differs(&env, (base, f), &at, Some(path))?.is_empty();
                        checked.insert(key, c);
                        c
                    }
                };
                if clean {
                    ok = true;
                    break;
                }
            }
            judged.push((i, ok));
        }
        Ok(judged)
    })();
    match judged {
        Ok(judged) => {
            for (i, ok) in judged {
                if !ok {
                    out[i] = Some(UNSETTLED);
                }
            }
        }
        Err(_) => {
            for (i, ..) in &pending {
                out[*i] = Some(UNKNOWN);
            }
        }
    }
    out
}

/// Whether a node file's front matter says `status: done`.
fn file_says_done(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    lines
        .take_while(|l| l.trim_end() != "---")
        .filter_map(|l| l.strip_prefix("status:"))
        .any(|v| v.trim().trim_matches(['"', '\'']) == "done")
}

/// A settle record's walk `{base, head}`, when it has one.
fn record_range(record: &Value) -> Option<(&str, &str)> {
    let range = record.get("range")?;
    Some((range.get("base")?.as_str()?, range.get("head")?.as_str()?))
}

/// Without the coordinator (a read that skipped a store it couldn't
/// read): whether a node whose file says `status: done` reads
/// [`UNKNOWN`], a git-work file with a `# Ledger` heading line.
pub(crate) fn unknown_without_store(node: &IndexNode) -> bool {
    node.status.as_deref() == Some("done")
        && !mdkg_graph::store_work::is_store_path(&node.path)
        && node.body.lines().any(|l| l.trim_end() == "# Ledger")
}

/// `done` refused for a failing gate (A29): `entry.gate_failed`, exit 5,
/// naming each failing condition and the entries it names (ids only, never
/// their text); `entry gate --json` gives the same failures as data.
fn gate_failed(node: &NodeLink, gate: &entries::Gate, committed: Option<&str>) -> GraphError {
    let list: Vec<String> = gate
        .failures
        .iter()
        .map(|f| {
            if f.items.is_empty() {
                format!("condition {} {}", f.condition, f.reason)
            } else {
                format!(
                    "condition {} {} ({})",
                    f.condition,
                    f.reason,
                    f.items.join(", ")
                )
            }
        })
        .collect();
    let after = committed.map_or_else(String::new, |c| {
        format!(
            "; {} is committed but unsettled, which counts as not done",
            &c[..c.len().min(12)]
        )
    });
    let hint = gate
        .failures
        .first()
        .map_or_else(String::new, |f| format!("{}; ", f.hint));
    GraphError::conflict(
        Code::EntryGateFailed,
        format!("{}'s gate fails: {}{after}", node.id, list.join("; ")),
    )
    .with_hint(format!(
        "{hint}`{} entry gate {}` lists each failure; run `done` again once they're settled",
        crate::BIN,
        node.id
    ))
}

fn gate(root: &Path, a: &GateArgs) -> Result<GateData, GraphError> {
    let repo = Repo::open(root)?;
    let node = crate::write::resolve_node(&repo, &a.node)?;
    let (at, gate) = gate_of(&repo, &node, a.at.as_deref())?;
    Ok(GateData {
        node: node.id.clone(),
        at,
        ok: gate.ok(),
        upto: gate.upto,
        failures: gate
            .failures
            .iter()
            .map(entries::GateFailure::to_json)
            .collect(),
        warnings: gate
            .warnings
            .iter()
            .map(entries::GateFailure::to_json)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review of 662cc79c: a hostile key can't reach the terminal raw, and
    /// a null still shows.
    #[test]
    fn value_text_escapes_keys_and_keeps_nulls() {
        let v = serde_json::json!({"a\u{1b}[2J\nforged": 1, "loop_guard": null});
        let text = value_text(&v);
        assert!(!text.contains('\u{1b}') && !text.contains('\n'), "{text:?}");
        assert!(text.contains("\\u{1b}[2J"), "{text:?}");
        assert!(text.contains("loop_guard: null}"), "{text:?}");
        assert!(text.starts_with('{') && !text.contains('"'), "{text:?}");
        // As fields, nested objects nest by indent and keep nulls; keys
        // stay escaped.
        let v = serde_json::json!({
            "values": {"harness": "claude", "role": "worker"},
            "origins": {"/agents/x/role": {"layer": "definition"}},
            "loop_guard": null,
            "k\u{1b}[2J": 1,
        });
        let lines = field_lines("  ", &v);
        assert!(lines.contains(&"  values:".to_string()), "{lines:#?}");
        assert!(
            lines.contains(&"    harness: claude".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"    /agents/x/role:".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"      layer: definition".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"  loop_guard: null".to_string()),
            "{lines:#?}"
        );
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')), "{lines:#?}");
        // A bad op field's hint names its flag, renamed or positional,
        // and nothing for a key no command takes (review of 6baee0b9).
        assert_eq!(bad_op_flag("title"), Some(Given::Flag("title")));
        assert_eq!(bad_op_flag("refs"), Some(Given::Flag("ref")));
        assert_eq!(bad_op_flag("commits"), Some(Given::Flag("commit")));
        assert_eq!(bad_op_flag("asked_of"), Some(Given::Flag("asked-of")));
        assert_eq!(bad_op_flag("round"), Some(Given::Argument));
        assert_eq!(bad_op_flag("req"), Some(Given::Argument));
        assert_eq!(bad_op_flag("bogus"), None);
        // A list prints `- ` items, an object's fields under it, so a
        // name holding `, ` stays one item; never JSON (reviews of
        // 4d8a588f and 40ae015b).
        let v = serde_json::json!({
            "reviews": [{"by": "terra", "verdict": "clean"}, "plain"],
            "paths": ["src/lib.rs", "alpha, beta"],
        });
        let lines = field_lines("", &v);
        assert_eq!(
            lines,
            [
                "reviews:",
                "  - by: terra",
                "    verdict: clean",
                "  - plain",
                "paths:",
                "  - src/lib.rs",
                "  - alpha, beta",
            ],
            "{lines:#?}"
        );
        assert!(lines.iter().all(|l| !l.contains('"')), "{lines:#?}");
    }

    /// Anchoring's main ref is the common config's `mdkg.mainRef`, else
    /// `main` (Opus's #6482, finding 1; the orchestrator on Terra's #6548,
    /// calls 2 and 3): it takes no environment, and a name git can't take
    /// as a branch is an error, never an absent main ref.
    #[test]
    fn anchoring_reads_only_the_configured_main_ref() {
        let dir = std::env::temp_dir().join(format!("mdkg-mainref-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir_all(&dir).expect("dir");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        let common = dir.join(".git");
        let main_ref = || anchoring_main_ref(&dir, &common);
        assert_eq!(
            main_ref().expect("default").as_deref(),
            Some("refs/heads/main")
        );
        git(&["config", "mdkg.mainRef", "refs/heads/trunk"]);
        assert_eq!(
            main_ref().expect("configured").as_deref(),
            Some("refs/heads/trunk")
        );
        // A remote-tracking main is allowed now graph.ref separates the
        // refs (the orchestrator on #10737), in full or, when no local
        // branch has the name, as a bare name.
        git(&["config", "mdkg.mainRef", "refs/remotes/o/main"]);
        assert_eq!(
            main_ref().expect("remote").as_deref(),
            Some("refs/remotes/o/main")
        );
        // A bad name, revision syntax or a ref that's neither a local
        // branch nor a remote-tracking ref fails closed (Terra's #7598,
        // finding 3).
        for bad in ["bad..name", "@{-1}", "main@{u}", "refs/tags/x", "-x"] {
            git(&["config", "mdkg.mainRef", bad]);
            assert!(main_ref().is_err(), "{bad} fails closed");
        }
        // An absent main ref is positively absent: no anchoring on main.
        git(&["config", "mdkg.mainRef", "trunk"]);
        assert_eq!(
            object_at(&common, "refs/heads/trunk^{commit}").expect("read"),
            None
        );
        // The upstream follows the remote's fetch refspec (Terra's #7598,
        // finding 2), and none configured is none.
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        assert_eq!(configured_upstream(&common, "main").expect("none"), None);
        git(&["remote", "add", "origin", "https://example.invalid/x.git"]);
        git(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/tracked/*",
        ]);
        git(&["config", "branch.main.remote", "origin"]);
        git(&["config", "branch.main.merge", "refs/heads/main"]);
        assert_eq!(
            configured_upstream(&common, "main")
                .expect("mapped")
                .as_deref(),
            Some("refs/remotes/origin/tracked/main")
        );
        // A bare name no local branch has, held by a remote-tracking ref,
        // names that ref; one a local branch has stays local.
        git(&["update-ref", "refs/remotes/o/trunk", "HEAD"]);
        git(&["config", "mdkg.mainRef", "o/trunk"]);
        assert_eq!(
            main_ref().expect("bare remote").as_deref(),
            Some("refs/remotes/o/trunk")
        );
        git(&["config", "mdkg.mainRef", "main"]);
        assert_eq!(
            main_ref().expect("bare local").as_deref(),
            Some("refs/heads/main")
        );
        // In ref mode, g-core's `resolve().main_ref`: the config value
        // only, so unset is no code base, never `main`.
        git(&["config", "mdkg.graph.ref", "refs/heads/graph"]);
        git(&["config", "mdkg.mainRef", "o/trunk"]);
        assert_eq!(
            main_ref().expect("ref mode").as_deref(),
            Some("refs/remotes/o/trunk")
        );
        git(&["config", "--unset", "mdkg.mainRef"]);
        assert_eq!(main_ref().expect("unset"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Terra's code review #10357, minor 2: once read, an env's git answers
    /// for full ids are kept, so the settle's evaluation inside the
    /// transaction spawns no git for what `warm` read; an abbreviated id
    /// or a ref is read again every time.
    #[test]
    fn an_envs_git_answers_for_full_ids_are_read_once() {
        let dir = std::env::temp_dir().join(format!("mdkg-gitmemo-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir_all(dir.join(".mdkg")).expect("dir");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::write(
            dir.join(".mdkg/config.json"),
            r#"{"schema_version": 3, "format_version": "0.7", "work": {"location": "git"}}"#,
        )
        .expect("config");
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        let one = git(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("a.txt"), "a\n").expect("file");
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "two"]);
        let two = git(&["rev-parse", "HEAD"]);
        let repo = Repo::open(&dir).expect("repo");
        let mut env = RepoEnv::git_only(&repo).expect("env");
        let read = |env: &RepoEnv| {
            let entry = env
                .tree_entry(&two, "a.txt")
                .expect("entry")
                .expect("a.txt");
            let oid = entry
                .split('\t')
                .next()
                .and_then(|h| h.split(' ').nth(2))
                .expect("oid");
            (
                entry.clone(),
                env.blob(oid).expect("blob"),
                env.contains(&one, &two).expect("contains"),
                env.changed_paths(&one, &two).expect("changed"),
                env.commit_paths(&two).expect("paths"),
                env.latest_heads(std::slice::from_ref(&one), &two)
                    .expect("heads"),
            )
        };
        let first = read(&env);
        assert_eq!(first.1, b"a\n");
        assert!(first.2);
        // With git gone from under it, the env answers from what it read.
        env.common = dir.join("no-such-common-dir");
        assert_eq!(read(&env), first);
        assert!(
            env.tree_entry("HEAD", "a.txt").is_err(),
            "a ref is read again"
        );
        assert!(env.contains(&one[..12], &two).is_err(), "so is a short id");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Terra's #8445: the reader opens through the repository's checks,
    /// so a nested repository git skipped (its HEAD gone) never reads the
    /// store of the repository around it.
    #[test]
    fn the_requirement_reader_never_reads_an_outer_store() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-req-reader-{}", mdkg_format::Uuid7::now()));
        let inner = dir.join("inner");
        std::fs::create_dir_all(inner.join(".mdkg")).expect("dir");
        let git = |at: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}");
        };
        std::fs::write(
            inner.join(".mdkg/config.json"),
            json!({"schema_version": 3, "format_version": "0.7",
                   "graph_id": "0192f3a0-0000-7000-8000-000000000000"})
            .to_string(),
        )
        .expect("config");
        git(&dir, &["init", "-q", "-b", "main"]);
        drop(mdkg_coord_sqlite::Store::open_for_repo(&dir, 1_000).expect("outer store"));
        git(&inner, &["init", "-q", "-b", "main"]);
        drop(mdkg_coord_sqlite::Store::open_for_repo(&inner, 1_000).expect("inner store"));
        // Refused while git skips it (first: git reads are cached per
        // process once they succeed).
        std::fs::rename(inner.join(".git/HEAD"), inner.join(".git/HEAD.away")).expect("move HEAD");
        let e = requirement_reader(&inner).expect_err("skipped");
        assert_eq!(e.code, Code::CoordStoreFailed);
        std::fs::rename(inner.join(".git/HEAD.away"), inner.join(".git/HEAD"))
            .expect("restore HEAD");
        let reader = requirement_reader(&inner).expect("the inner store");
        assert!(
            reader
                .states(".mdkg", "task-01a1878e-3e6f-710b-9d01-68d002aed3e3")
                .expect("states")
                .is_empty()
        );
        drop(reader);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn requirement_ids_come_from_the_section_outside_fences() {
        let body = "# Overview\n- R9: not here\n\n# Requirements\n\n- R1: The store shall keep it.\n- R2 (changes edd-x#R1): When x, y shall z.\n  - R5: nested, not an item\n- R3 (retired): gone\n```\n- R4: an example\n```\n## Requirements\n- R7: second section\n# Notes\n- R8: after\n";
        assert_eq!(requirement_ids(body), vec!["R1", "R2", "R7"]);
        // A duplicate id counts once, and a retired item never.
        let body = "# Requirements\n- R1: The store shall keep it.\n- R1: The store shall copy it.\n- R2 (retired): gone\n";
        assert_eq!(requirement_ids(body), vec!["R1"]);
    }

    /// When extraction is uncertain, the gate counts what the parser
    /// extracted and nothing more; the lint warns about the rest (the
    /// orchestrator's call on #11710: warnings, never refusals, A29
    /// condition 4).
    #[test]
    fn requirement_ids_on_uncertain_bodies_are_what_the_parser_extracted() {
        // An unclosed fence hides every item after it.
        let body = "# Requirements\n- R1: The store shall keep it.\n```\n- R2: The store shall copy it.\n- R3: The store shall sort it.\n";
        assert!(mdkg_graph::requirements::parse(body).uncertain());
        assert_eq!(requirement_ids(body), vec!["R1"]);
        // A malformed item is a bad line, not a requirement.
        let body =
            "# Requirements\n- R1: The store shall keep it.\n- R4 The store shall copy it.\n";
        assert!(mdkg_graph::requirements::parse(body).uncertain());
        assert_eq!(requirement_ids(body), vec!["R1"]);
    }

    #[test]
    fn an_op_subcommand_builds_only_the_fields_it_was_given() {
        let a = OpArgs {
            node: "n".into(),
            entry: Some("V1".into()),
            kind: None,
            title: None,
            text: None,
            severity: None,
            anchor: None,
            requirement: vec![],
            asked_of: None,
            blocking: false,
            slot: None,
            refs: vec![],
            supersedes: vec![],
            sha: None,
            commits: vec![],
            supersede: false,
            at: None,
            of: None,
            to: None,
            state: None,
            range: Some("a1..b2".into()),
            verdict: Some("clean".into()),
            json: false,
        };
        let obj = object_of("review.complete", &a);
        assert_eq!(
            Value::Object(obj.clone()),
            json!({"op": "review.complete", "round": "V1", "range": "a1..b2", "verdict": "clean"})
        );
        assert!(Op::from_object(&obj).is_ok());
    }

    /// Only the exact lowercase `human:` with a well-formed name is a
    /// person, wherever the class is decided (Opus's final pass #11368,
    /// finding 1).
    #[test]
    fn only_a_lowercase_human_with_a_valid_name_is_a_person() {
        assert!(is_person("human:nick"));
        assert!(is_person("human:nick.reames"));
        for other in [
            "HUMAN:nick",
            "Human:nick",
            "human:Ana",
            "human:",
            "human:a,b",
            "human:nick\u{202e}",
            "nick",
            "role:reviewer",
        ] {
            assert!(!is_person(other), "{other:?} isn't a person");
        }
    }

    /// A stored value that doesn't read back as its type byte for byte,
    /// and a key the record doesn't name, print as stored.
    #[test]
    fn an_entry_record_keeps_values_its_types_dont_fit() {
        let data = json!({
            "node": "task-01a1878e-3e6f-710b-9d01-68d002aed3e3",
            "opened": {"by": "rev", "at": 5, "message": null, "turn": null},
            "anchor": "src/lib.rs:3",
            "fix": {"sha": "abc", "commits": [], "by": "wrk", "at": 6, "extra": 1},
            "verified": {"sha": "abc", "by": "rev", "at": 7, "review": null},
            "anchored": null,
            "legacy": {"kept": true},
            "state": "shadowed",
        });
        let e = Entry {
            node: NodeLink::of("task-01a1878e-3e6f-710b-9d01-68d002aed3e3").expect("node"),
            id: "F1".into(),
            kind: Kind::Finding,
            state: "fixed".into(),
            severity: Some("major".into()),
            slot: None,
            blocking: false,
            title: "Off by one".into(),
            text: None,
            data: data.as_object().expect("data").clone(),
            opened_ms: 5,
            updated_ms: 6,
        };
        let record = EntryRecord::from(&e);
        assert!(record.opened.is_some() && record.anchored == Some(None));
        assert!(record.anchor.is_none() && record.fix.is_none() && record.verified.is_none());
        assert_eq!(
            record.other.keys().collect::<Vec<_>>(),
            ["anchor", "fix", "verified", "legacy"]
        );
        let printed = serde_json::to_string(&record).expect("json");
        assert_eq!(printed.matches("\"state\"").count(), 1, "{printed}");
        let mut expected = e.to_json();
        expected["state"] = json!("fixed");
        assert_eq!(serde_json::to_value(&record).expect("json"), expected);
    }
}
