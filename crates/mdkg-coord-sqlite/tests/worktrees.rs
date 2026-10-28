//! Two worktrees of one repository share the coordinator through the git
//! common dir: claims conflict across worktrees, leases expire, a follow-up
//! builds on unmerged work by stacking, and merges to main settle versions in
//! order. A direct edit on main wins over an open lease.

#[path = "common/temp.rs"]
mod temp;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use mdkg_coord::{
    Changes, ChangesetId, Command, Error, LeaseId, ObserveReport, Outcome, Scope, Status,
};
use mdkg_coord_sqlite::{Store, git};

const TTL: u64 = 1_000;
const T1: &str = ".mdkg/work/task-1.md";
const T2: &str = ".mdkg/work/task-2.md";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = temp::dir(&format!("mdkg-{name}"));
    Scratch(fs::canonicalize(dir).expect("canonical scratch dir"))
}

/// Runs git for test setup, isolated from the user's configuration (no
/// signing, hooks or templates).
fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, path: &str, text: &str) {
    fs::write(dir.join(path), text).expect("write node");
}

fn node(id: &str, body: &str) -> String {
    format!("---\nid: {id}\ntype: task\n---\n\n# Overview\n\n{body}\n")
}

fn claim(owner: &str, path: &str, now: u64) -> Command {
    Command::Claim {
        owner: owner.into(),
        scope: Scope::new(path),
        mode: mdkg_coord::Mode::Exclusive,
        now,
        request: None,
        node: None,
    }
}

/// Commits a lease with the worktree's current content of `path`.
fn commit(owner: &str, lease: &LeaseId, worktree: &Path, path: &str, now: u64) -> Command {
    let content = git::file_digest(worktree, path).expect("digest");
    Command::Commit {
        owner: owner.into(),
        lease: lease.clone(),
        changes: Changes::from([(Scope::new(path), content)]),
        source: Some(git::head(worktree).expect("head")),
        now,
        landed: false,
    }
}

fn lease(outcome: Outcome) -> LeaseId {
    match outcome {
        Outcome::Lease { lease } => lease,
        other => panic!("expected a lease, got {other:?}"),
    }
}

fn changeset(outcome: Outcome) -> ChangesetId {
    match outcome {
        Outcome::Committed { changeset } => changeset,
        other => panic!("expected a commit, got {other:?}"),
    }
}

fn observed(outcome: Outcome) -> ObserveReport {
    match outcome {
        Outcome::Observed { report } => report,
        other => panic!("expected an observation, got {other:?}"),
    }
}

#[test]
fn two_worktrees_coordinate_and_settle_through_main() {
    let dir = scratch("worktrees");
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join(".mdkg/work")).expect("graph dir");
    sh(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, T1, &node("task-1", "v0"));
    write(&repo, T2, &node("task-2", "v0"));
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    let wb = dir.0.join("wt-b");
    for (branch, wt) in [("a", &wa), ("b", &wb)] {
        let wt = wt.to_str().expect("utf-8 path");
        sh(&repo, &["worktree", "add", "-q", "-b", branch, wt, "main"]);
    }

    // Every worktree resolves to the same store, outside any worktree.
    let store = git::store_path(&repo).expect("store path");
    assert_eq!(git::store_path(&wa).expect("a"), store);
    assert_eq!(git::store_path(&wb).expect("b"), store);
    let mut sa = Store::open_for_repo(&wa, TTL).expect("store a");
    let mut sb = Store::open_for_repo(&wb, TTL).expect("store b");

    // The coordinator learns what main holds.
    let r = observed(sa.observe_main(&repo, "main", ".mdkg").expect("observe"));
    assert_eq!(r.external, vec![Scope::new(T1), Scope::new(T2)]);

    // A claims task-1; B, in the other worktree, is held off.
    let la = lease(sa.execute(&claim("a", T1, 0)).expect("claim"));
    let held = sb.execute(&claim("b", T1, 1)).expect("claim");
    assert!(matches!(held.error(), Some(Error::Held(_))));

    // A edits on branch a and commits the lease.
    write(&wa, T1, &node("task-1", "v1 by a"));
    sh(&wa, &["commit", "-q", "-am", "a: task one"]);
    let cs_a = changeset(sa.execute(&commit("a", &la, &wa, T1, 2)).expect("commit"));

    // B claims task-1 from A's unmerged version and stacks on it.
    let lb = lease(sb.execute(&claim("b", T1, 3)).expect("claim"));
    let want = sb.coordinator().expect("state").expected(&Scope::new(T1));
    let want = want.first().expect("expected leaf").clone();
    let source = want.source.clone().expect("unmerged version has a source");
    assert_eq!(source.branch, "a");
    assert_ne!(git::file_digest(&wb, T1).expect("digest"), want.content);
    git::stack_on(&wb, &source.commit).expect("stack on a");
    assert_eq!(git::file_digest(&wb, T1).expect("digest"), want.content);
    write(&wb, T1, &node("task-1", "v1 by a\n\nand v2 by b"));
    sh(&wb, &["commit", "-q", "-am", "b: task one, on a's work"]);
    let cs_b = changeset(sb.execute(&commit("b", &lb, &wb, T1, 4)).expect("commit"));
    let state = sb.coordinator().expect("state");
    assert_eq!(state.changeset(cs_b).expect("cs").depends_on, [cs_a].into());
    let queue: Vec<ChangesetId> = state.settlement_queue().iter().map(|c| c.id).collect();
    assert_eq!(queue, vec![cs_a, cs_b], "merge order");

    // A claim left behind expires, and the scope frees up.
    let stale = lease(sa.execute(&claim("a", T2, 5)).expect("claim"));
    assert!(
        sb.execute(&claim("b", T2, 6))
            .expect("claim")
            .error()
            .is_some()
    );
    sb.execute(&Command::ExpireDue { now: 5 + TTL })
        .expect("expire");
    let state = sb.coordinator().expect("state");
    assert_eq!(state.lease(&stale).expect("lease").status, Status::Expired);
    let lb2 = lease(sb.execute(&claim("b", T2, 6 + TTL)).expect("claim"));

    // Merging a settles A's version; merging b then settles B's, cleanly.
    sh(&repo, &["merge", "-q", "--no-ff", "-m", "merge a", "a"]);
    let r = observed(sa.observe_main(&repo, "main", ".mdkg").expect("observe"));
    assert_eq!(r.committed, vec![cs_a]);
    sh(&repo, &["merge", "-q", "--no-ff", "-m", "merge b", "b"]);
    let r = observed(sb.observe_main(&repo, "main", ".mdkg").expect("observe"));
    assert_eq!(r.committed, vec![cs_b]);
    assert!(
        sa.coordinator()
            .expect("state")
            .settlement_queue()
            .is_empty()
    );

    // A direct edit on main beats B's open lease on task-2.
    write(&repo, T2, &node("task-2", "edited on main"));
    sh(&repo, &["commit", "-q", "-am", "edit on main"]);
    let r = observed(sa.observe_main(&repo, "main", ".mdkg").expect("observe"));
    assert_eq!(r.external, vec![Scope::new(T2)]);
    write(&wb, T2, &node("task-2", "v1 by b"));
    sh(&wb, &["commit", "-q", "-am", "b: task two"]);
    let refused = sb
        .execute(&commit("b", &lb2, &wb, T2, 7 + TTL))
        .expect("commit");
    assert_eq!(refused.error(), Some(&Error::CasFailed));

    // Both worktrees see one database, and it replays exactly.
    assert_eq!(sa.coordinator().expect("a"), sb.coordinator().expect("b"));
    sa.verify().expect("journal replays to the stored state");
}

#[test]
fn reading_another_branchs_version_leaves_history_alone() {
    let dir = scratch("checkout-file");
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join(".mdkg/work")).expect("graph dir");
    sh(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, T1, &node("task-1", "v0"));
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    sh(&repo, &["switch", "-q", "-c", "other"]);
    write(&repo, T1, &node("task-1", "v1"));
    sh(&repo, &["commit", "-q", "-am", "v1"]);
    let v1 = git::head(&repo).expect("head").commit;
    let v1_digest = git::file_digest(&repo, T1).expect("digest");
    sh(&repo, &["switch", "-q", "main"]);
    git::checkout_file(&repo, &v1, T1).expect("checkout file");
    assert_eq!(git::file_digest(&repo, T1).expect("digest"), v1_digest);
    assert_eq!(git::head(&repo).expect("head").branch, "main");
    let leaves = git::leaves(&repo, "main", ".mdkg").expect("leaves");
    assert_ne!(
        leaves.get(&Scope::new(T1)).cloned(),
        v1_digest,
        "main is untouched"
    );
}

#[test]
fn losing_the_store_loses_only_in_flight_state() {
    let dir = scratch("lost-store");
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join(".mdkg/work")).expect("graph dir");
    sh(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, T1, &node("task-1", "v0"));
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let mut store = Store::open_for_repo(&repo, TTL).expect("store");
    store.observe_main(&repo, "main", ".mdkg").expect("observe");
    lease(store.execute(&claim("a", T1, 0)).expect("claim"));
    drop(store);

    let path = git::store_path(&repo).expect("store path");
    for suffix in ["", "-wal", "-shm"] {
        fs::remove_file(format!("{}{suffix}", path.display())).ok();
    }
    let mut store = Store::open_for_repo(&repo, TTL).expect("fresh store");
    store.observe_main(&repo, "main", ".mdkg").expect("observe");
    let state = store.coordinator().expect("state");
    assert_eq!(
        Some(state.snapshot(&Scope::new(T1))),
        git::file_digest(&repo, T1).expect("digest"),
        "knowledge comes back from main"
    );
    assert_eq!(state.leases().count(), 0, "in-flight leases are gone");
    lease(store.execute(&claim("b", T1, 1)).expect("claim again"));
}
