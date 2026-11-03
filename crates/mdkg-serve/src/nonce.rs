//! The nonce cache (S7, R15, R109).
//!
//! Each nonce is kept for at least 120 s of monotonic time after it was
//! inserted, and at least until its signature's `created` plus 65 s on the
//! wall clock: 5 s more than the window, so a backward step under the 1 s
//! rotation threshold can't bring a pruned nonce's request back inside it.
//! A forward jump of the wall clock can't evict a nonce early, since the
//! monotonic bound still holds it.
//!
//! Every check and prune takes one [`Observed`] reading from the boot id,
//! never the clocks directly (see [`crate::boot::Boot::observe`]).
//!
//! The set is guarded by one lock, and checking and inserting is one
//! atomic step, so of two concurrent copies of one request exactly one
//! proceeds. Nonces are scoped by key id, so one machine can't use up
//! another's. The cache is lost in a crash, with the boot id it served:
//! nothing signed for the old boot verifies after a restart.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use std::io;

use crate::boot::{BootId, Observed};

/// The signature window (S7): `created` within 60 s of the wall clock.
pub const WINDOW_S: i64 = 60;
/// Monotonic retention after insertion.
pub const MONO_RETENTION: Duration = Duration::from_secs(120);
/// Wall-clock retention after `created`: the window plus 5 s.
pub const WALL_RETENTION_S: i64 = WINDOW_S + 5;
/// The most nonces held at once. Rate limits keep a machine far below
/// this; reaching it refuses new requests rather than evicting a nonce
/// still inside its retention.
pub const DEFAULT_CAPACITY: usize = 1 << 20;

#[derive(Debug, Clone, Copy)]
struct Kept {
    inserted: Duration,
    created_s: i64,
}

impl Kept {
    fn expired(&self, mono: Duration, wall_ms: u64) -> bool {
        mono.saturating_sub(self.inserted) >= MONO_RETENTION
            && (wall_ms / 1000) as i64 >= self.created_s.saturating_add(WALL_RETENTION_S)
    }
}

/// What a check-and-insert found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// New: inserted, and the request may proceed.
    Fresh,
    /// Seen within its retention: `api.replayed`.
    Replayed,
    /// The cache is full of nonces still inside their retention: refuse
    /// for now (`api.rate_limited`), never evict.
    Full,
    /// The boot id rotated since the request's boot check: `api.replayed`.
    BootChanged(BootId),
    /// `created` left the window since the request's window check
    /// (`api.clock_skew`), with the server's time in seconds.
    Skew(i64),
    /// The clocks couldn't be observed (a rotation found no randomness).
    Unobserved,
}

#[derive(Debug)]
pub struct NonceCache {
    kept: Mutex<HashMap<(String, String), Kept>>,
    capacity: usize,
}

impl NonceCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            kept: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Checks `nonce` for `key_id` and inserts it in one step. Call only
    /// after the signature verified, so an unsigned request can't fill the
    /// cache, and before the body is read (S5).
    ///
    /// `observe` (the boot id's [`crate::boot::Boot::observe`]) runs under
    /// the cache's lock, and the boot id and the window are checked again
    /// against that reading. A prune holds the same lock, so a request
    /// that verified slowly can't insert against a reading older than a
    /// prune that dropped its nonce: a nonce is pruned only once the wall
    /// clock is past `created` + 65 s, where the window refuses it (Terra
    /// 1 #6697).
    pub fn check_and_insert(
        &self,
        key_id: &str,
        nonce: &str,
        created_s: i64,
        boot: BootId,
        observe: &dyn Fn() -> io::Result<Observed>,
    ) -> Check {
        match self.check_charge_insert::<()>(key_id, nonce, created_s, boot, observe, &|| Ok(())) {
            Ok(c) => c,
            Err(()) => unreachable!("the charge never fails"),
        }
    }

    /// [`NonceCache::check_and_insert`] with `charge` (a rate) run in the
    /// same critical section, in this order: the boot and window checks
    /// against a reading taken under the lock, the replay check, the
    /// charge, the scan of a full cache (which may refuse as full), the
    /// boot and window checks again against a second reading, and the
    /// insert. While the cache has room, of concurrent copies of one
    /// request only the first is charged and the rest are refused as
    /// replays. A full cache charges before it refuses (#7651), and since
    /// it inserts nothing, each copy sent to it is charged, then refused:
    /// by the charge's own refusal when its rate is spent, otherwise as
    /// full (Terra 7 #8463). A refusal after the charge keeps the charge, and a charge
    /// that refuses inserts nothing (orchestrator #7118, Terra 3 #7115). `charge` must be quick
    /// and take no lock that is held while this cache's is taken.
    pub fn check_charge_insert<E>(
        &self,
        key_id: &str,
        nonce: &str,
        created_s: i64,
        boot: BootId,
        observe: &dyn Fn() -> io::Result<Observed>,
        charge: &dyn Fn() -> Result<(), E>,
    ) -> Result<Check, E> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(at) = observe() else {
            return Ok(Check::Unobserved);
        };
        if at.boot != boot {
            return Ok(Check::BootChanged(at.boot));
        }
        let now_s = (at.wall_ms / 1000) as i64;
        if (created_s - now_s).abs() > WINDOW_S {
            return Ok(Check::Skew(now_s));
        }
        let key = (key_id.to_owned(), nonce.to_owned());
        let (mono, wall_ms) = (at.mono, at.wall_ms);
        if let Some(k) = kept.get(&key)
            && !k.expired(mono, wall_ms)
        {
            return Ok(Check::Replayed);
        }
        // A full cache charges first, so repeated sends of a fresh
        // request are limited like any other before the scan and the
        // refusal (orchestrator #7651, Terra 2 #7643).
        let full = kept.len() >= self.capacity && !kept.contains_key(&key);
        charge()?;
        if full {
            kept.retain(|_, k| !k.expired(mono, wall_ms));
            if kept.len() >= self.capacity {
                return Ok(Check::Full);
            }
        }
        // The charge may have waited on its own lock: the boot and the
        // window are checked again, still under this one, before the
        // insert (#7651).
        let Ok(at) = observe() else {
            return Ok(Check::Unobserved);
        };
        if at.boot != boot {
            return Ok(Check::BootChanged(at.boot));
        }
        let now_s = (at.wall_ms / 1000) as i64;
        if (created_s - now_s).abs() > WINDOW_S {
            return Ok(Check::Skew(now_s));
        }
        kept.insert(
            key,
            Kept {
                inserted: at.mono,
                created_s,
            },
        );
        Ok(Check::Fresh)
    }

    /// Drops every nonce past both retentions (the 60 s timer). The
    /// clocks are observed through the boot id, under the cache's lock, so
    /// the wall clock this prune trusted is the one the boot id answers
    /// for.
    pub fn prune(&self, observe: &dyn Fn() -> io::Result<Observed>) -> io::Result<usize> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let at = observe()?;
        let (mono, wall_ms) = (at.mono, at.wall_ms);
        let before = kept.len();
        kept.retain(|_, k| !k.expired(mono, wall_ms));
        Ok(before - kept.len())
    }

    pub fn len(&self) -> usize {
        self.kept.lock().map(|k| k.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn of_n_concurrent_copies_one_is_charged_whatever_the_interleaving() {
        // Terra 2 #7643: the replay check, the charge and the insert are
        // one critical section, so the outcome doesn't depend on how the
        // threads interleave: exactly one copy is Fresh and charged.
        let clocks = Arc::new(ManualClocks::at(T0));
        let boot = Arc::new(Boot::start(&*clocks).expect("boot"));
        let cache = Arc::new(NonceCache::default());
        let charged = Arc::new(AtomicUsize::new(0));
        let id = boot.current(&*clocks).expect("boot");
        // Every copy is released at once, and the first charge holds the
        // section long enough for the rest to be waiting on it (Terra 4
        // #8189).
        let start = Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (c, b, k, n) = (clocks.clone(), boot.clone(), cache.clone(), charged.clone());
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    k.check_charge_insert::<()>(
                        "m",
                        "n",
                        T0 / 1000,
                        id,
                        &|| b.observe(&*c),
                        &|| {
                            n.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            Ok(())
                        },
                    )
                    .expect("no charge refuses")
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .collect();
        assert_eq!(results.iter().filter(|r| **r == Check::Fresh).count(), 1);
        assert!(
            results
                .iter()
                .all(|r| *r == Check::Fresh || *r == Check::Replayed)
        );
        assert_eq!(charged.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_full_cache_charges_before_refusing_and_a_slow_charge_rechecks_the_clocks() {
        // #7651: on a full cache the charge comes first, so a refusing
        // charge limits repeated sends; and a charge that waited past the
        // window leaves the request refused for skew, not inserted.
        let clocks = Arc::new(ManualClocks::at(T0));
        let boot = Boot::start(&*clocks).expect("boot");
        let id = boot.current(&*clocks).expect("boot");
        let cache = NonceCache::new(1);
        let observe = || boot.observe(&*clocks);
        assert_eq!(
            cache.check_charge_insert::<()>("m", "a", T0 / 1000, id, &observe, &|| Ok(())),
            Ok(Check::Fresh)
        );
        let limited =
            cache.check_charge_insert("m", "b", T0 / 1000, id, &observe, &|| Err("limited"));
        assert_eq!(
            limited,
            Err("limited"),
            "charged before the full-cache refusal"
        );
        let fresh = NonceCache::default();
        let slow = fresh.check_charge_insert::<()>("m", "c", T0 / 1000, id, &observe, &|| {
            clocks.advance(61_000);
            Ok(())
        });
        assert_eq!(slow, Ok(Check::Skew(T0 / 1000 + 61)));
        assert!(fresh.is_empty());
    }

    #[test]
    fn a_boot_rotation_inside_the_charge_inserts_nothing() {
        // Terra 4 #8189: a backward wall-clock step while the charge runs
        // rotates the boot id; the second reading sees it, and the request
        // is refused for the new boot with no nonce kept.
        let clocks = Arc::new(ManualClocks::at(T0));
        let boot = Boot::start(&*clocks).expect("boot");
        let id = boot.current(&*clocks).expect("boot");
        let cache = NonceCache::default();
        let got = cache.check_charge_insert::<()>(
            "m",
            "a",
            T0 / 1000,
            id,
            &|| boot.observe(&*clocks),
            &|| {
                clocks.step_wall(-5_000);
                Ok(())
            },
        );
        let now = boot.current(&*clocks).expect("boot");
        assert_ne!(now, id, "the step rotated the boot id");
        assert_eq!(got, Ok(Check::BootChanged(now)));
        assert!(cache.is_empty());
    }

    use super::*;
    use crate::boot::Boot;
    use crate::boot::test_clocks::ManualClocks;
    use std::sync::Arc;

    fn ins(
        cache: &NonceCache,
        boot: &Boot,
        clocks: &ManualClocks,
        key: &str,
        nonce: &str,
        created: i64,
    ) -> Check {
        let id = boot.current(clocks).expect("boot");
        cache.check_and_insert(key, nonce, created, id, &|| boot.observe(clocks))
    }

    fn prune(cache: &NonceCache, boot: &Boot, clocks: &ManualClocks) -> usize {
        cache.prune(&|| boot.observe(clocks)).expect("prune")
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    const T0: i64 = 1_793_290_515_000;

    #[test]
    fn a_nonce_is_used_once_per_key() {
        let clocks = ManualClocks::at(T0);
        let boot = Boot::start(&clocks).expect("boot");
        let cache = NonceCache::default();
        let created = T0 / 1000;
        assert_eq!(
            ins(&cache, &boot, &clocks, "m1", "n", created),
            Check::Fresh
        );
        assert_eq!(
            ins(&cache, &boot, &clocks, "m1", "n", created),
            Check::Replayed
        );
        assert_eq!(
            ins(&cache, &boot, &clocks, "m2", "n", created),
            Check::Fresh
        );
    }

    #[test]
    fn kept_for_120_s_monotonic_even_if_the_wall_jumps() {
        let clocks = ManualClocks::at(T0);
        let boot = Boot::start(&clocks).expect("boot");
        let cache = NonceCache::default();
        let created = T0 / 1000;
        ins(&cache, &boot, &clocks, "m", "n", created);
        clocks.step_wall(3_600_000);
        clocks.mono.fetch_add(119_999, Ordering::SeqCst);
        assert_eq!(prune(&cache, &boot, &clocks), 0);
        // The jumped clock also puts the copy outside the window.
        assert!(matches!(
            ins(&cache, &boot, &clocks, "m", "n", created),
            Check::Skew(_)
        ));
        clocks.mono.fetch_add(1, Ordering::SeqCst);
        assert_eq!(prune(&cache, &boot, &clocks), 1);
    }

    #[test]
    fn kept_until_created_plus_65_s_on_the_wall() {
        let clocks = ManualClocks::at(T0);
        let boot = Boot::start(&clocks).expect("boot");
        let cache = NonceCache::default();
        // Signed 60 s in the future, the window's edge.
        let created = T0 / 1000 + 60;
        ins(&cache, &boot, &clocks, "m", "n", created);
        clocks.advance(120_000);
        assert_eq!(
            prune(&cache, &boot, &clocks),
            0,
            "created + 65 s is still ahead"
        );
        assert_eq!(
            ins(&cache, &boot, &clocks, "m", "n", created),
            Check::Replayed
        );
        clocks.advance(5_000);
        assert_eq!(prune(&cache, &boot, &clocks), 1);
    }

    #[test]
    fn a_full_cache_refuses_and_never_evicts_a_live_nonce() {
        let clocks = ManualClocks::at(T0);
        let boot = Boot::start(&clocks).expect("boot");
        let cache = NonceCache::new(2);
        let created = T0 / 1000;
        ins(&cache, &boot, &clocks, "m", "a", created);
        ins(&cache, &boot, &clocks, "m", "b", created);
        assert_eq!(ins(&cache, &boot, &clocks, "m", "c", created), Check::Full);
        assert_eq!(
            ins(&cache, &boot, &clocks, "m", "a", created),
            Check::Replayed
        );
        clocks.advance(125_000);
        let now = (T0 + 125_000) / 1000;
        assert_eq!(ins(&cache, &boot, &clocks, "m", "c", now), Check::Fresh);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_boot_and_window_are_checked_again_at_insert() {
        let clocks = ManualClocks::at(T0);
        let boot = Boot::start(&clocks).expect("boot");
        let cache = NonceCache::default();
        let id = boot.current(&clocks).expect("boot");
        // The clock stepped back after the request's own boot check.
        clocks.advance(5_000);
        clocks.step_wall(-2_000);
        let r = cache.check_and_insert("m", "n", T0 / 1000, id, &|| boot.observe(&clocks));
        assert!(matches!(r, Check::BootChanged(b) if b != id));
        // The window closed while the request verified.
        let id = boot.current(&clocks).expect("boot");
        let created = (crate::boot::Clocks::wall_ms(&clocks) / 1000) as i64 - 60;
        clocks.advance(2_000);
        let r = cache.check_and_insert("m", "n", created, id, &|| boot.observe(&clocks));
        assert!(matches!(r, Check::Skew(_)));
        assert!(cache.is_empty());
    }

    #[test]
    fn concurrent_copies_pass_exactly_once() {
        let clocks = Arc::new(ManualClocks::at(T0));
        let boot = Arc::new(Boot::start(&*clocks).expect("boot"));
        let cache = Arc::new(NonceCache::default());
        let passed = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (clocks, boot, cache, passed) =
                    (clocks.clone(), boot.clone(), cache.clone(), passed.clone());
                std::thread::spawn(move || {
                    if ins(&cache, &boot, &clocks, "m", "same", T0 / 1000) == Check::Fresh {
                        passed.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("thread");
        }
        assert_eq!(passed.load(Ordering::SeqCst), 1);
    }
}
