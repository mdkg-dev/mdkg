//! Effective launcher settings, separate from authored definition management.
//!
//! Capture once per operation and pass the same values to preparation. A later
//! admission check must capture again under its lease. This module never reads
//! session state, writes configuration, or serializes typed defaults into the
//! source-attributed values. Projections remain authored project settings;
//! orchestration naming keeps the ordinary layer precedence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mdkg_format::settings::{self, Effective, Environment, Layers};
use mdkg_graph::config::layers::Project;
use serde_json::Value;

use super::agent::{self, Definition};
use crate::RunError;

/// A captured configuration for runtime consumers. Management commands must
/// keep using `load_agent_config`, which contains authored project values only.
#[derive(Debug)]
pub struct Snapshot {
    /// Applied runtime values. Only captured definition files admit optional
    /// agent overrides; other entries and shared defaults stay project-only.
    /// Projections also stay project-only because they control shared files.
    pub effective: Effective,
    pub warnings: Vec<settings::git::Warning>,
    /// Legacy project entries not replaced by a definition file. Optional-only
    /// entries must not be described as deprecated inline project definitions.
    pub authored_inline_agents: Vec<String>,
    /// Physical definition filenames, never inferred from an inline schema.
    pub definition_names: BTreeSet<String>,
}

fn graph_error(error: mdkg_graph::GraphError) -> RunError {
    let diagnostic = error.diagnostic;
    RunError {
        kind: crate::error::ErrorKind::from_exit(i32::from(error.kind.exit_code())),
        code: diagnostic.code.as_str().into(),
        message: diagnostic.message,
        hint: diagnostic.hint,
        path: diagnostic.path,
    }
}

fn bad(message: impl Into<String>) -> RunError {
    RunError::usage(super::CODE_CONFIG, message)
}

/// Capture the existing HOME/XDG and Git discovery inputs at the process
/// boundary. No new configuration environment variables are introduced.
pub fn load(root: &Path) -> Result<Snapshot, RunError> {
    load_with(root, super::authority(root)?)
}

/// [`load`] from an authority the operation already pinned, so what it
/// builds and what it later compares against come from one graph tip.
pub fn load_with(
    root: &Path,
    authority: mdkg_graph::graphref::Authority,
) -> Result<Snapshot, RunError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let git_dir = std::env::var_os("GIT_DIR").map(PathBuf::from);
    let git_common = std::env::var_os("GIT_COMMON_DIR").map(PathBuf::from);
    let cwd = std::env::current_dir()
        .map_err(|e| RunError::io("can't determine the current directory", e))?;
    capture_with(
        root,
        Environment {
            home: home.as_deref(),
            xdg_config_home: xdg.as_deref(),
            git: Some(settings::git::Environment {
                cwd: &cwd,
                dir: git_dir.as_deref(),
                common_dir: git_common.as_deref(),
            }),
        },
        authority,
    )
}

/// Read one committed authority and its optional layers with explicit
/// environment inputs. A failed capture never falls back to checkout files.
pub fn capture(root: &Path, environment: Environment<'_>) -> Result<Snapshot, RunError> {
    capture_with(root, environment, super::authority(root)?)
}

/// [`capture`] from an authority already pinned.
pub fn capture_with(
    root: &Path,
    environment: Environment<'_>,
    authority: mdkg_graph::graphref::Authority,
) -> Result<Snapshot, RunError> {
    let project = Project::from_authority(&authority, root, environment).map_err(graph_error)?;
    // Optional execution settings cannot repair a malformed committed
    // definition or conceal an invalid authority field.
    agent::validate_authority(&authority)?;
    let definitions = agent::authority_layer_inputs(&authority)?;
    let files: BTreeSet<_> = definitions
        .iter()
        .filter_map(|input| input.value.get("agents").and_then(Value::as_object))
        .flat_map(|agents| agents.keys().cloned())
        .collect();
    let authored_inline_agents = project
        .project_input()
        .value
        .get("agents")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(name, entry)| {
            name.as_str() != "defaults" && !files.contains(*name) && entry.get("schema").is_none()
        })
        .map(|(name, _)| name.clone())
        .collect();
    let mut resolved =
        agent::resolve_layers(&project, &definitions, None, None).map_err(graph_error)?;
    // Rebuild agents and projections from built-in and authored project
    // settings, then copy the fully layered agent entries for captured files. This
    // neither rereads sources nor lets an optional root replacement erase a
    // legacy actor or change its shared policy defaults.
    let defaults = settings::defaults().map_err(|e| bad(e.to_string()))?;
    let mut authored = Layers {
        user: None,
        project: Some(project.project_input().clone()),
        local: None,
    }
    .resolve(&defaults, &[], None)
    .map_err(|e| bad(e.to_string()))?;
    for name in &files {
        let entry = resolved
            .effective
            .values
            .get("agents")
            .and_then(|agents| agents.get(name))
            .ok_or_else(|| {
                bad(format!(
                    "agent {name}: a local override removed its definition"
                ))
            })?;
        Definition::parse(name, entry.clone())?;
        let agents = &mut authored.values["agents"];
        if agents.is_null() {
            *agents = serde_json::json!({});
        }
        agents
            .as_object_mut()
            .ok_or_else(|| bad("agents must be an object"))?
            .insert(name.clone(), entry.clone());
        let pointer = format!("/agents/{}", name.replace('~', "~0").replace('/', "~1"));
        authored
            .origins
            .retain(|p, _| p != "/agents" && !at_or_below(p, &pointer));
        authored.origins.extend(
            resolved
                .effective
                .origins
                .iter()
                .filter(|(p, _)| at_or_below(p, &pointer))
                .map(|(p, o)| (p.clone(), o.clone())),
        );
    }
    for key in ["agents", "projections"] {
        if let Some(value) = authored.values.get_mut(key) {
            resolved.effective.values[key] = value.take();
        } else {
            // Absence retains legacy projection semantics; do not manufacture
            // an explicit null or an origin when no authored value exists.
            resolved
                .effective
                .values
                .as_object_mut()
                .ok_or_else(|| bad("configuration must be an object"))?
                .remove(key);
        }
    }
    let authored_root = |p: &str| at_or_below(p, "/agents") || at_or_below(p, "/projections");
    resolved.effective.origins.retain(|p, _| !authored_root(p));
    resolved.effective.origins.extend(
        authored
            .origins
            .into_iter()
            .filter(|(p, _)| authored_root(p)),
    );
    if let Some(agents) = resolved.effective.values.get("agents") {
        match agents {
            Value::Null => (),
            Value::Object(_) => (),
            _ => return Err(bad("agents must be an object")),
        }
    }
    Ok(Snapshot {
        effective: resolved.effective,
        warnings: resolved.warnings,
        authored_inline_agents,
        definition_names: files,
    })
}

fn at_or_below(pointer: &str, parent: &str) -> bool {
    pointer == parent
        || pointer
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_format::settings::{Layer, Origin};
    use serde_json::json;
    use std::fs;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("mdkg-runtime-layers-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(&dir).unwrap();
            let f = Self(fs::canonicalize(dir).unwrap());
            fs::create_dir_all(f.root()).unwrap();
            f.git(&["init", "-q", "-b", "main"]);
            f.write(
                "project/.mdkg/config.json",
                &json!({
                    "schema_version":3, "format_version":"0.7",
                    "graph_id":"0192f3a0-0000-7000-8000-000000000000"
                }),
            );
            f.commit_graph();
            f
        }
        fn root(&self) -> PathBuf {
            self.0.join("project")
        }
        fn git(&self, args: &[&str]) -> std::process::Output {
            let output = std::process::Command::new("git")
                .current_dir(self.root())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.0)
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
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success() || args == ["diff", "--cached", "--quiet"],
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }
        fn commit_graph(&self) {
            self.git(&["add", "-A", "--", ".mdkg"]);
            let changed = self.git(&["diff", "--cached", "--quiet"]);
            match changed.status.code() {
                Some(0) => (),
                Some(1) => {
                    self.git(&["commit", "-q", "-m", "fixture authority"]);
                }
                _ => panic!("can't inspect fixture graph changes"),
            }
        }
        fn bytes(&self, path: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }
        fn write(&self, path: &str, value: &Value) -> PathBuf {
            self.bytes(path, value.to_string().as_bytes())
        }
        fn read(&self) -> Result<Snapshot, RunError> {
            self.commit_graph();
            self.capture()
        }
        fn capture(&self) -> Result<Snapshot, RunError> {
            let home = self.0.join("home");
            capture(
                &self.root(),
                Environment {
                    home: Some(&home),
                    ..Default::default()
                },
            )
        }
        fn source(&self) -> PathBuf {
            super::super::authority(&self.root()).unwrap().source.into()
        }
        fn definition(&self, harness: &str) -> PathBuf {
            self.write(
                "project/.mdkg/agents/worker.json",
                &json!({
                    "schema":agent::SCHEMA, "harness":harness, "role":"worker"
                }),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn runtime_pins_graph_settings_across_checkout_and_ref_changes_for_each_harness() {
        for harness in ["claude", "codex", "opencode"] {
            let f = Fixture::new();
            f.definition(harness);
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

            f.definition("codex");
            f.commit_graph();
            fs::remove_dir_all(f.root().join(".mdkg")).unwrap();
            let snapshot = f.capture().unwrap();
            assert_eq!(
                snapshot.effective.values["agents"]["worker"]["harness"],
                harness
            );
            assert!(
                snapshot.effective.origins["/agents/worker/harness"]
                    .path
                    .as_ref()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("graph:refs/heads/mdkg-graph@")
            );

            f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
            let next = f.capture().unwrap();
            assert_eq!(
                next.effective.values["agents"]["worker"]["harness"],
                "codex"
            );
            assert_eq!(
                snapshot.effective.values["agents"]["worker"]["harness"],
                harness
            );
        }
    }

    #[test]
    fn runtime_preserves_inspection_origins_and_authored_management_for_every_harness() {
        for (harness, model, effort) in [
            ("codex", "gpt-6-astra", Some("ultra")),
            ("claude", "claude-haiku-5-5", Some("high")),
            ("opencode", "anthropic/claude-haiku-5-5", None),
        ] {
            let f = Fixture::new();
            let path = f.root().join(".mdkg/config.json");
            let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            project["agents"] = json!({
                "worker":{"harness":"codex","role":"worker","model":"inline","effort":"ultra"},
                "legacy":{}, "defaults":{"turn":{"max_turns":3}}
            });
            fs::write(&path, project.to_string()).unwrap();
            let user = f.write(
                "home/.config/mdkg/config.json",
                &json!({
                    "agents":{"worker":{"model":model,"instructions":"user"}}
                }),
            );
            let file = f.definition(harness);
            let local = f.write(
                "project/.git/mdkg/config.local.json",
                &json!({
                    "agents":{"worker":{"instructions":"local", "effort":effort}}
                }),
            );
            let before = [path.clone(), file.clone(), user.clone(), local.clone()].map(|p| {
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            });
            let runtime = f.read().unwrap();
            let value = &runtime.effective.values["agents"]["worker"];
            assert_eq!(value["model"], model);
            assert_eq!(value["effort"], json!(effort));
            assert_eq!(value["instructions"], "local");
            assert!(
                value.get("mode").is_none(),
                "typed defaults must not become authored"
            );
            assert_eq!(runtime.authored_inline_agents, ["legacy"]);
            for (key, layer, source) in [
                ("model", Layer::User, user),
                ("harness", Layer::Project, f.source()),
                ("instructions", Layer::Local, local),
            ] {
                assert_eq!(
                    runtime.effective.origins[&format!("/agents/worker/{key}")],
                    Origin::file(layer, source)
                );
            }
            let home = f.0.join("home");
            let authority = super::super::authority(&f.root()).unwrap();
            let captured = Project::from_authority(
                &authority,
                &f.root(),
                Environment {
                    home: Some(&home),
                    ..Default::default()
                },
            )
            .unwrap();
            let display = agent::resolve_layers(
                &captured,
                &agent::authority_layer_inputs(&authority).unwrap(),
                None,
                None,
            )
            .unwrap();
            assert_eq!(runtime.effective.values, display.effective.values);
            assert_eq!(runtime.effective.origins, display.effective.origins);
            let authored = super::super::load_agent_config(&f.root()).unwrap();
            for field in ["model", "instructions", "effort"] {
                assert!(authored["agents"]["worker"].get(field).is_none(), "{field}");
            }
            for (path, bytes) in before {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn effective_file_validation_cannot_be_bypassed_by_a_local_schema_override() {
        let f = Fixture::new();
        f.definition("codex");
        for patch in [
            json!({"schema":null}),
            json!({"schema":"wrong"}),
            json!({"harness":null}),
            json!({"harness":"claude","effort":"ultra"}),
            json!({"harness":"opencode","effort":"low"}),
            json!({"role":"reviewer","sandbox":"read-only"}),
            json!({"cwd":"here","worktree":true}),
            json!({"model":3}),
            json!({"unexpected":true}),
            json!(null),
        ] {
            f.write(
                "project/.git/mdkg/config.local.json",
                &json!({"agents":{"worker":patch}}),
            );
            assert!(f.read().is_err(), "{patch}");
        }
    }

    #[test]
    fn optional_actors_never_enter_the_runtime_agent_map() {
        let f = Fixture::new();
        f.write(
            "home/.config/mdkg/config.json",
            &json!({"agents":{
                "from_user":{"schema":agent::SCHEMA,"harness":"claude","role":"worker"},
                "legacy_partial":{"model":"override"}
            }}),
        );
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":{
                "from_local":{"schema":agent::SCHEMA,"harness":"opencode","role":"worker"}
            }}),
        );
        let loaded = f.read().unwrap();
        assert!(loaded.authored_inline_agents.is_empty());
        for name in ["from_user", "from_local", "legacy_partial"] {
            assert!(
                loaded.effective.values["agents"].get(name).is_none(),
                "{name}"
            );
            assert!(
                !loaded
                    .effective
                    .origins
                    .keys()
                    .any(|p| p.starts_with(&format!("/agents/{name}/")))
            );
        }
        assert!(
            super::super::load_agent_config(&f.root())
                .unwrap()
                .get("agents")
                .is_none()
        );
    }

    #[test]
    fn legacy_overrides_start_applying_only_after_migration_to_a_file() {
        for source in [
            "home/.config/mdkg/config.json",
            "project/.git/mdkg/config.local.json",
        ] {
            let f = Fixture::new();
            let path = f.root().join(".mdkg/config.json");
            let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            project["agents"] = json!({
                "worker":{"harness":"codex","role":"worker","model":"project","effort":"high"},
                "inline_schema":{"schema":"legacy","model":"project"}
            });
            fs::write(&path, project.to_string()).unwrap();
            let optional = f.write(
                source,
                &json!({"agents":{
                    "worker":{"model":"optional","effort":"ultra"},
                    "inline_schema":{"model":"optional"},
                    "saved_only":{"model":"optional"}
                }}),
            );
            let before = f.read().unwrap();
            assert_eq!(
                before.effective.values["agents"]["worker"]["model"],
                "project"
            );
            assert_eq!(
                before.effective.values["agents"]["worker"]["effort"],
                "high"
            );
            assert_eq!(
                before.effective.values["agents"]["inline_schema"]["model"],
                "project"
            );
            assert!(
                before.effective.values["agents"]
                    .get("saved_only")
                    .is_none()
            );
            assert_eq!(
                before.effective.origins["/agents/worker/model"],
                Origin::file(Layer::Project, f.source())
            );
            f.definition("codex");
            let after = f.read().unwrap();
            assert_eq!(
                after.effective.values["agents"]["worker"]["model"],
                "optional"
            );
            assert_eq!(
                after.effective.values["agents"]["worker"]["effort"],
                "ultra"
            );
            assert_eq!(
                after.effective.origins["/agents/worker/model"].path,
                Some(optional)
            );
            assert_eq!(
                after.effective.values["agents"]["inline_schema"]["model"],
                "project"
            );
            assert!(!after.authored_inline_agents.contains(&"worker".into()));
            assert_eq!(fs::read(&path).unwrap(), project.to_string().as_bytes());
        }
    }

    #[test]
    fn shared_defaults_remain_authored_for_legacy_and_file_backed_actors() {
        let f = Fixture::new();
        let path = f.root().join(".mdkg/config.json");
        let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        project["agents"] = json!({"legacy":{}, "defaults":{"timeout":42}});
        fs::write(&path, project.to_string()).unwrap();
        f.definition("codex");
        f.write(
            "home/.config/mdkg/config.json",
            &json!({"agents":{"defaults":{"timeout":1}}}),
        );
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":{"defaults":{"timeout":2,"auth":{"prefer":"api-key"}}}}),
        );
        let snapshot = f.read().unwrap();
        let authored = super::super::load_agent_config(&f.root()).unwrap();
        for name in ["legacy", "worker", "saved_only"] {
            let actual =
                crate::policy::Policy::for_agent(&snapshot.effective.values, name).unwrap();
            let expected = crate::policy::Policy::for_agent(&authored, name).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        assert_eq!(
            snapshot.effective.origins["/agents/defaults/timeout"],
            Origin::file(Layer::Project, f.source())
        );
        assert_eq!(
            snapshot.effective.origins["/agents/defaults/auth/prefer"].layer,
            Layer::BuiltIn
        );
    }

    #[test]
    fn projections_remain_authored_when_optional_layers_change_them() {
        for source in [
            "home/.config/mdkg/config.json",
            "project/.git/mdkg/config.local.json",
        ] {
            for authored in [
                None,
                Some(Value::Null),
                Some(json!([])),
                Some(json!(["mcp:claude"])),
                Some(json!("invalid")),
            ] {
                for optional in [
                    Value::Null,
                    json!([]),
                    json!(["hooks:codex"]),
                    json!({"nested":true}),
                    json!("invalid"),
                ] {
                    let f = Fixture::new();
                    let path = f.root().join(".mdkg/config.json");
                    let mut project: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    if let Some(value) = &authored {
                        project["projections"] = value.clone();
                    }
                    fs::write(&path, project.to_string()).unwrap();
                    let baseline = f.read().unwrap();
                    let source = f.write(source, &json!({"projections":optional}));
                    let before = [path, source.clone()].map(|p| {
                        let bytes = fs::read(&p).unwrap();
                        (p, bytes)
                    });
                    let snapshot = f.read().unwrap();
                    assert_eq!(
                        snapshot.effective.values.get("projections"),
                        authored.as_ref()
                    );
                    let origins = |effective: &Effective| {
                        effective
                            .origins
                            .iter()
                            .filter(|(p, _)| at_or_below(p, "/projections"))
                            .map(|(p, o)| (p.clone(), o.clone()))
                            .collect::<std::collections::BTreeMap<_, _>>()
                    };
                    assert_eq!(origins(&snapshot.effective), origins(&baseline.effective));
                    let expected = crate::projection::Projections::read(&f.root());
                    let actual =
                        crate::projection::Projections::from_config(&snapshot.effective.values);
                    match (actual, expected) {
                        (Ok(actual), Ok(expected)) => {
                            assert_eq!(actual.names(), expected.names());
                            assert_eq!(actual.declared(), expected.declared());
                        }
                        (Err(actual), Err(expected)) => {
                            assert_eq!(actual.code, expected.code);
                            assert_eq!(actual.message, expected.message);
                        }
                        _ => panic!("optional layers changed projection validation"),
                    }
                    for (path, bytes) in before {
                        assert_eq!(fs::read(path).unwrap(), bytes);
                    }
                }
            }
        }
    }

    #[test]
    fn whole_optional_agent_replacements_cannot_erase_legacy_or_hide_a_definition() {
        for replacement in [Value::Null, json!([]), json!("bad")] {
            let f = Fixture::new();
            let path = f.root().join(".mdkg/config.json");
            let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            project["agents"] = json!({"legacy":{"model":"project"}});
            fs::write(&path, project.to_string()).unwrap();
            f.write(
                "project/.git/mdkg/config.local.json",
                &json!({"agents":replacement}),
            );
            let before = f.read().unwrap();
            assert_eq!(
                before.effective.values["agents"]["legacy"]["model"],
                "project"
            );
            assert_eq!(
                before.effective.origins["/agents/legacy/model"],
                Origin::file(Layer::Project, f.source())
            );
            assert!(!before.effective.origins.contains_key("/agents"));
            f.definition("codex");
            assert!(
                f.read()
                    .unwrap_err()
                    .message
                    .contains("changes committed role")
            );
        }
    }

    #[test]
    fn a_null_authored_agents_root_gains_only_file_values_and_their_leaf_origins() {
        let f = Fixture::new();
        let path = f.root().join(".mdkg/config.json");
        let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        project["agents"] = Value::Null;
        fs::write(path, project.to_string()).unwrap();
        f.definition("codex");
        let loaded = f.read().unwrap();
        assert_eq!(loaded.definition_names, BTreeSet::from(["worker".into()]));
        assert!(!loaded.effective.origins.contains_key("/agents"));
        assert!(loaded.effective.values["agents"].get("defaults").is_none());
        assert_eq!(
            loaded.effective.origins["/agents/worker/harness"],
            Origin::file(Layer::Project, f.source())
        );
    }

    #[test]
    fn malformed_unrelated_inline_entries_cannot_hide_behind_a_valid_agent() {
        for invalid in [json!({"unused":null}), json!({"../unused":{}})] {
            let f = Fixture::new();
            f.definition("codex");
            let path = f.root().join(".mdkg/config.json");
            let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            project["agents"] = invalid;
            fs::write(&path, project.to_string()).unwrap();
            f.commit_graph();
            assert!(super::super::load_agent_config(&f.root()).is_err());
            assert!(f.capture().is_err());
        }
    }

    #[test]
    fn a_new_capture_observes_changes_without_mutating_an_earlier_snapshot() {
        let f = Fixture::new();
        f.definition("codex");
        f.write(
            "home/.config/mdkg/config.json",
            &json!({"agents":{"worker":{"model":"user","effort":"high"}}}),
        );
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":{"worker":{"model":"local","effort":"ultra"}}}),
        );
        let first = f.read().unwrap();
        f.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":{"worker":{"model":null,"effort":null}}}),
        );
        let cleared = f.read().unwrap();
        assert_eq!(
            first.effective.values["agents"]["worker"]["effort"],
            "ultra"
        );
        assert!(cleared.effective.values["agents"]["worker"]["effort"].is_null());
        fs::remove_file(f.root().join(".git/mdkg/config.local.json")).unwrap();
        let fallback = f.read().unwrap();
        assert_eq!(
            fallback.effective.values["agents"]["worker"]["model"],
            "user"
        );
        assert_eq!(
            fallback.effective.origins["/agents/worker/model"].layer,
            Layer::User
        );
    }

    #[test]
    fn missing_or_invalid_sources_never_fall_back_to_project_only_values() {
        let f = Fixture::new();
        f.definition("codex");
        for bytes in [
            b"{".as_slice(),
            br#"{"agents":{},"agents":{}}"#,
            &vec![b' '; (1 << 20) + 1],
        ] {
            f.bytes("home/.config/mdkg/config.json", bytes);
            assert!(f.read().is_err());
        }
        f.bytes("home/.config/mdkg/config.json", b"{}");
        fs::remove_file(f.root().join(".mdkg/config.json")).unwrap();
        assert!(f.read().is_err());
        f.write("project/.mdkg/config.json", &json!({"schema_version":9999}));
        f.bytes("home/.config/mdkg/config.json", b"{");
        assert!(!f.read().unwrap_err().message.contains("home/.config"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_optional_inputs_are_refused_even_when_the_target_is_missing() {
        let f = Fixture::new();
        let user = f.0.join("home/.config/mdkg/config.json");
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("missing", &user).unwrap();
        assert!(f.read().is_err());
    }

    #[test]
    fn runtime_without_committed_graph_authority_is_refused() {
        let f = Fixture::new();
        let root = f.0.join("standalone");
        f.write(
            "standalone/.mdkg/config.json",
            &json!({"agents":{"legacy":{}}}),
        );
        assert!(capture(&root, Environment::default()).is_err());
    }
}
