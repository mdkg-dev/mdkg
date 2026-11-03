use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use mdkg_ops::wire::{MachineState, Signer};
use mdkg_wire::keys::MachineKey;
use mdkg_wire::pair::vectors;
use mdkg_wire::person::possession_challenge;
use sha2::{Digest, Sha256};

use super::*;

/// The candidate's `Nc`, which the row's commitment is to.
const NC: [u8; 32] = [5; 32];

const GRAPH: &str = "01a18800-0000-7000-8000-000000000001";

/// A change to the rows, made inside a proof's transaction.
type Change = Box<dyn FnOnce(&mut HashMap<String, PersonPendingRow>) + Send>;

/// serve-store's `person_prove`, in memory: the lock is the transaction.
#[derive(Default)]
struct FakeStore {
    rows: Mutex<HashMap<String, PersonPendingRow>>,
    /// Machines the store holds other than approved.
    machines: Mutex<HashMap<String, MachineState>>,
    /// Runs inside the transaction before the row is read: what changed
    /// since the request was authenticated.
    meanwhile: Mutex<Option<Change>>,
    marks: std::sync::atomic::AtomicUsize,
    /// Rows' expiry, judged with the clock read inside the transaction.
    expires: Mutex<HashMap<String, u64>>,
    /// True while the fake's transaction is held.
    in_tx: std::sync::atomic::AtomicBool,
    /// The `Nc` recorded with each proof.
    revealed: Mutex<HashMap<String, [u8; 32]>>,
}

impl PersonPendingStore for FakeStore {
    fn prove(
        &self,
        id: &str,
        machine: &str,
        nc: &[u8; 32],
        now: &dyn Fn() -> u64,
        verify: &mut dyn FnMut(&PersonPendingRow) -> bool,
    ) -> Result<Proved, StoreFailed> {
        let mut rows = self.rows.lock().expect("lock");
        self.in_tx.store(true, Ordering::SeqCst);
        let got = self.prove_held(&mut rows, id, machine, nc, now, verify);
        self.in_tx.store(false, Ordering::SeqCst);
        got
    }
}

impl FakeStore {
    fn prove_held(
        &self,
        rows: &mut HashMap<String, PersonPendingRow>,
        id: &str,
        machine: &str,
        nc: &[u8; 32],
        now: &dyn Fn() -> u64,
        verify: &mut dyn FnMut(&PersonPendingRow) -> bool,
    ) -> Result<Proved, StoreFailed> {
        if let Some(change) = self.meanwhile.lock().expect("lock").take() {
            change(rows);
        }
        // The clock, read once the transaction is held.
        let now = now();
        if let Some(&until) = self.expires.lock().expect("lock").get(id)
            && now >= until
        {
            rows.remove(id);
        }
        if let Some(state) = self.machines.lock().expect("lock").get(machine) {
            return Ok(Proved::Machine(*state));
        }
        let Some(r) = rows.get_mut(id).filter(|r| r.machine_id == machine) else {
            return Ok(Proved::NotPending);
        };
        // As serve-store does: the commitment first, then the verifier.
        if !mdkg_wire::pair::reveal_matches(&r.commitment, nc) {
            return Ok(Proved::WrongNc);
        }
        if !verify(r) {
            return Ok(Proved::Invalid);
        }
        if r.proved_ms.is_none() {
            self.marks.fetch_add(1, Ordering::Relaxed);
            r.proved_ms = Some(now);
            self.revealed.lock().expect("lock").insert(id.into(), *nc);
        }
        Ok(Proved::Proved)
    }
}

fn proved(store: &FakeStore, id: &str) -> bool {
    store.rows.lock().expect("lock")[id].proved_ms.is_some()
}

/// The candidate: an Ed25519 file key, which wire's check verifies for
/// real.
fn candidate() -> MachineKey {
    MachineKey::from_parts("candidate", [7; 32])
}

fn row(id: &str, machine: &str) -> PersonPendingRow {
    PersonPendingRow {
        pending_id: id.into(),
        machine_id: machine.into(),
        person: "human:nick".into(),
        kind: PersonKeyKind::File,
        public_key: candidate().public_key().to_vec(),
        policy: "up".into(),
        ns: [3; 32],
        commitment: mdkg_wire::pair::commitment(&NC),
        proved_ms: None,
    }
}

fn world() -> (Arc<FakeStore>, PersonProofs) {
    let store = Arc::new(FakeStore::default());
    store
        .rows
        .lock()
        .expect("lock")
        .insert("p1".into(), row("p1", "m-ok"));
    let proofs = PersonProofs::new(GRAPH.into(), store.clone());
    (store, proofs)
}

fn signed(challenge: &[u8]) -> Vec<u8> {
    candidate().sign(challenge).expect("sign").0
}

fn body(proof: &[u8]) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"alg": "ed25519", "proof": b64::encode_url(proof), "nc": b64::encode_url(&NC)}),
    )
    .expect("json")
}

#[test]
fn the_challenge_is_mdkg_wires_published_vector() {
    // Orchestrator #8168: wire's pair::vectors::POSSESSION_CHALLENGE_HEX,
    // pending id ...0003, Ns bytes 0x00 to 0x1f, the commitment SHA-256 of
    // 32 bytes of 0xa5, graph ...0001.
    let commitment: [u8; 32] = Sha256::digest(vectors::NC).into();
    let issued = Possession {
        pending_id: vectors::POSSESSION_PENDING_ID,
        ns: &vectors::NS,
        commitment: &commitment,
        graph_id: GRAPH,
    };
    let hex: String = issued
        .challenge()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(hex, vectors::POSSESSION_CHALLENGE_HEX);
}

#[test]
fn a_valid_proof_marks_the_row_proved() {
    let (store, proofs) = world();
    let r = row("p1", "m-ok");
    let challenge = possession_challenge("p1", &r.ns, &r.commitment, GRAPH);
    let resp = proofs
        .prove(
            &Authenticated::for_tests("m-ok"),
            "p1",
            &body(&signed(&challenge)),
            &|| 0,
        )
        .expect("proved");
    assert_eq!(resp.status, 200);
    assert!(proved(&store, "p1"));
}

#[test]
fn a_bad_proof_or_another_machine_or_another_request_is_refused() {
    let (store, proofs) = world();
    let r = row("p1", "m-ok");
    let good = signed(&possession_challenge("p1", &r.ns, &r.commitment, GRAPH));
    // A proof over another graph's challenge, a proof under another alg,
    // and one by another key.
    let other_graph = signed(&possession_challenge(
        "p1",
        &r.ns,
        &r.commitment,
        "graph-other",
    ));
    let other_alg =
        serde_json::to_vec(&json!({"alg": "ecdsa-p256-sha256", "proof": b64::encode_url(&good), "nc": b64::encode_url(&NC)}))
            .expect("json");
    let other_key = MachineKey::from_parts("other", [8; 32])
        .sign(&possession_challenge("p1", &r.ns, &r.commitment, GRAPH))
        .expect("sign")
        .0;
    for b in [body(&other_graph), other_alg, body(&other_key)] {
        let e = proofs
            .prove(&Authenticated::for_tests("m-ok"), "p1", &b, &|| 0)
            .expect_err("bad proof");
        assert_eq!(
            (e.code, e.test_reason()),
            (Code::ApiUnauthenticated, "bad_proof")
        );
    }
    // Another machine, and an unknown request, are refused alike.
    for (machine, id) in [("m-else", "p1"), ("m-ok", "p9")] {
        let e = proofs
            .prove(&Authenticated::for_tests(machine), id, &body(&good), &|| 0)
            .expect_err("refused");
        assert_eq!(
            (e.code, e.test_reason()),
            (Code::ApiEnrollChanged, "not_pending"),
            "{machine} {id}"
        );
    }
    assert!(!proved(&store, "p1"));
    // A body without a proof.
    let e = proofs
        .prove(&Authenticated::for_tests("m-ok"), "p1", b"{}", &|| 0)
        .expect_err("no proof");
    assert_eq!(e.code, Code::UsageInvalidArguments);
}

#[test]
fn a_fido2_row_needs_a_webauthn_assertion() {
    // A bare Ed25519 signature doesn't prove a FIDO2 key: wire's check
    // reads the assertion and its flags.
    let (store, proofs) = world();
    let mut r = row("p1", "m-ok");
    r.kind = PersonKeyKind::Fido2;
    store
        .rows
        .lock()
        .expect("lock")
        .insert("p1".into(), r.clone());
    let bare = signed(&possession_challenge("p1", &r.ns, &r.commitment, GRAPH));
    let b = serde_json::to_vec(&json!({"alg": "fido2-ed25519", "proof": b64::encode_url(&bare), "nc": b64::encode_url(&NC)}))
        .expect("json");
    let e = proofs
        .prove(&Authenticated::for_tests("m-ok"), "p1", &b, &|| 0)
        .expect_err("not an assertion");
    assert_eq!(e.test_reason(), "bad_proof");
}

/// A FIDO2 Ed25519 assertion by the candidate over `challenge`, with
/// `flags` (UP 0x01, UV 0x04), as wire's check reads it.
fn assertion(challenge: &[u8], flags: u8) -> Vec<u8> {
    let mut auth = Sha256::digest(mdkg_wire::person::RP_ID.as_bytes()).to_vec();
    auth.push(flags);
    auth.extend_from_slice(&1u32.to_be_bytes());
    let msg = mdkg_wire::person::assertion_message(&auth, challenge);
    mdkg_wire::person::encode_assertion(&auth, &signed(&msg))
        .expect("an assertion")
        .0
}

fn fido2_body(proof: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({"alg": "fido2-ed25519", "proof": b64::encode_url(proof), "nc": b64::encode_url(&NC)}))
        .expect("json")
}

#[test]
fn a_fido2_proof_follows_the_rows_policy_after_a_restart() {
    // Terra 5 #8310, Opus #8308, orchestrator #8279: the policy is read
    // from the stored row at every proof, so a serve that restarted
    // between the request and its proof still needs UV under `uv`.
    for (policy, flags, ok) in [
        ("uv", 0x01, false),
        ("uv", 0x04, false),
        ("uv", 0x05, true),
        ("up", 0x01, true),
        ("up", 0x00, false),
    ] {
        let store = Arc::new(FakeStore::default());
        let mut r = row("p1", "m-ok");
        r.kind = PersonKeyKind::Fido2;
        r.policy = policy.into();
        store
            .rows
            .lock()
            .expect("lock")
            .insert("p1".into(), r.clone());
        // A fresh endpoint over the same store: the restart.
        drop(PersonProofs::new(GRAPH.into(), store.clone()));
        let proofs = PersonProofs::new(GRAPH.into(), store.clone());
        let challenge = possession_challenge("p1", &r.ns, &r.commitment, GRAPH);
        let got = proofs.prove(
            &Authenticated::for_tests("m-ok"),
            "p1",
            &fido2_body(&assertion(&challenge, flags)),
            &|| 0,
        );
        assert_eq!(got.is_ok(), ok, "{policy} {flags:#x}: {got:?}");
        assert_eq!(proved(&store, "p1"), ok, "{policy} {flags:#x}");
        if !ok {
            assert_eq!(got.expect_err("refused").test_reason(), "bad_proof");
        }
    }
}

#[test]
fn a_row_with_a_policy_this_build_cant_read_is_refused() {
    let (store, proofs) = world();
    store
        .rows
        .lock()
        .expect("lock")
        .get_mut("p1")
        .expect("row")
        .policy = "maybe".into();
    let r = row("p1", "m-ok");
    let good = signed(&possession_challenge("p1", &r.ns, &r.commitment, GRAPH));
    let e = proofs
        .prove(&Authenticated::for_tests("m-ok"), "p1", &body(&good), &|| 0)
        .expect_err("refused");
    assert_eq!(e.code, Code::CoordStoreFailed);
    assert!(!proved(&store, "p1"));
}

#[test]
fn what_changed_since_authentication_is_judged_in_the_transaction() {
    // Terra 3 #8363, Opus #8308: the machine revoked, the row expired or
    // rejected, or its id reused for another request after the proof's
    // request was authenticated: the store reads each in the transaction
    // that would mark the row, and the proof of the old row marks
    // nothing.
    let r = row("p1", "m-ok");
    let good = body(&signed(&possession_challenge(
        "p1",
        &r.ns,
        &r.commitment,
        GRAPH,
    )));
    let me = Authenticated::for_tests("m-ok");
    // Revoked.
    let (store, proofs) = world();
    store
        .machines
        .lock()
        .expect("lock")
        .insert("m-ok".into(), MachineState::Revoked);
    let e = proofs.prove(&me, "p1", &good, &|| 0).expect_err("revoked");
    assert_eq!(e.code, Code::ApiMachineRevoked);
    assert!(!proved(&store, "p1"));
    // Expired or rejected.
    let (store, proofs) = world();
    *store.meanwhile.lock().expect("lock") = Some(Box::new(|rows| {
        rows.clear();
    }));
    let e = proofs.prove(&me, "p1", &good, &|| 0).expect_err("gone");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiEnrollChanged, "not_pending")
    );
    // The id reused by another request, another key's.
    let (store, proofs) = world();
    *store.meanwhile.lock().expect("lock") = Some(Box::new(|rows| {
        let mut other = row("p1", "m-ok");
        other.public_key = MachineKey::from_parts("other", [8; 32])
            .public_key()
            .to_vec();
        other.ns = [9; 32];
        rows.insert("p1".into(), other);
    }));
    let e = proofs
        .prove(&me, "p1", &good, &|| 0)
        .expect_err("another row");
    assert_eq!(e.test_reason(), "bad_proof");
    assert!(!proved(&store, "p1"));
    assert_eq!(store.marks.load(Ordering::Relaxed), 0);
}

#[test]
fn a_second_proof_of_a_proved_row_is_200_and_marks_nothing() {
    let (store, proofs) = world();
    let r = row("p1", "m-ok");
    let good = body(&signed(&possession_challenge(
        "p1",
        &r.ns,
        &r.commitment,
        GRAPH,
    )));
    let me = Authenticated::for_tests("m-ok");
    proofs.prove(&me, "p1", &good, &|| 5).expect("proved");
    assert_eq!(
        proofs.prove(&me, "p1", &good, &|| 9).expect("again").status,
        200
    );
    assert_eq!(store.marks.load(Ordering::Relaxed), 1);
    assert_eq!(store.rows.lock().expect("lock")["p1"].proved_ms, Some(5));
}

#[test]
fn expiry_is_judged_by_the_clock_read_in_the_transaction() {
    // Terra 1 #8585, orchestrator #8601: a proof received before its row
    // expires but whose transaction starts after does not prove it.
    let (store, proofs) = world();
    store
        .expires
        .lock()
        .expect("lock")
        .insert("p1".into(), 1_000);
    let r = row("p1", "m-ok");
    let good = body(&signed(&possession_challenge(
        "p1",
        &r.ns,
        &r.commitment,
        GRAPH,
    )));
    // 999 until the transaction is held, 1000 inside it: a clock read
    // before the transaction would see the row unexpired (Terra 2 #8805).
    let held = store.clone();
    let now = move || {
        if held.in_tx.load(Ordering::SeqCst) {
            1_000
        } else {
            999
        }
    };
    let e = proofs
        .prove(&Authenticated::for_tests("m-ok"), "p1", &good, &now)
        .expect_err("expired by the time the transaction held");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiEnrollChanged, "not_pending")
    );
    assert_eq!(store.marks.load(Ordering::Relaxed), 0);
}

#[test]
fn a_bad_proof_of_a_proved_row_is_refused() {
    // Opus #8547: the verifier runs on every call, so a proved row gives
    // 200 only to a proof that verifies.
    let (store, proofs) = world();
    let r = row("p1", "m-ok");
    let me = Authenticated::for_tests("m-ok");
    let good = body(&signed(&possession_challenge(
        "p1",
        &r.ns,
        &r.commitment,
        GRAPH,
    )));
    proofs.prove(&me, "p1", &good, &|| 5).expect("proved");
    let e = proofs
        .prove(&me, "p1", &body(b"garbage"), &|| 6)
        .expect_err("not a proof");
    assert_eq!(e.test_reason(), "bad_proof");
    assert_eq!(store.marks.load(Ordering::Relaxed), 1);
}

#[test]
fn the_proof_reveals_nc_and_a_wrong_one_is_refused() {
    // Terra 6 #8558, orchestrator #8788: the bootstrap path has no reveal
    // route, so :prove carries Nc; it must match the commitment, and it is
    // recorded with the proof in the one transaction.
    let (store, proofs) = world();
    let r = row("p1", "m-ok");
    let proof = signed(&possession_challenge("p1", &r.ns, &r.commitment, GRAPH));
    let me = Authenticated::for_tests("m-ok");
    let with = |nc: Option<[u8; 32]>| {
        let mut v = json!({"alg": "ed25519", "proof": b64::encode_url(&proof)});
        if let Some(nc) = nc {
            v["nc"] = json!(b64::encode_url(&nc));
        }
        serde_json::to_vec(&v).expect("json")
    };
    let e = proofs
        .prove(&me, "p1", &with(None), &|| 0)
        .expect_err("no nc");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::UsageInvalidArguments, "nc")
    );
    let e = proofs
        .prove(&me, "p1", &with(Some([6; 32])), &|| 0)
        .expect_err("another nc");
    assert_eq!(
        (e.code, e.test_reason()),
        (Code::ApiEnrollChanged, "commitment")
    );
    assert!(!proved(&store, "p1"));
    assert!(store.revealed.lock().expect("lock").is_empty());
    proofs
        .prove(&me, "p1", &with(Some(NC)), &|| 0)
        .expect("proved");
    assert_eq!(store.revealed.lock().expect("lock")["p1"], NC);
}
