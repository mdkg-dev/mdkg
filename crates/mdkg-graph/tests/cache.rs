//! The graph cache (edd-01a18867): read-only modes, ctime verification,
//! rescue before rebuild, and write sessions.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_format::Uuid7;
use mdkg_graph::cache::{self, Cache, DriftKind, KeptKind};
use mdkg_graph::graphref;
use mdkg_graph::refwrite::{Change, GraphLock, LockMode, RefWriter, Request};

const GRAPH: &str = "refs/heads/mdkg-graph";
const GRAPH_ID: &str = "0192f3a0-0000-7000-8000-000000000000";
const CONFIG: &str = r#"{"graph_id":"0192f3a0-0000-7000-8000-000000000000"}"#;

struct Repo(PathBuf);
impl Drop for Repo {
    fn drop(&mut self) {
        // The cache is read-only by design.
        Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status()
            .ok();
        std::fs::remove_dir_all(&self.0).ok();
    }
}
impl Repo {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-cache-{}", Uuid7::now()));
        std::fs::create_dir_all(&dir).expect("fixture");
        let repo = Self(dir.canonicalize().expect("canonical"));
        repo.git(&["init", "-q", "-b", "trunk"]);
        repo.git(&["config", "user.name", "Cache Test"]);
        repo.git(&["config", "user.email", "cache@example.invalid"]);
        repo.git(&["commit", "-q", "--allow-empty", "-m", "code"]);
        repo.git(&["config", "mdkg.graph.ref", "mdkg-graph"]);
        let graph = graphref::resolve(&repo.0)
            .expect("resolve")
            .expect("ref mode");
        let writer = graph
            .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
            .expect("creator");
        writer
            .write(&Request {
                id: "seed".into(),
                base: None,
                changes: vec![
                    put(".mdkg/config.json", CONFIG.as_bytes()),
                    put(".mdkg/design/a.md", b"alpha\n"),
                    put(".mdkg/design/sub/b.md", b"beta\n"),
                ],
                message: "seed".into(),
                trailers: vec![],
            })
            .expect("seed graph");
        graph.complete_creation(&repo.0).expect("registered");
        repo
    }
    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd
            .current_dir(&self.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8").trim().into()
    }
    fn git_input(&self, args: &[&str], input: &[u8]) -> String {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .current_dir(&self.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("git");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("in");
        let out = child.wait_with_output().expect("git");
        assert!(out.status.success(), "{args:?}");
        String::from_utf8(out.stdout).expect("utf8").trim().into()
    }
    fn writer(&self) -> RefWriter {
        graphref::resolve(&self.0)
            .expect("resolve")
            .expect("ref mode")
            .writer(&self.0, LockMode::Local)
            .expect("writer")
    }
    fn tip(&self) -> String {
        self.git(&["rev-parse", GRAPH])
    }
}

fn put(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        bytes: bytes.into(),
    }
}

fn paths() -> Vec<String> {
    vec![".mdkg".into()]
}

fn lock(writer: &RefWriter) -> GraphLock {
    GraphLock::exclusive(writer.common(), LockMode::Local).expect("lock")
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).expect("stat").mode() & 0o777
}

#[test]
fn refresh_builds_a_read_only_copy_and_follows_the_tip() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert!(done.drift.is_empty() && done.rescued.is_none());
    let a = cache.root().join(".mdkg/design/a.md");
    assert_eq!(std::fs::read(&a).expect("a"), b"alpha\n");
    assert_eq!(mode(&a), 0o444);
    assert_eq!(mode(&cache.root().join(".mdkg/design")), 0o555);
    assert!(cache.verify().expect("verify"));
    assert_eq!(
        cache.manifest().expect("manifest").expect("some").commit,
        repo.tip()
    );
    drop(lock);

    let base = writer.snapshot().expect("tip");
    writer
        .write(&Request {
            id: "edit".into(),
            base: Some(base),
            changes: vec![
                put(".mdkg/design/a.md", b"alpha 2\n"),
                Change::Delete {
                    path: ".mdkg/design/sub/b.md".into(),
                },
                put(".mdkg/work/c.md", b"gamma\n"),
            ],
            message: "edit".into(),
            trailers: vec![],
        })
        .expect("write");
    let lock = self::lock(&writer);
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert_eq!(done.updated, 3);
    assert_eq!(std::fs::read(&a).expect("a"), b"alpha 2\n");
    assert!(!cache.root().join(".mdkg/design/sub").exists());
    assert_eq!(
        std::fs::read(cache.root().join(".mdkg/work/c.md")).expect("c"),
        b"gamma\n"
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn a_tampered_file_is_caught_by_ctime_rescued_and_rebuilt() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let a = cache.root().join(".mdkg/design/a.md");
    let before = std::fs::metadata(&a).expect("stat");
    // Same length, mtime restored: only ctime gives it away.
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::fs::write(&a, b"ALPHA\n").expect("tamper");
    let mtime = before.modified().expect("mtime");
    std::fs::File::options()
        .write(true)
        .open(&a)
        .expect("open")
        .set_modified(mtime)
        .expect("restore mtime");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    let after = std::fs::metadata(&a).expect("stat");
    assert_eq!(after.modified().expect("mtime"), mtime);
    assert_eq!((after.len(), after.ino()), (before.len(), before.ino()));
    let drift = cache.diff_against_manifest().expect("diff");
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].path, ".mdkg/design/a.md");
    assert_eq!(drift[0].kind, DriftKind::Modified);
    assert!(!cache.verify().expect("verify"));
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let rescued = done.rescued.expect("rescued");
    assert_eq!(
        std::fs::read(rescued.join("files/.mdkg/design/a.md")).expect("rescued copy"),
        b"ALPHA\n"
    );
    assert!(rescued.starts_with(repo.0.join(".git/mdkg/graph/rescued")));
    assert_eq!(std::fs::read(&a).expect("a"), b"alpha\n");
    assert!(cache.verify().expect("verify"));
    // Authority never reads the cache.
    assert_eq!(
        writer
            .read(&writer.snapshot().expect("tip"), ".mdkg/design/a.md")
            .expect("read"),
        Some(b"alpha\n".to_vec())
    );
}

#[test]
fn a_shared_lock_takes_no_session_or_refresh() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let shared = GraphLock::shared(writer.common(), LockMode::Local).expect("shared");
    let err = cache
        .refresh(&shared, &repo.tip(), &paths())
        .expect_err("refused");
    assert_eq!(err.code(), "graph.lock_required");
    let err = cache
        .session(&shared, "x", "new", "tester")
        .expect_err("refused");
    assert_eq!(err.code(), "graph.lock_required");
}

#[test]
fn sessions_and_refreshes_need_the_exclusive_lock() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let shared = GraphLock::shared(writer.common(), LockMode::Local).expect("shared");
    assert!(cache.refresh(&shared, &repo.tip(), &paths()).is_err());
    assert!(cache.session(&shared, "x", "new", "tester").is_err());
    // Shared holders don't block each other, and do block an exclusive one.
    let second = GraphLock::shared(writer.common(), LockMode::Local).expect("second shared");
    assert!(
        GraphLock::try_exclusive(writer.common(), LockMode::Local)
            .expect("try")
            .is_none()
    );
    drop((shared, second));
    assert!(
        GraphLock::try_exclusive(writer.common(), LockMode::Local)
            .expect("try")
            .is_some()
    );
}

/// A file in the cache replaced by someone else (folders are writable
/// during a session): written beside it, then renamed over it.
fn hand_edit(root: &Path, path: &str, bytes: &[u8]) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    let tmp = root.join(format!("{path}.hand"));
    std::fs::write(&tmp, bytes).expect("tmp");
    std::fs::rename(&tmp, &file).expect("rename");
}

fn publish(writer: &RefWriter, lock: &GraphLock, id: &str, changes: Vec<Change>) -> String {
    let request = Request {
        id: id.into(),
        base: Some(writer.snapshot().expect("tip")),
        changes,
        message: id.into(),
        trailers: vec![],
    };
    let prepared = writer.prepare(lock, &request).expect("prepare");
    writer.publish(lock, &prepared).expect("publish");
    writer.unpin(lock, &prepared).expect("unpin");
    prepared.commit
}

#[test]
fn a_session_publishes_its_writes() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "session", "new", "tester")
        .expect("session");
    let root = session.root().to_owned();
    assert_eq!(
        session.read(".mdkg/design/a.md").expect("read"),
        Some(b"alpha\n".to_vec())
    );
    session
        .write(".mdkg/design/new.md", b"new\n")
        .expect("write");
    session.delete(".mdkg/design/sub/b.md").expect("delete");
    assert!(session.write("source.txt", b"x").is_err());
    assert!(session.write(".mdkg/../x", b"x").is_err());
    let record = cache.session_record().expect("record").expect("some");
    assert_eq!(
        record.paths,
        [".mdkg/design/new.md", ".mdkg/design/sub/b.md"]
    );
    assert_eq!(record.temps.len(), 1);
    // What the session wrote reads back as its own.
    assert_eq!(
        session.read(".mdkg/design/new.md").expect("read own"),
        Some(b"new\n".to_vec())
    );
    let changes = session.changes().expect("changes");
    assert_eq!(
        changes,
        vec![
            put(".mdkg/design/new.md", b"new\n"),
            Change::Delete {
                path: ".mdkg/design/sub/b.md".into()
            },
        ]
    );
    let commit = publish(&writer, &lock, "session", changes);
    let ended = session.finish(&commit).expect("finish");
    assert!(ended.unclassified.is_empty());
    assert_eq!(cache.session_record().expect("record"), None);
    assert_eq!(
        cache.manifest().expect("m").expect("some").commit,
        repo.tip()
    );
    assert!(cache.verify().expect("clean after finish"));
    assert_eq!(mode(&root.join(".mdkg/design/new.md")), 0o444);
    assert_eq!(mode(&root.join(".mdkg/design")), 0o555);
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert!(done.drift.is_empty());
}

#[test]
fn a_refused_session_restores_with_no_rescue() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "refused", "done", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session
        .write(".mdkg/design/a.md", b"status: done\n")
        .expect("write");
    session
        .write(".mdkg/design/c.md", b"created\n")
        .expect("create");
    let ended = session.abort().expect("abort");
    assert!(ended.unclassified.is_empty());
    assert_eq!(
        std::fs::read(root.join(".mdkg/design/a.md")).expect("restored"),
        b"alpha\n"
    );
    assert!(!root.join(".mdkg/design/c.md").exists());
    assert!(
        !repo.0.join(".git/mdkg/graph/rescued").exists(),
        "nothing rescued"
    );
    assert_eq!(cache.session_record().expect("record"), None);
    assert!(cache.verify().expect("verify"));
    assert_eq!(mode(&root.join(".mdkg")), 0o555);
}

#[test]
fn an_edit_arriving_during_a_session_is_never_overwritten() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "preimage", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    hand_edit(&root, ".mdkg/design/a.md", b"by hand\n");
    let err = session
        .write(".mdkg/design/a.md", b"op\n")
        .expect_err("preimage");
    assert!(err.to_string().contains("graph.cache_edit"), "{err}");
    assert!(session.read(".mdkg/design/a.md").is_err());
    assert!(session.will_write(".mdkg/design/a.md").is_err());
    assert_eq!(
        std::fs::read(root.join(".mdkg/design/a.md")).expect("kept"),
        b"by hand\n"
    );
    // The op is refused; the edit is set aside, non-importable.
    let ended = session.abort().expect("abort");
    let folder = ended.unclassified.first().cloned().expect("set aside");
    assert_eq!(
        std::fs::read(folder.join("files/.mdkg/design/a.md")).expect("copy"),
        b"by hand\n"
    );
    assert_eq!(
        cache::kept(&folder).expect("marker").expect("kept").kind,
        KeptKind::Unclassified
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn an_undeclared_edit_during_a_session_is_set_aside_unclassified() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "declared", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session
        .write(".mdkg/design/a.md", b"alpha 2\n")
        .expect("write");
    let changes = session.changes().expect("changes");
    // Someone edits another file while the store records and publishes.
    hand_edit(&root, ".mdkg/design/sub/b.md", b"by hand\n");
    assert_eq!(session.undeclared().expect("undeclared").len(), 1);
    let err = session.changes().expect_err("unleased");
    assert!(err.to_string().contains("graph.unleased_edit"), "{err}");
    let commit = publish(&writer, &lock, "declared", changes);
    let ended = session.finish(&commit).expect("finish");
    let folder = ended.unclassified.first().cloned().expect("set aside");
    assert_eq!(
        std::fs::read(folder.join("files/.mdkg/design/sub/b.md")).expect("copy"),
        b"by hand\n"
    );
    assert_eq!(
        cache::unfinished(&folder).expect("m").expect("record").id,
        "declared"
    );
    // Never recorded as published, never wiped: restored, and kept.
    assert_eq!(
        std::fs::read(root.join(".mdkg/design/sub/b.md")).expect("b"),
        b"beta\n"
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn finish_sets_aside_a_declared_file_that_differs_from_the_commit() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "checked", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session
        .write(".mdkg/design/a.md", b"alpha 2\n")
        .expect("write");
    let commit = publish(
        &writer,
        &lock,
        "checked",
        session.changes().expect("changes"),
    );
    // Changed again after the commit was prepared.
    hand_edit(&root, ".mdkg/design/a.md", b"later\n");
    let ended = session.finish(&commit).expect("finish");
    assert_eq!(
        std::fs::read(root.join(".mdkg/design/a.md")).expect("a"),
        b"alpha 2\n"
    );
    let folder = ended.unclassified.first().cloned().expect("set aside");
    assert_eq!(
        std::fs::read(folder.join("files/.mdkg/design/a.md")).expect("copy"),
        b"later\n"
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn finish_on_a_tip_moved_meanwhile_lets_the_next_refresh_catch_up() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    let first = repo.tip();
    cache.refresh(&lock, &first, &paths()).expect("refresh");
    let mut session = cache
        .session(&lock, "moved", "update", "tester")
        .expect("session");
    session
        .write(".mdkg/design/a.md", b"alpha 2\n")
        .expect("write");
    // Someone moves the graph ref without the lock.
    let tip = repo.tip();
    let tree = {
        let index = repo.0.join(".git/other-index");
        let env = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo.0)
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        env(&["read-tree", &tip]);
        let blob = repo.git_input(&["hash-object", "-w", "--stdin"], b"theirs\n");
        env(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},.mdkg/design/theirs.md"),
        ]);
        env(&["write-tree"])
    };
    let moved = repo.git(&["commit-tree", &tree, "-p", &tip, "-m", "external"]);
    repo.git(&["update-ref", GRAPH, &moved, &tip]);
    let commit = publish(&writer, &lock, "moved", session.changes().expect("changes"));
    session.finish(&commit).expect("finish");
    assert_eq!(cache.manifest().expect("m").expect("some").commit, first);
    assert!(cache.verify().expect("verify"));
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert_eq!(
        std::fs::read(cache.root().join(".mdkg/design/theirs.md")).expect("caught up"),
        b"theirs\n"
    );
}

#[test]
fn a_crashed_session_is_kept_as_unfinished_never_as_hand_edits() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "crashed-op", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
        let root = session.root().to_owned();
        // A person's edit at the same time, which the record doesn't
        // account for, and a writer's own unrecorded temporary file.
        hand_edit(&root, ".mdkg/design/hand.md", b"by hand\n");
        std::fs::write(
            root.join(".mdkg/design/.mdkg-write-a.md-0192f3a0-0000-7000-8000-000000000000.tmp"),
            b"half",
        )
        .expect("temp");
        // The process dies here: no finish, no abort.
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert!(done.rescued.is_none(), "no hand edits in a session's crash");
    let ended = done.session.expect("ended session");
    assert_eq!(ended.record.id, "crashed-op");
    assert_eq!(ended.record.op, "done");
    let kept = ended.kept.first().cloned().expect("kept");
    assert!(kept.starts_with(repo.0.join(".git/mdkg/graph/rescued")));
    assert!(
        kept.to_string_lossy().ends_with("-unfinished-crashed-op"),
        "{}",
        kept.display()
    );
    assert_eq!(
        std::fs::read(kept.join("files/.mdkg/design/a.md")).expect("kept copy"),
        b"status: done\n"
    );
    let found = cache::kept(&kept).expect("marker").expect("kept");
    assert_eq!(
        (found.kind, found.record.id.as_str()),
        (KeptKind::Unfinished, "crashed-op")
    );
    let unclassified = ended.unclassified.first().cloned().expect("unclassified");
    assert!(unclassified.join("files/.mdkg/design/hand.md").is_file());
    assert!(
        unclassified
            .join("files/.mdkg/design/.mdkg-write-a.md-0192f3a0-0000-7000-8000-000000000000.tmp")
            .is_file()
    );
    assert_eq!(
        cache::kept(&unclassified)
            .expect("marker")
            .expect("kept")
            .kind,
        KeptKind::Unclassified
    );
    assert_eq!(
        std::fs::read(cache.root().join(".mdkg/design/a.md")).expect("restored"),
        b"alpha\n"
    );
    assert_eq!(cache.session_record().expect("record"), None);
    assert!(cache.verify().expect("verify"));
}

#[test]
fn a_crashed_sessions_recorded_temporary_file_is_dropped() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let temp = ".mdkg/design/.mdkg-session-a.md-1234.tmp";
    // The session recorded its output before writing the temporary file.
    let output = repo.git_input(&["hash-object", "--stdin"], b"half");
    std::fs::write(
        repo.0.join(".git/mdkg/graph/session.json"),
        serde_json::json!({"v": 1, "id": "tmp-crash", "op": "update", "actor": "t",
            "paths": [".mdkg/design/a.md"], "temps": [temp],
            "written": {".mdkg/design/a.md": output}})
        .to_string(),
    )
    .expect("record");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::write(root.join(temp), b"half").expect("temp");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let ended = done.session.expect("ended");
    assert!(ended.kept.is_empty());
    assert!(ended.unclassified.is_empty());
    assert!(done.rescued.is_none());
    assert!(!root.join(temp).exists());
    assert!(cache.verify().expect("verify"));
}

#[test]
fn a_session_published_by_replay_keeps_no_copy() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "published", "update", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"alpha 2\n")
            .expect("write");
        publish(
            &writer,
            &lock,
            "published",
            session.changes().expect("changes"),
        );
        // Crash before the session finishes.
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let ended = done.session.expect("ended");
    assert!(ended.kept.is_empty());
    assert!(ended.unclassified.is_empty());
    assert!(done.rescued.is_none());
    assert_eq!(
        std::fs::read(cache.root().join(".mdkg/design/a.md")).expect("a"),
        b"alpha 2\n"
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn links_in_the_cache_are_never_followed() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let outside = repo.0.join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("a.md"), b"outside\n").expect("outside file");
    std::fs::write(outside.join("secret"), b"credential").expect("secret");
    let root = cache.root().to_owned();
    // During a session: a folder swapped for a link, a file for a link.
    let mut session = cache
        .session(&lock, "links", "update", "tester")
        .expect("session");
    session.write(".mdkg/design/a.md", b"op\n").expect("write");
    std::fs::rename(root.join(".mdkg/design/sub"), repo.0.join("moved-sub")).expect("mv");
    std::os::unix::fs::symlink(&outside, root.join(".mdkg/design/sub")).expect("dir link");
    std::fs::remove_file(root.join(".mdkg/design/a.md")).expect("rm");
    std::os::unix::fs::symlink(outside.join("secret"), root.join(".mdkg/design/a.md"))
        .expect("file link");
    assert!(session.read(".mdkg/design/sub/b.md").is_err());
    assert!(session.changes().is_err());
    let ended = session.abort().expect("abort");
    // Nothing outside was read, removed or written.
    assert_eq!(
        std::fs::read(outside.join("a.md")).expect("outside"),
        b"outside\n"
    );
    assert_eq!(
        std::fs::read(outside.join("secret")).expect("secret"),
        b"credential"
    );
    assert!(!outside.join("b.md").exists());
    for folder in &ended.unclassified {
        assert!(
            !folder.join("files/.mdkg/design/a.md").exists(),
            "no link target copied"
        );
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let _ = done;
    assert_eq!(
        std::fs::read(outside.join("a.md")).expect("outside"),
        b"outside\n"
    );
    assert!(cache.verify().expect("verify"));
    assert!(
        std::fs::symlink_metadata(root.join(".mdkg/design/sub"))
            .expect("sub")
            .is_dir()
    );
}

#[test]
fn a_tampered_session_record_cannot_name_a_path_outside() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    for record in [
        serde_json::json!({"v": 1, "id": "../../../evil", "op": "x", "actor": "t", "paths": []}),
        serde_json::json!({"v": 1, "id": "ok", "op": "x", "actor": "t", "paths": ["/etc/passwd"]}),
        serde_json::json!({"v": 1, "id": "ok", "op": "x", "actor": "t", "paths": [".mdkg/../../x"]}),
    ] {
        std::fs::write(
            repo.0.join(".git/mdkg/graph/session.json"),
            record.to_string(),
        )
        .expect("record");
        assert!(cache.session_record().is_err(), "{record}");
        assert!(cache.refresh(&lock, &repo.tip(), &paths()).is_err());
    }
    assert!(!repo.0.join("evil").exists());
}

#[test]
fn a_hand_edit_over_the_limit_is_never_truncated_or_replaced() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    let big = vec![b'x'; (cache::MAX_FILE_BYTES + 1) as usize];
    std::fs::write(root.join(".mdkg/design/big.md"), &big).expect("big");
    assert!(cache.refresh(&lock, &repo.tip(), &paths()).is_err());
    assert_eq!(
        std::fs::metadata(root.join(".mdkg/design/big.md"))
            .expect("kept")
            .len(),
        big.len() as u64
    );
}

#[test]
fn edits_under_a_new_folder_are_rescued_before_a_whole_rebuild() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let root = cache.root().to_owned();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::create_dir_all(root.join("apps/web/.mdkg/design")).expect("mkdir");
    std::fs::write(root.join("apps/web/.mdkg/design/n.md"), b"new folder\n").expect("edit");
    let done = cache
        .refresh(
            &lock,
            &repo.tip(),
            &[".mdkg".into(), "apps/web/.mdkg".into()],
        )
        .expect("refresh");
    let rescued = done.rescued.expect("rescued");
    assert_eq!(
        std::fs::read(rescued.join("files/apps/web/.mdkg/design/n.md")).expect("copy"),
        b"new folder\n"
    );
    assert!(cache.verify().expect("verify"));
}

#[test]
fn rescues_keep_deletions_and_never_reuse_a_folder() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "tombstone", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session.write(".mdkg/design/a.md", b"op\n").expect("write");
    // A person deletes an undeclared file during the session.
    std::fs::remove_file(root.join(".mdkg/design/sub/b.md")).expect("rm");
    let first = session
        .abort()
        .expect("abort")
        .unclassified
        .first()
        .cloned()
        .expect("set aside");
    let drift = std::fs::read_to_string(first.join("DRIFT.json")).expect("drift list");
    assert!(
        drift.contains(".mdkg/design/sub/b.md") && drift.contains("deleted"),
        "{drift}"
    );
    // A marker file in the cache can't meet the folder's own marker.
    let session = cache
        .session(&lock, "tombstone", "update", "tester")
        .expect("session");
    hand_edit(&root, ".mdkg/UNFINISHED.json", b"not a marker");
    let second = session
        .abort()
        .expect("abort")
        .unclassified
        .first()
        .cloned()
        .expect("set aside");
    assert_ne!(first, second, "a new folder per attempt");
    assert!(second.join("files/.mdkg/UNFINISHED.json").is_file());
    assert_eq!(
        cache::kept(&second).expect("marker").expect("kept").kind,
        KeptKind::Unclassified
    );
}

#[test]
fn a_chmod_alone_is_a_changed_preimage() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "chmod", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design/a.md"),
        std::fs::Permissions::from_mode(0o555),
    )
    .expect("chmod");
    let err = session
        .write(".mdkg/design/a.md", b"op\n")
        .expect_err("mode changed");
    assert!(err.to_string().contains("graph.cache_edit"), "{err}");
}

#[test]
fn a_hand_edit_after_a_session_write_is_never_published() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "published-edit", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session.write(".mdkg/design/a.md", b"op\n").expect("write");
    hand_edit(&root, ".mdkg/design/a.md", b"by hand\n");
    let err = session.changes().expect_err("not the op's output");
    assert!(err.to_string().contains("graph.cache_edit"), "{err}");
    session.delete(".mdkg/design/sub/b.md").expect("delete");
    hand_edit(&root, ".mdkg/design/sub/b.md", b"recreated\n");
    assert!(session.changes().is_err());
}

#[test]
fn a_retried_crash_refresh_reports_what_it_kept() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "progress", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
    }
    // A first recovery that got as far as keeping the files, then crashed:
    // simulate by keeping, then putting the record's progress back.
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let kept = done
        .session
        .expect("ended")
        .kept
        .first()
        .cloned()
        .expect("kept");
    let record = serde_json::json!({"v": 1, "id": "progress", "op": "done", "actor": "tester",
        "paths": [".mdkg/design/a.md"], "temps": [], "written": {},
        "kept": kept.to_string_lossy(), "unclassified": null});
    std::fs::write(
        repo.0.join(".git/mdkg/graph/session.json"),
        record.to_string(),
    )
    .expect("record");
    let again = cache.refresh(&lock, &repo.tip(), &paths()).expect("retry");
    assert_eq!(again.session.expect("ended").kept, vec![kept]);
}

#[test]
fn an_oversized_whole_rebuild_is_refused_before_anything_is_removed() {
    // The 1 GiB check runs from the tree's sizes before the reset; a tree
    // that large can't be built in a test, so this checks the order: a
    // refused refresh (here, an over-64 MiB file) leaves the cache intact.
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let big = vec![b'x'; (cache::MAX_FILE_BYTES + 1) as usize];
    publish(
        &writer,
        &lock,
        "big",
        vec![put(".mdkg/design/big.md", &big)],
    );
    assert!(
        cache
            .refresh(&lock, &repo.tip(), &[".mdkg".into(), "apps/.mdkg".into()])
            .is_err()
    );
    assert!(cache.root().join(".mdkg/design/a.md").is_file());
}

#[test]
fn a_recorded_temp_name_holding_anything_else_is_kept_not_dropped() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let temp = ".mdkg/design/.mdkg-session-a.md-5678.tmp";
    // A crash before the temporary file was created; someone else's file
    // sits at that name.
    std::fs::write(
        repo.0.join(".git/mdkg/graph/session.json"),
        serde_json::json!({"v": 1, "id": "tmp-other", "op": "update", "actor": "t",
            "paths": [".mdkg/design/a.md"], "temps": [temp],
            "written": {".mdkg/design/a.md": "0000000000000000000000000000000000000001"}})
        .to_string(),
    )
    .expect("record");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::write(root.join(temp), b"someone else's").expect("file");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let folder = done
        .session
        .expect("ended")
        .unclassified
        .first()
        .cloned()
        .expect("kept");
    assert!(folder.join("files").join(temp).is_file());
}

#[test]
fn a_crafted_manifest_commit_is_never_passed_to_git() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let manifest = repo.0.join(".git/mdkg/graph/cache.json");
    let text = std::fs::read_to_string(&manifest).expect("manifest");
    let mut v: serde_json::Value = serde_json::from_str(&text).expect("json");
    let target = repo.0.join("overwritten");
    v["commit"] = serde_json::json!(format!("--output={}", target.display()));
    std::fs::write(&manifest, v.to_string()).expect("crafted");
    assert!(cache.refresh(&lock, &repo.tip(), &paths()).is_err());
    assert!(!target.exists());
}

#[test]
fn a_hard_linked_cache_file_is_never_read_or_chmodded() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let outside = repo.0.join("private.txt");
    std::fs::write(&outside, b"alpha\n").expect("outside");
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).expect("0600");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::remove_file(root.join(".mdkg/design/a.md")).expect("rm");
    std::fs::hard_link(&outside, root.join(".mdkg/design/a.md")).expect("link");
    let mut session = cache
        .session(&lock, "hardlink", "update", "tester")
        .expect("session");
    assert!(session.read(".mdkg/design/a.md").is_err());
    let _ = session.abort();
    assert_eq!(mode(&outside), 0o600);
}

#[test]
fn the_aggregate_refresh_limit_refuses_before_a_whole_rebuild() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache").with_refresh_limit(8);
    let lock = lock(&writer);
    // Every file is small, but together they're over 8 bytes.
    assert!(cache.refresh(&lock, &repo.tip(), &paths()).is_err());
    let full = Cache::open(&repo.0).expect("cache");
    full.refresh(&lock, &repo.tip(), &paths()).expect("refresh");
    // A changed folder set means a whole rebuild: refused before the reset.
    let err = cache
        .refresh(&lock, &repo.tip(), &[".mdkg".into(), "apps/.mdkg".into()])
        .expect_err("over the limit");
    assert!(err.to_string().contains("limit"), "{err}");
    assert!(full.root().join(".mdkg/design/a.md").is_file());
    assert!(full.verify().expect("intact"));
}

#[test]
fn an_incomplete_rescue_folder_is_never_visible() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let root = cache.root().to_owned();
    let base = repo.0.join(".git/mdkg/graph/rescued");
    std::fs::create_dir_all(&base).expect("rescued");
    let partial = |name: &str, path: &str, bytes: &[u8]| {
        let dir = base.join(format!(".{name}.partial"));
        let file = dir.join("files").join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(&file, bytes).expect("copy");
        let drift =
            serde_json::json!({"drift": [{"path": path, "kind": "modified", "mode": "100644"}]});
        std::fs::write(dir.join("DRIFT.json"), drift.to_string()).expect("drift");
        dir
    };
    // A crash while a rescue was built (orchestrator #8066):
    // - one whose copy matches the cache file now (the edit below is
    //   still in place) is redundant, removed;
    let redundant = partial(
        "20261029T000000Z-0123456789ab-hand",
        ".mdkg/design/a.md",
        b"by hand\n",
    );
    // - one holding the only copy of an edit the cache lost is kept,
    //   visible and reported;
    let only_copy = partial(
        "20261029T000001Z-0123456789ac-hand",
        ".mdkg/design/sub/b.md",
        b"an edit that rolled back\n",
    );
    // - anything not in the generated form is never touched.
    let notes = base.join(".notes.partial");
    std::fs::create_dir_all(&notes).expect("notes");
    std::fs::write(notes.join("keep.txt"), b"mine").expect("notes file");
    hand_edit_open(&root, ".mdkg/design/a.md", b"by hand\n");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert!(!redundant.exists(), "proven redundant: removed");
    assert!(!only_copy.exists());
    let orphan = base.join("orphan-20261029T000001Z-0123456789ac-hand");
    assert_eq!(done.orphaned, vec![orphan.clone()]);
    assert_eq!(
        std::fs::read(orphan.join("files/.mdkg/design/sub/b.md")).expect("kept"),
        b"an edit that rolled back\n"
    );
    assert!(notes.join("keep.txt").is_file(), "never touched");
    // The new rescue holds the current edit, bytes and mode.
    let rescued = done.rescued.expect("rescued");
    assert_eq!(
        std::fs::read(rescued.join("files/.mdkg/design/a.md")).expect("copy"),
        b"by hand\n"
    );
    let drift = std::fs::read_to_string(rescued.join("DRIFT.json")).expect("drift");
    assert!(drift.contains("100644"), "the mode is recorded: {drift}");
}

fn hand_edit_open(root: &Path, path: &str, bytes: &[u8]) {
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    hand_edit(root, path, bytes);
}

#[test]
fn an_output_edited_back_to_its_old_bytes_refuses_the_op() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "back", "update", "tester")
        .expect("session");
    let root = session.root().to_owned();
    session.write(".mdkg/design/a.md", b"op A\n").expect("A");
    session
        .write(".mdkg/design/sub/b.md", b"op B\n")
        .expect("B");
    // Someone puts A back as the manifest had it.
    hand_edit(&root, ".mdkg/design/a.md", b"alpha\n");
    let err = session.changes().expect_err("A was dropped");
    assert!(err.to_string().contains("graph.cache_edit"), "{err}");
}

#[test]
fn a_retried_recovery_keeps_drift_made_after_the_first_attempt() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "retry", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let kept = done
        .session
        .expect("ended")
        .kept
        .first()
        .cloned()
        .expect("kept");
    // A first recovery that kept the files and crashed before finishing:
    // its progress is in the record. Then someone edits another file.
    let record = serde_json::json!({"v": 1, "id": "retry", "op": "done", "actor": "tester",
        "paths": [".mdkg/design/a.md"], "temps": [], "written": {},
        "kept": kept.to_string_lossy(), "unclassified": null});
    std::fs::write(
        repo.0.join(".git/mdkg/graph/session.json"),
        record.to_string(),
    )
    .expect("record");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::set_permissions(
        root.join(".mdkg/design/sub"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    hand_edit(&root, ".mdkg/design/sub/b.md", b"after the crash\n");
    let again = cache.refresh(&lock, &repo.tip(), &paths()).expect("retry");
    let ended = again.session.expect("ended");
    assert_eq!(ended.kept, vec![kept]);
    let folder = ended.unclassified.first().cloned().expect("new drift kept");
    assert_eq!(
        std::fs::read(folder.join("files/.mdkg/design/sub/b.md")).expect("copy"),
        b"after the crash\n"
    );
}

/// Terra #7173: a crash after an earlier attempt recorded an unclassified
/// folder still keeps the session's own outputs as unfinished.
#[test]
fn a_retry_after_a_recorded_set_aside_keeps_session_outputs_unfinished() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "set-aside", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
        // The process dies here.
    }
    // An earlier attempt had recorded one unclassified folder, then died.
    let earlier = repo
        .0
        .join(".git/mdkg/graph/rescued/20261029T000000Z-feed-unclassified-set-aside");
    std::fs::create_dir_all(earlier.join("files")).expect("earlier");
    let path = repo.0.join(".git/mdkg/graph/session.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("json");
    record["unclassified"] = serde_json::json!([earlier.to_string_lossy()]);
    std::fs::write(&path, record.to_string()).expect("record");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let ended = done.session.expect("ended");
    let kept = ended.kept.first().cloned().expect("kept as unfinished");
    assert_eq!(
        std::fs::read(kept.join("files/.mdkg/design/a.md")).expect("copy"),
        b"status: done\n"
    );
    assert_eq!(
        cache::kept(&kept).expect("marker").expect("kept").kind,
        KeptKind::Unfinished
    );
    assert!(ended.unclassified.contains(&earlier), "still reported");
}

/// Terra #7173: a recorded temp name holding another target's output is
/// set aside, never deleted as the session's.
#[test]
fn a_temp_holding_another_targets_output_is_set_aside() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let temp;
    {
        let mut session = cache
            .session(&lock, "temps", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"for a\n")
            .expect("write a");
        session
            .write(".mdkg/design/sub/b.md", b"for b\n")
            .expect("write b");
        temp = cache
            .session_record()
            .expect("record")
            .expect("record")
            .temps
            .into_iter()
            .find(|t| t.contains("-a.md-"))
            .expect("a's temp");
        // a's temp name now holds b's output: not a's.
        std::fs::write(cache.root().join(&temp), b"for b\n").expect("collide");
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let ended = done.session.expect("ended");
    assert!(
        ended
            .unclassified
            .iter()
            .any(|f| f.join("files").join(&temp).is_file()),
        "{ended:?}"
    );
}

/// orchestrator #7290: a refresh over a link whose target isn't UTF-8 is
/// refused, and the link stays.
#[test]
fn a_link_with_a_non_utf8_target_refuses_the_refresh() {
    use std::os::unix::ffi::OsStrExt as _;
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let root = cache.root().to_owned();
    std::fs::set_permissions(
        root.join(".mdkg/design"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::os::unix::fs::symlink(
        std::ffi::OsStr::from_bytes(b"caf\xe9"),
        root.join(".mdkg/design/link.md"),
    )
    .expect("link");
    let err = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect_err("refused");
    assert!(err.to_string().contains("isn't UTF-8"), "{err}");
    assert_eq!(
        std::fs::read_link(root.join(".mdkg/design/link.md"))
            .expect("still there")
            .as_os_str()
            .as_bytes(),
        b"caf\xe9"
    );
}

/// orchestrator #7290 item 1: a temporary file is the session's only once
/// this call created it; a write whose creation failed records none.
#[test]
fn a_temp_this_call_never_created_is_never_recorded() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let mut session = cache
        .session(&lock, "no-temp", "done", "tester")
        .expect("session");
    let dir = cache.root().join(".mdkg/design");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    assert!(session.write(".mdkg/design/a.md", b"x\n").is_err());
    let record = cache.session_record().expect("record").expect("record");
    assert!(record.temps.is_empty(), "{:?}", record.temps);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    session
        .write(".mdkg/design/sub/b.md", b"y\n")
        .expect("a write that creates its temp");
    let record = cache.session_record().expect("record").expect("record");
    assert_eq!(record.temps.len(), 1, "{:?}", record.temps);
    assert!(record.temps[0].contains("-b.md-"));
    session.abort().expect("abort");
}

/// Terra #8228 and #8551: a link where a session's file was never blocks
/// crashed-session recovery; it's set aside as data.
#[test]
fn a_link_in_a_crashed_sessions_drift_never_blocks_recovery() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "linked", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
        let root = session.root().to_owned();
        std::fs::remove_file(root.join(".mdkg/design/a.md")).expect("rm");
        std::os::unix::fs::symlink("elsewhere", root.join(".mdkg/design/a.md")).expect("link");
        // The process dies here.
    }
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("recovery isn't blocked");
    let ended = done.session.expect("ended");
    let folder = ended.unclassified.first().cloned().expect("set aside");
    let drift = std::fs::read_to_string(folder.join("DRIFT.json")).expect("drift");
    assert!(drift.contains("\"link\": \"elsewhere\""), "{drift}");
    assert_eq!(
        std::fs::read(cache.root().join(".mdkg/design/a.md")).expect("restored"),
        b"alpha\n"
    );
}

/// orchestrator #8013: a session collects every requested path as a
/// preimage, unchanged ones included; an all-no-op session is a no-op
/// write (`graph.write_noop`), not an empty batch.
#[test]
fn a_sessions_unchanged_paths_stay_preimages() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let base = writer.snapshot().expect("tip");
    let mut session = cache
        .session(&lock, "all-same", "update", "tester")
        .expect("session");
    session
        .write(".mdkg/design/a.md", b"alpha\n")
        .expect("same bytes");
    let changes = session.changes().expect("changes");
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].path(), ".mdkg/design/a.md");
    let err = writer
        .prepare(
            &lock,
            &mdkg_graph::refwrite::Request {
                id: "all-same".into(),
                base: Some(base),
                changes,
                message: "update".into(),
                trailers: vec![],
            },
        )
        .expect_err("nothing to write");
    assert_eq!(err.code(), "graph.write_noop");
    session.abort().expect("abort");
}

/// Terra #9366: a link in `rescued/` never lets a partial match itself; a
/// partial holding only a link's target in its drift list isn't redundant
/// while the cache no longer holds that link; orphans are reported on every
/// refresh until settled.
#[test]
fn partials_are_judged_without_links_and_metadata_counts() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let base = repo.0.join(".git/mdkg/graph/rescued");
    std::fs::create_dir_all(&base).expect("rescued");
    // A partial with bytes the cache doesn't hold, and a visible link to it.
    let lone = base.join(".20261029T000002Z-0123456789ad-hand.partial");
    std::fs::create_dir_all(lone.join("files/.mdkg/design")).expect("dirs");
    std::fs::write(lone.join("files/.mdkg/design/a.md"), b"lost edit\n").expect("copy");
    std::fs::write(
        lone.join("DRIFT.json"),
        r#"{"drift":[{"path":".mdkg/design/a.md","kind":"modified","mode":"100644"}]}"#,
    )
    .expect("drift");
    std::os::unix::fs::symlink(&lone, base.join("looks-complete")).expect("link");
    // A partial whose only record is a link's target.
    let meta = base.join(".20261029T000003Z-0123456789ae-hand.partial");
    std::fs::create_dir_all(meta.join("files")).expect("dirs");
    std::fs::write(
        meta.join("DRIFT.json"),
        r#"{"drift":[{"path":".mdkg/design/link.md","kind":"added","link":"elsewhere"}]}"#,
    )
    .expect("drift");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let lone_orphan = base.join("orphan-20261029T000002Z-0123456789ad-hand");
    let meta_orphan = base.join("orphan-20261029T000003Z-0123456789ae-hand");
    assert_eq!(
        done.orphaned,
        vec![lone_orphan.clone(), meta_orphan.clone()]
    );
    // Reported again on the next refresh.
    let again = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    assert_eq!(again.orphaned, vec![lone_orphan, meta_orphan]);
}

/// Terra #10216: a partial is settled only by positive proof of the same
/// provenance: never through another rescue's linked `DRIFT.json`, never
/// by a hand rescue for an unfinished session's bytes, and a deletion
/// holds only while the cache path is provably absent.
#[test]
fn partials_need_positive_proof_of_the_same_provenance() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let base = repo.0.join(".git/mdkg/graph/rescued");
    std::fs::create_dir_all(&base).expect("rescued");
    let drift = r#"{"drift":[{"path":".mdkg/design/a.md","kind":"modified","mode":"100644"}]}"#;
    let rescue = |name: &str, list: Option<&str>, bytes: &[u8]| {
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join("files/.mdkg/design")).expect("dirs");
        std::fs::write(dir.join("files/.mdkg/design/a.md"), bytes).expect("copy");
        if let Some(list) = list {
            std::fs::write(dir.join("DRIFT.json"), list).expect("drift");
        }
        dir
    };
    // A hand partial whose only match has a linked drift list.
    let linked = rescue(
        ".20261029T000010Z-0123456789c0-hand.partial",
        Some(drift),
        b"linked edit\n",
    );
    let lookalike = rescue("20261029T000011Z-0123456789c1-hand", None, b"linked edit\n");
    std::os::unix::fs::symlink(linked.join("DRIFT.json"), lookalike.join("DRIFT.json"))
        .expect("link");
    // An unfinished session's partial whose only match is a hand rescue.
    let unfinished = rescue(
        ".20261029T000012Z-0123456789c2-unfinished-op1.partial",
        Some(drift),
        b"lost edit\n",
    );
    std::fs::write(unfinished.join(cache::MARKER), b"{}").expect("marker");
    rescue(
        "20261029T000013Z-0123456789c3-hand",
        Some(drift),
        b"lost edit\n",
    );
    // A deletion whose cache path is now a folder, with the copy's folder
    // of that name in the partial.
    let deleted = base.join(".20261029T000014Z-0123456789c4-hand.partial");
    std::fs::create_dir_all(deleted.join("files/.mdkg/design/gone")).expect("dirs");
    std::fs::write(
        deleted.join("DRIFT.json"),
        r#"{"drift":[{"path":".mdkg/design/gone","kind":"deleted"}]}"#,
    )
    .expect("drift");
    let design = cache.root().join(".mdkg/design");
    std::fs::set_permissions(&design, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::create_dir(design.join("gone")).expect("folder");
    std::fs::set_permissions(&design, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let names: Vec<String> = done
        .orphaned
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "orphan-20261029T000010Z-0123456789c0-hand",
            "orphan-20261029T000012Z-0123456789c2-unfinished-op1",
            "orphan-20261029T000014Z-0123456789c4-hand",
        ]
    );
}

/// Terra #10216: a crashed session's recorded rescue folder that is now a
/// link is never relied on as already holding a copy.
#[test]
fn a_linked_recorded_rescue_is_never_already_kept() {
    let repo = Repo::new();
    let writer = repo.writer();
    let cache = Cache::open(&repo.0).expect("cache");
    let lock = lock(&writer);
    cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    {
        let mut session = cache
            .session(&lock, "linked-op", "done", "tester")
            .expect("session");
        session
            .write(".mdkg/design/a.md", b"status: done\n")
            .expect("write");
        // The process dies here.
    }
    // Elsewhere, a copy with matching bytes and mode, and a link to it
    // under a recorded rescue's name.
    let elsewhere = repo.0.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("files/.mdkg/design")).expect("dirs");
    std::fs::write(elsewhere.join("files/.mdkg/design/a.md"), b"status: done\n").expect("copy");
    std::fs::write(
        elsewhere.join("DRIFT.json"),
        r#"{"drift":[{"path":".mdkg/design/a.md","kind":"modified","mode":"100644"}]}"#,
    )
    .expect("drift");
    let base = repo.0.join(".git/mdkg/graph/rescued");
    std::fs::create_dir_all(&base).expect("rescued");
    let link = base.join("20261029T000020Z-0123456789d0-unfinished-linked-op");
    std::os::unix::fs::symlink(&elsewhere, &link).expect("link");
    let record = repo.0.join(".git/mdkg/graph/session.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).expect("record")).expect("json");
    value["kept"] = serde_json::json!([link.to_string_lossy()]);
    std::fs::write(&record, serde_json::to_vec(&value).expect("json")).expect("record");
    let done = cache
        .refresh(&lock, &repo.tip(), &paths())
        .expect("refresh");
    let ended = done.session.expect("ended session");
    let real: Vec<_> = ended
        .kept
        .iter()
        .filter(|k| std::fs::symlink_metadata(k).is_ok_and(|m| m.is_dir()))
        .collect();
    assert!(
        real.iter()
            .any(|k| k.join("files/.mdkg/design/a.md").is_file()),
        "a real rescue holds the session's write: {:?}",
        ended.kept
    );
}
