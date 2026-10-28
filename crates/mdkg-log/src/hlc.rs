//! Hybrid logical clocks for event ordering.
//!
//! An HLC timestamp is wall-clock milliseconds plus a logical counter. It
//! never goes backwards, stays close to physical time, and respects
//! causality: an event created after observing another always gets a larger
//! timestamp, even when machine clocks disagree. The clock is sans-IO: the
//! caller passes the current physical time.

use serde::{Deserialize, Serialize};

/// A hybrid logical clock timestamp. Ordered by (wall, logical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    /// Milliseconds since the Unix epoch.
    pub wall: u64,
    /// Tie-breaker for events in the same millisecond.
    pub logical: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc {
        wall: 0,
        logical: 0,
    };
}

/// One writer's clock.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    last: Option<Hlc>,
}

impl Clock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last(&self) -> Option<Hlc> {
        self.last
    }

    /// A timestamp for a local event at physical time `now_ms`.
    pub fn tick(&mut self, now_ms: u64) -> Hlc {
        let next = match self.last {
            Some(last) if last.wall >= now_ms => bump(last.wall, last.logical),
            _ => Hlc {
                wall: now_ms,
                logical: 0,
            },
        };
        self.last = Some(next);
        next
    }

    /// A timestamp after receiving `remote` at physical time `now_ms`.
    pub fn observe(&mut self, remote: Hlc, now_ms: u64) -> Hlc {
        let last = self.last.unwrap_or(Hlc::ZERO);
        let wall = now_ms.max(last.wall).max(remote.wall);
        let next = if wall == last.wall && wall == remote.wall {
            bump(wall, last.logical.max(remote.logical))
        } else if wall == last.wall {
            bump(wall, last.logical)
        } else if wall == remote.wall {
            bump(wall, remote.logical)
        } else {
            Hlc { wall, logical: 0 }
        };
        self.last = Some(next);
        next
    }

    /// Like [`Clock::observe`], but refuses a remote timestamp more than
    /// `max_drift_ms` ahead of physical time, so one faulty clock can't drag
    /// every writer into the future.
    pub fn observe_bounded(
        &mut self,
        remote: Hlc,
        now_ms: u64,
        max_drift_ms: u64,
    ) -> Result<Hlc, ClockError> {
        if remote.wall > now_ms.saturating_add(max_drift_ms) {
            return Err(ClockError::TooFarAhead {
                remote: remote.wall,
                now: now_ms,
            });
        }
        Ok(self.observe(remote, now_ms))
    }
}

/// The next timestamp after (wall, logical): the logical counter rolls into
/// the next millisecond instead of saturating, so order stays strict.
fn bump(wall: u64, logical: u32) -> Hlc {
    match logical.checked_add(1) {
        Some(logical) => Hlc { wall, logical },
        None => Hlc {
            wall: wall.saturating_add(1),
            logical: 0,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    TooFarAhead { remote: u64, now: u64 },
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClockError::TooFarAhead { remote, now } => {
                write!(f, "remote clock {remote} is too far ahead of {now}")
            }
        }
    }
}

impl std::error::Error for ClockError {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ticks_are_monotonic_when_the_wall_clock_goes_backwards() {
        let mut c = Clock::new();
        let a = c.tick(1_000);
        let b = c.tick(900);
        assert!(b > a);
        assert_eq!(b.wall, 1_000);
    }

    #[test]
    fn observing_a_future_event_moves_past_it() {
        let mut c = Clock::new();
        c.tick(1_000);
        let remote = Hlc {
            wall: 5_000,
            logical: 7,
        };
        let after = c.observe(remote, 1_001);
        assert!(after > remote);
    }

    proptest! {
        #[test]
        fn any_sequence_is_strictly_increasing(steps in prop::collection::vec((any::<bool>(), 0u64..10_000, 0u64..10_000, 0u32..5), 1..200)) {
            let mut c = Clock::new();
            let mut previous: Option<Hlc> = None;
            for (is_observe, now, remote_wall, remote_logical) in steps {
                let remote = Hlc { wall: remote_wall, logical: remote_logical };
                let next = if is_observe { c.observe(remote, now) } else { c.tick(now) };
                if let Some(p) = previous {
                    prop_assert!(next > p);
                }
                if is_observe {
                    prop_assert!(next > remote);
                }
                prop_assert!(next.wall >= now);
                previous = Some(next);
            }
        }
    }
}
