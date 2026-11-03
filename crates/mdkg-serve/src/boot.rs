//! The boot id and the clocks (S7).
//!
//! Every signature covers the boot id (`mdkg-boot`), so a request signed
//! before a restart is refused after it, whatever its `created` says. A
//! boot id is 16 random bytes, minted at start and never reused. It also
//! rotates whenever the wall clock steps backwards by more than 1 s against
//! the monotonic clock: a captured request whose `created` falls back
//! inside the window after such a step was signed for the old boot id.

use std::io;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::random;

/// How far the wall clock may step back before the boot id rotates (S7).
pub const BACKWARD_STEP_MS: i128 = 1_000;

/// The two clocks `serve` reads. A trait so tests can step them.
pub trait Clocks: Send + Sync {
    /// Wall-clock milliseconds since the Unix epoch.
    fn wall_ms(&self) -> u64;
    /// Monotonic time since some fixed point in this process.
    fn mono(&self) -> Duration;
}

/// The OS clocks.
#[derive(Debug)]
pub struct SystemClocks {
    start: Instant,
}

impl SystemClocks {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClocks {
    fn default() -> Self {
        Self::new()
    }
}

impl Clocks for SystemClocks {
    fn wall_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn mono(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A boot id: 16 random bytes, shown as 32 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId([u8; 16]);

impl BootId {
    pub fn mint() -> io::Result<Self> {
        Ok(Self(random::bytes()?))
    }

    pub fn as_hex(&self) -> String {
        random::hex(&self.0)
    }
}

#[derive(Debug)]
struct State {
    id: BootId,
    /// The highest `wall - mono` seen since the last rotation. The wall
    /// clock may move forward freely; falling more than 1 s below this
    /// mark is a backward step, however it was reached (one step, or
    /// several small ones).
    high_offset_ms: i128,
}

/// The current boot id, rotated on a backward clock step.
#[derive(Debug)]
pub struct Boot {
    state: Mutex<State>,
}

impl Boot {
    pub fn start(clocks: &dyn Clocks) -> io::Result<Self> {
        Ok(Self {
            state: Mutex::new(State {
                id: BootId::mint()?,
                high_offset_ms: i128::from(clocks.wall_ms()) - clocks.mono().as_millis() as i128,
            }),
        })
    }

    /// The boot id now: checks the clocks first, and rotates on a step
    /// back. Errs only when a rotation can't draw random bytes; the caller
    /// then refuses the request rather than keep the old id.
    pub fn current(&self, clocks: &dyn Clocks) -> io::Result<BootId> {
        self.observe(clocks).map(|o| o.boot)
    }

    /// One reading of both clocks, with the boot id that reading leaves.
    ///
    /// Everything that judges time by the wall clock (the window, nonce
    /// retention, pruning) uses an observation, never a clock of its own:
    /// a forward jump seen by a prune then raises the mark here, so its
    /// correction rotates the boot id before a nonce the jump let expire
    /// could be replayed (Terra 6 #6612).
    pub fn observe(&self, clocks: &dyn Clocks) -> io::Result<Observed> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let wall_ms = clocks.wall_ms();
        let mono = clocks.mono();
        let now = i128::from(wall_ms) - mono.as_millis() as i128;
        if now < state.high_offset_ms - BACKWARD_STEP_MS {
            state.id = BootId::mint()?;
            state.high_offset_ms = now;
        } else if now > state.high_offset_ms {
            state.high_offset_ms = now;
        }
        Ok(Observed {
            boot: state.id,
            wall_ms,
            mono,
        })
    }
}

/// One observation of the clocks (see [`Boot::observe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub boot: BootId,
    pub wall_ms: u64,
    pub mono: Duration,
}

#[cfg(test)]
pub(crate) mod test_clocks {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    /// Clocks a test steps by hand.
    #[derive(Debug, Default)]
    pub struct ManualClocks {
        pub wall: AtomicI64,
        pub mono: AtomicU64,
    }

    impl ManualClocks {
        pub fn at(wall_ms: i64) -> Self {
            let c = Self::default();
            c.wall.store(wall_ms, Ordering::SeqCst);
            c
        }

        /// Both clocks move forward together.
        pub fn advance(&self, ms: u64) {
            self.wall.fetch_add(ms as i64, Ordering::SeqCst);
            self.mono.fetch_add(ms, Ordering::SeqCst);
        }

        /// The wall clock alone steps (negative: backwards).
        pub fn step_wall(&self, ms: i64) {
            self.wall.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clocks for ManualClocks {
        fn wall_ms(&self) -> u64 {
            self.wall.load(Ordering::SeqCst).max(0) as u64
        }

        fn mono(&self) -> Duration {
            Duration::from_millis(self.mono.load(Ordering::SeqCst))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_clocks::ManualClocks;
    use super::*;

    #[test]
    fn each_start_mints_a_new_id() {
        let clocks = ManualClocks::at(1_000_000);
        let a = Boot::start(&clocks).expect("boot");
        let b = Boot::start(&clocks).expect("boot");
        assert_ne!(
            a.current(&clocks).expect("id"),
            b.current(&clocks).expect("id")
        );
        assert_eq!(a.current(&clocks).expect("id").as_hex().len(), 32);
    }

    #[test]
    fn forward_time_and_small_steps_keep_the_id() {
        let clocks = ManualClocks::at(1_000_000);
        let boot = Boot::start(&clocks).expect("boot");
        let id = boot.current(&clocks).expect("id");
        clocks.advance(60_000);
        clocks.step_wall(30_000);
        clocks.step_wall(-1_000);
        assert_eq!(boot.current(&clocks).expect("id"), id);
    }

    #[test]
    fn a_backward_step_over_a_second_rotates() {
        let clocks = ManualClocks::at(1_000_000);
        let boot = Boot::start(&clocks).expect("boot");
        let id = boot.current(&clocks).expect("id");
        clocks.advance(5_000);
        clocks.step_wall(-1_001);
        let rotated = boot.current(&clocks).expect("id");
        assert_ne!(rotated, id);
        // The new mark is the stepped clock: no second rotation.
        clocks.advance(1_000);
        assert_eq!(boot.current(&clocks).expect("id"), rotated);
    }

    #[test]
    fn small_steps_that_add_up_rotate() {
        let clocks = ManualClocks::at(1_000_000);
        let boot = Boot::start(&clocks).expect("boot");
        let id = boot.current(&clocks).expect("id");
        for _ in 0..3 {
            clocks.step_wall(-600);
            clocks.advance(100);
            let _ = boot.current(&clocks).expect("id");
        }
        assert_ne!(boot.current(&clocks).expect("id"), id);
    }

    #[test]
    fn a_forward_jump_then_its_correction_rotates() {
        // A clock set an hour ahead and then corrected has stepped back:
        // nonces pruned by the ahead clock could otherwise replay.
        let clocks = ManualClocks::at(1_000_000);
        let boot = Boot::start(&clocks).expect("boot");
        clocks.step_wall(3_600_000);
        let ahead = boot.current(&clocks).expect("id");
        clocks.step_wall(-3_600_000);
        assert_ne!(boot.current(&clocks).expect("id"), ahead);
    }
}
