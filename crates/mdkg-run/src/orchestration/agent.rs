//! Committed agent definitions, separate from machine-local session state.

use std::fs;
use std::path::{Path, PathBuf};

use mdkg_format::settings::{Input, Layer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Harness, Role, RunError, policy::Policy, state};

pub const SCHEMA: &str = "mdkg.agent.v1";
const CAP: u64 = 1 << 20;

fn reports_to_default() -> String {
    "orchestrator".into()
}

/// One `.mdkg/agents/<name>.json` file. The filename names the actor.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = definition_schema)]
pub struct Definition {
    #[schemars(schema_with = "schema_field")]
    pub schema: String,
    /// Reserved for remote definitions. Accepted and preserved, but not
    /// resolved by this release; a launcher refuses it explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "extends_field")]
    pub extends: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protect: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Map<String, Value>>,
    /// The actor receiving deterministic turn receipts.
    #[serde(default = "reports_to_default")]
    pub reports_to: String,
    /// Cross-harness preset. This release supports auto only.
    #[serde(default = "mode_default")]
    #[schemars(schema_with = "mode_field")]
    pub mode: String,
    /// Override the preset with the harness's native sandbox value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Override the preset with the harness's native approval value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<String>,
}

fn mode_default() -> String {
    "auto".into()
}

fn mode_field(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "const": "auto" })
}

fn extends_field(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["string", "null"], "minLength": 1 })
}

fn definition_schema(schema: &mut schemars::Schema) {
    schema.insert("anyOf".into(), json!([
        {"required": ["harness", "role"], "properties": {"harness": {"type":"string"}, "role": {"type":"string"}}},
        {"required": ["extends"], "properties": {"extends": {"type":"string", "minLength":1}}}
    ]));
}

/// Validation shared by every execution path, including saved agents.
pub fn check_execution(config: &Value, name: &str) -> Result<(), RunError> {
    let entry = config.get("agents").and_then(|a| a.get(name));
    if entry
        .and_then(|a| a.get("extends"))
        .is_some_and(|v| !v.is_null())
    {
        return Err(bad(format!(
            "agent {name}: extends is reserved; remote definitions are not implemented"
        )));
    }
    if entry
        .and_then(|a| a.get("mode"))
        .is_some_and(|v| v != "auto")
    {
        return Err(bad(format!(
            "agent {name}: mode must be auto; other presets are reserved for a later release"
        )));
    }
    for field in ["sandbox", "approvals"] {
        if entry
            .and_then(|a| a.get(field))
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(bad(format!("agent {name}: {field} must be a string")));
        }
    }
    Ok(())
}

fn schema_field(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "const": SCHEMA })
}

fn bad(message: impl Into<String>) -> RunError {
    RunError::usage("usage.invalid_arguments", message)
}

pub fn check_name(name: &str) -> Result<(), RunError> {
    state::check_name(name)?;
    if name == "defaults" {
        return Err(bad("defaults is reserved for the shared turn policy"));
    }
    Ok(())
}

impl Definition {
    pub fn parse(name: &str, value: Value) -> Result<Self, RunError> {
        check_name(name)?;
        let definition: Self =
            serde_json::from_value(value).map_err(|e| bad(format!("agent {name}: {e}")))?;
        if definition.schema != SCHEMA {
            return Err(bad(format!("agent {name}: schema must be {SCHEMA}")));
        }
        if definition.mode != "auto" {
            return Err(bad(format!(
                "agent {name}: mode must be auto; other presets are reserved for a later release"
            )));
        }
        if definition.extends.as_ref().is_some_and(|s| s.is_empty()) {
            return Err(bad(format!("agent {name}: extends must not be empty")));
        }
        if definition.extends.is_none()
            && (definition.harness.is_none() || definition.role.is_none())
        {
            return Err(bad(format!("agent {name}: harness and role are required")));
        }
        if definition.cwd.is_some() && definition.worktree == Some(true) {
            return Err(bad(format!("agent {name}: use cwd or worktree, not both")));
        }
        state::check_configured_effort(definition.harness, definition.effort.as_deref()).map_err(
            |mut error| {
                error.message = format!("agent {name}: {}", error.message);
                error
            },
        )?;
        state::check_name(&definition.reports_to)?;
        if let (Some(harness), Some(role)) = (definition.harness, definition.role) {
            state::check_native(
                harness,
                role,
                &definition.mode,
                definition.sandbox.as_deref(),
                definition.approvals.as_deref(),
            )?;
        }
        Policy::for_agent(&json!({"agents": {name: &definition}}), name)?;
        Ok(definition)
    }
}

fn regular_dir(path: &Path) -> Result<bool, RunError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(bad(format!(
            "{} must be a directory, not a link or file",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(RunError::io("can't inspect agent definitions", e)),
    }
}

pub fn directory(root: &Path) -> Result<PathBuf, RunError> {
    let mdkg = root.join(".mdkg");
    regular_dir(&mdkg)?;
    let dir = mdkg.join("agents");
    regular_dir(&dir)?;
    Ok(dir)
}

pub fn path(root: &Path, name: &str) -> Result<PathBuf, RunError> {
    check_name(name)?;
    Ok(directory(root)?.join(format!("{name}.json")))
}

pub fn read(path: &Path, name: &str) -> Result<Definition, RunError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| RunError::io("can't inspect agent definition", e))?;
    if !meta.is_file() {
        return Err(bad(format!("{} must be a regular file", path.display())));
    }
    let bytes = mdkg_coord_sqlite::git::read_capped(path, CAP + 1)
        .map_err(|e| RunError::io("can't read agent definition", e))?;
    if bytes.len() as u64 > CAP {
        return Err(bad("agent definition exceeds 1 MiB"));
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|e| bad(format!("{} isn't JSON: {e}", path.display())))?;
    Definition::parse(name, value)
}

fn definition_paths(root: &Path) -> Result<Vec<PathBuf>, RunError> {
    let dir = directory(root)?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RunError::io("can't list agent definitions", e)),
    };
    let mut files = entries
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RunError::io("can't list agent definitions", e))?;
    files.sort();
    files.retain(|path| {
        path.extension().is_some_and(|e| e == "json")
            && !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
    });
    Ok(files)
}

/// Raw definition patches at project priority. Retain authored values and
/// source paths; serde defaults must not become falsely project-authored.
/// Parsing is bounded and rejects duplicate keys and links. Typed validation
/// belongs to the consumer after all layers have been merged.
pub fn layer_inputs(root: &Path) -> Result<Vec<Input>, RunError> {
    definition_paths(root)?
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .ok_or_else(|| bad("agent definition filename must be UTF-8"))?;
            check_name(name)?;
            let mut input = Input::read(Layer::Project, &path)
                .map_err(|e| bad(e.to_string()))?
                .ok_or_else(|| bad("agent definition disappeared while reading configuration"))?;
            input.value = json!({"agents": {name: input.value}});
            Input::new(input.origin, input.value).map_err(|e| bad(e.to_string()))
        })
        .collect()
}

/// Resolve raw file definitions using the same whole-entry replacement as
/// authored management reads, within the project layer only. Lower user values
/// and higher machine-local/flag values retain their ordinary precedence.
/// `definitions` must be the captured result of [`layer_inputs`]; none of the
/// captured project or definition files are reread or changed here.
///
/// A scoped edit candidate is resolved at its actual layer. A project candidate
/// is transformed before merging, while a user/local candidate is passed beside
/// the transformed captured project. The graph resolver checks all identities
/// and source paths; typed execution validation belongs to the consumer.
pub fn resolve_layers(
    project: &mdkg_graph::config::layers::Project,
    definitions: &[Input],
    replacement: Option<&Input>,
    flags: Option<&Value>,
) -> Result<mdkg_graph::config::layers::Resolved, mdkg_graph::GraphError> {
    let mut input = replacement
        .filter(|candidate| candidate.origin.layer == Layer::Project)
        .unwrap_or_else(|| project.project_input())
        .clone();
    if let Some(agents) = input.value.get_mut("agents").and_then(Value::as_object_mut) {
        for definition in definitions {
            if let Some(entries) = definition.value.get("agents").and_then(Value::as_object) {
                for name in entries.keys() {
                    agents.remove(name);
                }
            }
        }
    }
    let mut replacements = vec![input];
    if let Some(replacement) = replacement.filter(|r| r.origin.layer != Layer::Project) {
        replacements.push(replacement.clone());
    }
    project.resolve_replacements(&replacements, definitions, flags)
}

/// Overlay files on legacy inline entries. File definitions replace an
/// entry whole; shared `agents.defaults` remains the policy fallback.
pub fn merge(root: &Path, config: &mut Value) -> Result<(), RunError> {
    for path in definition_paths(root)? {
        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .ok_or_else(|| bad("agent definition filename must be UTF-8"))?;
        let definition = read(&path, name)?;
        if config.is_null() {
            *config = json!({});
        }
        let object = config
            .as_object_mut()
            .ok_or_else(|| bad("agent configuration must be an object"))?;
        let agents = object.entry("agents").or_insert_with(|| json!({}));
        if agents.is_null() {
            *agents = json!({});
        }
        agents
            .as_object_mut()
            .ok_or_else(|| bad("agents must be an object"))?
            .insert(
                name.into(),
                serde_json::to_value(definition).map_err(|e| bad(e.to_string()))?,
            );
    }
    Ok(())
}

/// Publish complete JSON atomically. Adding never overwrites a definition;
/// editing first checks that the existing destination is a regular file.
pub fn write(
    root: &Path,
    name: &str,
    definition: &Definition,
    replace: bool,
) -> Result<PathBuf, RunError> {
    let path = path(root, name)?;
    let dir = path.parent().expect("definition has a parent");
    fs::create_dir_all(dir)
        .map_err(|e| RunError::io("can't create agent definitions folder", e))?;
    if replace {
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| RunError::io("can't inspect agent definition", e))?;
        if !meta.is_file() {
            return Err(bad(format!("{} must be a regular file", path.display())));
        }
    }
    let mut bytes = serde_json::to_vec_pretty(definition).map_err(|e| bad(e.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > CAP {
        return Err(bad("agent definition exceeds 1 MiB"));
    }
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| RunError::io("can't create agent definition", e))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| RunError::io("can't write agent definition", e))?;
        if replace {
            fs::rename(&temp, &path)
        } else {
            fs::hard_link(&temp, &path)
        }
        .map_err(|e| RunError::io("can't publish agent definition", e))?;
        Ok(path)
    })();
    let _ = fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_format::settings::{Environment, Origin};
    use mdkg_graph::config::layers::Project;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("mdkg-agent-layers-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(&root).unwrap();
            let fixture = Self(fs::canonicalize(root).unwrap());
            for dir in ["project/.git/objects", "project/.git/refs"] {
                fs::create_dir_all(fixture.0.join(dir)).unwrap();
            }
            fixture.write("project/.git/HEAD", "ref: refs/heads/main\n");
            fixture.write("project/.git/config", "[core]\n bare=false\n");
            fixture
        }
        fn write(&self, path: &str, bytes: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn captured_agent_layers_replace_inline_and_preview_each_scope_without_authored_changes() {
        let fixture = Fixture::new();
        let project_value = json!({
            "schema_version":3, "format_version":"0.7",
            "graph_id":"0192f3a0-0000-7000-8000-000000000000",
            "agents":{
                "astra":{"harness":"codex","role":"worker","model":"inline","effort":"ultra",
                    "policy":{"inline_only":true}},
                "keep":{"model":"kept"}, "defaults":{"turn":{"max_turns":3}}
            }
        });
        let project_path = fixture.write("project/.mdkg/config.json", &project_value.to_string());
        let user_path = fixture.write(
            "home/.config/mdkg/config.json",
            &json!({
                "agents":{"astra":{"model":"user-model","instructions":"user instructions"}}
            })
            .to_string(),
        );
        let local_path = fixture.write(
            "project/.git/mdkg/config.local.json",
            &json!({
                "agents":{"astra":{"instructions":"local instructions"}}
            })
            .to_string(),
        );
        let file_path = fixture.write(
            "project/.mdkg/agents/astra.json",
            &json!({
                "schema":SCHEMA,"harness":"opencode","role":"worker"
            })
            .to_string(),
        );
        let root = fixture.0.join("project");
        let home = fixture.0.join("home");
        let project = Project::capture(
            &root,
            Environment {
                home: Some(&home),
                ..Environment::default()
            },
        )
        .unwrap();
        let definitions = layer_inputs(&root).unwrap();

        // Management still sees authored file values, never optional overrides.
        let authored = crate::orchestration::load_agent_config(&root).unwrap();
        assert!(authored["agents"]["astra"].get("model").is_none());
        assert!(authored["agents"]["astra"].get("instructions").is_none());
        assert!(authored["agents"]["astra"].get("effort").is_none());

        let before_project = fs::read(&project_path).unwrap();
        let before_file = fs::read(&file_path).unwrap();
        // Resolution must not recapture either project source.
        fs::write(&project_path, "changed project").unwrap();
        fs::write(&file_path, "changed definition").unwrap();
        let out = resolve_layers(&project, &definitions, None, None)
            .unwrap()
            .effective;
        assert_eq!(
            out.values["agents"]["astra"],
            json!({
                "schema":SCHEMA,"harness":"opencode","role":"worker",
                "model":"user-model","instructions":"local instructions"
            })
        );
        assert_eq!(out.values["agents"]["keep"]["model"], "kept");
        assert_eq!(out.values["agents"]["defaults"]["turn"]["max_turns"], 3);
        assert!(!out.origins.contains_key("/agents/astra/effort"));
        assert!(
            !out.origins
                .keys()
                .any(|key| key.starts_with("/agents/astra/policy"))
        );
        assert_eq!(
            out.origins["/agents/astra/harness"].path.as_ref(),
            Some(&file_path)
        );
        assert_eq!(
            out.origins["/agents/astra/model"].path.as_ref(),
            Some(&user_path)
        );
        assert_eq!(
            out.origins["/agents/astra/instructions"].path.as_ref(),
            Some(&local_path)
        );
        for (layer, path, model) in [
            (Layer::User, &user_path, "candidate-user"),
            (Layer::Local, &local_path, "candidate-local"),
            (Layer::Project, &project_path, "user-model"),
        ] {
            let value = if layer == Layer::Project {
                let mut value = project_value.clone();
                value["agents"]["astra"]["model"] = json!("ignored-inline-candidate");
                value["agents"]["keep"]["model"] = json!("candidate-project");
                value
            } else {
                json!({"agents":{"astra":{"model":model}}})
            };
            let candidate = Input::new(Origin::file(layer, path), value).unwrap();
            let out = resolve_layers(&project, &definitions, Some(&candidate), None)
                .unwrap()
                .effective;
            assert_eq!(out.values["agents"]["astra"]["model"], model);
            assert!(out.values["agents"]["astra"].get("effort").is_none());
            if layer == Layer::Project {
                assert_eq!(out.values["agents"]["keep"]["model"], "candidate-project");
            } else {
                assert_eq!(out.origins["/agents/astra/model"].layer, layer);
            }
        }
        let flags = json!({"agents":{"astra":{"model":"flag"}}});
        let out = resolve_layers(&project, &definitions, None, Some(&flags))
            .unwrap()
            .effective;
        assert_eq!(out.values["agents"]["astra"]["model"], "flag");
        assert_eq!(out.origins["/agents/astra/model"].layer, Layer::Flags);
        assert_eq!(project.project_input().value, project_value);
        assert_eq!(fs::read(&project_path).unwrap(), b"changed project");
        assert_eq!(fs::read(&file_path).unwrap(), b"changed definition");
        fs::write(&project_path, before_project).unwrap();
        fs::write(&file_path, before_file).unwrap();
    }

    #[test]
    fn definition_effort_uses_its_harness_and_remote_resolution_stays_reserved() {
        for harness in ["claude", "codex", "opencode"] {
            let definition = json!({
                "schema": SCHEMA, "harness": harness, "role": "worker", "effort": "ultra"
            });
            assert_eq!(
                Definition::parse("astra", definition).is_ok(),
                harness == "codex"
            );
        }
        let remote = json!({"schema": SCHEMA, "extends": "reserved", "effort": "ultra"});
        assert!(Definition::parse("astra", remote.clone()).is_ok());
        assert!(check_execution(&json!({"agents": {"astra": remote}}), "astra").is_err());
        assert!(
            Definition::parse(
                "astra",
                json!({
                    "schema": SCHEMA, "extends": "reserved", "effort": "unknown"
                })
            )
            .is_err()
        );
        assert!(
            Definition::parse(
                "astra",
                json!({
                    "schema": SCHEMA, "role": "worker", "effort": "ultra"
                })
            )
            .is_err()
        );
    }
}
