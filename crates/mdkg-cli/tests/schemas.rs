//! The schemas in `schemas/` match what the code generates (edd-4). After
//! changing an output type, regenerate them:
//! `cargo run -p mdkg -- schema --out schemas`.

mod common;

use std::path::Path;
use std::process::Command;

const COMMANDS: &[&str] = &[
    "list",
    "search",
    "index.rebuild",
    "index.status",
    "graph.refs",
    "graph.backrefs",
    "graph.ready",
    "graph.critical-path",
    "scratch.path",
    "work.localize",
    "config.show",
    "pack",
    "show",
    "validate",
    "claim",
    "release",
    "renew",
    "holds",
    "status",
    "sync",
    "abandon",
    "new",
    "done",
    "update",
    "next",
    "goal.current",
    "goal.next",
    "goal.activate",
    "goal.pause",
    "goal.done",
    "goal.claim",
    "format",
    "retype",
    "agent",
    "agents.add",
    "agents.edit",
    "agents.rm",
    "agents.up",
    "agents.status",
    "agents.resume",
    "install",
    "cli",
    "msg.send",
    "msg.inbox",
    "msg.wait",
    "msg.ack",
    "msg.thread",
    "msg.import",
    "msg.decisions",
    "room.create",
    "room.join",
    "room.leave",
    "room.list",
    "store.snapshots",
    "store.restore",
    "anchor",
    "export",
    "verify",
    "version",
    "init",
    "doctor",
    "skills.list",
    "skills.show",
    "skills.validate",
    "skills.sync",
    "skills.add",
    "migrate",
    "affected",
];

#[test]
fn checked_in_schemas_match_the_code() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
    let mut stale = Vec::new();
    for command in COMMANDS {
        let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .args(["schema", command])
            .output()
            .expect("run mdkg schema");
        assert!(out.status.success(), "schema {command} failed");
        let file = dir.join(format!("mdkg.{command}.v1.json"));
        let on_disk = std::fs::read(&file).unwrap_or_default();
        if on_disk != out.stdout {
            stale.push(file.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "stale schemas (run `cargo run -p mdkg -- schema --out schemas`): {stale:?}"
    );
}

#[test]
fn every_command_lists_its_schema() {
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("schema")
        .output()
        .expect("run mdkg schema");
    assert!(out.status.success(), "schema listing failed");
    // Exactly the same schema ids both ways.
    let mut got: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let mut expected: Vec<String> = COMMANDS.iter().map(|c| format!("mdkg.{c}.v1")).collect();
    expected.sort();
    got.sort();
    assert_eq!(
        got, expected,
        "the CLI's schema list and this test's differ"
    );
}

/// Every checked-in schema is one the list above checks, so none drifts
/// unnoticed.
#[test]
fn every_checked_in_schema_is_checked() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
    for entry in std::fs::read_dir(&dir).expect("schemas") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        // Only schemas: an editor's or the desktop's files (`.DS_Store`,
        // `._x.json`, `.#x.json`, swap files) don't count.
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let command = name
            .strip_prefix("mdkg.")
            .and_then(|n| n.strip_suffix(".v1.json"))
            .unwrap_or(&name)
            .to_string();
        assert!(COMMANDS.contains(&command.as_str()), "{name} isn't checked");
    }
}

#[test]
fn agent_schema_agrees_with_required_execution_fields_and_reserved_extends() {
    let schema = common::schema("agent");
    for (value, valid) in [
        (serde_json::json!({"schema":"mdkg.agent.v1"}), false),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "harness":"claude"}),
            false,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "harness":null, "role":"worker"}),
            false,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "extends":""}),
            false,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "extends":null}),
            false,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "extends":"https://example.invalid/agent.json"}),
            true,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "harness":"codex", "role":"worker"}),
            true,
        ),
        (
            serde_json::json!({"schema":"mdkg.agent.v1", "harness":"codex", "role":"worker", "mode":"other"}),
            false,
        ),
    ] {
        let mut errors = Vec::new();
        common::check(&schema, &schema, &value, "$", &mut errors);
        assert_eq!(errors.is_empty(), valid, "{value}: {errors:?}");
        assert_eq!(
            mdkg_run::orchestration::agent::Definition::parse("writer", value.clone()).is_ok(),
            valid,
            "{value}"
        );
    }
}

/// Orchestrator, 2026-10-21: the commands a lease authority runs say which
/// contract this build speaks, with no repository needed.
#[test]
fn authority_commands_carry_the_contract_marker() {
    for command in ["claim", "release", "renew", "holds", "status"] {
        let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(std::env::temp_dir())
            .args(["schema", command])
            .output()
            .expect("run mdkg schema");
        assert!(out.status.success(), "schema {command} failed");
        let schema: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
        let marker = &schema["x-mdkg-lease-authority"];
        if command == "status" {
            assert!(marker.is_null(), "{command}");
        } else {
            assert_eq!(marker, &serde_json::json!(1), "{command}");
        }
    }
}
