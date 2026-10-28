//! Projections: the files outside `.mdkg` that mdkg writes, each from what
//! is canonical inside it (format 0.7, section 2.1, `projections`).
//!
//! ```json
//! {"projections": ["agents-md", "mcp:claude", "skills:.claude/skills"]}
//! ```
//!
//! A config that lists them gets exactly those: nothing else outside
//! `.mdkg` is written, and `init` lists none. A config without the key keeps
//! today's behaviour, every projection but the hooks, so a graph made before
//! the key existed sees no change.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::error::RunError;

/// The config key.
pub const KEY: &str = "projections";

/// The code an unknown projection is refused with.
pub const CODE_CONFIG: &str = "usage.invalid_arguments";

/// An agent harness a projection is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Harness {
    Claude,
    Codex,
    OpenCode,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::OpenCode];

    /// Its name in a projection (`mcp:claude`).
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
        }
    }
}

/// The AGENTS.md block.
pub const AGENTS_MD: &str = "agents-md";

/// The skill mirrors, each a folder at the project's top.
pub const MIRRORS: [&str; 2] = [".agents/skills", ".claude/skills"];

/// Every projection, in the order they're listed and written.
pub const NAMES: &[&str] = &[
    AGENTS_MD,
    "mcp:claude",
    "mcp:codex",
    "mcp:opencode",
    "skills:.agents/skills",
    "skills:.claude/skills",
    "hooks:claude",
    "hooks:codex",
    "hooks:opencode",
];

/// What a config without the key gets: every projection there was before
/// the key existed.
pub const LEGACY: &[&str] = &[
    AGENTS_MD,
    "mcp:claude",
    "mcp:codex",
    "mcp:opencode",
    "skills:.agents/skills",
    "skills:.claude/skills",
];

/// The projections a project enables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projections {
    declared: bool,
    set: BTreeSet<&'static str>,
}

fn bad(what: String) -> RunError {
    RunError::usage(CODE_CONFIG, what)
        .with_hint(format!("projections are any of: {}", NAMES.join(", ")))
}

/// A projection's canonical name, or why it isn't one.
pub fn name(raw: &str) -> Result<&'static str, RunError> {
    let raw = raw.trim();
    NAMES
        .iter()
        .copied()
        .find(|n| *n == raw)
        .ok_or_else(|| bad(format!("{raw:?} isn't a projection")))
}

/// The names in a comma-separated list (`mcp:claude,agents-md`).
pub fn names(list: &str) -> Result<Vec<&'static str>, RunError> {
    list.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(name)
        .collect()
}

impl Projections {
    /// The projections a parsed `.mdkg/config.json` enables; today's set
    /// when it has no `projections` key.
    pub fn from_config(config: &Value) -> Result<Self, RunError> {
        let Some(listed) = config.get(KEY).filter(|v| !v.is_null()) else {
            return Ok(Self {
                declared: false,
                set: LEGACY.iter().copied().collect(),
            });
        };
        let Some(items) = listed.as_array() else {
            return Err(bad(format!("{KEY} must be a list of names")));
        };
        let mut set = BTreeSet::new();
        for item in items {
            let Some(s) = item.as_str() else {
                return Err(bad(format!("{KEY} must be a list of names")));
            };
            set.insert(name(s)?);
        }
        Ok(Self {
            declared: true,
            set,
        })
    }

    /// The projections `root`'s `.mdkg/config.json` enables.
    pub fn read(root: &Path) -> Result<Self, RunError> {
        Self::from_config(&crate::orchestration::load_config(root)?)
    }

    /// Exactly these, as a config that lists them.
    pub fn of(names: &[&'static str]) -> Self {
        Self {
            declared: true,
            set: names.iter().copied().collect(),
        }
    }

    /// None: what `init` writes.
    pub fn none() -> Self {
        Self::of(&[])
    }

    /// Whether the config lists them (false: today's behaviour, no key).
    pub fn declared(&self) -> bool {
        self.declared
    }

    pub fn has(&self, name: &str) -> bool {
        self.set.contains(name)
    }

    pub fn mcp(&self, h: Harness) -> bool {
        self.has(&format!("mcp:{}", h.name()))
    }

    pub fn hooks(&self, h: Harness) -> bool {
        self.has(&format!("hooks:{}", h.name()))
    }

    pub fn agents_md(&self) -> bool {
        self.has(AGENTS_MD)
    }

    /// The skill mirrors enabled, in [`MIRRORS`]'s order.
    pub fn mirrors(&self) -> Vec<&'static str> {
        MIRRORS
            .iter()
            .copied()
            .filter(|m| self.has(&format!("skills:{m}")))
            .collect()
    }

    /// The enabled names, in [`NAMES`]' order.
    pub fn names(&self) -> Vec<&'static str> {
        NAMES
            .iter()
            .copied()
            .filter(|n| self.set.contains(n))
            .collect()
    }

    /// These with `enable` added and `disable` taken out, as a list.
    pub fn with(&self, enable: &[&'static str], disable: &[&'static str]) -> Self {
        let mut set = self.set.clone();
        set.extend(enable.iter().copied());
        for d in disable {
            set.remove(d);
        }
        Self {
            declared: true,
            set,
        }
    }
}

/// `config` (a `.mdkg/config.json`'s text) with `projections` set to
/// `names`: every other member keeps its place and value, the key goes in
/// its old place or last, and the file keeps its indentation, final
/// newline and line endings. `None` when the text isn't a JSON object.
pub fn set_in_config(config: &str, names: &[&str]) -> Option<String> {
    let v: Value = serde_json::from_str(config).ok()?;
    let mut map = v.as_object()?.clone();
    let list = Value::Array(names.iter().map(|n| Value::String((*n).into())).collect());
    match map.get_mut(KEY) {
        Some(slot) => *slot = list,
        None => {
            map.insert(KEY.into(), list);
        }
    }
    let indent: String = config
        .split('\n')
        .nth(1)
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
        .filter(|i: &String| !i.is_empty())
        .unwrap_or_else(|| "  ".into());
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    serde::Serialize::serialize(&Value::Object(map), &mut ser).ok()?;
    let mut text = String::from_utf8(bytes).ok()?;
    if config.ends_with('\n') {
        text.push('\n');
    }
    if config.contains("\r\n") {
        text = text.replace('\n', "\r\n");
    }
    Some(text)
}

/// Records `p` in `root`'s `.mdkg/config.json`: written beside it and
/// renamed over it, so a reader sees the old file or the new one.
pub fn write(root: &Path, p: &Projections) -> Result<(), RunError> {
    let path = root.join(".mdkg").join("config.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| RunError::io("can't read .mdkg/config.json", e))?;
    let new = set_in_config(&text, &p.names())
        .ok_or_else(|| RunError::usage(CODE_CONFIG, ".mdkg/config.json isn't a JSON object"))?;
    if new == text {
        return Ok(());
    }
    let tmp = path.with_extension(format!("json.{}.projections.tmp", std::process::id()));
    let fail = |e| RunError::io("can't write .mdkg/config.json", e);
    std::fs::write(&tmp, new.as_bytes()).map_err(fail)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(e)
    })
}

/// The projections `root` already has, by what's in their files: the
/// AGENTS.md block, an `mdkg` MCP server in each harness's config, and
/// each mirror mdkg manages (its `.mdkg-managed.json`). What `migrate`
/// records, so an upgraded repository keeps exactly what it had.
pub fn detect(root: &Path) -> Vec<&'static str> {
    let text = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    let json = |rel: &str| text(rel).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let mut found = Vec::new();
    if text("AGENTS.md").is_some_and(|t| {
        t.contains(crate::install::BLOCK_START) || t.contains("<!-- mdkg:harness:start -->")
    }) {
        found.push(AGENTS_MD);
    }
    if json(".mcp.json").is_some_and(|v| v.pointer("/mcpServers/mdkg").is_some()) {
        found.push("mcp:claude");
    }
    if text(".codex/config.toml")
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .is_some_and(|d| d.get("mcp_servers").and_then(|t| t.get("mdkg")).is_some())
    {
        found.push("mcp:codex");
    }
    if json("opencode.json").is_some_and(|v| v.pointer("/mcp/mdkg").is_some()) {
        found.push("mcp:opencode");
    }
    for m in MIRRORS {
        if root.join(m).join(crate::install::MANAGED_LIST).is_file() {
            found.push(if m == MIRRORS[0] {
                "skills:.agents/skills"
            } else {
                "skills:.claude/skills"
            });
        }
    }
    found
}

/// What a config from before projections lists for a repository: what
/// [`detect`] finds, plus each mirror its `customization.skill_mirrors.targets`
/// names, which a sync would have written.
pub fn upgraded(root: &Path, config: &Value) -> Vec<&'static str> {
    let found = detect(root);
    let targets = legacy_targets(config);
    NAMES
        .iter()
        .copied()
        .filter(|n| {
            found.contains(n)
                || n.strip_prefix("skills:")
                    .is_some_and(|m| targets.contains(&m))
        })
        .collect()
}

/// The mirrors a config's `customization.skill_mirrors.targets` names.
pub fn legacy_targets(config: &Value) -> Vec<&str> {
    config
        .pointer("/customization/skill_mirrors/targets")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_key_keeps_todays_projections() {
        let p = Projections::from_config(&json!({"schema_version": 3})).expect("ok");
        assert!(!p.declared());
        assert!(p.agents_md() && p.mcp(Harness::Codex));
        assert_eq!(p.mirrors(), MIRRORS.to_vec());
        assert!(!p.hooks(Harness::Claude));
    }

    #[test]
    fn a_list_is_exactly_what_it_names() {
        let p = Projections::from_config(
            &json!({"projections": ["mcp:claude", "skills:.claude/skills"]}),
        )
        .expect("ok");
        assert!(p.declared() && p.mcp(Harness::Claude));
        assert!(!p.agents_md() && !p.mcp(Harness::Codex));
        assert_eq!(p.mirrors(), vec![".claude/skills"]);
        let none = Projections::from_config(&json!({"projections": []})).expect("ok");
        assert!(none.declared() && none.names().is_empty());
    }

    #[test]
    fn unknown_names_and_shapes_are_refused() {
        for bad in [
            json!({"projections": ["mcp:cursor"]}),
            json!({"projections": "agents-md"}),
            json!({"projections": [3]}),
        ] {
            assert!(Projections::from_config(&bad).is_err(), "{bad}");
        }
        assert!(names("agents-md, mcp:codex").is_ok());
        assert!(names("agents-md,nope").is_err());
    }

    #[test]
    fn enabling_and_disabling_make_a_list() {
        let p = Projections::none().with(&["mcp:codex", AGENTS_MD], &[]);
        assert_eq!(p.names(), vec![AGENTS_MD, "mcp:codex"]);
        let p = Projections::from_config(&json!({}))
            .expect("ok")
            .with(&[], &["mcp:opencode"]);
        assert!(p.declared() && !p.mcp(Harness::OpenCode) && p.mcp(Harness::Claude));
    }

    #[test]
    fn the_config_keeps_its_shape() {
        let config =
            "{\n    \"schema_version\": 3,\n    \"projections\": [],\n    \"work\": {}\n}\n";
        let out = set_in_config(config, &[AGENTS_MD]).expect("object");
        assert_eq!(
            out,
            "{\n    \"schema_version\": 3,\n    \"projections\": [\n        \"agents-md\"\n    ],\n    \"work\": {}\n}\n"
        );
        let crlf = "{\r\n  \"a\": 1\r\n}\r\n";
        assert_eq!(
            set_in_config(crlf, &[]).expect("object"),
            "{\r\n  \"a\": 1,\r\n  \"projections\": []\r\n}\r\n"
        );
        assert!(set_in_config("[1]", &[]).is_none());
    }
}
