use super::*;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub(super) struct Repo(pub(super) PathBuf);

impl Repo {
    pub(super) fn new(format: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-receipt-span-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&dir).expect("fixture folder");
        let r = Self(fs::canonicalize(dir).expect("canonical fixture"));
        r.git(&[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
        ]);
        r
    }

    pub(super) fn git(&self, args: &[&str]) -> String {
        self.git_input(args, None)
    }

    pub(super) fn git_input(&self, args: &[&str], input: Option<&[u8]>) -> String {
        let mut cmd = Command::new("git");
        for var in super::super::GIT_ENV
            .iter()
            .chain(super::super::GIT_ENV_CLEARED)
        {
            cmd.env_remove(var);
        }
        cmd.current_dir(&self.0)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=receipt fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args);
        let out = super::super::run_bounded(&mut cmd, input, 16 << 20, Duration::from_secs(60))
            .expect("fixture git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("Git text")
            .trim_end()
            .into()
    }

    pub(super) fn pin(&self) -> Pinned {
        Pinned {
            top: self.0.clone(),
            git_dir: self.0.join(".git"),
        }
    }

    pub(super) fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("fixture parents");
        fs::write(path, bytes).expect("fixture content");
    }

    pub(super) fn commit(&self) -> String {
        self.git(&["add", "--all", "--"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", "fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unborn_and_first_commit_have_an_exact_span() {
    let r = Repo::new("sha1");
    let empty = harvest(&r.pin(), None).expect("unborn span");
    assert_eq!(empty.branch.as_deref(), Some("refs/heads/main"));
    assert_eq!(empty.head, None);
    assert!(empty.commits.is_empty());
    r.write("task.md", "initial");
    let first = r.commit();
    let span = harvest(&r.pin(), None).expect("first span");
    assert_eq!(span.head.as_deref(), Some(first.as_str()));
    assert_eq!(span.commits.len(), 1);
    assert!(span.commits[0].parents.is_empty());
    assert_eq!(span.commits[0].changes[0].path, "task.md");
    assert!(span.commits[0].changes[0].before.is_none());
    assert_eq!(
        span.commits[0].changes[0].after.as_ref().unwrap().mode,
        "100644"
    );
}

#[test]
fn only_committed_turn_changes_are_reported_and_baseline_can_be_unchanged() {
    let r = Repo::new("sha1");
    r.write("old", "before");
    let start = r.commit();
    let empty = harvest(&r.pin(), Some(&start)).expect("unchanged");
    assert!(empty.commits.is_empty());
    r.write("old", "after");
    let end = r.commit();
    r.write("old", "unstaged");
    r.write("staged", "not a commit");
    r.git(&["add", "staged"]);
    r.write("untracked", "also not a commit");
    let span = harvest(&r.pin(), Some(&start)).expect("turn span");
    assert_eq!(span.start.as_deref(), Some(start.as_str()));
    assert_eq!(span.head.as_deref(), Some(end.as_str()));
    assert_eq!(span.commits.len(), 1);
    assert_eq!(span.commits[0].parents, [start]);
    assert_eq!(span.commits[0].changes.len(), 1);
    assert_eq!(span.commits[0].changes[0].path, "old");
    let object = &span.commits[0].changes[0].after.as_ref().unwrap().object;
    assert_eq!(r.git(&["cat-file", "blob", object]), "after");
}

#[test]
fn intermediate_reopen_and_identical_done_bytes_are_not_lost() {
    let r = Repo::new("sha1");
    let path = ".mdkg/work/task-example.md";
    let done = "---\nid: task-example\ntype: task\ntitle: Example\nstatus: done\n---\n";
    r.write(path, done);
    let start = r.commit();
    r.write(path, done.replace("status: done", "status: todo"));
    let reopened = r.commit();
    r.write(path, done);
    let finished = r.commit();
    assert!(
        r.git(&["diff", "--name-only", &start, &finished])
            .is_empty()
    );
    let span = harvest(&r.pin(), Some(&start)).expect("transitions");
    assert_eq!(
        span.commits
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [reopened, finished]
    );
    let reopen = &span.commits[0].changes[0];
    let finish = &span.commits[1].changes[0];
    assert_eq!(reopen.before, finish.after);
    assert_eq!(reopen.after, finish.before);
    assert_ne!(finish.before, finish.after);
    assert_eq!(
        r.git(&["cat-file", "blob", &finish.after.as_ref().unwrap().object]),
        done.trim_end()
    );
}

#[test]
fn merged_history_is_included_in_parent_before_child_order() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.git(&["checkout", "-q", "-b", "side"]);
    r.write("side", "imported history");
    let side = r.commit();
    r.git(&["checkout", "-q", "main"]);
    r.write("own", "own change");
    let own = r.commit();
    r.git(&["merge", "--no-ff", "--no-edit", "side"]);
    let merged = r.git(&["rev-parse", "HEAD"]);
    let span = harvest(&r.pin(), Some(&start)).expect("merge span");
    assert_eq!(span.commits.len(), 3);
    let ids: BTreeSet<_> = span.commits.iter().map(|c| c.id.clone()).collect();
    assert_eq!(
        ids,
        BTreeSet::from([side.clone(), own.clone(), merged.clone()])
    );
    assert_eq!(span.commits[2].id, merged);
    assert_eq!(span.commits[2].parents, [own, side]);
    assert_eq!(span.commits[2].changes.len(), 1);
    assert_eq!(span.commits[2].changes[0].path, "side");
}

#[test]
fn newly_merged_shallow_history_is_unavailable_even_with_an_ancestor_start() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.git(&["checkout", "-q", "-b", "side"]);
    r.write("side", "earlier history");
    r.commit();
    r.write("side", "shallow boundary");
    let boundary = r.commit();
    r.git(&["checkout", "-q", "main"]);
    r.write("own", "own change");
    r.commit();
    r.git(&["merge", "--no-ff", "--no-edit", "side"]);
    r.write(".git/shallow", format!("{boundary}\n"));
    r.git(&["merge-base", "--is-ancestor", &start, "HEAD"]);
    let error = harvest(&r.pin(), Some(&start)).expect_err("partial merge history");
    assert!(error.message.contains("shallow history is incomplete"));
}

#[test]
fn linked_worktree_head_is_used_instead_of_the_common_checkout() {
    let r = Repo::new("sha1");
    let start = r.commit();
    let linked = r.0.join("linked");
    r.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "worker",
        linked.to_str().unwrap(),
    ]);
    let worker = Repo(linked);
    worker.write("task", "worker change");
    let end = worker.commit();
    let pin = Pinned {
        top: worker.0.clone(),
        git_dir: PathBuf::from(worker.git(&["rev-parse", "--absolute-git-dir"])),
    };
    let span = harvest(&pin, Some(&start)).expect("worker span");
    assert_eq!(span.branch.as_deref(), Some("refs/heads/worker"));
    assert_eq!(span.head, Some(end));
    assert_eq!(span.commits.len(), 1);
    assert_eq!(span.commits[0].changes[0].path, "task");
    assert_eq!(r.git(&["rev-parse", "HEAD"]), start);
}

#[test]
fn diff_config_does_not_hide_submodules_or_change_path_order() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.write("z", "last");
    r.write("a", "first");
    let target = r.commit();
    r.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{target},module"),
    ]);
    r.git(&["commit", "-q", "-m", "record submodule"]);
    r.write(".git/reverse-order", "module\nz\na\n");
    r.git(&["config", "diff.orderFile", ".git/reverse-order"]);
    r.git(&["config", "diff.ignoreSubmodules", "all"]);
    let span = harvest(&r.pin(), Some(&start)).expect("configured diff");
    assert_eq!(
        span.commits[0]
            .changes
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert_eq!(span.commits[1].changes[0].path, "module");
    assert_eq!(
        span.commits[1].changes[0].after.as_ref().unwrap().mode,
        "160000"
    );
}

#[test]
fn unrelated_end_and_missing_start_refuse_instead_of_guessing() {
    let r = Repo::new("sha1");
    r.write("task", "before");
    let start = r.commit();
    r.git(&["checkout", "-q", "--orphan", "replacement"]);
    r.write("task", "different history");
    let end = r.commit();
    let err = harvest(&r.pin(), Some(&start)).expect_err("not an ancestor");
    assert!(
        err.message
            .contains(&format!("{end} does not contain {start}"))
    );
    assert!(harvest(&r.pin(), Some(&"a".repeat(40))).is_err());
    assert!(harvest(&r.pin(), Some("--all")).is_err());
}

#[test]
fn detached_head_is_reported_but_broken_head_is_not_unborn() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.git(&["checkout", "-q", "--detach", &start]);
    let detached = harvest(&r.pin(), Some(&start)).expect("detached");
    assert_eq!(detached.branch, None);
    assert_eq!(detached.head.as_deref(), Some(start.as_str()));
    r.write(".git/HEAD", format!("{}\n", "a".repeat(40)));
    assert!(harvest(&r.pin(), None).is_err());
    r.write(".git/HEAD", "ref: refs/heads/broken\n");
    r.write(".git/refs/heads/broken", format!("{}\n", "a".repeat(40)));
    assert!(harvest(&r.pin(), None).is_err());
}

#[test]
fn sha256_history_retains_full_object_ids() {
    let r = Repo::new("sha256");
    r.write("task", "done");
    let head = r.commit();
    let span = harvest(&r.pin(), None).expect("sha256 span");
    assert_eq!(span.head, Some(head));
    assert_eq!(span.commits[0].id.len(), 64);
    assert_eq!(
        span.commits[0].changes[0]
            .after
            .as_ref()
            .unwrap()
            .object
            .len(),
        64
    );
}

#[test]
fn rename_paths_with_tabs_and_newlines_and_symlink_modes_are_preserved() {
    let r = Repo::new("sha1");
    let old = "old\tname\n.md";
    let new = "new\tname\n.md";
    r.write(old, "done");
    let start = r.commit();
    fs::rename(r.0.join(old), r.0.join(new)).expect("rename fixture");
    std::os::unix::fs::symlink("missing-target", r.0.join("link")).expect("fixture link");
    r.commit();
    let span = harvest(&r.pin(), Some(&start)).expect("exact paths");
    let changes = &span.commits[0].changes;
    assert_eq!(changes.len(), 3);
    let removed = changes.iter().find(|c| c.path == old).expect("old path");
    let added = changes.iter().find(|c| c.path == new).expect("new path");
    assert_eq!(removed.before, added.after);
    assert!(removed.after.is_none() && added.before.is_none());
    let link = changes.iter().find(|c| c.path == "link").expect("link");
    assert_eq!(link.after.as_ref().unwrap().mode, "120000");
}

#[test]
fn collection_limits_refuse_whole_results_without_truncation() {
    let r = Repo::new("sha1");
    r.write("one", "one");
    r.write("two", "two");
    r.commit();
    r.commit();
    for limits in [
        Limits {
            commits: 1,
            ..Limits::default()
        },
        Limits {
            changes: 1,
            ..Limits::default()
        },
        Limits {
            bytes: 1,
            ..Limits::default()
        },
        Limits {
            time: Duration::ZERO,
            ..Limits::default()
        },
    ] {
        assert!(
            harvest_with(&r.pin(), None, limits)
                .expect_err("limit")
                .message
                .contains("limit")
        );
    }
}

#[test]
fn byte_budget_is_shared_across_successful_git_reads() {
    let r = Repo::new("sha1");
    let head = r.commit();
    let pin = r.pin();
    let mut reader = Reader::new(&pin, Limits::default()).expect("reader");
    // Either response fits alone; their combined output exceeds the
    // remaining collection budget by one byte, after earlier reads succeeded.
    reader.limits.bytes = reader.used + 2 * (head.len() + 1) - 1;
    assert_eq!(
        reader.ok(&["rev-parse", "HEAD"]).expect("first read"),
        format!("{head}\n").as_bytes()
    );
    let error = reader
        .ok(&["rev-parse", "HEAD"])
        .expect_err("cumulative cap");
    assert!(error.message.contains("collection limit"));
}

#[test]
fn malformed_or_non_utf8_diff_is_not_silently_omitted() {
    let header = format!(":000000 100644 {} {} A\0", "0".repeat(40), "a".repeat(40));
    for suffix in [b"".as_slice(), b"path", b"\xff\0", b"../outside\0", b"\0"] {
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(suffix);
        assert!(parse_diff(&raw).is_err(), "{suffix:?}");
    }
    assert!(parse_diff(b"junk\0file\0").is_err());
}

#[test]
fn an_unborn_end_does_not_erase_the_starting_history() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.git(&["checkout", "-q", "--orphan", "unborn"]);
    let error = harvest(&r.pin(), Some(&start)).expect_err("history disappeared");
    assert!(error.message.contains("starting history disappeared"));
}

#[test]
fn head_change_at_the_final_observation_refuses_the_whole_span() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.write("one", "first");
    r.commit();
    let error = harvest_observed(&r.pin(), Some(&start), Limits::default(), || {
        r.write("two", "raced collection");
        r.commit();
    })
    .expect_err("head changed");
    assert!(error.message.contains("HEAD changed while collecting"));
}

#[test]
fn unreadable_hook_configuration_is_not_treated_as_no_hooks() {
    let r = Repo::new("sha1");
    r.commit();
    r.write(".git/config", "[unclosed\n");
    let error = harvest(&r.pin(), None).expect_err("invalid config");
    assert!(error.message.contains("can't read Git hook configuration"));
}

#[test]
fn replacement_refs_and_grafts_do_not_change_observed_history() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.git(&["checkout", "-q", "-b", "side"]);
    r.write("side", "one");
    r.commit();
    r.write("side", "two");
    let side = r.commit();
    r.git(&["checkout", "-q", "main"]);
    r.write("own", "three");
    r.commit();
    r.git(&["merge", "--no-ff", "--no-edit", "side"]);
    let expected = harvest(&r.pin(), Some(&start)).expect("original span");
    r.git(&["replace", &side, &start]);
    r.write(".git/info/grafts", format!("{side}\n"));
    assert_eq!(
        harvest(&r.pin(), Some(&start)).expect("unaltered span"),
        expected
    );
}

#[test]
fn thousands_of_commits_including_empty_ones_are_read_as_one_diff_batch() {
    let r = Repo::new("sha1");
    let start = r.commit();
    let mut import = String::new();
    for i in 1..=3001 {
        import.push_str(&format!(
            "commit refs/heads/main\ncommitter receipt fixture <fixture@example.invalid> {time} +0000\ndata 4\nspan\n",
            time=1_000_000_000+i,
        ));
        if i == 1 {
            import.push_str(&format!("from {start}\n"));
        }
        // Every tenth commit changes a file; empty commits still need frames.
        if i % 10 == 0 {
            import.push_str(&format!(
                "M 100644 inline task\ndata {}\n{i}\n",
                i.to_string().len()
            ));
        }
        import.push('\n');
    }
    r.git_input(&["fast-import", "--quiet"], Some(import.as_bytes()));
    let span = harvest(&r.pin(), Some(&start)).expect("large complete span");
    assert_eq!(span.commits.len(), 3001);
    assert_eq!(
        span.commits
            .iter()
            .filter(|c| !c.changes.is_empty())
            .count(),
        300
    );
    assert_eq!(span.commits.first().unwrap().parents, [start]);
    assert_eq!(
        span.commits.last().unwrap().id,
        span.head.as_deref().unwrap()
    );
    for pair in span.commits.windows(2) {
        assert_eq!(pair[1].parents, [pair[0].id.clone()]);
    }
}

#[test]
fn excess_stderr_cannot_hide_behind_the_retained_prefix() {
    let r = Repo::new("sha1");
    let pin = r.pin();
    let mut reader = Reader::new(&pin, Limits::default()).expect("reader");
    // A per-invocation Git alias emits valid stdout and >16 MiB stderr. Go
    // through run_input so a regression that skips account also fails, without
    // replacing Git or mutating the test process's PATH/configuration.
    let alias = "alias.receipt-noise=!dd if=/dev/zero bs=65536 count=257 1>&2 2>/dev/null; printf 'sha1\\n'";
    assert!(
        reader
            .run_input(&["-c", alias, "receipt-noise"], Some(b""))
            .expect_err("discarded stderr")
            .message
            .contains("stderr reached its capture limit")
    );
}

#[test]
fn real_batch_keeps_header_shaped_and_newline_paths_between_empty_commits() {
    let r = Repo::new("sha1");
    let start = r.commit();
    let before = r.commit();
    let header = format!(":000000 100644 {} {} A", "0".repeat(40), "b".repeat(40));
    let hash = "a".repeat(40);
    let mut paths = vec![header.as_str(), hash.as_str(), "line\nbreak"];
    for path in &paths {
        r.write(path, path);
    }
    let changed = r.commit();
    let after = r.commit();
    let span = harvest(&r.pin(), Some(&start)).expect("framed real batch");
    assert_eq!(
        span.commits
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [before.as_str(), changed.as_str(), after.as_str()]
    );
    assert!(span.commits[0].changes.is_empty());
    assert!(span.commits[2].changes.is_empty());
    paths.sort();
    assert_eq!(
        span.commits[1]
            .changes
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        paths
    );
    for change in &span.commits[1].changes {
        assert!(change.before.is_none());
        let added = change.after.as_ref().expect("added file");
        assert_eq!(added.mode, "100644");
        assert_eq!(r.git(&["cat-file", "blob", &added.object]), change.path);
    }
}

#[test]
fn diff_batch_itself_exhausts_the_remaining_collection_budget() {
    let r = Repo::new("sha1");
    let start = r.commit();
    r.commit();
    let limits = Limits {
        bytes: 8 << 10,
        ..Limits::default()
    };
    // Discovery, HEAD, ancestry, commit list and an empty diff all fit.
    harvest_with(&r.pin(), Some(&start), limits).expect("preceding reads fit");
    for i in 0..100 {
        r.write(&format!("path-{i:03}"), "added");
    }
    r.commit();
    let pin = r.pin();
    let mut reader = Reader::new(&pin, limits).expect("discovery fits");
    let (_, head) = reader.head().expect("HEAD fits");
    let head = head.expect("committed HEAD");
    reader
        .ok(&["merge-base", "--is-ancestor", &start, &head])
        .expect("ancestry fits");
    reader
        .ok(&[
            "rev-list",
            "--reverse",
            "--topo-order",
            "--parents",
            "--max-count=10001",
            &head,
            &format!("^{start}"),
            "--",
        ])
        .expect("commit list fits");
    assert!(reader.used < limits.bytes);
    // Raw diff alone exceeds the *whole* limit, not just the remainder.
    assert!(
        r.git(&[
            "diff-tree",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "-r",
            "-z",
            &head
        ])
        .len()
            > limits.bytes
    );
    let error = harvest_with(&pin, Some(&start), limits).expect_err("batch exceeds cap");
    assert!(
        error.message.contains("collection limit"),
        "{}",
        error.message
    );
}

#[test]
fn batch_diff_requires_every_commit_frame_and_complete_path_pairs() {
    let id = "a".repeat(40);
    let base = Commit {
        id: id.clone(),
        parents: Vec::new(),
        changes: Vec::new(),
    };
    for bad in [
        Vec::new(),
        format!("{id}\n").into_bytes(),
        format!("{id}\0junk\0").into_bytes(),
    ] {
        assert!(batch_diff(&bad, &mut [base.clone()], 10).is_err());
    }
    batch_diff(format!("{id}\0").as_bytes(), &mut [base], 10).expect("empty commit frame");
}
