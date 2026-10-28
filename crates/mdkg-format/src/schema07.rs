//! Format 0.7's node schema: which keys each type may carry, and their
//! kinds. `schema/node-0.7.json` is the normative definition (format 0.7,
//! section 3.5); this module reads it. In a format 0.7 graph, templates
//! are authoring aids only and never change what is valid.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::node::{KeyKind, TemplateSchema};

/// The schema artifact, as shipped.
pub const NODE_SCHEMA_JSON: &str = include_str!("../schema/node-0.7.json");

/// Each type's keys and kinds, common keys included.
pub fn schemas() -> &'static HashMap<String, TemplateSchema> {
    static SCHEMAS: OnceLock<HashMap<String, TemplateSchema>> = OnceLock::new();
    SCHEMAS.get_or_init(|| parse(NODE_SCHEMA_JSON).unwrap_or_default())
}

/// The keys a type requires, from the artifact.
pub fn required(node_type: &str) -> Vec<String> {
    serde_json::from_str::<Value>(NODE_SCHEMA_JSON)
        .ok()
        .and_then(|v| {
            v.pointer(&format!("/types/{node_type}/required"))?
                .as_array()
                .cloned()
        })
        .map(|a| {
            a.iter()
                .filter_map(|k| k.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn kind(entry: &Value) -> Option<KeyKind> {
    match entry.get("kind")?.as_str()? {
        "scalar" => Some(KeyKind::Scalar),
        "list" => Some(KeyKind::List),
        "boolean" => Some(KeyKind::Boolean),
        _ => None,
    }
}

/// Parses an artifact; `None` if it isn't well formed.
pub fn parse(text: &str) -> Option<HashMap<String, TemplateSchema>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let common: Vec<(String, KeyKind)> = v
        .get("common_keys")?
        .as_object()?
        .iter()
        .map(|(k, e)| Some((k.clone(), kind(e)?)))
        .collect::<Option<_>>()?;
    let mut out = HashMap::new();
    for (node_type, t) in v.get("types")?.as_object()? {
        let mut keys: Vec<(String, KeyKind)> = t
            .get("keys")?
            .as_object()?
            .iter()
            .map(|(k, e)| Some((k.clone(), kind(e)?)))
            .collect::<Option<_>>()?;
        for (k, kind) in &common {
            if !keys.iter().any(|(have, _)| have == k) {
                keys.push((k.clone(), *kind));
            }
        }
        out.insert(
            node_type.clone(),
            TemplateSchema::from_keys(node_type.clone(), keys),
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ALLOWED_TYPES;

    #[test]
    fn the_artifact_covers_every_type() {
        let s = parse(NODE_SCHEMA_JSON).expect("the artifact parses");
        assert_eq!(s.len(), ALLOWED_TYPES.len());
        for t in ALLOWED_TYPES {
            let schema = &s[*t];
            for key in ["id", "type", "title", "created", "updated", "legacy_id"] {
                assert!(schema.allows(key), "{t} lacks {key}");
            }
            for key in required(t) {
                assert!(schema.allows(&key), "{t} requires {key} it doesn't allow");
            }
        }
        assert_eq!(s["task"].kind("relates"), Some(KeyKind::List));
        assert!(s["manifest"].allows("profile"), "a compatibility key");
        let v: Value = serde_json::from_str(NODE_SCHEMA_JSON).expect("json");
        assert_eq!(v["format_version"], "0.7");
    }

    /// The artifact's reference constraints are the validator's: a key it
    /// constrains refuses a value that isn't a ref, and a key it leaves as
    /// `text` takes any string.
    #[test]
    fn reference_constraints_match_the_validator() {
        use crate::node::{FormatVersion, ParseOptions, parse_node};
        let v: Value = serde_json::from_str(NODE_SCHEMA_JSON).expect("json");
        let schemas = schemas();
        let status = vec!["todo".to_string()];
        let opts = ParseOptions {
            work_status: &status,
            priority_min: 0,
            priority_max: 9,
            templates: schemas,
            allow_extension_keys: true,
            format: FormatVersion::V07,
        };
        let refs = ["node-id", "ref-or-uri", "unpinned-ref", "dec-id"];
        for (t, spec) in v["types"].as_object().expect("types") {
            let mut base = format!(
                "---\nid: {t}-0192f3a4-0000-7000-8000-000000000001\ntype: {t}\ntitle: T\ncreated: 2026-10-24\nupdated: 2026-10-24\n"
            );
            if t == "dec" {
                base.push_str("status: accepted\n");
            } else if required(t).iter().any(|k| k == "status") {
                base.push_str("status: todo\n");
            }
            if t == "goal" {
                base.push_str("goal_state: active\ngoal_condition: c\n");
            }
            let node = |extra: &str| parse_node(&format!("{base}{extra}---\n"), "f.md", &opts);
            assert!(node("").is_ok(), "{t}: {:?}", node(""));
            for (key, e) in spec["keys"].as_object().expect("keys") {
                if ["id", "type", "title", "created", "updated", "status"].contains(&key.as_str()) {
                    continue;
                }
                let bad = match e["kind"].as_str() {
                    Some("list") => "[not-a-ref!]",
                    Some("scalar") => "not-a-ref!",
                    _ => continue,
                };
                let line = format!("{key}: {bad}\n");
                match e.get("constraint").and_then(Value::as_str) {
                    Some(c) if refs.contains(&c) => {
                        assert!(node(&line).is_err(), "{t}.{key} ({c}) took {bad}");
                    }
                    None => assert!(
                        node(&line).is_ok(),
                        "{t}.{key} (text) refused {bad}: {:?}",
                        node(&line)
                    ),
                    Some(_) => {}
                }
            }
        }
    }
}
