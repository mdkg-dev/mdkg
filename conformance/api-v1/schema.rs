//! Shared conformance schema checks. Producers must declare emitted fields;
//! clients still accept fields and open-set values added by future versions.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

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
    "allOf",
    "if",
    "then",
    "else",
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
    // Annotations: an open string set's known values (A26's `KindName`).
    "examples",
    "x-mdkg-open",
];

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
    if let Some(all) = rules.get("allOf").and_then(Value::as_array) {
        for part in all {
            check(root, part, value, at, errors);
        }
    }
    if let Some(condition) = rules.get("if") {
        let mut mismatch = Vec::new();
        check(root, condition, value, at, &mut mismatch);
        let branch = if mismatch.is_empty() { "then" } else { "else" };
        if let Some(part) = rules.get(branch) {
            check(root, part, value, at, errors);
        }
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

/// Resolve local-file schema references without a network or paths outside
/// the schema directory. Recursive schemas fail explicitly rather than pass
/// through a validator that does not understand them.
pub fn load(path: &Path) -> Value {
    fn expand(value: &Value, root: &Value, path: &Path, stack: &mut Vec<String>) -> Value {
        if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
            let key = format!("{}:{reference}", path.display());
            assert!(!stack.contains(&key), "recursive schema reference: {key}");
            stack.push(key);
            let (file, fragment) = reference.split_once('#').unwrap_or((reference, ""));
            let target_file = if file.is_empty() {
                path.to_path_buf()
            } else {
                assert!(
                    Path::new(file).components().count() == 1
                        && !file.contains(':')
                        && file.ends_with(".json"),
                    "nonlocal schema reference: {reference}"
                );
                path.parent().expect("schema folder").join(file)
            };
            let external;
            let target_root = if file.is_empty() {
                root
            } else {
                external = serde_json::from_slice::<Value>(
                    &std::fs::read(&target_file).expect("referenced schema"),
                )
                .expect("schema JSON");
                &external
            };
            let target = if fragment.is_empty() {
                target_root
            } else {
                target_root.pointer(fragment).expect("schema fragment")
            };
            let resolved = expand(target, target_root, &target_file, stack);
            stack.pop();
            return resolved;
        }
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, _)| {
                        ![
                            "$defs",
                            "x-mdkg-status",
                            "x-mdkg-open",
                            "examples",
                            "$comment",
                        ]
                        .contains(&key.as_str())
                    })
                    .map(|(key, child)| (key.clone(), expand(child, root, path, stack)))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|child| expand(child, root, path, stack))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let root: Value =
        serde_json::from_slice(&std::fs::read(path).expect("schema file")).expect("schema JSON");
    expand(&root, &root, path, &mut Vec::new())
}

/// Validate the producer's current fields as well as JSON Schema constraints.
/// An unconstrained JSON value or an explicit additionalProperties schema is
/// opaque data, such as a message part or an imported record body.
pub fn check_emitted(
    root: &Value,
    schema: &Value,
    value: &Value,
    at: &str,
    errors: &mut Vec<String>,
) {
    check(root, schema, value, at, errors);
    declared(root, schema, value, at, errors);
}
fn declared(root: &Value, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    fn applicable<'a>(root: &'a Value, schema: &'a Value, value: &Value, out: &mut Vec<&'a Value>) {
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            if let Some(target) = root["$defs"].get(name) {
                applicable(root, target, value, out);
            }
            return;
        }
        out.push(schema);
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            for part in parts {
                applicable(root, part, value, out);
            }
        }
        for keyword in ["oneOf", "anyOf"] {
            if let Some(parts) = schema.get(keyword).and_then(Value::as_array) {
                for part in parts {
                    let mut mismatch = Vec::new();
                    check(root, part, value, "$", &mut mismatch);
                    if mismatch.is_empty() {
                        applicable(root, part, value, out);
                    }
                }
            }
        }
        if let Some(condition) = schema.get("if") {
            let mut mismatch = Vec::new();
            check(root, condition, value, "$", &mut mismatch);
            if let Some(part) = schema.get(if mismatch.is_empty() { "then" } else { "else" }) {
                applicable(root, part, value, out);
            }
        }
    }
    let mut parts = Vec::new();
    applicable(root, schema, value, &mut parts);
    if let Some(map) = value.as_object() {
        let mut properties: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
        for part in &parts {
            if let Some(props) = part.get("properties").and_then(Value::as_object) {
                for (key, child) in props {
                    properties.entry(key).or_default().push(child);
                }
            }
        }
        if !properties.is_empty() {
            for (key, child) in map {
                if let Some(schemas) = properties.get(key.as_str()) {
                    for schema in schemas {
                        declared(root, schema, child, &format!("{at}.{key}"), errors);
                    }
                } else if !parts.iter().any(|part| {
                    part.get("additionalProperties")
                        .is_some_and(|v| v == &Value::Bool(true) || v.is_object())
                }) {
                    errors.push(format!("{at}: undeclared emitted field {key}"));
                }
            }
        }
    }
    if let Some(array) = value.as_array() {
        for part in parts {
            if let Some(items) = part.get("items") {
                for (index, child) in array.iter().enumerate() {
                    declared(root, items, child, &format!("{at}[{index}]"), errors);
                }
            }
        }
    }
}
