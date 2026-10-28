//! The binary names itself as it was invoked (the orchestrator,
//! 2026-10-25): installed beside another mdkg under its own name (the
//! dogfood `mdkg-next`), its version, help, examples and hints name that
//! one, never the `mdkg` that PATH would run instead.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// The built binary, linked as `mdkg-rc` in a folder of its own.
fn installed_as_rc() -> (Scratch, PathBuf) {
    let dir = common::temp::dir("mdkg-cli-program-name");
    let link = dir.join("mdkg-rc");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mdkg"), &link).expect("link");
    (Scratch(dir), link)
}

fn run(exe: &PathBuf, args: &[&str]) -> (i32, String, String) {
    // HOME and XDG_CONFIG_HOME in the scratch dir, so nothing reads the
    // user's own config (the lease authority's lookup included).
    let scratch = exe.parent().expect("scratch dir");
    let out = common::without_launcher_env(&mut Command::new(exe))
        .args(args)
        .current_dir(std::env::temp_dir())
        .env("HOME", scratch)
        .env("XDG_CONFIG_HOME", scratch)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn version_help_examples_and_hints_name_the_binary_as_invoked() {
    let (_dir, rc) = installed_as_rc();
    let (code, out, _) = run(&rc, &["--version"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("mdkg-rc "), "{out}");
    let (_, out, _) = run(&rc, &["version"]);
    assert!(out.starts_with("mdkg-rc "), "{out}");
    let (_, out, _) = run(&rc, &["--help"]);
    assert!(out.contains("Usage: mdkg-rc "), "{out}");
    let (_, out, _) = run(&rc, &["list", "--help"]);
    assert!(out.contains("Usage: mdkg-rc list"), "{out}");
    assert!(out.contains("\n  mdkg-rc list --type task"), "{out}");
    assert!(!out.contains("  mdkg list"), "{out}");
    // A hint, in JSON and in text.
    let (_, out, _) = run(&rc, &["frob", "--json"]);
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["diagnostics"][0]["hint"], "run `mdkg-rc --help`", "{out}");
    let (_, _, err) = run(&rc, &["--root", "/nonexistent-mdkg-root", "list"]);
    assert!(err.contains("`mdkg-rc init`"), "{err}");
    assert!(!err.contains("`mdkg "), "{err}");
}

#[test]
fn under_its_own_name_the_binary_says_mdkg() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_mdkg"));
    let (_, out, _) = run(&exe, &["--version"]);
    assert!(out.starts_with("mdkg "), "{out}");
    let (_, out, _) = run(&exe, &["frob", "--json"]);
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["diagnostics"][0]["hint"], "run `mdkg --help`", "{out}");
}

/// Tier 2 review of 2132e17: an argv[0] holding odd text names nothing; it
/// falls back to `mdkg`, in clap's usage lines too.
#[test]
fn a_hostile_argv0_reaches_no_output() {
    use std::os::unix::process::CommandExt;
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg0("mdkg\nFORGED LINE")
        .arg("--help")
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("FORGED"), "{text}");
    assert!(text.contains("Usage: mdkg "), "{text}");
}

/// Tier 2 review of 2132e17: names are inserted where this binary writes
/// a command, so a 0.6.0 command in a hint keeps 0.6.0's name.
#[test]
fn a_0_6_0_command_in_a_hint_keeps_its_name() {
    let (dir, rc) = installed_as_rc();
    let graph = dir.0.join("v2");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/identity-v2");
    let ok = Command::new("cp")
        .arg("-R")
        .arg(&fixture)
        .arg(&graph)
        .status()
        .expect("cp")
        .success();
    assert!(ok);
    let (_, out, _) = run(
        &rc,
        &["--root", graph.to_str().expect("utf-8"), "list", "--json"],
    );
    let v: Value = serde_json::from_str(&out).expect("json");
    let hint = v["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("`mdkg-rc migrate --dry-run`"), "{out}");
    assert!(hint.contains("0.6.0's `mdkg graph recover`"), "{out}");
    assert!(hint.contains("(`mdkg graph migrate --apply`"), "{out}");
}

/// Tier 2 round 2 of 4e995cd: a lease-conflict hint names both of its
/// commands (release, then claim) as the binary was invoked.
#[test]
fn a_lease_conflict_hint_names_both_commands() {
    let (dir, rc) = installed_as_rc();
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join("src")).expect("src");
    fs::write(repo.join("src/file.rs"), "x").expect("file");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    let root = repo.to_str().expect("utf-8").to_string();
    let (code, out, err) = run(&rc, &["--root", &root, "init"]);
    assert_eq!(code, 0, "{out}{err}");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    let as_a = ["--root", root.as_str(), "--as", "agent-a"];
    let (code, out, _) = run(
        &rc,
        &[&as_a[..], &["claim", "src/file.rs", "--json"]].concat(),
    );
    assert_eq!(code, 0, "{out}");
    let (_, out, _) = run(&rc, &[&as_a[..], &["claim", "src", "--json"]].concat());
    let v: Value = serde_json::from_str(&out).expect("json");
    let hint = v["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("`mdkg-rc release "), "{out}");
    assert!(hint.contains("`mdkg-rc claim "), "{out}");
    assert!(!hint.contains("`mdkg "), "{out}");
}
