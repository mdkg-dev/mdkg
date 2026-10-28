//! Configuration inspection uses disposable homes and handwritten Git
//! metadata. No Git subprocess, coordinator or user configuration is used.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-config-cli-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(dir.join("project")).expect("create fixture");
        let dir = fs::canonicalize(dir).expect("canonical fixture");
        Self {
            root: dir.join("project"),
            home: dir.join("home"),
            dir,
        }
    }
    fn write(&self, path: impl AsRef<Path>, value: &Value) -> PathBuf {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create parents");
        fs::write(&path, value.to_string()).expect("write fixture JSON");
        path
    }
    fn project(&self) -> PathBuf {
        for dir in [".git/objects", ".git/refs"] {
            fs::create_dir_all(self.root.join(dir)).expect("create Git metadata");
        }
        fs::write(self.root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("write unborn HEAD");
        fs::write(self.root.join(".git/config"), "[core]\n bare=false\n")
            .expect("write Git config");
        self.write(
            "project/.mdkg/config.json",
            &json!({
                "schema_version":3, "format_version":"0.7",
                "graph_id":"0192f3a0-0000-7000-8000-000000000000"
            }),
        )
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        command
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            // Inspection must not consult Git executables or actor/store state.
            .env("PATH", "/mdkg-fixture-no-executables")
            .env("MDKG_ACTOR", "human")
            .args(args);
        command
    }
    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("CLI runs")
    }
    fn show(&self) -> Value {
        let out = self.output(&["config", "show", "--effective", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice(&out.stdout).expect("parse response")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).expect("remove fixture");
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).expect("list snapshot directory") {
            let entry = entry.expect("snapshot entry");
            if entry.file_type().expect("snapshot entry type").is_dir() {
                visit(root, &entry.path(), files);
            } else {
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("path below fixture root")
                        .into(),
                    fs::read(entry.path()).expect("read snapshot file"),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn show_reports_each_layer_and_definition_source_without_writes() {
    let f = Fixture::new();
    let project = f.project();
    let mut value: Value = serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
    value["work"] = json!({"priority_min":2,"location":"local"});
    value["agents"] = json!({"astra":{"harness":"codex","model":"inline","effort":"high"}});
    fs::write(&project, value.to_string()).unwrap();
    let user = f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "index":{"tolerant":true}, "work":{"priority_min":1},
            "agents":{"astra":{"model":"user"}}
        }),
    );
    let definition = f.write(
        "project/.mdkg/agents/astra.json",
        &json!({
            "schema":"mdkg.agent.v1", "role":"worker", "model":"gpt-6-astra", "effort":"ultra"
        }),
    );
    let local = f.write(
        "project/.git/mdkg/config.local.json",
        &json!({
            "work":{"priority_min":4}, "agents":{"astra":{"effort":"max"}}
        }),
    );
    let before = snapshot(&f.dir);
    let result = f.show();
    let data = &result["data"];
    assert_eq!(result["schema"], "mdkg.config.show.v1");
    assert_eq!(data["defaults_version"], "0.7.0");
    assert_eq!(data["values"]["work"]["priority_min"], 4);
    for (key, layer, path) in [
        ("/index/tolerant", "user", Some(user)),
        ("/graph_id", "project", Some(project)),
        ("/agents/astra/model", "project", Some(definition)),
        ("/agents/astra/effort", "local", Some(local)),
        ("/work/priority_max", "built_in", None),
    ] {
        assert_eq!(data["origins"][key]["layer"], layer, "{key}");
        assert_eq!(
            data["origins"][key].get("path").cloned(),
            path.map(|p| json!(p))
        );
    }
    assert!(data["values"]["agents"]["astra"].get("harness").is_none());
    assert!(data["origins"].get("/agents/astra/harness").is_none());
    assert!(
        data["values"]["agents"]["astra"].get("mode").is_none(),
        "no inserted serde defaults"
    );
    assert_eq!(snapshot(&f.dir), before);
    assert!(!f.root.join(".git/mdkg/coord.sqlite").exists());
    let plain: Value =
        serde_json::from_slice(&f.output(&["config", "show", "--json"]).stdout).unwrap();
    assert_eq!(result, plain);
}

#[test]
fn definition_replaces_inline_only_while_user_and_local_values_keep_their_origins() {
    let f = Fixture::new();
    let project = f.project();
    let mut value: Value = serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
    value["agents"] = json!({
        "astra":{"harness":"codex","role":"worker","effort":"ultra","model":"inline"},
        "keep":{"model":"inline-kept"}, "defaults":{"turn":{"max_turns":3}}
    });
    fs::write(&project, value.to_string()).unwrap();
    let user = f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "agents":{"astra":{"model":"user-model"}}
        }),
    );
    let definition = f.write(
        "project/.mdkg/agents/astra.json",
        &json!({
            "schema":"mdkg.agent.v1", "harness":"opencode", "role":"worker"
        }),
    );
    let local = f.write(
        "project/.git/mdkg/config.local.json",
        &json!({
            "agents":{"astra":{"instructions":"local instructions"}}
        }),
    );
    let before = snapshot(&f.dir);
    let result = f.show();
    let data = &result["data"];
    assert_eq!(
        data["values"]["agents"]["astra"],
        json!({
            "schema":"mdkg.agent.v1", "harness":"opencode", "role":"worker",
            "model":"user-model", "instructions":"local instructions"
        })
    );
    assert_eq!(data["values"]["agents"]["keep"]["model"], "inline-kept");
    assert_eq!(data["values"]["agents"]["defaults"]["turn"]["max_turns"], 3);
    for (key, path) in [
        ("/agents/astra/harness", definition),
        ("/agents/astra/model", user),
        ("/agents/astra/instructions", local),
        ("/agents/keep/model", project),
    ] {
        assert_eq!(data["origins"][key]["path"], json!(path));
    }
    assert!(data["origins"].get("/agents/astra/effort").is_none());
    let notice = "settings (agents, orchestration, projections) from user or machine-local layers are displayed here but are not yet applied by runtime commands in this build";
    assert!(
        data["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == notice)
    );
    let text = f.output(&["config", "show", "--effective"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stderr).contains(notice));
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn show_warns_when_a_local_layer_replaces_the_whole_agents_value() {
    let f = Fixture::new();
    f.project();
    f.write(
        "project/.mdkg/agents/astra.json",
        &json!({
            "schema":"mdkg.agent.v1", "harness":"codex", "role":"worker"
        }),
    );
    for agents in [Value::Null, json!([]), json!("invalid")] {
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":agents}),
        );
        let before = snapshot(&f.dir);
        let result = f.show();
        assert_eq!(result["data"]["values"]["agents"], agents);
        assert_eq!(result["data"]["origins"]["/agents"]["layer"], "local");
        assert!(
            result["data"]["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v
                    .as_str()
                    .is_some_and(|s| s.contains("not yet applied by runtime commands")))
        );
        assert_eq!(snapshot(&f.dir), before);
    }
}

#[test]
fn show_warns_about_unapplied_orchestration_and_projection_layers() {
    for (source, layer) in [
        ("home/.config/mdkg/config.json", "user"),
        ("project/.git/mdkg/config.local.json", "local"),
    ] {
        for (patch, pointer, expected) in [
            (
                json!({"orchestration":{"branch":"worker/{agent}"}}),
                "/orchestration/branch",
                json!("worker/{agent}"),
            ),
            (
                json!({"orchestration":{"worktree":"../agents/{agent}"}}),
                "/orchestration/worktree",
                json!("../agents/{agent}"),
            ),
            (json!({"orchestration":null}), "/orchestration", Value::Null),
            (
                json!({"projections":["hooks:codex"]}),
                "/projections/0",
                json!("hooks:codex"),
            ),
            (json!({"projections":[]}), "/projections", json!([])),
        ] {
            let f = Fixture::new();
            f.project();
            f.write(source, &patch);
            let before = snapshot(&f.dir);
            let result = f.show();
            let data = &result["data"];
            assert_eq!(data["values"].pointer(pointer), Some(&expected));
            assert_eq!(data["origins"][pointer]["layer"], layer);
            let warning = data["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .find(|text| text.contains("not yet applied by runtime commands"))
                .expect("unapplied settings must be called out for all runtime commands");
            let text = f.output(&["config", "show", "--effective"]);
            assert!(text.status.success());
            assert!(String::from_utf8_lossy(&text.stderr).contains(warning));
            assert_eq!(snapshot(&f.dir), before);
        }
    }
}

#[test]
fn show_does_not_warn_about_launcher_overrides_when_only_authored_values_apply() {
    let f = Fixture::new();
    let project = f.project();
    let mut config: Value = serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
    config["orchestration"] = json!({"branch":"agent/{agent}","worktree":".worktrees/{agent}"});
    config["projections"] = json!([]);
    fs::write(project, config.to_string()).unwrap();
    f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "agents":{"astra":{"model":"hidden-user"}}, "index":{"tolerant":true},
            "orchestration":{"branch":"hidden/{agent}","worktree":"hidden/{agent}"},
            "projections":["hooks:codex"],
            "orchestration_extra":{"branch":"unrelated/{agent}"}
        }),
    );
    f.write(
        "project/.mdkg/agents/astra.json",
        &json!({
            "schema":"mdkg.agent.v1", "harness":"codex", "role":"worker", "model":"authored"
        }),
    );
    f.write(
        "project/.git/mdkg/config.local.json",
        &json!({"work":{"priority_min":3}}),
    );
    let result = f.show();
    assert_eq!(
        result["data"]["values"]["agents"]["astra"]["model"],
        "authored"
    );
    assert!(result["data"]["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn graphless_show_reads_only_the_selected_user_file_and_normalizes_aliases() {
    let f = Fixture::new();
    f.project();
    f.write(
        "project/.git/mdkg/config.local.json",
        &json!({"work":{"priority_min":8}}),
    );
    f.write(
        "home/.config/mdkg/config.json",
        &json!({"work":{"committed":"none","priority_min":3}}),
    );
    let child = f.root.join("child");
    fs::create_dir(&child).unwrap();
    let mut command = f.command(&["config", "show", "--json"]);
    let out = command.current_dir(&child).output().unwrap();
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["data"]["values"]["work"]["location"], "local");
    assert!(result["data"]["values"]["work"].get("committed").is_none());
    assert_eq!(result["data"]["values"]["work"]["priority_min"], 3);
    assert!(result["data"]["sources"]["project"].is_null());
    assert!(result["data"]["sources"]["local"].is_null());
    assert!(
        result["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| {
                let text = v.as_str().unwrap();
                text.contains("showing user-only settings")
                    && text.contains("pass --root")
                    && text.contains(f.root.to_str().unwrap())
            })
    );
    // Existence checking does not parse the parent's project source.
    fs::write(f.root.join(".mdkg/config.json"), "broken parent source").unwrap();
    assert!(command.output().unwrap().status.success());
    f.write(
        "home/.config/mdkg/config.json",
        &json!({"work":{"committed":"none","location":"git"}}),
    );
    let refused = command.output().unwrap();
    assert!(!refused.status.success(), "conflicting aliases");
}

#[test]
fn xdg_selection_does_not_read_an_unselected_home_or_fall_back() {
    let f = Fixture::new();
    let user = f.write("home/.config/mdkg/config.json", &json!({}));
    fs::write(user, "malformed HOME settings").unwrap();
    let xdg = f.dir.join("xdg");
    let mut command = f.command(&["config", "show", "--json"]);
    let out = command.env("XDG_CONFIG_HOME", &xdg).output().unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["data"]["sources"]["user"],
        json!(xdg.join("mdkg/config.json"))
    );
    assert_eq!(
        value["data"]["origins"]["/index/tolerant"]["layer"],
        "built_in"
    );
    assert!(!xdg.exists());
    command.env_remove("XDG_CONFIG_HOME").env_remove("HOME");
    assert!(
        command.output().unwrap().status.success(),
        "unset HOME means no user layer"
    );
    command.env("HOME", "");
    assert!(
        !command.output().unwrap().status.success(),
        "empty HOME is invalid"
    );
}

#[test]
fn show_warns_about_conflicting_git_environment_without_selecting_it() {
    let f = Fixture::new();
    f.project();
    let other = f.dir.join("other");
    for dir in ["objects", "refs"] {
        fs::create_dir_all(other.join(dir)).unwrap();
    }
    fs::write(other.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(other.join("config"), "[core]\n bare=false\n").unwrap();
    f.write(
        "project/.git/mdkg/config.local.json",
        &json!({"work":{"priority_min":4}}),
    );
    f.write(
        "other/mdkg/config.local.json",
        &json!({"work":{"priority_min":8}}),
    );
    let out = f
        .command(&["config", "show", "--json"])
        .env("GIT_DIR", &other)
        .output()
        .unwrap();
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["data"]["values"]["work"]["priority_min"], 4);
    assert!(
        result["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w
                .as_str()
                .unwrap()
                .contains("GIT_DIR points elsewhere; mdkg uses"))
    );
}

#[test]
fn project_identity_is_checked_before_optional_sources() {
    let f = Fixture::new();
    let project = f.project();
    fs::write(project, r#"{"schema_version":999}"#).unwrap();
    let user = f.write("home/.config/mdkg/config.json", &json!({}));
    fs::write(user, "broken user source").unwrap();
    let out = f.output(&["config", "show", "--json"]);
    assert!(!out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        result["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("schema_version")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("broken user source"));
}

#[test]
fn malformed_duplicate_large_and_linked_sources_never_disappear_from_the_view() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let config = f.project();
    for bytes in [
        b"not json".to_vec(),
        br#"{"work":{},"work":{}}"#.to_vec(),
        vec![b' '; (1 << 20) + 1],
    ] {
        fs::write(&config, bytes).unwrap();
        assert!(!f.output(&["config", "show", "--json"]).status.success());
    }
    fs::remove_file(&config).unwrap();
    assert!(
        !f.output(&["config", "show", "--json"]).status.success(),
        "partial project"
    );
    fs::create_dir(&config).unwrap();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
    fs::remove_dir(&config).unwrap();
    symlink(f.dir.join("absent"), &config).unwrap();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
    fs::remove_file(&config).unwrap();
    fs::remove_dir(f.root.join(".mdkg")).unwrap();
    symlink(f.dir.join("absent"), f.root.join(".mdkg")).unwrap();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
}

#[test]
fn definition_inputs_are_bounded_strict_and_visible_without_typed_defaults() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.project();
    let definition = f.write("project/.mdkg/agents/astra.json", &json!({"effort":false}));
    // The read-only inspector may display a wrong-typed value for diagnosis.
    assert_eq!(
        f.show()["data"]["values"]["agents"]["astra"]["effort"],
        false
    );
    for bytes in [
        br#"{"model":"a","model":"b"}"#.to_vec(),
        vec![b' '; (1 << 20) + 1],
    ] {
        fs::write(&definition, bytes).unwrap();
        assert!(!f.output(&["config", "show", "--json"]).status.success());
    }
    fs::remove_file(&definition).unwrap();
    symlink(f.dir.join("absent"), &definition).unwrap();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
}

#[test]
fn human_output_escapes_source_control_characters_and_parse_errors_use_the_schema() {
    let f = Fixture::new();
    let missing = f.dir.join("missing-root");
    assert!(
        !f.output(&[
            "--root",
            missing.to_str().unwrap(),
            "config",
            "show",
            "--json"
        ])
        .status
        .success()
    );
    f.write(
        "home/.config/mdkg/config.json",
        &json!({"x\u{1b}[31m\u{202e}":"line\nnext\u{200b}\u{85}"}),
    );
    let out = f.output(&["config", "show"]);
    assert!(out.status.success());
    assert!(!out.stdout.contains(&0x1b));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.is_ascii(), "{text:?}");
    assert!(text.contains("\\u{202e}"));
    assert!(text.contains("\\u{200b}"));
    assert!(text.contains("\\u{85}"));
    assert!(text.contains("defaults version: 0.7.0"), "{text}");
    assert!(text.contains("sources: {\"user\":"), "{text}");
    assert!(text.contains("home/.config/mdkg/config.json"), "{text}");
    let out = f.output(&["config", "show", "--unknown", "--json"]);
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!out.status.success());
    assert_eq!(result["schema"], "mdkg.config.show.v1");
}

#[test]
fn display_redacts_credential_leaves_in_both_formats_without_changing_files() {
    let f = Fixture::new();
    f.project();
    f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "service": {"api_token": "SYNTHETIC_USER_CREDENTIAL", "ordinary": "visible"},
            "unknown_KEY": {"nested": ["SYNTHETIC_NESTED_CREDENTIAL", 7, true]},
            "agents": {
                "defaults": {"auth": {"api_key_fallback": true}, "session": {"kind":"resume-until", "context_tokens":100000}},
                "keymaster": {"harness":"codex", "model":"astra", "api_token":"SYNTHETIC_AGENT_CREDENTIAL"},
                "token-worker": {"effort":"ultra"}
            }
        }),
    );
    f.write(
        "project/.git/mdkg/config.local.json",
        &json!({
            "password": "SYNTHETIC_LOCAL_CREDENTIAL", "api_key_fallback": "SYNTHETIC_WRONG_TYPE",
            "credential/list": ["SYNTHETIC_ARRAY_CREDENTIAL"]
        }),
    );
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC_"), "{text}");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("visible"));
    }
    let result = f.show();
    assert_eq!(
        result["data"]["values"]["agents"]["defaults"]["auth"]["api_key_fallback"],
        true
    );
    assert_eq!(
        result["data"]["values"]["agents"]["keymaster"]["model"],
        "astra"
    );
    assert_eq!(
        result["data"]["values"]["agents"]["token-worker"]["effort"],
        "ultra"
    );
    assert_eq!(
        result["data"]["values"]["agents"]["defaults"]["session"]["context_tokens"],
        100000
    );
    assert!(
        result["data"]["redacted"]
            .as_array()
            .unwrap()
            .contains(&json!("/credential~1list/0"))
    );
    for pointer in result["data"]["origins"].as_object().unwrap().keys() {
        assert!(
            result["data"]["values"].pointer(pointer).is_some(),
            "{pointer}"
        );
    }
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn display_redacts_recognizable_credentials_under_ordinary_keys() {
    let f = Fixture::new();
    f.project();
    f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "notes": "Authorization: bEaReR SYNTHETIC_BEARER",
            "examples": [
                "Bearer\tSYNTHETIC_ARRAY_BEARER",
                "Authorization:Bearer SYNTHETIC_COMPACT_BEARER",
                "Authorization:Basic U1lOVEhFVElDOlBBU1M=",
                "Basic U1lOVEhFVElDOlBBU1M=",
                "-----BEGIN PRIVATE KEY-----\nSYNTHETIC_PKCS8\n-----END PRIVATE KEY-----",
                "-----BEGIN RSA PRIVATE KEY-----\nSYNTHETIC_RSA\n-----END RSA PRIVATE KEY-----",
                "-----BEGIN EC PRIVATE KEY-----\nSYNTHETIC_EC\n-----END EC PRIVATE KEY-----",
                "-----BEGIN OPENSSH PRIVATE KEY-----\nSYNTHETIC_SSH\n-----END OPENSSH PRIVATE KEY-----",
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nSYNTHETIC_ENCRYPTED\n-----END ENCRYPTED PRIVATE KEY-----"
            ],
            "ordinary": "visible description"
        }),
    );
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC_"), "credential value leaked");
        assert!(text.contains("visible description"));
    }
    let data = &f.show()["data"];
    assert_eq!(data["values"]["notes"], "[redacted]");
    assert_eq!(data["redacted"].as_array().unwrap().len(), 10);
    for pointer in data["redacted"].as_array().unwrap() {
        let pointer = pointer.as_str().unwrap();
        assert_eq!(data["values"].pointer(pointer).unwrap(), "[redacted]");
        assert_eq!(data["origins"][pointer]["layer"], "user");
    }
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn display_does_not_exempt_malformed_structural_entries_from_redaction() {
    let f = Fixture::new();
    f.project();
    f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "agents": {"api_token":"SYNTHETIC_ACTOR", "private_key":["SYNTHETIC_ACTOR_ARRAY"]},
            "workspaces": {"api_token":"SYNTHETIC_WORKSPACE", "private_key":["SYNTHETIC_WORKSPACE_ARRAY"]},
            "subgraphs": {"api_token":"SYNTHETIC_SUBGRAPH", "private_key":["SYNTHETIC_SUBGRAPH_ARRAY"]},
            "authority": {"delegates": {"api_token":"SYNTHETIC_DELEGATE", "private_key":["SYNTHETIC_DELEGATE_ARRAY"]}}
        }),
    );
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC_"), "malformed entry leaked");
    }
    let data = &f.show()["data"];
    for parent in [
        "/agents",
        "/workspaces",
        "/subgraphs",
        "/authority/delegates",
    ] {
        for field in ["api_token", "private_key/0"] {
            let pointer = format!("{parent}/{field}");
            assert_eq!(data["values"].pointer(&pointer).unwrap(), "[redacted]");
            assert!(
                data["redacted"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(pointer))
            );
            assert_eq!(data["origins"][&pointer]["layer"], "user");
        }
    }
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn display_hides_token_url_and_argument_credentials_in_both_formats() {
    let f = Fixture::new();
    f.project();
    let mut samples: Vec<String> = [
        "sk-",
        "sk-ant-",
        "sk-proj-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "AIza",
    ]
    .map(|prefix| format!("{prefix}{}", "SYNTHETIC".repeat(5)))
    .into();
    samples.extend([
        format!("AKIA{}", "SYNTHETIC1234567"),
        format!("ASIA{}", "SYNTHETIC1234567"),
        "eyJSYNTHETIC.eyJSYNTHETIC.c2lnbmF0dXJl".into(),
        "-----BEGIN PGP PRIVATE KEY BLOCK-----\nSYNTHETIC_PGP\n-----END PGP PRIVATE KEY BLOCK-----"
            .into(),
        "https://user:SYNTHETIC_PASSWORD@host/x".into(),
        "https://user:pass@host/x".into(),
        "https://api/x?access_token=SYNTHETIC_QUERY".into(),
        "https://api/x?%61ccess%5ftoken=SYNTHETIC_ENCODED_QUERY".into(),
        "https://api/x?ordinary=visible&client_secret=SYNTHETIC_SECOND_QUERY#fragment".into(),
        "command --api-key SYNTHETIC_COMMAND_ARG".into(),
        "command '--api-key' 'SYNTHETIC_QUOTED_ARG'".into(),
        "--password=SYNTHETIC_EQUALS_ARG".into(),
        "curl -H 'Authorization:Bearer SYNTHETIC_HEADER' https://host/x".into(),
        "Basic dTpw".into(),
        "Authorization:Basic dTpw".into(),
        "Use Basic dXNlcjpwYXNz for the request".into(),
        "Use Bearer SYNTHETIC_EMBEDDED for the request".into(),
    ]);
    f.write("home/.config/mdkg/config.json", &json!({
        "extension": {"samples":samples},
        "args":["--api-key", "SYNTHETIC_FLAG_VALUE", "--token=SYNTHETIC_ASSIGNED", "--model", "visible-model"]
    }));
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC"), "credential value leaked");
        assert!(!text.contains("user:pass@"), "URL userinfo leaked");
        assert!(text.contains("visible-model"));
    }
    let data = &f.show()["data"];
    for index in 0..samples.len() {
        let pointer = format!("/extension/samples/{index}");
        assert_eq!(
            data["values"].pointer(&pointer).unwrap(),
            "[redacted]",
            "sample {index}"
        );
        assert!(
            data["redacted"]
                .as_array()
                .unwrap()
                .contains(&json!(pointer))
        );
        assert_eq!(data["origins"][&pointer]["layer"], "user");
    }
    assert_eq!(
        data["values"]["args"],
        json!([
            "--api-key",
            "[redacted]",
            "[redacted]",
            "--model",
            "visible-model"
        ])
    );
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn display_hides_short_explicit_bearer_headers_without_hiding_prose() {
    let f = Fixture::new();
    f.project();
    let samples: Vec<String> = (1..=7)
        .flat_map(|len| {
            let payload = "z".repeat(len);
            [
                format!("Authorization: Bearer {payload}"),
                format!("Authorization:Bearer {payload}"),
                format!("aUtHoRiZaTiOn:\tbeARer\t{payload}"),
                format!("Use 'Authorization:   Bearer {payload}' for the request"),
                format!("curl -H 'Proxy-Authorization: Bearer {payload}' https://host/x"),
                format!("Proxy-Authorization:Bearer {payload}"),
                format!("curl -H'Authorization: Bearer {payload}' https://host/x"),
                format!("curl -H\"Authorization:Bearer {payload}\" https://host/x"),
                format!(r#"{{"Authorization": "Bearer {payload}"}}"#),
                format!("'Authorization': 'Bearer {payload}'"),
                format!(r#"{{"Proxy-Authorization":"Bearer {payload}"}}"#),
                format!("headers = {{'aUtHoRiZaTiOn': 'bEaReR {payload}'}}"),
                format!(r#"{{"Authorization" : "Bearer {payload}"}}"#),
                format!(r#"{{"Authorization" :"Bearer {payload}"}}"#),
                format!("{{'Proxy-Authorization'\t:\t'Bearer {payload}'}}"),
                format!(r#"{{"Authorization":"Bearer {payload}","Accept":"text/plain"}}"#),
                format!("{{'Proxy-Authorization':'Bearer {payload}','Accept':'text/plain'}}"),
            ]
        })
        .collect();
    let readable = [
        "Bearer bonds are documents",
        "Use bearer tokens for requests",
        "Description: Bearer bonds",
        "X-Authorization: Bearer bonds",
        "NotAuthorization: Bearer bonds",
        "Authorization: Bearer",
        "Authorization: Bearer ''",
        r#"{"Description": "Bearer bonds"}"#,
        r#"{"X-Authorization": "Bearer bonds"}"#,
        r#"{"Authorization": "Bearer"}"#,
        r#"{"Authorization" : "Bearer"}"#,
        r#"{"NotAuthorization" : "Bearer bonds"}"#,
        r#"{"Authorization":"Bearer ","Accept":"text/plain"}"#,
    ];
    f.write(
        "home/.config/mdkg/config.json",
        &json!({"notes": samples, "descriptions": readable}),
    );
    let before = snapshot(&f.dir);
    let data = &f.show()["data"];
    assert_eq!(data["redacted"].as_array().unwrap().len(), samples.len());
    for index in 0..samples.len() {
        let pointer = format!("/notes/{index}");
        assert_eq!(
            data["values"].pointer(&pointer).unwrap(),
            "[redacted]",
            "{pointer}"
        );
        assert_eq!(data["origins"][&pointer]["layer"], "user");
        assert!(
            data["redacted"]
                .as_array()
                .unwrap()
                .contains(&json!(pointer))
        );
    }
    assert_eq!(data["values"]["descriptions"], json!(readable));
    let out = f.output(&["config", "show"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for index in 0..samples.len() {
        assert!(
            text.contains(&format!("\"/notes/{index}\" = \"[redacted]\"")),
            "header not redacted in text output: /notes/{index}"
        );
    }
    for ordinary in readable {
        assert!(
            text.contains(&serde_json::to_string(ordinary).unwrap()),
            "ordinary text hidden: {ordinary}"
        );
    }
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn display_keeps_prose_and_auth_policy_visible_while_hiding_auth_fields() {
    let f = Fixture::new();
    f.project();
    let readable = [
        "Basic reviewer for docs",
        "note: basic setup",
        "Use basic checks before editing",
        "Basic introduction",
        "https://host:8080/path:a@b?ordinary=visible",
        "ssh://git@host/project",
        "the ghp_ prefix",
        "ASIA_PACIFIC_NORTHEAST",
    ];
    f.write("home/.config/mdkg/config.json", &json!({
        "descriptions":readable,
        "auth":"SYNTHETIC_AUTH",
        "cookie":["SYNTHETIC_COOKIE"],
        "extension":{"pat":"SYNTHETIC_PAT","bearer":"SYNTHETIC_NAMED_BEARER","session_key":"SYNTHETIC_SESSION"},
        "agents":{
            "defaults":{"auth":{"prefer":"subscription","api_key_fallback":true}},
            "worker":{"policy":{"auth":{"prefer":"subscription","api_key_fallback":false}}},
            "reset":{"policy":{"auth":null}},
            "nullable":{"policy":{"auth":{"prefer":null,"api_key_fallback":null}}},
            "broken":{"policy":{"auth":{"prefer":"SYNTHETIC_INVALID_POLICY"}}}
        },
        "authority":{"delegates":{"writer":{"rights":["edit"]}}}
    }));
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC"), "named credential leaked");
        for ordinary in readable {
            assert!(text.contains(ordinary), "ordinary text hidden: {ordinary}");
        }
    }
    let data = &f.show()["data"];
    assert_eq!(
        data["values"]["agents"]["defaults"]["auth"]["prefer"],
        "subscription"
    );
    assert_eq!(
        data["values"]["agents"]["defaults"]["auth"]["api_key_fallback"],
        true
    );
    assert_eq!(
        data["values"]["agents"]["worker"]["policy"]["auth"]["api_key_fallback"],
        false
    );
    assert!(data["values"]["agents"]["reset"]["policy"]["auth"].is_null());
    assert!(data["values"]["agents"]["nullable"]["policy"]["auth"]["api_key_fallback"].is_null());
    assert_eq!(
        data["values"]["authority"]["delegates"]["writer"]["rights"],
        json!(["edit"])
    );
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn cli_refuses_identity_in_optional_layers_and_a_bare_project_root() {
    for layer in [
        "home/.config/mdkg/config.json",
        "project/.git/mdkg/config.local.json",
    ] {
        for key in ["graph_id", "schema_version", "format_version"] {
            let f = Fixture::new();
            f.project();
            let source = f.write(layer, &json!({key:"forbidden identity"}));
            let out = f.output(&["config", "show", "--json"]);
            assert!(!out.status.success());
            let result: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert!(result["diagnostics"].as_array().unwrap().iter().any(|d|
                d["code"] == "config.invalid" && d["path"] == source.to_str().unwrap()), "{result}");
        }
    }
    let f = Fixture::new();
    f.project();
    fs::remove_dir_all(f.root.join(".git")).unwrap();
    for path in ["objects", "refs"] {
        fs::create_dir(f.root.join(path)).unwrap();
    }
    fs::write(f.root.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(f.root.join("config"), "[core]\n bare=true\n").unwrap();
    let out = f.output(&["config", "show", "--json"]);
    assert!(!out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "config.unreadable"
                && d["message"].as_str().unwrap().contains("checkout")),
        "{result}"
    );
}
