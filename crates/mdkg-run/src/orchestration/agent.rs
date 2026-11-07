//! Committed agent definitions, separate from machine-local session state.

use std::fs;
use std::path::{Path, PathBuf};

use mdkg_format::settings::{self, Effective, Input, Layer, Origin};
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
    /// How a worker follows the code base (mainRef). `notes`, the default:
    /// in ref mode (`mdkg.graph.ref` set) with a mainRef this clone holds,
    /// each turn `agents up` runs tells the worker how far its branch is
    /// behind, when it is; checkout mode, a level branch or a mainRef not
    /// here give no note. `merge` is accepted but acts as `notes` in this
    /// release: nothing is merged, and the note says to merge it yourself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "follow_field")]
    pub follow: Option<String>,
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

fn follow_field(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "enum": ["notes", "merge"] })
}

fn valid_follow(value: &Value) -> bool {
    matches!(value.as_str(), Some("notes" | "merge"))
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
        .and_then(|a| a.get("follow"))
        .is_some_and(|v| !valid_follow(v))
    {
        return Err(bad(format!("agent {name}: follow must be notes or merge")));
    }
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
        if value.get("follow").is_some_and(|v| !valid_follow(v)) {
            return Err(bad(format!("agent {name}: follow must be notes or merge")));
        }
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
        state::check_reports_to(name, &definition.reports_to)?;
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

/// Locate an explicit filesystem candidate for definition editing. Active
/// definitions are read from a pinned graph authority, never from this path.
pub fn path(root: &Path, name: &str) -> Result<PathBuf, RunError> {
    check_name(name)?;
    Ok(directory(root)?.join(format!("{name}.json")))
}

/// Validate a filesystem edit candidate; this is not an authority read.
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

/// Definition patches at project priority. Operations that also need the
/// config should pin one authority and use [`authority_layer_inputs`].
pub fn layer_inputs(root: &Path) -> Result<Vec<Input>, RunError> {
    authority_layer_inputs(&super::authority(root)?)
}

/// File-backed definition patches from the same pinned authority as the
/// project configuration. Keep raw authored fields and their shared graph
/// origin; typed execution validation is performed by runtime consumers.
pub fn authority_layer_inputs(
    authority: &mdkg_graph::graphref::Authority,
) -> Result<Vec<Input>, RunError> {
    authority
        .definitions
        .iter()
        .filter(|(_, definition)| !definition.inline)
        .map(|(name, definition)| {
            check_name(name)?;
            Input::new(
                Origin::file(Layer::Project, &authority.source),
                json!({"agents": {name: definition.value}}),
            )
            .map_err(|e| bad(e.to_string()))
        })
        .collect()
}

/// Refuse changes to graph-owned authority in one optional layer or flags,
/// even if a later layer would mask them. Compare after applying each patch
/// to the authored snapshot so replacing an enclosing object cannot bypass
/// the check. Flags may still create an ad hoc agent absent from the graph.
pub fn check_authority_override(authored: &Value, optional: &Input) -> Result<(), RunError> {
    if !matches!(
        optional.origin.layer,
        Layer::User | Layer::Local | Layer::Flags
    ) {
        return Err(bad(
            "authority override check expects an optional layer or flags",
        ));
    }
    let defaults = settings::defaults().map_err(|e| bad(e.to_string()))?;
    let project = Input::new(Origin::value(Layer::Project), authored.clone())
        .map_err(|e| bad(e.to_string()))?;
    let mut effective = Effective::new(&defaults).map_err(|e| bad(e.to_string()))?;
    effective.apply(&project).map_err(|e| bad(e.to_string()))?;
    let before = effective.values.clone();
    effective.apply(optional).map_err(|e| bad(e.to_string()))?;
    let after = &effective.values;
    let refused = |field: &str| {
        RunError::usage(
            "run.authority_override",
            format!(
                "the {:?} layer changes committed {field}",
                optional.origin.layer
            ),
        )
    };
    for field in ["branch", "worktree"] {
        if before.get("orchestration").and_then(|v| v.get(field))
            != after.get("orchestration").and_then(|v| v.get(field))
        {
            return Err(refused(field));
        }
    }
    // Optional-only entries are never admitted as agents. They are not
    // authority overrides of a graph-defined actor.
    let names = authored
        .get("agents")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|agents| agents.keys())
        .filter(|name| name.as_str() != "defaults");
    for name in names {
        for field in [
            "role",
            "reports_to",
            "protect",
            "placement",
            "follow",
            "cwd",
            "worktree",
            "base",
        ] {
            let get = |value: &Value| {
                value
                    .get("agents")
                    .and_then(|agents| agents.get(name))
                    .and_then(|agent| agent.get(field))
                    .cloned()
            };
            if get(&before) != get(after) {
                return Err(refused(field));
            }
        }
        // The environment its turns get: a worker can write the
        // machine-local layer, so it must not widen what its next
        // supervisor passes (#17692, orchestrator #17730).
        if env_of(&before, &[name.as_str(), "policy"]) != env_of(after, &[name.as_str(), "policy"])
        {
            return Err(refused("policy.env"));
        }
    }
    if env_of(&before, &["defaults"]) != env_of(after, &["defaults"]) {
        return Err(refused("defaults.env"));
    }
    Ok(())
}

/// `agents.<path…>.env` in `config`, if set.
fn env_of<'a>(config: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(config.get("agents")?, |v, k| v.get(*k))?
        .get("env")
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
    use mdkg_format::Code;
    use mdkg_graph::GraphError;

    let settings_error = |error: settings::Error| {
        let code = match error.kind {
            settings::ErrorKind::Invalid => Code::ConfigInvalid,
            settings::ErrorKind::Read | settings::ErrorKind::TooLarge => Code::ConfigUnreadable,
        };
        let mut result = GraphError::failure(code, error.to_string());
        result.diagnostic.path = error.origin.path.map(|p| p.to_string_lossy().into_owned());
        result
    };
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
    let defaults = settings::defaults().map_err(&settings_error)?;
    let authored = settings::Layers {
        user: None,
        project: Some(input.clone()),
        local: None,
    }
    .resolve(&defaults, definitions, None)
    .map_err(&settings_error)?;
    let mut replacements = vec![input];
    for (layer, path) in [
        (Layer::User, project.paths().user.as_deref()),
        (Layer::Local, project.paths().local.as_deref()),
    ] {
        let Some(path) = path else { continue };
        // Capture each optional source once. A missing source is represented
        // by an empty patch so the resolver cannot reread it later.
        let candidate = match replacement.filter(|r| r.origin.layer == layer) {
            Some(candidate) => candidate.clone(),
            None => match Input::read(layer, path).map_err(&settings_error)? {
                Some(candidate) => candidate,
                None => {
                    Input::new(Origin::file(layer, path), json!({})).map_err(&settings_error)?
                }
            },
        };
        check_authority_override(&authored.values, &candidate).map_err(|error| {
            GraphError::usage(
                Code::parse(&error.code).unwrap_or(Code::ConfigInvalid),
                error.message,
            )
        })?;
        replacements.push(candidate);
    }
    if let Some(flags) = flags {
        let flags =
            Input::new(Origin::value(Layer::Flags), flags.clone()).map_err(&settings_error)?;
        check_authority_override(&authored.values, &flags).map_err(|error| {
            GraphError::usage(
                Code::parse(&error.code).unwrap_or(Code::ConfigInvalid),
                error.message,
            )
        })?;
    }
    if let Some(replacement) = replacement.filter(|r| {
        !matches!(r.origin.layer, Layer::Project | Layer::User | Layer::Local)
            || (r.origin.layer == Layer::User && project.paths().user.is_none())
            || (r.origin.layer == Layer::Local && project.paths().local.is_none())
    }) {
        // Preserve the resolver's invalid-source refusal for other layers or
        // optional sources that discovery did not select.
        replacements.push(replacement.clone());
    }
    project.resolve_replacements(&replacements, definitions, flags)
}

/// Replace the value with one committed authority, including file-backed
/// definitions over legacy inline entries.
pub fn merge(root: &Path, config: &mut Value) -> Result<(), RunError> {
    *config = authority_config(&super::authority(root)?)?;
    Ok(())
}

/// Validate the surviving committed definitions before optional layers can
/// hide an invalid entry. File definitions replace inline entries whole.
pub fn validate_authority(authority: &mdkg_graph::graphref::Authority) -> Result<(), RunError> {
    for (name, entry) in &authority.definitions {
        if !entry.inline {
            Definition::parse(name, entry.value.clone())?;
            continue;
        }
        check_name(name)?;
        let fields = entry
            .value
            .as_object()
            .ok_or_else(|| bad(format!("agent {name}: definition must be an object")))?;
        let cwd = match fields.get("cwd") {
            None | Some(Value::Null) => false,
            Some(Value::String(_)) => true,
            Some(_) => return Err(bad(format!("agent {name}: cwd must be a string"))),
        };
        let worktree = match fields.get("worktree") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(bad(format!("agent {name}: worktree must be a boolean"))),
        };
        if cwd && worktree {
            return Err(bad(format!("agent {name}: use cwd or worktree, not both")));
        }
        check_execution(&authority.config, name)?;
    }
    Ok(())
}

/// Materialize the authored agent map from one pinned graph snapshot.
/// File definitions replace legacy inline entries whole, just as in
/// management reads; inline entries retain their authored values.
pub fn authority_config(authority: &mdkg_graph::graphref::Authority) -> Result<Value, RunError> {
    validate_authority(authority)?;
    let mut config = authority.config.clone();
    if !config.is_object() {
        return Err(bad("agent configuration must be an object"));
    }
    for (name, entry) in &authority.definitions {
        if entry.inline {
            continue;
        }
        let definition = Definition::parse(name, entry.value.clone())?;
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
                name.clone(),
                serde_json::to_value(definition).map_err(|e| bad(e.to_string()))?,
            );
    }
    Ok(config)
}

/// The `protect` list the graph authority gives `actor`, read from the
/// graph ref in ref mode, never from a checkout's definition file (#11129).
/// A positively absent checkout graph defines nobody, so it protects
/// nothing extra; any other failure is an error, which a caller refuses on.
pub fn committed_protect(root: &Path, actor: &str) -> Result<Vec<String>, RunError> {
    let Some(authority) = crate::orchestration::authority_or_absent(root)? else {
        return Ok(Vec::new());
    };
    let config = authority_config(&authority)?;
    match config
        .get("agents")
        .and_then(|agents| agents.get(actor))
        .and_then(|agent| agent.get("protect"))
    {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| {
                name.as_str()
                    .map(String::from)
                    .ok_or_else(|| bad("an agent's protect list must hold branch names"))
            })
            .collect(),
        Some(_) => Err(bad("an agent's protect list must be an array")),
    }
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
mod receipt_target_tests {
    use super::*;

    #[test]
    fn committed_inline_definitions_are_validated_before_use() {
        use mdkg_graph::graphref::{Authority, Def};

        for harness in ["claude", "codex", "opencode"] {
            let value = json!({"harness": harness, "role": "worker"});
            let mut authority = Authority {
                source: "graph:fixture".into(),
                graph_ref: Some("refs/heads/mdkg-graph".into()),
                commit: "fixture".into(),
                graph_id: "fixture".into(),
                config: json!({"agents": {"worker": value}}),
                definitions: std::collections::BTreeMap::from([(
                    "worker".into(),
                    Def {
                        blob: "fixture".into(),
                        value: value.clone(),
                        inline: true,
                    },
                )]),
            };
            validate_authority(&authority).unwrap();
            for (field, invalid) in [
                ("cwd", json!(42)),
                ("worktree", json!("yes")),
                ("follow", json!("always")),
                ("extends", json!("elsewhere")),
                ("mode", json!("manual")),
                ("sandbox", json!(false)),
                ("approvals", json!([])),
            ] {
                let mut invalid_value = value.clone();
                invalid_value[field] = invalid;
                authority.config["agents"]["worker"] = invalid_value.clone();
                authority.definitions.get_mut("worker").unwrap().value = invalid_value;
                assert!(validate_authority(&authority).is_err(), "{harness} {field}");
            }
            let both = json!({"cwd":"/worker", "worktree":true});
            authority.config["agents"]["worker"] = both.clone();
            authority.definitions.get_mut("worker").unwrap().value = both;
            assert!(
                validate_authority(&authority).is_err(),
                "{harness} locations"
            );

            // A file replaces the invalid inline entry rather than layering
            // it; only the surviving definition may govern the actor.
            authority.definitions.insert(
                "worker".into(),
                Def {
                    blob: "file".into(),
                    value: json!({"schema":SCHEMA,"harness":harness,"role":"worker"}),
                    inline: false,
                },
            );
            let resolved = authority_config(&authority).unwrap();
            assert_eq!(resolved["agents"]["worker"]["harness"], harness);
            assert!(resolved["agents"]["worker"].get("cwd").is_none());
        }
    }

    #[test]
    fn optional_layers_cannot_change_or_remove_authority_fields() {
        for harness in ["claude", "codex", "opencode"] {
            let authored = json!({
                "agents":{"worker":{
                    "harness":harness,"role":"worker","reports_to":"lead",
                    "protect":["main"],"placement":{"host":"one"},"follow":"notes",
                    "cwd":"/original","worktree":false,"base":"main","model":"old"
                }},
                "orchestration":{"branch":"agent/{agent}","worktree":".worktrees/{agent}"}
            });
            for layer in [Layer::User, Layer::Local] {
                for (field, changed) in [
                    ("role", json!("reviewer")),
                    ("reports_to", json!("other")),
                    ("protect", json!([])),
                    ("placement", json!({"host":"two"})),
                    ("follow", json!("merge")),
                    ("cwd", json!("/other")),
                    ("worktree", json!(true)),
                    ("base", json!("other")),
                ] {
                    let patch = Input::new(
                        Origin::value(layer),
                        json!({"agents":{"worker":{(field):changed}}}),
                    )
                    .unwrap();
                    assert_eq!(
                        check_authority_override(&authored, &patch)
                            .unwrap_err()
                            .code,
                        "run.authority_override",
                        "{harness} {layer:?} {field}"
                    );
                }
                for field in ["branch", "worktree"] {
                    let patch = Input::new(
                        Origin::value(layer),
                        json!({"orchestration":{(field):"other/{agent}"}}),
                    )
                    .unwrap();
                    assert_eq!(
                        check_authority_override(&authored, &patch)
                            .unwrap_err()
                            .code,
                        "run.authority_override"
                    );
                }
                for patch in [json!({"agents":null}), json!({"orchestration":null})] {
                    let patch = Input::new(Origin::value(layer), patch).unwrap();
                    assert_eq!(
                        check_authority_override(&authored, &patch)
                            .unwrap_err()
                            .code,
                        "run.authority_override"
                    );
                }
                let allowed = Input::new(
                    Origin::value(layer),
                    json!({"agents":{"worker":{"role":"worker","model":"new"}}}),
                )
                .unwrap();
                check_authority_override(&authored, &allowed).unwrap();
                let inactive = Input::new(
                    Origin::value(layer),
                    json!({"agents":{"other":{"role":"reviewer","cwd":"/other"}}}),
                )
                .unwrap();
                check_authority_override(&authored, &inactive).unwrap();
            }
        }
    }

    #[test]
    fn flags_cannot_change_a_committed_agent_but_can_create_an_ad_hoc_one() {
        let authored = json!({"agents":{"worker":{"role":"worker","cwd":"/work"}}});
        for patch in [
            json!({"agents":{"worker":{"role":"reviewer"}}}),
            json!({"agents":{"worker":{"cwd":"/other"}}}),
            json!({"agents":null}),
            json!({"orchestration":{"branch":"other/{agent}"}}),
        ] {
            let patch = Input::new(Origin::value(Layer::Flags), patch).unwrap();
            assert_eq!(
                check_authority_override(&authored, &patch)
                    .unwrap_err()
                    .code,
                "run.authority_override"
            );
        }
        for patch in [
            json!({"agents":{"worker":{"role":"worker","model":"new"}}}),
            json!({"agents":{"adhoc":{"role":"worker","cwd":"/adhoc"}}}),
        ] {
            let patch = Input::new(Origin::value(Layer::Flags), patch).unwrap();
            check_authority_override(&authored, &patch).unwrap();
        }
    }

    #[test]
    fn follow_is_validated_for_each_harness() {
        for harness in ["claude", "codex", "opencode"] {
            let mut value = json!({"schema": SCHEMA, "harness": harness, "role": "worker"});
            assert!(
                Definition::parse("worker", value.clone())
                    .unwrap()
                    .follow
                    .is_none()
            );
            for follow in ["notes", "merge"] {
                value["follow"] = json!(follow);
                assert_eq!(
                    Definition::parse("worker", value.clone())
                        .unwrap()
                        .follow
                        .as_deref(),
                    Some(follow)
                );
                assert!(check_execution(&json!({"agents":{"worker":value}}), "worker").is_ok());
            }
            for invalid in [json!("always"), Value::Null, json!(true)] {
                value["follow"] = invalid;
                assert!(Definition::parse("worker", value.clone()).is_err());
                assert!(check_execution(&json!({"agents":{"worker":value}}), "worker").is_err());
            }
        }
    }

    #[test]
    fn definitions_refuse_self_and_reserved_receipt_targets() {
        let value = json!({"schema": SCHEMA, "harness": "codex", "role": "worker"});
        assert_eq!(
            Definition::parse("worker", value.clone())
                .unwrap()
                .reports_to,
            "orchestrator"
        );
        assert!(
            Definition::parse("orchestrator", value.clone()).is_err(),
            "the omitted default must not report to itself"
        );
        for target in ["worker", "mdkg:receipts", "mdkg:other", "MDKG:receipts"] {
            let mut invalid = value.clone();
            invalid["reports_to"] = target.into();
            assert!(Definition::parse("worker", invalid).is_err(), "{target}");
        }
        for name in ["mdkg:receipts", "MDKG:other"] {
            assert!(Definition::parse(name, value.clone()).is_err());
        }
        let mut fixed = value;
        fixed["reports_to"] = "lead".into();
        assert_eq!(
            Definition::parse("orchestrator", fixed).unwrap().reports_to,
            "lead"
        );
    }

    #[test]
    fn stale_inline_receipt_targets_do_not_override_saved_state() {
        let saved = state::resolve(
            "worker",
            None,
            state::SpecFlags {
                harness: Some(Harness::Codex),
                role: Some(Role::Worker),
                reports_to: Some("lead".into()),
                ..Default::default()
            },
            Path::new("/repo/worker"),
        )
        .unwrap();
        for target in [json!("worker"), json!("mdkg:receipts"), Value::Null] {
            let config = json!({"agents": {"worker": {"reports_to": target}}});
            // Legacy inline entries initialize new agents only. Execution
            // validates the resolved spec, not these possibly stale fields.
            check_execution(&config, "worker").unwrap();
            let resolved = state::resolve(
                "worker",
                Some(saved.clone()),
                state::SpecFlags::default(),
                Path::new("/repo/worker"),
            )
            .unwrap();
            assert_eq!(resolved.spec.reports_to, "lead");
        }
    }
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
            fs::create_dir_all(fixture.0.join("project")).unwrap();
            fixture.git(&["init", "-q", "-b", "main"]);
            fixture
        }
        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(self.0.join("project"))
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
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn commit_graph(&self) {
            self.git(&["add", "--", ".mdkg"]);
            self.git(&["commit", "-q", "-m", "fixture graph"]);
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

    /// git.protect's list comes from the graph ref, whatever the agent's
    /// checkout file says or whether it's there (#11129).
    #[test]
    fn the_protect_list_is_read_from_the_graph_ref_not_the_checkout_file() {
        let f = Fixture::new();
        let graph_id = "0192f3a0-0000-7000-8000-000000000000";
        f.write(
            "project/.mdkg/config.json",
            &json!({"schema_version":3,"format_version":"0.7","graph_id":graph_id}).to_string(),
        );
        let file = f.write(
            "project/.mdkg/agents/lane-a.json",
            &json!({"schema":SCHEMA,"harness":"codex","role":"worker","protect":["release"]})
                .to_string(),
        );
        f.commit_graph();
        f.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        f.git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        f.git(&[
            "config",
            &format!("mdkg.graph.{graph_id}.ref"),
            "refs/heads/mdkg-graph",
        ]);
        let root = f.0.join("project");
        assert_eq!(committed_protect(&root, "lane-a").unwrap(), ["release"]);
        // The agent empties its own uncommitted file, then deletes it.
        fs::write(
            &file,
            json!({"schema":SCHEMA,"harness":"codex","role":"worker"}).to_string(),
        )
        .unwrap();
        assert_eq!(committed_protect(&root, "lane-a").unwrap(), ["release"]);
        fs::remove_file(&file).unwrap();
        assert_eq!(committed_protect(&root, "lane-a").unwrap(), ["release"]);
        assert!(committed_protect(&root, "nobody").unwrap().is_empty());
        // An unreadable graph ref is an error, never an empty list.
        f.git(&["update-ref", "-d", "refs/heads/mdkg-graph"]);
        assert!(committed_protect(&root, "lane-a").is_err());
    }

    #[test]
    fn a_worker_writable_layer_cannot_change_an_agents_environment() {
        let fixture = Fixture::new();
        fixture.write(
            "project/.mdkg/config.json",
            &json!({
                "schema_version":3, "format_version":"0.7",
                "graph_id":"0192f3a0-0000-7000-8000-000000000000",
                "agents":{"worker":{"role":"worker","policy":{"env":{"allow":["CARGO_HOME"]}}}}
            })
            .to_string(),
        );
        let local = fixture.write("project/.git/mdkg/config.local.json", "{}");
        let root = fixture.0.join("project");
        let home = fixture.0.join("home");
        fixture.commit_graph();
        let authority = crate::orchestration::authority(&root).unwrap();
        let project = Project::from_authority(
            &authority,
            &root,
            Environment {
                home: Some(&home),
                ..Default::default()
            },
        )
        .unwrap();
        let refused = |patch: Value, what: &str| {
            fs::write(&local, patch.to_string()).unwrap();
            let e = resolve_layers(&project, &[], None, None).unwrap_err();
            assert!(
                e.to_string()
                    .contains(&format!("Local layer changes committed {what}")),
                "{e}"
            );
            assert_eq!(e.diagnostic.code.as_str(), "run.authority_override");
        };
        // Widening, opting out, narrowing, or replacing the whole policy.
        refused(
            json!({"agents":{"worker":{"policy":{"env":{"allow":["CARGO_HOME","GITHUB_TOKEN"]}}}}}),
            "policy.env",
        );
        refused(
            json!({"agents":{"worker":{"policy":{"env":{"inherit":true}}}}}),
            "policy.env",
        );
        refused(
            json!({"agents":{"worker":{"policy":{"env":null}}}}),
            "policy.env",
        );
        refused(json!({"agents":{"worker":{"policy":null}}}), "policy.env");
        refused(
            json!({"agents":{"defaults":{"env":{"inherit":true}}}}),
            "defaults.env",
        );
        // Other policy fields still apply from the local layer.
        fs::write(
            &local,
            json!({"agents":{"worker":{"policy":{"env":{"allow":["CARGO_HOME"]},"timeout":900}}}})
                .to_string(),
        )
        .unwrap();
        let resolved = resolve_layers(&project, &[], None, None).unwrap();
        assert_eq!(
            resolved.effective.values["agents"]["worker"]["policy"]["env"],
            json!({"allow":["CARGO_HOME"]})
        );
    }

    #[test]
    fn each_optional_authority_override_is_refused_before_layer_merging() {
        let fixture = Fixture::new();
        fixture.write(
            "project/.mdkg/config.json",
            &json!({
                "schema_version":3, "format_version":"0.7",
                "graph_id":"0192f3a0-0000-7000-8000-000000000000",
                "agents":{"worker":{"role":"worker","model":"project"}}
            })
            .to_string(),
        );
        let user = fixture.write(
            "home/.config/mdkg/config.json",
            &json!({"agents":{"worker":{"role":"reviewer"}}}).to_string(),
        );
        let local = fixture.write(
            "project/.git/mdkg/config.local.json",
            &json!({"agents":{"worker":{"role":"worker"}}}).to_string(),
        );
        let root = fixture.0.join("project");
        let home = fixture.0.join("home");
        fixture.commit_graph();
        let authority = crate::orchestration::authority(&root).unwrap();
        let project = Project::from_authority(
            &authority,
            &root,
            Environment {
                home: Some(&home),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            resolve_layers(&project, &[], None, None)
                .unwrap_err()
                .to_string()
                .contains("User layer changes committed role")
        );
        fs::write(
            user,
            json!({"agents":{"worker":{"model":"user"}}}).to_string(),
        )
        .unwrap();
        fs::write(
            &local,
            json!({"agents":{"worker":{"role":"reviewer"}}}).to_string(),
        )
        .unwrap();
        assert!(
            resolve_layers(&project, &[], None, None)
                .unwrap_err()
                .to_string()
                .contains("Local layer changes committed role")
        );
        fs::write(
            local,
            json!({"agents":{"worker":{"model":"local"}}}).to_string(),
        )
        .unwrap();
        let resolved = resolve_layers(&project, &[], None, None).unwrap();
        assert_eq!(
            resolved.effective.values["agents"]["worker"]["model"],
            "local"
        );
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
        fixture.commit_graph();
        let authority = crate::orchestration::authority(&root).unwrap();
        let source = PathBuf::from(&authority.source);
        let project = Project::from_authority(
            &authority,
            &root,
            Environment {
                home: Some(&home),
                ..Environment::default()
            },
        )
        .unwrap();
        let definitions = authority_layer_inputs(&authority).unwrap();

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
            Some(&source)
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
            (Layer::Project, &source, "user-model"),
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
