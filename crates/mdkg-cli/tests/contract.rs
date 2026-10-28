//! The output contract (edd-4) on paths the 0.6.0 conformance suite never
//! reaches: real output against the published schemas, the default field
//! set, `--fields` edge cases, usage-error and config envelopes, and the
//! hint printed for 0.6.0 spellings.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;
use common::conforms;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch(case: &str) -> Scratch {
    let dir = common::temp::dir("mdkg-cli-contract");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &dir);
    copy_tree(&conformance.join("cases").join(case), &dir);
    fs::remove_file(dir.join("commands")).ok();
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

/// Runs mdkg in `dir`; returns (exit code, stdout JSON, stderr).
fn run(dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run mdkg");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("{args:?}: stdout isn't one JSON envelope: {stdout}"));
    common::conforms_envelope(&json);
    (
        out.status.code().unwrap_or(-1),
        json,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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
fn read_output_matches_the_published_schemas() {
    for case in [
        "g01-valid-basic",
        "e02-missing-target",
        "s02-workspaces",
        "q01-search",
    ] {
        let dir = scratch(case);
        for (command, args) in [
            ("list", vec!["list", "--json"]),
            ("list", vec!["list", "--fields", "all", "--json"]),
            ("search", vec!["search", "task", "--json"]),
            ("validate", vec!["validate", "--json"]),
            ("next", vec!["next", "--json"]),
        ] {
            let (_, out, _) = run(&dir.0, &args);
            conforms(command, &out);
        }
        let (_, list, _) = run(&dir.0, &["list", "--json"]);
        if let Some(id) = list["data"]["nodes"][0]["id"].as_str() {
            let (_, out, _) = run(&dir.0, &["show", id, "--json"]);
            conforms("show", &out);
        }
    }
}

#[test]
fn list_defaults_to_the_compact_field_set() {
    let dir = scratch("g01-valid-basic");
    let (code, out, _) = run(&dir.0, &["list", "--json"]);
    assert_eq!(code, 0, "{out}");
    let node = out["data"]["nodes"][0].as_object().expect("a node");
    let mut keys: Vec<&str> = node.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["id", "path", "priority", "status", "title", "type"]);
}

#[test]
fn fields_edge_cases() {
    let dir = scratch("s02-workspaces");
    // An empty selection is a usage error, not "everything".
    for empty in ["", ","] {
        let (code, out, _) = run(&dir.0, &["list", "--fields", empty, "--json"]);
        assert_eq!(
            (code, codes(&out)),
            (1, vec!["usage.unknown_field".to_string()])
        );
    }
    // `ws` stays with `id`, so records from different workspaces differ.
    let (_, out, _) = run(&dir.0, &["list", "--fields", "title", "--json"]);
    let mut seen = std::collections::BTreeSet::new();
    for n in out["data"]["nodes"].as_array().expect("nodes") {
        assert!(
            seen.insert((n["ws"].to_string(), n["id"].to_string())),
            "two records look the same: {n}"
        );
    }
    // --meta leaves out the body, so asking for only the body is an error.
    let (_, list, _) = run(&dir.0, &["list", "--json"]);
    let id = list["data"]["nodes"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    let (code, out, _) = run(
        &dir.0,
        &["show", &id, "--meta", "--fields", "body", "--json"],
    );
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.invalid_arguments".to_string()])
    );
}

#[test]
fn usage_errors_name_the_real_command_and_keep_the_message() {
    let dir = scratch("g01-valid-basic");
    let (code, out, _) = run(&dir.0, &["list", "--bogus", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.list.v1");
    assert_eq!(codes(&out), vec!["usage.invalid_arguments"]);

    // A 0.6.0 spelling maps to the new command's schema, and the missing
    // argument is named.
    let (code, out, _) = run(&dir.0, &["task", "start", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.claim.v1");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("<TARGET>"), "{message}");
}

#[test]
fn a_missing_config_is_a_coded_envelope() {
    let dir = scratch("g01-valid-basic");
    fs::remove_file(dir.0.join(".mdkg/config.json")).expect("remove config");
    let (code, out, _) = run(&dir.0, &["list", "--json"]);
    assert_eq!(code, 1, "a wrong root is a usage error: {out}");
    assert_eq!(out["schema"], "mdkg.list.v1");
    assert_eq!(out["ok"], false);
    assert_eq!(codes(&out), vec!["config.missing"]);
}

#[test]
fn invalid_references_are_not_quoted_in_messages() {
    let dir = scratch("g01-valid-basic");
    let task = fs::read_dir(dir.0.join(".mdkg/work"))
        .expect("work")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "md"))
        .expect("a node file");
    let text = fs::read_to_string(&task).expect("read");
    let marker = "sk_live!not-a-ref";
    let edited = text.replacen("\ntitle:", &format!("\nparent: {marker}\ntitle:"), 1);
    fs::write(&task, edited).expect("write");
    let (_, out, _) = run(&dir.0, &["validate", "--json"]);
    assert!(codes(&out).iter().any(|c| c == "node.bad_ref"), "{out}");
    assert!(!out.to_string().contains(marker), "{out}");

    // A semantic reference, too.
    fs::write(&task, &text).expect("restore");
    let semantic = "not a ref!secret";
    let edited = text.replacen(
        "\ntitle:",
        &format!("\ncontext_refs: [{semantic}]\ntitle:"),
        1,
    );
    fs::write(&task, edited).expect("write");
    let (_, out, _) = run(&dir.0, &["validate", "--json"]);
    assert!(codes(&out).iter().any(|c| c == "node.bad_ref"), "{out}");
    assert!(!out.to_string().contains("secret"), "{out}");
}

#[test]
fn usage_errors_never_quote_typed_values() {
    let dir = scratch("g01-valid-basic");
    let (code, out, _) = run(
        &dir.0,
        &["new", "task", "T", "--priority", "sk_live_x", "--json"],
    );
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.new.v1");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("--priority"), "{message}");
    assert!(!out.to_string().contains("sk_live_x"), "{out}");
    // Stray positionals and unknown commands aren't echoed either.
    for args in [
        vec!["claim", "task-1", "sk_live_q", "--json"],
        vec!["sk_live_cmd", "--json"],
    ] {
        let (code, out, _) = run(&dir.0, &args);
        assert_eq!(code, 1, "{out}");
        assert!(!out.to_string().contains("sk_live"), "{out}");
    }
    for typed in ["--bogus=sk_live_flag", "--sk-live-abc123"] {
        let (_, out, _) = run(&dir.0, &["list", typed, "--json"]);
        let message = out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.starts_with("unknown argument"), "{message}");
        assert!(
            !message.contains("sk_live") && !message.contains("sk-live"),
            "{message}"
        );
    }
    // A close misspelling gets clap's suggestion, which is a real flag.
    let (_, out, _) = run(&dir.0, &["list", "--jsno", "--json"]);
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("--json"), "{message}");
    // Typed text that starts with dashes isn't a flag name either.
    let (_, out, _) = run(&dir.0, &["new", "task", "T", "--my secret plan", "--json"]);
    assert!(!out.to_string().contains("secret"), "{out}");
    // A missing value and a repeated flag say so.
    let (_, out, _) = run(&dir.0, &["list", "--json", "--as"]);
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.starts_with("missing value"), "{message}");
    let (_, out, _) = run(&dir.0, &["list", "--json", "--json"]);
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("more than once"), "{message}");
    // A global flag between `task` and `start` still maps the old spelling.
    let (_, out, _) = run(&dir.0, &["task", "--as", "bob", "start", "--json"]);
    assert_eq!(out["schema"], "mdkg.claim.v1");
    // No command at all has its own published envelope.
    let (code, out, _) = run(&dir.0, &["bogus", "--json"]);
    assert_eq!((code, out["schema"].as_str()), (1, Some("mdkg.cli.v1")));
}

#[test]
fn status_and_type_filters_take_comma_lists() {
    let dir = scratch("s01-sorting");
    let ids = |args: &[&str]| -> std::collections::BTreeSet<String> {
        let (code, out, _) = run(&dir.0, args);
        assert_eq!(code, 0, "{args:?}: {out}");
        out["data"]["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .map(|n| format!("{}:{}", n["ws"], n["id"]))
            .collect()
    };
    let (_, all, _) = run(&dir.0, &["list", "--json"]);
    let statuses: std::collections::BTreeSet<&str> = all["data"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|n| n["status"].as_str())
        .collect();
    let statuses: Vec<&str> = statuses.into_iter().collect();
    assert!(statuses.len() >= 2, "the case needs two statuses: {all}");
    // A comma list is the union of its values, case and spaces ignored.
    let (a, b) = (statuses[0], statuses[1]);
    let mut union = ids(&["list", "--status", a, "--json"]);
    union.extend(ids(&["list", "--status", b, "--json"]));
    let spelled = format!("{}, {}", a.to_uppercase(), b);
    assert_eq!(ids(&["list", "--status", &spelled, "--json"]), union);
    let mut union = ids(&["list", "--type", "task", "--json"]);
    union.extend(ids(&["list", "--type", "rule", "--json"]));
    assert_eq!(ids(&["list", "--type", "task,rule", "--json"]), union);

    // A value no node can have is a usage error, not an empty result, and
    // the typed value isn't echoed.
    for (args, code) in [
        (
            vec!["list", "--status", "todo,sk_live_s", "--json"],
            "usage.unknown_status",
        ),
        (
            vec!["list", "--status", "", "--json"],
            "usage.unknown_status",
        ),
        (
            vec!["list", "--status", ",", "--json"],
            "usage.unknown_status",
        ),
        (
            vec!["list", "--type", "task,sk_live_t", "--json"],
            "usage.unknown_type",
        ),
        (vec!["list", "--type", "", "--json"], "usage.unknown_type"),
        (
            vec!["list", "--tags", ",", "--json"],
            "usage.invalid_arguments",
        ),
        (
            vec!["search", "a", "--status", "sk_live_q", "--json"],
            "usage.unknown_status",
        ),
        (
            vec!["search", "a", "--type", "sk_live_q", "--json"],
            "usage.unknown_type",
        ),
    ] {
        let (exit, out, _) = run(&dir.0, &args);
        assert_eq!((exit, codes(&out)), (1, vec![code.to_string()]), "{args:?}");
        assert!(!out.to_string().contains("sk_live"), "{out}");
    }
    // Other filter values that can't match are usage errors too, and an
    // unknown workspace isn't echoed.
    for (args, exit_code, code) in [
        (
            vec!["list", "--priority", "99", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["list", "--priority", "-1", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["list", "--ws", "", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["search", "a", "--ws", " ", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["show", "task-1", "--ws", "", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["list", "--epic", "", "--json"],
            1,
            "usage.invalid_arguments",
        ),
        (
            vec!["list", "--ws", "sk_live_w", "--json"],
            3,
            "config.unknown_workspace",
        ),
    ] {
        let (exit, out, _) = run(&dir.0, &args);
        assert_eq!(
            (exit, codes(&out)),
            (exit_code, vec![code.to_string()]),
            "{args:?}"
        );
        assert!(!out.to_string().contains("sk_live"), "{out}");
    }

    // `all` in any case means every workspace, and an epic is trimmed like
    // the other values.
    let every = ids(&["list", "--json"]);
    assert_eq!(ids(&["list", "--ws", " ALL ", "--json"]), every);
    let edges = scratch("e01-edges");
    let under = |epic: &str| {
        let (code, out, _) = run(&edges.0, &["list", "--epic", epic, "--json"]);
        (code, out["data"]["count"].clone())
    };
    assert_eq!(under(" epic-1 "), under("epic-1"));
    assert_eq!(under("epic-1").0, 0);

    // Statuses from the coordinator and decisions are known values, even
    // where the config lists only the lasting work statuses.
    let config_path = dir.0.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    config["work"]["status_enum"] = serde_json::json!(["backlog", "todo", "done"]);
    fs::write(&config_path, config.to_string()).expect("write config");
    for status in ["progress", "review", "blocked", "accepted", "archived"] {
        let (exit, out, _) = run(&dir.0, &["list", "--status", status, "--json"]);
        assert_eq!(exit, 0, "{status}: {out}");
    }
    // `skill` among the types keeps its pointer to `mdkg skill list`.
    let (exit, out, _) = run(&dir.0, &["list", "--type", "task,skill", "--json"]);
    assert_eq!(
        (exit, codes(&out)),
        (1, vec!["usage.unsupported_type".to_string()])
    );
}

#[test]
fn the_unknown_workspace_hint_names_only_indexed_workspaces() {
    let dir = scratch("g01-valid-basic");
    let config_path = dir.0.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    config["workspaces"]["off"] = serde_json::json!({ "path": "off", "enabled": false });
    config["subgraphs"] = serde_json::json!({ "ext": {} });
    fs::write(&config_path, config.to_string()).expect("write config");
    let (code, out, _) = run(&dir.0, &["list", "--ws", "nope", "--json"]);
    assert_eq!(
        (code, codes(&out)),
        (3, vec!["config.unknown_workspace".to_string()])
    );
    assert_eq!(
        out["diagnostics"][0]["hint"], "workspaces: root, all",
        "{out}"
    );
}

#[test]
fn validate_text_summarises_repeated_warnings() {
    let dir = scratch("g01-valid-basic");
    let text = |args: &[&str]| {
        let out = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .arg("--root")
            .arg(&dir.0)
            .args(args)
            .output()
            .expect("run mdkg");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (_, json, _) = run(&dir.0, &["validate", "--json"]);
    let repeated = codes(&json)
        .iter()
        .filter(|c| *c == "node.heading_missing")
        .count();
    assert!(repeated > 1, "the case needs a repeated warning: {json}");
    let mentions = |s: &str| {
        s.lines()
            .filter(|l| l.contains("node.heading_missing"))
            .count()
    };

    let (code, summary) = text(&["validate"]);
    assert_eq!(code, 0, "{summary}");
    assert_eq!(mentions(&summary), 1, "{summary}");
    assert!(
        summary.contains(&format!("{repeated} warnings")),
        "{summary}"
    );
    assert!(summary.contains("--warnings"), "{summary}");

    let (_, all) = text(&["validate", "--warnings"]);
    assert_eq!(mentions(&all), repeated, "{all}");
    let (_, quiet) = text(&["validate", "--quiet"]);
    assert_eq!(mentions(&quiet), 0, "{quiet}");
    let (code, _) = text(&["validate", "--quiet", "--warnings"]);
    assert_eq!(code, 1);
}

#[test]
fn validate_orders_files_and_slugs_as_0_6_0_does() {
    // Files are ordered by full path across the workspace, so `a-b.md`
    // (`-` sorts before `/`) is seen before `a/x.md`, and the duplicate is
    // reported at the later file.
    let dir = scratch("g01-valid-basic");
    let work = dir.0.join(".mdkg/work");
    let task = fs::read_to_string(work.join("task-1-basic.md")).expect("read");
    fs::remove_file(work.join("task-1-basic.md")).expect("remove");
    fs::create_dir_all(work.join("a")).expect("mkdir");
    fs::write(work.join("a-b.md"), &task).expect("write");
    fs::write(work.join("a/x.md"), &task).expect("write");
    let (code, out, _) = run(&dir.0, &["validate", "--json"]);
    assert_eq!(code, 2, "{out}");
    let dup = out["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == "node.duplicate_id")
        .expect("a duplicate-id error");
    assert_eq!(dup["path"], ".mdkg/work/a/x.md", "{dup}");

    // A skill folder's slug is its lowercased name, not its `name` field.
    let dir = scratch("g01-valid-basic");
    let skill = dir.0.join(".mdkg/skills/Mixed");
    fs::create_dir_all(&skill).expect("mkdir");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: Not The Folder\ndescription: A skill.\n---\n# Purpose\n",
    )
    .expect("write");
    let path = dir.0.join(".mdkg/work/task-2-other.md");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(
        &path,
        text.replacen("\ntitle:", "\nskills: [mixed]\ntitle:", 1),
    )
    .expect("write");
    let (code, out, _) = run(&dir.0, &["validate", "--json"]);
    assert_eq!((code, &out["ok"]), (0, &Value::Bool(true)), "{out}");
    assert!(
        !codes(&out).iter().any(|c| c == "skill.missing_slug"),
        "{out}"
    );
    // The node parsed, with its skill, so the slug was really checked.
    let (_, shown, _) = run(&dir.0, &["show", "task-2", "--fields", "skills", "--json"]);
    assert_eq!(
        shown["data"]["node"]["skills"],
        serde_json::json!(["mixed"]),
        "{shown}"
    );
}

#[test]
fn an_identity_v2_graph_is_refused_with_the_recovery() {
    let dir = scratch("g01-valid-basic");
    fs::write(dir.0.join(".mdkg/graph.json"), r#"{"format_version": 2}"#).expect("graph.json");
    for args in [vec!["list", "--json"], vec!["show", "task-1", "--json"]] {
        let (code, out, _) = run(&dir.0, &args);
        assert_eq!(code, 4, "{out}");
        let d = &out["diagnostics"][0];
        assert_eq!(d["code"], "config.graph_v2_unsupported", "{out}");
        assert_eq!(d["path"], ".mdkg/graph.json", "{out}");
        assert_eq!(d["hint"], mdkg_graph::config::identity_v2_hint(), "{out}");
    }
    let (code, out, _) = run(&dir.0, &["validate", "--json"]);
    assert_eq!(code, 2, "{out}");
    let d = out["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == "config.graph_v2_unsupported")
        .expect("refused");
    assert_eq!(d["hint"], mdkg_graph::config::identity_v2_hint(), "{out}");
    assert_eq!(d["path"], ".mdkg/graph.json", "{out}");
}
