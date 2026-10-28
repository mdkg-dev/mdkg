//! Regression tests for the 2026-10-18 correctness review (log findings).
//! Each is the reviewer's failing scenario with the assertion inverted.

#[path = "common/temp.rs"]
mod temp;

use mdkg_format::Digest;
use mdkg_log::checkpoint::{CheckpointHeader, HeaderDraft};
use mdkg_log::export::{ShardExport, VerifyError};
use mdkg_log::store::SegmentStore;
use mdkg_log::{Clock, Draft, Envelope, Hlc, Log, Visibility};

const SHARD: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";

fn ev(wall: u64, key: &str, parents: Vec<String>, actor: &str) -> Envelope {
    Draft {
        schema: 1,
        shard: SHARD.into(),
        parents,
        actor: actor.into(),
        event_type: "mdkg.lease.claim".into(),
        subject: vec![],
        inline: serde_json::Map::new(),
        payload: None,
        hlc: Hlc { wall, logical: 0 },
        seq: None,
        idempotency_key: key.into(),
        valid_until: wall + 60_000,
        visibility: Visibility::Private,
        trace: None,
    }
    .seal()
    .expect("seal")
}

fn header(log: &Log) -> CheckpointHeader {
    HeaderDraft {
        shard: SHARD.into(),
        seq_from: 1,
        seq_to: log.len() as u64,
        prev_header: None,
        log_root: log.root().to_string(),
        knowledge_root: Digest::of_bytes(b"").to_string(),
        hlc: Hlc {
            wall: 1,
            logical: 0,
        },
        sequencer: "local".into(),
    }
    .seal()
    .expect("seal header")
}

/// Finding 4: a key reused legitimately after its window still verifies.
#[test]
fn legit_key_reuse_verifies_and_imports() {
    let mut log = Log::new(SHARD);
    let a = ev(1_000, "k", vec![], "a");
    let b = ev(70_000, "k", vec![a.id.clone()], "b");
    log.append(a, 1_000).unwrap();
    log.append(b, 70_000).unwrap();
    let export = ShardExport::new(&log, vec![header(&log)]);
    export.verify().unwrap();
    assert_eq!(export.into_log().unwrap().root(), log.root());
}

/// Finding 3: events the checkpoint never committed to fail verification.
#[test]
fn uncommitted_events_fail_export_verify() {
    let mut log = Log::new(SHARD);
    let a = ev(1_000, "k", vec![], "a");
    log.append(a.clone(), 1_000).unwrap();
    let mut export = ShardExport::new(&log, vec![header(&log)]);
    let forged = ev(2_000, "k", vec![a.id.clone()], "mallory");
    let child = ev(3_000, "k2", vec![forged.id.clone()], "mallory");
    export.events.push(forged);
    export.events.push(child);
    assert!(matches!(
        export.verify(),
        Err(VerifyError::RootMismatch { .. })
    ));
    let mut duplicated = ShardExport::new(&log, vec![header(&log)]);
    duplicated.events.push(a);
    assert!(matches!(duplicated.verify(), Err(VerifyError::Events(_))));
}

/// Export headers must belong to the export's shard.
#[test]
fn foreign_headers_fail_export_verify() {
    let mut log = Log::new(SHARD);
    log.append(ev(1_000, "k", vec![], "a"), 1_000).unwrap();
    let mut draft = header(&log).draft;
    draft.shard = "another-shard".into();
    let export = ShardExport::new(&log, vec![draft.seal().unwrap()]);
    assert!(matches!(export.verify(), Err(VerifyError::Headers(_))));
}

/// Finding 10: sequencing after sealing doesn't change an event's id.
#[test]
fn sequencing_keeps_the_id() {
    let mut e = ev(1_000, "k", vec![], "a");
    let id = e.id.clone();
    e.draft.seq = Some(7);
    e.verify().unwrap();
    assert_eq!(e.id, id);
}

/// Finding 10: stored bytes are bound by the id; unknown fields and explicit
/// defaults are rejected.
#[test]
fn non_canonical_envelopes_are_rejected() {
    let e = ev(1_000, "k", vec![], "a");
    let canonical = e.to_canonical_json().unwrap();
    Envelope::parse_canonical(&canonical).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&canonical).unwrap();
    v["note"] = serde_json::json!("pay mallory");
    let tampered = mdkg_format::canonical::to_canonical_string(&v).unwrap();
    assert!(Envelope::parse_canonical(&tampered).is_err());
    let mut d: serde_json::Value = serde_json::from_str(&canonical).unwrap();
    d["inline"] = serde_json::json!({});
    let defaulted = mdkg_format::canonical::to_canonical_string(&d).unwrap();
    assert!(Envelope::parse_canonical(&defaulted).is_err());

    let mut log = Log::new(SHARD);
    log.append(e, 1_000).unwrap();
    let mut doc = serde_json::to_value(ShardExport::new(&log, vec![])).unwrap();
    doc["events"][0]["note"] = serde_json::json!("pay mallory");
    assert!(ShardExport::parse(&doc.to_string()).is_err());
}

/// Finding 14: the clock stays strictly increasing at the logical limit and
/// can refuse remote clocks that are too far ahead.
#[test]
fn hlc_rolls_over_and_bounds_drift() {
    let mut c = Clock::new();
    let remote = Hlc {
        wall: 5,
        logical: u32::MAX,
    };
    let a = c.observe(remote, 1);
    assert!(a > remote);
    let b = c.tick(1);
    assert!(b > a);
    let far = Hlc {
        wall: 10_000_000,
        logical: 0,
    };
    assert!(c.observe_bounded(far, 100, 60_000).is_err());
    assert!(
        c.observe_bounded(
            Hlc {
                wall: 150,
                logical: 0
            },
            100,
            60_000
        )
        .is_ok()
    );
}

/// Finding 14: a torn last line from a crash doesn't poison the store.
#[test]
fn torn_tail_is_recovered() {
    let parent = temp::dir("mdkg-torn");
    let dir = parent.join("segments");
    let store = SegmentStore::open(&dir, 100).unwrap();
    let a = ev(1_000, "a", vec![], "a");
    store.append(std::slice::from_ref(&a)).unwrap();
    let p = dir.join("00000001.jsonl");
    let text = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("{text}{{\"id\":\"sha256:")).unwrap();
    assert_eq!(
        store.read_all().unwrap(),
        vec![a.clone()],
        "torn tail is ignored on read"
    );
    let b = ev(2_000, "b", vec![a.id.clone()], "a");
    store.append(std::slice::from_ref(&b)).unwrap();
    assert_eq!(store.read_all().unwrap(), vec![a, b]);
    std::fs::write(dir.join("notes.jsonl"), "not a segment\n").unwrap();
    assert_eq!(
        store.read_all().unwrap().len(),
        2,
        "non-numeric files are not segments"
    );
    let _ = std::fs::remove_dir_all(&parent);
}
