//! Lease modes (spike-1): shared leases may overlap each other, never an
//! exclusive one, and compare-and-swap still guards every commit.

use std::collections::BTreeMap;

use mdkg_coord::{Changes, Command, Coordinator, Error, Mode, Request, Scope, Status};

fn s(x: &str) -> Scope {
    Scope::new(x)
}

fn one(leaf: &str, content: &str) -> Changes {
    BTreeMap::from([(s(leaf), Some(content.to_string()))])
}

#[test]
fn shared_leases_overlap_each_other_but_never_an_exclusive_one() {
    let mut c = Coordinator::new(10);
    let a = c
        .claim_mode("a", &s("go.sum"), Mode::Shared, 0, None)
        .unwrap();
    let b = c
        .claim_mode("b", &s("go.sum"), Mode::Shared, 1, None)
        .unwrap();
    assert_eq!(c.holders(&s("go.sum")).count(), 2);
    assert!(matches!(
        c.claim_mode("x", &s("go.sum"), Mode::Exclusive, 2, None),
        Err(Error::Held(_))
    ));
    // A folder overlaps the file inside it, whatever the spelling.
    assert!(matches!(
        c.claim("x", &s("."), 2, None),
        Err(Error::Held(_))
    ));
    c.release("a", &a, 3).unwrap();
    c.release("b", &b, 3).unwrap();

    c.claim("x", &s("src"), 4, None).unwrap();
    assert!(matches!(
        c.claim_mode("y", &s("src/lib.rs"), Mode::Shared, 5, None),
        Err(Error::Held(_))
    ));
}

#[test]
fn blockers_name_only_the_leases_a_claim_cant_sit_beside() {
    let mut c = Coordinator::new(10);
    c.claim_mode("a", &s("src"), Mode::Shared, 0, None).unwrap();
    assert_eq!(c.blockers(&s("src/lib.rs"), Mode::Shared).count(), 0);
    assert_eq!(c.blockers(&s("src/lib.rs"), Mode::Exclusive).count(), 1);
}

#[test]
fn shared_holders_still_commit_by_compare_and_swap() {
    let mut c = Coordinator::new(10);
    c.register(s("w/x"), "v0").unwrap();
    let a = c.claim_mode("a", &s("w/x"), Mode::Shared, 0, None).unwrap();
    let b = c.claim_mode("b", &s("w/x"), Mode::Shared, 0, None).unwrap();
    c.commit("a", &a, &one("w/x", "v1"), None, 1).unwrap();
    assert_eq!(
        c.commit("b", &b, &one("w/x", "v2"), None, 2),
        Err(Error::CasFailed),
        "b's base is stale, so it can't overwrite a's change"
    );
    assert_eq!(c.lease(&b).map(|l| l.status), Some(Status::Conflict));
}

#[test]
fn an_idempotency_key_is_bound_to_its_mode() {
    let mut c = Coordinator::new(10);
    let req = || {
        Some(Request {
            key: "k".into(),
            valid_until: 10,
        })
    };
    c.claim_mode("a", &s("f"), Mode::Shared, 0, req()).unwrap();
    assert_eq!(
        c.claim_mode("a", &s("f"), Mode::Exclusive, 1, req()),
        Err(Error::KeyReused("k".into()))
    );
}

#[test]
fn journals_and_states_from_before_modes_read_as_exclusive() {
    let old = r#"{"op":"claim","owner":"a","scope":"f","now":0,"request":null}"#;
    let command: Command = serde_json::from_str(old).unwrap();
    let Command::Claim { mode, .. } = &command else {
        panic!("not a claim: {command:?}");
    };
    assert_eq!(*mode, Mode::Exclusive);

    let mut c = Coordinator::new(10);
    let id = c.claim("a", &s("f"), 0, None).unwrap();
    let mut state = serde_json::to_value(&c).unwrap();
    let lease = &mut state["leases"][id.0.as_str()];
    lease.as_object_mut().unwrap().remove("mode");
    let back: Coordinator = serde_json::from_value(state).unwrap();
    assert_eq!(back.lease(&id).map(|l| l.mode), Some(Mode::Exclusive));
}
