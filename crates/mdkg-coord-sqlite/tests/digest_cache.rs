//! Reading main remembers blob hashes by object id (`blob-digests.sqlite`
//! beside the store), so only changed blobs are hashed again. The leaves
//! are the same with or without the cache, and a cache that is damaged,
//! locked, unwritable or caught mid-write only costs a rehash.

#[path = "common/temp.rs"]
mod temp;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use mdkg_coord_sqlite::{Store, git};
use mdkg_format::Digest;
use rusqlite::Connection;

const T1: &str = ".mdkg/work/task-1.md";
const T2: &str = ".mdkg/work/task-2.md";
const T3: &str = ".mdkg/bundles/sub.zip";
const FAKE: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

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

/// Runs git for test setup, isolated from the user's configuration.
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

/// A repository on `main` with two nodes and a binary bundle, and the
/// cache's path in its git common dir.
fn repo(dir: &Scratch) -> (PathBuf, PathBuf) {
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join(".mdkg/work")).expect("graph dir");
    fs::create_dir_all(repo.join(".mdkg/bundles")).expect("bundle dir");
    sh(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join(T1), "---\nid: task-1\n---\n\none\n").expect("write");
    fs::write(repo.join(T2), "---\nid: task-2\n---\n\ntwo\n").expect("write");
    fs::write(
        repo.join(T3),
        (0..=255u8).cycle().take(70_000).collect::<Vec<_>>(),
    )
    .expect("write");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let mdkg = repo.join(".git/mdkg");
    fs::create_dir_all(&mdkg).expect("mdkg dir");
    (repo, mdkg.join("blob-digests.sqlite"))
}

fn oid(repo: &Path, path: &str) -> String {
    sh(repo, &["rev-parse", &format!("main:{path}")])
}

fn rows(cache: &Path) -> Vec<(String, i64, String)> {
    let c = Connection::open(cache).expect("open cache");
    let mut stmt = c
        .prepare("SELECT oid, size, digest FROM digests ORDER BY oid")
        .expect("select");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// Plants `digest` for `oid` with a checksum that matches, as a deliberate
/// writer would (the cache's checksum is the `sha256:` of the fields).
fn plant(cache: &Path, oid: &str, digest: &str) {
    let c = Connection::open(cache).expect("open cache");
    let size: i64 = c
        .query_row("SELECT size FROM digests WHERE oid = ?1", [oid], |r| {
            r.get(0)
        })
        .expect("row");
    let sum = Digest::of_bytes(format!("{oid}\n{size}\n{digest}\n").as_bytes()).to_string();
    c.execute(
        "UPDATE digests SET digest = ?2, checksum = ?3 WHERE oid = ?1",
        [oid, digest, sum.as_str()],
    )
    .expect("plant");
}

fn set(cache: &Path, sql: &str, oid: &str) {
    let c = Connection::open(cache).expect("open cache");
    assert_eq!(c.execute(sql, [oid]).expect("update"), 1, "{sql}");
}

#[test]
fn the_cache_gives_the_same_leaves_and_hashes_only_new_blobs() {
    let dir = scratch("digests-same");
    let (repo, cache) = repo(&dir);
    let fresh = git::leaves(&repo, "main", ".mdkg").expect("leaves");
    assert!(!cache.exists(), "plain leaves() never makes a cache");

    let first = git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("first");
    assert_eq!(first, fresh);
    assert_eq!(rows(&cache).len(), 3, "one row per blob");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("second"),
        fresh
    );

    // A hit is used as it stands: a planted hash (right size, valid form,
    // matching checksum) comes back, so the second read didn't hash that
    // blob. Trusting it is the store's own trust (see digest_cache.rs): a
    // writer of this folder can write the store too.
    plant(&cache, &oid(&repo, T2), FAKE);
    let planted = git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("planted");
    assert_eq!(
        planted.get(&mdkg_coord::Scope::new(T2)).map(String::as_str),
        Some(FAKE)
    );
    assert_eq!(
        planted.get(&mdkg_coord::Scope::new(T1)),
        fresh.get(&mdkg_coord::Scope::new(T1))
    );

    // Main moves: the changed blob is hashed, the old one's row is pruned.
    let old = oid(&repo, T1);
    fs::write(repo.join(T1), "---\nid: task-1\n---\n\nedited\n").expect("write");
    fs::remove_file(repo.join(T2)).expect("remove");
    sh(&repo, &["commit", "-q", "-am", "edit"]);
    let moved = git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("moved");
    assert_eq!(moved, git::leaves(&repo, "main", ".mdkg").expect("fresh"));
    let held: Vec<String> = rows(&cache).into_iter().map(|(o, _, _)| o).collect();
    assert_eq!(held.len(), 2, "{held:?}");
    assert!(!held.contains(&old), "the replaced blob's row is gone");
    assert!(held.contains(&oid(&repo, T1)));
}

#[test]
fn a_row_that_does_not_check_out_is_hashed_again() {
    let dir = scratch("digests-damaged");
    let (repo, cache) = repo(&dir);
    let fresh = git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("leaves");
    let t1 = oid(&repo, T1);
    let t3 = oid(&repo, T3);
    // A wrong size with a planted hash, and a hash that isn't one.
    set(
        &cache,
        &format!("UPDATE digests SET size = size + 1, digest = '{FAKE}' WHERE oid = ?1"),
        &t1,
    );
    set(
        &cache,
        "UPDATE digests SET digest = 'sha256:nope' WHERE oid = ?1",
        &t3,
    );
    // One flipped character: still a valid hash of the right size, caught
    // by the row's checksum.
    let t2 = oid(&repo, T2);
    set(
        &cache,
        "UPDATE digests SET digest = substr(digest, 1, 7) || \
         (CASE substr(digest, 8, 1) WHEN '0' THEN '1' ELSE '0' END) || substr(digest, 9) \
         WHERE oid = ?1",
        &t2,
    );
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("again"),
        fresh
    );
    // And the rows are put right.
    let held = rows(&cache);
    let fixed = |o: &str| held.iter().find(|r| r.0 == o).map(|r| r.2.clone());
    assert_ne!(fixed(&t1).as_deref(), Some(FAKE));
    assert_ne!(fixed(&t3).as_deref(), Some("sha256:nope"));
    assert_eq!(
        fixed(&t2).as_deref(),
        fresh.get(&mdkg_coord::Scope::new(T2)).map(String::as_str)
    );
}

#[test]
fn a_cache_that_is_not_a_database_is_ignored_and_left_alone() {
    let dir = scratch("digests-corrupt");
    let (repo, cache) = repo(&dir);
    let garbage = b"not a database, just some bytes that fill a page or so".repeat(200);
    fs::write(&cache, &garbage).expect("garbage");
    let fresh = git::leaves(&repo, "main", ".mdkg").expect("leaves");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("leaves"),
        fresh
    );
    assert_eq!(fs::read(&cache).expect("read"), garbage);

    // A folder where the file should be, and a cache from a newer layout.
    let folder = dir.0.join("folder.sqlite");
    fs::create_dir_all(&folder).expect("folder");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&folder)).expect("leaves"),
        fresh
    );
    let newer = dir.0.join("newer.sqlite");
    Connection::open(&newer)
        .expect("newer")
        .execute_batch("PRAGMA user_version = 3;")
        .expect("version");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&newer)).expect("leaves"),
        fresh
    );
    let c = Connection::open(&newer).expect("newer");
    let tables: i64 = c
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .expect("tables");
    assert_eq!(tables, 0, "a newer layout is never written");

    // An older layout (1, without checksums) is rebuilt and then used.
    let older = dir.0.join("older.sqlite");
    Connection::open(&older)
        .expect("older")
        .execute_batch(
            "CREATE TABLE digests (oid TEXT PRIMARY KEY, size INTEGER NOT NULL, \
             digest TEXT NOT NULL) WITHOUT ROWID;
             INSERT INTO digests VALUES ('x', 1, 'sha256:bad');
             PRAGMA user_version = 1;",
        )
        .expect("older layout");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&older)).expect("leaves"),
        fresh
    );
    assert_eq!(rows(&older).len(), 3, "rebuilt with this listing's rows");
}

#[test]
fn a_failed_cache_write_keeps_the_old_rows_and_the_right_leaves() {
    let dir = scratch("digests-failed-write");
    let (repo, cache) = repo(&dir);
    git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("fill");
    let before = rows(&cache);

    fs::write(repo.join(T1), "---\nid: task-1\n---\n\nchanged\n").expect("write");
    sh(&repo, &["commit", "-q", "-am", "change"]);
    let fresh = git::leaves(&repo, "main", ".mdkg").expect("fresh");

    // Another connection holds the write lock: reads still work, the
    // rewrite times out and is dropped.
    let holder = Connection::open(&cache).expect("holder");
    holder.execute_batch("BEGIN IMMEDIATE;").expect("lock");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("locked"),
        fresh
    );
    holder.execute_batch("ROLLBACK;").expect("unlock");
    drop(holder);
    assert_eq!(rows(&cache), before, "the failed write changed nothing");

    // A read-only cache: same answer, nothing written.
    let mut perms = fs::metadata(&cache).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
    fs::set_permissions(&cache, perms).expect("chmod");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("read only"),
        fresh
    );
    assert_eq!(rows(&cache), before);

    // Writable again: the next read brings the rows up to date.
    let mut perms = fs::metadata(&cache).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
    fs::set_permissions(&cache, perms).expect("chmod");
    git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("writable");
    assert!(rows(&cache).iter().any(|r| r.0 == oid(&repo, T1)));
}

#[test]
fn a_crash_mid_write_leaves_the_old_rows() {
    let dir = scratch("digests-crash");
    let (repo, cache) = repo(&dir);
    git::leaves_cached(&repo, "main", ".mdkg", Some(&cache)).expect("fill");
    let before = rows(&cache);

    // The files as a crash would leave them: a rewrite half done, with
    // its journal, copied while the transaction is still open.
    let writer = Connection::open(&cache).expect("writer");
    writer
        .execute_batch(
            "PRAGMA cache_size = 1;
             BEGIN IMMEDIATE;
             DELETE FROM digests;
             INSERT INTO digests VALUES ('x', 1, 'sha256:bad', 'sha256:bad');",
        )
        .expect("half a write");
    let crashed = dir.0.join("crashed.sqlite");
    fs::copy(&cache, &crashed).expect("copy db");
    let journal = PathBuf::from(format!("{}-journal", cache.display()));
    assert!(journal.exists(), "the rewrite has a rollback journal");
    fs::copy(&journal, format!("{}-journal", crashed.display())).expect("copy journal");
    writer.execute_batch("ROLLBACK;").expect("rollback");

    let fresh = git::leaves(&repo, "main", ".mdkg").expect("fresh");
    assert_eq!(
        git::leaves_cached(&repo, "main", ".mdkg", Some(&crashed)).expect("after crash"),
        fresh
    );
    assert_eq!(
        rows(&crashed),
        before,
        "the journal rolled the half write back"
    );
}

#[test]
fn observing_main_keeps_the_cache_beside_the_store() {
    let dir = scratch("digests-store");
    let (repo, cache) = repo(&dir);
    let mut store = Store::open_for_repo(&repo, 1_000).expect("store");
    store.observe_main(&repo, "main", ".mdkg").expect("observe");
    assert_eq!(rows(&cache).len(), 3);
    let fresh = git::leaves(&repo, "main", ".mdkg").expect("fresh");
    assert_eq!(store.coordinator().expect("state").leaves(), fresh);
    // Observed again from the cache: the same state, nothing to apply.
    store
        .observe_main(&repo, "main", ".mdkg")
        .expect("observe again");
    assert_eq!(store.coordinator().expect("state").leaves(), fresh);
}
