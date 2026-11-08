//! The ledger (edd-01a1878e, "mdkg flow", A27 and A29's gate): small,
//! durable, typed entries on a node, each with a lifecycle tied to git
//! SHAs.
//!
//! - **Entries.** Questions, decisions, findings, issues, review rounds and
//!   requirement states, keyed by the node's UUID, so a retype or a move of
//!   the node file keeps them. Ids are per node and per kind letter (`F3`),
//!   allocated inside the write transaction and never reused; a
//!   requirement's entry takes the requirement's own id (`R2`).
//! - **Transitions.** Every op is checked, in this order, against the entry
//!   it names, then "Who may do what" (keyed on the actor's [`Class`]), then
//!   the kind's state machine, then the SHAs it cites. A refused op writes
//!   nothing: no row, no record.
//! - **Records.** Each change writes one `entry_events` row and one log
//!   record (`entry.open` or `entry.change`) in the same transaction. The
//!   record carries the entry's whole state after the change as a
//!   `snapshot`, without its texts: titles, texts and notes enter the log
//!   only as digests, like message bodies.
//! - **Untrusted text.** Titles and texts are data. The mail secret filter
//!   is the caller's (the CLI runs it before any op reaches the store); here
//!   they are only bounded.
//! - **Trust.** The class rules are guard rails inside one trust domain
//!   (API v1, "The trust model"): actor names are believed. They stop
//!   accidents and quoted-text injection, not a hostile actor.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::ledger::{NodeLink, check_entry_id};
use crate::store::{Store, StoreError};

/// A title's bounds, in bytes.
pub const TITLE_MAX: usize = 200;
/// A text's (or a note's) bound, in bytes.
pub const TEXT_MAX: usize = 4 * 1024;
/// A finding with no severity is taken as this one, so a reviewer who
/// leaves it out never lets a finding past the gate.
pub const DEFAULT_SEVERITY: &str = "major";

fn ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or_default()
}

fn sql(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// An entry's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Question,
    Decision,
    Finding,
    Issue,
    Review,
    Req,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Question,
        Kind::Decision,
        Kind::Finding,
        Kind::Issue,
        Kind::Review,
        Kind::Req,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Decision => "decision",
            Kind::Finding => "finding",
            Kind::Issue => "issue",
            Kind::Review => "review",
            Kind::Req => "req",
        }
    }

    /// The letter its ids start with.
    pub fn letter(self) -> char {
        match self {
            Kind::Question => 'Q',
            Kind::Decision => 'D',
            Kind::Finding => 'F',
            Kind::Issue => 'I',
            Kind::Review => 'V',
            Kind::Req => 'R',
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }

    /// The kind an entry id names, by its letter.
    pub fn of_id(id: &str) -> Option<Self> {
        let letter = id.chars().next()?;
        Self::ALL.into_iter().find(|k| k.letter() == letter)
    }

    /// Whether `state` is one of this kind's terminal states (A27, rule 1).
    pub fn is_terminal(self, state: &str) -> bool {
        match self {
            Kind::Question => matches!(state, "closed" | "withdrawn" | "moved"),
            Kind::Decision => matches!(state, "superseded" | "rejected"),
            Kind::Finding => matches!(
                state,
                "verified" | "closed" | "residual" | "duplicate" | "outdated"
            ),
            Kind::Issue => matches!(state, "resolved" | "residual" | "moved"),
            Kind::Review => state == "closed",
            Kind::Req => state == "waived",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An actor's class for "Who may do what" (A27, rule 4), from what agent
/// definitions already hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// An agent whose definition has `role: "worker"`.
    Worker,
    /// An agent whose definition has `role: "reviewer"`.
    Reviewer,
    /// A `human:*` actor.
    Person,
    /// On node N: an actor that isn't `human:*`, named as the
    /// `reports_to` of N's worker, an agent other than itself, and holding
    /// no live lease on N (A27 rule 4, R52). Judged per node, inside the
    /// op's write transaction ([`Env::definitions`]).
    Coordinator,
    Other,
}

/// Whether the ledger classes `actor` as a person: the exact lowercase
/// `human:` and a name of 1 to 120 of `a-z`, `0-9`, `.`, `_`, `-` and `:`,
/// the rule `msg send` holds a `human:` mailbox to. The one check the
/// store ([`Class::of`]) and the CLI both make, so they can't disagree;
/// any other spelling is no person (Opus #11368 and #11514).
pub fn is_person(actor: &str) -> bool {
    actor.strip_prefix("human:").is_some_and(|name| {
        !name.is_empty()
            && name.len() <= 120
            && name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | ':')
            })
    })
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Worker => "worker",
            Class::Reviewer => "reviewer",
            Class::Person => "person",
            Class::Coordinator => "coordinator",
            Class::Other => "other",
        }
    }

    /// The class of `actor` by its own definition, given every agent
    /// definition the graph branch commits: never `Coordinator`, which is
    /// judged per node ([`class_on`]).
    pub fn of(actor: &str, agents: &[AgentFacts]) -> Self {
        if is_person(actor) {
            return Class::Person;
        }
        match agents.iter().find(|a| a.name == actor) {
            Some(agent) => match agent.role.as_deref() {
                Some("reviewer") => Class::Reviewer,
                Some("worker") => Class::Worker,
                _ => Class::Other,
            },
            None => Class::Other,
        }
    }

    /// The agents whose definitions name `actor` as their `reports_to`.
    pub fn subordinates(actor: &str, agents: &[AgentFacts]) -> Vec<String> {
        agents
            .iter()
            .filter(|a| a.name != actor && a.reports_to.as_deref() == Some(actor))
            .map(|a| a.name.clone())
            .collect()
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an agent definition says that classes depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFacts {
    pub name: String,
    pub role: Option<String>,
    pub reports_to: Option<String>,
}

/// Why an entry read or write failed. Nothing here echoes an entry's or
/// an op's text: only ids, states, field names, classes and SHAs.
#[derive(Debug)]
pub enum EntryError {
    Store(StoreError),
    /// An op object that doesn't parse; names the field or the reason.
    BadOp(String),
    /// A title or text out of bounds; names which.
    BadText(&'static str),
    /// No entry has this path (`<node-id>#F3`).
    NotFound(String),
    /// An entry named without a node, where no node is implied.
    NoNode,
    /// A node handle that doesn't resolve.
    NodeNotFound(String),
    /// The entry's state doesn't allow this op (A27, rule 1).
    BadTransition {
        entry: String,
        state: String,
        op: String,
    },
    /// The actor's class may not make this op (A27, rule 4).
    NotAllowed {
        class: Class,
        op: String,
    },
    /// A reopen whose reviewed SHA doesn't hold the fix (A27, rule 2).
    FixAfterRange {
        entry: String,
        fix: String,
        at: String,
    },
    /// A verify whose SHA doesn't hold the fix (A27, rule 3).
    NotCovered {
        entry: String,
        fix: String,
        at: String,
    },
    /// A verify at a SHA where the fix is undone: some fix commit's paths
    /// all read there as at that commit's first parent (A27, rule 3).
    FixAbsent {
        entry: String,
        commit: String,
        at: String,
    },
    /// A `review.complete` whose range isn't its round's (A27, rule 8).
    RangeMismatch {
        entry: String,
    },
    /// A `findings` verdict with no finding the completing reviewer
    /// opened, reopened or seconded on the node since the round opened
    /// (A27 rule 8, "A `findings` verdict is backed").
    VerdictUnbacked {
        entry: String,
    },
    /// A SHA that isn't in the repository, or isn't unique (A27, rule 5).
    UnknownCommit(String),
    /// Git couldn't answer.
    Git(String),
    /// The actor's class can't be judged, so the op is refused (A27 rule
    /// 4, failing closed): the graph ref is missing or holds a definition
    /// that doesn't validate, a running agent's saved launcher spec
    /// differs from its committed `role` or `reports_to` (names the
    /// `<agent>.<field>`s), or a turn record can't be read.
    Unclassed(String),
    /// More walks of review rounds than are judged (Terra's code review
    /// #6548): coverage is refused rather than computed without a bound.
    WalkLimit(usize),
}

impl EntryError {
    /// The diagnostic code (A27); `None` for a store failure, which the
    /// caller reports as it reports any.
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::Store(_) => return None,
            Self::BadOp(_) => "entry.bad_op",
            Self::BadText(_) => "entry.bad_text",
            Self::NotFound(_) => "entry.not_found",
            Self::NoNode => "entry.no_node",
            Self::NodeNotFound(_) => "node.not_found",
            Self::BadTransition { .. } => "entry.bad_transition",
            Self::NotAllowed { .. } | Self::Unclassed(_) => "entry.not_allowed",
            Self::FixAfterRange { .. } => "entry.fix_after_range",
            Self::NotCovered { .. } | Self::WalkLimit(_) => "entry.not_covered",
            Self::FixAbsent { .. } => "entry.fix_absent",
            Self::RangeMismatch { .. } => "entry.range_mismatch",
            Self::VerdictUnbacked { .. } => "entry.verdict_unbacked",
            Self::UnknownCommit(_) => "entry.unknown_commit",
            Self::Git(_) => "entry.git",
        })
    }

    /// Facts for a diagnostic's `data`: ids, states, classes, SHAs.
    pub fn data(&self) -> Value {
        match self {
            Self::BadOp(field) => json!({"field": field}),
            Self::BadText(field) => json!({"field": field}),
            Self::NotFound(entry)
            | Self::RangeMismatch { entry }
            | Self::VerdictUnbacked { entry } => json!({"entry": entry}),
            Self::NodeNotFound(node) => json!({"node": node}),
            Self::BadTransition { entry, state, op } => {
                json!({"entry": entry, "state": state, "op": op})
            }
            Self::NotAllowed { class, op } => json!({"class": class.as_str(), "op": op}),
            Self::FixAfterRange { entry, fix, at } | Self::NotCovered { entry, fix, at } => {
                json!({"entry": entry, "fix": fix, "at": at})
            }
            Self::FixAbsent { entry, commit, at } => {
                json!({"entry": entry, "commit": commit, "at": at})
            }
            Self::UnknownCommit(sha) => json!({"sha": sha}),
            Self::WalkLimit(limit) => json!({"limit": limit}),
            Self::Store(_) | Self::NoNode | Self::Git(_) | Self::Unclassed(_) => json!({}),
        }
    }
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::BadOp(field) if field == "op" => write!(f, "not a ledger op"),
            // A missing or malformed field names the field, never reads
            // as an unknown op (e2e N5).
            Self::BadOp(field) => write!(
                f,
                "the op's {field} is missing, not valid, or not one this op takes"
            ),
            Self::BadText(field) => write!(
                f,
                "the {field} is out of bounds: a title is 1 to {TITLE_MAX} bytes on one line, \
                 a text at most {TEXT_MAX}"
            ),
            Self::NotFound(entry) => write!(f, "no entry {entry}"),
            Self::NoNode => f.write_str("the entry names no node, and none is implied"),
            Self::NodeNotFound(node) => write!(f, "no node {node}"),
            Self::BadTransition { entry, state, op } => {
                write!(f, "{entry} is {state}; `{op}` doesn't apply to it")
            }
            Self::NotAllowed { class, op } => write!(f, "a {class} may not `{op}`"),
            Self::FixAfterRange { entry, fix, at } => write!(
                f,
                "{entry}'s fix {fix} landed after the reviewed {at}: review a range that holds it"
            ),
            Self::NotCovered { entry, fix, at } => {
                write!(f, "{at} doesn't hold {entry}'s fix {fix}")
            }
            Self::RangeMismatch { entry } => {
                write!(
                    f,
                    "that isn't {entry}'s range: complete the round you reviewed"
                )
            }
            Self::VerdictUnbacked { entry } => write!(
                f,
                "a findings verdict on {entry} needs a finding you opened, reopened or seconded \
                 on the node since the round opened: add it, or complete it as clean"
            ),
            Self::FixAbsent { entry, commit, at } => write!(
                f,
                "{at} undoes {entry}'s fix commit {commit}: every path it changed reads as before it"
            ),
            Self::UnknownCommit(sha) => write!(f, "no commit {sha} here: fetch it first"),
            Self::Git(e) => write!(f, "git: {e}"),
            Self::Unclassed(why) => write!(f, "the actor's class can't be judged: {why}"),
            Self::WalkLimit(limit) => write!(
                f,
                "the node's review rounds make more than {limit} walks to judge: coverage is refused"
            ),
        }
    }
}

impl std::error::Error for EntryError {}

impl From<StoreError> for EntryError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for EntryError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

impl From<serde_json::Error> for EntryError {
    fn from(e: serde_json::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

/// What the ledger needs from outside the store: node handles resolved
/// against the graph index, and SHAs checked against the repository's
/// pinned common directory, never an agent's checkout (A27, rule 5).
pub trait Env {
    /// The node a handle (an id, a qid or a unique prefix) names.
    fn node(&self, handle: &str) -> Result<NodeLink, EntryError>;
    /// The full SHA a full or uniquely abbreviated one names;
    /// [`EntryError::UnknownCommit`] when there's none.
    fn commit(&self, sha: &str) -> Result<String, EntryError>;
    /// Whether `ancestor` is `head` or in its history (both full).
    fn contains(&self, ancestor: &str, head: &str) -> Result<bool, EntryError>;
    /// Whether `actor` holds the `role:<name>` lease `role` names now.
    fn holds(&self, actor: &str, role: &str) -> bool;
    /// The actor's class by its own definition, and the agents whose
    /// definitions name it as their `reports_to`, read when called: the
    /// store calls it inside the op's write transaction, so no definition
    /// read earlier stands in (A27 rule 4, "When it's judged"). `None`
    /// keeps the change's own class, with no subordinates.
    fn definitions(&self, _actor: &str) -> Result<Option<(Class, Vec<String>)>, EntryError> {
        Ok(None)
    }
    /// Whether `actor` is inside a launcher turn. For an agent: its D6
    /// progress record holds a turn that started and hasn't ended, or this
    /// process is that agent's turn. For a `human:*` actor: this process is
    /// marked as some turn's (`MDKG_RUN_TURN`, or the D6 record of the
    /// agent `MDKG_ACTOR` names). A coordinator-class op through the CLI or
    /// MCP gets only the actor's own class then, and a person op is
    /// refused: inside a turn an agent acts only through its ledger block
    /// (A27 rule 4, A32).
    fn in_turn(&self, _actor: &str) -> Result<bool, EntryError> {
        Ok(false)
    }
    /// The node folder holding `node`, as lease keys spell it
    /// (`<folder>#<id>`, lowercase), read from the graph ref, never the
    /// invoking checkout (the orchestrator on Terra's #6919, call 2).
    fn node_folder(&self, _node: &NodeLink) -> Result<NodeFolder, EntryError> {
        Ok(NodeFolder::Any)
    }
    /// The main ref's tip and its upstream's (full SHAs), read-only;
    /// `None` only when the main ref positively doesn't resolve (A27 rule
    /// 8, anchoring). The git reads below default to an error, so an env
    /// that doesn't implement them refuses rather than anchors or passes
    /// (Terra's code review #6548).
    fn main_tips(&self) -> Result<Option<Vec<String>>, EntryError> {
        Err(EntryError::Git("this env doesn't read the main ref".into()))
    }
    /// Of `heads` (full SHAs), the ones that are `at` or in its history
    /// and that no other such head descends from (`at` itself when it is
    /// one), in one walk of `at`'s history and one `merge-base
    /// --independent`, never a check per pair (Terra's code review #9504,
    /// major 5). A head not present locally is in no history here.
    fn latest_heads(&self, _heads: &[String], _at: &str) -> Result<Vec<String>, EntryError> {
        Err(EntryError::Git("this env doesn't walk history".into()))
    }
    /// Every path the commits in `base..head` changed, merges against
    /// their first parent only (`log --diff-merges=first-parent
    /// --no-renames --name-only -z --format=`), with `base..head`'s own
    /// diff; renames count as both paths (A29; the orchestrator on
    /// Terra's #6548: first-parent semantics throughout).
    fn changed_paths(&self, _base: &str, _head: &str) -> Result<BTreeSet<String>, EntryError> {
        Err(EntryError::Git(
            "this env doesn't list changed paths".into(),
        ))
    }
    /// The paths commit `c` changed against its first parent
    /// (`diff-tree -r --no-commit-id --no-renames --name-only -z <c>^1 <c>`,
    /// `--root` for a root commit), and that parent (`None` for a root).
    fn commit_paths(&self, _c: &str) -> Result<(BTreeSet<String>, Option<String>), EntryError> {
        Err(EntryError::Git(
            "this env doesn't list a commit's paths".into(),
        ))
    }
    /// `path`'s entry in commit `at`'s tree as `ls-tree -z --full-tree
    /// --literal-pathspecs` gives it (mode, type, object id and name), or
    /// `None` when it's absent there.
    fn tree_entry(&self, _at: &str, _path: &str) -> Result<Option<String>, EntryError> {
        Err(EntryError::Git("this env doesn't read trees".into()))
    }
    /// A blob's bytes by object id, for the node file's own comparison
    /// (A29, "The node's own file").
    fn blob(&self, _oid: &str) -> Result<Vec<u8>, EntryError> {
        Err(EntryError::Git("this env doesn't read blobs".into()))
    }
}

/// Which leases are on a node, by the folder of the graph holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeFolder {
    /// Leases on its id, in any folder: an env that reads no graph.
    Any,
    /// Leases keyed `<folder>#<id>` in this folder (lowercase).
    In(String),
    /// None: the node isn't on the graph ref, so no lease there makes
    /// anyone its worker, and no actor is its coordinator.
    Nowhere,
}

/// An entry named in an op: `F3`, or `<node>#F3`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRef {
    pub node: Option<String>,
    pub id: String,
}

impl EntryRef {
    pub fn parse(s: &str) -> Result<Self, EntryError> {
        let (node, id) = match s.rsplit_once('#') {
            Some((node, id)) if !node.is_empty() => (Some(node.to_string()), id),
            Some(_) => return Err(EntryError::BadOp("entry".into())),
            None => (None, s),
        };
        check_entry_id(id).map_err(|_| EntryError::BadOp("entry".into()))?;
        Ok(Self {
            node,
            id: id.to_string(),
        })
    }
}

/// A finding's place: a repository path and, optionally, a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub line: Option<u64>,
}

impl Anchor {
    fn to_json(&self) -> Value {
        json!({"path": self.path, "line": self.line})
    }
}

/// A requirement's state, as a `req` op sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReqState {
    Met,
    Unmet,
    Waived,
}

impl ReqState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReqState::Met => "met",
            ReqState::Unmet => "unmet",
            ReqState::Waived => "waived",
        }
    }
}

/// A reviewer's verdict on a round (`review.complete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Findings,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Clean => "clean",
            Verdict::Findings => "findings",
        }
    }
}

/// A new entry's content (`add`, `propose`, `decide`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct New {
    pub title: String,
    pub text: Option<String>,
    pub severity: Option<String>,
    pub anchor: Option<Anchor>,
    pub requirement: Vec<String>,
    pub asked_of: Option<String>,
    pub blocking: bool,
    pub slot: Option<String>,
    pub refs: Vec<EntryRef>,
    /// For decisions: what they supersede, on the same node.
    pub supersedes: Vec<EntryRef>,
}

/// One ledger op, in the form a ledger block, `mdkg entry update` and the
/// MCP `entry_update` tool share (A32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Opens a finding, an issue, a question (`ask`) or an accepted
    /// decision.
    Add {
        kind: Kind,
        new: New,
    },
    /// Opens a decision as `proposed`.
    Propose(New),
    /// With `entry`, accepts a proposed decision; without, opens one as
    /// `accepted`.
    Decide {
        entry: Option<EntryRef>,
        new: Option<New>,
    },
    Fix {
        entry: EntryRef,
        sha: String,
        /// Every commit of the fix; `[sha]` when none is named (A27).
        commits: Vec<String>,
        /// On a reopened finding, by one of its fixers: these commits
        /// replace every one declared before (op `supersede`).
        supersede: bool,
    },
    Verify {
        entry: EntryRef,
        at: String,
    },
    Reopen {
        entry: EntryRef,
        at: Option<String>,
        text: Option<String>,
    },
    Reject {
        entry: EntryRef,
        text: String,
    },
    Dispute {
        entry: EntryRef,
        text: String,
    },
    Residual {
        entry: EntryRef,
        text: String,
        /// For a verified finding that needs re-verify: the head whose
        /// walk's final SHA the residual is bound to (A29, "Re-verify").
        at: Option<String>,
    },
    Outdated {
        entry: EntryRef,
        text: Option<String>,
    },
    Duplicate {
        entry: EntryRef,
        of: EntryRef,
    },
    Answer {
        entry: EntryRef,
        text: String,
    },
    Close {
        entry: EntryRef,
    },
    Withdraw {
        entry: EntryRef,
    },
    Resolve {
        entry: EntryRef,
    },
    Move {
        entry: EntryRef,
        to: String,
    },
    Req {
        req: String,
        state: ReqState,
        at: Option<String>,
        text: Option<String>,
    },
    /// A named reviewer completes a review round (A27, rule 8): the only
    /// op that moves a round.
    ReviewComplete {
        round: EntryRef,
        base: String,
        sha: String,
        verdict: Verdict,
    },
}

impl Action {
    /// The op's name, as op objects spell it.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Add { .. } => "add",
            Action::Propose(_) => "propose",
            Action::Decide { .. } => "decide",
            Action::Fix { .. } => "fix",
            Action::Verify { .. } => "verify",
            Action::Reopen { .. } => "reopen",
            Action::Reject { .. } => "reject",
            Action::Dispute { .. } => "dispute",
            Action::Residual { .. } => "residual",
            Action::Outdated { .. } => "outdated",
            Action::Duplicate { .. } => "duplicate",
            Action::Answer { .. } => "answer",
            Action::Close { .. } => "close",
            Action::Withdraw { .. } => "withdraw",
            Action::Resolve { .. } => "resolve",
            Action::Move { .. } => "move",
            Action::Req { .. } => "req",
            Action::ReviewComplete { .. } => "review.complete",
        }
    }
}

/// An op and the node it's about, when it names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub node: Option<String>,
    pub action: Action,
}

/// Reads an op object's fields, refusing unknown ones and wrong types.
/// Refusals name the field only, never its value.
struct Fields<'a> {
    obj: &'a Map<String, Value>,
    used: BTreeSet<&'static str>,
}

impl<'a> Fields<'a> {
    fn new(obj: &'a Map<String, Value>) -> Self {
        let mut used = BTreeSet::new();
        used.insert("op");
        used.insert("node");
        Self { obj, used }
    }

    fn bad(field: &str) -> EntryError {
        EntryError::BadOp(field.to_string())
    }

    fn str(&mut self, key: &'static str) -> Result<Option<String>, EntryError> {
        self.used.insert(key);
        match self.obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(Self::bad(key)),
        }
    }

    fn need(&mut self, key: &'static str) -> Result<String, EntryError> {
        self.str(key)?.ok_or_else(|| Self::bad(key))
    }

    fn entry(&mut self, key: &'static str) -> Result<EntryRef, EntryError> {
        EntryRef::parse(&self.need(key)?).map_err(|_| Self::bad(key))
    }

    fn entries(&mut self, key: &'static str) -> Result<Vec<EntryRef>, EntryError> {
        self.strs(key)?
            .iter()
            .map(|s| EntryRef::parse(s).map_err(|_| Self::bad(key)))
            .collect()
    }

    fn strs(&mut self, key: &'static str) -> Result<Vec<String>, EntryError> {
        self.used.insert(key);
        match self.obj.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(vec![s.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_string).ok_or_else(|| Self::bad(key)))
                .collect(),
            Some(_) => Err(Self::bad(key)),
        }
    }

    fn bool(&mut self, key: &'static str) -> Result<bool, EntryError> {
        self.used.insert(key);
        match self.obj.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(Self::bad(key)),
        }
    }

    fn anchor(&mut self) -> Result<Option<Anchor>, EntryError> {
        self.used.insert("anchor");
        let bad = || Self::bad("anchor");
        match self.obj.get("anchor") {
            None | Some(Value::Null) => Ok(None),
            // `path:line`, or a bare path.
            Some(Value::String(s)) => {
                let (path, line) = match s.rsplit_once(':') {
                    Some((p, l)) if !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()) => {
                        (p, Some(l.parse::<u64>().map_err(|_| bad())?))
                    }
                    _ => (s.as_str(), None),
                };
                Ok(Some(Anchor {
                    path: path.to_string(),
                    line,
                }))
            }
            Some(Value::Object(o)) => {
                let path = o.get("path").and_then(Value::as_str).ok_or_else(bad)?;
                let line = match o.get("line") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_u64().ok_or_else(bad)?),
                };
                if o.keys().any(|k| k != "path" && k != "line") {
                    return Err(bad());
                }
                Ok(Some(Anchor {
                    path: path.to_string(),
                    line,
                }))
            }
            Some(_) => Err(bad()),
        }
    }

    fn finish(self) -> Result<(), EntryError> {
        match self.obj.keys().find(|k| !self.used.contains(k.as_str())) {
            Some(k) => Err(Self::bad(k)),
            None => Ok(()),
        }
    }
}

impl Op {
    /// Parses one op object (A32's table). Unknown fields and wrong types
    /// are refused, naming the field.
    pub fn from_object(obj: &Map<String, Value>) -> Result<Self, EntryError> {
        let mut f = Fields::new(obj);
        let node = match obj.get("node") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(Fields::bad("node")),
        };
        let op = match obj.get("op") {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Fields::bad("op")),
        };
        let new = |f: &mut Fields<'_>| -> Result<New, EntryError> {
            Ok(New {
                title: f.need("title")?,
                text: f.str("text")?,
                slot: f.str("slot")?,
                supersedes: f.entries("supersedes")?,
                ..New::default()
            })
        };
        let action = match op.as_str() {
            "add" | "ask" => {
                let kind = if op == "ask" {
                    f.used.insert("kind");
                    match f.obj.get("kind").and_then(Value::as_str) {
                        None | Some("question") => Kind::Question,
                        Some(_) => return Err(Fields::bad("kind")),
                    }
                } else {
                    let name = f.need("kind")?;
                    match Kind::parse(&name) {
                        Some(
                            k @ (Kind::Finding | Kind::Issue | Kind::Question | Kind::Decision),
                        ) => k,
                        _ => return Err(Fields::bad("kind")),
                    }
                };
                let mut n = New {
                    title: f.need("title")?,
                    text: f.str("text")?,
                    severity: f.str("severity")?,
                    anchor: f.anchor()?,
                    requirement: f.strs("requirement")?,
                    asked_of: f.str("asked_of")?,
                    blocking: f.bool("blocking")?,
                    slot: f.str("slot")?,
                    refs: f.entries("refs")?,
                    ..New::default()
                };
                if kind == Kind::Decision {
                    n.supersedes = f.entries("supersedes")?;
                }
                Action::Add { kind, new: n }
            }
            "propose" => Action::Propose(new(&mut f)?),
            "decide" => {
                f.used.insert("entry");
                if obj.contains_key("entry") {
                    Action::Decide {
                        entry: Some(f.entry("entry")?),
                        new: None,
                    }
                } else {
                    Action::Decide {
                        entry: None,
                        new: Some(new(&mut f)?),
                    }
                }
            }
            "fix" => Action::Fix {
                entry: f.entry("entry")?,
                sha: f.need("sha")?,
                commits: f.strs("commits")?,
                supersede: f.bool("supersede")?,
            },
            "verify" => Action::Verify {
                entry: f.entry("entry")?,
                at: f.need("at")?,
            },
            "reopen" => Action::Reopen {
                entry: f.entry("entry")?,
                at: f.str("at")?,
                text: f.str("text")?,
            },
            "reject" => Action::Reject {
                entry: f.entry("entry")?,
                text: f.need("text")?,
            },
            "dispute" => Action::Dispute {
                entry: f.entry("entry")?,
                text: f.need("text")?,
            },
            "residual" => Action::Residual {
                entry: f.entry("entry")?,
                text: f.need("text")?,
                at: f.str("at")?,
            },
            "outdated" => Action::Outdated {
                entry: f.entry("entry")?,
                text: f.str("text")?,
            },
            "duplicate" => Action::Duplicate {
                entry: f.entry("entry")?,
                of: f.entry("of")?,
            },
            "answer" => Action::Answer {
                entry: f.entry("entry")?,
                text: f.need("text")?,
            },
            "close" => Action::Close {
                entry: f.entry("entry")?,
            },
            "withdraw" => Action::Withdraw {
                entry: f.entry("entry")?,
            },
            "resolve" => Action::Resolve {
                entry: f.entry("entry")?,
            },
            "move" => Action::Move {
                entry: f.entry("entry")?,
                to: f.need("to")?,
            },
            "req" => {
                let req = f.need("req")?;
                if !req.starts_with('R') || check_entry_id(&req).is_err() {
                    return Err(Fields::bad("req"));
                }
                let state = match f.need("state")?.as_str() {
                    "met" => ReqState::Met,
                    "unmet" => ReqState::Unmet,
                    "waived" => ReqState::Waived,
                    _ => return Err(Fields::bad("state")),
                };
                Action::Req {
                    req,
                    state,
                    at: f.str("at")?,
                    text: f.str("text")?,
                }
            }
            "review.complete" => {
                let round = f.entry("round")?;
                if !round.id.starts_with('V') {
                    return Err(Fields::bad("round"));
                }
                let range = f.need("range")?;
                let (base, sha) = range
                    .split_once("..")
                    .filter(|(b, s)| !b.is_empty() && !s.is_empty() && !s.starts_with('.'))
                    .ok_or_else(|| Fields::bad("range"))?;
                let verdict = match f.need("verdict")?.as_str() {
                    "clean" => Verdict::Clean,
                    "findings" => Verdict::Findings,
                    _ => return Err(Fields::bad("verdict")),
                };
                Action::ReviewComplete {
                    round,
                    base: base.to_string(),
                    sha: sha.to_string(),
                    verdict,
                }
            }
            _ => return Err(Fields::bad("op")),
        };
        f.finish()?;
        Ok(Op { node, action })
    }
}

/// One entry, as the store holds it. `data` holds the rest of its fields
/// (A27): `node` (its id when opened), `opened`, `anchor`, `requirement`,
/// `asked_of`, `range`, `reviewers`, `answered_by`, `fix`, `verified`,
/// `answer`, `refs`, `duplicate_of`, `superseded_by`, `moved_to` and, for
/// a proposed decision, `supersedes`. Times are milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub node: NodeLink,
    pub id: String,
    pub kind: Kind,
    pub state: String,
    pub severity: Option<String>,
    pub slot: Option<String>,
    pub blocking: bool,
    pub title: String,
    pub text: Option<String>,
    pub data: Map<String, Value>,
    pub opened_ms: u64,
    pub updated_ms: u64,
}

impl Entry {
    /// `<node-id>#F3`, its name across the coordinator.
    pub fn path(&self) -> String {
        format!("{}#{}", self.node.id, self.id)
    }

    fn get_str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    fn get_strs(&self, key: &str) -> Vec<String> {
        self.data
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Who opened it.
    pub fn opened_by(&self) -> Option<&str> {
        self.data
            .get("opened")
            .and_then(|o| o.get("by"))
            .and_then(Value::as_str)
    }

    pub fn asked_of(&self) -> Option<&str> {
        self.get_str("asked_of")
    }

    /// The fix SHA, for a finding that has one.
    pub fn fix_sha(&self) -> Option<&str> {
        self.data
            .get("fix")
            .and_then(|o| o.get("sha"))
            .and_then(Value::as_str)
    }

    /// The SHA it was verified (or, for a requirement, met) at.
    pub fn verified_sha(&self) -> Option<&str> {
        self.data
            .get("verified")
            .and_then(|o| o.get("sha"))
            .and_then(Value::as_str)
    }

    pub fn requirement(&self) -> Vec<String> {
        self.get_strs("requirement")
    }

    pub fn refs(&self) -> Vec<String> {
        self.get_strs("refs")
    }

    pub fn reviewers(&self) -> Vec<String> {
        self.get_strs("reviewers")
    }

    pub fn answered_by(&self) -> Vec<String> {
        self.get_strs("answered_by")
    }

    /// A round's reviewers that have completed it.
    pub fn completed_by(&self) -> Vec<String> {
        self.data
            .get("completed")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("reviewer").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A review's range, `(base, sha)`.
    pub fn range(&self) -> Option<(String, String)> {
        let r = self.data.get("range")?;
        Some((
            r.get("base")?.as_str()?.to_string(),
            r.get("sha")?.as_str()?.to_string(),
        ))
    }

    pub fn is_terminal(&self) -> bool {
        self.kind.is_terminal(&self.state)
    }

    /// Its state after the change, without its texts (A27, "Records"):
    /// what `entry.open` and `entry.change` records carry.
    pub fn snapshot(&self) -> Value {
        let field = |k: &str| self.data.get(k).cloned().unwrap_or(Value::Null);
        let list = |k: &str| self.data.get(k).cloned().unwrap_or_else(|| json!([]));
        json!({
            "kind": self.kind.as_str(),
            "state": self.state,
            "severity": self.severity,
            "slot": self.slot,
            "blocking": self.blocking,
            "anchor": field("anchor"),
            "requirement": list("requirement"),
            "asked_of": field("asked_of"),
            "range": field("range"),
            "reviewers": list("reviewers"),
            "answered_by": list("answered_by"),
            "fix": field("fix"),
            "fix_commits": list("fix_commits"),
            "fixers": list("fixers"),
            "superseded_commits": list("superseded_commits"),
            "anchored": field("anchored"),
            "verified": field("verified"),
            "refs": list("refs"),
            "duplicate_of": field("duplicate_of"),
            "superseded_by": field("superseded_by"),
            "moved_to": field("moved_to"),
            "completed": list("completed"),
            "request": field("request"),
            "notified": field("notified"),
            "title_digest": crate::log::text_digest(&self.title),
            "text_digest": self.text.as_deref().map(crate::log::text_digest),
        })
    }

    /// The whole entry for a read: identity, classification, content,
    /// context and transitions (A27, "Fields"), with times as ms.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("node".into(), self.node.id.clone().into());
        out.insert("id".into(), self.id.clone().into());
        out.insert("kind".into(), self.kind.as_str().into());
        out.insert("state".into(), self.state.clone().into());
        out.insert("severity".into(), self.severity.clone().into());
        out.insert("slot".into(), self.slot.clone().into());
        out.insert("blocking".into(), self.blocking.into());
        out.insert("title".into(), self.title.clone().into());
        out.insert("text".into(), self.text.clone().into());
        for (k, v) in &self.data {
            if k != "node" {
                out.insert(k.clone(), v.clone());
            }
        }
        out.insert("opened_ms".into(), self.opened_ms.into());
        out.insert("updated_ms".into(), self.updated_ms.into());
        Value::Object(out)
    }
}

/// One transition in an entry's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
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
    pub log_seq: Option<u64>,
}

impl Event {
    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "op": self.op,
            "from": self.from,
            "to": self.to,
            "by": self.by,
            "at_ms": self.at_ms,
            "sha": self.sha,
            "message": self.message,
            "turn": self.turn,
            "note": self.note,
        })
    }
}

/// Who makes an op, and the context it came in.
#[derive(Debug, Clone, Copy)]
pub struct Change<'a> {
    pub op: &'a Op,
    pub actor: &'a str,
    pub class: Class,
    /// The node implied when the op names none: a CLI's `<node>`
    /// argument, or a turn's single card (A32).
    pub node: Option<&'a NodeLink>,
    /// The message the op came in, if any (a reply's import).
    pub message: Option<&'a str>,
    pub turn: Option<&'a str>,
    pub now_ms: u64,
}

/// One entry an op changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub node: NodeLink,
    pub id: String,
    pub op: String,
    pub from: Option<String>,
    pub to: String,
    pub log_seq: u64,
}

impl Changed {
    pub fn path(&self) -> String {
        format!("{}#{}", self.node.id, self.id)
    }
}

/// What an op did: the entries it changed, in order, or, for an `add`
/// that repeats an open finding, the finding it named instead (A32,
/// "Duplicates").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: Vec<Changed>,
    pub existing: Option<String>,
}

/// Which ops each class may make (A27, rule 4's table).
fn allowed(class: Class, rule: Rule) -> bool {
    use Class::*;
    match rule {
        Rule::Ask => true,
        Rule::Decide | Rule::Issue => matches!(class, Worker | Reviewer | Person | Coordinator),
        Rule::Review => matches!(class, Reviewer | Person | Coordinator),
        Rule::Fix => matches!(class, Worker | Person | Coordinator),
        Rule::Verify | Rule::Complete => matches!(class, Reviewer | Person),
        Rule::Residual => matches!(class, Person | Coordinator),
    }
}

/// The rows of A27's "Who may do what".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `ask`, `answer`, `withdraw`, `close` a question: everyone, on
    /// questions asked of them or their own.
    Ask,
    /// `propose`, `decide`, and rejecting a proposal.
    Decide,
    /// `add` an issue, `resolve`, `move`.
    Issue,
    /// `add` a finding, `reopen`, `dispute`, `duplicate`, `outdated`,
    /// `close` a rejected or disputed finding.
    Review,
    /// `fix`, `reject` a finding.
    Fix,
    /// `verify`, `req met`, `req unmet`.
    Verify,
    /// `residual`, `req waived`, `done --residual`.
    Residual,
    /// `review.complete`, by a round's named reviewer.
    Complete,
}

fn check_allowed(class: Class, rule: Rule, op: &str) -> Result<(), EntryError> {
    if allowed(class, rule) {
        Ok(())
    } else {
        Err(EntryError::NotAllowed {
            class,
            op: op.to_string(),
        })
    }
}

fn check_title(title: &str) -> Result<(), EntryError> {
    if title.trim().is_empty() || title.len() > TITLE_MAX || title.chars().any(char::is_control) {
        return Err(EntryError::BadText("title"));
    }
    Ok(())
}

fn check_text(text: Option<&str>) -> Result<(), EntryError> {
    match text {
        Some(t) if t.len() > TEXT_MAX => Err(EntryError::BadText("text")),
        _ => Ok(()),
    }
}

/// A severity: `blocker`, `major`, `minor`, `info`, or another short
/// lowercase word (the set is open).
fn check_severity(s: &str) -> Result<(), EntryError> {
    let ok = (1..=32).contains(&s.len())
        && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(EntryError::BadOp("severity".into()))
    }
}

fn check_anchor(a: &Anchor) -> Result<(), EntryError> {
    if a.path.is_empty()
        || a.path.len() > 512
        || a.path.chars().any(char::is_control)
        || a.line == Some(0)
    {
        return Err(EntryError::BadOp("anchor".into()));
    }
    Ok(())
}

/// A short name an op stores as given: an actor, a slot.
fn check_name(field: &'static str, s: &str) -> Result<(), EntryError> {
    let ok = (1..=200).contains(&s.len())
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-' | b'@' | b'/')
        });
    if ok {
        Ok(())
    } else {
        Err(EntryError::BadOp(field.into()))
    }
}

fn check_req_id(id: &str) -> Result<(), EntryError> {
    if id.starts_with('R') && check_entry_id(id).is_ok() {
        Ok(())
    } else {
        Err(EntryError::BadOp("requirement".into()))
    }
}

/// Reads one entry.
fn load(conn: &Connection, node_uuid: &str, id: &str) -> Result<Option<Entry>, EntryError> {
    let row = conn
        .query_row(
            "SELECT entry_id, kind, state, severity, slot, blocking, title, text, data,
                    opened_ms, updated_ms
             FROM entries WHERE node_uuid = ?1 AND entry_id = ?2",
            params![node_uuid, id],
            |r| row_of(node_uuid, r),
        )
        .optional()?;
    row.transpose()
}

type RowResult = Result<Entry, EntryError>;

fn row_of(node_uuid: &str, r: &rusqlite::Row<'_>) -> rusqlite::Result<RowResult> {
    let id: String = r.get(0)?;
    let kind: String = r.get(1)?;
    let data: String = r.get(8)?;
    let build = || -> RowResult {
        let data: Map<String, Value> = match serde_json::from_str(&data)? {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let node_id = data
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or(node_uuid)
            .to_string();
        Ok(Entry {
            node: NodeLink {
                uuid: node_uuid.to_string(),
                id: node_id,
            },
            kind: Kind::parse(&kind).ok_or_else(|| {
                EntryError::Store(StoreError::BadMeta(format!("entry {id} has kind {kind}")))
            })?,
            id: id.clone(),
            state: r.get(2)?,
            severity: r.get(3)?,
            slot: r.get(4)?,
            blocking: r.get::<_, i64>(5)? != 0,
            title: r.get(6)?,
            text: r.get(7)?,
            data,
            opened_ms: ms(r.get(9)?),
            updated_ms: ms(r.get(10)?),
        })
    };
    Ok(build())
}

/// Every entry on a node, by kind letter and number.
fn load_all(conn: &Connection, node_uuid: &str) -> Result<Vec<Entry>, EntryError> {
    let mut stmt = conn.prepare(
        "SELECT entry_id, kind, state, severity, slot, blocking, title, text, data,
                opened_ms, updated_ms
         FROM entries WHERE node_uuid = ?1
         ORDER BY substr(entry_id, 1, 1), CAST(substr(entry_id, 2) AS INTEGER)",
    )?;
    let rows = stmt.query_map(params![node_uuid], |r| row_of(node_uuid, r))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// The next id for `kind` on a node: its highest number plus one. Entries
/// are never deleted, so a number is never reused.
fn next_id(tx: &Connection, node_uuid: &str, kind: Kind) -> Result<String, EntryError> {
    let letter = kind.letter().to_string();
    let top: Option<i64> = tx.query_row(
        "SELECT MAX(CAST(substr(entry_id, 2) AS INTEGER)) FROM entries
         WHERE node_uuid = ?1 AND substr(entry_id, 1, 1) = ?2",
        params![node_uuid, letter],
        |r| r.get(0),
    )?;
    Ok(format!("{letter}{}", top.unwrap_or(0) + 1))
}

fn insert(tx: &Connection, e: &Entry) -> Result<(), EntryError> {
    tx.execute(
        "INSERT INTO entries (node_uuid, entry_id, kind, state, severity, slot, blocking, title,
                              text, data, opened_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            e.node.uuid,
            e.id,
            e.kind.as_str(),
            e.state,
            e.severity,
            e.slot,
            i64::from(e.blocking),
            e.title,
            e.text,
            serde_json::to_string(&e.data)?,
            sql(e.opened_ms),
            sql(e.updated_ms),
        ],
    )?;
    Ok(())
}

fn update(tx: &Connection, e: &Entry) -> Result<(), EntryError> {
    tx.execute(
        "UPDATE entries SET state = ?3, data = ?4, updated_ms = ?5, severity = ?6
         WHERE node_uuid = ?1 AND entry_id = ?2",
        params![
            e.node.uuid,
            e.id,
            e.state,
            serde_json::to_string(&e.data)?,
            sql(e.updated_ms),
            e.severity,
        ],
    )?;
    Ok(())
}

/// What one transition records, besides the entry after it.
struct Step<'a> {
    op: &'a str,
    from: Option<String>,
    /// The commit the op is about: a fix's SHA.
    sha: Option<String>,
    /// The commit it was checked at: a verify's, reopen's or req's `at`.
    at_sha: Option<String>,
    note: Option<String>,
}

/// Writes one transition's `entry_events` row and its log record, and
/// links the two by `log_seq`.
fn record(
    tx: &Connection,
    e: &Entry,
    step: Step<'_>,
    c: &Change<'_>,
) -> Result<Changed, EntryError> {
    let subject = vec![
        mdkg_log::record::node_ref(&e.node.id),
        format!("entry:{}#{}", e.node.uuid, e.id),
    ];
    let (kind, body) = match &step.from {
        None => (
            "entry.open",
            json!({
                "entry": e.id,
                "node": e.node.id,
                "op": "open",
                "to": e.state,
                "snapshot": e.snapshot(),
                "message": c.message,
                "turn": c.turn,
            }),
        ),
        Some(from) => (
            "entry.change",
            json!({
                "entry": e.id,
                "node": e.node.id,
                "op": step.op,
                "from": from,
                "to": e.state,
                "sha": step.sha,
                "at_sha": step.at_sha,
                "snapshot": e.snapshot(),
                "message": c.message,
                "turn": c.turn,
                "note_digest": step.note.as_deref().map(crate::log::text_digest),
            }),
        ),
    };
    let body = match body {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let (log_seq, _) = crate::log::append(tx, c.actor, kind, subject, body)?;
    tx.execute(
        "INSERT INTO entry_events (node_uuid, entry_id, op, from_state, to_state, actor, at_ms,
                                   sha, message_id, turn_id, note, log_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            e.node.uuid,
            e.id,
            if step.from.is_none() { "open" } else { step.op },
            step.from,
            e.state,
            c.actor,
            sql(c.now_ms),
            step.sha.or(step.at_sha),
            c.message,
            c.turn,
            step.note,
            sql(log_seq),
        ],
    )?;
    Ok(Changed {
        node: e.node.clone(),
        id: e.id.clone(),
        op: if step.from.is_none() {
            "open".into()
        } else {
            step.op.into()
        },
        from: step.from,
        to: e.state.clone(),
        log_seq,
    })
}

/// `{by, at, message, turn}`, the shape of an entry's transition fields.
fn stamp(c: &Change<'_>) -> Value {
    json!({"by": c.actor, "at": c.now_ms, "message": c.message, "turn": c.turn})
}

/// The node an op is about, for its actor's class: the entry's node for
/// an op on an entry, else the op's or the implied node.
fn op_node(c: &Change<'_>, env: &dyn Env) -> Result<NodeLink, EntryError> {
    use Action::*;
    let entry = match &c.op.action {
        Fix { entry, .. }
        | Verify { entry, .. }
        | Reopen { entry, .. }
        | Reject { entry, .. }
        | Dispute { entry, .. }
        | Residual { entry, .. }
        | Outdated { entry, .. }
        | Duplicate { entry, .. }
        | Answer { entry, .. }
        | Close { entry }
        | Withdraw { entry }
        | Resolve { entry }
        | Move { entry, .. } => Some(entry),
        Decide { entry, .. } => entry.as_ref(),
        ReviewComplete { round, .. } => Some(round),
        Add { .. } | Propose(_) | Req { .. } => None,
    };
    match entry {
        Some(r) => node_of(r, c, env),
        None => target(c, env),
    }
}

/// Whether a lease's node is `node`: by id, or by UUID after a retype,
/// within `folder`, the node folder of the graph holding `node` (a lease
/// key is `<folder>#<id>`), so a lease on a node of another graph sharing
/// the coordinator never counts (Terra's code review #6278, finding 2).
/// [`NodeFolder::Any`] (an env that reads no graph) matches on the id
/// alone; [`NodeFolder::Nowhere`] matches nothing.
fn lease_on(lease: &mdkg_coord::Lease, node: &NodeLink, folder: &NodeFolder) -> bool {
    let Some(key) = lease.node.as_deref() else {
        return false;
    };
    let id = match (folder, key.rsplit_once('#')) {
        (NodeFolder::Nowhere, _) => return false,
        (NodeFolder::Any, Some((_, id))) => id,
        (NodeFolder::Any, None) => key,
        (NodeFolder::In(folder), Some((f, id))) if f.eq_ignore_ascii_case(folder) => id,
        (NodeFolder::In(_), _) => return false,
    };
    id.eq_ignore_ascii_case(&node.id)
        || id
            .to_ascii_lowercase()
            .ends_with(&format!("-{}", node.uuid.to_ascii_lowercase()))
}

/// The actor's class on `node` (A27 rule 4, R52), judged in the op's
/// transaction from the definitions [`Env::definitions`] reads now and
/// the lease state this transaction reads: a coordinator on a node whose
/// worker (the owner of its live work lease, else of its most recent)
/// is another agent naming the actor as its `reports_to`, unless the
/// actor holds a live lease on the node itself; its own definition's
/// class everywhere else, and inside a running turn.
pub(crate) fn class_on(
    tx: &Connection,
    c: &Change<'_>,
    env: &dyn Env,
    node: Option<&NodeLink>,
) -> Result<Class, EntryError> {
    let (base, subordinates) = env.definitions(c.actor)?.unwrap_or((c.class, Vec::new()));
    let (Some(node), false) = (node, base == Class::Person || subordinates.is_empty()) else {
        return Ok(base);
    };
    let state = crate::store::coordinator_in(tx)?;
    let folder = env.node_folder(node)?;
    let live = |l: &mdkg_coord::Lease| {
        l.confirmed && l.status == mdkg_coord::lease::Status::Active && l.deadline > c.now_ms
    };
    // Unconfirmed leases (a denied claim, an unreconciled offline one)
    // never made anyone the node's worker.
    let leases: Vec<&mdkg_coord::Lease> = state
        .leases()
        .filter(|l| l.confirmed && lease_on(l, node, &folder))
        .collect();
    if leases.iter().any(|l| live(l) && l.owner == c.actor) {
        return Ok(base);
    }
    // Never its own work, wherever its lease is keyed: by the node's id
    // or UUID in any folder, the actor's own live lease, or the actor as
    // the owner of the node's live lease, else of its most recent, makes
    // it no coordinator. A key split from the gate's folder (a checkout
    // that moved the node's file to another workspace folder, or a
    // workspace list that changed since the claim) then only ever takes
    // the class away, never grants it (Terra's code review #10490; A27
    // rule 4, "Never its own work"). Format 0.7 ids carry the node's
    // UUID, so another node matches only through a copied file, the
    // stated UUID residual, and that too refuses.
    let anywhere: Vec<&mdkg_coord::Lease> = state
        .leases()
        .filter(|l| l.confirmed && lease_on(l, node, &NodeFolder::Any))
        .collect();
    let holder = anywhere
        .iter()
        .rev()
        .find(|l| live(l))
        .or_else(|| anywhere.last())
        .map(|l| l.owner.as_str());
    if holder == Some(c.actor) || anywhere.iter().any(|l| live(l) && l.owner == c.actor) {
        return Ok(base);
    }
    let worker = leases
        .iter()
        .rev()
        .find(|l| live(l))
        .or_else(|| leases.last())
        .map(|l| l.owner.as_str());
    match worker {
        Some(w) if w != c.actor && subordinates.iter().any(|s| s == w) => {
            // A ledger block (`turn` set) is the turn's own channel; the
            // CLI or MCP by an actor with a turn running isn't.
            if c.turn.is_none() && env.in_turn(c.actor)? {
                Ok(base)
            } else {
                Ok(Class::Coordinator)
            }
        }
        _ => Ok(base),
    }
}

/// The node an op is about: the op's own `node`, else the implied one.
fn target(c: &Change<'_>, env: &dyn Env) -> Result<NodeLink, EntryError> {
    match &c.op.node {
        Some(handle) => env.node(handle),
        None => c.node.cloned().ok_or(EntryError::NoNode),
    }
}

/// The node an entry reference names: its own node, else the op's, else
/// the implied one.
fn node_of(r: &EntryRef, c: &Change<'_>, env: &dyn Env) -> Result<NodeLink, EntryError> {
    match &r.node {
        Some(handle) => env.node(handle),
        None => target(c, env),
    }
}

fn find(tx: &Connection, r: &EntryRef, c: &Change<'_>, env: &dyn Env) -> Result<Entry, EntryError> {
    let node = node_of(r, c, env)?;
    load(tx, &node.uuid, &r.id)?
        .ok_or_else(|| EntryError::NotFound(format!("{}#{}", node.id, r.id)))
}

/// A finding's declared fix commits and its fixers, reconciled with its
/// history every time: what the entry records, together with every `fix`
/// and `supersede` event's actor and the commits its record's snapshot
/// named (else its SHA), less the commits a supersede withdrew
/// (`superseded_commits`). So a finding fixed before these fields were
/// kept, or one whose fields an earlier build filled from its last fix
/// only, loses no fixer, who could then verify, and no commit, which would
/// drop out of the checks (Terra's code reviews #7598, finding 4, and
/// #8466).
fn fix_record(tx: &Connection, e: &Entry) -> Result<(Vec<String>, Vec<String>), EntryError> {
    let mut declared = e.get_strs("fix_commits");
    let mut fixers = e.get_strs("fixers");
    let superseded = e.get_strs("superseded_commits");
    let push = |list: &mut Vec<String>, v: &str| {
        if !v.is_empty() && !list.iter().any(|d| d == v) {
            list.push(v.to_string());
        }
    };
    let mut fixes: Vec<(String, Option<String>, Option<Value>)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT ev.actor, ev.sha, l.record FROM entry_events ev
             LEFT JOIN log l ON l.seq = ev.log_seq
             WHERE ev.node_uuid = ?1 AND ev.entry_id = ?2 AND ev.op IN ('fix', 'supersede')
             ORDER BY ev.seq",
        )?;
        let rows = stmt.query_map(params![e.node.uuid, e.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (actor, sha, record) = row?;
            let fix = record
                .and_then(|r| serde_json::from_str::<Value>(&r).ok())
                .and_then(|r| r.pointer("/body/snapshot/fix").cloned());
            fixes.push((actor, sha, fix));
        }
    }
    if let Some(last) = e.data.get("fix") {
        fixes.push((
            last.get("by")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            last.get("sha").and_then(Value::as_str).map(str::to_string),
            Some(last.clone()),
        ));
    }
    for (actor, sha, fix) in &fixes {
        let listed = fix
            .as_ref()
            .and_then(|f| f.get("commits"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        for commit in listed {
            push(&mut declared, commit);
        }
        if let Some(sha) = sha {
            push(&mut declared, sha);
        }
        push(&mut fixers, actor);
    }
    declared.retain(|c| !superseded.contains(c));
    Ok((declared, fixers))
}

fn bad_transition(e: &Entry, op: &str) -> EntryError {
    EntryError::BadTransition {
        entry: e.path(),
        state: e.state.clone(),
        op: op.to_string(),
    }
}

/// Moves `e` to `to`, records it, and returns what changed.
fn transition(
    tx: &Connection,
    e: &mut Entry,
    to: &str,
    step: Step<'_>,
    c: &Change<'_>,
) -> Result<Changed, EntryError> {
    let step = Step {
        from: Some(e.state.clone()),
        ..step
    };
    e.state = to.to_string();
    e.updated_ms = c.now_ms;
    update(tx, e)?;
    record(tx, e, step, c)
}

fn plain<'a>(op: &'a str, note: Option<String>) -> Step<'a> {
    Step {
        op,
        from: None,
        sha: None,
        at_sha: None,
        note,
    }
}

/// Opens a new entry of `kind` on `node` in `state`, from `new`.
#[allow(clippy::too_many_arguments)]
fn open(
    tx: &Connection,
    node: &NodeLink,
    kind: Kind,
    state: &str,
    new: &New,
    refs: Vec<String>,
    extra: Map<String, Value>,
    c: &Change<'_>,
) -> Result<(Entry, Changed), EntryError> {
    check_title(&new.title)?;
    check_text(new.text.as_deref())?;
    let mut data = Map::new();
    data.insert("node".into(), node.id.clone().into());
    data.insert("opened".into(), stamp(c));
    if let Some(a) = &new.anchor {
        check_anchor(a)?;
        data.insert("anchor".into(), a.to_json());
    }
    for r in &new.requirement {
        check_req_id(r)?;
    }
    if !new.requirement.is_empty() {
        data.insert("requirement".into(), json!(new.requirement));
    }
    if let Some(who) = &new.asked_of {
        check_name("asked_of", who)?;
        data.insert("asked_of".into(), who.clone().into());
    }
    if !refs.is_empty() {
        data.insert("refs".into(), json!(refs));
    }
    if let Some(slot) = &new.slot {
        check_name("slot", slot)?;
    }
    data.extend(extra);
    let severity = match (kind, &new.severity) {
        (_, Some(s)) => {
            check_severity(s)?;
            Some(s.clone())
        }
        (Kind::Finding, None) => Some(DEFAULT_SEVERITY.to_string()),
        _ => None,
    };
    let id = next_id(tx, &node.uuid, kind)?;
    let e = Entry {
        node: node.clone(),
        id,
        kind,
        state: state.to_string(),
        severity,
        slot: new.slot.clone(),
        blocking: kind == Kind::Question && new.blocking,
        title: new.title.clone(),
        text: new.text.clone(),
        data,
        opened_ms: c.now_ms,
        updated_ms: c.now_ms,
    };
    insert(tx, &e)?;
    let changed = record(tx, &e, plain("open", None), c)?;
    Ok((e, changed))
}

/// Resolves entry references to `<node-id>#<id>`, each of which must
/// exist.
fn ref_paths(
    tx: &Connection,
    refs: &[EntryRef],
    c: &Change<'_>,
    env: &dyn Env,
) -> Result<Vec<String>, EntryError> {
    let mut out: Vec<String> = Vec::new();
    for r in refs {
        let path = find(tx, r, c, env)?.path();
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// The decisions on `node` that `supersedes` names: each must be on
/// the same node and a decision.
fn superseded_ids(
    tx: &Connection,
    node: &NodeLink,
    supersedes: &[EntryRef],
    c: &Change<'_>,
    env: &dyn Env,
) -> Result<Vec<String>, EntryError> {
    let mut out = Vec::new();
    for r in supersedes {
        let e = find(tx, r, c, env)?;
        if e.node.uuid != node.uuid || e.kind != Kind::Decision {
            return Err(EntryError::BadOp("supersedes".into()));
        }
        if !out.contains(&e.id) {
            out.push(e.id);
        }
    }
    Ok(out)
}

/// Supersedes each named decision by `by` (A27: `accepted` →
/// `superseded`). One that isn't accepted is refused, so nothing lands.
fn supersede(
    tx: &Connection,
    node: &NodeLink,
    ids: &[String],
    by: &str,
    c: &Change<'_>,
    out: &mut Vec<Changed>,
) -> Result<(), EntryError> {
    for id in ids {
        let mut old = load(tx, &node.uuid, id)?
            .ok_or_else(|| EntryError::NotFound(format!("{}#{id}", node.id)))?;
        if old.state != "accepted" {
            return Err(bad_transition(&old, "supersede"));
        }
        old.data
            .insert("superseded_by".into(), format!("{}#{by}", node.id).into());
        out.push(transition(
            tx,
            &mut old,
            "superseded",
            plain("supersede", None),
            c,
        )?);
    }
    Ok(())
}

/// An open finding on `node` with the same anchor and title (A32,
/// "Duplicates"), by title digest so the comparison is exact.
fn same_open_finding(
    tx: &Connection,
    node: &NodeLink,
    new: &New,
) -> Result<Option<Entry>, EntryError> {
    let anchor = new.anchor.as_ref().map(Anchor::to_json);
    // Only an `open` or `reopened` finding is repeated (Terra's review
    // #5962): a fixed, rejected or disputed one takes a new finding.
    Ok(load_all(tx, &node.uuid)?.into_iter().find(|e| {
        e.kind == Kind::Finding
            && matches!(e.state.as_str(), "open" | "reopened")
            && crate::log::text_digest(&e.title) == crate::log::text_digest(&new.title)
            && e.data.get("anchor").cloned() == anchor
    }))
}

/// A review round as walks read it (A29).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Round {
    pub id: String,
    pub number: u64,
    pub base: String,
    pub head: String,
    pub closed: bool,
    /// `main` or `first`, recorded at opening (A27 rule 8); none for an
    /// unanchored round.
    pub anchored: Option<String>,
}

/// The node's review rounds with a range, lowest number first; only the
/// closed ones when `closed_only`. A node's rounds are its own requests'
/// (decided 2026-10-29: one node per review request).
pub(crate) fn node_rounds(
    tx: &Connection,
    node: &NodeLink,
    closed_only: bool,
) -> Result<Vec<Round>, EntryError> {
    Ok(rounds_in(&load_all(tx, &node.uuid)?, closed_only))
}

/// [`node_rounds`] over entries already read.
pub(crate) fn rounds_in(entries: &[Entry], closed_only: bool) -> Vec<Round> {
    let mut out: Vec<Round> = entries
        .iter()
        .filter(|e| e.kind == Kind::Review)
        .filter(|e| !closed_only || e.state == "closed")
        .filter_map(|e| {
            let (base, head) = e.range()?;
            Some(Round {
                number: e.id[1..].parse().unwrap_or(u64::MAX),
                closed: e.state == "closed",
                anchored: e
                    .data
                    .get("anchored")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id: e.id.clone(),
                base,
                head,
            })
        })
        .collect();
    out.sort_by_key(|r| r.number);
    out
}

/// The final round by ancestry at `at` (A29, "The final round"): among
/// the closed rounds whose head is `at` or in its history, the one whose
/// head no other such head descends from, the lowest round number
/// breaking a tie. `None` when no closed round's head is in `at`.
///
/// A closed round whose range isn't two full lowercase commit ids (a row
/// an earlier build stored as given) fails closed: its head could match
/// no history and silently end no walk, skipping the content check
/// (Terra's code review #10220, major 4, with the orchestrator's call).
pub(crate) fn final_round<'a>(
    rounds: &'a [Round],
    at: &str,
    env: &dyn Env,
) -> Result<Option<&'a Round>, EntryError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut heads: Vec<String> = Vec::new();
    for r in rounds.iter().filter(|r| r.closed) {
        if !full_oid(&r.base) || !full_oid(&r.head) {
            return Err(EntryError::Git(format!(
                "review round {} records the range {}..{}, not full commit ids, so its walk \
                 can't be judged: this store holds a round an earlier build recorded with short \
                 ids; recreate the dev store",
                r.id, r.base, r.head
            )));
        }
        // Every closed round's head counts toward the walk limit, checked
        // as each is added, and membership is a set's (Terra's #10220,
        // major 6).
        if seen.insert(&r.head) {
            heads.push(r.head.clone());
            if heads.len() > WALK_WORK {
                return Err(EntryError::WalkLimit(WALK_WORK));
            }
        }
    }
    if heads.is_empty() {
        return Ok(None);
    }
    // One batched read finds the latest of them (Terra's #9504, major 5).
    // A head that isn't present locally (a branch never fetched) is in no
    // history here, so it's no final round, never an error that fails
    // the gate of a node condition 2 doesn't need (A29, "Failures": the
    // hint is to fetch).
    let latest: BTreeSet<String> = env.latest_heads(&heads, at)?.into_iter().collect();
    Ok(rounds
        .iter()
        .filter(|r| r.closed && latest.contains(&r.head))
        .min_by_key(|r| r.number))
}

/// The paths of `p` whose content at `at` differs from `f`'s (A29,
/// condition 2's item 2): mode, object id and name, or presence, the
/// node's own file (`node_path`) compared by [`node_revision_text`].
fn paths_differing(
    env: &dyn Env,
    p: &BTreeSet<String>,
    at: &str,
    f: &str,
    node_path: Option<&str>,
) -> Result<Vec<String>, EntryError> {
    let mut out = Vec::new();
    for path in p {
        let (now, then) = (env.tree_entry(at, path)?, env.tree_entry(f, path)?);
        if now == then {
            continue;
        }
        if Some(path.as_str()) == node_path && same_node_text(env, &now, &then)? {
            continue;
        }
        out.push(path.clone());
    }
    Ok(out)
}

/// Whether two `ls-tree` records of the node file hold the same mode and
/// the same text but for its `status`, `updated` and `# Ledger`.
fn same_node_text(
    env: &dyn Env,
    a: &Option<String>,
    b: &Option<String>,
) -> Result<bool, EntryError> {
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(false);
    };
    let parts = |r: &str| -> Option<(String, String, String)> {
        let (meta, _) = r.split_once('\t')?;
        let mut m = meta.split(' ');
        Some((m.next()?.into(), m.next()?.into(), m.next()?.into()))
    };
    let (Some((ma, ta, oa)), Some((mb, tb, ob))) = (parts(a), parts(b)) else {
        return Ok(false);
    };
    if ma != mb || ta != "blob" || tb != "blob" {
        return Ok(false);
    }
    Ok(node_revision_text(&env.blob(&oa)?) == node_revision_text(&env.blob(&ob)?))
}

/// The paths a settle record's walk `{base, head}` touched (A29's P, read
/// over `base..head`) whose content at `at` differs from `head`'s: a
/// settled read's content check (A29, "A done file that didn't settle
/// reads as unsettled"), by condition 2's own comparison.
pub fn walk_differs(
    env: &dyn Env,
    (base, head): (&str, &str),
    at: &str,
    node_path: Option<&str>,
) -> Result<Vec<String>, EntryError> {
    paths_differing(env, &env.changed_paths(base, head)?, at, head, node_path)
}

/// What a read needs to tell a settled done node from an unsettled one
/// (A29, "A done file that didn't settle reads as unsettled"), read once
/// for every node: which nodes have a ledger, and their `ledger.settle`
/// records.
#[derive(Debug, Clone, Default)]
pub struct LedgerReads {
    /// Lowercase UUIDs of the nodes with any entry, requirements included.
    ledger: BTreeSet<String>,
    /// Each node's settle record bodies, newest first, by lowercase UUID.
    settles: std::collections::BTreeMap<String, Vec<Value>>,
}

impl LedgerReads {
    /// Whether the node with this UUID has a ledger: any entry, or any
    /// requirement.
    pub fn has_ledger(&self, uuid: &str) -> bool {
        self.ledger.contains(&uuid.to_ascii_lowercase())
    }

    /// The node's settle record bodies, newest first.
    pub fn settles(&self, uuid: &str) -> &[Value] {
        self.settles
            .get(&uuid.to_ascii_lowercase())
            .map_or(&[], Vec::as_slice)
    }
}

/// Whether `s` is a full commit id as git prints one: 40 (SHA-1) or 64
/// (SHA-256) lowercase hex digits. A review round's range is stored only
/// so (Terra's code review #10220, major 4).
pub(crate) fn full_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `sha` resolved to the full lowercase id of the one commit it names,
/// else [`EntryError::UnknownCommit`].
fn full_commit(env: &dyn Env, sha: &str) -> Result<String, EntryError> {
    let full = env.commit(sha)?.to_ascii_lowercase();
    if full_oid(&full) {
        Ok(full)
    } else {
        Err(EntryError::UnknownCommit(sha.into()))
    }
}

/// A node file's text as the gate compares it and a settle records its
/// revision (A29, "The node's own file"; A31): without its `status` and
/// `updated` front-matter lines and the ledger writer's own `# Ledger`
/// section, the rest byte for byte.
///
/// The section is stripped only when it is unmistakably the writer's
/// (Terra's code review #9504, blocker 1, with the orchestrator's call):
/// exactly one `# Ledger` line at column 0 outside every fence, it the
/// file's last level-one heading, and every line after it the writer's
/// grammar ([`writer_section`]). Otherwise nothing is stripped and the
/// whole text compares: a false difference only costs a review, never a
/// pass.
///
/// What's stripped is only ever that grammar, so the scan's reading of
/// fences and headings only keeps false differences down and can't hide
/// text: a misread fence, or a heading it misses, at worst strips
/// writer-grammar lines or keeps the section (Terra's code review
/// #10220, blockers 2 and 3). HTML comments aren't tracked at all: a
/// `# Ledger` inside one counts, so the text compares whole (#10220,
/// major 5, with the orchestrator's call: every misread falls toward
/// comparing more).
pub fn node_revision_text(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let front = front_matter_len(bytes);
    for l in bytes[..front].split_inclusive(|b| *b == b'\n') {
        let b = bare_line(l);
        if b.starts_with(b"status:") || b.starts_with(b"updated:") {
            continue;
        }
        out.extend_from_slice(l);
    }
    let body: Vec<&[u8]> = bytes[front..].split_inclusive(|b| *b == b'\n').collect();
    for l in &body[..writer_cut(&body)] {
        out.extend_from_slice(l);
    }
    out
}

/// The front matter's length in bytes: from a first `---` line to the
/// next, both included, or to the end when none closes it; 0 without one.
fn front_matter_len(bytes: &[u8]) -> usize {
    let mut lines = bytes.split_inclusive(|b| *b == b'\n');
    let Some(first) = lines.next() else {
        return 0;
    };
    if bare_line(first) != b"---" {
        return 0;
    }
    let mut n = first.len();
    for l in lines {
        n += l.len();
        if bare_line(l) == b"---" {
            break;
        }
    }
    n
}

/// The index among a body's lines of the writer's own `# Ledger` heading,
/// where [`node_revision_text`] cuts, else the number of lines.
fn writer_cut(body: &[&[u8]]) -> usize {
    let mut fence: Option<(u8, usize)> = None;
    let mut candidates = Vec::new();
    let mut last_heading = None;
    let mut paragraph = false;
    for (i, l) in body.iter().enumerate() {
        let b = bare_line(l);
        let trimmed = b.trim_ascii_start();
        let indent = b.len() - trimmed.len();
        if let Some((c, n)) = fence {
            // A fence closes only on a run of its own character at least
            // as long, with nothing after it but spaces (Opus #9431,
            // minor 2).
            if let Some((d, m)) = fence_run(trimmed)
                && d == c
                && m >= n
                && trimmed[m..].iter().all(|b| *b == b' ' || *b == b'\t')
            {
                fence = None;
            }
            paragraph = false;
            continue;
        }
        if indent <= 3
            && let Some(run) = fence_run(trimmed)
        {
            fence = Some(run);
            paragraph = false;
            continue;
        }
        let atx = indent <= 3
            && trimmed.first() == Some(&b'#')
            && matches!(trimmed.get(1), None | Some(b' ' | b'\t'));
        let setext = paragraph
            && indent <= 3
            && !trimmed.is_empty()
            && trimmed.trim_ascii_end().iter().all(|b| *b == b'=');
        if atx || setext {
            last_heading = Some(i);
        }
        if b == b"# Ledger" {
            candidates.push(i);
        }
        paragraph = !atx && !trimmed.is_empty();
    }
    match candidates.as_slice() {
        [one] if last_heading == Some(*one) && writer_section(&body[*one + 1..]) => *one,
        _ => body.len(),
    }
}

/// Whether the lines after a final `# Ledger` heading are the ledger
/// writer's own rendering, and nothing else (the orchestrator on Opus
/// #10156, 1): blank lines, then `<!-- mdkg:ledger begin upto=<n> -->`,
/// entry lines, `<!-- mdkg:ledger end -->`, and blank lines to the end.
/// An entry line is `- <id> <rest>` for a question, decision, finding,
/// issue or round (`Q`, `D`, `F`, `I`, `V` and a number), `- R<n>
/// <state>` with an optional `@<sha>`, ` by <actor>` and `: <reason>`, or
/// `- … <n> more: <rest>`; none holds a comment marker. Anything else, an
/// appended requirement line included, keeps the section in the
/// comparison, which can only cost a review.
fn writer_section(lines: &[&[u8]]) -> bool {
    let text: Vec<&str> = match lines
        .iter()
        .map(|l| std::str::from_utf8(bare_line(l)))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(t) => t,
        Err(_) => return false,
    };
    let mut rest = text.iter().copied().skip_while(|l| l.trim().is_empty());
    let Some(begin) = rest.next() else {
        return false;
    };
    let upto = begin
        .strip_prefix("<!-- mdkg:ledger begin upto=")
        .and_then(|r| r.strip_suffix(" -->"));
    if !upto
        .is_some_and(|n| !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let number = |s: &str| {
        !s.is_empty()
            && s.len() <= 9
            && !s.starts_with('0')
            && s.bytes().all(|b| b.is_ascii_digit())
    };
    let entry = |l: &str| -> bool {
        if l.len() > 240 || l.contains("<!--") || l.contains("-->") {
            return false;
        }
        let Some(line) = l.strip_prefix("- ") else {
            return false;
        };
        if let Some(more) = line.strip_prefix("… ") {
            return more
                .split_once(" more: ")
                .is_some_and(|(n, _)| n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty());
        }
        let (id, after) = line.split_once(' ').unwrap_or((line, ""));
        let (letter, n) = id.split_at_checked(1).unwrap_or(("", ""));
        if !number(n) {
            return false;
        }
        match letter {
            "Q" | "D" | "F" | "I" | "V" => !after.is_empty(),
            "R" => {
                let state = after.split(['@', ' ', ':']).next().unwrap_or_default();
                matches!(state, "unverified" | "met" | "unmet" | "waived")
            }
            _ => false,
        }
    };
    let mut ended = false;
    for l in rest {
        if l.trim().is_empty() {
            continue;
        }
        if ended {
            return false;
        }
        if l == "<!-- mdkg:ledger end -->" {
            ended = true;
        } else if !entry(l) {
            return false;
        }
    }
    ended
}

/// A `# Ledger` line's cap, in bytes (A31).
pub const LEDGER_LINE_MAX: usize = 240;
/// The `# Ledger` section's cap, in bytes, heading and markers included
/// (A31).
pub const LEDGER_SECTION_MAX: usize = 16 * 1024;

/// Who moved an entry into its current state, and the note that came
/// with it (a residual's or a waiver's reason), for its `# Ledger` line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerBy {
    pub by: String,
    pub note: Option<String>,
}

/// The ledger writer's `# Ledger` section (A31), exactly the grammar the
/// strip takes ([`writer_section`]): the heading, a blank line, the begin
/// marker with `upto`, one line per entry and per requirement, ordered by
/// kind letter and number, and the end marker. `requirements` are the
/// node's `# Requirements` ids; one without an entry reads `unverified`.
///
/// Titles, actors, paths and notes are untrusted: control characters,
/// newlines and comment markers are stripped, each line is cut to
/// [`LEDGER_LINE_MAX`] bytes, and the section to [`LEDGER_SECTION_MAX`],
/// ending in `- … <n> more: mdkg entry list <node>`. A line that wouldn't
/// be in the grammar (an id the store never allocates) is left out.
pub fn render_ledger_section(
    node_id: &str,
    upto: u64,
    entries: &[Entry],
    by: &std::collections::HashMap<String, LedgerBy>,
    requirements: &[String],
) -> String {
    let mut lines: Vec<(char, u64, String)> = Vec::new();
    let number = |id: &str| -> Option<(char, u64)> {
        let letter = id.chars().next()?;
        let n = &id[letter.len_utf8()..];
        let ok = !n.is_empty()
            && n.len() <= 9
            && !n.starts_with('0')
            && n.bytes().all(|b| b.is_ascii_digit());
        Some((letter, n.parse().ok().filter(|_| ok)?))
    };
    let sha12 = |s: &str| ledger_text(&truncate(s, 12));
    for e in entries {
        let Some((letter, n)) = number(&e.id) else {
            continue;
        };
        if letter != e.kind.letter() {
            continue;
        }
        let who = by.get(&e.id);
        let mut line = format!("- {}", e.id);
        if e.kind == Kind::Req {
            if !matches!(e.state.as_str(), "unverified" | "met" | "unmet" | "waived") {
                continue;
            }
            line.push(' ');
            line.push_str(&e.state);
            if e.state == "met"
                && let Some(sha) = e.verified_sha()
            {
                line.push('@');
                line.push_str(&sha12(sha));
            }
        } else {
            if let Some(s) = e.severity.as_deref().filter(|s| !s.is_empty()) {
                line.push(' ');
                line.push_str(&ledger_text(s));
            }
            line.push(' ');
            line.push_str(&ledger_text(&e.state));
            if let Some(sha) = e.verified_sha().or_else(|| e.fix_sha()) {
                line.push('@');
                line.push_str(&sha12(sha));
            }
        }
        // A verification names its verifier, a re-verify's included.
        let verifier = e
            .data
            .get("verified")
            .and_then(|v| v.get("by"))
            .and_then(Value::as_str)
            .filter(|_| matches!(e.state.as_str(), "verified" | "met"));
        if let Some(name) = verifier
            .or_else(|| who.map(|w| w.by.as_str()))
            .filter(|n| !n.is_empty())
        {
            line.push_str(" by ");
            line.push_str(&ledger_text(name));
        }
        let reason = who
            .and_then(|w| w.note.as_deref())
            .filter(|_| matches!(e.state.as_str(), "residual" | "waived"));
        if e.kind == Kind::Req {
            if let Some(r) = reason {
                line.push_str(": ");
                line.push_str(&ledger_text(r));
            }
        } else {
            line.push_str(": ");
            line.push_str(&ledger_text(&e.title));
            let place = match (e.kind, e.data.get("anchor"), e.range()) {
                (Kind::Review, _, Some((base, head))) => {
                    Some(format!("{}..{}", sha12(&base), sha12(&head)))
                }
                (_, Some(a), _) => a.get("path").and_then(Value::as_str).map(|p| {
                    match a.get("line").and_then(Value::as_u64) {
                        Some(l) => format!("{}:{l}", ledger_text(p)),
                        None => ledger_text(p),
                    }
                }),
                _ => None,
            };
            if let Some(p) = place {
                line.push_str(&format!(" ({p})"));
            }
            if let Some(r) = reason {
                line.push_str("; reason: ");
                line.push_str(&ledger_text(r));
            }
        }
        lines.push((letter, n, truncate(&line, LEDGER_LINE_MAX)));
    }
    for id in requirements {
        let Some((letter, n)) = number(id) else {
            continue;
        };
        if letter == 'R' && !entries.iter().any(|e| e.kind == Kind::Req && e.id == *id) {
            lines.push((letter, n, format!("- {id} unverified")));
        }
    }
    lines.sort_by_key(|l| (l.0, l.1));
    lines.dedup_by(|a, b| (a.0, a.1) == (b.0, b.1));
    let begin = format!("# Ledger\n\n<!-- mdkg:ledger begin upto={upto} -->\n");
    let end = "<!-- mdkg:ledger end -->\n";
    let more = |n: usize| {
        truncate(
            &format!("- … {n} more: mdkg entry list {}", ledger_text(node_id)),
            LEDGER_LINE_MAX,
        )
    };
    let mut out = begin;
    // Room kept for the end marker and the longest `more` line.
    let room = LEDGER_SECTION_MAX - end.len() - LEDGER_LINE_MAX - 1;
    let total = lines.len();
    for (i, (_, _, line)) in lines.into_iter().enumerate() {
        if out.len() + line.len() + 1 > room {
            out.push_str(&more(total - i));
            out.push('\n');
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(end);
    out
}

/// Untrusted text for a `# Ledger` line: no control characters (newlines
/// included) and no comment markers, removed until none is left.
fn ledger_text(s: &str) -> String {
    let mut t: String = s.chars().filter(|c| !c.is_control()).collect();
    loop {
        let next = t.replace("<!--", "").replace("-->", "");
        if next == t {
            return t;
        }
        t = next;
    }
}

/// The node file `bytes` with the writer's `# Ledger` section, `section`
/// ([`render_ledger_section`]), at its end: a section of the writer's own
/// is replaced whole, else the section is appended after the last line.
/// `None` when the result wouldn't strip back to `bytes`'s text
/// ([`node_revision_text`]), as with an unclosed fence, another `# Ledger`
/// heading or no final newline: the section is display only, so then none
/// is written, and `done`'s own write can never fail its gate. The
/// section takes the file's line ending, from its first line, so a CRLF
/// file stays CRLF (Terra's #11075, minor 2).
pub fn with_ledger_section(bytes: &[u8], section: &str) -> Option<Vec<u8>> {
    let front = front_matter_len(bytes);
    let body: Vec<&[u8]> = bytes[front..].split_inclusive(|b| *b == b'\n').collect();
    let kept = front
        + body[..writer_cut(&body)]
            .iter()
            .map(|l| l.len())
            .sum::<usize>();
    let mut out = bytes[..kept].to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        return None;
    }
    let crlf = bytes
        .split_inclusive(|b| *b == b'\n')
        .next()
        .is_some_and(|l| l.ends_with(b"\r\n"));
    if crlf {
        out.extend_from_slice(section.replace('\n', "\r\n").as_bytes());
    } else {
        out.extend_from_slice(section.as_bytes());
    }
    (node_revision_text(&out) == node_revision_text(bytes)).then_some(out)
}

/// The node file `bytes` without the writer's own `# Ledger` section, or
/// `None` when it has none: for a `done` that can't write a new section,
/// so an out-of-date one never stays in the body (Terra's #11158). It
/// strips back to the same text by construction, and that's checked.
pub fn without_ledger_section(bytes: &[u8]) -> Option<Vec<u8>> {
    let front = front_matter_len(bytes);
    let body: Vec<&[u8]> = bytes[front..].split_inclusive(|b| *b == b'\n').collect();
    let cut = writer_cut(&body);
    if cut == body.len() {
        return None;
    }
    let kept = front + body[..cut].iter().map(|l| l.len()).sum::<usize>();
    let out = bytes[..kept].to_vec();
    (node_revision_text(&out) == node_revision_text(bytes)).then_some(out)
}

/// A line without its line ending.
fn bare_line(l: &[u8]) -> &[u8] {
    let mut end = l.len();
    while end > 0 && matches!(l[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &l[..end]
}

/// A fence's opening run, three or more backticks or tildes: its
/// character and length.
fn fence_run(t: &[u8]) -> Option<(u8, usize)> {
    let c = *t.first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = t.iter().take_while(|b| **b == c).count();
    (n >= 3).then_some((c, n))
}

/// Deepest first: every walk ending at `last` (A29, "Walks of rounds"):
/// sequences of rounds, the first anchored, each starting exactly at the
/// previous one's head, at most 64 deep. Predecessors are closed rounds,
/// and among those, rounds with the same range count once, so an open
/// duplicate never hides a closed one (the orchestrator on Terra's
/// #6548): the earliest anchored one, else the lowest-numbered, so an
/// unanchored duplicate never hides an anchored round, `last` included
/// (the orchestrator on Terra's #7598, finding 5). Ties keep the lowest
/// round numbers first. Every step counts toward [`WALK_WORK`], the
/// deduplication and each predecessor looked at included, and more
/// refuses ([`EntryError::WalkLimit`]) rather than run unbounded (Terra's
/// #7598, finding 6).
pub(crate) fn walks_to<'a>(
    rounds: &'a [Round],
    last: &'a Round,
) -> Result<Vec<Vec<&'a Round>>, EntryError> {
    use std::collections::HashMap;
    const DEPTH: usize = 64;
    let mut work = 0usize;
    let mut count = |n: usize| {
        work += n;
        if work > WALK_WORK {
            Err(EntryError::WalkLimit(WALK_WORK))
        } else {
            Ok(())
        }
    };
    // Which round stands for a range: anchored before unanchored, then
    // the lowest number.
    let rank = |r: &Round| (r.anchored.is_none(), r.number);
    let mut by_range: HashMap<(&str, &str), &Round> = HashMap::new();
    for r in rounds.iter().filter(|r| r.closed) {
        count(1)?;
        by_range
            .entry((r.base.as_str(), r.head.as_str()))
            .and_modify(|kept| {
                if rank(r) < rank(kept) {
                    *kept = r;
                }
            })
            .or_insert(r);
    }
    let last = match by_range.get(&(last.base.as_str(), last.head.as_str())) {
        Some(kept) if rank(kept) < rank(last) => *kept,
        _ => last,
    };
    // Predecessors by head, the highest number first, as the stack pops
    // the lowest first.
    let mut by_head: HashMap<&str, Vec<&Round>> = HashMap::new();
    for r in by_range.values() {
        by_head.entry(r.head.as_str()).or_default().push(r);
    }
    for list in by_head.values_mut() {
        list.sort_by_key(|r| std::cmp::Reverse(r.number));
    }
    let mut out: Vec<Vec<&Round>> = Vec::new();
    // Paths are built backwards from `last`.
    let mut stack: Vec<Vec<&Round>> = vec![vec![last]];
    while let Some(path) = stack.pop() {
        count(1)?;
        let first = path[path.len() - 1];
        if first.anchored.is_some() {
            out.push(path.iter().rev().copied().collect());
        }
        if path.len() >= DEPTH {
            continue;
        }
        for r in by_head.get(first.base.as_str()).into_iter().flatten() {
            count(1)?;
            if !path
                .iter()
                .any(|p| p.id == r.id || (p.base == r.base && p.head == r.head))
            {
                let mut next = path.clone();
                next.push(r);
                stack.push(next);
            }
        }
    }
    out.sort_by(|a, b| {
        b.len().cmp(&a.len()).then_with(|| {
            let nums = |w: &Vec<&Round>| w.iter().map(|r| r.number).collect::<Vec<_>>();
            nums(a).cmp(&nums(b))
        })
    });
    Ok(out)
}

/// The most partial walks [`walks_to`] builds.
const WALK_WORK: usize = 10_000;

/// The work base the verify's walk starts at: the longest walk ending at
/// `round` (the earliest work base); `None` without an anchored walk,
/// never the round's own base (the orchestrator on Terra's #6548).
fn walk_base(rounds: &[Round], round: &Round) -> Result<Option<String>, EntryError> {
    Ok(walks_to(rounds, round)?.first().map(|w| w[0].base.clone()))
}

/// The first fix commit `at` undoes, judged per commit against its own
/// first parent, never a base: every path the commit changed reads at
/// `at` as at its first parent (A27 rule 3, `entry.fix_absent`). A commit
/// whose paths can't be listed is never judged undone.
fn fix_absent(
    env: &dyn Env,
    fix_commits: &[String],
    at: &str,
) -> Result<Option<String>, EntryError> {
    for commit in fix_commits {
        let (paths, parent) = env.commit_paths(commit)?;
        if paths.is_empty() {
            continue;
        }
        let mut undone = true;
        for path in &paths {
            let before = match &parent {
                Some(p) => env.tree_entry(p, path)?,
                None => None,
            };
            if env.tree_entry(at, path)? != before {
                undone = false;
                break;
            }
        }
        if undone {
            return Ok(Some(commit.clone()));
        }
    }
    Ok(None)
}

/// Whether a round with `base` opened now on `node` is anchored, and how
/// (A27 rule 8), the one function anchoring goes through:
/// - `main` when the main ref resolves and holds the base (its tip or its
///   upstream's, or an ancestor of one);
/// - else `first` when the main ref doesn't resolve, or always for a round
///   on a graph range (`graph`: a design node's text, whose base main never
///   holds; the orchestrator on graph.ref revision 2, DECISION 4), and the
///   base is the node's work base: the base its first round recorded when
///   it opened. That anchor is never lost: a first round whose request
///   expired or that never closed keeps it, a replacement round from the
///   same base takes it too, and no later round re-anchors at another base
///   (the orchestrator on Terra's #6207). The requester picks that first
///   base: a residual inside one trust domain (Security / privacy).
pub(crate) fn anchored_in(
    tx: &Connection,
    env: &dyn Env,
    node: &NodeLink,
    base: &str,
    graph: bool,
) -> Result<Option<&'static str>, EntryError> {
    // A graph range never anchors on main, even when a full-shape graph
    // shares history with it (the orchestrator on Terra's #6548): the
    // graph-range rule is checked first.
    if !graph && let Some(tips) = env.main_tips()? {
        for tip in &tips {
            if env.contains(base, tip)? {
                return Ok(Some("main"));
            }
        }
        return Ok(None);
    }
    let first = node_rounds(tx, node, false)?.into_iter().next();
    Ok(match first {
        None => Some("first"),
        Some(r) if r.base == base => Some("first"),
        Some(_) => None,
    })
}

/// Applies one op inside the caller's transaction or savepoint (the mail
/// transaction, for a reply's import), checking A27's rules first. A refused op has
/// written nothing when it returns.
pub(crate) fn apply_in(
    tx: &Connection,
    c: &Change<'_>,
    env: &dyn Env,
) -> Result<Applied, EntryError> {
    let name = c.op.action.name();
    let on = op_node(c, env).ok();
    let class = class_on(tx, c, env, on.as_ref())?;
    // Person ops are refused inside a turn (decided 2026-10-29): a turn's
    // agent can't act as a person through the CLI or MCP.
    if class == Class::Person && c.turn.is_none() && env.in_turn(c.actor)? {
        return Err(EntryError::NotAllowed {
            class,
            op: name.to_string(),
        });
    }
    let c = &Change { class, ..*c };
    let mut changed = Vec::new();
    match &c.op.action {
        Action::Add { kind, new } => {
            let rule = match kind {
                Kind::Finding => Rule::Review,
                Kind::Issue => Rule::Issue,
                Kind::Question => Rule::Ask,
                Kind::Decision => Rule::Decide,
                Kind::Review | Kind::Req => return Err(EntryError::BadOp("kind".into())),
            };
            check_allowed(c.class, rule, name)?;
            let node = target(c, env)?;
            if *kind == Kind::Finding
                && let Some(mut e) = same_open_finding(tx, &node, new)?
            {
                // A second (A27 rule 8): the repeat records a `second`
                // event, its text kept on the event, and a higher severity
                // raises the finding's; nothing is ever lowered.
                check_text(new.text.as_deref())?;
                // An omitted severity is the default, `major` (Terra's code
                // review #6548).
                let sev = new.severity.clone().unwrap_or_else(|| "major".into());
                {
                    check_severity(&sev)?;
                    let rank = |s: Option<&str>| match s {
                        Some("blocker") => 4,
                        Some("major") | None => 3,
                        Some("minor") => 2,
                        Some("info") => 1,
                        Some(_) => 0,
                    };
                    if rank(Some(&sev)) > rank(e.severity.as_deref()) {
                        e.severity = Some(sev);
                    }
                }
                let state = e.state.clone();
                let ch = transition(tx, &mut e, &state, plain("second", new.text.clone()), c)?;
                changed.push(ch);
                return Ok(Applied {
                    changed,
                    existing: Some(e.path()),
                });
            }
            let refs = ref_paths(tx, &new.refs, c, env)?;
            if *kind == Kind::Decision {
                let old = superseded_ids(tx, &node, &new.supersedes, c, env)?;
                let (e, ch) = open(tx, &node, *kind, "accepted", new, refs, Map::new(), c)?;
                changed.push(ch);
                supersede(tx, &node, &old, &e.id, c, &mut changed)?;
            } else {
                changed.push(open(tx, &node, *kind, "open", new, refs, Map::new(), c)?.1);
            }
        }
        Action::Propose(new) => {
            check_allowed(c.class, Rule::Decide, name)?;
            let node = target(c, env)?;
            let refs = ref_paths(tx, &new.refs, c, env)?;
            let old = superseded_ids(tx, &node, &new.supersedes, c, env)?;
            let (mut e, ch) = open(
                tx,
                &node,
                Kind::Decision,
                "proposed",
                new,
                refs,
                Map::new(),
                c,
            )?;
            if !old.is_empty() {
                // Kept for its acceptance, and in the snapshot's next record.
                e.data.insert("supersedes".into(), json!(old));
                update(tx, &e)?;
            }
            changed.push(ch);
        }
        Action::Decide { entry: None, new } => {
            check_allowed(c.class, Rule::Decide, name)?;
            let new = new
                .as_ref()
                .ok_or_else(|| EntryError::BadOp("title".into()))?;
            let node = target(c, env)?;
            let refs = ref_paths(tx, &new.refs, c, env)?;
            let old = superseded_ids(tx, &node, &new.supersedes, c, env)?;
            let (e, ch) = open(
                tx,
                &node,
                Kind::Decision,
                "accepted",
                new,
                refs,
                Map::new(),
                c,
            )?;
            changed.push(ch);
            supersede(tx, &node, &old, &e.id, c, &mut changed)?;
        }
        Action::Decide { entry: Some(r), .. } => {
            check_allowed(c.class, Rule::Decide, name)?;
            let mut e = find(tx, r, c, env)?;
            if e.kind != Kind::Decision || e.state != "proposed" {
                return Err(bad_transition(&e, name));
            }
            let old = e.get_strs("supersedes");
            let node = e.node.clone();
            let id = e.id.clone();
            changed.push(transition(tx, &mut e, "accepted", plain(name, None), c)?);
            supersede(tx, &node, &old, &id, c, &mut changed)?;
        }
        Action::Fix {
            entry,
            sha,
            commits,
            supersede,
        } => {
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Fix, name)?;
            if !matches!(e.state.as_str(), "open" | "reopened") {
                return Err(bad_transition(&e, name));
            }
            let sha = env.commit(sha)?;
            let mut named = vec![sha.clone()];
            for commit in commits {
                let full = env.commit(commit)?;
                if !named.contains(&full) {
                    named.push(full);
                }
            }
            // A finding fixed before `fix_commits` and `fixers` were
            // recorded carries them forward from every fix its history
            // holds (Terra's code reviews #6548 and #7598, finding 4;
            // Opus's #6482, finding 3).
            let (mut declared, mut fixers) = fix_record(tx, &e)?;
            let op_name = if *supersede {
                // A superseded fix is withdrawn on a reopened finding, by
                // one of its fixers: these commits replace every one it
                // declared, which drop out of the checks and stay listed
                // as `superseded_commits` (the orchestrator on Terra's
                // #6548).
                if e.state != "reopened" {
                    return Err(bad_transition(&e, "supersede"));
                }
                if !fixers.iter().any(|f| f == c.actor) {
                    return Err(EntryError::NotAllowed {
                        class: c.class,
                        op: "supersede".into(),
                    });
                }
                let mut gone = e.get_strs("superseded_commits");
                for commit in &declared {
                    if !named.contains(commit) && !gone.contains(commit) {
                        gone.push(commit.clone());
                    }
                }
                // A commit declared again is no longer superseded (Opus's
                // code review #7552, minor 2).
                gone.retain(|c| !named.contains(c));
                e.data.insert("superseded_commits".into(), json!(gone));
                declared = named.clone();
                "supersede"
            } else {
                // Every fix commit ever declared counts, across reopens
                // (decided 2026-10-29), and every actor that recorded one is
                // a fixer, which never verifies (A27 rule 3, R53).
                for commit in &named {
                    if !declared.contains(commit) {
                        declared.push(commit.clone());
                    }
                }
                // A commit declared again is no longer superseded.
                let mut gone = e.get_strs("superseded_commits");
                if gone.iter().any(|c| named.contains(c)) {
                    gone.retain(|c| !named.contains(c));
                    e.data.insert("superseded_commits".into(), json!(gone));
                }
                name
            };
            if !fixers.iter().any(|f| f == c.actor) {
                fixers.push(c.actor.to_string());
            }
            e.data.insert(
                "fix".into(),
                json!({"sha": sha, "commits": named, "by": c.actor, "at": c.now_ms}),
            );
            e.data.insert("fix_commits".into(), json!(declared));
            e.data.insert("fixers".into(), json!(fixers));
            let step = Step {
                sha: Some(sha),
                ..plain(op_name, None)
            };
            changed.push(transition(tx, &mut e, "fixed", step, c)?);
        }
        Action::Verify { entry, at } => {
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Verify, name)?;
            // A verified finding takes one more verify, a re-verify (decided
            // 2026-10-29), recorded as op `reverify`.
            let op_name = match e.state.as_str() {
                "fixed" => name,
                "verified" => "reverify",
                _ => return Err(bad_transition(&e, name)),
            };
            // The fixer never verifies (R53): every recorded fixer, and the
            // last fix's `by` for a finding fixed before `fixers` was
            // recorded (Opus's code review #6482, finding 3).
            if fix_record(tx, &e)?.1.iter().any(|f| f == c.actor) {
                return Err(EntryError::NotAllowed {
                    class: c.class,
                    op: name.to_string(),
                });
            }
            let at = env.commit(at)?;
            let fix = e.fix_sha().unwrap_or_default().to_string();
            let not_covered = || EntryError::NotCovered {
                entry: e.path(),
                fix: fix.clone(),
                at: at.clone(),
            };
            if fix.is_empty() || !env.contains(&fix, &at)? {
                return Err(not_covered());
            }
            // Inside a round on the node, open or closed (A27 rule 3).
            let rounds = node_rounds(tx, &e.node, false)?;
            let holding = |sha: &str| -> Result<Vec<&Round>, EntryError> {
                let mut out = Vec::new();
                for r in &rounds {
                    if env.contains(sha, &r.head)? && !env.contains(sha, &r.base)? {
                        out.push(r);
                    }
                }
                Ok(out)
            };
            let covering = holding(&fix)?;
            if covering.is_empty() {
                return Err(not_covered());
            }
            // Every declared fix commit, rebuilt from the history for a
            // finding recorded before they were kept (Terra's #8466).
            let mut fix_commits = fix_record(tx, &e)?.0;
            if fix_commits.is_empty() {
                fix_commits.push(fix.clone());
            }
            if let Some(commit) = fix_absent(env, &fix_commits, &at)? {
                return Err(EntryError::FixAbsent {
                    entry: e.path(),
                    commit,
                    at,
                });
            }
            // The verify's walk: the lowest-numbered round holding V that
            // has an anchored walk (else the lowest such covering the fix),
            // then its longest walk. Without one, verify is refused (the
            // orchestrator on Terra's #6548).
            let at_rounds = holding(&at)?;
            let mut chosen = None;
            for r in at_rounds.iter().chain(covering.iter()) {
                if let Some(base) = walk_base(&rounds, r)? {
                    chosen = Some((*r, base));
                    break;
                }
            }
            let Some((round, base)) = chosen else {
                return Err(not_covered());
            };
            let mut fix_paths = env.changed_paths(&base, &fix)?;
            for commit in &fix_commits {
                fix_paths.extend(env.commit_paths(commit)?.0);
            }
            e.data.insert(
                "verified".into(),
                json!({
                    "sha": at, "by": c.actor, "at": c.now_ms, "review": round.id,
                    "fix_commits": fix_commits, "fix_paths": fix_paths,
                }),
            );
            let step = Step {
                at_sha: Some(at),
                ..plain(op_name, None)
            };
            changed.push(transition(tx, &mut e, "verified", step, c)?);
        }
        Action::Reopen { entry, at, text } => {
            check_text(text.as_deref())?;
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Review, name)?;
            match e.state.as_str() {
                "fixed" => {
                    let at = env.commit(
                        at.as_deref()
                            .ok_or_else(|| EntryError::BadOp("at".into()))?,
                    )?;
                    let fix = e.fix_sha().unwrap_or_default().to_string();
                    if fix.is_empty() || !env.contains(&fix, &at)? {
                        return Err(EntryError::FixAfterRange {
                            entry: e.path(),
                            fix,
                            at,
                        });
                    }
                    let step = Step {
                        at_sha: Some(at),
                        ..plain(name, text.clone())
                    };
                    changed.push(transition(tx, &mut e, "reopened", step, c)?);
                }
                "disputed" => {
                    changed.push(transition(
                        tx,
                        &mut e,
                        "open",
                        plain(name, text.clone()),
                        c,
                    )?);
                }
                _ => return Err(bad_transition(&e, name)),
            }
        }
        Action::Reject { entry, text } => {
            check_text(Some(text))?;
            let mut e = find(tx, entry, c, env)?;
            match e.kind {
                Kind::Finding => {
                    check_allowed(c.class, Rule::Fix, name)?;
                    if !matches!(e.state.as_str(), "open" | "reopened") {
                        return Err(bad_transition(&e, name));
                    }
                }
                Kind::Decision => {
                    check_allowed(c.class, Rule::Decide, name)?;
                    if e.state != "proposed" {
                        return Err(bad_transition(&e, name));
                    }
                }
                _ => return Err(bad_transition(&e, name)),
            }
            changed.push(transition(
                tx,
                &mut e,
                "rejected",
                plain(name, Some(text.clone())),
                c,
            )?);
        }
        Action::Dispute { entry, text } => {
            check_text(Some(text))?;
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Review, name)?;
            if e.state != "rejected" {
                return Err(bad_transition(&e, name));
            }
            changed.push(transition(
                tx,
                &mut e,
                "disputed",
                plain(name, Some(text.clone())),
                c,
            )?);
        }
        Action::Residual { entry, text, at } => {
            check_text(Some(text))?;
            check_allowed(c.class, Rule::Residual, name)?;
            residual(tx, entry, text, at.as_deref(), c, env, &mut changed)?;
        }
        Action::Outdated { entry, text } => {
            check_text(text.as_deref())?;
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Review, name)?;
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            changed.push(transition(
                tx,
                &mut e,
                "outdated",
                plain(name, text.clone()),
                c,
            )?);
        }
        Action::Duplicate { entry, of } => {
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Finding {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Review, name)?;
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            let other = find(tx, of, c, env)?;
            if other.kind != Kind::Finding || other.path() == e.path() {
                return Err(EntryError::BadOp("of".into()));
            }
            e.data.insert("duplicate_of".into(), other.path().into());
            changed.push(transition(tx, &mut e, "duplicate", plain(name, None), c)?);
        }
        Action::Answer { entry, text } => {
            check_text(Some(text))?;
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Question {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Ask, name)?;
            let asked = e.asked_of().map(str::to_string);
            let own = e.opened_by() == Some(c.actor);
            // Only the asker, or the actor it was asked of: a question asked
            // of no one is the asker's to answer (A27 rule 4; Terra's
            // review of e821ea6e..a1412008, finding 6).
            if !(own || asked.as_deref() == Some(c.actor)) {
                return Err(EntryError::NotAllowed {
                    class: c.class,
                    op: "answer (a question asked of someone else)".into(),
                });
            }
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            e.data.insert(
                "answer".into(),
                json!({"by": c.actor, "at": c.now_ms, "message": c.message, "text": text}),
            );
            e.data.insert("answered_by".into(), json!([c.actor]));
            changed.push(transition(
                tx,
                &mut e,
                "answered",
                plain(name, Some(text.clone())),
                c,
            )?);
        }
        Action::Close { entry } => {
            let mut e = find(tx, entry, c, env)?;
            match e.kind {
                Kind::Question => {
                    check_allowed(c.class, Rule::Ask, name)?;
                    if e.opened_by() != Some(c.actor) {
                        return Err(EntryError::NotAllowed {
                            class: c.class,
                            op: "close (a question someone else asked)".into(),
                        });
                    }
                    if e.state != "answered" {
                        return Err(bad_transition(&e, name));
                    }
                }
                Kind::Finding => {
                    check_allowed(c.class, Rule::Review, name)?;
                    if !matches!(e.state.as_str(), "rejected" | "disputed") {
                        return Err(bad_transition(&e, name));
                    }
                }
                _ => return Err(bad_transition(&e, name)),
            }
            changed.push(transition(tx, &mut e, "closed", plain(name, None), c)?);
        }
        Action::Withdraw { entry } => {
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Question {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Ask, name)?;
            if e.opened_by() != Some(c.actor) {
                return Err(EntryError::NotAllowed {
                    class: c.class,
                    op: "withdraw (a question someone else asked)".into(),
                });
            }
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            changed.push(transition(tx, &mut e, "withdrawn", plain(name, None), c)?);
        }
        Action::Resolve { entry } => {
            let mut e = find(tx, entry, c, env)?;
            if e.kind != Kind::Issue {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Issue, name)?;
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            changed.push(transition(tx, &mut e, "resolved", plain(name, None), c)?);
        }
        Action::Move { entry, to } => {
            let mut e = find(tx, entry, c, env)?;
            if !matches!(e.kind, Kind::Question | Kind::Issue) {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Issue, name)?;
            if e.state != "open" {
                return Err(bad_transition(&e, name));
            }
            let dest = env.node(to)?;
            if dest.uuid == e.node.uuid {
                return Err(EntryError::BadOp("to".into()));
            }
            // The entry goes on open at the destination, pointing back.
            let carried = New {
                title: e.title.clone(),
                text: e.text.clone(),
                asked_of: e.asked_of().map(str::to_string),
                blocking: e.blocking,
                requirement: Vec::new(),
                ..New::default()
            };
            let (copy, ch) = open(
                tx,
                &dest,
                e.kind,
                "open",
                &carried,
                vec![e.path()],
                Map::new(),
                c,
            )?;
            changed.push(ch);
            e.data.insert("moved_to".into(), copy.path().into());
            changed.push(transition(tx, &mut e, "moved", plain(name, None), c)?);
        }
        Action::ReviewComplete {
            round,
            base,
            sha,
            verdict,
        } => {
            let mut e = find(tx, round, c, env)?;
            if e.kind != Kind::Review {
                return Err(bad_transition(&e, name));
            }
            check_allowed(c.class, Rule::Complete, name)?;
            if e.is_terminal() {
                return Err(bad_transition(&e, name));
            }
            let (want_base, want_sha) = e
                .range()
                .ok_or_else(|| EntryError::RangeMismatch { entry: e.path() })?;
            let same = |given: &str, want: &str| -> Result<bool, EntryError> {
                Ok(env.commit(given)? == env.commit(want)?)
            };
            if !same(base, &want_base)? || !same(sha, &want_sha)? {
                return Err(EntryError::RangeMismatch { entry: e.path() });
            }
            // The reviewer this actor completes for: itself, or a role
            // reviewer whose lease it holds now.
            let done = e.completed_by();
            let names = e.reviewers();
            let named: Vec<&String> = names
                .iter()
                .filter(|r| {
                    r.as_str() == c.actor || (r.starts_with("role:") && env.holds(c.actor, r))
                })
                .collect();
            if named.is_empty() {
                return Err(EntryError::NotAllowed {
                    class: c.class,
                    op: "review.complete (a round that doesn't name you)".into(),
                });
            }
            let Some(reviewer) = named.into_iter().find(|r| !done.contains(r)).cloned() else {
                return Err(bad_transition(&e, name));
            };
            // A findings verdict is backed by a finding this actor opened,
            // reopened or seconded on the node since the round opened (A27
            // rule 8).
            if verdict.as_str() == "findings" {
                let backed: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM entry_events
                      WHERE node_uuid = ?1 AND entry_id LIKE 'F%' AND actor = ?2
                        AND op IN ('open', 'reopen', 'second')
                        AND seq > (SELECT MIN(seq) FROM entry_events
                                   WHERE node_uuid = ?1 AND entry_id = ?3))",
                    params![e.node.uuid, c.actor, e.id],
                    |r| r.get(0),
                )?;
                if !backed {
                    return Err(EntryError::VerdictUnbacked { entry: e.path() });
                }
            }
            let mut completed = e
                .data
                .get("completed")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            completed.push(json!({
                "reviewer": reviewer,
                "by": c.actor,
                "verdict": verdict.as_str(),
                "at": c.now_ms,
            }));
            let all = names.iter().all(|r| *r == reviewer || done.contains(r));
            e.data.insert("completed".into(), Value::Array(completed));
            let step = Step {
                at_sha: Some(env.commit(sha)?),
                ..plain(name, None)
            };
            let to = if all { "closed" } else { "answered" };
            changed.push(transition(tx, &mut e, to, step, c)?);
        }
        Action::Req {
            req,
            state,
            at,
            text,
        } => {
            check_text(text.as_deref())?;
            let rule = match state {
                ReqState::Met | ReqState::Unmet => Rule::Verify,
                ReqState::Waived => Rule::Residual,
            };
            check_allowed(c.class, rule, &format!("req {}", state.as_str()))?;
            let node = target(c, env)?;
            let at = at.as_deref().map(|s| env.commit(s)).transpose()?;
            req_change(
                tx,
                &node,
                req,
                *state,
                at,
                text.clone(),
                c,
                env,
                &mut changed,
            )?;
        }
    }
    Ok(Applied {
        changed,
        existing: None,
    })
}

/// Opens requirement `req`'s entry on `node` as `unverified` if it has
/// none, then moves it to `state` (A27: `unverified` → `met` | `unmet` |
/// `waived`; `unmet` → `met` | `waived`; `met` → `unmet`).
#[allow(clippy::too_many_arguments)]
fn req_change(
    tx: &Connection,
    node: &NodeLink,
    req: &str,
    state: ReqState,
    at: Option<String>,
    note: Option<String>,
    c: &Change<'_>,
    env: &dyn Env,
    out: &mut Vec<Changed>,
) -> Result<(), EntryError> {
    check_req_id(req)?;
    // The graph the requirement's node is in, as leases key it, so a
    // reader of one graph never takes another's state for a node UUID they
    // share (the orchestrator on Terra's #8445, DECISION 1).
    let graph = env.node_folder(node)?;
    let folder = match &graph {
        NodeFolder::In(f) => Some(f.clone()),
        NodeFolder::Any | NodeFolder::Nowhere => None,
    };
    let mut e = match load(tx, &node.uuid, req)? {
        Some(e) => e,
        None => {
            let mut data = Map::new();
            data.insert("node".into(), node.id.clone().into());
            data.insert("opened".into(), stamp(c));
            if let Some(f) = &folder {
                data.insert("graph_folder".into(), f.clone().into());
            }
            let e = Entry {
                node: node.clone(),
                id: req.to_string(),
                kind: Kind::Req,
                state: "unverified".into(),
                severity: None,
                slot: None,
                blocking: false,
                // The requirement's text is the node body's `R<n>` line
                // (A28); the entry holds only its id.
                title: req.to_string(),
                text: None,
                data,
                opened_ms: c.now_ms,
                updated_ms: c.now_ms,
            };
            insert(tx, &e)?;
            out.push(record(tx, &e, plain("open", None), c)?);
            e
        }
    };
    // Rows key on the node's UUID alone, so a requirement another graph
    // recorded is never written from this one: a folder that differs, or a
    // node whose graph can't be told, is refused (the orchestrator on
    // Terra's #8791), never written across graphs.
    if let Some(recorded) = e.data.get("graph_folder").and_then(Value::as_str) {
        let other = match &graph {
            // Folders fold ASCII case, like lease keys: a case-only
            // difference on a case-sensitive filesystem is a residual
            // (the orchestrator on Opus #10156, 3).
            NodeFolder::In(f) => !f.eq_ignore_ascii_case(recorded),
            NodeFolder::Nowhere => true,
            NodeFolder::Any => false,
        };
        if other {
            return Err(EntryError::NotAllowed {
                class: c.class,
                op: format!("req on {req}, which another graph's node recorded"),
            });
        }
    } else if let Some(f) = folder {
        e.data.insert("graph_folder".into(), f.into());
    }
    let op = format!("req {}", state.as_str());
    let ok = match state {
        ReqState::Met => matches!(e.state.as_str(), "unverified" | "unmet"),
        ReqState::Unmet => matches!(e.state.as_str(), "unverified" | "met"),
        ReqState::Waived => matches!(e.state.as_str(), "unverified" | "unmet"),
    };
    if !ok {
        return Err(bad_transition(&e, &op));
    }
    match state {
        ReqState::Met => {
            e.data.insert(
                "verified".into(),
                json!({"sha": at, "by": c.actor, "at": c.now_ms}),
            );
        }
        ReqState::Unmet => {
            e.data.remove("verified");
        }
        ReqState::Waived => {}
    }
    let step = Step {
        at_sha: at,
        ..plain("req", note)
    };
    out.push(transition(tx, &mut e, state.as_str(), step, c)?);
    Ok(())
}

/// Records a residual (A29): a finding or an issue becomes `residual`; a
/// requirement `waived`, noted `residual: <reason>`; an open question
/// stays open and gains a `residual` issue that names it.
#[allow(clippy::too_many_arguments)]
fn residual(
    tx: &Connection,
    r: &EntryRef,
    reason: &str,
    at: Option<&str>,
    c: &Change<'_>,
    env: &dyn Env,
    out: &mut Vec<Changed>,
) -> Result<(), EntryError> {
    let node = node_of(r, c, env)?;
    if r.id.starts_with('R') {
        let note = format!("residual: {reason}");
        check_text(Some(&note))?;
        return req_change(
            tx,
            &node,
            &r.id,
            ReqState::Waived,
            None,
            Some(note),
            c,
            env,
            out,
        );
    }
    let mut e = load(tx, &node.uuid, &r.id)?
        .ok_or_else(|| EntryError::NotFound(format!("{}#{}", node.id, r.id)))?;
    match e.kind {
        Kind::Finding
            if matches!(
                e.state.as_str(),
                "open" | "reopened" | "fixed" | "rejected" | "disputed"
            ) => {}
        Kind::Issue if e.state == "open" => {}
        // A verified finding stays verified: the residual is an issue
        // naming it, bound to the verification it waives (V) and the
        // walk's final SHA at `at` (F), and it passes the gate only while
        // both still hold (A29, "Re-verify"; Terra's ninth review,
        // finding 3).
        Kind::Question | Kind::Finding
            if e.state == "open" && e.kind == Kind::Question
                || e.state == "verified" && e.kind == Kind::Finding =>
        {
            let mut extra = Map::new();
            if e.kind == Kind::Finding {
                let at = env.commit(at.ok_or_else(|| EntryError::BadOp("at".into()))?)?;
                let verified = e.verified_sha().unwrap_or_default().to_string();
                let rounds = node_rounds(tx, &node, true)?;
                let not_covered = || EntryError::NotCovered {
                    entry: e.path(),
                    fix: e.fix_sha().unwrap_or_default().to_string(),
                    at: at.clone(),
                };
                let last = final_round(&rounds, &at, env)?.ok_or_else(not_covered)?;
                // The walk the residual accepts, by its work base B: the
                // first, deepest, walk ending at the final round whose
                // range holds V, as the gate takes them. A later walk from
                // another base may leave more fix commits outside, which
                // needs a new decision (Terra's #9504, major 3).
                let mut base = None;
                for w in walks_to(&rounds, last)? {
                    if env.contains(&verified, &w[w.len() - 1].head)?
                        && !env.contains(&verified, &w[0].base)?
                    {
                        base = Some(w[0].base.clone());
                        break;
                    }
                }
                let base = base.ok_or_else(not_covered)?;
                extra.insert(
                    "waives".into(),
                    json!({
                        "entry": e.path(),
                        "verified": verified,
                        "head": last.head,
                        "base": base,
                    }),
                );
            }
            let title = truncate(&format!("residual: {}", e.title), TITLE_MAX);
            let new = New {
                title,
                text: Some(reason.to_string()),
                ..New::default()
            };
            let (mut issue, ch) = open(
                tx,
                &node,
                Kind::Issue,
                "open",
                &new,
                vec![e.path()],
                extra,
                c,
            )?;
            out.push(ch);
            out.push(transition(
                tx,
                &mut issue,
                "residual",
                plain("residual", Some(reason.to_string())),
                c,
            )?);
            return Ok(());
        }
        _ => return Err(bad_transition(&e, "residual")),
    }
    out.push(transition(
        tx,
        &mut e,
        "residual",
        plain("residual", Some(reason.to_string())),
        c,
    )?);
    Ok(())
}

/// `s` cut to at most `max` bytes, at a character boundary.
fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Opens a `question` entry from a `question` message sent with `--node`
/// (A27, rule 8): asked of its recipient, blocking as `--blocking` says.
/// A binding defaulted from leases or inherited by a reply never comes
/// here (R36).
#[allow(clippy::too_many_arguments)]
// The send path calls it with P2's call sites in `mail.rs`.
#[allow(dead_code)]
pub(crate) fn open_question_from_mail_in(
    tx: &Connection,
    node: &NodeLink,
    title: &str,
    asked_of: Option<&str>,
    blocking: bool,
    sender: &str,
    message_id: &str,
    now_ms: u64,
) -> Result<Changed, EntryError> {
    let op = Op {
        node: None,
        action: Action::Add {
            kind: Kind::Question,
            new: New::default(),
        },
    };
    let c = Change {
        op: &op,
        actor: sender,
        class: Class::Other,
        node: Some(node),
        message: Some(message_id),
        turn: None,
        now_ms,
    };
    let new = New {
        title: truncate(title, TITLE_MAX),
        asked_of: asked_of.map(str::to_string),
        blocking,
        ..New::default()
    };
    Ok(open(
        tx,
        node,
        Kind::Question,
        "open",
        &new,
        Vec::new(),
        Map::new(),
        &c,
    )?
    .1)
}

/// Opens a `review` entry from a `review_request` message sent with
/// `--node` and `--range` (A27, rule 8): its range (full SHAs, resolved
/// by the caller), and its recipient mailboxes as its reviewers. Only
/// `review.complete` moves it after this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_review_from_mail_in(
    tx: &Connection,
    node: &NodeLink,
    title: &str,
    range: (&str, &str),
    reviewers: &[String],
    sender: &str,
    message_id: &str,
    now_ms: u64,
    anchored: Option<&str>,
) -> Result<Option<Changed>, EntryError> {
    // Stored only as full lowercase commit ids, which the gate's history
    // reads match exactly (Terra's code review #10220, major 4): callers
    // resolve the range first ([`Store::entry_open_round`]).
    if !full_oid(range.0) || !full_oid(range.1) {
        return Err(EntryError::BadOp(format!(
            "a review round's range must be two full lowercase commit ids, not {}..{}",
            range.0, range.1
        )));
    }
    // One request opens one round, on whichever node opens it first (the
    // send's primary node): a resend, or a second node of the same
    // request, opens nothing more, so a request's expiry notice and its
    // answered state each read one round (Terra's review of 3b7024a2).
    let opened: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM entries
                        WHERE kind = 'review' AND json_extract(data, '$.request') = ?1)",
        params![message_id],
        |r| r.get(0),
    )?;
    if opened {
        return Ok(None);
    }
    let op = Op {
        node: None,
        action: Action::Add {
            kind: Kind::Review,
            new: New::default(),
        },
    };
    let c = Change {
        op: &op,
        actor: sender,
        class: Class::Other,
        node: Some(node),
        message: Some(message_id),
        turn: None,
        now_ms,
    };
    let new = New {
        title: truncate(title, TITLE_MAX),
        ..New::default()
    };
    let mut extra = Map::new();
    extra.insert("range".into(), json!({"base": range.0, "sha": range.1}));
    extra.insert("reviewers".into(), json!(reviewers));
    extra.insert("completed".into(), json!([]));
    extra.insert("request".into(), message_id.into());
    // [`anchored_in`]'s answer, read by the caller in this transaction.
    extra.insert("anchored".into(), anchored.map_or(Value::Null, Value::from));
    Ok(open(
        tx,
        node,
        Kind::Review,
        "requested",
        &new,
        Vec::new(),
        extra,
        &c,
    )?
    .1
    .into())
}

/// The review round a `review_request` sent with `--node` and `--range`
/// opens (A27 rule 8), inside the caller's transaction: the mail send's,
/// so the request and its round commit together (Terra's #6548), or
/// [`Store::entry_open_round`]'s. Anchored through [`anchored_in`] in that
/// transaction. The range must already be full lowercase commit ids
/// (resolved through [`Env::commit`]); `None` when the request opened its
/// round before (a retried send).
#[allow(clippy::too_many_arguments)]
pub fn open_round_in(
    tx: &Connection,
    node: &NodeLink,
    title: &str,
    range: (&str, &str),
    reviewers: &[String],
    sender: &str,
    message_id: &str,
    now_ms: u64,
    graph: bool,
    env: &dyn Env,
) -> Result<Option<RoundOpened>, EntryError> {
    crate::store::check_mail_schema(tx)?;
    let anchored = anchored_in(tx, env, node, range.0, graph)?;
    Ok(open_review_from_mail_in(
        tx, node, title, range, reviewers, sender, message_id, now_ms, anchored,
    )?
    .map(|changed| RoundOpened { changed, anchored }))
}

/// A round [`open_round_in`] opened, and how it's anchored (`main`,
/// `first`, or none: A27 rule 8's anchoring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundOpened {
    pub changed: Changed,
    pub anchored: Option<&'static str>,
}

/// Which entries a list read returns.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub kind: Option<Kind>,
    pub state: Option<String>,
    pub severity: Option<String>,
    pub asked_of: Option<String>,
    pub blocking: Option<bool>,
}

impl Filter {
    fn matches(&self, e: &Entry) -> bool {
        self.kind.is_none_or(|k| k == e.kind)
            && self.state.as_deref().is_none_or(|s| s == e.state)
            && self
                .severity
                .as_deref()
                .is_none_or(|s| e.severity.as_deref() == Some(s))
            && self
                .asked_of
                .as_deref()
                .is_none_or(|a| e.asked_of() == Some(a))
            && self.blocking.is_none_or(|b| b == e.blocking)
    }
}

/// A node's open entries, counted (A27's `entries_open`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenCounts {
    pub finding: u64,
    pub question: u64,
    pub issue: u64,
    pub req: u64,
}

/// A node's decisions in force (A27, rule 6): its accepted decisions, the
/// latest per slot; one with no slot is its own slot.
pub fn decisions_in_force(entries: &[Entry]) -> Vec<&Entry> {
    let mut out: Vec<&Entry> = Vec::new();
    let number = |e: &Entry| e.id[1..].parse::<u64>().unwrap_or_default();
    for e in entries
        .iter()
        .filter(|e| e.kind == Kind::Decision && e.state == "accepted")
    {
        match &e.slot {
            None => out.push(e),
            Some(slot) => match out.iter().position(|o| o.slot.as_ref() == Some(slot)) {
                Some(i) if number(out[i]) < number(e) => out[i] = e,
                Some(_) => {}
                None => out.push(e),
            },
        }
    }
    out.sort_by_key(|e| number(e));
    out
}

/// A node's open blocking questions (A27, rule 7): it is input required
/// while there is one.
pub fn input_required(entries: &[Entry]) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| e.kind == Kind::Question && e.blocking && e.state == "open")
        .collect()
}

/// One condition of the gate that fails (A29).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFailure {
    /// The condition's number, 1 to 5.
    pub condition: u8,
    /// `findings_open`, `fixes_unverified`, `not_contained`,
    /// `input_required`, `requirements_open` or `design_edit`.
    pub reason: &'static str,
    /// The entries or requirements it names (`F3`, `R2`).
    pub items: Vec<String>,
    /// Whether `--residual` may record it (condition 2's out-of-range
    /// verified findings and condition 5 never).
    pub residual: bool,
    pub hint: String,
    /// Condition 2's facts: the finding, the walk's final SHA `final`,
    /// and the `paths` that failed; null elsewhere.
    pub data: Value,
}

impl GateFailure {
    pub fn to_json(&self) -> Value {
        json!({
            "condition": self.condition,
            "reason": self.reason,
            "items": self.items,
            "residual": self.residual,
            "hint": self.hint,
            "data": self.data,
        })
    }
}

/// The gate's verdict for conditions 1 to 4 (A29). Condition 5 reads
/// design-node content in git, so the caller adds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gate {
    /// `S`: the node's highest `entry_events` seq when it was read.
    pub upto: u64,
    pub failures: Vec<GateFailure>,
    /// Condition 2 for `minor` and `info` findings.
    pub warnings: Vec<GateFailure>,
    /// The walk `(B, F)` from a work base to the final round by ancestry
    /// at `at`, the deepest, whether or not condition 2 needed it; none
    /// without one (A29 step 4, the settle record's `range`).
    pub range: Option<(String, String)>,
}

impl Gate {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Condition 2's containment check (A29, "Containment, and the squash
/// bridge"; not the bridge itself, P6's), with its git reads memoized
/// across the node's findings.
struct Containment<'a> {
    at: &'a str,
    node_path: Option<&'a str>,
    env: &'a dyn Env,
    entries: &'a [Entry],
    /// The final round's head, F, and the walks ending at that round.
    walks: Vec<Vec<Round>>,
    last: Option<Round>,
    paths: std::collections::HashMap<(String, String), BTreeSet<String>>,
    /// Per walk: the paths of P whose content at `at` differs from F's.
    content: std::collections::HashMap<usize, Vec<String>>,
}

impl<'a> Containment<'a> {
    fn new(
        entries: &'a [Entry],
        at: &'a str,
        node_path: Option<&'a str>,
        env: &'a dyn Env,
    ) -> Result<Self, EntryError> {
        let rounds = rounds_in(entries, true);
        let last = final_round(&rounds, at, env)?.cloned();
        let walks = match &last {
            Some(l) => walks_to(&rounds, l)?
                .into_iter()
                .map(|w| w.into_iter().cloned().collect())
                .collect(),
            None => Vec::new(),
        };
        Ok(Self {
            at,
            node_path,
            env,
            entries,
            walks,
            last,
            paths: Default::default(),
            content: Default::default(),
        })
    }

    fn range(&self) -> Option<(String, String)> {
        let w = self.walks.first()?;
        Some((w[0].base.clone(), w[w.len() - 1].head.clone()))
    }

    fn changed(&mut self, base: &str, head: &str) -> Result<BTreeSet<String>, EntryError> {
        let key = (base.to_string(), head.to_string());
        if let Some(p) = self.paths.get(&key) {
            return Ok(p.clone());
        }
        let p = self.env.changed_paths(base, head)?;
        self.paths.insert(key, p.clone());
        Ok(p)
    }

    /// The paths of walk `i`'s P that differ between `at` and F.
    fn differing(&mut self, i: usize) -> Result<Vec<String>, EntryError> {
        if let Some(d) = self.content.get(&i) {
            return Ok(d.clone());
        }
        let walk = self.walks[i].clone();
        let f = walk[walk.len() - 1].head.clone();
        let mut p = BTreeSet::new();
        for r in &walk {
            p.extend(self.changed(&r.base, &r.head)?);
        }
        let out = paths_differing(self.env, &p, self.at, &f, self.node_path)?;
        self.content.insert(i, out.clone());
        Ok(out)
    }

    /// Whether `c` lies in walk `w`: in its final SHA's history and not
    /// in its base's.
    fn inside(&self, c: &str, w: &[Round]) -> Result<bool, EntryError> {
        Ok(self.env.contains(c, &w[w.len() - 1].head)? && !self.env.contains(c, &w[0].base)?)
    }

    /// Condition 2 for one verified finding whose SHA `at` holds: `None`
    /// when it's contained and needs no re-verify (or a bound residual
    /// still accepts that it does).
    fn check(&mut self, e: &Entry) -> Result<Option<GateFailure>, EntryError> {
        let v = e.verified_sha().unwrap_or_default().to_string();
        let verified = e.data.get("verified").cloned().unwrap_or(Value::Null);
        let strs = |key: &str| -> Vec<String> {
            verified
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        let fix_commits = {
            let recorded = strs("fix_commits");
            if recorded.is_empty() {
                e.fix_sha().map(str::to_string).into_iter().collect()
            } else {
                recorded
            }
        };
        let fix_paths: BTreeSet<String> = strs("fix_paths").into_iter().collect();
        let f = self.last.as_ref().map(|l| l.head.clone());
        let fail = |reason: &'static str, residual: bool, hint: String, data: Value| GateFailure {
            condition: 2,
            reason,
            items: vec![e.id.clone()],
            residual,
            hint,
            data,
        };
        if f.is_none() {
            return Ok(Some(fail(
                "not_covered",
                false,
                format!(
                    "no closed review round's head is in {}'s history: fetch and pull, or review                      the gap",
                    self.at
                ),
                json!({"entry": e.path(), "final": Value::Null, "paths": []}),
            )));
        }
        let f = f.unwrap_or_default();
        let mut covering: Option<(usize, Vec<String>)> = None;
        let mut reverify: Option<(Vec<String>, Vec<String>)> = None;
        for i in 0..self.walks.len() {
            let walk = self.walks[i].clone();
            if v.is_empty() || !self.inside(&v, &walk)? {
                continue;
            }
            let differing = self.differing(i)?;
            if !differing.is_empty() {
                covering.get_or_insert((i, differing));
                continue;
            }
            // Contained by this walk; does it need re-verify?
            let mut outside = Vec::new();
            for c in &fix_commits {
                if !self.inside(c, &walk)? {
                    outside.push(c.clone());
                }
            }
            let touched: Vec<String> = if v == f {
                Vec::new()
            } else {
                self.changed(&v, &f)?
                    .intersection(&fix_paths)
                    .cloned()
                    .collect()
            };
            if outside.is_empty() && touched.is_empty() {
                return Ok(None);
            }
            if self.residual_holds(e, &v, &walk[0].base, &f, &fix_paths)? {
                return Ok(None);
            }
            reverify.get_or_insert((touched, outside));
        }
        if let Some((touched, outside)) = reverify {
            return Ok(Some(fail(
                "needs_reverify",
                true,
                format!(
                    "{} changed after its verification, or a fix commit lies outside the                      reviewed walk: a reviewer other than the fixer verifies it again at a head                      that holds the change, or a coordinator records a residual",
                    e.path()
                ),
                json!({"entry": e.path(), "final": f, "paths": touched, "outside": outside}),
            )));
        }
        if let Some((_, paths)) = covering {
            return Ok(Some(fail(
                "not_covered",
                false,
                format!(
                    "{} doesn't hold what the rounds reviewed: the merge dropped or altered                      reviewed changes, or a later change needs a round of its own",
                    self.at
                ),
                json!({"entry": e.path(), "final": f, "paths": paths}),
            )));
        }
        Ok(Some(fail(
            "not_covered",
            false,
            format!(
                "{} was verified outside every reviewed walk ending at {f}: review the range that                  holds it",
                e.path()
            ),
            json!({"entry": e.path(), "final": f, "paths": []}),
        )))
    }

    /// A residual issue naming the finding and bound to its verification
    /// `v` and the walk it accepted, by its work base `b`, whose head `h`
    /// is `f` or in its history with nothing after it touching the fix's
    /// paths (A29, "Re-verify"). A residual recorded without its base
    /// binds nothing (Terra's #9504, major 3).
    fn residual_holds(
        &mut self,
        e: &Entry,
        v: &str,
        b: &str,
        f: &str,
        fix_paths: &BTreeSet<String>,
    ) -> Result<bool, EntryError> {
        let path = e.path();
        let bound: Vec<String> = self
            .entries
            .iter()
            .filter(|i| i.kind == Kind::Issue && i.state == "residual")
            .filter(|i| i.refs().contains(&path))
            .filter_map(|i| {
                let w = i.data.get("waives")?;
                (w.get("verified")?.as_str()? == v && w.get("base")?.as_str()? == b)
                    .then(|| w.get("head")?.as_str().map(str::to_string))?
            })
            .collect();
        for h in bound {
            if h == f {
                return Ok(true);
            }
            if self.env.contains(&h, f)? && self.changed(&h, f)?.is_disjoint(fix_paths) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn serious(e: &Entry) -> bool {
    matches!(e.severity.as_deref(), Some("blocker" | "major") | None)
}

/// Evaluates conditions 1 to 4 of A29's gate over a node's entries.
/// `requirements` are the `R<n>` ids of the node's `# Requirements`;
/// `at` is a full SHA; `node_path` is the node file's path from the
/// repository's top, compared without its `status`, `updated` and
/// `# Ledger` ([`node_revision_text`]).
pub fn evaluate_gate(
    entries: &[Entry],
    requirements: &[String],
    at: &str,
    upto: u64,
    node_path: Option<&str>,
    env: &dyn Env,
) -> Result<Gate, EntryError> {
    let mut gate = Gate {
        upto,
        ..Gate::default()
    };
    let findings = || entries.iter().filter(|e| e.kind == Kind::Finding);
    // 1. No open serious finding.
    let open: Vec<String> = findings()
        .filter(|e| serious(e))
        .filter(|e| {
            matches!(
                e.state.as_str(),
                "open" | "reopened" | "rejected" | "disputed"
            )
        })
        .map(|e| e.id.clone())
        .collect();
    if !open.is_empty() {
        gate.failures.push(GateFailure {
            condition: 1,
            reason: "findings_open",
            items: open,
            residual: true,
            hint: "fix them and have them verified, or have a coordinator record a residual".into(),
            data: Value::Null,
        });
    }
    // 2. Fixes are verified, at a SHA `at` holds.
    let mut unverified = (Vec::new(), Vec::new());
    let mut uncontained = (Vec::new(), Vec::new());
    let mut verified_in: Vec<&Entry> = Vec::new();
    for e in findings() {
        let list = |pair: &mut (Vec<String>, Vec<String>)| {
            if serious(e) {
                pair.0.push(e.id.clone());
            } else {
                pair.1.push(e.id.clone());
            }
        };
        match e.state.as_str() {
            "fixed" => list(&mut unverified),
            "verified" => {
                let held = match e.verified_sha() {
                    Some(sha) => env.contains(sha, at).unwrap_or(false),
                    None => false,
                };
                if held {
                    verified_in.push(e);
                } else {
                    list(&mut uncontained);
                }
            }
            _ => {}
        }
    }
    let fixes = |items: Vec<String>| GateFailure {
        condition: 2,
        reason: "fixes_unverified",
        items,
        residual: true,
        hint: "a reviewer verifies each fix at a SHA that holds it".into(),
        data: Value::Null,
    };
    let contained = |items: Vec<String>| GateFailure {
        condition: 2,
        reason: "not_contained",
        items,
        residual: false,
        hint: format!(
            "verified at a SHA that {at} doesn't hold: fetch and pull, or run the gate at a head \
             that holds it"
        ),
        data: Value::Null,
    };
    if !unverified.0.is_empty() {
        gate.failures.push(fixes(unverified.0));
    }
    if !uncontained.0.is_empty() {
        gate.failures.push(contained(uncontained.0));
    }
    if !unverified.1.is_empty() {
        gate.warnings.push(fixes(unverified.1));
    }
    if !uncontained.1.is_empty() {
        gate.warnings.push(contained(uncontained.1));
    }
    // Containment: coverage and content, then re-verify (A29).
    let mut c2 = Containment::new(entries, at, node_path, env)?;
    gate.range = c2.range();
    for e in verified_in {
        if let Some(failure) = c2.check(e)? {
            if serious(e) {
                gate.failures.push(failure);
            } else {
                gate.warnings.push(failure);
            }
        }
    }
    // The walk the gate records is always checked on content at `at`,
    // findings or none, so a range it passes reads as settled (A31): a
    // clean review whose paths changed after it fails until a round
    // covers the change (Terra's #9504, major 4). Never a residual.
    if let Some((base, head)) = gate.range.clone() {
        let paths = c2.differing(0)?;
        let said = gate.failures.iter().any(|f| {
            f.reason == "not_covered"
                && f.data.get("final").and_then(Value::as_str) == Some(head.as_str())
                && f.data.get("paths") == Some(&json!(paths))
        });
        if !paths.is_empty() && !said {
            gate.failures.push(GateFailure {
                condition: 2,
                reason: "not_covered",
                items: Vec::new(),
                residual: false,
                hint: format!(
                    "{at} doesn't hold what the rounds reviewed: the merge dropped or altered \
                     reviewed changes, or a later change needs a round of its own"
                ),
                data: json!({"entry": Value::Null, "base": base, "final": head, "paths": paths}),
            });
        }
    }
    // A residual issue covers what it names: a question by its refs, a
    // requirement by its `requirement` list.
    let residuals: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.kind == Kind::Issue && e.state == "residual")
        .collect();
    // 3. No blocking question is open.
    let waiting: Vec<String> = input_required(entries)
        .into_iter()
        .filter(|q| !residuals.iter().any(|i| i.refs().contains(&q.path())))
        .map(|q| q.id.clone())
        .collect();
    if !waiting.is_empty() {
        gate.failures.push(GateFailure {
            condition: 3,
            reason: "input_required",
            items: waiting,
            residual: true,
            hint: "answer the blocking questions".into(),
            data: Value::Null,
        });
    }
    // 4. Every requirement is settled.
    let unsettled: Vec<String> = requirements
        .iter()
        .filter(|r| {
            let state = entries
                .iter()
                .find(|e| e.kind == Kind::Req && &e.id == *r)
                .map(|e| e.state.as_str());
            !matches!(state, Some("met" | "waived"))
                && !residuals.iter().any(|i| i.requirement().contains(r))
        })
        .cloned()
        .collect();
    if !unsettled.is_empty() {
        gate.failures.push(GateFailure {
            condition: 4,
            reason: "requirements_open",
            items: unsettled,
            residual: true,
            hint: "a reviewer marks each requirement met, or a coordinator waives it".into(),
            data: Value::Null,
        });
    }
    Ok(gate)
}

/// What a node's settle checks and records (A29 step 4, A31).
#[derive(Debug, Clone, Copy)]
pub struct Settle<'a> {
    pub node: &'a NodeLink,
    /// The `R<n>` ids of the node's `# Requirements`.
    pub requirements: &'a [String],
    /// The HEAD the gate checks, a full SHA.
    pub at: &'a str,
    /// The node file's path from the repository's top (git work); none
    /// for a local node.
    pub node_path: Option<&'a str>,
    /// The digest of the node file's [`node_revision_text`].
    pub revision: &'a str,
    /// The changeset the mark settles (git work); none in local work.
    pub changeset: Option<&'a str>,
}

/// Why a settle wrote nothing.
#[derive(Debug)]
pub enum SettleError {
    /// The gate failed inside the settling transaction: `done` refuses
    /// with `entry.gate_failed`.
    Gate(Gate),
    Entry(EntryError),
}

impl From<EntryError> for SettleError {
    fn from(e: EntryError) -> Self {
        Self::Entry(e)
    }
}

impl From<rusqlite::Error> for SettleError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Entry(e.into())
    }
}

impl std::fmt::Display for SettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gate(g) => write!(f, "the gate fails ({} conditions)", g.failures.len()),
            Self::Entry(e) => e.fmt(f),
        }
    }
}

/// The settle entry point: run inside the settling transaction (the mark's
/// in git work, `complete_local`'s in local work), it evaluates the gate
/// over the entries this transaction reads, conditions 1 to 4 at
/// `settle.at`, and when it passes appends the `ledger.settle` record
/// `{node, upto, mail_upto, at, revision, range, changeset, entries,
/// requirements}` (A29 step 4; A31). When it fails it writes nothing and
/// the caller rolls its transaction back. The g-writers lanes call this,
/// and nothing else stands in for it (the orchestrator's bar, 2026-10-29).
pub fn settle_in(
    tx: &Connection,
    actor: &str,
    settle: &Settle<'_>,
    env: &dyn Env,
) -> Result<Gate, SettleError> {
    crate::store::check_mail_schema(tx).map_err(EntryError::from)?;
    let entries = load_all(tx, &settle.node.uuid)?;
    let top: Option<i64> = tx.query_row(
        "SELECT MAX(seq) FROM entry_events WHERE node_uuid = ?1",
        params![settle.node.uuid],
        |r| r.get(0),
    )?;
    let upto = top.map(ms).unwrap_or(0);
    let gate = evaluate_gate(
        &entries,
        settle.requirements,
        settle.at,
        upto,
        settle.node_path,
        env,
    )?;
    if !gate.ok() {
        return Err(SettleError::Gate(gate));
    }
    let mail: Option<i64> = tx.query_row("SELECT MAX(seq) FROM msg_messages", [], |r| r.get(0))?;
    let listed: Vec<Value> = entries
        .iter()
        .filter(|e| e.kind != Kind::Req)
        .map(|e| {
            json!({
                "id": e.id,
                "kind": e.kind.as_str(),
                "state": e.state,
                "severity": e.severity,
                "sha": e.verified_sha().or_else(|| e.fix_sha()),
                "title_digest": crate::log::text_digest(&e.title),
            })
        })
        .collect();
    let mut ids: Vec<&str> = settle.requirements.iter().map(String::as_str).collect();
    for e in entries.iter().filter(|e| e.kind == Kind::Req) {
        if !ids.contains(&e.id.as_str()) {
            ids.push(&e.id);
        }
    }
    let requirements: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let e = entries.iter().find(|e| e.kind == Kind::Req && e.id == id);
            json!({
                "id": id,
                "state": e.map_or("unverified", |e| e.state.as_str()),
                "sha": e.and_then(|e| e.get_str("at").or_else(|| e.verified_sha())),
            })
        })
        .collect();
    let mut body = Map::new();
    body.insert("node".into(), settle.node.id.clone().into());
    body.insert("upto".into(), upto.into());
    body.insert("mail_upto".into(), json!(mail.map(ms)));
    body.insert("at".into(), settle.at.into());
    body.insert("revision".into(), settle.revision.into());
    body.insert(
        "range".into(),
        gate.range
            .as_ref()
            .map_or(Value::Null, |(b, h)| json!({"base": b, "head": h})),
    );
    body.insert("changeset".into(), json!(settle.changeset));
    body.insert("entries".into(), listed.into());
    body.insert("requirements".into(), requirements.into());
    crate::log::append(
        tx,
        actor,
        "ledger.settle",
        vec![mdkg_log::record::node_ref(&settle.node.id)],
        body,
    )
    .map_err(EntryError::from)?;
    Ok(gate)
}

/// The latest `ledger.settle` record bodies for `node`, newest first.
pub fn settle_records(conn: &Connection, node: &NodeLink) -> Result<Vec<Value>, EntryError> {
    let subject = mdkg_log::record::node_ref(&node.id);
    let mut stmt = conn.prepare(
        "SELECT record FROM log WHERE instr(record, '\"ledger.settle\"') > 0 ORDER BY seq DESC",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let record: Value = serde_json::from_str(&row?)?;
        if record.get("kind").and_then(Value::as_str) != Some("ledger.settle") {
            continue;
        }
        let ours = record
            .get("subject")
            .and_then(Value::as_array)
            .is_some_and(|s| s.iter().any(|v| v.as_str() == Some(subject.as_str())));
        if ours && let Some(body) = record.get("body") {
            out.push(body.clone());
        }
    }
    Ok(out)
}

impl Store {
    fn entry_tx(&mut self) -> Result<Transaction<'_>, EntryError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::store::check_mail_schema(&tx)?;
        Ok(tx)
    }

    /// Applies one op in its own transaction.
    /// Opens the review round a `review_request` bound to `node` opens (A27
    /// rule 8), anchored through [`anchored_in`] in the same transaction;
    /// `None` when the request already opened its round. `graph` marks a
    /// round on a graph range (a design node's text). For the mail send's
    /// call site, and for tests that need a round. The range is resolved
    /// through [`Env::commit`] to full lowercase commit ids, and one that
    /// doesn't resolve is refused (Terra's code review #10220, major 4).
    #[allow(clippy::too_many_arguments)]
    pub fn entry_open_round(
        &mut self,
        node: &NodeLink,
        title: &str,
        range: (&str, &str),
        reviewers: &[String],
        sender: &str,
        message_id: &str,
        now_ms: u64,
        graph: bool,
        env: &dyn Env,
    ) -> Result<Option<Changed>, EntryError> {
        let (base, head) = (full_commit(env, range.0)?, full_commit(env, range.1)?);
        let tx = self.entry_tx()?;
        let opened = open_round_in(
            &tx,
            node,
            title,
            (&base, &head),
            reviewers,
            sender,
            message_id,
            now_ms,
            graph,
            env,
        )?;
        tx.commit()?;
        Ok(opened.map(|r| r.changed))
    }

    pub fn entry_apply(&mut self, c: &Change<'_>, env: &dyn Env) -> Result<Applied, EntryError> {
        let tx = self.entry_tx()?;
        let applied = apply_in(&tx, c, env)?;
        tx.commit()?;
        Ok(applied)
    }

    /// Applies ops in order, in one transaction. A refused op doesn't stop
    /// the others and leaves nothing behind (each runs in a savepoint), as
    /// a ledger block's lines do (A32).
    pub fn entry_apply_each(
        &mut self,
        changes: &[Change<'_>],
        env: &dyn Env,
    ) -> Result<Vec<Result<Applied, EntryError>>, EntryError> {
        let mut tx = self.entry_tx()?;
        let mut out = Vec::new();
        for c in changes {
            let sp = tx.savepoint()?;
            let result = apply_in(&sp, c, env);
            match result {
                Ok(a) => {
                    sp.commit()?;
                    out.push(Ok(a));
                }
                Err(e) => {
                    drop(sp);
                    out.push(Err(e));
                }
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Applies ops in order, all or none: the first refusal refuses the
    /// whole set (`done --residual`, A29's step 2).
    pub fn entry_apply_all(
        &mut self,
        changes: &[Change<'_>],
        env: &dyn Env,
    ) -> Result<Vec<Applied>, EntryError> {
        let tx = self.entry_tx()?;
        let mut out = Vec::new();
        for c in changes {
            out.push(apply_in(&tx, c, env)?);
        }
        tx.commit()?;
        Ok(out)
    }

    /// One entry.
    pub fn entry(&self, node_uuid: &str, id: &str) -> Result<Option<Entry>, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        load(&tx, node_uuid, id)
    }

    /// A node's entries that match `filter`, by kind letter and number.
    pub fn entries(&self, node_uuid: &str, filter: &Filter) -> Result<Vec<Entry>, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        Ok(load_all(&tx, node_uuid)?
            .into_iter()
            .filter(|e| filter.matches(e))
            .collect())
    }

    /// An entry's transitions, oldest first.
    pub fn entry_history(&self, node_uuid: &str, id: &str) -> Result<Vec<Event>, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let mut stmt = tx.prepare(
            "SELECT seq, op, from_state, to_state, actor, at_ms, sha, message_id, turn_id, note,
                    log_seq
             FROM entry_events WHERE node_uuid = ?1 AND entry_id = ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![node_uuid, id], |r| {
            Ok(Event {
                seq: ms(r.get(0)?),
                op: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                by: r.get(4)?,
                at_ms: ms(r.get(5)?),
                sha: r.get(6)?,
                message: r.get(7)?,
                turn: r.get(8)?,
                note: r.get(9)?,
                log_seq: r.get::<_, Option<i64>>(10)?.map(ms),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `S`: the node's highest `entry_events` seq, 0 with none (A29).
    pub fn ledger_upto(&self, node_uuid: &str) -> Result<u64, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let top: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM entry_events WHERE node_uuid = ?1",
            params![node_uuid],
            |r| r.get(0),
        )?;
        Ok(top.map(ms).unwrap_or(0))
    }

    /// The node's `# Ledger` section as `done` writes it into the node
    /// (A31), read in one transaction: every entry, the node's
    /// `requirements`, and `upto`, the ledger's `S` as of this read.
    /// `None` for a node with no ledger (no entry and no requirement),
    /// which `done` leaves as 0.7.0 did.
    pub fn ledger_section(
        &self,
        node: &NodeLink,
        requirements: &[String],
    ) -> Result<Option<String>, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let entries = load_all(&tx, &node.uuid)?;
        if entries.is_empty() && requirements.is_empty() {
            return Ok(None);
        }
        let mut by = std::collections::HashMap::new();
        let mut upto = 0;
        {
            // The last transition into each entry's state: one that
            // changed it, or opened it.
            let mut stmt = tx.prepare(
                "SELECT seq, entry_id, actor, note, from_state, to_state FROM entry_events
                 WHERE node_uuid = ?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![node.uuid], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (seq, id, actor, note, from, to) = row?;
                upto = upto.max(ms(seq));
                if from.as_deref() != Some(to.as_str()) {
                    by.insert(id, LedgerBy { by: actor, note });
                }
            }
        }
        Ok(Some(render_ledger_section(
            &node.id,
            upto,
            &entries,
            &by,
            requirements,
        )))
    }

    /// A node's open entries, counted.
    pub fn entries_open(&self, node_uuid: &str) -> Result<OpenCounts, EntryError> {
        let mut counts = OpenCounts::default();
        for e in self.entries(node_uuid, &Filter::default())? {
            if e.is_terminal() {
                continue;
            }
            match (e.kind, e.state.as_str()) {
                (Kind::Finding, _) => counts.finding += 1,
                (Kind::Question, "open") => counts.question += 1,
                (Kind::Issue, _) => counts.issue += 1,
                (Kind::Req, "unverified" | "unmet") => counts.req += 1,
                _ => {}
            }
        }
        Ok(counts)
    }

    /// The node's `ledger.settle` record bodies, newest first (A31).
    pub fn ledger_settles(&self, node: &NodeLink) -> Result<Vec<Value>, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        settle_records(&tx, node)
    }

    /// Every node's ledger presence and settle records, in one read
    /// transaction: one pass over the entries and one over the log.
    pub fn ledger_reads(&self) -> Result<LedgerReads, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let mut reads = LedgerReads::default();
        {
            let mut stmt = tx.prepare("SELECT DISTINCT lower(node_uuid) FROM entries")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                reads.ledger.insert(row?);
            }
        }
        let mut stmt = tx.prepare(
            "SELECT record FROM log WHERE instr(record, '\"ledger.settle\"') > 0 ORDER BY seq DESC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let record: Value = serde_json::from_str(&row?)?;
            if record.get("kind").and_then(Value::as_str) != Some("ledger.settle") {
                continue;
            }
            let Some(body) = record.get("body") else {
                continue;
            };
            let Some(link) = body
                .get("node")
                .and_then(Value::as_str)
                .and_then(|id| NodeLink::of(id).ok())
            else {
                continue;
            };
            reads
                .settles
                .entry(link.uuid.to_ascii_lowercase())
                .or_default()
                .push(body.clone());
        }
        Ok(reads)
    }

    /// A29's gate, conditions 1 to 4, at full SHA `at`, with `S` read in
    /// the same transaction as the entries.
    pub fn entry_gate(
        &self,
        node_uuid: &str,
        requirements: &[String],
        at: &str,
        node_path: Option<&str>,
        env: &dyn Env,
    ) -> Result<Gate, EntryError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::store::check_mail_schema(&tx)?;
        let entries = load_all(&tx, node_uuid)?;
        let top: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM entry_events WHERE node_uuid = ?1",
            params![node_uuid],
            |r| r.get(0),
        )?;
        evaluate_gate(
            &entries,
            requirements,
            at,
            top.map(ms).unwrap_or(0),
            node_path,
            env,
        )
    }
}

/// A read-only view of the ledger's requirement states, for P4's `req
/// list` and `pack`: it opens an existing store read-only and never
/// creates, upgrades or writes one. A missing, unreadable, linked or
/// other-version store is `coord.store_failed`, which its callers show as
/// an unknown state. The CLI opens it through the repository's checks
/// (`entry::requirement_reader`), so a nested repository git skipped
/// never reads the outer store.
///
/// The stated residual (the orchestrator on Terra's #8445, DECISION 2):
/// SQLite's read-only WAL reader may create or update `coord.sqlite-shm`
/// beside the store, which is neither creating nor upgrading a store; it
/// opens `mode=ro`, never `immutable=1`, which could read a torn state
/// while writers run. Where that sidecar can't be made (a folder this
/// user can't write), the open or the read fails as unavailable.
#[derive(Debug)]
pub struct RequirementStateReader {
    conn: Connection,
}

// P4's interface returns a `Diagnostic` as its error.
#[allow(clippy::result_large_err)]
impl RequirementStateReader {
    /// The store at `path`, read-only: an existing regular file, never
    /// a link itself (`SQLITE_OPEN_NOFOLLOW` as well as the check before
    /// it). SQLite's NOFOLLOW refuses a link in *any* part of the path, so
    /// the folder is resolved first (`/tmp` and `/var` are links on
    /// macOS, as a projects folder may be) and the store opened by its
    /// real path; only the file itself must not be a link.
    pub fn open_at(path: &std::path::Path) -> Result<Self, mdkg_format::Diagnostic> {
        use rusqlite::OpenFlags;
        let name = path
            .file_name()
            .ok_or_else(|| unavailable(&"the store path names no file"))?;
        let folder = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        };
        let path = std::fs::canonicalize(folder)
            .map_err(|e| unavailable(&e))?
            .join(name);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| unavailable(&e))?;
        if !meta.is_file() {
            return Err(unavailable(&"the store isn't a regular file"));
        }
        if !crate::store::store_file_present(&path).map_err(|e| unavailable(&e))? {
            return Err(unavailable(&"the coordinator store is absent"));
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| unavailable(&e))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| unavailable(&e))?;
        let reader = Self { conn };
        reader.read(|_| Ok(()))?;
        Ok(reader)
    }

    /// `f` inside one read transaction that first checks the store's
    /// versions, so a store another process upgraded since the open is
    /// reported, never read (Terra's code review #8445).
    fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, mdkg_format::Diagnostic> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| unavailable(&e))?;
        crate::store::check_mail_schema(&tx).map_err(|e| unavailable(&e))?;
        let out = f(&tx).map_err(|e| unavailable(&e))?;
        tx.finish().map_err(|e| unavailable(&e))?;
        Ok(out)
    }

    /// Each requirement entry's state on the node `node_id` (its canonical
    /// id) in the graph whose node folder is `graph_folder` (as lease keys
    /// spell it, `<folder>#<id>`: `Env::node_folder`), by `R<n>`:
    /// `unverified`, `met`, `unmet` or `waived`. A requirement with no
    /// entry yet, or one recorded without its graph, isn't listed: a
    /// requirement changed while its node wasn't on the graph ref reads as
    /// unrecorded until it changes again once it is (Opus's #8780). An id
    /// that isn't a canonical node id is `node.bad_id`.
    pub fn states(
        &self,
        graph_folder: &str,
        node_id: &str,
    ) -> Result<std::collections::HashMap<String, String>, mdkg_format::Diagnostic> {
        let node = NodeLink::of(node_id).map_err(|_| {
            mdkg_format::Diagnostic::error(
                mdkg_format::Code::NodeBadId,
                "requirement states are read by a node's canonical id",
            )
        })?;
        let folder = graph_folder.to_lowercase();
        self.read(|tx| {
            let mut stmt = tx.prepare(
                "SELECT entry_id, state FROM entries
                 WHERE node_uuid = ?1 AND kind = 'req'
                   AND json_extract(data, '$.graph_folder') = ?2",
            )?;
            let rows = stmt.query_map(params![node.uuid, folder], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect()
        })
    }
}

fn unavailable(e: &dyn std::fmt::Display) -> mdkg_format::Diagnostic {
    mdkg_format::Diagnostic::error(
        mdkg_format::Code::CoordStoreFailed,
        format!("the coordinator store is unavailable, so requirement states are unknown: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Upgrade;
    use std::collections::BTreeMap;

    const NODE_A: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e3";
    const NODE_B: &str = "bug-01a1878e-3e6f-710b-9d01-68d002aed3e4";

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("mdkg-entries-{name}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn store(&self) -> Store {
            Store::open_with(&self.0.join("coord.sqlite"), 1_000, Upgrade::Allow).expect("store")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A line of history, c1 ← c2 ← c3, plus a side commit s1 off c1, and
    /// `role:review` held by `rev2`.
    struct Fake {
        parents: BTreeMap<&'static str, Option<&'static str>>,
        /// Commit -> the paths it changed.
        paths: BTreeMap<&'static str, Vec<&'static str>>,
        /// (commit, path) -> its tree entry; absent when missing.
        trees: BTreeMap<(&'static str, &'static str), &'static str>,
        /// The main ref's tips, `None` when it doesn't resolve.
        main: Option<Vec<&'static str>>,
        folder: NodeFolder,
        /// Calls of `latest_heads` and of `contains`.
        walks: std::cell::Cell<usize>,
        checks: std::cell::Cell<usize>,
    }

    fn sha(short: &str) -> String {
        format!("{short}{}", "0".repeat(40 - short.len()))
    }

    impl Fake {
        fn new() -> Self {
            let mut parents = BTreeMap::new();
            parents.insert("c1", None);
            parents.insert("c2", Some("c1"));
            parents.insert("c3", Some("c2"));
            parents.insert("e1", Some("c1"));
            Fake {
                parents,
                paths: BTreeMap::new(),
                trees: BTreeMap::new(),
                main: None,
                folder: NodeFolder::Any,
                walks: Default::default(),
                checks: Default::default(),
            }
        }
    }

    impl Env for Fake {
        fn node(&self, handle: &str) -> Result<NodeLink, EntryError> {
            NodeLink::of(handle).map_err(|_| EntryError::NodeNotFound(handle.into()))
        }
        fn commit(&self, s: &str) -> Result<String, EntryError> {
            let hits: Vec<&&str> = self
                .parents
                .keys()
                .filter(|k| sha(k).starts_with(s) && s.len() >= 2)
                .collect();
            match hits.as_slice() {
                [one] => Ok(sha(one)),
                _ => Err(EntryError::UnknownCommit(s.into())),
            }
        }
        fn contains(&self, ancestor: &str, head: &str) -> Result<bool, EntryError> {
            self.checks.set(self.checks.get() + 1);
            let mut at = self.parents.keys().find(|k| sha(k) == head).copied();
            while let Some(k) = at {
                if sha(k) == ancestor {
                    return Ok(true);
                }
                at = self.parents.get(k).copied().flatten();
            }
            Ok(false)
        }
        fn holds(&self, actor: &str, role: &str) -> bool {
            actor == "rev2" && role == "role:review"
        }
        fn main_tips(&self) -> Result<Option<Vec<String>>, EntryError> {
            Ok(self
                .main
                .as_ref()
                .map(|t| t.iter().map(|k| sha(k)).collect()))
        }
        fn commit_paths(&self, c: &str) -> Result<(BTreeSet<String>, Option<String>), EntryError> {
            let key = self.parents.keys().find(|k| sha(k) == c).copied();
            let paths = key
                .and_then(|k| self.paths.get(k))
                .map(|p| p.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            let parent = key
                .and_then(|k| self.parents.get(k).copied().flatten())
                .map(sha);
            Ok((paths, parent))
        }
        /// Each commit's own paths, from `head` back along first parents
        /// to `base`.
        fn changed_paths(&self, base: &str, head: &str) -> Result<BTreeSet<String>, EntryError> {
            let mut out = BTreeSet::new();
            let mut at = self.parents.keys().find(|k| sha(k) == head).copied();
            while let Some(k) = at {
                if sha(k) == base {
                    break;
                }
                out.extend(
                    self.paths
                        .get(k)
                        .into_iter()
                        .flatten()
                        .map(|p| p.to_string()),
                );
                at = self.parents.get(k).copied().flatten();
            }
            Ok(out)
        }
        /// Pairwise over its own map, with no env calls, each call
        /// counted.
        fn latest_heads(&self, heads: &[String], at: &str) -> Result<Vec<String>, EntryError> {
            self.walks.set(self.walks.get() + 1);
            let history = |head: &str| -> Vec<String> {
                let mut out = Vec::new();
                let mut k = self.parents.keys().find(|k| sha(k) == head).copied();
                while let Some(c) = k {
                    out.push(sha(c));
                    k = self.parents.get(c).copied().flatten();
                }
                out
            };
            let held: Vec<&String> = {
                let mine = history(at);
                heads.iter().filter(|h| mine.contains(h)).collect()
            };
            Ok(held
                .iter()
                .filter(|h| !held.iter().any(|o| o != *h && history(o).contains(h)))
                .map(|h| (*h).clone())
                .collect())
        }
        fn tree_entry(&self, at: &str, path: &str) -> Result<Option<String>, EntryError> {
            Ok(self
                .trees
                .iter()
                .find(|((c, p), _)| sha(c) == at && *p == path)
                .map(|(_, e)| e.to_string()))
        }
        fn node_folder(&self, _node: &NodeLink) -> Result<NodeFolder, EntryError> {
            Ok(self.folder.clone())
        }
    }

    fn op(json: Value) -> Op {
        match json {
            Value::Object(m) => Op::from_object(&m).expect("op"),
            _ => panic!("not an object"),
        }
    }

    fn node(id: &str) -> NodeLink {
        NodeLink::of(id).expect("node")
    }

    /// Applies `json` on NODE_A as `actor` of `class` at `now`.
    fn run(s: &mut Store, actor: &str, class: Class, json: Value) -> Result<Applied, EntryError> {
        let a = node(NODE_A);
        let o = op(json);
        s.entry_apply(
            &Change {
                op: &o,
                actor,
                class,
                node: Some(&a),
                message: None,
                turn: Some("turn-1"),
                now_ms: 5,
            },
            &Fake::new(),
        )
    }

    fn state(s: &Store, id: &str) -> String {
        s.entry(&node(NODE_A).uuid, id)
            .expect("read")
            .expect("entry")
            .state
    }

    fn code(r: Result<Applied, EntryError>) -> &'static str {
        r.expect_err("refused").code().expect("a code")
    }

    use Class::{Coordinator, Other, Person, Reviewer, Worker};

    /// Opens round V<n> on NODE_A over `base..head`, anchored as a first
    /// round, for a verify to lie in.
    fn round(s: &mut Store, base: &str, head: &str, request: &str) {
        round_with(s, base, head, request, Some("first"));
    }

    /// A round on NODE_A recorded with `anchored`.
    fn round_with(s: &mut Store, base: &str, head: &str, request: &str, anchored: Option<&str>) {
        let tx = s.conn.transaction().expect("tx");
        open_review_from_mail_in(
            &tx,
            &node(NODE_A),
            "review",
            (&sha(base), &sha(head)),
            &["rev".into()],
            "orchestrator",
            request,
            1,
            anchored,
        )
        .expect("round");
        tx.commit().expect("commit");
    }

    /// [`Fake`], plus committed definitions and running turns, as the
    /// CLI's environment reads them inside the op's transaction.
    struct Classed {
        fake: Fake,
        agents: Vec<AgentFacts>,
        turning: Vec<&'static str>,
        unclassed: bool,
        /// The graph's node folder, as lease keys spell it.
        folder: NodeFolder,
    }

    fn facts(name: &str, role: &str, reports_to: &str) -> AgentFacts {
        AgentFacts {
            name: name.into(),
            role: Some(role.into()),
            reports_to: Some(reports_to.into()),
        }
    }

    impl Classed {
        /// `wrk` (worker) and `rev` (reviewer) report to `orch`, a worker
        /// agent itself; `lone` (worker) reports to `boss`.
        fn new() -> Self {
            Classed {
                fake: Fake::new(),
                agents: vec![
                    facts("wrk", "worker", "orch"),
                    facts("rev", "reviewer", "orch"),
                    facts("orch", "worker", "human:nick"),
                    facts("lone", "worker", "boss"),
                ],
                turning: Vec::new(),
                unclassed: false,
                folder: NodeFolder::Any,
            }
        }
    }

    impl Env for Classed {
        fn node(&self, handle: &str) -> Result<NodeLink, EntryError> {
            self.fake.node(handle)
        }
        fn commit(&self, s: &str) -> Result<String, EntryError> {
            self.fake.commit(s)
        }
        fn contains(&self, ancestor: &str, head: &str) -> Result<bool, EntryError> {
            self.fake.contains(ancestor, head)
        }
        fn holds(&self, actor: &str, role: &str) -> bool {
            self.fake.holds(actor, role)
        }
        fn definitions(&self, actor: &str) -> Result<Option<(Class, Vec<String>)>, EntryError> {
            if self.unclassed {
                return Err(EntryError::Unclassed(
                    "the graph ref doesn't resolve".into(),
                ));
            }
            if is_person(actor) {
                return Ok(Some((Person, Vec::new())));
            }
            Ok(Some((
                Class::of(actor, &self.agents),
                Class::subordinates(actor, &self.agents),
            )))
        }
        fn in_turn(&self, actor: &str) -> Result<bool, EntryError> {
            Ok(self.turning.contains(&actor))
        }
        fn main_tips(&self) -> Result<Option<Vec<String>>, EntryError> {
            self.fake.main_tips()
        }
        fn changed_paths(&self, base: &str, head: &str) -> Result<BTreeSet<String>, EntryError> {
            self.fake.changed_paths(base, head)
        }
        fn latest_heads(&self, heads: &[String], at: &str) -> Result<Vec<String>, EntryError> {
            self.fake.latest_heads(heads, at)
        }
        fn commit_paths(&self, c: &str) -> Result<(BTreeSet<String>, Option<String>), EntryError> {
            self.fake.commit_paths(c)
        }
        fn tree_entry(&self, at: &str, path: &str) -> Result<Option<String>, EntryError> {
            self.fake.tree_entry(at, path)
        }
        fn node_folder(&self, _node: &NodeLink) -> Result<NodeFolder, EntryError> {
            Ok(self.folder.clone())
        }
    }

    /// Applies `json` on `on` as `actor`, classed by `env` alone (the
    /// change's own class is a placeholder), from a ledger block when
    /// `turn` is set and through the CLI otherwise.
    fn run_classed(
        s: &mut Store,
        env: &Classed,
        on: &str,
        actor: &str,
        turn: Option<&str>,
        json: Value,
    ) -> Result<Applied, EntryError> {
        let n = node(on);
        let o = op(json);
        s.entry_apply(
            &Change {
                op: &o,
                actor,
                class: Other,
                node: Some(&n),
                message: None,
                turn,
                now_ms: 5,
            },
            env,
        )
    }

    fn lease_node(s: &mut Store, owner: &str, on: &str, file: &str) -> mdkg_coord::LeaseId {
        match s
            .execute(&mdkg_coord::Command::Claim {
                owner: owner.into(),
                scope: mdkg_coord::Scope::new(file),
                mode: mdkg_coord::Mode::Exclusive,
                now: 0,
                request: None,
                node: Some(on.into()),
                branch: None,
            })
            .expect("claim")
        {
            mdkg_coord::Outcome::Lease { lease } => lease,
            other => panic!("claim: {other:?}"),
        }
    }

    fn finding(title: &str) -> Value {
        json!({"op": "add", "kind": "finding", "title": title})
    }

    /// Applies `json` on NODE_A as `actor` of `class` against `env`.
    fn run_env(
        s: &mut Store,
        env: &dyn Env,
        actor: &str,
        class: Class,
        json: Value,
    ) -> Result<Applied, EntryError> {
        let a = node(NODE_A);
        let o = op(json);
        s.entry_apply(
            &Change {
                op: &o,
                actor,
                class,
                node: Some(&a),
                message: None,
                turn: None,
                now_ms: 5,
            },
            env,
        )
    }

    #[test]
    fn a_verify_needs_a_round_another_actor_and_the_fix_in_place() {
        let dir = Scratch::new("verify-rules");
        let mut s = dir.store();
        let mut env = Fake::new();
        // c2 changes src/a.rs; c3 puts it back as c1 had it (a revert).
        env.paths.insert("c2", vec!["src/a.rs"]);
        env.trees
            .insert(("c1", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        env.trees
            .insert(("c3", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        // rev2 is a reviewer here, and records the fix as a person might.
        run_env(
            &mut s,
            &env,
            "fixer",
            Person,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        // No round yet: not covered, a finding added outside a review too.
        let verify = |at: &str| json!({"op": "verify", "entry": "F1", "at": at});
        assert_eq!(
            code(run_env(&mut s, &env, "rev", Reviewer, verify("c2"))),
            "entry.not_covered"
        );
        round(&mut s, "c1", "c3", "m1");
        // The fixer never verifies, whatever its class (R53).
        let e = run_env(&mut s, &env, "fixer", Person, verify("c2")).expect_err("fixer");
        assert_eq!(e.code(), Some("entry.not_allowed"));
        // At c3 the fix is undone: c2's every path reads as c1 had it.
        let e = run_env(&mut s, &env, "rev", Reviewer, verify("c3")).expect_err("absent");
        assert_eq!(e.code(), Some("entry.fix_absent"));
        assert_eq!(e.data()["commit"], sha("c2"));
        // At c2 it's in place, and the verify records its commit set and paths.
        run_env(&mut s, &env, "rev", Reviewer, verify("c2")).expect("verify");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.data["verified"]["fix_commits"], json!([sha("c2")]));
        assert_eq!(f.data["verified"]["fix_paths"], json!(["src/a.rs"]));
        assert_eq!(f.data["verified"]["review"], "V1");
        // A verified finding takes a re-verify, recorded as `reverify`.
        run_env(&mut s, &env, "rev2", Reviewer, verify("c2")).expect("reverify");
        let ops: Vec<String> = s
            .entry_history(&node(NODE_A).uuid, "F1")
            .expect("history")
            .into_iter()
            .map(|h| h.op)
            .collect();
        assert_eq!(ops.last().map(String::as_str), Some("reverify"));
        assert_eq!(state(&s, "F1"), "verified");
    }

    #[test]
    fn every_declared_fix_commit_counts_across_reopens() {
        let dir = Scratch::new("fix-commits");
        let mut s = dir.store();
        let env = Fake::new();
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2", "commits": ["e1"]}),
        )
        .expect("fix");
        round(&mut s, "c1", "c3", "m1");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "reopen", "entry": "F1", "at": "c3"}),
        )
        .expect("reopen");
        run_env(
            &mut s,
            &env,
            "w2",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        )
        .expect("fix again");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(
            f.data["fix_commits"],
            json!([sha("c2"), sha("e1"), sha("c3")])
        );
        // Both fixers are fixers: neither verifies.
        for who in ["w", "w2"] {
            let r = run_env(
                &mut s,
                &env,
                who,
                Person,
                json!({"op": "verify", "entry": "F1", "at": "c3"}),
            );
            assert_eq!(code(r), "entry.not_allowed", "{who}");
        }
    }

    #[test]
    fn walks_start_anchored_and_chain_head_to_base() {
        let r = |id: &str, n: u64, base: &str, head: &str, anchored: bool| Round {
            id: id.into(),
            number: n,
            base: sha(base),
            head: sha(head),
            closed: true,
            anchored: anchored.then(|| "main".to_string()),
        };
        // V1 c1..c2 anchored, V2 c2..c3, V3 c1..c2 a duplicate, V4 e1..c3 unanchored.
        let rounds = vec![
            r("V1", 1, "c1", "c2", true),
            r("V2", 2, "c2", "c3", false),
            r("V3", 3, "c1", "c2", true),
            r("V4", 4, "e1", "c3", false),
        ];
        let walks = walks_to(&rounds, &rounds[1]).expect("walks");
        let ids: Vec<Vec<&str>> = walks
            .iter()
            .map(|w| w.iter().map(|r| r.id.as_str()).collect())
            .collect();
        assert_eq!(ids, vec![vec!["V1", "V2"]], "the duplicate counts once");
        assert!(
            walks_to(&rounds, &rounds[3]).expect("walks").is_empty(),
            "an unanchored lone round starts no walk"
        );
    }

    /// The orchestrator on Terra's #6548, call 5: predecessors are closed
    /// before ranges are deduplicated, so an open duplicate never hides a
    /// closed one.
    #[test]
    fn an_open_duplicate_never_hides_a_closed_predecessor() {
        let r = |id: &str, n: u64, base: &str, head: &str, closed: bool| Round {
            id: id.into(),
            number: n,
            base: sha(base),
            head: sha(head),
            closed,
            anchored: Some("first".into()),
        };
        // V1 c1..c2 expired open, V2 its replacement closed, V3 from c2.
        let rounds = vec![
            r("V1", 1, "c1", "c2", false),
            r("V2", 2, "c1", "c2", true),
            r("V3", 3, "c2", "c3", false),
        ];
        let walks = walks_to(&rounds, &rounds[2]).expect("walks");
        let ids: Vec<Vec<&str>> = walks
            .iter()
            .map(|w| w.iter().map(|r| r.id.as_str()).collect())
            .collect();
        assert_eq!(ids[0], vec!["V2", "V3"]);
    }

    /// The orchestrator on Terra's #7598, finding 5: an unanchored
    /// duplicate never hides an anchored round with the same range, as a
    /// predecessor or as the walk's own last round.
    #[test]
    fn an_unanchored_duplicate_never_hides_an_anchored_round() {
        let r = |id: &str, n: u64, base: &str, head: &str, anchored: Option<&str>| Round {
            id: id.into(),
            number: n,
            base: sha(base),
            head: sha(head),
            closed: true,
            anchored: anchored.map(str::to_string),
        };
        // V1 c1..c2 closed before main held c1; V2, the same range, after
        // main was fetched; V3 c2..c3; V4, V2 again, unanchored.
        let rounds = vec![
            r("V1", 1, "c1", "c2", None),
            r("V2", 2, "c1", "c2", Some("main")),
            r("V3", 3, "c2", "c3", None),
            r("V4", 4, "c1", "c2", None),
        ];
        let ids = |last: &Round| -> Vec<Vec<String>> {
            walks_to(&rounds, last)
                .expect("walks")
                .iter()
                .map(|w| w.iter().map(|r| r.id.clone()).collect())
                .collect()
        };
        assert_eq!(ids(&rounds[2]), vec![vec!["V2", "V3"]]);
        assert_eq!(ids(&rounds[0]), vec![vec!["V2"]]);
        assert_eq!(ids(&rounds[3]), vec![vec!["V2"]]);
    }

    /// Terra's code review #6548: branching and rejoining rounds refuse
    /// past a bound rather than enumerate every walk.
    #[test]
    fn walks_past_the_bound_refuse() {
        let mut rounds = Vec::new();
        let c = |i: usize, side: &str| format!("{side}{i:038}");
        for i in 0..20 {
            for (n, side) in ["m", "n"].iter().enumerate() {
                let number = (i * 4 + n * 2) as u64;
                rounds.push(Round {
                    id: format!("V{number}"),
                    number,
                    base: c(i, "k"),
                    head: c(i, side),
                    closed: true,
                    anchored: (i == 0).then(|| "first".to_string()),
                });
                rounds.push(Round {
                    id: format!("V{}", number + 1),
                    number: number + 1,
                    base: c(i, side),
                    head: c(i + 1, "k"),
                    closed: true,
                    anchored: None,
                });
            }
        }
        let last = rounds.last().expect("last").clone();
        let e = walks_to(&rounds, &last).expect_err("bounded");
        assert_eq!(e.code(), Some("entry.not_covered"));
    }

    /// The orchestrator on Terra's #6548, call 1: a graph range never
    /// anchors on main, even when main holds its base.
    #[test]
    fn a_graph_range_never_anchors_on_main() {
        let dir = Scratch::new("graph-anchor");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.main = Some(vec!["c3"]);
        let a = node(NODE_A);
        let at = |s: &mut Store, base: &str| {
            let tx = s.conn.transaction().expect("tx");
            anchored_in(&tx, &env, &a, &sha(base), true).expect("anchored")
        };
        assert_eq!(at(&mut s, "c2"), Some("first"));
        round(&mut s, "c2", "c3", "m1");
        // c1 is on main, but this graph node's work base is c2.
        assert_eq!(at(&mut s, "c1"), None);
        assert_eq!(at(&mut s, "c2"), Some("first"));
    }

    /// The orchestrator on Terra's #6548, call 4: verify needs an anchored
    /// walk; it never falls back to a round's own base, and picks a later
    /// round with a walk over an earlier one without.
    #[test]
    fn a_verify_needs_an_anchored_walk() {
        let dir = Scratch::new("verify-walk");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.paths.insert("c3", vec!["src/a.rs"]);
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        env.trees
            .insert(("c3", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        )
        .expect("fix");
        let verify = json!({"op": "verify", "entry": "F1", "at": "c3"});
        // V1 c2..c3 holds the fix but starts no walk.
        round_with(&mut s, "c2", "c3", "m1", None);
        assert_eq!(
            code(run_env(&mut s, &env, "rev", Reviewer, verify.clone())),
            "entry.not_covered"
        );
        // V2 c1..c3 is anchored: the verify takes it, not V1.
        round(&mut s, "c1", "c3", "m2");
        run_env(&mut s, &env, "rev", Reviewer, verify).expect("verify");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.data["verified"]["review"], "V2");
    }

    /// The orchestrator on Terra's #6548, call 7: on a reopened finding a
    /// fixer may replace its declared commits (`supersede`); they drop out
    /// of the checks.
    #[test]
    fn a_fixer_supersedes_its_declared_commits_on_a_reopened_finding() {
        let dir = Scratch::new("supersede");
        let mut s = dir.store();
        let mut env = Fake::new();
        // c2's fix is reverted by c3; the new fix is e1, elsewhere.
        env.paths.insert("c2", vec!["src/a.rs"]);
        env.paths.insert("e1", vec!["src/b.rs"]);
        env.trees
            .insert(("c1", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        env.trees
            .insert(("c3", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        env.trees
            .insert(("e1", "src/b.rs"), "100644 blob ccc\tsrc/b.rs");
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        let fix = |sha: &str, supersede: bool| json!({"op": "fix", "entry": "F1", "sha": sha, "supersede": supersede});
        // Not on an open finding.
        assert_eq!(
            code(run_env(&mut s, &env, "w", Worker, fix("c2", true))),
            "entry.bad_transition"
        );
        run_env(&mut s, &env, "w", Worker, fix("c2", false)).expect("fix");
        round(&mut s, "c1", "c3", "m1");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "reopen", "entry": "F1", "at": "c3"}),
        )
        .expect("reopen");
        // Only a fixer supersedes.
        assert_eq!(
            code(run_env(&mut s, &env, "w2", Worker, fix("e1", true))),
            "entry.not_allowed"
        );
        run_env(&mut s, &env, "w", Worker, fix("e1", true)).expect("supersede");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.data["fix_commits"], json!([sha("e1")]));
        assert_eq!(f.data["superseded_commits"], json!([sha("c2")]));
        let ops: Vec<String> = s
            .entry_history(&node(NODE_A).uuid, "F1")
            .expect("history")
            .into_iter()
            .map(|h| h.op)
            .collect();
        assert_eq!(ops.last().map(String::as_str), Some("supersede"));
        // c2, reverted at c3, no longer refuses the verify. e1 is a round
        // of its own: c1..e1.
        round(&mut s, "c1", "e1", "m2");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "e1"}),
        )
        .expect("verify");
    }

    /// Terra's code review #6548: a finding fixed before `fixers` and
    /// `fix_commits` were recorded carries them forward; its fixer never
    /// verifies.
    #[test]
    fn a_legacy_fix_carries_its_fixer_and_commit_forward() {
        let dir = Scratch::new("legacy-fix");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.paths.insert("c2", vec!["src/a.rs"]);
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        s.conn
            .execute(
                "UPDATE entries SET data = json_remove(data, '$.fixers', '$.fix_commits')",
                [],
            )
            .expect("legacy");
        round(&mut s, "c1", "c3", "m1");
        let verify = |at: &str| json!({"op": "verify", "entry": "F1", "at": at});
        assert_eq!(
            code(run_env(&mut s, &env, "w", Person, verify("c2"))),
            "entry.not_allowed"
        );
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "reopen", "entry": "F1", "at": "c3"}),
        )
        .expect("reopen");
        run_env(
            &mut s,
            &env,
            "w2",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        )
        .expect("fix again");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.data["fix_commits"], json!([sha("c2"), sha("c3")]));
        assert_eq!(f.data["fixers"], json!(["w", "w2"]));
    }

    /// Terra's code review #7598, finding 4: a legacy finding fixed twice,
    /// by two actors, carries both fixers and every commit forward from
    /// its history, not only its last fix.
    #[test]
    fn a_legacy_finding_carries_every_earlier_fix_forward() {
        let dir = Scratch::new("legacy-fixes");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.paths.insert("c2", vec!["src/a.rs"]);
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "alice",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2", "commits": ["c1"]}),
        )
        .expect("alice's fix");
        round(&mut s, "c1", "c3", "m1");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "reopen", "entry": "F1", "at": "c3"}),
        )
        .expect("reopen");
        run_env(
            &mut s,
            &env,
            "bob",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        )
        .expect("bob's fix");
        s.conn
            .execute(
                "UPDATE entries SET data = json_remove(data, '$.fixers', '$.fix_commits')",
                [],
            )
            .expect("legacy");
        let verify = json!({"op": "verify", "entry": "F1", "at": "c3"});
        for fixer in ["alice", "bob"] {
            assert_eq!(
                code(run_env(&mut s, &env, fixer, Person, verify.clone())),
                "entry.not_allowed",
                "{fixer}"
            );
        }
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        let (declared, fixers) = fix_record(&s.conn, &f).expect("record");
        assert_eq!(declared, vec![sha("c2"), sha("c1"), sha("c3")]);
        assert_eq!(fixers, vec!["alice", "bob"]);
        // A partly migrated record (an earlier build filled the fields
        // from the last fix only) is reconciled with its history too
        // (Terra's #8466).
        s.conn
            .execute(
                &format!(
                    "UPDATE entries SET data = json_set(data, '$.fixers', json('[\"bob\"]'),
                     '$.fix_commits', json('[\"{}\"]'))",
                    sha("c3")
                ),
                [],
            )
            .expect("partial");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        let (declared, fixers) = fix_record(&s.conn, &f).expect("record");
        assert_eq!(declared, vec![sha("c3"), sha("c2"), sha("c1")]);
        assert_eq!(fixers, vec!["bob", "alice"]);
        assert_eq!(
            code(run_env(&mut s, &env, "alice", Person, verify.clone())),
            "entry.not_allowed"
        );
    }

    /// Terra's code review #6548: a repeat with no severity is the
    /// default, `major`, and raises a minor finding.
    #[test]
    fn a_repeat_without_a_severity_raises_a_minor_to_major() {
        let dir = Scratch::new("second-default");
        let mut s = dir.store();
        let env = Fake::new();
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "nit", "severity": "minor"}),
        )
        .expect("add");
        run_env(&mut s, &env, "rev2", Reviewer, finding("nit")).expect("second");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.severity.as_deref(), Some("major"));
    }

    #[test]
    fn a_round_anchors_on_main_when_it_resolves_and_else_on_the_first_round() {
        let dir = Scratch::new("anchored");
        let mut s = dir.store();
        let mut env = Fake::new();
        let a = node(NODE_A);
        let at = |s: &mut Store, env: &Fake, base: &str, graph: bool| {
            let tx = s.conn.transaction().expect("tx");
            anchored_in(&tx, env, &a, &sha(base), graph).expect("anchored")
        };
        // Without a main ref: the node's first round anchors at its base.
        assert_eq!(at(&mut s, &env, "c2", false), Some("first"));
        round(&mut s, "c2", "c3", "m1");
        // That round never closes (its request may expire): its base stays
        // the work base, so a replacement round from it anchors too, and a
        // round from another base never re-anchors.
        assert_eq!(at(&mut s, &env, "c2", false), Some("first"));
        assert_eq!(at(&mut s, &env, "c1", false), None);
        assert_eq!(at(&mut s, &env, "c3", false), None);
        // With a main ref, only a base it holds anchors for a code round.
        env.main = Some(vec!["c2"]);
        assert_eq!(at(&mut s, &env, "c1", false), Some("main"));
        assert_eq!(at(&mut s, &env, "e1", false), None);
        let dir2 = Scratch::new("anchored-fresh");
        let mut fresh = dir2.store();
        assert_eq!(
            at(&mut fresh, &env, "e1", false),
            None,
            "a fresh code node with main gets no first"
        );
        // A graph range's first round anchors even when main resolves.
        assert_eq!(at(&mut fresh, &env, "e1", true), Some("first"));
    }

    #[test]
    fn a_coordinator_is_judged_per_node_from_the_leases_the_op_reads() {
        let dir = Scratch::new("coordinator");
        let mut s = dir.store();
        let env = Classed::new();
        // No agent ever leased NODE_A: no worker, so no coordinator, and
        // orch is the worker its own definition says.
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("unleased"));
        assert_eq!(code(r), "entry.not_allowed");
        // Once wrk, which reports to orch, holds the node, orch coordinates.
        let lease = lease_node(&mut s, "wrk", NODE_A, ".mdkg/work/a.md");
        run_classed(&mut s, &env, NODE_A, "orch", None, finding("leased")).expect("coordinator");
        // It stays wrk's node after the lease ends: its most recent worker.
        s.execute(&mdkg_coord::Command::Release {
            owner: "wrk".into(),
            lease,
            now: 1,
        })
        .expect("release");
        run_classed(&mut s, &env, NODE_A, "orch", None, finding("released")).expect("still");
        // A node whose worker reports to someone else gives orch nothing.
        lease_node(&mut s, "lone", NODE_B, ".mdkg/work/b.md");
        let r = run_classed(&mut s, &env, NODE_B, "orch", None, finding("not mine"));
        assert_eq!(code(r), "entry.not_allowed");
        // Nor does its own work: orch holding NODE_A makes it the worker.
        lease_node(&mut s, "orch", NODE_A, ".mdkg/work/a.md");
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("own work"));
        assert_eq!(code(r), "entry.not_allowed");
        // A reviewer stays a reviewer wherever it is: its own definition.
        run_classed(&mut s, &env, NODE_B, "rev", None, finding("by rev")).expect("reviewer");
    }

    /// Terra's code review #6278, finding 2: a lease on the same node id in
    /// another graph sharing the coordinator makes no one a coordinator.
    #[test]
    fn a_lease_in_another_graph_grants_no_coordinator() {
        let dir = Scratch::new("coordinator-graphs");
        let mut s = dir.store();
        let mut env = Classed::new();
        env.folder = NodeFolder::In(".mdkg".into());
        lease_node(
            &mut s,
            "wrk",
            &format!("b/.mdkg#{NODE_A}"),
            "b/.mdkg/work/a.md",
        );
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("other graph"));
        assert_eq!(code(r), "entry.not_allowed");
        lease_node(&mut s, "wrk", &format!(".mdkg#{NODE_A}"), ".mdkg/work/a.md");
        run_classed(&mut s, &env, NODE_A, "orch", None, finding("this graph"))
            .expect("coordinator");
        // A node the graph ref doesn't hold has no worker by any lease
        // (the orchestrator on Terra's #6919, call 2).
        env.folder = NodeFolder::Nowhere;
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("off the ref"));
        assert_eq!(code(r), "entry.not_allowed");
    }

    /// Terra's code review #10490: a lease of the actor's own on the node,
    /// keyed in another folder than the one the gate reads (a checkout
    /// that moved the file, a workspace list changed since the claim),
    /// never leaves it a coordinator on its own work, live or as the most
    /// recent holder; a later lease by its subordinate makes it one again.
    #[test]
    fn its_own_lease_keyed_elsewhere_still_makes_the_node_its_own_work() {
        let dir = Scratch::new("coordinator-split");
        let mut s = dir.store();
        let mut env = Classed::new();
        env.folder = NodeFolder::In(".mdkg".into());
        let old = lease_node(&mut s, "wrk", &format!(".mdkg#{NODE_A}"), ".mdkg/work/a.md");
        s.execute(&mdkg_coord::Command::Release {
            owner: "wrk".into(),
            lease: old,
            now: 1,
        })
        .expect("release");
        let own = lease_node(
            &mut s,
            "orch",
            &format!(".mdkg/work/nested#{NODE_A}"),
            ".mdkg/work/nested/a.md",
        );
        let r = run_classed(
            &mut s,
            &env,
            NODE_A,
            "orch",
            None,
            finding("held elsewhere"),
        );
        assert_eq!(code(r), "entry.not_allowed");
        s.execute(&mdkg_coord::Command::Release {
            owner: "orch".into(),
            lease: own,
            now: 1,
        })
        .expect("release");
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("most recent"));
        assert_eq!(code(r), "entry.not_allowed");
        lease_node(&mut s, "wrk", &format!(".mdkg#{NODE_A}"), ".mdkg/work/a.md");
        run_classed(&mut s, &env, NODE_A, "orch", None, finding("wrk's again"))
            .expect("coordinator");
    }

    #[test]
    fn inside_a_turn_a_coordinator_acts_only_through_its_ledger_block() {
        let dir = Scratch::new("coord-turn");
        let mut s = dir.store();
        let mut env = Classed::new();
        lease_node(&mut s, "wrk", NODE_A, ".mdkg/work/a.md");
        env.turning.push("orch");
        let r = run_classed(&mut s, &env, NODE_A, "orch", None, finding("from the CLI"));
        assert_eq!(code(r), "entry.not_allowed");
        run_classed(
            &mut s,
            &env,
            NODE_A,
            "orch",
            Some("turn-9"),
            finding("from a block"),
        )
        .expect("a ledger block coordinates");
    }

    #[test]
    fn a_person_op_is_refused_inside_a_turn_and_accepted_outside() {
        let dir = Scratch::new("person-turn");
        let mut s = dir.store();
        let mut env = Classed::new();
        run_classed(&mut s, &env, NODE_A, "human:nick", None, finding("outside"))
            .expect("a person outside a turn");
        env.turning.push("human:nick");
        let r = run_classed(&mut s, &env, NODE_A, "human:nick", None, finding("inside"));
        let e = r.expect_err("refused");
        assert_eq!(e.code(), Some("entry.not_allowed"));
        assert_eq!(e.data()["class"], "person");
    }

    /// Only the exact lowercase `human:` with a well-formed name is a
    /// person in the store too: `human:Ana` reaching an op without the
    /// CLI's check is classed by its definition, here none, so `other`
    /// (Opus #11514).
    #[test]
    fn a_misspelled_human_is_no_person_in_a_store_op() {
        assert_eq!(Class::of("human:Ana", &[]), Other);
        assert_eq!(Class::of("HUMAN:nick", &[]), Other);
        assert_eq!(Class::of("human:", &[]), Other);
        assert_eq!(Class::of("human:a,b", &[]), Other);
        assert_eq!(Class::of("human:nick", &[]), Person);
        let dir = Scratch::new("person-spelling");
        let mut s = dir.store();
        let env = Classed::new();
        for actor in ["human:Ana", "HUMAN:nick", "Human:nick"] {
            let e = run_classed(&mut s, &env, NODE_A, actor, None, finding("misspelled"))
                .expect_err("not a person");
            assert_eq!(e.code(), Some("entry.not_allowed"), "{actor}");
            assert_eq!(e.data()["class"], "other", "{actor}");
        }
        run_classed(
            &mut s,
            &env,
            NODE_A,
            "human:nick",
            None,
            finding("a person"),
        )
        .expect("a person adds a finding");
    }

    #[test]
    fn an_unjudgeable_class_refuses_every_op() {
        let dir = Scratch::new("unclassed");
        let mut s = dir.store();
        let mut env = Classed::new();
        env.unclassed = true;
        for actor in ["rev", "wrk"] {
            let r = run_classed(&mut s, &env, NODE_A, actor, None, finding("refused"));
            assert_eq!(code(r), "entry.not_allowed", "{actor}");
        }
        assert!(
            s.entries(&node(NODE_A).uuid, &Filter::default())
                .expect("read")
                .is_empty()
        );
    }

    #[test]
    fn a_finding_runs_open_fixed_verified_and_then_takes_nothing() {
        let dir = Scratch::new("finding");
        let mut s = dir.store();
        round(&mut s, "c1", "c3", "m-round");
        let added = run(&mut s, "rev", Reviewer, json!({"op": "add", "kind": "finding", "title": "leaks a lease", "anchor": "src/a.rs:3"})).expect("add");
        assert_eq!(added.changed[0].id, "F1");
        assert_eq!(state(&s, "F1"), "open");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.severity.as_deref(), Some(DEFAULT_SEVERITY));
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "c3"}),
        )
        .expect("verify");
        assert_eq!(state(&s, "F1"), "verified");
        for o in [
            json!({"op": "reopen", "entry": "F1", "at": "c3"}),
            json!({"op": "outdated", "entry": "F1"}),
        ] {
            assert_eq!(
                code(run(&mut s, "rev", Reviewer, o)),
                "entry.bad_transition"
            );
        }
        assert_eq!(
            code(run(
                &mut s,
                "h",
                Person,
                // A residual on a verified finding is bound to a head, so
                // it needs `at` (A29, "Re-verify").
                json!({"op": "residual", "entry": "F1", "text": "x"})
            )),
            "entry.bad_op"
        );
        // Ids are per node and kind, and never reused.
        let second = run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "another"}),
        )
        .expect("F2");
        assert_eq!(second.changed[0].id, "F2");
        let on_b = run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "issue", "title": "b", "node": NODE_B}),
        )
        .expect("I1");
        assert_eq!(on_b.changed[0].path(), format!("{NODE_B}#I1"));
        s.verify_log().expect("the chain holds");
    }

    #[test]
    fn who_may_do_what_is_checked_per_class() {
        let dir = Scratch::new("who");
        let mut s = dir.store();
        round(&mut s, "c1", "c3", "m-round");
        let add = json!({"op": "add", "kind": "finding", "title": "t"});
        assert_eq!(
            code(run(&mut s, "w", Worker, add.clone())),
            "entry.not_allowed"
        );
        assert_eq!(
            code(run(&mut s, "x", Other, add.clone())),
            "entry.not_allowed"
        );
        run(&mut s, "orchestrator", Coordinator, add).expect("a coordinator adds");
        assert_eq!(
            code(run(
                &mut s,
                "rev",
                Reviewer,
                json!({"op": "fix", "entry": "F1", "sha": "c2"})
            )),
            "entry.not_allowed"
        );
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        for (who, class) in [("w", Worker), ("orchestrator", Coordinator)] {
            let r = run(
                &mut s,
                who,
                class,
                json!({"op": "verify", "entry": "F1", "at": "c3"}),
            );
            assert_eq!(code(r), "entry.not_allowed", "{who}");
        }
        assert_eq!(
            code(run(
                &mut s,
                "w",
                Worker,
                json!({"op": "residual", "entry": "F1", "text": "later"})
            )),
            "entry.not_allowed"
        );
        assert_eq!(
            code(run(
                &mut s,
                "w",
                Worker,
                json!({"op": "req", "req": "R1", "state": "met"})
            )),
            "entry.not_allowed"
        );
        run(
            &mut s,
            "human:nick",
            Person,
            json!({"op": "verify", "entry": "F1", "at": "c3"}),
        )
        .expect("a person verifies");
        // The refused ops wrote nothing.
        let history = s.entry_history(&node(NODE_A).uuid, "F1").expect("history");
        let ops: Vec<&str> = history.iter().map(|e| e.op.as_str()).collect();
        assert_eq!(ops, vec!["open", "fix", "verify"]);
        assert!(history.iter().all(|e| e.log_seq.is_some()));
    }

    #[test]
    fn shas_are_checked_against_the_range() {
        let dir = Scratch::new("shas");
        let mut s = dir.store();
        round(&mut s, "c1", "c3", "m-round");
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "t"}),
        )
        .expect("add");
        assert_eq!(
            code(run(
                &mut s,
                "w",
                Worker,
                json!({"op": "fix", "entry": "F1", "sha": "ff"})
            )),
            "entry.unknown_commit"
        );
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        )
        .expect("fix");
        assert_eq!(
            code(run(
                &mut s,
                "rev",
                Reviewer,
                json!({"op": "verify", "entry": "F1", "at": "c2"})
            )),
            "entry.not_covered"
        );
        assert_eq!(
            code(run(
                &mut s,
                "rev",
                Reviewer,
                json!({"op": "reopen", "entry": "F1", "at": "e1"})
            )),
            "entry.fix_after_range"
        );
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "reopen", "entry": "F1", "at": "c3", "text": "still leaks"}),
        )
        .expect("reopen");
        assert_eq!(state(&s, "F1"), "reopened");
    }

    /// P4's reader: an existing store's requirement states, read-only and
    /// scoped to the graph; a missing, linked, foldered or other-version
    /// store is unavailable, nothing is created, and a bad id says so
    /// (Opus's #8400, Terra's #8445).
    #[test]
    fn the_requirement_reader_reads_states_and_never_creates_a_store() {
        use mdkg_format::Code;
        let dir = Scratch::new("req-reader");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.folder = NodeFolder::In(".mdkg".into());
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "req", "req": "R1", "state": "met"}),
        )
        .expect("met");
        run_env(
            &mut s,
            &env,
            "human:nick",
            Person,
            json!({"op": "req", "req": "R2", "state": "waived", "text": "later"}),
        )
        .expect("waived");
        // The same node UUID in another graph sharing the store.
        env.folder = NodeFolder::In("b/.mdkg".into());
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "req", "req": "R3", "state": "met"}),
        )
        .expect("other graph");
        // Graph B's change to the R1 graph A recorded is refused, never
        // written across graphs (Terra's #8791); so is one from a node
        // whose graph can't be told.
        for folder in [NodeFolder::In("b/.mdkg".into()), NodeFolder::Nowhere] {
            env.folder = folder;
            assert_eq!(
                code(run_env(
                    &mut s,
                    &env,
                    "rev",
                    Reviewer,
                    json!({"op": "req", "req": "R1", "state": "unmet"}),
                )),
                "entry.not_allowed"
            );
        }
        drop(s);
        let path = dir.0.join("coord.sqlite");
        let before = std::fs::read(&path).expect("store");
        let reader = RequirementStateReader::open_at(&path).expect("reader");
        let states = reader.states(".mdkg", NODE_A).expect("states");
        assert_eq!(states.get("R1").map(String::as_str), Some("met"));
        assert_eq!(states.get("R2").map(String::as_str), Some("waived"));
        assert_eq!(states.get("R3"), None, "another graph's state");
        assert_eq!(reader.states("B/.mdkg", NODE_A).expect("b").len(), 1);
        assert!(
            reader
                .states(".mdkg", NODE_B)
                .expect("other node")
                .is_empty()
        );
        assert_eq!(
            reader.states(".mdkg", "task-12").expect_err("bad id").code,
            Code::NodeBadId
        );
        assert_eq!(
            std::fs::read(&path).expect("store"),
            before,
            "nothing written"
        );
        // A store another process moved to another version since the open
        // is reported, never read.
        {
            let w = Connection::open(&path).expect("writer");
            w.execute("UPDATE meta SET value = '999' WHERE key = 'msg_schema'", [])
                .expect("bump");
        }
        assert_eq!(
            reader.states(".mdkg", NODE_A).expect_err("stale").code,
            Code::CoordStoreFailed
        );
        drop(reader);
        assert_eq!(
            RequirementStateReader::open_at(&path)
                .expect_err("other version")
                .code,
            Code::CoordStoreFailed
        );
        {
            let w = Connection::open(&path).expect("writer");
            w.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'msg_schema'",
                [crate::mail::MAIL_SCHEMA_VERSION],
            )
            .expect("restore");
        }
        // A link to the store, and a folder in its place, are refused.
        let link = dir.0.join("link.sqlite");
        std::os::unix::fs::symlink(&path, &link).expect("link");
        assert_eq!(
            RequirementStateReader::open_at(&link)
                .expect_err("link")
                .code,
            Code::CoordStoreFailed
        );
        let folder = dir.0.join("folder.sqlite");
        std::fs::create_dir(&folder).expect("folder");
        assert_eq!(
            RequirementStateReader::open_at(&folder)
                .expect_err("folder")
                .code,
            Code::CoordStoreFailed
        );
        let missing = dir.0.join("none").join("coord.sqlite");
        let e = RequirementStateReader::open_at(&missing).expect_err("unavailable");
        assert_eq!(e.code, Code::CoordStoreFailed);
        assert!(!missing.exists() && !missing.parent().expect("dir").exists());
    }

    #[test]
    fn the_requirement_reader_leaves_zero_byte_store_and_sidecars_untouched() {
        let dir = Scratch::new("req-reader-zero");
        let path = dir.0.join("coord.sqlite");
        let files = [
            (path.clone(), Vec::new()),
            (dir.0.join("coord.sqlite-wal"), b"existing wal".to_vec()),
            (dir.0.join("coord.sqlite-shm"), b"existing shm".to_vec()),
            (
                dir.0.join("coord.sqlite-journal"),
                b"existing journal".to_vec(),
            ),
        ];
        for (file, bytes) in &files {
            std::fs::write(file, bytes).expect("fixture");
        }
        let error = RequirementStateReader::open_at(&path).expect_err("absent store");
        for (file, bytes) in &files {
            assert_eq!(
                std::fs::read(file).expect("file remains"),
                *bytes,
                "{}",
                file.display()
            );
        }
        assert_eq!(error.code, mdkg_format::Code::CoordStoreFailed);
        assert!(error.message.contains("absent"), "{}", error.message);
    }

    /// SQLite's NOFOLLOW refuses a link anywhere in the path, so the
    /// reader resolves the folder first: a store reached through a linked
    /// folder opens (`/tmp` and `/var` are links on macOS), and a store
    /// file that is itself a link is still refused, through a linked
    /// folder or a real one. Built under the scratch folder's real path,
    /// so it doesn't depend on whether `$TMPDIR` holds a link.
    #[test]
    fn the_requirement_reader_follows_a_linked_folder_but_never_a_linked_store() {
        use mdkg_format::Code;
        let dir = Scratch::new("req-reader-links");
        let root = std::fs::canonicalize(&dir.0).expect("real scratch");
        let real = root.join("real");
        std::fs::create_dir(&real).expect("real folder");
        drop(Store::open_with(&real.join("coord.sqlite"), 1_000, Upgrade::Allow).expect("store"));
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&real, &linked).expect("linked folder");
        let reader =
            RequirementStateReader::open_at(&linked.join("coord.sqlite")).expect("through a link");
        assert!(reader.states(".mdkg", NODE_A).expect("states").is_empty());
        drop(reader);
        // Relative to a linked folder, too: `..` isn't resolved lexically.
        RequirementStateReader::open_at(&linked.join("../real/coord.sqlite"))
            .expect("through a link and ..");
        std::os::unix::fs::symlink(real.join("coord.sqlite"), real.join("link.sqlite"))
            .expect("linked store");
        for path in [real.join("link.sqlite"), linked.join("link.sqlite")] {
            assert_eq!(
                RequirementStateReader::open_at(&path)
                    .expect_err("a linked store")
                    .code,
                Code::CoordStoreFailed,
                "{}",
                path.display()
            );
        }
    }

    /// A29 condition 2: a serious verified finding is contained when a
    /// walk ending at the final round covers it and `--at` holds the
    /// walk's paths as F does; a later touch to its fix's paths needs a
    /// re-verify, which a bound residual accepts only while nothing after
    /// its head touches them.
    #[test]
    fn the_gate_checks_coverage_content_and_reverify() {
        let dir = Scratch::new("gate-two");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.parents.insert("d1", Some("c3"));
        env.parents.insert("d2", Some("d1"));
        env.paths.insert("c2", vec!["src/a.rs"]);
        env.paths.insert("c3", vec!["src/a.rs", "src/b.rs"]);
        env.paths.insert("d1", vec!["src/a.rs"]);
        for (at, oid) in [
            ("c1", "aaa"),
            ("c2", "bbb"),
            ("c3", "ccc"),
            ("d1", "ddd"),
            ("d2", "ddd"),
        ] {
            env.trees.insert(
                (at, "src/a.rs"),
                Box::leak(format!("100644 blob {oid}\tsrc/a.rs").into_boxed_str()),
            );
        }
        for at in ["c3", "d1", "d2"] {
            env.trees
                .insert((at, "src/b.rs"), "100644 blob eee\tsrc/b.rs");
        }
        let gate = |s: &Store, env: &Fake, at: &str| {
            s.entry_gate(&node(NODE_A).uuid, &[], &sha(at), None, env)
                .expect("gate")
        };
        let complete = |s: &mut Store, env: &Fake, round: &str, range: &str| {
            run_env(
                s,
                env,
                "rev",
                Reviewer,
                json!({"op": "review.complete", "round": round, "range": range, "verdict": "clean"}),
            )
            .expect("complete");
        };
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        round(&mut s, "c1", "c3", "m1");
        // Verified inside the round, at c2: c3 then touched src/a.rs.
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "c2"}),
        )
        .expect("verify");
        // No closed round: not covered.
        let g = gate(&s, &env, "c3");
        assert_eq!(g.failures[0].reason, "not_covered", "{:?}", g.failures);
        assert_eq!(g.range, None);
        complete(&mut s, &env, "V1", "c1..c3");
        let g = gate(&s, &env, "c3");
        assert_eq!(g.range, Some((sha("c1"), sha("c3"))));
        assert_eq!(g.failures.len(), 1, "{:?}", g.failures);
        assert_eq!(g.failures[0].reason, "needs_reverify");
        assert_eq!(g.failures[0].data["paths"], json!(["src/a.rs"]));
        assert!(g.failures[0].residual);
        // A residual bound to V (c2) and F (c3) passes it.
        run_env(
            &mut s,
            &env,
            "human:nick",
            Person,
            json!({"op": "residual", "entry": "F1", "text": "accepted", "at": "c3"}),
        )
        .expect("residual");
        assert!(
            gate(&s, &env, "c3").ok(),
            "{:?}",
            gate(&s, &env, "c3").failures
        );
        // d1 changes src/a.rs again without a round: P's content at d1
        // differs from F's, so the gate at d1 fails on content.
        let g = gate(&s, &env, "d1");
        assert_eq!(g.failures[0].reason, "not_covered");
        assert_eq!(g.failures[0].data["paths"], json!(["src/a.rs"]));
        assert_eq!(g.failures[0].data["final"], json!(sha("c3")));
        // A round c3..d1 covers it, but touches the fix's paths after the
        // residual's head: a new decision is needed.
        round(&mut s, "c3", "d1", "m2");
        complete(&mut s, &env, "V2", "c3..d1");
        let g = gate(&s, &env, "d2");
        assert_eq!(g.range, Some((sha("c1"), sha("d1"))));
        assert_eq!(g.failures[0].reason, "needs_reverify", "{:?}", g.failures);
        // A re-verify at d1 by a reviewer (not the fixer) settles it.
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "d1"}),
        )
        .expect("reverify");
        assert!(
            gate(&s, &env, "d2").ok(),
            "{:?}",
            gate(&s, &env, "d2").failures
        );
        assert!(gate(&s, &env, "d1").ok());
    }

    /// Terra's #9504, major 3: a residual is bound to the walk it
    /// accepted, by its work base. History C1 < B0 < C2 < B1 < V < F0 <
    /// F1; the fix's commits are C1 and C2. A residual accepted C1 outside
    /// the walk from B0; once the walk starts at B1, C2 is outside too,
    /// and the residual no longer holds.
    #[test]
    fn a_residual_binds_the_walk_base_it_accepted() {
        let dir = Scratch::new("residual-base");
        let mut s = dir.store();
        let mut env = Fake::new();
        for (c, p) in [
            ("a1", None),
            ("b0", Some("a1")),
            ("a2", Some("b0")),
            ("b1", Some("a2")),
            ("a5", Some("b1")),
            ("f0", Some("a5")),
            ("f1", Some("f0")),
        ] {
            env.parents.insert(c, p);
        }
        env.paths.insert("a1", vec!["src/x.rs"]);
        env.paths.insert("a2", vec!["src/y.rs"]);
        env.paths.insert("f1", vec!["src/z.rs"]);
        for at in ["a5", "f0", "f1"] {
            env.trees
                .insert((at, "src/x.rs"), "100644 blob aaa\tsrc/x.rs");
            env.trees
                .insert((at, "src/y.rs"), "100644 blob bbb\tsrc/y.rs");
        }
        let gate = |s: &Store, env: &Fake, at: &str| {
            s.entry_gate(&node(NODE_A).uuid, &[], &sha(at), None, env)
                .expect("gate")
        };
        run_env(&mut s, &env, "rev", Reviewer, finding("F")).expect("add");
        run_env(
            &mut s,
            &env,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "a2", "commits": ["a1"]}),
        )
        .expect("fix");
        round(&mut s, "b0", "f0", "m1");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "a5"}),
        )
        .expect("verify");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "review.complete", "round": "V1", "range": "b0..f0", "verdict": "clean"}),
        )
        .expect("complete");
        let g = gate(&s, &env, "f0");
        assert_eq!(g.failures[0].reason, "needs_reverify", "{:?}", g.failures);
        assert_eq!(g.failures[0].data["outside"], json!([sha("a1")]));
        run_env(
            &mut s,
            &env,
            "human:nick",
            Person,
            json!({"op": "residual", "entry": "F1", "text": "C1 accepted", "at": "f0"}),
        )
        .expect("residual");
        let residual = s
            .entries(&node(NODE_A).uuid, &Filter::default())
            .expect("list")
            .into_iter()
            .find(|e| e.kind == Kind::Issue)
            .expect("issue");
        assert_eq!(residual.data["waives"]["base"], json!(sha("b0")));
        assert!(
            gate(&s, &env, "f0").ok(),
            "{:?}",
            gate(&s, &env, "f0").failures
        );
        // A round from B1, which main holds, becomes the final round: its
        // walk starts at B1, leaving C2 outside as well. F1 touches no
        // fixed path, so only the base binds the residual.
        round_with(&mut s, "b1", "f1", "m2", Some("main"));
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "review.complete", "round": "V2", "range": "b1..f1", "verdict": "clean"}),
        )
        .expect("complete");
        let g = gate(&s, &env, "f1");
        assert_eq!(g.range, Some((sha("b1"), sha("f1"))));
        assert_eq!(g.failures[0].reason, "needs_reverify", "{:?}", g.failures);
        assert_eq!(g.failures[0].data["outside"], json!([sha("a2"), sha("a1")]));
    }

    /// Terra's #9504, major 4: the walk the gate records is checked on
    /// content even with no finding, so a clean review whose path changed
    /// after it fails rather than reading as unsettled.
    #[test]
    fn a_clean_walk_is_checked_on_content() {
        let dir = Scratch::new("clean-content");
        let mut s = dir.store();
        let mut env = Fake::new();
        env.parents.insert("d1", Some("c3"));
        env.paths.insert("c2", vec!["src/x.rs"]);
        env.paths.insert("d1", vec!["src/x.rs"]);
        env.trees
            .insert(("c3", "src/x.rs"), "100644 blob aaa\tsrc/x.rs");
        env.trees
            .insert(("d1", "src/x.rs"), "100644 blob bbb\tsrc/x.rs");
        round(&mut s, "c1", "c3", "m1");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "review.complete", "round": "V1", "range": "c1..c3", "verdict": "clean"}),
        )
        .expect("complete");
        let gate = |at: &str| {
            s.entry_gate(&node(NODE_A).uuid, &[], &sha(at), None, &env)
                .expect("gate")
        };
        assert!(gate("c3").ok(), "{:?}", gate("c3").failures);
        let g = gate("d1");
        assert_eq!(g.range, Some((sha("c1"), sha("c3"))));
        assert_eq!(g.failures.len(), 1, "{:?}", g.failures);
        assert_eq!(g.failures[0].reason, "not_covered");
        assert!(!g.failures[0].residual);
        assert_eq!(g.failures[0].data["paths"], json!(["src/x.rs"]));
    }

    /// Terra's #9504, major 5: the final round among many is found with
    /// one batched history read, never a check per pair of heads.
    #[test]
    fn the_final_round_among_many_takes_one_history_read() {
        let dir = Scratch::new("many-rounds");
        let mut s = dir.store();
        let mut env = Fake::new();
        // A line of 200 commits after c3, each a round's head.
        let names: Vec<&'static str> = (0..200)
            .map(|i| &*Box::leak(format!("d{i:03x}").into_boxed_str()))
            .collect();
        let mut prev = "c3";
        for n in &names {
            env.parents.insert(n, Some(prev));
            prev = n;
        }
        for (i, n) in names.iter().enumerate() {
            let base = if i == 0 { "c3" } else { names[i - 1] };
            round(&mut s, base, n, &format!("m{i}"));
        }
        {
            let tx = s.conn.transaction().expect("tx");
            tx.execute(
                "UPDATE entries SET state = 'closed' WHERE kind = 'review'",
                [],
            )
            .expect("close");
            tx.commit().expect("commit");
        }
        let entries = s
            .entries(&node(NODE_A).uuid, &Filter::default())
            .expect("list");
        let rounds = rounds_in(&entries, true);
        assert_eq!(rounds.len(), 200);
        env.checks.set(0);
        let last = final_round(&rounds, &sha(names[199]), &env)
            .expect("final")
            .expect("a round");
        assert_eq!(last.head, sha(names[199]));
        assert_eq!(env.walks.get(), 1);
        assert_eq!(env.checks.get(), 0, "no per-head ancestry check");
    }

    /// A29 step 4: the settle evaluates the gate inside the settling
    /// transaction; a failing gate writes nothing, a passing one appends
    /// `ledger.settle` with what it checked.
    #[test]
    fn a_settle_checks_the_gate_and_records_what_it_settled() {
        let dir = Scratch::new("settle");
        let mut s = dir.store();
        let env = Fake::new();
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "bad", "severity": "blocker"}),
        )
        .expect("add");
        let link = node(NODE_A);
        let reqs = vec!["R1".to_string()];
        let at = sha("c3");
        let settle = Settle {
            node: &link,
            requirements: &reqs,
            at: &at,
            node_path: None,
            revision: "sha256:rev",
            changeset: Some("cs-1"),
        };
        let log_before: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM log", [], |r| r.get(0))
            .expect("log");
        {
            let tx = s.conn.transaction().expect("tx");
            match settle_in(&tx, "w", &settle, &env) {
                Err(SettleError::Gate(g)) => {
                    let reasons: Vec<&str> = g.failures.iter().map(|f| f.reason).collect();
                    assert_eq!(reasons, vec!["findings_open", "requirements_open"]);
                }
                other => panic!("expected a gate failure: {other:?}"),
            }
        }
        let log_after: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM log", [], |r| r.get(0))
            .expect("log");
        assert_eq!(log_before, log_after, "nothing written");
        assert!(s.ledger_settles(&link).expect("settles").is_empty());
        run_env(
            &mut s,
            &env,
            "human:nick",
            Person,
            json!({"op": "residual", "entry": "F1", "text": "accepted"}),
        )
        .expect("residual");
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "req", "req": "R1", "state": "met"}),
        )
        .expect("met");
        {
            let tx = s.conn.transaction().expect("tx");
            let gate = settle_in(&tx, "w", &settle, &env).expect("settles");
            assert!(gate.ok());
            tx.commit().expect("commit");
        }
        let records = s.ledger_settles(&link).expect("settles");
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r["node"], json!(NODE_A));
        assert_eq!(r["at"], json!(at));
        assert_eq!(r["revision"], json!("sha256:rev"));
        assert_eq!(r["changeset"], json!("cs-1"));
        assert_eq!(r["range"], Value::Null);
        assert!(r["upto"].as_u64().is_some_and(|u| u > 0));
        assert_eq!(r["entries"][0]["id"], json!("F1"));
        assert_eq!(r["entries"][0]["state"], json!("residual"));
        assert_eq!(
            r["requirements"],
            json!([{"id": "R1", "state": "met", "sha": Value::Null}])
        );
        // Another node's settle isn't this one's.
        assert!(s.ledger_settles(&node(NODE_B)).expect("other").is_empty());
        s.verify_log().expect("the chain holds");
    }

    /// A27 rule 8: a findings verdict needs a finding the completing
    /// reviewer opened, reopened or seconded since the round opened.
    #[test]
    fn a_findings_verdict_is_backed() {
        let dir = Scratch::new("verdict");
        let mut s = dir.store();
        let env = Fake::new();
        run_env(&mut s, &env, "rev", Reviewer, finding("before")).expect("before the round");
        round(&mut s, "c1", "c3", "m1");
        let findings = json!({"op": "review.complete", "round": "V1", "range": "c1..c3", "verdict": "findings"});
        assert_eq!(
            code(run_env(&mut s, &env, "rev", Reviewer, findings.clone())),
            "entry.verdict_unbacked"
        );
        // Another reviewer's finding backs nothing of rev's.
        run_env(&mut s, &env, "rev3", Reviewer, finding("theirs")).expect("theirs");
        assert_eq!(
            code(run_env(&mut s, &env, "rev", Reviewer, findings.clone())),
            "entry.verdict_unbacked"
        );
        // Seconding the earlier finding backs it.
        run_env(&mut s, &env, "rev", Reviewer, finding("before")).expect("second");
        run_env(&mut s, &env, "rev", Reviewer, findings).expect("backed");
    }

    #[test]
    fn the_node_text_drops_status_updated_and_the_ledger() {
        let a = b"---\nid: x\nstatus: todo\nupdated: 1\n---\n# Body\n\nText.\n";
        let b = b"---\nid: x\nstatus: done\nupdated: 2\n---\n# Body\n\nText.\n# Ledger\n\n<!-- mdkg:ledger begin upto=3 -->\n- F1 major verified\n<!-- mdkg:ledger end -->\n";
        assert_eq!(node_revision_text(a), node_revision_text(b));
        let c = b"---\nid: x\nstatus: done\n---\n# Body\n\nText changed.\n";
        assert_ne!(node_revision_text(a), node_revision_text(c));
        // Only the writer's own rendering is stripped (Opus #10156, 1):
        // an appended requirement line, a lower heading or a line outside
        // the markers keeps the section in the comparison.
        let ledger = "# Ledger\n\n<!-- mdkg:ledger begin upto=7 -->\n- F1 major verified@0123456789ab by rev: Title (src/a.rs:3)\n- R2 met@0123456789ab by rev\n- R3 waived by human:nick: later\n- \u{2026} 4 more: mdkg entry list task-x\n<!-- mdkg:ledger end -->\n";
        let d = format!("# Body\nText.\n{ledger}");
        assert_eq!(
            node_revision_text(d.as_bytes()),
            b"# Body\nText.\n".to_vec()
        );
        for appended in [
            "- R1: the system shall do something unreviewed\n",
            "## More\n",
            "Text.\n",
        ] {
            let inside = d.replacen(
                "<!-- mdkg:ledger end -->",
                &format!("{appended}<!-- mdkg:ledger end -->"),
                1,
            );
            assert_eq!(
                node_revision_text(inside.as_bytes()),
                inside.as_bytes().to_vec(),
                "{appended}"
            );
            let after = format!("{d}{appended}");
            assert_eq!(
                node_revision_text(after.as_bytes()),
                after.as_bytes().to_vec(),
                "{appended}"
            );
        }
        let unmarked = b"# Body\n# Ledger\n- F1 major\n";
        assert_eq!(node_revision_text(unmarked), unmarked.to_vec());
        // Only the last level-one heading can be the writer's section:
        // one followed by another, ATX or setext, strips nothing (Terra's
        // #9504, blocker 1).
        for kept in [
            &b"# Ledger\n- F1\n# After\nkept\n"[..],
            b"# Ledger\n- F1\n   # Indented after\nkept\n",
            b"# Ledger\n- F1\nAfter\n=====\nkept\n",
        ] {
            assert_eq!(node_revision_text(kept), kept.to_vec());
        }
        // Two candidates strip nothing.
        let two = b"# Body\n# Ledger\n- R1: reviewed text\n# Ledger\n- F1\n";
        assert_eq!(node_revision_text(two), two.to_vec());
        // Never fenced or indented.
        for kept in [&b"```\n# Ledger\n```\n"[..], b"x\n # Ledger\n- F1\n"] {
            assert_eq!(node_revision_text(kept), kept.to_vec());
        }
        // A three-backtick line inside a four-backtick block is fenced
        // text, so a quoted `# Ledger` after it hides nothing (Opus #9431,
        // minor 2); a tilde run never closes a backtick fence.
        let f = b"````\n```\n# Ledger\nreviewed change\n````\n# After\n";
        assert_eq!(node_revision_text(f), f.to_vec());
        let g = b"```\n~~~\n# Ledger\nx\n```\n";
        assert_eq!(node_revision_text(g), g.to_vec());
        // A closing run may be longer, but not followed by text.
        let h = format!("```\n```rust\n# Ledger\nx\n`````\n{ledger}");
        assert_eq!(
            node_revision_text(h.as_bytes()),
            b"```\n```rust\n# Ledger\nx\n`````\n".to_vec()
        );
        // The reviewer's case: a ledger heading inside an HTML comment
        // never strips text that isn't the writer's, so a change to R1
        // after it shows. Comments aren't tracked (Terra's #10220, major
        // 5): the commented `# Ledger` counts, and `# Other` after it is
        // the last heading.
        let reviewed =
            b"# Overview\n<!--\n# Ledger\n-->\n## Requirements\n- R1: reviewed text\n# Other\n";
        let changed =
            b"# Overview\n<!--\n# Ledger\n-->\n## Requirements\n- R1: changed text\n# Other\n";
        assert_ne!(node_revision_text(reviewed), node_revision_text(changed));
        assert_eq!(node_revision_text(reviewed), reviewed.to_vec());
        // A comment opened mid-line hides nothing, and a tail that isn't
        // the grammar keeps the whole text; one opened and closed on its
        // line, or quoted in a code span, is just text before the section.
        let mid = b"Text <!-- note\n# Ledger\n-->\n";
        assert_eq!(node_revision_text(mid), mid.to_vec());
        let closed = format!("Text <!-- note --> and `<!--`\n{ledger}");
        assert_eq!(
            node_revision_text(closed.as_bytes()),
            b"Text <!-- note --> and `<!--`\n".to_vec()
        );
    }

    /// Terra's code review #10220, blockers 2 and 3, with the
    /// orchestrator's call: escaped comment text, a tab-indented fence or
    /// an over-indented closing fence between `# Ledger` and a real
    /// `# Requirements` strips nothing, since the tail isn't plainly the
    /// writer's grammar; so a change to R1 shows. Before the section, a
    /// misread can strip only writer-grammar lines.
    #[test]
    fn misread_markdown_after_the_ledger_heading_strips_nothing() {
        let grammar =
            "<!-- mdkg:ledger begin upto=1 -->\n- F1 major verified\n<!-- mdkg:ledger end -->\n";
        for between in [
            "\\<!-- literal\n",
            "\t```\n",
            "```\n    ```\n",
            "~~~\n     ~~~\n",
        ] {
            for shape in [
                format!("# Body\nText.\n# Ledger\n{between}# Requirements\n- R1: reviewed\n"),
                format!(
                    "# Body\nText.\n# Ledger\n\n{grammar}{between}# Requirements\n- R1: reviewed\n"
                ),
                format!(
                    "# Body\nText.\n# Ledger\n\n<!-- mdkg:ledger begin upto=1 -->\n{between}# Requirements\n- R1: reviewed\n<!-- mdkg:ledger end -->\n"
                ),
            ] {
                let changed = shape.replace("R1: reviewed", "R1: changed");
                assert_eq!(
                    node_revision_text(shape.as_bytes()),
                    shape.as_bytes().to_vec(),
                    "{shape:?}"
                );
                assert_ne!(
                    node_revision_text(shape.as_bytes()),
                    node_revision_text(changed.as_bytes()),
                    "{shape:?}"
                );
            }
        }
        // Before the heading, an over-indented closing fence (fenced text
        // to Markdown, which the scan closes on) can only expose a
        // `# Ledger` whose tail is the grammar: what's stripped is that
        // grammar and nothing else, and R1 above it still compares.
        let before =
            format!("# Requirements\n- R1: reviewed\n```\ncode\n    ```\n# Ledger\n\n{grammar}");
        assert_eq!(
            node_revision_text(before.as_bytes()),
            b"# Requirements\n- R1: reviewed\n```\ncode\n    ```\n".to_vec()
        );
        let changed = before.replace("R1: reviewed", "R1: changed");
        assert_ne!(
            node_revision_text(before.as_bytes()),
            node_revision_text(changed.as_bytes())
        );
    }

    /// Terra's code review #10220, major 5: unmatched backtick runs of
    /// every length up to two thousand on one line, about 2 MB, read in
    /// one linear pass (the comment scan that rescanned the line per run
    /// is gone).
    #[test]
    fn a_line_of_unmatched_backtick_runs_reads_in_linear_time() {
        let mut line = String::from("# Ledger ");
        for n in 1..=2000 {
            line.push_str(&"`".repeat(n));
            line.push(' ');
        }
        let text = format!("# Body\n{line}\n# Ledger\n- F1\n");
        let started = std::time::Instant::now();
        assert_eq!(
            node_revision_text(text.as_bytes()),
            text.as_bytes().to_vec()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    /// Terra's code review #10220, major 4, with the orchestrator's call:
    /// a round's range is stored as full lowercase commit ids, resolved
    /// when it opens; one that doesn't resolve, or isn't full, is
    /// refused; and a stored row that isn't (an earlier build's) fails the
    /// gate closed, never skipping the content check.
    #[test]
    fn round_ranges_are_full_commit_ids_and_a_short_stored_one_fails_closed() {
        let dir = Scratch::new("canonical");
        let mut s = dir.store();
        let env = Fake::new();
        let a = node(NODE_A);
        let open = |s: &mut Store, range: (&str, &str), id: &str| {
            s.entry_open_round(
                &a,
                "review",
                range,
                &["rev".into()],
                "orchestrator",
                id,
                1,
                false,
                &env,
            )
        };
        assert_eq!(
            open(&mut s, ("c1", "zz"), "m0")
                .expect_err("unknown")
                .code(),
            Some("entry.unknown_commit")
        );
        open(&mut s, ("c1", "c3"), "m1")
            .expect("open")
            .expect("a round");
        let entries = s.entries(&a.uuid, &Filter::default()).expect("list");
        assert_eq!(
            entries[0].range(),
            Some((sha("c1"), sha("c3"))),
            "stored whole"
        );
        // The store API refuses a range that isn't already whole.
        for range in [
            (sha("c1"), "c3".to_string()),
            (sha("c1"), sha("c3").to_ascii_uppercase()),
        ] {
            let tx = s.conn.transaction().expect("tx");
            let refused = open_review_from_mail_in(
                &tx,
                &a,
                "review",
                (&range.0, &range.1),
                &["rev".into()],
                "orchestrator",
                "m2",
                1,
                None,
            )
            .expect_err("refused");
            assert_eq!(refused.code(), Some("entry.bad_op"), "{range:?}");
        }
        run_env(
            &mut s,
            &env,
            "rev",
            Reviewer,
            json!({"op": "review.complete", "round": "V1", "range": "c1..c3", "verdict": "clean"}),
        )
        .expect("complete");
        assert!(
            s.entry_gate(&a.uuid, &[], &sha("c3"), None, &env)
                .expect("gate")
                .range
                .is_some()
        );
        // An earlier build's row, its head stored as given.
        for head in ["c3".to_string(), sha("c3").to_ascii_uppercase()] {
            s.conn
                .execute(
                    "UPDATE entries SET data = json_set(data, '$.range.sha', ?1)
                     WHERE kind = 'review'",
                    params![head],
                )
                .expect("legacy row");
            let err = s
                .entry_gate(&a.uuid, &[], &sha("c3"), None, &env)
                .expect_err("fails closed");
            assert_eq!(err.code(), Some("entry.git"), "{head}");
            assert!(err.to_string().contains("not full commit ids"), "{err}");
        }
    }

    /// Terra's code review #10220, major 6: the final round's distinct
    /// heads are counted as they're added, so more than the walk limit
    /// is refused before any history read.
    #[test]
    fn the_final_round_refuses_past_the_head_limit_before_reading_history() {
        let env = Fake::new();
        let rounds: Vec<Round> = (0..=WALK_WORK)
            .map(|i| Round {
                id: format!("V{}", i + 1),
                number: i as u64 + 1,
                closed: true,
                base: sha("c1"),
                head: format!("{:040x}", i + 1),
                anchored: None,
            })
            .collect();
        env.walks.set(0);
        let err = final_round(&rounds, &sha("c3"), &env).expect_err("limit");
        assert!(matches!(err, EntryError::WalkLimit(WALK_WORK)), "{err}");
        assert_eq!(env.walks.get(), 0, "no history read");
    }

    #[test]
    fn questions_block_the_node_until_answered_by_whom_they_ask() {
        let dir = Scratch::new("questions");
        let mut s = dir.store();
        run(&mut s, "w", Worker, json!({"op": "ask", "title": "which schema?", "asked_of": "orchestrator", "blocking": true})).expect("ask");
        let all = s
            .entries(&node(NODE_A).uuid, &Filter::default())
            .expect("list");
        assert_eq!(input_required(&all).len(), 1);
        let gate = s
            .entry_gate(&node(NODE_A).uuid, &[], &sha("c3"), None, &Fake::new())
            .expect("gate");
        assert_eq!(gate.failures[0].reason, "input_required");
        assert_eq!(
            code(run(
                &mut s,
                "rev",
                Reviewer,
                json!({"op": "answer", "entry": "Q1", "text": "4"})
            )),
            "entry.not_allowed"
        );
        assert_eq!(
            code(run(
                &mut s,
                "rev",
                Reviewer,
                json!({"op": "withdraw", "entry": "Q1"})
            )),
            "entry.not_allowed"
        );
        run(
            &mut s,
            "orchestrator",
            Coordinator,
            json!({"op": "answer", "entry": "Q1", "text": "schema 4"}),
        )
        .expect("answer");
        assert_eq!(
            code(run(
                &mut s,
                "orchestrator",
                Coordinator,
                json!({"op": "close", "entry": "Q1"})
            )),
            "entry.not_allowed"
        );
        run(&mut s, "w", Worker, json!({"op": "close", "entry": "Q1"})).expect("close");
        assert!(
            s.entry_gate(&node(NODE_A).uuid, &[], &sha("c3"), None, &Fake::new())
                .expect("gate")
                .ok()
        );
        // A blocking question asked of no one: only its asker answers it,
        // so no other actor can clear the gate by answering.
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "ask", "title": "unaddressed?", "blocking": true}),
        )
        .expect("ask");
        for (actor, class) in [
            ("rev", Reviewer),
            ("w2", Worker),
            ("orchestrator", Coordinator),
        ] {
            assert_eq!(
                code(run(
                    &mut s,
                    actor,
                    class,
                    json!({"op": "answer", "entry": "Q2", "text": "x"})
                )),
                "entry.not_allowed",
                "{actor}"
            );
        }
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "answer", "entry": "Q2", "text": "mine"}),
        )
        .expect("the asker answers");
        // The answer's text is on its event, never in the log.
        let q = s.entry_history(&node(NODE_A).uuid, "Q1").expect("history");
        assert_eq!(q[1].note.as_deref(), Some("schema 4"));
        let log = crate::log::texts(&s.conn).expect("log");
        assert!(
            log.iter()
                .all(|(_, _, t)| !t.contains("schema 4") && !t.contains("which schema"))
        );
    }

    #[test]
    fn decisions_in_force_are_the_latest_accepted_per_slot() {
        let dir = Scratch::new("decisions");
        let mut s = dir.store();
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "decide", "title": "use sqlite", "slot": "store"}),
        )
        .expect("D1");
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "decide", "title": "no daemon"}),
        )
        .expect("D2");
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "propose", "title": "use wal", "slot": "store", "supersedes": ["D1"]}),
        )
        .expect("D3");
        let force = |s: &Store| {
            let all = s
                .entries(&node(NODE_A).uuid, &Filter::default())
                .expect("list");
            decisions_in_force(&all)
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(force(&s), vec!["D1", "D2"]);
        assert_eq!(
            code(run(
                &mut s,
                "x",
                Other,
                json!({"op": "decide", "entry": "D3"})
            )),
            "entry.not_allowed"
        );
        run(
            &mut s,
            "orchestrator",
            Coordinator,
            json!({"op": "decide", "entry": "D3"}),
        )
        .expect("accept");
        assert_eq!(state(&s, "D1"), "superseded");
        assert_eq!(force(&s), vec!["D2", "D3"]);
        assert_eq!(
            code(run(
                &mut s,
                "w",
                Worker,
                json!({"op": "reject", "entry": "D3", "text": "no"})
            )),
            "entry.bad_transition"
        );
    }

    #[test]
    fn the_gate_fails_each_condition_alone_and_residuals_settle_what_they_may() {
        let dir = Scratch::new("gate");
        let mut s = dir.store();
        round(&mut s, "c1", "c3", "m-round");
        let uuid = node(NODE_A).uuid;
        let gate = |s: &Store, at: &str| {
            s.entry_gate(&uuid, &["R1".into()], &sha(at), None, &Fake::new())
                .expect("gate")
        };
        assert_eq!(gate(&s, "c3").failures[0].reason, "requirements_open");
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "req", "req": "R1", "state": "met", "at": "c3"}),
        )
        .expect("met");
        assert!(gate(&s, "c3").ok());
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "t", "severity": "blocker"}),
        )
        .expect("F1");
        assert_eq!(gate(&s, "c3").failures[0].reason, "findings_open");
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        assert_eq!(gate(&s, "c3").failures[0].reason, "fixes_unverified");
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "c3"}),
        )
        .expect("verify");
        // No closed round covers it yet (A29: no coverage without one).
        assert_eq!(gate(&s, "c3").failures[0].reason, "not_covered");
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "review.complete", "round": "V1", "range": "c1..c3", "verdict": "clean"}),
        )
        .expect("complete");
        assert!(gate(&s, "c3").ok());
        // At a head without the verified SHA: not waivable.
        let off = gate(&s, "e1");
        assert_eq!(off.failures[0].reason, "not_contained");
        assert!(!off.failures[0].residual);
        // A minor finding left fixed only warns.
        run(
            &mut s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "nit", "severity": "minor"}),
        )
        .expect("F2");
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F2", "sha": "c3"}),
        )
        .expect("fix");
        let g = gate(&s, "c3");
        assert!(g.ok());
        assert_eq!(g.warnings[0].items, vec!["F2".to_string()]);
        // A coordinator's residual on a blocking question passes condition 3.
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "ask", "title": "ok?", "blocking": true}),
        )
        .expect("Q1");
        assert!(!gate(&s, "c3").ok());
        let r = run(
            &mut s,
            "orchestrator",
            Coordinator,
            json!({"op": "residual", "entry": "Q1", "text": "ship without"}),
        )
        .expect("residual");
        assert_eq!(r.changed.last().map(|c| c.to.as_str()), Some("residual"));
        assert_eq!(state(&s, "Q1"), "open");
        assert!(gate(&s, "c3").ok());
        assert_eq!(gate(&s, "c3").upto, s.ledger_upto(&uuid).expect("upto"));
    }

    #[test]
    fn a_repeated_finding_names_the_open_one() {
        let dir = Scratch::new("dup");
        let mut s = dir.store();
        let add = json!({"op": "add", "kind": "finding", "title": "same", "anchor": {"path": "a.rs", "line": 2}});
        run(&mut s, "rev", Reviewer, add.clone()).expect("F1");
        let again = run(&mut s, "rev2", Reviewer, add.clone()).expect("again");
        assert_eq!(again.existing, Some(format!("{NODE_A}#F1")));
        // It seconds F1: one `second` event, state unchanged.
        assert_eq!(again.changed.len(), 1);
        assert_eq!(again.changed[0].op, "second");
        assert_eq!(state(&s, "F1"), "open");
        // A higher severity raises F1's; a lower one never lowers it.
        let mut blocker = add.clone();
        blocker["severity"] = json!("blocker");
        run(&mut s, "rev3", Reviewer, blocker).expect("raise");
        let mut minor = add.clone();
        minor["severity"] = json!("minor");
        run(&mut s, "rev4", Reviewer, minor).expect("no lower");
        let f = s.entry(&node(NODE_A).uuid, "F1").expect("r").expect("F1");
        assert_eq!(f.severity.as_deref(), Some("blocker"));
        // A fixed finding isn't repeated: the same add opens F2.
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c2"}),
        )
        .expect("fix");
        let fresh = run(&mut s, "rev", Reviewer, add).expect("new");
        assert_eq!(fresh.changed[0].id, "F2");
        assert_eq!(fresh.existing, None);
    }

    #[test]
    fn a_round_closes_only_by_its_named_reviewers_completing_it() {
        let dir = Scratch::new("review");
        let mut s = dir.store();
        let a = node(NODE_A);
        {
            let tx = s.conn.transaction().expect("tx");
            open_review_from_mail_in(
                &tx,
                &a,
                "review P3",
                (&sha("c1"), &sha("c3")),
                &["rev1".into(), "role:review".into()],
                "orchestrator",
                "m1",
                1,
                None,
            )
            .expect("open");
            tx.commit().expect("commit");
        }
        let complete = |range: &str| json!({"op": "review.complete", "round": "V1", "range": range, "verdict": "clean"});
        assert_eq!(
            code(run(&mut s, "rev1", Reviewer, complete("c1..c2"))),
            "entry.range_mismatch"
        );
        assert_eq!(
            code(run(&mut s, "rev3", Reviewer, complete("c1..c3"))),
            "entry.not_allowed"
        );
        assert_eq!(
            code(run(&mut s, "w", Worker, complete("c1..c3"))),
            "entry.not_allowed"
        );
        run(&mut s, "rev1", Reviewer, complete("c1..c3")).expect("rev1");
        assert_eq!(state(&s, "V1"), "answered");
        assert_eq!(
            code(run(&mut s, "rev1", Reviewer, complete("c1..c3"))),
            "entry.bad_transition"
        );
        run(&mut s, "rev2", Reviewer, complete("c1..c3")).expect("the role's holder");
        assert_eq!(state(&s, "V1"), "closed");
        let v = s.entry(&a.uuid, "V1").expect("r").expect("V1");
        assert_eq!(
            v.completed_by(),
            vec!["rev1".to_string(), "role:review".into()]
        );
        assert_eq!(v.snapshot()["completed"][1]["by"], "rev2");
    }

    #[test]
    fn op_objects_refuse_unknown_fields_and_any_actor() {
        let bad = |v: Value| match v {
            Value::Object(m) => Op::from_object(&m).expect_err("refused").code(),
            _ => None,
        };
        assert_eq!(
            bad(json!({"op": "fix", "entry": "F1", "sha": "c2", "actor": "rev"})),
            Some("entry.bad_op")
        );
        assert_eq!(
            bad(json!({"op": "verify", "entry": "F1", "at": "c2", "as": "human:nick"})),
            Some("entry.bad_op")
        );
        assert_eq!(
            bad(json!({"op": "add", "kind": "review", "title": "t"})),
            Some("entry.bad_op")
        );
        assert_eq!(bad(json!({"op": "nope"})), Some("entry.bad_op"));
        // A missing field names itself, never reads as an unknown op (e2e N5).
        let missing = Op::from_object(
            json!({"op": "add", "kind": "finding", "text": "x"})
                .as_object()
                .expect("obj"),
        )
        .expect_err("no title");
        assert_eq!(
            missing.to_string(),
            "the op's title is missing, not valid, or not one this op takes"
        );
        let unknown =
            Op::from_object(json!({"op": "nope"}).as_object().expect("obj")).expect_err("nope");
        assert_eq!(unknown.to_string(), "not a ledger op");
        assert_eq!(
            bad(
                json!({"op": "review.complete", "round": "F1", "range": "a..b", "verdict": "clean"})
            ),
            Some("entry.bad_op")
        );
        let o = op(json!({"op": "fix", "entry": format!("{NODE_B}#F3"), "sha": "c2"}));
        assert!(
            matches!(o.action, Action::Fix { ref entry, .. } if entry.node.as_deref() == Some(NODE_B) && entry.id == "F3")
        );
    }

    /// A node with an entry of every kind, a residual, and requirements met,
    /// waived and still unverified, for the `# Ledger` writer's tests.
    fn every_kind(s: &mut Store) {
        let mut env = Fake::new();
        env.paths.insert("c3", vec!["src/a.rs"]);
        env.trees
            .insert(("c2", "src/a.rs"), "100644 blob aaa\tsrc/a.rs");
        env.trees
            .insert(("c3", "src/a.rs"), "100644 blob bbb\tsrc/a.rs");
        let ok = |s: &mut Store, actor: &str, class: Class, v: Value| {
            run_env(s, &env, actor, class, v.clone()).unwrap_or_else(|e| panic!("{v}: {e}"));
        };
        ok(
            s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "title": "Leaks <!-- a lease -->", "anchor": "src/a.rs:3"}),
        );
        ok(
            s,
            "w",
            Worker,
            json!({"op": "fix", "entry": "F1", "sha": "c3"}),
        );
        round(s, "c1", "c3", "m1");
        ok(
            s,
            "rev",
            Reviewer,
            json!({"op": "verify", "entry": "F1", "at": "c3"}),
        );
        ok(
            s,
            "rev",
            Reviewer,
            json!({"op": "add", "kind": "finding", "severity": "minor", "title": "Off by -->one"}),
        );
        ok(
            s,
            "orch",
            Coordinator,
            json!({"op": "residual", "entry": "F2", "text": "later\n# Ledger\n<!-- x"}),
        );
        ok(
            s,
            "w",
            Worker,
            json!({"op": "add", "kind": "question", "title": "Which way?", "asked_of": "orch"}),
        );
        ok(
            s,
            "w",
            Worker,
            json!({"op": "propose", "title": "Use the store"}),
        );
        ok(
            s,
            "w",
            Worker,
            json!({"op": "add", "kind": "issue", "title": "Flaky test"}),
        );
        ok(
            s,
            "rev",
            Reviewer,
            json!({"op": "req", "req": "R1", "state": "met", "at": "c3"}),
        );
        ok(
            s,
            "human:nick",
            Person,
            json!({"op": "req", "req": "R2", "state": "waived", "text": "not in 0.7.1"}),
        );
    }

    const LEDGER_NODE: &str = "---\nid: x\nstatus: todo\nupdated: 1\n---\n# Overview\n\nText.\n\n# Requirements\n\n- R1: The writer shall render the strip's grammar.\n- R2: The writer shall cap its lines.\n- R3 (adds edd-x#R1): The writer shall cap its section.\n";

    /// A31 and the strip's grammar (the orchestrator on Opus #10156, 1):
    /// the writer's section for every entry kind and a node with
    /// requirements, written into the node file, strips back to the file's
    /// own text; rewriting it replaces it whole, and the same section
    /// leaves the same bytes.
    #[test]
    fn the_ledger_section_strips_back_for_every_kind() {
        let dir = Scratch::new("ledger-writer");
        let mut s = dir.store();
        every_kind(&mut s);
        let reqs: Vec<String> = ["R1", "R2", "R3"].map(String::from).to_vec();
        assert_eq!(s.ledger_section(&node(NODE_B), &[]).expect("none"), None);
        let section = s
            .ledger_section(&node(NODE_A), &reqs)
            .expect("section")
            .expect("a ledger");
        let lines: Vec<&str> = section.lines().collect();
        let upto = s.ledger_upto(&node(NODE_A).uuid).expect("upto");
        assert_eq!(lines[0], "# Ledger");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], format!("<!-- mdkg:ledger begin upto={upto} -->"));
        assert_eq!(lines[lines.len() - 1], "<!-- mdkg:ledger end -->");
        let short = &sha("c3")[..12];
        let want = [
            "- D1 proposed by w: Use the store".to_string(),
            format!("- F1 major verified@{short} by rev: Leaks  a lease  (src/a.rs:3)"),
            "- F2 minor residual by orch: Off by one; reason: later# Ledger x".to_string(),
            "- I1 open by w: Flaky test".to_string(),
            "- Q1 open by w: Which way?".to_string(),
            format!("- R1 met@{short} by rev"),
            "- R2 waived by human:nick: not in 0.7.1".to_string(),
            "- R3 unverified".to_string(),
            format!(
                "- V1 requested by orchestrator: review ({}..{short})",
                &sha("c1")[..12]
            ),
        ];
        assert_eq!(&lines[3..lines.len() - 1], &want[..], "{section}");
        assert_eq!(section.matches("<!--").count(), 2, "{section}");
        assert_eq!(section.matches("-->").count(), 2, "{section}");
        for file in [
            LEDGER_NODE.to_string(),
            LEDGER_NODE.replace('\n', "\r\n"),
            "---\nid: x\n---\n".to_string(),
            String::new(),
        ] {
            let written = with_ledger_section(file.as_bytes(), &section).expect("written");
            // In the file's own line ending (Terra's #11075, minor 2).
            let ending = if file.starts_with("---\r\n") {
                section.replace('\n', "\r\n")
            } else {
                section.clone()
            };
            assert!(written.ends_with(ending.as_bytes()), "{file:?}");
            assert_eq!(
                node_revision_text(&written),
                node_revision_text(file.as_bytes()),
                "{file:?}"
            );
            assert_eq!(
                with_ledger_section(&written, &section).as_deref(),
                Some(&written[..]),
                "the same section, the same bytes: {file:?}"
            );
        }
        // A later section replaces the writer's own whole.
        let written = with_ledger_section(LEDGER_NODE.as_bytes(), &section).expect("written");
        run(
            &mut s,
            "w",
            Worker,
            json!({"op": "add", "kind": "issue", "title": "Later"}),
        )
        .expect("I2");
        let later = s
            .ledger_section(&node(NODE_A), &reqs)
            .expect("later")
            .expect("a ledger");
        assert_ne!(later, section);
        let rewritten = with_ledger_section(&written, &later).expect("rewritten");
        assert_eq!(
            rewritten,
            [LEDGER_NODE.as_bytes(), later.as_bytes()].concat()
        );
        assert_eq!(
            node_revision_text(&rewritten),
            node_revision_text(LEDGER_NODE.as_bytes())
        );
    }

    /// Terra's #11158: a file's own section can be taken out again, leaving
    /// exactly the file it was written into; a file without one has none.
    #[test]
    fn the_writers_section_comes_out_whole() {
        let section = render_ledger_section(NODE_A, 3, &[], &Default::default(), &["R1".into()]);
        for file in [LEDGER_NODE.to_string(), LEDGER_NODE.replace('\n', "\r\n")] {
            let written = with_ledger_section(file.as_bytes(), &section).expect("written");
            assert_eq!(
                without_ledger_section(&written).as_deref(),
                Some(file.as_bytes())
            );
            assert_eq!(without_ledger_section(file.as_bytes()), None);
        }
    }

    /// A file that can't take a section the strip removes again gets none:
    /// another `# Ledger` heading, an unclosed fence, no final newline, an
    /// unclosed front matter.
    #[test]
    fn a_file_that_cannot_strip_the_section_back_gets_none() {
        let section = render_ledger_section(NODE_A, 3, &[], &Default::default(), &["R1".into()]);
        assert_eq!(
            section,
            "# Ledger\n\n<!-- mdkg:ledger begin upto=3 -->\n- R1 unverified\n<!-- mdkg:ledger end -->\n"
        );
        for file in [
            "# Body\n\n# Ledger\n\nMy own notes.\n",
            "# Body\n\n```\nopen fence\n",
            "# Body\n\nText.",
            "---\nid: x\nstatus: todo\n",
        ] {
            assert_eq!(
                with_ledger_section(file.as_bytes(), &section),
                None,
                "{file:?}"
            );
        }
    }

    /// The caps (A31): each line at most 240 bytes, cut at a character
    /// boundary, and the section at most 16 KiB, ending in the `more` line;
    /// either way it still strips back.
    #[test]
    fn the_ledger_section_is_capped_and_still_strips_back() {
        let entry = |n: u64, title: &str| Entry {
            node: node(NODE_A),
            id: format!("I{n}"),
            kind: Kind::Issue,
            state: "open".into(),
            severity: None,
            slot: None,
            blocking: false,
            title: title.into(),
            text: None,
            data: Map::new(),
            opened_ms: 1,
            updated_ms: 1,
        };
        let long = "é".repeat(150);
        let entries: Vec<Entry> = (1..=400).map(|n| entry(n, &long)).collect();
        let section = render_ledger_section(NODE_A, 9, &entries, &Default::default(), &[]);
        assert!(section.len() <= LEDGER_SECTION_MAX, "{}", section.len());
        assert!(section.lines().all(|l| l.len() <= LEDGER_LINE_MAX));
        let more = section.lines().rev().nth(1).expect("more");
        let shown = section.lines().filter(|l| l.starts_with("- I")).count();
        assert_eq!(
            more,
            format!("- … {} more: mdkg entry list {NODE_A}", 400 - shown)
        );
        let written = with_ledger_section(LEDGER_NODE.as_bytes(), &section).expect("written");
        assert_eq!(
            node_revision_text(&written),
            node_revision_text(LEDGER_NODE.as_bytes())
        );
    }
}
