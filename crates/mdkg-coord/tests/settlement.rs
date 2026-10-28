//! Settlement through git (edd-1, dec-2): versions are sequenced by lease
//! commits and settled by what main holds. Main always wins.

use std::collections::BTreeMap;

use mdkg_coord::{
    Changes, Command, Coordinator, EntryState, Error, Observation, Outcome, Scope, Settlement,
    Source, Status,
};

fn s(x: &str) -> Scope {
    Scope::new(x)
}

fn change(pairs: &[(&str, Option<&str>)]) -> Changes {
    pairs
        .iter()
        .map(|(l, c)| (s(l), c.map(String::from)))
        .collect()
}

fn src(branch: &str) -> Option<Source> {
    Some(Source {
        commit: format!("{branch}-sha"),
        branch: branch.to_string(),
    })
}

fn main(c: &Coordinator, pairs: &[(&str, &str)]) -> Observation {
    Observation {
        read_generation: c.generation(),
        leaves: pairs.iter().map(|(l, h)| (s(l), h.to_string())).collect(),
        ..Default::default()
    }
}

fn state(c: &Coordinator, id: mdkg_coord::ChangesetId) -> Settlement {
    c.changeset(id).expect("changeset").state
}

/// A commits x1 on branch a; B builds x2 on top, stacked on branch b.
fn stacked() -> (
    Coordinator,
    mdkg_coord::ChangesetId,
    mdkg_coord::ChangesetId,
) {
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").expect("stacked setup");
    let la = c.claim("a", &s("w/x"), 0, None).expect("stacked setup");
    let a = c
        .commit("a", &la, &change(&[("w/x", Some("x1"))]), src("a"), 1)
        .expect("stacked setup");
    let lb = c.claim("b", &s("w/x"), 2, None).expect("stacked setup");
    assert_eq!(
        c.lease(&lb).expect("stacked setup").base,
        "x1",
        "B starts from A's version"
    );
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x2"))]), src("b"), 3)
        .expect("stacked setup");
    (c, a, b)
}

#[test]
fn stacked_work_settles_in_merge_order() {
    let (mut c, a, b) = stacked();
    assert_eq!(c.changeset(b).unwrap().depends_on, [a].into());
    let queue: Vec<_> = c.settlement_queue().iter().map(|cs| cs.id).collect();
    assert_eq!(queue, vec![a, b]);
    let want = &c.expected(&s("w"))[0];
    assert_eq!(want.content.as_deref(), Some("x2"));
    assert_eq!(want.source.as_ref().unwrap().branch, "b");

    // Main hasn't moved: nothing settles, nothing conflicts.
    let r = c.observe_main(&main(&c, &[("w/x", "x0")])).unwrap();
    assert!(r.committed.is_empty() && r.conflicted.is_empty() && r.external.is_empty());

    let r = c.observe_main(&main(&c, &[("w/x", "x1")])).unwrap();
    assert_eq!(r.committed, vec![a]);
    assert_eq!(state(&c, b), Settlement::Sequenced);

    let r = c.observe_main(&main(&c, &[("w/x", "x2")])).unwrap();
    assert_eq!(r.committed, vec![b]);
    assert!(c.settlement_queue().is_empty());
}

#[test]
fn merging_the_later_branch_settles_both() {
    let (mut c, a, b) = stacked();
    let r = c.observe_main(&main(&c, &[("w/x", "x2")])).unwrap();
    assert_eq!(r.committed, vec![a, b]);
}

#[test]
fn abandon_cascades_and_rolls_back() {
    let (mut c, a, b) = stacked();
    let lz = c.claim("z", &s("w/x"), 4, None).unwrap();
    assert_eq!(c.abandon(a).unwrap(), vec![a, b]);
    assert_eq!(c.snapshot(&s("w/x")), "x0");
    assert_eq!(state(&c, b), Settlement::Abandoned);
    // Work based on abandoned content can't commit.
    assert_eq!(
        c.commit("z", &lz, &change(&[("w/x", Some("x3"))]), None, 5),
        Err(Error::CasFailed)
    );
    assert!(matches!(c.abandon(a), Err(Error::AlreadySettled(_))));
}

#[test]
fn external_edit_on_main_conflicts_pending_work() {
    let (mut c, a, b) = stacked();
    let r = c.observe_main(&main(&c, &[("w/x", "edited")])).unwrap();
    assert_eq!(r.external, vec![s("w/x")]);
    assert_eq!(r.conflicted, vec![a, b]);
    assert_eq!(c.snapshot(&s("w/x")), "edited", "main wins");
    let l = c.claim("z", &s("w/x"), 4, None).unwrap();
    assert_eq!(c.lease(&l).unwrap().base, "edited");
    // Dropping the conflicted work leaves main's content in place.
    assert_eq!(c.abandon(a).unwrap(), vec![a, b]);
    assert_eq!(c.snapshot(&s("w/x")), "edited");
}

#[test]
fn conflicted_work_that_reaches_main_settles() {
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    let l = c.claim("a", &s("w/x"), 0, None).unwrap();
    let a = c
        .commit("a", &l, &change(&[("w/x", Some("x1"))]), src("a"), 1)
        .unwrap();
    // A slow observer saw an external edit; by the time it applies, main
    // has merged branch a instead.
    let slow = main(&c, &[("w/x", "edited")]);
    c.observe_main(&slow).unwrap();
    assert_eq!(state(&c, a), Settlement::Conflicted);
    let r = c.observe_main(&main(&c, &[("w/x", "x1")])).unwrap();
    assert_eq!(r.committed, vec![a]);
    assert_eq!(c.snapshot(&s("w/x")), "x1");
}

#[test]
fn a_revert_on_main_is_an_external_edit() {
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    let l = c.claim("a", &s("w/x"), 0, None).unwrap();
    let a = c
        .commit("a", &l, &change(&[("w/x", Some("x1"))]), src("a"), 1)
        .unwrap();
    c.observe_main(&main(&c, &[("w/x", "x1")])).unwrap();
    assert_eq!(state(&c, a), Settlement::Committed);
    let r = c.observe_main(&main(&c, &[("w/x", "x0")])).unwrap();
    assert_eq!(r.external, vec![s("w/x")]);
    assert_eq!(c.snapshot(&s("w/x")), "x0");
    assert_eq!(state(&c, a), Settlement::Committed, "settled stays settled");
}

#[test]
fn created_and_deleted_leaves_settle_by_presence() {
    let mut c = Coordinator::new(100);
    c.register(s("w/old"), "o0").unwrap();
    let l = c.claim("a", &s("w"), 0, None).unwrap();
    let a = c
        .commit(
            "a",
            &l,
            &change(&[("w/new", Some("n1")), ("w/old", None)]),
            src("a"),
            1,
        )
        .unwrap();
    assert_eq!(
        c.leaves().keys().cloned().collect::<Vec<_>>(),
        vec![s("w/new")]
    );
    // Main still has the old file and lacks the new one: it lags.
    let r = c.observe_main(&main(&c, &[("w/old", "o0")])).unwrap();
    assert!(r.external.is_empty());
    assert_eq!(state(&c, a), Settlement::Sequenced);
    let r = c.observe_main(&main(&c, &[("w/new", "n1")])).unwrap();
    assert_eq!(r.committed, vec![a]);
}

#[test]
fn new_files_on_main_become_leaves() {
    let mut c = Coordinator::new(100);
    let l = c.claim("a", &s("w"), 0, None).unwrap();
    let base = c.lease(&l).unwrap().base.clone();
    let r = c.observe_main(&main(&c, &[("w/x", "x0")])).unwrap();
    assert_eq!(r.external, vec![s("w/x")]);
    assert_eq!(c.snapshot(&s("w/x")), "x0");
    assert_ne!(c.snapshot(&s("w")), base);
    assert_eq!(
        c.commit("a", &l, &change(&[("w/y", Some("y1"))]), None, 1),
        Err(Error::CasFailed),
        "the directory changed under the lease"
    );
}

#[test]
fn commit_outside_the_scope_is_refused_and_changes_nothing() {
    let mut c = Coordinator::new(100);
    c.register(s("w/a"), "a0").unwrap();
    c.register(s("w/b"), "b0").unwrap();
    let l = c.claim("x", &s("w/a"), 0, None).unwrap();
    assert!(matches!(
        c.commit("x", &l, &change(&[("w/b", Some("b1"))]), None, 1),
        Err(Error::OutOfScope(_))
    ));
    assert_eq!(c.lease(&l).unwrap().status, Status::Active);
    c.commit("x", &l, &change(&[("w/a", Some("a1"))]), None, 2)
        .unwrap();
    assert_eq!(c.snapshot(&s("w/b")), "b0");
}

#[test]
fn a_child_commit_changes_the_parent_snapshot() {
    let mut c = Coordinator::new(100);
    c.register(s("w/a"), "a0").unwrap();
    c.register(s("w/b"), "b0").unwrap();
    let before = c.snapshot(&s("w"));
    let l = c.claim("x", &s("w/a"), 0, None).unwrap();
    c.commit("x", &l, &change(&[("w/a", Some("a1"))]), None, 1)
        .unwrap();
    assert_ne!(c.snapshot(&s("w")), before);
    assert_eq!(c.snapshot(&s("w/b")), "b0", "siblings are unaffected");
}

#[test]
fn an_empty_commit_is_settled_at_once() {
    let mut c = Coordinator::new(100);
    c.register(s("w/a"), "a0").unwrap();
    let l = c.claim("x", &s("w/a"), 0, None).unwrap();
    let cs = c.commit("x", &l, &Changes::new(), None, 1).unwrap();
    assert_eq!(state(&c, cs), Settlement::Committed);
    assert!(c.settlement_queue().is_empty());
}

#[test]
fn leaf_history_keeps_every_version() {
    let (c, _, _) = stacked();
    let states: Vec<EntryState> = c
        .leaf(&s("w/x"))
        .unwrap()
        .entries
        .iter()
        .map(|e| e.state)
        .collect();
    assert_eq!(
        states,
        vec![
            EntryState::Committed,
            EntryState::Pending,
            EntryState::Pending
        ]
    );
}

#[test]
fn replaying_commands_rebuilds_identical_state() {
    let commands = vec![
        Command::Register {
            leaf: s("w/x"),
            content: "x0".into(),
        },
        Command::Claim {
            owner: "a".into(),
            scope: s("w/x"),
            mode: mdkg_coord::Mode::Exclusive,
            now: 0,
            request: None,
            node: None,
        },
        Command::Commit {
            owner: "a".into(),
            lease: mdkg_coord::LeaseId("lease-0".into()),
            changes: change(&[("w/x", Some("x1"))]),
            source: src("a"),
            now: 1,
            landed: false,
        },
        Command::Claim {
            owner: "b".into(),
            scope: s("w"),
            mode: mdkg_coord::Mode::Exclusive,
            now: 2,
            request: None,
            node: None,
        },
        Command::ObserveMain {
            observation: Observation {
                read_generation: 2,
                leaves: BTreeMap::from([(s("w/x"), "x1".to_string())]),
                ..Default::default()
            },
        },
        Command::ExpireDue { now: 500 },
    ];
    let mut first = Coordinator::new(100);
    let outcomes: Vec<Outcome> = commands.iter().map(|cmd| first.apply(cmd)).collect();
    assert!(outcomes.iter().all(|o| o.error().is_none()), "{outcomes:?}");

    let journal = serde_json::to_string(&commands).unwrap();
    let replayed_commands: Vec<Command> = serde_json::from_str(&journal).unwrap();
    let mut second = Coordinator::new(100);
    let replayed: Vec<Outcome> = replayed_commands
        .iter()
        .map(|cmd| second.apply(cmd))
        .collect();
    first.drain_events();
    second.drain_events();
    assert_eq!(outcomes, replayed);
    assert_eq!(first, second);

    let stored = serde_json::to_string(&first).unwrap();
    let restored: Coordinator = serde_json::from_str(&stored).unwrap();
    assert_eq!(restored, first);
}

fn merged(c: &Coordinator, pairs: &[(&str, &str)], commits: &[&str]) -> Observation {
    Observation {
        merged: commits.iter().map(|x| x.to_string()).collect(),
        ..main(c, pairs)
    }
}

#[test]
fn a_change_undone_on_another_branch_settles_nothing_while_main_holds_still() {
    // cs-1 takes x0 to x1 on branch a; cs-2, stacked on it, takes it back to
    // x0 on branch b. Main still holds x0 and merged neither.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    let la = c.claim("a", &s("w/x"), 0, None).unwrap();
    let a = c
        .commit("a", &la, &change(&[("w/x", Some("x1"))]), src("a"), 1)
        .unwrap();
    let lb = c.claim("b", &s("w/x"), 2, None).unwrap();
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x0"))]), src("b"), 3)
        .unwrap();
    let r = c.observe_main(&main(&c, &[("w/x", "x0")])).unwrap();
    assert!(r.committed.is_empty(), "{r:?}");
    assert_eq!(
        (state(&c, a), state(&c, b)),
        (Settlement::Sequenced, Settlement::Sequenced)
    );

    // Merging b for real (its history carries a) settles both.
    let r = c
        .observe_main(&merged(&c, &[("w/x", "x0")], &["a-sha", "b-sha"]))
        .unwrap();
    assert_eq!(r.committed, vec![a, b]);
}

#[test]
fn history_settles_work_whose_content_main_changed_since() {
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    let la = c.claim("a", &s("w/x"), 0, None).unwrap();
    let a = c
        .commit("a", &la, &change(&[("w/x", Some("x1"))]), src("a"), 1)
        .unwrap();
    // Branch a merged, and main edited the file again before anyone looked.
    let r = c
        .observe_main(&merged(&c, &[("w/x", "x1-edited")], &["a-sha"]))
        .unwrap();
    assert_eq!(r.committed, vec![a], "its commit is on main");
    assert_eq!(
        r.external,
        vec![s("w/x")],
        "and main's edit is a new version"
    );
    assert_eq!(c.snapshot(&s("w/x")), "x1-edited");
}

#[test]
fn a_squash_merge_settles_by_content_once_main_changes() {
    let (mut c, a, b) = stacked();
    // Squash-merging b puts x2 on main with no history: content settles it,
    // and a, which b was built on.
    let r = c.observe_main(&main(&c, &[("w/x", "x2")])).unwrap();
    assert_eq!(r.committed, vec![a, b]);
}

#[test]
fn leaves_outside_the_observed_prefix_are_left_alone() {
    let mut c = Coordinator::new(100);
    c.register(s("src/lib.rs"), "l0").unwrap();
    c.register(s(".mdkg/work/t.md"), "t0").unwrap();
    let obs = Observation {
        prefix: Some(s(".mdkg")),
        ..main(&c, &[(".mdkg/work/t.md", "t0")])
    };
    let r = c.observe_main(&obs).unwrap();
    assert!(
        r.external.is_empty(),
        "src/lib.rs wasn't read, so it isn't deleted: {r:?}"
    );
    assert_eq!(c.snapshot(&s("src/lib.rs")), "l0");
}

#[test]
fn a_squash_merge_settles_only_the_earliest_version_with_mains_content() {
    // a: x0 to x1; b, stacked: back to x0; c, stacked: x1 again. Only a is
    // squash-merged, so main moves x0 to x1: a settles, b and c don't.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    let mut ids = Vec::new();
    for (i, (who, content)) in [("a", "x1"), ("b", "x0"), ("c", "x1")].iter().enumerate() {
        let now = 10 * i as u64;
        let l = c.claim(who, &s("w/x"), now, None).unwrap();
        ids.push(
            c.commit(
                who,
                &l,
                &change(&[("w/x", Some(content))]),
                src(who),
                now + 1,
            )
            .unwrap(),
        );
    }
    let r = c.observe_main(&main(&c, &[("w/x", "x1")])).unwrap();
    assert_eq!(r.committed, vec![ids[0]]);
    assert_eq!(state(&c, ids[1]), Settlement::Sequenced);
    assert_eq!(state(&c, ids[2]), Settlement::Sequenced);
}

#[test]
fn history_settles_leaves_outside_the_observed_prefix() {
    let mut c = Coordinator::new(100);
    c.register(s("src/lib.rs"), "l0").unwrap();
    let l = c.claim("a", &s("src/lib.rs"), 0, None).unwrap();
    let a = c
        .commit("a", &l, &change(&[("src/lib.rs", Some("l1"))]), src("a"), 1)
        .unwrap();
    let obs = Observation {
        prefix: Some(s(".mdkg")),
        ..merged(&c, &[], &["a-sha"])
    };
    let r = c.observe_main(&obs).unwrap();
    assert_eq!(r.committed, vec![a], "its commit is on main");
}

#[test]
fn a_version_merged_by_history_lands_what_it_was_built_on() {
    // b starts from a's version (stacked by content, not by merging a), and
    // only b is merged for real: a landed with it.
    let (mut c, a, b) = stacked();
    let r = c
        .observe_main(&merged(&c, &[("w/x", "x2")], &["b-sha"]))
        .unwrap();
    assert_eq!(r.committed, vec![a, b]);
}

#[test]
fn a_merged_version_lands_only_what_it_was_built_on() {
    // A deletes w/x; B, holding only v, creates w/x anew outside its scope.
    // B didn't build on A, so merging B lands B alone.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    c.register(s("v/y"), "y0").unwrap();
    let la = c.claim("a", &s("w"), 0, None).unwrap();
    let a = c
        .commit("a", &la, &change(&[("w/x", None)]), src("a"), 1)
        .unwrap();
    let lb = c.claim("b", &s("v"), 2, None).unwrap();
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x9"))]), src("b"), 3)
        .unwrap();
    let r = c
        .observe_main(&merged(&c, &[("w/x", "x9"), ("v/y", "y0")], &["b-sha"]))
        .unwrap();
    assert_eq!(r.committed, vec![b]);
    assert_eq!(state(&c, a), Settlement::Conflicted, "main moved past a");
}

#[test]
fn a_dependency_lands_only_in_the_files_its_successor_changed() {
    // A changes w/x and w/z; B builds on A's w/x only, and only B merges.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    c.register(s("w/z"), "z0").unwrap();
    let la = c.claim("a", &s("w"), 0, None).unwrap();
    let a = c
        .commit(
            "a",
            &la,
            &change(&[("w/x", Some("x1")), ("w/z", Some("z1"))]),
            src("a"),
            1,
        )
        .unwrap();
    let lb = c.claim("b", &s("w/x"), 2, None).unwrap();
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x2"))]), src("b"), 3)
        .unwrap();
    let r = c
        .observe_main(&merged(&c, &[("w/x", "x2"), ("w/z", "z0")], &["b-sha"]))
        .unwrap();
    assert_eq!(
        r.committed,
        vec![b],
        "a's w/z isn't on main, so a isn't done"
    );
    assert_eq!(state(&c, a), Settlement::Sequenced);
}

#[test]
fn a_merged_version_never_revives_what_main_overrode() {
    // A changes w/x and w/z; main overrides A's w/x; B builds w/x on main's
    // version (and depends on A through w/z). Merging B doesn't settle A's
    // overridden w/x.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    c.register(s("w/z"), "z0").unwrap();
    let la = c.claim("a", &s("w"), 0, None).unwrap();
    let a = c
        .commit(
            "a",
            &la,
            &change(&[("w/x", Some("x1")), ("w/z", Some("z1"))]),
            src("a"),
            1,
        )
        .unwrap();
    c.observe_main(&main(&c, &[("w/x", "x7"), ("w/z", "z0")]))
        .unwrap();
    assert_eq!(state(&c, a), Settlement::Conflicted);
    let lb = c.claim("b", &s("w"), 2, None).unwrap();
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x8"))]), src("b"), 3)
        .unwrap();
    c.observe_main(&merged(&c, &[("w/x", "x8"), ("w/z", "z0")], &["b-sha"]))
        .unwrap();
    assert_eq!(state(&c, b), Settlement::Committed);
    // Squash-merging A's z1 later doesn't make A committed: its w/x lost.
    c.observe_main(&main(&c, &[("w/x", "x8"), ("w/z", "z1")]))
        .unwrap();
    assert_eq!(
        state(&c, a),
        Settlement::Conflicted,
        "main overrode a's w/x"
    );
}

#[test]
fn a_squash_merge_settles_only_what_its_version_was_built_on() {
    // A deletes w/x; B creates it anew outside its scope; only B is
    // squash-merged. A's deletion isn't on main.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").unwrap();
    c.register(s("v/y"), "y0").unwrap();
    let la = c.claim("a", &s("w"), 0, None).unwrap();
    let a = c
        .commit("a", &la, &change(&[("w/x", None)]), src("a"), 1)
        .unwrap();
    let lb = c.claim("b", &s("v"), 2, None).unwrap();
    let b = c
        .commit("b", &lb, &change(&[("w/x", Some("x9"))]), src("b"), 3)
        .unwrap();
    let r = c
        .observe_main(&main(&c, &[("w/x", "x9"), ("v/y", "y0")]))
        .unwrap();
    assert_eq!(r.committed, vec![b]);
    assert_eq!(state(&c, a), Settlement::Conflicted, "main moved past a");
}

#[test]
fn a_passed_over_version_still_settles_when_it_reaches_main() {
    // A deletes w/x; B recreates it outside its scope and merges: A is
    // conflicted. A's branch then merges for real, or squash-merges.
    let setup = || {
        let mut c = Coordinator::new(100);
        c.register(s("w/x"), "x0").unwrap();
        c.register(s("v/y"), "y0").unwrap();
        let la = c.claim("a", &s("w"), 0, None).unwrap();
        let a = c
            .commit("a", &la, &change(&[("w/x", None)]), src("a"), 1)
            .unwrap();
        let lb = c.claim("b", &s("v"), 2, None).unwrap();
        c.commit("b", &lb, &change(&[("w/x", Some("x9"))]), src("b"), 3)
            .unwrap();
        c.observe_main(&merged(&c, &[("w/x", "x9"), ("v/y", "y0")], &["b-sha"]))
            .unwrap();
        assert_eq!(state(&c, a), Settlement::Conflicted);
        (c, a)
    };
    let (mut c, a) = setup();
    c.observe_main(&merged(&c, &[("v/y", "y0")], &["a-sha"]))
        .unwrap();
    assert_eq!(state(&c, a), Settlement::Committed, "merged for real");
    let (mut c, a) = setup();
    c.observe_main(&main(&c, &[("v/y", "y0")])).unwrap();
    assert_eq!(state(&c, a), Settlement::Committed, "squash-merged");
}

/// A lease on w/x whose base main moved past (x0 to x1, by an observation)
/// before the holder commits.
fn stale() -> (Coordinator, mdkg_coord::LeaseId) {
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").expect("register");
    let l = c.claim("a", &s("w/x"), 0, None).expect("claim");
    c.observe_main(&main(&c, &[("w/x", "x1")]))
        .expect("observe");
    (c, l)
}

#[test]
fn a_landed_commit_passes_a_stale_base_only_when_main_already_has_it() {
    // Main already holds exactly the holder's content: accepted, and it
    // settles by history at once.
    let (mut c, l) = stale();
    let cs = c
        .commit_landed(
            "a",
            &l,
            &change(&[("w/x", Some("x1"))]),
            src("main"),
            1,
            true,
        )
        .expect("landed");
    assert_eq!(c.lease(&l).expect("lease").status, Status::Committed);
    let mut seen = main(&c, &[("w/x", "x1")]);
    seen.merged = ["main-sha".to_string()].into();
    c.observe_main(&seen).expect("observe");
    assert_eq!(state(&c, cs), Settlement::Committed);

    // Content main doesn't have: the compare-and-swap as before.
    let (mut c, l) = stale();
    assert_eq!(
        c.commit_landed("a", &l, &change(&[("w/x", Some("x2"))]), src("b"), 1, true),
        Err(Error::CasFailed)
    );
    // No change named, or `landed` false: as before.
    let (mut c, l) = stale();
    assert_eq!(
        c.commit_landed("a", &l, &Changes::new(), src("b"), 1, true),
        Err(Error::CasFailed)
    );
    let (mut c, l) = stale();
    assert_eq!(
        c.commit("a", &l, &change(&[("w/x", Some("x1"))]), src("b"), 1),
        Err(Error::CasFailed)
    );
}

#[test]
fn a_landed_commit_must_name_the_whole_scope() {
    // A folder lease over two files; main moved one of them. Naming only
    // the moved one doesn't vouch for the other.
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").expect("register");
    c.register(s("w/y"), "y0").expect("register");
    let l = c.claim("a", &s("w"), 0, None).expect("claim");
    c.observe_main(&main(&c, &[("w/x", "x1"), ("w/y", "y0")]))
        .expect("observe");
    assert_eq!(
        c.commit_landed(
            "a",
            &l,
            &change(&[("w/x", Some("x1"))]),
            src("main"),
            1,
            true
        ),
        Err(Error::CasFailed)
    );
    let mut c = Coordinator::new(100);
    c.register(s("w/x"), "x0").expect("register");
    c.register(s("w/y"), "y0").expect("register");
    let l = c.claim("a", &s("w"), 0, None).expect("claim");
    c.observe_main(&main(&c, &[("w/x", "x1"), ("w/y", "y0")]))
        .expect("observe");
    c.commit_landed(
        "a",
        &l,
        &change(&[("w/x", Some("x1")), ("w/y", Some("y0"))]),
        src("main"),
        1,
        true,
    )
    .expect("the whole scope, as main has it");
}

#[test]
fn journal_entries_from_before_landed_replay_unchanged() {
    // A commit as earlier builds wrote it: no `landed` key.
    let old = r#"{"op":"commit","owner":"a","lease":"lease-0","changes":{"w/x":"x1"},"source":null,"now":1}"#;
    let parsed: Command = serde_json::from_str(old).expect("an old commit parses");
    match &parsed {
        Command::Commit { landed, .. } => assert!(!landed),
        other => panic!("{other:?}"),
    }
    // And a commit that isn't landed writes exactly what earlier builds
    // wrote, so records hash the same.
    assert_eq!(serde_json::to_string(&parsed).expect("json"), old);
    let landed = Command::Commit {
        owner: "a".into(),
        lease: mdkg_coord::LeaseId("lease-0".into()),
        changes: change(&[("w/x", Some("x1"))]),
        source: None,
        now: 1,
        landed: true,
    };
    let text = serde_json::to_string(&landed).expect("json");
    assert!(text.ends_with(r#""now":1,"landed":true}"#), "{text}");
}
