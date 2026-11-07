//! Configuration inspection uses disposable Git repositories and homes.
//! No coordinator or user configuration is used.
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
        self.git(&["init", "-q", "-b", "main"]);
        let path = self.write(
            "project/.mdkg/config.json",
            &json!({
                "schema_version":3, "format_version":"0.7",
                "graph_id":"0192f3a0-0000-7000-8000-000000000000"
            }),
        );
        self.commit_graph();
        path
    }
    fn git(&self, args: &[&str]) -> Output {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "init.templateDir=",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                // No detached auto-maintenance after a commit: git 2.56
                // runs it in the background, and its lock would show up
                // in a snapshot taken meanwhile.
                "-c",
                "maintenance.auto=false",
            ])
            .args(args)
            .output()
            .expect("run fixture git");
        assert!(
            out.status.success() || args == ["diff", "--cached", "--quiet"],
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn git_out(&self, args: &[&str]) -> String {
        String::from_utf8(self.git(args).stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_owned()
    }
    /// A config read that may find nothing: `None` when git exits 1.
    fn git_try(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("run fixture git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
    fn commit_graph(&self) {
        self.git(&["add", "-A", "--", ".mdkg"]);
        match self.git(&["diff", "--cached", "--quiet"]).status.code() {
            Some(0) => (),
            Some(1) => {
                self.git(&["commit", "-q", "-m", "fixture graph"]);
            }
            _ => panic!("fixture git diff failed"),
        }
    }
    fn source(&self) -> PathBuf {
        let commit = self.git(&["rev-parse", "HEAD"]);
        let commit = String::from_utf8(commit.stdout).expect("git commit id is UTF-8");
        format!("checkout:HEAD@{}", &commit[..12]).into()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        command
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
        // A ref-mode graph cache is read-only by design.
        Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.dir)
            .status()
            .ok();
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
    f.write(
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
    f.commit_graph();
    let source = f.source();
    let before = snapshot(&f.dir);
    let result = f.show();
    let data = &result["data"];
    assert_eq!(result["schema"], "mdkg.config.show.v1");
    assert_eq!(data["defaults_version"], "0.7.0");
    assert_eq!(data["values"]["work"]["priority_min"], 4);
    for (key, layer, path) in [
        ("/index/tolerant", "user", Some(user)),
        ("/graph_id", "project", Some(source.clone())),
        ("/agents/astra/model", "project", Some(source)),
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
    f.write(
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
    f.commit_graph();
    let source = f.source();
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
        ("/agents/astra/harness", source.clone()),
        ("/agents/astra/model", user),
        ("/agents/astra/instructions", local),
        ("/agents/keep/model", source),
    ] {
        assert_eq!(data["origins"][key]["path"], json!(path));
    }
    assert!(data["origins"].get("/agents/astra/effort").is_none());
    let notice = data["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|s| s.contains("including receiver policy"))
        .expect("active-watch snapshot notice");
    let text = f.output(&["config", "show", "--effective"]);
    assert!(text.status.success());
    assert!(!String::from_utf8_lossy(&text.stderr).contains("not yet applied"));
    assert!(String::from_utf8_lossy(&text.stderr).contains(notice));
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn show_uses_one_graph_ref_for_config_and_all_harness_definitions() {
    for harness in ["claude", "codex", "opencode"] {
        let f = Fixture::new();
        let project = f.project();
        let definition = f.write(
            "project/.mdkg/agents/worker.json",
            &json!({
                "schema":"mdkg.agent.v1", "harness":harness,
                "role":"worker", "model":"graph-model"
            }),
        );
        f.commit_graph();
        f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        f.git(&[
            "config",
            "--local",
            "mdkg.graph.ref",
            "refs/heads/mdkg-graph",
        ]);
        f.git(&[
            "config",
            "--local",
            "mdkg.graph.0192f3a0-0000-7000-8000-000000000000.ref",
            "refs/heads/mdkg-graph",
        ]);
        let graph = f.git(&["rev-parse", "refs/heads/mdkg-graph"]);
        let graph = String::from_utf8(graph.stdout).unwrap();
        let source = format!("graph:refs/heads/mdkg-graph@{}", &graph[..12]);

        f.write(
            "project/.mdkg/agents/worker.json",
            &json!({
                "schema":"mdkg.agent.v1", "harness":"codex",
                "role":"orchestrator", "model":"checkout-model"
            }),
        );
        f.commit_graph();
        fs::write(project, "uncommitted invalid config").unwrap();
        fs::write(definition, "uncommitted invalid definition").unwrap();
        let before = snapshot(&f.dir);
        let data = &f.show()["data"];
        assert_eq!(data["values"]["agents"]["worker"]["harness"], harness);
        assert_eq!(data["values"]["agents"]["worker"]["role"], "worker");
        assert_eq!(data["values"]["agents"]["worker"]["model"], "graph-model");
        assert_eq!(data["origins"]["/agents/worker/model"]["path"], source);
        assert_eq!(data["origins"]["/graph_id"]["path"], source);
        assert_eq!(data["sources"]["project"], source);
        // Terra #15782: where mdkg read the config, for a manual change.
        let graph_dir = data["graph"]["common_dir"].as_str().expect("common dir");
        assert!(Path::new(graph_dir).ends_with(".git"), "{data}");
        assert_eq!(data["graph"]["ref"], "refs/heads/mdkg-graph");
        assert_eq!(data["graph"]["commit"], graph.trim());
        assert_eq!(data["graph"]["root"], "");
        assert_eq!(data["graph"]["dir"], ".mdkg");
        assert_eq!(data["graph"]["config_path"], ".mdkg/config.json");
        assert_eq!(snapshot(&f.dir), before);

        fs::remove_dir_all(f.root.join(".mdkg")).unwrap();
        let graphless = f.show();
        assert_eq!(
            graphless["data"]["values"]["agents"]["worker"]["harness"],
            harness
        );
        assert_eq!(graphless["data"]["sources"]["project"], source);
        fs::create_dir_all(f.root.join("subdir")).unwrap();
        let nested = f
            .command(&["config", "show", "--json"])
            .current_dir(f.root.join("subdir"))
            .output()
            .unwrap();
        assert!(nested.status.success());
        let nested: Value = serde_json::from_slice(&nested.stdout).unwrap();
        assert_eq!(nested["data"]["sources"]["project"], source);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.dir.join("absent"), f.root.join(".mdkg")).unwrap();
            let linked = f.show();
            assert_eq!(linked["data"]["sources"]["project"], source);
        }
        f.git(&["update-ref", "-d", "refs/heads/mdkg-graph"]);
        let missing = f.output(&["config", "show", "--json"]);
        assert!(!missing.status.success());
        let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
        assert_eq!(missing["diagnostics"][0]["code"], "graph.ref_missing");
    }
}

#[test]
fn show_refuses_when_a_local_layer_removes_graph_authority() {
    let f = Fixture::new();
    f.project();
    f.write(
        "project/.mdkg/agents/astra.json",
        &json!({
            "schema":"mdkg.agent.v1", "harness":"codex", "role":"worker"
        }),
    );
    f.commit_graph();
    for agents in [Value::Null, json!([]), json!("invalid")] {
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":agents}),
        );
        let before = snapshot(&f.dir);
        let out = f.output(&["config", "show", "--json"]);
        assert!(!out.status.success());
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["diagnostics"][0]["code"], "run.authority_override");
        assert_eq!(snapshot(&f.dir), before);
    }
}

#[test]
fn show_refuses_orchestration_overrides_but_explains_project_only_projections() {
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
            if pointer.starts_with("/orchestration") {
                let out = f.output(&["config", "show", "--json"]);
                assert!(!out.status.success());
                let result: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(result["diagnostics"][0]["code"], "run.authority_override");
                assert_eq!(snapshot(&f.dir), before);
                continue;
            }
            let result = f.show();
            let data = &result["data"];
            assert_eq!(data["values"].pointer(pointer), Some(&expected));
            assert_eq!(data["origins"][pointer]["layer"], layer);
            let text = f.output(&["config", "show", "--effective"]);
            assert!(text.status.success());
            let warning = data["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .find(|s| s.contains("projections are project-only"))
                .expect("shared repository projection rule");
            assert!(String::from_utf8_lossy(&text.stderr).contains(warning));
            assert_eq!(snapshot(&f.dir), before);
        }
    }
}

#[test]
fn show_labels_optional_defaults_and_agents_without_definition_files_as_ignored() {
    for source in [
        "home/.config/mdkg/config.json",
        "project/.git/mdkg/config.local.json",
    ] {
        let f = Fixture::new();
        f.project();
        f.write(
            source,
            &json!({"agents":{
                "defaults":{"turn":{"max_turns":3}},
                "legacy":{"model":"optional-model"}
            }}),
        );
        let before = snapshot(&f.dir);
        let shown = f.show();
        let warnings = shown["data"]["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|v| v.as_str().unwrap().contains(
            "agents.defaults from user and machine-local layers are not applied in 0.7.0"
        )));
        assert!(warnings.iter().any(|v| {
            v.as_str()
                .unwrap()
                .contains("inactive: no graph definition")
        }));
        let text = f.output(&["config", "show"]);
        assert!(text.status.success());
        for warning in warnings {
            assert!(String::from_utf8_lossy(&text.stderr).contains(&warning.to_string()));
        }
        assert_eq!(snapshot(&f.dir), before);
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
    f.commit_graph();
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
    f.commit_graph();
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
        f.commit_graph();
        assert!(!f.output(&["config", "show", "--json"]).status.success());
    }
    fs::remove_file(&config).unwrap();
    f.commit_graph();
    assert!(
        !f.output(&["config", "show", "--json"]).status.success(),
        "partial project"
    );
    fs::create_dir(&config).unwrap();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
    fs::remove_dir(&config).unwrap();
    symlink(f.dir.join("absent"), &config).unwrap();
    f.commit_graph();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
    fs::remove_file(&config).unwrap();
    fs::remove_dir(f.root.join(".mdkg")).unwrap();
    symlink(f.dir.join("absent"), f.root.join(".mdkg")).unwrap();
    f.commit_graph();
    assert!(!f.output(&["config", "show", "--json"]).status.success());
}

#[test]
fn definition_inputs_are_bounded_strict_and_visible_without_typed_defaults() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.project();
    let definition = f.write("project/.mdkg/agents/astra.json", &json!({"effort":false}));
    f.commit_graph();
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
        f.commit_graph();
        assert!(!f.output(&["config", "show", "--json"]).status.success());
    }
    fs::remove_file(&definition).unwrap();
    symlink(f.dir.join("absent"), &definition).unwrap();
    f.commit_graph();
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
                concat!("-----BEGIN PRIV", "ATE KEY-----\nSYNTHETIC_PKCS8\n-----END PRIV", "ATE KEY-----"),
                concat!("-----BEGIN RSA PRIV", "ATE KEY-----\nSYNTHETIC_RSA\n-----END RSA PRIV", "ATE KEY-----"),
                concat!("-----BEGIN EC PRIV", "ATE KEY-----\nSYNTHETIC_EC\n-----END EC PRIV", "ATE KEY-----"),
                concat!("-----BEGIN OPENSSH PRIV", "ATE KEY-----\nSYNTHETIC_SSH\n-----END OPENSSH PRIV", "ATE KEY-----"),
                concat!("-----BEGIN ENCRYPTED PRIV", "ATE KEY-----\nSYNTHETIC_ENCRYPTED\n-----END ENCRYPTED PRIV", "ATE KEY-----")
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

/// A key that itself looks like a credential is shown renamed, with
/// everything under it hidden, in both formats; `origins` and `redacted`
/// name the renamed pointers, and a key already named like a renamed one
/// keeps its own entry (hidden for its name, which holds "key").
#[test]
fn display_renames_keys_that_look_like_credentials() {
    let f = Fixture::new();
    f.project();
    let url_key = concat!("https://someone:SYNTHETIC_KEY_PASS", "@example.test/x");
    let header_key = concat!("Authorization: Bearer SYNTHETIC_", "HEADER_KEY");
    f.write(
        "home/.config/mdkg/config.json",
        &json!({
            "mirrors": {
                url_key: {"note": "plain", "list": ["also plain"]},
                header_key: "plain value",
                "[redacted key 1]": "visible collision",
                "ordinary": "visible"
            }
        }),
    );
    let before = snapshot(&f.dir);
    for args in [&["config", "show", "--json"][..], &["config", "show"][..]] {
        let out = f.output(args);
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("SYNTHETIC_"), "{text}");
        assert!(text.contains("visible"), "{text}");
    }
    let data = &f.show()["data"];
    let mirrors = data["values"]["mirrors"].as_object().unwrap();
    assert_eq!(mirrors["ordinary"], "visible");
    // The two renamed keys skip the name the object already had.
    let mut keys: Vec<&String> = mirrors.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "[redacted key 1]",
            "[redacted key 2]",
            "[redacted key 3]",
            "ordinary"
        ],
        "{mirrors:?}"
    );
    assert!(
        ["[redacted key 2]", "[redacted key 3]"]
            .iter()
            .any(|k| mirrors[*k]["note"] == "[redacted]"),
        "{mirrors:?}"
    );
    // Every leaf under a renamed key is hidden and listed.
    let redacted = data["redacted"].as_array().unwrap();
    for pointer in redacted {
        let pointer = pointer.as_str().unwrap();
        assert_eq!(data["values"].pointer(pointer).unwrap(), "[redacted]");
    }
    assert_eq!(redacted.len(), 4, "{redacted:?}");
    for pointer in data["origins"].as_object().unwrap().keys() {
        assert!(!pointer.contains("SYNTHETIC_"), "{pointer}");
        assert!(data["values"].pointer(pointer).is_some(), "{pointer}");
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
        concat!("eyJSYNTHETIC.", "eyJSYNTHETIC.c2lnbmF0dXJl").into(),
        "-----BEGIN PGP PRIVATE KEY BLOCK-----\nSYNTHETIC_PGP\n-----END PGP PRIVATE KEY BLOCK-----"
            .into(),
        concat!("https://user:SYNTHETIC_PASSWORD", "@host/x").into(),
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

impl Fixture {
    /// A graph `mdkg init` made, committed on main.
    fn initialized(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        let out = self.output(&["init"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.commit_graph();
    }
    fn config(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join(".mdkg/config.json")).expect("config"))
            .expect("config JSON")
    }
    fn set_config(&self, value: &Value) {
        fs::write(self.root.join(".mdkg/config.json"), value.to_string()).expect("config");
    }
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let out = self.output(args);
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&out.stdout)));
        (out.status.code().expect("exit code"), value)
    }
    fn doctor_config(&self) -> Value {
        let (_, doctor) = self.json(&["doctor", "--json"]);
        doctor["data"]["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["name"] == "config")
            .cloned()
            .expect("config check")
    }
}

/// e2e 0.7.1 B13: a committed `coordination.messages` setting that
/// refuses every send and ack fails doctor's config check and validate.
#[test]
fn doctor_flags_invalid_ack_grace() {
    let f = Fixture::new();
    f.initialized();
    let good = f.config();
    assert_eq!(f.doctor_config()["status"], "ok", "{}", f.doctor_config());
    for bad in [json!(3651), json!(7.0), json!("7")] {
        let mut config = good.clone();
        config["coordination"] = json!({"messages": {"ack_grace_days": bad}});
        f.set_config(&config);
        f.commit_graph();
        let check = f.doctor_config();
        assert_eq!(check["status"], "error", "{bad}: {check}");
        assert!(
            check["summary"]
                .as_str()
                .expect("summary")
                .contains("ack_grace_days"),
            "{check}"
        );
        let (code, validate) = f.json(&["validate", "--json"]);
        assert_ne!(code, 0, "{bad}: {validate}");
        assert!(
            validate["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["code"] == "config.invalid"),
            "{validate}"
        );
    }
    // Retention and the human hold refuse sends too.
    let mut config = good.clone();
    config["coordination"] = json!({"messages": {"retention": "forever", "human_hold_days": 0}});
    f.set_config(&config);
    f.commit_graph();
    let check = f.doctor_config();
    assert_eq!(check["status"], "error", "{check}");
    let summary = check["summary"].as_str().expect("summary");
    assert!(
        summary.contains("retention") && summary.contains("human_hold_days"),
        "{check}"
    );
    // What runs is the committed value: a fix not yet committed still fails.
    f.set_config(&good);
    assert_eq!(f.doctor_config()["status"], "error");
    f.commit_graph();
    assert_eq!(f.doctor_config()["status"], "ok");
    let (code, validate) = f.json(&["validate", "--json"]);
    assert_eq!(code, 0, "{validate}");
}

/// Terra #16037: ref mode binds the repository-root graph at `.mdkg`, so
/// doctor and `config show` refuse a registered root folder elsewhere
/// (`mdkg.graph.<id>.dir`) instead of passing a graph no command opens.
#[test]
fn ref_mode_refuses_a_registered_root_folder_commands_dont_read() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    for (key, value) in [
        ("mdkg.graph.ref".to_owned(), "refs/heads/mdkg-graph"),
        (format!("mdkg.graph.{id}.ref"), "refs/heads/mdkg-graph"),
    ] {
        f.git(&["config", "--local", &key, value]);
    }
    let doctor_check = |name: &str| {
        let (_, doctor) = f.json(&["doctor", "--json"]);
        doctor["data"]["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name}: {doctor}"))
    };
    assert_eq!(doctor_check("graph.ref")["status"], "ok");
    assert_eq!(f.doctor_config()["status"], "ok");
    let (code, show) = f.json(&["config", "show", "--json"]);
    assert_eq!(code, 0, "{show}");
    assert_eq!(show["data"]["graph"]["dir"], ".mdkg", "{show}");
    // A committed mail setting that refuses every send and ack fails
    // doctor's config check in ref mode too (B13).
    let good = f.config();
    let mut bad = good.clone();
    bad["coordination"] = json!({"messages": {"ack_grace_days": 3651}});
    f.set_config(&bad);
    f.git(&["commit", "-q", "-am", "bad ack grace"]);
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    let check = f.doctor_config();
    assert_eq!(check["status"], "error", "{check}");
    assert!(
        check["summary"]
            .as_str()
            .expect("summary")
            .contains("ack_grace_days"),
        "{check}"
    );
    f.set_config(&good);
    f.git(&["commit", "-q", "-am", "good ack grace"]);
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    assert_eq!(f.doctor_config()["status"], "ok");
    // The same graph moved to `knowledge/` and registered there.
    f.git(&["mv", ".mdkg", "knowledge"]);
    f.git(&["commit", "-q", "-m", "graph in knowledge"]);
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, "knowledge"]);
    let check = doctor_check("graph.ref");
    assert_eq!(check["status"], "error", "{check}");
    let summary = check["summary"].as_str().expect("summary");
    assert!(
        summary.contains(&dir_key) && summary.contains("`knowledge/config.json`"),
        "names the record and where the graph's config is: {check}"
    );
    let hint = check["hint"].as_str().expect("hint");
    assert!(
        hint.contains("--checkout-mode") && hint.contains("move the graph's `knowledge` folder"),
        "{check}"
    );
    assert!(!hint.starts_with("unset"), "{check}");
    // config show and a bound command refuse it the same way: what
    // doctor reports agrees with what runs.
    for args in [&["config", "show", "--json"][..], &["status", "--json"][..]] {
        let (code, v) = f.json(args);
        assert_ne!(code, 0, "{args:?}: {v}");
        let d = &v["diagnostics"][0];
        assert_eq!(d["code"], "config.invalid", "{args:?}: {v}");
        assert!(
            d["message"].as_str().expect("message").contains(&dir_key),
            "{args:?}: {v}"
        );
    }
    // An MCP server still starts, and its tools return the refusal
    // (Terra #16243).
    // Run again in the half-repaired state below (#16260).
    // `tool` with `named` the actor, or none (identity is then captured by
    // the server itself, Terra #16296).
    let mcp_refuses = |tool: &str, named: bool| {
        let mut command = f.command(&["mcp"]);
        if !named {
            command.env_remove("MDKG_ACTOR");
        }
        let mut mcp = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start mcp");
        {
            use std::io::Write as _;
            let mut stdin = mcp.stdin.take().expect("stdin");
            let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": {}}});
            writeln!(stdin, "{call}").expect("write");
        }
        let out = mcp.wait_with_output().expect("mcp exits at end of input");
        let reply: Value = serde_json::from_str(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_else(|| panic!("no reply; exit {:?}", out.status)),
        )
        .expect("one JSON reply");
        let text = reply["result"]["content"]
            .as_array()
            .and_then(|c| c.last())
            .and_then(|c| c["text"].as_str())
            .unwrap_or_else(|| panic!("{reply}"));
        let listed: Value = serde_json::from_str(text).expect("envelope");
        assert_eq!(listed["ok"], false, "{listed}");
        assert_eq!(
            listed["diagnostics"][0]["code"], "config.invalid",
            "{listed}"
        );
        assert!(
            listed["diagnostics"][0]["message"]
                .as_str()
                .expect("message")
                .contains(&dir_key),
            "{listed}"
        );
        listed
    };
    let mcp_list_refuses = || {
        mcp_refuses("list", true);
        // A tool that needs the actor gets the folder refusal too, and is
        // told to restart the server once it's fixed.
        let inbox = mcp_refuses("inbox", false);
        assert!(
            inbox["diagnostics"][0]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("restart this MCP server")),
            "{inbox}"
        );
    };
    mcp_list_refuses();
    // The rollback the hint names isn't blocked by the turns install holds
    // (Terra #16224): a real one goes back to checkout mode, keeping the
    // graph ref.
    let tip = f.git_out(&["rev-parse", "refs/heads/mdkg-graph"]);
    let (code, v) = f.json(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        f.git_try(&["config", "--local", "mdkg.graph.ref"])
            .is_none(),
        "back in checkout mode: {v}"
    );
    assert_eq!(f.git_out(&["rev-parse", "refs/heads/mdkg-graph"]), tip);
    // Back in ref mode, part-way through the other repair: the graph is
    // moved back to `.mdkg` on the ref, its `.dir` record not yet removed.
    // Every command names the record, not a config missing from
    // `knowledge/`, and the hint asks only for the record's removal.
    f.git(&[
        "config",
        "--local",
        "mdkg.graph.ref",
        "refs/heads/mdkg-graph",
    ]);
    f.git(&["mv", "knowledge", ".mdkg"]);
    f.git(&["commit", "-q", "-m", "graph back in .mdkg"]);
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    for args in [
        &["config", "show", "--json"][..],
        &["status", "--json"][..],
        &["claim", "--new", "notes.txt", "--json"][..],
        &["msg", "inbox", "--json"][..],
    ] {
        let (code, v) = f.json(args);
        assert_ne!(code, 0, "{args:?}: {v}");
        let d = &v["diagnostics"][0];
        assert_eq!(d["code"], "config.invalid", "{args:?}: {v}");
        assert!(
            d["message"].as_str().expect("message").contains(&dir_key),
            "{args:?}: {v}"
        );
        let hint = d["hint"].as_str().expect("hint");
        assert!(
            hint.contains("already in `.mdkg`") && !hint.contains("move the graph's"),
            "{args:?}: {v}"
        );
    }
    mcp_list_refuses();
    // An unnamed server started in this state keeps no actor after the
    // repair: a ledger update refuses rather than running as the
    // checkout's folder name (Terra #16406).
    let mut mcp = f
        .command(&["mcp"])
        .env_remove("MDKG_ACTOR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start mcp");
    let mut stdin = mcp.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(mcp.stdout.take().expect("stdout"));
    let mut call = |id: u64, tool: &str, arguments: Value| -> Value {
        use std::io::{BufRead as _, Write as _};
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}});
        writeln!(stdin, "{request}").expect("write");
        stdin.flush().expect("flush");
        let mut line = String::new();
        stdout.read_line(&mut line).expect("reply");
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line}"));
        let text = reply["result"]["content"]
            .as_array()
            .and_then(|c| c.last())
            .and_then(|c| c["text"].as_str())
            .unwrap_or_else(|| panic!("{reply}"));
        serde_json::from_str(text).expect("envelope")
    };
    // Started (its identity captured) before the repair.
    call(1, "list", json!({}));
    f.git(&["config", "--local", "--unset", &dir_key]);
    let update = call(
        2,
        "entry_update",
        json!({"node": "task-x", "op": {"op": "add", "kind": "finding",
            "severity": "minor", "title": "t"}}),
    );
    // Every tool that writes, as the billing mark does (Terra #16535).
    let key = call(3, "agents_use_key", json!({"agent": "lane-agent"}));
    for refused in [&update, &key] {
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(
            refused["diagnostics"][0]["code"], "config.invalid",
            "{refused}"
        );
        assert!(
            refused["diagnostics"][0]["message"]
                .as_str()
                .expect("message")
                .contains(&dir_key),
            "{refused}"
        );
    }
    mcp.kill().ok();
    mcp.wait().ok();
    let (code, v) = f.json(&["status", "--json"]);
    assert_eq!(code, 0, "{v}");
}

/// Terra #16224: another graph's record, naming another graph ref and
/// another folder, is ignored by the resolver, so it blocks neither the
/// move into ref mode nor install in it.
#[test]
fn setup_ignores_another_refs_root_folder_record() {
    let f = Fixture::new();
    f.initialized();
    // The graph ref's first commit is the user's: theirs, never guessed.
    f.git(&["config", "--local", "user.name", "Test"]);
    f.git(&["config", "--local", "user.email", "test@example.invalid"]);
    let other = "0192f3a0-0000-7000-8000-0000000000aa";
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{other}.ref"),
        "refs/heads/old-graph",
    ]);
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{other}.dir"),
        ".old",
    ]);
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        f.git_out(&["config", "--local", "mdkg.graph.ref"]),
        "refs/heads/mdkg-graph"
    );
    let (code, v) = f.json(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = f.json(&["status", "--json"]);
    assert_eq!(code, 0, "{v}");
}

/// Terra #16243: the graph's own record counts whatever ref it named
/// before (a rollback from a custom graph ref leaves one), since the move
/// rewrites its `.ref`: refused before anything is written.
#[test]
fn setup_refuses_the_graphs_own_folder_record_under_another_ref() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{id}.ref"),
        "refs/heads/team-graph",
    ]);
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, "knowledge"]);
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    let d = &v["diagnostics"][0];
    assert_eq!(d["code"], "config.invalid", "{v}");
    assert!(
        d["message"].as_str().expect("message").contains(&dir_key),
        "{v}"
    );
    // It names where it read the config: the checkout's commit, not the
    // graph ref setup would create (Terra #16296).
    let message = d["message"].as_str().expect("message");
    assert!(
        message.contains("the checkout's commit") && !message.contains("on refs/heads/mdkg-graph"),
        "{v}"
    );
    // The source has the graph in `.mdkg`: nothing to move (#16260).
    let hint = d["hint"].as_str().expect("hint");
    assert!(
        hint.contains("already in `.mdkg`") && !hint.contains("move the graph's"),
        "{v}"
    );
    assert!(
        f.git_try(&["config", "--local", "mdkg.graph.ref"])
            .is_none(),
        "still in checkout mode"
    );
    assert_eq!(
        f.git_out(&["config", "--local", &format!("mdkg.graph.{id}.ref")]),
        "refs/heads/team-graph",
        "the record is untouched"
    );
}

/// Terra #16260 (orchestrator #16270): with the graph only in the
/// registered folder on HEAD, setup finds no graph at `.mdkg`, and its own
/// record naming another ref still refuses the move: it never switches to
/// an empty graph ref and leaves that graph behind.
#[test]
fn setup_refuses_when_no_graph_is_found_and_a_record_names_another_folder() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    f.git(&["mv", ".mdkg", "knowledge"]);
    f.git(&["commit", "-q", "-m", "graph in knowledge"]);
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{id}.ref"),
        "refs/heads/team-graph",
    ]);
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, "knowledge"]);
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    let d = &v["diagnostics"][0];
    assert_eq!(d["code"], "config.invalid", "{v}");
    assert!(
        d["message"].as_str().expect("message").contains(&dir_key),
        "{v}"
    );
    assert!(
        f.git_try(&["config", "--local", "mdkg.graph.ref"])
            .is_none(),
        "still in checkout mode"
    );
    assert!(
        f.git_try(&["rev-parse", "-q", "--verify", "refs/heads/mdkg-graph"])
            .is_none(),
        "no graph ref made"
    );
}

/// Terra #16379 (orchestrator #16386): another graph's record under
/// another ref still refuses the move when its folder holds the
/// checkout's graph, so a found graph ref never replaces it unseen.
#[test]
fn setup_refuses_a_foreign_record_whose_folder_holds_the_checkouts_graph() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    // An unrelated valid graph B at the destination ref.
    f.git(&["checkout", "-q", "-b", "other"]);
    let mut other = f.config();
    other["graph_id"] = json!("0192f3a0-0000-7000-8000-0000000000bb");
    f.set_config(&other);
    f.commit_graph();
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    // The checkout's graph A, only in `knowledge/`, recorded under
    // another ref.
    f.git(&["mv", ".mdkg", "knowledge"]);
    f.git(&["commit", "-q", "-m", "graph in knowledge"]);
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{id}.ref"),
        "refs/heads/team-graph",
    ]);
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, "knowledge"]);
    // Tracked on HEAD, but neither in the worktree nor in the object
    // store (a blob-filtered sparse clone): the tree entry still counts
    // (Terra #16506).
    fs::remove_dir_all(f.root.join("knowledge")).expect("drop worktree copy");
    let blob = f.git_out(&["rev-parse", "HEAD:knowledge/config.json"]);
    let loose = f
        .root
        .join(".git/objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    let mut perms = fs::metadata(&loose).expect("loose blob").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&loose, perms).ok();
    fs::remove_file(&loose).expect("drop the blob");
    assert!(
        f.git_try(&["cat-file", "-e", "HEAD:knowledge/config.json"])
            .is_none(),
        "the blob is gone"
    );
    let refused = |what: &str| {
        let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
        assert_ne!(code, 0, "{what}: {v}");
        assert_eq!(v["diagnostics"][0]["code"], "config.invalid", "{what}: {v}");
        assert!(
            f.git_try(&["config", "--local", "mdkg.graph.ref"])
                .is_none(),
            "{what}: still in checkout mode"
        );
    };
    refused("blob missing");
    // The folder's tree missing too (a tree-filtered sparse clone): the
    // lookup fails, and fails closed (Terra #16594).
    let tree = f.git_out(&["rev-parse", "HEAD:knowledge"]);
    let loose = f
        .root
        .join(".git/objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    fs::remove_file(&loose).expect("drop the tree");
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    let d = &v["diagnostics"][0];
    assert_eq!(d["code"], "config.invalid", "{v}");
    assert!(
        d["message"].as_str().expect("message").contains(&dir_key),
        "{v}"
    );
    assert!(
        f.git_try(&["config", "--local", "mdkg.graph.ref"])
            .is_none(),
        "still in checkout mode"
    );
}

/// Terra #16594 (orchestrator #16612): the folder is matched literally,
/// so a `:`-prefixed name isn't read as pathspec magic and missed.
#[test]
fn setup_matches_a_colon_prefixed_folder_literally() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    f.git(&["checkout", "-q", "-b", "other"]);
    let mut other = f.config();
    other["graph_id"] = json!("0192f3a0-0000-7000-8000-0000000000bb");
    f.set_config(&other);
    f.commit_graph();
    f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    f.git(&["checkout", "-q", "main"]);
    f.git(&["mv", ".mdkg", ":knowledge"]);
    f.git(&["commit", "-q", "-m", "graph in :knowledge"]);
    // Tracked on HEAD only, so the tree lookup decides.
    fs::remove_dir_all(f.root.join(":knowledge")).expect("drop worktree copy");
    f.git(&[
        "config",
        "--local",
        &format!("mdkg.graph.{id}.ref"),
        "refs/heads/team-graph",
    ]);
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, ":knowledge"]);
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "config.invalid", "{v}");
    assert!(
        f.git_try(&["config", "--local", "mdkg.graph.ref"])
            .is_none(),
        "still in checkout mode"
    );
}

/// #16095: the move into ref mode refuses a clone that registers the
/// repository-root graph in a folder other than `.mdkg`, before writing
/// any key.
#[test]
fn setup_refuses_a_registered_root_folder_ref_mode_doesnt_read() {
    let f = Fixture::new();
    f.initialized();
    let id = f.config()["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    let dir_key = format!("mdkg.graph.{id}.dir");
    f.git(&["config", "--local", &dir_key, "knowledge"]);
    let (code, v) = f.json(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    let d = &v["diagnostics"][0];
    assert_eq!(d["code"], "config.invalid", "{v}");
    assert!(
        d["message"].as_str().expect("message").contains(&dir_key),
        "{v}"
    );
    assert!(
        d["hint"]
            .as_str()
            .expect("hint")
            .contains("--checkout-mode"),
        "{v}"
    );
    assert!(
        d["hint"]
            .as_str()
            .expect("hint")
            .contains(&format!("`git config --unset {dir_key}`")),
        "{v}"
    );
    let local = fs::read_to_string(f.root.join(".git/config")).expect("git config");
    assert!(!local.contains("ref = "), "still in checkout mode: {local}");
}

/// e2e 0.7.1 B14: on a 0.6 branch cut before main's migration, doctor
/// says what `migrate` says (it refuses the branch), not to run it.
#[test]
fn doctor_gives_migrates_guidance_for_a_branch_cut_before_main_migrated() {
    let f = Fixture::new();
    f.initialized();
    // A 0.6.0 config, with no graph_id: before main's migration.
    let old = json!({
        "schema_version": 1, "tool": "mdkg",
        "workspaces": {"root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg"}}
    });
    f.git(&["checkout", "-q", "-b", "old"]);
    f.set_config(&old);
    f.commit_graph();
    let check = f.doctor_config();
    let hint = check["hint"].as_str().unwrap_or_else(|| panic!("{check}"));
    assert!(hint.contains("already migrated"), "{check}");
    assert!(hint.contains("merge main into this branch"), "{check}");
    assert!(!hint.contains("0.7.2 migrates"), "{check}");
    // With no migrated main, `migrate` is the way on.
    f.git(&["branch", "-q", "-f", "main", "old"]);
    let check = f.doctor_config();
    let hint = check["hint"].as_str().unwrap_or_else(|| panic!("{check}"));
    assert!(hint.contains("migrate` moves a 0.6.0 graph"), "{check}");
}

/// Review of 9b00cc5d (e2e 0.7.1 B3): mail and claims read only the
/// committed project's `coordination`, so `config show` says that a user
/// or machine-local value it shows changes nothing.
#[test]
fn show_explains_project_only_coordination() {
    for (source, layer) in [
        ("home/.config/mdkg/config.json", "user"),
        ("project/.git/mdkg/config.local.json", "local"),
    ] {
        let f = Fixture::new();
        f.project();
        f.write(
            source,
            &json!({"coordination": {"messages": {"ack_grace_days": 30}}}),
        );
        let result = f.show();
        let data = &result["data"];
        let pointer = "/coordination/messages/ack_grace_days";
        assert_eq!(data["origins"][pointer]["layer"], layer);
        let warning = data["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .filter_map(Value::as_str)
            .find(|s| s.contains("coordination is project-only"))
            .unwrap_or_else(|| panic!("{data}"));
        let text = f.output(&["config", "show", "--effective"]);
        assert!(String::from_utf8_lossy(&text.stderr).contains(warning));
    }
}
