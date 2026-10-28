//! Deterministic simulation of the coordinator with overlapping subtree
//! scopes and settlement through git, which the Quint model (single nodes,
//! no git) doesn't cover.
//!
//! A seeded RNG drives agents through every lease action (claims, offline
//! claims, reconcile, heartbeats, releases, expiry, and commits that change,
//! create and delete leaves, sometimes back to content the leaf had before).
//! A ghost main branch takes merges (real ones, which put the commit in
//! main's history, and squash merges, which don't), external edits and
//! reverts; observers read it fresh, or read it and apply later.
//! After every step the safety invariants are checked:
//!
//! 1. no two confirmed active leases have overlapping scopes;
//! 2. a committed lease committed before its deadline; an expired lease has
//!    no result;
//! 3. no lost update: a commit succeeds only if every leaf in its scope
//!    still holds the content its base was taken from;
//! 4. no false conflict: a fresh observation marks work conflicted only when
//!    main holds content the coordinator didn't sequence after the last
//!    settled version, or a later version settled past it;
//! 5. settled stays settled and abandoned stays abandoned;
//! 6. no false settlement, per version: each version an observation settles
//!    was merged, or was built on, in its own file and since its last
//!    settled version, by one merged by history or whose content main moved
//!    to; or main moved to its content off the last settled version; or it
//!    is conflicted work whose content reached main.
//!
//! Each run ends with a fresh observation, after which every leaf agrees with
//! main (or is sequenced work main hasn't merged), and every changeset whose
//! commit is in main's history is settled.
//!
//! Any failure prints the seed and step, so a run reproduces exactly.

use std::collections::{BTreeMap, BTreeSet};

use mdkg_coord::{
    Agent, Changes, ChangesetId, Coordinator, EntryState, Error, LeaseId, Observation,
    Origin as EntryOrigin, Scope, Settlement, Source, Status,
};

const SCOPES: [&str; 6] = [
    "root", "root/a", "root/a/x", "root/a/y", "root/b", "root/b/n",
];
/// `root/b/n` starts absent, so commits create it.
const LEAVES: [&str; 5] = ["root/a", "root/a/x", "root/a/y", "root/b", "root/b/n"];
const AGENTS: [&str; 3] = ["claude", "codex", "nick"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
}

fn inside(leaves: &BTreeMap<Scope, String>, scope: &Scope) -> BTreeMap<Scope, String> {
    leaves
        .iter()
        .filter(|(l, _)| scope.contains(l))
        .map(|(l, c)| (l.clone(), c.clone()))
        .collect()
}

/// `id` and everything it was built on, in `c`.
fn lineage_in(c: &Coordinator, id: ChangesetId) -> BTreeSet<ChangesetId> {
    let mut out = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(x) = stack.pop() {
        if out.insert(x)
            && let Some(cs) = c.changeset(x)
        {
            stack.extend(cs.depends_on.iter().copied());
        }
    }
    out
}

struct World {
    coord: Coordinator,
    agents: BTreeMap<String, Agent>,
    /// What each agent saw when it last synced (its offline base).
    seen: BTreeMap<String, BTreeMap<Scope, String>>,
    /// Leaf contents inside each lease's scope when its base was taken.
    bases: BTreeMap<LeaseId, BTreeMap<Scope, String>>,
    leases: Vec<(String, LeaseId)>,
    /// The ghost main branch, and every content each leaf has had on it.
    main: BTreeMap<Scope, String>,
    history: BTreeMap<Scope, Vec<Option<String>>>,
    merged: BTreeSet<ChangesetId>,
    /// Commits in main's history (real merges, not squash ones).
    merged_commits: BTreeSet<String>,
    late: Vec<Observation>,
    states: BTreeMap<ChangesetId, Settlement>,
    /// Changesets that were ever conflicted.
    conflicts: usize,
    counter: u64,
    now: u64,
}

impl World {
    fn new(ttl: u64) -> Self {
        let mut coord = Coordinator::new(ttl);
        let mut main = BTreeMap::new();
        for leaf in &LEAVES[..4] {
            let content = format!("{leaf}#0");
            coord
                .register(Scope::new(*leaf), content.clone())
                .expect("fresh leaf");
            main.insert(Scope::new(*leaf), content);
        }
        let agents = AGENTS
            .iter()
            .map(|a| (a.to_string(), Agent::new(a, &coord)))
            .collect();
        let seen = AGENTS
            .iter()
            .map(|a| (a.to_string(), coord.leaves()))
            .collect();
        World {
            coord,
            agents,
            seen,
            bases: BTreeMap::new(),
            leases: Vec::new(),
            history: main.keys().map(|l| (l.clone(), Vec::new())).collect(),
            main,
            merged: BTreeSet::new(),
            merged_commits: BTreeSet::new(),
            late: Vec::new(),
            states: BTreeMap::new(),
            conflicts: 0,
            counter: 0,
            now: 0,
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}#{}", self.counter)
    }

    fn set_main(&mut self, leaf: &Scope, content: Option<String>) {
        let old = self.main.get(leaf).cloned();
        self.history.entry(leaf.clone()).or_default().push(old);
        match content {
            Some(c) => self.main.insert(leaf.clone(), c),
            None => self.main.remove(leaf),
        };
    }

    fn observation(&self) -> Observation {
        Observation {
            read_generation: self.coord.generation(),
            leaves: self.main.clone(),
            merged: self
                .coord
                .settlement_queue()
                .into_iter()
                .filter_map(|c| c.source.as_ref().map(|s| s.commit.clone()))
                .filter(|c| self.merged_commits.contains(c))
                .collect(),
            prefix: None,
        }
    }

    /// Recreates, from a lease that doesn't cover it, a leaf another pending
    /// changeset deleted, so a later settlement can pass over the deletion.
    fn recreate_outside_scope(&mut self, rng: &mut Rng) -> bool {
        let deleted: Vec<Scope> = LEAVES
            .map(Scope::new)
            .into_iter()
            .filter(|l| {
                self.coord
                    .leaf(l)
                    .and_then(|c| c.live())
                    .is_some_and(|e| e.state == EntryState::Pending && e.content.is_none())
            })
            .collect();
        let Some(leaf) = rng.pick(&deleted).cloned() else {
            return false;
        };
        let elsewhere: Vec<Scope> = SCOPES
            .map(Scope::new)
            .into_iter()
            .filter(|s| !s.overlaps(&leaf))
            .collect();
        let Some(scope) = rng.pick(&elsewhere).cloned() else {
            return false;
        };
        let agent = AGENTS[rng.below(AGENTS.len())].to_string();
        let now = self.now;
        let a = self.agents.get_mut(&agent).expect("agent");
        let Ok(id) = a.claim(&mut self.coord, &scope, now) else {
            return false;
        };
        self.bases
            .insert(id.clone(), inside(&self.coord.leaves(), &scope));
        self.leases.push((agent.clone(), id.clone()));
        let changes = Changes::from([(leaf.clone(), Some(self.fresh(leaf.as_str())))]);
        let source = Source {
            commit: self.fresh(&format!("commit-{agent}")),
            branch: agent.clone(),
        };
        let a = self.agents.get_mut(&agent).expect("agent");
        a.commit(&mut self.coord, &id, &changes, Some(source), now)
            .is_ok()
    }

    /// Applies an observation. For a fresh one, conflicts must be justified
    /// by main's real content (invariant 4).
    fn observe(&mut self, obs: &Observation, fresh: bool) {
        let before = self.coord.clone();
        match self.coord.observe_main(obs) {
            Ok(report) => {
                // Invariant 6: no false settlement, per version. Each version
                // this observation settled was merged, or was built on (in its
                // own file, since the last settled version) by one merged by
                // history or whose content main moved to, or holds content
                // main moved to, or is conflicted work whose content reached
                // main.
                let direct: BTreeSet<ChangesetId> = before
                    .changesets()
                    .filter(|c| {
                        c.source
                            .as_ref()
                            .is_some_and(|s| obs.merged.contains(&s.commit))
                    })
                    .map(|c| c.id)
                    .collect();
                let lineage = |id: ChangesetId| {
                    let mut out = BTreeSet::new();
                    let mut stack = vec![id];
                    while let Some(c) = stack.pop() {
                        if out.insert(c)
                            && let Some(cs) = before.changeset(c)
                        {
                            stack.extend(cs.depends_on.iter().copied());
                        }
                    }
                    out
                };
                for leaf in LEAVES.map(Scope::new) {
                    let (Some(b), Some(a)) = (before.leaf(&leaf), self.coord.leaf(&leaf)) else {
                        continue;
                    };
                    let main = obs.leaves.get(&leaf).cloned();
                    let last_settled = b
                        .entries
                        .iter()
                        .rev()
                        .find(|e| e.state == EntryState::Committed)
                        .map(|e| e.content.clone());
                    let moved_to = |content: &Option<String>| {
                        *content == main && last_settled.as_ref() != Some(&main)
                    };
                    for (i, e) in b.entries.iter().enumerate() {
                        let now = a.entries.get(i).map(|x| x.state);
                        if e.state == EntryState::Committed || now != Some(EntryState::Committed) {
                            continue;
                        }
                        let EntryOrigin::Changeset(c) = e.origin else {
                            continue;
                        };
                        let built_on =
                            b.entries.iter().enumerate().skip(i + 1).any(|(j, later)| {
                                let EntryOrigin::Changeset(d) = later.origin else {
                                    return false;
                                };
                                let nothing_settled_between = b.entries[i + 1..j]
                                    .iter()
                                    .all(|x| x.state != EntryState::Committed);
                                nothing_settled_between
                                    && lineage(d).contains(&c)
                                    && (direct.contains(&d) || moved_to(&later.content))
                            });
                        let ok = self.merged.contains(&c)
                            || direct.contains(&c)
                            || built_on
                            || moved_to(&e.content)
                            || (e.state == EntryState::Conflicted && e.content == main);
                        assert!(ok, "false settlement: {c} in {leaf}");
                    }
                }
                if fresh {
                    for id in &report.conflicted {
                        let cs = before.changeset(*id).expect("changeset");
                        // Judged on the chain as this observation's history
                        // settlement left it (its own entries, not the main
                        // entries it appended).
                        let justified = cs.changes.keys().any(|leaf| {
                            let main = obs.leaves.get(leaf).map(String::as_str);
                            let (Some(old), Some(new)) = (before.leaf(leaf), self.coord.leaf(leaf))
                            else {
                                return main.is_some();
                            };
                            let chain = &new.entries[..old.entries.len()];
                            let settled = chain
                                .iter()
                                .rposition(|e| e.state == EntryState::Committed)
                                .unwrap_or(0);
                            // Main moved past it: a later version that wasn't
                            // built on it settled.
                            let passed_over = chain
                                .iter()
                                .position(|e| e.origin == EntryOrigin::Changeset(*id))
                                .is_some_and(|i| {
                                    chain.iter().enumerate().skip(i + 1).any(|(j, e)| {
                                        j <= settled
                                            && e.state == EntryState::Committed
                                            && matches!(e.origin, EntryOrigin::Changeset(d)
                                                if !lineage_in(&before, d).contains(id))
                                    })
                                });
                            passed_over
                                || !chain[settled..].iter().any(|e| {
                                    e.state != EntryState::Abandoned && e.content.as_deref() == main
                                })
                        });
                        assert!(justified, "false conflict: {id}");
                    }
                }
            }
            Err(Error::ObservationRaced(_)) => assert!(!fresh, "a fresh observation raced"),
            Err(e) => panic!("observation refused: {e:?}"),
        }
    }

    fn step(&mut self, rng: &mut Rng) {
        if rng.below(12) == 0 && self.recreate_outside_scope(rng) {
            return;
        }
        let agent = AGENTS[rng.below(AGENTS.len())].to_string();
        let scope = Scope::new(SCOPES[rng.below(SCOPES.len())]);
        // Mostly act on one of this agent's live leases, sometimes on any.
        let live: Vec<LeaseId> = self
            .leases
            .iter()
            .filter(|(o, id)| {
                *o == agent
                    && (self
                        .coord
                        .lease(id)
                        .is_some_and(|l| l.status == Status::Active)
                        || self.agents[&agent]
                            .provisional()
                            .any(|p| &p.id == id && p.status == Status::Active))
            })
            .map(|(_, id)| id.clone())
            .collect();
        let owned = if rng.below(4) == 0 {
            rng.pick(&self.leases)
                .filter(|(o, _)| *o == agent)
                .map(|(_, id)| id.clone())
        } else {
            rng.pick(&live).cloned()
        };
        let now = self.now;
        let ttl = self.coord.ttl();
        match rng.below(16) {
            0 => self.now += rng.below(4) as u64,
            1 => {
                self.seen.insert(agent.clone(), self.coord.leaves());
                let a = self.agents.get_mut(&agent).expect("agent");
                a.disconnect(&self.coord);
            }
            2 => self.agents.get_mut(&agent).expect("agent").reconnect(),
            3 => {
                let a = self.agents.get_mut(&agent).expect("agent");
                match a.claim(&mut self.coord, &scope, now) {
                    Ok(id) => {
                        self.bases
                            .insert(id.clone(), inside(&self.coord.leaves(), &scope));
                        self.leases.push((agent, id));
                    }
                    Err(Error::Held(id)) => self.leases.push((agent, id)),
                    Err(_) => {}
                }
            }
            4 => {
                let a = self.agents.get_mut(&agent).expect("agent");
                if let Ok(id) = a.claim_offline(&scope, now, ttl) {
                    self.bases
                        .insert(id.clone(), inside(&self.seen[&agent], &scope));
                    self.leases.push((agent, id));
                }
            }
            5 => {
                if let Some(id) = owned {
                    let a = self.agents.get_mut(&agent).expect("agent");
                    a.heartbeat(&mut self.coord, &id, now).ok();
                }
            }
            6 => {
                if let Some(id) = owned {
                    let a = self.agents.get_mut(&agent).expect("agent");
                    a.release(&mut self.coord, &id, now).ok();
                }
            }
            7 if rng.below(3) == 0 => {
                // Stack on sequenced work: claim a leaf whose current version
                // is pending and commit on top, so the new changeset depends
                // on that one.
                let pending: Vec<Scope> = LEAVES
                    .map(Scope::new)
                    .into_iter()
                    .filter(|l| {
                        self.coord
                            .leaf(l)
                            .and_then(|c| c.live())
                            .is_some_and(|e| e.state == EntryState::Pending)
                    })
                    .collect();
                let Some(leaf) = rng.pick(&pending).cloned() else {
                    return;
                };
                let a = self.agents.get_mut(&agent).expect("agent");
                let Ok(id) = a.claim(&mut self.coord, &leaf, now) else {
                    return;
                };
                self.bases
                    .insert(id.clone(), inside(&self.coord.leaves(), &leaf));
                self.leases.push((agent.clone(), id.clone()));
                let changes = Changes::from([(leaf.clone(), Some(self.fresh(leaf.as_str())))]);
                let source = Source {
                    commit: self.fresh(&format!("commit-{agent}")),
                    branch: agent.clone(),
                };
                let a = self.agents.get_mut(&agent).expect("agent");
                a.commit(&mut self.coord, &id, &changes, Some(source), now)
                    .ok();
            }
            7 => {
                let Some(id) = owned else { return };
                let Some(l) = self.coord.lease(&id).cloned() else {
                    return;
                };
                let targets: Vec<Scope> = LEAVES
                    .iter()
                    .map(|x| Scope::new(*x))
                    .filter(|x| l.scope.contains(x) || rng.below(10) == 0)
                    .collect();
                let mut changes = Changes::new();
                for _ in 0..1 + rng.below(2) {
                    if let Some(leaf) = rng.pick(&targets).cloned() {
                        // Sometimes back to content the leaf had before (A, B,
                        // then A again), which content alone can't tell apart.
                        let earlier: Vec<Option<String>> = self
                            .coord
                            .leaf(&leaf)
                            .map(|l| l.entries.iter().map(|e| e.content.clone()).collect())
                            .unwrap_or_default();
                        let content = if rng.below(5) == 0 && !earlier.is_empty() {
                            rng.pick(&earlier).cloned().flatten()
                        } else {
                            (rng.below(6) != 0).then(|| self.fresh(leaf.as_str()))
                        };
                        changes.insert(leaf, content);
                    }
                }
                let before = inside(&self.coord.leaves(), &l.scope);
                let live = self.coord.leaves();
                let commit = self.fresh(&format!("commit-{agent}"));
                let source = Source {
                    commit,
                    branch: agent.clone(),
                };
                let a = self.agents.get_mut(&agent).expect("agent");
                if a.commit(&mut self.coord, &id, &changes, Some(source), now)
                    .is_ok()
                {
                    // Outside its scope, a commit may only create a new leaf.
                    assert!(
                        changes.iter().all(|(leaf, content)| l.scope.contains(leaf)
                            || (content.is_some() && !live.contains_key(leaf))),
                        "{id} changed an existing leaf outside its scope"
                    );
                    assert_eq!(
                        Some(&before),
                        self.bases.get(&id),
                        "lost update: {id} committed {} from a changed base",
                        l.scope
                    );
                }
            }
            8 => {
                if let Some(id) = owned {
                    let a = self.agents.get_mut(&agent).expect("agent");
                    if let Ok((confirmed, _)) = a.reconcile(&mut self.coord, &id, now) {
                        if let Some(base) = self.bases.get(&id).cloned() {
                            self.bases.insert(confirmed.clone(), base);
                        }
                        for entry in &mut self.leases {
                            if entry.1 == id {
                                entry.1 = confirmed.clone();
                            }
                        }
                    }
                }
            }
            9 => {
                self.coord.expire_due(now);
            }
            10 if rng.below(4) == 0 => {
                // A real merge of a branch that had its dependencies' content
                // by copying, not merging: only its own commit is in history,
                // and only its own files change on main.
                let open: Vec<ChangesetId> = self
                    .coord
                    .changesets()
                    .filter(|c| c.state == Settlement::Sequenced && !self.merged.contains(&c.id))
                    .map(|c| c.id)
                    .collect();
                // Mostly one stacked on unmerged work, the case this models.
                let stacked: Vec<ChangesetId> = open
                    .iter()
                    .copied()
                    .filter(|id| {
                        self.coord
                            .changeset(*id)
                            .is_some_and(|c| c.depends_on.iter().any(|d| !self.merged.contains(d)))
                    })
                    .collect();
                let pool = if !stacked.is_empty() && rng.below(4) != 0 {
                    &stacked
                } else {
                    &open
                };
                if let Some(id) = rng.pick(pool).copied() {
                    let cs = self.coord.changeset(id).expect("changeset").clone();
                    for (leaf, content) in cs.changes {
                        self.set_main(&leaf, content);
                    }
                    self.merged.insert(id);
                    if let Some(s) = cs.source {
                        self.merged_commits.insert(s.commit);
                    }
                }
            }
            10 => {
                // Merge a sequenced changeset whose dependencies are merged;
                // sometimes a conflicted one, resolved in the branch's favour.
                let conflicted_too = rng.below(3) == 0;
                let ready: Vec<ChangesetId> = self
                    .coord
                    .changesets()
                    .filter(|c| {
                        (c.state == Settlement::Sequenced
                            || (conflicted_too && c.state == Settlement::Conflicted))
                            && !self.merged.contains(&c.id)
                    })
                    .filter(|c| c.depends_on.iter().all(|d| self.merged.contains(d)))
                    .map(|c| c.id)
                    .collect();
                if let Some(id) = rng.pick(&ready).copied() {
                    let changes = self.coord.changeset(id).expect("changeset").changes.clone();
                    for (leaf, content) in changes {
                        self.set_main(&leaf, content);
                    }
                    self.merged.insert(id);
                    // A real merge puts the commit, and the commits it was
                    // stacked on, in main's history; a squash merge doesn't.
                    if rng.below(3) != 0 {
                        let mut stack = vec![id];
                        while let Some(c) = stack.pop() {
                            let Some(cs) = self.coord.changeset(c) else {
                                continue;
                            };
                            if let Some(s) = &cs.source {
                                self.merged_commits.insert(s.commit.clone());
                            }
                            stack.extend(cs.depends_on.iter().copied());
                        }
                    }
                }
            }
            11 => {
                // Someone edits main directly.
                let leaf = Scope::new(LEAVES[rng.below(LEAVES.len())]);
                let content = (rng.below(4) != 0).then(|| self.fresh("main"));
                self.set_main(&leaf, content);
            }
            12 => {
                // Someone reverts a leaf on main.
                let leaf = Scope::new(LEAVES[rng.below(LEAVES.len())]);
                let earlier = self.history.get(&leaf).cloned().unwrap_or_default();
                if let Some(content) = rng.pick(&earlier).cloned() {
                    self.set_main(&leaf, content);
                }
            }
            13 => {
                let obs = self.observation();
                self.observe(&obs, true);
            }
            14 => {
                if rng.below(2) == 0 {
                    let obs = self.observation();
                    self.late.push(obs);
                } else if !self.late.is_empty() {
                    let i = rng.below(self.late.len());
                    let obs = self.late.swap_remove(i);
                    self.observe(&obs, false);
                }
            }
            _ => {
                let open: Vec<ChangesetId> = self
                    .coord
                    .changesets()
                    .filter(|c| matches!(c.state, Settlement::Sequenced | Settlement::Conflicted))
                    .map(|c| c.id)
                    .collect();
                if rng.below(3) == 0
                    && let Some(id) = rng.pick(&open).copied()
                {
                    self.coord
                        .abandon(id)
                        .expect("open changesets can be abandoned");
                }
            }
        }
    }

    fn check(&mut self) {
        let live: Vec<_> = self
            .coord
            .leases()
            .filter(|l| l.confirmed && l.status == Status::Active)
            .collect();
        for (i, x) in live.iter().enumerate() {
            for y in &live[i + 1..] {
                assert!(
                    !x.scope.overlaps(&y.scope),
                    "mutual exclusion: {} ({}) and {} ({})",
                    x.id,
                    x.scope,
                    y.id,
                    y.scope
                );
            }
        }
        for l in self.coord.leases() {
            if l.status == Status::Committed {
                assert!(
                    l.committed_at.is_some_and(|t| t < l.deadline),
                    "{} committed late",
                    l.id
                );
                assert!(l.confirmed, "{} committed unconfirmed", l.id);
            }
            if l.status == Status::Expired {
                assert!(l.result.is_none(), "{} expired with a result", l.id);
            }
        }
        for cs in self.coord.changesets() {
            if let Some(old) = self.states.get(&cs.id) {
                let allowed = match old {
                    Settlement::Committed | Settlement::Abandoned => cs.state == *old,
                    Settlement::Conflicted => cs.state != Settlement::Sequenced,
                    Settlement::Sequenced => true,
                };
                assert!(allowed, "{} went from {old:?} to {:?}", cs.id, cs.state);
            }
            if cs.state == Settlement::Conflicted
                && self.states.get(&cs.id) != Some(&Settlement::Conflicted)
            {
                self.conflicts += 1;
            }
        }
        self.states = self.coord.changesets().map(|c| (c.id, c.state)).collect();
    }

    /// After a fresh observation, the coordinator agrees with main.
    fn converge(&mut self) {
        let obs = self.observation();
        let before = self.coord.clone();
        self.observe(&obs, true);
        // Main moved off its last settled version to content sequenced work
        // holds (a squash merge, say): that content is now settled.
        for leaf in LEAVES.map(Scope::new) {
            let main = self.main.get(&leaf).cloned();
            let (Some(b), Some(a)) = (before.leaf(&leaf), self.coord.leaf(&leaf)) else {
                continue;
            };
            let lc = b
                .entries
                .iter()
                .rposition(|e| e.state == EntryState::Committed)
                .unwrap_or(0);
            let moved_to_pending = b.entries[lc].content != main
                && b.entries[lc + 1..]
                    .iter()
                    .any(|e| e.state == EntryState::Pending && e.content == main);
            if moved_to_pending {
                let settled = a
                    .entries
                    .iter()
                    .rev()
                    .find(|e| e.state == EntryState::Committed)
                    .map(|e| e.content.clone());
                assert_eq!(
                    settled,
                    Some(main),
                    "{leaf}: main's new content didn't settle"
                );
            }
        }
        for leaf in LEAVES.map(Scope::new) {
            let main = self.main.get(&leaf).map(String::as_str);
            let Some(chain) = self.coord.leaf(&leaf) else {
                assert!(main.is_none(), "{leaf} is on main but unknown");
                continue;
            };
            let live = chain.live().expect("live entry");
            if live.content.as_deref() == main {
                continue;
            }
            assert_eq!(
                live.state,
                EntryState::Pending,
                "{leaf} disagrees with main"
            );
            let settled = chain
                .entries
                .iter()
                .rposition(|e| e.state == EntryState::Committed)
                .expect("settled entry");
            assert_eq!(
                chain.entries[settled].content.as_deref(),
                main,
                "{leaf}: main isn't the last settled version"
            );
        }
        for cs in self.coord.changesets() {
            let in_history = cs
                .source
                .as_ref()
                .is_some_and(|s| self.merged_commits.contains(&s.commit));
            if in_history && cs.state != Settlement::Abandoned {
                assert_eq!(
                    cs.state,
                    Settlement::Committed,
                    "{} is in main's history",
                    cs.id
                );
            }
        }
    }
}

#[test]
fn safety_holds_across_seeded_runs() {
    let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut world = World::new(1 + seed % 5);
        for step in 0..400 {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                world.step(&mut rng);
                world.check();
            }));
            assert!(
                result.is_ok(),
                "invariant failed at seed {seed}, step {step}"
            );
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.converge()));
        assert!(result.is_ok(), "convergence failed at seed {seed}");
        for l in world.coord.leases() {
            let key = match l.status {
                Status::Committed => "lease commits",
                Status::Conflict => "lease conflicts",
                _ => continue,
            };
            *totals.entry(key).or_default() += 1;
        }
        for cs in world.coord.changesets() {
            let key = match cs.state {
                Settlement::Committed => "changesets committed",
                Settlement::Abandoned => "changesets abandoned",
                Settlement::Sequenced | Settlement::Conflicted => continue,
            };
            *totals.entry(key).or_default() += 1;
        }
        *totals.entry("changesets ever conflicted").or_default() += world.conflicts;
    }
    eprintln!("300 seeds x 400 steps: {totals:?}");
    for (key, min) in [
        ("lease commits", 100),
        ("lease conflicts", 100),
        ("changesets committed", 50),
        ("changesets ever conflicted", 20),
        ("changesets abandoned", 20),
    ] {
        assert!(
            totals.get(key).copied().unwrap_or(0) >= min,
            "simulation should exercise {key}: {totals:?}"
        );
    }
}

#[test]
fn same_seed_same_history() {
    let run = |seed| {
        let mut rng = Rng(seed);
        let mut world = World::new(3);
        for _ in 0..500 {
            world.step(&mut rng);
        }
        serde_json::to_string(&world.coord).expect("state serializes")
    };
    assert_eq!(run(42), run(42));
}
