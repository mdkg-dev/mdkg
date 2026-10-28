//! Ordering events, separate from executing their effects.
//!
//! A sequencer assigns each event of a shard its `seq`. Owned shards
//! sequence themselves (no coordinator round trip); shared shards go through
//! their home coordinator. Later implementations (remote coordinator,
//! committee, anchoring) plug in behind the same trait.

use std::collections::HashMap;

/// Whether a shard needs ordering by a coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// A single writing actor: events are self-sequenced.
    Owned,
    /// Several writers: events are ordered by the home coordinator.
    Shared,
}

/// An event's finality: how far it has progressed toward settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finality {
    Local,
    Sequenced,
    Committed,
    Anchored,
}

pub trait Sequencer {
    /// The next sequence number for `shard`, starting at 1.
    fn assign(&mut self, shard: &str) -> u64;
    /// The last number assigned for `shard`, or 0.
    fn last(&self, shard: &str) -> u64;
}

/// One counter per shard, held locally (the local coordinator, or an owned
/// shard sequencing itself).
#[derive(Debug, Clone, Default)]
pub struct LocalSequencer {
    counters: HashMap<String, u64>,
}

impl Sequencer for LocalSequencer {
    fn assign(&mut self, shard: &str) -> u64 {
        let n = self.counters.entry(shard.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    fn last(&self, shard: &str) -> u64 {
        self.counters.get(shard).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_are_per_shard_and_contiguous() {
        let mut s = LocalSequencer::default();
        assert_eq!(s.assign("a"), 1);
        assert_eq!(s.assign("a"), 2);
        assert_eq!(s.assign("b"), 1);
        assert_eq!(s.last("a"), 2);
        assert_eq!(s.last("missing"), 0);
        assert!(Finality::Local < Finality::Anchored);
    }
}
