//! The log tailer and the SSE fan-out (the serve edd's Process model and
//! Transport: Streams).
//!
//! One tailer reads the store's log for every `GET /v1/events` stream. At
//! each 1 s tick it checks that the record at its position is still the
//! one it read (its hash anchor), and only then, when `PRAGMA data_version` moved, closes the streams of
//! machines no longer approved and reads the records after its position,
//! rendered as events by [`LogSource`] (a-events' renderer, behind the
//! trait). It only reads: nothing here writes the store. A record renders
//! at most one event (A9); a record that carries more is refused as a
//! corrupt log, so a resume never has to point inside a record
//! (orchestrator #9998).
//!
//! Each stream is a [`Subscription`] with one byte budget, 1 MiB, shared
//! by its backfill page and its live queue: a stream whose client can't
//! keep up within it is closed, and the client resumes from its cursor;
//! `serve` never buffers more. A resuming stream subscribes first, then
//! backfills from the store up to the tailer's position at that moment, a
//! page at a time as its writer asks, ahead of live events; live events
//! the backfill or the client's cursor covers are dropped, so a resume
//! has no gap and no repeat.
//!
//! A stream is written to only while its grant holds: asked before every
//! event and heartbeat, between backfill reads, at least once a second
//! while its writer waits, and every second for every stream by a grant
//! monitor the tailer starts, apart from its tick, so neither a blocked
//! writer nor a failing log read delays it (Terra 6 #10224, Claude
//! #10058). The monitor asks the grants in turn, so one grant waiting on
//! a busy store delays the check of those after it; every grant reads
//! the same store, so that wait is the store's (Terra 3 #10383). A grant
//! that panics is taken as failed, and the monitor goes on. A
//! revocation cuts the stream's connection through the hook its writer
//! set, so a writer blocked on a socket is released, even when the stream
//! had already closed for another reason. A bounded residual: an event
//! that passed its check just before a revoke committed may still be
//! written, at most one per stream (#10122, orchestrator #10141).
//!
//! A store restored, replaced or forked under the tailer is caught by the
//! anchor: the record at the tailer's position no longer hashes as it did,
//! or a page doesn't follow on from it. Every stream then closes `Reset`
//! (a-events' cursor-reset diagnostic), and the tailer starts again at the
//! new head, never skipping records behind a stale position (Terra 1
//! #9989, Terra 6 #10224).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mdkg_ops::wire::{Event, KeyDirectory, MachineState};

use crate::refusal::Refusal;

/// The store failed; the tick is retried at the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreFailed;

/// A durable event and the log `seq` it renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailed {
    pub seq: u64,
    pub event: Event,
}

/// A record's hash anchor, as a-events' cursors carry it; `None` for
/// `seq` 0, before the first record, and for a record the log doesn't
/// have.
pub type Anchor = Option<String>;

/// One page of the log, read in one transaction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    /// In `seq` order, at most one per record.
    pub events: Vec<Tailed>,
    /// The anchor of the record the page was read after, as this read saw
    /// it: a page follows on from a position only when it matches.
    pub after_anchor: Anchor,
    /// The last record read, whether or not it rendered an event; `None`
    /// when there was none.
    pub through: Option<u64>,
    /// That record's anchor.
    pub through_anchor: Anchor,
    /// Whether records remain after `through`.
    pub more: bool,
}

/// The log, read-only, with events rendered (a-events implements it).
/// It reads through a connection of its own, never one `serve` writes
/// on, and keeps that one connection for every `data_version` read:
/// `PRAGMA data_version` moves only for other connections' commits, and
/// only as one connection sees it, so a fresh connection per read would
/// see no change (Opus #9957, Terra 2 #10122).
pub trait LogSource: Send + Sync {
    /// `PRAGMA data_version`, on the source's one connection.
    fn data_version(&self) -> Result<i64, StoreFailed>;

    /// The log head's `seq` and its anchor, read together.
    fn head(&self) -> Result<(u64, Anchor), StoreFailed>;

    /// The anchor of the record `seq`.
    fn anchor(&self, seq: u64) -> Result<Anchor, StoreFailed>;

    /// The events of up to `records` records after `seq`, in order, in
    /// one read, stopping before a record whose event would take them
    /// past `max_bytes` (at least one record is always read). An event
    /// larger than a stream's whole budget is legitimate data that can't
    /// be carried: each stream gets [`Next::TooLarge`] in its place, with
    /// its cursor, and carries on past it (orchestrator #10487, Terra 2
    /// #10469). A rendered event's `data` is never empty (it is the
    /// compact JSON line the CLI prints): the fan-out uses empty `data` as
    /// that stand-in, so a source must never render one (Terra 4 #10518).
    fn events_after(&self, seq: u64, records: usize, max_bytes: usize)
    -> Result<Page, StoreFailed>;

    /// A stream's `after` cursor as a `seq`, refused with a-events'
    /// cursor codes (`api.bad_cursor`, `api.cursor_foreign`, …).
    fn cursor_seq(&self, cursor: &str) -> Result<u64, Refusal>;
}

/// How many records one page holds.
pub const PAGE: usize = 512;

/// The most pages one tick reads; a longer backlog is read on the next
/// ticks, so a tick always returns (Terra 1 #9989).
pub const PAGES_PER_TICK: usize = 64;

/// A9's heartbeat, every 15 s of silence (S4).
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// The most one stream buffers, backfill and live together (S4).
pub const STREAM_BUFFER: usize = 1024 * 1024;

/// How often a waiting stream rechecks its grant: a revocation is seen
/// within this, whatever the tailer does (S6, R17).
pub const GRANT_CHECK: Duration = Duration::from_secs(1);

/// Why a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// `serve` is stopping: a normal close; the client resumes (A15).
    Draining,
    /// The client fell behind by more than the buffer.
    Overflow,
    /// The stream's machine is no longer approved.
    MachineRevoked,
    /// The person grant the stream was opened under lapsed, its machine
    /// still approved: never reported as the machine's revocation
    /// (orchestrator #10592, Terra 4 #10578).
    PersonLapsed,
    /// The store failed: a backfill read, or a grant that couldn't be
    /// read, never taken as valid; the client resumes.
    Failed,
    /// The store was restored, replaced or forked: the client's cursor is
    /// stale, a-events' cursor-reset diagnostic.
    Reset,
    /// A record carried more than one event: a corrupt log.
    Corrupt,
}

impl Closed {
    /// Whether the stream's connection is cut, not ended at a frame: the
    /// writer may be blocked on a peer that stopped reading.
    fn cuts(self) -> bool {
        matches!(
            self,
            Closed::MachineRevoked | Closed::PersonLapsed | Closed::Overflow
        )
    }
}

/// Why a tick failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickFailed {
    /// Retried at the next tick.
    Store,
    /// The record `seq` carried more than one event (A9), or couldn't be
    /// read [`READ_FAILURES`] ticks running while `data_version` still
    /// read: every stream was closed `Corrupt`, and the tailer stays
    /// before it. Returned once, when found, for the caller to log;
    /// [`Tailer::corrupt_at`] reports it until the log is repaired, by
    /// `mdkg store restore` from the last snapshot before it, after which
    /// the anchor change resets the tailer (Claude #10145, orchestrator
    /// #10487).
    Corrupt { seq: u64 },
}

impl From<StoreFailed> for TickFailed {
    fn from(_: StoreFailed) -> Self {
        TickFailed::Store
    }
}

/// What a stream's writer gets next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// An event. One with an empty `id` is ephemeral and carries no
    /// cursor: the writer sends no `id:`, so the client's last cursor
    /// stands (Terra 1 #9989).
    Event(Event),
    /// An event too large for any stream's budget: the writer sends an
    /// `mdkg.stream.error` frame naming `seq` and `event_too_large`, with
    /// `id` the record's cursor, so a resume passes it, and the stream
    /// carries on (orchestrator #10487).
    TooLarge {
        id: String,
        seq: u64,
    },
    /// Nothing for the heartbeat interval: send A9's heartbeat.
    Heartbeat,
    Closed(Closed),
}

/// How many ticks running a read may fail at one position, while
/// `data_version` still reads, before the record there is taken as corrupt
/// (orchestrator #10487). A busy store fails `data_version` too, and keeps
/// waiting.
pub const READ_FAILURES: u32 = 30;

/// What a stream carries for an event: the event, or, past the whole
/// budget, its stand-in, the cursor alone with no data.
fn carriable(e: &Event, buffer: usize) -> Event {
    if size(e) > buffer {
        Event {
            id: e.id.clone(),
            data: String::new(),
        }
    } else {
        e.clone()
    }
}

/// What the writer gets for a queued item.
fn handed(seq: u64, mut e: Event) -> Next {
    if seq == 0 {
        // Ephemeral: no cursor of its own.
        e.id.clear();
        Next::Event(e)
    } else if e.data.is_empty() {
        // A rendered event is never empty: this is a too-large stand-in.
        Next::TooLarge { id: e.id, seq }
    } else {
        Next::Event(e)
    }
}

/// Whether a stream may still be written to: its machine still approved,
/// and the person grant it was opened under, if any, still valid, as the
/// store holds them now, never a snapshot from before (orchestrator
/// #9962). A read that fails is an error, which closes the stream
/// `Failed`, never taken as a lapse or as valid (Terra 2 #10122).
pub type Grant = Arc<dyn Fn() -> Result<Granted, StoreFailed> + Send + Sync>;

/// What a stream's grant found, a store error apart: which check failed,
/// so the close reports it truthfully (orchestrator #10592).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granted {
    Valid,
    MachineRevoked,
    PersonLapsed,
}

/// Cuts a stream's connection (its closer), for a writer blocked in a
/// write.
pub type Cut = Box<dyn Fn() + Send + Sync>;

/// What a grant says of the stream: `None` while it holds. A grant that
/// panics is a failed one, so the monitor, and the writer, carry on
/// (Terra 3 #10383).
fn ask(grant: &Grant) -> Option<Closed> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| grant())) {
        Ok(Ok(Granted::Valid)) => None,
        Ok(Ok(Granted::MachineRevoked)) => Some(Closed::MachineRevoked),
        Ok(Ok(Granted::PersonLapsed)) => Some(Closed::PersonLapsed),
        Ok(Err(StoreFailed)) | Err(_) => Some(Closed::Failed),
    }
}

#[derive(Debug)]
struct Queue {
    events: VecDeque<(u64, Event)>,
    /// The live queue's bytes.
    bytes: usize,
    /// The backfill page's bytes still held: the one budget counts both
    /// (Terra 2 #10122, Terra 6 #10224).
    back_bytes: usize,
    closed: Option<Closed>,
    /// Whether the connection was cut.
    cut: bool,
    /// Live events at or below this `seq` were already sent by the
    /// backfill, or are covered by the client's cursor.
    skip_through: u64,
}

struct Sub {
    machine: String,
    queue: Mutex<Queue>,
    ready: Condvar,
    cut: Mutex<Option<Cut>>,
    grant: Grant,
}

impl Sub {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Closes the stream with `why`, the first reason kept, and drops
    /// what it queued. A revocation or an overflow also cuts its
    /// connection, once, even when it had already closed for another
    /// reason (Terra 6 #10224). The reason it holds.
    fn close(&self, why: Closed) -> Closed {
        let (held, cut_now) = {
            let mut q = self.lock();
            if q.closed.is_none() {
                q.events.clear();
                q.bytes = 0;
                q.closed = Some(why);
            }
            let cut_now = why.cuts() && !q.cut;
            q.cut |= cut_now;
            (q.closed.unwrap_or(why), cut_now)
        };
        self.ready.notify_all();
        if cut_now && let Some(cut) = self.cut.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            cut();
        }
        held
    }
}

fn size(e: &Event) -> usize {
    e.id.len() + e.data.len() + 16
}

/// Whether two events of a page share a record.
fn repeated(events: &[Tailed]) -> Option<u64> {
    events
        .windows(2)
        .find(|w| w[0].seq == w[1].seq)
        .map(|w| w[0].seq)
}

/// A resume's backfill still to send, read a page at a time.
#[derive(Default)]
struct Backfill {
    source: Option<Arc<dyn LogSource>>,
    /// The last record read.
    from: u64,
    /// Its anchor, once a page has been read: each next page must follow
    /// on from it.
    from_anchor: Option<Anchor>,
    /// The tailer's position when the stream resumed, and its anchor:
    /// where the backfill must end.
    through: u64,
    through_anchor: Anchor,
    page: VecDeque<(u64, Event)>,
}

/// What a stream's writer reads its frames from: a fan-out
/// [`Subscription`] for `GET /v1/events`; a turn's output and `msg wait`
/// adapt their own sources to it.
pub trait Frames: Send {
    /// The next frame, as [`Subscription::next`].
    fn next(&mut self) -> Next;
    /// The hook that cuts the stream's connection.
    fn set_cut(&self, cut: Cut);
}

impl Frames for Subscription {
    fn next(&mut self) -> Next {
        Subscription::next(self)
    }
    fn set_cut(&self, cut: Cut) {
        Subscription::set_cut(self, cut);
    }
}

/// One stream's end of the fan-out. Dropping it unsubscribes.
pub struct Subscription {
    sub: Arc<Sub>,
    fanout: Arc<Fanout>,
    heartbeat: Duration,
    back: Mutex<Backfill>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Subscription {
    /// The hook that cuts this stream's connection, for a writer blocked
    /// in a write when it is revoked (Terra 1 #9989).
    pub fn set_cut(&self, cut: Cut) {
        *self.sub.cut.lock().unwrap_or_else(|e| e.into_inner()) = Some(cut);
    }

    /// The next event, or a heartbeat after `heartbeat` of silence, or the
    /// close. A backfill's events come first, read as they are asked for.
    /// Whether the stream is open and its grant valid is checked before
    /// anything is read and again before anything is handed to the
    /// writer (orchestrator #9962, Opus #9957).
    pub fn next(&self) -> Next {
        if let Some(closed) = self.check() {
            return closed;
        }
        let next = self.next_unchecked();
        if let Next::Closed(_) = next {
            self.drop_backfill();
            return next;
        }
        self.check().unwrap_or(next)
    }

    /// `Closed` when the stream was closed, or its grant no longer holds,
    /// which closes it; the backfill is dropped either way. The queue's
    /// lock is released before the grant is asked (Claude #10058).
    fn check(&self) -> Option<Next> {
        let closed = self.sub.lock().closed;
        let closed = match closed {
            Some(c) => c,
            None => self.sub.close(ask(&self.sub.grant)?),
        };
        self.drop_backfill();
        Some(Next::Closed(closed))
    }

    fn drop_backfill(&self) {
        let mut b = self.back.lock().unwrap_or_else(|e| e.into_inner());
        b.source = None;
        b.page.clear();
        self.sub.lock().back_bytes = 0;
    }

    fn next_unchecked(&self) -> Next {
        if let Some(n) = self.next_backfilled() {
            return n;
        }
        let deadline = Instant::now() + self.heartbeat;
        let mut q = self.sub.lock();
        loop {
            if let Some(c) = q.closed {
                return Next::Closed(c);
            }
            if let Some((seq, e)) = q.events.pop_front() {
                q.bytes = q.bytes.saturating_sub(size(&e));
                return handed(seq, e);
            }
            let now = Instant::now();
            if now >= deadline {
                return Next::Heartbeat;
            }
            q = self
                .sub
                .ready
                .wait_timeout(q, (deadline - now).min(GRANT_CHECK))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
            // At least once a second, whatever the tailer does; asked
            // with the queue unlocked, so a grant that reads the store
            // never holds up the fan-out's deliveries (Claude #10058).
            if q.closed.is_none() {
                drop(q);
                if let Some(why) = ask(&self.sub.grant) {
                    return Next::Closed(self.sub.close(why));
                }
                q = self.sub.lock();
            }
        }
    }

    /// The next backfilled event, reading the next page when the last is
    /// spent; `None` once the backfill is done, or when there is none.
    fn next_backfilled(&self) -> Option<Next> {
        let mut b = self.back.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some((seq, e)) = b.page.pop_front() {
                let mut q = self.sub.lock();
                q.back_bytes = q.back_bytes.saturating_sub(size(&e));
                return Some(handed(seq, e));
            }
            let source = b.source.clone()?;
            if b.from >= b.through {
                b.source = None;
                // It must end on the record the tailer stood on.
                if b.from_anchor
                    .as_ref()
                    .is_some_and(|a| *a != b.through_anchor)
                {
                    return Some(Next::Closed(self.sub.close(Closed::Reset)));
                }
                return None;
            }
            // Closed or lapsed meanwhile, silent pages included: no
            // further read (Opus #9957, Terra 2 #10122).
            let open = self.sub.lock().closed;
            if let Some(c) = open.or_else(|| ask(&self.sub.grant)) {
                b.source = None;
                return Some(Next::Closed(self.sub.close(c)));
            }
            let room = {
                let q = self.sub.lock();
                self.fanout.buffer.saturating_sub(q.bytes + q.back_bytes)
            };
            if room == 0 {
                b.source = None;
                return Some(Next::Closed(self.sub.close(Closed::Overflow)));
            }
            // Never past where it must end, so the last page ends on it.
            let records = usize::try_from(b.through - b.from)
                .unwrap_or(PAGE)
                .min(PAGE);
            let page = match source.events_after(b.from, records, room) {
                Err(StoreFailed) => {
                    b.source = None;
                    b.page.clear();
                    // The reason recorded first, should it already have
                    // closed (Opus #9957).
                    return Some(Next::Closed(self.sub.close(Closed::Failed)));
                }
                Ok(page) => page,
            };
            if self.fanout.unsendable(&page.events).is_some() {
                b.source = None;
                return Some(Next::Closed(self.sub.close(Closed::Corrupt)));
            }
            let follows = b
                .from_anchor
                .as_ref()
                .is_none_or(|a| *a == page.after_anchor);
            let advanced = page.through.filter(|t| *t > b.from && *t <= b.through);
            let Some(last) = advanced.filter(|_| follows) else {
                // Another history, or one shorter than the tailer saw.
                b.source = None;
                return Some(Next::Closed(self.sub.close(Closed::Reset)));
            };
            let buffer = self.fanout.buffer;
            let carried: Vec<(u64, Event)> = page
                .events
                .iter()
                .map(|t| (t.seq, carriable(&t.event, buffer)))
                .collect();
            let bytes: usize = carried.iter().map(|(_, e)| size(e)).sum();
            if bytes > room {
                // More than the room left: the client is too far behind.
                b.source = None;
                return Some(Next::Closed(self.sub.close(Closed::Overflow)));
            }
            self.sub.lock().back_bytes += bytes;
            b.page.extend(carried);
            b.from = last;
            b.from_anchor = Some(page.through_anchor);
        }
    }

    /// Resumes after `after_seq`, the client's cursor: the records from
    /// there up to the tailer's position now come first, read from
    /// `source` as the writer asks, and live events at or below either
    /// are dropped, so a cursor ahead of the tailer (a list read just
    /// before) admits nothing it covers (Terra 1 #9989). Called once,
    /// right after subscribing.
    pub fn backfill(&self, source: Arc<dyn LogSource>, after_seq: u64) {
        let (through, through_anchor) = self.fanout.position_anchored();
        let covered = through.max(after_seq);
        {
            let mut q = self.sub.lock();
            q.skip_through = q.skip_through.max(covered);
            q.events.retain(|(seq, _)| *seq > covered || *seq == 0);
            q.bytes = q.events.iter().map(|(_, e)| size(e)).sum();
        }
        *self.back.lock().unwrap_or_else(|e| e.into_inner()) = Backfill {
            source: Some(source),
            from: after_seq,
            from_anchor: None,
            through,
            through_anchor,
            page: VecDeque::new(),
        };
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut subs = self.fanout.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| !Arc::ptr_eq(s, &self.sub));
    }
}

/// The tailer's position: the last record published, and its anchor.
struct Position {
    seq: u64,
    anchor: Anchor,
}

/// The fan-out: every open stream.
pub struct Fanout {
    subs: Mutex<Vec<Arc<Sub>>>,
    position: Mutex<Position>,
    buffer: usize,
    /// The coordinator host's machine id: only its streams see enrolment
    /// notifications (S6; orchestrator #9998).
    host: String,
    /// The corrupt record the tailer stands before, while it does: a new
    /// stream is refused (Terra 2 #10378).
    corrupt: Mutex<Option<u64>>,
}

impl std::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fanout").finish_non_exhaustive()
    }
}

impl Fanout {
    /// At `seq`, whose anchor is `anchor`; `buffer` is the bytes one
    /// stream may hold (1 MiB, S4).
    pub fn new(seq: u64, anchor: Anchor, buffer: usize, host: &str) -> Arc<Self> {
        Arc::new(Self {
            subs: Mutex::new(Vec::new()),
            position: Mutex::new(Position { seq, anchor }),
            buffer,
            host: host.to_owned(),
            corrupt: Mutex::new(None),
        })
    }

    /// A new stream, as [`Fanout::subscribe`], unless the tailer stands
    /// before a corrupt record: then `coord.store_failed` (500, which
    /// clients don't retry), naming its `seq` (Terra 2 #10378,
    /// orchestrator #10487).
    pub fn open(
        self: &Arc<Self>,
        machine: &str,
        heartbeat: Duration,
        grant: Grant,
    ) -> Result<Subscription, Refusal> {
        if let Some(seq) = *self.corrupt.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(Refusal::new(
                mdkg_format::Code::CoordStoreFailed,
                mdkg_graph::ErrorKind::Failure,
                "corrupt_log",
            )
            .with("seq", serde_json::Value::from(seq)));
        }
        Ok(self.subscribe(machine, heartbeat, grant))
    }

    fn set_corrupt(&self, seq: Option<u64>) {
        *self.corrupt.lock().unwrap_or_else(|e| e.into_inner()) = seq;
    }

    /// A record no stream can carry: more than one event (A9). An event
    /// past the budget is carried as its stand-in instead.
    fn unsendable(&self, events: &[Tailed]) -> Option<u64> {
        repeated(events)
    }

    fn pos(&self) -> std::sync::MutexGuard<'_, Position> {
        self.position.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn position(&self) -> u64 {
        self.pos().seq
    }

    fn position_anchored(&self) -> (u64, Anchor) {
        let p = self.pos();
        (p.seq, p.anchor.clone())
    }

    /// A new stream for `machine`, written to only while `grant` holds.
    pub fn subscribe(
        self: &Arc<Self>,
        machine: &str,
        heartbeat: Duration,
        grant: Grant,
    ) -> Subscription {
        let sub = Arc::new(Sub {
            machine: machine.to_owned(),
            queue: Mutex::new(Queue {
                events: VecDeque::new(),
                bytes: 0,
                back_bytes: 0,
                closed: None,
                cut: false,
                skip_through: 0,
            }),
            ready: Condvar::new(),
            cut: Mutex::new(None),
            grant,
        });
        self.subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sub.clone());
        Subscription {
            sub,
            fanout: self.clone(),
            heartbeat,
            back: Mutex::new(Backfill::default()),
        }
    }

    fn subs(&self) -> Vec<Arc<Sub>> {
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Queues `event` on every stream, or on `only` that machine's.
    fn deliver(&self, seq: u64, event: &Event, only: Option<&str>) {
        for s in self.subs() {
            if only.is_some_and(|m| m != s.machine) {
                continue;
            }
            let overflow = {
                let mut q = s.lock();
                if q.closed.is_some() || (seq != 0 && seq <= q.skip_through) {
                    continue;
                }
                let n = size(event);
                if q.bytes + q.back_bytes + n > self.buffer {
                    true
                } else {
                    q.bytes += n;
                    q.events.push_back((seq, event.clone()));
                    false
                }
            };
            if overflow {
                s.close(Closed::Overflow);
            } else {
                s.ready.notify_all();
            }
        }
    }

    /// Publishes a page's durable events in `seq` order and moves the
    /// position, with its anchor, to the last record read, past records
    /// that rendered none. A page in which a record carries more than one
    /// event is refused whole, its `seq` returned, and nothing of it
    /// published (#9998).
    pub fn publish(&self, page: &Page) -> Result<(), u64> {
        if let Some(seq) = self.unsendable(&page.events) {
            return Err(seq);
        }
        let mut pos = self.pos();
        let start = pos.seq;
        for t in page.events.iter().filter(|t| t.seq > start) {
            self.deliver(t.seq, &carriable(&t.event, self.buffer), None);
        }
        if let Some(through) = page.through.filter(|t| *t > start) {
            pos.seq = through;
            pos.anchor = page.through_anchor.clone();
        }
        Ok(())
    }

    /// Publishes an enrolment's ephemeral event (`durable: false`) to the
    /// host's streams alone (S6, orchestrator #9998): no record and no
    /// cursor of its own, never replayed.
    pub fn publish_enrolment(&self, event: &Event) {
        let _pos = self.pos();
        self.deliver(0, event, Some(&self.host));
    }

    /// Closes every stream with `why`.
    pub fn close_all(&self, why: Closed) {
        for s in self.subs() {
            s.close(why);
        }
    }

    /// Runs [`Fanout::check_grants`] every `every` on a thread of its
    /// own, for as long as the fan-out lives.
    pub fn watch_grants(self: &Arc<Self>, every: Duration) {
        let fanout = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                let Some(f) = fanout.upgrade() else { return };
                f.check_grants();
            }
        });
    }

    /// Asks every stream's grant, with no lock held, and closes those
    /// that no longer hold, cutting their connections: whether or not
    /// their writers are blocked, and whatever the log does (Terra 6
    /// #10224). A stream already cut is skipped.
    pub fn check_grants(&self) -> usize {
        let mut closed = 0;
        for s in self.subs() {
            if s.lock().cut {
                continue;
            }
            if let Some(why) = ask(&s.grant) {
                s.close(why);
                closed += 1;
            }
        }
        closed
    }

    /// Closes the streams of every machine whose key isn't approved now,
    /// cutting a connection still open even when its stream had closed
    /// for another reason.
    pub fn close_revoked(&self, keys: &dyn KeyDirectory) -> usize {
        let mut closed = 0;
        for s in self.subs() {
            let approved = keys
                .machine(&s.machine)
                .is_some_and(|k| k.state == MachineState::Approved);
            if !approved && !s.lock().cut {
                s.close(Closed::MachineRevoked);
                closed += 1;
            }
        }
        closed
    }

    /// A store restored, replaced or forked: every stream closes `Reset`,
    /// and the position starts again at `head`.
    fn reset(&self, head: u64, anchor: Anchor) {
        let mut pos = self.pos();
        for s in self.subs() {
            s.close(Closed::Reset);
        }
        *pos = Position { seq: head, anchor };
    }

    /// Open streams, for `serve status`.
    pub fn open_streams(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }
}

/// The tailer: one per `serve`.
pub struct Tailer {
    source: Arc<dyn LogSource>,
    fanout: Arc<Fanout>,
    version: Option<i64>,
    /// A record found carrying more than one event, or unreadable, until
    /// the log is repaired.
    corrupt: Option<u64>,
    /// A read failing at a position, and how many ticks running.
    failing: Option<(u64, u32)>,
}

impl std::fmt::Debug for Tailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tailer")
            .field("version", &self.version)
            .field("corrupt", &self.corrupt)
            .finish_non_exhaustive()
    }
}

impl Tailer {
    /// Starts at the log head (Lifecycle, step 5); `host` is the
    /// coordinator host's machine id.
    pub fn start(
        source: Arc<dyn LogSource>,
        buffer: usize,
        host: &str,
    ) -> Result<Self, StoreFailed> {
        let (head, anchor) = source.head()?;
        let fanout = Fanout::new(head, anchor, buffer, host);
        fanout.watch_grants(GRANT_CHECK);
        Ok(Self {
            fanout,
            source,
            version: None,
            corrupt: None,
            failing: None,
        })
    }

    pub fn fanout(&self) -> &Arc<Fanout> {
        &self.fanout
    }

    /// The record found carrying more than one event, or unreadable, for
    /// `serve status`, until the log is repaired (Claude #10145,
    /// orchestrator #10487).
    pub fn corrupt_at(&self) -> Option<u64> {
        self.corrupt
    }

    fn reset(&mut self) -> Result<(), StoreFailed> {
        let (head, anchor) = self.source.head()?;
        self.fanout.reset(head, anchor);
        self.corrupt = None;
        self.fanout.set_corrupt(None);
        self.failing = None;
        Ok(())
    }

    /// Stops every stream before the corrupt record `seq`: reported once.
    fn corrupt(&mut self, seq: u64) -> Result<(), TickFailed> {
        // The flag first, so a stream opened meanwhile is refused or closed
        // here, never left open until the next tick (Terra 4 #10578).
        self.fanout.set_corrupt(Some(seq));
        self.fanout.close_all(Closed::Corrupt);
        let first = self.corrupt != Some(seq);
        self.corrupt = Some(seq);
        if first {
            Err(TickFailed::Corrupt { seq })
        } else {
            Ok(())
        }
    }

    /// A read of the one record after `from` failed, with `data_version`
    /// read just before: after [`READ_FAILURES`] ticks running at that
    /// position that record is taken as corrupt (orchestrator #10487).
    fn failed_read(&mut self, from: u64) -> Result<usize, TickFailed> {
        let count = match self.failing {
            Some((at, n)) if at == from => n + 1,
            _ => 1,
        };
        self.failing = Some((from, count));
        if count >= READ_FAILURES {
            self.corrupt(from + 1)?;
            return Ok(0);
        }
        Err(TickFailed::Store)
    }

    /// The 1 s tick. Grants are asked apart from it (`watch_grants`).
    /// 1. The record at the position must still hash as it did, whether
    ///    or not `data_version` moved, so a store replaced without a
    ///    commit on this connection is caught; else every stream resets.
    /// 2. When `data_version` moved: the streams of machines `keys` no
    ///    longer has approved close (`keys` reads the store's state now;
    ///    Opus #9957), and the records after the position are published,
    ///    each page following on from it, at most [`PAGES_PER_TICK`]
    ///    pages, the rest at the next ticks.
    ///
    /// Returns how many events were published.
    pub fn tick(&mut self, keys: &dyn KeyDirectory) -> Result<usize, TickFailed> {
        let (held, anchor) = self.fanout.position_anchored();
        if self.source.anchor(held)? != anchor {
            self.reset()?;
            return Ok(0);
        }
        let v = self.source.data_version()?;
        if self.version == Some(v) && self.corrupt.is_none() {
            return Ok(0);
        }
        self.fanout.close_revoked(keys);
        let mut published = 0;
        for _ in 0..PAGES_PER_TICK {
            let (from, anchor) = self.fanout.position_anchored();
            // A page that can't be read is read again one record at a
            // time, so the records before the bad one are published and
            // the failure is counted against its exact `seq` (Terra 4
            // #10518).
            let page = match self
                .source
                .events_after(from, PAGE, usize::MAX)
                .or_else(|StoreFailed| self.source.events_after(from, 1, usize::MAX))
            {
                Ok(page) => {
                    self.failing = None;
                    page
                }
                Err(StoreFailed) => return self.failed_read(from),
            };
            if page.after_anchor != anchor {
                // Replaced between the check and the read.
                self.reset()?;
                return Ok(published);
            }
            if let Err(seq) = self.fanout.publish(&page) {
                return self.corrupt(seq).map(|()| published);
            }
            if self.corrupt.take().is_some() {
                self.fanout.set_corrupt(None);
            }
            published += page.events.iter().filter(|t| t.seq > from).count();
            if !page.more || page.through.is_none_or(|t| t <= from) {
                // Caught up: only now, so a failed or capped read above
                // retries at the next tick.
                self.version = Some(v);
                break;
            }
        }
        Ok(published)
    }
}

#[cfg(test)]
mod tests;
