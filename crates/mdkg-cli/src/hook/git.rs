//! The git adapter: git's own `pre-commit`, `pre-merge-commit`,
//! `pre-push` and `reference-transaction` hooks, which `hooks:git`
//! installs, call `mdkg hooks run --harness git --native <hook> [-- <git's
//! arguments>]`. It sees every commit, merge, push and ref update, however
//! it was made: through an agent's shell, a script, or a person.
//!
//! The actor is an agent when any harness's marker is in the environment
//! ([`event::AGENT_MARKERS`]), else a human. Rules: `git.protect` (pushes),
//! `lease.staged` and `git.graph_path` (commits; merges too for the
//! latter), `secret.scan` (commits and pushes), and `git.graph_ref` (ref
//! updates).
//! The hooks guard against mistakes, not evasion: a shell can run git with
//! hooks off, as mdkg's own plumbing does, or write a ref file directly.
//!
//! A refusal prints its reason on stderr and exits 1, which stops git; a
//! warning prints and exits 0. Outside a graph, or on an internal error,
//! it allows.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mdkg_format::program::BIN;

use super::event::{
    self, Actor, ActorKind, Decision, EventName, GitCall, GitVerb, HarnessName, HookEvent, Native,
    Verdict,
};
use super::rules::{self, Rule, Said};
use super::{Holds, Lease, live};
use crate::coord::Repo;

/// The git hooks mdkg routes.
pub fn native(name: &str) -> Option<EventName> {
    match name {
        "pre-commit" => Some(EventName::GitPreCommit),
        "pre-push" => Some(EventName::GitPrePush),
        "pre-merge-commit" => Some(EventName::GitPreMergeCommit),
        "reference-transaction" => Some(EventName::GitReferenceTransaction),
        _ => None,
    }
}

/// One line of pre-push's input: what a push would update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub local_ref: String,
    pub local_sha: String,
    pub remote_ref: String,
    pub remote_sha: String,
}

/// The most of pre-push's input read: a line per ref pushed.
const INPUT_MAX: u64 = 1 << 20;

/// pre-push's input lines: `<local ref> <local sha> <remote ref> <remote
/// sha>`; anything else is skipped.
pub fn updates(input: &str) -> Vec<Update> {
    input
        .lines()
        .filter_map(|line| {
            let mut w = line.split_whitespace();
            let u = Update {
                local_ref: w.next()?.into(),
                local_sha: w.next()?.into(),
                remote_ref: w.next()?.into(),
                remote_sha: w.next()?.into(),
            };
            w.next().is_none().then_some(u)
        })
        .collect()
}

fn zero(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|b| b == b'0')
}

/// A push as a canonical git call. An update forces when it deletes a
/// ref, when it moves a tag the remote has (git needs `--force` for that,
/// even to a descendant), or when the remote's commit isn't an ancestor
/// of what replaces it; `ancestor` answers that, `None` when it can't
/// tell (the remote's commit isn't here), which counts as forcing.
pub fn push_call(
    remote: Option<&str>,
    updates: &[Update],
    ancestor: impl Fn(&str, &str) -> Option<bool>,
) -> GitCall {
    let mut call = GitCall {
        verb: GitVerb::Push,
        remote: remote.map(String::from),
        ..GitCall::default()
    };
    for u in updates {
        let branch = u.remote_ref.strip_prefix("refs/heads/");
        if let Some(b) = branch
            && !call.branches.iter().any(|x| x == b)
        {
            call.branches.push(b.to_string());
        }
        let forced = if zero(&u.local_sha) {
            true
        } else if zero(&u.remote_sha) {
            false
        } else if u.remote_ref.starts_with("refs/tags/") {
            u.remote_sha != u.local_sha
        } else {
            ancestor(&u.remote_sha, &u.local_sha) != Some(true)
        };
        call.force |= forced;
    }
    call
}

/// The protected branches (R33): `main`, `master`, `mainRef`'s short
/// name, the graph ref's branch and `graph.remoteRef`'s, each from its
/// config value (never `$MDKG_MAIN_REF`: an agent's shell sets that), the
/// actor's committed `protect` list (from the graph authority, never the
/// checkout's definition file, #11129), and GitHub's (the cache a-hooks-sync
/// writes, `<git common dir>/mdkg/hooks/github-protection.json`, read if
/// it's there: `{"branches": [...]}`). A hook never calls the network. When
/// the authority or the graph-ref settings can't be read, every branch is
/// protected (`**`, which matches across `/`, so `release/x` too): the push
/// is refused. A rule whose `match.branch` replaces the protected set
/// without naming `protected` never used the actor's list, so it keeps
/// only its own globs.
pub fn protected(root: &Path, actor: &str, identity: &Pinned) -> Vec<String> {
    use mdkg_run::profile::{ProtectedRefs, protected_refs};
    let mut names: Vec<String> = Vec::new();
    match committed_protect(identity, actor) {
        Ok(list) => names.extend(list),
        Err(()) => names.push("**".into()),
    }
    let common = super::common_dir(root);
    if let Some(common) = &common {
        names.extend(github_protected(common));
    }
    match mdkg_graph::graphref::resolve(root) {
        Ok(Some(graph)) => protected_refs(
            &ProtectedRefs {
                main_ref: graph.main_ref.as_deref(),
                graph_ref: Some(&graph.name),
                remote_ref: graph.remote.as_ref().map(|r| r.remote_ref.as_str()),
            },
            &names,
        ),
        // Checkout mode: `mdkg.mainRef` as the common config file holds it.
        // A read that fails (git won't run, or exits other than "not
        // set") protects every branch, as broken settings do (review of
        // 570f01e1).
        Ok(None) => {
            let main = match common.as_deref() {
                Some(c) => common_value(c, "mdkg.mainRef"),
                None => Err(()),
            };
            if main.is_err() {
                names.push("**".into());
            }
            let main = main.ok().flatten();
            protected_refs(
                &ProtectedRefs {
                    main_ref: main.as_deref(),
                    ..ProtectedRefs::default()
                },
                &names,
            )
        }
        Err(_) => {
            names.push("**".into());
            protected_refs(&ProtectedRefs::default(), &names)
        }
    }
}

/// One identity for the whole hook invocation (#6945): the actor and the
/// graph authority captured once, before any rule reads either.
pub type Pinned = Result<mdkg_run::identity::IdentitySnapshot, mdkg_run::RunError>;

/// Captures the [`Pinned`] identity for `cwd` in the repository at `root`.
/// A checkout whose HEAD commits no graph, in a clone whose main ref does
/// (an orphan or pre-graph branch), reads the main ref's graph as git
/// objects (orchestrator #11871); one that can't be read is an error.
pub fn pin(root: &Path, cwd: &Path) -> Pinned {
    let snapshot = mdkg_run::identity::capture_for_cwd(root, cwd)?;
    if snapshot.authority.is_some() {
        return Ok(snapshot);
    }
    match main_ref_graph(root) {
        Ok(Some(authority)) => mdkg_run::identity::capture_with(root, cwd, authority),
        Ok(None) => Ok(snapshot),
        Err(why) => Err(mdkg_run::RunError::failure("graph.authority_invalid", why)),
    }
}

/// The graph the clone's main ref commits, for a checkout-mode worktree
/// whose own HEAD commits none: `mdkg.mainRef` from the common config,
/// else `main`, tried as `refs/heads/<name>` and then `refs/remotes/<name>`
/// (so the default `main` never reaches `origin/main`; set `mdkg.mainRef
/// origin/main` for a clone with only that). `Ok(None)` when no main ref
/// exists or it commits no `.mdkg/config.json`: the clone has no graph.
/// `Err` names what can't be read, which refuses.
pub fn main_ref_graph(top: &Path) -> Result<Option<mdkg_graph::graphref::Authority>, String> {
    let common = mdkg_coord_sqlite::git::common_dir(top)
        .map_err(|_| "the repository's git folder can't be found".to_string())?;
    let name = common_value(&common, "mdkg.mainRef")
        .map_err(|()| "mdkg.mainRef can't be read".to_string())?
        .unwrap_or_else(|| "main".into());
    let candidates = if name.starts_with("refs/") {
        vec![name]
    } else {
        vec![format!("refs/heads/{name}"), format!("refs/remotes/{name}")]
    };
    // A ref git says is missing (`show-ref --verify` exits 1) is no main
    // ref; any other failure, or one that doesn't name a readable commit,
    // can't be read and refuses (review of 6670d321).
    let mut commit = None;
    for r in &candidates {
        let shown = Command::new("git")
            .arg("-C")
            .arg(top)
            .args(["show-ref", "--verify", "--quiet", r.as_str()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| "git can't run".to_string())?;
        match shown.code() {
            Some(0) => {
                let c = git_out(
                    top,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{r}^{{commit}}"),
                    ],
                )
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .ok_or_else(|| "the main ref doesn't name a readable commit".to_string())?;
                commit = Some(c);
                break;
            }
            // git says missing; a loose ref file that's there anyway is a
            // corrupt ref, which can't be read (review of 25daabd3).
            Some(1) if common.join(r).is_file() => {
                return Err("the main ref is there but can't be read".into());
            }
            Some(1) => continue,
            _ => return Err("the main ref can't be read".into()),
        }
    }
    let Some(commit) = commit else {
        return Ok(None);
    };
    let listed = git_out(
        top,
        &["ls-tree", "--name-only", &commit, "--", ".mdkg/config.json"],
    )
    .ok_or_else(|| "the main ref's tree can't be listed".to_string())?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    mdkg_graph::graphref::authority_at(top, &commit)
        .map(Some)
        .map_err(|e| format!("the main ref's graph ({})", e.code))
}

/// A [`Pinned`] identity captured at its first use and kept for the rest
/// of the invocation, so a hook no rule needs it for reads no graph.
pub struct LazyPin<'a> {
    root: &'a Path,
    cwd: &'a Path,
    cell: std::cell::OnceCell<Pinned>,
}

impl<'a> LazyPin<'a> {
    pub fn new(root: &'a Path, cwd: &'a Path) -> Self {
        Self {
            root,
            cwd,
            cell: std::cell::OnceCell::new(),
        }
    }

    pub fn get(&self) -> &Pinned {
        self.cell.get_or_init(|| pin(self.root, self.cwd))
    }
}

/// The actor's committed `protect` list, read from the pinned authority,
/// never a later graph tip (as `agent::committed_protect` reads it). No
/// authority (a positively absent graph) protects nothing more; an
/// authority that couldn't be read, or a list that isn't one, is `Err`.
fn committed_protect(identity: &Pinned, actor: &str) -> Result<Vec<String>, ()> {
    let snapshot = identity.as_ref().map_err(|_| ())?;
    let Some(authority) = &snapshot.authority else {
        return Ok(Vec::new());
    };
    let config = mdkg_run::orchestration::agent::authority_config(authority).map_err(|_| ())?;
    match config
        .get("agents")
        .and_then(|agents| agents.get(actor))
        .and_then(|agent| agent.get("protect"))
    {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(names)) => names
            .iter()
            .map(|n| n.as_str().map(String::from).ok_or(()))
            .collect(),
        Some(_) => Err(()),
    }
}

/// A value of the repository's own config file only (`<common>/config`),
/// never a worktree's, the environment's or the user's: `Ok(None)` when
/// it isn't set (git's exit 1), `Err` when it can't be read.
fn common_value(common: &Path, key: &str) -> Result<Option<String>, ()> {
    let file = common.join("config");
    let out = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(&file)
        .args(["--get", key])
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| ())?;
    match out.status.code() {
        Some(0) => {
            let v = String::from_utf8(out.stdout).map_err(|_| ())?;
            let v = v.trim();
            Ok((!v.is_empty()).then(|| v.to_string()))
        }
        Some(1) => Ok(None),
        _ => Err(()),
    }
}

/// The GitHub protection cache's branch names, or none.
pub(crate) fn github_protected(common: &Path) -> Vec<String> {
    let file = mdkg_run::hooks::dir(common).join("github-protection.json");
    let Ok(bytes) = mdkg_coord_sqlite::git::read_capped(&file, INPUT_MAX) else {
        return Vec::new();
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("branches")?.as_array().map(|a| {
                a.iter()
                    .filter_map(|b| b.as_str())
                    .filter(|b| !b.is_empty())
                    .map(String::from)
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// `git.protect`: refuses a push to a protected branch, or any force push.
/// `heads` are where a push naming `HEAD` or no branch can go: the branch
/// checked out, and its push target (`@{push}`) when it has one.
/// The branches it guards are the rule's `match.branch` (globs, with
/// `protected` for the protected set), else the protected set. Names
/// compare without case: a case-folding disk makes `Main` `main`.
pub fn protect(
    rule: &Rule,
    git: &GitCall,
    heads: &[&str],
    protected: &[String],
    unmapped: Option<String>,
) -> Option<(Verdict, String, &'static str)> {
    if git.verb != GitVerb::Push {
        return None;
    }
    if git.skips_hooks {
        return Some((
            Verdict::Deny,
            "mdkg refuses an agent's push that skips git's hooks (`--no-verify`, or a \
             `core.hooksPath` set for the command). Push without it."
                .into(),
            "hook.git_protect.skips_hooks",
        ));
    }
    if git.force {
        return Some((
            Verdict::Deny,
            "mdkg refuses an agent's force push (`--force`, `+<ref>`, a deletion, or a \
             commit the remote has that this push would drop). Push new commits on \
             top instead; if the remote moved, fetch and rebase or merge first."
                .into(),
            "hook.git_protect.force",
        ));
    }
    let guarded: Vec<String> = match &rule.matches.branch {
        None => protected.to_vec(),
        Some(globs) => globs
            .iter()
            .flat_map(|g| {
                if g == "protected" {
                    protected.to_vec()
                } else {
                    vec![g.clone()]
                }
            })
            .collect(),
    };
    let matches = |b: &str| {
        let b = b.to_lowercase();
        guarded
            .iter()
            .find(|g| rules::glob_whole(&g.to_lowercase(), &b))
            .cloned()
    };
    let targets: Vec<&str> = if git.branches.is_empty() {
        heads.to_vec()
    } else {
        git.branches
            .iter()
            .flat_map(|b| {
                if b == "HEAD" || b == "@" {
                    heads.to_vec()
                } else {
                    vec![b.as_str()]
                }
            })
            .filter(|b| !b.is_empty())
            .collect()
    };
    let explicit = "Push an explicit refspec to a named branch, one remote per command: \
                    `git push <remote> <branch>:<branch>`.";
    if git.all {
        guarded.first()?;
        return Some((
            Verdict::Deny,
            format!(
                "mdkg refuses an agent's push it can't read branch by branch (`--all`, \
                 `--mirror`, a refspec pattern, a branch or remote the shell expands, or \
                 pushes to several remotes in one command): it could reach a protected \
                 branch. {explicit}"
            ),
            "hook.git_protect.denied",
        ));
    }
    if let Some(why) = unmapped {
        guarded.first()?;
        return Some((
            Verdict::Deny,
            format!(
                "mdkg can't tell which branch this push updates ({why}), so it refuses it for \
                 an agent. {explicit}"
            ),
            "hook.git_protect.denied",
        ));
    }
    let guard = targets.iter().find_map(|b| matches(b))?;
    Some((
        Verdict::Deny,
        format!(
            "mdkg refuses an agent's push to the protected branch {}. Push a feature \
             branch and open a pull request, or ask a person to push it.",
            super::printable(&guard)
        ),
        "hook.git_protect.denied",
    ))
}

/// `lease.staged`: the edit guard on a commit's staged paths, which also
/// catches what a shell wrote. An agent's commit is refused; a person's
/// gets a warning. Paths are never echoed.
pub fn staged(
    kind: ActorKind,
    paths: &[String],
    holds: &dyn Holds,
) -> Option<(Verdict, String, &'static str)> {
    let mut unheld = 0usize;
    let mut unknown = false;
    for p in paths {
        match holds.lease(p) {
            Ok(Lease::External | Lease::Held) => {}
            Ok(Lease::Unheld(_) | Lease::GitAdmin) => unheld += 1,
            Err(_) => unknown = true,
        }
    }
    if unheld == 0 && !unknown {
        return None;
    }
    let what = if unheld == 0 {
        "mdkg couldn't check your leases on this commit's staged files".to_string()
    } else {
        let noun = if unheld == 1 { "file" } else { "files" };
        format!("mdkg shows no lease of yours on {unheld} staged {noun}")
    };
    Some(match kind {
        ActorKind::Human => (
            Verdict::Context,
            format!("{what}. (A warning: people's commits aren't refused.)"),
            "hook.lease_staged.warned",
        ),
        _ => (
            Verdict::Deny,
            format!(
                "{what}. Run `git diff --cached --name-only` to inspect them, claim each \
                 (`{BIN} claim <path>`, `--new` for a new file) or unstage it, then commit again."
            ),
            "hook.lease_staged.denied",
        ),
    })
}

/// Where a path a commit changes sits, for `git.graph_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owned {
    /// In a graph-owned folder (`.mdkg/**`, or a workspace's graph folder).
    Graph,
    /// A `node edit` copy (`.mdkg-edit/**`), which is never committed.
    Edit,
}

/// The folder `node edit` writes its copies to in a worktree.
pub const EDIT_DIR: &str = ".mdkg-edit";

/// `git.graph_path` (G17, R36, C21): in ref mode the graph lives on the
/// graph ref, so an agent's commit on a code branch that changes a
/// graph-owned file is refused, and a person's gets a warning. `changed`
/// holds each such path's place and whether an exemption covers it: a
/// merge result equal to the blob of a parent that `mainRef` holds, or a
/// design copy equal to the graph tip's. `.mdkg-edit/` copies are never
/// exempt. Paths are never echoed.
pub fn graph_path(
    kind: ActorKind,
    changed: &[(Owned, bool)],
) -> Option<(Verdict, String, &'static str)> {
    let count = |o: Owned| {
        changed
            .iter()
            .filter(|(owned, exempt)| *owned == o && (o == Owned::Edit || !exempt))
            .count()
    };
    let (graph, edit) = (count(Owned::Graph), count(Owned::Edit));
    if graph == 0 && edit == 0 {
        return None;
    }
    let files = |n: usize| if n == 1 { "file" } else { "files" };
    let mut what = Vec::new();
    if graph > 0 {
        what.push(format!(
            "this commit changes {graph} graph {} on a code branch, and the graph lives \
             on the graph ref",
            files(graph)
        ));
    }
    if edit > 0 {
        what.push(format!(
            "it commits {edit} {} from `{EDIT_DIR}/`, which holds `{BIN} node edit` copies",
            files(edit)
        ));
    }
    let what = what.join("; ");
    Some(match kind {
        ActorKind::Human => (
            Verdict::Context,
            format!("mdkg: {what}. (A warning: people's commits aren't refused.)"),
            "hook.git_graph_path.warned",
        ),
        _ => (
            Verdict::Deny,
            format!(
                "mdkg refuses an agent's commit: {what}. Unstage them (`git diff --cached \
                 --name-only` lists them), then change nodes through mdkg: `{BIN} node edit \
                 <id>`, then `{BIN} node save <id>`. A merge that keeps the main branch's \
                 graph files as they are passes, and so does a design node equal to the \
                 graph's."
            ),
            "hook.git_graph_path.denied",
        ),
    })
}

/// One path part as a case-folding disk may read it: with the code points
/// HFS+ ignores dropped, as git's `is_hfs_dot_generic` does, then
/// lowercased. git-serve's receiving hook folds parts the same way, so a
/// commit it would refuse at push is refused here first.
fn fold(part: &str) -> String {
    part.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}'
            )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `path` is in one of `dirs`, part by part as [`fold`] reads them.
fn in_dirs(path: &str, dirs: &[String]) -> bool {
    let parts: Vec<String> = path.split('/').map(fold).collect();
    dirs.iter().any(|d| {
        let dir: Vec<String> = d
            .trim_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .map(fold)
            .collect();
        !dir.is_empty() && parts.starts_with(&dir)
    })
}

/// Where `path` sits, each part read as [`fold`] does: in [`EDIT_DIR`] at
/// the worktree's top, or, in ref mode (graph-owned `dirs` given), in one
/// of them or under a `.mdkg` part anywhere, as git-serve judges a push.
/// With no `dirs` (checkout mode) only edit copies count.
pub fn owned(path: &str, dirs: &[String]) -> Option<Owned> {
    let parts: Vec<String> = path.split('/').map(fold).collect();
    if parts.first().is_some_and(|p| p == EDIT_DIR) {
        Some(Owned::Edit)
    } else if !dirs.is_empty() && (parts.iter().any(|p| p == ".mdkg") || in_dirs(path, dirs)) {
        Some(Owned::Graph)
    } else {
        None
    }
}

/// What decides `git.graph_path`'s exemptions for one commit.
pub struct GraphFacts<'a> {
    /// The graph-owned folders (`graphref::graph_paths`).
    pub dirs: &'a [String],
    /// Their design folders (`graphref::design_paths`).
    pub design: &'a [String],
    /// The graph tip, whose design blobs a design copy may equal.
    pub tip: Option<&'a str>,
    /// The merge's parents other than `HEAD` that `mainRef` holds.
    pub main_parents: &'a [String],
}

/// The graph-owned paths the commit being made at `top` changes, each
/// with whether an exemption covers it ([`graph_path`]); `None` when git
/// can't say, which the caller refuses.
pub fn graph_changes(top: &Path, facts: &GraphFacts<'_>) -> Option<Vec<(Owned, bool)>> {
    let changed: Vec<(String, Owned)> = staged_paths(top)?
        .into_iter()
        .filter_map(|p| owned(&p, facts.dirs).map(|o| (p, o)))
        .collect();
    let graph: Vec<&str> = changed
        .iter()
        .filter(|(_, o)| *o == Owned::Graph)
        .map(|(p, _)| p.as_str())
        .collect();
    if graph.is_empty() {
        return Some(changed.into_iter().map(|(_, o)| (o, false)).collect());
    }
    let result = index_blobs(top, &graph)?;
    let mut trees = Vec::new();
    for parent in facts.main_parents {
        trees.push(tree_blobs(top, parent, &graph)?);
    }
    let design: Vec<&str> = graph
        .iter()
        .copied()
        .filter(|p| in_dirs(p, facts.design))
        .collect();
    let tip = match facts.tip {
        Some(tip) if !design.is_empty() => Some(tree_blobs(top, tip, &design)?),
        _ => None,
    };
    Some(
        changed
            .iter()
            .map(|(path, o)| {
                let blob = result.get(path);
                let exempt = *o == Owned::Graph
                    && (trees.iter().any(|t| t.get(path) == blob)
                        || tip.as_ref().is_some_and(|t| {
                            design.contains(&path.as_str()) && t.get(path) == blob
                        }));
                (*o, exempt)
            })
            .collect(),
    )
}

/// The merge's parents besides `HEAD`: `MERGE_HEAD` (a `git commit`
/// that concludes a merge) and git's `GITHEAD_<sha>` variables (set for
/// `pre-merge-commit`, before `MERGE_HEAD` is written). An agent could
/// set one, but a parent only exempts a blob equal to what `mainRef`
/// already holds.
pub fn merge_parents(top: &Path, var: impl Fn() -> Vec<(String, String)>) -> Vec<String> {
    let mut parents: Vec<String> = git_out(top, &["rev-parse", "--git-path", "MERGE_HEAD"])
        .map(|p| top.join(p.trim()))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    for (key, _) in var() {
        if let Some(sha) = key.strip_prefix("GITHEAD_") {
            parents.push(sha.to_string());
        }
    }
    parents.retain(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_hexdigit()));
    parents.sort();
    parents.dedup();
    parents
}

/// Of `parents`, those `main_ref` holds (it, or an ancestor of it).
pub fn held_by(top: &Path, main_ref: &str, parents: &[String]) -> Vec<String> {
    parents
        .iter()
        .filter(|p| {
            Command::new("git")
                .arg("-C")
                .arg(top)
                .args(["merge-base", "--is-ancestor", p.as_str(), main_ref])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .cloned()
        .collect()
}

/// `paths`' blobs in the index git commits from (`GIT_INDEX_FILE` for a
/// partial commit); a path missing from the map is deleted.
fn index_blobs(top: &Path, paths: &[&str]) -> Option<std::collections::BTreeMap<String, String>> {
    let mut args = vec!["ls-files", "--stage", "-z", "--"];
    let literal: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(literal.iter().map(String::as_str));
    let out = git(top, &args)?;
    Some(
        out.split(|b| *b == 0)
            .filter_map(|entry| {
                let entry = std::str::from_utf8(entry).ok()?;
                let (meta, path) = entry.split_once('\t')?;
                let mut w = meta.split(' ');
                let (_, oid) = (w.next()?, w.next()?);
                Some((path.to_string(), oid.to_string()))
            })
            .collect(),
    )
}

/// `paths`' blobs in `commit`'s tree; a path missing from the map isn't
/// there.
fn tree_blobs(
    top: &Path,
    commit: &str,
    paths: &[&str],
) -> Option<std::collections::BTreeMap<String, String>> {
    let tree = format!("{commit}^{{tree}}");
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", tree.as_str(), "--"];
    let literal: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(literal.iter().map(String::as_str));
    let out = git(top, &args)?;
    Some(
        out.split(|b| *b == 0)
            .filter_map(|entry| {
                let entry = std::str::from_utf8(entry).ok()?;
                let (meta, path) = entry.split_once('\t')?;
                let mut w = meta.split(' ');
                let (_, kind, oid) = (w.next()?, w.next()?, w.next()?);
                (kind == "blob").then(|| (path.to_string(), oid.to_string()))
            })
            .collect(),
    )
}

/// The reference-transaction guard (`git.graph_ref`, C20): a transaction
/// git has `prepared` that updates the graph ref is an agent's direct
/// update, refused, and a warning for a person. mdkg's own plumbing runs
/// git with hooks off, so its writes never reach this. `input` is the
/// hook's stdin, a `<old> <new> <ref>` line per update; the other states
/// (`committed`, `aborted`) can't stop anything and say nothing. Names
/// compare without case: a case-folding disk stores `refs/heads/MDKG-GRAPH`
/// as the graph ref's own file.
pub fn graph_ref_update(
    kind: ActorKind,
    state: &str,
    graph_ref: &str,
    input: impl IntoIterator<Item = String>,
) -> Option<(Verdict, String, &'static str)> {
    if state != "prepared" {
        return None;
    }
    let graph = graph_ref.to_lowercase();
    let hit = input.into_iter().any(|line| {
        line.split(' ')
            .nth(2)
            .is_some_and(|r| r.trim_end().to_lowercase() == graph)
    });
    if !hit {
        return None;
    }
    let name = super::printable(graph_ref);
    Some(match kind {
        ActorKind::Human => (
            Verdict::Context,
            format!(
                "mdkg: this updates the graph ref {name} directly. mdkg reads it as the \
                 graph's authority, and its next write or sync sees this as external \
                 content. (A warning: people's updates aren't refused.)"
            ),
            "hook.git_graph_ref.warned",
        ),
        _ => (
            Verdict::Deny,
            format!(
                "mdkg refuses an agent's direct update of the graph ref {name}. Change the \
                 graph through mdkg (`{BIN} node edit <id>` and `{BIN} node save <id>`, or \
                 the MCP tools), or ask a person."
            ),
            "hook.git_graph_ref.denied",
        ),
    })
}

/// A git command at `top`, inheriting git's hook environment (its index,
/// for a partial commit); its stdout, or `None` when it fails.
fn git(top: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// The paths a commit stages, relative to the worktree's top.
fn staged_paths(top: &Path) -> Option<Vec<String>> {
    let out = git(
        top,
        &["diff", "--cached", "--name-only", "-z", "--no-renames"],
    )?;
    Some(
        out.split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect(),
    )
}

fn git_out(top: &Path, args: &[&str]) -> Option<String> {
    git(top, args).map(|o| String::from_utf8_lossy(&o).into_owned())
}

fn current_branch(top: &Path) -> Option<String> {
    let out = git(top, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let b = String::from_utf8(out).ok()?.trim().to_string();
    (!b.is_empty()).then_some(b)
}

/// What a git hook prints and exits with, for a decision.
pub fn render(d: &Decision) -> (String, u8) {
    let text = d.text.clone().unwrap_or_default();
    match d.decision {
        Verdict::Deny => (format!("{text}\n"), 1),
        Verdict::Context | Verdict::BlockStop | Verdict::Wake => (format!("{text}\n"), 0),
        Verdict::Allow => (String::new(), 0),
    }
}

/// Runs a git hook through the rules; its exit code.
pub fn run(root: Option<&Path>, actor: Option<String>, name: &str, args: &[String]) -> u8 {
    let Some(event_name) = native(name) else {
        eprintln!(
            "mdkg routes no git hook named {}; nothing ran",
            super::printable(name)
        );
        return 0;
    };
    if event_name == EventName::GitReferenceTransaction {
        return transaction(actor, args.first().map_or("", String::as_str));
    }
    // pre-push's input is read whole first, so git never blocks writing it.
    let mut input = String::new();
    let mut whole = true;
    if event_name == EventName::GitPrePush {
        let read = std::io::stdin()
            .take(INPUT_MAX + 1)
            .read_to_string(&mut input);
        whole = read.is_ok() && input.len() as u64 <= INPUT_MAX;
    }
    let cwd = std::env::current_dir().ok();
    let env = |k: &str| std::env::var(k).ok();
    // The worktree git runs the hook in, pinned before any graph lookup:
    // a graph found above it (another worktree's) or none at all can't
    // say who runs it.
    let Some(git_top) = cwd
        .as_deref()
        .and_then(|c| mdkg_coord_sqlite::git::toplevel(c).ok())
    else {
        // No worktree (a bare repository): only a live turn among the
        // hook's ancestors can say an agent runs it, and then no rule can
        // be read, so it's refused.
        let common = cwd.as_deref().and_then(|c| {
            git(
                c,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
        });
        if common.is_some_and(|c| ancestor_turn(&c).is_some()) {
            eprintln!(
                "mdkg refuses an agent's {name} from a repository with no worktree: no \
                 rule can be read there. Push from a worktree of it."
            );
            return 1;
        }
        return 0;
    };
    let start = super::Input {
        cwd,
        ..super::Input::default()
    };
    // The graph: this worktree's own `.mdkg`, or in ref mode the graph ref
    // with the worktree as its root (`graph_root`'s graphless binding). In
    // checkout mode a positively identified absent graph (an unborn HEAD,
    // or no committed config) lets the hook through with only the rules
    // that need no graph; anything broken fails closed (G24, R49,
    // dec-01a188da).
    let graph = super::graph_root(root, &start)
        .and_then(|r| Repo::open(&r).ok().map(|repo| (r, repo)))
        .filter(|(_, repo)| same_folder(&repo.top, &git_top));
    let (root, repo, has_graph) = match graph {
        Some((root, repo)) => (root, repo, true),
        None => {
            // HEAD commits no graph: absent only when the clone has none,
            // so an orphan or pre-graph branch reads the main ref's graph
            // (orchestrator #11871).
            let absent = match mdkg_graph::graphref::lookup_in(&git_top, "") {
                Ok(mdkg_graph::graphref::Lookup::Absent(_)) => match main_ref_graph(&git_top) {
                    Ok(main) => Ok(main.is_none()),
                    Err(why) => Err(why),
                },
                Ok(mdkg_graph::graphref::Lookup::Found(_)) => Ok(false),
                Err(e) => Err(e.code.to_string()),
            };
            let repo = Repo::open(&git_top);
            match (absent, repo) {
                (Ok(absent), Ok(repo)) => (git_top.clone(), repo, !absent),
                (Err(code), _) => {
                    eprintln!(
                        "mdkg can't read this clone's graph ({}), so it can't tell who runs \
                         this {name} or check its rules, and refuses it. Run `{BIN} doctor`, and \
                         `{BIN} install` to repair the graph settings; a person can run git \
                         with --no-verify.",
                        super::printable(&code)
                    );
                    return 1;
                }
                (Ok(_), Err(_)) => {
                    eprintln!(
                        "mdkg can't open this repository, so it can't check this {name}'s \
                         rules, and refuses it. A person can run git with --no-verify."
                    );
                    return 1;
                }
            }
        }
    };
    let top = repo.top.clone();
    let identity = pin(&root, &top);
    let (kind, actor) = match worktree_actor(&identity, &top, repo.actor(actor), env) {
        Runner::Known(kind, actor) => (kind, actor),
        Runner::Unresolved(why) => {
            eprintln!(
                "mdkg can't tell which agent this worktree belongs to ({}), so it refuses \
                 the {name} rather than check another actor's rules. Run `{BIN} doctor`; \
                 `{BIN} install` repairs broken graph settings. Or ask a person.",
                super::printable(&why)
            );
            return 1;
        }
    };
    let pushed = updates(&input);
    let git_call = match event_name {
        EventName::GitPrePush => {
            push_call(args.first().map(String::as_str), &pushed, |old, new| {
                let out = Command::new("git")
                    .arg("-C")
                    .arg(&top)
                    .args(["merge-base", "--is-ancestor", old, new])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .ok()?;
                match out.code() {
                    Some(0) => Some(true),
                    Some(1) => Some(false),
                    _ => None,
                }
            })
        }
        _ => GitCall {
            verb: GitVerb::Commit,
            ..GitCall::default()
        },
    };
    // Input past the cap may hold any update: read it as every branch.
    let mut git_call = git_call;
    git_call.all |= !whole;
    let e = HookEvent {
        schema: event::EVENT_SCHEMA.into(),
        event: event_name,
        harness: HarnessName::Git,
        session: None,
        turn: None,
        cwd: Some(top.clone()),
        actor: Actor {
            id: Some(actor.clone()),
            kind,
        },
        tool: None,
        stop_active: false,
        native: Native { event: name.into() },
    };
    let live = live::Live {
        root: root.clone(),
        actor: actor.clone(),
        cwd: Some(top.clone()),
    };
    let ctx = Git {
        root: &root,
        top: &top,
        actor: &actor,
        call: &git_call,
        live: &live,
        remote: args.first().map(String::as_str),
        updates: whole.then_some(pushed.as_slice()),
        identity: &identity,
        has_graph,
    };
    let d = evaluate(&rules::defaults(), &e, &ctx);
    let (text, code) = render(&d);
    eprint!("{text}");
    code
}

/// The reference-transaction hook, on a path of its own: git runs it for
/// every ref update (each commit, checkout and fetch), so it reads nothing
/// but git's input and the graph-ref settings until a transaction git has
/// `prepared` in ref mode names the graph ref; then `git.graph_ref`
/// decides by who updates it. It never needs a graph checkout. Settings
/// that can't be read leave the graph ref unknown: an agent's update is
/// refused, a person's warned (R49).
fn transaction(actor: Option<String>, state: &str) -> u8 {
    use std::io::BufRead as _;
    let lines: Vec<String> = std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .collect();
    if state != "prepared" {
        return 0;
    }
    let Some(cwd) = std::env::current_dir().ok() else {
        return 0;
    };
    let graph = match mdkg_graph::graphref::resolve(&cwd) {
        Ok(None) => return 0,
        Ok(Some(graph)) => Ok(graph.name),
        Err(e) => Err(e.code),
    };
    if let Ok(name) = &graph
        && graph_ref_update(ActorKind::Agent, state, name, lines.iter().cloned()).is_none()
    {
        return 0;
    }
    let env = |k: &str| std::env::var(k).ok();
    let top = mdkg_coord_sqlite::git::toplevel(&cwd).ok();
    let fallback = actor.unwrap_or_else(|| "person".into());
    let runner = match &top {
        Some(top) => worktree_actor(&pin(top, top), top, fallback, env),
        None => match mdkg_coord_sqlite::git::common_dir(&cwd)
            .ok()
            .and_then(|c| ancestor_turn(&c))
        {
            Some(turn) => Runner::Known(ActorKind::Agent, turn),
            None => Runner::Known(event::git_actor_kind(env), fallback),
        },
    };
    let (kind, actor) = match runner {
        Runner::Known(kind, actor) => (kind, actor),
        Runner::Unresolved(why) => {
            eprintln!(
                "mdkg can't tell which agent this worktree belongs to ({}), so it refuses \
                 this update of the graph ref. Fix the agent records (`{BIN} doctor`), or \
                 ask a person.",
                super::printable(&why)
            );
            return 1;
        }
    };
    let e = HookEvent {
        schema: event::EVENT_SCHEMA.into(),
        event: EventName::GitReferenceTransaction,
        harness: HarnessName::Git,
        session: None,
        turn: None,
        cwd: top.clone().or(Some(cwd)),
        actor: Actor {
            id: Some(actor),
            kind,
        },
        tool: None,
        stop_active: false,
        native: Native {
            event: "reference-transaction".into(),
        },
    };
    let mut said = Vec::new();
    for rule in rules::defaults() {
        if rule.uses != "builtin:git.graph_ref" || !rules::applies(&rule, &e, None) {
            continue;
        }
        let got = match &graph {
            Ok(name) => graph_ref_update(kind, state, name, lines.iter().cloned()),
            Err(code) => Some(match kind {
                ActorKind::Human => (
                    Verdict::Context,
                    format!(
                        "mdkg: this clone's graph-ref settings can't be read ({code}), so it \
                         can't tell whether this updates the graph ref. Run `{BIN} doctor`. \
                         (A warning: people's updates aren't refused.)"
                    ),
                    "hook.git_graph_ref.warned",
                ),
                _ => (
                    Verdict::Deny,
                    format!(
                        "mdkg can't read this clone's graph-ref settings ({code}), so it \
                         refuses an agent's ref update that might be the graph ref's. Run \
                         `{BIN} doctor`, and `{BIN} install` to repair them, or ask a person."
                    ),
                    "hook.git_graph_ref.unreadable",
                ),
            }),
        };
        if let Some((verdict, text, code)) = got {
            said.push(Said {
                rule: rule.id.clone(),
                verdict: rules::moded(&rule, verdict),
                text,
                code: Some(code.into()),
            });
        }
    }
    let (text, code) = render(&rules::combine(said));
    eprint!("{text}");
    code
}

/// Who runs a git hook, decided by the worktree before the environment
/// (orchestrator, 2026-10-29, #5226's follow-up): a worktree that maps to
/// an agent (a lane's own, or one a launcher saved) is that agent whatever
/// the environment says, so `env -i` can't make it a person, and a person
/// working there is treated as the agent. Evidence that can't be read or
/// disagrees is [`Runner::Unresolved`], which refuses. Elsewhere, a
/// launcher's turn among the hook's ancestor processes ([`ancestor_turn`]) makes it that turn's
/// agent, wherever the push runs; then the environment's markers decide
/// ([`event::git_actor_kind`]). `actor` is the id otherwise used.
pub fn worktree_actor(
    identity: &Pinned,
    top: &Path,
    actor: String,
    var: impl Fn(&str) -> Option<String>,
) -> Runner {
    match identity.as_ref().map(|s| s.actor.clone()) {
        Ok(Some(lane)) => Runner::Known(ActorKind::Agent, lane),
        Err(e) => Runner::Unresolved(e.message.clone()),
        Ok(None) => match mdkg_coord_sqlite::git::common_dir(top)
            .ok()
            .and_then(|c| ancestor_turn(&c))
        {
            Some(turn_actor) => Runner::Known(ActorKind::Agent, turn_actor),
            None => Runner::Known(event::git_actor_kind(var), actor),
        },
    }
}

/// Who runs a git hook: an actor of a kind, or identity evidence that
/// can't be read or disagrees, which refuses the hook's operation rather
/// than check some other actor's rules.
#[derive(Debug, PartialEq, Eq)]
pub enum Runner {
    Known(ActorKind, String),
    Unresolved(String),
}

fn same_folder(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The deepest ancestors walked for a turn's mark.
const ANCESTORS_MAX: usize = 64;

/// The agent whose launcher turn one of this process's ancestors is in:
/// its environment carries the turn's mark
/// ([`mdkg_run::profile::TURN_MARK`]), which the harness process keeps
/// even when a push runs under `env -i` in another folder, and the mark is
/// a live turn of this repository ([`mdkg_run::hooks::live_turn_context`]:
/// the launcher still holds its record's lock), whose actor it is. An
/// inherited mark alone proves nothing, and another repository's turn is
/// that repository's. Read as the launcher reads it: `/proc` on Linux,
/// `ps -E` on macOS (which hides the environment of the system's own
/// programs, such as `/bin/sh`, but not a harness's). `None` when no
/// ancestor carries a live one or the chain can't be read: the worktree and
/// the environment decide.
pub fn ancestor_turn(common: &Path) -> Option<String> {
    let table = Processes::read()?;
    let mark = format!("{}=", mdkg_run::profile::TURN_MARK);
    let mut pid = table.parent(std::process::id())?;
    for _ in 0..ANCESTORS_MAX {
        if pid <= 1 {
            return None;
        }
        let turn = table
            .environment(pid)
            .and_then(|env| {
                env.iter()
                    .find_map(|v| v.strip_prefix(&mark).map(String::from))
            })
            .filter(|m| !m.is_empty())
            .and_then(|m| mdkg_run::hooks::live_turn_context(common, &m));
        if let Some(turn) = turn {
            return Some(turn.actor);
        }
        pid = table.parent(pid)?;
    }
    None
}

/// [`ancestor_turn`] for a person-only action, failing closed (orchestrator,
/// 2026-10-29): a process table that can't be read, an unknown parent or a
/// chain deeper than [`ANCESTORS_MAX`] is `Err`, naming why, which the
/// caller takes as inside a turn. `Ok(None)` means the chain reached the
/// system's first process with no live turn of this repository on it.
pub fn ancestor_turn_checked(common: &Path) -> Result<Option<String>, String> {
    checked_walk(Processes::read(), std::process::id(), common)
}

fn checked_walk(
    table: Option<Processes>,
    own: u32,
    common: &Path,
) -> Result<Option<String>, String> {
    let table = table.ok_or("the process table can't be read")?;
    let mark = format!("{}=", mdkg_run::profile::TURN_MARK);
    let unknown = |pid: u32| format!("process {pid}'s parent is unknown");
    let mut pid = table.parent(own).ok_or_else(|| unknown(own))?;
    for _ in 0..ANCESTORS_MAX {
        if pid <= 1 {
            return Ok(None);
        }
        // An ancestor whose environment can't be read fails closed too
        // (review of e52416d5); another user's process can't be this
        // user's turn, so its hidden environment is fine.
        let env = table
            .environment_checked(pid)
            .ok_or_else(|| format!("process {pid}'s environment can't be read"))?;
        let turn = env
            .iter()
            .find_map(|v| v.strip_prefix(&mark).map(String::from))
            .filter(|m| !m.is_empty())
            .and_then(|m| mdkg_run::hooks::live_turn_context(common, &m));
        if let Some(turn) = turn {
            return Ok(Some(turn.actor));
        }
        pid = table.parent(pid).ok_or_else(|| unknown(pid))?;
    }
    Err(format!(
        "the process chain is deeper than {ANCESTORS_MAX} processes"
    ))
}

/// Processes' parents and environments: `/proc` read as needed on Linux;
/// elsewhere two `ps -A` listings, read once.
enum Processes {
    Proc,
    Ps {
        parents: std::collections::HashMap<u32, u32>,
        environments: std::collections::HashMap<u32, Vec<String>>,
    },
}

impl Processes {
    fn read() -> Option<Self> {
        if Path::new("/proc/self/stat").exists() {
            return Some(Self::Proc);
        }
        // `ps -E` prints the command, then the environment: only what
        // follows the command as plain `ps` prints it is the environment,
        // so an argument can't pose as a mark.
        let plain = ps(&["-A", "-ww", "-o", "pid=,ppid=,command="], false)?;
        let with_env = ps(&["-A", "-ww", "-o", "pid=,ppid=,command="], true)?;
        let mut parents = std::collections::HashMap::new();
        let mut commands = std::collections::HashMap::new();
        for l in plain.lines() {
            if let Some((pid, ppid, command)) = ps_fields(l) {
                parents.insert(pid, ppid);
                commands.insert(pid, command);
            }
        }
        // Matched by pid and the command's text, never by the padded
        // line: a process that starts or ends between the two listings can
        // change the columns' widths.
        let environments = with_env
            .lines()
            .filter_map(|l| {
                let (pid, _, rest) = ps_fields(l)?;
                let env = rest.strip_prefix(commands.get(&pid)?)?;
                Some((pid, env.split_whitespace().map(String::from).collect()))
            })
            .collect();
        Some(Self::Ps {
            parents,
            environments,
        })
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        match self {
            Self::Proc => {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // `pid (comm) state ppid ...`: the name can hold spaces.
                let rest = &stat[stat.rfind(')')? + 1..];
                rest.split_whitespace().nth(1)?.parse().ok()
            }
            Self::Ps { parents, .. } => parents.get(&pid).copied(),
        }
    }

    fn environment(&self, pid: u32) -> Option<Vec<String>> {
        match self {
            Self::Proc => {
                let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
                Some(
                    env.split(|b| *b == 0)
                        .filter(|v| !v.is_empty())
                        .map(|v| String::from_utf8_lossy(v).into_owned())
                        .collect(),
                )
            }
            Self::Ps { environments, .. } => environments.get(&pid).cloned(),
        }
    }

    /// [`Self::environment`] for a check that fails closed: `None` only
    /// when it can't be read. Another user's process, whose environment
    /// `/proc` keeps from this one, reads as empty. (`ps -E` prints
    /// another user's process without its environment, which reads as
    /// empty already; a process missing from that listing is `None`.)
    fn environment_checked(&self, pid: u32) -> Option<Vec<String>> {
        use std::os::unix::fs::MetadataExt as _;
        match self.environment(pid) {
            Some(env) => Some(env),
            None if matches!(self, Self::Proc) => {
                let owner = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
                let own = std::fs::metadata("/proc/self").ok()?.uid();
                (owner != own).then(Vec::new)
            }
            None => None,
        }
    }
}

/// A `ps -o pid=,ppid=,command=` line's pid, parent and the rest (the
/// command, then with `-E` the environment), whatever the padding.
fn ps_fields(line: &str) -> Option<(u32, u32, &str)> {
    let line = line.trim();
    let (pid, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (ppid, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((pid.parse().ok()?, ppid.parse().ok()?, rest.trim_start()))
}

fn ps(args: &[&str], env: bool) -> Option<String> {
    let bin = ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|p| Path::new(p).is_file())?;
    let mut cmd = Command::new(bin);
    if env {
        cmd.arg("-E");
    }
    let out = cmd
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the git built-ins read.
struct Git<'a> {
    root: &'a Path,
    top: &'a PathBuf,
    actor: &'a str,
    call: &'a GitCall,
    live: &'a live::Live,
    /// pre-push's remote: a name, or a URL.
    remote: Option<&'a str>,
    /// pre-push's updates, `None` when its input was past the cap.
    updates: Option<&'a [Update]>,
    /// The hook's one identity (#6945).
    identity: &'a Pinned,
    /// Whether there's a graph to read leases and graph files from: not
    /// for a positively absent one, where only `git.protect` and
    /// `secret.scan` run.
    has_graph: bool,
}

fn evaluate(rules: &[Rule], e: &HookEvent, c: &Git<'_>) -> Decision {
    let mut said = Vec::new();
    for rule in rules {
        if !rules::applies(rule, e, Some(c.call)) {
            continue;
        }
        let got = match rule.uses.as_str() {
            // git names each ref a push updates; `heads` only matter for
            // the tool layer's reading of a command.
            "builtin:git.protect" => protect(
                rule,
                c.call,
                &[],
                &protected(c.root, c.actor, c.identity),
                None,
            ),
            "builtin:lease.staged" if !c.has_graph => None,
            "builtin:git.graph_path" if !c.has_graph => graph_path_at(e.actor.kind, c.top, None),
            "builtin:lease.staged" => match staged_paths(c.top) {
                Some(paths) => staged(e.actor.kind, &paths, &live::Entries(c.live)),
                None => None,
            },
            "builtin:git.graph_path" => graph_path_at(
                e.actor.kind,
                c.top,
                c.identity.as_ref().ok().and_then(|s| s.authority.as_ref()),
            ),
            "builtin:secret.scan" => secret_scan_at(e.event, c),
            _ => None,
        };
        if let Some((verdict, text, code)) = got {
            said.push(Said {
                rule: rule.id.clone(),
                verdict: rules::moded(rule, verdict),
                text,
                code: Some(code.into()),
            });
        }
    }
    rules::combine(said)
}

/// `git.graph_path` for the commit being made at `top`. Only in ref mode:
/// in checkout mode code branches carry the graph, and only `.mdkg-edit/`
/// copies are refused. An agent's commit on the graph ref's own branch
/// (named exactly: `MDKG-graph` is another branch) is refused, since only
/// mdkg's write path moves the graph ref; a person's passes (orchestrator
/// #11241). Graph-ref settings or an authority that can't be read refuse
/// for everyone (R49), as does a commit whose changes git can't list.
fn graph_path_at(
    kind: ActorKind,
    top: &Path,
    pinned: Option<&mdkg_graph::graphref::Authority>,
) -> Option<(Verdict, String, &'static str)> {
    use mdkg_graph::graphref;
    let unreadable = |what: &str| {
        Some((
            Verdict::Deny,
            format!(
                "mdkg can't read {what}, so it can't check this commit's graph files and \
                 refuses it. Run `{BIN} doctor`, and `{BIN} install` to repair the graph \
                 settings; a person can commit with --no-verify."
            ),
            "hook.git_graph_path.unreadable",
        ))
    };
    let no_graph = GraphFacts {
        dirs: &[],
        design: &[],
        tip: None,
        main_parents: &[],
    };
    let graph = match graphref::resolve(top) {
        Ok(Some(graph)) => graph,
        Ok(None) => {
            return match graph_changes(top, &no_graph) {
                Some(changed) => graph_path(kind, &changed),
                None => unreadable("this commit's staged changes"),
            };
        }
        Err(e) => return unreadable(&format!("this clone's graph-ref settings ({})", e.code)),
    };
    let head = git_out(top, &["symbolic-ref", "--quiet", "HEAD"]);
    if head.is_some_and(|h| h.trim() == graph.name) {
        return (kind != ActorKind::Human).then(|| {
            (
                Verdict::Deny,
                format!(
                    "mdkg refuses an agent's commit on the graph ref's own branch {}: only \
                     mdkg's write path moves the graph ref. Change nodes with `{BIN} node \
                     edit <id>` and `{BIN} node save <id>` (or the MCP tools), and commit \
                     code on a code branch.",
                    super::printable(&graph.name)
                ),
                "hook.git_graph_path.graph_branch",
            )
        });
    }
    // The hook's pinned authority (#6945), never a later tip.
    let Some(authority) = pinned else {
        return unreadable("the graph ref's authority");
    };
    let dir = graph
        .graph_dirs
        .get(&authority.graph_id)
        .map_or(".mdkg", String::as_str);
    let dirs = match mdkg_graph::config::from_snapshot(authority.config.clone())
        .map_err(|e| e.to_string())
        .and_then(|cfg| graphref::graph_paths_dir(&cfg, dir).map_err(|e| e.code.to_string()))
    {
        Ok(dirs) => dirs,
        Err(_) => return unreadable("the graph's committed config"),
    };
    let design: Vec<String> = dirs.iter().map(|d| format!("{d}/design")).collect();
    let main_parents = graph.main_ref.as_deref().map_or_else(Vec::new, |main| {
        held_by(
            top,
            main,
            &merge_parents(top, || std::env::vars().collect()),
        )
    });
    let facts = GraphFacts {
        dirs: &dirs,
        design: &design,
        tip: Some(&authority.commit),
        main_parents: &main_parents,
    };
    match graph_changes(top, &facts) {
        Some(changed) => graph_path(kind, &changed),
        None => unreadable("this commit's staged changes"),
    }
}

/// `secret.scan` at git's own hooks, for everyone: a commit's staged
/// changes, or the commits a push sends. A scan that can't run refuses
/// (orchestrator, 2026-10-30); a person can still skip git's hooks.
fn secret_scan_at(event: EventName, c: &Git<'_>) -> Option<(Verdict, String, &'static str)> {
    let scanned = match event {
        EventName::GitPrePush => match c.updates {
            None => Err(super::secret::ScanError(
                "pre-push's input is past what mdkg reads".into(),
            )),
            Some(updates) => {
                super::secret::outgoing(c.top, c.remote, updates).and_then(|(commits, full)| {
                    let git_dir = git_out(c.top, &["rev-parse", "--absolute-git-dir"])
                        .map(|d| PathBuf::from(d.trim()))
                        .ok_or_else(|| {
                            super::secret::ScanError("can't find the git folder".into())
                        })?;
                    let mut sent = super::secret::scan(Path::new("git"), &git_dir, &commits)?;
                    sent.full_history = full;
                    Ok(sent)
                })
            }
        },
        _ => super::secret::scan_staged(c.top),
    };
    secret_verdict(scanned)
}

/// `secret.scan` for an agent harness's shell tool (`tool.before`), for
/// everyone: before a commit, what it would stage (its index, and the
/// working-tree changes its `-a` or pathspecs bring in); before a push,
/// the commits it would send, and first what a commit the same command
/// line makes would add. It runs where the command does (`cd`, `-C`). A
/// scan that can't run refuses (orchestrator, 2026-10-30).
pub fn secret_scan_tool(
    root: &Path,
    cwd: Option<&Path>,
    git: &GitCall,
) -> Option<(Verdict, String, &'static str)> {
    let top = Repo::open(root).ok().map(|r| r.top);
    let Some(dir) = cwd.map(Path::to_path_buf).or(top) else {
        return secret_verdict(Err(super::secret::ScanError(
            "the command's folder is unknown".into(),
        )));
    };
    // Where the commit or push runs can't be read: refuse rather than
    // scan the folder the command started in (review of 689b8498).
    if git.at_unknown {
        return secret_verdict(Err(super::secret::ScanError(
            "the command's folder is unknown".into(),
        )));
    }
    // Pathspecs are read from where the command line started: absolute
    // here, so they name the same files from the folder git runs in.
    let stages = git.commit.as_ref().map(|s| {
        let mut s = s.clone();
        for p in s.paths.iter_mut().chain(s.forced.iter_mut()) {
            if !p.starts_with(':') {
                *p = dir.join(p.as_str()).to_string_lossy().into_owned();
            }
        }
        s
    });
    let dir = match &git.at {
        Some(at) => dir.join(at),
        None => dir,
    };
    let commit = || match &stages {
        Some(stages) => super::secret::scan_commit(&dir, stages),
        None => Ok(super::secret::Scanned::default()),
    };
    let scanned = match git.verb {
        GitVerb::Commit => commit(),
        GitVerb::Push => commit().and_then(|first| {
            if first.finding.is_some() {
                return Ok(first);
            }
            let remote = git
                .remote
                .clone()
                .or_else(|| current_branch(&dir).and_then(|b| default_remote(&dir, &b)));
            let (commits, full) =
                super::secret::outgoing_from(&dir, remote.as_deref(), &git.sources, git.all)?;
            let git_dir = git_out(&dir, &["rev-parse", "--absolute-git-dir"])
                .map(|d| PathBuf::from(d.trim()))
                .ok_or_else(|| super::secret::ScanError("can't find the git folder".into()))?;
            let mut sent = super::secret::scan(Path::new("git"), &git_dir, &commits)?;
            sent.skipped.splice(0..0, first.skipped);
            sent.full_history = full;
            Ok(sent)
        }),
        GitVerb::Other => return None,
    };
    secret_verdict(scanned)
}

/// A scan's result as `secret.scan`'s verdict: a finding refuses, a scan
/// that can't run refuses, and blobs too large to read are a warning
/// naming them (orchestrator #11600). Paths are printed made safe; what
/// matched never is.
pub fn secret_verdict(
    scanned: Result<super::secret::Scanned, super::secret::ScanError>,
) -> Option<(Verdict, String, &'static str)> {
    let then = "Remove it, and rotate the credential if it was ever real. A person \
                can skip git's hooks (--no-verify) for a false match.";
    // A push no tracking ref bounded scanned the whole history; say so,
    // since the commit may already be on the remote (review of 8cf3c619).
    let full = scanned.as_ref().is_ok_and(|s| s.full_history);
    let scanned = scanned.map(|s| (s.finding, s.skipped));
    match scanned {
        Ok((None, skipped)) if skipped.is_empty() => None,
        Ok((None, skipped)) => Some((
            Verdict::Context,
            unscanned_line(&skipped),
            "hook.secret_scan.unscanned",
        )),
        Ok((Some(f), _)) if f.commit.is_empty() => Some((
            Verdict::Deny,
            format!(
                "mdkg refuses this commit: the changes it commits add a credential-shaped \
                 string to {}. Unstage it or take it out, then commit again. {then}",
                super::printable(&f.path)
            ),
            "hook.secret_scan.denied",
        )),
        Ok((Some(f), _)) => Some((
            Verdict::Deny,
            format!(
                "mdkg refuses this push: commit {} adds a credential-shaped string to {}. \
                 Rewrite that commit without it (amend, or an interactive rebase) before \
                 pushing. {then}{}",
                super::printable(f.commit.get(..12).unwrap_or(&f.commit)),
                super::printable(&f.path),
                if full {
                    " This scan read the branch's whole history: this clone has no tracking \
                     refs of the remote to tell what it already holds. If that commit is \
                     already there, fetch from the remote first, then push again."
                } else {
                    ""
                }
            ),
            "hook.secret_scan.denied",
        )),
        Err(e) => Some((
            Verdict::Deny,
            format!(
                "mdkg couldn't scan this for credentials ({}), so it refuses it. Fix what \
                 stopped the scan and try again; a person can skip git's hooks \
                 (--no-verify).",
                super::printable(&e.0)
            ),
            "hook.secret_scan.unreadable",
        )),
    }
}

/// The warning for blobs a scan didn't read: each path, made safe.
pub fn unscanned_line(skipped: &[String]) -> String {
    let paths: Vec<String> = skipped.iter().map(|p| super::printable(p)).collect();
    format!(
        "mdkg: not scanned for credentials: larger than {} MiB: {}",
        super::secret::BLOB_MAX >> 20,
        paths.join(", ")
    )
}

/// `git.protect` for an agent harness's shell tool (`tool.before`): the
/// same rule, on the push the command names. Where the command alone
/// doesn't say which branches it updates, git's config decides; a push
/// that config makes ambiguous is refused (decided 2026-10-28: fail closed;
/// GitHub's branch protection stays the backstop).
pub fn protect_tool(
    rule: &Rule,
    root: &Path,
    cwd: Option<&Path>,
    actor: &str,
    git: &GitCall,
    identity: &Pinned,
) -> Option<(Verdict, String, &'static str)> {
    let top = Repo::open(root).ok().map(|r| r.top);
    let dir = cwd.map(Path::to_path_buf).or(top);
    let Some(dir) = dir else {
        return protect(rule, git, &[], &protected(root, actor, identity), None);
    };
    // `git -C <dir>` runs there: it must be a folder of this repository
    // (any worktree), whose protected branches are the ones read here.
    let (dir, elsewhere) = match &git.at {
        Some(at) => {
            let there = dir.join(at);
            let same = there.is_dir()
                && match (
                    mdkg_coord_sqlite::git::common_dir(&there).and_then(std::fs::canonicalize),
                    mdkg_coord_sqlite::git::common_dir(&dir).and_then(std::fs::canonicalize),
                ) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => false,
                };
            (there, !same)
        }
        None => (dir, false),
    };
    if elsewhere {
        return protect(
            rule,
            git,
            &[],
            &protected(root, actor, identity),
            Some("`git -C` names a folder outside this repository".into()),
        );
    }
    let current = current_branch(&dir);
    let target = push_target(&dir);
    // A push naming no branch goes to the default target (`@{push}`); a
    // `HEAD` it names goes to the branch checked out, by name.
    let heads: Vec<String> = if git.branches.is_empty() {
        [current.clone(), target.clone()]
            .into_iter()
            .flatten()
            .collect()
    } else {
        current.clone().into_iter().collect()
    };
    let remote = git
        .remote
        .clone()
        .or_else(|| current.as_deref().and_then(|b| default_remote(&dir, b)))
        .filter(|r| !r.starts_with('-'));
    let config = |key: &str| config_all(&dir, key);
    let why = unmapped(
        git,
        current.as_deref(),
        target.as_deref(),
        &remote
            .as_deref()
            .map_or_else(Vec::new, |r| config(&format!("remote.{r}.push"))),
        // As git reads a boolean: `yes`, `on`, `1`, `TRUE`, a bare key.
        remote.as_deref().is_some_and(|r| {
            git_out(
                &dir,
                &[
                    "config",
                    "--type=bool",
                    "--get",
                    &format!("remote.{r}.mirror"),
                ],
            )
            .is_some_and(|v| v.trim() == "true")
        }),
        config("push.default").pop().as_deref(),
    );
    let heads: Vec<&str> = heads.iter().map(String::as_str).collect();
    protect(rule, git, &heads, &protected(root, actor, identity), why)
}

/// Why `git.protect` can't tell from a command which branches its push
/// updates, `None` when it can: the remote mirrors, the remote's own push
/// refspecs (`remote.<name>.push`) decide for a push naming no
/// destination, `push.default` is `matching` or another mode that doesn't
/// push only the branch checked out, or nothing is checked out.
pub fn unmapped(
    git: &GitCall,
    current: Option<&str>,
    target: Option<&str>,
    push_specs: &[String],
    mirror: bool,
    push_default: Option<&str>,
) -> Option<String> {
    if git.verb != GitVerb::Push {
        return None;
    }
    if mirror {
        return Some("the remote is a mirror (`remote.<name>.mirror`)".into());
    }
    let to_head = git.branches.is_empty() || git.bare.iter().any(|b| b == "HEAD" || b == "@");
    if !push_specs.is_empty() && (git.branches.is_empty() || !git.bare.is_empty()) {
        return Some(
            "the remote's push refspecs (`remote.<name>.push`) decide where it goes".into(),
        );
    }
    if to_head && current.is_none() {
        return Some("no branch is checked out".into());
    }
    // `upstream` pushes to the upstream branch, which must resolve.
    if git.branches.is_empty()
        && target.is_none()
        && push_default.is_some_and(|m| matches!(m, "upstream" | "tracking"))
    {
        return Some("`push.default` is upstream, and the upstream doesn't resolve here".into());
    }
    if git.branches.is_empty()
        && let Some(mode) = push_default
        && !matches!(mode, "simple" | "current" | "upstream" | "tracking")
    {
        return Some(format!("`push.default` is {}", super::printable(mode)));
    }
    None
}

/// Where the branch checked out pushes to by default (`@{push}`), as a
/// branch name: `origin/main` is `main`.
fn push_target(top: &Path) -> Option<String> {
    let out = git(top, &["rev-parse", "--symbolic-full-name", "@{push}"])?;
    let full = String::from_utf8(out).ok()?;
    let rest = full.trim().strip_prefix("refs/remotes/")?;
    let (_, branch) = rest.split_once('/')?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// The remote a plain `git push` from `branch` uses: its `pushRemote`,
/// `remote.pushDefault`, its `remote`, else `origin`.
fn default_remote(top: &Path, branch: &str) -> Option<String> {
    [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".to_string(),
        format!("branch.{branch}.remote"),
    ]
    .iter()
    .find_map(|k| config_all(top, k).pop())
    .or_else(|| Some("origin".into()))
}

fn config_all(top: &Path, key: &str) -> Vec<String> {
    git(top, &["config", "--get-all", key])
        .map(|o| {
            String::from_utf8_lossy(&o)
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    /// On Linux, another user's process (pid 1, root's) reads as an empty
    /// environment, never as unreadable: it can't be this user's turn
    /// (review of f61f4a58). Skipped when the tests run as root.
    #[cfg(target_os = "linux")]
    #[test]
    fn another_users_process_reads_as_an_empty_environment() {
        use std::os::unix::fs::MetadataExt as _;
        let own = std::fs::metadata("/proc/self").expect("self").uid();
        let init = std::fs::metadata("/proc/1").expect("pid 1").uid();
        if own == init {
            return;
        }
        assert_eq!(Processes::Proc.environment_checked(1), Some(Vec::new()));
    }

    #[test]
    fn a_person_only_check_counts_an_unreadable_chain_as_inside_a_turn() {
        let common = Path::new("/nonexistent-common");
        // Every listed process with a readable (empty) environment.
        let table = |pairs: &[(u32, u32)]| Processes::Ps {
            parents: pairs.iter().copied().collect(),
            environments: pairs
                .iter()
                .flat_map(|&(pid, ppid)| [(pid, Vec::new()), (ppid, Vec::new())])
                .collect(),
        };
        assert!(checked_walk(None, 50, common).is_err(), "no table");
        let unknown = checked_walk(Some(table(&[(50, 40)])), 50, common);
        assert!(unknown.unwrap_err().contains("40's parent is unknown"));
        assert_eq!(
            checked_walk(Some(table(&[(50, 40), (40, 1)])), 50, common),
            Ok(None)
        );
        // An ancestor whose environment can't be read fails closed.
        let hidden = Processes::Ps {
            parents: [(50, 40), (40, 1)].into_iter().collect(),
            environments: std::collections::HashMap::new(),
        };
        assert_eq!(
            checked_walk(Some(hidden), 50, common),
            Err("process 40's environment can't be read".into())
        );
        let deep: Vec<(u32, u32)> = (2..200).map(|p| (p + 1, p)).collect();
        assert!(
            checked_walk(Some(table(&deep)), 199, common).is_err(),
            "too deep"
        );
    }

    use super::*;

    fn rule() -> Rule {
        rules::defaults()
            .into_iter()
            .find(|r| r.id == "git.protect")
            .expect("rule")
    }

    fn push(branches: &[&str]) -> GitCall {
        GitCall {
            verb: GitVerb::Push,
            remote: Some("origin".into()),
            branches: branches.iter().map(|b| b.to_string()).collect(),
            ..GitCall::default()
        }
    }

    fn protected() -> Vec<String> {
        mdkg_run::profile::protected("trunk", &["master".into(), "release".into()])
    }

    #[test]
    fn pushes_to_protected_branches_and_force_pushes_are_refused() {
        let r = rule();
        let p = protected();
        for b in ["main", "master", "trunk", "release", "MAIN"] {
            let got = protect(&r, &push(&[b]), &[], &p, None).expect("refused");
            assert_eq!(got.2, "hook.git_protect.denied", "{b}");
        }
        assert_eq!(
            protect(&r, &push(&["feature/x"]), &["main"], &p, None),
            None
        );
        // HEAD, or no branch at all, is the branch checked out.
        assert!(protect(&r, &push(&["HEAD"]), &["main"], &p, None).is_some());
        assert!(protect(&r, &push(&[]), &["main"], &p, None).is_some());
        assert_eq!(protect(&r, &push(&[]), &["feat"], &p, None), None);
        assert_eq!(protect(&r, &push(&[]), &[], &p, None), None);
        // A branch whose push target is main goes to main.
        assert!(protect(&r, &push(&[]), &["feat", "main"], &p, None).is_some());
        let mut skipping = push(&["feat"]);
        skipping.skips_hooks = true;
        assert_eq!(
            protect(&r, &skipping, &[], &p, None).expect("refused").2,
            "hook.git_protect.skips_hooks"
        );
        let mut forced = push(&["feat"]);
        forced.force = true;
        assert_eq!(
            protect(&r, &forced, &[], &p, None).expect("force").2,
            "hook.git_protect.force"
        );
        let mut all = push(&[]);
        all.all = true;
        assert!(protect(&r, &all, &["feat"], &p, None).is_some());
        let commit = GitCall::default();
        assert_eq!(protect(&r, &commit, &["main"], &p, None), None);
    }

    /// #11244: an authority that can't be read protects every branch,
    /// one segment or several, end to end through `protected`.
    #[test]
    fn an_unreadable_authority_refuses_every_push() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-protect-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dir)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        // Ref mode, with the graph ref gone: the authority can't be read.
        git(&["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
        let p = super::protected(&dir, "lane-a", &super::pin(&dir, &dir));
        std::fs::remove_dir_all(&dir).ok();
        assert!(p.contains(&"**".to_string()), "{p:?}");
        let r = rule();
        for b in ["main", "release", "release/x", "deploy/prod/eu"] {
            let got = protect(&r, &push(&[b]), &["feat"], &p, None).expect("refused");
            assert_eq!(got.2, "hook.git_protect.denied", "{b}");
        }
    }

    #[test]
    fn a_rules_branch_match_replaces_or_extends_the_protected_set() {
        let mut r = rule();
        r.matches.branch = Some(vec!["docs/*".into()]);
        let p = protected();
        assert!(protect(&r, &push(&["docs/a"]), &[], &p, None).is_some());
        assert_eq!(protect(&r, &push(&["main"]), &[], &p, None), None);
        r.matches.branch = Some(vec!["protected".into(), "docs/*".into()]);
        assert!(protect(&r, &push(&["main"]), &[], &p, None).is_some());
        assert!(protect(&r, &push(&["docs/a"]), &[], &p, None).is_some());
        assert_eq!(protect(&r, &push(&["src/a"]), &[], &p, None), None);
    }

    #[test]
    fn ps_lines_split_by_fields_whatever_the_padding() {
        assert_eq!(
            ps_fields("  412     1 /usr/bin/claude -p x A=1"),
            Some((412, 1, "/usr/bin/claude -p x A=1"))
        );
        assert_eq!(
            ps_fields("412 1 /usr/bin/claude -p x A=1"),
            Some((412, 1, "/usr/bin/claude -p x A=1"))
        );
        assert_eq!(ps_fields("  9 1"), Some((9, 1, "")));
        assert_eq!(ps_fields("pid ppid"), None);
    }

    #[test]
    fn a_push_its_config_makes_ambiguous_is_unmapped() {
        let named = |b: &str| {
            let mut c = push(&[b]);
            c.bare = vec![b.to_string()];
            c
        };
        let explicit = push(&["work"]);
        let specs = vec!["refs/heads/work:refs/heads/main".to_string()];
        // Plain pushes map exactly.
        assert_eq!(
            unmapped(&push(&[]), Some("work"), None, &[], false, None),
            None
        );
        assert_eq!(
            unmapped(&named("work"), Some("work"), None, &[], false, None),
            None
        );
        assert_eq!(
            unmapped(
                &push(&[]),
                Some("w"),
                Some("main"),
                &[],
                false,
                Some("upstream")
            ),
            None
        );
        // An upstream that doesn't resolve can't be checked.
        assert!(unmapped(&push(&[]), Some("w"), None, &[], false, Some("upstream")).is_some());
        // An explicit destination is exact whatever the config says.
        assert_eq!(
            unmapped(&explicit, Some("work"), None, &specs, false, None),
            None
        );
        // The rest is refused.
        for (call, current, specs, mirror, default) in [
            (push(&[]), Some("work"), specs.clone(), false, None),
            (named("work"), Some("work"), specs.clone(), false, None),
            (named("HEAD"), Some("work"), specs.clone(), false, None),
            (
                push(&[]),
                Some("work"),
                vec![":refs/heads/main".into()],
                false,
                None,
            ),
            (
                push(&[]),
                Some("work"),
                vec!["refs/tags/*:refs/heads/*".into()],
                false,
                None,
            ),
            (push(&[]), Some("work"), vec![], false, Some("matching")),
            (explicit.clone(), Some("work"), vec![], true, None),
            (push(&[]), None, vec![], false, None),
            (named("HEAD"), None, vec![], false, None),
        ] {
            assert!(
                unmapped(&call, current, None, &specs, mirror, default).is_some(),
                "{call:?} {specs:?} {mirror} {default:?}"
            );
        }
    }

    #[test]
    fn pre_push_input_reads_as_a_push_with_its_forces() {
        let zeros = "0".repeat(40);
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let input = format!(
            "refs/heads/feat {a} refs/heads/feat {b}\n\
             refs/heads/x {a} refs/heads/new {zeros}\n\
             garbage line\n\
             refs/tags/v1 {a} refs/tags/v1 {zeros}\n"
        );
        let u = updates(&input);
        assert_eq!(u.len(), 3);
        let ff = push_call(Some("origin"), &u, |_, _| Some(true));
        assert_eq!(ff.branches, ["feat", "new"]);
        assert!(!ff.force);
        assert!(push_call(None, &u, |_, _| Some(false)).force);
        assert!(
            push_call(None, &u, |_, _| None).force,
            "unknown counts as forcing"
        );
        let delete = updates(&format!("(delete) {zeros} refs/heads/feat {b}\n"));
        assert!(push_call(None, &delete, |_, _| Some(true)).force);
        // Moving a tag the remote has needs force, even to a descendant;
        // so does deleting one.
        let tag = updates(&format!("refs/tags/v1 {a} refs/tags/v1 {b}\n"));
        assert!(push_call(None, &tag, |_, _| Some(false)).force);
        assert!(push_call(None, &tag, |_, _| Some(true)).force);
        let same = updates(&format!("refs/tags/v1 {a} refs/tags/v1 {a}\n"));
        assert!(!push_call(None, &same, |_, _| Some(true)).force);
        let untag = updates(&format!("(delete) {zeros} refs/tags/v1 {b}\n"));
        assert!(push_call(None, &untag, |_, _| Some(true)).force);
    }

    struct Leases(Vec<(&'static str, Result<Lease, &'static str>)>);

    impl Holds for Leases {
        fn lease(&self, path: &str) -> Result<Lease, String> {
            self.0
                .iter()
                .find(|(p, _)| *p == path)
                .map_or(Ok(Lease::Unheld(path.into())), |(_, r)| {
                    r.clone().map_err(String::from)
                })
        }
    }

    #[test]
    fn graph_files_on_a_code_branch_refuse_an_agent_and_warn_a_person() {
        let dirs = vec![".mdkg".to_string(), "pkg/.mdkg".to_string()];
        assert_eq!(owned(".mdkg/work/a.md", &dirs), Some(Owned::Graph));
        assert_eq!(owned("pkg/.mdkg/design/b.md", &dirs), Some(Owned::Graph));
        assert_eq!(owned(".MDKG/work/a.md", &dirs), Some(Owned::Graph));
        assert_eq!(
            owned(".mdkg-edit/.mdkg/work/a.md", &dirs),
            Some(Owned::Edit)
        );
        assert_eq!(owned(".mdkg-edit/x", &[]), Some(Owned::Edit));
        assert_eq!(owned(".mdkgx/a.md", &dirs), None);
        assert_eq!(owned("pkg/.mdkgx/a.md", &dirs), None);
        // As git-serve judges a push (review of b2132376): a `.mdkg` part
        // anywhere, and the code points HFS+ ignores, count.
        assert_eq!(owned("src/.mdkg/x.md", &dirs), Some(Owned::Graph));
        assert_eq!(owned(".md\u{200C}kg/work/x.md", &dirs), Some(Owned::Graph));
        assert_eq!(owned("pkg/.MDKG\u{FEFF}/a.md", &dirs), Some(Owned::Graph));
        assert_eq!(owned(".mdkg\u{200D}-edit/x", &[]), Some(Owned::Edit));
        assert_eq!(owned("src/.mdkg-edit/x", &[]), None);
        // Checkout mode: only edit copies.
        assert_eq!(owned(".mdkg/work/a.md", &[]), None);

        assert_eq!(graph_path(ActorKind::Agent, &[]), None);
        // Exempt graph files say nothing.
        let exempt = [(Owned::Graph, true), (Owned::Graph, true)];
        assert_eq!(graph_path(ActorKind::Agent, &exempt), None);
        let (v, text, code) = graph_path(
            ActorKind::Agent,
            &[(Owned::Graph, false), (Owned::Graph, true)],
        )
        .expect("refused");
        assert_eq!((v, code), (Verdict::Deny, "hook.git_graph_path.denied"));
        assert!(
            text.contains("changes 1 graph file on a code branch"),
            "{text}"
        );
        assert!(
            !text.contains(".mdkg/work"),
            "paths are never echoed: {text}"
        );
        // An edit copy is never exempt.
        let (_, text, _) =
            graph_path(ActorKind::Orchestrator, &[(Owned::Edit, true)]).expect("refused");
        assert!(text.contains("1 file from `.mdkg-edit/`"), "{text}");
        let (v, text, code) = graph_path(
            ActorKind::Human,
            &[(Owned::Graph, false), (Owned::Edit, false)],
        )
        .expect("warned");
        assert_eq!((v, code), (Verdict::Context, "hook.git_graph_path.warned"));
        assert!(text.contains("A warning"), "{text}");
    }

    /// A ref-mode repository whose graph ref holds `config` at
    /// `.mdkg/config.json`; [`advance`] moves the ref to another config.
    fn ref_repo(config: &str) -> (Scratch, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mdkg-pin-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let top = dir.canonicalize().expect("canonical");
        sh(&top, &["init", "-q", "-b", "main"]);
        std::fs::write(top.join("code.txt"), "code\n").expect("code");
        sh(&top, &["add", "."]);
        sh(&top, &["commit", "-qm", "code"]);
        sh(&top, &["config", "mdkg.graph.ref", "mdkg-graph"]);
        sh(
            &top,
            &["config", &format!("mdkg.graph.{PIN_ID}.ref"), "mdkg-graph"],
        );
        advance(&top, config);
        (Scratch(dir), top)
    }

    const PIN_ID: &str = "0192f3a0-0000-7000-8000-00000000b1d5";

    /// Points the graph ref at a new commit holding `agents` as the
    /// config's `agents`, written with plumbing (no checkout).
    fn advance(top: &Path, agents: &str) {
        let config = format!(
            r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{PIN_ID}","agents":{agents}}}"#
        );
        let path = top.join(".git/pin-config.json");
        std::fs::write(&path, config).expect("config");
        let blob = sh(top, &["hash-object", "-w", path.to_str().expect("utf8")]);
        let mdkg = run_input(
            top,
            &["mktree"],
            &format!("100644 blob {blob}\tconfig.json\n"),
        );
        let root = run_input(top, &["mktree"], &format!("040000 tree {mdkg}\t.mdkg\n"));
        let commit = sh(top, &["commit-tree", &root, "-m", "graph"]);
        sh(top, &["update-ref", "refs/heads/mdkg-graph", &commit]);
    }

    fn run_input(top: &Path, args: &[&str], input: &str) -> String {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(top)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// #6945: a hook keeps the identity it captured first. Advancing the
    /// graph ref between two reads, so the worktree maps to someone else
    /// and the agent's protect list changes, changes neither the pinned
    /// actor nor the list `git.protect` reads; a fresh capture sees both.
    #[test]
    fn a_hook_keeps_its_pinned_identity_when_the_graph_ref_moves() {
        let (_dir, top) = ref_repo("{}");
        let cfg = |who: &str, protect: &str| {
            format!(
                r#"{{"{who}":{{"cwd":"{}","protect":["{protect}"]}}}}"#,
                top.display()
            )
        };
        advance(&top, &cfg("alice", "release"));
        let lazy = LazyPin::new(&top, &top);
        let first = lazy.get().as_ref().expect("identity").actor.clone();
        assert_eq!(first.as_deref(), Some("alice"));
        let pinned = lazy.get();
        assert!(
            super::protected(&top, "alice", pinned)
                .iter()
                .any(|b| b == "release")
        );
        advance(&top, &cfg("bob", "hotfix"));
        assert_eq!(
            lazy.get().as_ref().expect("identity").actor.as_deref(),
            Some("alice"),
            "the second read keeps the first capture"
        );
        let list = super::protected(&top, "alice", lazy.get());
        assert!(list.iter().any(|b| b == "release"), "{list:?}");
        assert!(!list.iter().any(|b| b == "hotfix"), "{list:?}");
        let fresh = pin(&top, &top);
        assert_eq!(fresh.as_ref().expect("fresh").actor.as_deref(), Some("bob"));
        assert!(
            super::protected(&top, "bob", &fresh)
                .iter()
                .any(|b| b == "hotfix")
        );
    }

    #[test]
    fn only_a_prepared_update_of_the_graph_ref_is_refused() {
        let graph = "refs/heads/mdkg-graph";
        let line = |r: &str| format!("{} {} {r}", "0".repeat(40), "1".repeat(40));
        let hit = vec![line("refs/heads/work"), line(graph)];
        let (v, text, code) =
            graph_ref_update(ActorKind::Agent, "prepared", graph, hit.clone()).expect("refused");
        assert_eq!((v, code), (Verdict::Deny, "hook.git_graph_ref.denied"));
        assert!(text.contains("refs/heads/mdkg-graph"), "{text}");
        let (v, _, code) =
            graph_ref_update(ActorKind::Human, "prepared", graph, hit.clone()).expect("warned");
        assert_eq!((v, code), (Verdict::Context, "hook.git_graph_ref.warned"));
        // The other states can't stop anything.
        for state in ["committed", "aborted", "preparing", ""] {
            assert_eq!(
                graph_ref_update(ActorKind::Agent, state, graph, hit.clone()),
                None
            );
        }
        // Another ref, a prefix of the name, or a short line isn't it; a
        // case-folding disk's spelling is.
        let miss = vec![
            line("refs/heads/work"),
            line("refs/heads/mdkg-graph-old"),
            "short".to_string(),
        ];
        assert_eq!(
            graph_ref_update(ActorKind::Agent, "prepared", graph, miss),
            None
        );
        let folded = vec![line("refs/heads/MDKG-Graph")];
        assert!(graph_ref_update(ActorKind::Agent, "prepared", graph, folded).is_some());
    }

    /// Review of 6670d321: only a main ref git says is missing reads as
    /// "no graph"; one it can't read refuses.
    #[test]
    fn a_main_ref_that_cant_be_read_refuses_and_a_missing_one_is_no_graph() {
        let (_dir, top) = graph_repo();
        // `main` commits no config.json: no graph.
        assert!(matches!(main_ref_graph(&top), Ok(None)));
        sh(&top, &["config", "mdkg.mainRef", "nosuch"]);
        assert!(
            matches!(main_ref_graph(&top), Ok(None)),
            "a missing main ref"
        );
        sh(&top, &["config", "mdkg.mainRef", "main"]);
        // A main ref naming an object that isn't there can't be read.
        let gone = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(top.join(".git/refs/heads/main"), format!("{gone}\n")).expect("ref");
        assert!(main_ref_graph(&top).is_err());
        // A corrupt loose ref, which git reads as missing, can't be read
        // either (review of 25daabd3).
        std::fs::write(top.join(".git/refs/heads/main"), "not a ref\n").expect("corrupt");
        assert!(main_ref_graph(&top).is_err());
    }

    /// A throwaway folder, removed when dropped.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A throwaway repository on `main` with `.mdkg/work/a.md` and
    /// `.mdkg/design/d.md`, hooks off.
    fn graph_repo() -> (Scratch, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mdkg-graph-path-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp");
        let top = dir.canonicalize().expect("canonical");
        sh(&top, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(top.join(".mdkg/work")).expect("work");
        std::fs::create_dir_all(top.join(".mdkg/design")).expect("design");
        std::fs::write(top.join(".mdkg/work/a.md"), "a1\n").expect("a");
        std::fs::write(top.join(".mdkg/design/d.md"), "d1\n").expect("d");
        sh(&top, &["add", "."]);
        sh(&top, &["commit", "-qm", "init"]);
        (Scratch(dir), top)
    }

    fn sh(top: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(top)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_merge_keeping_mains_blobs_and_a_design_copy_equal_to_the_tip_are_exempt() {
        let (_dir, top) = graph_repo();
        let dirs = vec![".mdkg".to_string()];
        let design = vec![".mdkg/design".to_string()];
        // The graph tip holds a newer design node than main.
        sh(&top, &["checkout", "-q", "-b", "graph"]);
        std::fs::write(top.join(".mdkg/design/d.md"), "d2\n").expect("d2");
        sh(&top, &["commit", "-qam", "graph"]);
        let tip = sh(&top, &["rev-parse", "HEAD"]);
        // main moves a work node; the code branch changes it too.
        sh(&top, &["checkout", "-q", "main"]);
        std::fs::write(top.join(".mdkg/work/a.md"), "a-main\n").expect("main");
        sh(&top, &["commit", "-qam", "main moves"]);
        let main = sh(&top, &["rev-parse", "HEAD"]);
        sh(&top, &["checkout", "-q", "-b", "code", "HEAD~1"]);
        std::fs::write(top.join(".mdkg/work/a.md"), "a-code\n").expect("code");
        sh(&top, &["commit", "-qam", "code moves"]);
        let changes = |main_parents: &[String]| {
            let facts = GraphFacts {
                dirs: &dirs,
                design: &design,
                tip: Some(&tip),
                main_parents,
            };
            graph_changes(&top, &facts).expect("changes")
        };
        // A conflicting merge resolved to main's blob is exempt...
        sh(&top, &["merge", "-q", "main"]);
        let parents = merge_parents(&top, Vec::new);
        assert_eq!(parents, std::slice::from_ref(&main));
        let held = held_by(&top, "refs/heads/main", &parents);
        assert_eq!(held, std::slice::from_ref(&main));
        sh(&top, &["checkout", "--theirs", "--", ".mdkg/work/a.md"]);
        sh(&top, &["add", ".mdkg/work/a.md"]);
        let changed = changes(&held);
        assert_eq!(changed, [(Owned::Graph, true)]);
        // ...and the same file written differently isn't.
        std::fs::write(top.join(".mdkg/work/a.md"), "a-resolved\n").expect("resolve");
        sh(&top, &["add", ".mdkg/work/a.md"]);
        let changed = changes(&held);
        assert_eq!(changed, [(Owned::Graph, false)]);
        // A parent main doesn't hold exempts nothing.
        assert!(held_by(&top, "refs/heads/graph", &parents).is_empty());
        sh(&top, &["merge", "--abort"]);
        // A design copy equal to the graph tip's is exempt; another isn't.
        std::fs::write(top.join(".mdkg/design/d.md"), "d2\n").expect("copy");
        sh(&top, &["add", ".mdkg/design/d.md"]);
        let changed = changes(&[]);
        assert_eq!(changed, [(Owned::Graph, true)]);
        std::fs::write(top.join(".mdkg/design/d.md"), "d3\n").expect("other");
        sh(&top, &["add", ".mdkg/design/d.md"]);
        let changed = changes(&[]);
        assert_eq!(changed, [(Owned::Graph, false)]);
        // A work node equal to the tip's blob gets no design exemption.
        sh(&top, &["reset", "-q", "--hard"]);
        std::fs::write(top.join(".mdkg/work/a.md"), "a1\n").expect("tip's");
        sh(&top, &["add", ".mdkg/work/a.md"]);
        let changed = changes(&[]);
        assert_eq!(changed, [(Owned::Graph, false)]);
        // git's `GITHEAD_<sha>` names a pre-merge-commit's other parents.
        let var = || vec![(format!("GITHEAD_{main}"), "main".to_string())];
        assert_eq!(merge_parents(&top, var), [main]);
    }

    #[test]
    fn staged_paths_without_a_lease_refuse_an_agent_and_warn_a_person() {
        let leases = Leases(vec![
            ("held.rs", Ok(Lease::Held)),
            ("bad.rs", Err("locked")),
        ]);
        let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            staged(ActorKind::Agent, &paths(&["held.rs"]), &leases),
            None
        );
        let agent = staged(
            ActorKind::Agent,
            &paths(&["held.rs", "UNTRUSTED_name.rs"]),
            &leases,
        )
        .expect("refused");
        assert_eq!(
            (agent.0, agent.2),
            (Verdict::Deny, "hook.lease_staged.denied")
        );
        assert!(
            agent.1.contains("1 staged file") && !agent.1.contains("UNTRUSTED_"),
            "{}",
            agent.1
        );
        let human = staged(ActorKind::Human, &paths(&["a", "b"]), &leases).expect("warned");
        assert_eq!(
            (human.0, human.2),
            (Verdict::Context, "hook.lease_staged.warned")
        );
        assert!(human.1.contains("2 staged files"));
        let unknown = staged(ActorKind::Agent, &paths(&["bad.rs"]), &leases).expect("refused");
        assert!(unknown.1.contains("couldn't check") && !unknown.1.contains("locked"));
    }

    #[test]
    fn a_refusal_exits_one_and_a_warning_zero() {
        assert_eq!(render(&Decision::allow()), (String::new(), 0));
        assert_eq!(
            render(&Decision::new(Verdict::Deny, Some("no".into()))),
            ("no\n".into(), 1)
        );
        assert_eq!(
            render(&Decision::new(Verdict::Context, Some("hm".into()))),
            ("hm\n".into(), 0)
        );
    }
}
