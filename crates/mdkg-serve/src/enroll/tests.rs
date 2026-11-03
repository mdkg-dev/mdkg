use std::collections::HashMap;
use std::sync::Mutex;

use mdkg_wire::keys::fingerprint;
use mdkg_wire::pair::{Pairing, machine_code, vectors};
use sha2::{Digest, Sha256};

use super::*;
use crate::auth::tests::{Fixture, Req, fixture, key_of};
use crate::http::RequestHead;

/// A pending request and its revealed `Nc`.
type Row = (NewPending, Option<[u8; 32]>);

#[derive(Default)]
struct FakeStore {
    pending: Mutex<HashMap<String, Row>>,
    machines: Mutex<HashMap<String, ([u8; 32], String)>>,
    /// Lookups left that report no `Nc`, as one read before a concurrent
    /// reveal committed would.
    stale: Mutex<u32>,
    /// Fail `request` this way, when set.
    fail: Mutex<Option<StoreFailed>>,
    /// Fail `lookup` this way, when set: kept apart from `fail`, so the
    /// write's failure path is still exercised on its own (#7789).
    fail_lookup: Mutex<Option<StoreFailed>>,
    /// Store lookups made.
    lookups: std::sync::atomic::AtomicUsize,
    /// `lookup` finds nothing, as a read before a concurrent request
    /// committed would; `request` still sees every row (Terra 3 #8179).
    blind: std::sync::atomic::AtomicBool,
}

impl EnrollStore for FakeStore {
    fn request(
        &self,
        p: &NewPending,
        max: usize,
        now_ms: u64,
    ) -> Result<Result<(), PendingRefused>, StoreFailed> {
        if let Some(e) = *self.fail.lock().expect("lock") {
            return Err(e);
        }
        let mut pending = self.pending.lock().expect("lock");
        pending.retain(|_, (r, _)| r.expires_ms > now_ms);
        if self
            .machines
            .lock()
            .expect("lock")
            .contains_key(&p.machine_id)
        {
            return Ok(Err(PendingRefused::Exists));
        }
        if let Some((held, _)) = pending.get(&p.machine_id) {
            return Ok(Err(PendingRefused::Pending(held.public_key)));
        }
        if pending.len() >= max {
            return Ok(Err(PendingRefused::Full));
        }
        pending.insert(p.machine_id.clone(), (p.clone(), None));
        Ok(Ok(()))
    }

    fn lookup(&self, machine_id: &str, now_ms: u64) -> Result<Option<Enrolment>, StoreFailed> {
        if let Some(e) = *self.fail_lookup.lock().expect("lock") {
            return Err(e);
        }
        self.lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.blind.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some((pk, state)) = self.machines.lock().expect("lock").get(machine_id) {
            return Ok(Some(Enrolment::Machine {
                public_key: *pk,
                state: state.clone(),
            }));
        }
        Ok(self
            .pending
            .lock()
            .expect("lock")
            .get(machine_id)
            .filter(|(r, _)| r.expires_ms > now_ms)
            .map(|(r, nc)| {
                let mut stale = self.stale.lock().expect("lock");
                let nc = if *stale > 0 {
                    *stale -= 1;
                    None
                } else {
                    *nc
                };
                Enrolment::Pending(PendingRow {
                    pending_id: r.pending_id.clone(),
                    public_key: r.public_key,
                    commitment: r.commitment,
                    nc,
                    ns: r.ns,
                    expires_ms: r.expires_ms,
                })
            }))
    }

    fn reveal(&self, pending_id: &str, nc: &[u8; 32], _now: u64) -> Result<bool, StoreFailed> {
        let mut pending = self.pending.lock().expect("lock");
        for (r, had) in pending.values_mut() {
            if r.pending_id == pending_id {
                // True when it now holds `nc`, as the trait says.
                return Ok(*had.get_or_insert(*nc) == *nc);
            }
        }
        Ok(false)
    }
}

struct World {
    f: Fixture,
    store: Arc<FakeStore>,
    enroller: Enroller,
}

fn world() -> World {
    let f = fixture();
    let store = Arc::new(FakeStore::default());
    let enroller = Enroller::new(f.gate.clone(), store.clone(), Limits::default());
    World { f, store, enroller }
}

/// The worker's side: its key, `Nc` and the request it signs.
struct Worker {
    id: String,
    pk: [u8; 32],
    nc: [u8; 32],
    n: u32,
}

impl Worker {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            pk: key_of(id),
            nc: [id.len() as u8; 32],
            n: 0,
        }
    }

    fn body(&self, name: &str, graph: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "machine_id": self.id,
            "public_key": b64::encode_url(&self.pk),
            "name": name,
            "graph_id": graph,
            "commitment": b64::encode_url(&mdkg_wire::pair::commitment(&self.nc)),
        }))
        .expect("json")
    }

    fn req(&mut self, w: &World, method: &str, path: &str, body: &[u8]) -> RequestHead {
        let b = w.f.gate.boot_id().expect("boot");
        let mut r = if method == "GET" {
            Req::get(path, &b)
        } else {
            Req::post(path, &b, body)
                .without("idempotency-key")
                .without("mdkg-actor")
        };
        r.keyid = self.id.clone();
        r.sign_with = self.pk;
        r.created = (w.f.gate.now_ms() / 1000) as i64;
        self.n += 1;
        let mut nonce = [0u8; 16];
        nonce[..4].copy_from_slice(&self.n.to_be_bytes());
        nonce[4..].copy_from_slice(&self.pk[..12]);
        r.nonce = b64::encode_url(&nonce);
        r.signed()
    }

    fn send(
        &mut self,
        w: &World,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Handled, Refusal> {
        let head = self.req(w, method, path, body);
        let route = Route::of(&head).expect("an enrolment route");
        w.enroller.admit(&route, &head, &peer(1))?;
        w.enroller.handle(&route, &head, body, &peer(1))
    }

    fn enrol(&mut self, w: &World, name: &str) -> Result<Handled, Refusal> {
        let body = self.body(name, crate::auth::tests::GRAPH);
        self.send(w, "POST", "/enroll", &body)
    }

    fn reveal(&mut self, w: &World, nc: [u8; 32]) -> Result<Handled, Refusal> {
        let body = serde_json::to_vec(&json!({"nc": b64::encode_url(&nc)})).expect("json");
        let path = format!("/enroll/{}:reveal", self.id);
        self.send(w, "POST", &path, &body)
    }

    fn poll(&mut self, w: &World) -> Result<Value, Refusal> {
        let path = format!("/enroll/{}", self.id);
        let h = self.send(w, "GET", &path, b"")?;
        let v: Value = serde_json::from_slice(&h.response.body).expect("json");
        Ok(v["data"].clone())
    }
}

fn peer(n: u8) -> crate::limits::Peer {
    crate::limits::Peer::Tcp(std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, n)))
}

fn code(r: Result<Handled, Refusal>) -> (Code, &'static str) {
    let e = r.expect_err("refused");
    (e.code, e.test_reason())
}

#[test]
fn request_reveal_poll_and_the_code() {
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000001");
    let h = m.enrol(&w, "build-box").expect("enrolled");
    let ev = h.event.expect("enroll_requested");
    assert_eq!(ev.kind, "mdkg.machine.enroll_requested");
    assert_eq!(ev.data["fingerprint"], fingerprint(&m.pk));
    let data: Value = serde_json::from_slice(&h.response.body).expect("json");
    let ns: [u8; 32] = b64::decode_url(data["data"]["ns"].as_str().expect("ns"))
        .expect("b64")
        .try_into()
        .expect("32");
    assert_eq!(m.poll(&w).expect("poll")["revealed"], false);
    // The wrong Nc can't be revealed; the right one can, once.
    assert_eq!(
        code(m.reveal(&w, [0xee; 32])),
        (Code::ApiEnrollChanged, "commitment")
    );
    // Reveals and polls are one a second per pending id (burst two).
    assert_eq!(
        code(m.reveal(&w, m.nc)),
        (Code::ApiRateLimited, "enroll_poll")
    );
    w.f.clocks.advance(1_000);
    m.reveal(&w, m.nc).expect("revealed");
    w.f.clocks.advance(1_000);
    m.reveal(&w, m.nc).expect("the same reveal again is fine");
    w.f.clocks.advance(1_000);
    assert_eq!(m.poll(&w).expect("poll")["revealed"], true);
    // Both sides compute the same code from what each saw.
    let code = |serve_fp: &str| {
        machine_code(&Pairing {
            serve_fp,
            machine_fp: &fingerprint(&m.pk),
            graph_id: "graph-1",
            ns: &ns,
            nc: &m.nc,
        })
    };
    let serve_fp = fingerprint(&key_of("serve-tls"));
    let host = code(&serve_fp);
    assert_eq!(host, code(&serve_fp));
    assert_eq!(host.len(), 8);
    // A relay that showed the worker another TLS key gets another code.
    let relayed = code(&fingerprint(&key_of("relay")));
    assert_ne!(host, relayed);
    // After approval (the host's CAS, simulated), the poll says so.
    w.store.pending.lock().expect("lock").clear();
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(m.id.clone(), (m.pk, "approved".into()));
    w.f.clocks.advance(1_000);
    assert_eq!(m.poll(&w).expect("poll")["state"], "approved");
}

#[test]
fn a_worker_may_poll_every_two_seconds_for_fifteen_minutes() {
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000009");
    m.enrol(&w, "poller").expect("pending");
    // Every 2 s until just before the request expires at 15 minutes.
    for _ in 0..449 {
        w.f.clocks.advance(2_000);
        m.poll(&w).expect("a poll every 2 s");
    }
    w.f.clocks.advance(2_000);
    // An expired request is refused as an unknown id is (#6717).
    assert_eq!(
        m.poll(&w).expect_err("expired").test_reason(),
        "bad_signature"
    );
}

#[test]
fn known_pending_and_full_are_refused() {
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000002");
    m.enrol(&w, "a").expect("first");
    assert_eq!(code(m.enrol(&w, "a")), (Code::ApiEnrollPending, "pending"));
    // A known machine id, in any state, is refused (R78).
    let mut known = Worker::new("01a188cc-aaaa-7000-8000-000000000003");
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(known.id.clone(), (key_of("old"), "revoked".into()));
    assert_eq!(
        code(known.enrol(&w, "b")),
        (Code::ApiEnrollExists, "exists")
    );
    // Sixteen pending; the seventeenth is refused.
    for i in 0..15 {
        let mut x = Worker::new(&format!("01a188cc-bbbb-7000-8000-0000000000{i:02}"));
        let enrolment = World {
            f: Fixture {
                gate: w.f.gate.clone(),
                clocks: w.f.clocks.clone(),
                keys: w.f.keys.clone(),
                persons: w.f.persons.clone(),
            },
            store: w.store.clone(),
            // A fresh limiter per address in the test: the rate limit is
            // tested apart.
            enroller: Enroller::new(w.f.gate.clone(), w.store.clone(), Limits::default()),
        };
        x.enrol(&enrolment, "c").expect("pending");
    }
    let mut last = Worker::new("01a188cc-cccc-7000-8000-000000000001");
    let fresh = Enroller::new(w.f.gate.clone(), w.store.clone(), Limits::default());
    let body = last.body("d", crate::auth::tests::GRAPH);
    let head = last.req(&w, "POST", "/enroll", &body);
    assert_eq!(
        code(fresh.handle(&Route::Request, &head, &body, &peer(1))),
        (Code::ApiRateLimited, "pending_full")
    );
    // Expiry frees the slots.
    w.f.clocks.advance(PENDING_MS + 1);
    let head = last.req(&w, "POST", "/enroll", &body);
    fresh
        .handle(&Route::Request, &head, &body, &peer(1))
        .expect("after expiry");
}

#[test]
fn bad_requests() {
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000004");
    let body = m.body("ok", "graph-2");
    assert_eq!(
        code(m.send(&w, "POST", "/enroll", &body)),
        (Code::ApiGraphUnknown, "graph_unknown")
    );
    assert_eq!(
        code(m.enrol(&w, "Bad Name")),
        (Code::UsageInvalidArguments, "name")
    );
    // Five a minute from one address.
    let w = world();
    for i in 0..5 {
        let mut x = Worker::new(&format!("01a188cc-dddd-7000-8000-0000000000{i:02}"));
        x.enrol(&w, "e").expect("within the rate");
    }
    let mut sixth = Worker::new("01a188cc-dddd-7000-8000-000000000099");
    assert_eq!(code(sixth.enrol(&w, "e")), (Code::ApiRateLimited, "enroll"));
    // A body over 4 KiB is refused before it is read.
    let w = world();
    let big = vec![b' '; 5000];
    let mut head = m.req(&w, "POST", "/enroll", &big);
    head.headers.retain(|(n, _)| n != "content-length");
    head.headers.push(("content-length".into(), "5000".into()));
    assert_eq!(
        w.enroller
            .admit(&Route::Request, &head, &peer(9))
            .expect_err("big")
            .code,
        Code::ApiTooLarge
    );
}

#[test]
fn a_pending_key_cannot_reveal_or_poll_another_id() {
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000005");
    m.enrol(&w, "a").expect("pending");
    let mut other = Worker::new("01a188cc-aaaa-7000-8000-000000000006");
    other.enrol(&w, "b").expect("pending");
    // `other` signs a reveal for m's id with its own key.
    let body = serde_json::to_vec(&json!({"nc": b64::encode_url(&m.nc)})).expect("json");
    let path = format!("/enroll/{}:reveal", m.id);
    let mut head = other.req(&w, "POST", &path, &body);
    for (n, v) in head.headers.iter_mut() {
        if n == "signature-input" {
            *v = v.replace(&other.id, &m.id);
        }
    }
    let route = Route::of(&head).expect("route");
    assert_eq!(
        code(w.enroller.handle(&route, &head, &body, &peer(1))),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    // An unknown id has no key to sign with.
    let mut ghost = Worker::new("01a188cc-aaaa-7000-8000-000000000007");
    assert_eq!(
        ghost.poll(&w).expect_err("unknown").test_reason(),
        "bad_signature"
    );
}

#[test]
fn an_enrolment_needs_an_id_its_routes_can_carry() {
    let w = world();
    for id in ["bad.id", "a/b", "a%2fb", "m_1", &"a".repeat(65)] {
        let mut m = Worker::new(id);
        assert_eq!(
            code(m.enrol(&w, "a")),
            (Code::UsageInvalidArguments, "machine_id"),
            "{id}"
        );
    }
    assert!(w.store.pending.lock().expect("lock").is_empty());
    assert_eq!(w.f.gate.nonces_held(), 0, "no nonce used");
    // Five a minute from one address: a fresh one for the valid id.
    let w = world();
    Worker::new(&"a".repeat(64))
        .enrol(&w, "a")
        .expect("64 is fine");
}

#[test]
fn a_caller_without_the_key_cannot_tell_what_an_id_is() {
    // #6717: unknown, pending and enrolled ids all refuse a reveal or a
    // poll signed by the wrong key in the same way.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000011");
    pending.enrol(&w, "a").expect("pending");
    let machine = "01a188cc-aaaa-7000-8000-000000000012";
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(machine.into(), (key_of(machine), "approved".into()));
    let ghost = "01a188cc-aaaa-7000-8000-000000000013";
    let mut intruder = Worker::new("01a188cc-aaaa-7000-8000-000000000014");
    for id in [pending.id.as_str(), machine, ghost] {
        intruder.id = id.into();
        let reveal = intruder.reveal(&w, [1; 32]).expect_err("reveal");
        let poll = intruder.poll(&w).expect_err("poll");
        for e in [reveal, poll] {
            assert_eq!(
                (e.code, e.test_reason()),
                (Code::ApiUnauthenticated, "bad_signature"),
                "{id}"
            );
        }
    }
    // The machine's own key learns it isn't pending.
    let mut own = Worker::new(machine);
    assert_eq!(
        code(own.reveal(&w, [1; 32])),
        (Code::ApiEnrollChanged, "not_pending")
    );
}

#[test]
fn a_reveal_that_loses_a_race_reports_what_won() {
    // #6717: two reveals read an empty Nc; the loser of the compare-and-set
    // is told the request was revealed, or succeeds if it sent the same Nc.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000021");
    m.enrol(&w, "a").expect("pending");
    let id = m.id.clone();
    let set = |nc: [u8; 32]| {
        let mut p = w.store.pending.lock().expect("lock");
        p.get_mut(&id).expect("row").1 = Some(nc);
        *w.store.stale.lock().expect("lock") = 1;
    };
    set([0xee; 32]);
    assert_eq!(
        code(m.reveal(&w, m.nc)),
        (Code::ApiEnrollChanged, "revealed")
    );
    set(m.nc);
    m.reveal(&w, m.nc).expect("the same Nc won");
}

#[test]
fn routes() {
    let head = |m: &str, p: &str| RequestHead {
        method: m.into(),
        path: p.into(),
        query: None,
        headers: vec![],
    };
    assert_eq!(Route::of(&head("POST", "/enroll")), Some(Route::Request));
    assert_eq!(
        Route::of(&head("POST", "/enroll/abc-1:reveal")),
        Some(Route::Reveal("abc-1".into()))
    );
    assert_eq!(
        Route::of(&head("GET", "/enroll/abc-1")),
        Some(Route::Poll("abc-1".into()))
    );
    for (m, p) in [
        ("GET", "/enroll"),
        ("POST", "/enroll/abc:approve"),
        ("GET", "/enroll/a/b"),
        ("GET", "/enroll/a%2fb"),
        ("POST", "/v1/machines:approve"),
    ] {
        assert_eq!(Route::of(&head(m, p)), None, "{m} {p}");
    }
    assert!(valid_name("build-box.2"));
    assert!(!valid_name("-x") && !valid_name("") && !valid_name("A"));
    assert!(!valid_name(&"x".repeat(65)));
}

#[test]
fn polls_over_their_rate_take_no_nonce() {
    // Terra 1 #6726: a pending key's polls past its rate are refused after
    // the signature and before the nonce, so they can't fill the cache.
    let mut f = fixture();
    let gate = Arc::new(
        Arc::try_unwrap(std::mem::replace(&mut f.gate, fixture().gate))
            .expect("one owner")
            .with_pending_nonces(crate::nonce::NonceCache::new(4)),
    );
    f.gate = gate.clone();
    let store = Arc::new(FakeStore::default());
    let w = World {
        f,
        store: store.clone(),
        enroller: Enroller::new(gate.clone(), store, Limits::default()),
    };
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000031");
    m.enrol(&w, "a").expect("pending");
    let mut limited = 0;
    for _ in 0..10 {
        match m.poll(&w) {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e.test_reason(), "enroll_poll");
                limited += 1;
            }
        }
    }
    assert_eq!(limited, 8, "a burst of two");
    assert_eq!(gate.nonces_held(), 3, "the enrolment and two polls");
    // Room is left for another worker.
    Worker::new("01a188cc-aaaa-7000-8000-000000000032")
        .enrol(&w, "b")
        .expect("another enrolment");
}

#[test]
fn reveals_and_polls_are_limited_per_source_before_the_store() {
    // Terra 1 #6726: unsigned lookups of unknown ids can't drive the store.
    let w = world();
    let mut ghost = Worker::new("01a188cc-aaaa-7000-8000-000000000041");
    for _ in 0..LOOKUPS_PER_MINUTE {
        assert_eq!(
            ghost.poll(&w).expect_err("unknown").test_reason(),
            "bad_signature"
        );
    }
    let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        ghost.poll(&w).expect_err("limited").test_reason(),
        "enroll_lookup"
    );
    assert_eq!(
        ghost
            .reveal(&w, [1; 32])
            .expect_err("limited")
            .test_reason(),
        "enroll_lookup"
    );
    assert_eq!(
        w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "refused before the store"
    );
    // Another source isn't affected.
    let head = ghost.req(&w, "GET", &format!("/enroll/{}", ghost.id), b"");
    let route = Route::of(&head).expect("route");
    w.enroller
        .admit(&route, &head, &peer(2))
        .expect("another source");
    // The window ends.
    w.f.clocks.advance(60_000);
    assert_eq!(
        ghost.poll(&w).expect_err("unknown").test_reason(),
        "bad_signature"
    );
}

#[test]
fn a_busy_store_is_a_retryable_503() {
    // Terra 1 #6726: a lock timeout is retried by clients, not a 500.
    let w = world();
    *w.store.fail.lock().expect("lock") = Some(StoreFailed::Busy);
    let e = Worker::new("01a188cc-aaaa-7000-8000-000000000051")
        .enrol(&w, "a")
        .expect_err("busy");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::CoordStoreFailed, "store_busy")
    );
    let resp = e.response();
    assert_eq!(resp.status, 503);
    assert!(resp.headers.contains(&("Retry-After".into(), "1".into())));
    *w.store.fail.lock().expect("lock") = Some(StoreFailed::Failed);
    let e = Worker::new("01a188cc-aaaa-7000-8000-000000000052")
        .enrol(&w, "a")
        .expect_err("failed");
    assert_eq!(e.response().status, 500);
}

#[test]
fn a_signed_poll_returns_what_the_enrolment_response_did() {
    // Terra 1 #6726: the poll answers the pending request's own key with
    // the values the enrolment response carried, for a pinned worker to
    // check against its pins. A worker that lost that response doesn't
    // take them as pins: it starts over (orchestrator #8343, Terra 4
    // #8656). The server's answer is what this tests.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000053");
    let lost = m.enrol(&w, "a").expect("pending");
    let sent: Value = serde_json::from_slice(&lost.response.body).expect("json");
    assert_eq!(code(m.enrol(&w, "a")), (Code::ApiEnrollPending, "pending"));
    let polled = m.poll(&w).expect("poll");
    assert_eq!(polled["state"], "pending");
    assert_eq!(polled["ns"], sent["data"]["ns"]);
    assert_eq!(polled["expires_ms"], sent["data"]["expires_ms"]);
    assert_eq!(polled["pending_id"], sent["data"]["pending_id"]);
    m.reveal(&w, m.nc).expect("a pinned worker's reveal");
    // Another key polling that id gets no Ns, only the keyless refusal.
    let mut other = Worker::new("01a188cc-aaaa-7000-8000-000000000056");
    other.id = m.id.clone();
    let e = other.poll(&w).expect_err("another key");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiUnauthenticated, "bad_signature")
    );
}

#[test]
fn an_encoded_own_id_authenticates_and_a_re_encoding_after_signing_does_not() {
    // Terra 4 #9569: the id is decoded once (S4) to select the row; the
    // signature covers the raw path, so a spelling changed after signing
    // fails even when it decodes to the same id.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000061");
    m.enrol(&w, "e").expect("pending");
    let encoded = format!("/enroll/%30{}", &m.id[1..]);
    let polled = {
        let h = m.send(&w, "GET", &encoded, b"").expect("an encoded poll");
        let v: Value = serde_json::from_slice(&h.response.body).expect("json");
        v["data"].clone()
    };
    assert_eq!(polled["state"], "pending");
    // Signed raw, then sent encoded: the same id, another path.
    let mut head = m.req(&w, "GET", &format!("/enroll/{}", m.id), b"");
    head.path = encoded.clone();
    let route = Route::of(&head).expect("route");
    w.enroller.admit(&route, &head, &peer(1)).expect("admitted");
    let e = w
        .enroller
        .handle(&route, &head, b"", &peer(1))
        .expect_err("re-encoded");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    // Another id, encoded, is another id: this key can't poll it.
    let other = format!("/enroll/%30{}", "1a188cc-aaaa-7000-8000-000000000062");
    let e = m.send(&w, "GET", &other, b"").expect_err("another id");
    assert_eq!(e.code, Code::ApiUnauthenticated);
}

/// A pending key, signing through mdkg-wire's own signer: the server's
/// signature base is never what made the signature (Terra 5 #9849).
struct WireSigner {
    id: String,
    pk: [u8; 32],
}

impl mdkg_ops::wire::Signer for WireSigner {
    fn key_id(&self) -> &str {
        &self.id
    }
    fn alg(&self) -> &'static str {
        mdkg_ops::wire::profile::ALG
    }
    fn sign(&self, base: &[u8]) -> Result<mdkg_ops::wire::Signature, Box<mdkg_format::Diagnostic>> {
        Ok(mdkg_ops::wire::Signature(
            crate::auth::tests::fake_sign(&self.pk, base).to_vec(),
        ))
    }
}

#[test]
fn a_client_signed_encoded_path_is_verified_as_sent() {
    // Terra 5 #9849: a lowercase `%2d` for the id's `-`, signed by
    // wire's client code, verifies as sent: the server never canonicalizes
    // escapes before checking `@path`.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000071");
    m.enrol(&w, "f").expect("pending");
    let path = format!("/enroll/{}", m.id.replacen('-', "%2d", 1));
    let mut out = mdkg_wire::sign::Outgoing::new("GET", path.clone(), None);
    out.header("Mdkg-Graph", crate::auth::tests::GRAPH)
        .expect("graph");
    out.header("Mdkg-Boot", w.f.gate.boot_id().expect("boot"))
        .expect("boot");
    let signer = WireSigner {
        id: m.id.clone(),
        pk: m.pk,
    };
    let created = (w.f.gate.now_ms() / 1000) as i64;
    mdkg_wire::sign::sign_machine(&mut out, &signer, created, [7; 16]).expect("signed");
    let head = RequestHead {
        method: out.method.clone(),
        path: out.path.clone(),
        query: None,
        headers: out
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .collect(),
    };
    let route = Route::of(&head).expect("route");
    assert_eq!(route, Route::Poll(m.id.clone()), "decoded once");
    w.enroller.admit(&route, &head, &peer(1)).expect("admitted");
    let h = w
        .enroller
        .handle(&route, &head, b"", &peer(1))
        .expect("verified as sent");
    let v: Value = serde_json::from_slice(&h.response.body).expect("json");
    assert_eq!(v["data"]["state"], "pending");
    // The same request with its escape upper-cased after signing fails.
    let mut changed = head.clone();
    changed.path = path.replacen("%2d", "%2D", 1);
    let route = Route::of(&changed).expect("route");
    w.enroller
        .admit(&route, &changed, &peer(1))
        .expect("admitted");
    let e = w
        .enroller
        .handle(&route, &changed, b"", &peer(1))
        .expect_err("another @path");
    assert_eq!(e.code, Code::ApiUnauthenticated);
}

#[test]
fn a_pending_request_records_its_source() {
    // Terra 1 #6726: `machine list --pending` shows where it came from.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000054");
    let h = m.enrol(&w, "a").expect("pending");
    let row = w.store.pending.lock().expect("lock")[&m.id].0.clone();
    assert_eq!(row.source, "192.0.2.1");
    // The address stays in the row: the event, which every stream
    // subscriber sees, doesn't carry it under any key (#7049, #7348).
    let event = serde_json::to_string(&h.event.expect("event").data).expect("json");
    assert!(!event.contains("192.0.2.1"), "{event}");
    let mut u = Worker::new("01a188cc-aaaa-7000-8000-000000000055");
    let body = u.body("b", crate::auth::tests::GRAPH);
    let head = u.req(&w, "POST", "/enroll", &body);
    w.enroller
        .handle(&Route::Request, &head, &body, &crate::limits::Peer::Unix)
        .expect("over the socket");
    assert_eq!(
        w.store.pending.lock().expect("lock")[&u.id].0.source,
        "unix"
    );
}

#[test]
fn the_pairing_code_is_mdkg_wires_and_matches_its_vectors() {
    // Orchestrator #8168: the code is mdkg-wire's, checked here against
    // its published vectors, serve-core's S6 vector (Terra 1 #6726) among
    // them.
    assert_eq!(hex(&vectors::MACHINE.input()), vectors::MACHINE_INPUT_HEX);
    assert_eq!(machine_code(&vectors::MACHINE), vectors::MACHINE_CODE);
    let s6 = |serve_fp, machine_fp, graph_id, ns: &[u8; 32], nc: &[u8; 32]| {
        machine_code(&Pairing {
            serve_fp,
            machine_fp,
            graph_id,
            ns,
            nc,
        })
    };
    let base = s6("mdkg:serve", "mdkg:machine", "graph-1", &[1; 32], &[2; 32]);
    assert_eq!(base, vectors::S6_CODE);
    // Every field moves the code, and fields can't run into each other.
    for other in [
        s6("mdkg:serv", "emdkg:machine", "graph-1", &[1; 32], &[2; 32]),
        s6("mdkg:serve", "mdkg:machine", "graph-2", &[1; 32], &[2; 32]),
        s6("mdkg:serve", "mdkg:machine", "graph-1", &[3; 32], &[2; 32]),
        s6("mdkg:serve", "mdkg:machine", "graph-1", &[1; 32], &[4; 32]),
    ] {
        assert_ne!(other, base);
    }
    // The fingerprint a pending row records is wire's too.
    assert_eq!(fingerprint(&[0; 32]), vectors::FP_ZERO_ED25519);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_store_is_asked_when_the_key_directory_lags() {
    // Terra 6 #6939: a machine the store holds but the gate's key
    // directory hasn't loaded yet is refused, and so is a second request
    // for an id already pending; each refusal used its nonce (#8480).
    let w = world();
    let mut lagged = Worker::new("01a188cc-aaaa-7000-8000-000000000061");
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(lagged.id.clone(), (key_of("old"), "approved".into()));
    let before = w.f.gate.nonces_held();
    assert_eq!(
        code(lagged.enrol(&w, "a")),
        (Code::ApiEnrollExists, "exists")
    );
    assert_eq!(w.f.gate.nonces_held(), before + 1, "its nonce is used");
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000062");
    m.enrol(&w, "b").expect("pending");
    let held = w.f.gate.nonces_held();
    assert_eq!(code(m.enrol(&w, "b")), (Code::ApiEnrollPending, "pending"));
    assert_eq!(w.f.gate.nonces_held(), held + 1, "its nonce is used");
}

#[test]
fn a_signature_that_verifies_against_the_stand_in_key_takes_no_nonce() {
    // #6951: were a verifier ever to accept a signature against the
    // stand-in key (a full-order point since #6982), an unknown id is
    // still refused, before its nonce.
    let w = world();
    let mut forger = Worker::new("01a188cc-aaaa-7000-8000-000000000071");
    forger.pk = NO_KEY; // FakeVerifier accepts what it signs under this key.
    let before = w.f.gate.nonces_held();
    let e = forger.poll(&w).expect_err("unknown");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    let e = forger.reveal(&w, [1; 32]).expect_err("unknown");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    assert_eq!(w.f.gate.nonces_held(), before, "no nonce for an unknown id");
}

#[test]
fn the_stand_in_key_is_a_hash_nobody_chose() {
    // Terra 1 #6982: not the all-zero, small-order key a strict verifier
    // refuses early. Its bytes are SHA-256 of a fixed label; that they
    // decode to a full-order point was checked apart, with the curve's
    // own arithmetic, when it was picked.
    assert_ne!(NO_KEY, [0; 32]);
    let h: [u8; 32] = Sha256::digest(b"mdkg-serve no-key stand-in 19").into();
    assert_eq!(NO_KEY, h);
}

#[test]
fn four_workers_behind_one_source_poll_every_two_seconds_unlimited() {
    // Orchestrator #7056: a pending key's polls cost only its own bucket,
    // never the source's lookup budget.
    let w = world();
    let mut workers: Vec<Worker> = (0..4)
        .map(|i| Worker::new(&format!("01a188cc-aaaa-7000-8000-0000000008{i:02}")))
        .collect();
    for (i, m) in workers.iter_mut().enumerate() {
        m.enrol(&w, &format!("w{i}")).expect("pending");
    }
    // Two minutes: 240 polls from one address, four times the per-source
    // lookup budget.
    for _ in 0..60 {
        w.f.clocks.advance(2_000);
        for m in workers.iter_mut() {
            m.poll(&w).expect("a poll every 2 s is never limited");
        }
    }
    // Lookups of unknown ids from that address still have their budget.
    let mut ghost = Worker::new("01a188cc-aaaa-7000-8000-000000000899");
    assert_eq!(
        ghost.poll(&w).expect_err("unknown").test_reason(),
        "bad_signature"
    );
}

#[test]
fn the_lookup_table_keeps_room_for_the_unix_socket() {
    // Orchestrator #7056, applying #6822: TCP sources filling the lookup
    // table never refuse the socket's reveals and polls.
    let f = fixture();
    let store = Arc::new(FakeStore::default());
    let mut enroller = Enroller::new(f.gate.clone(), store.clone(), Limits::default());
    enroller.lookups = EnrollLimiter::new(LOOKUPS_PER_MINUTE, "enroll_lookup").with_capacity(2);
    let w = World { f, store, enroller };
    let mut ghost = Worker::new("01a188cc-aaaa-7000-8000-000000000901");
    let path = format!("/enroll/{}", ghost.id);
    for n in 1..=2u8 {
        let head = ghost.req(&w, "GET", &path, b"");
        let route = Route::of(&head).expect("route");
        w.enroller.admit(&route, &head, &peer(n)).expect("room");
        let _ = w.enroller.handle(&route, &head, b"", &peer(n));
    }
    // Two TCP sources fill the table; a third is refused before the store.
    let head = ghost.req(&w, "GET", &path, b"");
    let route = Route::of(&head).expect("route");
    assert_eq!(
        w.enroller
            .admit(&route, &head, &peer(3))
            .expect_err("table full")
            .test_reason(),
        "enroll_lookup"
    );
    let head = ghost.req(&w, "GET", &path, b"");
    let route = Route::of(&head).expect("route");
    w.enroller
        .admit(&route, &head, &crate::limits::Peer::Unix)
        .expect("the socket keeps its room");
    let e = w
        .enroller
        .handle(&route, &head, b"", &crate::limits::Peer::Unix)
        .expect_err("unknown");
    assert_eq!(e.test_reason(), "bad_signature");
}

#[test]
fn concurrent_copies_of_one_poll_charge_one_token() {
    // Orchestrator #7118: a pending key's copies sent at once spend one
    // token of its 1/s bucket (burst two), the rest are replays.
    let w = Arc::new(world());
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000a01");
    m.enrol(&w, "a").expect("pending");
    let head = m.req(&w, "GET", &format!("/enroll/{}", m.id), b"");
    let start = Arc::new(std::sync::Barrier::new(6));
    let results: Vec<_> = (0..6)
        .map(|_| {
            let (w, h, s) = (w.clone(), head.clone(), start.clone());
            std::thread::spawn(move || {
                s.wait();
                let route = Route::of(&h).expect("route");
                w.enroller
                    .handle(&route, &h, b"", &peer(1))
                    .map(|_| ())
                    .map_err(|e| e.test_reason())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().expect("thread"))
        .collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || *r == Err("nonce_reused"))
    );
    m.poll(&w).expect("the second token is still there");
    assert_eq!(
        m.poll(&w).expect_err("over the rate").test_reason(),
        "enroll_poll"
    );
}

#[test]
fn a_busy_store_at_the_known_id_lookup_is_a_503_and_a_resigned_retry_passes() {
    // #7182: the known-id lookup maps a busy store to a retryable 503.
    // The request used its nonce (#8480), so its retry is signed again, as
    // every retry is.
    let w = world();
    *w.store.fail_lookup.lock().expect("lock") = Some(StoreFailed::Busy);
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000b01");
    let e = m.enrol(&w, "a").expect_err("busy");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::CoordStoreFailed, "store_busy")
    );
    assert_eq!(e.response().status, 503);
    *w.store.fail_lookup.lock().expect("lock") = None;
    m.enrol(&w, "a").expect("the retry, signed again");
}

#[test]
fn a_body_for_another_graph_is_refused_before_the_known_id_checks() {
    // Terra 2 #7204: this server's Mdkg-Graph with another graph in the
    // signed body gets graph_unknown, whether the store holds the id as a
    // machine or as a pending request.
    let w = world();
    let mut machine = Worker::new("01a188cc-aaaa-7000-8000-000000000c01");
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(machine.id.clone(), (key_of("old"), "approved".into()));
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000c02");
    pending.enrol(&w, "p").expect("pending");
    for m in [&mut machine, &mut pending] {
        let body = m.body("a", "graph-other");
        assert_eq!(
            code(m.send(&w, "POST", "/enroll", &body)),
            (Code::ApiGraphUnknown, "graph_unknown")
        );
    }
}

/// A reveal or poll for `id` with no signature at all.
fn unsigned_poll(w: &World, id: &str) -> RequestHead {
    RequestHead {
        method: "GET".into(),
        path: format!("/enroll/{id}"),
        query: None,
        headers: vec![
            ("mdkg-graph".into(), crate::auth::tests::GRAPH.into()),
            ("mdkg-boot".into(), w.f.gate.boot_id().expect("boot")),
        ],
    }
}

#[test]
fn unsigned_polls_for_a_pending_and_an_unknown_id_are_limited_alike() {
    // Orchestrator #7392 and #7770: every request that fails to
    // authenticate costs the source, whatever the row, so a keyless caller
    // gets the same answers, refusal for refusal and Retry-After for
    // Retry-After, for an id that is pending and one that doesn't exist.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000d01");
    pending.enrol(&w, "p").expect("pending");
    let ghost = "01a188cc-aaaa-7000-8000-000000000d02";
    let answers = |id: &str, from: u8| -> Vec<(Code, &'static str, Option<u64>)> {
        (0..61)
            .map(|_| {
                let head = unsigned_poll(&w, id);
                let route = Route::of(&head).expect("route");
                let e = w
                    .enroller
                    .admit(&route, &head, &peer(from))
                    .and_then(|()| {
                        w.enroller
                            .handle(&route, &head, b"", &peer(from))
                            .map(|_| ())
                    })
                    .expect_err("never authenticated");
                (e.code, e.test_reason(), e.retry_after)
            })
            .collect()
    };
    let for_pending = answers(&pending.id, 7);
    let for_ghost = answers(ghost, 8);
    assert_eq!(for_pending, for_ghost);
    assert!(
        for_pending[..60]
            .iter()
            .all(|a| a.0 == Code::ApiUnauthenticated)
    );
    assert_eq!(
        (for_pending[60].0, for_pending[60].1),
        (Code::ApiRateLimited, "enroll_lookup")
    );
    // The verified pending key still polls: it pays only its own bucket.
    pending.poll(&w).expect("the key's own poll");
}

#[test]
fn a_foreign_key_learns_only_that_an_id_exists_whatever_its_state() {
    // Orchestrator #7853, Terra 3 #7835: a self-signed enrolment with
    // another key gets enroll_exists for a pending id and for an approved
    // one alike, with the same data; only the pending row's own key
    // learns enroll_pending.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000e01");
    pending.enrol(&w, "p").expect("pending");
    let approved = "01a188cc-aaaa-7000-8000-000000000e02";
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(approved.into(), (key_of(approved), "approved".into()));
    // One foreign key, a fresh nonce for each request.
    let foreign = |id: &str, n: u32| {
        let mut f = Worker::new(id);
        f.pk = key_of("foreign");
        f.n = n;
        let e = f.enrol(&w, "f").expect_err("exists");
        (e.code, e.test_reason(), e.data)
    };
    let a = foreign(&pending.id, 0);
    let b = foreign(approved, 1);
    assert_eq!(a, b);
    assert_eq!((a.0, a.1), (Code::ApiEnrollExists, "exists"));
    assert_eq!(
        code(pending.enrol(&w, "p")),
        (Code::ApiEnrollPending, "pending"),
        "the row's own key"
    );
}

#[test]
fn a_reveal_or_poll_malformed_or_unsigned_never_reaches_the_store() {
    // Orchestrator #7278: the request's form is checked before the
    // store is read, so unsigned or malformed requests can't time the
    // lookup; the source is still charged.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000e11");
    pending.enrol(&w, "p").expect("pending");
    let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
    let head = unsigned_poll(&w, &pending.id);
    let route = Route::of(&head).expect("route");
    let e = w
        .enroller
        .handle(&route, &head, b"", &peer(3))
        .expect_err("unsigned");
    assert_eq!(e.code, Code::ApiUnauthenticated);
    // A reveal whose body has no Nc is refused before the store too.
    let mut reveal = Worker::new(&pending.id);
    reveal.pk = pending.pk;
    let path = format!("/enroll/{}:reveal", pending.id);
    let bad = reveal.req(&w, "POST", &path, b"{}");
    let route = Route::of(&bad).expect("route");
    assert_eq!(
        code(w.enroller.handle(&route, &bad, b"{}", &peer(3))),
        (Code::UsageInvalidArguments, "nc")
    );
    // A repeated Content-Digest too: the signature's parse refuses it
    // before the digest check (Opus #8547).
    let mut twice = reveal.req(&w, "POST", &path, b"{}");
    let digest_value = twice
        .headers
        .iter()
        .find(|(n, _)| n == "content-digest")
        .map(|(_, v)| v.clone())
        .expect("a digest");
    twice.headers.push(("content-digest".into(), digest_value));
    let route = Route::of(&twice).expect("route");
    assert_eq!(
        code(w.enroller.handle(&route, &twice, b"{}", &peer(3))),
        (Code::ApiUnauthenticated, "header_repeated")
    );
    // A malformed Content-Digest too (Terra 1 #8339).
    let mut digest = reveal.req(&w, "POST", &path, b"{}");
    for (n, v) in digest.headers.iter_mut() {
        if n == "content-digest" {
            *v = "sha-256=:not-base64:".into();
        }
    }
    let route = Route::of(&digest).expect("route");
    assert_eq!(
        code(w.enroller.handle(&route, &digest, b"{}", &peer(3))),
        (Code::ApiUnauthenticated, "bad_digest")
    );
    assert_eq!(
        w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "no lookup for any"
    );
}

#[test]
fn a_wrong_key_refusal_carries_no_ns() {
    // Terra 3 #7348: the keyless refusal's data holds nothing of the row.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000e21");
    pending.enrol(&w, "p").expect("pending");
    let mut other = Worker::new("01a188cc-aaaa-7000-8000-000000000e22");
    other.id = pending.id.clone();
    let e = other.poll(&w).expect_err("wrong key");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiUnauthenticated, "bad_signature")
    );
    assert!(e.data.is_empty(), "{:?}", e.data);
    let body = String::from_utf8(e.response().body).expect("utf8");
    assert!(!body.contains("\"ns\""), "{body}");
}

fn world_with_pending_nonces(capacity: usize) -> World {
    let mut f = fixture();
    let gate = std::mem::replace(&mut f.gate, fixture().gate);
    f.gate = Arc::new(
        Arc::try_unwrap(gate)
            .expect("one owner")
            .with_pending_nonces(crate::nonce::NonceCache::new(capacity)),
    );
    let store = Arc::new(FakeStore::default());
    let enroller = Enroller::new(f.gate.clone(), store.clone(), Limits::default());
    World { f, store, enroller }
}

#[test]
fn a_replayed_enrolment_is_refused_as_a_replay_before_the_store() {
    // Terra 3 #8179, orchestrator #8197: a relay holding the signed
    // POST /enroll, not the key, learns nothing of the id's state by
    // sending it again: before approval and after, it is a replay, and
    // the store isn't asked.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f01");
    let body = m.body("box", crate::auth::tests::GRAPH);
    let head = m.req(&w, "POST", "/enroll", &body);
    let send = |w: &World| {
        let route = Route::of(&head).expect("route");
        w.enroller.admit(&route, &head, &peer(1))?;
        w.enroller.handle(&route, &head, &body, &peer(1))
    };
    send(&w).expect("pending");
    let replay = |w: &World| {
        let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
        let got = code(send(w));
        assert_eq!(
            w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "the store isn't asked"
        );
        got
    };
    assert_eq!(replay(&w), (Code::ApiReplayed, "nonce_reused"));
    // Approved (the host's CAS, simulated): the same answer.
    w.store.pending.lock().expect("lock").clear();
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(m.id.clone(), (m.pk, "approved".into()));
    assert_eq!(replay(&w), (Code::ApiReplayed, "nonce_reused"));
}

#[test]
fn a_request_that_loses_the_store_race_learns_only_its_own_keys_state() {
    // Terra 3 #8179: two keys both find the id unused; the one whose
    // insert comes second is classified in the store's transaction. A
    // foreign key learns only that the id exists, and the row's own key
    // that it is pending.
    let w = world();
    let mut first = Worker::new("01a188cc-aaaa-7000-8000-000000000f11");
    first.enrol(&w, "a").expect("pending");
    w.store
        .blind
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut foreign = Worker::new(&first.id);
    foreign.pk = key_of("foreign");
    let e = foreign.enrol(&w, "b").expect_err("exists");
    assert_eq!((e.code, e.test_reason()), (Code::ApiEnrollExists, "exists"));
    assert_eq!(
        code(first.enrol(&w, "a")),
        (Code::ApiEnrollPending, "pending"),
        "the row's own key"
    );
    // An approved machine's id, raced the same way, exists for every key.
    let approved = "01a188cc-aaaa-7000-8000-000000000f12";
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(approved.into(), (key_of(approved), "approved".into()));
    assert_eq!(
        code(Worker::new(approved).enrol(&w, "c")),
        (Code::ApiEnrollExists, "exists")
    );
}

#[test]
fn a_verified_poll_on_a_full_cache_costs_its_source_nothing() {
    // Orchestrator #8197, Terra 2 #8164: a request whose signature
    // verified is refused as full and never charged to the source window;
    // only requests that fail to authenticate are.
    let w = world_with_pending_nonces(1);
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f21");
    m.enrol(&w, "box").expect("pending, using the one place");
    // The source spends all but one of its window on failures.
    let mut ghost = Worker::new("01a188cc-aaaa-7000-8000-000000000f22");
    for _ in 1..LOOKUPS_PER_MINUTE {
        assert_eq!(
            ghost.poll(&w).expect_err("unknown").test_reason(),
            "bad_signature"
        );
    }
    // Verified polls from that source, inside the same window, are
    // refused as full and cost it nothing.
    for _ in 0..10 {
        w.f.clocks.advance(1_000);
        assert_eq!(
            m.poll(&w).expect_err("full").test_reason(),
            "nonce_cache_full",
            "verified"
        );
    }
    assert_eq!(
        ghost.poll(&w).expect_err("last").test_reason(),
        "bad_signature"
    );
    assert_eq!(
        ghost.poll(&w).expect_err("spent").test_reason(),
        "enroll_lookup"
    );
}

#[test]
fn a_malformed_reveal_costs_its_source_and_its_origin_comes_first() {
    // Opus #8132, Terra 3 #8179: a reveal with no Nc is refused before
    // the store and charged to its source like any failure; and a refused
    // Origin is answered as such before the body is read.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f31");
    m.enrol(&w, "box").expect("pending");
    let path = format!("/enroll/{}:reveal", m.id);
    let send = |head: RequestHead| {
        let route = Route::of(&head).expect("route");
        w.enroller.admit(&route, &head, &peer(3))?;
        w.enroller.handle(&route, &head, b"{}", &peer(3))
    };
    let mut origin = m.req(&w, "POST", &path, b"{}");
    origin
        .headers
        .push(("origin".into(), "https://evil.example".into()));
    assert_eq!(
        code(send(origin)),
        (Code::ApiOriginRefused, "origin"),
        "the origin before the body"
    );
    let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
    for _ in 1..LOOKUPS_PER_MINUTE {
        assert_eq!(
            code(send(m.req(&w, "POST", &path, b"{}"))),
            (Code::UsageInvalidArguments, "nc")
        );
    }
    assert_eq!(
        code(send(m.req(&w, "POST", &path, b"{}"))),
        (Code::ApiRateLimited, "enroll_lookup")
    );
    assert_eq!(
        w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "none reached the store"
    );
}

#[test]
fn a_signed_enrolment_with_a_bad_form_uses_no_nonce_and_no_store() {
    // Terra 7 #8463, orchestrator #8480: a request missing its
    // commitment, or with a bad name, is refused after its origin and
    // before its signature, the store or a nonce.
    let w = world();
    for (body, reason) in [
        (
            json!({"machine_id": "01a188cc-aaaa-7000-8000-000000000f41",
                   "public_key": b64::encode_url(&key_of("01a188cc-aaaa-7000-8000-000000000f41")),
                   "name": "box", "graph_id": crate::auth::tests::GRAPH}),
            "commitment",
        ),
        (
            json!({"machine_id": "01a188cc-aaaa-7000-8000-000000000f41",
                   "public_key": b64::encode_url(&key_of("01a188cc-aaaa-7000-8000-000000000f41")),
                   "name": "Bad Name", "graph_id": crate::auth::tests::GRAPH,
                   "commitment": b64::encode_url(&[1; 32])}),
            "name",
        ),
    ] {
        let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f41");
        let body = serde_json::to_vec(&body).expect("json");
        let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            code(m.send(&w, "POST", "/enroll", &body)),
            (Code::UsageInvalidArguments, reason)
        );
        assert_eq!(
            w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "{reason}: no store"
        );
        assert_eq!(w.f.gate.nonces_held(), 0, "{reason}: no nonce");
    }
}

#[test]
fn a_malformed_enrolment_gets_one_answer_whatever_the_ids_state() {
    // Terra 7 #8632, orchestrator #8648: the form is refused before the
    // key directory or the store is consulted, so a foreign key's
    // malformed request for a pending, an approved or an unknown id gets
    // the same answer.
    let w = world();
    let mut pending = Worker::new("01a188cc-aaaa-7000-8000-000000000f51");
    pending.enrol(&w, "p").expect("pending");
    let approved = "01a188cc-aaaa-7000-8000-000000000f52";
    w.store
        .machines
        .lock()
        .expect("lock")
        .insert(approved.into(), (key_of(approved), "approved".into()));
    w.f.keys.0.lock().expect("lock").insert(
        approved.into(),
        mdkg_ops::wire::MachineKey {
            machine: approved.into(),
            public_key: key_of(approved),
            state: mdkg_ops::wire::MachineState::Approved,
        },
    );
    let unknown = "01a188cc-aaaa-7000-8000-000000000f53";
    let answers: Vec<_> = [pending.id.as_str(), approved, unknown]
        .iter()
        .enumerate()
        .map(|(n, id)| {
            let mut f = Worker::new(id);
            f.pk = key_of("foreign");
            f.n = n as u32 * 10;
            let body = serde_json::to_vec(&json!({
                "machine_id": id,
                "public_key": b64::encode_url(&f.pk),
                "name": "f",
                "graph_id": crate::auth::tests::GRAPH,
            }))
            .expect("json");
            let e = f.send(&w, "POST", "/enroll", &body).expect_err("refused");
            (e.code, e.test_reason(), e.data)
        })
        .collect();
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[1], answers[2]);
    assert_eq!(
        (answers[0].0, answers[0].1),
        (Code::UsageInvalidArguments, "commitment")
    );
}

#[test]
fn the_form_is_refused_before_the_signature_is_checked() {
    // Terra 1 #8723: a malformed enrolment signed with the wrong key gets
    // the form's refusal, not the signature's: the form comes first.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f61");
    m.pk = key_of("not-the-bodys-key");
    let body = serde_json::to_vec(&json!({
        "machine_id": m.id,
        "public_key": b64::encode_url(&key_of("the-bodys-key")),
        "name": "box",
        "graph_id": crate::auth::tests::GRAPH,
    }))
    .expect("json");
    assert_eq!(
        code(m.send(&w, "POST", "/enroll", &body)),
        (Code::UsageInvalidArguments, "commitment")
    );
    // The same body made well-formed is refused for its signature.
    let mut ok: Value = serde_json::from_slice(&body).expect("json");
    ok["commitment"] = json!(b64::encode_url(&[1; 32]));
    let ok = serde_json::to_vec(&ok).expect("json");
    assert_eq!(
        code(m.send(&w, "POST", "/enroll", &ok)),
        (Code::ApiUnauthenticated, "bad_signature")
    );
}

#[test]
fn replayed_polls_are_charged_to_their_source_and_then_refused_before_the_store() {
    // Terra 5 #8890, orchestrator #8788: a captured, verified poll sent
    // again costs a store lookup before its nonce check refuses it, so
    // each replay is charged to the source window; once it is spent the
    // next request is refused before the store.
    let w = world();
    let mut m = Worker::new("01a188cc-aaaa-7000-8000-000000000f71");
    m.enrol(&w, "box").expect("pending");
    let head = m.req(&w, "GET", &format!("/enroll/{}", m.id), b"");
    let route = Route::of(&head).expect("route");
    let send = || {
        w.enroller.admit(&route, &head, &peer(4))?;
        w.enroller.handle(&route, &head, b"", &peer(4))
    };
    send().expect("the original poll");
    for _ in 0..LOOKUPS_PER_MINUTE {
        assert_eq!(code(send()), (Code::ApiReplayed, "nonce_reused"));
    }
    let before = w.store.lookups.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(code(send()), (Code::ApiRateLimited, "enroll_lookup"));
    assert_eq!(
        w.store.lookups.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "refused before the store"
    );
}
