//! `mdkg affected` end to end, on throwaway git repositories
//! for the checks that fail closed to a full run, and the
//! per-path report.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("MDKG_KEEP_SCRATCH").is_none() {
            fs::remove_dir_all(&self.0).ok();
        }
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-affected");
    Scratch(fs::canonicalize(dir).expect("canonical"))
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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    fs::write(file, text).expect("write");
}

/// A repository with a config and one commit; returns it and that commit.
fn repo() -> (Scratch, String) {
    let s = scratch();
    git(&s.0, &["init", "-q", "-b", "main"]);
    write(
        &s.0,
        ".mdkg/config.json",
        "{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\"}\n",
    );
    write(
        &s.0,
        ".mdkg/work/task-fixture.md",
        "---\nid: task-01900000-0000-7000-8000-000000000002\ntype: task\ntitle: Fixture task\nstatus: todo\npriority: 2\ncreated: 2024-06-09T00:00:00.000Z\nupdated: 2024-06-09T00:00:00.000Z\n---\nFixture body.\n",
    );
    write(&s.0, "README.md", "hello\n");
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-q", "-m", "base"]);
    let base = git(&s.0, &["rev-parse", "HEAD"]);
    (s, base)
}

fn commit(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Runs `affected --json`; checks the envelope against the schema.
fn affected(dir: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(dir))
        .arg("affected")
        .args(args)
        .arg("--json")
        .output()
        .expect("run mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms("affected", &v);
    (out.status.code().unwrap_or(-1), v)
}

fn codes(v: &Value) -> Vec<String> {
    v["data"]["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|r| r["code"].as_str().expect("code").to_string())
        .collect()
}

#[test]
fn no_change_is_selective_with_nothing_to_run() {
    let (s, base) = repo();
    let (exit, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(exit, 0);
    assert_eq!(v["data"]["mode"], "selective");
    assert!(codes(&v).is_empty());
    assert_eq!(v["data"]["paths"], serde_json::json!([]));
    assert_eq!(v["data"]["base"]["sha"], base.as_str());
    assert_eq!(v["data"]["head"]["sha"], base.as_str());
}

#[test]
fn a_path_nothing_maps_forces_a_full_run() {
    let (s, base) = repo();
    write(&s.0, "z.txt", "z\n");
    write(&s.0, "a b.txt", "a\n");
    commit(&s.0, "two files");
    let (exit, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(exit, 0, "a full run is a decision, not a failure");
    assert_eq!(v["data"]["mode"], "full");
    let paths: Vec<&str> = v["data"]["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .map(|p| p["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["a b.txt", "z.txt"], "sorted, spaces kept");
    assert_eq!(codes(&v), ["unmatched_path", "unmatched_path"]);
    assert_eq!(v["data"]["selected"]["go"], serde_json::json!([]));
}

#[test]
fn a_rename_reports_both_paths() {
    let (s, base) = repo();
    fs::create_dir_all(s.0.join("docs")).expect("mkdir");
    git(&s.0, &["mv", "README.md", "docs/README.md"]);
    commit(&s.0, "move");
    let (_, v) = affected(&s.0, &["--base", &base]);
    let p = &v["data"]["paths"][0];
    assert_eq!(p["status"], "R");
    assert_eq!(p["path"], "docs/README.md");
    assert_eq!(p["old_path"], "README.md");
}

#[test]
fn a_missing_base_is_full() {
    let (s, _) = repo();
    let (exit, v) = affected(
        &s.0,
        &["--base", "0123456789012345678901234567890123456789"],
    );
    assert_eq!(exit, 0);
    assert_eq!(v["data"]["mode"], "full");
    assert_eq!(codes(&v), ["base_missing"]);
    assert!(v["data"]["base"].get("sha").is_none());
}

#[test]
fn a_revision_that_looks_like_an_option_is_only_a_revision() {
    let (s, _) = repo();
    let (_, v) = affected(&s.0, &["--base=--output=leaked.txt"]);
    assert_eq!(codes(&v), ["base_missing"]);
    assert!(!s.0.join("leaked.txt").exists());
}

#[test]
fn a_base_that_isnt_an_ancestor_is_full() {
    let (s, base) = repo();
    git(&s.0, &["checkout", "-q", "-b", "side"]);
    write(&s.0, "side.txt", "s\n");
    let side = commit(&s.0, "side");
    git(&s.0, &["checkout", "-q", "main"]);
    write(&s.0, "main.txt", "m\n");
    commit(&s.0, "main");
    let (_, v) = affected(&s.0, &["--base", &side]);
    assert!(codes(&v).contains(&"base_not_ancestor".to_string()), "{v}");
    assert_eq!(v["data"]["mode"], "full");
    let _ = base;
}

#[test]
fn the_same_inputs_give_the_same_bytes() {
    let (s, base) = repo();
    for f in ["c.txt", "a.txt", "b/x.txt"] {
        write(&s.0, f, "x\n");
    }
    commit(&s.0, "three");
    let run = || {
        isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
            .args(["affected", "--base", &base, "--json"])
            .output()
            .expect("run")
            .stdout
    };
    assert_eq!(run(), run());
}

#[test]
fn text_output_names_the_mode_and_reasons() {
    let (s, base) = repo();
    write(&s.0, "x.txt", "x\n");
    commit(&s.0, "x");
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .args(["affected", "--base", &base])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("mode: full\n"), "{text}");
    assert!(text.contains("reason: unmatched_path x.txt:"), "{text}");
}

#[test]
fn a_deletion_is_reported() {
    let (s, base) = repo();
    git(&s.0, &["rm", "-q", "README.md"]);
    commit(&s.0, "delete");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["paths"][0]["status"], "D");
    assert_eq!(v["data"]["paths"][0]["path"], "README.md");
    assert_eq!(v["data"]["mode"], "full", "nothing maps README.md");
}

#[test]
fn a_missing_head_is_full() {
    let (s, base) = repo();
    let (exit, v) = affected(&s.0, &["--base", &base, "--head", "no-such-branch"]);
    assert_eq!(exit, 0);
    assert_eq!(codes(&v), ["head_missing"]);
    assert!(v["data"]["head"].get("sha").is_none());
}

#[test]
fn a_shallow_clone_without_the_history_says_to_fetch_more() {
    let (s, base) = repo();
    for i in 0..3 {
        write(&s.0, &format!("f{i}.txt"), "x\n");
        commit(&s.0, &format!("c{i}"));
    }
    let clone = scratch();
    let url = format!("file://{}", s.0.display());
    git(&clone.0, &["clone", "-q", "--depth", "1", &url, "c"]);
    let c = clone.0.join("c");
    git(&c, &["fetch", "-q", "--depth", "1", "origin", &base]);
    let (_, v) = affected(&c, &["--base", &base]);
    assert_eq!(codes(&v)[0], "base_not_ancestor", "{v}");
    assert_eq!(v["data"]["mode"], "full");
    let detail = v["data"]["reasons"][0]["detail"]
        .as_str()
        .unwrap_or_default();
    assert!(detail.contains("shallow clone"), "{detail}");
}

#[test]
fn a_path_that_isnt_utf8_is_full() {
    use std::os::unix::ffi::OsStrExt;
    let (s, base) = repo();
    let blob = git(&s.0, &["hash-object", "-w", "README.md"]);
    let mut info = format!("100644,{blob},").into_bytes();
    info.extend_from_slice(b"bad\xff.txt");
    let out = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&s.0)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(std::ffi::OsStr::from_bytes(&info)),
    )
    .output()
    .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(&s.0, &["commit", "-q", "-m", "non-utf8"]);
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert!(codes(&v).contains(&"unreadable_path".to_string()), "{v}");
    assert_eq!(v["data"]["mode"], "full");
}

#[test]
fn outside_a_repository_is_an_error() {
    let s = scratch();
    write(
        &s.0,
        ".mdkg/config.json",
        "{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\"}\n",
    );
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .env("GIT_CEILING_DIRECTORIES", s.0.parent().expect("parent"))
        .args(["affected", "--base", "HEAD", "--json"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(4));
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    common::conforms("affected", &v);
    assert_eq!(v["ok"], false);
    assert_eq!(v["diagnostics"][0]["code"], "io.git_failed");
}

#[test]
fn replace_refs_dont_hide_a_change() {
    let (s, base) = repo();
    write(&s.0, "main.go", "package main\n");
    let head = commit(&s.0, "add main.go");
    // A replacement for head with base's tree: with replace refs honoured,
    // head looks like it changes nothing.
    let tree = git(&s.0, &["rev-parse", &format!("{base}^{{tree}}")]);
    let fake = git(&s.0, &["commit-tree", &tree, "-p", &base, "-m", "fake"]);
    git(&s.0, &["replace", &head, &fake]);
    git(&s.0, &["reset", "-q", "--hard"]);
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    let paths: Vec<&str> = v["data"]["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .map(|p| p["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["main.go"], "the real head's change is seen");
}

#[test]
fn grafts_and_repository_replace_settings_dont_hide_a_change() {
    for how in ["graft file", "graft env", "useReplaceRefs"] {
        let (s, base) = repo();
        write(&s.0, "main.go", "package main\n");
        let head = commit(&s.0, "add main.go");
        // A commit with head's tree: grafted in as head's parent, or as the
        // parent of head's replacement, it makes `HEAD~1` name a commit
        // with no change.
        let tree = git(&s.0, &["rev-parse", "HEAD^{tree}"]);
        let fake = git(&s.0, &["commit-tree", &tree, "-p", &base, "-m", "fake"]);
        let graft = format!("{head} {fake}\n");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        isolated(cmd.current_dir(&s.0));
        match how {
            "graft file" => fs::write(s.0.join(".git/info/grafts"), &graft).expect("graft"),
            "graft env" => {
                let file = s.0.join("grafts");
                fs::write(&file, &graft).expect("graft");
                cmd.env("GIT_GRAFT_FILE", &file);
            }
            _ => {
                let same = git(&s.0, &["commit-tree", &tree, "-p", &fake, "-m", "same"]);
                git(&s.0, &["replace", &head, &same]);
                git(&s.0, &["config", "core.useReplaceRefs", "true"]);
            }
        }
        let out = cmd
            .args(["affected", "--base", "HEAD~1", "--json"])
            .output()
            .expect("run mdkg");
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(v["data"]["base"]["sha"], base.as_str(), "{how}: {v}");
        let paths: Vec<&str> = v["data"]["paths"]
            .as_array()
            .expect("paths")
            .iter()
            .map(|p| p["path"].as_str().expect("path"))
            .collect();
        assert_eq!(paths, ["main.go"], "{how}");
    }
}

/// The decision's data, for comparing two runs.
fn decision(dir: &Path, base: &str) -> Value {
    let (exit, v) = affected(dir, &["--base", base]);
    assert_eq!(exit, 0, "{v}");
    v["data"].clone()
}

#[test]
fn the_checkout_doesnt_change_the_answer() {
    type Setup = fn(&Path);
    let setups: [(&str, Setup); 6] = [
        ("tracked edit", |d| write(d, "README.md", "edited\n")),
        ("untracked file", |d| write(d, "new.go", "package main\n")),
        ("staged edit", |d| {
            write(d, "README.md", "staged\n");
            git(d, &["add", "README.md"]);
        }),
        ("skip-worktree", |d| {
            git(d, &["update-index", "--skip-worktree", "README.md"]);
            write(d, "README.md", "hidden\n");
        }),
        ("sparse checkout", |d| {
            git(d, &["sparse-checkout", "set", "a", ".mdkg"]);
        }),
        ("an unrelated branch, and a dirty edit", |d| {
            git(d, &["checkout", "-q", "-b", "other"]);
            write(d, "other.txt", "o\n");
            git(d, &["add", "-A"]);
            git(d, &["commit", "-q", "-m", "other"]);
            git(d, &["checkout", "-q", "main"]);
            write(d, "README.md", "dirty on main\n");
        }),
    ];
    for (case, setup) in setups {
        let (s, base) = repo();
        write(&s.0, "a/x.txt", "x\n");
        write(&s.0, "b/y.txt", "y\n");
        commit(&s.0, "two folders");
        let clean = decision(&s.0, &base);
        setup(&s.0);
        assert_eq!(decision(&s.0, &base), clean, "{case}");
    }
}

#[test]
fn head_can_be_any_commit() {
    let (s, base) = repo();
    write(&s.0, "one.txt", "1\n");
    let one = commit(&s.0, "one");
    write(&s.0, "two.txt", "2\n");
    commit(&s.0, "two");
    let (_, v) = affected(&s.0, &["--base", &base, "--head", &one]);
    assert_eq!(v["data"]["head"]["sha"], one.as_str());
    let paths: Vec<&str> = v["data"]["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .map(|p| p["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["one.txt"], "only what changed up to --head");
}

#[test]
fn no_filter_runs_even_for_racily_clean_entries() {
    let (s, _) = repo();
    let marker = s.0.join("filter-ran");
    let script = format!("sh -c 'echo ran >> {}; cat'", marker.display());
    // Attributes from every source git reads: the tree, info/attributes and
    // core.attributesFile.
    write(&s.0, ".gitattributes", "*.txt filter=probe diff=probe\n");
    write(&s.0, "f.txt", "a\n");
    commit(&s.0, "attributes");
    // An inexact rename: rename detection is where git reads blob content.
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    write(&s.0, "g.txt", &long);
    commit(&s.0, "g");
    let base = git(&s.0, &["rev-parse", "HEAD"]);
    git(&s.0, &["mv", "g.txt", "h.txt"]);
    write(&s.0, "h.txt", &format!("{long}one more\n"));
    commit(&s.0, "rename g with an edit");
    write(&s.0, ".git/info/attributes", "* filter=probe\n");
    let extra = s.0.join("extra-attributes");
    fs::write(&extra, "* filter=probe\n").expect("write");
    git(
        &s.0,
        &[
            "config",
            "core.attributesFile",
            extra.to_str().expect("utf8"),
        ],
    );
    git(&s.0, &["config", "filter.probe.clean", &script]);
    git(&s.0, &["config", "filter.probe.smudge", &script]);
    git(&s.0, &["config", "filter.probe.process", "false"]);
    git(&s.0, &["config", "diff.probe.textconv", &script]);
    git(&s.0, &["config", "diff.probe.command", &script]);
    // Make the entries racily clean (the index no older than the files),
    // so git would compare their content through the filter.
    let out = isolated(
        Command::new("touch")
            .arg("-r")
            .arg(s.0.join("f.txt"))
            .arg(s.0.join(".git/index")),
    )
    .output()
    .expect("touch");
    assert!(out.status.success());
    let (exit, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(exit, 0, "{v}");
    assert_eq!(v["data"]["paths"][0]["status"], "R", "{v}");
    assert!(!marker.exists(), "a filter ran");
}

#[test]
fn a_partial_clone_is_full_without_reading_anything() {
    let (s, base) = repo();
    write(&s.0, "x.txt", "x\n");
    commit(&s.0, "x");
    git(&s.0, &["config", "uploadpack.allowFilter", "true"]);
    let clone = scratch();
    let url = format!("file://{}", s.0.display());
    git(&clone.0, &["clone", "-q", "--filter=blob:none", &url, "c"]);
    let c = clone.0.join("c");
    let (exit, v) = affected(&c, &["--base", &base]);
    assert_eq!(exit, 0);
    assert_eq!(codes(&v), ["partial_clone"], "{v}");
    // A GIT_CONFIG of the caller's can't hide the clone's own settings.
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&c))
        .env("GIT_CONFIG", "/dev/null")
        .args(["affected", "--base", &base, "--json"])
        .output()
        .expect("run mdkg");
    let hidden: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(codes(&hidden), ["partial_clone"], "{hidden}");
    assert_eq!(v["data"]["mode"], "full");
    assert!(
        v["data"]["base"].get("sha").is_none(),
        "nothing was resolved"
    );
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn selected_go(v: &Value) -> Vec<String> {
    v["data"]["selected"]["go"]
        .as_array()
        .expect("go")
        .iter()
        .flat_map(|m| m["packages"].as_array().expect("packages").clone())
        .map(|p| p.as_str().expect("package").to_string())
        .collect()
}

/// A Go module: a <- b <- c by imports; d's tests import b; e stands alone.
/// The config (committed with the base) carries `rules`.
fn go_repo(rules: &str) -> (Scratch, String) {
    let s = scratch();
    git(&s.0, &["init", "-q", "-b", "main"]);
    write(
        &s.0,
        ".mdkg/config.json",
        &format!(
            "{{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\", \"affected\": {{\"rules\": [{rules}]}}}}\n"
        ),
    );
    write(&s.0, "go.mod", "module example.com/fx\n\ngo 1.21\n");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 1 }\n");
    write(&s.0, "a/schema.sql", "create table t (x int);\n");
    write(&s.0, "a/testdata/in.json", "{}\n");
    write(
        &s.0,
        "b/b.go",
        "package b\n\nimport \"example.com/fx/a\"\n\nfunc B() int { return a.A() }\n",
    );
    write(
        &s.0,
        "c/c.go",
        "package c\n\nimport \"example.com/fx/b\"\n\nfunc C() int { return b.B() }\n",
    );
    write(&s.0, "d/d.go", "package d\n\nfunc D() int { return 4 }\n");
    write(
        &s.0,
        "d/d_test.go",
        "package d\n\nimport (\n\t\"testing\"\n\n\t\"example.com/fx/b\"\n)\n\nfunc TestD(t *testing.T) { _ = b.B() }\n",
    );
    write(&s.0, "e/e.go", "package e\n\nfunc E() int { return 5 }\n");
    write(&s.0, "README.md", "fixture\n");
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-q", "-m", "base"]);
    let base = git(&s.0, &["rev-parse", "HEAD"]);
    (s, base)
}

fn fx(pkgs: &[&str]) -> Vec<String> {
    pkgs.iter().map(|p| format!("example.com/fx/{p}")).collect()
}

#[test]
fn go_a_package_selects_itself_its_importers_and_their_test_importers() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    for (path, text, want) in [
        (
            "a/a.go",
            "package a\n\nfunc A() int { return 2 }\n",
            vec!["a", "b", "c", "d"],
        ),
        (
            "d/d_test.go",
            "package d\n\nimport \"testing\"\n\nfunc TestD(t *testing.T) {}\n",
            vec!["d"],
        ),
        (
            "e/e.go",
            "package e\n\nfunc E() int { return 6 }\n",
            vec!["e"],
        ),
        (
            "a/testdata/in.json",
            "{\"x\": 1}\n",
            vec!["a", "b", "c", "d"],
        ),
        (
            "a/schema.sql",
            "create table t (y int);\n",
            vec!["a", "b", "c", "d"],
        ),
        ("f/f.go", "package f\n\nfunc F() {}\n", vec!["f"]),
    ] {
        let (s, base) = go_repo("");
        write(&s.0, path, text);
        commit(&s.0, path);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], "selective", "{path}: {v}");
        assert_eq!(selected_go(&v), fx(&want), "{path}");
        assert_eq!(v["data"]["selected"]["go"][0]["module"], "example.com/fx");
        assert_eq!(v["data"]["selected"]["go"][0]["dir"], ".");
        assert_eq!(
            v["data"]["paths"][0]["matched_by"],
            serde_json::json!(["go"]),
            "{path}"
        );
    }
}

#[test]
fn go_a_rename_selects_both_sides() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    git(&s.0, &["mv", "e/e.go", "d/e.go"]);
    write(&s.0, "d/e.go", "package d\n\nfunc E() int { return 5 }\n");
    write(&s.0, "e/keep.go", "package e\n");
    commit(&s.0, "move");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(selected_go(&v), fx(&["d", "e"]));
}

#[test]
fn go_a_removed_package_is_full() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    git(&s.0, &["rm", "-q", "e/e.go"]);
    commit(&s.0, "remove e");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(codes(&v), ["package_removed"], "{v}");
    assert_eq!(v["data"]["mode"], "full");
}

#[test]
fn go_a_file_no_package_builds_is_full() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "g/g.go", "//go:build never\n\npackage g\n");
    commit(&s.0, "excluded");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(codes(&v), ["unmatched_path"], "{v}");
}

#[test]
fn go_broken_code_selects_the_package_that_wont_compile() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    // go list reads only the package clause and imports, so a syntax error
    // later in the file still maps to its package, whose tests then fail to
    // build: the breakage shows.
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A( {\n");
    commit(&s.0, "broken");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(selected_go(&v), fx(&["a", "b", "c", "d"]));
}

#[test]
fn go_an_import_that_cant_resolve_fails_closed() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(
        &s.0,
        "e/e.go",
        "package e\n\nimport \"example.com/fx/nowhere\"\n\nfunc E() { nowhere.X() }\n",
    );
    commit(&s.0, "bad import");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(codes(&v), ["adapter_failed"], "{v}");
    assert_eq!(v["data"]["mode"], "full");
}

#[test]
fn go_globals_and_rules() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let rules = r#"{"paths": ["migrations/**"], "select": {"go": ["./b"], "suites": ["db"]}},
                   {"paths": ["**/*.md"], "select": "none"},
                   {"paths": ["proto/**"], "select": "all"},
                   {"paths": ["stale/**"], "select": {"go": ["./nothing/..."]}}"#;
    for (path, want_mode, want_codes, want_go) in [
        ("go.mod", "full", vec!["global_file"], vec![]),
        ("migrations/001.sql", "selective", vec![], vec!["b"]),
        ("docs/guide.md", "selective", vec![], vec![]),
        ("proto/x.proto", "full", vec!["rule_selects_all"], vec![]),
        ("stale/x.txt", "full", vec!["config_invalid"], vec![]),
        ("scripts/x.sh", "full", vec!["unmatched_path"], vec![]),
    ] {
        let (s, base) = go_repo(rules);
        let text = if path == "go.mod" {
            "module example.com/fx\n\ngo 1.22\n"
        } else {
            "x\n"
        };
        write(&s.0, path, text);
        commit(&s.0, path);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], want_mode, "{path}: {v}");
        assert_eq!(codes(&v), want_codes, "{path}");
        assert_eq!(selected_go(&v), fx(&want_go), "{path}");
        if path == "migrations/001.sql" {
            assert_eq!(v["data"]["selected"]["suites"], serde_json::json!(["db"]));
        }
    }
}

#[test]
fn a_config_change_is_global_so_a_pr_cant_exempt_itself() {
    let (s, base) = repo();
    write(&s.0, "scripts/x.sh", "x\n");
    write(
        &s.0,
        ".mdkg/config.json",
        "{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\", \"affected\": {\"rules\": [{\"paths\": [\"**\"], \"select\": \"none\"}]}}\n",
    );
    commit(&s.0, "exempt everything");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(codes(&v).contains(&"global_file".to_string()));
}

#[test]
fn an_invalid_config_section_is_full() {
    let (s, base) = repo();
    write(
        &s.0,
        ".mdkg/config.json",
        "{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\", \"affected\": {\"rulez\": []}}\n",
    );
    let base2 = commit(&s.0, "bad config");
    let (_, v) = affected(&s.0, &["--base", &base2]);
    assert_eq!(codes(&v), ["config_invalid"], "{v}");
    let _ = base;
}

/// A Cargo workspace: cli depends on core, tests dev-depends on cli.
fn rust_repo() -> (Scratch, String) {
    let s = scratch();
    git(&s.0, &["init", "-q", "-b", "main"]);
    write(
        &s.0,
        ".mdkg/config.json",
        "{\"tool\": \"mdkg\", \"schema_version\": 3, \"format_version\": \"0.7\", \"graph_id\": \"01900000-0000-7000-8000-000000000001\"}\n",
    );
    write(
        &s.0,
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &s.0,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    );
    write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 1 }\n");
    write(
        &s.0,
        "crates/cli/Cargo.toml",
        "[package]\nname = \"cli-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\ncore-fx = { path = \"../core\" }\n",
    );
    write(
        &s.0,
        "crates/cli/src/lib.rs",
        "pub fn g() -> u8 { core_fx::f() }\n",
    );
    write(
        &s.0,
        "crates/checks/Cargo.toml",
        "[package]\nname = \"checks-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dev-dependencies]\ncli-fx = { path = \"../cli\" }\n",
    );
    write(&s.0, "crates/checks/src/lib.rs", "\n");
    write(
        &s.0,
        "crates/lone/Cargo.toml",
        "[package]\nname = \"lone-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    );
    write(&s.0, "crates/lone/src/lib.rs", "\n");
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-q", "-m", "base"]);
    let base = git(&s.0, &["rev-parse", "HEAD"]);
    (s, base)
}

#[test]
fn rust_a_crate_selects_itself_and_its_dependents() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    for (path, want_mode, want) in [
        (
            "crates/core/src/lib.rs",
            "selective",
            vec!["checks-fx", "cli-fx", "core-fx"],
        ),
        (
            "crates/cli/tests/it.rs",
            "selective",
            vec!["checks-fx", "cli-fx"],
        ),
        ("crates/checks/src/lib.rs", "selective", vec!["checks-fx"]),
        // Global: features unify across the workspace.
        ("crates/lone/Cargo.toml", "full", vec![]),
        ("Cargo.toml", "full", vec![]),
        ("Cargo.lock", "full", vec![]),
        ("tools/x.sh", "full", vec![]),
    ] {
        let (s, base) = rust_repo();
        let text = match path {
            "Cargo.toml" => "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n# changed\n",
            "crates/lone/Cargo.toml" => {
                "[package]\nname = \"lone-fx\"\nversion = \"0.0.1\"\nedition = \"2021\"\n"
            }
            _ => "// changed\n",
        };
        write(&s.0, path, text);
        commit(&s.0, path);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], want_mode, "{path}: {v}");
        let rust: Vec<String> = v["data"]["selected"]["rust"]
            .as_array()
            .expect("rust")
            .iter()
            .map(|c| c.as_str().expect("crate").to_string())
            .collect();
        assert_eq!(rust, want, "{path}");
    }
}

#[test]
fn go_the_graph_comes_from_the_head_not_the_checkout() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(selected_go(&v), fx(&["a", "b", "c", "d"]), "{v}");
    let clean = decision(&s.0, &base);
    // Uncommitted: e starts importing a, and c's code is gone.
    write(
        &s.0,
        "e/e.go",
        "package e\n\nimport \"example.com/fx/a\"\n\nfunc E() int { return a.A() }\n",
    );
    fs::remove_file(s.0.join("c/c.go")).expect("remove");
    write(&s.0, "go.mod", "not a go.mod\n");
    assert_eq!(decision(&s.0, &base), clean);
}

#[test]
fn the_written_tree_is_removed_afterwards() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let tmp = scratch();
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .env("TMPDIR", &tmp.0)
        .args(["affected", "--base", &base, "--json"])
        .output()
        .expect("run mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    let left: Vec<_> = fs::read_dir(&tmp.0)
        .expect("read tmp")
        .map(|e| e.expect("entry").file_name())
        .filter(|n| n.to_string_lossy().starts_with("mdkg-affected-"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// Adds `path` with `text` to the index directly, so a case-insensitive
/// disk can still commit paths that differ only in case.
fn stage(dir: &Path, path: &str, text: &str) {
    let tmp = dir.join(".stage-tmp");
    fs::write(&tmp, text).expect("write");
    let oid = git(dir, &["hash-object", "-w", ".stage-tmp"]);
    fs::remove_file(&tmp).expect("remove");
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{oid},{path}"),
        ],
    );
}

#[test]
fn a_head_tree_that_cant_be_written_faithfully_is_full() {
    let (s, base) = go_repo("");
    stage(&s.0, "e/E.go", "package e\n");
    stage(&s.0, "e/e.GO", "package e\n");
    git(&s.0, &["commit", "-q", "-m", "case twins"]);
    let (code, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(codes(&v).contains(&"adapter_failed".to_string()), "{v}");
}

#[test]
fn a_symlink_in_the_head_is_full_and_nothing_is_written_through_it() {
    let (s, base) = go_repo("");
    let outside = scratch();
    let target = outside.0.to_str().expect("utf8").to_string();
    // `A` points outside; `a/pwned.txt` would land there through it on a
    // case-insensitive disk.
    let oid = {
        let tmp = s.0.join(".link-tmp");
        fs::write(&tmp, &target).expect("write");
        let oid = git(&s.0, &["hash-object", "-w", ".link-tmp"]);
        fs::remove_file(&tmp).expect("remove");
        oid
    };
    git(
        &s.0,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{oid},A"),
        ],
    );
    stage(&s.0, "a/pwned.txt", "x\n");
    git(&s.0, &["commit", "-q", "-m", "link"]);
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(codes(&v).contains(&"adapter_failed".to_string()), "{v}");
    let leaked: Vec<_> = fs::read_dir(&outside.0).expect("read").collect();
    assert!(leaked.is_empty(), "wrote outside the temporary folder");
}

#[test]
fn a_root_crate_claims_only_its_target_folders() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    for (path, want_mode, want) in [
        ("src/lib.rs", "selective", vec!["app-fx"]),
        ("fixtures/data.json", "full", vec![]),
        ("scripts/ci.sh", "full", vec![]),
    ] {
        let (s, _) = rust_repo();
        write(
            &s.0,
            "Cargo.toml",
            "[package]\nname = \"app-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
        );
        write(&s.0, "src/lib.rs", "\n");
        write(&s.0, "fixtures/data.json", "{}\n");
        let base = commit(&s.0, "root crate");
        write(&s.0, path, "// changed\n");
        commit(&s.0, path);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], want_mode, "{path}: {v}");
        assert_eq!(
            v["data"]["selected"]["rust"],
            serde_json::json!(want),
            "{path}"
        );
    }
}

#[test]
fn go_imports_in_excluded_files_count() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, _) = go_repo("");
    // e's tests, under a tag no default build sets, import a; another OS's
    // file in d imports c.
    write(
        &s.0,
        "e/e_integration_test.go",
        "//go:build integration\n\npackage e\n\nimport (\n\t\"testing\"\n\n\t\"example.com/fx/a\" // the tagged test's import\n)\n\nfunc TestE(t *testing.T) { _ = a.A() }\n",
    );
    write(
        &s.0,
        "d/d_plan9.go",
        "package d\n\nimport /* c */ \"example.com/fx/c\"\n\nvar _ = c.C\n",
    );
    let base = commit(&s.0, "excluded files");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(selected_go(&v), fx(&["a", "b", "c", "d", "e"]), "{v}");
    // A change to an excluded file selects its package.
    write(
        &s.0,
        "e/e_integration_test.go",
        "//go:build integration\n\npackage e\n",
    );
    let head = commit(&s.0, "edit the tagged test");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(selected_go(&v), fx(&["e"]), "{v}");
}

#[test]
fn go_the_modules_top_folder_files_arent_its_packages() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, _) = go_repo("");
    write(&s.0, "main.go", "package main\n\nfunc main() {}\n");
    write(&s.0, "Makefile", "all:\n");
    let base = commit(&s.0, "root package");
    write(&s.0, "Makefile", "all:\n\techo\n");
    commit(&s.0, "Makefile");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(codes(&v), ["unmatched_path"], "{v}");
}

#[test]
fn a_full_answer_with_a_tool_error_is_the_same_every_run() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "x/x.go", "package x\n");
    write(&s.0, "x/y.go", "package y\n");
    commit(&s.0, "two packages in one folder");
    let (_, one) = affected(&s.0, &["--base", &base]);
    let (_, two) = affected(&s.0, &["--base", &base]);
    assert_eq!(one["data"]["mode"], "full", "{one}");
    assert_eq!(one, two);
    assert!(!one.to_string().contains("mdkg-affected-"), "{one}");
}

#[test]
fn go_a_folder_go_list_doesnt_report_makes_its_dependencies_full() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, _) = go_repo("");
    // Every file of e2e is excluded by default; its tests import a.
    write(
        &s.0,
        "e2e/e2e_test.go",
        "//go:build integration\n\npackage e2e\n\nimport (\n\t\"testing\"\n\n\t\"example.com/fx/a\"\n)\n\nfunc TestE2E(t *testing.T) { _ = a.A() }\n",
    );
    // A generator nothing builds: its imports don't count, and it parses
    // as nothing.
    write(
        &s.0,
        "gen/gen.go",
        "//go:build ignore\n\npackage {{.Name}}\n\nimport \"example.com/fx/e\"\n",
    );
    let base = commit(&s.0, "tagged package");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert_eq!(codes(&v), ["unmatched_path"], "{v}");
    // e is imported only by the ignored generator: still selective.
    write(&s.0, "e/e.go", "package e\n\nfunc E() int { return 7 }\n");
    let head = commit(&s.0, "change e");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(selected_go(&v), fx(&["e"]));
}

#[test]
fn go_the_callers_go_environment_doesnt_change_the_answer() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let clean = decision(&s.0, &base);
    for (k, v) in [
        ("GO111MODULE", "off"),
        ("GOFLAGS", "-tags=integration -mod=mod"),
        ("GOOS", "windows"),
        ("CGO_ENABLED", "0"),
    ] {
        let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
            .env(k, v)
            .args(["affected", "--base", &base, "--json"])
            .output()
            .expect("run mdkg");
        let got: Value = serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(got["data"], clean, "{k}={v}");
    }
}

#[test]
fn a_root_target_deep_in_a_folder_claims_only_its_own_folder() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    let (s, _) = rust_repo();
    write(
        &s.0,
        "Cargo.toml",
        "[package]\nname = \"app-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"tool\"\npath = \"tools/tool/main.rs\"\n\n[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(&s.0, "tools/tool/main.rs", "fn main() {}\n");
    write(&s.0, "tools/data/x.json", "{}\n");
    commit(&s.0, "deep bin");
    for (path, want_mode) in [
        ("tools/tool/main.rs", "selective"),
        ("tools/data/x.json", "full"),
    ] {
        write(&s.0, path, "// changed\n");
        let head = commit(&s.0, path);
        let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
        assert_eq!(v["data"]["mode"], want_mode, "{path}: {v}");
    }
}

#[test]
fn go_excluded_files_at_the_modules_top_count_too() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, _) = go_repo("");
    write(
        &s.0,
        "e2e_test.go",
        "//go:build integration\n\npackage fx_test\n\nimport (\n\t\"testing\"\n\n\t\"example.com/fx/a\"\n)\n\nfunc TestE2E(t *testing.T) { _ = a.A() }\n",
    );
    // A typo Go reads as a plain comment: the file builds on windows, so its
    // imports count.
    write(
        &s.0,
        "w/w_windows.go",
        "//go:buildignore\n\npackage w\n\nimport \"example.com/fx/e\"\n\nvar _ = e.E\n",
    );
    write(&s.0, "w/w.go", "package w\n");
    let base = commit(&s.0, "top-level tagged test");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    write(&s.0, "e/e.go", "package e\n\nfunc E() int { return 9 }\n");
    let head = commit(&s.0, "change e");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(selected_go(&v), fx(&["e", "w"]), "{v}");
}

#[test]
fn a_target_outside_its_crate_claims_its_folder() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    let (s, _) = rust_repo();
    write(
        &s.0,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[test]]\nname = \"shared\"\npath = \"../../conformance/run.rs\"\n",
    );
    write(&s.0, "conformance/run.rs", "mod common;\n");
    write(&s.0, "conformance/common/mod.rs", "\n");
    commit(&s.0, "shared test");
    write(&s.0, "conformance/common/mod.rs", "// changed\n");
    let head = commit(&s.0, "change the shared module");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(
        v["data"]["selected"]["rust"],
        serde_json::json!(["checks-fx", "cli-fx", "core-fx"])
    );
}

#[test]
fn an_outside_target_claims_only_rust_files_and_a_missing_one_is_full() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    let (s, _) = rust_repo();
    // core's test lives beside the crates; lone reads a fixture there.
    write(
        &s.0,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[test]]\nname = \"shared\"\npath = \"../shared_test.rs\"\n",
    );
    write(&s.0, "crates/shared_test.rs", "\n");
    write(&s.0, "crates/lone/fixtures/data.json", "{}\n");
    commit(&s.0, "outside test");
    write(&s.0, "crates/lone/fixtures/data.json", "{\"x\": 1}\n");
    let head = commit(&s.0, "change the fixture");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(v["data"]["mode"], "full", "a fixture isn't a .rs file: {v}");
    // The outside target's file goes missing: cargo still lists it.
    git(&s.0, &["rm", "-q", "crates/shared_test.rs"]);
    let head = commit(&s.0, "drop the shared test");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(codes(&v).contains(&"adapter_failed".to_string()), "{v}");
}

#[test]
fn go_writes_nothing_in_the_callers_home() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let home = scratch();
    let real_home = std::env::var_os("HOME").unwrap_or_default();
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .env("HOME", &home.0)
        // The module cache lives under the real HOME; pass it on.
        .env("GOMODCACHE", Path::new(&real_home).join("go/pkg/mod"))
        .args(["affected", "--base", &base, "--json"])
        .output()
        .expect("run mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    let written: Vec<_> = fs::read_dir(&home.0).expect("read").collect();
    assert!(written.is_empty(), "go wrote in HOME: {written:?}");
}

#[test]
fn cargo_redirections_make_the_run_full() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    for (path, text) in [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[patch.crates-io]\ncore-fx = { path = \"crates/core\" }\n",
        ),
        (".cargo/config.toml", "paths = [\"crates/core\"]\n"),
    ] {
        let (s, _) = rust_repo();
        write(&s.0, path, text);
        commit(&s.0, "redirect");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        let head = commit(&s.0, "change core");
        let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
        assert_eq!(v["data"]["mode"], "full", "{path}: {v}");
        assert!(
            codes(&v).contains(&"adapter_failed".to_string()),
            "{path}: {v}"
        );
    }
}

#[test]
fn a_data_file_an_outside_target_can_read_is_full() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    let (s, _) = rust_repo();
    write(
        &s.0,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[test]]\nname = \"shared\"\npath = \"../lone/tests/shared/run.rs\"\n",
    );
    write(
        &s.0,
        "crates/lone/tests/shared/run.rs",
        "const D: &str = include_str!(\"data.txt\");\n",
    );
    write(&s.0, "crates/lone/tests/shared/data.txt", "1\n");
    commit(&s.0, "shared test reads data");
    write(&s.0, "crates/lone/tests/shared/data.txt", "2\n");
    let head = commit(&s.0, "change the data");
    let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
}

#[test]
fn go_leaves_nothing_behind_in_the_temporary_folder() {
    if !have("go") {
        eprintln!("skipped: go isn't on PATH");
        return;
    }
    let (s, base) = go_repo("");
    write(&s.0, "a/a.go", "package a\n\nfunc A() int { return 2 }\n");
    commit(&s.0, "change a");
    let tmp = scratch();
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .env("TMPDIR", &tmp.0)
        .args(["affected", "--base", &base, "--json"])
        .output()
        .expect("run mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    // A process go left running (its telemetry uploader) would write here
    // after the run.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let left: Vec<_> = fs::read_dir(&tmp.0)
        .expect("read")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[test]
fn an_excluded_path_crate_and_quoted_patches_are_full() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    type Setup = fn(&Path);
    let cases: [(&str, Setup); 3] = [
        ("excluded path crate", |d| {
            write(
                d,
                "Cargo.toml",
                "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\nexclude = [\"tools/helper\"]\n",
            );
            write(
                d,
                "tools/helper/Cargo.toml",
                "[package]\nname = \"helper-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\ncore-fx = { path = \"../../crates/core\" }\n",
            );
            write(d, "tools/helper/src/lib.rs", "\n");
            write(
                d,
                "crates/lone/Cargo.toml",
                "[package]\nname = \"lone-fx\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhelper-fx = { path = \"../../tools/helper\" }\n",
            );
        }),
        ("quoted patch", |d| {
            write(
                d,
                "Cargo.toml",
                "\u{feff}[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[\"patch\".crates-io]\ncore-fx = { path = \"crates/core\" }\n",
            );
        }),
        ("toolchain by path", |d| {
            write(
                d,
                "rust-toolchain.toml",
                "[toolchain]\npath = \"/nonexistent/toolchain\"\n",
            );
        }),
    ];
    for (case, setup) in cases {
        let (s, _) = rust_repo();
        setup(&s.0);
        commit(&s.0, case);
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 3 }\n");
        let head = commit(&s.0, "change core");
        let (_, v) = affected(&s.0, &["--base", &format!("{head}^")]);
        assert_eq!(v["data"]["mode"], "full", "{case}: {v}");
        if case == "excluded path crate" {
            // The relative path passes preflight. Metadata itself must reject
            // this nonmember rather than silently lose its dependency edges.
            assert!(
                v["data"]["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reason| {
                        reason["detail"]
                            .as_str()
                            .unwrap()
                            .contains("a crate that isn't a workspace member")
                    }),
                "{v}"
            );
        }
    }
}

#[test]
fn v07_graph_document_rules_select_checks() {
    let (s, _) = repo();
    let path = s.0.join(".mdkg/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["affected"] = serde_json::json!({"version": 1, "rules": [
        {"paths": [".mdkg/work/**"], "select": {"suites": ["graph-checks"]}}
    ]});
    write(&s.0, ".mdkg/config.json", &config.to_string());
    let base = commit(&s.0, "graph checks rule");
    // Verify the fixture is actually readable in the current graph format.
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .args([
            "show",
            "task-01900000-0000-7000-8000-000000000002",
            "--json",
        ])
        .output()
        .expect("read 0.7 node");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let node = s.0.join(".mdkg/work/task-fixture.md");
    let text = fs::read_to_string(&node)
        .unwrap()
        .replace("Fixture body.", "Updated body.");
    fs::write(node, text).unwrap();
    commit(&s.0, "update graph document");
    let (exit, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(exit, 0, "{v}");
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(
        v["data"]["selected"]["suites"],
        serde_json::json!(["graph-checks"])
    );
    assert_eq!(v["data"]["paths"][0]["path"], ".mdkg/work/task-fixture.md");
}

#[test]
fn unmapped_v07_graph_document_is_full() {
    let (s, base) = repo();
    let path = s.0.join(".mdkg/work/task-fixture.md");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("Fixture body.", "Updated body.");
    fs::write(path, text).unwrap();
    commit(&s.0, "unmapped graph document");
    let (exit, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(exit, 0, "{v}");
    assert_eq!(v["data"]["mode"], "full");
    assert_eq!(codes(&v), ["unmatched_path"]);
    assert_eq!(
        v["data"]["selected"],
        serde_json::json!({"go": [], "rust": [], "suites": []})
    );
}

#[test]
fn checkout_config_never_changes_the_committed_decision() {
    let (s, base) = repo();
    let expected = affected(&s.0, &["--base", &base]);
    assert_eq!(expected.1["data"]["mode"], "selective");
    for text in ["not JSON", "[]", r#"{"schema_version":999}"#] {
        write(&s.0, ".mdkg/config.json", text);
        assert_eq!(affected(&s.0, &["--base", &base]), expected);
    }
    fs::remove_file(s.0.join(".mdkg/config.json")).unwrap();
    assert_eq!(affected(&s.0, &["--base", &base]), expected);
}

#[test]
fn invalid_committed_graph_configs_fail_closed() {
    let (s, _) = repo();
    for text in [
        "not JSON",
        "[]",
        "null",
        r#"{"schema_version":999}"#,
        r#"{"schema_version":2}"#,
        r#"{"schema_version":3,"format_version":"0.8","graph_id":"01900000-0000-7000-8000-000000000001"}"#,
        r#"{"schema_version":3,"format_version":"0.7"}"#,
    ] {
        write(&s.0, ".mdkg/config.json", text);
        let head = commit(&s.0, "invalid committed config");
        let (exit, v) = affected(&s.0, &["--base", &head]);
        assert_eq!(exit, 0, "{v}");
        assert_eq!(v["data"]["mode"], "full", "{v}");
        assert_eq!(codes(&v), ["config_invalid"], "{v}");
        assert_eq!(
            v["data"]["selected"],
            serde_json::json!({"go": [], "rust": [], "suites": []})
        );
    }
    fs::remove_file(s.0.join(".mdkg/config.json")).unwrap();
    let head = commit(&s.0, "missing committed config");
    let (exit, v) = affected(&s.0, &["--base", &head]);
    assert_eq!(exit, 0, "{v}");
    assert_eq!(codes(&v), ["config_invalid"]);
}

#[cfg(unix)]
fn assert_cargo_not_invoked(dir: &Path, base: &str) {
    assert_tool_not_invoked(dir, base, "cargo");
}

#[cfg(unix)]
fn assert_tool_not_invoked(dir: &Path, base: &str, tool: &str) -> Value {
    use std::os::unix::fs::PermissionsExt;
    let tools = scratch();
    let marker = tools.0.join("called");
    let tool_path = tools.0.join(tool);
    fs::write(
        &tool_path,
        format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
    )
    .expect("write tool sentinel");
    fs::set_permissions(&tool_path, fs::Permissions::from_mode(0o755))
        .expect("make tool sentinel executable");
    let path_env = std::env::join_paths(std::iter::once(tools.0.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("fixture PATH");
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(dir))
        .env("PATH", path_env)
        .args(["affected", "--base", base, "--json"])
        .output()
        .expect("run affected");
    let v: Value = serde_json::from_slice(&out.stdout).expect("affected JSON");
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(
        !marker.exists(),
        "{tool} ran before the redirection was refused"
    );
    v
}

#[cfg(unix)]
#[test]
fn review_escaped_toml_guards_run_before_cargo() {
    let cases = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n[\"pat\\u0063h\".crates-io]\ncore-fx = { path = \"crates/core\" }\n",
        ),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n[\"repl\\u0061ce\"]\n\"core-fx:0.0.0\" = { path = \"crates/core\" }\n",
        ),
        (
            "rust-toolchain.toml",
            "[toolchain]\n\"pa\\u0074h\" = \"/nonexistent/toolchain\"\n",
        ),
        (
            "rust-toolchain",
            "\"toolch\\u0061in\" = { \"pa\\u0074h\" = \"/nonexistent/toolchain\" }\n",
        ),
    ];
    for (path, text) in cases {
        let (s, _) = rust_repo();
        write(&s.0, path, text);
        let base = commit(&s.0, "escaped redirection");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        commit(&s.0, "change core");
        assert_cargo_not_invoked(&s.0, &base);
    }
}

#[test]
fn review_external_rust_dependency_is_full() {
    if !have("cargo") {
        return;
    }
    let (s, _) = rust_repo();
    let bridge = scratch();
    write(
        &bridge.0,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"bridge\"\nversion = \"0.0.0\"\n[dependencies]\ncore-fx = {{ path = {:?} }}\n",
            s.0.join("crates/core").to_str().unwrap()
        ),
    );
    write(&bridge.0, "src/lib.rs", "pub fn bridge() {}\n");
    write(
        &s.0,
        "crates/cli/Cargo.toml",
        &format!(
            "[package]\nname = \"cli-fx\"\nversion = \"0.0.0\"\n[dependencies]\nbridge = {{ path = {:?} }}\n",
            bridge.0.to_str().unwrap()
        ),
    );
    let base = commit(&s.0, "external bridge");
    write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
    commit(&s.0, "change core");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
    assert!(codes(&v).contains(&"adapter_failed".to_string()), "{v}");
}

#[test]
fn review_go_source_selects_its_embed_owners() {
    if !have("go") {
        return;
    }
    let (s, _) = go_repo("");
    write(
        &s.0,
        "p/p.go",
        "package p\nimport _ \"embed\"\n//go:embed child/value.go\nvar Value string\n",
    );
    write(&s.0, "p/child/value.go", "package child\nconst Value = 1\n");
    write(
        &s.0,
        "q/q.go",
        "package q\nimport \"example.com/fx/p\"\nvar Value = p.Value\n",
    );
    let base = commit(&s.0, "embedded Go source");
    write(&s.0, "p/child/value.go", "package child\nconst Value = 2\n");
    commit(&s.0, "change embedded Go source");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(selected_go(&v), fx(&["p", "p/child", "q"]), "{v}");
}

#[test]
fn review_deleted_embed_with_surviving_files_is_full() {
    if !have("go") {
        return;
    }
    let (s, _) = go_repo("");
    write(
        &s.0,
        "p/p.go",
        "package p\nimport \"embed\"\n//go:embed child/*.txt\nvar Values embed.FS\n",
    );
    write(&s.0, "p/child/child.go", "package child\n");
    write(&s.0, "p/child/first.txt", "first\n");
    write(&s.0, "p/child/second.txt", "second\n");
    let base = commit(&s.0, "embedded assets");
    fs::remove_file(s.0.join("p/child/first.txt")).unwrap();
    commit(&s.0, "delete one embedded asset");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
}

#[test]
fn review_build_excluded_embeds_are_full() {
    if !have("go") {
        return;
    }
    for listed in [true, false] {
        let (s, _) = go_repo(r#"{"paths":["**"],"select":"none"}"#);
        if listed {
            write(&s.0, "p/p.go", "package p\n");
        }
        write(
            &s.0,
            "p/p_windows.go",
            "package p\nimport _ \"embed\"\n//go:embed child/value.txt\nvar Value string\n",
        );
        write(&s.0, "p/child/child.go", "package child\n");
        write(&s.0, "p/child/value.txt", "first\n");
        let base = commit(&s.0, "platform embed");
        write(&s.0, "p/child/value.txt", "second\n");
        commit(&s.0, "change platform asset");
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], "full", "{v}");
    }
}

#[test]
fn review_duplicate_go_modules_are_full() {
    if !have("go") {
        return;
    }
    let (s, _) = repo();
    for module in ["a", "b"] {
        write(
            &s.0,
            &format!("{module}/go.mod"),
            "module example.com/duplicate\n\ngo 1.21\n",
        );
        write(
            &s.0,
            &format!("{module}/pkg/pkg.go"),
            "package pkg\nconst Value = 1\n",
        );
    }
    let base = commit(&s.0, "independent modules with the same import path");
    write(&s.0, "a/pkg/pkg.go", "package pkg\nconst Value = 2\n");
    commit(&s.0, "change first module");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "full", "{v}");
}

#[test]
fn review_changed_gitlinks_override_none_rules() {
    for case in ["add", "update", "delete", "rename", "to_blob", "from_blob"] {
        let (s, object) = repo();
        let mut config: Value =
            serde_json::from_slice(&fs::read(s.0.join(".mdkg/config.json")).unwrap()).unwrap();
        config["affected"] = serde_json::json!({"rules":[{"paths":["**"],"select":"none"}]});
        write(&s.0, ".mdkg/config.json", &config.to_string());
        let other_object = commit(&s.0, "none rule");
        if case == "from_blob" {
            write(&s.0, "plugin", "before\n");
            commit(&s.0, "ordinary file");
        } else if case != "add" {
            git(
                &s.0,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{object},plugin"),
                ],
            );
            git(&s.0, &["commit", "-qm", "base gitlink"]);
        }
        let base = git(&s.0, &["rev-parse", "HEAD"]);
        match case {
            "delete" => {
                git(&s.0, &["update-index", "--force-remove", "plugin"]);
            }
            "rename" => {
                git(&s.0, &["update-index", "--force-remove", "plugin"]);
                git(
                    &s.0,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("160000,{object},renamed"),
                    ],
                );
            }
            "to_blob" => {
                git(&s.0, &["update-index", "--force-remove", "plugin"]);
                write(&s.0, "plugin", "after\n");
                git(&s.0, &["add", "plugin"]);
            }
            _ => {
                git(
                    &s.0,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("160000,{other_object},plugin"),
                    ],
                );
            }
        }
        git(&s.0, &["commit", "-qm", "change gitlink"]);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], "full", "{case}: {v}");
    }
}

#[cfg(unix)]
#[test]
fn review_external_manifest_paths_never_invoke_cargo() {
    let cases = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/**/../../outside\"]\n",
        ),
        ("Cargo.toml", "[workspace]\nmembers = [\"../outside\"]\n"),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n[workspace.dependencies]\nbridge = { path = \"/outside\" }\n",
        ),
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n[["),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\n[dependencies]\nbridge = { path = \"../../../outside\" }\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\n[target.'cfg(unix)'.build-dependencies.bridge]\npath = \"../../../outside\"\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nworkspace = \"../../../outside\"\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\nbuild = \"../../../outside.rs\"\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\n[lib]\npath = \"../../../outside.rs\"\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\n[[bin]]\nname = \"outside\"\npath = \"../../../outside.rs\"\n",
        ),
        (
            "crates/core/Cargo.toml",
            "bin = [{ name = 'outside', path = '/outside.rs' }]\n[package]\nname = \"core-fx\"\nversion = \"0.0.0\"\n",
        ),
    ];
    for (path, text) in cases {
        let (s, _) = rust_repo();
        write(&s.0, path, text);
        let base = commit(&s.0, "outside manifest path");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        commit(&s.0, "change core");
        assert_cargo_not_invoked(&s.0, &base);
    }
}

#[cfg(unix)]
#[test]
fn round3_legacy_dependencies_never_invoke_cargo() {
    for scope in ["", "target.'cfg(unix)'.", "target.'cfg(windows)'."] {
        for key in ["dev_dependencies", "build_dependencies"] {
            let (s, _) = rust_repo();
            write(
                &s.0,
                "crates/core/Cargo.toml",
                &format!(
                    "[package]\nname = 'core-fx'\nversion = '0.0.0'\nedition = '2021'\n[{scope}{key}.bridge]\npath = '/outside'\n"
                ),
            );
            let base = commit(&s.0, "legacy dependency path");
            write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
            commit(&s.0, "change core");
            assert_cargo_not_invoked(&s.0, &base);
        }
    }
}

#[test]
fn round3_confined_legacy_aliases_preserve_dependency_selection() {
    if !have("cargo") {
        eprintln!("skipped: cargo isn't on PATH");
        return;
    }
    let (s, _) = rust_repo();
    for (path, before, after) in [
        ("crates/core/Cargo.toml", "[package]", "[project]"),
        (
            "crates/cli/Cargo.toml",
            "[dependencies]",
            "[target.'cfg(windows)'.build_dependencies]",
        ),
        (
            "crates/checks/Cargo.toml",
            "[dev-dependencies]",
            "[dev_dependencies]",
        ),
    ] {
        let text = fs::read_to_string(s.0.join(path))
            .unwrap()
            .replace(before, after);
        write(&s.0, path, &text);
    }
    let base = commit(&s.0, "confined legacy manifest aliases");
    write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
    commit(&s.0, "change core");
    let (_, v) = affected(&s.0, &["--base", &base]);
    assert_eq!(v["data"]["mode"], "selective", "{v}");
    assert_eq!(
        v["data"]["selected"]["rust"],
        serde_json::json!(["checks-fx", "cli-fx", "core-fx"])
    );
}

#[cfg(unix)]
#[test]
fn round3_project_alias_never_invokes_cargo() {
    for key in ["workspace", "build"] {
        let (s, _) = rust_repo();
        write(
            &s.0,
            "crates/core/Cargo.toml",
            &format!(
                "[project]\nname = 'core-fx'\nversion = '0.0.0'\nedition = '2021'\n{key} = '/outside'\n"
            ),
        );
        let base = commit(&s.0, "legacy project path");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        commit(&s.0, "change core");
        assert_cargo_not_invoked(&s.0, &base);
    }
}

#[cfg(unix)]
#[test]
fn round3_case_variant_manifest_never_invokes_cargo() {
    for name in ["cargo.toml", "CARGO.TOML", "Cargo.Toml"] {
        let (s, _) = rust_repo();
        write(
            &s.0,
            &format!("crates/extra/{name}"),
            "[package]\nname = 'extra-fx'\nversion = '0.0.0'\nedition = '2021'\n[dependencies.bridge]\npath = '/outside'\n",
        );
        write(&s.0, "crates/extra/src/lib.rs", "pub fn extra() {}\n");
        let base = commit(&s.0, "case variant manifest");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        commit(&s.0, "change core");
        // Reject consistently on every host: Cargo would follow this manifest
        // on a case-insensitive filesystem, regardless of this test host.
        assert_cargo_not_invoked(&s.0, &base);
    }
}

#[cfg(unix)]
#[test]
fn root_manifest_case_variants_override_none_rules_before_cargo() {
    for name in ["cargo.toml", "CARGO.TOML", "Cargo.Toml"] {
        let (s, _) = rust_repo();
        // Use two renames so Git records the case change even on a
        // case-insensitive filesystem. There is no Go module in this fixture
        // to trigger snapshot creation on the Rust adapter's behalf.
        git(&s.0, &["mv", "Cargo.toml", "renaming-manifest"]);
        git(&s.0, &["mv", "renaming-manifest", name]);
        let mut config: Value =
            serde_json::from_slice(&fs::read(s.0.join(".mdkg/config.json")).unwrap()).unwrap();
        config["affected"] = serde_json::json!({"rules":[{"paths":["**"],"select":"none"}]});
        write(&s.0, ".mdkg/config.json", &config.to_string());
        let base = commit(&s.0, "root manifest case and none rule");
        write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
        commit(&s.0, "change core");
        assert_cargo_not_invoked(&s.0, &base);
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(codes(&v), ["adapter_failed"], "{name}: {v}");
        assert!(
            v["data"]["reasons"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("noncanonical casing"),
            "{name}: {v}"
        );
        assert_eq!(
            v["data"]["selected"],
            serde_json::json!({"go":[],"rust":[],"suites":[]})
        );
    }
}

#[test]
fn removed_case_variant_manifests_override_none_rules_without_an_adapter() {
    for path in [
        "cargo.toml",
        "crates/extra/CARGO.TOML",
        "GO.MOD",
        "tools/Go.Mod",
    ] {
        for action in ["delete", "rename"] {
            let (s, _) = repo();
            let mut config: Value =
                serde_json::from_slice(&fs::read(s.0.join(".mdkg/config.json")).unwrap()).unwrap();
            config["affected"] = serde_json::json!({"rules":[{"paths":["**"],"select":"none"}]});
            write(&s.0, ".mdkg/config.json", &config.to_string());
            write(
                &s.0,
                path,
                "[package]\nname = 'removed'\nversion = '0.0.0'\n",
            );
            let base = commit(&s.0, "case variant manifest before removal");
            if action == "delete" {
                fs::remove_file(s.0.join(path)).unwrap();
            } else {
                git(&s.0, &["mv", path, "retired-manifest.txt"]);
            }
            commit(&s.0, "remove manifest");
            let (_, v) = affected(&s.0, &["--base", &base]);
            assert_eq!(v["data"]["mode"], "full", "{path} {action}: {v}");
            assert_eq!(codes(&v), ["global_file"], "{path} {action}: {v}");
        }
    }
}

#[cfg(unix)]
#[test]
fn round5_go_module_case_variants_override_none_rules_before_go() {
    for dir in ["", "z/"] {
        for name in ["GO.MOD", "Go.mod", "go.Mod"] {
            let (s, _) = repo();
            let mut config: Value =
                serde_json::from_slice(&fs::read(s.0.join(".mdkg/config.json")).unwrap()).unwrap();
            config["affected"] = serde_json::json!({"rules":[{"paths":["**"],"select":"none"}]});
            write(&s.0, ".mdkg/config.json", &config.to_string());
            if !dir.is_empty() {
                // Validate every manifest before loading even the first
                // canonical module, which sorts before z/.
                write(&s.0, "a/go.mod", "module example.com/a\n\ngo 1.21\n");
                write(&s.0, "a/a.go", "package a\n");
            }
            write(
                &s.0,
                &format!("{dir}{name}"),
                "module example.com/fx\n\ngo 1.21\n",
            );
            let source = format!("{dir}pkg/p.go");
            write(&s.0, &source, "package p\nconst N = 1\n");
            let base = commit(&s.0, "case variant Go module");
            write(&s.0, &source, "package p\nconst N = 2\n");
            commit(&s.0, "change Go source");
            let v = assert_tool_not_invoked(&s.0, &base, "go");
            assert_eq!(codes(&v), ["adapter_failed"], "{dir}{name}: {v}");
            assert!(
                v["data"]["reasons"][0]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("noncanonical casing"),
                "{dir}{name}: {v}"
            );
        }
    }
}

#[test]
fn round5_builtin_global_case_variants_override_none_rules() {
    for path in [
        "cargo.lock",
        "testdata/cargo.toml",
        "docs/Cargo.toml",
        "Go.sum",
        "Rust-Toolchain.toml",
        "GO.WORK",
        ".CARGO/CONFIG.TOML",
        ".GITHUB/WORKFLOWS/test.yml",
    ] {
        let (s, _) = repo();
        let mut config: Value =
            serde_json::from_slice(&fs::read(s.0.join(".mdkg/config.json")).unwrap()).unwrap();
        config["affected"] = serde_json::json!({"rules":[{"paths":["**"],"select":"none"}]});
        write(&s.0, ".mdkg/config.json", &config.to_string());
        write(&s.0, path, "before\n");
        let base = commit(&s.0, "case variant global file");
        write(&s.0, path, "after\n");
        commit(&s.0, "change global file");
        let (_, v) = affected(&s.0, &["--base", &base]);
        assert_eq!(v["data"]["mode"], "full", "{path}: {v}");
        assert_eq!(codes(&v), ["global_file"], "{path}: {v}");
    }
}

#[test]
fn round3_text_output_escapes_manifest_controls() {
    let (s, _) = rust_repo();
    write(
        &s.0,
        "crates/core/Cargo.toml",
        "[package]\nname = 'core-fx'\nversion = '0.0.0'\n[dependencies.bridge]\npath = \"/outside\\u001b[2J\\r\\nforged\\u009b\\u202e\"\n",
    );
    let base = commit(&s.0, "terminal control in manifest path");
    write(&s.0, "crates/core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
    commit(&s.0, "change core");
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .args(["affected", "--base", &base])
        .output()
        .expect("text affected");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("mode: full\n"), "{text:?}");
    assert!(
        !text.contains(['\x1b', '\r', '\u{009b}', '\u{202e}']),
        "{text:?}"
    );
    assert!(text.contains("\\u{1b}[2J\\u{d}\\u{a}forged"), "{text:?}");
    // JSON retains the original detail for consumers; escaping is text-only.
    let (_, json) = affected(&s.0, &["--base", &base]);
    assert!(
        json["data"]["reasons"][0]["detail"]
            .as_str()
            .unwrap()
            .contains('\x1b')
    );
}

#[cfg(unix)]
#[test]
fn round3_text_output_escapes_changed_path_controls() {
    let (s, base) = repo();
    write(&s.0, "unknown\x1b[2J\nforged.txt", "data\n");
    commit(&s.0, "terminal control in path");
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(&s.0))
        .args(["affected", "--base", &base])
        .output()
        .expect("text affected");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains('\x1b'), "{text:?}");
    assert!(
        text.contains("unknown\\u{1b}[2J\\u{a}forged.txt"),
        "{text:?}"
    );
}
