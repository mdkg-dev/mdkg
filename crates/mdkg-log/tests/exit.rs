//! The exit guarantee and storage, end to end: build a shard log, checkpoint
//! it, persist it, export it, and verify everything without a coordinator.

#[path = "common/temp.rs"]
mod temp;

use mdkg_format::Digest;
use mdkg_log::checkpoint::{HeaderDraft, knowledge_root};
use mdkg_log::export::{ShardExport, VerifyError};
use mdkg_log::store::SegmentStore;
use mdkg_log::{Clock, Draft, Log, Visibility};

const SHARD: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";

fn event(
    clock: &mut Clock,
    now: u64,
    key: &str,
    parents: Vec<String>,
    verb: &str,
) -> mdkg_log::Envelope {
    Draft {
        schema: 1,
        shard: SHARD.into(),
        parents,
        actor: "agent:claude-code".into(),
        event_type: format!("mdkg.lease.{verb}"),
        subject: vec![format!("{SHARD}/task-1")],
        inline: serde_json::Map::new(),
        payload: None,
        hlc: clock.tick(now),
        seq: None,
        idempotency_key: key.into(),
        valid_until: now + 60_000,
        visibility: Visibility::Private,
        trace: None,
    }
    .seal()
    .expect("seal")
}

fn sample() -> (Log, ShardExport) {
    let mut clock = Clock::new();
    let mut log = Log::new(SHARD);
    let claim = event(&mut clock, 1_000, "claim-1", vec![], "claim");
    let beat = event(
        &mut clock,
        2_000,
        "beat-1",
        vec![claim.id.clone()],
        "heartbeat",
    );
    let release = event(
        &mut clock,
        3_000,
        "release-1",
        vec![beat.id.clone()],
        "release",
    );
    for e in [claim, beat, release] {
        log.append(e, 3_000).expect("append");
    }
    let header = HeaderDraft {
        shard: SHARD.into(),
        seq_from: 1,
        seq_to: 3,
        prev_header: None,
        log_root: log.root().to_string(),
        knowledge_root: knowledge_root(vec![(
            ".mdkg/work/task-1.md".into(),
            Digest::of_bytes(b"task"),
        )])
        .to_string(),
        hlc: clock.tick(3_001),
        sequencer: "local".into(),
    }
    .seal()
    .expect("header");
    let export = ShardExport::new(&log, vec![header]);
    (log, export)
}

#[test]
fn export_verifies_and_round_trips_through_json() {
    let (log, export) = sample();
    export.verify().expect("verifies");
    let text = serde_json::to_string(&export).expect("serialize");
    let back: ShardExport = serde_json::from_str(&text).expect("parse");
    let imported = back.into_log().expect("import");
    assert_eq!(imported.root(), log.root());
    assert_eq!(imported.heads(), log.heads());
}

#[test]
fn tampered_exports_are_rejected() {
    let (_, export) = sample();

    let mut edited = export.clone();
    edited.events[1].draft.actor = "agent:mallory".into();
    assert!(matches!(edited.verify(), Err(VerifyError::Events(_))));

    let mut dropped = export.clone();
    dropped.events.pop();
    assert!(matches!(
        dropped.verify(),
        Err(VerifyError::RootMismatch { .. })
    ));

    let mut reordered = export;
    reordered.events.swap(0, 1);
    assert!(matches!(reordered.verify(), Err(VerifyError::Events(_))));
}

#[test]
fn replay_is_deterministic() {
    let (_, a) = sample();
    let (_, b) = sample();
    assert_eq!(a, b, "same inputs give byte-identical exports");
}

#[test]
fn segment_store_persists_and_detects_corruption() {
    let parent = temp::dir("mdkg-log-store");
    let dir = parent.join("segments");
    let store = SegmentStore::open(&dir, 2).expect("open");
    let (log, _) = sample();
    store.append(log.events()).expect("append");
    let segments = std::fs::read_dir(&dir).expect("dir").count();
    assert_eq!(segments, 2, "three events at two per segment");
    let read = store.read_all().expect("read");
    assert_eq!(read, log.events());

    // Flip one byte of a stored event: reading must fail closed.
    let first = dir.join("00000001.jsonl");
    let text = std::fs::read_to_string(&first).expect("segment");
    std::fs::write(&first, text.replacen("claude-code", "claude-c0de", 1)).expect("write");
    assert!(store.read_all().is_err());
    let _ = std::fs::remove_dir_all(&parent);
}
