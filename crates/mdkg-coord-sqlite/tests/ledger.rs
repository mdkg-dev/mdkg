//! The flow tables (edd-01a1878e, Storage): store schema 4. A schema 3
//! store upgrades on open, copied first, its chain going on; a build that
//! refuses upgrades refuses it; and a store restore from before the
//! upgrade comes back whole, to upgrade again on the next open.

#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;

use mdkg_coord_sqlite::ledger::NodeLink;
use mdkg_coord_sqlite::mail::{InboxQuery, Outgoing, Page, Retention, Shown, To};
use mdkg_coord_sqlite::{Store, StoreError, Upgrade};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

const FLOW_TABLES: &[&str] = &["msg_links", "msg_meta", "entries", "entry_events"];

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Scratch(temp::dir(&format!("mdkg-ledger-{name}")))
    }

    fn db(&self) -> PathBuf {
        self.0.join("mdkg").join("coord.sqlite")
    }

    fn open(&self, upgrade: Upgrade) -> Store {
        Store::open_with(&self.db(), 1_000, upgrade).expect("store")
    }

    fn raw(&self) -> Connection {
        Connection::open(self.db()).expect("raw")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .expect("meta")
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("tables");
    stmt.query_map([], |r| r.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("names")
}

/// What 0.7.0 left: the same store at schema 3, without the flow tables.
fn as_schema_3(conn: &Connection) {
    for t in FLOW_TABLES {
        conn.execute_batch(&format!("DROP TABLE {t}"))
            .expect("drop");
    }
    conn.execute("UPDATE meta SET value = '3' WHERE key = 'schema'", [])
        .expect("stamp 3");
}

fn chain_len(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM log", [], |r| r.get(0))
        .expect("log")
}

fn copies(dir: &Scratch) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.db().parent().expect("dir"))
        .expect("list")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".pre-"))
        // A copy's proof of its final barrier sits beside it.
        .filter(|n| !n.ends_with(".synced"))
        .collect();
    names.sort();
    names
}

fn note(id: &str, to: &str, now_ms: u64) -> Outgoing {
    Outgoing {
        message_id: id.into(),
        context_id: None,
        kind: "note".into(),
        sender: "orchestrator".into(),
        via: None,
        sender_mailboxes: vec!["orchestrator".into()],
        to: To::Mailboxes(vec![to.into()]),
        reference_ids: vec![],
        parts: json!([{"kind": "text", "text": id}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 0,
        now_ms,
    }
}

fn read_and_ack(s: &mut Store, mailbox: &str, now_ms: u64) -> Vec<String> {
    let inbox = s
        .inbox(&InboxQuery {
            mailboxes: vec![mailbox.into()],
            now_ms,
            ..InboxQuery::default()
        })
        .expect("inbox");
    let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
    s.mark_shown(&listed, &[mailbox.into()], now_ms)
        .expect("shown");
    let ids: Vec<String> = inbox
        .messages
        .iter()
        .map(|m| m.message_id.clone())
        .collect();
    s.ack(&ids, &[mailbox.into()], mailbox, None, None, now_ms)
        .expect("ack");
    ids
}

#[test]
fn a_new_store_is_current_with_the_flow_tables() {
    let dir = Scratch::new("new");
    drop(dir.open(Upgrade::Allow));
    let raw = dir.raw();
    let names = tables(&raw);
    for t in FLOW_TABLES {
        assert!(names.contains(&t.to_string()), "{t} in {names:?}");
    }
    assert_eq!(meta(&raw, "schema").as_deref(), Some("5"));
    assert_eq!(meta(&raw, "msg_schema").as_deref(), Some("5"));
    assert_eq!(mdkg_coord_sqlite::build_label(), "5+msg5");
}

#[test]
fn a_schema_3_store_upgrades_with_a_copy_and_its_chain_goes_on() {
    let dir = Scratch::new("upgrade");
    {
        let mut s = dir.open(Upgrade::Allow);
        s.send(&note("old", "a-flow", 1)).expect("send");
    }
    as_schema_3(&dir.raw());
    let before = chain_len(&dir.raw());
    // A build that refuses upgrades (a development build) leaves it alone.
    match Store::open_with(&dir.db(), 1_000, Upgrade::Refuse) {
        Err(StoreError::UpgradeRefused { from, to }) => {
            assert_eq!((from.as_str(), to.as_str()), ("3+msg5", "5+msg5"));
        }
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("a refusing build upgraded the store"),
    }
    assert_eq!(meta(&dir.raw(), "schema").as_deref(), Some("3"));
    assert!(copies(&dir).is_empty());
    let mut s = dir.open(Upgrade::Allow);
    assert_eq!(
        copies(&dir),
        vec!["coord.sqlite.pre-3+msg5-to-5+msg5.sqlite"]
    );
    assert!(s.backup().is_some());
    let raw = dir.raw();
    assert_eq!(meta(&raw, "schema").as_deref(), Some("5"));
    for t in FLOW_TABLES {
        assert!(tables(&raw).contains(&t.to_string()), "{t}");
    }
    // No second genesis: the chain is the one it had.
    assert_eq!(chain_len(&raw), before);
    let flow = s.message_flow("old").expect("flow");
    assert!(flow.nodes.is_empty() && flow.expires_ms.is_none());
    assert_eq!(read_and_ack(&mut s, "a-flow", 2), vec!["old"]);
    s.verify().expect("state");
    s.verify_log().expect("chain");
    // The copy is schema 3, as it was.
    let copy = Connection::open(
        dir.db()
            .with_file_name("coord.sqlite.pre-3+msg5-to-5+msg5.sqlite"),
    )
    .expect("copy");
    assert_eq!(meta(&copy, "schema").as_deref(), Some("3"));
    assert!(!tables(&copy).contains(&"entries".to_string()));
}

/// What an older build meets (decided 2026-10-29: one bump, so nothing
/// predating the flow rules writes around them): a store at a schema it
/// doesn't know is refused as newer. This build stands in for it with a
/// store one version past its own.
#[test]
fn a_store_past_this_builds_schema_is_refused_as_newer() {
    let dir = Scratch::new("newer");
    drop(dir.open(Upgrade::Allow));
    dir.raw()
        .execute("UPDATE meta SET value = '6' WHERE key = 'schema'", [])
        .expect("stamp 6");
    match Store::open_with(&dir.db(), 1_000, Upgrade::Allow) {
        Err(StoreError::Schema(v)) => assert_eq!(v, "6"),
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("opened a newer store"),
    }
}

#[test]
fn flow_rows_survive_reopening_and_mail_goes_on_beside_them() {
    let dir = Scratch::new("rows");
    drop(dir.open(Upgrade::Allow));
    let node = NodeLink::of("task-01a1878e-3e6f-710b-9d01-68d002aed3e3").expect("node");
    {
        let raw = dir.raw();
        raw.execute(
            "INSERT INTO msg_links (seq, node_uuid, node_id, entry) VALUES (999, ?1, ?2, '')",
            [&node.uuid, &node.id],
        )
        .expect("link");
        raw.execute(
            "INSERT INTO entries (node_uuid, entry_id, kind, state, title, data, opened_ms, \
             updated_ms) VALUES (?1, 'F1', 'finding', 'open', 't', '{}', 1, 1)",
            [&node.uuid],
        )
        .expect("entry");
    }
    let mut s = dir.open(Upgrade::Refuse);
    s.send(&note("m1", "a-flow", 10)).expect("send");
    s.send(&note("m2", "a-flow", 11)).expect("send");
    assert_eq!(read_and_ack(&mut s, "a-flow", 12), vec!["m1", "m2"]);
    s.send(&note("m3", "a-flow", 13)).expect("send");
    let (_, thread) = s
        .thread(
            "m3",
            Page {
                after: None,
                limit: None,
                newest: false,
            },
            14,
        )
        .expect("thread");
    assert_eq!(thread.len(), 1);
    s.verify().expect("state");
    s.verify_log().expect("chain");
    drop(s);
    let s = dir.open(Upgrade::Refuse);
    assert_eq!(s.bound_to(&node.uuid, 0).expect("bound"), vec![999]);
    let n: i64 = dir
        .raw()
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 1);
}

#[test]
fn a_restore_from_before_the_upgrade_lands_whole_and_upgrades_again() {
    let dir = Scratch::new("restore");
    {
        let mut s = dir.open(Upgrade::Allow);
        s.send(&note("old", "a-flow", 1)).expect("send");
    }
    as_schema_3(&dir.raw());
    drop(dir.open(Upgrade::Allow));
    let copy = dir
        .db()
        .with_file_name("coord.sqlite.pre-3+msg5-to-5+msg5.sqlite");
    dir.raw()
        .execute(
            "INSERT INTO entries (node_uuid, entry_id, kind, state, title, data, opened_ms, \
             updated_ms) VALUES ('u', 'F1', 'finding', 'open', 't', '{}', 1, 1)",
            [],
        )
        .expect("entry");
    let restored = mdkg_coord_sqlite::restore(&dir.db(), &copy).expect("restore");
    assert_eq!(
        (restored.replaced.as_str(), restored.restored.as_str()),
        ("5+msg5", "3+msg5")
    );
    {
        let raw = dir.raw();
        assert_eq!(meta(&raw, "schema").as_deref(), Some("3"));
        assert!(!tables(&raw).contains(&"entries".to_string()));
    }
    let s = dir.open(Upgrade::Allow);
    let raw = dir.raw();
    assert_eq!(meta(&raw, "schema").as_deref(), Some("5"));
    let n: i64 = raw
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 0);
    s.verify_log().expect("chain");
}
