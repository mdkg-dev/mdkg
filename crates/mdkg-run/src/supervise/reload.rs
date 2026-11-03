//! A long-running `agents up` restarts at turn boundaries: when a selected
//! agent's definition (or the config that sets its policy) changes in the
//! graph authority (the graph ref, in ref mode: R1), when its binary is
//! replaced, or when a newer build moves the coordinator store, it stops
//! admitting work, lets every worker finish its turn or queue dispatch, and
//! re-execs itself with the same arguments. The new process reads the definitions afresh and keeps each
//! agent's unfinished watch through an exec token. A store that moved
//! under an unreplaced binary stops instead: re-execing the same build
//! would only fail again.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mdkg_coord_sqlite::git;
use serde::{Deserialize, Serialize};

/// Carries the drained process's watches into its re-exec. It names that
/// process's pid, which exec keeps, so a child that inherits the variable
/// doesn't adopt them: its pid differs. It isn't a credential: any process
/// of the same user can write one with its own pid, as it can run `agents
/// resume`. Either way an unresolved turn or dispatch is never adopted.
pub const EXEC_ENV: &str = "MDKG_SUPERVISOR_EXEC";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecToken {
    pub pid: u32,
    /// Each agent's watch id.
    pub watches: BTreeMap<String, String>,
}

impl ExecToken {
    /// The token this process was re-execed with, if any. One for another
    /// pid, or one that doesn't parse, is ignored.
    pub fn inherited() -> Option<Self> {
        Self::parse(&std::env::var_os(EXEC_ENV)?, std::process::id())
    }

    fn parse(value: &std::ffi::OsStr, pid: u32) -> Option<Self> {
        let token: Self = serde_json::from_str(value.to_str()?).ok()?;
        (token.pid == pid).then_some(token)
    }

    pub fn watch(&self, agent: &str) -> Option<&str> {
        self.watches.get(agent).map(String::as_str)
    }
}

/// What a supervisor's definitions come from: the graph authority (R1), as
/// one pinned read gives it. Each poll reads the authority again and
/// compares the committed config and each selected agent's definition, so
/// whatever publishes to the graph ref counts, however it got there. In
/// ref mode a poll first resolves the graph ref's tip, and reads no further
/// while it's the pinned commit.
#[derive(Debug)]
pub struct Definitions {
    /// The worktree, for naming an edit the authority doesn't hold.
    top: PathBuf,
    /// The graph root the authority is read for.
    root: PathBuf,
    prefix: String,
    agents: Vec<String>,
    paths: Vec<String>,
    seen: BTreeMap<String, String>,
    /// The pinned read's source, its committed config, and each selected
    /// agent's definition blob (`None` when it has no file of its own).
    source: String,
    config: serde_json::Value,
    blobs: BTreeMap<String, Option<String>>,
    /// In ref mode: the graph ref and the pinned commit.
    pinned: Option<(String, String)>,
    /// When the authority was last read in full.
    read_at: std::sync::Mutex<Instant>,
}

/// How often a poll reads the authority in full even when the graph ref's
/// tip hasn't moved, so a change to which ref or root the repository uses
/// is still seen.
const FULL_READ: Duration = Duration::from_secs(60);

/// An agent's definition file inside its worktree, as
/// [`crate::orchestration::agent::path`] names it: the name unencoded.
fn definition_path(prefix: &str, agent: &str) -> String {
    format!("{prefix}.mdkg/agents/{agent}.json")
}

impl Definitions {
    /// The first path under `.mdkg/agents/`, or `.mdkg/config.json`, that
    /// `git status` reports: an uncommitted edit, addition or deletion of
    /// any definition, selected or not, since which agents run can depend on
    /// them. A status git can't give counts as `.mdkg/agents`.
    pub fn dirty(&self) -> Option<String> {
        let config = &self.paths[0];
        let agents = format!("{}agents", config.strip_suffix("config.json")?);
        let out = git::command(&self.top)
            .args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--",
            ])
            .args([config.as_str(), agents.as_str()])
            .run(None);
        match out {
            Ok(out) if out.status.success() => out
                .stdout
                .split(|b| *b == 0)
                .find(|r| r.len() > 3)
                .map(|r| String::from_utf8_lossy(&r[3..]).into_owned()),
            _ => Some(agents),
        }
    }

    /// `top` is the worktree and `root` the graph root, at `prefix` (`""`
    /// or `sub/dir/`) inside it. What counts as a change is measured from
    /// `authority`, the read the configuration was built from.
    pub fn new(
        top: &Path,
        root: &Path,
        prefix: &str,
        agents: &[String],
        authority: &mdkg_graph::graphref::Authority,
    ) -> Self {
        let mut paths = vec![format!("{prefix}.mdkg/config.json")];
        paths.extend(agents.iter().map(|a| definition_path(prefix, a)));
        let blobs = agents
            .iter()
            .map(|a| {
                let def = authority.definitions.get(a).filter(|d| !d.inline);
                (definition_path(prefix, a), def.map(|d| d.blob.clone()))
            })
            .collect();
        let mut this = Self {
            top: top.into(),
            root: root.into(),
            prefix: prefix.into(),
            agents: agents.to_vec(),
            paths,
            seen: BTreeMap::new(),
            source: authority.source.clone(),
            config: authority.config.clone(),
            blobs,
            pinned: authority
                .graph_ref
                .clone()
                .map(|r| (r, authority.commit.clone())),
            read_at: std::sync::Mutex::new(Instant::now()),
        };
        this.seen = this.fingerprint(authority);
        this
    }

    /// Each watched path's committed content in `authority`: the config as
    /// JSON, and each selected definition's blob and value. An agent the
    /// authority doesn't define has no entry.
    fn fingerprint(&self, authority: &mdkg_graph::graphref::Authority) -> BTreeMap<String, String> {
        let mut seen = BTreeMap::new();
        seen.insert(self.paths[0].clone(), authority.config.to_string());
        for agent in &self.agents {
            if let Some(def) = authority.definitions.get(agent) {
                seen.insert(
                    definition_path(&self.prefix, agent),
                    format!("{}:{}", def.blob, def.value),
                );
            }
        }
        seen
    }

    /// Whether HEAD's tree has `path`; not when HEAD can't be read.
    fn head_has(&self, path: &str) -> bool {
        git::command(&self.top)
            .args(["cat-file", "-e", &format!("HEAD:{path}")])
            .run(None)
            .is_ok_and(|out| out.status.success())
    }

    /// The pinned read's source, as `graph:<ref>@<sha12>` or
    /// `checkout:<rev>@<sha12>`.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The first watched path whose file in the worktree isn't what the
    /// pinned authority holds: an edit not committed, or committed on a
    /// branch but not published to the graph ref. A config that doesn't
    /// parse, and a file git can't hash, count as different. In ref mode a
    /// code branch needn't carry the graph, which lives on the ref (review
    /// #12068): when neither the worktree nor HEAD has the config, a file
    /// the worktree doesn't have isn't an edit. A checkout that carries the
    /// graph still warns on a deletion, committed or not (review #12257).
    pub fn differs(&self) -> Option<String> {
        let missing = |path: &str| !self.top.join(path).exists();
        let config = &self.paths[0];
        let ref_mode = self.pinned.is_some() && missing(config) && !self.head_has(config);
        let here: Option<serde_json::Value> = std::fs::read(self.top.join(config))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if here.as_ref() != Some(&self.config) && !(ref_mode && missing(config)) {
            return Some(config.clone());
        }
        self.blobs
            .iter()
            .find(|(path, blob)| {
                worktree_object(&self.top, path) != **blob && !(ref_mode && missing(path))
            })
            .map(|(path, _)| path.clone())
    }

    /// The first changed path, if any: added, edited or removed in the
    /// graph authority since the pinned read. An authority that can't be
    /// read now changes nothing; the next poll reads it again.
    pub fn changed(&self) -> Option<String> {
        let mut read_at = self.read_at.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((graph_ref, commit)) = &self.pinned
            && read_at.elapsed() < FULL_READ
            && git::commit_of(&self.root, graph_ref).is_ok_and(|tip| tip == *commit)
        {
            return None;
        }
        *read_at = Instant::now();
        drop(read_at);
        // Each full read reads the repository fresh (#13066 D2).
        mdkg_git::fresh_reads();
        let now = crate::orchestration::authority(&self.root).ok()?;
        let now = self.fingerprint(&now);
        self.paths
            .iter()
            .find(|p| now.get(*p) != self.seen.get(*p))
            .cloned()
    }
}

/// Why a long-running watch drains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A committed definition file changed (its path).
    Definition(String),
    /// The binary this process runs, or the path that started it, was
    /// replaced: an install.
    Binary,
    /// The coordinator store is now at versions this build doesn't use
    /// (its label, `3+msg5`): a newer build upgraded it.
    Store(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definition(path) => write!(f, "{path} changed"),
            Self::Binary => f.write_str("the mdkg binary was replaced"),
            Self::Store(label) => write!(
                f,
                "the coordinator store moved to {label} (this build uses {})",
                mdkg_coord_sqlite::build_label()
            ),
        }
    }
}

/// Everything a long-running watch restarts on: its committed definitions,
/// its binary ([`crate::builds::BinaryWatch`]) and the store's versions.
#[derive(Debug)]
pub struct Watch {
    definitions: Definitions,
    binary: Option<crate::builds::BinaryWatch>,
    store: Option<(PathBuf, Option<String>)>,
}

impl Watch {
    /// `binary` is from [`binary_at_start`]; `store` is the coordinator
    /// store's path. A store that can't be read at start isn't watched.
    pub fn new(
        definitions: Definitions,
        binary: Option<crate::builds::BinaryWatch>,
        store: Option<PathBuf>,
    ) -> Self {
        let store = store.and_then(|path| {
            let at_start = mdkg_coord_sqlite::stored_label(&path).ok()?;
            Some((path, at_start))
        });
        Self {
            definitions,
            binary,
            store,
        }
    }

    /// Whether the binary was replaced since start.
    pub fn binary_replaced(&self) -> bool {
        self.binary.as_ref().is_some_and(|b| b.changed())
    }

    /// The first reason to restart, if any. A store counts once it holds
    /// versions other than this build's and other than at start, so this
    /// build's own upgrade of an older store doesn't.
    pub fn changed(&self) -> Option<Reason> {
        if self.binary_replaced() {
            return Some(Reason::Binary);
        }
        if let Some((path, at_start)) = &self.store
            && let Ok(Some(now)) = mdkg_coord_sqlite::stored_label(path)
            && now != mdkg_coord_sqlite::build_label()
            && Some(&now) != at_start.as_ref()
        {
            return Some(Reason::Store(now));
        }
        self.definitions.changed().map(Reason::Definition)
    }
}

/// The running binary and the path that started it, fingerprinted as
/// early as the command can: a replacement before this goes unseen. `None`
/// when either can't be read.
pub fn binary_at_start() -> Option<crate::builds::BinaryWatch> {
    let exe = std::env::current_exe().ok()?;
    let invocation = invocation_path().ok()?;
    crate::builds::BinaryWatch::new(&exe, &invocation).ok()
}

/// The object id git would give `path`'s file in the worktree; `None` when
/// there's no file or it can't be hashed.
fn worktree_object(root: &Path, path: &str) -> Option<String> {
    if !root.join(path).is_file() {
        return None;
    }
    let out = git::command(root)
        .args(["hash-object", "--", path])
        .run(None)
        .ok()?;
    let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !id.is_empty()).then_some(id)
}

/// The commit that last changed `agent`'s definition file as of `at`, the
/// pinned authority's commit this process read; for the turn prompt. `None`
/// when there is none or it can't be read.
pub fn definition_commit(root: &Path, prefix: &str, agent: &str, at: &str) -> Option<String> {
    let path = definition_path(prefix, agent);
    let out = git::command(root)
        .args(["log", "-1", "--format=%H", at, "--"])
        .arg(format!(":(top,literal){path}"))
        .run(None)
        .ok()?;
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && sha.len() >= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(sha)
}

/// The path this process was started by: `argv[0]` when it names a file,
/// else the first `PATH` entry holding it, else the running executable.
pub fn invocation_path() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let Some(arg0) = std::env::args_os().next().map(PathBuf::from) else {
        return Ok(current);
    };
    if arg0.components().count() > 1 {
        return Ok(arg0);
    }
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&arg0))
        .find(|p| p.is_file());
    Ok(found.unwrap_or(current))
}

/// Replaces this process with `program` and its own arguments, carrying
/// `token` and `env`. Returns only on failure.
pub fn exec(program: &Path, token: &ExecToken, env: &[(&str, String)]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let token = match serde_json::to_string(token) {
        Ok(token) => token,
        Err(e) => return std::io::Error::other(e),
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    std::process::Command::new(program)
        .args(args)
        .env(EXEC_ENV, token)
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_for_another_pid_or_malformed_is_ignored() {
        let watch = uuid::Uuid::now_v7().to_string();
        let token = ExecToken {
            pid: 42,
            watches: BTreeMap::from([("alice".to_string(), watch.clone())]),
        };
        let text = serde_json::to_string(&token).unwrap();
        assert_eq!(ExecToken::parse(text.as_ref(), 42), Some(token.clone()));
        assert_eq!(ExecToken::parse(text.as_ref(), 43), None);
        assert_eq!(ExecToken::parse("{".as_ref(), 42), None);
        assert_eq!(token.watch("alice"), Some(watch.as_str()));
        assert_eq!(token.watch("bob"), None);
    }

    /// A throwaway checkout-mode repository whose committed graph holds
    /// `.mdkg/agents/<name>.json` files.
    struct Repo(PathBuf);

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Repo {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("mdkg-reload-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(dir.join(".mdkg/agents")).expect("fixture");
            let repo = Self(std::fs::canonicalize(dir).expect("canonical"));
            repo.git(&["init", "-q", "-b", "main"]);
            std::fs::write(repo.0.join(".mdkg/config.json"), "{}").expect("config");
            repo.git(&["add", "-A"]);
            repo.git(&["commit", "-q", "-m", "graph"]);
            repo
        }

        fn git(&self, args: &[&str]) {
            let out = std::process::Command::new("git")
                .current_dir(&self.0)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.0)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}: {out:?}");
        }

        fn definition(text: &str) -> String {
            serde_json::json!({"schema":"mdkg.agent.v1","harness":"claude",
                "role":"reviewer","instructions":text})
            .to_string()
        }

        fn commit(&self, agent: &str, text: &str) {
            std::fs::write(
                self.0.join(format!(".mdkg/agents/{agent}.json")),
                Self::definition(text),
            )
            .expect("definition");
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", text]);
        }

        fn authority(&self) -> mdkg_graph::graphref::Authority {
            crate::orchestration::authority(&self.0).expect("authority")
        }

        fn definitions(
            &self,
            agents: &[&str],
            authority: &mdkg_graph::graphref::Authority,
        ) -> Definitions {
            let agents: Vec<String> = agents.iter().map(|a| a.to_string()).collect();
            Definitions::new(&self.0, &self.0, "", &agents, authority)
        }
    }

    #[test]
    fn an_edit_committed_after_the_pinned_read_reloads() {
        let repo = Repo::new();
        repo.commit("alice", "first");
        let pinned = repo.authority();
        // Committed after the read the configuration was built from.
        repo.commit("alice", "second");
        let definitions = repo.definitions(&["alice"], &pinned);
        assert_eq!(
            definitions.changed().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
        let definitions = repo.definitions(&["alice"], &repo.authority());
        assert_eq!(definitions.changed(), None);
        // The committed config counts too.
        std::fs::write(repo.0.join(".mdkg/config.json"), "{\"projections\":[]}").unwrap();
        repo.git(&["commit", "-qam", "config"]);
        assert_eq!(definitions.changed().as_deref(), Some(".mdkg/config.json"));
    }

    #[test]
    fn a_name_with_a_colon_is_watched_at_its_own_path() {
        let repo = Repo::new();
        repo.commit("codex:pr19", "first");
        let first = git::commit_of(&repo.0, "HEAD").expect("head");
        let pinned = repo.authority();
        assert_eq!(
            definition_commit(&repo.0, "", "codex:pr19", &pinned.commit).as_deref(),
            Some(first.as_str())
        );
        let definitions = repo.definitions(&["codex:pr19"], &pinned);
        assert_eq!(definitions.changed(), None);
        repo.commit("codex:pr19", "second");
        assert_eq!(
            definitions.changed().as_deref(),
            Some(".mdkg/agents/codex:pr19.json")
        );
        // The prompt still names the commit the pinned read came from.
        assert_eq!(
            definition_commit(&repo.0, "", "codex:pr19", &pinned.commit).as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn an_uncommitted_definition_edit_is_named_and_never_reloads() {
        let repo = Repo::new();
        repo.commit("alice", "first");
        let definitions = repo.definitions(&["alice"], &repo.authority());
        assert_eq!(definitions.differs(), None);
        let file = repo.0.join(".mdkg/agents/alice.json");
        std::fs::write(&file, "edited").expect("edit");
        assert_eq!(
            definitions.differs().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
        // Only committed authority reloads.
        assert_eq!(definitions.changed(), None);
        std::fs::remove_file(&file).expect("remove");
        assert_eq!(
            definitions.differs().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
        std::fs::write(&file, Repo::definition("first")).expect("restore");
        assert_eq!(definitions.differs(), None);
        std::fs::write(repo.0.join(".mdkg/config.json"), "{\"x\":1}").unwrap();
        assert_eq!(definitions.differs().as_deref(), Some(".mdkg/config.json"));
    }

    /// Ref mode: the graph ref is the authority, so a definition committed
    /// on the worktree's branch but never published still differs (review
    /// #11614); and a poll reads no further while the ref's tip is the
    /// pinned commit.
    #[test]
    fn in_ref_mode_a_branch_commit_differs_and_only_the_ref_reloads() {
        let repo = Repo::new();
        let id = "0192f3a0-0000-7000-8000-000000000001";
        std::fs::write(
            repo.0.join(".mdkg/config.json"),
            serde_json::json!({"schema_version": 3, "format_version": "0.7", "graph_id": id})
                .to_string(),
        )
        .unwrap();
        repo.commit("alice", "first");
        repo.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        repo.git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        repo.git(&[
            "config",
            &format!("mdkg.graph.{id}.ref"),
            "refs/heads/mdkg-graph",
        ]);
        let pinned = repo.authority();
        assert_eq!(pinned.graph_ref.as_deref(), Some("refs/heads/mdkg-graph"));
        let definitions = repo.definitions(&["alice"], &pinned);
        assert!(
            definitions.source().starts_with("graph:"),
            "{}",
            definitions.source()
        );
        assert_eq!(definitions.differs(), None);
        // Committed on the branch, not published: HEAD and the worktree
        // agree, the authority doesn't.
        repo.commit("alice", "second");
        assert_eq!(
            definitions.differs().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
        assert_eq!(definitions.changed(), None);
        // A checkout that carries the graph warns on a deletion, whether
        // it's committed or not (review #12257)...
        let file = repo.0.join(".mdkg/agents/alice.json");
        let kept = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        let alice = Some(".mdkg/agents/alice.json");
        assert_eq!(definitions.differs().as_deref(), alice);
        repo.git(&["commit", "-qam", "drop alice"]);
        assert_eq!(definitions.differs().as_deref(), alice);
        repo.git(&["revert", "--no-edit", "HEAD"]);
        assert_eq!(std::fs::read(&file).unwrap(), kept);
        // ...but a code branch that doesn't carry it isn't an edit (review
        // #12068): a worktree of a commit with no graph at all.
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let out = std::process::Command::new("git")
            .current_dir(&repo.0)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["commit-tree", empty_tree, "-m", "code"])
            .output()
            .expect("commit-tree");
        let code = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let checkout = repo.0.join(".worktrees/code");
        repo.git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            checkout.to_str().unwrap(),
            &code,
        ]);
        let elsewhere = Definitions::new(
            &checkout,
            &repo.0,
            "",
            &["alice".to_string()],
            &repo.authority(),
        );
        assert!(elsewhere.source().starts_with("graph:"));
        assert_eq!(elsewhere.differs(), None);
        repo.git(&["worktree", "remove", "--force", checkout.to_str().unwrap()]);
        // Published: the tip moved, so it reloads.
        repo.git(&["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
        assert_eq!(
            definitions.changed().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
        // The repository now names another ref, holding another definition.
        let definitions = repo.definitions(&["alice"], &repo.authority());
        repo.commit("alice", "third");
        repo.git(&["update-ref", "refs/heads/other", "HEAD"]);
        repo.git(&["config", "mdkg.graph.ref", "refs/heads/other"]);
        repo.git(&[
            "config",
            &format!("mdkg.graph.{id}.ref"),
            "refs/heads/other",
        ]);
        // The pinned ref's tip hasn't moved, so a poll reads no further...
        assert_eq!(definitions.changed(), None);
        // ...until a full read is due, once a minute.
        let due = Instant::now()
            .checked_sub(FULL_READ + Duration::from_secs(1))
            .unwrap();
        *definitions.read_at.lock().unwrap() = due;
        assert_eq!(
            definitions.changed().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
    }

    /// In checkout mode every poll is a full read, and each starts with
    /// fresh reads of the repository.
    #[test]
    fn each_full_read_reads_fresh() {
        let repo = Repo::new();
        repo.commit("alice", "first");
        let definitions = repo.definitions(&["alice"], &repo.authority());
        let before = mdkg_git::fresh_reads_count();
        for _ in 0..2 {
            assert_eq!(definitions.changed(), None);
        }
        assert!(mdkg_git::fresh_reads_count() >= before + 2);
    }

    /// A linked worktree replaced at the same path between two polls (moved
    /// away, and another branch added where it was) is read as it is now,
    /// not through the check the first poll kept.
    #[test]
    fn a_worktree_replaced_at_its_path_is_read_anew() {
        let repo = Repo::new();
        repo.commit("alice", "first");
        let live = repo.0.join("wt/live");
        let moved = repo.0.join("wt/moved");
        let live_path = live.to_str().unwrap();
        repo.git(&["worktree", "add", "-q", "-b", "a", live_path, "HEAD"]);
        let pinned = crate::orchestration::authority(&live).expect("authority");
        let definitions = Definitions::new(&live, &live, "", &["alice".to_string()], &pinned);
        assert_eq!(definitions.changed(), None);
        repo.git(&["worktree", "move", live_path, moved.to_str().unwrap()]);
        repo.commit("alice", "second");
        repo.git(&["worktree", "add", "-q", "-b", "b", live_path, "HEAD"]);
        assert_eq!(
            definitions.changed().as_deref(),
            Some(".mdkg/agents/alice.json")
        );
    }

    #[test]
    fn an_authority_that_cant_be_read_changes_nothing() {
        let repo = Repo::new();
        repo.commit("alice", "first");
        let definitions = repo.definitions(&["alice"], &repo.authority());
        std::fs::write(repo.0.join(".mdkg/config.json"), "{not JSON").unwrap();
        repo.git(&["commit", "-qam", "broken config"]);
        assert_eq!(definitions.changed(), None);
    }
}
