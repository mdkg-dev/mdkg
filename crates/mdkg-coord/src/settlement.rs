//! Settlement through git history on the main branch.
//!
//! The coordinator orders in-flight work; the main branch settles it.
//!
//! - Every **leaf** (one node file) has a version chain of [`Entry`]s. Its
//!   current content is the last entry that isn't abandoned.
//! - A successful lease commit records a [`Changeset`]: the leaves it
//!   changed, the git commit and branch holding them, and the pending
//!   changesets it built on. Its entries start `Pending` (finality
//!   `sequenced`).
//! - An [`Observation`] of main settles a changeset (finality `committed`)
//!   when its commit is in main's history. For squash and rebase merges,
//!   which leave no history, it also settles entries whose content main now
//!   has, but only once main has changed to that content: if a change is
//!   undone on another branch (A to B to A) while main still holds A,
//!   nothing settles. Content main has that the
//!   coordinator never sequenced is an external edit: it becomes the leaf's
//!   current content, and pending entries it overrides become `Conflicted`.
//!   **Main wins.**
//! - [`Coordinator::abandon`] drops a changeset whose branch won't merge,
//!   with every changeset built on it, and rolls their leaves back.
//!
//!
//! Observations should be read and applied in one step; the local store does
//! both inside its write transaction. As defense in depth, an observation
//! read before a later change to a leaf it would override is refused
//! (`ObservationRaced`, the guard from the review's racy-refresh finding),
//! and conflicted work whose exact content later appears on main settles, so
//! a stale observation can't leave work wrongly conflicted for good.

use std::collections::{BTreeMap, BTreeSet};

use mdkg_format::Digest;
use mdkg_log::merkle::MerkleTree;
use serde::{Deserialize, Serialize};

use crate::lease::{Coordinator, Error, LeaseId, Ms};
use crate::scope::Scope;
use crate::sequencer::Finality;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangesetId(pub u64);

impl std::fmt::Display for ChangesetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cs-{}", self.0)
    }
}

/// Leaf -> new content hash, or `None` to delete the leaf.
pub type Changes = BTreeMap<Scope, Option<String>>;

/// Where a changeset's content lives in git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub commit: String,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Content the coordinator started from (registered, or absent).
    Base,
    Changeset(ChangesetId),
    /// Content found on main that no changeset produced.
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    /// On main.
    Committed,
    /// Sequenced by the coordinator, not yet on main.
    Pending,
    /// Overridden by different content on main.
    Conflicted,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub content: Option<String>,
    pub origin: Origin,
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    pub entries: Vec<Entry>,
    /// The generation of the last change to this leaf.
    pub touched: u64,
}

impl Leaf {
    pub(crate) fn settled(content: String, generation: u64) -> Self {
        Self {
            entries: vec![Entry {
                content: Some(content),
                origin: Origin::Base,
                state: EntryState::Committed,
            }],
            touched: generation,
        }
    }

    /// A leaf first created by a changeset: it was absent on main.
    fn absent() -> Self {
        Self {
            entries: vec![Entry {
                content: None,
                origin: Origin::Base,
                state: EntryState::Committed,
            }],
            touched: 0,
        }
    }

    /// The last entry that isn't abandoned.
    pub fn live(&self) -> Option<&Entry> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.state != EntryState::Abandoned)
    }

    pub fn current(&self) -> Option<&str> {
        self.live().and_then(|e| e.content.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    Sequenced,
    Committed,
    Conflicted,
    Abandoned,
}

impl Settlement {
    /// The finality level for changesets still on the way to main or on it.
    pub fn finality(self) -> Option<Finality> {
        match self {
            Settlement::Sequenced => Some(Finality::Sequenced),
            Settlement::Committed => Some(Finality::Committed),
            Settlement::Conflicted | Settlement::Abandoned => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub id: ChangesetId,
    pub lease: LeaseId,
    pub owner: String,
    pub scope: Scope,
    pub changes: Changes,
    pub source: Option<Source>,
    /// Pending changesets whose content was in the lease's scope when it
    /// committed. They must merge first, and abandoning one abandons this.
    pub depends_on: BTreeSet<ChangesetId>,
    pub state: Settlement,
    pub at: Ms,
}

/// What main holds, read after reading [`Coordinator::generation`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub read_generation: u64,
    /// Every leaf on main under `prefix` with its content hash. Leaves under
    /// `prefix` missing here are absent on main.
    pub leaves: BTreeMap<Scope, String>,
    /// Commits of open changesets that are in main's history.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub merged: BTreeSet<String>,
    /// The part of the repository that was read; known leaves outside it
    /// are left alone. `None` means everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<Scope>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveReport {
    /// Changesets that reached main.
    pub committed: Vec<ChangesetId>,
    /// Changesets main overrode with different content.
    pub conflicted: Vec<ChangesetId>,
    /// Leaves whose main content no changeset produced.
    pub external: Vec<Scope>,
}

/// What a worktree should hold for one leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    pub leaf: Scope,
    pub content: Option<String>,
    pub state: EntryState,
    /// For content not yet on main: the commit to check it out from.
    pub source: Option<Source>,
}

fn as_digest(content: &str) -> Digest {
    Digest::parse(content).unwrap_or_else(|| Digest::of_bytes(content.as_bytes()))
}

/// A scope's snapshot from leaf contents: a lone leaf equal to the scope is
/// its own content; otherwise the Merkle root of the leaves inside the scope.
pub(crate) fn snapshot_of<'a>(
    leaves: impl Iterator<Item = (&'a Scope, &'a str)>,
    scope: &Scope,
) -> String {
    let inside: Vec<(&Scope, &str)> = leaves.filter(|(leaf, _)| scope.contains(leaf)).collect();
    if let [(leaf, content)] = inside.as_slice()
        && *leaf == scope
    {
        return (*content).to_string();
    }
    MerkleTree::build(
        inside
            .iter()
            .map(|(leaf, content)| (leaf.to_string(), as_digest(content)))
            .collect(),
    )
    .root()
    .to_string()
}

enum Action {
    /// Main has a pending version's content: settle these entries (that
    /// version and what it was built on), and conflict the pending ones in
    /// between it wasn't built on: main moved past them.
    Settle {
        settle: Vec<usize>,
        conflict: Vec<usize>,
    },
    /// Main has content the chain can't explain.
    External(Option<String>),
}

impl Coordinator {
    /// `id` and everything it was built on (its `depends_on`, transitively).
    fn lineage(&self, id: ChangesetId) -> BTreeSet<ChangesetId> {
        let mut out = BTreeSet::new();
        let mut stack = vec![id];
        while let Some(c) = stack.pop() {
            if out.insert(c)
                && let Some(cs) = self.changesets.get(&c)
            {
                stack.extend(cs.depends_on.iter().copied());
            }
        }
        out
    }
}

impl Coordinator {
    pub(crate) fn record_changeset(
        &mut self,
        lease: &LeaseId,
        owner: &str,
        scope: &Scope,
        changes: &Changes,
        source: Option<Source>,
        now: Ms,
    ) -> ChangesetId {
        let id = ChangesetId(self.next_changeset);
        self.next_changeset += 1;
        let depends_on = self
            .leaves
            .iter()
            .filter(|(leaf, _)| scope.contains(leaf))
            .filter_map(|(_, leaf)| leaf.live())
            .filter(|e| e.state == EntryState::Pending)
            .filter_map(|e| match e.origin {
                Origin::Changeset(c) => Some(c),
                _ => None,
            })
            .collect();
        if !changes.is_empty() {
            self.generation += 1;
        }
        for (name, content) in changes {
            let leaf = self.leaves.entry(name.clone()).or_insert_with(Leaf::absent);
            leaf.entries.push(Entry {
                content: content.clone(),
                origin: Origin::Changeset(id),
                state: EntryState::Pending,
            });
            leaf.touched = self.generation;
        }
        let state = if changes.is_empty() {
            Settlement::Committed
        } else {
            Settlement::Sequenced
        };
        self.changesets.insert(
            id,
            Changeset {
                id,
                lease: lease.clone(),
                owner: owner.to_string(),
                scope: scope.clone(),
                changes: changes.clone(),
                source,
                depends_on,
                state,
                at: now,
            },
        );
        id
    }

    pub fn changeset(&self, id: ChangesetId) -> Option<&Changeset> {
        self.changesets.get(&id)
    }

    pub fn changesets(&self) -> impl Iterator<Item = &Changeset> {
        self.changesets.values()
    }

    /// Changesets not yet on main, in the order they must merge.
    pub fn settlement_queue(&self) -> Vec<&Changeset> {
        self.changesets
            .values()
            .filter(|c| matches!(c.state, Settlement::Sequenced | Settlement::Conflicted))
            .collect()
    }

    /// What a worktree should hold for every leaf inside `scope`.
    pub fn expected(&self, scope: &Scope) -> Vec<Expected> {
        self.leaves
            .iter()
            .filter(|(leaf, _)| scope.contains(leaf))
            .filter_map(|(name, leaf)| {
                let live = leaf.live()?;
                let source = match live.origin {
                    Origin::Changeset(c) if live.state != EntryState::Committed => {
                        self.changesets.get(&c).and_then(|cs| cs.source.clone())
                    }
                    _ => None,
                };
                Some(Expected {
                    leaf: name.clone(),
                    content: live.content.clone(),
                    state: live.state,
                    source,
                })
            })
            .collect()
    }

    /// Applies what main holds: settles changesets main now contains and
    /// records external edits. Refused, with nothing applied, if an external
    /// edit would land on a leaf changed after the observation was read.
    pub fn observe_main(&mut self, obs: &Observation) -> Result<ObserveReport, Error> {
        let names: BTreeSet<Scope> = self
            .leaves
            .keys()
            .filter(|l| obs.prefix.as_ref().is_none_or(|p| p.contains(l)))
            .chain(obs.leaves.keys())
            .cloned()
            .collect();
        // First, history: entries whose changeset commit main has merged
        // are on main, whatever main's content is now.
        // This needs no content, so it covers every leaf, inside the
        // observed prefix or not (a claimed code file, say).
        // A version merged by history also lands, in the same file, the
        // versions it was built on (its `depends_on`, transitively), even when
        // its branch had them by content rather than by merging. Only
        // versions since the file's last settled one count:
        // anything main overrode before stays conflicted. Other files those
        // changesets changed aren't on main through it, so they wait.
        let open = |e: &Entry| matches!(e.state, EntryState::Pending | EntryState::Conflicted);
        let origin = |e: &Entry| match e.origin {
            Origin::Changeset(c) => Some(c),
            _ => None,
        };
        let direct: BTreeSet<ChangesetId> = self
            .changesets
            .values()
            .filter(|c| matches!(c.state, Settlement::Sequenced | Settlement::Conflicted))
            .filter(|c| {
                c.source
                    .as_ref()
                    .is_some_and(|s| obs.merged.contains(&s.commit))
            })
            .map(|c| c.id)
            .collect();
        let mut by_history: BTreeSet<(Scope, usize)> = BTreeSet::new();
        // Pending versions a merged one passed over without building on them:
        // main moved past them, so they conflict.
        let mut passed_over: BTreeSet<(Scope, usize)> = BTreeSet::new();
        for (name, leaf) in &self.leaves {
            for (j, e) in leaf.entries.iter().enumerate() {
                let Some(d) = origin(e).filter(|d| open(e) && direct.contains(d)) else {
                    continue;
                };
                let line = self.lineage(d);
                let from = leaf.entries[..j]
                    .iter()
                    .rposition(|e| e.state == EntryState::Committed)
                    .map_or(0, |c| c + 1);
                for (i, earlier) in leaf.entries.iter().enumerate().take(j + 1).skip(from) {
                    if open(earlier) && origin(earlier).is_some_and(|c| line.contains(&c)) {
                        by_history.insert((name.clone(), i));
                    } else if earlier.state == EntryState::Pending {
                        passed_over.insert((name.clone(), i));
                    }
                }
            }
        }
        let passed_over: BTreeSet<(Scope, usize)> =
            passed_over.difference(&by_history).cloned().collect();
        let mut plan = Vec::new();
        for name in names {
            let main = obs.leaves.get(&name).map(String::as_str);
            let Some(leaf) = self.leaves.get(&name) else {
                if let Some(content) = main {
                    plan.push((name, Action::External(Some(content.to_string()))));
                }
                continue;
            };
            // The chain as it will be once history has settled it.
            let mut entries = leaf.entries.clone();
            for (_, i) in by_history.iter().filter(|(n, _)| *n == name) {
                entries[*i].state = EntryState::Committed;
            }
            for (_, i) in passed_over.iter().filter(|(n, _)| *n == name) {
                entries[*i].state = EntryState::Conflicted;
            }
            let current = entries
                .iter()
                .rev()
                .find(|e| e.state != EntryState::Abandoned)
                .and_then(|e| e.content.as_deref());
            // The earliest version since the last settled one that holds
            // main's content. If that's the settled version itself, main
            // hasn't moved and content proves nothing (A to B to A). If it's
            // pending work, main moved to it: a squash or rebase merge
            // settles it and what it built on, but not later versions that
            // happen to share its content.
            let settled = entries
                .iter()
                .rposition(|e| e.state == EntryState::Committed);
            let earliest = (settled.unwrap_or(0)..entries.len()).find(|&i| {
                entries[i].state != EntryState::Abandoned && entries[i].content.as_deref() == main
            });
            match earliest {
                Some(k) if Some(k) == settled => {}
                Some(k) => {
                    // That version and what it was built on, since the last
                    // settled one.
                    let line = origin(&entries[k])
                        .map(|c| self.lineage(c))
                        .unwrap_or_default();
                    let from = settled.map_or(0, |c| c + 1);
                    let settle: Vec<usize> = (from..=k)
                        .filter(|&i| {
                            entries[i].state == EntryState::Pending
                                && (i == k
                                    || origin(&entries[i]).is_some_and(|c| line.contains(&c)))
                        })
                        .collect();
                    let conflict: Vec<usize> = (from..k)
                        .filter(|i| entries[*i].state == EntryState::Pending && !settle.contains(i))
                        .collect();
                    if !settle.is_empty() {
                        plan.push((name, Action::Settle { settle, conflict }));
                    }
                }
                None => {
                    if current != main {
                        // Unknown content, or a revert below settled content.
                        plan.push((name, Action::External(main.map(String::from))));
                    }
                }
            }
        }
        for (name, action) in &plan {
            if let Action::External(_) = action
                && self
                    .leaves
                    .get(name)
                    .is_some_and(|l| l.touched > obs.read_generation)
            {
                return Err(Error::ObservationRaced(name.to_string()));
            }
        }
        let mut report = ObserveReport::default();
        if plan.is_empty() && by_history.is_empty() && passed_over.is_empty() {
            return Ok(report);
        }
        self.generation += 1;
        let generation = self.generation;
        let mut affected = BTreeSet::new();
        let marks = by_history
            .into_iter()
            .map(|at| (at, EntryState::Committed))
            .chain(
                passed_over
                    .into_iter()
                    .map(|at| (at, EntryState::Conflicted)),
            );
        for ((name, i), state) in marks {
            if let Some(leaf) = self.leaves.get_mut(&name) {
                let e = &mut leaf.entries[i];
                e.state = state;
                if let Origin::Changeset(c) = e.origin {
                    affected.insert(c);
                }
                leaf.touched = generation;
            }
        }
        for (name, action) in plan {
            let leaf = self.leaves.entry(name.clone()).or_insert_with(|| Leaf {
                entries: Vec::new(),
                touched: 0,
            });
            match action {
                Action::Settle { settle, conflict } => {
                    let marks = settle
                        .into_iter()
                        .map(|i| (i, EntryState::Committed))
                        .chain(conflict.into_iter().map(|i| (i, EntryState::Conflicted)));
                    for (i, state) in marks {
                        let e = &mut leaf.entries[i];
                        if e.state == EntryState::Pending {
                            e.state = state;
                            if let Origin::Changeset(c) = e.origin {
                                affected.insert(c);
                            }
                        }
                    }
                }
                Action::External(content) => {
                    for e in &mut leaf.entries {
                        let next = match e.state {
                            EntryState::Pending => EntryState::Conflicted,
                            // Conflicted work whose exact content reached
                            // main after all (say, after a stale read) settles.
                            EntryState::Conflicted if e.content == content => EntryState::Committed,
                            _ => continue,
                        };
                        e.state = next;
                        if let Origin::Changeset(c) = e.origin {
                            affected.insert(c);
                        }
                    }
                    leaf.entries.push(Entry {
                        content,
                        origin: Origin::Main,
                        state: EntryState::Committed,
                    });
                    report.external.push(name);
                }
            }
            leaf.touched = generation;
        }
        for id in affected {
            let states: Vec<EntryState> = self.changeset_entries(id);
            let Some(cs) = self.changesets.get_mut(&id) else {
                continue;
            };
            if states.iter().all(|s| *s == EntryState::Committed) {
                cs.state = Settlement::Committed;
                report.committed.push(id);
            } else if states.contains(&EntryState::Conflicted) && cs.state != Settlement::Conflicted
            {
                cs.state = Settlement::Conflicted;
                report.conflicted.push(id);
            }
        }
        Ok(report)
    }

    fn changeset_entries(&self, id: ChangesetId) -> Vec<EntryState> {
        let Some(cs) = self.changesets.get(&id) else {
            return Vec::new();
        };
        cs.changes
            .keys()
            .filter_map(|name| self.leaves.get(name))
            .filter_map(|leaf| {
                leaf.entries
                    .iter()
                    .find(|e| e.origin == Origin::Changeset(id))
                    .map(|e| e.state)
            })
            .collect()
    }

    /// Abandons a changeset that won't reach main, and every changeset built
    /// on it. Their leaves roll back to the last surviving entry. Returns the
    /// abandoned changesets.
    pub fn abandon(&mut self, id: ChangesetId) -> Result<Vec<ChangesetId>, Error> {
        let cs = self
            .changesets
            .get(&id)
            .ok_or(Error::UnknownChangeset(id))?;
        if matches!(cs.state, Settlement::Committed | Settlement::Abandoned) {
            return Err(Error::AlreadySettled(id));
        }
        let mut doomed = BTreeSet::from([id]);
        loop {
            let more: Vec<ChangesetId> = self
                .changesets
                .values()
                .filter(|c| matches!(c.state, Settlement::Sequenced | Settlement::Conflicted))
                .filter(|c| !doomed.contains(&c.id) && !c.depends_on.is_disjoint(&doomed))
                .map(|c| c.id)
                .collect();
            if more.is_empty() {
                break;
            }
            doomed.extend(more);
        }
        self.generation += 1;
        let generation = self.generation;
        for c in &doomed {
            let Some(cs) = self.changesets.get_mut(c) else {
                continue;
            };
            cs.state = Settlement::Abandoned;
            let names: Vec<Scope> = cs.changes.keys().cloned().collect();
            for name in names {
                if let Some(leaf) = self.leaves.get_mut(&name) {
                    for e in &mut leaf.entries {
                        if e.origin == Origin::Changeset(*c) {
                            e.state = EntryState::Abandoned;
                        }
                    }
                    leaf.touched = generation;
                }
            }
        }
        Ok(doomed.into_iter().collect())
    }
}
