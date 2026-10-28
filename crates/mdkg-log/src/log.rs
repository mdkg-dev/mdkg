//! An in-memory, hash-linked event log for one shard (sans-IO).
//!
//! Appends are checked: the envelope must verify, belong to this shard,
//! reference only known parents, and not repeat an idempotency key inside its
//! validity window. Concurrent writers are fine: the log
//! is a DAG, and `heads` are the events nothing references yet.

use std::collections::{HashMap, HashSet};

use mdkg_format::Digest;

use crate::envelope::{Envelope, EnvelopeError};
use crate::merkle::MerkleTree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    Envelope(EnvelopeError),
    WrongShard {
        expected: String,
        found: String,
    },
    UnknownParent(String),
    /// The same intended effect was already recorded; `existing` is its id.
    Duplicate {
        key: String,
        existing: String,
    },
    /// Replay found the same event twice.
    DuplicateEvent(String),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppendError::Envelope(e) => write!(f, "{e}"),
            AppendError::WrongShard { expected, found } => {
                write!(f, "event belongs to shard {found}, not {expected}")
            }
            AppendError::UnknownParent(p) => write!(f, "event references unknown parent {p}"),
            AppendError::Duplicate { key, existing } => {
                write!(f, "idempotency key {key} already recorded as {existing}")
            }
            AppendError::DuplicateEvent(id) => write!(f, "event {id} appears more than once"),
        }
    }
}

impl std::error::Error for AppendError {}

/// What happened on append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Appended {
    New(String),
    /// Exactly the same envelope was already present (a harmless retry).
    AlreadyPresent(String),
}

#[derive(Debug, Clone)]
pub struct Log {
    shard: String,
    events: Vec<Envelope>,
    by_id: HashMap<String, usize>,
    referenced: HashSet<String>,
    /// idempotency key -> (event id, valid_until)
    keys: HashMap<String, (String, u64)>,
}

impl Log {
    pub fn new(shard: impl Into<String>) -> Self {
        Self {
            shard: shard.into(),
            events: Vec::new(),
            by_id: HashMap::new(),
            referenced: HashSet::new(),
            keys: HashMap::new(),
        }
    }

    pub fn shard(&self) -> &str {
        &self.shard
    }

    /// Events in append order (parents always precede children).
    pub fn events(&self) -> &[Envelope] {
        &self.events
    }

    pub fn get(&self, id: &str) -> Option<&Envelope> {
        self.by_id.get(id).map(|i| &self.events[*i])
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Events no other event lists as a parent, sorted by id.
    pub fn heads(&self) -> Vec<String> {
        let mut heads: Vec<String> = self
            .events
            .iter()
            .filter(|e| !self.referenced.contains(&e.id))
            .map(|e| e.id.clone())
            .collect();
        heads.sort();
        heads
    }

    /// Appends a sealed event. `now_ms` decides whether an earlier event with
    /// the same idempotency key is still inside its dedup window.
    pub fn append(&mut self, event: Envelope, now_ms: u64) -> Result<Appended, AppendError> {
        event.verify().map_err(AppendError::Envelope)?;
        if event.draft.shard != self.shard {
            return Err(AppendError::WrongShard {
                expected: self.shard.clone(),
                found: event.draft.shard.clone(),
            });
        }
        if self.by_id.contains_key(&event.id) {
            return Ok(Appended::AlreadyPresent(event.id));
        }
        if let Some(parent) = event
            .draft
            .parents
            .iter()
            .find(|p| !self.by_id.contains_key(*p))
        {
            return Err(AppendError::UnknownParent(parent.clone()));
        }
        if let Some((existing, valid_until)) = self.keys.get(&event.draft.idempotency_key)
            && now_ms <= *valid_until
        {
            return Err(AppendError::Duplicate {
                key: event.draft.idempotency_key.clone(),
                existing: existing.clone(),
            });
        }
        self.keys.insert(
            event.draft.idempotency_key.clone(),
            (event.id.clone(), event.draft.valid_until),
        );
        self.referenced.extend(event.draft.parents.iter().cloned());
        self.by_id.insert(event.id.clone(), self.events.len());
        let id = event.id.clone();
        self.events.push(event);
        Ok(Appended::New(id))
    }

    /// Rebuilds the log from already-accepted history (export import or
    /// storage reload). Ids, shard and parents are checked, and repeated
    /// events are errors. Idempotency windows are not re-evaluated: they were
    /// enforced when each event was first appended, against that moment's
    /// time, which replay can't reconstruct.
    pub fn replay(&mut self, event: Envelope) -> Result<(), AppendError> {
        event.verify().map_err(AppendError::Envelope)?;
        if event.draft.shard != self.shard {
            return Err(AppendError::WrongShard {
                expected: self.shard.clone(),
                found: event.draft.shard.clone(),
            });
        }
        if self.by_id.contains_key(&event.id) {
            return Err(AppendError::DuplicateEvent(event.id));
        }
        if let Some(parent) = event
            .draft
            .parents
            .iter()
            .find(|p| !self.by_id.contains_key(*p))
        {
            return Err(AppendError::UnknownParent(parent.clone()));
        }
        let previous = self
            .keys
            .get(&event.draft.idempotency_key)
            .map(|(_, until)| *until);
        if previous.is_none_or(|until| event.draft.valid_until >= until) {
            self.keys.insert(
                event.draft.idempotency_key.clone(),
                (event.id.clone(), event.draft.valid_until),
            );
        }
        self.referenced.extend(event.draft.parents.iter().cloned());
        self.by_id.insert(event.id.clone(), self.events.len());
        self.events.push(event);
        Ok(())
    }

    /// Drops expired idempotency keys (bounded state, invariant 15).
    pub fn prune_keys(&mut self, now_ms: u64) -> usize {
        let before = self.keys.len();
        self.keys
            .retain(|_, (_, valid_until)| *valid_until >= now_ms);
        before - self.keys.len()
    }

    /// Merkle root over (event id -> event id digest), i.e. the set of events.
    pub fn root(&self) -> Digest {
        MerkleTree::build(
            self.events
                .iter()
                .filter_map(|e| Digest::parse(&e.id).map(|d| (e.id.clone(), d)))
                .collect(),
        )
        .root()
    }
}

/// Verifies a sequence of events independently: every id matches its
/// contents and every parent appears earlier.
pub fn verify_chain(events: &[Envelope]) -> Result<(), AppendError> {
    let mut seen = HashSet::new();
    for event in events {
        event.verify().map_err(AppendError::Envelope)?;
        if let Some(parent) = event.draft.parents.iter().find(|p| !seen.contains(*p)) {
            return Err(AppendError::UnknownParent(parent.clone()));
        }
        seen.insert(event.id.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::tests::draft;

    const SHARD: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";

    #[test]
    fn appends_a_dag_and_tracks_heads() {
        let mut log = Log::new(SHARD);
        let a = draft("a", vec![], 1).seal().unwrap();
        let b = draft("b", vec![a.id.clone()], 2).seal().unwrap();
        let c = draft("c", vec![a.id.clone()], 2).seal().unwrap();
        log.append(a.clone(), 1).unwrap();
        log.append(b.clone(), 2).unwrap();
        log.append(c.clone(), 2).unwrap();
        let mut expected = vec![b.id.clone(), c.id.clone()];
        expected.sort();
        assert_eq!(log.heads(), expected);
        verify_chain(log.events()).unwrap();
    }

    #[test]
    fn rejects_unknown_parents_and_wrong_shards() {
        let mut log = Log::new(SHARD);
        let orphan = draft("x", vec!["sha256:00".into()], 1).seal().unwrap();
        assert!(matches!(
            log.append(orphan, 1),
            Err(AppendError::UnknownParent(_))
        ));
        let mut other = draft("y", vec![], 1);
        other.shard = "elsewhere".into();
        assert!(matches!(
            log.append(other.seal().unwrap(), 1),
            Err(AppendError::WrongShard { .. })
        ));
    }

    #[test]
    fn idempotency_window() {
        let mut log = Log::new(SHARD);
        let first = draft("same-effect", vec![], 1_000).seal().unwrap();
        log.append(first.clone(), 1_000).unwrap();
        // An exact retry is harmless.
        assert_eq!(
            log.append(first.clone(), 1_001).unwrap(),
            Appended::AlreadyPresent(first.id.clone())
        );
        // A different event claiming the same effect inside the window is rejected.
        let mut retry = draft("same-effect", vec![], 1_500);
        retry.actor = "agent:codex".into();
        assert!(matches!(
            log.append(retry.clone().seal().unwrap(), 1_500),
            Err(AppendError::Duplicate { .. })
        ));
        // After the window (valid_until = 61_000) the key may be reused.
        assert!(log.append(retry.seal().unwrap(), 61_001).is_ok());
    }

    #[test]
    fn tampered_history_fails_verification() {
        let mut log = Log::new(SHARD);
        log.append(draft("a", vec![], 1).seal().unwrap(), 1)
            .unwrap();
        let mut events = log.events().to_vec();
        events[0].draft.subject = vec!["something-else".into()];
        assert!(verify_chain(&events).is_err());
    }
}
