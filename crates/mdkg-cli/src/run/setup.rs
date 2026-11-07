//! `mdkg install` in ref mode: the three setups (edd-01a18867, Config,
//! "Install, the three setups").
//!
//! - `--setup personal|professional|local` moves a checkout-mode clone to
//!   the graph ref, and an explicit setup on a ref-mode clone rewrites its
//!   keys. Without one, a ref-mode clone keeps its keys and the setup is
//!   inferred, for the report and the ignore rules.
//! - The graph ref is kept, adopted from `refs/remotes/<remote>/<ref>`
//!   (a clone of a ref-mode repository; install fetches nothing), or
//!   created as an orphan, graph-only commit of the checkout's committed
//!   graph by a create compare-and-swap (R30). A repository with no graph
//!   gets the keys, and `init` creates the graph on the graph ref.
//! - Configuration alone never pushes: install pins no destination (G23).
//! - mdkg never requires main: no step reads one unless it's the code base
//!   being recorded (R2).

use std::path::{Path, PathBuf};

use mdkg_format::program::BIN;
use mdkg_graph::GraphError;
use mdkg_graph::graphref::{self, GraphRef};

use crate::coord::graph_git::graph_error;

/// The graph ref install writes.
pub(crate) const GRAPH: &str = "refs/heads/mdkg-graph";
/// The common `info/exclude` line that keeps `node edit`'s copies out of
/// every commit (R45).
pub(crate) const EDIT_IGNORE: &str = mdkg_run::install::worktree::EDIT_IGNORE;

/// One of the three setups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// `graph.remote = origin`: the graph syncs with the code's own remote.
    Personal,
    /// `graph.remote` is a private repository of your own, so the company
    /// remote is never touched.
    Professional,
    /// No remote.
    Local,
}

impl Setup {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Professional => "professional",
            Self::Local => "local",
        }
    }
}

/// What a ref-mode install was asked for.
pub(crate) struct Choice<'a> {
    pub setup: Option<Setup>,
    pub graph_remote: Option<&'a str>,
    pub publish_design: bool,
    pub view: bool,
    pub dry_run: bool,
}

/// What a ref-mode install did, or would do.
pub(crate) struct Done {
    pub setup: Setup,
    pub notes: Vec<String>,
    /// The graph ref exists (kept, adopted or created): projections sync
    /// from its tip.
    pub graph: bool,
    /// The fetched ref the graph ref is (or would be) adopted from.
    pub adopts: Option<String>,
}

fn refused(code: &'static str, message: impl Into<String>) -> GraphError {
    graph_error(code, message.into())
}

fn io(e: std::io::Error) -> GraphError {
    graph_error("graph.io", e.to_string())
}

fn from_ref(e: graphref::Error) -> GraphError {
    graph_error(e.code, e.message)
}

/// git in `top`, hardened: no hooks, no user config's redirections.
fn git(top: &Path, args: &[&str]) -> Result<std::process::Output, GraphError> {
    mdkg_coord_sqlite::git::command(top)
        .args(args)
        .run(None)
        .map_err(io)
}

fn git_ok(top: &Path, args: &[&str]) -> Result<String, GraphError> {
    let out = git(top, args)?;
    if !out.status.success() {
        return Err(refused(
            "io.git_failed",
            format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_some(top: &Path, args: &[&str]) -> Option<String> {
    git(top, args)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// The clone's common config, read and written as a file only (R4).
struct Common {
    top: PathBuf,
    file: String,
}

impl Common {
    fn get(&self, key: &str) -> Option<String> {
        git_some(&self.top, &["config", "--file", &self.file, "--get", key])
    }

    fn set(&self, key: &str, value: &str) -> Result<(), GraphError> {
        git_ok(&self.top, &["config", "--file", &self.file, key, value]).map(drop)
    }

    fn unset(&self, key: &str) -> Result<(), GraphError> {
        let out = git(&self.top, &["config", "--file", &self.file, "--unset", key])?;
        // 5: the key wasn't set.
        if out.status.success() || out.status.code() == Some(5) {
            Ok(())
        } else {
            Err(refused(
                "io.git_failed",
                format!(
                    "can't unset {key}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ))
        }
    }
}

fn commit_of(top: &Path, name: &str) -> Option<String> {
    git_some(
        top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    )
}

/// The remote's configured URLs, fetch and push.
fn urls(common: &Common, remote: &str) -> Vec<String> {
    let mut all: Vec<String> = ["url", "pushurl"]
        .iter()
        .filter_map(|k| common.get(&format!("remote.{remote}.{k}")))
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Where a remote's fetches and pushes go, compared as host and path:
/// `insteadOf` and `pushInsteadOf` applied (`git remote get-url`), then
/// the scheme, user, port, a trailing `.git` and slashes dropped, so
/// `git@host:org/app.git` and `https://host/org/app` are one destination.
fn destinations(top: &Path, remote: &str) -> Vec<String> {
    let mut all: Vec<String> = [
        &["remote", "get-url", "--all", remote][..],
        &["remote", "get-url", "--push", "--all", remote][..],
    ]
    .iter()
    .filter_map(|args| git_some(top, args))
    .flat_map(|s| s.lines().map(destination).collect::<Vec<_>>())
    .collect();
    all.sort();
    all.dedup();
    all
}

fn destination(url: &str) -> String {
    let url = url.trim();
    let (rest, scheme) = match url.split_once("://") {
        Some((_, rest)) => (rest, true),
        None => (url, false),
    };
    let local = !scheme && (rest.starts_with('/') || rest.starts_with('.'));
    let (authority, path) = if local {
        ("", rest)
    } else if scheme {
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        // scp-like `[user@]host:path`.
        rest.split_once(':').unwrap_or(("", rest))
    };
    // A user is dropped from the authority only, so an `@` in the path
    // (`org/app@v2.git`) stays part of the path.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    format!("{host}/{path}")
}

pub(crate) fn remotes(top: &Path) -> Vec<String> {
    git_some(top, &["remote"])
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The graph remote a setup names, checked: it exists (mdkg never creates
/// one, G18), and a professional one shares no URL with another remote,
/// so the company remote is never touched (Sync, "Destination guard").
fn graph_remote(
    common: &Common,
    setup: Setup,
    named: Option<&str>,
    kept: Option<&str>,
) -> Result<Option<String>, GraphError> {
    let name = match setup {
        Setup::Local => {
            if let Some(n) = named {
                return Err(refused(
                    "usage.invalid_arguments",
                    format!("--graph-remote {n} needs --setup professional (local has no remote)"),
                ));
            }
            return Ok(None);
        }
        Setup::Personal => {
            if let Some(n) = named.filter(|n| *n != "origin") {
                return Err(refused(
                    "usage.invalid_arguments",
                    format!(
                        "--graph-remote {n}: the personal setup syncs with origin; use --setup professional for another remote"
                    ),
                ));
            }
            "origin".to_owned()
        }
        Setup::Professional => match named.or(kept) {
            Some(n) => n.to_owned(),
            None => {
                return Err(refused(
                    "usage.invalid_arguments",
                    "the professional setup needs --graph-remote <name>, a private repository of your own",
                ));
            }
        },
    };
    let mine = urls(common, &name);
    if mine.is_empty() {
        return Err(refused(
            "graph.remote_invalid",
            format!(
                "there's no remote named {name}; add it with `git remote add {name} <url>`, then install again"
            ),
        ));
    }
    if setup == Setup::Professional {
        let mine = destinations(&common.top, &name);
        for other in remotes(&common.top).iter().filter(|r| **r != name) {
            if destinations(&common.top, other)
                .iter()
                .any(|u| mine.contains(u))
            {
                return Err(refused(
                    "graph.remote_is_code",
                    format!(
                        "remote {name} shares a URL with remote {other}; the professional setup's graph remote must be a repository of its own"
                    ),
                ));
            }
        }
    }
    Ok(Some(name))
}

/// The code base a setup records when none is configured: origin's
/// default branch (personal and professional), else a local `main` or
/// `master` (personal and local); `None` when there's none.
fn code_base(top: &Path, setup: Setup) -> Option<String> {
    let remote_default = || {
        git_some(top, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
            .filter(|r| commit_of(top, r).is_some())
    };
    let local = || {
        ["refs/heads/main", "refs/heads/master"]
            .into_iter()
            .find(|r| commit_of(top, r).is_some())
            .map(str::to_owned)
    };
    match setup {
        Setup::Personal => remote_default().or_else(local),
        Setup::Professional => remote_default(),
        Setup::Local => local(),
    }
}

/// The setup a ref-mode clone already has.
pub(crate) fn inferred(top: &Path, graph: Option<&GraphRef>) -> Setup {
    match graph.and_then(|g| g.remote.as_ref()) {
        Some(r) if r.name == "origin" => Setup::Personal,
        Some(_) => Setup::Professional,
        None if graph.is_none() && commit_of(top, "refs/remotes/origin/mdkg-graph").is_some() => {
            Setup::Personal
        }
        None => Setup::Local,
    }
}

/// How the graph ref comes to be.
enum Source {
    /// It exists.
    Keep(String),
    /// From a remote-tracking ref a clone or fetch made.
    Adopt { from: String, tip: String },
    /// From the checkout's committed graph at this commit.
    Create(String),
    /// No graph anywhere: `init` creates it.
    Init,
}

/// Private, disposable view of the selected commit's projection inputs.
/// No shared cache, ref or configuration is changed for setup preflight.
struct ProjectionSource(PathBuf, Vec<PathBuf>);

impl Drop for ProjectionSource {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

impl ProjectionSource {
    fn read(top: &Path, tip: &str) -> Result<Self, GraphError> {
        use std::io::Write as _;
        let path =
            std::env::temp_dir().join(format!("mdkg-install-preview-{}", uuid::Uuid::now_v7()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(io)?;
        let mut source = Self(path, Vec::new());
        let mut objects = mdkg_git::ObjectReader::open(top).map_err(io)?;
        let mut stack = vec![(format!("{tip}:.mdkg"), PathBuf::from(".mdkg"), 0usize)];
        let mut bytes = 0u64;
        let mut entries = 0usize;
        while let Some((oid, prefix, depth)) = stack.pop() {
            if depth > 128 || bytes >= mdkg_graph::cache::MAX_REFRESH_BYTES {
                return Err(refused(
                    "install.unmergeable",
                    "projection source exceeds its preview limit",
                ));
            }
            let (info, children) = objects
                .tree(
                    &oid,
                    (4 << 20).min(mdkg_graph::cache::MAX_REFRESH_BYTES - bytes),
                )
                .map_err(io)?
                .ok_or_else(|| {
                    refused("install.unmergeable", "projection source tree is missing")
                })?;
            bytes += info.size;
            for child in children {
                entries += 1;
                if entries > 100_000 {
                    return Err(refused(
                        "install.unmergeable",
                        "projection source has too many entries",
                    ));
                }
                let name = std::str::from_utf8(&child.name).map_err(|_| {
                    refused("install.unmergeable", "projection source name is not UTF-8")
                })?;
                if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0'])
                {
                    return Err(refused(
                        "install.unmergeable",
                        "projection source has an invalid name",
                    ));
                }
                if depth == 0 && !matches!(name, "config.json" | "README.md" | "skills" | "hooks") {
                    continue;
                }
                let rel = prefix.join(name);
                if rel.as_os_str().len() > 4096 {
                    return Err(refused(
                        "install.unmergeable",
                        "projection source path is too long",
                    ));
                }
                match child.mode.as_str() {
                    "40000" => stack.push((child.oid, rel, depth + 1)),
                    "100644" | "100755" => {
                        let cap = mdkg_graph::cache::MAX_FILE_BYTES
                            .min(mdkg_graph::cache::MAX_REFRESH_BYTES.saturating_sub(bytes));
                        let (info, data) = objects
                            .contents(&child.oid, cap)
                            .map_err(io)?
                            .ok_or_else(|| {
                                refused("install.unmergeable", "projection source blob is missing")
                            })?;
                        if info.kind != "blob" {
                            return Err(refused(
                                "install.unmergeable",
                                "projection source is not a blob",
                            ));
                        }
                        bytes += info.size;
                        let out = source.0.join(rel);
                        if let Some(parent) = out.parent() {
                            std::fs::create_dir_all(parent).map_err(io)?;
                        }
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&out)
                            .map_err(io)?;
                        file.write_all(&data).map_err(io)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            let mode = if child.mode == "100755" { 0o555 } else { 0o444 };
                            file.set_permissions(std::fs::Permissions::from_mode(mode))
                                .map_err(io)?;
                        }
                    }
                    // Omitted by the real cache, but not absent from Git.
                    _ => source.1.push(rel),
                }
            }
        }
        Ok(source)
    }
}

/// Whether a tip holds files outside the exact graph folders. A nested
/// graph owns only its folder's descendants, never its parent folders.
/// Unreadable trees or paths refuse classification rather than proving
/// a graph-only shape. Doctor uses the same proof (#14632).
pub(crate) fn full_shape(top: &Path, tip: &str, paths: &[String]) -> Result<bool, GraphError> {
    full_shape_within(top, tip, paths, entry_limit())
}

fn full_shape_within(
    top: &Path,
    tip: &str,
    paths: &[String],
    entries: usize,
) -> Result<bool, GraphError> {
    let objects = mdkg_git::ObjectReader::open(top)
        .map_err(|e| refused("graph.shape", format!("can't read the graph tree: {e}")))?;
    ShapeWalk::new(objects, paths, entries).tree(&format!("{tip}^{{tree}}"), "", 0)
}

/// How many tree entries a shape walk visits at most, reused subtrees
/// included. This is what ships.
pub(crate) const SHAPE_ENTRIES: usize = 100_000;

/// The walk's entry limit: [`SHAPE_ENTRIES`]. Debug builds also read
/// `MDKG_SHAPE_ENTRY_LIMIT`, and only a smaller number counts, so a test
/// trips the limit in seconds rather than walking 100,000 entries (F140).
/// Release builds never read it.
fn entry_limit() -> usize {
    if !cfg!(debug_assertions) {
        return SHAPE_ENTRIES;
    }
    lowered_limit(std::env::var("MDKG_SHAPE_ENTRY_LIMIT").ok().as_deref())
}

/// The override's value, if it's a number below [`SHAPE_ENTRIES`];
/// anything else leaves the shipped limit.
fn lowered_limit(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n < SHAPE_ENTRIES)
        .unwrap_or(SHAPE_ENTRIES)
}

/// A tip's shape as doctor reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    GraphOnly,
    Full,
    /// Full, found before the walk ran out of its budget: the rest of the
    /// tree wasn't checked (the message says which limit). A personal
    /// graph keeps a full shape, so that's a warning there, not an error
    /// (Terra #14932).
    FullUnchecked(String),
}

/// [`full_shape`], telling a full shape found before a budget limit apart
/// from one that couldn't be classified. Paths found invalid still fail.
pub(crate) fn shape(top: &Path, tip: &str, paths: &[String]) -> Result<Shape, GraphError> {
    shape_within(top, tip, paths, entry_limit())
}

fn shape_within(
    top: &Path,
    tip: &str,
    paths: &[String],
    entries: usize,
) -> Result<Shape, GraphError> {
    let objects = mdkg_git::ObjectReader::open(top)
        .map_err(|e| refused("graph.shape", format!("can't read the graph tree: {e}")))?;
    let mut walk = ShapeWalk::new(objects, paths, entries);
    match walk.tree(&format!("{tip}^{{tree}}"), "", 0) {
        Ok(true) => Ok(Shape::Full),
        Ok(false) => Ok(Shape::GraphOnly),
        Err(e) if walk.full_seen && walk.over_budget => {
            Ok(Shape::FullUnchecked(e.diagnostic.message))
        }
        Err(e) => Err(e),
    }
}

/// Read tree objects individually, never an expanded `ls-tree -r` stream
/// (#14759). Header preflight and reader caps bound Rust-owned tree data;
/// every visit, including reused DAG subtrees, consumes the total budget.
/// Git's internal inflation/packed-base memory is a residual (#14813).
struct ShapeWalk<'a> {
    objects: mdkg_git::ObjectReader,
    paths: &'a [String],
    bytes: u64,
    entries: usize,
    /// At most this many entries ([`entry_limit`]).
    entry_limit: usize,
    /// An entry outside the graph folders was found: the shape is full.
    full_seen: bool,
    /// The walk stopped at a depth, size or entry limit, not at a path it
    /// found invalid.
    over_budget: bool,
}

impl<'a> ShapeWalk<'a> {
    fn new(objects: mdkg_git::ObjectReader, paths: &'a [String], entry_limit: usize) -> Self {
        Self {
            objects,
            paths,
            bytes: 0,
            entries: 0,
            entry_limit,
            full_seen: false,
            over_budget: false,
        }
    }

    /// A budget limit's refusal, marked as one.
    fn limit(&mut self, message: &'static str) -> GraphError {
        self.over_budget = true;
        refused("graph.shape", message)
    }

    fn tree(&mut self, oid: &str, prefix: &str, depth: usize) -> Result<bool, GraphError> {
        const TREE_BYTES: u64 = 4 * 1024 * 1024;
        const TOTAL_BYTES: u64 = 16 * 1024 * 1024;
        const DEPTH: usize = 128;
        const PATH_BYTES: usize = 4096;
        let bad = |message| refused("graph.shape", message);
        if depth > DEPTH {
            return Err(self.limit("the graph tree exceeds the shape traversal depth limit"));
        }
        let cap = TREE_BYTES.min(TOTAL_BYTES - self.bytes);
        if self
            .objects
            .info(oid)
            .map_err(|e| {
                refused(
                    "graph.shape",
                    format!("can't read the graph tree header: {e}"),
                )
            })?
            .is_none_or(|info| info.kind != "tree" || info.size > cap)
        {
            // A tree over the per-tree cap or what's left of the total:
            // a budget limit, which a full shape found already turns into
            // "unchecked" (Terra #18260). Missing or not a tree is invalid.
            let header = self.objects.info(oid).ok().flatten();
            if header.is_some_and(|i| i.kind == "tree" && i.size > cap) {
                return Err(self.limit("the graph tree exceeds the shape traversal size limit"));
            }
            return Err(bad(
                "the graph tree header is missing, invalid or exceeds the size limit",
            ));
        }
        let (info, entries) = self
            .objects
            .tree(oid, cap)
            .map_err(|e| refused("graph.shape", format!("can't read the graph tree: {e}")))?
            .ok_or_else(|| bad("the graph tree can't be found"))?;
        self.bytes += info.size;
        match self
            .entries
            .checked_add(entries.len())
            .filter(|n| *n <= self.entry_limit)
        {
            Some(n) => self.entries = n,
            None => {
                return Err(self.limit("the graph tree exceeds the shape traversal entry limit"));
            }
        }
        let mut full = false;
        for entry in entries {
            let name = std::str::from_utf8(&entry.name)
                .map_err(|_| bad("a graph tree path isn't UTF-8; can't classify its shape"))?;
            if name.is_empty() || matches!(name, "." | "..") || name.contains('/') {
                return Err(bad("a graph tree path can't be classified"));
            }
            let separator = if prefix.is_empty() { 0 } else { 1 };
            if prefix.len() + separator + name.len() > PATH_BYTES {
                return Err(bad(
                    "a graph tree path exceeds the shape traversal length limit",
                ));
            }
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let tree = match u32::from_str_radix(&entry.mode, 8)
                .ok()
                .map(|m| m & 0o170000)
            {
                Some(0o040000) => true,
                Some(0o100000 | 0o120000 | 0o160000) => false,
                _ => return Err(bad("a graph tree entry has an unclassifiable mode")),
            };
            let owned = self.paths.iter().any(|p| {
                (tree && *p == path)
                    || path
                        .strip_prefix(p)
                        .is_some_and(|rest| rest.starts_with('/'))
            });
            // Only tree entries may be structural ancestors of graph
            // folders; their sibling files and trees remain unowned.
            let ancestor = tree
                && self.paths.iter().any(|p| {
                    p.strip_prefix(&path)
                        .is_some_and(|rest| rest.starts_with('/'))
                });
            if !owned && !ancestor {
                full = true;
                self.full_seen = true;
            }
            // A full shape still needs every later path checked, including
            // unowned subtrees: G20 doesn't permit unreadable paths (#14806).
            if tree {
                full |= self.tree(&entry.oid, &path, depth + 1)?;
            }
        }
        Ok(full)
    }
}

/// A tip as authority reads it, strictly valid: its graph id and graph
/// folders.
fn checked_tip(top: &Path, tip: &str) -> Result<(String, Vec<String>), GraphError> {
    let authority = graphref::authority_at(top, tip).map_err(from_ref)?;
    mdkg_graph::committed::load_strict(top, tip, "", &authority.graph_id)?;
    let config = mdkg_graph::config::from_snapshot(authority.config)?;
    let paths = graphref::graph_paths(&config).map_err(from_ref)?;
    Ok((authority.graph_id, paths))
}

/// Where the graph ref `name` (the clone's own `mdkg.graph.ref`, else
/// [`GRAPH`]) comes from.
fn source(
    top: &Path,
    name: &str,
    remote: Option<&str>,
    remote_ref: &str,
    ours: Option<&Path>,
) -> Result<Source, GraphError> {
    if let Some(tip) = commit_of(top, name) {
        if let Some(branch) = name.strip_prefix("refs/heads/")
            && let Some(path) = checked_out(top, branch)
            // Only a resolved path that is the identified checkout is
            // exempt: one git lists but this can't resolve still refuses.
            && std::fs::canonicalize(&path)
                .ok()
                .zip(ours)
                .is_none_or(|(p, o)| p.as_path() != o)
        {
            return Err(refused(
                "graph.view",
                format!(
                    "{path} has the graph checked out on {branch} (0.7.0's layout); detach it first, from that checkout, once `git -C {path} rev-parse --absolute-git-dir` names its own folder under the git folder's worktrees/ (`{BIN} doctor --repair` checks a registration that doesn't), then install again"
                ),
            ));
        }
        return Ok(Source::Keep(tip));
    }
    if let Some(remote) = remote {
        let short = remote_ref.strip_prefix("refs/heads/").unwrap_or(remote_ref);
        let from = format!("refs/remotes/{remote}/{short}");
        if let Some(tip) = commit_of(top, &from) {
            return Ok(Source::Adopt { from, tip });
        }
    }
    // The checkout's committed graph: no main is read (#14187).
    if let Some(commit) = commit_of(top, "HEAD")
        && git(
            top,
            &["cat-file", "-e", &format!("{commit}:.mdkg/config.json")],
        )?
        .status
        .success()
    {
        return Ok(Source::Create(commit));
    }
    Ok(Source::Init)
}

/// How many first-parent commits of an existing graph branch
/// [`move_keeps_graph`] looks back through for the checkout's graph.
const HISTORY_CAP: usize = 500;

/// The graph-owned paths of `rev`'s committed graph (graphref's list:
/// nested workspace and subgraph folders too), from its config alone.
/// `None` when it commits no graph config (a clone of a ref-mode
/// repository, design files only, or none at all). A committed config
/// that can't be read refuses rather than narrowing the check to `.mdkg`
/// (#14296).
pub(crate) fn committed_graph_paths(
    top: &Path,
    rev: &str,
) -> Result<Option<Vec<String>>, GraphError> {
    let Some(commit) = commit_of(top, rev) else {
        return Ok(None);
    };
    let has_config = git(
        top,
        &[
            "cat-file",
            "-e",
            &format!("{commit}:{}", mdkg_graph::config::CONFIG_PATH),
        ],
    )?
    .status
    .success();
    if !has_config {
        return Ok(None);
    }
    // The config alone, bounded as the binding reads it: the paths need
    // nothing else (#14462).
    let object = format!("{commit}:{}", mdkg_graph::config::CONFIG_PATH);
    let size = git_ok(top, &["cat-file", "-s", &object])?
        .parse::<u64>()
        .unwrap_or(u64::MAX);
    if size > 1 << 20 {
        return Err(refused(
            "graph.authority_invalid",
            format!("{object} is over 1 MiB"),
        ));
    }
    let text = git_ok(top, &["show", &object])?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        refused(
            "graph.authority_invalid",
            format!("{object} isn't valid JSON: {e}"),
        )
    })?;
    let config = mdkg_graph::config::from_snapshot(value)?;
    graphref::graph_paths(&config).map(Some).map_err(from_ref)
}

/// HEAD's graph-owned paths, and whether it commits a graph; `.mdkg` alone
/// when it doesn't, for the uncommitted check.
fn checkout_graph_paths(top: &Path) -> Result<(Vec<String>, bool), GraphError> {
    Ok(match committed_graph_paths(top, "HEAD")? {
        Some(paths) => (paths, true),
        None => (vec![mdkg_graph::committed::DEFAULT_DIR.to_owned()], false),
    })
}

/// `commit`'s tree entries for `paths`, as `git ls-tree` lists them: the
/// combined tree of the graph-owned paths, compared by object id.
fn graph_tree(top: &Path, commit: &str, paths: &[String]) -> Option<String> {
    let mut args = vec!["ls-tree", "-z", "--full-tree", commit, "--"];
    args.extend(paths.iter().map(String::as_str));
    git_some(top, &args)
}

/// The move from checkout mode to the graph ref keeps the graph when it
/// can't lose graph content that exists only in this checkout
/// (orchestrator, #14187; no main is read):
/// 1. uncommitted changes under any graph-owned path refuse it;
/// 2. with no graph branch (`name`, or the remote-tracking one setup
///    would adopt from `remote`), it starts from the checkout's committed
///    graph;
/// 3. an existing one is adopted when the checkout's graph is the graph
///    at its tip or at a commit in its first-parent history (a checkout
///    that's only behind), looking back at most [`HISTORY_CAP`] commits;
/// 4. otherwise it's refused: the checkout has graph changes the branch
///    doesn't.
///
/// Used by setup and by install's note offering the move. A change made
/// meanwhile, by another process, is a residual (dec-01a18ee0 item 13).
pub(crate) fn move_keeps_graph(
    top: &Path,
    name: &str,
    remote: Option<&str>,
    remote_ref: &str,
) -> Result<(), GraphError> {
    let (paths, has_graph) = checkout_graph_paths(top)?;
    let mut status = vec!["status", "--porcelain", "-z", "--untracked-files=all", "--"];
    status.extend(paths.iter().map(String::as_str));
    let dirty = git_ok(top, &status)?;
    // By content too, so an edit git's stat cache calls clean counts
    // (#15961, one rule with the graph checkout's).
    let hidden =
        commit_of(top, "HEAD").is_some() && content_differs(top, Some(&paths)).unwrap_or(true);
    if !dirty.is_empty() || hidden {
        return Err(refused(
            "graph.uncommitted",
            format!(
                "the checkout's graph ({}) has uncommitted changes, which the move to the graph \
                 ref would leave behind; commit them first, then run install again",
                paths.join(", ")
            ),
        ));
    }
    let branch = commit_of(top, name).or_else(|| {
        let remote = remote?;
        let short = remote_ref.strip_prefix("refs/heads/").unwrap_or(remote_ref);
        commit_of(top, &format!("refs/remotes/{remote}/{short}"))
    });
    let Some(tip) = branch else {
        return Ok(());
    };
    // A checkout with no committed graph of its own (a clone of a ref-mode
    // repository, or one holding only published design files) has
    // nothing to lose (#14296).
    if !has_graph {
        return Ok(());
    }
    let Some(mine) = graph_tree(top, "HEAD", &paths).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let history = git_ok(
        top,
        &[
            "rev-list",
            "--first-parent",
            &format!("--max-count={HISTORY_CAP}"),
            &tip,
        ],
    )?;
    if history
        .lines()
        .any(|c| graph_tree(top, c, &paths).as_deref() == Some(mine.as_str()))
    {
        return Ok(());
    }
    Err(refused(
        "graph.diverged",
        format!(
            "the checkout's graph has changes the graph branch {name} doesn't (looked back {} \
             commits); bring them together first, then run install again",
            HISTORY_CAP
        ),
    ))
}

/// The graph checkout the move inspects (orchestrator #15983, one
/// reader): the registered one, as install's own `registered_checkout`
/// validates it, wherever it is; with no records, the primary checkout's
/// `.worktrees/graph` when it's a plain folder that is a linked worktree
/// of this clone. Records that can't be resolved refuse.
fn graph_checkout(top: &Path) -> Result<Option<PathBuf>, GraphError> {
    let registered = mdkg_run::install::worktree::registered_graph_checkout(top).map_err(|e| {
        refused(
            "graph.view",
            format!(
                "the registered graph checkout can't be checked ({}); repair it with `{BIN} doctor \
                 --repair`, then install again",
                e.message
            ),
        )
    })?;
    if let Some(path) = registered {
        return std::fs::canonicalize(path).map(Some).map_err(io);
    }
    let Some(primary) = mdkg_run::install::worktree::primary_checkout(top)
        .ok()
        .flatten()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        return Ok(None);
    };
    let folder = primary.join(".worktrees");
    let slot = folder.join("graph");
    for p in [&folder, &slot] {
        if !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
            return Ok(None);
        }
    }
    let common = mdkg_coord_sqlite::git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(io)?;
    let linked = git_some(&slot, &["rev-parse", "--absolute-git-dir"])
        .and_then(|d| std::fs::canonicalize(d).ok())
        .is_some_and(|d| d.parent() == Some(common.join("worktrees").as_path()));
    Ok(linked.then_some(slot))
}

/// The primary checkout's `.worktrees/graph`, reached through no link.
fn primary_slot(top: &Path) -> Option<PathBuf> {
    let primary = mdkg_run::install::worktree::primary_checkout(top)
        .ok()
        .flatten()?;
    let primary = std::fs::canonicalize(primary).ok()?;
    let folder = primary.join(".worktrees");
    let slot = folder.join("graph");
    [&folder, &slot]
        .iter()
        .all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .then_some(slot)
}

/// The graph checkout setup detaches: the one [`graph_checkout`] finds,
/// when it's the primary's `.worktrees/graph` on the graph branch. On the
/// move from checkout mode (`moving`) it's inspected whether it's on the
/// branch or detached, in the slot or moved, by one rule (#15983):
/// nothing the graph branch lacks, so no edit (whatever git's stat cache
/// says) and no commit off the branch. In ref mode already, the slot is
/// the view and stays as it is.
fn view_to_detach(top: &Path, name: &str, moving: bool) -> Result<Option<PathBuf>, GraphError> {
    let Some(branch) = name.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if !moving {
        return Ok(None);
    }
    let Some(checkout) = graph_checkout(top)? else {
        return Ok(None);
    };
    let shown = checkout.display();
    if has_edits(&checkout) {
        return Err(refused(
            "graph.view",
            format!(
                "{shown} has uncommitted changes or ignored files, which ref mode wouldn't read; commit, move or discard them, then install again"
            ),
        ));
    }
    let behind = git(
        &checkout,
        &[
            "merge-base",
            "--is-ancestor",
            "HEAD",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if behind.status.code() != Some(0) {
        return Err(refused(
            "graph.view",
            format!(
                "{shown} is at a commit the graph branch {branch} doesn't hold; bring it onto {branch} (or discard it), then install again"
            ),
        ));
    }
    let attached = git_some(&checkout, &["symbolic-ref", "-q", "HEAD"])
        .is_some_and(|h| h == format!("refs/heads/{branch}"));
    Ok((attached && primary_slot(top).as_ref() == Some(&checkout)).then_some(checkout))
}

/// Whether a checkout holds anything its HEAD doesn't: changes, untracked
/// or ignored files whatever the status settings, index entries flagged
/// so status skips them (assume-unchanged, skip-worktree), and tracked
/// bytes or modes that differ from HEAD even where git's stat cache
/// calls them clean (#15486). Any doubt is yes.
fn has_edits(checkout: &Path) -> bool {
    let status = git_some(
        checkout,
        &[
            "-c",
            "status.showUntrackedFiles=all",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    );
    // `ls-files -v` tags assume-unchanged entries in lower case. A
    // skip-worktree (`S`) entry isn't refused for its flag: a sparse
    // checkout is a clean one, and content_differs compares a present
    // flagged file and skips only an absent one (Terra #16232).
    let flagged = git_some(checkout, &["ls-files", "-v"]).map(|l| {
        l.lines().any(|line| {
            line.chars()
                .next()
                .is_some_and(|tag| tag.is_ascii_lowercase())
        })
    });
    status.is_none_or(|s| !s.is_empty())
        || flagged.is_none_or(|f| f)
        || content_differs(checkout, None).unwrap_or(true)
}

/// The working files against HEAD by content, all of them or only under
/// `paths`: HEAD's entries read into a private index carry no stat data,
/// so its refresh hashes each file. `None` when git can't tell.
fn content_differs(checkout: &Path, paths: Option<&[String]>) -> Option<bool> {
    let index = std::env::temp_dir().join(format!("mdkg-view-index-{}", uuid::Uuid::now_v7()));
    let run = |args: &[&str], input: Option<&[u8]>| {
        mdkg_coord_sqlite::git::command(checkout)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .run(input)
            .ok()
    };
    let differs = (|| {
        match paths {
            None => run(&["read-tree", "HEAD"], None)?
                .status
                .success()
                .then_some(())?,
            Some(paths) => {
                let mut ls = vec!["ls-tree", "-r", "-z", "--full-tree", "HEAD", "--"];
                ls.extend(paths.iter().map(String::as_str));
                let listed = run(&ls, None)?;
                listed.status.success().then_some(())?;
                run(
                    &["update-index", "-z", "--index-info"],
                    Some(&listed.stdout),
                )?
                .status
                .success()
                .then_some(())?;
            }
        }
        // Sparse checkout: a path the real index marks skip-worktree and
        // the worktree doesn't have is intentionally absent, not deleted;
        // a present one is still compared (Terra #16163).
        let mut ls = vec!["ls-files", "-t", "-z", "--"];
        if let Some(paths) = paths {
            ls.extend(paths.iter().map(String::as_str));
        }
        let tagged = mdkg_coord_sqlite::git::command(checkout)
            .args(&ls)
            .run(None)
            .ok()?;
        tagged.status.success().then_some(())?;
        let mut sparse = Vec::new();
        for record in tagged.stdout.split(|b| *b == 0) {
            if let Some(path) = record.strip_prefix(b"S ") {
                // Git paths are bytes: kept as they are for both the check
                // and update-index, never re-encoded (Terra #16374).
                use std::os::unix::ffi::OsStrExt as _;
                let on_disk = checkout.join(std::ffi::OsStr::from_bytes(path));
                // Only a confirmed absence is sparse; any other error is
                // doubt, and doubt refuses (Terra #16232).
                match std::fs::symlink_metadata(on_disk) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        sparse.extend_from_slice(path);
                        sparse.push(0);
                    }
                    Err(_) => return None,
                }
            }
        }
        if !sparse.is_empty() {
            run(
                &["update-index", "-z", "--skip-worktree", "--stdin"],
                Some(&sparse),
            )?
            .status
            .success()
            .then_some(())?;
        }
        // Exit 1 lists stale entries; their content is what's compared.
        run(&["update-index", "-q", "--refresh"], None)?;
        let out = run(&["diff-files", "--quiet"], None)?;
        match out.status.code() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        }
    })();
    std::fs::remove_file(&index).ok();
    differs
}

/// The `mdkg.graph.<graph_id>.worktree` record of the graph setup
/// records `.ref` for: its checkout-mode graph checkout identity, which
/// ref mode has no use for. Other graphs' records (a nested graph's) are
/// their identity and stay.
fn worktree_record(common: &Common, graph_id: Option<&str>, record: bool) -> Option<String> {
    let key = format!("mdkg.graph.{}.worktree", graph_id.filter(|_| record)?);
    common.get(&key).map(|_| key)
}

/// The worktree a branch is checked out in, if any.
fn checked_out(top: &Path, branch: &str) -> Option<String> {
    let list = git_some(top, &["worktree", "list", "--porcelain"])?;
    let mut path = None;
    let want = format!("branch refs/heads/{branch}");
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_owned());
        } else if line == want {
            return path;
        }
    }
    None
}

/// An orphan, graph-only commit of `commit`'s graph folders, built in a
/// private index: the files and their modes exactly as committed.
fn graph_only_commit(
    top: &Path,
    common: &Path,
    commit: &str,
    paths: &[String],
    graph_id: &str,
) -> Result<String, GraphError> {
    let index = common.join(format!("mdkg/graph/install-{}.index", std::process::id()));
    let with_index = |args: &[&str]| -> Result<String, GraphError> {
        let out = mdkg_coord_sqlite::git::command(top)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .run(None)
            .map_err(io)?;
        if !out.status.success() {
            return Err(refused(
                "io.git_failed",
                format!(
                    "git {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    std::fs::create_dir_all(common.join("mdkg/graph")).map_err(io)?;
    let built = (|| {
        with_index(&["read-tree", "--empty"])?;
        for p in paths {
            if git(top, &["cat-file", "-e", &format!("{commit}:{p}")])?
                .status
                .success()
            {
                with_index(&[
                    "read-tree",
                    &format!("--prefix={p}/"),
                    &format!("{commit}:{p}"),
                ])?;
            }
        }
        let tree = with_index(&["write-tree"])?;
        git_ok(
            top,
            &[
                "commit-tree",
                &tree,
                "-m",
                &format!("mdkg: install graph {graph_id}"),
                "-m",
                &format!("Mdkg-From: {commit}"),
            ],
        )
    })();
    std::fs::remove_file(&index).ok();
    built
}

/// The common `info/exclude`'s path and bytes (empty when it doesn't
/// exist yet), refusing anything but a plain file. Read as bytes: git
/// allows any encoding there, and a lossy read could hide a final line.
fn exclude_file(common: &Path) -> Result<(PathBuf, Vec<u8>), GraphError> {
    let path = common.join("info").join("exclude");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err(refused(
            "install.unsupported_layout",
            format!("{} must be a plain file", path.display()),
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(io(e)),
    };
    Ok((path, bytes))
}

/// Whether git ignores what `line` names, asked of git itself
/// (`check-ignore --no-index` on a path inside it): the one test init,
/// install, doctor and `node edit` share. A rule that a later negation
/// undoes, or a near miss, isn't one (Terra #16291). `false` when git
/// can't answer, so the rule is (re)added.
pub(crate) fn excluded(top: &Path, line: &str) -> Result<bool, GraphError> {
    let name = line.trim_start_matches('/').trim_end_matches('/');
    let probe = format!("{name}/probe");
    let out = mdkg_coord_sqlite::git::command(top)
        .args(["check-ignore", "-q", "--no-index", "--", &probe])
        .run(None)
        .map_err(io)?;
    Ok(out.status.code() == Some(0))
}

/// Makes git ignore what `line` names, with a rule in the common
/// `info/exclude` (never a tracked file): appended last, so no earlier
/// negation undoes it. A final line without a newline is ended first. The
/// file is replaced whole (a private temporary file with the old file's
/// mode, synced, renamed over it, the folder synced), so a failed write
/// leaves it as it was (Terra #16176). A read-only file is refused, not
/// replaced. The folder is synced even when nothing is written, so a rename
/// a crash interrupted before its sync is made durable by the retry.
pub(crate) fn exclude(top: &Path, common: &Path, line: &str) -> Result<bool, GraphError> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = common.join("info");
    let created = !dir.exists();
    std::fs::create_dir_all(&dir).map_err(io)?;
    let sync = |d: &Path| {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(io)
    };
    if created {
        sync(common)?;
    }
    if excluded(top, line)? {
        sync(&dir)?;
        return Ok(false);
    }
    let (path, mut bytes) = exclude_file(common)?;
    // An existing file's mode is kept; a new one gets the umask's.
    let mode = match std::fs::metadata(&path) {
        Ok(m) => Some(m.permissions().mode() & 0o7777),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    if mode.is_some_and(|m| m & 0o200 == 0) {
        return Err(refused(
            "install.unsupported_layout",
            format!(
                "{} is read-only, and git doesn't ignore {line} yet: add the rule to it by hand",
                path.display()
            ),
        ));
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let tmp = dir.join(format!(".exclude-{}.tmp", uuid::Uuid::now_v7()));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Some(mode) = mode {
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(&dir)?.sync_all()
    })();
    if written.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    written.map_err(io)?;
    Ok(true)
}

/// Moves the clone to the graph ref, or brings a ref-mode clone's setup up
/// to date. The caller holds the clone's turns.
/// 0.7.1's ref mode reads the repository-root graph from `.mdkg` only, so
/// the move into it refuses a clone whose records register the root graph
/// in another folder (`mdkg.graph.<id>.dir` with no `.root`), with the
/// refusal doctor and every bound command give (orchestrator #16095).
/// Every root-level record naming another folder refuses, with one
/// exception: setup found the graph (`graph_id`), and the record is another
/// graph's whose `.ref` names a ref other than `name`, which the resolver
/// ignores (Terra #16224), and whose folder holds no graph in this checkout
/// (Terra #16379). With no graph found (`Source::Init`), any such
/// record refuses, so a graph that may live in that folder is never left
/// behind (Terra #16260, orchestrator #16270); the graph's own record
/// counts whatever its `.ref`, since the move rewrites it (Terra #16243).
fn root_folder_supported(
    top: &Path,
    common: &Common,
    name: &str,
    graph_id: Option<&str>,
    tip: Option<&str>,
    source: &str,
) -> Result<(), GraphError> {
    let Some(list) = git_some(
        top,
        &[
            "config",
            "--file",
            &common.file,
            "--get-regexp",
            r"^mdkg\.graph\..*\.dir$",
        ],
    ) else {
        return Ok(());
    };
    for line in list.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(id) = key
            .strip_prefix("mdkg.graph.")
            .and_then(|k| k.strip_suffix(".dir"))
        else {
            continue;
        };
        let root_key = common.get(&format!("mdkg.graph.{id}.root"));
        let root = root_key
            .as_deref()
            .map(graphref::normalize_root)
            .unwrap_or_default();
        let full = |r: &str| {
            if r.starts_with("refs/") {
                r.to_owned()
            } else {
                format!("refs/heads/{r}")
            }
        };
        let dir = graphref::normalize_root(value);
        // A record naming a folder that holds a graph in this checkout
        // (tracked on HEAD, or in the worktree) is never foreign: that
        // graph would be left behind (Terra #16379, orchestrator #16386).
        let present = !dir.is_empty()
            && (top.join(&dir).join("config.json").is_file()
                // The tree entry, not the blob: a partial clone may lack it
                // (Terra #16506). A literal pathspec, and a lookup that
                // fails (a subtree a sparse clone lacks) counts as present:
                // fail closed (Terra #16594).
                || git(
                    top,
                    &[
                        "--literal-pathspecs",
                        "ls-tree",
                        "--name-only",
                        "HEAD",
                        "--",
                        &format!("{dir}/config.json"),
                    ],
                )
                .map_or(true, |o| !o.status.success() || !o.stdout.is_empty()));
        let foreign = !present
            && graph_id.is_some_and(|g| g != id)
            && common
                .get(&format!("mdkg.graph.{id}.ref"))
                .is_some_and(|own| full(&own) != full(name));
        let used = !foreign;
        if used && root.is_empty() && !dir.is_empty() && dir != mdkg_graph::committed::DEFAULT_DIR {
            return Err(crate::config_cmd::unsupported_root_folder(
                top,
                &dir,
                &format!("mdkg.graph.{id}.dir"),
                tip,
                source,
            ));
        }
    }
    Ok(())
}

pub(crate) fn setup(
    top: &Path,
    choice: &Choice<'_>,
    preflight: impl FnOnce(
        &Path,
        &[PathBuf],
        &mdkg_run::projection::Projections,
        Setup,
    ) -> Result<(), GraphError>,
) -> Result<Done, GraphError> {
    let common_dir = mdkg_coord_sqlite::git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(io)?;
    let common = Common {
        top: top.to_path_buf(),
        file: common_dir.join("config").to_string_lossy().into_owned(),
    };
    let existing = graphref::resolve(top).map_err(from_ref)?;
    let explicit = choice.setup.is_some();
    let setup = choice
        .setup
        .unwrap_or_else(|| inferred(top, existing.as_ref()));
    let mut notes = vec![format!(
        "setup: {}{}",
        setup.name(),
        if explicit { "" } else { " (inferred)" }
    )];
    if let Some(e) = &existing
        && e.name != GRAPH
        && explicit
    {
        notes.push(format!("graph ref: {} (kept)", e.name));
    }
    let name = existing
        .as_ref()
        .map_or(GRAPH, |e| e.name.as_str())
        .to_owned();
    let kept_remote = existing
        .as_ref()
        .and_then(|e| e.remote.as_ref())
        .map(|r| r.name.clone());
    let remote = if explicit {
        graph_remote(&common, setup, choice.graph_remote, kept_remote.as_deref())?
    } else {
        kept_remote.clone()
    };
    let remote_ref = existing
        .as_ref()
        .and_then(|e| e.remote.as_ref())
        .map_or_else(|| name.clone(), |r| r.remote_ref.clone());
    // The keys, in order; `mdkg.graph.ref` last, which switches the clone
    // to ref mode only once the rest is written (Migration, step 7).
    let mut keys: Vec<(&'static str, Option<String>)> = Vec::new();
    if explicit {
        keys.push(("mdkg.graph.remote", remote.clone()));
        if common.get("mdkg.mainRef").is_none()
            && let Some(base) = code_base(top, setup)
        {
            keys.push(("mdkg.mainRef", Some(base)));
        }
        keys.push((
            "mdkg.follow.fetch",
            Some(
                if setup == Setup::Personal {
                    "on"
                } else {
                    "off"
                }
                .into(),
            ),
        ));
        let publish = setup == Setup::Personal || choice.publish_design;
        keys.push((
            "mdkg.graph.publishDesign",
            Some(if publish { "on" } else { "off" }.into()),
        ));
    } else if common.get("mdkg.graph.publishDesign").is_none() {
        // The setup's default, once (G16).
        let publish = setup == Setup::Personal || choice.publish_design;
        keys.push((
            "mdkg.graph.publishDesign",
            Some(if publish { "on" } else { "off" }.into()),
        ));
    }
    // The move from checkout mode keeps the graph (#14187).
    if existing.is_none() {
        move_keeps_graph(top, &name, remote.as_deref(), &remote_ref)?;
    }
    let detach = view_to_detach(top, &name, existing.is_none())?;
    let src = source(
        top,
        &name,
        remote.as_deref(),
        &remote_ref,
        detach.as_deref(),
    )?;
    // What the graph ref will hold, checked before anything is written.
    let (graph_id, record, action) = match &src {
        Source::Keep(tip) => {
            let (id, paths) = checked_tip(top, tip)?;
            if setup == Setup::Professional && full_shape(top, tip, &paths)? {
                return Err(refused(
                    "graph.shape",
                    format!(
                        "{name} holds code as well as the graph (0.7.0's full shape), which a professional graph remote must never receive; convert it to a graph-only ref first"
                    ),
                ));
            }
            (
                Some(id),
                true,
                format!("graph ref: {name} at {} (kept)", short(tip)),
            )
        }
        Source::Adopt { from, tip } => {
            let (id, paths) = checked_tip(top, tip)?;
            if setup == Setup::Professional && full_shape(top, tip, &paths)? {
                return Err(refused(
                    "graph.shape",
                    format!(
                        "{from} holds code as well as the graph; a professional graph remote must hold a graph-only ref"
                    ),
                ));
            }
            (
                Some(id),
                true,
                format!("graph ref: {name} from {from} at {}", short(tip)),
            )
        }
        Source::Create(commit) => {
            let authority = graphref::authority_at(top, commit).map_err(from_ref)?;
            (
                Some(authority.graph_id),
                true,
                format!(
                    "graph ref: {name}, created graph-only from the checkout's graph at {}",
                    short(commit)
                ),
            )
        }
        Source::Init => (
            None,
            false,
            format!("graph ref: {name}; there's no graph yet: `{BIN} init` creates it there"),
        ),
    };
    // A recorded identity must be this graph's (R6).
    if let (Some(e), Some(id)) = (&existing, &graph_id) {
        e.check_identity_or_unrecorded(id, "").map_err(from_ref)?;
    }
    // Let projection refusals inspect the actual kept/adopted/created
    // source, before a preview succeeds or any ref/config write occurs.
    let tip = match &src {
        Source::Keep(tip) | Source::Create(tip) | Source::Adopt { tip, .. } => Some(tip),
        Source::Init => None,
    };
    // Before any key or ref is written; the hint follows where the
    // source's config is (Terra #16260).
    root_folder_supported(
        top,
        &common,
        &name,
        graph_id.as_deref(),
        tip.map(String::as_str),
        // Where the config was read, named so it can be inspected: the
        // destination ref doesn't exist yet when adopting or creating
        // (Terra #16296).
        &match &src {
            Source::Keep(_) | Source::Init => name.clone(),
            Source::Adopt { from, .. } => from.clone(),
            Source::Create(commit) => format!("the checkout's commit {}", short(commit)),
        },
    )?;
    if let Some(tip) = tip {
        let authority = graphref::authority_at(top, tip).map_err(from_ref)?;
        let listed = mdkg_run::projection::Projections::from_config(&authority.config)
            .map_err(crate::run::graph_error)?;
        let snapshot = ProjectionSource::read(top, tip)?;
        preflight(&snapshot.0, &snapshot.1, &listed, setup)?;
    }
    notes.push(action);
    for (key, value) in &keys {
        notes.push(match value {
            Some(v) => format!("{key} = {v}"),
            None => format!("{key} unset"),
        });
    }
    if existing.as_ref().is_none_or(|e| e.name != name) {
        notes.push(format!("mdkg.graph.ref = {name}"));
    }
    if let Some(view) = &detach {
        notes.push(format!(
            "view: {} detached from {name}; it's a read-only view now",
            view.display()
        ));
    }
    let stale = worktree_record(&common, graph_id.as_deref(), record);
    if let Some(key) = &stale {
        notes.push(format!("{key} unset: ref mode has no graph checkout"));
    }
    if let Some(r) = &remote {
        notes.push(format!(
            "graph remote {r}: {}; nothing is pushed to it until a person approves this destination, which arrives with graph sync in a later release",
            urls(&common, r).join(", ")
        ));
    }
    let adopts = match &src {
        Source::Adopt { from, .. } => Some(from.clone()),
        _ => None,
    };
    if choice.dry_run {
        let mut dry: Vec<String> = notes.into_iter().map(|n| format!("dry run: {n}")).collect();
        dry.push(format!(
            "dry run: {} would exclude {EDIT_IGNORE}",
            common_dir.join("info/exclude").display()
        ));
        return Ok(Done {
            setup,
            notes: dry,
            graph: false,
            adopts,
        });
    }
    let _lock = mdkg_graph::refwrite::GraphLock::exclusive(
        &common_dir,
        mdkg_graph::refwrite::LockMode::Local,
    )
    .map_err(|e| graph_error(e.code(), e.to_string()))?;
    if exclude(top, &common_dir, EDIT_IGNORE)? {
        notes.push(format!(
            "{EDIT_IGNORE} added to {}",
            common_dir.join("info/exclude").display()
        ));
    }
    // The graph ref, by create compare-and-swap: never over one that
    // appeared meanwhile.
    let zero = "0".repeat(40);
    match &src {
        Source::Adopt { tip, .. } => {
            git_ok(top, &["update-ref", &name, tip, &zero])?;
        }
        Source::Create(commit) => {
            let authority = graphref::authority_at(top, commit).map_err(from_ref)?;
            let config = mdkg_graph::config::from_snapshot(authority.config)?;
            let paths = graphref::graph_paths(&config).map_err(from_ref)?;
            let made = graph_only_commit(top, &common_dir, commit, &paths, &authority.graph_id)?;
            // Strictly valid before anything can read it as authority.
            mdkg_graph::committed::load_strict(top, &made, "", &authority.graph_id)?;
            git_ok(top, &["update-ref", &name, &made, &zero])?;
        }
        Source::Keep(_) | Source::Init => {}
    }
    if record && let Some(id) = &graph_id {
        let key = format!("mdkg.graph.{id}.ref");
        if common.get(&key).as_deref() != Some(name.as_str()) {
            common.set(&key, &name)?;
            notes.push(format!("{key} = {name}"));
        }
    }
    for (key, value) in &keys {
        match value {
            Some(v) => common.set(key, v)?,
            None => common.unset(key)?,
        }
    }
    // The checkout leaves the graph branch before the switch, so no
    // command sees ref mode with a checkout still on it.
    if let Some(view) = &detach {
        git_ok(view, &["switch", "-q", "--detach"])?;
    }
    common.set("mdkg.graph.ref", &name)?;
    if let Some(key) = &stale {
        let out = git(top, &["config", "--file", &common.file, "--unset-all", key])?;
        if !out.status.success() && out.status.code() != Some(5) {
            return Err(refused(
                "io.git_failed",
                format!(
                    "can't unset {key}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
    }
    if choice.view {
        notes.push(view(top, &name)?);
    }
    Ok(Done {
        setup,
        notes,
        graph: graph_id.is_some(),
        adopts,
    })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// `--view`: `.worktrees/graph`, detached at the graph tip, read-only by
/// convention (G6); refreshed when it's clean.
fn view(top: &Path, name: &str) -> Result<String, GraphError> {
    let path = top.join(".worktrees/graph");
    let shown = path.display().to_string();
    if !path.join(".git").exists() {
        mdkg_run::install::worktree::ignore_worktrees(top, false)
            .map_err(crate::run::graph_error)?;
        git_ok(
            top,
            &["worktree", "add", "--detach", &path.to_string_lossy(), name],
        )?;
        return Ok(format!("view: {shown}, detached at {name}"));
    }
    if git_some(&path, &["symbolic-ref", "-q", "HEAD"]).is_some() {
        return Err(refused(
            "graph.view",
            format!(
                "{shown} is checked out on a branch; the view must be detached: `git -C {shown} switch --detach`"
            ),
        ));
    }
    if git_some(&path, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        return Ok(format!(
            "view: {shown} holds hand edits, so it wasn't refreshed"
        ));
    }
    // An ignored draft at a path the tip now has stays, refusing the
    // refresh, rather than being overwritten.
    git_ok(
        &path,
        &["checkout", "-q", "--no-overwrite-ignore", "--detach", name],
    )?;
    Ok(format!("view: {shown}, refreshed to {name}"))
}

/// `--checkout-mode`: back to 0.7.0's checkout mode in this binary (Terra
/// #6198 major 18). Unsets `mdkg.graph.ref`; the graph ref only moved
/// forward, so checkout mode reads it as its graph ahead of main. A
/// detached `.worktrees/graph` is reattached to the graph branch when it
/// sits at the tip and is clean. With the view removed, the records naming
/// it are unset first, and install's own graph checkout path makes and
/// registers a new one, so authority never falls back to this checkout's
/// HEAD (#15944). Rerun in checkout mode, it finishes an interrupted
/// cleanup.
pub(crate) fn checkout_mode(top: &Path, dry_run: bool) -> Result<Vec<String>, GraphError> {
    let common_dir = mdkg_coord_sqlite::git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(io)?;
    let common = Common {
        top: top.to_path_buf(),
        file: common_dir.join("config").to_string_lossy().into_owned(),
    };
    // Full names, as the authority reader resolves them: `mdkg-graph` is
    // `refs/heads/mdkg-graph`, never a same-named tag (#16207).
    let full = |v: String| graphref::graph_ref_name(&v).map_err(from_ref);
    let name = common.get("mdkg.graph.ref").map(full).transpose()?;
    // The graph being rolled back: the clone's ref, or after an
    // interrupted rollback the one its identity records name.
    let graph = match &name {
        Some(n) => n.clone(),
        None => recorded_ref(top, &common)
            .map(full)
            .transpose()?
            .unwrap_or_else(|| GRAPH.to_owned()),
    };
    let (gone, kept) = gone_records(top, &common, &common_dir, &graph)?;
    let mut notes = kept;
    let Some(name) = name else {
        notes.insert(0, "already in checkout mode".into());
        // A clone that never used a graph ref has nothing to finish.
        if gone.is_empty() && recorded_ref(top, &common).is_none() {
            return Ok(notes);
        }
        // A move from a custom ref interrupted before its records named
        // mdkg-graph: `mdkg-graph` is at the custom ref's commit.
        let graph = if graph != GRAPH
            && commit_of(top, &graph).is_some()
            && commit_of(top, &graph) == commit_of(top, GRAPH)
        {
            if dry_run {
                notes.push(format!(
                    "dry run: each mdkg.graph.<id>.ref naming {graph} would name {GRAPH}"
                ));
            } else {
                let _lock = mdkg_graph::refwrite::GraphLock::exclusive(
                    &common_dir,
                    mdkg_graph::refwrite::LockMode::Local,
                )
                .map_err(|e| graph_error(e.code(), e.to_string()))?;
                retarget_records(&common, &graph, &mut notes)?;
            }
            GRAPH.to_owned()
        } else {
            graph
        };
        if graph == GRAPH {
            // A rollback interrupted before its view's switch: the view
            // still sits detached at the graph's tip.
            let view = slot(top)?;
            occupied(top, &common, &common_dir, &view)?;
            reattach(
                top,
                &common_dir,
                &view,
                commit_of(top, GRAPH),
                dry_run,
                &mut notes,
            )?;
        }
        return finish(top, &common, &common_dir, &graph, &gone, dry_run, notes);
    };
    // Install's graph checkout is made on mdkg-graph only, so a custom
    // graph ref moves there first (#16358; 0.7.2): `mdkg-graph` is created
    // at its tip, and the custom ref is kept where it is. An existing
    // `mdkg-graph` at another commit is refused before anything changes.
    let custom = (name != GRAPH).then(|| name.clone());
    let created = match &custom {
        Some(custom) => graph_branch_for(top, custom)?,
        None => None,
    };
    // The graph's tip, read from the ref being rolled back: in a dry run
    // `mdkg-graph` may not exist yet.
    let tip = commit_of(top, custom.as_deref().unwrap_or(GRAPH));
    let view = slot(top)?;
    // Before any mode change: a switch or a new graph checkout on an
    // mdkg-graph checked out elsewhere would fail after it.
    occupied(top, &common, &common_dir, &view)?;
    if dry_run {
        if let Some(custom) = &custom {
            if let Some(tip) = &created {
                notes.push(format!(
                    "dry run: {GRAPH} would be created at {custom}'s tip {}",
                    short(tip)
                ));
            }
            notes.push(format!(
                "dry run: each mdkg.graph.<id>.ref naming {custom} would name {GRAPH}"
            ));
        }
        notes.push("dry run: mdkg.graph.ref would be unset".into());
        reattach(top, &common_dir, &view, tip, true, &mut notes)?;
        // The real run goes on with mdkg-graph, custom ref or not.
        return finish(top, &common, &common_dir, GRAPH, &gone, true, notes);
    }
    {
        let _lock = mdkg_graph::refwrite::GraphLock::exclusive(
            &common_dir,
            mdkg_graph::refwrite::LockMode::Local,
        )
        .map_err(|e| graph_error(e.code(), e.to_string()))?;
        if let (Some(custom), Some(tip)) = (&custom, &created) {
            // Create-only: an `mdkg-graph` made since the check is never
            // moved.
            git_ok(top, &["update-ref", GRAPH, tip, ""])?;
            notes.push(format!(
                "{GRAPH} created at {custom}'s tip {}: checkout mode's graph branch",
                short(tip)
            ));
        }
        // The records first: a rerun in checkout mode finds what's left.
        for key in &gone {
            common.unset(key)?;
            notes.push(format!(
                "{key} unset: its graph checkout (the view) is gone"
            ));
        }
        common.unset("mdkg.graph.ref")?;
        match &custom {
            Some(custom) => {
                notes.push(format!(
                    "mdkg.graph.ref unset: back in checkout mode on {GRAPH}; {custom} is kept \
                     and no longer moves"
                ));
                retarget_records(&common, custom, &mut notes)?;
            }
            None => notes.push(format!(
                "mdkg.graph.ref unset: back in checkout mode; {GRAPH} is kept"
            )),
        }
        // Last: a rerun in checkout mode makes this switch if it didn't
        // happen.
        reattach(top, &common_dir, &view, tip, false, &mut notes)?;
    }
    finish(top, &common, &common_dir, GRAPH, &[], false, notes)
}

/// Switches the managed view, a linked worktree of this repository
/// detached at the graph's commit `tip`, to mdkg-graph, so install
/// registers it as the graph checkout. Anything else (another clone in the
/// slot, a branch elsewhere: #18056 major 1) is left as it is. In a dry
/// run mdkg-graph may not exist yet; a real run's sits at `tip`.
fn reattach(
    top: &Path,
    common_dir: &Path,
    view: &Path,
    tip: Option<String>,
    dry_run: bool,
    notes: &mut Vec<String>,
) -> Result<(), GraphError> {
    let branch = GRAPH.strip_prefix("refs/heads/").unwrap_or(GRAPH);
    // The slot itself, never a link to another worktree (#18506 major 2).
    let linked =
        |p: &Path| std::fs::symlink_metadata(p).map_or(true, |m| m.file_type().is_symlink());
    if std::fs::symlink_metadata(view).is_err() {
        return Ok(());
    }
    // The link itself: the slot, or the folder holding it (#18724).
    let link = if linked(view) {
        Some(view)
    } else {
        view.parent().filter(|p| linked(p))
    };
    if let Some(link) = link {
        // Left alone, and install refuses a link there too: say how to
        // clear it (#18535), naming the link, so `rm` removes only it.
        notes.push(format!(
            "{} is a symlink, so {} isn't the managed view: it was left alone and no graph \
             checkout is made there; remove the link itself (`rm {}`, which keeps what it \
             points to) and run `{BIN} install --checkout-mode` again",
            link.display(),
            view.display(),
            shell_word(&link.to_string_lossy())
        ));
        return Ok(());
    }
    let detached_at_tip = tip.is_some()
        && admin_name(common_dir, view).is_some()
        && git_some(view, &["symbolic-ref", "-q", "HEAD"]).is_none()
        && commit_of(view, "HEAD") == tip
        && (dry_run || commit_of(top, GRAPH) == tip);
    if !detached_at_tip {
        return Ok(());
    }
    // The view is detached, so a checkout of mdkg-graph is elsewhere:
    // occupied() let it through as a registered graph checkout, which is
    // the graph checkout, and the view stays detached (#18506 major 1).
    if let Some(at) = checked_out(top, branch) {
        notes.push(format!(
            "{} stays detached: {branch} is checked out in the registered graph checkout {at}",
            view.display()
        ));
        return Ok(());
    }
    if dry_run {
        notes.push(format!(
            "dry run: {} would switch to {branch}",
            view.display()
        ));
    } else {
        git_ok(view, &["switch", "-q", branch])?;
        notes.push(format!(
            "{} is on {branch} again; `{BIN} install` registers it as the graph checkout",
            view.display()
        ));
    }
    Ok(())
}

/// Refuses, changing nothing, when mdkg-graph is checked out somewhere
/// other than the managed view or a registered graph checkout: checkout
/// mode's view switch, or install's new graph checkout, needs the branch.
fn occupied(top: &Path, common: &Common, common_dir: &Path, view: &Path) -> Result<(), GraphError> {
    let branch = GRAPH.strip_prefix("refs/heads/").unwrap_or(GRAPH);
    let Some(at) = checked_out(top, branch) else {
        return Ok(());
    };
    let same = matches!(
        (std::fs::canonicalize(view), std::fs::canonicalize(&at)),
        (Ok(a), Ok(b)) if a == b
    );
    // A `.worktree` record holds the checkout's admin name, not its path,
    // so a registered checkout moved with `git worktree move` still
    // matches (#18056 major 3).
    let registered = admin_name(common_dir, Path::new(&at))
        .is_some_and(|name| worktree_keys(common).iter().any(|(_, a)| *a == name));
    if same || registered {
        return Ok(());
    }
    Err(refused(
        "graph.ref_invalid",
        format!(
            "checkout mode's graph branch {GRAPH} is checked out at {at}, which isn't the graph \
             checkout; the rollback changed no mode or registration"
        ),
    )
    .with_hint(format!(
        "keep or commit the work in {at}, then free the branch there (`git -C {} switch \
         --detach`, or remove that worktree) and run `{BIN} install --checkout-mode` again",
        shell_word(&at)
    )))
}

/// The admin name (`<common>/worktrees/<name>`) of the linked worktree of
/// this repository whose top folder is `path`, or `None` for anything else:
/// another clone, the primary checkout, a plain folder.
fn admin_name(common_dir: &Path, path: &Path) -> Option<String> {
    let out = git_some(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--git-dir",
        ],
    )?;
    let mut lines = out.lines();
    let (top, common, dir) = (lines.next()?, lines.next()?, lines.next()?);
    let canon = |p: &str| std::fs::canonicalize(p).ok();
    if canon(top)? != std::fs::canonicalize(path).ok()?
        || canon(common)? != std::fs::canonicalize(common_dir).ok()?
    {
        return None;
    }
    let dir = canon(dir)?;
    if dir.parent()? != canon(common)?.join("worktrees") {
        return None;
    }
    Some(dir.file_name()?.to_str()?.to_owned())
}

/// The commit `mdkg-graph` must be created at for the custom graph ref
/// `custom` (its tip), or `None` when `mdkg-graph` is already there (a
/// rerun). Refuses, changing nothing, when `mdkg-graph` holds another
/// commit: its commits are someone's to keep or merge.
fn graph_branch_for(top: &Path, custom: &str) -> Result<Option<String>, GraphError> {
    let tip = commit_of(top, custom).ok_or_else(|| {
        refused(
            "graph.ref_missing",
            format!("the graph ref {custom} names no commit; the rollback changed nothing"),
        )
    })?;
    match commit_of(top, GRAPH) {
        None => {
            // Branches under `mdkg-graph/` (the custom ref among them)
            // keep git's files backend from making `mdkg-graph`.
            let under = git_some(
                top,
                &["for-each-ref", "--format=%(refname)", &format!("{GRAPH}/")],
            )
            .unwrap_or_default();
            match under.lines().next() {
                None => Ok(Some(tip)),
                Some(first) => Err(refused(
                    "graph.ref_invalid",
                    format!(
                        "checkout mode's graph branch is {GRAPH}, which git can't create while \
                         {first} exists under it; the rollback changed nothing"
                    ),
                )
                .with_hint(format!(
                    "rename the branches under {GRAPH}/ (`git branch -m`); if one is this \
                     clone's graph ref {custom}, set `mdkg.graph.ref` and each \
                     `mdkg.graph.<id>.ref` naming it to the new name, then run `{BIN} install \
                     --checkout-mode` again; or stay in ref mode"
                ))),
            }
        }
        Some(at) if at == tip => Ok(None),
        Some(at) => {
            // Moving a branch checked out in a worktree leaves its index
            // and files at the old commit, so a later commit there could
            // revert the graph (#17890 major 3): free it first.
            let branch = GRAPH.strip_prefix("refs/heads/").unwrap_or(GRAPH);
            let free = checked_out(top, branch)
                .map(|wt| {
                    format!(
                        "{GRAPH} is checked out at {wt}: keep or commit its work there, then free \
                         the branch (`git -C {} switch --detach`) before moving it; ",
                        shell_word(&wt)
                    )
                })
                .unwrap_or_default();
            Err(refused(
            "graph.ref_invalid",
            format!(
                "checkout mode's graph branch is {GRAPH}, which already exists at {} while this \
                 clone's graph ref {custom} is at {}; the rollback changed no mode or \
                 registration, and ref mode keeps working",
                short(&at),
                short(&tip)
            ),
        )
        .with_hint(format!(
            "{free}inspect mdkg-graph (`git log --oneline {tip}..{GRAPH}`) and keep or merge its \
             commits; once nothing on it is wanted, `git update-ref {GRAPH} {tip} {at}` points \
             it at the graph (refusing if it moved since), and `{BIN} install --checkout-mode` \
             runs again; or stay in ref mode"
        )))
        }
    }
}

/// Points each `mdkg.graph.<id>.ref` record naming `custom` at
/// `mdkg-graph`, as the moved graph's.
fn retarget_records(
    common: &Common,
    custom: &str,
    notes: &mut Vec<String>,
) -> Result<(), GraphError> {
    let Some(list) = git_some(
        &common.top,
        &[
            "config",
            "--file",
            &common.file,
            "--get-regexp",
            r"^mdkg\.graph\..*\.ref$",
        ],
    ) else {
        return Ok(());
    };
    for (key, value) in list.lines().filter_map(|l| l.split_once(' ')) {
        let full = graphref::graph_ref_name(value).ok();
        if full.as_deref() == Some(custom) {
            common.set(key, GRAPH)?;
            notes.push(format!("{key} now names {GRAPH}"));
        }
    }
    Ok(())
}

/// The rest of a rollback whose view is gone: unset `gone` (an
/// interrupted run's leftovers), then install's graph checkout path makes
/// and registers the replacement, which records every graph again.
fn finish(
    top: &Path,
    common: &Common,
    common_dir: &Path,
    graph: &str,
    gone: &[String],
    dry_run: bool,
    mut notes: Vec<String>,
) -> Result<Vec<String>, GraphError> {
    for key in gone {
        if dry_run {
            notes.push(format!(
                "dry run: {key} would be unset: its graph checkout (the view) is gone"
            ));
        } else {
            let _lock = mdkg_graph::refwrite::GraphLock::exclusive(
                common_dir,
                mdkg_graph::refwrite::LockMode::Local,
            )
            .map_err(|e| graph_error(e.code(), e.to_string()))?;
            common.unset(key)?;
            notes.push(format!(
                "{key} unset: its graph checkout (the view) is gone"
            ));
        }
    }
    let slot = slot(top)?;
    // A dry run counts the records it would unset as gone already, so it
    // names the replacement the real run makes.
    let unregistered = std::fs::symlink_metadata(&slot).is_err()
        && worktree_keys(common)
            .iter()
            .all(|(key, _)| dry_run && gone.contains(key));
    if unregistered && graph == GRAPH && dry_run {
        // Named, not planned: until the switch, mdkg-graph is the graph
        // ref, a protected branch install's create would refuse.
        notes.push(format!(
            "dry run: would create graph worktree: {} on mdkg-graph, registered as the graph \
             checkout (install's own path, with its prerequisites)",
            slot.display()
        ));
    } else if unregistered && graph == GRAPH {
        let plan =
            mdkg_run::install::worktree::GraphPlan::inspect(top, &crate::coord::main_ref(top))
                .map_err(crate::run::graph_error)?;
        let note = plan.create(dry_run).map_err(crate::run::graph_error)?;
        notes.push(if dry_run {
            format!("dry run: {note}")
        } else {
            note
        });
    } else if unregistered {
        // Install's graph checkout is made on `mdkg-graph` only.
        notes.push(format!(
            "no graph checkout is registered, and install makes one only for {GRAPH}, not \
             {graph}: graph reads refuse (graph.checkout_missing) until one is. To keep using \
             {graph}, go back to ref mode: `git config mdkg.graph.ref {}`, then `{BIN} \
             install --setup local`",
            shell_word(graph)
        ));
    }
    Ok(notes)
}

/// `s` as one shell word, for a command a hint shows (#16263).
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The managed `.worktrees/graph` slot, under the primary checkout as
/// install places it, whichever checkout runs the command (#16207).
fn slot(top: &Path) -> Result<PathBuf, GraphError> {
    let primary =
        mdkg_run::install::worktree::primary_checkout(top).map_err(crate::run::graph_error)?;
    Ok(primary
        .unwrap_or_else(|| top.to_path_buf())
        .join(".worktrees/graph"))
}

/// A `mdkg.graph.<id>.ref` record's value: the graph ref the clone used.
/// One naming a ref other than mdkg-graph comes first, so a move from a
/// custom ref whose retarget stopped part way (some records on mdkg-graph
/// already) is still found and finished (#17890 major 2).
fn recorded_ref(top: &Path, common: &Common) -> Option<String> {
    let list = git_some(
        &common.top,
        &[
            "config",
            "--file",
            &common.file,
            "--get-regexp",
            r"^mdkg\.graph\..*\.ref$",
        ],
    )?;
    let records: Vec<(&str, &str)> = list
        .lines()
        .filter_map(|l| {
            let (key, value) = l.split_once(' ')?;
            let id = key.strip_prefix("mdkg.graph.")?.strip_suffix(".ref")?;
            Some((id, value))
        })
        .collect();
    let at = commit_of(top, GRAPH);
    // The moved graph's own identity (committed on mdkg-graph) naming a
    // custom ref at mdkg-graph's commit; another graph's record naming an
    // old ref is never taken for it (#18056 major 2).
    let ids = match &at {
        Some(_) => mdkg_run::install::worktree::graph_ids_on(top, GRAPH).unwrap_or_default(),
        None => Vec::new(),
    };
    let moved = records.iter().find(|(id, value)| {
        let full = graphref::graph_ref_name(value).ok();
        full.as_deref() != Some(GRAPH)
            && ids.iter().any(|i| i == id)
            && full.is_some_and(|f| at.is_some() && commit_of(top, &f) == at)
    });
    let default = records
        .iter()
        .find(|(_, v)| graphref::graph_ref_name(v).ok().as_deref() == Some(GRAPH));
    moved
        .or(default)
        .or(records.first())
        .map(|(_, v)| (*v).to_owned())
}

/// Every `mdkg.graph.<id>.worktree` record, key and value exactly as
/// stored (no trimming).
fn worktree_keys(common: &Common) -> Vec<(String, String)> {
    let Ok(out) = git(
        &common.top,
        &[
            "config",
            "--file",
            &common.file,
            "--null",
            "--get-regexp",
            r"^mdkg\.graph\..*\.worktree$",
        ],
    ) else {
        return Vec::new();
    };
    out.stdout
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .filter_map(|e| {
            let e = std::str::from_utf8(e).ok()?;
            let (k, v) = e.split_once('\n')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

/// The `.worktree` records rollback may drop (#15944, one rule): the
/// record is the rolled-back graph's (its id is committed on `graph`),
/// and its checkout is provably gone: the admin folder, or the `.git` its
/// `gitdir` names (relative to the admin folder, as git resolves it), is
/// NotFound. Any other read error keeps the record, with a repair note.
fn gone_records(
    top: &Path,
    common: &Common,
    common_dir: &Path,
    graph: &str,
) -> Result<(Vec<String>, Vec<String>), GraphError> {
    let records = worktree_keys(common);
    if records.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ids = match commit_of(top, graph) {
        Some(_) => mdkg_run::install::worktree::graph_ids_on(top, graph)
            .map_err(crate::run::graph_error)?,
        None => Vec::new(),
    };
    let mut gone = Vec::new();
    let mut kept = Vec::new();
    let repair =
        |key: &str, why: String| format!("{key} kept: {why}; `{BIN} doctor --repair` checks it");
    for (key, admin) in records {
        let Some(id) = key
            .strip_prefix("mdkg.graph.")
            .and_then(|k| k.strip_suffix(".worktree"))
        else {
            continue;
        };
        if !ids.iter().any(|i| i == id) {
            continue;
        }
        let mut parts = Path::new(&admin).components();
        if !matches!(parts.next(), Some(std::path::Component::Normal(_))) || parts.next().is_some()
        {
            kept.push(repair(
                &key,
                format!("{admin:?} isn't a worktree admin name"),
            ));
            continue;
        }
        let folder = common_dir.join("worktrees").join(&admin);
        match std::fs::symlink_metadata(&folder) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                gone.push(key);
                continue;
            }
            Err(e) => {
                kept.push(repair(
                    &key,
                    format!("can't read {}: {e}", folder.display()),
                ));
                continue;
            }
            Ok(m) if !m.is_dir() => {
                kept.push(repair(&key, format!("{} isn't a folder", folder.display())));
                continue;
            }
            Ok(_) => {}
        }
        let file = folder.join("gitdir");
        let back = std::fs::symlink_metadata(&file).and_then(|m| {
            if m.is_file() && m.len() <= 1 << 20 {
                std::fs::read_to_string(&file)
            } else {
                Err(std::io::Error::other("not a plain file of at most 1 MiB"))
            }
        });
        let back = match back {
            Ok(b) => b,
            Err(e) => {
                kept.push(repair(&key, format!("can't read {}: {e}", file.display())));
                continue;
            }
        };
        let line = back.strip_suffix('\n').unwrap_or(&back);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let target = folder.join(line);
        match std::fs::symlink_metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => gone.push(key),
            Err(e) => kept.push(repair(
                &key,
                format!("can't read {}: {e}", target.display()),
            )),
            Ok(_) => {}
        }
    }
    Ok((gone, kept))
}

#[cfg(test)]
mod tests {
    use super::{
        SHAPE_ENTRIES, Shape, destination, entry_limit, full_shape, full_shape_within,
        lowered_limit, shape, shape_within,
    };

    fn shape_fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mdkg-shape-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).expect("fixture");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&dir)
                .status()
                .expect("git init")
                .success()
        );
        dir
    }

    fn object(dir: &std::path::Path, args: &[&str], input: &[u8]) -> String {
        let out = mdkg_git::command(dir)
            .args(args)
            .run(Some(input))
            .expect("git");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout)
            .expect("object id")
            .trim()
            .to_owned()
    }

    /// A small entry limit for the walk tests, so each trips it in seconds
    /// (F140); the shipped limit is [`SHAPE_ENTRIES`].
    const TEST_ENTRIES: usize = 1_000;

    /// The limit that ships, and that the debug-only override can only
    /// lower it: without the variable, or with a larger number, the walk
    /// uses the shipped limit.
    #[test]
    fn the_shipped_shape_entry_limit_is_100_000() {
        assert_eq!(SHAPE_ENTRIES, 100_000);
        assert_eq!(lowered_limit(None), SHAPE_ENTRIES);
        assert_eq!(lowered_limit(Some("500")), 500);
        assert_eq!(lowered_limit(Some("100000")), SHAPE_ENTRIES);
        assert_eq!(lowered_limit(Some("5000000")), SHAPE_ENTRIES);
        assert_eq!(lowered_limit(Some("lots")), SHAPE_ENTRIES);
        if std::env::var_os("MDKG_SHAPE_ENTRY_LIMIT").is_none() {
            assert_eq!(entry_limit(), SHAPE_ENTRIES);
        }
    }

    #[test]
    fn a_compact_tree_dag_is_refused_before_expanding_all_paths() {
        let dir = shape_fixture();
        let blob = object(&dir, &["hash-object", "-w", "--stdin"], b"x");
        let mut tree = object(
            &dir,
            &["mktree", "-z"],
            format!("100644 blob {blob}\tleaf\0").as_bytes(),
        );
        // Only 18 tiny tree objects, but a recursive listing expands to
        // 65,536 leaves and more than 100,000 visited entries. The walk
        // runs with a 1,000-entry limit (TEST_ENTRIES) so it trips in
        // seconds; the shipped limit is checked on its own.
        for _ in 0..16 {
            tree = object(
                &dir,
                &["mktree", "-z"],
                format!(
                    "040000 tree {tree}\ta\0\
                040000 tree {tree}\tb\0"
                )
                .as_bytes(),
            );
        }
        let root = object(
            &dir,
            &["mktree", "-z"],
            format!("040000 tree {tree}\t.mdkg\0").as_bytes(),
        );
        let err = full_shape_within(&dir, &root, &[".mdkg".into()], TEST_ENTRIES)
            .expect_err("repeated paths must consume the traversal budget");
        assert_eq!(err.diagnostic.code.as_str(), "graph.shape");
        assert!(err.diagnostic.message.contains("limit"), "{err:?}");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// A full shape found before the budget runs out is a full shape the
    /// rest of which wasn't checked, not an unclassifiable tree; one that
    /// runs out before anything outside the graph folders stays an error,
    /// and `full_shape` keeps refusing both (Terra #14932).
    #[test]
    fn a_full_shape_past_the_budget_is_full_unchecked() {
        let dir = shape_fixture();
        let blob = object(&dir, &["hash-object", "-w", "--stdin"], b"x");
        let mut tree = object(
            &dir,
            &["mktree", "-z"],
            format!("100644 blob {blob}\tleaf\0").as_bytes(),
        );
        for _ in 0..16 {
            tree = object(
                &dir,
                &["mktree", "-z"],
                format!("040000 tree {tree}\ta\0040000 tree {tree}\tb\0").as_bytes(),
            );
        }
        let graph = object(
            &dir,
            &["mktree", "-z"],
            format!("100644 blob {blob}\tconfig.json\0").as_bytes(),
        );
        // `.mdkg` and `README` sort before `src`: the shape is full before
        // the walk reaches the big subtree.
        let full = object(
            &dir,
            &["mktree", "-z"],
            format!(
                "040000 tree {graph}\t.mdkg\0100644 blob {blob}\tREADME\0040000 tree {tree}\tsrc\0"
            )
            .as_bytes(),
        );
        let paths = [".mdkg".to_string()];
        match shape_within(&dir, &full, &paths, TEST_ENTRIES).expect("classified") {
            Shape::FullUnchecked(why) => assert!(why.contains("limit"), "{why}"),
            other => panic!("{other:?}"),
        }
        full_shape_within(&dir, &full, &paths, TEST_ENTRIES).expect_err("full_shape still refuses");
        // The big subtree inside the graph folder: nothing outside it seen.
        let inside = object(
            &dir,
            &["mktree", "-z"],
            format!("040000 tree {tree}\t.mdkg\0").as_bytes(),
        );
        shape_within(&dir, &inside, &paths, TEST_ENTRIES).expect_err("graph-only can't be proved");
        // One tree over the 4 MiB per-tree cap after a full shape is found:
        // unchecked as well, not invalid (Terra #18260).
        let mut wide = Vec::new();
        for i in 0..60_000 {
            wide.extend_from_slice(format!("100644 blob {blob}\t{i:0>50}\0").as_bytes());
        }
        let wide = object(&dir, &["mktree", "-z"], &wide);
        let full_wide = object(
            &dir,
            &["mktree", "-z"],
            format!(
                "040000 tree {graph}\t.mdkg\0100644 blob {blob}\tREADME\0040000 tree {wide}\tsrc\0"
            )
            .as_bytes(),
        );
        match shape(&dir, &full_wide, &paths).expect("classified") {
            Shape::FullUnchecked(why) => assert!(why.contains("size limit"), "{why}"),
            other => panic!("{other:?}"),
        }
        // A small full shape is just full.
        let small = object(
            &dir,
            &["mktree", "-z"],
            format!("040000 tree {graph}\t.mdkg\0100644 blob {blob}\tREADME\0").as_bytes(),
        );
        assert_eq!(shape(&dir, &small, &paths).expect("full"), Shape::Full);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn a_failed_tree_listing_refuses_shape_classification() {
        let dir = std::env::temp_dir().join(format!("mdkg-shape-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).expect("fixture");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&dir)
                .status()
                .expect("git init")
                .success()
        );
        let err = full_shape(&dir, "missing-revision", &[".mdkg".into()])
            .expect_err("a failed listing isn't graph-only");
        assert_eq!(err.diagnostic.code.as_str(), "graph.shape");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn spellings_of_one_destination_agree() {
        for url in [
            "https://Example.invalid/org/app.git",
            "ssh://git@example.invalid:22/org/app/",
            "git@example.invalid:org/app.git",
            "example.invalid:org/app",
        ] {
            assert_eq!(destination(url), "example.invalid/org/app", "{url}");
        }
        assert_eq!(destination("/srv/app.git"), "/srv/app");
    }

    #[test]
    fn an_at_sign_in_the_path_stays_in_the_path() {
        // Terra-2 #13236: only the authority's `@` names a user.
        assert_eq!(
            destination("https://example.invalid/org/app@v2.git"),
            "example.invalid/org/app@v2"
        );
        assert_eq!(
            destination("ssh://git@example.invalid/org/app@v2.git"),
            "example.invalid/org/app@v2"
        );
        assert_eq!(
            destination("git@example.invalid:org/app@v2.git"),
            "example.invalid/org/app@v2"
        );
    }
}
