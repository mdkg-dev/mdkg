//! Regression tests for the 2026-10-18 correctness review. Each test is the
//! reviewer's failing scenario with the assertion inverted.

use mdkg_coord::aliases::Aliases;
use std::collections::BTreeMap;

use mdkg_coord::{
    Agent, Changes, Coordinator, Error, Observation, Request, Scope, Settlement, Status,
};

fn s(x: &str) -> Scope {
    Scope::new(x)
}

fn one(leaf: &str, content: &str) -> Changes {
    BTreeMap::from([(s(leaf), Some(content.to_string()))])
}

fn leaves(pairs: &[(&str, &str)]) -> BTreeMap<Scope, String> {
    pairs.iter().map(|(l, c)| (s(l), c.to_string())).collect()
}

/// Finding 1: an offline lease based on a subtree snapshot taken before a
/// child commit can't be confirmed. Snapshots are computed from leaves, so
/// the child commit changes the subtree's snapshot with no refresh step.
#[test]
fn child_commit_makes_offline_subtree_lease_stale() {
    let mut c = Coordinator::new(10);
    c.register(s("root/a"), "P@0").unwrap();
    c.register(s("root/a/y"), "C@0").unwrap();
    let mut x = Agent::new("x", &c);
    let mut y = Agent::new("y", &c);
    x.disconnect(&c);
    let prov = x.claim_offline(&s("root/a"), 0, 10).unwrap();
    let l = y.claim(&mut c, &s("root/a/y"), 1).unwrap();
    y.commit(&mut c, &l, &one("root/a/y", "C@1"), None, 2)
        .unwrap();
    x.reconnect();
    let (_, status) = x.reconcile(&mut c, &prov, 3).unwrap();
    assert_eq!(status, Status::Conflict, "stale base must not be confirmed");
}

/// Finding 1, for settlement: an observation of main read before a later
/// change to a leaf can't override that change; a fresh read can.
#[test]
fn racy_observation_is_refused() {
    let mut c = Coordinator::new(10);
    c.register(s("root/a"), "P@0").unwrap();
    c.register(s("root/a/y"), "C@0").unwrap();
    let main = leaves(&[("root/a", "P@0"), ("root/a/y", "E@1")]);
    let stale = Observation {
        read_generation: c.generation(),
        leaves: main.clone(),
        ..Default::default()
    };
    let l = c.claim("y", &s("root/a/y"), 1, None).unwrap();
    let cs = c.commit("y", &l, &one("root/a/y", "C@1"), None, 2).unwrap();
    assert!(matches!(
        c.observe_main(&stale),
        Err(Error::ObservationRaced(_))
    ));
    assert_eq!(c.changeset(cs).unwrap().state, Settlement::Sequenced);
    let fresh = Observation {
        read_generation: c.generation(),
        leaves: main,
        ..Default::default()
    };
    let report = c.observe_main(&fresh).unwrap();
    assert_eq!(report.conflicted, vec![cs], "main really overrode it");
    assert_eq!(c.snapshot(&s("root/a/y")), "E@1");
}

/// Finding 2: a restarted agent's provisional lease can't replace another.
#[test]
fn restarted_agent_leases_stay_distinct() {
    let mut c = Coordinator::new(100);
    c.register(s("root/a"), "A@0").unwrap();
    c.register(s("root/b"), "B@0").unwrap();
    let mut x1 = Agent::new("x", &c);
    x1.disconnect(&c);
    let a = x1.claim_offline(&s("root/a"), 0, 100).unwrap();
    x1.reconnect();
    let (a_id, status) = x1.reconcile(&mut c, &a, 1).unwrap();
    assert_eq!(status, Status::Active);
    let mut x2 = Agent::new("x", &c);
    x2.disconnect(&c);
    let b = x2.claim_offline(&s("root/b"), 2, 100).unwrap();
    assert_ne!(a, b, "provisional ids are unique across restarts");
    x2.reconnect();
    let (b_id, _) = x2.reconcile(&mut c, &b, 3).unwrap();
    assert_ne!(a_id, b_id);
    let mut z = Agent::new("z", &c);
    assert!(matches!(
        z.claim(&mut c, &s("root/a"), 4),
        Err(Error::Held(_))
    ));
    c.commit("x", &a_id, &one("root/a", "A@1"), None, 5)
        .unwrap();
    assert_eq!(c.snapshot(&s("root/a")), "A@1");
    assert_eq!(c.snapshot(&s("root/b")), "B@0");
}

/// Finding 2: a retried reconcile returns the original outcome and never
/// rewrites history.
#[test]
fn replayed_reconcile_is_idempotent() {
    let mut c = Coordinator::new(100);
    c.register(s("root/a"), "A@0").unwrap();
    let mut x = Agent::new("x", &c);
    x.disconnect(&c);
    let id = x.claim_offline(&s("root/a"), 0, 100).unwrap();
    let copy = x.provisional().next().unwrap().clone();
    x.reconnect();
    let (confirmed, _) = x.reconcile(&mut c, &id, 1).unwrap();
    x.commit(&mut c, &confirmed, &one("root/a", "A@1"), None, 2)
        .unwrap();
    let (again, status) = c.reconcile(copy, 3).unwrap();
    assert_eq!(again, confirmed);
    assert_eq!(status, Status::Committed);
    assert_eq!(c.lease(&confirmed).unwrap().status, Status::Committed);
}

/// Finding 7: the coordinator caps a reconciled lease at its own TTL.
#[test]
fn reconcile_caps_the_deadline() {
    let mut c = Coordinator::new(10);
    c.register(s("root/a"), "A@0").unwrap();
    let mut x = Agent::new("x", &c);
    x.disconnect(&c);
    let id = x.claim_offline(&s("root/a"), 0, 1_000_000_000).unwrap();
    x.reconnect();
    let (confirmed, _) = x.reconcile(&mut c, &id, 1).unwrap();
    assert_eq!(c.lease(&confirmed).unwrap().deadline, 11);
    assert_eq!(c.expire_due(11), vec![confirmed]);
}

/// Finding 8: an idempotency key can't be reused for another scope or owner.
#[test]
fn idempotency_key_is_bound_to_owner_and_scope() {
    let mut c = Coordinator::new(100);
    c.register(s("root/a"), "A@0").unwrap();
    c.register(s("root/b"), "B@0").unwrap();
    let req = || {
        Some(Request {
            key: "k".into(),
            valid_until: 50,
        })
    };
    let la = c.claim("x", &s("root/a"), 0, req()).unwrap();
    assert_eq!(
        c.claim("x", &s("root/a"), 1, req()).unwrap(),
        la,
        "same claim replays"
    );
    assert!(matches!(
        c.claim("x", &s("root/b"), 1, req()),
        Err(Error::KeyReused(_))
    ));
    assert!(matches!(
        c.claim("z", &s("root/a"), 1, req()),
        Err(Error::KeyReused(_))
    ));
}

/// Finding 9: spellings of one path are one scope, so leases overlap.
#[test]
fn dot_segments_cannot_bypass_overlap() {
    let mut c = Coordinator::new(100);
    c.register(s("root/b"), "B@0").unwrap();
    assert!(matches!(
        c.register(s("root/a/../b"), "B@0"),
        Err(Error::AlreadyRegistered(_))
    ));
    c.claim("x", &s("root/b"), 0, None).unwrap();
    assert!(matches!(
        c.claim("y", &s("root/./b"), 0, None),
        Err(Error::Held(_))
    ));
}

/// Finding 14: time arithmetic saturates instead of overflowing.
#[test]
fn claim_near_the_end_of_time_does_not_panic() {
    let mut c = Coordinator::new(10);
    c.register(s("root/a"), "A@0").unwrap();
    let id = c.claim("x", &s("root/a"), u64::MAX - 1, None).unwrap();
    assert_eq!(c.lease(&id).unwrap().deadline, u64::MAX);
}

/// Finding 11: a provisional alias can never equal a sequential one.
#[test]
fn provisional_aliases_stay_out_of_the_sequential_namespace() {
    let offline_node = "0192f4c2-1b2a-7c3d-8e4f-000000001234";
    let online_node = "0192f4c2-1b2a-7c3d-8e4f-0000000000aa";
    let mut home = Aliases::new();
    home.record("task-1233", "0192f4c2-1b2a-7c3d-8e4f-0000000000bb")
        .unwrap();
    let mut laptop = home.clone();
    let prov = laptop.issue_provisional("task", offline_node);
    let seq = home.issue_sequential("task", online_node);
    assert_ne!(prov, seq);
    home.record(&prov, offline_node).unwrap();
}

/// Audit task-36, C4: refused claims don't grow the state without bound.
/// It keeps the newest few per owner and a bounded number in all; the
/// holder's lease is untouched.
#[test]
fn refused_claims_are_bounded() {
    let mut c = Coordinator::new(1_000);
    let held = c.claim("a", &s("x"), 0, None).unwrap();
    let denied = |c: &Coordinator, owner: &str| {
        c.leases()
            .filter(|l| l.owner == owner && l.status == Status::Conflict)
            .count()
    };
    for _ in 0..30 {
        assert!(matches!(
            c.claim("b", &s("x"), 1, None),
            Err(Error::Held(_))
        ));
    }
    assert_eq!(denied(&c, "b"), mdkg_coord::MAX_DENIED_PER_OWNER);
    for i in 0..200 {
        let owner = format!("o{i}");
        assert!(c.claim(&owner, &s("x/y"), 1, None).is_err());
    }
    let all = c.leases().filter(|l| l.status == Status::Conflict).count();
    assert_eq!(all, mdkg_coord::MAX_DENIED);
    assert_eq!(c.lease(&held).map(|l| l.status), Some(Status::Active));
}
