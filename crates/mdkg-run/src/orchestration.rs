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
pub mod runtime;

/// Read one authority snapshot for the selected graph root. The graph may be
/// nested inside its repository; carry the caller's request-scoped Git trust
/// into the graph reader rather than inheriting a shared daemon's pins.
pub fn authority(root: &Path) -> Result<mdkg_graph::graphref::Authority, RunError> {
    graph_read(root, true, mdkg_graph::graphref::authority_in)
}

/// [`authority`], or `None` when the checkout positively has no graph: an
/// unborn or unregistered checkout-mode HEAD that commits no graph config
/// (dec-01a188da). Every other failure stays an error.
pub(crate) fn authority_or_absent(
    root: &Path,
) -> Result<Option<mdkg_graph::graphref::Authority>, RunError> {
    graph_read(root, true, mdkg_graph::graphref::lookup_in).map(|lookup| match lookup {
        mdkg_graph::graphref::Lookup::Found(authority) => Some(authority),
        mdkg_graph::graphref::Lookup::Absent(_) => None,
    })
}

/// [`authority_or_absent`] reading only the committed config, for runtime
/// settings: no agent definition is parsed, so a broken one doesn't
/// refuse them (orchestrator, 2026-11-02, #15433).
pub fn config_authority_or_absent(
    root: &Path,
) -> Result<Option<mdkg_graph::graphref::Authority>, RunError> {
    graph_read(root, true, mdkg_graph::graphref::lookup_config_in).map(|lookup| match lookup {
        mdkg_graph::graphref::Lookup::Found(authority) => Some(authority),
        mdkg_graph::graphref::Lookup::Absent(_) => None,
    })
}

/// [`config_authority_or_absent`] for a command that already checked
/// `root`'s checkout freshly (`Repo::open`, a ref-mode bind): reuses that
/// check ([`mdkg_git::checked_worktree`]; a fresh one when none is kept),
/// so a command asks git for the repository's folders once (git_spawns).
pub fn config_authority_or_absent_checked(
    root: &Path,
) -> Result<Option<mdkg_graph::graphref::Authority>, RunError> {
    graph_read(root, false, mdkg_graph::graphref::lookup_config_in).map(|lookup| match lookup {
        mdkg_graph::graphref::Lookup::Found(authority) => Some(authority),
        mdkg_graph::graphref::Lookup::Absent(_) => None,
    })
}

/// [`authority`] reading only the committed config (#15706): the ref-mode
/// entry gate for coordination commands, which a broken agent definition
/// must not refuse (#15433).
pub fn config_authority(root: &Path) -> Result<mdkg_graph::graphref::Authority, RunError> {
    config_authority_or_absent(root)?.ok_or_else(|| {
        RunError::failure(
            "graph.authority_invalid",
            "this checkout commits no graph config",
        )
    })
}

fn graph_read<T>(
    root: &Path,
    fresh: bool,
    read: impl FnOnce(&Path, &str) -> Result<T, mdkg_graph::graphref::Error>,
) -> Result<T, RunError> {
    let trust = mdkg_coord_sqlite::git::WorktreeTrust::current();
    let root =
        std::fs::canonicalize(root).map_err(|e| RunError::io("can't resolve the graph root", e))?;
    // A fresh check unless the caller checked this command already:
    // callers decide writes and admissions from what this reads
    // (dec-01a18ee0 item 9; #13401).
    let checked = if fresh {
        mdkg_coord_sqlite::git::check_worktree_with_trust(&root, &trust)
    } else {
        mdkg_git::checked_worktree_with_trust(&root, &trust)
    };
    let checkout = checked.map_err(|e| {
        RunError::failure(
            "config.unreadable",
            format!("can't check the graph's checkout: {e}"),
        )
    })?;
    let relative = root.strip_prefix(&checkout.top).map_err(|_| {
        RunError::failure(
            "graph.authority_invalid",
            "graph root is outside its checkout",
        )
    })?;
    let mut parts = Vec::new();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(RunError::failure(
                "graph.authority_invalid",
                "invalid graph root",
            ));
        };
        let part = part.to_str().ok_or_else(|| {
            RunError::failure("graph.authority_invalid", "graph root is not UTF-8")
        })?;
        if part.contains('\\') {
            return Err(RunError::failure(
                "graph.authority_invalid",
                "a graph-root component contains a backslash",
            )
            .with_hint("rename the graph-root folder to remove the backslash, then retry"));
        }
        parts.push(part);
    }
    let prefix = parts.join("/");
    // Read with this check itself: the shared cache's entry for the top
    // may be another thread's by now (#13510). The binding matches the top
    // exactly, so `read` gets `checkout.top`, never `root`.
    mdkg_graph::graphref::with_git_trust(trust.common, trust.worktree, || {
        mdkg_git::with_checkout(&checkout, || read(&checkout.top, &prefix))
    })
    .map_err(|e| RunError::failure(e.code, e.message))
}

/// A checkout's prospective `.mdkg/config.json`, parsed; `null` when absent.
/// Projection and installation planning use this before a graph is committed.
/// Agent definitions and worktree identity must use the authority reader.
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
    agent::authority_config(&authority(root)?)
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

    /// The names committed by the graph authority selected for `root`.
    pub fn load(root: &Path) -> Result<Self, RunError> {
        Self::from_config(&authority(root)?.config)
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
    fn a_worktree_replaced_at_the_same_path_is_read_fresh() {
        let dir = std::env::temp_dir().join(format!("mdkg-graph-read-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("main")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let git = |at: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(at)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let main = dir.join("main");
        git(&main, &["init", "-q", "-b", "main"]);
        let config = |branch: &str| {
            serde_json::json!({
                "schema_version": 3, "format_version": "0.7",
                "graph_id": "0192f3a0-0000-7000-8000-000000000000",
                "orchestration": {"branch": format!("{branch}/{{agent}}")}
            })
            .to_string()
        };
        std::fs::create_dir_all(main.join(".mdkg")).unwrap();
        for branch in ["one", "two"] {
            git(&main, &["checkout", "-q", "-B", branch]);
            std::fs::write(main.join(".mdkg/config.json"), config(branch)).unwrap();
            git(&main, &["add", "-A"]);
            git(&main, &["commit", "-q", "-m", branch]);
        }
        git(&main, &["checkout", "-q", "-B", "main"]);
        let (at, moved) = (dir.join("wt"), dir.join("moved"));
        let wt = at.to_str().unwrap();
        git(&main, &["worktree", "add", "-q", wt, "one"]);
        assert_eq!(Names::load(&at).unwrap().branch_for("a"), "one/a");
        // Within one process: the first worktree moves away, keeping its
        // admin folder, and another branch is added at the same path.
        git(&main, &["worktree", "move", wt, moved.to_str().unwrap()]);
        git(&main, &["worktree", "add", "-q", wt, "two"]);
        assert_eq!(Names::load(&at).unwrap().branch_for("a"), "two/a");
        std::fs::remove_dir_all(&dir).ok();
    }

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
