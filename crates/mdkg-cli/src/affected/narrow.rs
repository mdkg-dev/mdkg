//! Built-in global files whose change can be narrowed.
//!
//! Two kinds of edit to a global file can't change what every test does:
//! an edit to `.mdkg/config.json` confined to its `affected` rules, and a
//! Cargo manifest or lock change confined to the versions of the
//! repository's own packages. Both sides are parsed and compared; when
//! either side doesn't parse, or anything else differs, the file stays
//! global and the run is full.
//!
//! A narrowed file is judged like any other path: a rule matching it names
//! its tests (the version tests for a manifest, the `affected` tests for
//! the config), and without one it still requests a full run. A narrowed
//! config also selects the targets of every rule it added, changed or
//! removed, and every changed path is judged under the base's rules as
//! well as the current ones, so an edit can't narrow its own range.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::config::{self, Config, Rule};
use super::git::{Change, Repo, TreeEntry};

/// The config file whose `affected`-only edits are narrowed.
pub const CONFIG_PATH: &str = ".mdkg/config.json";

/// The largest blob read for a comparison; a bigger file stays global.
const MAX_BYTES: u64 = 16 << 20;

/// What narrowing found for one range.
#[derive(Debug, Default)]
pub struct Narrowing {
    /// Built-in global paths whose change was confined, each with the
    /// changed rules whose targets it selects: a label for messages, and
    /// the rule.
    pub paths: BTreeMap<String, Vec<(String, Rule)>>,
    /// The base's config, when its `affected`-only edit was narrowed.
    pub base: Option<Config>,
}

/// Finds the changed global files that can be narrowed. Anything it can't
/// read leaves the file global.
pub fn find(repo: &Repo, changes: &[Change], base: &[TreeEntry], head: &[TreeEntry]) -> Narrowing {
    let mut pairs: Vec<(String, String, String)> = Vec::new();
    for change in changes {
        if change.status != 'M' || change.old_path.is_some() {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&change.path) else {
            continue;
        };
        if kind(path).is_none() {
            continue;
        }
        let entry = |entries: &[TreeEntry]| {
            entries
                .iter()
                .find(|e| e.path == change.path)
                .filter(|e| matches!(e.mode.as_str(), "100644" | "100755"))
                .filter(|e| e.size.is_some_and(|s| s <= MAX_BYTES))
                .cloned()
        };
        if let (Some(b), Some(h)) = (entry(base), entry(head))
            && b.mode == h.mode
        {
            pairs.push((path.to_string(), b.oid, h.oid));
        }
    }
    let mut out = Narrowing::default();
    if pairs.is_empty() {
        return out;
    }
    let oids: Vec<String> = pairs
        .iter()
        .flat_map(|(_, b, h)| [b.clone(), h.clone()])
        .collect();
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(oids.len());
    if repo
        .each_blob(&oids, |_, bytes| {
            blobs.push(bytes.to_vec());
            Ok(())
        })
        .is_err()
        || blobs.len() != oids.len()
    {
        return out;
    }
    for (i, (path, _, _)) in pairs.into_iter().enumerate() {
        let (b, h) = (&blobs[2 * i], &blobs[2 * i + 1]);
        match kind(&path) {
            Some(Kind::Config) => {
                if let Some(edit) = config_edit(b, h) {
                    out.base = Some(edit.base);
                    out.paths.insert(path, edit.changed);
                }
            }
            Some(Kind::Manifest) if version_only_manifest(b, h) => {
                out.paths.insert(path, Vec::new());
            }
            Some(Kind::Lock) if version_only_lock(b, h) => {
                out.paths.insert(path, Vec::new());
            }
            _ => {}
        }
    }
    out
}

enum Kind {
    Config,
    Manifest,
    Lock,
}

/// Exact names only: a case variant stays global (and the Rust adapter
/// refuses a manifest's).
fn kind(path: &str) -> Option<Kind> {
    if path == CONFIG_PATH {
        return Some(Kind::Config);
    }
    match path.rsplit('/').next() {
        Some("Cargo.toml") => Some(Kind::Manifest),
        Some("Cargo.lock") => Some(Kind::Lock),
        _ => None,
    }
}

/// A config edit confined to the `affected` rules.
#[derive(Debug)]
pub struct ConfigEdit {
    /// The base's config.
    pub base: Config,
    /// Every rule added, changed or removed, with a label for messages.
    pub changed: Vec<(String, Rule)>,
}

/// The rules an edit changed, when nothing else differs: every other key
/// of the config and of its `affected` section (`version`, `global`) must
/// be equal, and both sections must parse. Rules are compared as parsed
/// JSON values, as a multiset: reordering them changes nothing, since
/// every matching rule applies.
pub fn config_edit(base: &[u8], head: &[u8]) -> Option<ConfigEdit> {
    let base_text = std::str::from_utf8(base).ok()?;
    let head_text = std::str::from_utf8(head).ok()?;
    let base_config = config::parse(base_text).ok()?;
    let head_config = config::parse(head_text).ok()?;
    let mut base_root: Value = serde_json::from_str(base_text).ok()?;
    let mut head_root: Value = serde_json::from_str(head_text).ok()?;
    let (base_rest, base_rules) = split(base_root.as_object_mut()?.remove("affected"))?;
    let (head_rest, head_rules) = split(head_root.as_object_mut()?.remove("affected"))?;
    if base_root != head_root || base_rest != head_rest {
        return None;
    }
    // `parse` keeps each rule at its index; check, since labels rely on it.
    if base_rules.len() != base_config.rules.len() || head_rules.len() != head_config.rules.len() {
        return None;
    }
    let mut matched = vec![false; base_rules.len()];
    let mut changed = Vec::new();
    for (i, rule) in head_rules.iter().enumerate() {
        match (0..base_rules.len()).find(|&j| !matched[j] && base_rules[j] == *rule) {
            Some(j) => matched[j] = true,
            None => changed.push((
                format!("affected.rules[{i}] (added or changed in this range)"),
                head_config.rules[i].clone(),
            )),
        }
    }
    for (j, seen) in matched.into_iter().enumerate() {
        if !seen {
            changed.push((
                format!("the base's affected.rules[{j}] (removed or changed in this range)"),
                base_config.rules[j].clone(),
            ));
        }
    }
    Some(ConfigEdit {
        base: base_config,
        changed,
    })
}

/// An `affected` section without its rules, and the rules.
fn split(section: Option<Value>) -> Option<(Map<String, Value>, Vec<Value>)> {
    let Some(section) = section else {
        return Some((Map::new(), Vec::new()));
    };
    let mut section = match section {
        Value::Object(o) => o,
        _ => return None,
    };
    let rules = match section.remove("rules") {
        None => Vec::new(),
        Some(Value::Array(rules)) => rules,
        Some(_) => return None,
    };
    Some((section, rules))
}

/// A TOML value with formatting dropped, for comparison.
#[derive(Debug, Clone, PartialEq)]
enum Toml {
    Str(String),
    Int(i64),
    /// The bits, so every float compares (NaN included).
    Float(u64),
    Bool(bool),
    Date(String),
    Array(Vec<Toml>),
    Table(BTreeMap<String, Toml>),
}

fn parse_toml(bytes: &[u8]) -> Option<BTreeMap<String, Toml>> {
    let doc = std::str::from_utf8(bytes)
        .ok()?
        .parse::<toml_edit::DocumentMut>()
        .ok()?;
    Some(table(doc.as_table()))
}

fn table(t: &toml_edit::Table) -> BTreeMap<String, Toml> {
    t.iter()
        .filter_map(|(k, item)| item_value(item).map(|v| (k.to_string(), v)))
        .collect()
}

fn item_value(item: &toml_edit::Item) -> Option<Toml> {
    match item {
        toml_edit::Item::None => None,
        toml_edit::Item::Value(v) => Some(value(v)),
        toml_edit::Item::Table(t) => Some(Toml::Table(table(t))),
        toml_edit::Item::ArrayOfTables(a) => Some(Toml::Array(
            a.iter().map(|t| Toml::Table(table(t))).collect(),
        )),
    }
}

fn value(v: &toml_edit::Value) -> Toml {
    match v {
        toml_edit::Value::String(s) => Toml::Str(s.value().clone()),
        toml_edit::Value::Integer(i) => Toml::Int(*i.value()),
        toml_edit::Value::Float(f) => Toml::Float(f.value().to_bits()),
        toml_edit::Value::Boolean(b) => Toml::Bool(*b.value()),
        toml_edit::Value::Datetime(d) => Toml::Date(d.value().to_string()),
        toml_edit::Value::Array(a) => Toml::Array(a.iter().map(value).collect()),
        toml_edit::Value::InlineTable(t) => {
            Toml::Table(t.iter().map(|(k, v)| (k.to_string(), value(v))).collect())
        }
    }
}

fn get_table<'a>(
    t: &'a mut BTreeMap<String, Toml>,
    key: &str,
) -> Option<&'a mut BTreeMap<String, Toml>> {
    match t.get_mut(key) {
        Some(Toml::Table(t)) => Some(t),
        _ => None,
    }
}

/// Dependency tables, with Cargo's legacy spellings.
const DEPENDENCY_KEYS: &[&str] = &[
    "dependencies",
    "dev-dependencies",
    "dev_dependencies",
    "build-dependencies",
    "build_dependencies",
];

/// Drops the version requirement of every `path` dependency: Cargo builds
/// the code at the path, and the requirement only has to match its version.
fn drop_path_versions(dependencies: &mut BTreeMap<String, Toml>) {
    for dependency in dependencies.values_mut() {
        if let Toml::Table(d) = dependency
            && d.contains_key("path")
        {
            d.remove("version");
        }
    }
}

fn drop_dependency_versions(t: &mut BTreeMap<String, Toml>) {
    for key in DEPENDENCY_KEYS {
        if let Some(d) = get_table(t, key) {
            drop_path_versions(d);
        }
    }
}

/// A manifest with the package's version, the workspace's package version
/// and the version requirements of `path` dependencies dropped.
fn manifest_without_versions(bytes: &[u8]) -> Option<BTreeMap<String, Toml>> {
    let mut root = parse_toml(bytes)?;
    for key in ["package", "project"] {
        if let Some(p) = get_table(&mut root, key) {
            p.remove("version");
        }
    }
    if let Some(w) = get_table(&mut root, "workspace") {
        if let Some(p) = get_table(w, "package") {
            p.remove("version");
        }
        if let Some(d) = get_table(w, "dependencies") {
            drop_path_versions(d);
        }
    }
    drop_dependency_versions(&mut root);
    if let Some(targets) = get_table(&mut root, "target") {
        for target in targets.values_mut() {
            if let Toml::Table(t) = target {
                drop_dependency_versions(t);
            }
        }
    }
    Some(root)
}

/// Whether two manifests differ only in versions: the package's own, the
/// workspace's package version, or a `path` dependency's requirement.
pub fn version_only_manifest(base: &[u8], head: &[u8]) -> bool {
    match (
        manifest_without_versions(base),
        manifest_without_versions(head),
    ) {
        (Some(b), Some(h)) => b == h,
        _ => false,
    }
}

/// A lock with the versions of local packages (those without a `source`)
/// dropped, from their entries and from dependency lists naming them as
/// `name version`.
fn lock_without_versions(bytes: &[u8]) -> Option<BTreeMap<String, Toml>> {
    let mut root = parse_toml(bytes)?;
    let Some(Toml::Array(packages)) = root.get_mut("package") else {
        return Some(root);
    };
    let mut local = BTreeSet::new();
    for package in packages.iter() {
        if let Toml::Table(p) = package
            && !p.contains_key("source")
            && let (Some(Toml::Str(name)), Some(Toml::Str(version))) =
                (p.get("name"), p.get("version"))
        {
            local.insert(format!("{name} {version}"));
        }
    }
    for package in packages.iter_mut() {
        let Toml::Table(p) = package else { continue };
        if !p.contains_key("source") {
            p.remove("version");
        }
        if let Some(Toml::Array(dependencies)) = p.get_mut("dependencies") {
            for dependency in dependencies {
                if let Toml::Str(d) = dependency
                    && local.contains(d.as_str())
                    && let Some((name, _)) = d.split_once(' ')
                {
                    *d = name.to_string();
                }
            }
        }
    }
    Some(root)
}

/// Whether two locks differ only in the versions of local packages.
pub fn version_only_lock(base: &[u8], head: &[u8]) -> bool {
    match (lock_without_versions(base), lock_without_versions(head)) {
        (Some(b), Some(h)) => b == h,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = r#""tool": "mdkg", "schema_version": 3"#;

    fn edit(base: &str, head: &str) -> Option<Vec<String>> {
        config_edit(base.as_bytes(), head.as_bytes())
            .map(|e| e.changed.into_iter().map(|(label, _)| label).collect())
    }

    #[test]
    fn a_config_edit_confined_to_the_rules_names_each_changed_rule() {
        let a = r#"{"paths": ["a/**"], "select": "none"}"#;
        let b = r#"{"paths": ["b/**"], "select": {"suites": ["b"]}}"#;
        let b2 = r#"{"paths": ["b/**"], "select": {"suites": ["b2"]}}"#;
        let c = r#"{"paths": ["c/**"], "select": {"suites": ["c"]}}"#;
        let cfg = |rules: &[&str]| {
            format!(
                r#"{{{ID}, "affected": {{"rules": [{}]}}}}"#,
                rules.join(", ")
            )
        };
        assert_eq!(edit(&cfg(&[a, b]), &cfg(&[a, b])), Some(vec![]));
        assert_eq!(edit(&cfg(&[a, b]), &cfg(&[b, a])), Some(vec![]), "order");
        assert_eq!(
            edit(&cfg(&[a, b]), &cfg(&[a, b, c])),
            Some(vec![
                "affected.rules[2] (added or changed in this range)".into()
            ])
        );
        assert_eq!(
            edit(&cfg(&[a, b]), &cfg(&[b])),
            Some(vec![
                "the base's affected.rules[0] (removed or changed in this range)".into()
            ])
        );
        assert_eq!(
            edit(&cfg(&[a, b]), &cfg(&[a, b2])),
            Some(vec![
                "affected.rules[1] (added or changed in this range)".into(),
                "the base's affected.rules[1] (removed or changed in this range)".into()
            ])
        );
        // A duplicate counts once per copy.
        assert_eq!(edit(&cfg(&[a, a]), &cfg(&[a])).map(|c| c.len()), Some(1));
        // No section on one side: its rules are all added.
        assert_eq!(
            edit(&format!("{{{ID}}}"), &cfg(&[a])).map(|c| c.len()),
            Some(1)
        );
        // Whitespace and key order inside a rule don't matter.
        let a2 = r#"{ "select": "none",  "paths": ["a/**"] }"#;
        assert_eq!(edit(&cfg(&[a]), &cfg(&[a2])), Some(vec![]));
    }

    #[test]
    fn a_config_edit_outside_the_rules_or_that_doesnt_parse_stays_global() {
        let rules = r#""rules": [{"paths": ["a/**"], "select": "none"}]"#;
        let base = format!(r#"{{{ID}, "affected": {{{rules}}}}}"#);
        for head in [
            format!(r#"{{{ID}, "x": 1, "affected": {{{rules}}}}}"#),
            format!(r#"{{"tool": "mdkg", "schema_version": 4, "affected": {{{rules}}}}}"#),
            format!(r#"{{{ID}, "affected": {{"global": ["x/**"], {rules}}}}}"#),
            format!(r#"{{{ID}, "affected": {{"version": 1, {rules}}}}}"#),
            format!(r#"{{{ID}, "affected": {{"rulez": []}}}}"#),
            format!(r#"{{{ID}, "affected": {{"rules": [{{"paths": [], "select": "none"}}]}}}}"#),
            "not JSON".to_string(),
            "[]".to_string(),
        ] {
            assert!(edit(&base, &head).is_none(), "{head}");
            assert!(edit(&head, &base).is_none(), "{head} as the base");
        }
    }

    #[test]
    fn a_manifest_differing_only_in_versions_is_version_only() {
        let base = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { path = \"../b\", version = \"0.1.0\" }\nserde = \"1\"\n\n[target.'cfg(unix)'.dev-dependencies]\nc = { path = \"../c\", version = \"0.1\" }\n";
        let head = "[package]\nname = \"a\"\nversion = \"0.2.0\" # bumped\n\n[dependencies]\nb = { version = \"0.2.0\", path = \"../b\" }\nserde = \"1\"\n\n[target.'cfg(unix)'.dev-dependencies.c]\npath = \"../c\"\nversion = \"0.2\"\n";
        assert!(version_only_manifest(base.as_bytes(), head.as_bytes()));
        let ws = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n\n[workspace.dependencies]\na = { path = \"a\", version = \"0.1.0\" }\n";
        let ws2 = ws.replace("0.1.0", "0.2.0");
        assert!(version_only_manifest(ws.as_bytes(), ws2.as_bytes()));
        for changed in [
            // A registry dependency's requirement picks other code.
            base.replace("serde = \"1\"", "serde = \"1.0.200\""),
            // So does dropping the path.
            base.replace(
                "b = { path = \"../b\", version = \"0.1.0\" }",
                "b = \"0.1.0\"",
            ),
            base.replace("name = \"a\"", "name = \"z\""),
            base.replace(
                "serde = \"1\"",
                "serde = { version = \"1\", features = [\"derive\"] }",
            ),
            format!("{base}\n[features]\nx = []\n"),
            "[package\n".to_string(),
        ] {
            assert!(
                !version_only_manifest(base.as_bytes(), changed.as_bytes()),
                "{changed}"
            );
        }
        assert!(!version_only_manifest(
            ws.as_bytes(),
            ws.replace("[\"a\"]", "[\"a\", \"b\"]").as_bytes()
        ));
    }

    #[test]
    fn a_lock_differing_only_in_local_versions_is_version_only() {
        let lock = |a: &str, serde: &str| {
            format!(
                "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"{a}\"\ndependencies = [\n \"b {a}\",\n \"serde\",\n]\n\n[[package]]\nname = \"b\"\nversion = \"{a}\"\n\n[[package]]\nname = \"b\"\nversion = \"9.0.0\"\nsource = \"registry+https://example.invalid/index\"\nchecksum = \"00\"\n\n[[package]]\nname = \"serde\"\nversion = \"{serde}\"\nsource = \"registry+https://example.invalid/index\"\nchecksum = \"00\"\n"
            )
        };
        assert!(version_only_lock(
            lock("0.1.0", "1.0.0").as_bytes(),
            lock("0.2.0", "1.0.0").as_bytes()
        ));
        assert!(!version_only_lock(
            lock("0.1.0", "1.0.0").as_bytes(),
            lock("0.1.0", "1.0.1").as_bytes()
        ));
        // A dependency moved from the local package to the registry one.
        let moved = lock("0.1.0", "1.0.0").replace("\"b 0.1.0\"", "\"b 9.0.0\"");
        assert!(!version_only_lock(
            lock("0.1.0", "1.0.0").as_bytes(),
            moved.as_bytes()
        ));
        assert!(!version_only_lock(
            lock("0.1.0", "1.0.0").as_bytes(),
            b"[[package]\n"
        ));
    }
}
