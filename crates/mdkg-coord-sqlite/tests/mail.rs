//! Rooms and messaging in the store: dedupe, fan-out, cursors, acks and
//! retention.

#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;

use mdkg_coord_sqlite::mail::{InboxQuery, MailError, Outgoing, Page, Retention, Shown, To};
use mdkg_coord_sqlite::{Store, Upgrade};
use serde_json::json;

/// Lists `mailbox`'s mail, acked or not, and records it as shown, which an
/// ack needs. The CLI records it once the listing is out.
fn show(s: &mut Store, mailbox: &str, now_ms: u64) -> Vec<String> {
    let inbox = s
        .inbox(&InboxQuery {
            mailboxes: vec![mailbox.into()],
            all: true,
            now_ms,
            ..InboxQuery::default()
        })
        .expect("inbox");
    let listed: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
    s.mark_shown(&listed, &[mailbox.into()], now_ms)
        .expect("mark shown");
    inbox.messages.into_iter().map(|m| m.message_id).collect()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = temp::dir(&format!("mdkg-mail-{name}"));
        Scratch(dir)
    }

    fn store(&self) -> Store {
        Store::open_with(
            &self.0.join("mdkg").join("coord.sqlite"),
            1_000,
            Upgrade::Allow,
        )
        .expect("store")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// What an older build stored: message `id` in `context`.
fn older_context(dir: &Scratch, id: &str, context: &str) {
    rusqlite::Connection::open(dir.0.join("mdkg").join("coord.sqlite"))
        .expect("open")
        .execute(
            "UPDATE msg_messages SET context_id = ?1 WHERE message_id = ?2",
            rusqlite::params![context, id],
        )
        .expect("older row");
}

fn text(t: &str) -> serde_json::Value {
    json!([{"kind": "text", "text": t}])
}

fn post(id: &str, sender: &str, to: To, body: &str, now: u64) -> Outgoing {
    Outgoing {
        message_id: id.into(),
        context_id: None,
        kind: "note".into(),
        sender: sender.into(),
        via: None,
        sender_mailboxes: vec![sender.into()],
        to,
        reference_ids: Vec::new(),
        parts: text(body),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: now,
    }
}

fn inbox(store: &mut Store, mailbox: &str, after: Option<u64>) -> Vec<String> {
    store
        .inbox(&InboxQuery {
            mailboxes: vec![mailbox.into()],
            after,
            ..InboxQuery::default()
        })
        .expect("inbox")
        .messages
        .into_iter()
        .map(|m| m.message_id)
        .collect()
}

#[test]
fn a_room_post_fans_out_to_every_member_but_the_sender() {
    let dir = Scratch::new("fanout");
    let mut s = dir.store();
    s.room_create("review", "a", &["b".into(), "role:reviewer".into()], 0)
        .unwrap();
    let (m, dup) = s
        .send(&post("m1", "a", To::Room("review".into()), "hi", 1))
        .unwrap();
    assert!(!dup);
    assert_eq!(m.context_id, "room:review");
    assert_eq!(m.to(), vec!["b".to_string(), "role:reviewer".to_string()]);
    assert_eq!(inbox(&mut s, "b", None), vec!["m1"]);
    assert_eq!(inbox(&mut s, "role:reviewer", None), vec!["m1"]);
    assert!(inbox(&mut s, "a", None).is_empty(), "not to the sender");

    // A non-member can't post; an unknown room is named.
    assert!(matches!(
        s.send(&post("m2", "x", To::Room("review".into()), "hi", 2)),
        Err(MailError::NotMember { .. })
    ));
    assert!(matches!(
        s.send(&post("m3", "a", To::Room("nope".into()), "hi", 2)),
        Err(MailError::UnknownRoom(_))
    ));
}

#[test]
fn the_message_id_dedupes_and_is_never_reused() {
    let dir = Scratch::new("dedupe");
    let mut s = dir.store();
    let to = || To::Mailboxes(vec!["b".into()]);
    let (first, dup) = s.send(&post("m1", "a", to(), "hi", 1)).unwrap();
    assert!(!dup);
    let (again, dup) = s.send(&post("m1", "a", to(), "hi", 9)).unwrap();
    assert!(dup, "a retry stores nothing new");
    assert_eq!(again.seq, first.seq);
    assert_eq!(inbox(&mut s, "b", None).len(), 1);
    assert!(matches!(
        s.send(&post("m1", "a", to(), "changed", 2)),
        Err(MailError::IdReused(_))
    ));
    assert!(matches!(
        s.send(&post("m1", "c", to(), "hi", 2)),
        Err(MailError::IdReused(_))
    ));
}

#[test]
fn the_cursor_skips_mail_already_seen_and_acks_purge_bodies() {
    let dir = Scratch::new("cursor");
    let mut s = dir.store();
    let to = || To::Mailboxes(vec!["b".into(), "c".into()]);
    s.send(&post("m1", "a", to(), "one", 1)).unwrap();
    let first = s
        .inbox(&InboxQuery {
            mailboxes: vec!["b".into()],
            ..InboxQuery::default()
        })
        .unwrap();
    assert_eq!(first.cursor, 1);
    s.send(&post("m2", "a", to(), "two", 2)).unwrap();
    assert_eq!(inbox(&mut s, "b", Some(first.cursor)), vec!["m2"]);

    // b acks m1: c hasn't, so the body stays.
    show(&mut s, "b", 3);
    let acked = s
        .ack(&["m1".into()], &["b".into()], "b", None, Some("seen"), 3)
        .unwrap();
    assert_eq!(acked.acked, vec![("m1".to_string(), "b".to_string())]);
    assert!(acked.purged.is_empty());
    assert_eq!(
        inbox(&mut s, "b", None),
        vec!["m2"],
        "acked mail leaves the inbox"
    );
    // c acks too: every delivery is acked, so the body goes; the record stays.
    show(&mut s, "c", 4);
    let acked = s
        .ack(&["m1".into()], &["c".into()], "c", None, None, 4)
        .unwrap();
    assert_eq!(acked.purged, vec!["m1".to_string()]);
    let m1 = s.message("m1", 5).unwrap();
    assert_eq!(m1.parts, None);
    assert!(m1.digest.starts_with("sha256:"));
    assert_eq!(m1.deliveries[0].outcome.as_deref(), Some("seen"));
    // Acking again is a no-op; a mailbox that got nothing is refused.
    s.ack(&["m1".into()], &["c".into()], "c", None, None, 6)
        .unwrap();
    assert!(matches!(
        s.ack(&["m1".into()], &["z".into()], "z", None, None, 6),
        Err(MailError::NotHolder(_))
    ));
}

#[test]
fn ttl_bodies_go_on_time_and_durable_ones_stay() {
    let dir = Scratch::new("ttl");
    let mut s = dir.store();
    let day = 24 * 60 * 60 * 1000;
    let mut ttl = post("t", "a", To::Mailboxes(vec!["b".into()]), "x", 0);
    ttl.retention = Retention::Ttl(1);
    s.send(&ttl).unwrap();
    let mut durable = post("d", "a", To::Mailboxes(vec!["b".into()]), "y", 0);
    durable.retention = Retention::Durable;
    s.send(&durable).unwrap();
    show(&mut s, "b", 1);
    s.ack(&["d".into()], &["b".into()], "b", None, None, 1)
        .unwrap();
    assert!(s.message("t", day - 1).unwrap().parts.is_some());
    assert!(s.message("t", day).unwrap().parts.is_none(), "a day later");
    assert!(s.message("d", 10 * day).unwrap().parts.is_some(), "durable");
}

#[test]
fn replies_share_the_context_and_threads_list_it_in_order() {
    let dir = Scratch::new("thread");
    let mut s = dir.store();
    s.send(&post("q", "a", To::Mailboxes(vec!["b".into()]), "?", 1))
        .unwrap();
    let mut reply = post("r", "b", To::Mailboxes(vec!["a".into()]), "!", 2);
    reply.reference_ids = vec!["q".into()];
    let (r, _) = s.send(&reply).unwrap();
    assert_eq!(r.context_id, "q");
    let (context, messages) = s.thread("r", Page::default(), 3).unwrap();
    assert_eq!(context, "q");
    let ids: Vec<_> = messages.iter().map(|m| m.message_id.as_str()).collect();
    assert_eq!(ids, vec!["q", "r"]);
    assert!(matches!(
        s.thread("nothing", Page::default(), 3),
        Err(MailError::UnknownContext(_))
    ));
}

#[test]
fn an_import_reaches_the_room_but_not_its_writer() {
    let dir = Scratch::new("import");
    let mut s = dir.store();
    s.room_create("review", "lane", &["orchestrator".into()], 0)
        .unwrap();
    let mut findings = post("f", "codex:run-1", To::Room("review".into()), "{}", 1);
    findings.via = Some("lane".into());
    let (m, _) = s.send(&findings).unwrap();
    assert_eq!(m.to(), vec!["orchestrator".to_string()]);
    assert_eq!(m.via.as_deref(), Some("lane"));
}

#[test]
fn rooms_list_their_members_and_leaving_keeps_deliveries() {
    let dir = Scratch::new("rooms");
    let mut s = dir.store();
    s.room_create("r", "a", &[], 0).unwrap();
    assert!(matches!(
        s.room_create("r", "b", &[], 1),
        Err(MailError::RoomExists(_))
    ));
    s.room_join("r", &["b".into(), "c".into()], "a", &["a".into()], 2)
        .unwrap();
    s.send(&post("m", "a", To::Room("r".into()), "x", 3))
        .unwrap();
    let (room, absent) = s
        .room_leave("r", &["c".into()], "a", &["a".into()], false)
        .unwrap();
    assert!(absent.is_empty());
    assert_eq!(room.members, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        inbox(&mut s, "c", None),
        vec!["m"],
        "delivered before leaving"
    );
    assert_eq!(s.rooms().unwrap().len(), 1);
    assert_eq!(
        s.unacked(&["b".into(), "z".into()]).unwrap(),
        vec![("b".to_string(), 1)]
    );
}

#[test]
fn mail_never_touches_the_lease_journal() {
    let dir = Scratch::new("journal");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    s.send(&post("m", "a", To::Room("r".into()), "x", 1))
        .unwrap();
    assert!(s.journal().unwrap().is_empty());
    s.verify().unwrap();
}

#[test]
fn a_retry_must_ask_for_the_same_thing() {
    let dir = Scratch::new("request");
    let mut s = dir.store();
    s.send(&post("d1", "a", To::Mailboxes(vec!["b".into()]), "x", 1))
        .unwrap();
    // Same id and body, another destination or kind: refused, not dropped.
    assert!(matches!(
        s.send(&post("d1", "a", To::Mailboxes(vec!["c".into()]), "x", 2)),
        Err(MailError::IdReused(_))
    ));
    let mut other_kind = post("d1", "a", To::Mailboxes(vec!["b".into()]), "x", 2);
    other_kind.kind = "question".into();
    assert!(matches!(s.send(&other_kind), Err(MailError::IdReused(_))));
}

#[test]
fn a_role_holder_posts_to_a_room_its_role_is_in() {
    let dir = Scratch::new("rolepost");
    let mut s = dir.store();
    s.room_create("work", "a", &["role:reviewer".into()], 0)
        .unwrap();
    let mut reply = post("f", "b", To::Room("work".into()), "done", 1);
    reply.sender_mailboxes = vec!["b".into(), "role:reviewer".into()];
    let (m, _) = s.send(&reply).unwrap();
    assert!(m.to().contains(&"a".to_string()));
    assert!(
        !m.to().contains(&"role:reviewer".to_string()),
        "a role the sender holds gets no copy of its own post (D1)"
    );
}

/// D2 (orchestrator, 2026-10-21): a `purge_on_ack` body waits for a human
/// recipient up to the hold, then goes once everyone else has acked; the
/// human's own ack purges it at once.
#[test]
fn a_body_waits_for_a_human_until_the_hold_ends() {
    let dir = Scratch::new("human-hold");
    let mut s = dir.store();
    let day = 24 * 60 * 60 * 1000;
    let to = || To::Mailboxes(vec!["b".into(), "human:nick".into()]);
    s.send(&post("h1", "a", to(), "one", 0)).unwrap();
    s.send(&post("h2", "a", to(), "two", 0)).unwrap();
    show(&mut s, "b", 1);
    show(&mut s, "human:nick", 1);
    let acked = s
        .ack(
            &["h1".into(), "h2".into()],
            &["b".into()],
            "b",
            None,
            None,
            1,
        )
        .unwrap();
    assert!(acked.purged.is_empty(), "the human hasn't acked");
    let acked = s
        .ack(
            &["h1".into()],
            &["human:nick".into()],
            "human:nick",
            None,
            None,
            2,
        )
        .unwrap();
    assert_eq!(acked.purged, vec!["h1".to_string()]);
    assert!(s.message("h2", 7 * day - 1).unwrap().parts.is_some());
    assert!(
        s.message("h2", 7 * day).unwrap().parts.is_none(),
        "hold over"
    );

    // Until every agent acks, the hold doesn't purge either.
    s.send(&post("h3", "a", to(), "three", 0)).unwrap();
    assert!(s.message("h3", 30 * day).unwrap().parts.is_some());
}

/// D4, refined (orchestrator, 2026-10-21): a reply keeps the replied-to
/// conversation only when it goes to the same place; a direct reply to a
/// room post starts its own conversation, pointing at the post.
#[test]
fn a_reply_keeps_the_context_only_in_the_same_place() {
    let dir = Scratch::new("reply-place");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    s.room_create("q", "a", &["b".into()], 0).unwrap();
    s.send(&post("p", "a", To::Room("r".into()), "post", 1))
        .unwrap();

    let mut same_room = post("p2", "b", To::Room("r".into()), "re", 2);
    same_room.reference_ids = vec!["p".into()];
    assert_eq!(s.send(&same_room).unwrap().0.context_id, "room:r");

    let mut other_room = post("p3", "b", To::Room("q".into()), "re", 3);
    other_room.reference_ids = vec!["p".into()];
    assert_eq!(s.send(&other_room).unwrap().0.context_id, "room:q");

    let mut direct = post("d", "b", To::Mailboxes(vec!["a".into()]), "re", 4);
    direct.reference_ids = vec!["p".into()];
    let (d, _) = s.send(&direct).unwrap();
    assert_eq!(d.context_id, "d", "its own conversation");
    assert_eq!(d.reference_ids, vec!["p".to_string()]);

    // A room post answering a direct message doesn't take its context.
    let mut back = post("p4", "a", To::Room("r".into()), "re", 5);
    back.reference_ids = vec!["d".into()];
    assert_eq!(s.send(&back).unwrap().0.context_id, "room:r");
}

/// D5 (orchestrator, 2026-10-21): rooms are open to join, only members add
/// or remove others, and only the human removes a `human:*` member.
#[test]
fn only_members_add_or_remove_others_and_humans_leave_themselves() {
    let dir = Scratch::new("room-authority");
    let mut s = dir.store();
    s.room_create("r", "a", &["human:nick".into()], 0).unwrap();
    s.room_join("r", &["z".into()], "z", &["z".into()], 1)
        .unwrap();
    assert!(matches!(
        s.room_join("r", &["y".into()], "x", &["x".into()], 2),
        Err(MailError::NotMember { actor, .. }) if actor == "x"
    ));
    assert!(matches!(
        s.room_leave("r", &["z".into()], "x", &["x".into()], false),
        Err(MailError::NotMember { actor, .. }) if actor == "x"
    ));
    assert!(matches!(
        s.room_leave("r", &["human:nick".into()], "a", &["a".into()], false),
        Err(MailError::HumanMember(m)) if m == "human:nick"
    ));
    let (room, _) = s
        .room_leave("r", &["z".into()], "a", &["a".into()], false)
        .unwrap();
    assert_eq!(
        room.members,
        vec!["a".to_string(), "human:nick".to_string()]
    );
    let (room, _) = s
        .room_leave(
            "r",
            &["human:nick".into()],
            "human:nick",
            &["human:nick".into()],
            false,
        )
        .unwrap();
    assert_eq!(room.members, vec!["a".to_string()]);
}

#[test]
fn an_unknown_mail_schema_is_refused() {
    let dir = Scratch::new("msgschema");
    drop(dir.store());
    rusqlite::Connection::open(dir.0.join("mdkg").join("coord.sqlite"))
        .unwrap()
        .execute("UPDATE meta SET value = '9' WHERE key = 'msg_schema'", [])
        .unwrap();
    assert!(matches!(
        Store::open(&dir.0.join("mdkg").join("coord.sqlite"), 1_000),
        Err(mdkg_coord_sqlite::StoreError::Schema(v)) if v == "msg 9"
    ));
}

#[test]
fn mail_tables_from_schema_1_gain_the_request_digest() {
    // Both development builds left "1" stores: a76dd2c without the column,
    // f028e82 with it. Either way old rows keep working, and an old id
    // can't be retried.
    for drop_column in [true, false] {
        let dir = Scratch::new(if drop_column {
            "msgupgrade-a"
        } else {
            "msgupgrade-b"
        });
        let db = dir.0.join("mdkg").join("coord.sqlite");
        {
            let mut s = dir.store();
            s.send(&post("old", "a", To::Mailboxes(vec!["b".into()]), "x", 1))
                .expect("seed");
        }
        {
            let conn = rusqlite::Connection::open(&db).expect("open");
            if drop_column {
                conn.execute_batch(
                    "ALTER TABLE msg_messages DROP COLUMN request_digest;
                     ALTER TABLE msg_deliveries DROP COLUMN acked_via;",
                )
                .expect("drop");
            } else {
                conn.execute("UPDATE msg_messages SET request_digest = 'sha256:old'", [])
                    .expect("old digest");
            }
            conn.execute("UPDATE meta SET value = '1' WHERE key = 'msg_schema'", [])
                .expect("downgrade");
        }
        let mut s = dir.store();
        assert_eq!(
            s.message("old", 2).expect("old row").message_id,
            "old",
            "old rows survive"
        );
        assert!(matches!(
            s.send(&post("old", "a", To::Mailboxes(vec!["b".into()]), "x", 3)),
            Err(MailError::IdReused(_))
        ));
        s.send(&post("new", "a", To::Mailboxes(vec!["b".into()]), "x", 4))
            .expect("send after the upgrade");
        let version: String = rusqlite::Connection::open(&db)
            .expect("open")
            .query_row("SELECT value FROM meta WHERE key = 'msg_schema'", [], |r| {
                r.get(0)
            })
            .expect("version");
        assert_eq!(version, mdkg_coord_sqlite::mail::MAIL_SCHEMA_VERSION);
    }
}

/// Round 1 answers on 585b125 (orchestrator, 2026-10-21): a "2" store
/// gains `acked_via`, and a human's ack records the worktree's actor.
#[test]
fn mail_tables_from_schema_2_gain_acked_via() {
    let dir = Scratch::new("msgupgrade-via");
    let db = dir.0.join("mdkg").join("coord.sqlite");
    {
        let mut s = dir.store();
        s.send(&post(
            "old",
            "a",
            To::Mailboxes(vec!["human:nick".into()]),
            "x",
            1,
        ))
        .expect("seed");
    }
    {
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch(
            "ALTER TABLE msg_deliveries DROP COLUMN acked_via;
             UPDATE meta SET value = '2' WHERE key = 'msg_schema';",
        )
        .expect("downgrade");
    }
    let mut s = dir.store();
    // A delivery from before mail schema 4 has no shown time: listed first.
    show(&mut s, "human:nick", 2);
    s.ack(
        &["old".into()],
        &["human:nick".into()],
        "human:nick",
        Some("wt-a"),
        None,
        2,
    )
    .expect("ack");
    let m = s.message("old", 3).expect("message");
    assert_eq!(m.deliveries[0].acked_via.as_deref(), Some("wt-a"));
}

/// Round 2 of 585b125: a reply keeps an explicit context in the same room,
/// and a direct reply never joins a conversation that holds a room post,
/// whatever its context is called.
#[test]
fn explicit_contexts_follow_the_same_place_rule() {
    let dir = Scratch::new("explicit-context");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    let mut p = post("p", "a", To::Room("r".into()), "post", 1);
    p.context_id = Some("task-18".into());
    s.send(&p).unwrap();
    let mut same = post("p2", "b", To::Room("r".into()), "re", 2);
    same.reference_ids = vec!["p".into()];
    assert_eq!(s.send(&same).unwrap().0.context_id, "task-18");

    // A direct message can't join it (task-36, B1)...
    let mut old = post("d1", "b", To::Mailboxes(vec!["a".into()]), "re", 3);
    old.context_id = Some("task-18".into());
    assert!(matches!(s.send(&old), Err(MailError::NotInConversation)));
    // ...but an older build's direct reply did.
    old.context_id = None;
    s.send(&old).unwrap();
    older_context(&dir, "d1", "task-18");
    let mut direct = post("d2", "a", To::Mailboxes(vec!["b".into()]), "re", 4);
    direct.reference_ids = vec!["d1".into()];
    assert_eq!(s.send(&direct).unwrap().0.context_id, "d2");
}

/// Review round 8 of a76dd2c: a self-leave is decided inside the leave's
/// transaction, so a second (or overlapping) one reports the mailbox as
/// absent instead of failing; an explicit leave of a non-member fails.
#[test]
fn leaving_again_is_absent_not_an_error() {
    let dir = Scratch::new("leave-again");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    let (_, absent) = s
        .room_leave("r", &["a".into()], "a", &["a".into()], true)
        .unwrap();
    assert!(absent.is_empty());
    let (room, absent) = s
        .room_leave("r", &["a".into()], "a", &["a".into()], true)
        .unwrap();
    assert_eq!(absent, vec!["a".to_string()]);
    assert_eq!(room.members, vec!["b".to_string()]);
    assert!(matches!(
        s.room_leave("r", &["a".into()], "a", &["a".into()], false),
        Err(MailError::Absent { .. })
    ));
}

/// Review of 585b125: a member through a role it holds counts as a member
/// for room authority, and leaves or joins as that role.
#[test]
fn a_role_holder_is_a_member_through_its_role() {
    let dir = Scratch::new("role-member");
    let mut s = dir.store();
    s.room_create("work", "a", &["role:reviewer".into()], 0)
        .unwrap();
    let b = vec!["b".to_string(), "role:reviewer".to_string()];
    s.room_join("work", &["c".into()], "b", &b, 1).unwrap();
    let (room, _) = s
        .room_leave("work", &["role:reviewer".into()], "b", &b, false)
        .unwrap();
    assert_eq!(room.members, vec!["a".to_string(), "c".to_string()]);
    s.room_join("work", &["role:reviewer".into()], "b", &b, 2)
        .unwrap();
    // Removing a typo is "absent", checked before the human rule.
    assert!(matches!(
        s.room_leave("work", &["human:typo".into()], "a", &["a".into()], false),
        Err(MailError::Absent { mailbox, .. }) if mailbox == "human:typo"
    ));
}

/// Review of 585b125: the ack that ends a human's hold reports the purge,
/// and a direct reply never inherits a `room:` context, even from a row an
/// older build wrote.
#[test]
fn a_late_ack_reports_the_purge_and_direct_replies_leave_rooms() {
    let dir = Scratch::new("late-ack");
    let mut s = dir.store();
    let day = 24 * 60 * 60 * 1000;
    let to = To::Mailboxes(vec!["b".into(), "human:nick".into()]);
    s.send(&post("h", "a", to, "x", 0)).unwrap();
    show(&mut s, "b", 1);
    let acked = s
        .ack(&["h".into()], &["b".into()], "b", None, None, 8 * day)
        .unwrap();
    assert_eq!(acked.purged, vec!["h".to_string()]);

    s.send(&post("o", "a", To::Mailboxes(vec!["b".into()]), "x", 1))
        .unwrap();
    older_context(&dir, "o", "room:work");
    let mut reply = post("r", "b", To::Mailboxes(vec!["a".into()]), "re", 2);
    reply.reference_ids = vec!["o".into()];
    assert_eq!(s.send(&reply).unwrap().0.context_id, "r");
}

/// Round 3 of 585b125: the purge treats only lowercase `human:` as a human,
/// as `is_human` does, so an older build's `Human:y` delivery is an agent's
/// and the hold never purges past it.
#[test]
fn only_lowercase_human_mailboxes_are_held() {
    let dir = Scratch::new("human-case");
    let mut s = dir.store();
    let day = 24 * 60 * 60 * 1000;
    let to = To::Mailboxes(vec!["b".into(), "human:x".into(), "Human:y".into()]);
    s.send(&post("m", "a", to, "x", 0)).unwrap();
    show(&mut s, "b", 1);
    s.ack(&["m".into()], &["b".into()], "b", None, None, 1)
        .unwrap();
    assert!(
        s.message("m", 8 * day).unwrap().parts.is_some(),
        "Human:y hasn't acked"
    );
    // An ack after the hold doesn't purge past Human:y either.
    s.send(&post(
        "n",
        "a",
        To::Mailboxes(vec!["b".into(), "human:x".into(), "Human:y".into()]),
        "x",
        0,
    ))
    .unwrap();
    show(&mut s, "b", 1);
    let acked = s
        .ack(&["n".into()], &["b".into()], "b", None, None, 8 * day)
        .unwrap();
    assert!(acked.purged.is_empty());
    assert!(s.message("n", 8 * day).unwrap().parts.is_some());
}

/// Room paging: after a cursor, the oldest N or the newest N, oldest first;
/// a context thread pages the same way, and an empty page isn't "unknown".
#[test]
fn rooms_and_threads_page() {
    let dir = Scratch::new("page");
    let mut s = dir.store();
    s.room_create("r", "a", &["b".into()], 0).unwrap();
    let mut seqs = Vec::new();
    for id in ["p1", "p2", "p3", "p4"] {
        seqs.push(
            s.send(&post(id, "a", To::Room("r".into()), "x", 1))
                .unwrap()
                .0
                .seq,
        );
    }
    let ids = |m: Vec<mdkg_coord_sqlite::mail::Message>| {
        m.into_iter().map(|m| m.message_id).collect::<Vec<_>>()
    };
    let page = |after, limit, newest| Page {
        after,
        limit,
        newest,
    };
    assert_eq!(
        ids(s.room_messages("r", page(None, Some(2), false), 2).unwrap()),
        ["p1", "p2"]
    );
    assert_eq!(
        ids(s
            .room_messages("r", page(Some(seqs[1]), Some(2), false), 2)
            .unwrap()),
        ["p3", "p4"]
    );
    assert_eq!(
        ids(s.room_messages("r", page(None, Some(2), true), 2).unwrap()),
        ["p3", "p4"]
    );
    assert!(
        s.room_messages("r", page(Some(seqs[3]), None, false), 2)
            .unwrap()
            .is_empty()
    );
    let (_, m) = s.thread("p1", page(Some(seqs[2]), None, false), 2).unwrap();
    assert_eq!(ids(m), ["p4"]);
    let (_, m) = s.thread("p1", page(Some(seqs[3]), None, false), 2).unwrap();
    assert!(m.is_empty(), "an empty page, not an unknown context");
}

/// Review of 8d0596c: a handle opened before another build moved the mail
/// tables on writes nothing, and a damaged version reads as damage, not as
/// a newer build.
#[test]
fn mail_refuses_a_mail_schema_changed_since_it_opened() {
    use mdkg_coord_sqlite::StoreError;
    let dir = Scratch::new("msg-schema-moved");
    let db = dir.0.join("mdkg").join("coord.sqlite");
    let mut store = dir.store();
    let set = |v: &str| {
        rusqlite::Connection::open(&db)
            .expect("open")
            .execute("UPDATE meta SET value = ?1 WHERE key = 'msg_schema'", [v])
            .expect("update");
    };
    set("999");
    let to = To::Mailboxes(vec!["b".into()]);
    let sent = store.send(&post("m-1", "a", to.clone(), "hello", 1));
    assert!(
        matches!(sent, Err(MailError::Store(StoreError::Schema(_)))),
        "{sent:?}"
    );
    assert!(store.rooms().is_err());
    set("0");
    let sent = store.send(&post("m-1", "a", to, "hello", 1));
    assert!(
        matches!(sent, Err(MailError::Store(StoreError::BadMeta(_)))),
        "{sent:?}"
    );
    drop(store);
    let reopened = Store::open(&db, 1_000);
    assert!(
        matches!(reopened, Err(StoreError::BadMeta(_))),
        "{reopened:?}"
    );
}

/// Audit task-36, B1: a reply or a named context joins only a conversation
/// the sender is part of, so nobody posts into another's thread with a
/// human; room posts and direct messages stay apart.
#[test]
fn only_a_conversation_youre_part_of() {
    let dir = Scratch::new("hijack");
    let mut s = dir.store();
    s.send(&post(
        "q",
        "a",
        To::Mailboxes(vec!["human:nick".into()]),
        "merge?",
        1,
    ))
    .unwrap();
    s.room_create("r", "x", &["a".into()], 2).unwrap();
    let mut reply = post("x1", "x", To::Mailboxes(vec!["a".into()]), "yes", 3);
    reply.reference_ids = vec!["q".into()];
    assert!(matches!(s.send(&reply), Err(MailError::NotInConversation)));
    let mut named = post("x2", "x", To::Mailboxes(vec!["a".into()]), "yes", 3);
    named.context_id = Some("q".into());
    assert!(matches!(s.send(&named), Err(MailError::NotInConversation)));
    let mut in_room = post("x3", "x", To::Room("r".into()), "yes", 3);
    in_room.context_id = Some("q".into());
    assert!(matches!(
        s.send(&in_room),
        Err(MailError::NotInConversation)
    ));
    // Its sender and its recipient are part of it.
    let mut answer = post(
        "h1",
        "human:nick",
        To::Mailboxes(vec!["a".into()]),
        "yes",
        4,
    );
    answer.reference_ids = vec!["q".into()];
    assert_eq!(s.send(&answer).unwrap().0.context_id, "q");
    let mut more = post("a1", "a", To::Mailboxes(vec!["human:nick".into()]), "ok", 5);
    more.context_id = Some("q".into());
    assert_eq!(s.send(&more).unwrap().0.context_id, "q");
    // So is a role's holder, for mail to the role, and a room's member, for
    // posts from before it joined.
    s.send(&post(
        "w",
        "a",
        To::Mailboxes(vec!["role:rev".into()]),
        "review",
        6,
    ))
    .unwrap();
    let mut by_role = post("b1", "b", To::Mailboxes(vec!["a".into()]), "done", 7);
    by_role.reference_ids = vec!["w".into()];
    by_role.sender_mailboxes = vec!["role:rev".into()];
    assert_eq!(s.send(&by_role).unwrap().0.context_id, "w");
    s.send(&post("p", "x", To::Room("r".into()), "post", 8))
        .unwrap();
    s.room_join("r", &["c".into()], "x", &["x".into()], 9)
        .unwrap();
    let mut late = post("c1", "c", To::Room("r".into()), "re", 10);
    late.reference_ids = vec!["p".into()];
    assert_eq!(s.send(&late).unwrap().0.context_id, "room:r");
    // A new context is anyone's to start.
    let mut fresh = post("x4", "x", To::Mailboxes(vec!["a".into()]), "hi", 11);
    fresh.context_id = Some("task-9".into());
    assert_eq!(s.send(&fresh).unwrap().0.context_id, "task-9");
}

/// Review round 1 of audit-core (H1): a new message's id names its own
/// conversation, so an id someone already used as a context is taken, and
/// never joins theirs or mixes a direct message into a room's.
#[test]
fn an_id_naming_a_used_context_is_taken() {
    let dir = Scratch::new("id-context");
    let mut s = dir.store();
    let mut q = post(
        "q1",
        "a",
        To::Mailboxes(vec!["human:nick".into()]),
        "merge?",
        1,
    );
    q.context_id = Some("task-18".into());
    s.send(&q).unwrap();
    let squat = post("task-18", "x", To::Mailboxes(vec!["a".into()]), "yes", 2);
    assert!(matches!(s.send(&squat), Err(MailError::IdReused(_))));
    s.room_create("r", "a", &["b".into()], 3).unwrap();
    let mut p = post("p", "a", To::Room("r".into()), "post", 4);
    p.context_id = Some("zz".into());
    s.send(&p).unwrap();
    let mixed = post("zz", "z", To::Mailboxes(vec!["b".into()]), "aside", 5);
    assert!(matches!(s.send(&mixed), Err(MailError::IdReused(_))));
    assert_eq!(s.thread("task-18", Page::default(), 6).unwrap().1.len(), 1);
}

/// Review round 2 of audit-core: ids and contexts are one namespace, so a
/// reply's or a room post's id can't take a conversation's name, and a
/// named context can't be another conversation's message.
#[test]
fn ids_and_contexts_are_one_namespace() {
    let dir = Scratch::new("namespace");
    let mut s = dir.store();
    let mut q = post("q1", "a", To::Mailboxes(vec!["human:nick".into()]), "?", 1);
    q.context_id = Some("task-18".into());
    s.send(&q).unwrap();
    s.room_create("ops", "x", &["a".into()], 2).unwrap();
    s.send(&post("x0", "x", To::Mailboxes(vec!["a".into()]), "hi", 3))
        .unwrap();
    let mut reply = post("task-18", "x", To::Mailboxes(vec!["a".into()]), "yes", 4);
    reply.reference_ids = vec!["x0".into()];
    assert!(matches!(s.send(&reply), Err(MailError::IdReused(_))));
    let in_room = post("task-18", "x", To::Room("ops".into()), "yes", 5);
    assert!(matches!(s.send(&in_room), Err(MailError::IdReused(_))));
    // Squatting first: the context named later is another conversation's
    // message.
    s.send(&post("task-21", "x", To::Room("ops".into()), "yes", 6))
        .unwrap();
    let mut later = post("q2", "a", To::Mailboxes(vec!["human:nick".into()]), "?", 7);
    later.context_id = Some("task-21".into());
    assert!(matches!(s.send(&later), Err(MailError::NotInConversation)));
    assert_eq!(s.thread("task-18", Page::default(), 8).unwrap().1.len(), 1);
}

#[test]
fn a_duplicate_send_keeps_the_purge_it_ran() {
    // A ttl body past its time is purged by the next send, and a send that
    // is a duplicate commits that purge too (RC1 O6): it used to roll back.
    let dir = Scratch::new("dup-purge");
    let mut s = dir.store();
    let day = 24 * 60 * 60 * 1000;
    let mut ttl = post("t", "a", To::Mailboxes(vec!["b".into()]), "x", 0);
    ttl.retention = Retention::Ttl(1);
    s.send(&ttl).unwrap();
    let mut again = ttl.clone();
    again.now_ms = day + 1;
    let (_, duplicate) = s.send(&again).unwrap();
    assert!(duplicate, "the same id and request");
    drop(s);
    let db = rusqlite::Connection::open(dir.0.join("mdkg").join("coord.sqlite")).unwrap();
    let left: i64 = db
        .query_row(
            "SELECT count(*) FROM msg_bodies b JOIN msg_messages m ON m.seq = b.seq
             WHERE m.message_id = 't'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "the body is gone from the store, not just hidden");
}

/// An ack never purges a message the recipient hasn't been shown.
/// Listed by inbox, then a new message arrives, then every id is acked:
/// the new one is refused, nothing in
/// that ack is acked, and its body stays; listed, it acks.
#[test]
fn an_ack_never_purges_mail_its_mailbox_was_never_shown() {
    let dir = Scratch::new("not-shown");
    let mut s = dir.store();
    let to = || To::Mailboxes(vec!["b".into()]);
    s.send(&post("seen", "a", to(), "one", 1)).unwrap();
    assert_eq!(show(&mut s, "b", 1), vec!["seen"]);
    s.send(&post("new", "a", to(), "two", 2)).unwrap();
    let refused = s.ack(
        &["seen".into(), "new".into()],
        &["b".into()],
        "b",
        None,
        None,
        3,
    );
    assert!(
        matches!(&refused, Err(MailError::NotShown(id)) if id == "new"),
        "{refused:?}"
    );
    for id in ["seen", "new"] {
        let m = s.message(id, 3).unwrap();
        assert!(m.parts.is_some(), "{id}'s body stays");
        assert_eq!(m.deliveries[0].state, "delivered", "{id} isn't acked");
    }
    // The seen one alone acks; once listed, the new one does too.
    let acked = s
        .ack(&["seen".into()], &["b".into()], "b", None, None, 4)
        .unwrap();
    assert_eq!(acked.purged, vec!["seen".to_string()]);
    assert_eq!(
        inbox(&mut s, "b", None),
        vec!["new"],
        "a bare listing records nothing"
    );
    assert!(matches!(
        s.ack(&["new".into()], &["b".into()], "b", None, None, 4),
        Err(MailError::NotShown(_))
    ));
    assert_eq!(show(&mut s, "b", 4), vec!["seen", "new"]);
    let acked = s
        .ack(&["new".into()], &["b".into()], "b", None, None, 5)
        .unwrap();
    assert_eq!(acked.purged, vec!["new".to_string()]);
}

/// What a thread read lists can be recorded as shown to the reader's
/// mailboxes, and only to them.
#[test]
fn a_thread_read_shows_its_messages_to_the_reader() {
    let dir = Scratch::new("shown-thread");
    let mut s = dir.store();
    s.send(&post(
        "q",
        "a",
        To::Mailboxes(vec!["b".into(), "c".into()]),
        "?",
        1,
    ))
    .unwrap();
    let (_, messages) = s.thread("q", Page::default(), 2).unwrap();
    let listed: Vec<Shown> = messages.iter().map(Shown::of).collect();
    s.mark_shown(&listed, &["b".into()], 2).unwrap();
    s.ack(&["q".into()], &["b".into()], "b", None, None, 3)
        .unwrap();
    // c read nothing: refused.
    assert!(matches!(
        s.ack(&["q".into()], &["c".into()], "c", None, None, 3),
        Err(MailError::NotShown(_))
    ));
}

/// A mark names the message as listed (number, id and digest). A restore
/// between a listing and its mark can hand the same number and id to a
/// message with other content: that one is left unshown.
#[test]
fn a_mark_after_a_restore_leaves_a_different_message_unshown() {
    let dir = Scratch::new("shown-restore");
    let db = dir.0.join("mdkg").join("coord.sqlite");
    let snapshot = dir.0.join("snapshot.sqlite");
    drop(dir.store());
    std::fs::copy(&db, &snapshot).expect("snapshot");
    let to = || To::Mailboxes(vec!["b".into()]);
    let listed = {
        let mut s = dir.store();
        s.send(&post("m", "a", to(), "the first", 1)).unwrap();
        let inbox = s
            .inbox(&InboxQuery {
                mailboxes: vec!["b".into()],
                now_ms: 2,
                ..InboxQuery::default()
            })
            .unwrap();
        inbox.messages.iter().map(Shown::of).collect::<Vec<_>>()
    };
    // Restored before the mark: the same id, other content, the same number.
    for suffix in ["-wal", "-shm"] {
        let side = dir.0.join("mdkg").join(format!("coord.sqlite{suffix}"));
        let _ = std::fs::remove_file(side);
    }
    std::fs::copy(&snapshot, &db).expect("restore");
    let mut s = dir.store();
    let (m, _) = s.send(&post("m", "a", to(), "another", 3)).unwrap();
    assert_eq!((m.seq, m.message_id.as_str()), (listed[0].seq, "m"));
    s.mark_shown(&listed, &["b".into()], 4).unwrap();
    assert!(matches!(
        s.ack(&["m".into()], &["b".into()], "b", None, None, 5),
        Err(MailError::NotShown(_))
    ));
    s.mark_shown(&[Shown::of(&m)], &["b".into()], 6).unwrap();
    s.ack(&["m".into()], &["b".into()], "b", None, None, 7)
        .unwrap();
}
