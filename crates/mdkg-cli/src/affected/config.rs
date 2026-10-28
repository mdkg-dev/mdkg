//! The `affected` section of `.mdkg/config.json`.
//!
//! ```json
//! "affected": {
//!   "version": 1,
//!   "global": ["scripts/ci/run_unit_tests.py"],
//!   "rules": [
//!     {"paths": ["migrations/**"], "select": {"go": ["./internal/repositories/..."], "suites": ["migrations"]}},
//!     {"paths": ["api/**/*.proto"], "select": "all"},
//!     {"paths": ["docs/**", "**/*.md"], "select": "none"}
//!   ]
//! }
//! ```
//!
//! The section is read strictly: an unknown key or a value of the wrong
//! shape makes the whole run full, so a typo can't quietly drop a rule.

use serde_json::{Map, Value};

use super::glob::Glob;

/// Files every test depends on: a change runs everything. The config's
/// `global` list adds to these; it can't remove them. Built-in paths match
/// ignoring ASCII case, since the tools can open those names on such hosts.
pub const DEFAULT_GLOBAL: &[&str] = &[
    "**/go.mod",
    "**/go.sum",
    "go.work",
    "go.work.sum",
    ".go-version",
    // Any manifest: a member's features unify across the workspace, so
    // dropping one can break another crate's build.
    "**/Cargo.toml",
    "**/Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config",
    ".cargo/config.toml",
    ".tool-versions",
    ".mdkg/config.json",
    ".github/workflows/**",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// ASCII-folded built-in globs, separate from user-defined globs.
    builtin_global: Vec<Glob>,
    /// User-defined globals keep their case-sensitive matching semantics.
    pub global: Vec<Glob>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub paths: Vec<Glob>,
    pub select: RuleSelect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSelect {
    /// A full run.
    All,
    /// The paths need no tests.
    None,
    /// These packages, crates and suites, as named (no dependents added).
    Targets {
        go: Vec<String>,
        rust: Vec<String>,
        suites: Vec<String>,
    },
}

impl Config {
    /// The defaults, for a config with no `affected` section.
    pub fn defaults() -> Self {
        Self {
            builtin_global: default_global(),
            global: Vec::new(),
            rules: Vec::new(),
        }
    }

    /// Whether `path` is a global file.
    pub fn is_global(&self, path: &str) -> bool {
        // A removed manifest leaves no adapter to reject its casing. The
        // built-in globals apply whether or not a language adapter exists.
        let folded = path.to_ascii_lowercase();
        self.builtin_global.iter().any(|g| g.matches(&folded))
            || self.global.iter().any(|g| g.matches(path))
    }
}

fn default_global() -> Vec<Glob> {
    DEFAULT_GLOBAL
        .iter()
        .filter_map(|g| Glob::parse(&g.to_ascii_lowercase()).ok())
        .collect()
}

/// Reads the section from the config's JSON text.
pub fn parse(config_json: &str) -> Result<Config, String> {
    let root: Value = serde_json::from_str(config_json)
        .map_err(|e| format!(".mdkg/config.json is not valid JSON: {e}"))?;
    if !root.is_object() {
        return Err(".mdkg/config.json must be a JSON object".into());
    }
    let Some(section) = root.get("affected") else {
        return Ok(Config::defaults());
    };
    let section = section.as_object().ok_or("`affected` must be an object")?;
    only_keys(section, "affected", &["version", "global", "rules"])?;
    match section.get("version") {
        None => {}
        Some(v) if v.as_u64() == Some(1) => {}
        Some(v) => return Err(format!("affected.version {v} is not supported (only 1)")),
    }
    let mut config = Config::defaults();
    if let Some(global) = section.get("global") {
        config.global.extend(globs(global, "affected.global")?);
    }
    if let Some(rules) = section.get("rules") {
        let rules = rules.as_array().ok_or("affected.rules must be a list")?;
        for (i, rule) in rules.iter().enumerate() {
            config
                .rules
                .push(parse_rule(rule, &format!("affected.rules[{i}]"))?);
        }
    }
    Ok(config)
}

fn only_keys(object: &Map<String, Value>, at: &str, allowed: &[&str]) -> Result<(), String> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!(
            "{at} has an unknown key `{k}` (allowed: {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

fn globs(value: &Value, at: &str) -> Result<Vec<Glob>, String> {
    let list = value
        .as_array()
        .ok_or(format!("{at} must be a list of path patterns"))?;
    if list.is_empty() {
        return Err(format!("{at} is empty"));
    }
    list.iter()
        .map(|v| {
            let s = v.as_str().ok_or(format!("{at} holds a non-string"))?;
            Glob::parse(s).map_err(|e| format!("{at}: {e}"))
        })
        .collect()
}

fn strings(
    value: &Value,
    at: &str,
    check: fn(&str) -> bool,
    what: &str,
) -> Result<Vec<String>, String> {
    let list = value.as_array().ok_or(format!("{at} must be a list"))?;
    let mut out = Vec::new();
    for v in list {
        let s = v.as_str().ok_or(format!("{at} holds a non-string"))?;
        if !check(s) {
            return Err(format!("{at}: `{s}` is not {what}"));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

fn parse_rule(rule: &Value, at: &str) -> Result<Rule, String> {
    let object = rule.as_object().ok_or(format!("{at} must be an object"))?;
    only_keys(object, at, &["paths", "select"])?;
    let paths = globs(
        object.get("paths").ok_or(format!("{at} needs `paths`"))?,
        &format!("{at}.paths"),
    )?;
    let select = match object.get("select").ok_or(format!("{at} needs `select`"))? {
        Value::String(s) if s == "all" => RuleSelect::All,
        Value::String(s) if s == "none" => RuleSelect::None,
        Value::Object(o) => {
            let at = format!("{at}.select");
            only_keys(o, &at, &["go", "rust", "suites"])?;
            let field = |key: &str, check: fn(&str) -> bool, what: &str| match o.get(key) {
                Some(v) => strings(v, &format!("{at}.{key}"), check, what),
                None => Ok(Vec::new()),
            };
            let go = field(
                "go",
                is_go_pattern,
                "a `./dir` or `./dir/...` package pattern",
            )?;
            let rust = field("rust", is_name, "a crate name")?;
            let suites = field("suites", is_name, "a suite name ([A-Za-z0-9._-])")?;
            if go.is_empty() && rust.is_empty() && suites.is_empty() {
                return Err(format!(
                    "{at} selects nothing; use \"none\" for paths that need no tests"
                ));
            }
            RuleSelect::Targets { go, rust, suites }
        }
        other => {
            return Err(format!(
                "{at}.select must be \"all\", \"none\" or an object of go, rust and suites (got {other})"
            ));
        }
    };
    Ok(Rule { paths, select })
}

/// `.`, `./...`, `./dir` or `./dir/...`, with no `..` or empty segments.
pub fn is_go_pattern(s: &str) -> bool {
    if s == "." || s == "./..." {
        return true;
    }
    let Some(rest) = s.strip_prefix("./") else {
        return false;
    };
    let rest = rest.strip_suffix("/...").unwrap_or(rest);
    !rest.is_empty()
        && rest
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && seg != "...")
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_section_gives_the_defaults() {
        let c = parse("{\"tool\": \"mdkg\"}").unwrap();
        assert!(c.rules.is_empty());
        for g in [
            "go.mod",
            "a/b/go.sum",
            "Cargo.lock",
            ".mdkg/config.json",
            ".github/workflows/ci.yml",
        ] {
            assert!(c.is_global(g), "{g}");
        }
        assert!(!c.is_global("src/main.rs"));
        assert!(
            c.is_global("crates/x/Cargo.toml"),
            "a member's manifest is global: features unify across the workspace"
        );
    }

    #[test]
    fn reads_rules_and_extends_the_globals() {
        let c = parse(
            r#"{"affected": {"version": 1, "global": ["scripts/ci/**"], "rules": [
                {"paths": ["migrations/**"], "select": {"go": ["./internal/repositories/..."], "suites": ["migrations"]}},
                {"paths": ["api/**/*.proto"], "select": "all"},
                {"paths": ["docs/**"], "select": "none"}]}}"#,
        )
        .unwrap();
        assert!(c.is_global("scripts/ci/run.py"));
        assert!(c.is_global("go.mod"), "defaults stay");
        assert_eq!(c.rules.len(), 3);
        assert_eq!(
            c.rules[0].select,
            RuleSelect::Targets {
                go: vec!["./internal/repositories/...".into()],
                rust: vec![],
                suites: vec!["migrations".into()],
            }
        );
        assert_eq!(c.rules[1].select, RuleSelect::All);
        assert_eq!(c.rules[2].select, RuleSelect::None);
    }

    #[test]
    fn round5_builtin_globals_ignore_ascii_case() {
        let c = Config::defaults();
        for pattern in DEFAULT_GLOBAL {
            let path = pattern.replace("**", "nested");
            for variant in [path.to_ascii_lowercase(), path.to_ascii_uppercase()] {
                assert!(c.is_global(&variant), "{variant}");
            }
        }
    }

    #[test]
    fn round5_user_global_globs_remain_case_sensitive() {
        let c = parse(r#"{"affected":{"global":["Scripts/CI/**"]}}"#).unwrap();
        assert!(c.is_global("Scripts/CI/test.sh"));
        assert!(!c.is_global("scripts/ci/test.sh"));
    }

    #[test]
    fn anything_unexpected_is_an_error() {
        for bad in [
            "null",
            "[]",
            "1",
            r#"{"affected": []}"#,
            r#"{"affected": {"version": 2}}"#,
            r#"{"affected": {"rulez": []}}"#,
            r#"{"affected": {"global": []}}"#,
            r#"{"affected": {"global": ["/abs"]}}"#,
            r#"{"affected": {"rules": {}}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"]}]}}"#,
            r#"{"affected": {"rules": [{"select": "all"}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": "some"}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": {}}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": {"go": ["internal/x"]}}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": {"go": ["./../x"]}}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": {"suites": ["a b"]}}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": {"python": ["x"]}}]}}"#,
            r#"{"affected": {"rules": [{"paths": ["a"], "select": "all", "why": "x"}]}}"#,
            "not json",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn go_patterns() {
        for good in [".", "./...", "./internal/x", "./internal/x/..."] {
            assert!(is_go_pattern(good), "{good}");
        }
        for bad in ["internal/x", "./", "./a//b", "./a/../b", "./.../x", "../x"] {
            assert!(!is_go_pattern(bad), "{bad}");
        }
    }
}
