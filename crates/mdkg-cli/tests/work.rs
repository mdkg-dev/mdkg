//! The work-state commands end to end (task-39): `anchor` commits the
//! store's chain head, `export --work` writes a bundle, and `verify`
//! checks a bundle with no repository, or the store, against an anchor.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-work");
    Scratch(fs::canonicalize(dir).expect("canonical"))
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn repo(dir: &Scratch) -> PathBuf {
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "."],
        &["commit", "-q", "-m", "init"],
    ] {
        let out = isolated(Command::new("git").arg("-C").arg(&repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    }
    repo
}

/// Runs `mdkg` in `cwd` with `args` and `--json`.
fn run_in(cwd: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(cwd)
        .args(["--as", "a"])
        .args(args)
        .arg("--json")
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "{args:?}: not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms_envelope(&envelope);
    (out.status.code().unwrap_or(-1), envelope)
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

#[test]
fn anchor_export_and_verify() {
    let dir = scratch();
    let repo = repo(&dir);
    let (code, out) = run_in(&repo, &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");

    // No anchor yet: the chain verifies, with a warning.
    let (code, out) = run_in(&repo, &["verify"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(codes(&out), ["log.anchor_missing"], "{out}");
    let store = out["data"]["store"].as_str().expect("store").to_string();

    let (code, out) = run_in(&repo, &["anchor"]);
    assert_eq!(code, 0, "{out}");
    let anchor_file = repo.join(format!(".mdkg/anchors/{store}.json"));
    assert!(anchor_file.is_file());
    assert_eq!(
        out["data"]["path"],
        format!(".mdkg/anchors/{store}.json"),
        "{out}"
    );
    let anchored = out["data"]["seq"].as_u64().expect("seq");
    let text = fs::read_to_string(&anchor_file).expect("anchor");
    assert!(text.starts_with("{\"format\":\"mdkg-anchor\""), "{text}");
    // The graph reads past its anchors.
    let (code, v) = run_in(&repo, &["validate"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        !v.to_string().contains("anchors"),
        "validate ignores .mdkg/anchors: {v}"
    );

    // More work after the anchor: still verifies against it.
    let (code, out) = run_in(&repo, &["release", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_in(&repo, &["verify"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["anchored"], anchored, "{out}");
    assert!(out["data"]["count"].as_u64() > Some(anchored), "{out}");

    // The bundle verifies with no repository, against the anchor.
    let bundle = dir.0.join("work.jsonl");
    let b = bundle.to_str().expect("utf-8");
    let (code, out) = run_in(&repo, &["export", "--work", "--out", b]);
    assert_eq!(code, 0, "{out}");
    let (code, again) = run_in(&repo, &["export", "--work", "--out", b]);
    assert_ne!(code, 0, "{again}");
    assert_eq!(codes(&again), ["io.write_failed"], "{again}");
    let outside = dir.0.join("elsewhere");
    fs::create_dir_all(&outside).expect("dir");
    let a = anchor_file.to_str().expect("utf-8");
    let (code, v) = run_in(&outside, &["verify", b, "--anchor", a]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["source"], "bundle");
    assert_eq!(v["data"]["head"], out["data"]["head"], "{v}");
    assert_eq!(v["data"]["anchored"], anchored, "{v}");

    // A changed record fails, exit 2.
    let tampered = dir.0.join("tampered.jsonl");
    let changed = fs::read_to_string(&bundle).expect("bundle").replacen(
        "\"actor\":\"a\"",
        "\"actor\":\"z\"",
        1,
    );
    fs::write(&tampered, changed).expect("write");
    let (code, v) = run_in(&outside, &["verify", tampered.to_str().expect("utf-8")]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(codes(&v), ["log.broken"], "{v}");

    // `export` needs `--work`.
    let (code, v) = run_in(&repo, &["export"]);
    assert_eq!(code, 1, "{v}");
}

/// A store restored from an older copy no longer holds the anchored
/// record: verify and anchor say so, and `anchor --force` re-anchors.
#[test]
fn a_restored_store_contradicts_its_anchor() {
    let dir = scratch();
    let repo = repo(&dir);
    let (code, out) = run_in(&repo, &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let db = repo.join(".git/mdkg/coord.sqlite");
    let copy = mdkg_coord_sqlite::snapshot(&db, "test").expect("snapshot");
    let (code, out) = run_in(&repo, &["release", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_in(&repo, &["anchor"]);
    assert_eq!(code, 0, "{out}");

    let (code, out) = run_in(&repo, &["store", "restore", copy.to_str().expect("utf-8")]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_in(&repo, &["verify"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(codes(&out), ["log.anchor_mismatch"], "{out}");
    let (code, out) = run_in(&repo, &["anchor"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(codes(&out), ["log.anchor_mismatch"], "{out}");
    let (code, out) = run_in(&repo, &["anchor", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(codes(&out), ["log.anchor_mismatch"], "a warning: {out}");
    assert!(out["data"]["previous"].is_object(), "{out}");
    let (code, out) = run_in(&repo, &["verify"]);
    assert_eq!((code, codes(&out)), (0, vec![]), "{out}");
}

/// `goal done` anchors the chain and says so (edd-6 W2).
#[test]
fn a_goal_done_anchors_the_chain() {
    let dir = scratch();
    let repo = repo(&dir);
    let (code, out) = run_in(&repo, &["new", "goal", "Ship", "it"]);
    assert_eq!(code, 0, "{out}");
    let goal = out["data"]["node"]["id"].as_str().expect("id").to_string();
    // Recorded on the branch, as `done` records nodes.
    let (code, out) = run_in(&repo, &["claim", &goal]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_in(&repo, &["goal", "done", &goal]);
    assert_eq!(code, 0, "{out}");
    let notice = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "log.anchored")
        .unwrap_or_else(|| panic!("no anchor notice: {out}"));
    assert_eq!(notice["severity"], "info", "{out}");
    let (code, out) = run_in(&repo, &["verify"]);
    assert_eq!((code, codes(&out)), (0, vec![]), "anchored: {out}");
}

#[test]
fn anchor_text_counts_actors_and_json_keeps_the_complete_list() {
    let dir = scratch();
    let repo = repo(&dir);
    // Seed a genuine journal without hundreds of CLI subprocesses.
    let mut actors: Vec<String> = (0..160)
        .map(|i| {
            mdkg_run::mail::CliMail::new(
                env!("CARGO_BIN_EXE_mdkg").into(),
                repo.clone(),
                if i % 2 == 0 {
                    "launcher"
                } else {
                    "orchestrator"
                }
                .into(),
            )
            .holder
        })
        .collect();
    actors.extend(
        [
            "claude-worker",
            "codex-worker",
            "opencode-worker",
            "launcher@other",
            "orchestrator@run-not-a-uuid",
            "mdkg",
        ]
        .map(String::from),
    );
    actors.sort();
    let seed = || {
        let mut store =
            mdkg_coord_sqlite::Store::open(&repo.join(".git/mdkg/coord.sqlite"), 7_200_000)
                .expect("fixture store");
        for (i, actor) in actors.iter().enumerate() {
            let outcome = store
                .execute(&mdkg_coord::Command::Claim {
                    owner: actor.clone(),
                    scope: mdkg_coord::Scope::new(format!("fixture/actor-{i}")),
                    mode: mdkg_coord::lease::Mode::Exclusive,
                    now: 1,
                    request: None,
                    node: None,
                })
                .unwrap();
            let mdkg_coord::Outcome::Lease { lease } = outcome else {
                panic!("{outcome:?}")
            };
            assert_eq!(
                store
                    .execute(&mdkg_coord::Command::Release {
                        owner: actor.clone(),
                        lease,
                        now: 1,
                    })
                    .unwrap(),
                mdkg_coord::Outcome::Done
            );
        }
    };
    seed();
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&repo)
        .args(["--as", "named-worker", "anchor"])
        .output()
        .expect("anchor text");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8(out.stdout).expect("text");
    assert!(
        text.lines().next().unwrap().starts_with("anchored store "),
        "{text}"
    );
    assert!(
        text.lines().next().unwrap().ends_with("; commit it"),
        "{text}"
    );
    let store = mdkg_coord_sqlite::Store::open(&repo.join(".git/mdkg/coord.sqlite"), 7_200_000)
        .expect("fixture store");
    let (checkpoint, _) = store.log_head().expect("checkpoint head");
    assert_eq!(
        text.lines().nth(1).expect("checkpoint line"),
        format!(
            "checkpoint {checkpoint}: records 1 to {}; actors: launcher 160, named 5, system 1 (full list: --json)",
            checkpoint - 1
        )
    );
    drop(store);
    assert!(!text.contains("launcher@run-"), "{text}");
    assert!(text.len() < 700, "{text}");

    // A new range with more actors than fit in the stored prose summary.
    seed();
    let (code, out) = run_in(&repo, &["anchor"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["checkpoint"]["actors"],
        serde_json::json!(actors)
    );
    let (code, out) = run_in(&repo, &["anchor"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out["data"].get("checkpoint").is_none(),
        "no fresh records: {out}"
    );
}
