//! Every write that changes coordination state appends one record to the
//! store's chain, in the same transaction (task-39).

#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;

use mdkg_coord::{Command, Mode, Outcome, Scope};
use mdkg_coord_sqlite::mail::{Outgoing, Retention, Shown, To};
use mdkg_coord_sqlite::{Store, StoreError, Upgrade};
use mdkg_log::record::{GENESIS, Record};
use serde_json::json;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = temp::dir(&format!("mdkg-log-{name}"));
        Scratch(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("mdkg").join("coord.sqlite")
    }

    fn store(&self) -> Store {
        Store::open_with(&self.db(), 1_000, Upgrade::Allow).expect("store")
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).expect("open")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn records(store: &Store) -> Vec<Record> {
    store
        .log_texts()
        .expect("log")
        .iter()
        .map(|t| Record::parse(t).expect("record").0)
        .collect()
}

fn register(leaf: &str) -> Command {
    Command::Register {
        leaf: Scope::new(leaf),
        content: format!("{leaf}@0"),
    }
}

fn claim(owner: &str, leaf: &str, node: Option<&str>) -> Command {
    Command::Claim {
        owner: owner.into(),
        scope: Scope::new(leaf),
        mode: Mode::Exclusive,
        now: 0,
        request: None,
        node: node.map(String::from),
    }
}

fn post(id: &str, body: &str) -> Outgoing {
    Outgoing {
        message_id: id.into(),
        context_id: None,
        kind: "note".into(),
        sender: "a".into(),
        via: None,
        sender_mailboxes: vec!["a".into()],
        to: To::Room("r".into()),
        reference_ids: Vec::new(),
        parts: json!([{"kind": "text", "text": body}]),
        retention: Retention::Durable,
        human_hold_days: 7,
        now_ms: 1,
    }
}

#[test]
fn a_new_store_opens_its_chain_with_a_genesis() {
    let dir = Scratch::new("genesis");
    let store = dir.store();
    let r = records(&store);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, GENESIS);
    assert_eq!(r[0].seq, 1);
    assert_eq!(r[0].body["store"], json!(store.store_id().unwrap()));
    assert_eq!(r[0].body["journal_seq"], json!(0));
    let report = store.verify_log().unwrap();
    assert_eq!(report.count, 1);
    assert_eq!(store.log_head().unwrap(), (1, report.head));
    // Reopening adds nothing.
    drop(store);
    assert_eq!(records(&dir.store()).len(), 1);
}

#[test]
fn each_command_appends_one_record_naming_its_node() {
    let dir = Scratch::new("commands");
    let mut store = dir.store();
    store.execute(&register("w/task-7.md")).unwrap();
    let lease = match store
        .execute(&claim("lane-a", "w/task-7.md", Some("w#task-7")))
        .unwrap()
    {
        Outcome::Lease { lease } => lease,
        other => panic!("{other:?}"),
    };
    store
        .execute(&Command::Heartbeat {
            owner: "lane-a".into(),
            lease: lease.clone(),
            now: 5,
        })
        .unwrap();
    // A refusal is journaled, so it is recorded too.
    store
        .execute(&claim("lane-b", "w/task-7.md", Some("w#task-7")))
        .unwrap();
    store
        .execute(&Command::Release {
            owner: "lane-a".into(),
            lease: lease.clone(),
            now: 6,
        })
        .unwrap();
    let r = records(&store);
    let kinds: Vec<&str> = r.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            GENESIS,
            "leaf.register",
            "lease.claim",
            "lease.renew",
            "lease.claim",
            "lease.release"
        ]
    );
    assert_eq!(r[1].subject, ["path:w/task-7.md"]);
    assert_eq!(
        r[2].subject,
        [
            "legacy:task-7",
            "path:w/task-7.md",
            &format!("lease:{lease}")
        ]
    );
    assert_eq!(r[3].subject[0], "legacy:task-7");
    assert_eq!(r[2].actor, "lane-a", "the command's own actor");
    assert_eq!(r[4].actor, "lane-b");
    // The body is the journal entry, whole.
    let journal = store.journal().unwrap();
    for (record, (seq, command, outcome)) in r[1..].iter().zip(&journal) {
        assert_eq!(record.body["journal"], json!(seq));
        assert_eq!(
            record.body["command"],
            serde_json::to_value(command).unwrap()
        );
        assert_eq!(
            record.body["outcome"],
            serde_json::to_value(outcome).unwrap()
        );
    }
    assert!(r[4].body["outcome"]["result"] == "refused");
    let report = store.verify_log().unwrap();
    assert_eq!(report.count, 6);
}

/// One failed write never leaves a mutation without its record, or a
/// record without its mutation: either insert failing rolls back both.
#[test]
fn a_failed_write_lands_neither_the_mutation_nor_its_record() {
    let dir = Scratch::new("atomic");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    for table in ["log", "journal", "state"] {
        dir.sql()
            .execute_batch(&format!(
                "CREATE TRIGGER fail BEFORE INSERT ON {table} \
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;"
            ))
            .unwrap();
        let before = (store.journal().unwrap(), store.log_texts().unwrap());
        let failed = store.execute(&claim("x", "w/a", None));
        // `state` is upserted: only its first insert would fire the
        // trigger, so it may succeed; the other two must fail.
        if table != "state" {
            assert!(
                matches!(failed, Err(StoreError::Sqlite(_))),
                "{table}: {failed:?}"
            );
            assert_eq!(
                (store.journal().unwrap(), store.log_texts().unwrap()),
                before,
                "{table}: nothing landed"
            );
        }
        dir.sql().execute_batch("DROP TRIGGER fail;").unwrap();
        store.verify_log().unwrap();
        store.verify().unwrap();
    }
}

/// A store from before the log (schema "2", no `log` table) gains the
/// table and a genesis after its journal; nothing else changes.
#[test]
fn an_upgraded_store_opens_its_chain_after_its_journal() {
    let dir = Scratch::new("upgrade");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    store.execute(&claim("x", "w/a", None)).unwrap();
    let state = store.coordinator().unwrap();
    drop(store);
    dir.sql()
        .execute_batch(
            "DROP TABLE log; DELETE FROM meta WHERE key IN ('log_head', 'last_checkpoint');
             UPDATE meta SET value = '2' WHERE key = 'schema';",
        )
        .unwrap();

    let mut store = dir.store();
    assert!(store.backup().is_some());
    assert_eq!(store.coordinator().unwrap(), state);
    let r = records(&store);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].body["journal_seq"], json!(2));
    store.execute(&register("w/b")).unwrap();
    let r = records(&store);
    assert_eq!(r[1].body["journal"], json!(3));
    assert_eq!(store.verify_log().unwrap().journal_from, 2);
}

#[test]
fn verify_finds_every_kind_of_damage() {
    let damaged = |name: &str, sql: &str| {
        let dir = Scratch::new(name);
        let mut store = dir.store();
        store.execute(&register("w/a")).unwrap();
        store.execute(&register("w/b")).unwrap();
        store.verify_log().unwrap();
        dir.sql().execute_batch(sql).unwrap();
        let found = store.verify_log();
        assert!(
            matches!(found, Err(StoreError::Log(_))),
            "{name}: {found:?}"
        );
    };
    damaged(
        "edited",
        "UPDATE log SET record = replace(record, 'w/a', 'w/z') WHERE seq = 2;",
    );
    damaged(
        "rehashed",
        "UPDATE log SET hash = 'sha256:0000000000000000000000000000000000000000000000000000000000000000' WHERE seq = 3;",
    );
    damaged("dropped", "DELETE FROM log WHERE seq = 3;");
    damaged("cut", "DELETE FROM log WHERE seq = 2;");
    damaged(
        "unrecorded",
        "INSERT INTO journal (seq, command, outcome) VALUES (3, '{}', '{}');",
    );
    damaged(
        "other store",
        "UPDATE meta SET value = 'ffff' WHERE key = 'store_id';",
    );
}

#[test]
fn mail_records_carry_digests_never_bodies() {
    let dir = Scratch::new("mail");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    s.room_join("r", &["c".into()], "a", &["a".into()], 0)
        .unwrap();
    // Joining again changes nothing, so records nothing.
    s.room_join("r", &["c".into()], "a", &["a".into()], 0)
        .unwrap();
    let (m, _) = s.send(&post("m1", "the secret plan")).unwrap();
    // A retry of the same send stores nothing new.
    assert!(s.send(&post("m1", "the secret plan")).unwrap().1);
    // Shown to b first: an ack needs that, and it records nothing.
    s.mark_shown(&[Shown::of(&m)], &["b".into()], 1).unwrap();
    s.ack(&["m1".into()], &["b".into()], "b", None, Some("did it"), 2)
        .unwrap();
    s.ack(&["m1".into()], &["b".into()], "b", None, Some("did it"), 3)
        .unwrap();
    s.room_leave("r", &["c".into()], "c", &["c".into()], false)
        .unwrap();
    let r = records(&s);
    let kinds: Vec<&str> = r.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            GENESIS,
            "room.create",
            "room.join",
            "msg.send",
            "msg.ack",
            "room.leave"
        ]
    );
    let text = s.log_texts().unwrap().join("\n");
    assert!(!text.contains("secret"), "no body in the log");
    assert!(!text.contains("did it"), "no outcome text in the log");
    assert_eq!(r[3].body["digest"], json!(m.digest));
    assert_eq!(r[3].body["to"], json!(["b", "c"]));
    assert_eq!(r[3].subject, ["msg:m1", "room:r"]);
    assert_eq!(r[3].actor, "a");
    assert_eq!(
        r[4].body["acked"],
        json!([{"message_id": "m1", "mailbox": "b"}])
    );
    assert_eq!(r[4].actor, "b");
    assert_eq!(r[5].body["removed"], json!(["c"]));
    s.verify_log().unwrap();
}

/// Lane 3's import (#22): one transaction, once per import id, with the
/// records' own times and actors after a `log.import` marker.
#[test]
fn an_import_lands_once_with_its_own_times() {
    use mdkg_coord_sqlite::log::Imported;
    let dir = Scratch::new("import");
    let mut store = dir.store();
    let mut body = serde_json::Map::new();
    body.insert("line".into(), json!(1));
    let imported = vec![
        Imported {
            ts_ms: 1_000,
            actor: "nick".into(),
            kind: "legacy.event".into(),
            subject: vec!["legacy:task-19".into()],
            body: body.clone(),
        },
        Imported {
            ts_ms: 2_000,
            actor: "lane-a".into(),
            kind: "legacy.checkpoint".into(),
            subject: vec![],
            body,
        },
    ];
    let first = store.import_records("events-v1", &imported).unwrap();
    assert_eq!((first.appended, first.already), (3, false));
    let again = store.import_records("events-v1", &imported).unwrap();
    assert_eq!((again.appended, again.already), (0, true));
    let r = records(&store);
    let kinds: Vec<&str> = r.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [GENESIS, "log.import", "legacy.event", "legacy.checkpoint"]
    );
    assert_eq!(r[1].body["count"], json!(2));
    assert_eq!(r[2].ts, "1970-01-01T00:00:01.000Z");
    assert_eq!(r[3].actor, "lane-a");
    // A kind that isn't <domain>.<verb> refuses the whole import.
    let mut bad = imported.clone();
    bad[1].kind = "Legacy".into();
    assert!(store.import_records("events-v2", &bad).is_err());
    assert_eq!(records_len(&store), 4);
    store.execute(&register("w/a")).unwrap();
    store.verify_log().unwrap();
}

fn records_len(store: &Store) -> usize {
    store.log_texts().expect("log").len()
}

/// Lane 3's snapshot (#22): a copy `store snapshots` lists and `store
/// restore` takes.
#[test]
fn a_snapshot_is_a_copy_restore_takes() {
    let dir = Scratch::new("snapshot");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    drop(store);
    let copy = mdkg_coord_sqlite::snapshot(&dir.db(), "migrate").unwrap();
    assert!(
        copy.ends_with("coord.sqlite.pre-3+msg5-to-migrate.sqlite"),
        "{copy:?}"
    );
    let listed = mdkg_coord_sqlite::backups(&dir.db()).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(mdkg_coord_sqlite::copy_of(
        &dir.db(),
        &listed[0].path,
        listed[0].backup_of.as_deref(),
        listed[0].store_id.as_deref()
    ));
    assert!(mdkg_coord_sqlite::snapshot(&dir.db(), "../x").is_err());
    let mut store = dir.store();
    store.execute(&register("w/b")).unwrap();
    drop(store);
    mdkg_coord_sqlite::restore(&dir.db(), &copy).unwrap();
    let store = dir.store();
    assert_eq!(store.journal().unwrap().len(), 1, "back to the snapshot");
    store.verify_log().unwrap();
}

/// Compaction (task-39): a `log.checkpoint` summarizes the records since
/// the last one in at most 150 words, citing their first and last hashes;
/// with nothing new there is nothing to write.
#[test]
fn a_checkpoint_summarizes_what_came_since_the_last() {
    let dir = Scratch::new("checkpoint");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    store.execute(&claim("x", "w/a", None)).unwrap();
    let first = store.checkpoint().unwrap().expect("a checkpoint");
    let r = records(&store);
    assert_eq!((first.from, first.to), (1, 3));
    assert_eq!(first.seq, 4);
    let hashes: Vec<String> = store
        .log_texts()
        .unwrap()
        .iter()
        .map(|t| Record::parse(t).unwrap().1)
        .collect();
    assert_eq!(first.first, hashes[0]);
    assert_eq!(first.last, hashes[2]);
    assert!(first.summary.contains(&hashes[0]) && first.summary.contains(&hashes[2]));
    assert!(first.summary.contains("lease.claim 1"), "{}", first.summary);
    assert_eq!(r[3].kind, "log.checkpoint");
    assert_eq!(r[3].body["summary"], json!(first.summary));
    assert!(store.checkpoint().unwrap().is_none(), "nothing new");

    store.execute(&register("w/b")).unwrap();
    let second = store.checkpoint().unwrap().expect("a second");
    assert_eq!((second.from, second.to), (5, 5), "after the first");
    store.verify_log().unwrap();
}

#[test]
fn summaries_stay_within_150_words() {
    use mdkg_coord_sqlite::log::{SUMMARY_WORDS, summarize};
    let mut chain: Vec<(Record, String)> = Vec::new();
    for i in 0..400u64 {
        let after = chain.last().map(|(r, h)| (r.seq, h.as_str()));
        let r = Record::next(
            after,
            i,
            &format!("actor-{}", i % 90),
            &format!("kind.k{}", i % 120),
            vec![],
            serde_json::Map::new(),
        );
        let h = r.seal().unwrap().1;
        chain.push((r, h));
    }
    let s = summarize(&chain);
    assert!(s.split_whitespace().count() <= SUMMARY_WORDS, "{s}");
    assert!(s.contains("and more") && s.contains(&chain[399].1), "{s}");
}

/// A 0.7 node is named by its id, bare; the lease keys it by folder.
#[test]
fn a_0_7_node_is_named_by_its_id() {
    let dir = Scratch::new("nodeid");
    let mut store = dir.store();
    let id = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    store.execute(&register("w/task-x.md")).unwrap();
    store
        .execute(&claim("a", "w/task-x.md", Some(&format!("w#{id}"))))
        .unwrap();
    assert_eq!(records(&store)[2].subject[0], id);
}

/// Review of 2faa470: bodies purged under their retention are recorded
/// (`msg.purge`) by whichever command found them due, and a failed record
/// keeps the body.
#[test]
fn retention_purges_are_recorded_with_the_deletion() {
    use mdkg_coord_sqlite::mail::InboxQuery;
    let dir = Scratch::new("purge");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    let mut ttl = post("m1", "short-lived");
    ttl.retention = Retention::Ttl(1);
    s.send(&ttl).unwrap();
    let read = |s: &mut Store, now| {
        s.inbox(&InboxQuery {
            mailboxes: vec!["b".into()],
            now_ms: now,
            ..InboxQuery::default()
        })
    };
    // Due, but the record can't be written: the read fails, the body stays.
    dir.sql()
        .execute_batch(
            "CREATE TRIGGER fail BEFORE INSERT ON log BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    let later = 10 * 24 * 3_600_000;
    assert!(read(&mut s, later).is_err());
    dir.sql().execute_batch("DROP TRIGGER fail;").unwrap();
    assert!(s.message("m1", 2).unwrap().parts.is_some(), "kept");
    let inbox = read(&mut s, later).unwrap();
    assert!(inbox.messages[0].parts.is_none(), "purged");
    let r = records(&s);
    let last = r.last().unwrap();
    assert_eq!(last.kind, "msg.purge");
    assert_eq!(last.subject, ["msg:m1"]);
    assert_eq!(last.body["purged"], json!(["m1"]));
    // Nothing more is due: no more records.
    let n = r.len();
    read(&mut s, later + 1).unwrap();
    assert_eq!(records(&s).len(), n);
    s.verify_log().unwrap();
}

/// Review of 2faa470: verify compares every journal entry with its record,
/// and notices a record dropped from the end of the chain, a mail one
/// included.
#[test]
fn verify_compares_the_journal_and_the_head() {
    let check = |name: &str, sql: &str| {
        let dir = Scratch::new(name);
        let mut store = dir.store();
        store.execute(&register("w/a")).unwrap();
        store.execute(&register("w/b")).unwrap();
        store.execute(&register("w/c")).unwrap();
        store.room_create("r", "a", &[], 0).unwrap();
        store.verify_log().unwrap();
        dir.sql().execute_batch(sql).unwrap();
        let found = store.verify_log();
        assert!(
            matches!(found, Err(StoreError::Log(_))),
            "{name}: {found:?}"
        );
    };
    check("journal-gap", "DELETE FROM journal WHERE seq = 2;");
    check(
        "journal-edit",
        "UPDATE journal SET command = replace(command, 'w/b', 'w/z') WHERE seq = 2;",
    );
    check(
        "mail-tail",
        "DELETE FROM log WHERE seq = (SELECT MAX(seq) FROM log);",
    );
}

/// Review of 2faa470: snapshots taken at once each land whole, under
/// their own names.
#[test]
fn concurrent_snapshots_each_land_whole() {
    let dir = Scratch::new("snapshots");
    let mut store = dir.store();
    for i in 0..50 {
        store.execute(&register(&format!("w/{i}"))).unwrap();
    }
    drop(store);
    let db = dir.db();
    let copies: Vec<PathBuf> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| mdkg_coord_sqlite::snapshot(&db, "same").unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut names: Vec<_> = copies.iter().collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 4, "{copies:?}");
    for copy in &copies {
        let c = rusqlite::Connection::open(copy).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 50, "{copy:?} is whole");
    }
    let leftovers: Vec<_> = std::fs::read_dir(db.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// The Sonnet review of 594da29: role and turn leases are named as such.
#[test]
fn role_and_turn_leases_are_named_as_such() {
    let dir = Scratch::new("roles");
    let mut store = dir.store();
    store.execute(&claim("a", "role:reviewer", None)).unwrap();
    let r = records(&store);
    assert_eq!(r[1].subject[0], "role:reviewer");
}

/// Lane 3's rollback (#72): restore only while the chain still ends at
/// the import's last record, decided under the restore's own lock.
#[test]
fn restore_if_head_refuses_once_the_head_moved() {
    use mdkg_coord_sqlite::log::Imported;
    let dir = Scratch::new("restore-if-head");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    drop(store);
    let copy = mdkg_coord_sqlite::snapshot(&dir.db(), "pre-migrate").unwrap();
    let mut store = dir.store();
    let one = Imported {
        ts_ms: 1,
        actor: "nick".into(),
        kind: "legacy.event".into(),
        subject: vec![],
        body: serde_json::Map::new(),
    };
    store.import_records("plan-1", &[one]).unwrap();
    let (marker, _) = store.import_marker("plan-1").unwrap().expect("marker");
    assert_eq!(marker, 3);
    assert_eq!(store.import_marker("plan-2").unwrap(), None);
    let (_, imported_head) = store.log_head().unwrap();
    // An agent's claim lands after the import.
    store.execute(&claim("agent", "w/a", None)).unwrap();
    drop(store);
    // Refused in either journal mode, which it keeps, seen through a raw
    // connection (opening a Store would move it to WAL).
    let mode_of = |db: &std::path::Path| -> String {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    };
    for mode in ["wal", "delete"] {
        let _: String = dir
            .sql()
            .query_row(&format!("PRAGMA journal_mode = {mode}"), [], |r| r.get(0))
            .unwrap();
        let refused = mdkg_coord_sqlite::restore_if_head(&dir.db(), &copy, &imported_head);
        assert!(
            matches!(&refused, Err(StoreError::Restore(m)) if m.contains("head moved")),
            "{refused:?}"
        );
        assert_eq!(mode_of(&dir.db()), mode, "kept");
    }
    let store = dir.store();
    assert_eq!(store.journal().unwrap().len(), 2, "the claim is kept");
    let (_, head) = store.log_head().unwrap();
    drop(store);
    // At the head it names, it restores.
    mdkg_coord_sqlite::restore_if_head(&dir.db(), &copy, &head).unwrap();
    assert_eq!(dir.store().journal().unwrap().len(), 1);
}

/// Spec-freeze finding 6: other kinds' bodies stay opaque. An imported
/// record that happens to carry `journal` (even of a coordinator kind)
/// doesn't count as one of this store's journal entries.
#[test]
fn imported_bodies_stay_opaque() {
    use mdkg_coord_sqlite::log::Imported;
    let dir = Scratch::new("opaque");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    let mut custom = serde_json::Map::new();
    custom.insert("journal".into(), json!("source-book"));
    let mut claim_body = serde_json::Map::new();
    claim_body.insert("journal".into(), json!(1));
    let records = vec![
        Imported {
            ts_ms: 1,
            actor: "nick".into(),
            kind: "legacy.custom".into(),
            subject: vec![],
            body: custom,
        },
        Imported {
            ts_ms: 2,
            actor: "nick".into(),
            kind: "lease.claim".into(),
            subject: vec![],
            body: claim_body,
        },
    ];
    store.import_records("plan-x", &records).unwrap();
    store.execute(&register("w/b")).unwrap();
    store.verify_log().unwrap();
    let (report, texts) = store.verified_log().unwrap();
    let bundle = mdkg_log::bundle::write_bundle(&report.store, &texts).unwrap();
    mdkg_log::bundle::verify_bundle(&bundle, None).unwrap();
}

/// Spec-freeze finding 7: once a record is cut from the chain's end, the
/// next write refuses instead of moving the store's note past the gap,
/// and verify still says so.
#[test]
fn a_cut_tail_refuses_the_next_write() {
    let dir = Scratch::new("cut-then-write");
    let mut store = dir.store();
    store.execute(&register("w/a")).unwrap();
    store.room_create("r", "a", &[], 0).unwrap();
    dir.sql()
        .execute_batch("DELETE FROM log WHERE seq = (SELECT MAX(seq) FROM log);")
        .unwrap();
    let journal = store.journal().unwrap().len();
    let refused = store.execute(&register("w/b"));
    assert!(matches!(refused, Err(StoreError::Log(_))), "{refused:?}");
    assert_eq!(store.journal().unwrap().len(), journal, "nothing landed");
    assert!(matches!(
        store.room_create("q", "a", &[], 0),
        Err(mdkg_coord_sqlite::mail::MailError::Store(StoreError::Log(
            _
        )))
    ));
    assert!(matches!(store.verify_log(), Err(StoreError::Log(_))));
}

/// Orchestrator (2026-10-25), for unmerged branches migrated after main:
/// a record whose migrate_digest the chain already holds is skipped, judged
/// inside the import's own transaction, so a rollback of main before the
/// branch's import means the branch imports them again.
#[test]
fn imports_skip_what_the_chain_already_holds() {
    use mdkg_coord_sqlite::log::Imported;
    let rec = |digest: &str, n: i64| {
        let mut body = serde_json::Map::new();
        body.insert("n".into(), json!(n));
        body.insert("migrate_digest".into(), json!(digest));
        Imported {
            ts_ms: 1_000 + n as u64,
            actor: "nick".into(),
            kind: "legacy.event".into(),
            subject: vec![],
            body,
        }
    };
    let digests_in = |store: &Store| -> Vec<String> {
        records(store)
            .iter()
            .filter_map(|r| r.body.get("migrate_digest").and_then(|d| d.as_str()))
            .map(String::from)
            .collect()
    };
    let main_records = vec![rec("sha256:a", 1), rec("sha256:b", 2)];
    let branch_records = vec![rec("sha256:a", 1), rec("sha256:b", 2), rec("sha256:c", 3)];

    // Main, then a branch sharing two records: those exist once.
    let dir = Scratch::new("dedupe");
    let mut store = dir.store();
    let main = store.import_records("plan-main", &main_records).unwrap();
    assert_eq!((main.appended, main.skipped), (3, 0));
    let branch = store
        .import_records("plan-branch", &branch_records)
        .unwrap();
    assert_eq!(
        (branch.appended, branch.skipped),
        (2, 2),
        "the marker and c"
    );
    assert_eq!(digests_in(&store), ["sha256:a", "sha256:b", "sha256:c"]);
    let markers: Vec<_> = records(&store)
        .into_iter()
        .filter(|r| r.kind == "log.import")
        .collect();
    assert_eq!(markers[1].body["count"], json!(1));
    assert_eq!(markers[1].body["skipped"], json!(2));
    store.execute(&register("w/a")).unwrap();
    store.verify_log().unwrap();
    drop(store);

    // Main imported, then rolled back (its records gone): the branch
    // imports them again.
    let dir = Scratch::new("dedupe-rollback");
    drop(dir.store());
    let before = mdkg_coord_sqlite::snapshot(&dir.db(), "pre-migrate").unwrap();
    let mut store = dir.store();
    store.import_records("plan-main", &main_records).unwrap();
    drop(store);
    mdkg_coord_sqlite::restore(&dir.db(), &before).unwrap();
    let mut store = dir.store();
    let branch = store
        .import_records("plan-branch", &branch_records)
        .unwrap();
    assert_eq!((branch.appended, branch.skipped), (4, 0));
    assert_eq!(digests_in(&store), ["sha256:a", "sha256:b", "sha256:c"]);
    store.verify_log().unwrap();

    // Records without a digest are never skipped.
    let mut plain = rec("x", 9);
    plain.body.remove("migrate_digest");
    let again = store
        .import_records("plan-plain", &[plain.clone(), plain])
        .unwrap();
    assert_eq!((again.appended, again.skipped), (3, 0));
}
