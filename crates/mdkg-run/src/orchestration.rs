//! The orchestration settings in `.mdkg/config.json`: the names an
//! agent's branch and worktree get.
//!
//! ```json
//! {"orchestration": {"branch": "agent/{agent}", "worktree": ".worktrees/{agent}"}}
//! ```
//!
//! `{agent}` is the agent's name, a `:` written `%3A` as in its state
//! file's name, so two agents never share a branch or a folder. Unset keys
//! keep these defaults.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::error::RunError;

pub mod agent;

/// A graph's `.mdkg/config.json`, parsed; `null` when there's none.
pub fn load_config(root: &Path) -> Result<Value, RunError> {
    let path = root.join(".mdkg").join("config.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            RunError::usage(CODE_CONFIG, format!("{} isn't JSON: {e}", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(RunError::io("can't read .mdkg/config.json", e)),
    }
}

/// Agent configuration with committed definition files overlaid on the
/// legacy inline entries. Other config consumers do not read agent files.
pub fn load_agent_config(root: &Path) -> Result<Value, RunError> {
    let mut config = load_config(root)?;
    agent::merge(root, &mut config)?;
    Ok(config)
}

/// The placeholder both patterns hold.
pub const AGENT: &str = "{agent}";

/// The default branch pattern.
pub const DEFAULT_BRANCH: &str = "agent/{agent}";

/// The default worktree pattern, from the main checkout's top.
pub const DEFAULT_WORKTREE: &str = ".worktrees/{agent}";

/// The code a bad orchestration setting is refused with.
pub const CODE_CONFIG: &str = "usage.invalid_arguments";

/// Where agents' branches and worktrees are: `orchestration.branch` and
/// `orchestration.worktree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    branch: String,
    worktree: String,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            branch: DEFAULT_BRANCH.into(),
            worktree: DEFAULT_WORKTREE.into(),
        }
    }
}

fn bad(what: String) -> RunError {
    RunError::usage(CODE_CONFIG, what).with_hint(
        "fix `orchestration` in .mdkg/config.json (the defaults: `agent/{agent}`, `.worktrees/{agent}`)",
    )
}

/// Why a branch pattern can't name agents' branches, or `None`: it holds
/// `{agent}` once, outside `refs/`, and with a name in its place it's a
/// branch name git takes (`git check-ref-format --branch`). The launcher
/// runs any pattern this passes; `init` and `doctor` refuse what it
/// refuses, so a graph never names branches its launcher can't make
/// (format 0.7, section 2.1).
pub fn branch_problem(pattern: &str) -> Option<&'static str> {
    if pattern.matches(AGENT).count() != 1 {
        return Some("must hold `{agent}` exactly once");
    }
    // `{agent}` stands in for a name like `alice`.
    let sample = pattern.replace(AGENT, "agent");
    if sample.starts_with("refs/") {
        return Some("can't start with `refs/`: it is the short name of each branch");
    }
    if sample
        .chars()
        .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
    {
        return Some("can't hold control characters, spaces or any of ~ ^ : ? * [ \\");
    }
    if sample.starts_with(['-', '/'])
        || sample.ends_with(['/', '.'])
        || sample.contains("..")
        || sample.contains("//")
        || sample.contains("@{")
        || sample
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return Some("isn't a valid git branch name");
    }
    None
}

/// Why a worktree pattern can't place agents' worktrees, or `None`: it
/// holds `{agent}` once, in its last part (so a link one level up can't
/// make two agents share a folder), and is a relative path from the main
/// checkout of plain and `..` parts, with no control characters. The
/// launcher resolves `..` safely wherever it is.
pub fn worktree_problem(pattern: &str) -> Option<&'static str> {
    if pattern.matches(AGENT).count() != 1 {
        return Some("must hold `{agent}` exactly once");
    }
    if pattern.chars().any(char::is_control) {
        return Some("can't hold control characters");
    }
    let path = Path::new(pattern);
    let parts_ok = path.components().all(|c| match c {
        Component::Normal(_) | Component::ParentDir => true,
        Component::CurDir | Component::RootDir | Component::Prefix(_) => false,
    });
    if path.is_absolute() || !parts_ok || pattern.ends_with('/') || pattern.contains("//") {
        return Some("must be a relative path from the main checkout, of plain and `..` parts");
    }
    let last = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !last.contains(AGENT) {
        return Some("must hold `{agent}` in its last part, so no two agents share a folder");
    }
    None
}

/// Whether a worktree pattern leads more than one folder above the main
/// checkout once its `..` parts are folded (`../../x/{agent}`; not
/// `../agents/{agent}`, beside it). The config may say so (format 0.7,
/// section 2.1), but the launcher makes no worktree outside the folder
/// that holds the main checkout, `init` won't write such a pattern and
/// `doctor` warns about one.
pub fn leaves_project(pattern: &str) -> bool {
    let (mut depth, mut up) = (0usize, 0usize);
    for c in Path::new(pattern).components() {
        match c {
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => up += 1,
            _ => depth += 1,
        }
    }
    up > 1
}

impl Names {
    /// Checked patterns ([`branch_problem`], [`worktree_problem`]).
    pub fn new(branch: &str, worktree: &str) -> Result<Self, RunError> {
        if let Some(why) = branch_problem(branch) {
            return Err(bad(format!("orchestration.branch {why}: {branch:?}")));
        }
        if let Some(why) = worktree_problem(worktree) {
            return Err(bad(format!("orchestration.worktree {why}: {worktree:?}")));
        }
        Ok(Self {
            branch: branch.into(),
            worktree: worktree.into(),
        })
    }

    /// The names in a parsed `.mdkg/config.json`: the defaults for keys it
    /// doesn't set.
    pub fn from_config(config: &Value) -> Result<Self, RunError> {
        let o = config.get("orchestration");
        if o.is_some_and(|o| !o.is_object() && !o.is_null()) {
            return Err(bad("orchestration must be an object".into()));
        }
        let get = |key: &str, default: &str| -> Result<String, RunError> {
            match o.and_then(|o| o.get(key)) {
                None | Some(Value::Null) => Ok(default.into()),
                Some(Value::String(s)) => Ok(s.clone()),
                Some(_) => Err(bad(format!("orchestration.{key} must be a string"))),
            }
        };
        Self::new(
            &get("branch", DEFAULT_BRANCH)?,
            &get("worktree", DEFAULT_WORKTREE)?,
        )
    }

    /// The names a graph's `.mdkg/config.json` sets, read from `root`; the
    /// defaults when there's no config file.
    pub fn load(root: &Path) -> Result<Self, RunError> {
        Self::from_config(&load_config(root)?)
    }

    /// The agent's own branch, as a short name.
    pub fn branch_for(&self, agent: &str) -> String {
        self.branch.replace(AGENT, &crate::state::file_name(agent))
    }

    /// The agent's worktree, relative to the main checkout's top.
    pub fn worktree_for(&self, agent: &str) -> PathBuf {
        PathBuf::from(
            self.worktree
                .replace(AGENT, &crate::state::file_name(agent)),
        )
    }

    /// The branch pattern, for messages.
    pub fn branch_pattern(&self) -> &str {
        &self.branch
    }

    /// The worktree pattern, for messages.
    pub fn worktree_pattern(&self) -> &str {
        &self.worktree
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_edd_6s() {
        let n = Names::default();
        assert_eq!(n.branch_for("lane-1"), "agent/lane-1");
        assert_eq!(n.worktree_for("lane-1"), PathBuf::from(".worktrees/lane-1"));
        assert_eq!(n.branch_for("codex:pr19"), "agent/codex%3Apr19");
        assert_eq!(
            Names::from_config(&serde_json::json!({"schema_version": 1})).expect("ok"),
            n
        );
    }

    #[test]
    fn a_config_sets_either_name() {
        let n = Names::from_config(&serde_json::json!({"orchestration": {
            "branch": "lanes/{agent}/work",
            "worktree": "../agents/wt-{agent}",
        }}))
        .expect("ok");
        assert_eq!(n.branch_for("a"), "lanes/a/work");
        assert_eq!(n.worktree_for("a"), PathBuf::from("../agents/wt-a"));
        let only =
            Names::from_config(&serde_json::json!({"orchestration": {"branch": "w/{agent}"}}))
                .expect("ok");
        assert_eq!(only.worktree_for("a"), PathBuf::from(".worktrees/a"));
    }

    #[test]
    fn branch_patterns_follow_git_ref_rules() {
        assert_eq!(branch_problem("agent/{agent}"), None);
        assert_eq!(branch_problem("lanes/{agent}-work"), None);
        // git takes these, so the launcher runs them, as it always has.
        assert_eq!(branch_problem("agent/\"{agent}\""), None);
        assert_eq!(branch_problem("é/{agent}"), None);
        assert!(branch_problem("agent {agent}").is_some());
        assert!(branch_problem("-{agent}").is_some());
        assert!(branch_problem("{agent}.lock").is_some());
        assert!(branch_problem("a..{agent}").is_some());
        assert!(branch_problem("a/.{agent}").is_some());
        assert!(branch_problem("a/{agent}.").is_some());
        assert!(branch_problem("a@{/{agent}").is_some());
        assert!(branch_problem("refs/{agent}").is_some());
        assert!(branch_problem("agent/x").is_some());
    }

    #[test]
    fn worktree_patterns_stay_relative_with_the_agent_last() {
        assert_eq!(worktree_problem(".worktrees/{agent}"), None);
        assert_eq!(worktree_problem("../repo-{agent}"), None);
        assert_eq!(worktree_problem("packages/agent-{agent}"), None);
        // Shapes the launcher has always resolved safely stay launchable.
        assert_eq!(worktree_problem("scratch/../.worktrees/{agent}"), None);
        assert_eq!(worktree_problem("~x/{agent}"), None);
        assert_eq!(worktree_problem("wt\"q\"/{agent}"), None);
        assert!(worktree_problem("/tmp/{agent}").is_some());
        assert!(worktree_problem("./{agent}").is_some());
        assert!(worktree_problem("{agent}/wt").is_some());
        assert!(worktree_problem("{agent}/checkout").is_some());
        assert!(worktree_problem("a//{agent}").is_some());
        // The format allows any depth; where worktrees may be made is the
        // launcher's check.
        assert_eq!(worktree_problem("../../agents/{agent}"), None);
    }

    #[test]
    fn patterns_that_leave_the_project_are_named() {
        assert!(!leaves_project(".worktrees/{agent}"));
        assert!(!leaves_project("../agents/{agent}"));
        assert!(!leaves_project("a/../../x-{agent}"));
        assert!(!leaves_project("scratch/../.worktrees/{agent}"));
        assert!(leaves_project("../../x/{agent}"));
        assert!(leaves_project("../a/../../b/{agent}"));
        assert!(leaves_project("../../../../../tmp-escape/{agent}"));
    }

    #[test]
    fn patterns_that_could_share_or_escape_are_refused() {
        for (branch, worktree) in [
            ("agent/x", DEFAULT_WORKTREE),
            ("{agent}/{agent}", DEFAULT_WORKTREE),
            ("refs/heads/{agent}", DEFAULT_WORKTREE),
            ("a b/{agent}", DEFAULT_WORKTREE),
            ("a..b/{agent}", DEFAULT_WORKTREE),
            ("-{agent}", DEFAULT_WORKTREE),
            (DEFAULT_BRANCH, "/abs/{agent}"),
            (DEFAULT_BRANCH, ".worktrees/x"),
            (DEFAULT_BRANCH, "{agent}/wt"),
            (DEFAULT_BRANCH, "./{agent}"),
            (DEFAULT_BRANCH, ".worktrees//{agent}"),
            (DEFAULT_BRANCH, ".worktrees/{agent}/"),
            (DEFAULT_BRANCH, "src\n/wt-{agent}"),
        ] {
            assert!(Names::new(branch, worktree).is_err(), "{branch} {worktree}");
        }
        for bad in [
            serde_json::json!({"orchestration": "x"}),
            serde_json::json!({"orchestration": {"branch": 3}}),
        ] {
            assert!(Names::from_config(&bad).is_err(), "{bad}");
        }
    }
}
