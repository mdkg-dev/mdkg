//! Faults in the mail store (a-flow A26): numbered copies, the direct
//! dedupe across mailboxes, a resend that stops at the store's copy, and
//! clearing by condition.

#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;

use mdkg_coord_sqlite::mail::{
    FAULT_SCHEMA, FAULT_SENDER, FaultOut, FaultSent, FaultTally, InboxQuery, MailError, Outgoing,
    Retention, Shown, To,
};
use mdkg_coord_sqlite::{Store, Upgrade};
use serde_json::{Value, json};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        Scratch(temp::dir(&format!("mdkg-faults-{name}")))
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

const TURN: &str = "the agent's next turn ends without an error";
const LOOP: &str = "no thread of the agent stays paused";

/// A fault body the way mdkg-run builds one, with a marker the store never
/// reads.
fn body(
    fault_id: &str,
    agent: &str,
    code: &str,
    clears: &str,
    marker: u64,
) -> impl Fn(&FaultTally) -> Value {
    let (fault_id, agent, code, clears) = (
        fault_id.to_string(),
        agent.to_string(),
        code.to_string(),
        clears.to_string(),
    );
    move |t: &FaultTally| {
        json!([
            {"kind": "text", "text": format!("{code} fault for {agent}")},
            {"kind": "data", "mediaType": "application/json", "data": {
                "schema": FAULT_SCHEMA, "fault_id": fault_id, "agent": agent,
                "code": code, "count": t.count, "first_at": t.first_at,
                "clears": clears, "worktree": {"path": "/w", "dirty": true, "changed": marker},
            }}
        ])
    }
}

fn send(
    s: &mut Store,
    fault_id: &str,
    agent: &str,
    code: &str,
    clears: &str,
    to: &[&str],
    now: u64,
) -> FaultSent {
    let to: Vec<String> = to.iter().map(|t| t.to_string()).collect();
    s.send_fault(
        &FaultOut {
            fault_id,
            agent,
            code,
            to: &to,
            human_hold_days: 7,
            now_ms: now,
        },
        body(fault_id, agent, code, clears, now),
    )
    .expect("fault send")
}

fn sent_id(sent: &FaultSent) -> String {
    match sent {
        FaultSent::Sent { message, .. } => message.message_id.clone(),
        other => panic!("not sent: {other:?}"),
    }
}

fn data(s: &mut Store, id: &str) -> Value {
    let m = s.message(id, 0).expect("message");
    m.parts.expect("body")[1]["data"].clone()
}

/// The deliveries of `id`: (mailbox, state, acked_by, acked_via).
fn deliveries(s: &mut Store, id: &str) -> Vec<(String, String, Option<String>, Option<String>)> {
    s.message(id, 0)
        .expect("message")
        .deliveries
        .into_iter()
        .map(|d| (d.mailbox, d.state, d.acked_by, d.acked_via))
        .collect()
}

fn unacked(s: &mut Store, mailbox: &str) -> Vec<String> {
    s.inbox(&InboxQuery {
        mailboxes: vec![mailbox.into()],
        ..InboxQuery::default()
    })
    .expect("inbox")
    .messages
    .into_iter()
    .map(|m| m.message_id)
    .collect()
}

#[test]
fn a_new_occurrence_supersedes_the_open_one_and_carries_its_count() {
    let dir = Scratch::new("dedupe");
    let mut s = dir.store();
    let first = send(
        &mut s,
        "f1",
        "lane",
        "run.agent_failed",
        TURN,
        &["boss"],
        1_000,
    );
    assert_eq!(sent_id(&first), "fault.f1.r0");
    let d = data(&mut s, "fault.f1.r0");
    assert_eq!(d["count"], 1);
    let first_at = d["first_at"].clone();
    let second = send(
        &mut s,
        "f2",
        "lane",
        "run.agent_failed",
        TURN,
        &["boss"],
        9_000,
    );
    let FaultSent::Sent { superseded, .. } = &second else {
        panic!("{second:?}");
    };
    assert_eq!(superseded, &vec!["fault.f1.r0".to_string()]);
    let d = data(&mut s, "fault.f2.r0");
    assert_eq!(d["count"], 2, "R2: one higher");
    assert_eq!(d["first_at"], first_at, "R2: first_at unchanged");
    assert_eq!(
        deliveries(&mut s, "fault.f1.r0"),
        vec![(
            "boss".into(),
            "acked".into(),
            Some("mdkg".into()),
            Some("supersede".into())
        )]
    );
    assert_eq!(unacked(&mut s, "boss"), vec!["fault.f2.r0".to_string()]);
    let m = s.message("fault.f2.r0", 0).unwrap();
    assert_eq!(
        (m.kind.as_str(), m.sender.as_str()),
        ("fault", FAULT_SENDER)
    );
    assert_eq!(m.retention, Retention::Durable.to_string());
}

#[test]
fn faults_of_two_agents_or_two_codes_stay_open_side_by_side() {
    let dir = Scratch::new("apart");
    let mut s = dir.store();
    send(&mut s, "a1", "a", "run.agent_failed", TURN, &["boss"], 1);
    send(&mut s, "b1", "b", "run.agent_failed", TURN, &["boss"], 2);
    send(&mut s, "a2", "a", "run.loop_paused", LOOP, &["boss"], 3);
    assert_eq!(
        unacked(&mut s, "boss"),
        vec!["fault.a1.r0", "fault.b1.r0", "fault.a2.r0"]
    );
    assert_eq!(data(&mut s, "fault.b1.r0")["count"], 1);
}

#[test]
fn a_resend_stops_at_the_stores_copy_whatever_its_body() {
    let dir = Scratch::new("resend");
    let mut s = dir.store();
    send(&mut s, "f1", "lane", "run.agent_failed", TURN, &["boss"], 1);
    // Derived again after a failed pending-file write: another time and
    // worktree count, the same occurrence.
    let again = send(
        &mut s,
        "f1",
        "lane",
        "run.agent_failed",
        TURN,
        &["boss"],
        77,
    );
    let FaultSent::Stood(m) = again else {
        panic!("{again:?}");
    };
    assert_eq!(m.message_id, "fault.f1.r0");
    assert_eq!(data(&mut s, "fault.f1.r0")["worktree"]["changed"], 1);
    assert_eq!(unacked(&mut s, "boss").len(), 1);
}

#[test]
fn an_active_occurrence_follows_a_changed_recipient_and_counts_its_routings() {
    let dir = Scratch::new("route");
    let mut s = dir.store();
    send(
        &mut s,
        "f1",
        "lane",
        "run.not_logged_in",
        "login",
        &["a"],
        1,
    );
    let to_b = send(
        &mut s,
        "f1",
        "lane",
        "run.not_logged_in",
        "login",
        &["b"],
        2,
    );
    assert_eq!(sent_id(&to_b), "fault.f1.r1");
    // The old recipient's copy is superseded: one fault open per agent.
    assert_eq!(
        deliveries(&mut s, "fault.f1.r0")[0].3.as_deref(),
        Some("supersede")
    );
    assert_eq!(
        data(&mut s, "fault.f1.r1")["count"],
        1,
        "the same occurrence"
    );
    let back = send(
        &mut s,
        "f1",
        "lane",
        "run.not_logged_in",
        "login",
        &["a"],
        3,
    );
    assert_eq!(sent_id(&back), "fault.f1.r2", "A -> B -> A is three alerts");
    assert_eq!(unacked(&mut s, "a"), vec!["fault.f1.r2".to_string()]);
    assert!(unacked(&mut s, "b").is_empty());
    // Another failure between two recipients: the new one supersedes the
    // old recipient's copy too.
    send(&mut s, "g1", "lane", "run.agent_failed", TURN, &["a"], 4);
    let g2 = send(&mut s, "g2", "lane", "run.agent_failed", TURN, &["b"], 5);
    assert_eq!(sent_id(&g2), "fault.g2.r0");
    assert!(
        unacked(&mut s, "a")
            .iter()
            .all(|id| !id.starts_with("fault.g"))
    );
    // An occurrence a later one replaced stays where it is.
    let old = send(&mut s, "g1", "lane", "run.agent_failed", TURN, &["b"], 6);
    assert!(matches!(old, FaultSent::Stood(_)), "{old:?}");
    assert_eq!(unacked(&mut s, "b"), vec!["fault.g2.r0".to_string()]);
}

#[test]
fn only_the_fault_its_condition_names_is_cleared_and_the_end_is_recorded() {
    let dir = Scratch::new("clear");
    let mut s = dir.store();
    send(&mut s, "f1", "lane", "run.agent_failed", TURN, &["boss"], 1);
    send(&mut s, "p1", "lane", "run.loop_paused", LOOP, &["boss"], 2);
    send(
        &mut s,
        "o1",
        "other",
        "run.agent_failed",
        TURN,
        &["boss"],
        3,
    );
    let cleared = s.clear_faults("lane", TURN, 4).unwrap();
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].fault_id, "f1");
    assert_eq!(
        deliveries(&mut s, "fault.f1.r0")[0].3.as_deref(),
        Some("clear"),
        "R3"
    );
    assert_eq!(
        s.message("fault.f1.r0", 0).unwrap().deliveries[0]
            .outcome
            .as_deref(),
        Some(format!("cleared: {TURN}").as_str())
    );
    assert_eq!(
        unacked(&mut s, "boss"),
        vec!["fault.p1.r0", "fault.o1.r0"],
        "the agent's other faults, and other agents', stay open"
    );
    // Seen again, the condition clears nothing more.
    assert!(s.clear_faults("lane", TURN, 5).unwrap().is_empty());
    // A cleared occurrence is never sent again, to anyone.
    assert_eq!(
        send(
            &mut s,
            "f1",
            "lane",
            "run.agent_failed",
            TURN,
            &["elsewhere"],
            6
        ),
        FaultSent::Cleared
    );
    assert_eq!(s.fault_clears("lane", "run.agent_failed").unwrap(), 1);
    assert_eq!(s.fault_clears("lane", "run.loop_paused").unwrap(), 0);
}

#[test]
fn a_fault_acked_by_hand_still_records_its_clear() {
    let dir = Scratch::new("hand");
    let mut s = dir.store();
    send(
        &mut s,
        "l1",
        "lane",
        "run.not_logged_in",
        "login",
        &["boss"],
        1,
    );
    let inbox = s
        .inbox(&InboxQuery {
            mailboxes: vec!["boss".into()],
            ..InboxQuery::default()
        })
        .unwrap();
    let shown: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
    s.mark_shown(&shown, &["boss".into()], 2).unwrap();
    s.ack(
        &["fault.l1.r0".into()],
        &["boss".into()],
        "boss",
        None,
        None,
        3,
    )
    .unwrap();
    assert_eq!(s.fault_clears("lane", "run.not_logged_in").unwrap(), 0);
    let cleared = s.clear_faults("lane", "login", 4).unwrap();
    assert_eq!(cleared.len(), 1);
    assert!(cleared[0].acked.is_empty(), "nothing was left to ack");
    // The spell's end is durable: the counter moves, so the next spell's
    // occurrence gets a new id; clearing again records nothing more.
    assert_eq!(s.fault_clears("lane", "run.not_logged_in").unwrap(), 1);
    assert!(s.clear_faults("lane", "login", 5).unwrap().is_empty());
    assert_eq!(s.fault_clears("lane", "run.not_logged_in").unwrap(), 1);
}

#[test]
fn ack_through_skips_faults_unless_the_kind_names_them() {
    let dir = Scratch::new("through");
    let mut s = dir.store();
    send(&mut s, "f1", "lane", "run.agent_failed", TURN, &["boss"], 1);
    let mut note = Outgoing {
        message_id: "n1".into(),
        context_id: None,
        kind: "note".into(),
        sender: "x".into(),
        via: None,
        sender_mailboxes: vec![],
        to: To::Mailboxes(vec!["boss".into()]),
        reference_ids: vec![],
        parts: json!([{"kind": "text", "text": "hi"}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: 2,
    };
    s.send(&note).unwrap();
    let inbox = s
        .inbox(&InboxQuery {
            mailboxes: vec!["boss".into()],
            ..InboxQuery::default()
        })
        .unwrap();
    let shown: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
    s.mark_shown(&shown, &["boss".into()], 3).unwrap();
    let (acked, skipped) = s
        .ack_through(u64::MAX >> 2, &[], &["boss".into()], "boss", None, None, 4)
        .unwrap();
    assert_eq!(acked.acked, vec![("n1".to_string(), "boss".to_string())]);
    assert_eq!(skipped.len(), 1);
    assert_eq!(
        (skipped[0].message_id.as_str(), skipped[0].reason),
        ("fault.f1.r0", "fault"),
        "R6: skipped and named"
    );
    let (acked, skipped) = s
        .ack_through(
            u64::MAX >> 2,
            &["fault".into()],
            &["boss".into()],
            "boss",
            None,
            None,
            5,
        )
        .unwrap();
    assert_eq!(acked.acked.len(), 1);
    assert!(skipped.is_empty());
    note.message_id = "n2".into();
    s.send(&note).unwrap();
}

#[test]
fn another_row_under_a_fault_id_is_store_damage_never_the_alert() {
    let dir = Scratch::new("integrity");
    let mut s = dir.store();
    // An mdkg: sender passes the reservation, so only the store's own
    // check stands between this row and the alert.
    s.send(&Outgoing {
        message_id: "fault.f1.r0".into(),
        context_id: None,
        kind: "note".into(),
        sender: "mdkg:other".into(),
        via: None,
        sender_mailboxes: vec![],
        to: To::Mailboxes(vec!["boss".into()]),
        reference_ids: vec![],
        parts: json!([{"kind": "text", "text": "not a fault"}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: 1,
    })
    .unwrap();
    let to = vec!["boss".to_string()];
    let e = s
        .send_fault(
            &FaultOut {
                fault_id: "f1",
                agent: "lane",
                code: "run.agent_failed",
                to: &to,
                human_hold_days: 7,
                now_ms: 2,
            },
            body("f1", "lane", "run.agent_failed", TURN, 2),
        )
        .unwrap_err();
    assert!(matches!(e, MailError::Integrity(_)), "{e:?}");
    assert_eq!(unacked(&mut s, "boss"), vec!["fault.f1.r0".to_string()]);
}

#[test]
fn only_a_clear_records_own_kind_and_fault_id_count_as_a_clear() {
    let dir = Scratch::new("decoy");
    let mut s = dir.store();
    let sent = send(&mut s, "l9", "lane", "run.agent_failed", TURN, &["boss"], 1);
    assert_eq!(sent_id(&sent), "fault.l9.r0");
    drop(s);
    // A record of another kind that carries a clear's fields deeper down,
    // as one embedding caller data might. Planted past the chain's end, it
    // is read but never appended after: the resend below writes nothing.
    let decoy =
        json!({"kind": "x.other", "body": {"data": {"kind": "fault.clear", "fault_id": "l9"}}});
    rusqlite::Connection::open(dir.0.join("mdkg").join("coord.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO log (hash, record) VALUES ('decoy', ?1)",
            [decoy.to_string()],
        )
        .unwrap();
    let mut s = dir.store();
    // Not taken as the clear: the resend stops at the store's copy instead
    // of reporting the occurrence cleared.
    let again = send(&mut s, "l9", "lane", "run.agent_failed", TURN, &["boss"], 2);
    assert!(matches!(again, FaultSent::Stood(_)), "{again:?}");
}

#[test]
fn a_fault_records_no_expiry_where_a_note_records_its_kinds() {
    let dir = Scratch::new("expiry");
    let mut s = dir.store();
    send(&mut s, "e1", "lane", "run.agent_failed", TURN, &["boss"], 1);
    // The control: a note sent through the same send path, at the same
    // time, records its kind's three days (A25).
    s.send(&Outgoing {
        message_id: "n1".into(),
        context_id: None,
        kind: "note".into(),
        sender: "x".into(),
        via: None,
        sender_mailboxes: vec![],
        to: To::Mailboxes(vec!["boss".into()]),
        reference_ids: vec![],
        parts: json!([{"kind": "text", "text": "hi"}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: 1,
    })
    .unwrap();
    drop(s);
    // Expiry acks only deliveries whose message records an `expires_ms`
    // (ledger::expire_due_in), so a fault, which records none, is never
    // expired: only its condition's end clears it (A25, A26).
    let db = rusqlite::Connection::open(dir.0.join("mdkg").join("coord.sqlite")).unwrap();
    let expiry = |id: &str| -> Option<i64> {
        db.query_row(
            "SELECT mm.expires_ms FROM msg_messages m
             LEFT JOIN msg_meta mm ON mm.seq = m.seq WHERE m.message_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(expiry("fault.e1.r0"), None);
    assert_eq!(expiry("n1"), Some(1 + 3 * 24 * 60 * 60 * 1000));
}
