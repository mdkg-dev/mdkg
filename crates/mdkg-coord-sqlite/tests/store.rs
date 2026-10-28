//! The store persists, replays and serializes writers across connections.

#[path = "common/temp.rs"]
mod temp;

use std::collections::BTreeSet;
use std::path::PathBuf;

use mdkg_coord::{Command, Error, LeaseId, Outcome, Scope};
use mdkg_coord_sqlite::{Store, StoreError, Upgrade};

/// Opens a store as an installed build does: test binaries run from a
/// Cargo build folder, which refuses upgrades by default.
fn allowing(path: &std::path::Path, ttl: u64) -> Result<Store, StoreError> {
    Store::open_with(path, ttl, Upgrade::Allow)
}

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = temp::dir(&format!("mdkg-coord-sqlite-{name}"));
        Scratch(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("mdkg").join("coord.sqlite")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn register(leaf: &str) -> Command {
    Command::Register {
        leaf: Scope::new(leaf),
        content: format!("{leaf}@0"),
    }
}

fn claim(owner: &str, scope: &str, now: u64) -> Command {
    Command::Claim {
        owner: owner.into(),
        scope: Scope::new(scope),
        mode: mdkg_coord::Mode::Exclusive,
        now,
        request: None,
        node: None,
    }
}

#[test]
fn state_survives_reopening() {
    let dir = Scratch::new("reopen");
    let mut store = Store::open(&dir.db(), 100).unwrap();
    store.execute(&register("w/a")).unwrap();
    let outcome = store.execute(&claim("x", "w/a", 0)).unwrap();
    assert!(matches!(outcome, Outcome::Lease { .. }));
    let before = store.coordinator().unwrap();
    drop(store);
    let store = Store::open(&dir.db(), 5).unwrap();
    assert_eq!(
        store.ttl(),
        100,
        "the ttl is fixed when the store is created"
    );
    assert_eq!(store.coordinator().unwrap(), before);
    store.verify().unwrap();
}

#[test]
fn refusals_are_journaled_and_replay_identically() {
    let dir = Scratch::new("refusals");
    let mut store = Store::open(&dir.db(), 100).unwrap();
    store.execute(&register("w/a")).unwrap();
    store.execute(&claim("x", "w/a", 0)).unwrap();
    let held = store.execute(&claim("y", "w/a", 1)).unwrap();
    assert!(matches!(
        held,
        Outcome::Refused {
            error: Error::Held(_)
        }
    ));
    let not_owner = store
        .execute(&Command::Release {
            owner: "y".into(),
            lease: LeaseId("lease-0".into()),
            now: 2,
        })
        .unwrap();
    assert_eq!(not_owner.error(), Some(&Error::NotOwner));
    assert_eq!(store.journal().unwrap().len(), 4);
    store.verify().unwrap();
}

#[test]
fn a_lost_snapshot_is_detected_and_rebuilt_from_the_journal() {
    let dir = Scratch::new("rebuild");
    let mut store = Store::open(&dir.db(), 100).unwrap();
    for leaf in ["w/a", "w/b"] {
        store.execute(&register(leaf)).unwrap();
    }
    store.execute(&claim("x", "w", 0)).unwrap();
    let before = store.coordinator().unwrap();
    drop(store);

    rusqlite::Connection::open(dir.db())
        .unwrap()
        .execute("DELETE FROM state", [])
        .unwrap();
    let mut store = Store::open(&dir.db(), 100).unwrap();
    assert!(matches!(
        store.coordinator(),
        Err(StoreError::Inconsistent { .. })
    ));
    assert_eq!(store.rebuild().unwrap(), before);
    assert_eq!(store.coordinator().unwrap(), before);
    store.execute(&claim("y", "w/a", 1)).unwrap();
    store.verify().unwrap();
}

#[test]
fn concurrent_writers_take_turns() {
    let dir = Scratch::new("concurrent");
    let mut store = Store::open(&dir.db(), 1_000).unwrap();
    for t in 0..4 {
        for i in 0..25 {
            store.execute(&register(&format!("w/{t}/{i}"))).unwrap();
        }
    }
    let db = dir.db();
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&db, 1_000).expect("open");
                (0..25)
                    .map(|i| {
                        match store
                            .execute(&claim(&format!("agent-{t}"), &format!("w/{t}/{i}"), i))
                            .expect("execute")
                        {
                            Outcome::Lease { lease } => lease,
                            other => panic!("claim refused: {other:?}"),
                        }
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let leases: Vec<LeaseId> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("thread"))
        .collect();
    let unique: BTreeSet<_> = leases.iter().collect();
    assert_eq!(unique.len(), 100, "every claim got its own lease id");
    assert_eq!(store.journal().unwrap().len(), 200);
    store.verify().unwrap();
}

#[test]
fn reads_never_see_a_half_written_state_while_another_worktree_writes() {
    let dir = Scratch::new("reads");
    let mut writer = Store::open(&dir.db(), 1_000).expect("open");
    writer.execute(&register("w/a")).expect("register");
    let path = dir.db();
    // Opened before the writer starts: every open takes the write lock
    // briefly, and SQLite's busy wait isn't fair, so an open racing a
    // writer that never pauses can wait out the whole busy timeout under
    // load (seconds were seen). This test is about reads during writes.
    let reader = Store::open(&dir.db(), 1_000).expect("open");
    let mut w = Store::open(&path, 1_000).expect("open");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut now = 0;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            now += 1;
            w.execute(&Command::ExpireDue { now }).expect("write");
        }
    });
    for _ in 0..3_000 {
        reader.coordinator().expect("a consistent read");
    }
    reader.verify().expect("verify reads one snapshot");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handle.join().expect("writer");
}

#[test]
fn rebuild_never_drops_a_command_another_worktree_wrote() {
    let dir = Scratch::new("rebuild-race");
    let mut store = Store::open(&dir.db(), 1_000).expect("open");
    store.execute(&register("w/0")).expect("register");
    let path = dir.db();
    let handle = std::thread::spawn(move || {
        let mut w = Store::open(&path, 1_000).expect("open");
        for i in 1..200 {
            w.execute(&register(&format!("w/{i}"))).expect("register");
        }
    });
    for _ in 0..60 {
        store.rebuild().expect("rebuild");
    }
    handle.join().expect("writer");
    let replayed = store.replay().expect("replay");
    assert_eq!(
        store.coordinator().expect("state"),
        replayed,
        "rebuild lost writes"
    );
    assert_eq!(replayed.leaves().len(), 200);
}

#[test]
fn an_observation_that_changes_nothing_is_not_journaled() {
    let dir = Scratch::new("quiet");
    let mut store = Store::open(&dir.db(), 1_000).expect("open");
    store.execute(&register("w/a")).expect("register");
    let observe = Command::ObserveMain {
        observation: mdkg_coord::Observation {
            read_generation: store.coordinator().expect("state").generation(),
            leaves: [(Scope::new("w/a"), "w/a@0".to_string())].into(),
            ..Default::default()
        },
    };
    let before = store.journal().expect("journal").len();
    for _ in 0..5 {
        store.execute(&observe).expect("observe");
    }
    assert_eq!(store.journal().expect("journal").len(), before);
    store.verify().expect("state still matches the journal");
}

fn schema_of(db: &std::path::Path) -> String {
    rusqlite::Connection::open(db)
        .expect("store")
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .expect("store")
}

fn set_schema(db: &std::path::Path, version: &str) {
    rusqlite::Connection::open(db)
        .expect("store")
        .execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [version])
        .expect("store");
}

/// Leases carry a mode since schema "2" (task-14), and every write a
/// record since "3" (task-39). A "1" store upgrades in
/// place and keeps its state; any other version is refused, as an older
/// binary refuses "2".
#[test]
fn a_schema_1_store_upgrades_in_place_and_others_are_refused() {
    let dir = Scratch::new("schema");
    let mut store = allowing(&dir.db(), 100).unwrap();
    store.execute(&register("w/a")).unwrap();
    store.execute(&claim("x", "w/a", 0)).unwrap();
    let before = store.coordinator().unwrap();
    drop(store);
    assert_eq!(schema_of(&dir.db()), "3");

    // A store as a pre-mode binary left it: schema "1", leases without a
    // mode field.
    set_schema(&dir.db(), "1");
    {
        let conn = rusqlite::Connection::open(dir.db()).unwrap();
        let json: String = conn
            .query_row("SELECT coordinator FROM state WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut state: serde_json::Value = serde_json::from_str(&json).unwrap();
        for lease in state["leases"].as_object_mut().unwrap().values_mut() {
            lease.as_object_mut().unwrap().remove("mode");
        }
        conn.execute(
            "UPDATE state SET coordinator = ?1 WHERE id = 1",
            [state.to_string()],
        )
        .unwrap();
        // The journal too: a pre-mode build wrote claims without a mode.
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT seq, command FROM journal")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (seq, command) in rows {
            let mut command: serde_json::Value = serde_json::from_str(&command).unwrap();
            if let Some(o) = command.as_object_mut() {
                o.remove("mode");
            }
            conn.execute(
                "UPDATE journal SET command = ?1 WHERE seq = ?2",
                rusqlite::params![command.to_string(), seq],
            )
            .unwrap();
        }
    }
    let store = allowing(&dir.db(), 100).unwrap();
    assert_eq!(store.coordinator().unwrap(), before);
    store.verify().unwrap();
    drop(store);
    assert_eq!(schema_of(&dir.db()), "3", "upgraded in place");

    set_schema(&dir.db(), "4");
    assert!(matches!(
        allowing(&dir.db(), 100),
        Err(StoreError::Schema(v)) if v == "4"
    ));
    // Review round 3 of 1bca39d: a value no build wrote is damage, not a
    // newer build's store.
    for damaged in ["", "0", "two"] {
        set_schema(&dir.db(), damaged);
        assert!(
            matches!(allowing(&dir.db(), 100), Err(StoreError::BadMeta(_))),
            "{damaged:?}"
        );
    }
}

/// Review of a837594: a handle that opened before another build changed
/// the schema can't write over it.
#[test]
fn a_handle_opened_before_an_upgrade_refuses_to_write() {
    let dir = Scratch::new("schema-race");
    let mut store = allowing(&dir.db(), 100).unwrap();
    store.execute(&register("w/a")).unwrap();
    set_schema(&dir.db(), "4");
    assert!(matches!(
        store.execute(&claim("x", "w/a", 0)),
        Err(StoreError::Schema(v)) if v == "4"
    ));
    assert!(matches!(store.coordinator(), Err(StoreError::Schema(_))));
    assert!(matches!(store.rebuild(), Err(StoreError::Schema(_))));
    assert!(matches!(store.replay(), Err(StoreError::Schema(_))));
    assert!(matches!(store.journal(), Err(StoreError::Schema(_))));
}

fn meta_of(db: &std::path::Path, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(db)
        .expect("store")
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .expect("meta")
}

/// A store left at schema "1" with one registered worktree and a claim.
fn schema_1_store(dir: &Scratch) -> mdkg_coord::Coordinator {
    let mut store = allowing(&dir.db(), 100).expect("store");
    store.execute(&register("w/a")).expect("register");
    store.execute(&claim("x", "w/a", 0)).expect("claim");
    let state = store.coordinator().expect("state");
    drop(store);
    set_schema(&dir.db(), "1");
    state
}

/// Upgrade safety (orchestrator, 2026-10-21, D): an upgrade first copies
/// the store beside it, synced, and a copy that fails stops the upgrade.
#[test]
fn an_upgrade_copies_the_store_first_or_not_at_all() {
    let dir = Scratch::new("backup");
    let before = schema_1_store(&dir);
    let copy = dir
        .db()
        .with_file_name("coord.sqlite.pre-1+msg5-to-3+msg5.sqlite");

    // The copy can't be written (its folder takes no new files, while a
    // held connection keeps the WAL the store itself writes to): nothing
    // is upgraded.
    {
        use std::os::unix::fs::PermissionsExt;
        let folder = dir.db().parent().unwrap().to_path_buf();
        let held = rusqlite::Connection::open(dir.db()).unwrap();
        let _: String = held
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let opened = allowing(&dir.db(), 100);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(held);
        assert!(matches!(opened, Err(StoreError::Backup(_))), "{opened:?}");
    }
    assert_eq!(schema_of(&dir.db()), "1", "not upgraded");
    assert!(!copy.exists());

    let store = allowing(&dir.db(), 100).unwrap();
    assert_eq!(store.backup(), Some(copy.as_path()));
    assert_eq!(store.coordinator().unwrap(), before);
    drop(store);
    assert_eq!(schema_of(&dir.db()), "3");
    assert_eq!(schema_of(&copy), "1", "the copy is the store as it was");
    assert_eq!(
        meta_of(&copy, "backup_of").as_deref(),
        Some(std::fs::canonicalize(dir.db()).unwrap().to_str().unwrap())
    );
    // Opening again upgrades nothing and copies nothing.
    let store = allowing(&dir.db(), 100).unwrap();
    assert_eq!(store.backup(), None);
    drop(store);
    let listed = mdkg_coord_sqlite::backups(&dir.db()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, copy);
    assert_eq!(listed[0].schema.as_deref(), Some("1"));
}

/// Upgrade safety: a restore lands the copy whole, over a WAL a crashed
/// process left behind, after copying the store as it was; it refuses
/// while anything has the store open, and refuses copies of other stores,
/// copies a newer build wrote, and files that aren't copies.
#[test]
fn a_restore_lands_whole_and_refuses_what_it_cant_trust() {
    let dir = Scratch::new("restore");
    let before = schema_1_store(&dir);
    let mut store = allowing(&dir.db(), 100).unwrap();
    let copy = store.backup().unwrap().to_path_buf();
    store.execute(&register("w/b")).unwrap();
    drop(store);

    // Something has the store open: refused, nothing changed.
    let held = allowing(&dir.db(), 100).unwrap();
    let refused = mdkg_coord_sqlite::restore(&dir.db(), &copy);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("open")),
        "{refused:?}"
    );
    drop(held);
    assert_eq!(schema_of(&dir.db()), "3");

    // A WAL left behind with a write in it, and nothing holding it.
    let db = dir.db();
    let wal = db.with_extension("sqlite-wal");
    let shm = db.with_extension("sqlite-shm");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('wal-marker', '1')",
            [],
        )
        .unwrap();
        for (from, to) in [(&db, "db"), (&wal, "wal"), (&shm, "shm")] {
            std::fs::copy(from, dir.0.join(to)).unwrap();
        }
    }
    for (to, from) in [(&db, "db"), (&wal, "wal"), (&shm, "shm")] {
        std::fs::copy(dir.0.join(from), to).unwrap();
    }
    assert!(wal.exists() && std::fs::metadata(&wal).unwrap().len() > 0);

    let restored = mdkg_coord_sqlite::restore(&dir.db(), &copy).unwrap();
    assert_eq!(
        (restored.replaced.as_str(), restored.restored.as_str()),
        ("3+msg5", "1+msg5")
    );
    assert!(!wal.exists(), "no WAL left behind");
    assert_eq!(schema_of(&dir.db()), "1");
    assert_eq!(meta_of(&dir.db(), "backup_of"), None);
    assert_eq!(meta_of(&dir.db(), "wal-marker"), None, "the copy, whole");
    assert_eq!(
        meta_of(&restored.previous, "wal-marker").as_deref(),
        Some("1"),
        "the store as it was, its WAL included, is kept"
    );
    // It opens (and upgrades again, copying first) to the copied state.
    let store = allowing(&dir.db(), 100).unwrap();
    assert!(store.backup().is_some());
    assert_eq!(store.coordinator().unwrap(), before);
    store.verify().unwrap();
    drop(store);

    // A copy a newer build wrote.
    let newer = dir.db().with_file_name("coord.sqlite.pre-9-to-10.sqlite");
    std::fs::copy(&copy, &newer).unwrap();
    set_schema(&newer, "9");
    let refused = mdkg_coord_sqlite::restore(&dir.db(), &newer);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("newer build")),
        "{refused:?}"
    );
    // A copy of another repository's store.
    let other = Scratch::new("restore-other");
    drop(allowing(&other.db(), 100).unwrap());
    let refused = mdkg_coord_sqlite::restore(&other.db(), &copy);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("not of this")),
        "{refused:?}"
    );
    // A store that isn't a copy.
    let refused = mdkg_coord_sqlite::restore(&dir.db(), &other.db());
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("isn't a copy")),
        "{refused:?}"
    );
    assert_eq!(schema_of(&dir.db()), "3", "none of them changed the store");
}

fn copies_of(db: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(db.parent().expect("dir"))
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".pre-"))
        .collect();
    names.sort();
    names
}

/// Review of 1e2b400: mail-only upgrades copy first too; an upgrade that
/// fails after the copy leaves none behind; copies read without a WAL.
#[test]
fn every_upgrade_copies_and_a_failed_one_leaves_nothing() {
    // Mail tables at "2".
    let dir = Scratch::new("mail-up");
    drop(allowing(&dir.db(), 100).unwrap());
    rusqlite::Connection::open(dir.db())
        .unwrap()
        .execute("UPDATE meta SET value = '2' WHERE key = 'msg_schema'", [])
        .unwrap();
    let store = allowing(&dir.db(), 100).unwrap();
    assert!(
        store
            .backup()
            .is_some_and(|b| b.ends_with("coord.sqlite.pre-3+msg2-to-3+msg5.sqlite")),
        "{:?}",
        store.backup()
    );
    drop(store);
    // No mail tables at all (a lease-guard build's store).
    let dir = Scratch::new("mail-new");
    drop(allowing(&dir.db(), 100).unwrap());
    {
        let conn = rusqlite::Connection::open(dir.db()).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'msg_%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for t in tables {
            conn.execute_batch(&format!("DROP TABLE {t}")).unwrap();
        }
        conn.execute("DELETE FROM meta WHERE key = 'msg_schema'", [])
            .unwrap();
    }
    let store = allowing(&dir.db(), 100).unwrap();
    assert!(
        store
            .backup()
            .is_some_and(|b| b.ends_with("coord.sqlite.pre-3-to-3+msg5.sqlite")),
        "{:?}",
        store.backup()
    );
    drop(store);
    // Listing reads a copy without leaving a WAL beside it.
    let listed = mdkg_coord_sqlite::backups(&dir.db()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].schema.as_deref(), Some("3"));
    assert_eq!(
        copies_of(&dir.db()),
        vec!["coord.sqlite.pre-3-to-3+msg5.sqlite"]
    );

    // The upgrade fails after the copy (a damaged ttl): nothing is left,
    // however often it's tried.
    let dir = Scratch::new("failed-up");
    schema_1_store(&dir);
    rusqlite::Connection::open(dir.db())
        .unwrap()
        .execute("UPDATE meta SET value = 'bogus' WHERE key = 'ttl'", [])
        .unwrap();
    for _ in 0..3 {
        assert!(matches!(
            allowing(&dir.db(), 100),
            Err(StoreError::BadMeta(_))
        ));
    }
    assert_eq!(copies_of(&dir.db()), Vec::<String>::new());
    assert_eq!(schema_of(&dir.db()), "1");
}

/// Review of 1e2b400: a moved repository's copies still restore (the
/// store's own id matches), a damaged copy is refused, and a restore
/// leaves no rollback journal behind.
#[test]
fn a_moved_store_restores_its_copies_and_damage_is_refused() {
    let dir = Scratch::new("moved");
    schema_1_store(&dir);
    // Every store in the field predates ids: the upgrading open mints one
    // before copying, so the copy carries it.
    rusqlite::Connection::open(dir.db())
        .unwrap()
        .execute("DELETE FROM meta WHERE key = 'store_id'", [])
        .unwrap();
    let store = allowing(&dir.db(), 100).unwrap();
    let copy = store.backup().unwrap().to_path_buf();
    drop(store);
    let id = meta_of(&dir.db(), "store_id");
    assert!(id.is_some());
    assert_eq!(meta_of(&copy, "store_id"), id, "the copy carries the id");
    // A copy written as 1e2b400 wrote them, in WAL mode: restoring it must
    // leave no journal either.
    rusqlite::Connection::open(&copy)
        .unwrap()
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    // The repository moves: the store and its copy, as `cp -a` would.
    let moved = Scratch::new("moved-to");
    std::fs::create_dir_all(moved.db().parent().unwrap()).unwrap();
    std::fs::copy(dir.db(), moved.db()).unwrap();
    let moved_copy = moved.db().with_file_name(copy.file_name().unwrap());
    std::fs::copy(&copy, &moved_copy).unwrap();
    // Listed as its own, and restored.
    let listed = mdkg_coord_sqlite::backups(&moved.db()).unwrap();
    assert!(listed.iter().any(|b| b.path == moved_copy
        && mdkg_coord_sqlite::copy_of(
            &moved.db(),
            &b.path,
            b.backup_of.as_deref(),
            b.store_id.as_deref()
        )));
    // Another clone's copy, by its own path, is refused though the ids
    // match: only a copy beside the store goes by id.
    let refused = mdkg_coord_sqlite::restore(&moved.db(), &copy);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("not of this")),
        "{refused:?}"
    );
    let restored = mdkg_coord_sqlite::restore(&moved.db(), &moved_copy).unwrap();
    assert_eq!(restored.restored, "1+msg5");
    let journal = moved.db().with_file_name("coord.sqlite-journal");
    assert!(!journal.exists(), "no journal left behind");

    // Without ids on both sides, a moved copy doesn't match.
    for db in [&moved.db(), &moved_copy] {
        rusqlite::Connection::open(db)
            .unwrap()
            .execute("DELETE FROM meta WHERE key = 'store_id'", [])
            .unwrap();
    }
    let refused = mdkg_coord_sqlite::restore(&moved.db(), &moved_copy);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("not of this")),
        "{refused:?}"
    );

    // A damaged copy.
    let damaged = moved.db().with_file_name("coord.sqlite.pre-x-to-y.sqlite");
    std::fs::write(&damaged, b"not a database at all, just bytes").unwrap();
    let refused = mdkg_coord_sqlite::restore(&moved.db(), &damaged);
    assert!(
        matches!(&refused, Err(StoreError::Restore(m)) if m.contains("can't be read") || m.contains("damaged")),
        "{refused:?}"
    );
}

/// task-39, the 10-22 incident: a build running from a Cargo build folder
/// (as this test does) refuses to upgrade an existing store, changing and
/// copying nothing, while it still creates new stores; an installed build,
/// or one told to, upgrades.
#[test]
fn a_build_folder_binary_refuses_to_upgrade_a_store() {
    let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    assert!(mdkg_coord_sqlite::in_build_folder(&exe), "{exe:?}");
    assert_eq!(Upgrade::for_this_process(), Upgrade::Refuse);

    // A new store is no upgrade.
    let fresh = Scratch::new("guard-new");
    drop(Store::open(&fresh.db(), 100).expect("a new store"));
    assert_eq!(schema_of(&fresh.db()), "3");

    let dir = Scratch::new("guard");
    let before = schema_1_store(&dir);
    for _ in 0..2 {
        match Store::open(&dir.db(), 100) {
            Err(StoreError::UpgradeRefused { from, to }) => {
                assert_eq!((from.as_str(), to.as_str()), ("1+msg5", "3+msg5"));
            }
            other => panic!("not refused: {other:?}"),
        }
    }
    assert_eq!(schema_of(&dir.db()), "1", "not upgraded");
    assert_eq!(copies_of(&dir.db()), Vec::<String>::new(), "nothing copied");

    // Mail-only upgrades are upgrades too.
    let mail = Scratch::new("guard-mail");
    drop(Store::open(&mail.db(), 100).unwrap());
    rusqlite::Connection::open(mail.db())
        .unwrap()
        .execute("UPDATE meta SET value = '2' WHERE key = 'msg_schema'", [])
        .unwrap();
    assert!(matches!(
        Store::open_with(&mail.db(), 100, Upgrade::Refuse),
        Err(StoreError::UpgradeRefused { .. })
    ));
    assert_eq!(meta_of(&mail.db(), "msg_schema").as_deref(), Some("2"));

    // Allowed, it upgrades as before.
    let store = allowing(&dir.db(), 100).unwrap();
    assert!(store.backup().is_some());
    assert_eq!(store.coordinator().unwrap(), before);
    drop(store);
    // At this build's schema there's nothing to refuse.
    drop(Store::open(&dir.db(), 100).expect("current schema opens"));
}

/// What counts as a Cargo build folder: one this crate was built in, or
/// one whose root carries cargo's `CACHEDIR.TAG` or `.rustc_info.json`,
/// up to four folders above the binary; anything else (an install folder,
/// another tool's cache tag) is an installed binary. (Scratch folders sit
/// outside this workspace and its target folder.)
#[test]
fn build_folders_are_found_by_cargos_markers() {
    use mdkg_coord_sqlite::in_build_folder;
    let dir = Scratch::new("markers");
    let tag = "Signature: 8a477f597d28d172789f06886806bc55\n\
               # This file is a cache directory tag created by cargo.\n";
    let target = dir.0.join("target");
    std::fs::create_dir_all(target.join("aarch64-apple-darwin/release/deps")).unwrap();
    std::fs::write(target.join("CACHEDIR.TAG"), tag).unwrap();
    assert!(in_build_folder(&target.join("debug/mdkg")));
    assert!(in_build_folder(&target.join("debug/deps/store-0123")));
    assert!(in_build_folder(
        &target.join("aarch64-apple-darwin/release/deps/x")
    ));
    assert!(!in_build_folder(&target.join("a/b/c/d/mdkg")), "too deep");

    let other = dir.0.join("rustc");
    std::fs::create_dir_all(other.join("debug")).unwrap();
    std::fs::write(other.join(".rustc_info.json"), "{}").unwrap();
    assert!(in_build_folder(&other.join("debug/mdkg")));

    let cache = dir.0.join("cache");
    std::fs::create_dir_all(cache.join("bin")).unwrap();
    std::fs::write(
        cache.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# created by borg\n",
    )
    .unwrap();
    assert!(!in_build_folder(&cache.join("bin/mdkg")));
    assert!(!in_build_folder(&dir.0.join("bin/mdkg")));

    // Review of 9a2e736: markers alone don't make an install a build, and
    // a folder's name doesn't either.
    use mdkg_coord_sqlite::has_cargo_markers;
    let install = dir.0.join("opt");
    assert!(!has_cargo_markers(&install.join("release/mdkg")));
    assert!(!in_build_folder(&install.join("release/mdkg")));
    assert!(!in_build_folder(&install.join("debug/mdkg")));
    assert!(has_cargo_markers(&target.join("debug/mdkg")));
}

/// Review of e427c19: a refusal doesn't checkpoint a WAL that a crashed
/// writer left: the store file and its WAL stay as they were.
#[test]
fn a_refusal_leaves_a_crashed_writers_wal_alone() {
    let dir = Scratch::new("guard-wal");
    schema_1_store(&dir);
    set_schema(&dir.db(), "2");
    // A writer moves it back to "1" in its WAL and "crashes": the files
    // are copied while it still has them open, so nothing checkpointed.
    let crashed = Scratch::new("guard-wal-copy");
    std::fs::create_dir_all(crashed.db().parent().unwrap()).unwrap();
    {
        let writer = rusqlite::Connection::open(dir.db()).unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        writer
            .execute("UPDATE meta SET value = '1' WHERE key = 'schema'", [])
            .unwrap();
        let wal = |p: &std::path::Path| p.with_file_name("coord.sqlite-wal");
        std::fs::copy(dir.db(), crashed.db()).unwrap();
        std::fs::copy(wal(&dir.db()), wal(&crashed.db())).unwrap();
    }
    let wal = crashed.db().with_file_name("coord.sqlite-wal");
    let (db_before, wal_before) = (
        std::fs::read(crashed.db()).unwrap(),
        std::fs::read(&wal).unwrap(),
    );
    assert!(!wal_before.is_empty());
    match Store::open_with(&crashed.db(), 100, Upgrade::Refuse) {
        Err(StoreError::UpgradeRefused { from, .. }) => assert_eq!(from, "1+msg5"),
        other => panic!("not refused: {other:?}"),
    }
    assert_eq!(
        std::fs::read(crashed.db()).unwrap(),
        db_before,
        "store file"
    );
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before, "its WAL");
}

/// Review of fe4746c: a refusal writes nothing at all, not even a store
/// id or the journal mode; the store's file is byte for byte the same.
#[test]
fn a_refused_upgrade_leaves_the_file_unchanged() {
    let dir = Scratch::new("guard-bytes");
    schema_1_store(&dir);
    {
        let conn = rusqlite::Connection::open(dir.db()).unwrap();
        conn.execute("DELETE FROM meta WHERE key = 'store_id'", [])
            .unwrap();
        // As a restore leaves it.
        let mode: String = conn
            .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }
    let before = std::fs::read(dir.db()).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            Store::open_with(&dir.db(), 100, Upgrade::Refuse),
            Err(StoreError::UpgradeRefused { .. })
        ));
    }
    assert_eq!(std::fs::read(dir.db()).unwrap(), before, "unchanged");
    assert_eq!(meta_of(&dir.db(), "store_id"), None);
    assert_eq!(copies_of(&dir.db()), Vec::<String>::new());
}

#[test]
fn a_chained_command_is_recorded_with_the_first_or_neither_is() {
    let dir = Scratch::new("chained");
    let mut store = Store::open(&dir.db(), 1_000).expect("open");
    store.execute(&register("w/a")).expect("register");
    store.execute(&register("w/b")).expect("register");
    let before = store.journal().expect("journal").len();

    // Both recorded: a claim, then a second claim built from its outcome.
    let chained = store
        .execute_then(&claim("x", "w/a", 1), |_, outcome| {
            matches!(outcome, Outcome::Lease { .. }).then(|| claim("x", "w/b", 1))
        })
        .expect("chained");
    assert!(chained.recorded);
    assert_eq!(store.journal().expect("journal").len(), before + 2);

    // The second refused (y releases a lease it doesn't own): the first
    // is rolled back with it, and nothing is journaled.
    let lease = match chained.first {
        Outcome::Lease { lease } => lease,
        other => panic!("{other:?}"),
    };
    let chained = store
        .execute_then(&register("w/c"), |_, _| {
            Some(Command::Release {
                owner: "y".into(),
                lease: lease.clone(),
                now: 2,
            })
        })
        .expect("chained");
    assert!(!chained.recorded);
    assert!(chained.second.as_ref().is_some_and(|o| o.error().is_some()));
    assert_eq!(store.journal().expect("journal").len(), before + 2);
    let state = store.coordinator().expect("state");
    assert!(
        state.leaf(&Scope::new("w/c")).is_none(),
        "w/c was rolled back"
    );

    // No second command: the first alone, as execute does.
    let chained = store
        .execute_then(&register("w/c"), |_, _| None)
        .expect("chained");
    assert!(chained.recorded && chained.second.is_none());
    assert_eq!(store.journal().expect("journal").len(), before + 3);
    store.verify().expect("the journal replays to the state");
}
