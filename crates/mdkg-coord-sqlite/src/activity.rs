//! Native coordinator activity for a launcher's turn receipt. Reads use the
//! journal, never imported log records; an imported claim is not a local act.
//!
//! Cursors bind to the database file's device/inode and mdkg restore generation,
//! as well as its store id and log history. These are advisory observations,
//! not authentication against a same-user writer. An external copy over the
//! same inode that preserves the baseline and generation cannot be detected.
//! A same-user process that swaps the store file while it is being opened can
//! make the cursor read another store. Completion evidence requires native
//! revision metadata; an unversioned done status alone is not promoted to a fact.
//! A reopened task completed with identical bytes before its timestamp advances
//! is reported once by native facts; shared work can additionally use the turn's
//! own commit-span done transition as evidence.

use std::collections::{BTreeMap, BTreeSet};

use mdkg_coord::{Command, Coordinator, Lease, Outcome, Status};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{StoreError, log};

/// An opaque in-process receipt baseline, anchored to this store and history.
/// A restored/forked history cannot silently reuse its journal sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityCursor {
    file: (u64, u64),
    restore_generation: u64,
    store: String,
    journal: i64,
    log_seq: u64,
    log_hash: String,
}

#[cfg(unix)]
pub(crate) fn identity_at(path: &std::path::Path) -> Result<Option<(u64, u64)>, StoreError> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(file) => Ok(Some((file.dev(), file.ino()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub(crate) fn identity_at(_path: &std::path::Path) -> Result<Option<(u64, u64)>, StoreError> {
    Ok(None)
}

fn file_identity(conn: &Connection, opened: Option<(u64, u64)>) -> Result<(u64, u64), StoreError> {
    let path = conn.path().filter(|p| !p.is_empty());
    let (Some(path), Some(opened)) = (path, opened) else {
        return Err(StoreError::Log(
            "activity unavailable: database file identity is unavailable".into(),
        ));
    };
    if identity_at(std::path::Path::new(path))? != Some(opened) {
        return Err(replaced());
    }
    Ok(opened)
}

pub(crate) fn replaced() -> StoreError {
    StoreError::Log("activity unavailable: store replaced or restored during the turn".into())
}

/// A successful claim or active authority reconciliation in the interval.
/// Kept even if that lease was subsequently released, committed or expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseClaim {
    pub lease: String,
    pub scope: String,
    /// The coordinator's canonical node key, when the claim names a node.
    pub node: Option<String>,
}

/// Stable identity of one completed node revision, independent of retry or actor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Completion {
    /// Canonical node identity, independent of the node file's path.
    pub node: String,
    /// Immutable content hash, or the commit/changeset that marked it done.
    pub revision: String,
}

/// Receipt evidence is a fact in the interval or a done transition from the
/// launcher's proven commit span. Callers must supply only those transitions;
/// a node's current done status by itself is deliberately not an input.
/// Commit evidence uses the same node identity and content-revision convention.
pub fn completions_for_receipt(
    facts: &[Completion],
    committed_done: &[Completion],
) -> Vec<Completion> {
    facts
        .iter()
        .chain(committed_done)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// One actor's facts in a single read transaction, sorted deterministically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivitySnapshot {
    pub through: ActivityCursor,
    pub claims: Vec<LeaseClaim>,
    /// Task summary of the versioned completion facts, not current file status.
    pub done_marks: Vec<String>,
    pub completions: Vec<Completion>,
    /// Active, confirmed leases whose deadlines are later than the query's now.
    pub leases: Vec<Lease>,
}

fn completion_value(leaf: &str, node: &str, revision: &str) -> Result<String, StoreError> {
    if leaf.is_empty() || node.is_empty() || revision.is_empty() || revision.len() > 256 {
        return Err(StoreError::Log(
            "completion identity is missing or too long".into(),
        ));
    }
    Ok(serde_json::to_string(&(leaf, node, revision))?)
}

fn completion_key(node: &str, revision: &str) -> Result<String, StoreError> {
    let value = serde_json::to_string(&(node, revision))?;
    Ok(format!(
        "completion.identity:{}",
        mdkg_format::Digest::of_bytes(value.as_bytes())
    ))
}

fn journal_key(seq: i64) -> String {
    format!("completion.journal:{seq:020}")
}

/// Receipt-only indexes, in the same transaction as the native operation.
/// A retry may clear a leftover lease/status, but cannot move the first fact
/// into a later cursor. Imports cannot create these native-journal indexes.
pub(crate) fn record_completion(
    conn: &Connection,
    seq: i64,
    leaf: &str,
    node: &str,
    revision: &str,
) -> Result<(), StoreError> {
    let value = completion_value(leaf, node, revision)?;
    let key = completion_key(node, revision)?;
    let first: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(first) = first {
        let first: i64 = first
            .parse()
            .map_err(|_| StoreError::Log("invalid first completion position".into()))?;
        let known: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [journal_key(first)],
                |r| r.get(0),
            )
            .optional()?;
        let known: Option<(String, String, String)> =
            known.map(|v| serde_json::from_str(&v)).transpose()?;
        if first <= 0 || first > seq || !known.is_some_and(|(_, n, r)| n == node && r == revision) {
            return Err(StoreError::Log(
                "completion identity index is inconsistent".into(),
            ));
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2), (?3, ?4)",
        params![key, seq.to_string(), journal_key(seq), value],
    )?;
    Ok(())
}

pub(crate) fn cursor(
    conn: &Connection,
    file: Option<(u64, u64)>,
    journal: i64,
) -> Result<ActivityCursor, StoreError> {
    let store = conn.query_row("SELECT value FROM meta WHERE key = 'store_id'", [], |r| {
        r.get(0)
    })?;
    let head = log::head(conn)?;
    if head != log::noted_head(conn)? {
        return Err(StoreError::Log(
            "the activity baseline's log head is inconsistent".into(),
        ));
    }
    let (log_seq, log_hash) = head.ok_or_else(|| StoreError::Log("the chain is empty".into()))?;
    Ok(ActivityCursor {
        file: file_identity(conn, file)?,
        restore_generation: crate::store::restore_generation(conn)?,
        store,
        journal,
        log_seq,
        log_hash,
    })
}

pub(crate) fn read(
    conn: &Connection,
    state: &Coordinator,
    through: ActivityCursor,
    after: &ActivityCursor,
    actor: &str,
    now: u64,
) -> Result<ActivitySnapshot, StoreError> {
    if after.file != through.file || after.restore_generation != through.restore_generation {
        return Err(replaced());
    }
    let anchor: Option<String> = conn
        .query_row(
            "SELECT hash FROM log WHERE seq = ?1",
            [i64::try_from(after.log_seq)
                .map_err(|_| StoreError::Log("activity cursor is too large".into()))?],
            |r| r.get(0),
        )
        .optional()?;
    if after.store != through.store
        || after.journal > through.journal
        || after.log_seq > through.log_seq
        || anchor.as_deref() != Some(after.log_hash.as_str())
    {
        return Err(StoreError::Log(
            "the activity cursor belongs to another store or history".into(),
        ));
    }
    let IntervalFacts {
        mut claims,
        completions,
        done_marks,
    } = entries(
        conn,
        state,
        after.journal,
        through.journal,
        actor,
        100_000,
        16 * 1024 * 1024,
    )?;
    claims.sort_by(|a, b| (&a.scope, &a.node, &a.lease).cmp(&(&b.scope, &b.node, &b.lease)));
    let mut leases: Vec<Lease> = state
        .leases()
        .filter(|l| {
            l.owner == actor && l.confirmed && l.status == Status::Active && l.deadline > now
        })
        .cloned()
        .collect();
    leases.sort_by(|a, b| (&a.scope, &a.id).cmp(&(&b.scope, &b.id)));
    Ok(ActivitySnapshot {
        through,
        claims,
        done_marks,
        completions,
        leases,
    })
}

#[derive(Debug)]
struct IntervalFacts {
    claims: Vec<LeaseClaim>,
    completions: Vec<Completion>,
    done_marks: Vec<String>,
}

fn entries(
    conn: &Connection,
    state: &Coordinator,
    after: i64,
    through: i64,
    actor: &str,
    max_entries: usize,
    max_bytes: u64,
) -> Result<IntervalFacts, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT length(CAST(j.command AS BLOB)) + length(CAST(j.outcome AS BLOB))
                + COALESCE(length(CAST(m.value AS BLOB)), 0), j.command, j.outcome, m.value, j.seq
         FROM journal j LEFT JOIN meta m ON m.key = printf('completion.journal:%020d', j.seq)
         WHERE j.seq > ?1 AND j.seq <= ?2 ORDER BY j.seq",
    )?;
    let mut rows = stmt.query(params![after, through])?;
    let mut count = 0;
    let mut bytes = 0u64;
    let mut claims = BTreeMap::new();
    let mut done = BTreeSet::new();
    let mut done_marks = BTreeSet::new();
    while let Some(row) = rows.next()? {
        count += 1;
        let length = u64::try_from(row.get::<_, i64>(0)?)
            .map_err(|_| StoreError::Log("activity row has an invalid byte length".into()))?;
        bytes = bytes.saturating_add(length);
        if count > max_entries || bytes > max_bytes {
            return Err(StoreError::Log(
                "turn activity exceeds its read limit; facts are unavailable".into(),
            ));
        }
        let command: Command = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let outcome: Outcome = serde_json::from_str(&row.get::<_, String>(2)?)?;
        let claimed = match (command, outcome) {
            (Command::Claim { owner, .. }, Outcome::Lease { lease }) if owner == actor => {
                Some(lease)
            }
            (
                Command::Reconcile { lease, .. },
                Outcome::Reconciled {
                    lease: id,
                    status: Status::Active,
                },
            ) if lease.owner == actor => Some(id),
            (
                Command::SetWork {
                    leaf,
                    status: Some(status),
                    actor: who,
                    ..
                },
                Outcome::Done,
            ) if who == actor && status == "done" => {
                if let Some(value) = row.get::<_, Option<String>>(3)? {
                    let (scope, node, revision): (String, String, String) =
                        serde_json::from_str(&value)?;
                    if scope != leaf.as_str()
                        || completion_value(&scope, &node, &revision)? != value
                    {
                        return Err(StoreError::Log(
                            "completion metadata does not match its native operation".into(),
                        ));
                    }
                    let first: Option<String> = conn
                        .query_row(
                            "SELECT value FROM meta WHERE key = ?1",
                            [completion_key(&node, &revision)?],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if first.as_deref() != Some(row.get::<_, i64>(4)?.to_string().as_str()) {
                        return Err(StoreError::Log(
                            "completion first position is inconsistent".into(),
                        ));
                    }
                    done_marks.insert(scope);
                    done.insert(Completion { node, revision });
                }
                None
            }
            _ => None,
        };
        if let Some(id) = claimed {
            // Idempotent calls may submit different node metadata while
            // returning the original lease. The snapshot is authoritative.
            let lease = state
                .lease(&id)
                .filter(|l| l.owner == actor && l.confirmed)
                .ok_or_else(|| {
                    StoreError::Log(
                        "a successful activity lease is unavailable in the snapshot".into(),
                    )
                })?;
            let claim = LeaseClaim {
                lease: id.0,
                scope: lease.scope.to_string(),
                node: lease.node.clone(),
            };
            claims.entry(claim.lease.clone()).or_insert(claim);
        }
    }
    Ok(IntervalFacts {
        claims: claims.into_values().collect(),
        completions: done.into_iter().collect(),
        done_marks: done_marks.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, Upgrade};
    use mdkg_coord::{Agent, LeaseId, Mode, Request, Scope};
    use std::path::PathBuf;

    struct Fixture {
        dir: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("mdkg-activity-{}-{now}-{id}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            Self { dir }
        }
        fn path(&self) -> PathBuf {
            self.dir.join("coord.sqlite")
        }
        fn open(&self) -> Store {
            Store::open_with(&self.path(), 1_000, Upgrade::Allow).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn command(owner: &str, scope: &str, node: Option<&str>) -> Command {
        Command::Claim {
            owner: owner.into(),
            scope: Scope::new(scope),
            mode: Mode::Exclusive,
            now: 0,
            request: Some(Request {
                key: format!("{owner}:{scope}"),
                valid_until: 1_000,
            }),
            node: node.map(String::from),
        }
    }
    fn claim(store: &mut Store, owner: &str, scope: &str, node: Option<&str>) -> LeaseId {
        match store.execute(&command(owner, scope, node)).unwrap() {
            Outcome::Lease { lease } => lease,
            other => panic!("claim failed: {other:?}"),
        }
    }

    #[test]
    fn activity_keeps_claims_released_inside_the_turn_and_filters_actor_and_outcome() {
        let f = Fixture::new();
        let mut store = f.open();
        claim(&mut store, "alice", "prior", None);
        let before = store.activity_cursor().unwrap();
        let ended = claim(&mut store, "alice", "a", Some("graph#task-a"));
        store
            .execute(&Command::Release {
                owner: "alice".into(),
                lease: ended.clone(),
                now: 1,
            })
            .unwrap();
        claim(&mut store, "bob", "b", Some("graph#task-b"));
        let kept = claim(&mut store, "alice", "c", Some("graph#task-c"));
        assert!(matches!(
            store.execute(&command("bob", "c", None)).unwrap(),
            Outcome::Refused { .. }
        ));
        // Another successful call on the same lease is listed once.
        assert_eq!(claim(&mut store, "alice", "c", Some("graph#task-c")), kept);
        for (actor, leaf, status) in [
            ("alice", "a", Some("done")),
            ("alice", "a", Some("review")),
            ("alice", "a", None),
            ("bob", "b", Some("done")),
            ("bob", "c", Some("done")),
        ] {
            if status == Some("done") {
                store
                    .complete_local(&Scope::new(leaf), leaf, actor, "fixture-revision", 2)
                    .unwrap();
            } else {
                store
                    .execute(&Command::SetWork {
                        leaf: Scope::new(leaf),
                        status: status.map(String::from),
                        changeset: None,
                        actor: actor.into(),
                        now: 2,
                    })
                    .unwrap();
            }
        }
        let facts = store.activity_since(&before, "alice", 3).unwrap();
        assert_eq!(
            facts.claims,
            [
                LeaseClaim {
                    lease: ended.0,
                    scope: "a".into(),
                    node: Some("graph#task-a".into())
                },
                LeaseClaim {
                    lease: kept.0,
                    scope: "c".into(),
                    node: Some("graph#task-c".into())
                },
            ]
        );
        assert_eq!(facts.done_marks, ["a"]);
        assert_eq!(
            facts
                .leases
                .iter()
                .map(|l| l.scope.as_str())
                .collect::<Vec<_>>(),
            ["c", "prior"]
        );
        let empty = store.activity_since(&facts.through, "alice", 3).unwrap();
        assert!(empty.claims.is_empty() && empty.done_marks.is_empty());
        assert_eq!(empty.leases, facts.leases);
    }

    #[test]
    fn activity_confirms_only_successful_active_reconciliations() {
        let f = Fixture::new();
        let mut store = f.open();
        claim(&mut store, "bob", "blocked", None);
        let state = store.coordinator().unwrap();
        let mut offline = Agent::new("alice", &state);
        offline.disconnect(&state);
        for (scope, ttl) in [("remote", 1_000), ("expired", 1), ("blocked", 1_000)] {
            offline.claim_offline(&Scope::new(scope), 0, ttl).unwrap();
        }
        let before = store.activity_cursor().unwrap();
        for lease in offline.provisional() {
            for _ in 0..2 {
                store
                    .execute(&Command::Reconcile {
                        lease: Box::new(lease.clone()),
                        now: 2,
                    })
                    .unwrap();
            }
        }
        let facts = store.activity_since(&before, "alice", 3).unwrap();
        assert_eq!(facts.claims.len(), 1);
        assert_eq!(facts.claims[0].scope, "remote");
        assert_eq!(facts.leases.len(), 1);
        assert!(facts.leases[0].confirmed);
        assert_eq!(facts.claims[0].lease, facts.leases[0].id.0);
    }

    #[test]
    fn activity_claim_retry_uses_the_original_lease_node() {
        metadata_retry(false);
    }

    #[test]
    fn activity_reconcile_retry_uses_the_original_lease_node() {
        metadata_retry(true);
    }

    fn metadata_retry(reconcile: bool) {
        let f = Fixture::new();
        let mut store = f.open();
        let mut operation = if reconcile {
            let state = store.coordinator().unwrap();
            let mut offline = Agent::new("alice", &state);
            offline.disconnect(&state);
            offline.claim_offline(&Scope::new("a"), 0, 1_000).unwrap();
            let mut lease = offline.provisional().next().unwrap().clone();
            lease.node = Some("graph#task-original".into());
            Command::Reconcile {
                lease: Box::new(lease),
                now: 1,
            }
        } else {
            command("alice", "a", Some("graph#task-original"))
        };
        let id = match store.execute(&operation).unwrap() {
            Outcome::Lease { lease }
            | Outcome::Reconciled {
                lease,
                status: Status::Active,
            } => lease,
            other => panic!("unexpected {other:?}"),
        };
        let before = store.activity_cursor().unwrap();
        // These native idempotency handlers keep the original lease's node.
        match &mut operation {
            Command::Claim { node, .. } => *node = Some("graph#task-submitted".into()),
            Command::Reconcile { lease, .. } => lease.node = Some("graph#task-submitted".into()),
            _ => unreachable!(),
        }
        store.execute(&operation).unwrap();
        let facts = store.activity_since(&before, "alice", 2).unwrap();
        assert_eq!(facts.claims.len(), 1);
        assert_eq!(facts.claims[0].lease, id.0);
        assert_eq!(facts.claims[0].node.as_deref(), Some("graph#task-original"));
        assert_eq!(facts.claims[0].node, facts.leases[0].node);
    }

    #[test]
    fn activity_expiry_filter_is_read_only_and_imports_are_not_native_activity() {
        let f = Fixture::new();
        let mut store = f.open();
        claim(&mut store, "alice", "old", None);
        let before = store.activity_cursor().unwrap();
        store.import_records("receipt-test", &[log::Imported {
            ts_ms: 1, actor: "alice".into(), kind: "lease.claim".into(), subject: vec!["path:fake".into()],
            body: serde_json::json!({"journal": 10, "command": command("alice", "fake", Some("task-fake")), "outcome": {"result": "lease", "lease": "fake"}}).as_object().unwrap().clone(),
        }]).unwrap();
        let journal = store.journal().unwrap();
        let log = store.log_texts().unwrap();
        let state = store.coordinator().unwrap();
        let facts = store.activity_since(&before, "alice", 1_000).unwrap();
        assert!(facts.claims.is_empty() && facts.done_marks.is_empty() && facts.leases.is_empty());
        assert_eq!(store.journal().unwrap(), journal);
        assert_eq!(store.log_texts().unwrap(), log);
        assert_eq!(store.coordinator().unwrap(), state);
    }

    #[test]
    fn activity_cursor_rejects_another_store_and_a_same_sequence_history_fork() {
        let f = Fixture::new();
        let genesis = f.dir.join("genesis.sqlite");
        drop(f.open());
        std::fs::copy(f.path(), &genesis).unwrap();
        let cursor = {
            let mut store = f.open();
            claim(&mut store, "alice", "original", None);
            store.activity_cursor().unwrap()
        };
        let other = Fixture::new();
        assert!(other.open().activity_since(&cursor, "alice", 1).is_err());
        std::fs::copy(&genesis, f.path()).unwrap();
        {
            let mut store = f.open();
            assert!(
                store.activity_since(&cursor, "alice", 1).is_err(),
                "rewound before baseline"
            );
            claim(&mut store, "bob", "fork", None);
            assert!(
                store.activity_since(&cursor, "alice", 1).is_err(),
                "same journal sequence, different log anchor"
            );
        }
    }

    #[test]
    fn activity_rejects_a_copied_store_that_keeps_its_baseline() {
        let f = Fixture::new();
        let mut store = f.open();
        claim(&mut store, "alice", "baseline", None);
        let before = store.activity_cursor().unwrap();
        drop(store);
        let copy = f.dir.join("copy.sqlite");
        std::fs::copy(f.path(), &copy).unwrap();
        let mut copied = Store::open_with(&copy, 1_000, Upgrade::Refuse).unwrap();
        claim(&mut copied, "alice", "copied-event", None);
        let result = copied.activity_since(&before, "alice", 1);
        assert!(
            matches!(&result, Err(StoreError::Log(e)) if e.contains("store replaced or restored during the turn")),
            "copied baseline must not attribute the copy's events: {result:?}"
        );
        let mut original = f.open();
        claim(&mut original, "alice", "original-event", None);
        let facts = original.activity_since(&before, "alice", 1).unwrap();
        assert_eq!(facts.claims.len(), 1);
        assert_eq!(facts.claims[0].scope, "original-event");
    }

    #[test]
    fn activity_cursor_refuses_a_replaced_file_behind_an_open_handle() {
        let f = Fixture::new();
        let mut store = f.open();
        claim(&mut store, "alice", "baseline", None);
        let before = store.activity_cursor().unwrap();
        let copy = crate::snapshot(&f.path(), "held-file").unwrap();
        std::fs::rename(copy, f.path()).unwrap();
        for result in [
            store.activity_cursor().map(|_| ()),
            store.activity_since(&before, "alice", 1).map(|_| ()),
        ] {
            assert!(
                matches!(&result, Err(StoreError::Log(e)) if e.contains("store replaced or restored during the turn")),
                "open handle must not adopt the replacement's identity: {result:?}"
            );
        }
    }

    #[test]
    fn activity_rejects_mdkg_restores_that_keep_the_baseline_and_inode() {
        use std::os::unix::fs::MetadataExt;
        for conditional in [false, true] {
            let f = Fixture::new();
            let mut store = f.open();
            claim(&mut store, "alice", "baseline", None);
            let mut before = store.activity_cursor().unwrap();
            let (_, head) = store.log_head().unwrap();
            drop(store);
            let copy = crate::snapshot(&f.path(), "after-baseline").unwrap();
            let backup_bytes = std::fs::read(&copy).unwrap();
            let metadata = std::fs::metadata(f.path()).unwrap();
            // Restoring the same older snapshot twice must not reuse its
            // generation, even though it retains the original log anchor.
            for generation in 1..=2 {
                if conditional {
                    crate::restore_if_head(&f.path(), &copy, &head).unwrap();
                } else {
                    crate::restore(&f.path(), &copy).unwrap();
                }
                let after = std::fs::metadata(f.path()).unwrap();
                assert_eq!((after.dev(), after.ino()), (metadata.dev(), metadata.ino()));
                let restored = f.open();
                let result = restored.activity_since(&before, "alice", 1);
                assert!(
                    matches!(&result, Err(StoreError::Log(e)) if e.contains("store replaced or restored during the turn")),
                    "restored baseline must invalidate activity: {result:?}"
                );
                before = restored.activity_cursor().unwrap();
                assert_eq!(before.restore_generation, generation);
                drop(restored);
            }
            assert_eq!(
                std::fs::read(copy).unwrap(),
                backup_bytes,
                "backup unchanged"
            );
        }
    }

    #[test]
    fn activity_stores_keep_restore_exclusion_after_a_sibling_closes() {
        const CHILD: &str = "MDKG_ACTIVITY_RESTORE_PROBE";
        if let Some(path) = std::env::var_os(CHILD) {
            let copy = std::env::var_os("MDKG_ACTIVITY_RESTORE_COPY").unwrap();
            let result = crate::restore(std::path::Path::new(&path), std::path::Path::new(&copy));
            assert!(
                matches!(&result, Err(StoreError::Restore(e)) if e.contains("open")),
                "external restore must refuse while the sibling Store survives: {result:?}"
            );
            return;
        }
        let f = Fixture::new();
        let held = f.open();
        let copy = crate::snapshot(&f.path(), "sibling-open").unwrap();
        let sibling = f.open();
        held.activity_cursor().unwrap();
        sibling.activity_cursor().unwrap();
        drop(sibling);
        let log = f.dir.join("restore-probe.log");
        let output = std::fs::File::create(&log).unwrap();
        struct Running(std::process::Child);
        impl Drop for Running {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Running(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "activity::tests::activity_stores_keep_restore_exclusion_after_a_sibling_closes", "--nocapture"])
            .env(CHILD, f.path())
            .env("MDKG_ACTIVITY_RESTORE_COPY", &copy)
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "external restore probe timed out after 60 seconds"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(log).unwrap()
        );
        held.activity_cursor().unwrap();
        drop(held);
        crate::restore(&f.path(), &copy).unwrap();
    }

    #[test]
    fn activity_generation_is_read_only_and_invalid_metadata_is_refused() {
        let f = Fixture::new();
        let store = f.open();
        assert_eq!(store.activity_cursor().unwrap().restore_generation, 0);
        let count = || {
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = 'restore_generation'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(count(), 0, "reading an old store does not add metadata");
        for bad in ["not-a-number", "-1", "18446744073709551616"] {
            store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('restore_generation', ?1)",
                    [bad],
                )
                .unwrap();
            assert!(matches!(
                store.activity_cursor(),
                Err(StoreError::BadMeta(_))
            ));
        }
    }

    #[test]
    fn activity_restore_generation_covers_newer_backups_and_refuses_exhaustion() {
        let f = Fixture::new();
        drop(f.open());
        let copy = crate::snapshot(&f.path(), "generation").unwrap();
        let source = Connection::open(&copy).unwrap();
        source
            .execute(
                "INSERT INTO meta (key, value) VALUES ('restore_generation', '9')",
                [],
            )
            .unwrap();
        drop(source);
        crate::restore(&f.path(), &copy).unwrap();
        let mut store = f.open();
        assert_eq!(store.activity_cursor().unwrap().restore_generation, 10);
        claim(&mut store, "alice", "kept-on-refusal", None);
        let before = store.activity_cursor().unwrap();
        let state = store.coordinator().unwrap();
        let log = store.log_texts().unwrap();
        drop(store);
        let source = Connection::open(&copy).unwrap();
        source
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'restore_generation'",
                [u64::MAX.to_string()],
            )
            .unwrap();
        drop(source);
        assert!(
            matches!(crate::restore(&f.path(), &copy), Err(StoreError::Restore(e)) if e.contains("exhausted"))
        );
        // SQLite can change its file header while restoring journal mode;
        // compare logical content and the cursor, rather than file bytes.
        let kept = f.open();
        assert_eq!(kept.coordinator().unwrap(), state);
        assert_eq!(kept.log_texts().unwrap(), log);
        assert_eq!(kept.activity_cursor().unwrap(), before);
    }

    #[test]
    fn completion_identity_keeps_its_first_position_and_new_revision_is_distinct() {
        let f = Fixture::new();
        let mut store = f.open();
        let before = store.activity_cursor().unwrap();
        let leaf = Scope::new("node:task-a");
        let node = "graph#task-a";
        store
            .complete_local(&leaf, node, "alice", "sha256:version-a", 1)
            .unwrap();
        let first = store.activity_since(&before, "alice", 2).unwrap();
        assert_eq!(
            first.completions,
            [Completion {
                node: node.into(),
                revision: "sha256:version-a".into()
            }]
        );
        drop(store);
        let mut store = f.open();
        store
            .complete_local(&leaf, node, "alice", "sha256:version-a", 3)
            .unwrap();
        store
            .complete_local(
                &Scope::new("renamed-task-a"),
                node,
                "bob",
                "sha256:version-a",
                3,
            )
            .unwrap();
        for actor in ["alice", "bob"] {
            assert!(
                store
                    .activity_since(&first.through, actor, 4)
                    .unwrap()
                    .completions
                    .is_empty()
            );
        }
        store
            .complete_local(&leaf, node, "alice", "sha256:version-b", 5)
            .unwrap();
        let later = store.activity_since(&first.through, "alice", 6).unwrap();
        assert_eq!(
            later.completions,
            [Completion {
                node: node.into(),
                revision: "sha256:version-b".into()
            }]
        );
        let all = store.activity_since(&before, "alice", 6).unwrap();
        assert_eq!(all.completions.len(), 2);
        assert_eq!(all.done_marks, [leaf.to_string()]);
    }

    #[test]
    fn receipt_completion_selection_uses_span_evidence_and_version_identity() {
        let a = Completion {
            node: "node:task-a".into(),
            revision: "sha256:done-a".into(),
        };
        let b = Completion {
            node: "node:task-b".into(),
            revision: "commit:done-b".into(),
        };
        assert!(completions_for_receipt(&[], &[]).is_empty());
        assert_eq!(
            completions_for_receipt(std::slice::from_ref(&a), &[]).as_slice(),
            std::slice::from_ref(&a)
        );
        assert_eq!(
            completions_for_receipt(&[], std::slice::from_ref(&b)).as_slice(),
            std::slice::from_ref(&b)
        );
        assert_eq!(
            completions_for_receipt(&[a.clone(), a.clone()], &[a.clone(), b.clone()]),
            [a, b]
        );
        let f = Fixture::new();
        let mut store = f.open();
        let before = store.activity_cursor().unwrap();
        // An unversioned status, including an older command, is not evidence
        // for any particular done revision and cannot be promoted by the reader.
        store
            .execute(&Command::SetWork {
                leaf: Scope::new("old"),
                status: Some("done".into()),
                changeset: None,
                actor: "alice".into(),
                now: 1,
            })
            .unwrap();
        let facts = store.activity_since(&before, "alice", 2).unwrap();
        assert!(completions_for_receipt(&facts.completions, &[]).is_empty());
    }

    #[test]
    fn shared_completion_uses_the_leased_node_and_its_committed_content_revision() {
        let f = Fixture::new();
        let mut store = f.open();
        let leaf = Scope::new("work/task-a.md");
        store
            .execute(&Command::Register {
                leaf: leaf.clone(),
                content: "before".into(),
            })
            .unwrap();
        let lease = claim(&mut store, "alice", leaf.as_str(), Some("graph#task-a"));
        let outcome = store
            .execute(&Command::Commit {
                owner: "alice".into(),
                lease,
                changes: [(leaf.clone(), Some("done-content-hash".into()))].into(),
                source: Some(mdkg_coord::Source {
                    commit: "1".repeat(40),
                    branch: "agent/alice".into(),
                }),
                now: 1,
                landed: false,
            })
            .unwrap();
        let Outcome::Committed { changeset } = outcome else {
            panic!("{outcome:?}")
        };
        let before = store.activity_cursor().unwrap();
        let mark = Command::SetWork {
            leaf,
            status: Some("done".into()),
            changeset: Some(changeset),
            actor: "alice".into(),
            now: 2,
        };
        store.execute(&mark).unwrap();
        let first = store.activity_since(&before, "alice", 3).unwrap();
        assert_eq!(
            first.completions,
            [Completion {
                node: "graph#task-a".into(),
                revision: "done-content-hash".into()
            }]
        );
        store.execute(&mark).unwrap();
        assert!(
            store
                .activity_since(&first.through, "alice", 3)
                .unwrap()
                .completions
                .is_empty()
        );
    }

    #[test]
    fn inconsistent_completion_identity_refuses_and_rolls_back_the_retry() {
        let f = Fixture::new();
        let mut store = f.open();
        let leaf = Scope::new("a");
        store
            .complete_local(&leaf, leaf.as_str(), "alice", "revision", 1)
            .unwrap();
        let key = completion_key("a", "revision").unwrap();
        store
            .conn
            .execute("UPDATE meta SET value = '999999' WHERE key = ?1", [key])
            .unwrap();
        let journal = store.journal().unwrap();
        let log = store.log_texts().unwrap();
        let state = store.coordinator().unwrap();
        assert!(
            store
                .complete_local(&leaf, leaf.as_str(), "alice", "revision", 2)
                .is_err()
        );
        assert_eq!(store.journal().unwrap(), journal);
        assert_eq!(store.log_texts().unwrap(), log);
        assert_eq!(store.coordinator().unwrap(), state);
    }

    #[test]
    fn activity_limits_refuse_instead_of_omitting_or_decoding_excess_rows() {
        let conn = Connection::open_in_memory().unwrap();
        let mut state = Coordinator::new(1_000);
        let outcome = state.apply(&command("alice", "a", None));
        assert!(matches!(outcome, Outcome::Lease { .. }));
        conn.execute_batch("CREATE TABLE journal(seq INTEGER, command TEXT, outcome TEXT); CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute(
            "INSERT INTO journal VALUES (1, ?1, ?2)",
            params![
                serde_json::to_string(&command("alice", "a", None)).unwrap(),
                serde_json::to_string(&outcome).unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            entries(&conn, &state, 0, 1, "alice", 1, 4096)
                .unwrap()
                .claims
                .len(),
            1
        );
        assert!(
            matches!(entries(&conn, &Coordinator::new(1_000), 0, 1, "alice", 1, 4096), Err(StoreError::Log(e)) if e.contains("unavailable in the snapshot"))
        );
        for (count, bytes) in [(0, 4096), (1, 1)] {
            assert!(
                matches!(entries(&conn, &state, 0, 1, "alice", count, bytes), Err(StoreError::Log(e)) if e.contains("read limit"))
            );
        }
        conn.execute("UPDATE journal SET command = 'invalid JSON'", [])
            .unwrap();
        assert!(matches!(
            entries(&conn, &state, 0, 1, "alice", 1, 4096),
            Err(StoreError::Json(_))
        ));
    }
}
