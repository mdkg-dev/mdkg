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

/// Terra #15584: a served call's child names itself as the server was
/// invoked. The server runs its own program by real path (Linux's
/// `/proc/self/exe`, else the canonical path, here the link's target
/// `mdkg`), so without its own argv[0] the child's hints named that.
#[test]
fn an_mcp_call_s_hint_names_the_binary_as_invoked() {
    let (dir, rc) = installed_as_rc();
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let ok = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git")
        .success();
    assert!(ok);
    let root = repo.to_str().expect("utf-8").to_string();
    let (code, out, err) = run(&rc, &["--root", &root, "init"]);
    assert_eq!(code, 0, "{out}{err}");
    let mut child = common::without_launcher_env(&mut Command::new(&rc))
        .args(["--root", &root, "--as", "agent-x", "mcp"])
        .env("HOME", &dir.0)
        .env("XDG_CONFIG_HOME", &dir.0)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("mcp");
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "lease_claim", "arguments": { "target": "nope" } },
    });
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "{call}").expect("write");
    }
    let out = child.wait_with_output().expect("mcp output");
    let reply: Value = serde_json::from_slice(&out.stdout).expect("one reply");
    let text = reply["result"]["content"]
        .as_array()
        .and_then(|c| c.last())
        .and_then(|c| c["text"].as_str())
        .expect("envelope text");
    let envelope: Value = serde_json::from_str(text).expect("envelope");
    let hint = envelope["diagnostics"][0]["hint"]
        .as_str()
        .unwrap_or_default();
    assert!(hint.contains("`mdkg-rc claim --new "), "{text}");
    assert!(!hint.contains("`mdkg "), "{text}");
}

/// e2e B16: a hint that writes a command names it as this binary runs
/// (`BIN`), never a literal `mdkg`, so mdkg-next's hints never mix the
/// two. Scans every non-test line of the CLI's sources for a string that
/// writes `mdkg <command>`. A 0.6.0 command keeps its name (a line naming
/// 0.6), and text persisted as history is listed with why.
#[test]
fn no_hint_writes_a_literal_mdkg_command() {
    // Text that names the product or the public command on purpose, each
    // with why: (file, a snippet of the line).
    const ALLOWED: &[(&str, &str)] = &[
        // The migration's commit message: history, which names the
        // product, not the binary that wrote it.
        (
            "mdkg-cli/src/migrate/apply.rs",
            "`mdkg migrate --rollback`.",
        ),
        // localize's own pattern.
        ("mdkg-format/src/program.rs", "\"`mdkg \""),
        // The ref-mode README's replacements: they must match the generated
        // README's own text (init_readme.md), which names the public command.
        ("mdkg-cli/src/init.rs", "run `mdkg skills sync` to refresh"),
        (
            "mdkg-cli/src/init.rs",
            "`mdkg install`\\nrefreshes the enabled mirrors",
        ),
        (
            "mdkg-cli/src/init.rs",
            "add it later with\\n`mdkg skills add",
        ),
        ("mdkg-cli/src/init.rs", "enabled (`mdkg install --enable"),
        // The ref-mode README's lead for agents: human documentation in the
        // user's repository, where `mdkg` is the published command, like the
        // README's `mdkg doctor` line.
        (
            "mdkg-cli/src/init.rs",
            "Agents: read `mdkg skills show mdkg-workflow` before any task",
        ),
        (
            "mdkg-cli/src/init.rs",
            "`mdkg init` in an existing clone: it would start a second graph.",
        ),
        // The AGENTS.md block install writes: the public command on PATH.
        (
            "mdkg-run/src/install.rs",
            "This project uses mdkg for its graph",
        ),
        (
            "mdkg-run/src/install.rs",
            "- Start each turn with `mdkg msg inbox`",
        ),
        (
            "mdkg-run/src/install.rs",
            "- Before editing, claim the task and every file",
        ),
        (
            "mdkg-run/src/install.rs",
            "- Run relevant checks and `mdkg validate`",
        ),
        (
            "mdkg-run/src/install.rs",
            "- Send questions, findings and handoffs through",
        ),
        ("mdkg-run/src/install.rs", "Use `mdkg --help` for commands."),
        // The OpenCode plugin's own source, as written to disk.
        (
            "mdkg-run/src/hooks.rs",
            "written by `mdkg install` from .mdkg/hooks/",
        ),
        ("mdkg-run/src/hooks.rs", "console.warn(`mdkg hook ${event}"),
    ];
    // Every crate's shipped source, the libraries' hints included (review
    // of 591a4913).
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates")
        .to_path_buf();
    let mut files: Vec<std::path::PathBuf> = fs::read_dir(&crates)
        .expect("crates")
        .map(|e| e.expect("entry").path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    let mut hits = Vec::new();
    while let Some(path) = files.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(&path).expect("dir") {
                files.push(entry.expect("entry").path());
            }
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let rel = path
            .strip_prefix(&crates)
            .expect("in crates")
            .to_string_lossy()
            .into_owned();
        // A test module file (`mod tests;` puts one beside its parent).
        if rel.ends_with("/tests.rs") || rel.contains("/tests/") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("source");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            // An inline test module ends what ships; `mod tests;` doesn't,
            // the file's code goes on after it (review of 591a4913).
            if t == "#[cfg(test)]"
                && lines
                    .get(i + 1)
                    .is_some_and(|n| n.contains("mod ") && n.trim_end().ends_with('{'))
            {
                break;
            }
            // Comments aside, every line counts, a multi-line string's
            // continuation (no quote of its own) included.
            if t.starts_with("//") || t.contains("0.6") {
                continue;
            }
            let writes = [
                "`mdkg ",
                "run mdkg ",
                "with: mdkg ",
                "then mdkg ",
                "running mdkg ",
            ]
            .iter()
            .any(|p| t.contains(p));
            let allowed = ALLOWED
                .iter()
                .any(|(file, snippet)| rel.ends_with(file) && t.contains(snippet));
            if writes && !allowed {
                hits.push(format!("{rel}:{}: {t}", i + 1));
            }
        }
    }
    assert!(hits.is_empty(), "hints with a literal mdkg: {hits:#?}");
}

/// A migration's "apply with" line names the binary as invoked (e2e B16;
/// review of 591a4913).
#[test]
fn a_migration_plan_s_apply_line_names_the_binary_as_invoked() {
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
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &["commit", "-qm", "init"],
    ] {
        // No inherited repository or index redirect reaches the fixture's
        // git (review of f02a3c6a).
        let ok = Command::new("git")
            .arg("-C")
            .arg(&graph)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    }
    let root = graph.to_str().expect("utf-8");
    let (code, out, err) = run(&rc, &["--root", root, "--as", "a", "migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("\napply with: mdkg-rc migrate --apply "),
        "{out}"
    );
    assert!(!out.contains("mdkg migrate"), "{out}");
}
