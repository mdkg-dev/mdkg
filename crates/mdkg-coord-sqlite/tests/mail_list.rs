//! `msg list` in the store (edd-01a1a1f1): mail across mailboxes by kind,
//! scope and state, paged, and read-only: the store's bytes don't change.

#[path = "common/temp.rs"]
mod temp;

use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::mail::{InboxQuery, Outgoing, Retention, Shown, To};
use mdkg_coord_sqlite::mail_list::{ListQuery, ListState, read_list};
use mdkg_coord_sqlite::{Store, Upgrade};
use serde_json::json;

const DAY: u64 = 24 * 60 * 60 * 1000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Scratch(temp::dir(&format!("mdkg-mail-list-{name}")))
    }

    fn db(&self) -> PathBuf {
        self.0.join("mdkg").join("coord.sqlite")
    }

    fn store(&self) -> Store {
        Store::open_with(&self.db(), 1_000, Upgrade::Allow).expect("store")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn mail(id: &str, kind: &str, sender: &str, to: To, now: u64) -> Outgoing {
    Outgoing {
        message_id: id.into(),
        context_id: None,
        kind: kind.into(),
        sender: sender.into(),
        via: None,
        sender_mailboxes: vec![sender.into()],
        to,
        reference_ids: Vec::new(),
        parts: json!([{"kind": "text", "text": format!("body of {id}")}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: now,
    }
}

fn to(mailboxes: &[&str]) -> To {
    To::Mailboxes(mailboxes.iter().map(|m| m.to_string()).collect())
}

fn query(kinds: &[&str]) -> ListQuery {
    ListQuery {
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        limit: 50,
        now_ms: 10,
        ..ListQuery::default()
    }
}

fn ids(path: &Path, q: &ListQuery) -> Vec<String> {
    read_list(path, q)
        .expect("list")
        .messages
        .into_iter()
        .map(|m| m.message_id)
        .collect()
}

/// Lists `mailbox`'s mail and marks it shown, as the CLI's inbox does, so
/// the mailbox may ack it.
fn show(s: &mut Store, mailbox: &str) {
    let inbox = s
        .inbox(&InboxQuery {
            mailboxes: vec![mailbox.into()],
            all: true,
            now_ms: 5,
            ..InboxQuery::default()
        })
        .expect("inbox");
    let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
    s.mark_shown(&listed, &[mailbox.into()], 5).expect("mark");
}

fn ack(s: &mut Store, id: &str, mailbox: &str, now: u64) {
    s.ack(
        &[id.to_string()],
        &[mailbox.to_string()],
        mailbox,
        None,
        None,
        now,
    )
    .expect("ack");
}

/// Every file of the store but SQLite's shared-memory index, which a
/// reader may maintain.
fn store_bytes(dir: &Scratch) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir.0.join("mdkg"))
        .expect("store dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| !p.to_string_lossy().ends_with("-shm"))
        .map(|p| {
            let bytes = std::fs::read(&p).expect("read");
            (p.to_string_lossy().into_owned(), bytes)
        })
        .collect();
    files.sort();
    files
}

/// Names the first file and offset that differ, rather than the bytes.
/// A read-only open in WAL mode may create an empty `-wal` file where
/// none was (SQLite's bookkeeping, holding no data); nothing else may
/// appear or change.
fn assert_same(before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)]) {
    for (name, bytes) in after {
        if !before.iter().any(|(n, _)| n == name) {
            assert!(
                name.ends_with("-wal") && bytes.is_empty(),
                "{name} appeared with {} bytes",
                bytes.len()
            );
        }
    }
    for (name, a) in before {
        let b = &after
            .iter()
            .find(|(n, _)| n == name)
            .expect("still there")
            .1;
        let first = a.iter().zip(b).position(|(x, y)| x != y);
        assert!(
            a.len() == b.len() && first.is_none(),
            "{name} changed: {} to {} bytes, first difference at {first:?}",
            a.len(),
            b.len()
        );
    }
}

#[test]
fn lists_kinds_across_every_mailbox() {
    let dir = Scratch::new("kinds");
    let mut s = dir.store();
    for (id, kind, sender, rcpt) in [
        ("f1", "fault", "mdkg:faults", &["orchestrator"][..]),
        ("r1", "review_request", "lane-a", &["rev-1", "rev-2"][..]),
        ("x1", "findings", "rev-1", &["lane-a"][..]),
        ("n1", "note", "lane-b", &["lane-a"][..]),
        ("q1", "question", "lane-b", &["orchestrator"][..]),
        ("u1", "custom_kind", "lane-b", &["lane-a"][..]),
    ] {
        s.send(&mail(id, kind, sender, to(rcpt), 1)).expect("send");
    }
    drop(s);
    let db = dir.db();
    assert_eq!(ids(&db, &query(&["fault", "review_request"])), ["f1", "r1"]);
    // No kind: every kind but receipts, unknown kinds included.
    assert_eq!(ids(&db, &query(&[])), ["f1", "r1", "x1", "n1", "q1", "u1"]);
    let mut q = query(&[]);
    q.senders = vec!["lane-b".into()];
    assert_eq!(ids(&db, &q), ["n1", "q1", "u1"]);
}

#[test]
fn scope_is_by_recipient_and_a_channels_current_members() {
    let dir = Scratch::new("scope");
    let mut s = dir.store();
    s.room_create(
        "dev",
        "lane-a",
        &["lane-b".into(), "orchestrator".into()],
        0,
    )
    .expect("channel");
    s.send(&mail("p1", "question", "lane-a", To::Room("dev".into()), 1))
        .expect("post");
    s.send(&mail(
        "d1",
        "fault",
        "mdkg:faults",
        to(&["orchestrator"]),
        2,
    ))
    .expect("direct to a member");
    s.send(&mail("d2", "fault", "mdkg:faults", to(&["outsider"]), 3))
        .expect("direct to a non-member");
    s.send(&mail("d3", "fault", "mdkg:faults", to(&["lane-b"]), 4))
        .expect("direct to a member who leaves");
    let mut q = query(&[]);
    q.channels = vec!["dev".into()];
    let listing = read_list(&dir.db(), &q).expect("list");
    assert_eq!(
        listing.mailboxes,
        ["lane-a", "lane-b", "orchestrator"],
        "the scope resolves to the members"
    );
    assert_eq!(ids(&dir.db(), &q), ["p1", "d1", "d3"]);
    s.room_leave(
        "dev",
        &["lane-b".into()],
        "lane-b",
        &["lane-b".into()],
        false,
    )
    .expect("leave");
    // The post stays in scope, still unacked by the orchestrator.
    assert_eq!(ids(&dir.db(), &q), ["p1", "d1"]);
    // --to and --channel together are the union, each message once.
    q.to = vec!["outsider".into(), "orchestrator".into()];
    assert_eq!(ids(&dir.db(), &q), ["p1", "d1", "d2"]);
    let mut q = query(&[]);
    q.channels = vec!["nope".into()];
    assert!(read_list(&dir.db(), &q).is_err(), "an unknown channel");
}

#[test]
fn state_counts_only_deliveries_in_scope() {
    let dir = Scratch::new("state");
    let mut s = dir.store();
    s.send(&mail(
        "r1",
        "review_request",
        "lane-a",
        to(&["rev-1", "rev-2"]),
        1,
    ))
    .expect("send");
    show(&mut s, "rev-1");
    ack(&mut s, "r1", "rev-1", 6);
    let db = dir.db();
    let mut q = query(&["review_request"]);
    assert_eq!(ids(&db, &q), ["r1"], "rev-2 still has it");
    q.to = vec!["rev-1".into()];
    assert!(ids(&db, &q).is_empty(), "rev-1 acked it");
    q.state = ListState::Acked;
    assert_eq!(ids(&db, &q), ["r1"]);
    q.to = vec!["rev-2".into()];
    assert!(ids(&db, &q).is_empty());
    q.state = ListState::Any;
    assert_eq!(ids(&db, &q), ["r1"]);
}

#[test]
fn a_list_changes_nothing_in_the_store() {
    let dir = Scratch::new("readonly");
    let mut s = dir.store();
    s.send(&mail(
        "f1",
        "fault",
        "mdkg:faults",
        to(&["orchestrator"]),
        1,
    ))
    .expect("send");
    let mut ttl = mail("t1", "note", "lane-a", to(&["lane-b"]), 1);
    ttl.retention = Retention::Ttl(1);
    s.send(&ttl).expect("ttl");
    let mut q = query(&[]);
    q.now_ms = 3 * DAY;
    q.state = ListState::Any;
    // A writer still has the store open, its WAL not checkpointed: what a
    // running supervisor leaves. The list changes neither file.
    let before = store_bytes(&dir);
    assert!(
        before
            .iter()
            .any(|(n, b)| n.ends_with("-wal") && !b.is_empty())
    );
    read_list(&dir.db(), &q).expect("list beside a writer");
    assert_same(&before, &store_bytes(&dir));
    drop(s);
    let before = store_bytes(&dir);
    let head = mdkg_coord_sqlite::events::head(&dir.db()).expect("head");
    // Past the ttl: a purging read would delete t1's body and log it.
    let listing = read_list(&dir.db(), &q).expect("list");
    assert_eq!(listing.messages.len(), 2);
    assert_eq!(listing.event_cursor.as_deref(), Some(head.as_str()));
    assert_same(&before, &store_bytes(&dir));
    // Nothing was marked shown: the recipient still can't ack it unseen.
    let mut s = dir.store();
    let (acked, skipped) = s
        .ack_through(
            u64::MAX,
            &["fault".to_string()],
            &["orchestrator".to_string()],
            "orchestrator",
            None,
            None,
            7,
        )
        .expect("ack through");
    assert!(acked.acked.is_empty());
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].reason, "not_shown");
}

#[test]
fn due_bodies_read_as_purged_and_stay_until_a_purge() {
    let dir = Scratch::new("due");
    let mut s = dir.store();
    s.set_ack_grace_days(1);
    // A ttl body at its time.
    let mut ttl = mail("t1", "note", "lane-a", to(&["lane-b"]), 1);
    ttl.retention = Retention::Ttl(1);
    s.send(&ttl).expect("ttl");
    // A purge_on_ack body past its ack grace.
    s.send(&mail("g1", "note", "lane-a", to(&["lane-c"]), 1))
        .expect("grace");
    show(&mut s, "lane-c");
    ack(&mut s, "g1", "lane-c", 6);
    // A body held for a human, the other delivery acked, the hold over.
    s.send(&mail(
        "h1",
        "note",
        "lane-a",
        to(&["lane-d", "human:nick"]),
        1,
    ))
    .expect("hold");
    show(&mut s, "lane-d");
    ack(&mut s, "h1", "lane-d", 6);
    // Kept: unacked purge_on_ack.
    s.send(&mail("k1", "note", "lane-a", to(&["lane-e"]), 1))
        .expect("kept");
    drop(s);
    let late = 8 * DAY + 10;
    let mut q = query(&[]);
    q.state = ListState::Any;
    q.now_ms = late;
    let listing = read_list(&dir.db(), &q).expect("list");
    let bodies: Vec<(&str, bool)> = listing
        .messages
        .iter()
        .map(|m| (m.message_id.as_str(), m.parts.is_some()))
        .collect();
    assert_eq!(
        bodies,
        [("t1", false), ("g1", false), ("h1", false), ("k1", true)]
    );
    let stored = |dir: &Scratch| -> i64 {
        rusqlite::Connection::open(dir.db())
            .expect("open")
            .query_row("SELECT COUNT(*) FROM msg_bodies", [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(stored(&dir), 4, "the list deleted nothing");
    // A purging read at the same time deletes exactly the due set: the
    // list and the purge share one condition.
    let mut s = dir.store();
    s.message("k1", late).expect("purging read");
    drop(s);
    assert_eq!(stored(&dir), 1);
    assert_eq!(
        read_list(&dir.db(), &q).expect("list").messages,
        listing.messages
    );
}

#[test]
fn pages_follow_the_cursor_and_stop_under_a_mebibyte() {
    let dir = Scratch::new("pages");
    let mut s = dir.store();
    for i in 1..=5 {
        s.send(&mail(&format!("m{i}"), "question", "lane-a", to(&["o"]), i))
            .expect("send");
    }
    let mut q = query(&["question"]);
    q.limit = 2;
    let one = read_list(&dir.db(), &q).expect("page 1");
    assert_eq!(one.messages.len(), 2);
    assert!(one.more);
    q.after = one.cursor;
    let two = read_list(&dir.db(), &q).expect("page 2");
    q.after = two.cursor;
    let three = read_list(&dir.db(), &q).expect("page 3");
    let seen: Vec<String> = [one, two, three.clone()]
        .into_iter()
        .flat_map(|p| p.messages.into_iter().map(|m| m.message_id))
        .collect();
    assert_eq!(seen, ["m1", "m2", "m3", "m4", "m5"]);
    assert!(!three.more);
    q.after = three.cursor;
    let empty = read_list(&dir.db(), &q).expect("past the end");
    assert!(empty.messages.is_empty());
    assert_eq!(empty.cursor, three.cursor);

    // Bodies of 600 KB: one per page, however large the limit.
    let big = "x".repeat(600 * 1024);
    for i in 1..=2 {
        let mut m = mail(&format!("b{i}"), "findings", "rev", to(&["o"]), 10);
        m.parts = json!([{"kind": "text", "text": big}]);
        s.send(&m).expect("big");
    }
    let q = query(&["findings"]);
    let page = read_list(&dir.db(), &q).expect("big page");
    assert_eq!(page.messages.len(), 1);
    assert!(page.more);
}

#[test]
fn a_missing_store_has_no_mail() {
    let dir = Scratch::new("missing");
    let listing = read_list(&dir.db(), &query(&["fault"])).expect("list");
    assert!(listing.messages.is_empty());
    assert_eq!(listing.event_cursor, None);
    assert!(!dir.db().exists(), "a list creates no store");
}
