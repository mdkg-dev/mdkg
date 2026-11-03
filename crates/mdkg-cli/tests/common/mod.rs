//! Shared by the integration tests: isolate commands from the outer
//! launcher and check JSON envelopes against their published schemas, so
//! a field that drifts from `schemas/` fails whichever test prints it.
#![allow(dead_code)]

pub mod temp;
pub mod template;

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const LAUNCHER_ENV: &[&str] = &[
    "MDKG_RUN_TURN",
    "MDKG_BIN",
    "MDKG_ACTOR",
    "MDKG_MAIN_REF",
    "MDKG_TRUSTED_COMMON_DIR",
    "MDKG_TRUSTED_WORKTREE",
];

/// Keep a test's temporary repository independent of the outer launcher.
/// Call before applying any launcher context the test intentionally uses.
/// These are the mdkg context variables set by profile.rs's agent_env and
/// mcp_env; HOME, XDG and Git settings belong to each test's own setup.
pub fn without_launcher_env(command: &mut Command) -> &mut Command {
    for key in LAUNCHER_ENV {
        command.env_remove(key);
    }
    command
}

/// Execution fixtures use `turn <name> <definition fields> <turn flags>`
/// to keep setup next to the assertion. This writes the fixture definition,
/// then returns the real public `agents up` argv. It is not a CLI alias.
pub fn agent_turn_args(root: &Path, args: &[&str]) -> Vec<String> {
    if args.first() != Some(&"turn") {
        return args.iter().map(|s| (*s).into()).collect();
    }
    let name = args.get(1).copied().unwrap_or("");
    let mut out = vec!["agents".into(), "up".into(), name.into()];
    let watch = args.contains(&"--watch");
    if !watch {
        out.push("--once".into());
    }
    let graph = fixture_authority_root(root);
    let path = graph.join(".mdkg/agents").join(format!("{name}.json"));
    let mut definition: Value = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .or_else(|| {
            fs::read(graph.join(".mdkg/config.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v.get("agents").and_then(|a| a.get(name)).cloned())
        })
        .unwrap_or_else(|| serde_json::json!({}));
    let mut changed = false;
    let mut lists = std::collections::BTreeMap::<&str, Vec<Value>>::new();
    let mut i = 2;
    while i < args.len() {
        let field = args[i].trim_start_matches("--");
        if [
            "harness", "role", "cwd", "model", "effort", "base", "allow", "protect",
        ]
        .contains(&field)
            && i + 1 < args.len()
        {
            let value = Value::String(args[i + 1].into());
            if matches!(field, "allow" | "protect") {
                lists.entry(field).or_default().push(value);
            } else {
                if field == "cwd" && !args.contains(&"--worktree") {
                    definition
                        .as_object_mut()
                        .expect("definition object")
                        .remove("worktree");
                }
                definition[field] = value;
            }
            changed = true;
            i += 2;
        } else if field == "worktree" {
            if !args.contains(&"--cwd") {
                definition
                    .as_object_mut()
                    .expect("definition object")
                    .remove("cwd");
            }
            definition["worktree"] = Value::Bool(true);
            changed = true;
            i += 1;
        } else if field == "watch" {
            i += 1;
        } else {
            out.push(args[i].into());
            i += 1;
        }
    }
    for (key, values) in lists {
        definition[key] = Value::Array(values);
    }
    if changed && mdkg_run::state::check_name(name).is_ok() && name != "defaults" {
        definition["schema"] = Value::String("mdkg.agent.v1".into());
        // Old execution cases explicitly exercise the worker-layout refusal;
        // their fixture has no implicit worktree. Agent-management tests
        // separately cover the new worker default.
        if definition.get("worktree").is_none() {
            definition["worktree"] = Value::Bool(false);
        }
        fs::create_dir_all(path.parent().expect("definition parent"))
            .expect("fixture definition folder");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&definition).expect("definition JSON"),
        )
        .expect("fixture definition");
        commit_fixture_definition(&graph, &path);
    }
    out
}

/// A fixture's git, isolated from the caller's repository, config and hooks.
fn fixture_git(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "user.name=Test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs")
}

/// Where `root`'s graph authority is committed: the same folder in the
/// repository's registered graph checkout (`mdkg.graph.<id>.worktree`, a
/// linked worktree's name), else `root` itself.
fn fixture_authority_root(root: &Path) -> std::path::PathBuf {
    let text = |args: &[&str]| {
        let out = fixture_git(root, args);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let (Some(top), Some(common)) = (
        text(&["rev-parse", "--show-toplevel"]),
        text(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ) else {
        return root.to_path_buf();
    };
    let registered = text(&["config", "--get-regexp", r"^mdkg\.graph\..*\.worktree$"]);
    // The linked worktree's `gitdir` names its `.git` file.
    let checkout = registered
        .as_deref()
        .and_then(|lines| lines.lines().next())
        .and_then(|line| line.split_once(' '))
        .and_then(|(_, name)| {
            fs::read_to_string(
                Path::new(&common)
                    .join("worktrees")
                    .join(name)
                    .join("gitdir"),
            )
            .ok()
        })
        .and_then(|gitdir| Path::new(gitdir.trim()).parent().map(Path::to_path_buf));
    let (Some(checkout), Ok(top), Ok(real)) =
        (checkout, fs::canonicalize(&top), fs::canonicalize(root))
    else {
        return root.to_path_buf();
    };
    match real.strip_prefix(&top) {
        Ok(relative) if checkout.is_dir() => checkout.join(relative),
        _ => root.to_path_buf(),
    }
}

/// Agent definitions are read only from committed authority, so a fixture
/// definition is committed like any other deliberate setup. Outside a Git
/// checkout it stays a file, and the command reports that itself.
fn commit_fixture_definition(root: &Path, path: &Path) {
    let git = |args: &[&str]| fixture_git(root, args);
    if !git(&["rev-parse", "--is-inside-work-tree"])
        .status
        .success()
    {
        return;
    }
    let path = path.to_str().expect("UTF-8 fixture path");
    let added = git(&["add", "--", path]);
    assert!(added.status.success(), "{added:?}");
    // An unchanged definition has nothing to commit.
    let staged = git(&["diff", "--cached", "--quiet", "--", path]);
    if staged.status.code() == Some(1) {
        let committed = git(&["commit", "-q", "-m", "fixture definition", "--", path]);
        assert!(committed.status.success(), "{committed:?}");
    }
}

/// Validate the actual event envelopes, then expose the one turn's data to
/// execution assertions. Other commands continue using their own envelopes.
pub fn one_turn(stdout: &[u8]) -> Value {
    let events: Vec<Value> = stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("event JSON"))
        .collect();
    assert!(!events.is_empty(), "manual pass emitted no event");
    for event in &events {
        conforms_envelope(event);
    }
    let mut event = events
        .iter()
        .find(|e| e["ok"] == false)
        .or_else(|| events.iter().find(|e| e["data"].get("turn").is_some()))
        .unwrap_or(&events[0])
        .clone();
    if let Some(turn) = event["data"].get("turn").cloned() {
        event["data"] = turn;
    }
    event
}

/// Tests that call a library which spawns mdkg cannot configure its Command.
/// Re-run just that test in a clean child, without mutating the environment
/// shared by parallel tests. Return true when the child has done the work.
pub fn rerun_without_launcher_env(test: &str) -> bool {
    if !LAUNCHER_ENV
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return false;
    }
    let output = without_launcher_env(
        Command::new(std::env::current_exe().expect("test binary")).args(["--exact", test]),
    )
    .output()
    .expect("run isolated test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "isolated {test}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[path = "../../../../conformance/api-v1/schema.rs"]
pub mod api_schema;
pub use api_schema::check;

pub fn schema(command: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(format!("mdkg.{command}.v1.json"));
    serde_json::from_slice(&fs::read(&path).expect("schema file")).expect("schema JSON")
}

pub fn conforms(command: &str, envelope: &Value) {
    let schema = schema(command);
    let mut errors = Vec::new();
    check(&schema, &schema, envelope, "$", &mut errors);
    assert!(
        errors.is_empty(),
        "{command} output breaks its schema:\n{}",
        errors.join("\n")
    );
}

/// Checks an envelope against the schema its `schema` field names.
pub fn conforms_envelope(envelope: &Value) {
    let id = envelope["schema"].as_str().unwrap_or_default();
    let command = id
        .strip_prefix("mdkg.")
        .and_then(|r| r.strip_suffix(".v1"))
        .unwrap_or_else(|| panic!("not a v1 schema id: {envelope}"));
    conforms(command, envelope);
}
