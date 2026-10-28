use mdkg_coord_sqlite::{
    Store, Upgrade,
    mail::{self, DecisionGrant, Outgoing, Retention, To},
};
use serde_json::json;
use std::{fs, path::PathBuf};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mdkg-decision-store-{}", mdkg_format::Uuid7::now()));
        fs::create_dir_all(&path).expect("decision fixture");
        Self(path)
    }
    fn db(&self) -> PathBuf {
        self.0.join("coord.sqlite")
    }
    fn open(&self) -> Store {
        Store::open_with(&self.db(), 1000, Upgrade::Allow).expect("decision fixture")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}
fn outgoing() -> Outgoing {
    Outgoing {
        message_id: "decision-1".into(),
        context_id: None,
        kind: "decision".into(),
        sender: "orchestrator".into(),
        via: None,
        sender_mailboxes: vec!["orchestrator".into()],
        to: To::Mailboxes(vec!["worker".into()]),
        reference_ids: vec![],
        parts: json!([{"kind":"text","text":"context"}]),
        retention: Retention::PurgeOnAck,
        human_hold_days: 7,
        now_ms: 1000,
    }
}
fn grant() -> DecisionGrant {
    DecisionGrant {
        human: "human:nick".into(),
        source_file: "/operator/config.json".into(),
        action: "implement".into(),
        node_ref: "dec-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b".into(),
        node_commit: "a".repeat(40),
        node_path: ".mdkg/design/decision.md".into(),
        node_digest: format!("sha256:{}", "b".repeat(64)),
        graph_config_path: ".mdkg/config.json".into(),
    }
}

#[test]
fn decision_transaction_is_atomic_and_retries_preserve_original_provenance() {
    let temp = Scratch::new();
    let mut store = temp.open();
    let mut out = outgoing();
    let mut g = grant();
    assert!(store.send(&out).is_err());
    out.to = To::Mailboxes(vec![out.sender.clone()]);
    assert!(store.send_decision(&out, &g).is_err());
    assert!(
        store
            .decisions(0, 100)
            .expect("decision fixture")
            .is_empty()
    );
    out.to = To::Mailboxes(vec!["worker".into()]);
    let (first, duplicate) = store.send_decision(&out, &g).expect("decision fixture");
    assert!(!duplicate);
    out.now_ms = 2000;
    g.node_commit = "c".repeat(40);
    let (retry, duplicate) = store.send_decision(&out, &g).expect("decision fixture");
    assert!(duplicate);
    assert_eq!(first, retry);
    assert_eq!(
        retry.decision.as_ref().expect("decision fixture")["sentAtMs"],
        1000
    );
    g.action = "commit".into();
    assert!(store.send_decision(&out, &g).is_err());
    g.action = "push".into();
    out.message_id = "new-id".into();
    assert!(store.send_decision(&out, &g).is_err());
    g.action = "implement".into();
    out.via = Some("worker".into());
    assert!(store.send_decision(&out, &g).is_err());
    assert_eq!(store.decisions(0, 100).expect("decision fixture").len(), 1);
    let before = store.message("decision-1", 2000).expect("decision fixture");
    let audit = mail::read_decisions(&temp.db(), 0, 100, &[]).expect("decision fixture");
    assert_eq!(audit, store.decisions(0, 100).expect("decision fixture"));
    assert_eq!(
        before,
        store.message("decision-1", 2000).expect("decision fixture")
    );
}

#[test]
fn decision_schema_upgrade_is_additive_copied_and_audit_read_never_upgrades() {
    let temp = Scratch::new();
    let mut store = temp.open();
    let mut out = outgoing();
    out.kind = "note".into();
    let (old, _) = store.send(&out).expect("decision fixture");
    drop(store);
    let conn = rusqlite::Connection::open(temp.db()).expect("decision fixture");
    conn.execute_batch(
        "DROP TABLE msg_decisions; UPDATE meta SET value='4' WHERE key='msg_schema';",
    )
    .expect("decision fixture");
    drop(conn);
    assert!(mail::read_decisions(&temp.db(), 0, 100, &[]).is_err());
    let conn = rusqlite::Connection::open(temp.db()).expect("decision fixture");
    assert_eq!(
        conn.query_row("SELECT value FROM meta WHERE key='msg_schema'", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("decision fixture"),
        "4"
    );
    drop(conn);
    let mut upgraded = temp.open();
    assert!(
        upgraded
            .backup()
            .expect("decision fixture")
            .ends_with("coord.sqlite.pre-3+msg4-to-3+msg5.sqlite")
    );
    assert_eq!(
        upgraded
            .message("decision-1", 1000)
            .expect("decision fixture"),
        old
    );
    assert!(
        upgraded
            .decisions(0, 100)
            .expect("decision fixture")
            .is_empty()
    );
    let missing = temp.0.join("never-created.sqlite");
    assert!(
        mail::read_decisions(&missing, 0, 100, &[])
            .expect("decision fixture")
            .is_empty()
    );
    assert!(!missing.exists());
}
