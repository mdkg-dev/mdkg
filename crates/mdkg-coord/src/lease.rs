//! Leases and claims, as specified in `spec/lease.qnt`.
//!
//! The coordinator is the single authority for a shard. It is sans-IO: the
//! caller passes the current time, and every lease change is also recorded
//! as an event in an outbox the caller drains into the event log.
//!
//! Semantics, all checked against the Quint model's traces:
//! - A claim on a free scope is confirmed; a claim on a held scope is
//!   recorded as a `Conflict` lease (reason `Held`), never silently queued.
//! - Heartbeats extend the deadline; a lease past its deadline can't
//!   heartbeat or commit, and the coordinator expires it.
//! - Commit is compare-and-swap on the base snapshot; a stale base ends the
//!   lease in `Conflict` (reason `CasFailed`) and leaves the scope as is.
//! - Offline (provisional) leases are reconciled on reconnect: confirmed if
//!   still in time, unheld and not stale; otherwise `Expired` or `Conflict`.
//!   The coordinator assigns the confirmed lease its id and caps its
//!   deadline at its own TTL.
//!
//! Leases come in two modes. An exclusive lease
//! overlaps no other active lease; shared leases may overlap each other, but
//! never an exclusive one. Compare-and-swap still guards every commit, so a
//! shared holder can't silently overwrite another's settled change. Which
//! paths may be shared is the caller's policy, not the coordinator's.
//!
//! Beyond the single-node model, scopes nest, and snapshots are computed
//! rather than stored: the coordinator keeps a version chain per leaf (one
//! node file), and a scope's snapshot is derived from the current content of
//! the leaves inside it (see [`crate::settlement`]). A commit inside a scope
//! changes the snapshot of every scope containing a changed leaf, so nothing
//! is ever refreshed from a worktree.

use std::collections::{BTreeMap, BTreeSet};

use mdkg_format::Digest;
use serde::{Deserialize, Serialize};

use crate::scope::Scope;
use crate::settlement::{Changes, ChangesetId, Leaf, Source, snapshot_of};

/// Milliseconds (or any monotonic unit the caller chooses).
pub type Ms = u64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(pub String);

impl std::fmt::Display for LeaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Committed,
    Released,
    Expired,
    Conflict,
}

/// How a lease shares its scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Overlaps no other active lease (the default, and the Quint model's).
    #[default]
    Exclusive,
    /// May overlap other shared leases, never an exclusive one.
    Shared,
}

impl Mode {
    /// Whether a lease in this mode can be active beside one in `other`
    /// on overlapping scopes.
    pub fn coexists_with(self, other: Mode) -> bool {
        self == Mode::Shared && other == Mode::Shared
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    /// Another confirmed lease held an overlapping scope.
    Held,
    /// A provisional lease's base no longer matched the scope's snapshot.
    Stale,
    /// Commit-time compare-and-swap failed.
    CasFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: LeaseId,
    pub owner: String,
    pub scope: Scope,
    /// Exclusive unless the claim asked to share (absent in older stores).
    #[serde(default)]
    pub mode: Mode,
    /// Snapshot of the scope when the lease was taken.
    pub base: String,
    pub deadline: Ms,
    pub status: Status,
    /// Accepted by the coordinator (false for an unreconciled offline lease).
    pub confirmed: bool,
    /// Taken while the coordinator was unreachable.
    pub offline: bool,
    pub committed_at: Option<Ms>,
    /// Snapshot of the scope after a successful commit.
    pub result: Option<String>,
    pub reason: Option<ConflictReason>,
    /// For a reconciled offline lease: the agent's provisional id.
    pub local_id: Option<LeaseId>,
    /// The changeset a successful commit recorded.
    pub changeset: Option<ChangesetId>,
    /// The node the lease holds, when its scope is a node file: the node
    /// is held under this id whatever path another worktree gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    Offline,
    UnknownLease,
    /// `register` was called for a leaf the coordinator already tracks.
    AlreadyRegistered(String),
    NotOwner,
    NotActive,
    NotConfirmed,
    AlreadyConfirmed,
    PastDeadline,
    NotDue,
    /// The claim was recorded as a `Conflict` lease with this id.
    Held(LeaseId),
    /// The commit was refused and the lease ended in `Conflict`.
    CasFailed,
    /// A commit changed a leaf outside the lease's scope; nothing changed.
    OutOfScope(String),
    /// A main-branch observation was read before a later change to this
    /// leaf; read main again.
    ObservationRaced(String),
    UnknownChangeset(ChangesetId),
    /// The changeset is already committed or abandoned.
    AlreadySettled(ChangesetId),
    /// An idempotency key was reused for a different owner or scope.
    KeyReused(String),
    /// A different provisional lease with this local id was already reconciled.
    DuplicateLease(LeaseId),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Offline => write!(f, "the coordinator is unreachable"),
            Error::UnknownLease => write!(f, "unknown lease"),
            Error::AlreadyRegistered(s) => write!(f, "leaf {s} is already registered"),
            Error::NotOwner => write!(f, "lease belongs to another actor"),
            Error::NotActive => write!(f, "lease is not active"),
            Error::NotConfirmed => write!(f, "lease is not confirmed"),
            Error::AlreadyConfirmed => write!(f, "lease is already confirmed"),
            Error::PastDeadline => write!(f, "lease deadline has passed"),
            Error::NotDue => write!(f, "lease deadline has not passed"),
            Error::Held(id) => write!(f, "scope is held; recorded conflict lease {id}"),
            Error::CasFailed => write!(f, "base snapshot is stale; lease ended in conflict"),
            Error::OutOfScope(s) => write!(f, "{s} is outside the lease's scope"),
            Error::ObservationRaced(s) => write!(
                f,
                "main was read before a later change to {s}; read main again"
            ),
            Error::UnknownChangeset(id) => write!(f, "unknown changeset {id}"),
            Error::AlreadySettled(id) => write!(f, "changeset {id} is already settled"),
            Error::KeyReused(k) => write!(f, "idempotency key {k} was used for a different claim"),
            Error::DuplicateLease(id) => write!(
                f,
                "provisional lease {id} was already reconciled differently"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// What happened, for the event log (`mdkg.lease.<verb>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Claim,
    Conflict,
    Heartbeat,
    Release,
    Expire,
    Commit,
    Confirm,
}

impl EventKind {
    pub fn event_type(self) -> &'static str {
        match self {
            EventKind::Claim => "mdkg.lease.claim",
            EventKind::Conflict => "mdkg.lease.conflict",
            EventKind::Heartbeat => "mdkg.lease.heartbeat",
            EventKind::Release => "mdkg.lease.release",
            EventKind::Expire => "mdkg.lease.expire",
            EventKind::Commit => "mdkg.lease.commit",
            EventKind::Confirm => "mdkg.lease.confirm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub lease: LeaseId,
    pub actor: String,
    pub scope: Scope,
    pub at: Ms,
}

/// A claim's idempotency key and the end of its dedup window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub key: String,
    pub valid_until: Ms,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedRequest {
    owner: String,
    scope: Scope,
    #[serde(default)]
    mode: Mode,
    outcome: Result<LeaseId, Error>,
    valid_until: Ms,
}

/// Limits on refused claims the state keeps per owner, and in all.
pub const MAX_DENIED_PER_OWNER: usize = 8;
pub const MAX_DENIED: usize = 128;

/// The coordinator's whole state. It serializes, so a backend can persist it
/// and a replica can rebuild it by replaying commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coordinator {
    pub(crate) ttl: Ms,
    /// Version chain per leaf (one node file).
    pub(crate) leaves: BTreeMap<Scope, Leaf>,
    /// Counts changes to leaves; each change gets the next generation.
    pub(crate) generation: u64,
    pub(crate) leases: BTreeMap<LeaseId, Lease>,
    pub(crate) order: Vec<LeaseId>,
    pub(crate) next: u64,
    pub(crate) changesets: BTreeMap<ChangesetId, crate::settlement::Changeset>,
    pub(crate) next_changeset: u64,
    pub(crate) requests: BTreeMap<String, RecordedRequest>,
    /// owner -> provisional id -> coordinator lease id.
    pub(crate) reconciled: BTreeMap<String, BTreeMap<LeaseId, LeaseId>>,
    /// Readable aliases issued so far.
    #[serde(default)]
    pub(crate) aliases: crate::aliases::Aliases,
    /// Temporary work states per leaf (`review`, `blocked`).
    #[serde(default)]
    pub(crate) work: BTreeMap<Scope, crate::work::WorkStatus>,
    /// `done` marks per leaf, kept apart from temporary states so clearing
    /// one doesn't lose the other.
    #[serde(default)]
    pub(crate) done: BTreeMap<Scope, crate::work::DoneMark>,
    #[serde(skip)]
    pub(crate) outbox: Vec<Event>,
}

pub const COORDINATOR: &str = "coordinator";

impl Coordinator {
    /// A coordinator granting leases of `ttl` per claim or heartbeat.
    pub fn new(ttl: Ms) -> Self {
        Self {
            ttl,
            leaves: BTreeMap::new(),
            generation: 0,
            leases: BTreeMap::new(),
            order: Vec::new(),
            next: 0,
            changesets: BTreeMap::new(),
            next_changeset: 1,
            requests: BTreeMap::new(),
            reconciled: BTreeMap::new(),
            aliases: crate::aliases::Aliases::new(),
            work: BTreeMap::new(),
            done: BTreeMap::new(),
            outbox: Vec::new(),
        }
    }

    pub fn ttl(&self) -> Ms {
        self.ttl
    }

    /// Registers a leaf with its content hash, as already settled on main.
    pub fn register(&mut self, leaf: Scope, content: impl Into<String>) -> Result<(), Error> {
        if self.leaves.contains_key(&leaf) {
            return Err(Error::AlreadyRegistered(leaf.to_string()));
        }
        self.generation += 1;
        self.leaves
            .insert(leaf, Leaf::settled(content.into(), self.generation));
        Ok(())
    }

    /// The change generation. Read it *before* reading main, and pass it in
    /// the [`crate::settlement::Observation`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The scope's current snapshot: a lone leaf's content, or the Merkle
    /// root of the leaves inside the scope.
    pub fn snapshot(&self, scope: &Scope) -> String {
        snapshot_of(self.current(), scope)
    }

    fn current(&self) -> impl Iterator<Item = (&Scope, &str)> {
        self.leaves
            .iter()
            .filter_map(|(s, l)| l.current().map(|c| (s, c)))
    }

    /// Every live leaf's current content.
    pub fn leaves(&self) -> BTreeMap<Scope, String> {
        self.current()
            .map(|(s, c)| (s.clone(), c.to_string()))
            .collect()
    }

    pub fn leaf(&self, leaf: &Scope) -> Option<&Leaf> {
        self.leaves.get(leaf)
    }

    pub fn lease(&self, id: &LeaseId) -> Option<&Lease> {
        self.leases.get(id)
    }

    /// Every recorded temporary work state.
    pub fn work_states(&self) -> impl Iterator<Item = (&Scope, &crate::work::WorkStatus)> {
        self.work.iter()
    }

    /// Every lease, in the order the coordinator recorded them.
    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.order.iter().filter_map(|id| self.leases.get(id))
    }

    /// Confirmed, active leases overlapping `scope`.
    pub fn holders<'a>(&'a self, scope: &'a Scope) -> impl Iterator<Item = &'a Lease> + 'a {
        self.leases
            .values()
            .filter(move |l| l.confirmed && l.status == Status::Active && l.scope.overlaps(scope))
    }

    /// Holders overlapping `scope` that a new lease in `mode` can't be
    /// active beside: every holder for an exclusive claim, and exclusive
    /// holders for a shared one.
    pub fn blockers<'a>(
        &'a self,
        scope: &'a Scope,
        mode: Mode,
    ) -> impl Iterator<Item = &'a Lease> + 'a {
        self.holders(scope)
            .filter(move |l| !mode.coexists_with(l.mode))
    }

    /// Takes the events recorded since the last drain.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.outbox)
    }

    fn emit(&mut self, kind: EventKind, lease: &Lease, actor: &str, at: Ms) {
        self.outbox.push(Event {
            kind,
            lease: lease.id.clone(),
            actor: actor.to_string(),
            scope: lease.scope.clone(),
            at,
        });
    }

    /// Makes room for one more refused claim: the state keeps only the
    /// newest few per owner, and a bounded number in all, so refusals
    /// can't grow the snapshot every command rewrites without bound. The
    /// journal and events keep the full record.
    fn prune_denied(&mut self, owner: &str) {
        // Only plain refused claims: never a reconciled offline lease (its
        // provisional id must keep resolving), nor one an idempotent
        // request's recorded outcome names.
        let named: BTreeSet<LeaseId> = self
            .requests
            .values()
            .filter_map(|r| match &r.outcome {
                Ok(id) | Err(Error::Held(id)) => Some(id.clone()),
                Err(_) => None,
            })
            .collect();
        let denied = |l: &Lease| {
            !l.confirmed
                && l.status == Status::Conflict
                && l.reason == Some(ConflictReason::Held)
                && l.local_id.is_none()
                && !named.contains(&l.id)
        };
        let mine: Vec<LeaseId> = self
            .leases()
            .filter(|l| denied(l) && l.owner == owner)
            .map(|l| l.id.clone())
            .collect();
        let all: Vec<LeaseId> = self
            .leases()
            .filter(|l| denied(l))
            .map(|l| l.id.clone())
            .collect();
        let mut drop: BTreeSet<LeaseId> = BTreeSet::new();
        drop.extend(
            mine.iter()
                .take((mine.len() + 1).saturating_sub(MAX_DENIED_PER_OWNER))
                .cloned(),
        );
        drop.extend(
            all.iter()
                .take((all.len() + 1).saturating_sub(MAX_DENIED))
                .cloned(),
        );
        if drop.is_empty() {
            return;
        }
        self.order.retain(|id| !drop.contains(id));
        self.leases.retain(|id, _| !drop.contains(id));
    }

    fn insert(&mut self, lease: Lease) {
        self.order.push(lease.id.clone());
        self.leases.insert(lease.id.clone(), lease);
    }

    fn fresh_id(&mut self) -> LeaseId {
        let id = LeaseId(format!("lease-{}", self.next));
        self.next += 1;
        id
    }

    fn live(&mut self, owner: &str, id: &LeaseId) -> Result<&mut Lease, Error> {
        let lease = self.leases.get_mut(id).ok_or(Error::UnknownLease)?;
        if lease.owner != owner {
            return Err(Error::NotOwner);
        }
        if !lease.confirmed {
            return Err(Error::NotConfirmed);
        }
        if lease.status != Status::Active {
            return Err(Error::NotActive);
        }
        Ok(lease)
    }

    /// Claims `scope` exclusively for `owner`; see [`Self::claim_mode`].
    pub fn claim(
        &mut self,
        owner: &str,
        scope: &Scope,
        now: Ms,
        request: Option<Request>,
    ) -> Result<LeaseId, Error> {
        self.claim_mode(owner, scope, Mode::Exclusive, now, request)
    }

    /// Claims `scope` for `owner` in `mode`. With a request, a repeat of the
    /// same key by the same owner for the same scope and mode inside its
    /// window returns the original outcome (idempotency); any other reuse of
    /// the key is refused.
    pub fn claim_mode(
        &mut self,
        owner: &str,
        scope: &Scope,
        mode: Mode,
        now: Ms,
        request: Option<Request>,
    ) -> Result<LeaseId, Error> {
        self.claim_node(owner, scope, mode, None, now, request)
    }

    /// The active lease, if any, holding the node `qid` (ids compare in any
    /// letter case).
    pub fn node_holder(&self, qid: &str) -> Option<&Lease> {
        self.leases.values().find(|l| {
            l.confirmed
                && l.status == Status::Active
                && l.node
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(qid))
        })
    }

    /// A claim of `scope`, which holds the node `node` when given: refused
    /// when anything overlapping the scope blocks it, or when the node is
    /// held under another path (a file renamed in one worktree).
    pub fn claim_node(
        &mut self,
        owner: &str,
        scope: &Scope,
        mode: Mode,
        node: Option<&str>,
        now: Ms,
        request: Option<Request>,
    ) -> Result<LeaseId, Error> {
        if let Some(req) = &request
            && let Some(recorded) = self.requests.get(&req.key)
            && now <= recorded.valid_until
        {
            if recorded.owner != owner || recorded.scope != *scope || recorded.mode != mode {
                return Err(Error::KeyReused(req.key.clone()));
            }
            return recorded.outcome.clone();
        }
        let base = self.snapshot(scope);
        let id = self.fresh_id();
        let held = self.blockers(scope, mode).next().is_some()
            || node.is_some_and(|qid| self.node_holder(qid).is_some_and(|l| l.scope != *scope));
        let lease = Lease {
            id: id.clone(),
            owner: owner.to_string(),
            scope: scope.clone(),
            mode,
            base,
            deadline: now.saturating_add(self.ttl),
            status: if held {
                Status::Conflict
            } else {
                Status::Active
            },
            confirmed: !held,
            offline: false,
            committed_at: None,
            result: None,
            reason: held.then_some(ConflictReason::Held),
            local_id: None,
            changeset: None,
            node: node.map(str::to_string),
        };
        let kind = if held {
            EventKind::Conflict
        } else {
            EventKind::Claim
        };
        self.emit(kind, &lease, owner, now);
        if held {
            self.prune_denied(owner);
        }
        self.insert(lease);
        let outcome = if held { Err(Error::Held(id)) } else { Ok(id) };
        if let Some(req) = request {
            self.requests.insert(
                req.key,
                RecordedRequest {
                    owner: owner.to_string(),
                    scope: scope.clone(),
                    mode,
                    outcome: outcome.clone(),
                    valid_until: req.valid_until,
                },
            );
        }
        outcome
    }

    /// Extends a live lease's deadline.
    pub fn heartbeat(&mut self, owner: &str, id: &LeaseId, now: Ms) -> Result<Ms, Error> {
        let ttl = self.ttl;
        let lease = self.live(owner, id)?;
        if now >= lease.deadline {
            return Err(Error::PastDeadline);
        }
        lease.deadline = now.saturating_add(ttl);
        let (deadline, snapshot) = (lease.deadline, lease.clone());
        self.emit(EventKind::Heartbeat, &snapshot, owner, now);
        Ok(deadline)
    }

    /// Ends a live lease without committing.
    pub fn release(&mut self, owner: &str, id: &LeaseId, now: Ms) -> Result<(), Error> {
        let lease = self.live(owner, id)?;
        lease.status = Status::Released;
        let snapshot = lease.clone();
        self.emit(EventKind::Release, &snapshot, owner, now);
        Ok(())
    }

    /// Expires one confirmed lease whose deadline has passed.
    pub fn expire(&mut self, id: &LeaseId, now: Ms) -> Result<(), Error> {
        let lease = self.leases.get_mut(id).ok_or(Error::UnknownLease)?;
        if !lease.confirmed || lease.status != Status::Active {
            return Err(Error::NotActive);
        }
        if now < lease.deadline {
            return Err(Error::NotDue);
        }
        lease.status = Status::Expired;
        let snapshot = lease.clone();
        self.emit(EventKind::Expire, &snapshot, COORDINATOR, now);
        Ok(())
    }

    /// Expires every lease that is due; returns their ids.
    pub fn expire_due(&mut self, now: Ms) -> Vec<LeaseId> {
        let due: Vec<LeaseId> = self
            .leases()
            .filter(|l| l.confirmed && l.status == Status::Active && now >= l.deadline)
            .map(|l| l.id.clone())
            .collect();
        for id in &due {
            self.expire(id, now).ok();
        }
        due
    }

    /// Commits a live lease: compare-and-swap of the scope's snapshot from
    /// the lease's base, applying `changes` (leaf -> new content, `None` to
    /// delete) as one changeset. `source` names the git commit holding the
    /// changes, so other worktrees can check them out.
    pub fn commit(
        &mut self,
        owner: &str,
        id: &LeaseId,
        changes: &Changes,
        source: Option<Source>,
        now: Ms,
    ) -> Result<ChangesetId, Error> {
        self.commit_landed(owner, id, changes, source, now, false)
    }

    /// [`Self::commit`], where `landed` says the caller checked that main
    /// already holds the changes and nothing of main's is lost to them (the
    /// holder committed on main itself, or its branch merged). Then a
    /// stale base passes when applying the changes inside the scope would
    /// change nothing: main's content there is already the holder's. Anything
    /// else, or `landed` false, is the compare-and-swap as before.
    pub fn commit_landed(
        &mut self,
        owner: &str,
        id: &LeaseId,
        changes: &Changes,
        source: Option<Source>,
        now: Ms,
        landed: bool,
    ) -> Result<ChangesetId, Error> {
        let lease = self.live(owner, id)?;
        if now >= lease.deadline {
            return Err(Error::PastDeadline);
        }
        let scope = lease.scope.clone();
        let base = lease.base.clone();
        // A change outside the scope is allowed only when it creates a leaf
        // that doesn't exist yet: its path is unique by its issued alias.
        let creates = |leaf: &Scope, content: &Option<String>| {
            content.is_some() && self.leaves.get(leaf).is_none_or(|l| l.current().is_none())
        };
        if let Some((outside, _)) = changes
            .iter()
            .find(|(leaf, content)| !scope.contains(leaf) && !creates(leaf, content))
        {
            return Err(Error::OutOfScope(outside.to_string()));
        }
        let current = self.snapshot(&scope);
        let inside: Vec<_> = changes.iter().filter(|(l, _)| scope.contains(l)).collect();
        // The holder names the scope's whole content (every live leaf in
        // it), and it must be main's already.
        let named = |leaf: &Scope| inside.iter().any(|(l, _)| *l == leaf);
        let already = landed
            && !inside.is_empty()
            && inside.iter().all(|(leaf, content)| {
                self.leaves.get(*leaf).and_then(|l| l.current()) == content.as_deref()
            })
            && self
                .leaves
                .iter()
                .filter(|(leaf, l)| scope.contains(leaf) && l.current().is_some())
                .all(|(leaf, _)| named(leaf));
        if current != base && !already {
            let lease = self.leases.get_mut(id).ok_or(Error::UnknownLease)?;
            lease.status = Status::Conflict;
            lease.reason = Some(ConflictReason::CasFailed);
            let snapshot = lease.clone();
            self.emit(EventKind::Conflict, &snapshot, owner, now);
            return Err(Error::CasFailed);
        }
        let changeset = self.record_changeset(id, owner, &scope, changes, source, now);
        let result = self.snapshot(&scope);
        let lease = self.leases.get_mut(id).ok_or(Error::UnknownLease)?;
        lease.status = Status::Committed;
        lease.committed_at = Some(now);
        lease.result = Some(result);
        lease.changeset = Some(changeset);
        let snapshot = lease.clone();
        self.emit(EventKind::Commit, &snapshot, owner, now);
        Ok(changeset)
    }

    /// Reconciles a provisional (offline) lease presented on reconnect and
    /// returns the coordinator's id for it. Presenting the same provisional
    /// lease again returns the original outcome.
    pub fn reconcile(&mut self, mut lease: Lease, now: Ms) -> Result<(LeaseId, Status), Error> {
        if let Some(existing) = self
            .reconciled
            .get(&lease.owner)
            .and_then(|by_local| by_local.get(&lease.id))
            .cloned()
        {
            let recorded = self.leases.get(&existing).ok_or(Error::UnknownLease)?;
            if recorded.scope != lease.scope || recorded.base != lease.base {
                return Err(Error::DuplicateLease(lease.id));
            }
            return Ok((existing, recorded.status));
        }
        if lease.confirmed {
            return Err(Error::AlreadyConfirmed);
        }
        if lease.status != Status::Active {
            return Err(Error::NotActive);
        }
        let local_id = lease.id.clone();
        lease.id = self.fresh_id();
        lease.local_id = Some(local_id.clone());
        let owner = lease.owner.clone();
        if now >= lease.deadline {
            lease.status = Status::Expired;
            self.emit(EventKind::Expire, &lease, COORDINATOR, now);
        } else if self.blockers(&lease.scope, lease.mode).next().is_some() {
            lease.status = Status::Conflict;
            lease.reason = Some(ConflictReason::Held);
            self.emit(EventKind::Conflict, &lease, COORDINATOR, now);
        } else if self.snapshot(&lease.scope) != lease.base {
            lease.status = Status::Conflict;
            lease.reason = Some(ConflictReason::Stale);
            self.emit(EventKind::Conflict, &lease, COORDINATOR, now);
        } else {
            // The coordinator, not the agent, decides how long a lease lasts.
            lease.deadline = lease.deadline.min(now.saturating_add(self.ttl));
            lease.confirmed = true;
            self.emit(EventKind::Confirm, &lease, &owner, now);
        }
        let status = lease.status;
        let id = lease.id.clone();
        self.reconciled
            .entry(owner)
            .or_default()
            .insert(local_id, id.clone());
        self.insert(lease);
        Ok((id, status))
    }

    /// Drops idempotency records whose windows have closed (invariant 15).
    pub fn prune_requests(&mut self, now: Ms) -> usize {
        let before = self.requests.len();
        self.requests.retain(|_, r| r.valid_until >= now);
        before - self.requests.len()
    }
}

/// The agent side: online state, the leaves it last synced, and the
/// provisional leases it took while offline.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    online: bool,
    seen: BTreeMap<Scope, String>,
    provisional: BTreeMap<LeaseId, Lease>,
    next_local: u64,
}

impl Agent {
    pub fn new(name: &str, coord: &Coordinator) -> Self {
        Self {
            name: name.to_string(),
            online: true,
            seen: coord.leaves(),
            provisional: BTreeMap::new(),
            next_local: 0,
        }
    }

    pub fn online(&self) -> bool {
        self.online
    }

    pub fn provisional(&self) -> impl Iterator<Item = &Lease> {
        self.provisional.values()
    }

    fn require_online(&self) -> Result<(), Error> {
        if self.online {
            Ok(())
        } else {
            Err(Error::Offline)
        }
    }

    pub fn disconnect(&mut self, coord: &Coordinator) {
        self.seen = coord.leaves();
        self.online = false;
    }

    pub fn reconnect(&mut self) {
        self.online = true;
    }

    pub fn claim(
        &mut self,
        coord: &mut Coordinator,
        scope: &Scope,
        now: Ms,
    ) -> Result<LeaseId, Error> {
        self.require_online()?;
        coord.claim(&self.name, scope, now, None)
    }

    /// A provisional lease, based on the leaves seen before disconnecting.
    /// Its id is content-derived, so it is unique even across agent restarts.
    pub fn claim_offline(&mut self, scope: &Scope, now: Ms, ttl: Ms) -> Result<LeaseId, Error> {
        if self.online {
            return Err(Error::AlreadyConfirmed);
        }
        let base = snapshot_of(self.seen.iter().map(|(s, c)| (s, c.as_str())), scope);
        let seed = format!("{}\n{scope}\n{base}\n{now}\n{}", self.name, self.next_local);
        let digest = Digest::of_bytes(seed.as_bytes());
        let id = LeaseId(format!("lease-~{}", &digest.hex()[..16]));
        self.next_local += 1;
        self.provisional.insert(
            id.clone(),
            Lease {
                id: id.clone(),
                owner: self.name.clone(),
                scope: scope.clone(),
                mode: Mode::Exclusive,
                base,
                deadline: now.saturating_add(ttl),
                status: Status::Active,
                confirmed: false,
                offline: true,
                committed_at: None,
                result: None,
                reason: None,
                local_id: None,
                changeset: None,
                node: None,
            },
        );
        Ok(id)
    }

    pub fn heartbeat(
        &mut self,
        coord: &mut Coordinator,
        id: &LeaseId,
        now: Ms,
    ) -> Result<Ms, Error> {
        self.require_online()?;
        coord.heartbeat(&self.name, id, now)
    }

    /// Releases a lease: confirmed leases through the coordinator (online
    /// only); provisional ones locally, online or not.
    pub fn release(&mut self, coord: &mut Coordinator, id: &LeaseId, now: Ms) -> Result<(), Error> {
        if let Some(lease) = self.provisional.get_mut(id) {
            if lease.status != Status::Active {
                return Err(Error::NotActive);
            }
            lease.status = Status::Released;
            return Ok(());
        }
        self.require_online()?;
        coord.release(&self.name, id, now)
    }

    pub fn commit(
        &mut self,
        coord: &mut Coordinator,
        id: &LeaseId,
        changes: &Changes,
        source: Option<Source>,
        now: Ms,
    ) -> Result<ChangesetId, Error> {
        self.require_online()?;
        coord.commit(&self.name, id, changes, source, now)
    }

    /// Presents a provisional lease to the coordinator; returns the
    /// coordinator's id for it and the outcome.
    pub fn reconcile(
        &mut self,
        coord: &mut Coordinator,
        id: &LeaseId,
        now: Ms,
    ) -> Result<(LeaseId, Status), Error> {
        self.require_online()?;
        let lease = self
            .provisional
            .get(id)
            .cloned()
            .ok_or(Error::UnknownLease)?;
        if lease.status != Status::Active {
            return Err(Error::NotActive);
        }
        let outcome = coord.reconcile(lease, now)?;
        self.provisional.remove(id);
        Ok(outcome)
    }
}
