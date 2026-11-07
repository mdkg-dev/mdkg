//! Effective settings exercised through isolated public CLI calls and fake harnesses.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    graph: PathBuf,
}
impl Fixture {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("runner PATH"))
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("home/.config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false");
        for name in ["claude", "codex", "opencode"] {
            cmd.env(
                format!("MDKG_RUN_{}", name.to_uppercase()),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../mdkg-run/tests/fixtures")
                    .join(format!("fake-{name}.sh")),
            );
        }
        cmd
    }
    fn git(&self, cwd: &Path, args: &[&str]) {
        let out = self
            .command("git", cwd)
            .args(args)
            .output()
            .expect("fixture git executes");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    fn cli(&self, root: &Path, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_mdkg"), root)
            .args(["--as", "fixture"])
            .args(args)
            .arg("--json")
            .output()
            .expect("fixture CLI executes")
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(&self.graph, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).expect("fixture response JSON")
    }
    fn write(&self, path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("fixture parent directory");
        fs::write(path, value.to_string()).expect("write fixture JSON");
    }
    fn optional(&self, user: &Value, local: &Value) {
        self.write(&self.dir.join("home/.config/mdkg/config.json"), user);
        self.write(&self.repo.join(".git/mdkg/config.local.json"), local);
    }
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-agent-layers-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(dir.join("repo")).expect("fixture repository directory");
        fs::create_dir(dir.join("home")).expect("fixture home directory");
        let dir = fs::canonicalize(dir).expect("canonical fixture path");
        let f = Self {
            repo: dir.join("repo"),
            graph: dir.join("repo/.worktrees/graph"),
            dir,
        };
        f.git(&f.repo, &["init", "-q", "-b", "main"]);
        let out = f.cli(&f.repo, &["init", "--work", "local"]);
        assert!(out.status.success(), "{out:?}");
        fs::write(
            f.repo.join(".gitignore"),
            "*.args\n*.prompt\n*.environ\n/.worktrees/\n",
        )
        .expect("fixture ignore file");
        f.git(&f.repo, &["add", ".mdkg", ".gitignore"]);
        f.git(&f.repo, &["commit", "-qm", "fixture"]);
        let out = f.cli(&f.repo, &["install"]);
        assert!(out.status.success(), "{out:?}");
        f
    }
    fn turn(&self, name: &str) {
        let sent = self
            .command(env!("CARGO_BIN_EXE_mdkg"), &self.graph)
            .args([
                "--as",
                "sender",
                "msg",
                "send",
                "--to",
                name,
                "--kind",
                "note",
                "--text",
                "Layer fixture",
                "--json",
            ])
            .output()
            .expect("fixture CLI executes");
        assert!(sent.status.success(), "{sent:?}");
        let out = self.cli(&self.graph, &["agents", "up", name, "--once"]);
        assert!(out.status.success(), "{out:?}");
        let events: Vec<Value> = String::from_utf8(out.stdout)
            .expect("UTF-8 fixture output")
            .lines()
            .map(|s| serde_json::from_str(s).expect("fixture event JSON"))
            .collect();
        assert!(
            events
                .iter()
                .any(|e| e["data"]["turn"]["outcome"] == "replied"),
            "{events:?}"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn all_harnesses_apply_named_layers_and_report_the_same_values_and_origins() {
    for (harness, model, effort) in [
        ("codex", "gpt-6-astra", Some("ultra")),
        ("claude", "claude-haiku-5-5", Some("high")),
        ("opencode", "anthropic/claude-haiku-5-5", None),
    ] {
        let f = Fixture::new();
        f.ok(&[
            "agents",
            "add",
            "worker",
            "--harness",
            harness,
            "--role",
            "worker",
        ]);
        let definition = f.graph.join(".mdkg/agents/worker.json");
        let authored = fs::read(&definition).unwrap();
        let mut local = json!({"agents":{"worker":{"instructions":"Local standing instruction"}}});
        if let Some(effort) = effort {
            local["agents"]["worker"]["effort"] = json!(effort);
        }
        f.optional(&json!({"agents":{"worker":{"model":model}}}), &local);
        let show = f.ok(&["config", "show", "--effective"]);
        let status = f.ok(&["agents", "status"]);
        let effective = &status["data"]["agents"][0]["effective"];
        assert_eq!(
            effective["values"],
            show["data"]["values"]["agents"]["worker"]
        );
        assert_eq!(effective["values"]["model"], model);
        for key in ["harness", "model", "instructions"] {
            let pointer = format!("/agents/worker/{key}");
            assert_eq!(
                effective["origins"][&pointer],
                show["data"]["origins"][&pointer]
            );
        }
        assert_eq!(
            effective["origins"]["/agents/worker/model"]["layer"],
            "user"
        );
        assert_eq!(
            effective["origins"]["/agents/worker/instructions"]["layer"],
            "local"
        );
        assert!(
            !f.repo.join(".worktrees/worker").exists(),
            "status must not prepare a worker"
        );
        f.turn("worker");
        let cwd = f.repo.join(".worktrees/worker");
        let args = fs::read_to_string(cwd.join(format!("{harness}.args"))).unwrap();
        assert!(args.lines().any(|s| s == model), "{args}");
        if let Some(effort) = effort {
            let expected = if harness == "codex" {
                format!("model_reasoning_effort=\"{effort}\"")
            } else {
                effort.into()
            };
            assert!(args.lines().any(|s| s == expected), "{args}");
        }
        let prompt = if harness == "opencode" {
            args
        } else {
            fs::read_to_string(cwd.join(format!("{harness}.prompt"))).unwrap()
        };
        assert!(prompt.contains("Local standing instruction"), "{prompt}");
        let saved: Value =
            serde_json::from_slice(&fs::read(f.repo.join(".git/mdkg/run/worker.json")).unwrap())
                .unwrap();
        assert_eq!(saved["spec"]["model"], model);
        assert_eq!(saved["spec"]["effort"], json!(effort));
        assert_eq!(saved["spec"]["cwd"], json!(cwd));
        assert_eq!(fs::read(&definition).unwrap(), authored);
        // A new diagnostic capture sees changed optional settings without
        // overwriting saved execution state, and shares display redaction.
        local["agents"]["worker"]["model"] = json!("next-model");
        local["agents"]["worker"]["instructions"] =
            json!(format!("Authorization: Bearer {}", "sample".repeat(3)));
        f.optional(&json!({"agents":{"worker":{"model":model}}}), &local);
        let status = f.ok(&["agents", "status"]);
        let next = &status["data"]["agents"][0]["effective"];
        assert_eq!(next["values"]["model"], "next-model");
        assert_eq!(next["origins"]["/agents/worker/model"]["layer"], "local");
        assert!(
            next["redacted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "/agents/worker/instructions")
        );
        assert!(!status.to_string().contains(&"sample".repeat(3)));
        let text = f
            .command(env!("CARGO_BIN_EXE_mdkg"), &f.graph)
            .args(["agents", "status"])
            .output()
            .unwrap();
        assert!(text.status.success(), "{text:?}");
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(
            text.contains("effective:") && text.contains("next-model"),
            "{text}"
        );
        assert!(!text.contains(&"sample".repeat(3)));
        let still_saved: Value =
            serde_json::from_slice(&fs::read(f.repo.join(".git/mdkg/run/worker.json")).unwrap())
                .unwrap();
        assert_eq!(still_saved, saved);
        f.ok(&[
            "agents",
            "edit",
            "worker",
            "--instructions",
            "Authored edit",
        ]);
        let authored: Value = serde_json::from_slice(&fs::read(definition).unwrap()).unwrap();
        assert!(authored.get("model").is_none());
        assert!(authored.get("effort").is_none());
        assert_eq!(authored["instructions"], "Authored edit");
    }
}

#[test]
fn optional_only_agents_are_shown_as_inactive_and_not_admitted() {
    let f = Fixture::new();
    f.optional(
        &json!({"agents":{"from_user":{"role":"reviewer","harness":"codex"}}}),
        &json!({"agents":{"from_local":{"role":"worker","harness":"claude"}}}),
    );
    let show = f.ok(&["config", "show", "--effective"]);
    let warnings = show["data"]["warnings"].as_array().unwrap();
    for name in ["from_user", "from_local"] {
        assert!(warnings.iter().any(|warning| {
            warning.as_str().is_some_and(|text| {
                text.contains(name)
                    && text.contains("inactive")
                    && text.contains("no graph definition")
            })
        }));
    }
    let status = f.ok(&["agents", "status"]);
    assert!(status["data"]["agents"].as_array().unwrap().is_empty());
}

/// `agents list` names each agent with the layer that defines it and every
/// layer that sets its fields, for each harness, and reads no state.
#[test]
fn agents_list_names_each_agent_with_its_layers() {
    for harness in ["codex", "claude", "opencode"] {
        let f = Fixture::new();
        f.ok(&[
            "agents",
            "add",
            "worker",
            "--harness",
            harness,
            "--role",
            "worker",
        ]);
        // The same field in several layers: each layer still counts, though
        // only the highest one's value wins (Terra #20933).
        f.optional(
            &json!({"agents":{"from_user":{"role":"reviewer","harness":"codex"},
                              "worker":{"role":"worker"}}}),
            &json!({"agents":{"worker":{"instructions":"Local standing instruction"},
                              "from_user":{"role":"worker"},
                              "from_local":{"role":"worker","harness":"claude"}}}),
        );
        let list = f.ok(&["agents", "list"]);
        let agents = list["data"]["agents"].as_array().unwrap();
        let entry = |name: &str| {
            agents
                .iter()
                .find(|a| a["name"] == name)
                .unwrap_or_else(|| panic!("{name} listed: {list}"))
        };
        let worker = entry("worker");
        assert_eq!(worker["defined_in"], "project", "{list}");
        assert_eq!(worker["file_backed"], true);
        assert_eq!(
            worker["layers"],
            json!(["user", "project", "local"]),
            "{list}"
        );
        assert_eq!(worker["harness"], harness);
        assert_eq!(worker["role"], "worker");
        let from_user = entry("from_user");
        assert_eq!(from_user["defined_in"], "user", "{list}");
        assert_eq!(from_user["file_backed"], false);
        assert_eq!(from_user["layers"], json!(["user", "local"]), "{list}");
        let from_local = entry("from_local");
        assert_eq!(from_local["defined_in"], "local", "{list}");
        assert_eq!(from_local["layers"], json!(["local"]));
        assert_eq!(agents.len(), 3, "{list}");
        // Text names the same agents.
        let out = f
            .command(env!("CARGO_BIN_EXE_mdkg"), &f.graph)
            .args(["--as", "fixture", "agents", "list"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains("worker: defined in project (its own file)"),
            "{text}"
        );
        // An uncommitted, malformed checkout config doesn't stop it: it
        // reads the committed settings, as `config show` does.
        let config = f.graph.join(".mdkg/config.json");
        let committed = fs::read(&config).unwrap();
        fs::write(&config, "{").unwrap();
        let again = f.ok(&["agents", "list"]);
        assert_eq!(again["data"]["agents"], list["data"]["agents"]);
        fs::write(&config, committed).unwrap();
    }
    // With no project, the user layer's agents are still listed.
    let f = Fixture::new();
    f.optional(
        &json!({"agents":{"from_user":{"role":"reviewer"}}}),
        &json!({}),
    );
    let bare = f.dir.join("bare");
    fs::create_dir_all(&bare).unwrap();
    let out = f.cli(&bare, &["agents", "list"]);
    assert!(out.status.success(), "{out:?}");
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list["data"]["agents"][0]["name"], "from_user", "{list}");
    assert_eq!(list["data"]["agents"][0]["defined_in"], "user");
}

/// Credential-shaped names are listed under distinct numbered names, never
/// raw, and never the name of a real agent already called like a label.
#[test]
fn agents_list_numbers_hidden_credential_shaped_names_apart() {
    let f = Fixture::new();
    let first = concat!("https://someone:SYNTHETIC_FIRST", "@example.test/a");
    let second = concat!("https://someone:SYNTHETIC_SECOND", "@example.test/b");
    // A real agent already named like a label, too (Terra #20956).
    f.optional(
        &json!({"agents":{first:{"role":"reviewer"}, "[redacted key 1]":{"role":"lead"}}}),
        &json!({"agents":{second:{"role":"worker"}}}),
    );
    let out = f.cli(&f.graph, &["agents", "list"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{out:?}");
    assert!(!text.contains("SYNTHETIC_"), "{text}");
    let list: Value = serde_json::from_str(&text).unwrap();
    let agents = list["data"]["agents"].as_array().unwrap();
    let mut names: Vec<&str> = agents.iter().map(|a| a["name"].as_str().unwrap()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 3, "{list}");
    assert!(
        names.iter().all(|n| n.starts_with("[redacted key ")),
        "{list}"
    );
    // The real one keeps its own entry; the hidden ones keep theirs.
    let real = agents
        .iter()
        .find(|a| a["name"] == "[redacted key 1]")
        .unwrap();
    assert_eq!(real["role"], "lead", "{list}");
    assert_eq!(real["defined_in"], "user");
    let local = agents.iter().find(|a| a["defined_in"] == "local").unwrap();
    assert_eq!(local["role"], "[redacted]", "{list}");
    assert_ne!(local["name"], "[redacted key 1]", "{list}");
}

/// Two credential-shaped names the shown tree lacks (a local `agents: null`
/// hides them) are listed under distinct numbered names, never raw.
#[test]
fn agents_list_numbers_names_a_local_null_hides_apart() {
    let f = Fixture::new();
    let first = concat!("https://someone:SYNTHETIC_FIRST", "@example.test/a");
    let second = concat!("https://someone:SYNTHETIC_SECOND", "@example.test/b");
    f.optional(
        &json!({"agents":{first:{"role":"reviewer"}, second:{"role":"worker"}}}),
        &json!({"agents": null}),
    );
    let out = f.cli(&f.graph, &["agents", "list"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{out:?}");
    assert!(!text.contains("SYNTHETIC_"), "{text}");
    let list: Value = serde_json::from_str(&text).unwrap();
    let mut names: Vec<&str> = list["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{list}");
    assert_ne!(names[0], names[1], "{list}");
    assert!(
        names.iter().all(|n| n.starts_with("[redacted key ")),
        "{list}"
    );
}

/// Terra #20956/#20965: a project `agents: null` hides the user's agent
/// named `[redacted key 1]`, and a local credential-shaped name recreates
/// the map. The labels stay distinct and the hidden user entry keeps its own
/// role (none, since it's masked), never the local entry's.
#[test]
fn agents_list_keeps_a_hidden_label_shaped_name_apart_from_a_local_secret() {
    let f = Fixture::new();
    let path = f.graph.join(".mdkg/config.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    project["agents"] = Value::Null;
    f.write(&path, &project);
    f.git(&f.graph, &["add", ".mdkg/config.json"]);
    f.git(&f.graph, &["commit", "-qm", "project agents null"]);
    let secret = concat!("https://someone:SYNTHETIC_LOCAL", "@example.test/c");
    f.optional(
        &json!({"agents":{"[redacted key 1]":{"role":"lead"}}}),
        &json!({"agents":{secret:{"role":"worker"}}}),
    );
    let out = f.cli(&f.graph, &["agents", "list"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{out:?}");
    assert!(!text.contains("SYNTHETIC_"), "{text}");
    let list: Value = serde_json::from_str(&text).unwrap();
    let agents = list["data"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2, "{list}");
    assert_ne!(agents[0]["name"], agents[1]["name"], "{list}");
    let user = agents.iter().find(|a| a["defined_in"] == "user").unwrap();
    assert_eq!(user["name"], "[redacted key 1]", "{list}");
    // Masked by the project's null, it shows no role, and not the local one.
    assert!(user.get("role").is_none(), "{list}");
    assert_eq!(user["layers"], json!(["user"]), "{list}");
    let local = agents.iter().find(|a| a["defined_in"] == "local").unwrap();
    assert_ne!(local["name"], "[redacted key 1]", "{list}");
    assert_eq!(local["role"], "[redacted]", "{list}");
}

/// A `null` removes an agent rather than defining it (Opus #21026): the
/// layer that sets it isn't in `layers`, the agent it masks is listed with
/// no harness or role, and a name only a `null` gives isn't listed.
#[test]
fn agents_list_does_not_count_a_null_entry_as_a_layer() {
    let f = Fixture::new();
    f.optional(
        &json!({"agents":{"from_user":{"role":"reviewer"}}}),
        &json!({"agents":{"from_user": null, "ghost": null}}),
    );
    let list = f.ok(&["agents", "list"]);
    let agents = list["data"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{list}");
    let entry = &agents[0];
    assert_eq!(entry["name"], "from_user", "{list}");
    assert_eq!(entry["defined_in"], "user", "{list}");
    assert_eq!(entry["layers"], json!(["user"]), "{list}");
    assert!(entry.get("role").is_none(), "{list}");
    assert!(entry.get("harness").is_none(), "{list}");
    // The same for a committed project entry that is `null` (Opus #21038):
    // alone it lists nothing, and masking a user agent it adds no layer.
    let f = Fixture::new();
    let path = f.graph.join(".mdkg/config.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    project["agents"] = json!({"ghost": null, "from_user": null});
    f.write(&path, &project);
    f.git(&f.graph, &["add", ".mdkg/config.json"]);
    f.git(&f.graph, &["commit", "-qm", "project null entries"]);
    // The user entry sets no committed field: a role over the project's
    // `null` is an authority override, refused as before.
    f.optional(&json!({"agents":{"from_user":{}}}), &json!({}));
    let list = f.ok(&["agents", "list"]);
    let agents = list["data"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{list}");
    assert_eq!(agents[0]["name"], "from_user", "{list}");
    assert_eq!(agents[0]["defined_in"], "user", "{list}");
    assert_eq!(agents[0]["layers"], json!(["user"]), "{list}");
    assert!(agents[0].get("role").is_none(), "{list}");
}

#[test]
fn legacy_agent_ignores_optional_values_until_migrated_and_management_keeps_authored_values() {
    let f = Fixture::new();
    let path = f.graph.join(".mdkg/config.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    project["agents"] =
        json!({"old":{"harness":"codex","role":"worker","model":"legacy-model","effort":"low"}});
    f.write(&path, &project);
    f.git(&f.graph, &["add", ".mdkg/config.json"]);
    f.git(&f.graph, &["commit", "-qm", "legacy fixture"]);
    f.optional(
        &json!({"agents":{"old":{"model":"user-model"},"defaults":{"turn":{"timeout_secs":1}},"optional_only":{"harness":"codex"}}}),
        &json!({"agents":{"old":{"model":"gpt-6-astra","effort":"ultra"},"defaults":{"turn":{"timeout_secs":2}}}}),
    );
    let status = f.ok(&["agents", "status"]);
    assert_eq!(status["data"]["agents"].as_array().unwrap().len(), 1);
    assert!(status["data"]["agents"][0].get("effective").is_none());
    f.turn("old");
    let saved = f.repo.join(".git/mdkg/run/old.json");
    let state: Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
    assert_eq!(state["spec"]["model"], "legacy-model");
    assert_eq!(state["spec"]["effort"], "low");
    // Migration copies only project-authored values. Removing those physical
    // model/effort fields then lets the same optional values take effect.
    f.ok(&["agents", "edit", "old", "--instructions", "Migrated"]);
    let definition = f.graph.join(".mdkg/agents/old.json");
    let authored: Value = serde_json::from_slice(&fs::read(&definition).unwrap()).unwrap();
    assert_eq!(authored["model"], "legacy-model");
    assert_eq!(authored["effort"], "low");
    f.ok(&[
        "agents", "edit", "old", "--unset", "model", "--unset", "effort",
    ]);
    let status = f.ok(&["agents", "status"]);
    assert_eq!(
        status["data"]["agents"][0]["effective"]["values"]["model"],
        "gpt-6-astra"
    );
    assert_eq!(
        status["data"]["agents"][0]["effective"]["values"]["effort"],
        "ultra"
    );
    f.turn("old");
    let state: Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
    assert_eq!(state["spec"]["model"], "gpt-6-astra");
    assert_eq!(state["spec"]["effort"], "ultra");
}

#[test]
fn status_redacts_duplicate_policy_and_allow_diagnostics_in_both_formats() {
    let f = Fixture::new();
    f.ok(&[
        "agents",
        "add",
        "worker",
        "--harness",
        "codex",
        "--role",
        "worker",
    ]);
    let credential = format!("Bearer {}", "sample".repeat(3));
    f.optional(
        &json!({}),
        &json!({"agents":{"worker":{
            "policy":{"trigger":{"kind":"schedule","authorization":credential}},
            "allow":[format!("Bash(mdkg --token={} *)", "sample".repeat(3))]
        }}}),
    );
    let status = f.ok(&["agents", "status"]);
    let agent = &status["data"]["agents"][0];
    assert_eq!(agent["policy"]["trigger"]["kind"], "later");
    assert!(
        agent["dropped_allow"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "{status}"
    );
    assert!(
        !status.to_string().contains(&"sample".repeat(3)),
        "{status}"
    );
    let text = f
        .command(env!("CARGO_BIN_EXE_mdkg"), &f.graph)
        .args(["agents", "status"])
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    assert!(!String::from_utf8_lossy(&text.stdout).contains(&"sample".repeat(3)));
}

#[test]
fn invalid_optional_definitions_redact_errors_in_both_formats() {
    let f = Fixture::new();
    f.ok(&[
        "agents",
        "add",
        "worker",
        "--harness",
        "codex",
        "--role",
        "worker",
    ]);
    let definition = f.graph.join(".mdkg/agents/worker.json");
    let authored = fs::read(&definition).unwrap();
    let credential = format!("sk-{}", "example".repeat(5));
    for (layer, field) in [
        ("local", "harness"),
        ("local", "role"),
        ("local", "effort"),
        ("user", "effort"),
    ] {
        let mut value = json!({"agents":{"worker":{}}});
        value["agents"]["worker"][field] = json!(credential);
        let optional = if layer == "user" {
            f.optional(&value, &json!({}));
            f.dir.join("home/.config/mdkg/config.json")
        } else {
            f.optional(&json!({}), &value);
            f.repo.join(".git/mdkg/config.local.json")
        };
        let before = fs::read(&optional).unwrap();
        for args in [
            vec!["agents", "status"],
            vec!["agents", "up", "worker", "--once"],
        ] {
            for json in [false, true] {
                let mut cmd = f.command(env!("CARGO_BIN_EXE_mdkg"), &f.graph);
                cmd.args(&args);
                if json {
                    cmd.arg("--json");
                }
                let out = cmd.output().unwrap();
                assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    !stdout.contains(&credential),
                    "{args:?}: stdout exposed credential"
                );
                assert!(
                    !stderr.contains(&credential),
                    "{args:?}: stderr exposed credential"
                );
                if field == "role" {
                    assert!(
                        stdout.contains("changes committed role")
                            || stderr.contains("changes committed role")
                    );
                } else {
                    assert!(
                        stdout.contains("[redacted]") || stderr.contains("[redacted]"),
                        "{out:?}"
                    );
                    assert!(
                        stdout.contains("config show --effective")
                            || stderr.contains("config show --effective")
                    );
                }
                if json {
                    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
                    assert_eq!(report["ok"], false);
                    assert_eq!(
                        report["diagnostics"][0]["code"],
                        if field == "role" {
                            "run.authority_override"
                        } else {
                            "usage.invalid_arguments"
                        }
                    );
                }
            }
        }
        assert_eq!(fs::read(&definition).unwrap(), authored);
        assert_eq!(fs::read(optional).unwrap(), before);
        assert!(!f.repo.join(".git/mdkg/run/worker.json").exists());
        assert!(!f.repo.join(".worktrees/worker").exists());
    }
    f.optional(
        &json!({}),
        &json!({"agents":{"worker":{"harness":"unknown-harness"}}}),
    );
    let out = f.cli(&f.graph, &["agents", "status"]);
    assert_eq!(out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let message = report["diagnostics"][0]["message"].as_str().unwrap();
    assert!(message.contains("unknown variant"), "{message}");
    assert!(message.contains("unknown-harness"), "{message}");

    // A policy validation error keeps its non-secret repair hint.
    f.optional(
        &json!({}),
        &json!({"agents":{"worker":{"policy":{"trigger":credential}}}}),
    );
    for json in [false, true] {
        let mut cmd = f.command(env!("CARGO_BIN_EXE_mdkg"), &f.graph);
        cmd.args(["agents", "up", "worker", "--once"]);
        if json {
            cmd.arg("--json");
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains(&credential),
            "policy stdout exposed credential"
        );
        assert!(
            !stderr.contains(&credential),
            "policy stderr exposed credential"
        );
        assert!(stdout.contains("[redacted]") || stderr.contains("[redacted]"));
        assert!(stdout.contains("docs/agents.md") || stderr.contains("docs/agents.md"));
    }
    assert_eq!(fs::read(&definition).unwrap(), authored);
    assert!(!f.repo.join(".git/mdkg/run/worker.json").exists());
    assert!(!f.repo.join(".worktrees/worker").exists());
}

#[test]
fn optional_naming_authority_overrides_are_refused_without_leaking_values() {
    let f = Fixture::new();
    f.ok(&[
        "agents",
        "add",
        "worker",
        "--harness",
        "codex",
        "--role",
        "worker",
    ]);
    let credential = format!("sk-{}", "example".repeat(5));
    let definition = f.graph.join(".mdkg/agents/worker.json");
    let authored = fs::read(&definition).unwrap();
    for field in ["branch", "worktree"] {
        let local = json!({"orchestration":{field:credential}});
        f.optional(&json!({}), &local);
        for args in [
            vec!["agents", "status"],
            vec!["agents", "up", "worker", "--once"],
            vec!["config", "show", "--effective"],
        ] {
            for json in [false, true] {
                let mut cmd = f.command(env!("CARGO_BIN_EXE_mdkg"), &f.graph);
                cmd.args(&args);
                if json {
                    cmd.arg("--json");
                }
                let out = cmd.output().unwrap();
                assert_eq!(out.status.code(), Some(1));
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    !stdout.contains(&credential),
                    "{field}: stdout exposed credential"
                );
                assert!(
                    !stderr.contains(&credential),
                    "{field}: stderr exposed credential"
                );
                assert!(
                    stdout.contains("changes committed") || stderr.contains("changes committed")
                );
                if json {
                    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
                    assert_eq!(report["ok"], false);
                    assert_eq!(report["diagnostics"][0]["code"], "run.authority_override");
                }
            }
        }
        let saved: Value =
            serde_json::from_slice(&fs::read(f.repo.join(".git/mdkg/config.local.json")).unwrap())
                .unwrap();
        assert_eq!(saved, local);
        assert_eq!(fs::read(&definition).unwrap(), authored);
        assert!(!f.repo.join(".git/mdkg/run/worker.json").exists());
        assert!(!f.repo.join(".worktrees/worker").exists());
    }
}

#[test]
fn late_turn_diagnostics_are_redacted_in_both_formats() {
    let f = Fixture::new();
    f.ok(&[
        "agents",
        "add",
        "worker",
        "--harness",
        "opencode",
        "--role",
        "worker",
    ]);
    let credential = format!("sk-{}", "example".repeat(5));
    f.optional(
        &json!({}),
        &json!({"agents":{"worker":{"model":format!("opencode/{credential}")}}}),
    );
    f.ok(&["agents", "status"]);
    for json in [false, true] {
        let mut cmd = f.command(env!("CARGO_BIN_EXE_mdkg"), &f.graph);
        cmd.args([
            "agents",
            "up",
            "worker",
            "--once",
            "--dry-run",
            "--prompt",
            "Fixture turn",
        ]);
        if json {
            cmd.arg("--json");
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stdout.contains(&credential));
        assert!(!stderr.contains(&credential));
        assert!(stdout.contains("[redacted]") || stderr.contains("[redacted]"));
        assert!(
            stdout.contains("config show --effective")
                || stderr.contains("config show --effective")
        );
        if json {
            let report: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(report["ok"], false);
            assert_eq!(report["data"]["event"], "turn");
        }
    }
    assert!(!f.repo.join(".git/mdkg/run/worker.json").exists());
    assert!(!f.repo.join(".worktrees/worker").exists());
}

#[test]
fn completed_turns_redact_dropped_rule_warnings_in_both_formats() {
    let f = Fixture::new();
    f.ok(&[
        "agents",
        "add",
        "worker",
        "--harness",
        "codex",
        "--role",
        "worker",
    ]);
    let credential = format!("sk-{}", "example".repeat(5));
    f.optional(
        &json!({}),
        &json!({"agents":{"worker":{"allow":[format!("Bash(mdkg --token={credential} *)")]}}}),
    );
    let source = f.repo.join(".git/mdkg/config.local.json");
    let before = fs::read(&source).unwrap();
    for json in [false, true] {
        let mut cmd = f.command(env!("CARGO_BIN_EXE_mdkg"), &f.graph);
        cmd.args([
            "agents",
            "up",
            "worker",
            "--once",
            "--prompt",
            "Fixture turn",
        ]);
        if json {
            cmd.arg("--json");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stdout.contains(&credential));
        assert!(!stderr.contains(&credential));
        assert!(stdout.contains("[redacted]") || stderr.contains("[redacted]"));
        if json {
            let report: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                report["data"]["turn"]["dropped_allow"][0]["rule"],
                "[redacted]"
            );
        }
    }
    assert_eq!(fs::read(source).unwrap(), before);
}
