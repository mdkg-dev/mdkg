//! Runs mdkg against the format conformance suite (`conformance/`).
//!
//! The goldens are mdkg 0.6.0's own output, kept as recorded. mdkg's
//! output follows its own contract (edd-4), so the comparison is by facts,
//! not bytes:
//! - the exit code is identical;
//! - `list`, `search` and `show` return the same nodes with the same field
//!   values (order is checked separately: mdkg sorts by byte order);
//! - errors and warnings are the same codes at the same locations. Each
//!   0.6.0 message maps to a code through `CODE_TABLE`; an unmapped message
//!   fails the test, so the table stays complete.
//!
//! Text-mode goldens are compared by exit code only: text isn't a contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

use serde_json::{Map, Value, json};

/// 0.6.0 message fragment -> code, checked in order (specific first).
const CODE_TABLE: &[(&str, &str)] = &[
    (
        "frontmatter must start with ---",
        "frontmatter.missing_open",
    ),
    ("frontmatter closing --- not found", "frontmatter.unclosed"),
    (
        "frontmatter lines must not be blank",
        "frontmatter.blank_line",
    ),
    (
        "frontmatter lines must be key: value",
        "frontmatter.not_key_value",
    ),
    ("invalid key: ", "frontmatter.invalid_key"),
    ("duplicate key: ", "frontmatter.duplicate_key"),
    ("value must not be empty", "frontmatter.empty_value"),
    ("list must be enclosed in [ ]", "frontmatter.bad_list"),
    (
        "list items must be non-empty",
        "frontmatter.empty_list_item",
    ),
    ("type must be one of", "node.unknown_type"),
    ("unknown key: ", "node.unknown_key"),
    ("must be YYYY-MM-DD", "node.bad_date"),
    ("status is required", "node.missing_status"),
    ("status must be one of", "node.bad_status"),
    ("priority must be", "node.bad_priority"),
    (
        "refs entries must be portable ids or URI refs",
        "node.bad_ref",
    ),
    ("must be kebab-case", "node.bad_skill_slug"),
    ("must be lowercase", "node.not_lowercase"),
    ("must be a string", "node.wrong_kind"),
    ("must be a list", "node.wrong_kind"),
    ("must be a boolean", "node.wrong_kind"),
    ("duplicate id ", "node.duplicate_id"),
    ("not found", "node.not_found"),
    (
        "scope_refs references missing node",
        "goal.scope_ref_missing",
    ),
    (
        "active_node references missing node",
        "goal.active_node_missing",
    ),
    ("references missing node", "edge.missing_target"),
    ("missing matching", "edge.prev_next_asymmetric"),
    ("cycle detected in prev/next chain", "edge.cycle"),
    ("multiple active root goals", "goal.multiple_active"),
    ("skills reference missing slug", "skill.missing_slug"),
    ("mirror root missing", "skill.mirror_missing"),
    ("mirror manifest missing", "skill.mirror_manifest_missing"),
];

/// 0.6.0 warning id -> code, for warnings whose id alone decides it.
const WARNING_TABLE: &[(&str, &str)] = &[
    ("heading.missing", "node.heading_missing"),
    ("template_schema.fallback", "template.fallback"),
    ("raw-content.raw_prompt", "node.raw_prompt"),
    ("raw-content.raw_payload", "node.raw_payload"),
    ("raw-content.raw_secret", "node.raw_secret"),
    ("manifest.compat.spec_legacy", "node.legacy_spec_file"),
    ("manifest.compat.type_spec", "node.legacy_spec_type"),
];

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance")
}

fn copy_tree(src: &Path, dest: &Path, skip_top_level: &[&str]) {
    for entry in fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let name = entry.file_name();
        if skip_top_level.iter().any(|s| name == *s) {
            continue;
        }
        let target = dest.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            fs::create_dir_all(&target).expect("create dir");
            copy_tree(&entry.path(), &target, &[]);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn assemble(case: &str) -> PathBuf {
    let root = common::temp::dir("mdkg-conf");
    copy_tree(&conformance_dir().join("base"), &root, &[]);
    copy_tree(
        &conformance_dir().join("cases").join(case),
        &root,
        &["commands", "README.md"],
    );
    root
}

fn code_for(message: &str) -> Result<&'static str, String> {
    CODE_TABLE
        .iter()
        .find(|(fragment, _)| message.contains(fragment))
        .map(|(_, code)| *code)
        .ok_or_else(|| format!("no code mapping for 0.6.0 message: {message}"))
}

/// A reference in qid form, so `edd-1` and `root:edd-1` compare equal.
fn qid(value: &str) -> String {
    if value.contains(':') {
        value.to_string()
    } else {
        format!("root:{value}")
    }
}

fn qids(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Array(items)) => Value::Array(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| json!(qid(s)))
                .collect(),
        ),
        Some(Value::String(s)) => json!(qid(s)),
        _ => Value::Null,
    }
}

fn strings(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Array(items)) if !items.is_empty() => Value::Array(items.clone()),
        _ => json!([]),
    }
}

fn body(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_str) {
        Some(b) => {
            let b = b.trim_start_matches(['\n', '\r']).trim_end();
            if b.is_empty() { Value::Null } else { json!(b) }
        }
        None => Value::Null,
    }
}

/// One node's facts. `edges` and `fields` come from `edges`/`refs` and
/// `attributes` in 0.6.0, or `edges` and `fields` in mdkg.
fn node_facts(node: &Value, golden: bool) -> (String, Value) {
    let id = node["id"].as_str().unwrap_or_default();
    let ws = node.get("ws").and_then(Value::as_str).unwrap_or("root");
    let key = format!("{ws}:{id}");
    let edges = node.get("edges").cloned().unwrap_or(json!({}));
    let mut edge_facts = Map::new();
    for single in ["epic", "parent", "prev", "next"] {
        edge_facts.insert(single.into(), qids(edges.get(single)));
    }
    for list in [
        "relates",
        "blocked_by",
        "blocks",
        "context_refs",
        "evidence_refs",
    ] {
        edge_facts.insert(
            list.into(),
            qids(edges.get(list))
                .as_array()
                .map_or(json!([]), |a| json!(a)),
        );
    }
    let refs = if golden {
        node.get("refs")
    } else {
        edges.get("refs")
    };
    edge_facts.insert(
        "refs".into(),
        qids(refs).as_array().map_or(json!([]), |a| json!(a)),
    );
    let fields = if golden {
        node.get("attributes")
    } else {
        node.get("fields")
    };
    let mut facts = json!({
        "type": node.get("type"),
        "title": node.get("title"),
        "status": node.get("status"),
        "priority": node.get("priority"),
        "created": node.get("created"),
        "updated": node.get("updated"),
        "path": node.get("path"),
        "tags": strings(node.get("tags")),
        "owners": strings(node.get("owners")),
        "links": strings(node.get("links")),
        "artifacts": strings(node.get("artifacts")),
        "aliases": strings(node.get("aliases")),
        "skills": strings(node.get("skills")),
        "edges": Value::Object(edge_facts),
        "fields": fields.cloned().filter(|f| f.as_object().is_some_and(|m| !m.is_empty())).unwrap_or(json!({})),
        "body": body(node.get("body")),
    });
    if let Some(m) = facts.as_object_mut() {
        m.retain(|_, v| !v.is_null());
    }
    (key, facts)
}

/// A diagnostic's facts. Fields the golden doesn't state are not compared.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fact {
    severity: &'static str,
    code: String,
    path: Option<String>,
    line: Option<u64>,
    node: Option<String>,
}

impl Fact {
    fn matches(&self, actual: &Fact) -> bool {
        self.severity == actual.severity
            && self.code == actual.code
            && self
                .path
                .as_ref()
                .is_none_or(|p| actual.path.as_ref() == Some(p))
            && self.line.is_none_or(|l| actual.line == Some(l))
            && self
                .node
                .as_ref()
                .is_none_or(|n| actual.node.as_ref() == Some(n))
    }
}

/// Parses a 0.6.0 error line: `<ROOT>/path:line: message`, `path: message`,
/// `ws:id: message` or a bare message.
fn golden_error(line: &str) -> Result<Option<Fact>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("validation failed with") {
        return Ok(None);
    }
    let rest = line.strip_prefix("<ROOT>/").unwrap_or(line);
    let mut fact = Fact {
        severity: "error",
        code: String::new(),
        path: None,
        line: None,
        node: None,
    };
    let message = if let Some((loc, msg)) = rest.split_once(": ") {
        let mut parts = loc.rsplitn(2, ':');
        let last = parts.next().unwrap_or_default();
        match (last.parse::<u64>(), parts.next()) {
            (Ok(n), Some(path)) if path.ends_with(".md") => {
                fact.path = Some(path.to_string());
                fact.line = Some(n);
            }
            _ if loc.ends_with(".md") => fact.path = Some(loc.to_string()),
            _ if loc.contains(':') && !loc.contains(' ') => fact.node = Some(qid(loc)),
            _ => {
                return code_for(line).map(|c| {
                    fact.code = c.into();
                    Some(fact)
                });
            }
        }
        msg
    } else {
        line
    };
    fact.code = code_for(message)?.into();
    Ok(Some(fact))
}

fn golden_warning(w: &Value) -> Result<Fact, String> {
    let id = w["id"].as_str().unwrap_or_default();
    let message = w["message"].as_str().unwrap_or_default();
    let code = WARNING_TABLE
        .iter()
        .find(|(warning_id, _)| *warning_id == id)
        .map(|(_, c)| *c)
        .map_or_else(|| code_for(message), Ok)?;
    Ok(Fact {
        severity: "warning",
        code: code.into(),
        path: w.get("path").and_then(Value::as_str).map(String::from),
        line: None,
        node: w.get("qid").and_then(Value::as_str).map(qid),
    })
}

fn actual_facts(diagnostics: &Value) -> Vec<Fact> {
    diagnostics
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| Fact {
            severity: if d["severity"] == "warning" {
                "warning"
            } else {
                "error"
            },
            code: d["code"].as_str().unwrap_or_default().into(),
            path: d.get("path").and_then(Value::as_str).map(String::from),
            line: d.get("line").and_then(Value::as_u64),
            node: d.get("node").and_then(Value::as_str).map(qid),
        })
        .collect()
}

/// Every golden fact matches a distinct actual diagnostic, and nothing is left.
fn compare_facts(want: &[Fact], got: &[Fact], diffs: &mut Vec<String>) {
    let mut unused: Vec<&Fact> = got.iter().collect();
    for w in want {
        match unused.iter().position(|g| w.matches(g)) {
            Some(i) => {
                unused.remove(i);
            }
            None => diffs.push(format!("missing diagnostic {w:?}")),
        }
    }
    for g in unused {
        diffs.push(format!("unexpected diagnostic {g:?}"));
    }
}

fn compare_nodes(golden: &[Value], actual: &[Value], diffs: &mut Vec<String>) {
    compare_node_set(golden, actual, true, diffs);
}

/// Every 0.6.0 node is in `actual` with the same facts. With `exact`,
/// nothing else is, and `actual` is in byte order of qids.
fn compare_node_set(golden: &[Value], actual: &[Value], exact: bool, diffs: &mut Vec<String>) {
    let want: BTreeMap<String, Value> = golden.iter().map(|n| node_facts(n, true)).collect();
    // 0.6.0 lists carry no body; `--fields all` does.
    let with_body = golden.iter().any(|n| n.get("body").is_some());
    let got: BTreeMap<String, Value> = actual
        .iter()
        .map(|n| {
            let (key, mut facts) = node_facts(n, false);
            if !with_body && let Some(m) = facts.as_object_mut() {
                m.remove("body");
            }
            (key, facts)
        })
        .collect();
    for (key, w) in &want {
        match got.get(key) {
            Some(g) if g == w => {}
            Some(g) => diffs.push(format!("node {key} differs\n  0.6.0: {w}\n  next:  {g}")),
            None => diffs.push(format!("node {key} missing")),
        }
    }
    if !exact {
        return;
    }
    for key in got.keys().filter(|k| !want.contains_key(*k)) {
        diffs.push(format!("unexpected node {key}"));
    }
    let order: Vec<String> = actual.iter().map(|n| node_facts(n, false).0).collect();
    let mut sorted = order.clone();
    sorted.sort();
    if order != sorted {
        diffs.push(format!("nodes are not in byte order: {order:?}"));
    }
}

/// Runs one golden and returns a description of every difference.
fn check(entry: &str) -> Vec<String> {
    let (case, number) = entry.split_once('/').expect("entry is case/nn");
    let golden_path = conformance_dir()
        .join("expected")
        .join(case)
        .join(format!("{number}.json"));
    let golden: Value =
        serde_json::from_str(&fs::read_to_string(&golden_path).expect("read golden"))
            .expect("golden is JSON");
    let mut args: Vec<String> = golden["args"]
        .as_array()
        .expect("args array")
        .iter()
        .map(|a| a.as_str().expect("arg string").to_string())
        .collect();
    let command = args[0].clone();
    let json_mode = args.iter().any(|a| a == "--json");
    if json_mode && matches!(command.as_str(), "list" | "search") {
        args.extend(["--fields".into(), "all".into()]);
    }

    let root = assemble(case);
    let output = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .args(&args)
        .current_dir(&root)
        .env("NO_COLOR", "1")
        .output()
        .expect("run mdkg");
    fs::remove_dir_all(&root).ok();

    let mut diffs = Vec::new();
    let exit = i64::from(output.status.code().unwrap_or(-1));
    if golden["exit_code"].as_i64() != Some(exit) {
        diffs.push(format!(
            "exit code: expected {}, got {exit}",
            golden["exit_code"]
        ));
    }
    if !json_mode {
        return diffs;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Ok(envelope) = serde_json::from_str::<Value>(&stdout) else {
        diffs.push(format!("stdout is not one JSON document: {stdout}"));
        return diffs;
    };
    common::conforms_envelope(&envelope);
    if envelope["schema"] != json!(format!("mdkg.{command}.v1")) {
        diffs.push(format!("schema is {}", envelope["schema"]));
    }
    if envelope["ok"] != json!(exit == 0) && command != "validate" {
        diffs.push(format!("ok is {} with exit {exit}", envelope["ok"]));
    }
    let actual = actual_facts(&envelope["diagnostics"]);
    let golden_json = golden["stdout"].get("json").cloned().unwrap_or(Value::Null);

    let mut want: Vec<Fact> = Vec::new();
    let mut table_errors = Vec::new();
    for line in golden["stderr"].as_str().unwrap_or_default().lines() {
        match golden_error(line) {
            Ok(Some(f)) => want.push(f),
            Ok(None) => {}
            Err(e) => table_errors.push(e),
        }
    }
    if command == "validate" {
        for e in golden_json["errors"].as_array().into_iter().flatten() {
            match golden_error(e.as_str().unwrap_or_default()) {
                Ok(Some(f)) => want.push(f),
                Ok(None) => {}
                Err(e) => table_errors.push(e),
            }
        }
        for w in golden_json["warning_diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
        {
            match golden_warning(w) {
                Ok(f) => want.push(f),
                Err(e) => table_errors.push(e),
            }
        }
        if envelope["ok"] != golden_json["ok"] {
            diffs.push(format!(
                "ok is {}, 0.6.0 said {}",
                envelope["ok"], golden_json["ok"]
            ));
        }
    }
    diffs.extend(table_errors);
    compare_facts(&want, &actual, &mut diffs);

    if exit == 0 {
        let data = &envelope["data"];
        match command.as_str() {
            "list" => compare_nodes(
                golden_json["items"]
                    .as_array()
                    .map_or(&[][..], Vec::as_slice),
                data["nodes"].as_array().map_or(&[][..], Vec::as_slice),
                &mut diffs,
            ),
            "search" => {
                // 0.7.0 searches bodies too and ranks by relevance (edd-6,
                // lane 4): every node 0.6.0 found is still found, with the
                // same facts, and more may be.
                if golden_json["limit"] != data["limit"] {
                    diffs.push(format!(
                        "limit: 0.6.0 {} next {}",
                        golden_json["limit"], data["limit"]
                    ));
                }
                let count = data["count"].as_u64().unwrap_or(0);
                if count < golden_json["count"].as_u64().unwrap_or(0) {
                    diffs.push(format!(
                        "count: 0.6.0 {} next {count}",
                        golden_json["count"]
                    ));
                }
                let returned = data["nodes"].as_array().map_or(0, Vec::len) as u64;
                let limit = data["limit"].as_u64().unwrap_or(0);
                if returned != count.min(limit) {
                    diffs.push(format!(
                        "search returned {returned} nodes of {count}, limit {limit}"
                    ));
                }
                if golden_json["truncated"] == json!(false) && data["truncated"] == json!(false) {
                    compare_node_set(
                        golden_json["items"]
                            .as_array()
                            .map_or(&[][..], Vec::as_slice),
                        data["nodes"].as_array().map_or(&[][..], Vec::as_slice),
                        false,
                        &mut diffs,
                    );
                }
            }
            "show" => compare_nodes(
                std::slice::from_ref(&golden_json["item"]),
                std::slice::from_ref(&data["node"]),
                &mut diffs,
            ),
            _ => {}
        }
    }
    diffs
}

fn passing_entries() -> Vec<String> {
    fs::read_to_string(conformance_dir().join("passing.txt"))
        .expect("read passing.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn all_goldens() -> Vec<String> {
    let mut out = Vec::new();
    for case in fs::read_dir(conformance_dir().join("expected")).expect("expected dir") {
        let case = case.expect("case entry");
        for golden in fs::read_dir(case.path()).expect("case dir") {
            let golden = golden.expect("golden entry").path();
            let stem = golden
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("golden name")
                .to_string();
            out.push(format!("{}/{stem}", case.file_name().to_string_lossy()));
        }
    }
    out.sort();
    out
}

#[test]
fn passing_list_names_real_goldens() {
    let all = all_goldens();
    for entry in passing_entries() {
        assert!(
            all.contains(&entry),
            "passing.txt names unknown golden {entry}"
        );
    }
}

#[test]
fn listed_goldens_match_on_facts() {
    let entries = passing_entries();
    let mut failures = Vec::new();
    for entry in &entries {
        let diffs = check(entry);
        if !diffs.is_empty() {
            failures.push(format!("{entry}:\n{}", diffs.join("\n")));
        }
    }
    eprintln!(
        "conformance: {} of {} goldens listed as passing",
        entries.len(),
        all_goldens().len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_mapped_code_exists() {
    let output = common::without_launcher_env(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .args(["schema", "validate"])
        .output()
        .expect("run mdkg schema");
    let schema: Value = serde_json::from_slice(&output.stdout).expect("schema JSON");
    let text = schema.to_string();
    for (_, code) in CODE_TABLE.iter().chain(WARNING_TABLE) {
        assert!(
            text.contains(&format!("\"{code}\"")),
            "{code} is not a registered code"
        );
    }
}
