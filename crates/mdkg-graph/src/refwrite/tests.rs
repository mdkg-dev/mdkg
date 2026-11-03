use super::*;
use crate::refwrite::{LockKind, Prepared};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::Duration;

const GRAPH: &str = "refs/heads/mdkg-graph";
const MAIN: &str = "refs/heads/main";
const GID: &str = "0192f3a0-0000-7000-8000-000000000000";

struct Repo(PathBuf);
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Repo {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mdkg-refwrite-{}-{}",
            std::process::id(),
            mdkg_format::Uuid7::now()
        ));
        std::fs::create_dir_all(root.join(".mdkg/design")).expect("mkdir");
        let repo = Self(root);
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Refwrite Test"]);
        repo.git(&["config", "user.email", "refwrite@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.put(".mdkg/design/base.md", b"base\n");
        repo.put(
            ".mdkg/config.json",
            format!(r#"{{"graph_id":"{GID}"}}"#).as_bytes(),
        );
        repo.put("source.txt", b"code\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "initial"]);
        repo.git(&["update-ref", GRAPH, "HEAD"]);
        repo
    }
    fn git(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("git spawn");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git text")
            .trim()
            .into()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["-c", "core.hooksPath=/dev/null", "-C"])
            .arg(&self.0)
            .args(args);
        command
    }
    fn put(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
    fn writer(&self, mode: LockMode) -> RefWriter {
        RefWriter::open(&self.0, GRAPH, Some(MAIN), mode)
            .expect("open writer")
            .with_graph_id(GID)
    }
    /// Someone else's update of the graph ref, without the graph lock: a
    /// commit on the tip that writes `path`.
    fn external(&self, path: &str, bytes: &[u8]) -> String {
        let index = self.0.join(".git/external-index");
        let run = |args: &[&str], input: Option<&[u8]>| {
            let mut cmd = self.command(args);
            cmd.env("GIT_INDEX_FILE", &index)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut child = cmd.spawn().expect("git");
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(input)
                    .expect("in");
            }
            drop(child.stdin.take());
            let out = child.wait_with_output().expect("git");
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        run(&["read-tree", GRAPH], None);
        let blob = run(&["hash-object", "-w", "--stdin"], Some(bytes));
        run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            None,
        );
        let tree = run(&["write-tree"], None);
        let tip = self.git(&["rev-parse", GRAPH]);
        let commit = run(&["commit-tree", &tree, "-p", &tip, "-m", "external"], None);
        self.git(&["update-ref", GRAPH, &commit, &tip]);
        std::fs::remove_file(&index).ok();
        commit
    }
    fn main_commit(&self, path: &str, body: &[u8]) -> String {
        self.put(path, body);
        self.git(&["add", path]);
        self.git(&["commit", "-qm", "main advance"]);
        self.git(&["rev-parse", MAIN])
    }
}
fn put(path: &str, value: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        bytes: value.into(),
    }
}
fn request(writer: &RefWriter, id: &str, changes: Vec<Change>) -> Request {
    Request {
        id: id.into(),
        base: Some(writer.snapshot().expect("snapshot")),
        changes,
        message: "test write".into(),
        trailers: vec![],
    }
}
fn read(writer: &RefWriter, path: &str) -> Option<Vec<u8>> {
    writer
        .read(&writer.snapshot().expect("head"), path)
        .expect("read")
}

#[test]
fn writes_nested_batch_preserving_modes_byte_names_and_checkout() {
    let repo = Repo::new();
    repo.put(".mdkg/design/tool", b"executable");
    std::fs::set_permissions(
        repo.0.join(".mdkg/design/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    symlink("base.md", repo.0.join(".mdkg/design/link")).expect("symlink");
    let odd = b"odd\xff\t\n".to_vec();
    repo.git(&["add", "."]);
    // Some filesystems cannot materialize this name; Git trees still can.
    let blob = repo.git(&["rev-parse", "HEAD:.mdkg/design/base.md"]);
    let mut record = format!("100644 {blob}\t.mdkg/design/").into_bytes();
    record.extend_from_slice(&odd);
    record.push(0);
    let mut index = repo
        .command(&["update-index", "-z", "--index-info"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("index fixture");
    index
        .stdin
        .take()
        .expect("stdin")
        .write_all(&record)
        .expect("entry");
    assert!(index.wait().expect("index fixture").success());
    let initial = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{initial},.mdkg/design/submodule"),
    ]);
    repo.git(&["commit", "-qm", "unusual entries"]);
    repo.git(&["update-ref", GRAPH, "HEAD"]);
    let writer = repo.writer(LockMode::Local);
    let before = writer.tree(&format!("{GRAPH}:.mdkg/design")).expect("tree");
    let index_before = std::fs::read(repo.0.join(".git/index")).expect("index");
    let work_before = repo.git(&["status", "--porcelain"]);
    let req = request(
        &writer,
        "batch",
        vec![
            put(".mdkg/design/a/b.md", b"nested\0body"),
            Change::Delete {
                path: ".mdkg/design/base.md".into(),
            },
        ],
    );
    let receipt = writer.write(&req).expect("write");
    assert!(!receipt.already_applied);
    assert_eq!(
        read(&writer, ".mdkg/design/a/b.md"),
        Some(b"nested\0body".to_vec())
    );
    assert_eq!(read(&writer, ".mdkg/design/base.md"), None);
    let after = writer.tree(&format!("{GRAPH}:.mdkg/design")).expect("tree");
    for name in [
        b"tool".to_vec(),
        b"link".to_vec(),
        b"submodule".to_vec(),
        odd,
    ] {
        assert_eq!(
            before.get(&name),
            after.get(&name),
            "untouched entry {name:?}"
        );
    }
    assert_eq!(repo.git(&["status", "--porcelain"]), work_before);
    assert_eq!(
        std::fs::read(repo.0.join(".git/index")).expect("index"),
        index_before
    );
    assert!(!repo.0.join(".mdkg/design/a").exists());
    assert_eq!(
        repo.git(&["rev-parse", MAIN]),
        req.base.as_ref().expect("base").commit
    );
}

#[test]
fn a_lost_cas_to_an_outside_writer_retries_without_lost_changes() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "racer-a", vec![put(".mdkg/design/a", b"A")]);
    let mut first = true;
    let receipt = writer
        .write_hooked(&req, &mut |step| {
            if step == Step::BeforeCas && first {
                first = false;
                repo.external(".mdkg/design/b", b"B");
            }
            Ok(())
        })
        .expect("write");
    assert_eq!(receipt.retries, 1);
    assert!(receipt.rebuilt);
    assert_eq!(read(&writer, ".mdkg/design/a"), Some(b"A".to_vec()));
    assert_eq!(read(&writer, ".mdkg/design/b"), Some(b"B".to_vec()));
}

#[test]
fn stale_reader_cannot_overwrite_winner_or_partially_apply_a_batch() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let stale = request(
        &writer,
        "stale",
        vec![
            put(".mdkg/design/unrelated", b"must not land"),
            put(".mdkg/design/base.md", b"stale"),
        ],
    );
    let winner = request(
        &writer,
        "winner",
        vec![put(".mdkg/design/base.md", b"winner")],
    );
    writer.write(&winner).expect("winner");
    assert_eq!(
        writer
            .read(stale.base.as_ref().expect("base"), ".mdkg/design/base.md")
            .expect("old snapshot"),
        Some(b"base\n".to_vec())
    );
    assert!(matches!(writer.write(&stale), Err(Error::Stale { .. })));
    assert_eq!(
        read(&writer, ".mdkg/design/base.md"),
        Some(b"winner".to_vec())
    );
    assert_eq!(read(&writer, ".mdkg/design/unrelated"), None);
}

#[test]
fn retry_identity_survives_later_edit_and_second_parent_main_merge() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "persistent",
        vec![put(".mdkg/design/base.md", b"first")],
    );
    let first = writer.write(&req).expect("first");
    let later = request(
        &writer,
        "later",
        vec![put(".mdkg/design/base.md", b"later")],
    );
    writer.write(&later).expect("later");
    repo.main_commit("source.txt", b"changed code");
    repo.git(&["merge", "--no-ff", "-qm", "integrate graph", GRAPH]);
    // An external update (a sync merge) holding the write as a second parent.
    repo.git(&["update-ref", GRAPH, "HEAD"]);
    let head = writer.snapshot().expect("head");
    let replay = writer.write(&req).expect("replay");
    assert!(replay.already_applied);
    assert_eq!(replay.commit, first.commit);
    assert_eq!(writer.snapshot().expect("head"), head);
    assert_eq!(
        read(&writer, ".mdkg/design/base.md"),
        Some(b"later".to_vec())
    );
    let mut different = req;
    different.changes = vec![put(".mdkg/design/base.md", b"other")];
    assert!(matches!(
        writer.write(&different),
        Err(Error::RequestIdReused(_))
    ));
}

#[test]
fn invalid_batches_and_checked_out_or_symbolic_refs_are_refused() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let before = writer.snapshot().expect("head");
    for changes in [
        vec![put(".mdkg/design/a", b"valid"), put("../escape", b"bad")],
        vec![put(".mdkg/a", b"one"), put(".mdkg/a", b"two")],
        vec![put(".mdkg/a", b"one"), put(".mdkg/a/b", b"two")],
    ] {
        assert!(matches!(
            writer.write(&request(&writer, "bad", changes)),
            Err(Error::Invalid(_))
        ));
        assert_eq!(writer.snapshot().expect("head"), before);
    }
    assert!(RefWriter::open(&repo.0, MAIN, Some(MAIN), LockMode::Local).is_err());
    repo.git(&["symbolic-ref", "refs/heads/alias", MAIN]);
    assert!(RefWriter::open(&repo.0, "refs/heads/alias", Some(MAIN), LockMode::Local).is_err());
    repo.git(&["checkout", "-q", "mdkg-graph"]);
    assert!(RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).is_err());
}

fn crash_request(base: Snapshot) -> Request {
    Request {
        id: "crash-request".into(),
        base: Some(base),
        changes: vec![
            put(".mdkg/design/crash/a", b"A"),
            put(".mdkg/design/crash/b", b"B"),
        ],
        message: "crash test".into(),
        trailers: vec![],
    }
}

// Invoked as a separate process by the parent below. process::exit does not run
// destructors, so lock recovery is tested through OS process death, not unwind.
#[test]
fn crash_child() {
    let Some(root) = std::env::var_os("MDKG_REFWRITE_CHILD_REPO") else {
        return;
    };
    let writer = RefWriter::open(Path::new(&root), GRAPH, Some(MAIN), LockMode::Local)
        .expect("child writer")
        .with_graph_id(GID);
    let base = std::env::var("MDKG_REFWRITE_CHILD_BASE").expect("base");
    let target: usize = std::env::var("MDKG_REFWRITE_CHILD_STEP")
        .expect("step")
        .parse()
        .expect("number");
    let mut count = 0;
    writer
        .write_hooked(
            &crash_request(writer.snapshot_at(&base).expect("restore persisted base")),
            &mut |_| {
                count += 1;
                if count == target {
                    std::process::exit(73);
                }
                Ok(())
            },
        )
        .expect("child write");
    panic!("crash step not reached");
}

#[test]
fn process_crash_at_every_plumbing_boundary_plus_failed_write_is_exactly_once() {
    let reference = Repo::new();
    let writer = reference.writer(LockMode::Local);
    let mut steps = Vec::new();
    writer
        .write_hooked(
            &crash_request(writer.snapshot().expect("base")),
            &mut |step| {
                steps.push(step);
                Ok(())
            },
        )
        .expect("enumerate boundaries");
    assert!(steps.contains(&Step::Published));
    for (offset, step) in steps.iter().enumerate() {
        let repo = Repo::new();
        let writer = repo.writer(LockMode::Local);
        let base = writer.snapshot().expect("base").commit;
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "refwrite::tests::crash_child",
                "--test-threads=1",
            ])
            .env("MDKG_REFWRITE_CHILD_REPO", &repo.0)
            .env("MDKG_REFWRITE_CHILD_BASE", &base)
            .env("MDKG_REFWRITE_CHILD_STEP", (offset + 1).to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("child");
        assert_eq!(
            child.wait().expect("child exit").code(),
            Some(73),
            "boundary {offset}: {step:?}"
        );
        // Fail on a leaked OS lock instead of hanging the parent's next write.
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(repo.0.join(".git/mdkg/graph/lock"))
            .expect("retained lock");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match probe.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "lock leaked after boundary {offset}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(error)) => panic!("lock probe: {error}"),
            }
        }
        drop(probe);
        let after_crash = writer.snapshot().expect("after crash");
        assert_eq!(after_crash.commit == base, *step != Step::Published);
        // A separate failed write after the crash cannot consume half a batch or
        // require removing our OS lock. A stale Git lock is deliberately left to
        // Git to refuse; only this fixture removes its own injected fault.
        let lock_path = repo.0.join(".git/refs/heads/mdkg-graph.lock");
        std::fs::write(&lock_path, b"injected failure").expect("fault");
        let failed = request(
            &writer,
            "failed",
            vec![put(".mdkg/design/failed", b"never")],
        );
        assert!(
            matches!(writer.write(&failed), Err(Error::Git { .. })),
            "boundary {offset}"
        );
        assert_eq!(writer.snapshot().expect("failed head"), after_crash);
        std::fs::remove_file(&lock_path).expect("remove injected fault");
        let receipt = writer
            .write(&crash_request(
                writer.snapshot_at(&base).expect("restore persisted base"),
            ))
            .expect("recover");
        assert_eq!(receipt.already_applied, *step == Step::Published);
        let again = writer
            .write(&crash_request(
                writer.snapshot_at(&base).expect("restore persisted base"),
            ))
            .expect("retry again");
        assert!(again.already_applied);
        assert_eq!(receipt.commit, again.commit);
        assert_eq!(read(&writer, ".mdkg/design/crash/a"), Some(b"A".to_vec()));
        assert_eq!(read(&writer, ".mdkg/design/crash/b"), Some(b"B".to_vec()));
        assert_eq!(read(&writer, ".mdkg/design/failed"), None);
        assert_eq!(
            repo.git(&["rev-list", "--count", &format!("{base}..{GRAPH}")]),
            "1"
        );
        assert!(
            repo.0.join(".git/mdkg/graph/lock").is_file(),
            "OS lock inode retained"
        );
    }
}

#[test]
fn local_lock_serializes_cooperating_writers() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let requests: Vec<_> = (0..4)
        .map(|i| {
            request(
                &writer,
                &format!("local-{i}"),
                vec![put(&format!(".mdkg/design/{i}"), b"body")],
            )
        })
        .collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = requests
            .iter()
            .map(|request| {
                let writer = &writer;
                scope.spawn(move || writer.write(request).expect("locked writer"))
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().expect("writer").retries, 0);
        }
    });
    for i in 0..4 {
        assert_eq!(
            read(&writer, &format!(".mdkg/design/{i}")),
            Some(b"body".to_vec())
        );
    }
}

#[test]
fn restoring_a_snapshot_requires_an_immutable_commit_oid() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let snapshot = writer.snapshot().expect("snapshot");
    assert_eq!(
        writer.snapshot_at(snapshot.commit()).expect("restore"),
        snapshot
    );
    assert!(writer.snapshot_at(GRAPH).is_err());
    assert!(writer.snapshot_at(&snapshot.commit()[..8]).is_err());
    assert!(
        writer
            .snapshot_at(&repo.git(&["rev-parse", "HEAD^{tree}"]))
            .is_err()
    );
}

#[test]
fn opening_from_a_repository_subdirectory_preserves_the_entire_root_tree() {
    let repo = Repo::new();
    let root_writer = repo.writer(LockMode::Local);
    let initial = root_writer.snapshot().expect("initial");
    let nested = RefWriter::open(
        &repo.0.join(".mdkg/design"),
        GRAPH,
        Some(MAIN),
        LockMode::Local,
    )
    .expect("nested writer")
    .with_graph_id(GID);
    nested
        .write(&request(
            &nested,
            "nested-cwd",
            vec![put(".mdkg/design/new", b"new")],
        ))
        .expect("nested write");
    let after = root_writer.snapshot().expect("after");
    assert_eq!(
        root_writer
            .entry(Some(initial.commit()), "source.txt")
            .expect("old source"),
        root_writer
            .entry(Some(after.commit()), "source.txt")
            .expect("new source")
    );
    assert_eq!(
        read(&root_writer, ".mdkg/design/base.md"),
        Some(b"base\n".to_vec())
    );
    assert_eq!(
        read(&nested, ".mdkg/design/base.md"),
        Some(b"base\n".to_vec())
    );
    assert_eq!(
        read(&root_writer, ".mdkg/design/new"),
        Some(b"new".to_vec())
    );
}

#[test]
fn review_symbolic_main_cannot_alias_the_graph_ref() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "main-alias", vec![put(".mdkg/design/new", b"new")]);
    repo.git(&["checkout", "--detach", "-q"]);
    repo.git(&["symbolic-ref", MAIN, GRAPH]);
    let before = repo.git(&["rev-parse", MAIN]);
    assert!(RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).is_err());
    assert!(
        writer.write(&req).is_err(),
        "also reject aliases created after open"
    );
    assert_eq!(repo.git(&["rev-parse", MAIN]), before);
    assert_eq!(repo.git(&["rev-parse", GRAPH]), before);
}

#[test]
fn review_same_path_cas_loser_rechecks_preimages_before_retrying() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let base = writer.snapshot().expect("base");
    let req = request(
        &writer,
        "overlap-a",
        vec![put(".mdkg/design/base.md", b"A")],
    );
    let mut first = true;
    let result = writer.write_hooked(&req, &mut |step| {
        if step == Step::BeforeCas && first {
            first = false;
            repo.external(".mdkg/design/base.md", b"B");
        }
        Ok(())
    });
    assert!(matches!(result, Err(Error::Stale { .. })));
    assert_eq!(read(&writer, ".mdkg/design/base.md"), Some(b"B".to_vec()));
    assert_eq!(
        repo.git(&[
            "rev-list",
            "--count",
            &format!("{}..{GRAPH}", base.commit())
        ]),
        "1"
    );
}

#[test]
fn review_replace_refs_and_shallow_files_cannot_hide_data_or_receipts() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "original-history",
        vec![put(".mdkg/design/retained", b"retained")],
    );
    let receipt = writer.write(&req).expect("original");
    let later = writer
        .write(&request(
            &writer,
            "later-history",
            vec![put(".mdkg/design/later", b"later")],
        ))
        .expect("later");
    repo.git(&[
        "replace",
        &later.commit,
        req.base.as_ref().expect("base").commit(),
    ]);
    repo.put(".git/shallow", format!("{}\n", later.commit).as_bytes());
    assert_eq!(
        read(&writer, ".mdkg/design/retained"),
        Some(b"retained".to_vec())
    );
    assert_eq!(read(&writer, ".mdkg/design/later"), Some(b"later".to_vec()));
    let replay = writer.write(&req).expect("replay through real history");
    assert!(replay.already_applied);
    assert_eq!(replay.commit, receipt.commit);
    assert_eq!(replay.head, later.commit);
}

#[test]
fn review_log_signatures_and_marker_mentions_do_not_corrupt_replay() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "receipt",
        vec![put(".mdkg/design/receipt", b"body")],
    );
    let receipt = writer.write(&req).expect("original");
    let tree = repo.git(&["rev-parse", &format!("{}^{{tree}}", receipt.commit)]);
    // An unrelated signed descendant mentions the older request's marker in its
    // body. Grep must neither run a signature tool nor stop at that false match.
    let raw = format!(
        "tree {tree}\nparent {}\nauthor Test <test@example.invalid> 1700000000 +0000\ncommitter Test <test@example.invalid> 1700000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n invalid-fixture-signature\n -----END PGP SIGNATURE-----\n\nunrelated descendant\n\nmdkg-refwrite-v1 receipt {}\n",
        receipt.commit,
        writer.fingerprint(&req).expect("digest")
    );
    let signed = writer
        .git(
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(raw.as_bytes()),
        )
        .expect("signed fixture");
    let signed = text(&signed).expect("oid").trim();
    repo.git(&["update-ref", GRAPH, signed, &receipt.commit]);
    repo.put(".git/fake-gpg", b"#!/bin/sh\nprintf 'called\\n' > \"$0.called\"\nprintf '[GNUPG:] BADSIG fixture\\n' >&2\nexit 1\n");
    let program = repo.0.join(".git/fake-gpg");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    repo.git(&["config", "gpg.program", program.to_str().expect("path")]);
    repo.git(&["config", "log.showSignature", "true"]);
    let replay = writer.write(&req).expect("replay");
    assert!(replay.already_applied);
    assert_eq!(replay.commit, receipt.commit);
    assert_eq!(replay.head, signed);
    assert!(
        !repo.0.join(".git/fake-gpg.called").exists(),
        "signature verification must not run"
    );
}

#[test]
fn hardening_child() {
    let Some(root) = std::env::var_os("MDKG_REFWRITE_ENV_REPO") else {
        return;
    };
    let writer = RefWriter::open(Path::new(&root), GRAPH, Some(MAIN), LockMode::Local)
        .expect("isolated writer")
        .with_graph_id(GID);
    writer
        .write(&request(
            &writer,
            "env-isolation",
            vec![put(".mdkg/design/real", b"real")],
        ))
        .expect("real write");
}

#[test]
fn review_inherited_git_locations_cannot_redirect_a_write() {
    let real = Repo::new();
    let decoy = Repo::new();
    let decoy_main = decoy.git(&["rev-parse", MAIN]);
    let decoy_graph = decoy.git(&["rev-parse", GRAPH]);
    let real_main = real.git(&["rev-parse", MAIN]);
    let decoy_index = std::fs::read(decoy.0.join(".git/index")).expect("index");
    let real_index = std::fs::read(real.0.join(".git/index")).expect("index");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "refwrite::tests::hardening_child",
            "--test-threads=1",
        ])
        .env("MDKG_REFWRITE_ENV_REPO", &real.0)
        .env("GIT_DIR", decoy.0.join(".git"))
        .env("GIT_WORK_TREE", &decoy.0)
        .env("GIT_INDEX_FILE", decoy.0.join(".git/index"))
        .env("GIT_OBJECT_DIRECTORY", decoy.0.join(".git/objects"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.name")
        .env("GIT_CONFIG_VALUE_0", "Injected Identity")
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let writer = real.writer(LockMode::Local);
    assert_eq!(read(&writer, ".mdkg/design/real"), Some(b"real".to_vec()));
    assert_eq!(
        real.git(&["show", "-s", "--format=%an", GRAPH]),
        "Refwrite Test"
    );
    assert_eq!(real.git(&["rev-parse", MAIN]), real_main);
    assert_eq!(decoy.git(&["rev-parse", MAIN]), decoy_main);
    assert_eq!(decoy.git(&["rev-parse", GRAPH]), decoy_graph);
    assert_eq!(
        std::fs::read(real.0.join(".git/index")).expect("index"),
        real_index
    );
    assert_eq!(
        std::fs::read(decoy.0.join(".git/index")).expect("index"),
        decoy_index
    );
    assert!(real.git(&["status", "--porcelain"]).is_empty());
    assert!(decoy.git(&["status", "--porcelain"]).is_empty());
}

fn journal(repo: &Repo) -> crate::intent::Journal {
    crate::intent::Journal::open(&repo.0.join(".git")).expect("journal")
}

fn intent_of(prepared: &Prepared) -> crate::intent::Intent {
    crate::intent::Intent {
        prepared: prepared.clone(),
        message: "test write".into(),
        actor: "tester".into(),
        leases: vec!["lease-1".into(), "lease-2".into()],
        op: "update".into(),
    }
}

/// Steps 5 to 8 of the write path, stopping (a crash) after `stop`, then
/// a replay that asks a fake store whether it recorded the request.
#[test]
fn replay_finishes_or_discards_at_every_crash_point() {
    use crate::intent::Replayed;
    for stop in 5..=8 {
        let repo = Repo::new();
        let writer = repo.writer(LockMode::Local);
        let base = writer.snapshot().expect("tip");
        let req = request(
            &writer,
            "crash-point",
            vec![put(".mdkg/design/a", b"A"), put(".mdkg/design/b", b"B")],
        );
        let journal = journal(&repo);
        let mut store_recorded = false;
        {
            let lock = writer.lock(LockKind::Exclusive).expect("lock");
            let prepared = writer.prepare(&lock, &req).expect("prepare");
            journal.record(&intent_of(&prepared)).expect("intent");
            if stop >= 6 {
                store_recorded = true;
            }
            if stop >= 7 {
                writer.publish(&lock, &prepared).expect("publish");
            }
            if stop >= 8 {
                journal
                    .finish(&writer, &lock, &intent_of(&prepared))
                    .expect("finish");
            }
        }
        // One failed replay first (the store is busy): nothing changes.
        let lock = writer.lock(LockKind::Exclusive).expect("lock");
        let failed = journal
            .replay(&writer, &lock, |_| Err::<bool, _>("store busy"))
            .expect("replay");
        assert!(
            failed.iter().all(|r| matches!(r, Replayed::Failed { .. })),
            "{stop}"
        );
        let replayed = journal
            .replay(&writer, &lock, |intent| {
                assert_eq!(intent.leases.len(), 2);
                Ok::<_, String>(store_recorded)
            })
            .expect("replay");
        match stop {
            5 => assert!(matches!(replayed.as_slice(), [Replayed::Discarded { .. }])),
            6 => assert!(
                matches!(replayed.as_slice(), [Replayed::Published { receipt, .. }] if !receipt.already_applied)
            ),
            7 => assert!(
                matches!(replayed.as_slice(), [Replayed::Published { receipt, .. }] if receipt.already_applied)
            ),
            _ => assert!(replayed.is_empty()),
        }
        assert!(journal.pending().expect("pending").is_empty(), "{stop}");
        assert!(writer.pending().expect("pins").is_empty(), "{stop}");
        let published = repo.git(&[
            "rev-list",
            "--count",
            &format!("{}..{GRAPH}", base.commit()),
        ]);
        assert_eq!(published, if stop == 5 { "0" } else { "1" }, "{stop}");
        // A rerun of the same request id returns the original receipt.
        if stop > 5 {
            drop(lock);
            assert!(writer.write(&req).expect("rerun").already_applied);
        }
    }
}

#[test]
fn a_stale_replay_is_kept_and_reported() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "stale-replay",
        vec![put(".mdkg/design/base.md", b"mine")],
    );
    let journal = journal(&repo);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    journal.record(&intent_of(&prepared)).expect("intent");
    // A sync overrides the touched path before the replay.
    let other = request(
        &writer,
        "sync",
        vec![put(".mdkg/design/base.md", b"remote")],
    );
    let p2 = writer.prepare(&lock, &other).expect("prepare other");
    writer.publish(&lock, &p2).expect("publish other");
    writer.unpin(&lock, &p2).expect("unpin other");
    let replayed = journal
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(
        matches!(replayed.as_slice(), [Replayed::Stale { path, .. }] if path == ".mdkg/design/base.md")
    );
    assert_eq!(journal.pending().expect("kept").len(), 1);
    assert!(
        writer
            .pending()
            .expect("pins")
            .iter()
            .any(|(id, c)| id == "stale-replay" && *c == prepared.commit)
    );
    assert_eq!(
        read(&writer, ".mdkg/design/base.md"),
        Some(b"remote".to_vec())
    );
}

#[test]
fn publish_rebuilds_on_a_newer_tip_and_keeps_both_writes() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mine = request(&writer, "mine", vec![put(".mdkg/design/mine", b"M")]);
    let prepared = writer.prepare(&lock, &mine).expect("prepare");
    let theirs = request(&writer, "theirs", vec![put(".mdkg/design/theirs", b"T")]);
    let p2 = writer.prepare(&lock, &theirs).expect("prepare theirs");
    writer.publish(&lock, &p2).expect("publish theirs");
    let receipt = writer.publish(&lock, &prepared).expect("publish mine");
    assert!(receipt.rebuilt && !receipt.already_applied);
    assert_ne!(receipt.commit, prepared.commit);
    assert_eq!(read(&writer, ".mdkg/design/mine"), Some(b"M".to_vec()));
    assert_eq!(read(&writer, ".mdkg/design/theirs"), Some(b"T".to_vec()));
    // The rebuild keeps the request's marker: a rerun finds it.
    assert_eq!(
        writer.published(&prepared).expect("lookup"),
        Some(receipt.commit.clone())
    );
    let again = writer.publish(&lock, &prepared).expect("again");
    assert!(again.already_applied);
    // The pending ref pins the prepared commit until it's unpinned.
    assert!(
        writer
            .pending()
            .expect("pins")
            .iter()
            .any(|(id, _)| id == "mine")
    );
    writer.unpin(&lock, &prepared).expect("unpin");
    assert!(
        !writer
            .pending()
            .expect("pins")
            .iter()
            .any(|(id, _)| id == "mine")
    );
}

#[test]
fn prepare_and_publish_need_the_exclusive_lock() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "locked", vec![put(".mdkg/design/x", b"x")]);
    let shared = writer.lock(LockKind::Shared).expect("shared");
    assert!(writer.prepare(&shared, &req).is_err());
    drop(shared);
    let other = Repo::new();
    let foreign = other
        .writer(LockMode::Local)
        .lock(LockKind::Exclusive)
        .expect("other lock");
    assert!(writer.prepare(&foreign, &req).is_err());
}

#[test]
fn a_rerun_from_a_newer_base_returns_the_original_receipt() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let first = request(&writer, "rerun", vec![put(".mdkg/design/x", b"1")]);
    let receipt = writer.write(&first).expect("first");
    // The response was lost; the caller rereads the tip and retries.
    let same = request(&writer, "rerun", vec![put(".mdkg/design/x", b"1")]);
    let again = writer.write(&same).expect("rerun");
    assert!(again.already_applied);
    assert_eq!(again.commit, receipt.commit);
    // The same id with another payload (a new node minted again) is
    // refused, never applied twice.
    let other = request(&writer, "rerun", vec![put(".mdkg/design/y", b"2")]);
    assert!(matches!(
        writer.write(&other),
        Err(Error::RequestIdReused(_))
    ));
    assert_eq!(
        repo.git(&[
            "rev-list",
            "--count",
            &format!("{}..{GRAPH}", first.base.as_ref().expect("base").commit())
        ]),
        "1"
    );
}

/// One failed write at each plumbing step (hash-object, mktree,
/// commit-tree, the pin, update-ref), then a retry of the same request:
/// exactly one publish, and no pending ref left.
#[test]
fn one_failed_step_then_a_retry_publishes_exactly_once() {
    let steps = [
        Step::Blob,
        Step::Tree,
        Step::Commit,
        Step::Pinned,
        Step::BeforeCas,
        Step::Published,
    ];
    for fail in steps {
        for changes in [
            vec![put(".mdkg/design/one", b"1")],
            vec![put(".mdkg/design/one", b"1"), put(".mdkg/work/two", b"2")],
        ] {
            let repo = Repo::new();
            let writer = repo.writer(LockMode::Local);
            let base = writer.snapshot().expect("tip");
            let req = request(&writer, "one-fault", changes);
            let result = writer.write_hooked(&req, &mut |step| {
                if step == fail {
                    return Err(Error::Io(io::Error::other("injected")));
                }
                Ok(())
            });
            assert!(result.is_err(), "{fail:?}");
            let published = repo.git(&[
                "rev-list",
                "--count",
                &format!("{}..{GRAPH}", base.commit()),
            ]);
            assert_eq!(
                published,
                if fail == Step::Published { "1" } else { "0" },
                "{fail:?}"
            );
            let receipt = writer.write(&req).expect("retry");
            assert_eq!(receipt.already_applied, fail == Step::Published, "{fail:?}");
            assert_eq!(
                repo.git(&[
                    "rev-list",
                    "--count",
                    &format!("{}..{GRAPH}", base.commit())
                ]),
                "1",
                "{fail:?}"
            );
            assert!(writer.pending().expect("pins").is_empty(), "{fail:?}");
            assert_eq!(read(&writer, ".mdkg/design/one"), Some(b"1".to_vec()));
        }
    }
}

/// C7 at the library level: two writers of the same path at once. One
/// wins; the other is refused as stale before it publishes.
#[test]
fn same_path_writers_one_wins_the_other_is_refused_before_publishing() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let a = request(&writer, "same-a", vec![put(".mdkg/design/base.md", b"A")]);
    let b = request(&writer, "same-b", vec![put(".mdkg/design/base.md", b"B")]);
    let barrier = Arc::new(Barrier::new(2));
    let results: Vec<_> = std::thread::scope(|scope| {
        [&a, &b]
            .map(|req| {
                let barrier = barrier.clone();
                let writer = &writer;
                scope.spawn(move || {
                    barrier.wait();
                    writer.write(req)
                })
            })
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });
    let won = results.iter().filter(|r| r.is_ok()).count();
    let stale = results
        .iter()
        .filter(|r| matches!(r, Err(Error::Stale { .. })))
        .count();
    assert_eq!((won, stale), (1, 1));
    assert_eq!(
        repo.git(&[
            "rev-list",
            "--count",
            &format!("{}..{GRAPH}", a.base.as_ref().expect("base").commit())
        ]),
        "1"
    );
}

#[test]
fn a_create_never_lands_on_a_graph_created_meanwhile() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    let writer = RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).expect("creator");
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let create = Request {
        id: "create".into(),
        base: None,
        changes: vec![
            put(".mdkg/design/mine.md", b"mine"),
            put(
                ".mdkg/config.json",
                format!(r#"{{"graph_id":"{GID}"}}"#).as_bytes(),
            ),
        ],
        message: "init".into(),
        trailers: vec![],
    };
    let prepared = writer.prepare(&lock, &create).expect("prepare");
    // Someone else creates the graph, with no path in common.
    repo.git(&["update-ref", GRAPH, "HEAD"]);
    assert!(matches!(
        writer.publish(&lock, &prepared),
        Err(Error::Exists(_))
    ));
    assert_eq!(
        repo.git(&["rev-parse", GRAPH]),
        repo.git(&["rev-parse", "HEAD"])
    );
}

#[test]
fn replay_refuses_a_shared_lock() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Shared).expect("shared lock");
    let err = journal(&repo)
        .replay(&writer, &lock, |_| Ok::<_, String>(false))
        .expect_err("refused");
    assert_eq!(err.code(), "graph.lock_required");
}

#[test]
fn only_a_plain_graph_branch_can_be_written() {
    let repo = Repo::new();
    for bad in [
        "refs/tags/graph",
        "refs/remotes/origin/mdkg-graph",
        "refs/notes/graph",
        "refs/heads/main",
        "refs/heads/Master",
        "refs/mdkg/pending/x",
    ] {
        assert!(
            RefWriter::open(&repo.0, bad, None, LockMode::Local).is_err(),
            "{bad}"
        );
    }
    // A tracking branch.
    repo.git(&["config", "branch.mdkg-graph.remote", "origin"]);
    repo.git(&["config", "branch.mdkg-graph.merge", "refs/heads/main"]);
    assert!(RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).is_err());
    repo.git(&["config", "--unset", "branch.mdkg-graph.remote"]);
    repo.git(&["config", "--unset", "branch.mdkg-graph.merge"]);
    // Upstream configured after open is caught at the next write.
    let writer = repo.writer(LockMode::Local);
    repo.git(&["config", "branch.mdkg-graph.merge", "refs/heads/main"]);
    let req = request(&writer, "tracking", vec![put(".mdkg/design/t", b"t")]);
    assert!(matches!(writer.write(&req), Err(Error::Invalid(_))));
}

#[test]
fn a_writer_refuses_a_tip_of_another_graph() {
    let repo = Repo::new();
    let writer = RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local)
        .expect("open")
        .with_graph_id("0192f3a0-0000-7000-8000-0000000000ff");
    let req = request(&writer, "other", vec![put(".mdkg/design/o", b"o")]);
    assert!(matches!(writer.write(&req), Err(Error::GraphIdentity(_))));
    // A writer with no identity may only create the graph.
    let unbound = RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).expect("open");
    let req = request(&unbound, "unbound", vec![put(".mdkg/design/o", b"o")]);
    assert!(matches!(unbound.write(&req), Err(Error::GraphIdentity(_))));
}

#[test]
fn an_edited_prepared_record_cannot_rewind_the_graph() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "rewind", vec![put(".mdkg/design/r", b"r")]);
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    let other = request(&writer, "later", vec![put(".mdkg/design/l", b"l")]);
    let p2 = writer.prepare(&lock, &other).expect("prepare later");
    let tip = writer.publish(&lock, &p2).expect("publish later").commit;
    // The record now claims the current tip as its parent: a CAS from it
    // to the prepared commit would drop "later".
    let mut edited = prepared.clone();
    edited.parent = Some(tip.clone());
    assert!(matches!(
        writer.publish(&lock, &edited),
        Err(Error::Invalid(_))
    ));
    let mut edited = prepared.clone();
    edited.paths = vec![".mdkg/design/l".into()];
    assert!(matches!(
        writer.publish(&lock, &edited),
        Err(Error::Invalid(_))
    ));
    assert_eq!(repo.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn a_copied_marker_without_the_change_is_not_a_publish() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "copied", vec![put(".mdkg/design/c", b"real")]);
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    // Someone commits the same subject without the change.
    let subject = repo.git(&["log", "-1", "--format=%s", &prepared.commit]);
    let tip = repo.git(&["rev-parse", GRAPH]);
    let tree = repo.git(&["rev-parse", &format!("{tip}^{{tree}}")]);
    let fake = repo.git(&["commit-tree", &tree, "-p", &tip, "-m", &subject]);
    repo.git(&["update-ref", GRAPH, &fake, &tip]);
    assert_eq!(writer.published(&prepared).expect("lookup"), None);
    let receipt = writer.publish(&lock, &prepared).expect("publish");
    assert!(!receipt.already_applied && receipt.rebuilt);
    assert_eq!(read(&writer, ".mdkg/design/c"), Some(b"real".to_vec()));
}

#[test]
fn a_rebuilt_publish_keeps_the_recorded_commit_in_history() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "kept", vec![put(".mdkg/design/k", b"k")]);
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    repo.external(".mdkg/design/other", b"o");
    let receipt = writer.publish(&lock, &prepared).expect("publish");
    assert!(receipt.rebuilt);
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &receipt.commit]);
    assert!(parents.ends_with(&prepared.commit), "{parents}");
    repo.git(&["merge-base", "--is-ancestor", &prepared.commit, GRAPH]);
}

#[test]
fn replay_drops_a_pin_that_has_no_intent() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "orphan", vec![put(".mdkg/design/o", b"o")]);
    // A crash between the pin and the intent write.
    writer.prepare(&lock, &req).expect("prepare");
    let replayed = journal(&repo)
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(matches!(replayed.as_slice(), [Replayed::Unpinned { id }] if id == "orphan"));
    assert!(writer.pending().expect("pins").is_empty());
    assert_eq!(read(&writer, ".mdkg/design/o"), None);
}

#[test]
fn the_journal_refuses_an_intent_it_could_not_read_back() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "big", vec![put(".mdkg/design/b", b"b")]);
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    let mut intent = intent_of(&prepared);
    intent.leases = (0..200_000).map(|i| format!("lease-{i:08}")).collect();
    assert!(journal(&repo).record(&intent).is_err());
    assert!(journal(&repo).pending().expect("pending").is_empty());
}

#[test]
fn replay_refuses_a_journal_of_another_clone() {
    let a = Repo::new();
    let b = Repo::new();
    let writer_b = b.writer(LockMode::Local);
    let lock_b = writer_b.lock(LockKind::Exclusive).expect("lock b");
    let writer_a = a.writer(LockMode::Local);
    let lock_a = writer_a.lock(LockKind::Exclusive).expect("lock a");
    let req = request(&writer_a, "clone-a", vec![put(".mdkg/design/a", b"a")]);
    let prepared = writer_a.prepare(&lock_a, &req).expect("prepare");
    journal(&a).record(&intent_of(&prepared)).expect("intent");
    drop(lock_a);
    // Clone A's journal, with clone B's writer and lock.
    assert!(
        journal(&a)
            .replay(&writer_b, &lock_b, |_| Ok::<_, String>(false))
            .is_err()
    );
    assert_eq!(journal(&a).pending().expect("kept").len(), 1);
}

#[test]
fn a_rerun_of_an_overwritten_request_returns_its_receipt_superseded() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let first = request(&writer, "overwritten", vec![put(".mdkg/design/x", b"mine")]);
    let receipt = writer.write(&first).expect("first");
    assert!(!receipt.superseded);
    // A later write (a sync, say) replaces the same path.
    let later = request(&writer, "later", vec![put(".mdkg/design/x", b"theirs")]);
    writer.write(&later).expect("later");
    let again = writer.write(&first).expect("rerun");
    assert!(again.already_applied && again.superseded);
    assert_eq!(again.commit, receipt.commit);
    assert_eq!(read(&writer, ".mdkg/design/x"), Some(b"theirs".to_vec()));
}

#[test]
fn a_write_can_not_drop_or_change_the_graph_id() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    for config in [
        r#"{"graph_id":"0192f3a0-0000-7000-8000-0000000000ff"}"#,
        "{}",
    ] {
        let req = request(
            &writer,
            "identity",
            vec![put(".mdkg/config.json", config.as_bytes())],
        );
        assert!(
            matches!(writer.write(&req), Err(Error::GraphIdentity(_))),
            "{config}"
        );
    }
    let req = request(
        &writer,
        "identity-delete",
        vec![Change::Delete {
            path: ".mdkg/config.json".into(),
        }],
    );
    assert!(matches!(writer.write(&req), Err(Error::GraphIdentity(_))));
    assert!(writer.pending().expect("pins").is_empty());
}

#[test]
fn a_no_op_request_is_refused_and_no_ops_are_left_out() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let same = request(
        &writer,
        "noop",
        vec![put(".mdkg/design/base.md", b"base\n")],
    );
    assert!(matches!(writer.write(&same), Err(Error::NoChange)));
    let mixed = request(
        &writer,
        "mixed",
        vec![
            put(".mdkg/design/base.md", b"base\n"),
            Change::Delete {
                path: ".mdkg/design/absent".into(),
            },
            put(".mdkg/design/new", b"new"),
        ],
    );
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let prepared = writer.prepare(&lock, &mixed).expect("prepare");
    assert_eq!(prepared.paths, [".mdkg/design/new"]);
    writer
        .publish(&lock, &prepared)
        .expect("publish verifies its own record");
}

#[test]
fn a_crash_after_a_rebuilt_publish_is_resumed_from_its_receipt() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "rebuilt-crash", vec![put(".mdkg/design/r", b"r")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    repo.external(".mdkg/design/other", b"o");
    let receipt = writer.publish(&lock, &prepared).expect("publish");
    assert!(receipt.rebuilt);
    // Crash before the unpin: a fresh rerun still returns the receipt.
    drop(lock);
    let again = writer.write(&req).expect("rerun");
    assert!(again.already_applied);
    assert_eq!(again.commit, receipt.commit);
    assert!(writer.pending().expect("pins").is_empty());
}

#[test]
fn a_pruned_receipt_commit_prepares_afresh() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let base = writer.snapshot().expect("base");
    let req = request(&writer, "pruned", vec![put(".mdkg/design/p", b"p")]);
    writer.write(&req).expect("first");
    // The graph is rewound and the published commit pruned.
    repo.git(&["update-ref", GRAPH, base.commit()]);
    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]);
    let again = writer.write(&req).expect("fresh prepare");
    assert!(!again.already_applied);
    assert_eq!(read(&writer, ".mdkg/design/p"), Some(b"p".to_vec()));
}

#[test]
fn an_orphan_pin_of_a_published_request_keeps_its_receipt() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "no-receipt", vec![put(".mdkg/design/n", b"n")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    writer.publish(&lock, &prepared).expect("publish");
    // The receipt write failed (simulated by removing it); the pin stays.
    std::fs::remove_file(repo.0.join(".git/mdkg/graph/receipts/no-receipt.json")).expect("rm");
    let replayed = journal(&repo)
        .replay(&writer, &lock, |_| Ok::<_, String>(false))
        .expect("replay");
    assert!(matches!(replayed.as_slice(), [Replayed::Unpinned { .. }]));
    assert!(
        repo.0
            .join(".git/mdkg/graph/receipts/no-receipt.json")
            .is_file()
    );
    drop(lock);
    // Another write changes the path; a rerun from a fresh base is still
    // the original request, never applied twice.
    let other = request(&writer, "overwrite", vec![put(".mdkg/design/n", b"x")]);
    writer.write(&other).expect("other");
    let again = writer.write(&request(
        &writer,
        "no-receipt",
        vec![put(".mdkg/design/n", b"n")],
    ));
    let again = again.expect("rerun");
    assert!(again.already_applied && again.superseded);
}

#[test]
fn a_request_whose_folder_became_a_file_reruns_as_superseded() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let first = request(
        &writer,
        "folder-gone",
        vec![put(".mdkg/design/group.md/child.md", b"child")],
    );
    let receipt = writer.write(&first).expect("first");
    // A later update replaces the folder with a file.
    let later = request(
        &writer,
        "folder-to-file",
        vec![Change::Delete {
            path: ".mdkg/design/group.md/child.md".into(),
        }],
    );
    writer.write(&later).expect("delete child");
    let file = request(&writer, "file", vec![put(".mdkg/design/group.md", b"file")]);
    writer.write(&file).expect("file in its place");
    let again = writer.write(&first).expect("rerun");
    assert!(again.already_applied && again.superseded);
    assert_eq!(again.commit, receipt.commit);
}

#[test]
fn an_unchanged_path_in_a_request_keeps_its_preimage_on_a_rebuild() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    // "base.md" is put with its current bytes: a no-op path, but the
    // request read it.
    let req = request(
        &writer,
        "preimage",
        vec![
            put(".mdkg/design/base.md", b"base\n"),
            put(".mdkg/design/b", b"B"),
        ],
    );
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    assert_eq!(prepared.paths, [".mdkg/design/b"]);
    assert_eq!(prepared.preimages.len(), 2);
    repo.external(".mdkg/design/base.md", b"changed");
    assert!(matches!(
        writer.publish(&lock, &prepared),
        Err(Error::Stale { path }) if path == ".mdkg/design/base.md"
    ));
}

#[test]
fn a_prepared_record_without_its_pin_is_never_published() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let req = request(&writer, "unpinned", vec![put(".mdkg/design/u", b"u")]);
    let prepared = writer.prepare(&lock, &req).expect("prepare");
    repo.git(&[
        "update-ref",
        "-d",
        &format!("refs/mdkg/pending/{}", prepared.id),
    ]);
    assert!(matches!(
        writer.publish(&lock, &prepared),
        Err(Error::Invalid(_))
    ));
    assert_eq!(read(&writer, ".mdkg/design/u"), None);
}

#[test]
fn private_folders_are_created_private_and_tightened() {
    let repo = Repo::new();
    let graph = repo.0.join(".git/mdkg/graph");
    let _ = repo
        .writer(LockMode::Local)
        .lock(LockKind::Shared)
        .expect("lock");
    let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&graph), 0o700);
    std::fs::set_permissions(&graph, std::fs::Permissions::from_mode(0o755)).expect("widen");
    let _ = repo
        .writer(LockMode::Local)
        .lock(LockKind::Shared)
        .expect("lock");
    assert_eq!(mode(&graph), 0o700);
}

#[test]
fn the_lock_trusts_local_filesystems_and_refuses_ecryptfs() {
    // Linux magics: ext4 and overlayfs are trusted; eCryptfs, NFS, FUSE and
    // an unknown magic are not.
    assert_eq!(untrusted_by_magic(0xEF53), None);
    assert_eq!(untrusted_by_magic(0x794C_7630), None);
    for refused in [0xF15F_u32, 0x6969, 0x6573_5546, 0x1234_5678] {
        assert_eq!(
            untrusted_by_magic(refused),
            Some(format!("{refused:#x}")),
            "{refused:#x}"
        );
    }
    // BSD names: apfs is trusted; nfs, smbfs, macfuse and ecryptfs are not.
    assert_eq!(untrusted_by_name("apfs"), None);
    for refused in ["nfs", "smbfs", "macfuse", "ecryptfs"] {
        assert_eq!(untrusted_by_name(refused), Some(refused.to_owned()));
    }
    assert!(!trusted_list().contains("ecryptfs"));
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    assert!(trusted_list().contains("ext2/ext3/ext4"));
}

/// A crash after publication and before the intent's cleanup, then a later
/// write that turns the request's folder into a file: replay finds the
/// request already applied and superseded, and cleans up the intent and pin.
#[test]
fn replay_after_publication_and_a_folder_replaced_by_a_file() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    let first = request(
        &writer,
        "folder-crash",
        vec![put(".mdkg/design/group.md/child.md", b"child")],
    );
    let published = {
        let lock = writer.lock(LockKind::Exclusive).expect("lock");
        let prepared = writer.prepare(&lock, &first).expect("prepare");
        journal.record(&intent_of(&prepared)).expect("intent");
        writer.publish(&lock, &prepared).expect("publish")
        // Crash: the intent and the pin stay.
    };
    let delete = request(
        &writer,
        "folder-delete",
        vec![Change::Delete {
            path: ".mdkg/design/group.md/child.md".into(),
        }],
    );
    writer.write(&delete).expect("delete child");
    let file = request(
        &writer,
        "folder-file",
        vec![put(".mdkg/design/group.md", b"file")],
    );
    writer.write(&file).expect("file in its place");
    let tip = writer.snapshot().expect("tip");
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let replayed = journal
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(
        matches!(
            replayed.as_slice(),
            [Replayed::Published { id, receipt }]
                if id == "folder-crash"
                    && receipt.already_applied
                    && receipt.superseded
                    && receipt.commit == published.commit
        ),
        "{replayed:?}"
    );
    assert!(journal.pending().expect("pending").is_empty());
    assert!(writer.pending().expect("pins").is_empty());
    // Nothing doubled: the graph tip didn't move.
    assert_eq!(writer.snapshot().expect("tip").commit(), tip.commit());
    assert_eq!(
        read(&writer, ".mdkg/design/group.md"),
        Some(b"file".to_vec())
    );
}

/// orchestrator #7002: a no-op has no receipt; its retry after an
/// intervening write is refused by its preimage, never applied over it.
#[test]
fn a_retried_no_op_after_an_intervening_write_is_refused_by_its_preimage() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let same = request(
        &writer,
        "noop-retry",
        vec![put(".mdkg/design/base.md", b"base\n")],
    );
    assert!(matches!(writer.write(&same), Err(Error::NoChange)));
    // Nothing changed: still a no-op, and no receipt was kept.
    assert!(matches!(writer.write(&same), Err(Error::NoChange)));
    assert!(
        !repo
            .0
            .join(".git/mdkg/graph/receipts/noop-retry.json")
            .exists()
    );
    let other = request(
        &writer,
        "intervening",
        vec![put(".mdkg/design/base.md", b"changed\n")],
    );
    writer.write(&other).expect("intervening write");
    assert!(matches!(
        writer.write(&same),
        Err(Error::Stale { path }) if path == ".mdkg/design/base.md"
    ));
    assert_eq!(
        read(&writer, ".mdkg/design/base.md"),
        Some(b"changed\n".to_vec())
    );
}

/// Terra #6994: a create that changes nothing is refused, never a no-op.
#[test]
fn a_create_that_changes_nothing_is_refused() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    let writer = RefWriter::open(&repo.0, GRAPH, Some(MAIN), LockMode::Local).expect("creator");
    let create = Request {
        id: "empty-create".into(),
        base: None,
        changes: vec![Change::Delete {
            path: ".mdkg/design/absent".into(),
        }],
        message: "init".into(),
        trailers: vec![],
    };
    let err = writer.write(&create).expect_err("nothing to create");
    assert!(matches!(err, Error::Invalid(_)), "{err}");
    assert!(writer.tip().expect("tip").is_none());
}

/// orchestrator #7181: pins move only under the exclusive graph lock.
#[test]
fn unpinning_needs_the_exclusive_lock() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "fenced", vec![put(".mdkg/design/f", b"f")]);
    let prepared = {
        let lock = writer.lock(LockKind::Exclusive).expect("lock");
        writer.prepare(&lock, &req).expect("prepare")
    };
    let shared = writer.lock(LockKind::Shared).expect("shared");
    assert!(writer.unpin(&shared, &prepared).is_err());
    assert!(
        writer
            .unpin_orphan(&shared, &prepared.id, &prepared.commit)
            .is_err()
    );
    drop(shared);
    assert_eq!(writer.pending().expect("pins").len(), 1);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    writer.unpin(&lock, &prepared).expect("unpin");
    assert!(writer.pending().expect("pins").is_empty());
}

/// Terra #7173: a prepared record whose preimage set was edited no longer
/// matches its commit, so it's never published against a dropped preimage.
#[test]
fn an_edited_preimage_set_fails_verification() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "preimages",
        vec![
            put(".mdkg/design/base.md", b"base\n"),
            put(".mdkg/design/p", b"p"),
        ],
    );
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    assert_eq!(prepared.paths, [".mdkg/design/p"]);
    assert_eq!(prepared.preimages.len(), 2);
    prepared.preimages.retain(|p| p != ".mdkg/design/base.md");
    let err = writer
        .publish(&lock, &prepared)
        .expect_err("dropped preimage");
    assert!(err.to_string().contains("preimages"), "{err}");
    // The record as prepared still publishes.
    let prepared = writer.prepare(&lock, &req).expect("prepare again");
    writer.publish(&lock, &prepared).expect("publish");
}

/// Terra #7173: a pinned request on a ref that now holds another graph is
/// refused at prepare, before any intent is recorded.
#[test]
fn a_pinned_request_on_another_graph_is_refused_at_prepare() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "moved-graph", vec![put(".mdkg/design/m", b"m")]);
    {
        let lock = writer.lock(LockKind::Exclusive).expect("lock");
        writer.prepare(&lock, &req).expect("prepare");
    }
    repo.external(
        ".mdkg/config.json",
        br#"{"graph_id":"0192f3a0-0000-7000-8000-0000000000ee"}"#,
    );
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    assert!(matches!(
        writer.prepare(&lock, &req),
        Err(Error::GraphIdentity(_))
    ));
}

/// Terra #7173: an existing folder keeps its owner bits; one its owner
/// can't use is refused, never widened.
#[test]
fn private_dir_never_widens_an_existing_folder() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = Repo::new();
    let parent = repo.0.join("dirs");
    std::fs::create_dir(&parent).expect("parent");
    let wide = parent.join("wide");
    std::fs::create_dir(&wide).expect("wide");
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    private_dir(&wide, &parent).expect("tightened");
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&wide), 0o700);
    let locked = parent.join("locked");
    std::fs::create_dir(&locked).expect("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    assert!(private_dir(&locked, &parent).is_err());
    assert_eq!(mode(&locked), 0o555, "never widened");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
    let fresh = parent.join("fresh");
    private_dir(&fresh, &parent).expect("created");
    assert_eq!(mode(&fresh), 0o700);
}

/// Terra #7300: a lock refusal reads as one sentence.
#[test]
fn a_lock_refusal_renders_as_one_sentence() {
    let err = Error::LockRequired("intent replay needs this clone's exclusive graph lock".into());
    assert_eq!(
        err.to_string(),
        "graph lock refused: intent replay needs this clone's exclusive graph lock"
    );
}

/// Terra #7480: a crash right after the graph ref moved, before the
/// receipt was written, then a later write that turns the request's folder
/// into a file: replay finds the publish by history, keeps it superseded,
/// and cleans up.
#[test]
fn replay_after_a_receipt_free_crash_and_a_folder_replaced_by_a_file() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    let first = request(
        &writer,
        "receipt-free",
        vec![put(".mdkg/design/group.md/child.md", b"child")],
    );
    {
        let lock = writer.lock(LockKind::Exclusive).expect("lock");
        let prepared = writer.prepare(&lock, &first).expect("prepare");
        journal.record(&intent_of(&prepared)).expect("intent");
        let crashed = writer.publish_with(&lock, &prepared, &mut |step| match step {
            Step::Published => Err(invalid("crash after the CAS")),
            _ => Ok(()),
        });
        assert!(crashed.is_err());
    }
    assert!(
        !repo
            .0
            .join(".git/mdkg/graph/receipts/receipt-free.json")
            .exists()
    );
    let published = repo.git(&["rev-parse", GRAPH]);
    writer
        .write(&request(
            &writer,
            "rf-delete",
            vec![Change::Delete {
                path: ".mdkg/design/group.md/child.md".into(),
            }],
        ))
        .expect("delete child");
    writer
        .write(&request(
            &writer,
            "rf-file",
            vec![put(".mdkg/design/group.md", b"file")],
        ))
        .expect("file in its place");
    let tip = writer.snapshot().expect("tip");
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let replayed = journal
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(
        matches!(
            replayed.as_slice(),
            [Replayed::Published { id, receipt }]
                if id == "receipt-free"
                    && receipt.already_applied
                    && receipt.superseded
                    && receipt.commit == published
        ),
        "{replayed:?}"
    );
    assert!(journal.pending().expect("pending").is_empty());
    assert!(writer.pending().expect("pins").is_empty());
    assert_eq!(writer.snapshot().expect("tip").commit(), tip.commit());
    // The receipt is recorded, so a rerun from any base finds the publish
    // without walking history (Opus #8542).
    assert!(
        repo.0
            .join(".git/mdkg/graph/receipts/receipt-free.json")
            .is_file()
    );
}

/// Terra #8228: a preimage list edited to smuggle two paths into one
/// (with a NUL) fails verification instead of hashing to the same digest.
#[test]
fn a_preimage_with_a_nul_fails_verification() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(
        &writer,
        "nul-preimage",
        vec![put(".mdkg/design/a", b"a"), put(".mdkg/design/b", b"b")],
    );
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    prepared.preimages = vec![".mdkg/design/a\0.mdkg/design/b".into()];
    prepared.paths.clear();
    assert!(writer.publish(&lock, &prepared).is_err());
}

/// Terra #8228: a squash that also changed another graph file is still the
/// request's publish, and its retry returns the receipt.
#[test]
fn a_retry_finds_its_publish_inside_a_wider_squash() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let base = writer.snapshot().expect("base");
    let req = request(&writer, "squashed", vec![put(".mdkg/design/s", b"s")]);
    let published = writer.write(&req).expect("publish").commit;
    // Rewrite history as one squash carrying the marker and an extra file.
    let message = repo.git(&["log", "-1", "--format=%B", &published]);
    let extra = repo.external(".mdkg/design/extra", b"x");
    let tree = repo.git(&["rev-parse", &format!("{extra}^{{tree}}")]);
    let squash = repo.git(&["commit-tree", &tree, "-p", base.commit(), "-m", &message]);
    repo.git(&["update-ref", GRAPH, &squash]);
    std::fs::remove_file(repo.0.join(".git/mdkg/graph/receipts/squashed.json")).ok();
    let again = writer.write(&req).expect("retry");
    assert!(again.already_applied);
    assert_eq!(again.commit, squash);
}

/// Opus #8328: a bare id of an object that exists but isn't a commit is an
/// error, never absence.
#[test]
fn a_bare_id_of_a_tree_is_an_error_not_absence() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let tree = repo.git(&["rev-parse", &format!("{GRAPH}^{{tree}}")]);
    assert!(writer.snapshot_at(&tree).is_err());
    assert!(writer.resolve(&tree).is_err());
}

/// orchestrator #8586: after a rewind strands a receipt, orphan cleanup
/// keeps the pin and reports the request unpublished.
#[test]
fn a_stale_receipt_never_drops_a_pin() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let base = writer.snapshot().expect("base");
    let req = request(&writer, "rewound", vec![put(".mdkg/design/r", b"r")]);
    writer.write(&req).expect("publish");
    // The graph ref is rewound; the receipt now names a stranded commit.
    repo.git(&["update-ref", GRAPH, base.commit()]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let prepared = writer.prepare(&lock, &req).expect("fresh prepare");
    let replayed = journal(&repo)
        .replay(&writer, &lock, |_| Ok::<_, String>(false))
        .expect("replay");
    assert!(
        matches!(replayed.as_slice(), [Replayed::Failed { .. }]),
        "{replayed:?}"
    );
    assert_eq!(
        writer.pending().expect("pins"),
        vec![(prepared.id.clone(), prepared.commit.clone())]
    );
}

/// orchestrator #8234: an intent an earlier build prepared (no preimage
/// trailer) is published only onto its own parent, else refused; once on
/// the graph, it's recognized as applied.
#[test]
fn an_older_intent_without_the_trailer_is_never_rebuilt() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "older", vec![put(".mdkg/design/o", b"o")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    // As an earlier build made it: the same commit without the trailer.
    let body = repo.git(&["log", "-1", "--format=%B", &prepared.commit]);
    let message: String = body
        .lines()
        .filter(|l| !l.starts_with("Mdkg-Preimages: "))
        .collect::<Vec<_>>()
        .join("\n");
    let parent = prepared.parent.clone().expect("parent");
    let old = repo.git(&["commit-tree", &prepared.tree, "-p", &parent, "-m", &message]);
    repo.git(&[
        "update-ref",
        &format!("refs/mdkg/pending/{}", prepared.id),
        &old,
    ]);
    prepared.commit = old.clone();
    // The graph moved: refused, never rebuilt.
    repo.external(".mdkg/design/other", b"x");
    assert!(writer.publish(&lock, &prepared).is_err());
    // Back on its own parent: published as it is.
    repo.git(&["update-ref", GRAPH, &parent]);
    let receipt = writer.publish(&lock, &prepared).expect("onto its parent");
    assert_eq!(receipt.commit, old);
    // Already on the graph: applied, no second write.
    let again = writer.publish(&lock, &prepared).expect("applied");
    assert!(again.already_applied);
}

/// orchestrator #8398: FAT and exFAT are refused.
#[test]
fn fat_and_exfat_are_refused() {
    for magic in [0x0000_4D44_u32, 0x2011_BAB0] {
        assert!(untrusted_by_magic(magic).is_some(), "{magic:#x}");
    }
    for name in ["msdos", "exfat"] {
        assert!(untrusted_by_name(name).is_some(), "{name}");
    }
}

/// orchestrator #8013: the journal remembers a request whole; a retry by
/// id resends the original base, so a no-op retried after an intervening
/// write is refused, and a different request under the id is id reuse.
#[test]
fn a_remembered_request_is_retried_with_its_original_base() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    let original = request(
        &writer,
        "cli-retry",
        vec![put(".mdkg/design/base.md", b"base\n")],
    );
    journal.remember(&original).expect("remember");
    journal.remember(&original).expect("the same again");
    assert!(matches!(writer.write(&original), Err(Error::NoChange)));
    writer
        .write(&request(
            &writer,
            "intervening",
            vec![put(".mdkg/design/base.md", b"changed\n")],
        ))
        .expect("intervening write");
    // A retry by id (the CLI or MCP) reuses the remembered request.
    let stored = journal
        .request("cli-retry")
        .expect("read")
        .expect("remembered");
    assert_eq!(stored, original);
    assert!(matches!(writer.write(&stored), Err(Error::Stale { .. })));
    // A new request under the same id with a fresh base is id reuse.
    let rebuilt = request(
        &writer,
        "cli-retry",
        vec![put(".mdkg/design/base.md", b"base\n")],
    );
    assert!(matches!(
        journal.remember(&rebuilt),
        Err(Error::RequestIdReused(_))
    ));
    journal.forget("cli-retry").expect("forget");
    assert!(journal.request("cli-retry").expect("read").is_none());
}

/// orchestrator #9415 (Terra #9366): an intent whose recorded path list
/// was emptied never matches a cherry-pick that kept its marker but dropped
/// the change; replay keeps the real intent and pin.
#[test]
fn an_emptied_intent_never_matches_a_marker_only_cherry_pick() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    let req = request(&writer, "emptied", vec![put(".mdkg/design/e", b"e")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    // A conflict-resolved cherry-pick: the marker, no change of its own.
    let message = repo.git(&["log", "-1", "--format=%B", &prepared.commit]);
    let tip = repo.git(&["rev-parse", GRAPH]);
    let tree = repo.git(&["rev-parse", &format!("{tip}^{{tree}}")]);
    let pick = repo.git(&["commit-tree", &tree, "-p", &tip, "-m", &message]);
    repo.git(&["update-ref", GRAPH, &pick]);
    // The intent's path list, emptied.
    prepared.paths.clear();
    journal.record(&intent_of(&prepared)).expect("intent");
    let replayed = journal
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(
        matches!(replayed.as_slice(), [Replayed::Failed { .. }]),
        "{replayed:?}"
    );
    assert_eq!(journal.pending().expect("pending").len(), 1, "intent kept");
    assert_eq!(writer.pending().expect("pins").len(), 1, "pin kept");
}

/// orchestrator #9415: a link where a private folder belongs is refused,
/// and the link's target keeps its mode.
#[test]
fn private_dir_never_changes_a_link_target() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = Repo::new();
    let parent = repo.0.join("dirs");
    std::fs::create_dir(&parent).expect("parent");
    let target = repo.0.join("target");
    std::fs::create_dir(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    symlink(&target, parent.join("linked")).expect("link");
    assert!(private_dir(&parent.join("linked"), &parent).is_err());
    let mode = std::fs::metadata(&target)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "the target is untouched");
}

/// Claude #10147: a temporary name a crashed process left (its pid reused)
/// doesn't make `remember` refuse a request nobody recorded, and a stale
/// leftover is swept.
#[test]
fn a_stale_temporary_never_reads_as_a_reused_request() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    assert!(journal.request("crashed").expect("read").is_none());
    let dir = graph_dir(&repo.0.join(".git"))
        .expect("graph dir")
        .join("requests");
    let next = crate::intent::TEMP_SEQ.load(std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    for n in next..next + 64 {
        std::fs::write(dir.join(format!(".crashed.json.{pid}-{n}.tmp")), b"partial")
            .expect("leftover");
    }
    let stale = dir.join(".crashed.json.1-0.tmp");
    let old = std::fs::File::create(&stale).expect("stale");
    old.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
        .expect("age");
    drop(old);
    let req = request(&writer, "crashed", vec![put(".mdkg/design/c", b"c")]);
    journal
        .remember(&req)
        .expect("remembered despite the leftovers");
    assert_eq!(journal.request("crashed").expect("read"), Some(req));
    assert!(!stale.exists(), "an hour-old leftover is swept");
}

/// Claude #10147: a prepared record whose commit isn't its pinned commit
/// is refused and kept.
#[test]
fn a_record_naming_another_commit_than_its_pin_is_refused() {
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let req = request(&writer, "repinned", vec![put(".mdkg/design/r", b"r")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    prepared.commit = repo.git(&["rev-parse", GRAPH]);
    let err = writer.publish(&lock, &prepared).expect_err("refused");
    assert!(
        err.to_string().contains("doesn't match its pinned commit"),
        "{err}"
    );
    assert_eq!(writer.pending().expect("pins").len(), 1, "pin kept");
}

/// Terra #10216: an intent naming its pinned commit as its own parent, with
/// no paths, diffs to nothing; it's refused against the pin's real parent,
/// so a marker-only cherry-pick never settles it.
#[test]
fn an_intent_naming_another_parent_than_its_pin_is_kept() {
    use crate::intent::Replayed;
    let repo = Repo::new();
    let writer = repo.writer(LockMode::Local);
    let journal = journal(&repo);
    let req = request(&writer, "reparented", vec![put(".mdkg/design/p", b"p")]);
    let lock = writer.lock(LockKind::Exclusive).expect("lock");
    let mut prepared = writer.prepare(&lock, &req).expect("prepare");
    let message = repo.git(&["log", "-1", "--format=%B", &prepared.commit]);
    let tip = repo.git(&["rev-parse", GRAPH]);
    let tree = repo.git(&["rev-parse", &format!("{tip}^{{tree}}")]);
    let pick = repo.git(&["commit-tree", &tree, "-p", &tip, "-m", &message]);
    repo.git(&["update-ref", GRAPH, &pick]);
    prepared.parent = Some(prepared.commit.clone());
    prepared.paths.clear();
    journal.record(&intent_of(&prepared)).expect("intent");
    let replayed = journal
        .replay(&writer, &lock, |_| Ok::<_, String>(true))
        .expect("replay");
    assert!(
        matches!(replayed.as_slice(), [Replayed::Failed { .. }]),
        "{replayed:?}"
    );
    assert_eq!(journal.pending().expect("pending").len(), 1, "intent kept");
    assert_eq!(writer.pending().expect("pins").len(), 1, "pin kept");
}
