use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_ops::wire::MachineKey;

use super::*;

/// A log of records, each rendering some events, appended by tests. A
/// record's anchor hashes its generation, `seq` and content.
#[derive(Default)]
struct FakeLog {
    /// Each record's events, by `seq` (index + 1).
    records: Mutex<Vec<Vec<Tailed>>>,
    version: AtomicI64,
    generation: Mutex<String>,
    /// Marks content, so a fork of the same generation differs.
    marker: Mutex<String>,
    reads: AtomicUsize,
    fail_version: AtomicBool,
    fail_reads: AtomicBool,
    /// A record that can't be read: a read whose range holds it fails.
    bad_seq: Mutex<Option<u64>>,
    /// Replaced by a restore to this head, then this many appends, just
    /// before the next page read.
    swap_before_read: Mutex<Option<(usize, usize)>>,
}

impl FakeLog {
    /// `n` records of one event each.
    fn append(&self, n: usize) {
        for _ in 0..n {
            self.append_rendering(1);
        }
    }

    /// One record that renders `events` events (0 or 1; 2 is a corrupt
    /// log).
    fn append_rendering(&self, events: usize) {
        let marker = self.marker.lock().expect("lock").clone();
        let mut r = self.records.lock().expect("lock");
        let seq = r.len() as u64 + 1;
        r.push((0..events).map(|_| ev_marked(seq, &marker)).collect());
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// One record whose event's data is `bytes` long.
    fn append_big(&self, bytes: usize) {
        let mut r = self.records.lock().expect("lock");
        let seq = r.len() as u64 + 1;
        let mut t = ev(seq);
        t.event.data = "x".repeat(bytes);
        r.push(vec![t]);
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Restored to its first `head` records, as another generation.
    fn restore(&self, head: usize) {
        self.restore_quietly(head);
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// The same, with no commit this connection would see (a file put
    /// back by hand).
    fn restore_quietly(&self, head: usize) {
        self.records.lock().expect("lock").truncate(head);
        let mut g = self.generation.lock().expect("lock");
        *g = format!("{g}+r{head}");
    }

    /// An older copy of the same generation put back, then advanced with
    /// other content.
    fn fork(&self, head: usize, then: usize) {
        self.records.lock().expect("lock").truncate(head);
        *self.marker.lock().expect("lock") = "fork".into();
        self.append(then);
    }

    fn anchor_of(&self, r: &[Vec<Tailed>], seq: u64) -> Anchor {
        if seq == 0 {
            return None;
        }
        let rec = r.get(seq as usize - 1)?;
        let content: Vec<&str> = rec.iter().map(|t| t.event.data.as_str()).collect();
        let g = self.generation.lock().expect("lock");
        Some(format!("{g}:{seq}:{}", content.join("|")))
    }
}

fn ev(seq: u64) -> Tailed {
    ev_marked(seq, "")
}

fn ev_marked(seq: u64, marker: &str) -> Tailed {
    Tailed {
        seq,
        event: Event {
            id: format!("s1:{seq}"),
            data: format!("{{\"seq\":{seq}{marker}}}"),
        },
    }
}

impl LogSource for FakeLog {
    fn data_version(&self) -> Result<i64, StoreFailed> {
        if self.fail_version.load(Ordering::SeqCst) {
            return Err(StoreFailed);
        }
        Ok(self.version.load(Ordering::SeqCst))
    }

    fn head(&self) -> Result<(u64, Anchor), StoreFailed> {
        let r = self.records.lock().expect("lock");
        let head = r.len() as u64;
        Ok((head, self.anchor_of(&r, head)))
    }

    fn anchor(&self, seq: u64) -> Result<Anchor, StoreFailed> {
        let r = self.records.lock().expect("lock");
        Ok(self.anchor_of(&r, seq))
    }

    fn events_after(
        &self,
        seq: u64,
        records: usize,
        max_bytes: usize,
    ) -> Result<Page, StoreFailed> {
        if let Some((head, then)) = self.swap_before_read.lock().expect("lock").take() {
            self.restore(head);
            self.append(then);
        }
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(StoreFailed);
        }
        if let Some(bad) = *self.bad_seq.lock().expect("lock") {
            let last = seq + records as u64;
            if bad > seq && bad <= last && bad as usize <= self.records.lock().expect("lock").len()
            {
                return Err(StoreFailed);
            }
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        let r = self.records.lock().expect("lock");
        let after: Vec<_> = r.iter().enumerate().skip(seq as usize).collect();
        let (mut events, mut bytes, mut through, mut taken) = (Vec::new(), 0, None, 0);
        for (i, e) in after.iter().take(records) {
            let n: usize = e.iter().map(|t| size(&t.event)).sum();
            if taken > 0 && bytes + n > max_bytes {
                break;
            }
            bytes += n;
            events.extend(e.iter().cloned());
            through = Some(*i as u64 + 1);
            taken += 1;
        }
        Ok(Page {
            events,
            after_anchor: self.anchor_of(&r, seq),
            through,
            through_anchor: through.and_then(|t| self.anchor_of(&r, t)),
            more: after.len() > taken,
        })
    }

    fn cursor_seq(&self, cursor: &str) -> Result<u64, Refusal> {
        cursor
            .strip_prefix("s1:")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, "cursor"))
    }
}

/// Machines and their states.
struct Keys(Mutex<HashMap<String, MachineState>>);

impl KeyDirectory for Keys {
    fn machine(&self, m: &str) -> Option<MachineKey> {
        self.0.lock().expect("lock").get(m).map(|s| MachineKey {
            machine: m.into(),
            public_key: [0; 32],
            state: *s,
        })
    }
    fn person(&self, _: &str) -> Option<mdkg_ops::wire::PersonKey> {
        None
    }
}

/// Every stream's machine approved.
fn keys() -> Keys {
    Keys(Mutex::new(
        ["m", "m1", "m2", "n", "host"]
            .into_iter()
            .map(|m| (m.to_owned(), MachineState::Approved))
            .collect(),
    ))
}

/// A grant that always holds.
fn open() -> Grant {
    Arc::new(|| Ok(Granted::Valid))
}

/// A grant a test can lapse.
fn switch() -> (Grant, Arc<AtomicBool>) {
    let on = Arc::new(AtomicBool::new(true));
    let o = on.clone();
    (
        Arc::new(move || {
            Ok(if o.load(Ordering::SeqCst) {
                Granted::Valid
            } else {
                Granted::MachineRevoked
            })
        }),
        on,
    )
}

/// A cut hook, and whether it ran.
fn cut_flag(s: &Subscription) -> Arc<AtomicBool> {
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    s.set_cut(Box::new(move || c.store(true, Ordering::SeqCst)));
    cut
}

fn start(log: &Arc<FakeLog>, buffer: usize) -> Tailer {
    Tailer::start(log.clone(), buffer, "host").expect("start")
}

fn drain(s: &Subscription) -> Vec<String> {
    let mut out = Vec::new();
    while let Next::Event(e) = s.next() {
        out.push(e.id);
    }
    out
}

const HB: Duration = Duration::from_millis(20);
#[test]
fn the_tailer_starts_at_the_head_and_fans_out_in_order() {
    let log = Arc::new(FakeLog::default());
    log.append(3);
    let mut t = start(&log, 1 << 20);
    let a = t.fanout().subscribe("m1", HB, open());
    let b = t.fanout().subscribe("m2", HB, open());
    assert_eq!(t.tick(&keys()).expect("tick"), 0, "nothing past the head");
    log.append(2);
    assert_eq!(t.tick(&keys()).expect("tick"), 2);
    assert_eq!(t.tick(&keys()).expect("tick"), 0, "data_version unchanged");
    assert_eq!(drain(&a), ["s1:4", "s1:5"]);
    assert_eq!(drain(&b), ["s1:4", "s1:5"]);
    assert_eq!(a.next(), Next::Heartbeat);
}

#[test]
fn a_silent_record_is_passed_and_a_record_with_two_events_is_corrupt() {
    // Orchestrator #9998: A9's one event per record, enforced.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.append_rendering(1);
    log.append_rendering(0);
    log.append_rendering(1);
    assert_eq!(t.tick(&keys()).expect("tick"), 2);
    assert_eq!(drain(&s), ["s1:1", "s1:3"]);
    assert_eq!(t.fanout().position(), 3, "past a record with no event");
    log.append_rendering(2);
    assert_eq!(t.tick(&keys()), Err(TickFailed::Corrupt { seq: 4 }));
    assert_eq!(s.next(), Next::Closed(Closed::Corrupt));
    assert_eq!(t.fanout().position(), 3, "the tailer stays before it");
    // A backfill ends at the tailer's position, before the corrupt
    // record, and the next tick closes the stream.
    let r = t.fanout().subscribe("m", HB, open());
    r.backfill(log.clone(), 0);
    assert_eq!(drain(&r), ["s1:1", "s1:3"]);
    t.tick(&keys()).expect("reported once");
    assert_eq!(r.next(), Next::Closed(Closed::Corrupt));
}

#[test]
fn more_than_a_page_is_read_in_one_tick_and_a_long_backlog_over_several() {
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 30);
    let a = t.fanout().subscribe("m", HB, open());
    log.append(PAGE * 2 + 3);
    assert_eq!(t.tick(&keys()).expect("tick"), PAGE * 2 + 3);
    assert_eq!(drain(&a).len(), PAGE * 2 + 3);
    // Past the per-tick cap: the rest at the next ticks, so each returns.
    log.append(PAGE * PAGES_PER_TICK + 1);
    assert_eq!(t.tick(&keys()).expect("tick"), PAGE * PAGES_PER_TICK);
    assert_eq!(
        t.tick(&keys()).expect("tick"),
        1,
        "data_version unchanged, not caught up"
    );
    assert_eq!(drain(&a).len(), PAGE * PAGES_PER_TICK + 1);
}

#[test]
fn a_resume_has_no_gap_and_no_repeat() {
    let log = Arc::new(FakeLog::default());
    log.append(5);
    let mut t = start(&log, 1 << 20);
    log.append(2);
    t.tick(&keys()).expect("tick");
    // A client resuming after s1:3: subscribe, then a record lands and
    // the tailer publishes it before the backfill reads.
    let s = t.fanout().subscribe("m", HB, open());
    log.append(1);
    t.tick(&keys()).expect("tick");
    let after = log.cursor_seq("s1:3").expect("cursor");
    s.backfill(log.clone(), after);
    assert_eq!(drain(&s), ["s1:4", "s1:5", "s1:6", "s1:7", "s1:8"]);
    log.append(1);
    t.tick(&keys()).expect("tick");
    assert_eq!(drain(&s), ["s1:9"]);
}

#[test]
fn a_cursor_ahead_of_the_tailer_admits_nothing_it_covers() {
    // Terra 1 #9989: a list read just before the watch returned cursor
    // s1:7 while the tailer was at 5; 6 and 7 must not come again.
    let log = Arc::new(FakeLog::default());
    log.append(5);
    let mut t = start(&log, 1 << 20);
    log.append(3);
    let s = t.fanout().subscribe("m", HB, open());
    s.backfill(log.clone(), 7);
    t.tick(&keys()).expect("tick");
    assert_eq!(drain(&s), ["s1:8"]);
}

#[test]
fn a_backfill_page_is_bounded_by_what_is_left_of_the_buffer() {
    // Terra 1 #9989: the page is read within the stream's room, so a
    // backfill never holds more than the buffer; and it still streams
    // through a far resume.
    let log = Arc::new(FakeLog::default());
    log.append(PAGE * 3);
    let t = start(&log, 200);
    let s = t.fanout().subscribe("m", HB, open());
    s.backfill(log.clone(), 0);
    let before = log.reads.load(Ordering::SeqCst);
    assert_eq!(drain(&s).len(), PAGE * 3, "no overflow");
    let reads = log.reads.load(Ordering::SeqCst) - before;
    // Three full pages would be 3 reads; a 200-byte room holds a few
    // records a page.
    assert!(reads > 100, "pages of a few records each: {reads}");
}

#[test]
fn a_slow_stream_is_closed_not_buffered_without_bound() {
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 200);
    let slow = t.fanout().subscribe("m", HB, open());
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    slow.set_cut(Box::new(move || c.store(true, Ordering::SeqCst)));
    log.append(10);
    t.tick(&keys()).expect("tick");
    assert_eq!(slow.next(), Next::Closed(Closed::Overflow));
    assert!(cut.load(Ordering::SeqCst), "its connection is cut");
    // A stream that keeps up is unaffected.
    let fast = t.fanout().subscribe("n", HB, open());
    log.append(1);
    t.tick(&keys()).expect("tick");
    assert_eq!(drain(&fast), ["s1:11"]);
}

#[test]
fn revocation_comes_first_cuts_the_writer_and_draining_closes_all() {
    let k = keys();
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let a = t.fanout().subscribe("m1", HB, open());
    let b = t.fanout().subscribe("m2", HB, open());
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    a.set_cut(Box::new(move || c.store(true, Ordering::SeqCst)));
    k.0.lock()
        .expect("lock")
        .insert("m1".into(), MachineState::Revoked);
    // The revoke's tick closes m1's stream even though its read fails:
    // revocation doesn't wait on the log (Terra 1 #9989).
    log.append_rendering(2);
    assert!(t.tick(&k).is_err());
    assert_eq!(a.next(), Next::Closed(Closed::MachineRevoked));
    assert!(cut.load(Ordering::SeqCst), "a blocked writer is released");
    assert_eq!(b.next(), Next::Closed(Closed::Corrupt));
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let d = t.fanout().subscribe("m2", HB, open());
    t.fanout().close_all(Closed::Draining);
    assert_eq!(d.next(), Next::Closed(Closed::Draining));
    drop(d);
    assert_eq!(t.fanout().open_streams(), 0);
}

#[test]
fn a_revocation_during_a_long_backfill_stops_the_stream_at_once() {
    // Orchestrator #9962, Opus #9957: nothing past the revocation, and no
    // further read of the store for it.
    let log = Arc::new(FakeLog::default());
    log.append(PAGE * 3);
    let t = start(&log, 1 << 20);
    let (grant, approved) = switch();
    let s = t.fanout().subscribe("m", HB, grant);
    s.backfill(log.clone(), 0);
    assert!(matches!(s.next(), Next::Event(_)));
    let reads = log.reads.load(Ordering::SeqCst);
    approved.store(false, Ordering::SeqCst);
    assert_eq!(s.next(), Next::Closed(Closed::MachineRevoked));
    assert_eq!(s.next(), Next::Closed(Closed::MachineRevoked));
    assert_eq!(log.reads.load(Ordering::SeqCst), reads, "no read after it");
    // A stream closed by the fan-out stops mid-backfill too.
    let r = t.fanout().subscribe("m", HB, open());
    r.backfill(log.clone(), 0);
    assert!(matches!(r.next(), Next::Event(_)));
    t.fanout().close_all(Closed::Draining);
    let reads = log.reads.load(Ordering::SeqCst);
    assert_eq!(r.next(), Next::Closed(Closed::Draining));
    assert_eq!(log.reads.load(Ordering::SeqCst), reads);
}

#[test]
fn a_waiting_stream_sees_its_grant_lapse_within_a_second() {
    // Terra 1 #9989: a stream subscribed after the revoke's tick, or one
    // that simply waits, still closes, by its own recheck.
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let (grant, approved) = switch();
    let s = t.fanout().subscribe("m", Duration::from_secs(30), grant);
    let started = Instant::now();
    let waiter = std::thread::spawn(move || s.next());
    std::thread::sleep(Duration::from_millis(100));
    approved.store(false, Ordering::SeqCst);
    assert_eq!(
        waiter.join().expect("thread"),
        Next::Closed(Closed::MachineRevoked)
    );
    assert!(started.elapsed() < Duration::from_millis(1600));
}

#[test]
fn a_slow_grant_never_holds_up_another_stream_s_delivery() {
    // Claude #10058: a grant that reads a busy store is asked with the
    // queue unlocked, so publishing to every stream goes on meanwhile.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    // The waiter's grant signals once it is being asked from its wait
    // loop (its second call), then holds there until the tick is done, or
    // 3 s at most, so a grant asked under the queue's lock would hold the
    // tick up (Terra 1 #10372: no dependence on sleeps).
    // Only the waiter's own calls count: the grant monitor asks too, with
    // no lock held, and must not be the one that holds (Terra 4 #10480).
    let (entered, inside) = std::sync::mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let waiter: Arc<Mutex<Option<std::thread::ThreadId>>> = Arc::new(Mutex::new(None));
    let slow: Grant = {
        let (release, calls, waiter) = (release.clone(), calls.clone(), waiter.clone());
        let entered = Mutex::new(entered);
        Arc::new(move || {
            let mine = *waiter.lock().expect("lock") == Some(std::thread::current().id());
            if mine && calls.fetch_add(1, Ordering::SeqCst) == 1 {
                let _ = entered.lock().expect("lock").send(());
                let until = Instant::now() + Duration::from_secs(3);
                while !release.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(Granted::Valid)
        })
    };
    let waiting = t.fanout().subscribe("m1", Duration::from_secs(10), slow);
    let other = t.fanout().subscribe("m2", HB, open());
    let w = {
        let waiter = waiter.clone();
        std::thread::spawn(move || {
            *waiter.lock().expect("lock") = Some(std::thread::current().id());
            waiting.next()
        })
    };
    inside
        .recv_timeout(Duration::from_secs(5))
        .expect("the wait loop asked its grant");
    log.append(1);
    let started = Instant::now();
    t.tick(&keys()).expect("tick");
    let took = started.elapsed();
    release.store(true, Ordering::SeqCst);
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!(drain(&other), ["s1:1"]);
    assert!(matches!(w.join().expect("thread"), Next::Event(_)));
}

#[test]
fn a_lapsed_grant_withholds_a_queued_event() {
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let (grant, approved) = switch();
    let s = t.fanout().subscribe("m", HB, grant);
    log.append(2);
    t.tick(&keys()).expect("tick");
    assert!(matches!(s.next(), Next::Event(_)));
    approved.store(false, Ordering::SeqCst);
    assert_eq!(s.next(), Next::Closed(Closed::MachineRevoked));
}

#[test]
fn a_restored_store_resets_its_streams_and_skips_nothing() {
    // Terra 1 #9989: restored from head 100 to 50, records 51 to 100 are
    // new, never hidden behind the old position.
    let log = Arc::new(FakeLog::default());
    log.append(100);
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.restore(50);
    assert_eq!(t.tick(&keys()).expect("tick"), 0);
    assert_eq!(s.next(), Next::Closed(Closed::Reset));
    assert_eq!(t.fanout().position(), 50);
    let r = t.fanout().subscribe("m", HB, open());
    log.append(1);
    t.tick(&keys()).expect("tick");
    assert_eq!(drain(&r), ["s1:51"]);
}

#[test]
fn a_store_failure_during_a_backfill_closes_the_stream() {
    struct Failing;
    impl LogSource for Failing {
        fn data_version(&self) -> Result<i64, StoreFailed> {
            Ok(0)
        }
        fn head(&self) -> Result<(u64, Anchor), StoreFailed> {
            Ok((10, Some("a10".into())))
        }
        fn anchor(&self, seq: u64) -> Result<Anchor, StoreFailed> {
            Ok(Some(format!("a{seq}")))
        }
        fn events_after(&self, _: u64, _: usize, _: usize) -> Result<Page, StoreFailed> {
            Err(StoreFailed)
        }
        fn cursor_seq(&self, _: &str) -> Result<u64, Refusal> {
            Ok(0)
        }
    }
    let t = Tailer::start(Arc::new(Failing), 1 << 20, "host").expect("start");
    let s = t.fanout().subscribe("m", HB, open());
    s.backfill(Arc::new(Failing), 3);
    assert_eq!(s.next(), Next::Closed(Closed::Failed));
    assert_eq!(s.next(), Next::Closed(Closed::Failed), "and stays closed");
}

#[test]
fn enrolment_events_reach_the_host_s_live_streams_only_without_a_cursor() {
    // Orchestrator #9998: no other machine's stream sees one; Terra 1
    // #9989: it carries no cursor, so the client's last one stands.
    let log = Arc::new(FakeLog::default());
    log.append(3);
    let mut t = start(&log, 1 << 20);
    let host = t.fanout().subscribe("host", HB, open());
    let other = t.fanout().subscribe("m", HB, open());
    t.fanout().publish_enrolment(&Event {
        id: "s1:2".into(),
        data: "{\"type\":\"mdkg.machine.enroll_requested\"}".into(),
    });
    log.append(1);
    t.tick(&keys()).expect("tick");
    let mut ids = Vec::new();
    while let Next::Event(e) = host.next() {
        ids.push(e.id);
    }
    assert_eq!(ids, ["", "s1:4"]);
    assert_eq!(drain(&other), ["s1:4"], "no enrolment event");
    assert_eq!(t.fanout().position(), 4, "no cursor of its own");
}

#[test]
fn a_waiting_stream_wakes_on_publish() {
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", Duration::from_secs(10), open());
    let waiter = std::thread::spawn(move || s.next());
    std::thread::sleep(Duration::from_millis(50));
    log.append(1);
    t.tick(&keys()).expect("tick");
    assert_eq!(waiter.join().expect("thread"), Next::Event(ev(1).event));
}

#[test]
fn a_store_replaced_without_a_commit_is_still_caught() {
    // Terra 6 #10224: data_version need not move; the anchor check runs
    // every tick.
    let log = Arc::new(FakeLog::default());
    log.append(10);
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    t.tick(&keys()).expect("tick");
    log.restore_quietly(5);
    t.tick(&keys()).expect("tick");
    assert_eq!(s.next(), Next::Closed(Closed::Reset));
    assert_eq!(t.fanout().position(), 5);
}

#[test]
fn a_fork_of_the_same_generation_past_the_position_is_caught() {
    // Terra 6 #10224: an older copy put back and advanced past the old
    // position: same generation, same seq, another record there.
    let log = Arc::new(FakeLog::default());
    log.append(10);
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.fork(5, 8);
    t.tick(&keys()).expect("tick");
    assert_eq!(s.next(), Next::Closed(Closed::Reset));
    assert_eq!(t.fanout().position(), 13, "the new head");
}

#[test]
fn a_store_replaced_between_the_check_and_the_read_is_caught() {
    // Terra 6 #10224: the page doesn't follow on from the position, so
    // nothing of it is published.
    let log = Arc::new(FakeLog::default());
    log.append(100);
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    *log.swap_before_read.lock().expect("lock") = Some((50, 51));
    log.version.fetch_add(1, Ordering::SeqCst);
    assert_eq!(t.tick(&keys()).expect("tick"), 0);
    assert_eq!(s.next(), Next::Closed(Closed::Reset));
    assert_eq!(t.fanout().position(), 101, "restarted at the new head");
}

#[test]
fn the_tick_cuts_a_blocked_writer_whose_person_grant_lapsed() {
    // Terra 6 #10224: the writer never calls next(); the tailer asks
    // every grant, even when its log read fails.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let (grant, valid) = switch();
    let s = t.fanout().subscribe("m", HB, grant);
    let cut = cut_flag(&s);
    log.fail_version.store(true, Ordering::SeqCst);
    valid.store(false, Ordering::SeqCst);
    assert!(t.tick(&keys()).is_err(), "the log read failed");
    // The grant monitor, apart from the tick.
    // The grant monitor, which may have run already (Terra 3 #10383).
    t.fanout().check_grants();
    assert!(cut.load(Ordering::SeqCst), "cut all the same");
    assert_eq!(s.next(), Next::Closed(Closed::MachineRevoked));
}

#[test]
fn a_revocation_after_another_close_still_cuts() {
    // Terra 6 #10224: drained first, revoked after: the first reason is
    // kept, and the connection is cut.
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let (grant, valid) = switch();
    let s = t.fanout().subscribe("m1", HB, grant);
    let cut = cut_flag(&s);
    t.fanout().close_all(Closed::Draining);
    assert!(!cut.load(Ordering::SeqCst), "a drain ends at a frame");
    valid.store(false, Ordering::SeqCst);
    t.fanout().check_grants();
    assert!(cut.load(Ordering::SeqCst));
    assert_eq!(s.next(), Next::Closed(Closed::Draining));
}

#[test]
fn the_grant_monitor_runs_by_itself_every_second() {
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let (grant, valid) = switch();
    let s = t.fanout().subscribe("m", HB, grant);
    let cut = cut_flag(&s);
    valid.store(false, Ordering::SeqCst);
    let until = Instant::now() + Duration::from_millis(2500);
    while !cut.load(Ordering::SeqCst) {
        assert!(Instant::now() < until, "never cut");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_grant_that_can_t_be_read_closes_the_stream_failed() {
    // Terra 2 #10122: never taken as valid, and not called a revocation.
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, Arc::new(|| Err(StoreFailed)));
    assert_eq!(s.next(), Next::Closed(Closed::Failed));
}

#[test]
fn a_backfill_page_and_the_live_queue_share_one_budget() {
    // Terra 2 #10122, Terra 6 #10224: live events arriving while a page
    // is held count against the same buffer.
    let log = Arc::new(FakeLog::default());
    log.append(20);
    let mut t = start(&log, 400);
    let s = t.fanout().subscribe("m", HB, open());
    s.backfill(log.clone(), 0);
    assert!(matches!(s.next(), Next::Event(_)), "a page is now held");
    log.append(3);
    t.tick(&keys()).expect("tick");
    assert_eq!(s.next(), Next::Closed(Closed::Overflow));
}

#[test]
fn an_event_past_the_whole_budget_is_skipped_with_its_cursor() {
    // Orchestrator #10487, Terra 2 #10469: legitimate data no stream can
    // carry: an `mdkg.stream.error` in its place, its cursor kept so a
    // resume passes it, and the stream carries on; live and in backfill.
    let log = Arc::new(FakeLog::default());
    log.append(1);
    let mut t = start(&log, 400);
    let live = t.fanout().subscribe("n", HB, open());
    log.append_big(1000);
    log.append(1);
    assert_eq!(t.tick(&keys()).expect("tick"), 2);
    assert_eq!(
        live.next(),
        Next::TooLarge {
            id: "s1:2".into(),
            seq: 2
        }
    );
    assert_eq!(drain(&live), ["s1:3"], "carried on past it");
    let r = t.fanout().subscribe("m", HB, open());
    r.backfill(log.clone(), 0);
    assert!(matches!(r.next(), Next::Event(e) if e.id == "s1:1"));
    assert_eq!(
        r.next(),
        Next::TooLarge {
            id: "s1:2".into(),
            seq: 2
        }
    );
    assert_eq!(drain(&r), ["s1:3"]);
}

#[test]
fn a_read_failing_at_one_position_for_30_ticks_is_a_corrupt_record() {
    // Orchestrator #10487: while data_version still reads; the count
    // restarts after a read that works.
    let log = Arc::new(FakeLog::default());
    log.append(2);
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.append(1);
    log.fail_reads.store(true, Ordering::SeqCst);
    for _ in 0..READ_FAILURES - 1 {
        assert_eq!(t.tick(&keys()), Err(TickFailed::Store));
    }
    assert_eq!(t.tick(&keys()), Err(TickFailed::Corrupt { seq: 3 }));
    assert_eq!(t.corrupt_at(), Some(3));
    assert_eq!(s.next(), Next::Closed(Closed::Corrupt));
    // Readable again: published, and no longer reported.
    log.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(t.tick(&keys()).expect("tick"), 1);
    assert_eq!(t.corrupt_at(), None);
}

#[test]
fn an_unreadable_record_is_named_exactly_and_those_before_it_published() {
    // Terra 4 #10518: record 7 of 10 can't be read; 1 to 6 are published,
    // and after 30 ticks the report names 7, not the page's first record.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.append(10);
    *log.bad_seq.lock().expect("lock") = Some(7);
    assert_eq!(t.tick(&keys()), Err(TickFailed::Store));
    assert_eq!(t.fanout().position(), 6, "up to the bad record");
    let mut got = Vec::new();
    while let Next::Event(e) = s.next() {
        got.push(e.id);
    }
    assert_eq!(got, ["s1:1", "s1:2", "s1:3", "s1:4", "s1:5", "s1:6"]);
    let mut last = None;
    for _ in 1..READ_FAILURES {
        last = Some(t.tick(&keys()));
    }
    assert_eq!(last, Some(Err(TickFailed::Corrupt { seq: 7 })));
    assert_eq!(s.next(), Next::Closed(Closed::Corrupt));
}

#[test]
fn a_read_that_works_restarts_the_count() {
    // Terra 4 #10518, #10525: 20 failures, one read that works without
    // moving the position (nothing new to read), then 29 failures at that
    // same position: no report, which only the reset can give.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let bump = || {
        log.version.fetch_add(1, Ordering::SeqCst);
    };
    bump();
    log.fail_reads.store(true, Ordering::SeqCst);
    for _ in 0..20 {
        assert_eq!(t.tick(&keys()), Err(TickFailed::Store));
    }
    log.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(t.tick(&keys()).expect("works"), 0);
    assert_eq!(t.fanout().position(), 0, "the position didn't move");
    bump();
    log.fail_reads.store(true, Ordering::SeqCst);
    for _ in 0..READ_FAILURES - 1 {
        assert_eq!(t.tick(&keys()), Err(TickFailed::Store));
    }
    assert_eq!(t.corrupt_at(), None);
}

#[test]
fn a_busy_store_that_fails_data_version_keeps_waiting() {
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let s = t.fanout().subscribe("m", HB, open());
    log.append(1);
    log.fail_reads.store(true, Ordering::SeqCst);
    log.fail_version.store(true, Ordering::SeqCst);
    for _ in 0..READ_FAILURES * 2 {
        assert_eq!(t.tick(&keys()), Err(TickFailed::Store));
    }
    assert_eq!(t.corrupt_at(), None);
    assert_eq!(s.next(), Next::Heartbeat, "still open");
}

#[test]
fn a_grant_that_panics_is_failed_and_the_monitor_goes_on() {
    // Terra 3 #10383.
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let bad = t
        .fanout()
        .subscribe("m", HB, Arc::new(|| panic!("a grant's own bug")));
    let (grant, valid) = switch();
    let good = t.fanout().subscribe("n", HB, grant);
    let cut = cut_flag(&good);
    valid.store(false, Ordering::SeqCst);
    t.fanout().check_grants();
    assert_eq!(bad.next(), Next::Closed(Closed::Failed));
    assert!(
        cut.load(Ordering::SeqCst),
        "the next stream was still checked"
    );
}

#[test]
fn a_grant_lapsing_across_silent_pages_stops_the_reads() {
    // Terra 2 #10122: records that render nothing read page after page in
    // one next(); the grant is asked between reads.
    let log = Arc::new(FakeLog::default());
    for _ in 0..PAGE * 3 {
        log.append_rendering(0);
    }
    log.append(1);
    let t = start(&log, 1 << 20);
    let l = log.clone();
    let grant: Grant = Arc::new(move || {
        Ok(if l.reads.load(Ordering::SeqCst) == 0 {
            Granted::Valid
        } else {
            Granted::MachineRevoked
        })
    });
    let s = t.fanout().subscribe("m", HB, grant);
    s.backfill(log.clone(), 0);
    assert_eq!(s.next(), Next::Closed(Closed::MachineRevoked));
    assert_eq!(
        log.reads.load(Ordering::SeqCst),
        1,
        "no read after it lapsed"
    );
}

#[test]
fn at_most_one_event_in_flight_follows_a_revocation() {
    // Orchestrator #10141: an event that passed its check before the
    // revoke commits may still be written; then the stream closes.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    let (grant, valid) = switch();
    let s = t.fanout().subscribe("m", HB, grant);
    log.append(3);
    t.tick(&keys()).expect("tick");
    let in_flight = s.next();
    assert!(
        matches!(in_flight, Next::Event(_)),
        "checked before the revoke"
    );
    valid.store(false, Ordering::SeqCst);
    let after: Vec<Next> = (0..3).map(|_| s.next()).collect();
    assert!(
        after
            .iter()
            .all(|n| *n == Next::Closed(Closed::MachineRevoked)),
        "{after:?}"
    );
}

#[test]
fn a_corrupt_record_is_reported_once_and_kept_for_status() {
    // Claude #10145: for the caller to log once; status reads it.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    log.append_rendering(2);
    assert_eq!(t.tick(&keys()), Err(TickFailed::Corrupt { seq: 1 }));
    assert_eq!(t.tick(&keys()), Ok(0), "not again");
    assert_eq!(t.corrupt_at(), Some(1));
    // A stream opened while it stands closes too.
    let s = t.fanout().subscribe("m", HB, open());
    t.tick(&keys()).expect("tick");
    assert_eq!(s.next(), Next::Closed(Closed::Corrupt));
}

#[test]
fn a_new_stream_is_refused_while_a_corrupt_record_stands() {
    // Terra 2 #10378, orchestrator #10487: `coord.store_failed`, naming
    // the seq; open again once the log reads past it.
    let log = Arc::new(FakeLog::default());
    let mut t = start(&log, 1 << 20);
    log.append_rendering(2);
    assert_eq!(t.tick(&keys()), Err(TickFailed::Corrupt { seq: 1 }));
    let e = t
        .fanout()
        .open("m", HB, open())
        .expect_err("refused while it stands");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::CoordStoreFailed, "corrupt_log")
    );
    assert_eq!(e.data.get("seq"), Some(&serde_json::Value::from(1)));
    assert_eq!(e.response().status, 500);
    // Repaired: the record now reads as one event.
    log.records.lock().expect("lock")[0].truncate(1);
    log.version.fetch_add(1, Ordering::SeqCst);
    t.tick(&keys()).expect("tick");
    assert!(t.fanout().open("m", HB, open()).is_ok());
}

#[test]
fn a_person_grant_lapse_is_not_called_a_machine_revocation() {
    // Orchestrator #10592: the close says which check failed.
    let log = Arc::new(FakeLog::default());
    let t = start(&log, 1 << 20);
    let lapsed = t
        .fanout()
        .subscribe("m", HB, Arc::new(|| Ok(Granted::PersonLapsed)));
    let revoked = t
        .fanout()
        .subscribe("n", HB, Arc::new(|| Ok(Granted::MachineRevoked)));
    assert_eq!(lapsed.next(), Next::Closed(Closed::PersonLapsed));
    assert_eq!(revoked.next(), Next::Closed(Closed::MachineRevoked));
}
