//! Shared by the integration tests: isolate commands from the outer
//! launcher and check JSON envelopes against their published schemas, so
//! a field that drifts from `schemas/` fails whichever test prints it.
#![allow(dead_code)]

pub mod temp;

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
    let path = root.join(".mdkg/agents").join(format!("{name}.json"));
    let mut definition: Value = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .or_else(|| {
            fs::read(root.join(".mdkg/config.json"))
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
            path,
            serde_json::to_vec_pretty(&definition).expect("definition JSON"),
        )
        .expect("fixture definition");
    }
    out
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

/// The keywords the generated schemas use. Anything else fails the check,
/// so a schemars upgrade that emits new keywords can't pass unchecked.
const KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "oneOf",
    "minimum",
    "minLength",
    "prefixItems",
    "minItems",
    "maxItems",
    "$ref",
    "description",
    // Annotation only: an omitted input field's default value.
    "default",
    "format",
    "title",
    "$schema",
    "$defs",
    // An annotation: the lease-authority contract a build speaks.
    "x-mdkg-lease-authority",
];

pub fn schema(command: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(format!("mdkg.{command}.v1.json"));
    serde_json::from_slice(&fs::read(&path).expect("schema file")).expect("schema JSON")
}

/// Checks `value` against the subset of JSON Schema the generated schemas
/// use: `type`, `properties`, `required`, `additionalProperties`, `items`,
/// `prefixItems` (tuples), `minItems`, `maxItems`, `enum`, `const`, `anyOf`,
/// `oneOf`, `minimum` and local `$ref`s. Pushes one line per violation.
pub fn check(root: &Value, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(rules) = schema.as_object() else {
        if schema != &Value::Bool(true) {
            errors.push(format!("{at}: schema isn't an object: {schema}"));
        }
        return;
    };
    for key in rules.keys() {
        if !KEYWORDS.contains(&key.as_str()) {
            errors.push(format!("{at}: unsupported schema keyword {key}"));
        }
    }
    if let Some(r) = rules.get("$ref").and_then(Value::as_str) {
        match r
            .strip_prefix("#/$defs/")
            .and_then(|name| root["$defs"].get(name))
        {
            Some(target) => check(root, target, value, at, errors),
            None => errors.push(format!("{at}: unresolved $ref {r}")),
        }
        return;
    }
    if let Some(any) = rules.get("anyOf").and_then(Value::as_array) {
        let fits = any.iter().any(|s| {
            let mut e = Vec::new();
            check(root, s, value, at, &mut e);
            e.is_empty()
        });
        if !fits {
            errors.push(format!("{at}: matches no anyOf branch"));
        }
    }
    if let Some(one) = rules.get("oneOf").and_then(Value::as_array) {
        let fitting = one
            .iter()
            .filter(|s| {
                let mut e = Vec::new();
                check(root, s, value, at, &mut e);
                e.is_empty()
            })
            .count();
        if fitting != 1 {
            errors.push(format!(
                "{at}: matches {fitting} oneOf branches, not exactly one"
            ));
        }
    }
    if let Some(t) = rules.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let ok = types.iter().any(|t| match *t {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            _ => false,
        });
        if !ok {
            errors.push(format!("{at}: expected {types:?}, got {value}"));
            return;
        }
    }
    if let Some(c) = rules.get("const")
        && c != value
    {
        errors.push(format!("{at}: expected {c}, got {value}"));
    }
    if let Some(e) = rules.get("enum").and_then(Value::as_array)
        && !e.contains(value)
    {
        errors.push(format!("{at}: {value} isn't one of the enum values"));
    }
    if let (Some(min), Some(n)) = (rules.get("minimum").and_then(Value::as_f64), value.as_f64())
        && n < min
    {
        errors.push(format!("{at}: {n} is below {min}"));
    }
    if let (Some(min), Some(text)) = (
        rules.get("minLength").and_then(Value::as_u64),
        value.as_str(),
    ) && (text.chars().count() as u64) < min
    {
        errors.push(format!("{at}: shorter than minLength"));
    }
    if let Some(map) = value.as_object() {
        for key in rules
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !map.contains_key(key) {
                errors.push(format!("{at}: missing required {key}"));
            }
        }
        let props = rules.get("properties").and_then(Value::as_object);
        for (key, v) in map {
            match props.and_then(|p| p.get(key)) {
                Some(s) => check(root, s, v, &format!("{at}.{key}"), errors),
                None => match rules.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        errors.push(format!("{at}: unexpected property {key}"));
                    }
                    Some(s @ Value::Object(_)) => check(root, s, v, &format!("{at}.{key}"), errors),
                    _ => {}
                },
            }
        }
    }
    if let Some(list) = value.as_array() {
        if let Some(tuple) = rules.get("prefixItems").and_then(Value::as_array) {
            for (i, (s, v)) in tuple.iter().zip(list).enumerate() {
                check(root, s, v, &format!("{at}[{i}]"), errors);
            }
        }
        let len = list.len() as u64;
        if rules
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|n| len < n)
        {
            errors.push(format!("{at}: fewer than minItems items"));
        }
        if rules
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|n| len > n)
        {
            errors.push(format!("{at}: more than maxItems items"));
        }
    }
    if let (Some(items), Some(list)) = (rules.get("items"), value.as_array()) {
        // After a tuple's prefix, as JSON Schema 2020-12 has it.
        let skip = rules
            .get("prefixItems")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (i, v) in list.iter().enumerate().skip(skip) {
            check(root, items, v, &format!("{at}[{i}]"), errors);
        }
    }
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
