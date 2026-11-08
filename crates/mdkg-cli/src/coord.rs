//! Coordination commands: `claim`, `release`, `status`, `sync` and
//! `abandon`, on the shared store in the git common dir.
//!
//! Each command first observes main, so settlement is current, then runs one
//! coordinator command. The command defaults are:
//! - the actor is `--as`, then `$MDKG_ACTOR`, then the worktree's folder name;
//! - claiming a scope you already hold renews the lease (except a turn,
//!   which isn't re-entrant), and `renew` renews all of yours;
//! - leases last 2 hours (`$MDKG_LEASE_TTL_MS` when the store is created);
//! - a lease is exclusive unless `coordination.leases` in the config shares
//!   its path; node files are always exclusive;
//! - main is the `main` branch (`$MDKG_MAIN_REF`);
//! - `$MDKG_NOW_MS` pins the clock, for tests.

use std::path::{Path, PathBuf};

use mdkg_coord::{
    ChangesetId, Command, Coordinator, EntryState, Error as CoordError, Lease, LeaseId, Mode,
    Outcome, Scope, Settlement, Status,
};
use mdkg_coord_sqlite::Store;
use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::resolve::{Resolution, resolve_error, resolve_qid};
use mdkg_graph::{ErrorKind, GraphError, IndexNode, config, index};
use mdkg_index::resolve::{handle_error, resolve_noted};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::authority;
use crate::coord::graph_git::{GraphGit, RefMode, git};
use crate::output::{self, CoordinationRecord, LeaseBrief, PendingBrief, Report, rfc3339};
use mdkg_format::program::BIN;

// Declared here until `main.rs` is free to declare it (ref mode's seam,
// edd-01a18867); `crate::coord::graph_git`.
#[path = "graph_git.rs"]
pub(crate) mod graph_git;
#[path = "node_edit.rs"]
pub(crate) mod node_edit;

const DEFAULT_TTL_MS: u64 = 2 * 60 * 60 * 1000;

/// How long `status` keeps showing a lease that expired.
const EXPIRED_SHOWN_MS: u64 = 24 * 60 * 60 * 1000;

pub(crate) fn now_ms() -> u64 {
    if let Some(ms) = std::env::var("MDKG_NOW_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return ms;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

pub(crate) fn ttl_ms() -> u64 {
    std::env::var("MDKG_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TTL_MS)
}

/// The branch work settles on: `$MDKG_MAIN_REF`, then the repository's
/// `git config mdkg.mainRef` (shared by every worktree, never committed),
/// then `main`.
pub(crate) fn main_ref(top: &Path) -> String {
    git::main_ref(top)
}

pub(crate) fn git_error(e: std::io::Error) -> GraphError {
    GraphError::failure(Code::IoGitFailed, e.to_string())
}

/// The clone's store, opened one way for every caller (#14475): never past
/// a link to a store that isn't there, where SQLite would start an empty
/// one, and an upgrade says so.
pub(crate) fn store_for(top: &Path) -> Result<Store, GraphError> {
    let path = git::store_path(top).map_err(git_error)?;
    unreachable_store(&path)?;
    let store = Store::open_for_repo(top, ttl_ms()).map_err(store_error)?;
    upgraded(&store);
    Ok(store)
}

/// Says so when opening the store just upgraded it, with where the copy
/// of it as it was went; and when it just created it with a plain fsync
/// only (Terra #16506).
pub(crate) fn upgraded(store: &Store) {
    if store.created_weak() {
        crate::output::notice(
            Diagnostic::warning(
                Code::CoordStoreWeakSync,
                format!(
                    "the coordinator store was created with a plain fsync only ({}=1): this file \
                     system refused the full sync barrier, so the guarantee is weaker",
                    mdkg_coord_sqlite::PLAIN_FSYNC_ENV
                ),
            )
            .with_hint("keep the repository's git folder on a local disk for the full guarantee"),
        );
    }
    if let Some(copy) = store.backup() {
        // The override's weaker guarantee, which the upgraded store also
        // records as `meta.upgrade_barrier` (Terra's code review #5973,
        // finding 5).
        let weak = if store.backup_weak() {
            format!(
                ", synced with a plain fsync only ({}=1), a weaker guarantee than the full barrier",
                mdkg_coord_sqlite::PLAIN_FSYNC_ENV
            )
        } else {
            String::new()
        };
        crate::output::notice(
            Diagnostic::warning(
                Code::CoordStoreUpgraded,
                format!(
                    "the coordinator store was upgraded to this build's schema; a copy of it as it \
                     was is at {}{weak}",
                    copy.display()
                ),
            )
            .with_hint(format!(
                "older builds can't use it now; `{BIN} store restore <copy>` brings the copy \
                 back (stop every mdkg process first)"
            )),
        );
    }
}

pub(crate) fn store_error(e: mdkg_coord_sqlite::StoreError) -> GraphError {
    store_error_in(e, None)
}

/// [`store_error`] for the store a read with `--root root` opened
/// ([`Repo::command_root`]). A read that finds the store needs an upgrade or
/// crash recovery left it unchanged, in every mode; under --read-only that
/// is a refused write (`store.read_only`), as the flag documents.
///
/// Store errors name no command (#17360): the instruction is only in this
/// hint, which says what must happen and where, not a command to copy. A
/// copied command would run in the current folder's repository, and with
/// whatever build answers to the name, which needn't be one that can
/// upgrade this store (#17297). It names the graph root, not the git
/// checkout, since a graph may sit in a folder inside it (#17326), and
/// `the same --root` when the caller doesn't know it. `status` never
/// writes, so it is named as not enough.
pub(crate) fn store_error_in(e: mdkg_coord_sqlite::StoreError, root: Option<&Path>) -> GraphError {
    let read_only = mdkg_coord_sqlite::store_writes_forbidden();
    let place = root.map_or_else(
        || "the same --root".to_string(),
        |root| format!("--root {}", root.display()),
    );
    let (without, again) = if read_only {
        (" and without --read-only", "read with --read-only again")
    } else {
        ("", "retry this read")
    };
    let writer = match &e {
        mdkg_coord_sqlite::StoreError::ReadUpgradeRequired { .. } => Some(format!(
            "this store needs an upgrade (it is copied first), which only a writing command \
             does: run one with {place}{without} (`renew`, not `status`), with the installed \
             build for this store, then {again}"
        )),
        mdkg_coord_sqlite::StoreError::ReadAbsent => Some(format!(
            "only a writing command creates the store: run one with {place}{without} (`msg \
             inbox`, not `status`), then {again}"
        )),
        mdkg_coord_sqlite::StoreError::ReadRecoveryRequired(_) => Some(format!(
            "this store needs crash recovery, which only a writing command does: run one with \
             {place}{without} (`msg inbox`, not `status`), then {again}"
        )),
        _ => None,
    };
    if let Some(hint) = writer {
        let error = if read_only {
            GraphError::usage(Code::StoreReadOnly, e.to_string())
        } else {
            GraphError::failure(Code::CoordStoreFailed, e.to_string())
        };
        return error.with_hint(hint);
    }
    if matches!(e, mdkg_coord_sqlite::StoreError::WritesForbidden) {
        return GraphError::usage(Code::StoreReadOnly, e.to_string()).with_hint(
            "run it without --read-only to let it write; status, `msg inbox`, `msg wait` and \
             `msg thread` read without writing",
        );
    }
    let newer = matches!(e, mdkg_coord_sqlite::StoreError::Schema(_));
    let refused = matches!(e, mdkg_coord_sqlite::StoreError::UpgradeRefused { .. });
    let error = GraphError::failure(Code::CoordStoreFailed, e.to_string());
    if refused {
        error.with_hint(
            "use the installed mdkg; pass --allow-store-upgrade only for a scratch or test \
             store (it is copied first)",
        )
    } else if newer {
        error.with_hint(
            "a newer mdkg upgraded this store; use that build (see the release notes before reinstalling)",
        )
    } else {
        error
    }
}

/// The graph's place in its git repository.
pub(crate) struct Repo {
    pub(crate) root: PathBuf,
    /// The worktree's top folder; leaf paths are relative to it.
    pub(crate) top: PathBuf,
    /// The graph root's path inside the worktree (`""` or `sub/dir/`).
    pub(crate) prefix: String,
    /// The node folders' names and scopes, read from the config once.
    folders: std::cell::OnceCell<(Vec<String>, Vec<Scope>)>,
    /// Under `work.committed: none`, the store's work folder.
    store_work: std::cell::OnceCell<Option<PathBuf>>,
    /// The committed config [`crate::msg::committed_config`] reads, once
    /// per command (each read asks git for the repository's folders).
    pub(crate) committed: std::cell::OnceCell<serde_json::Value>,
    /// Each store-folder node file's UUID, read once per command;
    /// `None` for a file that names no id.
    store_ids: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<String>>>,
    /// Ref mode: the graph is the graph ref, read from the cache (`root`),
    /// and `top` is the invoking checkout, for code paths and the store.
    pub(crate) mode: Option<std::sync::Arc<RefMode>>,
}

/// Each graph root `Repo::open` checked in this process, by its canonical
/// path, with the worktree top and prefix the checks found and what they
/// saw on disk (#53). A command opens its repository several times; the
/// checks run on the first, and every git command after it names the
/// folders they found outright (git's own cache of checked worktrees). A
/// later open is served from here only while everything that decides which
/// repository the root belongs to is unchanged ([`Seen`]); anything else
/// runs every check again, which refreshes git's cache too. A root that
/// can't be canonicalized is always checked, and a failed check is never
/// kept.
static OPENED: std::sync::Mutex<Vec<(PathBuf, Opened)>> = std::sync::Mutex::new(Vec::new());

struct Opened {
    trust: git::WorktreeTrust,
    top: PathBuf,
    prefix: String,
    seen: Seen,
    at: std::time::Instant,
}

/// How long a kept check serves: a command's opens, not a long-running
/// process's, which checks again as it did before #53. It also bounds what
/// the fingerprint can't see: the system config (root's to change).
const KEPT_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// The most of a config file read to look for includes.
const CONFIG_CAP: u64 = 1 << 20;

/// A file or folder as found: device and inode, and for a file its size
/// and change time, so the same name holding something else, or the same
/// file rewritten, differs. A folder's own size and times change whenever
/// an entry in it does, which says nothing about what it is.
type Stamp = (u64, u64, u64, i64, i64);

/// What decides which repository a graph root belongs to, as on disk: every
/// folder from the root up to `/` with its `.git` entry (or none), and,
/// for the nearest `.git`, the git and common folders it leads to and the
/// repository's config files (which name hooks).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// Each folder, root first.
    path: Vec<Step>,
    /// Where the nearest `.git` leads.
    repo: Option<Leads>,
}

/// A folder on the way up from the root, and its `.git` if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    dir: Stamp,
    git: Option<GitEntry>,
}

/// A `.git` entry: its stamp, and what a `.git` file says or a `.git` link
/// points at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitEntry {
    stamp: Stamp,
    says: Option<String>,
}

/// The git folder and common folder a `.git` leads to (canonical, with
/// stamps), and the stamps of the repository's config files.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leads {
    git_dir: PathBuf,
    git_dir_stamp: Stamp,
    common: PathBuf,
    common_stamp: Stamp,
    configs: Vec<Option<Stamp>>,
}

#[cfg(unix)]
fn stamp(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    if meta.is_dir() {
        (meta.dev(), meta.ino(), 0, 0, 0)
    } else {
        (
            meta.dev(),
            meta.ino(),
            meta.size(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    }
}

/// What a `.git` entry says: a file's text, or a link's target.
fn says(entry: &Path, meta: &std::fs::Metadata) -> Option<String> {
    if meta.file_type().is_symlink() {
        std::fs::read_link(entry)
            .ok()
            .map(|t| t.display().to_string())
    } else if meta.is_file() {
        git::read_small(entry).ok()
    } else {
        None
    }
}

/// The git folder a `.git` entry leads to: the folder a `.git` folder or
/// link resolves to, or the one a `.git` file names.
fn leads_to(entry: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(entry).ok()?;
    if meta.is_file() {
        let text = git::read_small(entry).ok()?;
        let dir = text
            .trim_end_matches(['\n', '\r'])
            .strip_prefix("gitdir: ")?;
        std::fs::canonicalize(entry.parent()?.join(dir)).ok()
    } else {
        std::fs::canonicalize(entry).ok()
    }
}

#[cfg(unix)]
fn seen(root: &Path) -> Option<Seen> {
    let mut path = Vec::new();
    let mut nearest = None;
    for dir in root.ancestors() {
        let entry = dir.join(".git");
        let git = match std::fs::symlink_metadata(&entry) {
            Ok(meta) => {
                nearest.get_or_insert_with(|| entry.clone());
                Some(GitEntry {
                    stamp: stamp(&meta),
                    says: says(&entry, &meta),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        path.push(Step {
            dir: stamp(&std::fs::metadata(dir).ok()?),
            git,
        });
    }
    let repo = nearest.and_then(|entry| {
        let git_dir = leads_to(&entry)?;
        let common = match git::read_small(&git_dir.join("commondir")) {
            Ok(text) => {
                std::fs::canonicalize(git_dir.join(text.trim_end_matches(['\n', '\r']))).ok()?
            }
            Err(_) => git_dir.clone(),
        };
        let configs = config_stamps(&common, &git_dir)?;
        Some(Leads {
            git_dir_stamp: stamp(&std::fs::metadata(&git_dir).ok()?),
            common_stamp: stamp(&std::fs::metadata(&common).ok()?),
            git_dir,
            common,
            configs,
        })
    });
    Some(Seen { path, repo })
}

/// The config files git reads hooks from that a fingerprint can stamp: the
/// repository's, the worktree's, the user's, and the system's as
/// `$GIT_CONFIG_SYSTEM` names it (default `/etc/gitconfig`). `None` when
/// one includes another file (git would read that too), can't be read
/// whole, or a variable naming one is set but empty: then nothing is
/// kept, and every open checks. A git build's own defaults file (Apple's
/// `share/git-core/gitconfig`) belongs to whoever installed git, and
/// [`KEPT_FOR`] bounds it as git's hook discovery itself does between its
/// read and a later git command.
#[cfg(unix)]
fn config_stamps(common: &Path, git_dir: &Path) -> Option<Vec<Option<Stamp>>> {
    use std::os::unix::fs::FileTypeExt;
    // A variable set to nothing: what git makes of it isn't ours to guess.
    let named = |var: &str| match std::env::var_os(var) {
        Some(v) if v.is_empty() => Err(()),
        v => Ok(v.map(PathBuf::from)),
    };
    let mut files = vec![common.join("config"), git_dir.join("config.worktree")];
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    match named("GIT_CONFIG_GLOBAL").ok()? {
        Some(file) => files.push(file),
        None => {
            files.extend(home.as_ref().map(|h| h.join(".gitconfig")));
            // An empty XDG_CONFIG_HOME is unset, to git.
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|x| !x.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".config")));
            files.extend(xdg.map(|x| x.join("git").join("config")));
        }
    }
    // Git reads it as a boolean: only a true one skips the system config.
    let no_system = std::env::var("GIT_CONFIG_NOSYSTEM")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    if !no_system {
        files.push(
            named("GIT_CONFIG_SYSTEM")
                .ok()?
                .unwrap_or_else(|| "/etc/gitconfig".into()),
        );
    }
    files
        .iter()
        .map(|file| match std::fs::metadata(file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
            // `/dev/null` (as `GIT_CONFIG_GLOBAL` often is) reads as
            // empty; its times change with every write to it, so it's
            // stamped by identity only.
            Ok(meta) if meta.file_type().is_char_device() => {
                use std::os::unix::fs::MetadataExt;
                Some(Some((meta.dev(), meta.ino(), 0, 0, 0)))
            }
            Ok(meta) => {
                let text = git::read_capped(file, CONFIG_CAP + 1).ok()?;
                let includes = String::from_utf8_lossy(&text)
                    .to_ascii_lowercase()
                    .contains("include");
                (text.len() as u64 <= CONFIG_CAP && !includes).then(|| Some(stamp(&meta)))
            }
        })
        .collect()
}

/// Without inodes nothing can tell a folder replaced at the same path, so
/// nothing is kept: every open checks.
#[cfg(not(unix))]
fn seen(_root: &Path) -> Option<Seen> {
    None
}

impl Repo {
    pub(crate) fn open(root: &Path) -> Result<Self, GraphError> {
        // Ref mode: the root `bind` handed the command is the cache, which
        // no git check applies to; its checkout was checked when bound.
        if let Some(mode) = graph_git::for_root(root) {
            return Ok(Self {
                root: mode.root().to_path_buf(),
                top: mode.checkout.clone(),
                prefix: String::new(),
                folders: std::cell::OnceCell::new(),
                store_work: std::cell::OnceCell::new(),
                committed: std::cell::OnceCell::new(),
                store_ids: Default::default(),
                mode: Some(mode),
            });
        }
        let trust = git::WorktreeTrust::current();
        let Ok(canonical) = std::fs::canonicalize(root) else {
            return Ok(Self::check(root)?.0);
        };
        let before = seen(&canonical);
        let kept = OPENED.lock().ok().and_then(|opened| {
            opened
                .iter()
                .find(|(r, o)| {
                    *r == canonical
                        && o.trust == trust
                        && o.at.elapsed() < KEPT_FOR
                        && before.as_ref() == Some(&o.seen)
                })
                .map(|(_, o)| (o.top.clone(), o.prefix.clone()))
        });
        if let Some((top, prefix)) = kept {
            return Ok(Self {
                root: root.to_path_buf(),
                top,
                prefix,
                folders: std::cell::OnceCell::new(),
                store_work: std::cell::OnceCell::new(),
                committed: std::cell::OnceCell::new(),
                store_ids: Default::default(),
                mode: None,
            });
        }
        let (repo, git_dir) = Self::check(root)?;
        // Kept only if nothing changed while the checks ran, and the nearest
        // `.git` leads to the git folder they trusted: so what's kept is
        // what was checked.
        let after = seen(&canonical);
        let checked = after
            .as_ref()
            .and_then(|s| s.repo.as_ref())
            .is_some_and(|leads| leads.git_dir == git_dir);
        if let (true, Some(seen), Ok(mut opened)) =
            (checked && before == after, after, OPENED.lock())
        {
            opened.retain(|(r, _)| *r != canonical);
            opened.push((
                canonical,
                Opened {
                    trust,
                    top: repo.top.clone(),
                    prefix: repo.prefix.clone(),
                    seen,
                    at: std::time::Instant::now(),
                },
            ));
        }
        Ok(repo)
    }

    /// Every check `open` makes, run now, and the git folder they trusted.
    fn check(root: &Path) -> Result<(Self, PathBuf), GraphError> {
        let top = git::toplevel(root).map_err(|e| {
            git_error(e).with_hint("coordination needs the graph to be inside a git repository")
        })?;
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        // Git skips a `.git` it can't use and finds a repository further
        // up: every command refuses when the skipped one may hold this
        // graph's store, so nothing is granted in the wrong repository.
        let top_canonical = std::fs::canonicalize(&top).unwrap_or_else(|_| top.clone());
        if let Some(entry) = skipped_store(&canonical, &top_canonical) {
            return Err(GraphError::failure(
                Code::CoordStoreFailed,
                format!(
                    "git skipped {}, which may hold this graph's coordinator store",
                    entry.display()
                ),
            )
            .with_hint(if dangling(&entry) {
                format!(
                    "{} links to {}, which is gone (a disk that isn't mounted?): restore the target",
                    entry.display(),
                    std::fs::read_link(&entry)
                        .map(|t| t.display().to_string())
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "git can't use {} (its HEAD, objects or refs are missing, or it can't be \
                     read): restore it or fix its permissions; remove it only if it holds no \
                     mdkg/coord.sqlite",
                    entry.display()
                )
            }));
        }
        // Checked once, then named outright to every git command, so an
        // agent can't swap where `.git` leads in between. A refusal is a
        // store failure: nobody may act or read leases as if none were held.
        let checkout = git::check_worktree(root).map_err(|e| {
            let message = e.to_string();
            let hint = if message.contains("this Git has no --attr-source") {
                git::upgrade_hint()
            } else {
                "the worktree's .git must link to its repository's git folder (see `git worktree repair`)"
                    .to_string()
            };
            GraphError::failure(
                Code::CoordStoreFailed,
                format!("won't trust this worktree's git link: {message}"),
            )
            .with_hint(hint)
        })?;
        // The top every later git command uses is the one checked, never
        // the earlier unchecked reading (a `.git` swapped in between).
        if checkout.top != top_canonical {
            return Err(GraphError::failure(
                Code::CoordStoreFailed,
                format!(
                    "won't trust this worktree's git link: git placed it at {} and then at {}",
                    top_canonical.display(),
                    checkout.top.display()
                ),
            )
            .with_hint("is something rewriting .git? Run the command again"));
        }
        let git_dir = checkout.git_dir;
        let top = checkout.top;
        let prefix = canonical
            .strip_prefix(&top)
            .map(|p| {
                let s = p.to_string_lossy().replace('\\', "/");
                if s.is_empty() { s } else { format!("{s}/") }
            })
            .unwrap_or_default();
        Ok((
            Self {
                root: root.to_path_buf(),
                top,
                prefix,
                folders: std::cell::OnceCell::new(),
                store_work: std::cell::OnceCell::new(),
                committed: std::cell::OnceCell::new(),
                store_ids: Default::default(),
                mode: None,
            },
            git_dir,
        ))
    }

    pub(crate) fn scope_for(&self, graph_path: &str) -> Scope {
        if let Some(scope) = self.store_scope(graph_path) {
            return scope;
        }
        Scope::new(format!("{}{graph_path}", self.prefix))
    }

    /// Under profile C, a node file in the store's work folder is leased
    /// and recorded as `node:<uuid>`, its id's UUID (format 3.1, 9.1):
    /// a name, the same from every worktree and under any file
    /// name or type, so a rename or a retype keeps its lease and states, and
    /// never a path git settles. The only place such a scope is built. A
    /// file there that names no id gets the empty `node:`, which
    /// [`Repo::leaf`] never leases.
    pub(crate) fn store_scope(&self, graph_path: &str) -> Option<Scope> {
        if !mdkg_graph::store_work::is_store_path(graph_path) {
            return None;
        }
        // Only a file in this graph's store work folder: any other absolute
        // path is a path, never a node's name.
        let dir = self
            .store_work
            .get_or_init(|| config::load(&self.root).ok().and_then(|c| c.store_work_dir))
            .as_ref()?;
        let full = self.root.join(graph_path);
        if !full.starts_with(dir) {
            return None;
        }
        let uuid = self
            .store_ids
            .borrow_mut()
            .entry(full.clone())
            .or_insert_with(|| store_node_uuid(&full, graph_path))
            .clone();
        Some(Scope::new(format!(
            "{STORE_SCOPE_PREFIX}{}",
            uuid.unwrap_or_default()
        )))
    }

    /// How a command names `scope` from this graph: its path relative to the
    /// graph root, or a role or turn as it is.
    /// Spelled so it resolves back to exactly `scope`, relative to the graph
    /// root: `./<rest>` inside it (never a bare word, which could read as a
    /// node id or a role), `.` for the root itself, `../…` outside it (no
    /// machine-specific absolute path in a hint).
    pub(crate) fn target_of(&self, scope: &Scope) -> String {
        let s = scope.as_str();
        if is_named_scope(scope) {
            return s.to_string();
        }
        let root = self.prefix.trim_end_matches('/');
        let depth = if root.is_empty() {
            0
        } else {
            root.split('/').count()
        };
        let inside = root.is_empty() || Scope::new(root).contains(scope);
        if !inside {
            // Up to the folders the root and the scope share, then down.
            let ups: Vec<&str> = root.split('/').collect();
            let downs: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
            let shared = ups
                .iter()
                .zip(&downs)
                .take_while(|(a, b)| Scope::new(**a) == Scope::new(**b))
                .count();
            let mut parts: Vec<&str> = vec![".."; ups.len() - shared];
            parts.extend(&downs[shared..]);
            return parts.join("/");
        }
        let rest: Vec<&str> = s.split('/').skip(depth).collect();
        if rest.is_empty() || rest.iter().all(|r| r.is_empty()) {
            ".".to_string()
        } else {
            format!("./{}", rest.join("/"))
        }
    }

    /// A node's scope for a lease or a record: refused when its first
    /// segment starts like a role or a turn (a graph or workspace in a
    /// folder named `run:x`), whose leases would overlap the named one.
    pub(crate) fn leaf(&self, graph_path: &str) -> Result<Scope, GraphError> {
        if let Some(scope) = self.store_scope(graph_path) {
            if scope.as_str() == STORE_SCOPE_PREFIX {
                return Err(GraphError::failure(
                    Code::NodeBadId,
                    format!("{graph_path} names no node id, so it can't be leased"),
                )
                .with_hint("give the file an `id:`, or remove it from the store's work folder"));
            }
            return Ok(scope);
        }
        let scope = self.scope_for(graph_path);
        if aliases_named(scope.as_str()) {
            return Err(named_folder(scope.as_str()));
        }
        Ok(scope)
    }

    pub(crate) fn graph_scope(&self) -> Scope {
        self.scope_for(".mdkg")
    }

    /// Whether `scope` is a whole graph: this graph's `.mdkg` folder or any
    /// folder holding it, or any `.mdkg` folder (another graph's, or a
    /// workspace's): one lease on it would block everyone.
    fn whole_graph(&self, scope: &Scope) -> bool {
        scope.as_str().is_empty()
            || scope.contains(&self.graph_scope())
            || scope
                .as_str()
                .rsplit('/')
                .next()
                .is_some_and(|p| p.eq_ignore_ascii_case(".mdkg"))
    }

    /// Whether a lease on `target` would hold node folders wholesale: a
    /// folder (or a path that may become one) that is, holds or sits in
    /// any `.mdkg` folder or a workspace's node folder. A file inside one
    /// (a node, the config) is fine. Checked on the scope granted, after
    /// links resolve, so a target flipped to a link between a caller's
    /// check and this one is caught.
    fn node_folder_lease(&self, target: &Target) -> Result<bool, GraphError> {
        // A role or a turn is a name, never a folder.
        if is_named_scope(&target.scope) {
            return Ok(false);
        }
        let s = target.scope.as_str().to_lowercase();
        // As the disk spells it: scopes ignore case, a case-sensitive disk
        // doesn't.
        let spelled = self.on_disk(target.scope.as_str());
        let on_disk = self.disk_base(&spelled).join(&spelled);
        // Node folders of this graph and of every graph above the scope.
        let mut folders: Vec<Scope> = self.node_folders().1.clone();
        let parts: Vec<&str> = spelled.split('/').collect();
        for i in 0..parts.len() {
            let dir = parts[..i].join("/");
            if self
                .disk_base(&dir)
                .join(&dir)
                .join(config::CONFIG_PATH)
                .is_file()
            {
                folders.extend(self.nested(&dir).node_folders().1.iter().cloned());
            }
        }
        // Never a scope that is or holds a node folder, whatever it looks
        // like or whether it exists yet, including dotted folder names
        // such as `w/docs.v1`.
        if s.rsplit('/').next() == Some(".mdkg") || folders.iter().any(|f| target.scope.contains(f))
        {
            return Ok(true);
        }
        // A path that doesn't exist yet is a file unless it was named as a
        // folder (a trailing `/`); inside a node folder, a dotless name is
        // a folder too (`work`, `design`), and a dotfile or a dotted name a
        // file (`.gitignore`, a node).
        let inside =
            s.split('/').any(|p| p == ".mdkg") || folders.iter().any(|f| f.contains(&target.scope));
        let last = s.rsplit('/').next().unwrap_or(&s);
        let missing_file = !inside || last.ends_with(".md") || last.starts_with('.');
        let file = !target.folder && (on_disk.is_file() || (!on_disk.exists() && missing_file));
        if file {
            return Ok(false);
        }
        // A folder inside a node folder.
        if inside {
            return Ok(true);
        }
        // A graph below it, anywhere: its node folders are inside.
        if on_disk.join(config::CONFIG_PATH).is_file() {
            return Ok(true);
        }
        let files = if self.mode.as_ref().is_some_and(|m| m.holds(&spelled)) {
            self.graph_git().files_under(&spelled)
        } else {
            git::files_under(&self.top, &spelled)
        }
        .map_err(git_error)?;
        Ok(files.iter().any(|f| {
            let f = f.to_lowercase();
            f == config::CONFIG_PATH || f.ends_with(&format!("/{}", config::CONFIG_PATH))
        }))
    }

    /// Whether a claim of `scope` would still be granted by this build's
    /// rules on text, whole graphs and node folders: `renew` lets a lease
    /// an older build granted otherwise lapse.
    pub(crate) fn lease_still_granted(&self, scope: &Scope) -> bool {
        if is_named_scope(scope) {
            return true;
        }
        let target = Target {
            scope: scope.clone(),
            label: String::new(),
            missing: false,
            folder: {
                let spelled = self.on_disk(scope.as_str());
                self.disk_base(&spelled).join(&spelled).is_dir()
            },
            node: None,
            qid: None,
            node_key: None,
        };
        check_scope_text(scope).is_ok()
            && !self.whole_graph(scope)
            // A listing that fails says nothing about the lease: kept.
            && !self.node_folder_lease(&target).unwrap_or(false)
    }

    /// Another graph in this worktree, its root at `dir` (from the top):
    /// it shares this one's top, so deriving each node's lease key needs
    /// no git call.
    fn nested(&self, dir: &str) -> Repo {
        Repo {
            root: self.disk_base(dir).join(dir),
            top: self.top.clone(),
            prefix: if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            },
            folders: std::cell::OnceCell::new(),
            store_work: std::cell::OnceCell::new(),
            committed: std::cell::OnceCell::new(),
            store_ids: Default::default(),
            mode: self.mode.clone(),
        }
    }

    /// The seam every graph-file git question goes through: the cache and
    /// its commit in ref mode, the checkout and its HEAD otherwise.
    pub(crate) fn graph_git(&self) -> GraphGit<'_> {
        match &self.mode {
            Some(mode) => GraphGit::Ref { mode },
            None => GraphGit::Checkout { top: &self.top },
        }
    }

    /// Where repository-relative `rel` is on disk: in ref mode, a graph
    /// path is in the cache, which mirrors the repository's layout; any
    /// other path is in the checkout.
    pub(crate) fn disk_base(&self, rel: &str) -> &Path {
        match &self.mode {
            Some(mode) if mode.holds(rel) => mode.root(),
            _ => &self.top,
        }
    }

    /// `rel` (from the repository's top) as the disk spells it: each
    /// existing segment matched in any letter case, the rest as given.
    fn on_disk(&self, rel: &str) -> String {
        let mut here = self.disk_base(rel).to_path_buf();
        let mut out: Vec<String> = Vec::new();
        let mut parts = rel.split('/').filter(|p| !p.is_empty()).peekable();
        while let Some(part) = parts.next() {
            let found = if here.join(part).exists() {
                Some(part.to_string())
            } else {
                std::fs::read_dir(&here).ok().and_then(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .find(|name| name.eq_ignore_ascii_case(part))
                })
            };
            match found {
                Some(name) => {
                    here.push(&name);
                    out.push(name);
                }
                None => {
                    out.push(part.to_string());
                    out.extend(parts.map(String::from));
                    break;
                }
            }
        }
        out.join("/")
    }

    /// The graph that has `scope` as a node file: the nearest graph root
    /// above it (this graph's included) whose node folders hold it, `None`
    /// for this graph or when none does. In a repository with several
    /// graphs it may not be this one.
    fn graph_of(&self, scope: &Scope) -> Result<Option<Repo>, GraphError> {
        let spelled = self.on_disk(scope.as_str());
        let parts: Vec<&str> = spelled.split('/').collect();
        for i in (0..parts.len()).rev() {
            let dir = parts[..i].join("/");
            if dir.eq_ignore_ascii_case(self.prefix.trim_end_matches('/')) {
                if self.node_file(scope) {
                    return Ok(None);
                }
                continue;
            }
            if self
                .disk_base(&dir)
                .join(&dir)
                .join(config::CONFIG_PATH)
                .is_file()
            {
                let graph = self.nested(&dir);
                if graph.node_file(scope) {
                    return Ok(Some(graph));
                }
            }
        }
        Ok(None)
    }

    /// The node folders, by folder name (lowercase) and by scope: this
    /// graph's `.mdkg` and each workspace's (`.` is the graph root's own),
    /// the lease-key input. The workspaces are the graph ref's committed
    /// config's, the list the gate reads a node's leases by, never the
    /// checkout's own edit (Terra's code review #10490, the major; #6919
    /// call 2); the checkout's config only where no graph-ref config
    /// resolves ([`crate::entry::graph_ref_workspaces`], a residual the
    /// edd names). A config that can't be read gives this graph's `.mdkg`
    /// alone.
    fn node_folders(&self) -> &(Vec<String>, Vec<Scope>) {
        self.folders.get_or_init(|| {
            let mut names = vec![".mdkg".to_string()];
            let mut folders = vec![self.graph_scope()];
            let workspaces = crate::entry::graph_ref_workspaces(self).unwrap_or_else(|| {
                config::load(&self.root)
                    .map(|c| c.workspaces)
                    .unwrap_or_default()
            });
            for w in workspaces {
                if let Some(name) = w.mdkg_dir.trim_matches('/').rsplit('/').next() {
                    names.push(name.to_lowercase());
                }
                folders.push(self.scope_for(&workspace_folder(&w)));
            }
            (names, folders)
        })
    }

    /// The node folder holding `scope`, the innermost if folders nest.
    fn node_folder_of(&self, scope: &Scope) -> Option<&Scope> {
        innermost_node_folder(&self.node_folders().1, scope)
    }

    /// The key a node's lease is held under: the node folder holding its
    /// file and its bare id, so every graph that opens the file keys it
    /// the same (ids repeat across graphs and workspaces).
    pub(crate) fn node_key(&self, scope: &Scope, qid: &str) -> String {
        // In the graph that has the file as a node, so every graph keys it
        // the same, even where one graph's workspace sits in another's.
        let other = self.graph_of(scope).ok().flatten();
        let graph = other.as_ref().unwrap_or(self);
        let folder = graph
            .node_folder_of(scope)
            .cloned()
            .unwrap_or_else(|| graph.graph_scope());
        let id = qid.split_once(':').map_or(qid, |(_, id)| id);
        format!(
            "{}#{}",
            folder.as_str().to_lowercase(),
            id.to_ascii_lowercase()
        )
    }

    /// Whether `scope` may be a node's file: a `.md` file in a node folder.
    fn node_file(&self, scope: &Scope) -> bool {
        scope.as_str().to_ascii_lowercase().ends_with(".md") && self.node_folder_of(scope).is_some()
    }

    pub(crate) fn store(&self) -> Result<Store, GraphError> {
        let mut store = store_for(&self.top)?;
        // Acked mail stays readable for the configured grace whatever
        // command acks it (friction item 72). The mail commands that ack or
        // send refuse a bad value; elsewhere it falls back to the default.
        store.set_ack_grace_days(
            crate::msg::committed_config(self)
                .and_then(|config| crate::msg::ack_grace_days(&config))
                .unwrap_or(crate::msg::DEFAULT_ACK_GRACE_DAYS),
        );
        Ok(store)
    }

    /// Brings the coordinator up to date before a command acts on it:
    /// applies what main holds, so settlement is current, then expires leases
    /// past their deadline, so a lease that ran out never blocks a claim, a
    /// `done` or its owner's fresh claim.
    /// `now` is the command's one clock reading, so a lease judged live here
    /// is still live when the command acts.
    pub(crate) fn observe(&self, store: &mut Store, now: u64) -> Result<(), GraphError> {
        self.observe_main(store)?;
        self.expire_due(store, now)
    }

    /// Expires leases past their deadline: all a role or a turn needs,
    /// since neither depends on main.
    pub(crate) fn expire_due(&self, store: &mut Store, now: u64) -> Result<(), GraphError> {
        let due = store
            .coordinator()
            .map_err(store_error)?
            .leases()
            .any(|l| l.confirmed && l.status == Status::Active && now >= l.deadline);
        if due {
            store
                .execute(&Command::ExpireDue { now })
                .map_err(store_error)?;
        }
        Ok(())
    }

    fn observe_main(&self, store: &mut Store) -> Result<(), GraphError> {
        let Some(main) = self.observation_ref()? else {
            return Ok(());
        };
        store
            .observe_main(&self.top, &main, &self.graph_scope().to_string())
            .map(drop)
            .map_err(store_error)
    }

    /// The ref both writers and status observe. Resolving it runs no store IO.
    fn observation_ref(&self) -> Result<Option<String>, GraphError> {
        // Ref mode: the graph ref wins (G5). It was checked when bound.
        if let Some(mode) = &self.mode {
            return Ok(Some(mode.graph.name.clone()));
        }
        let main = main_ref(&self.top);
        // A repository with no commit yet: main is unborn and holds
        // nothing, so there's nothing to observe, and work goes on before
        // the first commit. A missing main beside other branches is still
        // a misnamed one.
        if !git::has_ref(&self.top, &main) && git::no_commits(&self.top).map_err(git_error)? {
            return Ok(None);
        }
        if !git::has_ref(&self.top, &main) {
            return Err(GraphError::failure(
                Code::IoGitFailed,
                format!("main branch {main} not found"),
            )
            .with_hint("set it with `git config mdkg.mainRef <branch>` (or $MDKG_MAIN_REF)"));
        }
        Ok(Some(main))
    }

    /// The actor a worktree names by default, its folder's name, with no
    /// look at `$MDKG_ACTOR` (a Codex hook in a shared daemon).
    pub(crate) fn folder_actor(&self) -> String {
        self.top
            .file_name()
            .map_or("agent".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Where a command must run to act on this repository's graph and
    /// store, as a `--read-only` refusal names it: the graph root in
    /// checkout mode, which may be a folder inside the git checkout, and
    /// the checkout in ref mode, whose root is the ref's cache (#17326).
    pub(crate) fn command_root(&self) -> &Path {
        if self.mode.is_some() {
            &self.top
        } else {
            &self.root
        }
    }

    pub(crate) fn actor(&self, flag: Option<String>) -> String {
        flag.or_else(|| std::env::var("MDKG_ACTOR").ok())
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| {
                self.top
                    .file_name()
                    .map_or("agent".into(), |n| n.to_string_lossy().into_owned())
            })
    }

    /// A node id, or a file or folder path, as a scope and a label for
    /// messages.
    pub(crate) fn resolve(&self, target: &str) -> Result<Target, GraphError> {
        // Refusals below may quote the target: never hidden characters.
        refuse_hidden(target)?;
        // Named scopes (M1), held as leases on their own names: a role
        // mailbox (`role:<name>`), and an agent's turn the launcher holds
        // (`run:<name>`, no mailbox). Names are lowercase and may contain
        // `:` (`run:codex:pr19`); any other case is refused, so no
        // spelling can squat on one.
        for kind in ["role", "run"] {
            let prefix = format!("{kind}:");
            if !target
                .get(..prefix.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(&prefix))
            {
                continue;
            }
            let name = target.get(prefix.len()..).unwrap_or_default();
            let noun = if kind == "run" { "turn" } else { "role" };
            if !target.starts_with(&prefix) || !crate::msg::is_lower_name(name) {
                return Err(GraphError::usage(
                    Code::MsgBadId,
                    format!(
                        "a {noun} is `{kind}:<name>`, lowercase a-z, 0-9, `.`, `_`, `-` and `:`"
                    ),
                ));
            }
            return Ok(Target {
                scope: Scope::new(target),
                label: target.to_string(),
                missing: false,
                folder: false,
                node: None,
                qid: None,
                node_key: None,
            });
        }
        let config = config::load(&self.root)?;
        // A graph that can't be indexed still leases plain paths; a node
        // file among them is refused later, where its key is read.
        let looks_like_path = target.contains('/')
            || self.root.join(target).exists()
            || self
                .in_other_worktree(&format!("{}{target}", self.prefix))
                .is_some();
        let index = match index::build(&self.root, &config) {
            Ok(index) => index,
            Err(_) if looks_like_path && !target.starts_with("mdkg://") => {
                return self.path_target(target);
            }
            Err(e) => return Err(e),
        };
        match resolve_qid(&index, target, None) {
            Resolution::Ok(qid) => self.node_target(&index, &qid),
            // Not a node: a path, if it looks like one or names a file or
            // folder in any worktree of the repository (so `claim README.md`
            // works). Identity
            // refs and ambiguous ids are never paths.
            Resolution::Missing(_)
                if !target.starts_with("mdkg://")
                    && (target.contains('/')
                        || self.root.join(target).exists()
                        || self
                            .in_other_worktree(&format!("{}{target}", self.prefix))
                            .is_some()) =>
            {
                self.path_target(target)
            }
            other => Err(resolve_error("id", target, &other, None)),
        }
    }

    /// A node's lease target, by qid.
    fn node_target(&self, index: &mdkg_graph::Index, qid: &str) -> Result<Target, GraphError> {
        // One wording for a handle nothing matches (the audit's E2).
        let node = index.get(qid).ok_or_else(no_node_matches)?;
        let scope = self.leaf(&node.path)?;
        let node_key = Some(self.node_key(&scope, qid));
        Ok(Target {
            scope,
            label: output::display_ref(qid),
            missing: false,
            folder: false,
            node: Some(node.status.as_deref() == Some("done")),
            node_key,
            qid: Some(qid.to_string()),
        })
    }

    /// [`Repo::resolve`] for what a person or agent typed to name a node
    /// (`claim`, `sync`), with every kind of handle:
    /// - a target that names an existing path is that path, with a warning
    ///   when it also names a node;
    /// - otherwise a full id, an exact legacy id, a unique id prefix, or a
    ///   title fragment matching exactly one node (several refuse, exit 7,
    ///   listing them);
    /// - any handle that isn't the node's full id is named in a
    ///   `node.resolved` note (id and title), so the actor sees what was
    ///   claimed.
    pub(crate) fn resolve_handle_target(&self, target: &str) -> Result<Target, GraphError> {
        self.resolve_handle(target, false)
    }

    /// [`Repo::resolve_handle_target`] for a claim: a name that's no node
    /// and no path in any worktree (and has no `/` to read as one) is
    /// refused as a missing path, whose hint names `--new` and a trailing
    /// `/`, since the claimer most likely means a path it's about to
    /// create.
    pub(crate) fn resolve_claim_target(&self, target: &str) -> Result<Target, GraphError> {
        self.resolve_handle(target, true)
    }

    fn resolve_handle(&self, target: &str, claim: bool) -> Result<Target, GraphError> {
        refuse_hidden(target)?;
        let named = ["role:", "run:"].iter().any(|p| {
            target
                .get(..p.len())
                .is_some_and(|t| t.eq_ignore_ascii_case(p))
        });
        if named || target.starts_with("mdkg://") {
            return self.resolve(target);
        }
        // Repository-relative, read where it lives: in ref mode the root
        // is the graph cache, and a code file is in the checkout.
        let rel = format!("{}{target}", self.prefix);
        let exists =
            self.disk_base(&rel).join(&rel).exists() || self.in_other_worktree(&rel).is_some();
        let config = config::load(&self.root)?;
        let index = match index::build(&self.root, &config) {
            Ok(index) => index,
            // As `resolve`: a graph that can't be indexed still leases
            // plain paths.
            Err(_) if exists || target.contains('/') => return self.path_target(target),
            Err(e) => return Err(e),
        };
        let (resolved, notes) = resolve_noted(&index, target, None);
        if exists {
            if let Resolution::Ok(qid) = &resolved {
                let title = index.get(qid).map_or("", |n| n.title.as_str());
                let id = mdkg_index::resolve::display_id(&index, qid);
                // A 0.7 id is unmistakable, so a title that also matches
                // is only worth a note (a fresh agent found a warning
                // alarming); a 0.6 graph's short ids keep the warning.
                let severity = if index.graph_id.is_some() {
                    Severity::Info
                } else {
                    Severity::Warning
                };
                output::notice(
                    Diagnostic::new(
                        Code::NodeAmbiguous,
                        severity,
                        format!(
                            "the target names an existing path, so the path is leased; it also \
                             names {id}: {title}"
                        ),
                    )
                    .with_hint(format!("to lease the node, give its full id {id}")),
                );
            }
            return self.path_target(target);
        }
        match resolved {
            Resolution::Ok(qid) => {
                let found = self.node_target(&index, &qid)?;
                notes.into_iter().for_each(output::notice);
                Ok(found)
            }
            // A path that doesn't exist yet, as `resolve` reads one.
            Resolution::Missing(_) if target.contains('/') => self.path_target(target),
            Resolution::Missing(_) if claim => Err(missing_claim()),
            other => Err(handle_error(&index, "id", target, &other, None)),
        }
    }

    /// The node `id` a worker says `scope` is, checked in the graph here
    /// that holds the scope: its qid and lease key. Refused when the graph
    /// has the node at another path (merge the move first), names more
    /// than one node by it, or has another node at the scope. An id the
    /// graph lacks (a node so far only on a branch) keys the lease only on
    /// a node file, so nobody squats future ids on other paths.
    fn node_at_host(
        &self,
        scope: &Scope,
        id: &str,
    ) -> Result<(Option<String>, Option<String>), GraphError> {
        // An id's own form: nothing else is ever keyed or shown.
        let well_formed = !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'));
        if !well_formed {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "that isn't a node id",
            ));
        }
        let other = self.graph_of(scope)?;
        let graph = other.as_ref().unwrap_or(self);
        let config = config::load(&graph.root)?;
        let index = index::build(&graph.root, &config)?;
        let bare = |q: &str| q.split_once(':').map_or(q, |(_, b)| b).to_ascii_lowercase();
        let not_here = || {
            GraphError::conflict(
                Code::CoordLeaseHeld,
                "the node id doesn't name a node file at that scope here",
            )
            .with_hint("a node is leased at its own file in a node folder")
        };
        // The node at the scope, by its bare id: another graph opening the
        // same file names its workspace differently.
        if let Some(n) = index
            .nodes
            .iter()
            .find(|n| graph.scope_for(&n.path) == *scope)
        {
            if bare(&n.qid) != bare(id) {
                return Err(not_here());
            }
            return Ok((Some(n.qid.clone()), Some(graph.node_key(scope, &n.qid))));
        }
        // A move: a node here under the key the scope would have, at
        // another path, whatever workspace the worker named (ids repeat
        // across node folders, and another graph names workspaces
        // differently).
        let key = graph.node_key(scope, id);
        // Keys match only when bare ids do: those first, keys only for them.
        let moved = index
            .nodes
            .iter()
            .filter(|n| bare(&n.qid) == bare(id))
            .find_map(|n| {
                let here = graph.scope_for(&n.path);
                (here != *scope && graph.node_key(&here, &n.qid) == key)
                    .then(|| (n.qid.clone(), here))
            });
        if let Some((qid, here)) = moved {
            // Where it is here goes in `path`, never the message: a node's
            // file name is anyone's to choose.
            return Err(GraphError::conflict(
                Code::CoordLeaseHeld,
                format!(
                    "the node {} is at another path here, not at the scope asked for",
                    output::display_ref(&qid)
                ),
            )
            .with_hint(
                "a node is leased at the path the lease host has it; merge the move first, then \
                 claim it",
            )
            .at(here.to_string(), None));
        }
        if matches!(resolve_qid(&index, id, None), Resolution::Ambiguous(_)) {
            return Err(GraphError::ambiguous(
                "that node id names nodes in more than one workspace",
            )
            .with_hint("send the node's qualified id (`ws:id`)"));
        }
        // A node so far only on a branch (or one whose id repeats in another
        // node folder): its id keys the lease, on a node file only.
        if !graph.node_file(scope) {
            return Err(not_here());
        }
        let qid = if id.contains(':') {
            id.to_ascii_lowercase()
        } else {
            format!("root:{}", id.to_ascii_lowercase())
        };
        Ok((Some(qid.clone()), Some(graph.node_key(scope, &qid))))
    }

    /// A canonical scope, relative to the repository's top, as a lease
    /// authority's host is given it: no `.`/`..`, never absolute, never
    /// under a role or a turn. A role or a turn names itself.
    pub(crate) fn canonical_target(
        &self,
        target: &str,
        node: Option<&str>,
    ) -> Result<Target, GraphError> {
        // A profile C node's scope as `status` lists it: `node:<uuid>`,
        // the canonical spelling only.
        if let Some(uuid) = target.strip_prefix(STORE_SCOPE_PREFIX) {
            let canonical = mdkg_format::Uuid7::parse(uuid)
                .ok()
                .filter(|u| u.to_string() == uuid);
            if canonical.is_none() {
                return Err(GraphError::usage(
                    Code::UsageInvalidArguments,
                    format!("`{target}` isn't a canonical scope"),
                )
                .with_hint(
                    "a profile C node's scope is `node:<uuid>`, lowercase, as `status` lists it",
                ));
            }
            return Ok(Target {
                scope: Scope::new(target),
                label: target.to_string(),
                missing: false,
                folder: false,
                node: None,
                qid: None,
                node_key: None,
            });
        }
        if is_named_scope(&Scope::new(target)) && !target.contains('/') {
            return self.resolve(target);
        }
        refuse_hidden(target)?;
        let bad = target.is_empty()
            || target.starts_with('/')
            || target.contains('\\')
            || target
                .trim_end_matches('/')
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
            || aliases_named(target)
            || target
                .split('/')
                .next()
                .is_some_and(|first| first.eq_ignore_ascii_case(".git"));
        if bad {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "that isn't a canonical scope",
            )
            .with_hint(
                "name a file or folder relative to the repository root, without `.`, `..` or a leading `/`",
            ));
        }
        let rel = target.trim_end_matches('/');
        let missing =
            !self.disk_base(rel).join(rel).exists() && self.in_other_worktree(rel).is_none();
        let scope = Scope::new(rel);
        // The node, where the graph holding the scope has it, is here under
        // its own path: a worker's copy renamed in its worktree names
        // another scope, so its lease must be refused, never granted beside
        // the real one.
        let (qid, node_key) = match node {
            Some(id) => self.node_at_host(&scope, id)?,
            None => (None, None),
        };
        Ok(Target {
            scope,
            label: rel.to_string(),
            missing,
            folder: target.ends_with('/') || self.disk_base(rel).join(rel).is_dir(),
            node: node.map(|_| false),
            qid,
            node_key,
        })
    }

    /// A path as a scope relative to the worktree's top folder. Relative
    /// paths start at the graph root. `.` and `..` are resolved, symlinks
    /// followed and, on a case-insensitive disk, the stored case used, so
    /// every spelling of one file names one scope. A path is missing only
    /// when no worktree of the repository has it: an agent may claim from
    /// one worktree a file that so far exists only on its branch in another.
    pub(crate) fn path_target(&self, target: &str) -> Result<Target, GraphError> {
        refuse_hidden(target)?;
        let outside = || {
            GraphError::usage(
                Code::UsagePathOutsideWorktree,
                "that path is outside this worktree",
            )
            .with_hint("claim paths inside the worktree you edit")
        };
        let raw = Path::new(target);
        // Ref mode: the graph root is the cache, which mirrors the
        // repository; a relative path outside the graph folders is a code
        // path, from the checkout's top.
        let root = match &self.mode {
            Some(m) if !m.holds(target) => self.top.clone(),
            _ => std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone()),
        };
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            root.join(raw)
        };
        let resolved = true_path(&normalize(&joined));
        let base = match &self.mode {
            Some(m) => {
                let cache = std::fs::canonicalize(m.root()).unwrap_or_else(|_| m.root().into());
                if resolved.starts_with(&cache) {
                    cache
                } else {
                    self.top.clone()
                }
            }
            None => self.top.clone(),
        };
        let rel = resolved.strip_prefix(&base).map_err(|_| outside())?;
        let rel = rel.to_string_lossy();
        let rel = if cfg!(windows) {
            rel.replace('\\', "/")
        } else {
            rel.into_owned()
        };
        let scope = Scope::new(rel.trim_end_matches('/'));
        // A path can't start like a named scope (`role:`, `run:`, in any
        // case): its lease would overlap the role's or the turn's.
        if aliases_named(scope.as_str()) {
            return Err(GraphError::usage(
                Code::MsgBadId,
                "that path would overlap a role or a turn",
            )
            .with_hint("`role:<name>` and `run:<name>` name roles and turns, never paths"));
        }
        if scope.as_str().is_empty()
            || scope.as_str() == ".git"
            || scope.as_str().starts_with(".git/")
        {
            return Err(outside());
        }
        let found = if resolved.exists() {
            Some(resolved.is_dir())
        } else {
            self.in_other_worktree(&rel)
        };
        Ok(Target {
            label: scope.to_string(),
            missing: found.is_none(),
            // Named as a folder (a trailing `/`) when no worktree has it.
            folder: found.unwrap_or(target.ends_with('/')),
            node: None,
            qid: None,
            node_key: None,
            scope,
        })
    }

    /// Whether another worktree of the repository has `rel` (relative to
    /// the top folder), and if so whether it's a folder.
    fn in_other_worktree(&self, rel: &str) -> Option<bool> {
        git::worktrees(&self.top)
            .unwrap_or_default()
            .into_iter()
            .filter(|wt| *wt != self.top)
            .map(|wt| wt.join(rel))
            .find(|p| p.exists())
            .map(|p| p.is_dir())
    }

    /// The mode a claim on `target` gets: exclusive for nodes, for anything
    /// holding the graph and when asked; otherwise the config's policy for
    /// the path relative to the graph root (its default outside the root).
    /// A path no worktree has yet reads as a folder, the stricter reading,
    /// with or without `--new` (orchestrator, 2026-10-21, option (b)): a
    /// name that turns out to be a folder can never hold shared what's
    /// exclusive inside it. A new path is shared only where the config
    /// shares everything that could be inside it (a shared `dir/**`, or a
    /// shared default with no exclusive glob reaching inside). A new file
    /// only a file glob shares (`**/*_test.go`) is exclusive, and the lease
    /// keeps that mode until its holder releases it; after that, a plain
    /// claim of the existing file reads it as a file. Roles and turns are
    /// always exclusive.
    fn lease_mode(&self, config: &config::Config, target: &Target, exclusive: bool) -> Mode {
        // A node is exclusive, whichever graph's file it is, including
        // a nested graph's node found by path.
        if exclusive
            || target.node.is_some()
            || target.node_key.is_some()
            || is_named_scope(&target.scope)
        {
            return Mode::Exclusive;
        }
        let path = target.scope.as_str();
        let holds_graph = target.scope.overlaps(&self.graph_scope());
        let rel = if self.prefix.is_empty() {
            Some(path)
        } else {
            path.get(..self.prefix.len())
                .filter(|p| p.eq_ignore_ascii_case(&self.prefix))
                .map(|_| &path[self.prefix.len()..])
        };
        let mode = match rel {
            _ if holds_graph => config::LeaseMode::Exclusive,
            Some(rel) => config.lease_mode(rel, target.folder || target.missing),
            None => config.leases.default,
        };
        match mode {
            config::LeaseMode::Exclusive => Mode::Exclusive,
            config::LeaseMode::Shared => Mode::Shared,
        }
    }
}

/// `coordination.leases` in the committed config `config show` reports,
/// never a working-tree file; the default policy while the checkout
/// commits no graph yet.
fn committed_lease_policy(repo: &Repo) -> Result<config::LeasePolicy, GraphError> {
    match crate::msg::committed_config(repo)? {
        serde_json::Value::Null => Ok(config::LeasePolicy::default()),
        committed => Ok(config::from_snapshot(committed)?.leases),
    }
}

/// Whether a coordinator store may sit in a `.git` entry of `root` or a
/// folder above it. Decided by the store, not by git: an entry that can't
/// be read, or a worktree whose repository has moved, may hold one, so it
/// counts; a readable entry without a store (a stray folder, a repository
/// that never coordinated) doesn't.
fn store_may_exist(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .map(|dir| dir.join(".git"))
        .find(|entry| may_hold_store(entry))
}

/// Whether `path` exists, or may: a path that can't be checked, or a
/// symlink whose target is gone (a git folder on a disk that isn't
/// mounted), counts.
fn may_exist(path: &Path) -> bool {
    match path.try_exists() {
        Ok(true) | Err(_) => true,
        Ok(false) => std::fs::symlink_metadata(path).is_ok(),
    }
}

fn may_hold_store(entry: &Path) -> bool {
    use std::io::ErrorKind::NotFound;
    let meta = match std::fs::metadata(entry) {
        Ok(meta) => meta,
        // A dangling `.git` link may lead to a store git can't reach now.
        Err(e) if e.kind() == NotFound => {
            return std::fs::symlink_metadata(entry).is_ok_and(|m| m.file_type().is_symlink());
        }
        Err(_) => return true,
    };
    // The git folder: the entry itself, or a `gitdir:` file's target.
    let gitdir = if meta.is_dir() {
        entry.to_path_buf()
    } else {
        let text = match git::read_small(entry) {
            Ok(text) => text,
            Err(_) => return true,
        };
        let Some(target) = text.trim().strip_prefix("gitdir:").map(str::trim) else {
            return false;
        };
        let gitdir = entry.parent().unwrap_or(Path::new(".")).join(target);
        if !gitdir.try_exists().unwrap_or(false) {
            // The repository moved (`git worktree repair` fixes it): its
            // store may still be there, so fail closed.
            return true;
        }
        gitdir
    };
    // A worktree's folder names the shared one in `commondir`, whichever
    // way it was reached; the store lives there.
    let common = match git::read_small(&gitdir.join("commondir")) {
        Ok(c) => gitdir.join(c.trim()),
        Err(e) if e.kind() == NotFound => gitdir,
        Err(_) => return true,
    };
    let mdkg = common.join("mdkg");
    if !mdkg.try_exists().unwrap_or(true)
        && std::fs::symlink_metadata(&mdkg).is_ok_and(|m| m.file_type().is_symlink())
    {
        // A dangling `mdkg/` link: the store may be where it pointed.
        return true;
    }
    may_exist(&mdkg.join("coord.sqlite"))
}

/// A symlink whose target is gone.
fn dangling(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        && !p.try_exists().unwrap_or(true)
}

/// Refuses a store path behind a dangling link (`mdkg/` or the store itself
/// pointing at a disk that isn't mounted): its store may be there.
pub(crate) fn unreachable_store(path: &Path) -> Result<(), GraphError> {
    if dangling(path) || path.parent().is_some_and(dangling) {
        return Err(GraphError::failure(
            Code::CoordStoreFailed,
            format!("the coordinator store {} can't be reached", path.display()),
        )
        .with_hint(
            "a link on the way to it dangles (a disk that isn't mounted?): restore its target",
        ));
    }
    Ok(())
}

/// The `.git` entries git skipped on its way from `root` up to `top` (a
/// folder git can't use, so it searched further): one that may hold a
/// store is never ignored.
fn skipped_store(root: &Path, top: &Path) -> Option<PathBuf> {
    if !root.starts_with(top) {
        return None;
    }
    root.ancestors()
        .take_while(|dir| *dir != top)
        .map(|dir| dir.join(".git"))
        .find(|entry| may_hold_store(entry))
}

/// A workspace aliased `run` or `role` would make `run:<node-id>` both a turn
/// and a node: claim and sync refuse rather than guess.
fn refuse_ambiguous(config: &config::Config, target: &str) -> Result<(), GraphError> {
    let Some(kind) = ["role", "run"]
        .into_iter()
        .find(|k| target.strip_prefix(k).is_some_and(|r| r.starts_with(':')))
    else {
        return Ok(());
    };
    if !config.workspaces.iter().any(|w| w.alias == kind) {
        return Ok(());
    }
    let noun = if kind == "run" { "turn" } else { "role" };
    Err(GraphError::usage(
        Code::MsgBadId,
        format!("{target} is ambiguous: a workspace is aliased `{kind}`"),
    )
    .with_hint(format!(
        "rename the `{kind}` workspace; `{kind}:` names a {noun}"
    )))
}

/// Whether a path's first segment starts like a role or a turn, in any
/// case: a lease on it would overlap `role:<name>` or `run:<name>`.
pub(crate) fn aliases_named(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    ["role:", "run:", STORE_SCOPE_PREFIX].iter().any(|p| {
        first
            .get(..p.len())
            .is_some_and(|f| f.eq_ignore_ascii_case(p))
    })
}

fn named_folder(path: &str) -> GraphError {
    let first = path.split('/').next().unwrap_or_default();
    GraphError::usage(
        Code::MsgBadId,
        format!(
            "the folder `{first}` is named like a role or a turn, so its leases would overlap one"
        ),
    )
    .with_hint("rename the folder: `role:` and `run:` name roles and turns")
}

/// A `role:` or `run:` scope, or a profile C node's `node:` one: a name,
/// not a path, and always exclusive.
pub(crate) fn is_named_scope(scope: &Scope) -> bool {
    is_role_or_turn(scope) || is_store_scope(scope)
}

/// The prefix of a profile C node's scope, `node:<uuid>` (format 9.1);
/// [`Repo::store_scope`] is the only place one is built.
pub(crate) const STORE_SCOPE_PREFIX: &str = "node:";

/// The UUID a store-folder node file's id names, read with the format's
/// own frontmatter parser. `None` when the file can't be read or names no
/// node id.
fn store_node_uuid(path: &Path, label: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // Store nodes exist only in format 0.7 graphs: its lexical rules.
    let lex = mdkg_format::frontmatter::Lexical::Ascii;
    let parsed = mdkg_format::frontmatter::parse_with(&text, label, lex).ok()?;
    let id = parsed.frontmatter.get("id")?.as_str()?;
    let id = mdkg_format::NodeId::parse(id).ok()?;
    Some(id.uuid().to_string().to_ascii_lowercase())
}

/// Whether `scope` is a profile C node's: the one test every
/// caller uses, so the scope's spelling lives in one place.
pub(crate) fn is_store_scope(scope: &Scope) -> bool {
    is_store_name(scope.as_str())
}

/// [`is_store_scope`] for text a user typed or a record holds.
pub(crate) fn is_store_name(text: &str) -> bool {
    text.starts_with(STORE_SCOPE_PREFIX)
}

/// A `role:` or `run:` scope, which never holds or covers a node. A
/// profile C node's `node:` scope is named too, but holds its node.
pub(crate) fn is_role_or_turn(scope: &Scope) -> bool {
    let s = scope.as_str();
    s.starts_with("role:") || s.starts_with("run:")
}

/// What a command acts on.
pub(crate) struct Target {
    pub(crate) scope: Scope,
    /// How messages name it.
    pub(crate) label: String,
    /// A path that doesn't exist yet in any worktree.
    pub(crate) missing: bool,
    /// A path that names a folder.
    pub(crate) folder: bool,
    /// For a node: whether its file says `done`.
    pub(crate) node: Option<bool>,
    /// The node the scope is, by id: its lease holds the node whatever
    /// path another worktree gives the file.
    pub(crate) qid: Option<String>,
    /// The key its lease holds the node under, in the graph that has it.
    pub(crate) node_key: Option<String>,
}

/// Whether a scope holds a character that can't be shown as itself: a
/// control, format or bidirectional character, which would let a path
/// rewrite the text around it wherever it's shown to another actor.
pub(crate) fn hidden_characters(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            || matches!(c,
                '\u{00AD}' | '\u{061C}' | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}')
    })
}

/// The most leases one actor holds at once, limiting how much it can block.
pub(crate) const MAX_LEASES: usize = 32;

/// Whether `actor` holds `MAX_LEASES` live leases, so a new one is refused.
pub(crate) fn at_lease_cap(state: &Coordinator, actor: &str, now: u64) -> bool {
    state
        .leases()
        .filter(|l| {
            l.owner == actor && l.confirmed && l.status == Status::Active && l.deadline > now
        })
        .count()
        >= MAX_LEASES
}

pub(crate) fn lease_cap_error() -> GraphError {
    GraphError::conflict(
        Code::CoordLeaseLimit,
        format!("you hold {MAX_LEASES} leases, the most one actor may"),
    )
    .with_hint(format!(
        "release what you're done with; `{BIN} status` lists your leases"
    ))
}

/// The most bytes a lease's scope holds: it's shown to other actors.
const MAX_SCOPE: usize = 512;

/// Checks a scope as it will be stored, after links and node files are
/// resolved (a symlink's target or a node's file name can
/// carry what the typed text didn't). It must be ASCII, since other
/// characters can be spelled more than one way (NFC, NFD, `ſ` for `s`) and
/// each spelling would get its own lease (refused on every disk, as branch
/// names are, until scope keys are Unicode-normalized, post-v0.1); it can't
/// hold control characters, which would rewrite text around it wherever
/// it's shown; and it's bounded.
pub(crate) fn check_scope_text(scope: &Scope) -> Result<(), GraphError> {
    let s = scope.as_str();
    let (problem, hint) = if !s.is_ascii() {
        (
            "a lease's scope must be ASCII: other characters can be spelled more than one way",
            "rename the file or folder with ASCII characters to lease it",
        )
    } else if s.chars().any(|c| c.is_ascii_control()) {
        (
            "a lease's scope can't hold control characters",
            "rename the file or folder without control characters to lease it",
        )
    } else if s.len() > MAX_SCOPE {
        (
            "a lease's scope is at most 512 bytes",
            "lease a shorter path, such as a folder above it",
        )
    } else {
        return Ok(());
    };
    Err(GraphError::usage(Code::UsageInvalidArguments, problem).with_hint(hint))
}

fn refuse_hidden(target: &str) -> Result<(), GraphError> {
    if hidden_characters(target) {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "a lease's scope can't hold control, format or bidirectional characters",
        )
        .with_hint("name the path with visible characters only"));
    }
    Ok(())
}

/// Resolves `.` and `..` without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The path as the disk stores it: symlinks resolved for the part that
/// exists, and each existing name in its stored case. The part that doesn't
/// exist yet is kept as given.
fn true_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (
            existing.file_name().map(ToOwned::to_owned),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    // canonicalize keeps the case it was given; take the stored names.
    let mut cased = PathBuf::new();
    for part in out.components() {
        let name = part.as_os_str();
        let stored = std::fs::read_dir(if cased.as_os_str().is_empty() {
            Path::new("/")
        } else {
            cased.as_path()
        })
        .ok()
        .and_then(|entries| {
            let entries: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .collect();
            if entries.iter().any(|e| e == name) {
                return None;
            }
            entries.into_iter().find(|e| {
                e.to_string_lossy().to_lowercase() == name.to_string_lossy().to_lowercase()
            })
        });
        match stored {
            Some(s) => cased.push(s),
            None => cased.push(name),
        }
    }
    out = cased;
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

/// A handle nothing matches: one wording everywhere, without quoting
/// caller-supplied text in the diagnostic.
pub(crate) fn no_node_matches() -> GraphError {
    GraphError::not_found(Code::NodeNotFound, "no node matches that handle").with_hint(format!(
        "`{bin} search <words>` or `{bin} list` finds it; a handle is a full id, a unique id \
         prefix or a title fragment",
        bin = crate::BIN
    ))
}

/// A claim of a name that's no node and no path yet: what was typed is
/// never quoted.
fn missing_claim() -> GraphError {
    GraphError::not_found(
        Code::CoordPathMissing,
        "no node matches that handle, and no worktree has that path",
    )
    .with_hint(format!(
        "to claim a file or folder you're about to create, claim it with `{bin} claim --new \
         <path>` (MCP: lease_claim with `new: true`), or end a folder's name with `/`; to find \
         a node, `{bin} search <words>`",
        bin = crate::BIN
    ))
}

/// A coordinator refusal as an error with its exit class.
pub(crate) fn refusal(e: &CoordError) -> GraphError {
    let (kind, code) = match e {
        CoordError::Held(_) => (ErrorKind::Conflict, Code::CoordLeaseHeld),
        CoordError::CasFailed => (ErrorKind::Conflict, Code::CoordCasFailed),
        CoordError::PastDeadline => (ErrorKind::Conflict, Code::CoordPastDeadline),
        CoordError::NotOwner => (ErrorKind::Conflict, Code::CoordNotOwner),
        CoordError::NotActive | CoordError::NotConfirmed => {
            (ErrorKind::Conflict, Code::CoordNotActive)
        }
        CoordError::AlreadySettled(_) => (ErrorKind::Conflict, Code::CoordAlreadySettled),
        CoordError::ObservationRaced(_) => (ErrorKind::Conflict, Code::CoordObservationRaced),
        CoordError::KeyReused(_) => (ErrorKind::Conflict, Code::CoordKeyReused),
        CoordError::DuplicateLease(_) => (ErrorKind::Conflict, Code::CoordDuplicateLease),
        CoordError::UnknownLease => (ErrorKind::NotFound, Code::CoordUnknownLease),
        CoordError::UnknownChangeset(_) => (ErrorKind::NotFound, Code::CoordUnknownChangeset),
        CoordError::OutOfScope(_) => (ErrorKind::Usage, Code::CoordOutOfScope),
        CoordError::Offline
        | CoordError::AlreadyConfirmed
        | CoordError::AlreadyRegistered(_)
        | CoordError::NotDue => (ErrorKind::Failure, Code::CoordStoreFailed),
    };
    // What to do next (the audit's E1), naming nothing that was typed. A
    // caller with a better hint sets its own.
    let bin = crate::BIN;
    let hint = match e {
        CoordError::Held(_) => Some(format!(
            "pick other work, or see who holds what with `{bin} status`"
        )),
        CoordError::CasFailed => Some("someone changed it first; run the command again".into()),
        CoordError::PastDeadline => Some("the lease ended; claim it again".into()),
        CoordError::NotOwner => Some(format!(
            "only the lease's owner can do that; see who holds what with `{bin} status`"
        )),
        CoordError::NotActive | CoordError::NotConfirmed => {
            Some("that lease isn't active; claim it again".into())
        }
        CoordError::AlreadySettled(_) => Some(format!(
            "it's settled already (merged or abandoned); `{bin} status` shows what's still open"
        )),
        CoordError::ObservationRaced(_) => {
            Some("another command observed main at the same time; run it again".into())
        }
        CoordError::UnknownLease => Some(format!("see your leases with `{bin} status`")),
        CoordError::UnknownChangeset(_) => Some(format!("see open changesets with `{bin} status`")),
        CoordError::Offline
        | CoordError::AlreadyConfirmed
        | CoordError::AlreadyRegistered(_)
        | CoordError::NotDue => Some(format!("`{bin} doctor` checks the store")),
        CoordError::OutOfScope(_) | CoordError::KeyReused(_) | CoordError::DuplicateLease(_) => {
            None
        }
    };
    let error = GraphError::new(kind, Diagnostic::error(code, e.to_string()));
    match hint {
        Some(h) => error.with_hint(h),
        None => error,
    }
}

/// The node a profile C scope (`node:<uuid>`) names, so a reader never
/// sees a bare UUID alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeName {
    /// Its current full id.
    pub id: String,
    pub title: String,
}

impl NodeName {
    /// ` (<id>: <title>)`, for a text line.
    pub(crate) fn suffix(name: Option<&NodeName>) -> String {
        name.map_or(String::new(), |n| format!(" ({}: {})", n.id, n.title))
    }
}

/// The node each profile C scope in this graph names, read from the graph
/// (empty when it can't be read: the scopes are then shown alone).
pub(crate) fn store_names(repo: &Repo) -> std::collections::HashMap<String, NodeName> {
    let Ok(config) = config::load(&repo.root) else {
        return Default::default();
    };
    let Ok(graph) = index::build(&repo.root, &config) else {
        return Default::default();
    };
    graph
        .nodes
        .iter()
        .filter_map(|n| {
            let scope = repo.store_scope(&n.path)?;
            (scope.as_str() != STORE_SCOPE_PREFIX).then(|| {
                (
                    scope.to_string(),
                    NodeName {
                        id: n.id.clone(),
                        title: n.title.clone(),
                    },
                )
            })
        })
        .collect()
}

/// Names each record's profile C scope, reading the graph only when one has.
pub(crate) fn name_store_scopes<'a>(
    repo: &Repo,
    records: impl IntoIterator<Item = (&'a str, &'a mut Option<NodeName>)>,
) {
    let mut names: Option<std::collections::HashMap<String, NodeName>> = None;
    for (scope, slot) in records {
        if is_store_name(scope) {
            let names = names.get_or_insert_with(|| store_names(repo));
            *slot = names.get(scope).cloned();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaseRecord {
    pub id: String,
    pub owner: String,
    pub scope: String,
    /// For a profile C scope (`node:<uuid>`): the node it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeName>,
    pub status: String,
    /// When the lease ends, RFC 3339 UTC.
    pub until: String,
    pub deadline_ms: u64,
    /// `exclusive`, or `shared` when the config lets others share the path.
    pub mode: String,
    /// Owners of other active leases overlapping a shared one, or one
    /// held on a branch (theirs on other branches).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
    /// The branch a path lease holds its scope on, where the repository
    /// scopes path leases by branch; absent for one held on every branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset: Option<String>,
    /// In `status`: whether the lease is the acting actor's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mine: Option<bool>,
    /// In `status`: true when an active lease is in the last eighth of its
    /// time (15 minutes of the default 2 hours); `mdkg renew` extends
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiring: Option<bool>,
    /// In `status`: true for a path lease with no changes in its owner's
    /// worktree, past the first quarter of its term, while no
    /// turn of the owner's (`run:<owner>`) runs. `mdkg renew` won't keep
    /// it, and its owner can give it up with `mdkg release <path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<bool>,
    /// In `status` and `holds`, for an active lease: milliseconds since it
    /// was last claimed, claimed again or renewed (read from its deadline
    /// and the store's term).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// In `status` and `holds`, for an active path lease: true when its
    /// paths have no changes, staged or not, in its owner's worktree. Absent
    /// when git can't say or the owner's worktree isn't found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<bool>,
    /// In `status` and `holds`, beside `clean`: true when the owner has
    /// committed the lease's paths since it was granted, nothing in them
    /// is uncommitted, and the owner works in that worktree. Its owner's
    /// `mdkg release --committed` releases such a lease; nothing releases
    /// it by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed: Option<bool>,
    /// In `status`, for your shared path leases: each co-holder whose
    /// branch head conflicts with yours (by `git merge-tree`, committed
    /// content only) in files under the lease. Advisory: nothing is
    /// refused, and the merge settles it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_conflicts: Vec<MergeConflict>,
}

/// A co-holder's branch that conflicts with yours under a shared lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergeConflict {
    /// The co-holder.
    pub owner: String,
    /// The conflicting files under the lease, relative to the top.
    pub paths: Vec<String>,
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Active => "active",
        Status::Committed => "committed",
        Status::Released => "released",
        Status::Expired => "expired",
        Status::Conflict => "conflict",
    }
}

pub(crate) fn lease_record(l: &Lease) -> LeaseRecord {
    LeaseRecord {
        id: l.id.to_string(),
        owner: l.owner.clone(),
        scope: l.scope.to_string(),
        node: None,
        status: status_name(l.status).into(),
        until: rfc3339(l.deadline),
        deadline_ms: l.deadline,
        mode: mode_name(l.mode).into(),
        shared_with: Vec::new(),
        branch: l.branch.clone(),
        changeset: l.changeset.map(|c| c.to_string()),
        mine: None,
        expiring: None,
        idle: None,
        age_ms: None,
        clean: None,
        committed: None,
        merge_conflicts: Vec::new(),
    }
}

/// `record` with the lease's age, and for a path lease whose owner's
/// worktree is found, whether its paths are clean and committed.
fn with_path_state(
    record: LeaseRecord,
    state: &Coordinator,
    l: &Lease,
    path: Option<PathState>,
    now: u64,
) -> LeaseRecord {
    let path = path.unwrap_or_default();
    LeaseRecord {
        age_ms: Some(now.saturating_sub(granted_at(state, l))),
        clean: path.clean,
        committed: path.clean.map(|_| path.committed),
        ..record
    }
}

/// A lease's age for text output: `12 min`, `1h 5m`.
fn age_text(ms: u64) -> String {
    let mins = ms / 60_000;
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

/// What text output says of a lease's paths: `, clean`, `, committed`,
/// `, uncommitted changes`, or nothing when it isn't known.
fn paths_text(l: &LeaseRecord) -> &'static str {
    match (l.clean, l.committed) {
        (Some(true), Some(true)) => {
            ", committed and clean: its owner's `release --committed` releases it"
        }
        (Some(true), _) => ", clean",
        (Some(false), _) => ", uncommitted changes",
        (None, _) => "",
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Exclusive => "exclusive",
        Mode::Shared => "shared",
    }
}

/// Whether `l` may be held beside other actors' leases: a shared lease,
/// or one held on a branch.
fn co_holdable(l: &Lease) -> bool {
    l.mode == Mode::Shared || l.branch.is_some()
}

/// Other actors' active leases overlapping `l`, for a lease held beside
/// them ([`co_holdable`]).
fn co_holders<'a>(state: &'a Coordinator, l: &'a Lease) -> Vec<&'a Lease> {
    if !co_holdable(l) || l.status != Status::Active {
        return Vec::new();
    }
    state
        .holders(&l.scope)
        .filter(|h| h.owner != l.owner)
        .collect()
}

/// A lease's record with the owners it shares its scope with.
fn lease_record_in(state: &Coordinator, l: &Lease) -> LeaseRecord {
    let mut owners: Vec<String> = co_holders(state, l)
        .into_iter()
        .map(|h| h.owner.clone())
        .collect();
    owners.sort();
    owners.dedup();
    LeaseRecord {
        shared_with: owners,
        ..lease_record(l)
    }
}

/// The notices for a lease held beside others: `coord.lease_shared` for
/// co-holders on its branch (or every branch), `coord.lease_other_branch`
/// for those on other branches.
fn co_hold_notices(state: &Coordinator, l: &Lease) -> Vec<Diagnostic> {
    shared_notice(state, l)
        .into_iter()
        .chain(other_branch_notice(state, l))
        .collect()
}

/// The `coord.lease_other_branch` notice for a lease with co-holders on
/// other branches.
fn other_branch_notice(state: &Coordinator, l: &Lease) -> Option<Diagnostic> {
    let others: Vec<&Lease> = co_holders(state, l)
        .into_iter()
        .filter(|h| h.on_other_branch(l.branch.as_deref()))
        .collect();
    if others.is_empty() {
        return None;
    }
    // Their names are data, never prose: other actors chose them.
    Some(
        Diagnostic::warning(
            Code::CoordLeaseOtherBranch,
            format!(
                "your lease {} overlaps {} other holder(s)' on other branches",
                l.id,
                others.len()
            ),
        )
        .at(l.scope.to_string(), None)
        .held_by(others.iter().map(|h| h.owner.clone()))
        .with_hint(
            "this repository scopes path leases by branch; tell them what you change, \
             and expect to merge",
        ),
    )
}

/// The `coord.lease_shared` notice for a shared lease with co-holders on
/// its branch, or on every branch.
fn shared_notice(state: &Coordinator, l: &Lease) -> Option<Diagnostic> {
    let others: Vec<&Lease> = co_holders(state, l)
        .into_iter()
        .filter(|h| !h.on_other_branch(l.branch.as_deref()))
        .collect();
    if others.is_empty() {
        return None;
    }
    // Their names are data, never prose: other actors chose them.
    Some(
        Diagnostic::warning(
            Code::CoordLeaseShared,
            format!(
                "your lease {} is shared with {} other holder(s)",
                l.id,
                others.len()
            ),
        )
        .at(l.scope.to_string(), None)
        .held_by(others.iter().map(|h| h.owner.clone()))
        .with_hint("the config shares this path; tell them what you change, and expect to merge"),
    )
}

fn settlement_name(s: Settlement) -> &'static str {
    match s {
        Settlement::Sequenced => "sequenced",
        Settlement::Committed => "committed",
        Settlement::Conflicted => "conflicted",
        Settlement::Abandoned => "abandoned",
    }
}

fn entry_name(s: EntryState) -> &'static str {
    match s {
        EntryState::Committed => "committed",
        EntryState::Pending => "pending",
        EntryState::Conflicted => "conflicted",
        EntryState::Abandoned => "abandoned",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChangesetRecord {
    pub id: String,
    pub lease: String,
    pub owner: String,
    pub scope: String,
    /// `sequenced` or `conflicted` while it waits for main.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Changesets that must merge first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Files it changes.
    pub files: Vec<String>,
}

fn changeset_record(c: &mdkg_coord::Changeset) -> ChangesetRecord {
    ChangesetRecord {
        id: c.id.to_string(),
        lease: c.lease.to_string(),
        owner: c.owner.clone(),
        scope: c.scope.to_string(),
        state: settlement_name(c.state).into(),
        branch: c.source.as_ref().map(|s| s.branch.clone()),
        commit: c.source.as_ref().map(|s| s.commit.clone()),
        depends_on: c.depends_on.iter().map(ToString::to_string).collect(),
        files: c.changes.keys().map(ToString::to_string).collect(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileRecord {
    pub path: String,
    /// The coordinator's version: `pending` until it reaches main.
    pub state: String,
    /// The version's content hash; absent for a deletion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Pending versions in `scope` that the worktree doesn't hold. A version is
/// held when its commit is in HEAD's history, so editing after `sync`
/// doesn't make it missing again, or when the file has its exact content.
pub(crate) fn missing_versions(
    repo: &Repo,
    state: &Coordinator,
    scope: &Scope,
) -> Result<Vec<FileRecord>, GraphError> {
    let mut out = Vec::new();
    for e in state.expected(scope) {
        if e.state != EntryState::Pending {
            continue;
        }
        // Ref mode: a graph file's version is held when the graph ref has
        // its commit, or the cache its content.
        let graph = repo.mode.as_ref().filter(|m| m.holds(e.leaf.as_str()));
        let in_history = match (&e.source, graph) {
            (Some(s), Some(m)) => {
                git::is_ancestor_of(&repo.top, &s.commit, &m.graph.name).unwrap_or(false)
            }
            (Some(s), None) => git::is_ancestor(&repo.top, &s.commit).unwrap_or(false),
            (None, _) => false,
        };
        if in_history {
            continue;
        }
        let have = if graph.is_some() {
            let seam = repo.graph_git();
            let path = seam.disk_spelling(e.leaf.as_str());
            seam.file_digest(&path)
        } else {
            let path = git::disk_spelling(&repo.top, e.leaf.as_str());
            git::file_digest(&repo.top, &path)
        }
        .map_err(git_error)?;
        if have != e.content {
            out.push(FileRecord {
                path: e.leaf.to_string(),
                state: entry_name(e.state).into(),
                content: e.content.clone(),
                branch: e.source.as_ref().map(|s| s.branch.clone()),
                commit: e.source.as_ref().map(|s| s.commit.clone()),
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClaimData {
    pub lease: LeaseRecord,
    /// True when an existing lease of yours was renewed.
    pub renewed: bool,
    /// Unmerged versions in the scope this worktree lacks; run `sync`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<FileRecord>,
    /// With several targets: each one's claim, in the order given, one
    /// lease per target (the fields above are the first's). Absent for one
    /// target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<ClaimedTarget>,
}

/// One target's claim in a `claim` of several.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClaimedTarget {
    /// The target as given.
    pub target: String,
    pub lease: LeaseRecord,
    /// True when an existing lease of yours was renewed.
    pub renewed: bool,
    /// Unmerged versions in the scope this worktree lacks; run `sync`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<FileRecord>,
}

pub(crate) fn own_lease<'a>(
    state: &'a Coordinator,
    actor: &str,
    scope: &Scope,
) -> Option<&'a Lease> {
    state.leases().find(|l| {
        l.owner == actor && l.confirmed && l.status == Status::Active && l.scope == *scope
    })
}

/// [`own_lease`] for a checkout on the branch `here` gives (`None`:
/// detached), read only when the actor holds `scope` on more than one
/// branch: the one held there, else the first (Terra #21585 F4).
fn own_lease_here<'a>(
    state: &'a Coordinator,
    actor: &str,
    scope: &Scope,
    here: impl FnOnce() -> Option<String>,
) -> Option<&'a Lease> {
    let mine: Vec<&Lease> = state
        .leases()
        .filter(|l| {
            l.owner == actor && l.confirmed && l.status == Status::Active && l.scope == *scope
        })
        .collect();
    if mine.len() < 2 {
        return mine.first().copied();
    }
    let here = here();
    mine.iter()
        .find(|l| l.holds_on(here.as_deref()))
        .or(mine.first())
        .copied()
}

/// The branch the checkout asking about a lease is on: the `--branch` a
/// lease authority's client named (empty: `--detached`), else `repo`'s own.
fn asking_branch(repo: &Repo, asked: Option<&str>) -> Option<String> {
    match asked {
        Some(b) => Some(b).filter(|b| !b.is_empty()).map(str::to_string),
        None => checked_out_branch(&repo.top),
    }
}

/// The branch a host's `holds` or `release --scope` was asked on: its
/// client's `--branch`, or empty for `--detached`; `None` when neither
/// was given.
pub fn asked_branch(branch: Option<&str>, detached: bool) -> Option<&str> {
    branch.or(detached.then_some(""))
}

/// What a client tells its lease authority's `holds` and `release
/// --scope` about its checkout, so the host judges the lease on the
/// client's branch, not its own (Terra #21585 F1): `--branch <name>`, or
/// `--detached`; nothing where this repository doesn't scope path leases
/// by branch.
fn branch_option(repo: &Repo) -> Result<Option<(&'static str, Option<String>)>, GraphError> {
    let policy = committed_lease_policy(repo)?;
    if lease_scope(repo, &policy)? != config::LeaseScope::Branch {
        return Ok(None);
    }
    Ok(Some(branch_flag(Some(
        checked_out_branch(&repo.top).unwrap_or_default(),
    ))))
}

/// The option that passes the branch `asked` on (empty: detached) to a
/// lease authority.
fn branch_flag(asked: Option<String>) -> (&'static str, Option<String>) {
    match asked.filter(|b| !b.is_empty()) {
        Some(b) => ("--branch", Some(b)),
        None => ("--detached", None),
    }
}

/// The actor's active lease on `scope` or on a folder containing it, so
/// claiming a file inside a folder you hold renews the folder.
pub(crate) fn covering_lease<'a>(
    state: &'a Coordinator,
    actor: &str,
    scope: &Scope,
) -> Option<&'a Lease> {
    covering_lease_where(state, actor, scope, |_| true)
}

/// [`covering_lease`] among the leases `keep` accepts: an actor whose
/// worktree switched branches may hold one lease per branch.
fn covering_lease_where<'a>(
    state: &'a Coordinator,
    actor: &str,
    scope: &Scope,
    keep: impl Fn(&Lease) -> bool,
) -> Option<&'a Lease> {
    own_lease(state, actor, scope)
        .filter(|l| keep(l))
        .or_else(|| {
            state.leases().find(|l| {
                // A role or a turn covers only itself: never renewed by a claim
                // of a scope inside it (a node squatting a `role:x/...` folder).
                l.owner == actor
                    && l.confirmed
                    && l.status == Status::Active
                    && l.scope.contains(scope)
                    && !is_role_or_turn(&l.scope)
                    && keep(l)
            })
        })
}

/// How to claim.
#[derive(Debug, Clone, Default)]
pub struct ClaimOptions {
    /// The target is a path you're about to create: read it as a path,
    /// and don't warn that it's missing.
    pub new: bool,
    /// Take an exclusive lease even where the config shares the path.
    pub exclusive: bool,
    /// The target is a canonical scope (relative to the repository's top)
    /// taken in this mode: how a lease authority's host is asked to grant
    /// what a worker resolved.
    pub mode: Option<Mode>,
    /// With `mode`: the node the scope is, by id (always exclusive).
    pub node: Option<String>,
    /// With `mode`: the branch the worker's checkout has, for a
    /// repository that scopes path leases by branch.
    pub branch: Option<String>,
    /// The target is an id or scope this build already resolved (`goal
    /// claim`'s pick, a turn's scope): taken exactly, never as a handle, so
    /// a file named like it can't take its place.
    pub exact: bool,
}

impl ClaimOptions {
    /// Options for a target taken exactly, never as a handle: an id this
    /// build already resolved, or a 0.6.0 alias's id.
    pub fn exact() -> Self {
        ClaimOptions {
            exact: true,
            ..ClaimOptions::default()
        }
    }
}

/// Claims `target` and reports the lease, without printing.
pub(crate) fn claim_report(
    root: &Path,
    actor: Option<String>,
    target: &str,
) -> Result<Report<ClaimData>, GraphError> {
    let report = claim_report_with(root, actor, target, ClaimOptions::exact())?
        .with(authority::committed_key_warning(root));
    // An authority's refusal fails the caller as a local one would.
    match (
        report.failure,
        report
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error),
    ) {
        (Some(kind), Some(d)) => Err(GraphError::new(kind, d.clone())),
        _ => Ok(report),
    }
}

/// Claims through the lease authority when one is configured, else in the
/// local store.
fn claim_report_with(
    root: &Path,
    actor: Option<String>,
    target: &str,
    opts: ClaimOptions,
) -> Result<Report<ClaimData>, GraphError> {
    let plan = plan_claim(root, actor, target, &opts)?;
    claim_planned(root, plan)
}

/// Claims what [`plan_claim`] resolved, through the lease authority when
/// one is configured, else in the local store.
fn claim_planned(root: &Path, plan: Plan) -> Result<Report<ClaimData>, GraphError> {
    // Roles and turns are per node and per store: never delegated
    // (orchestrator, 2026-10-21), so a link outage can't stop a turn.
    if is_named_scope(&plan.scope) {
        return claim_local(plan);
    }
    let Some(authority) = authority::find(&plan.repo)? else {
        return claim_local(plan);
    };
    // The authority grants the scope and mode resolved here.
    let mut options = vec![("--mode", Some(mode_name(plan.mode).to_string()))];
    if plan.exclusive {
        options.push(("--exclusive", None));
    }
    if let Some(q) = &plan.qid {
        // The qualified id, so a host with the id in two workspaces
        // knows which.
        options.push(("--node", Some(q.clone())));
    }
    if let Some(b) = &plan.branch {
        options.push(("--branch", Some(b.clone())));
    }
    options.push(("--as", Some(plan.actor.clone())));
    let mut report: Report<ClaimData> =
        authority.delegate(root, "claim", &[plan.scope.to_string()], &options)?;
    if report.failure.is_none() {
        // What only this worktree can tell (versions it lacks, a missing
        // path, a done node), read once granted, so it's current; a local
        // failure must never turn a granted claim into a refusal.
        match local_notes(&plan) {
            Ok((sync, notes)) => {
                if let Some(d) = report.data.as_mut() {
                    d.sync = sync;
                }
                report = report.with(notes);
            }
            Err(e) => {
                report = report.with([Diagnostic::warning(
                    Code::CoordStoreFailed,
                    format!(
                        "the claim was granted, but this worktree's notes couldn't be read: {}",
                        e.diagnostic.message
                    ),
                )]);
            }
        }
    }
    Ok(report)
}

/// A wider claim over your own narrower leases: keep what you hold.
/// Releasing to make room lets another actor take it first, and leaves
/// you holding neither.
fn narrower_hint(retry: String) -> String {
    format!(
        "keep claiming the narrower scope; to widen, every lease you hold inside it must go \
         first, and anyone may take them in between: {retry}"
    )
}

/// "Release that, then claim this again", with the commands as typed. A
/// host (a canonical claim) says it without commands: its paths are the
/// repository's, not the worker's graph's.
fn retry_hint(
    repo: &Repo,
    held: &Scope,
    canonical: bool,
    target: &str,
    new: bool,
    exclusive: bool,
) -> String {
    if canonical {
        return format!(
            "release your lease on {} first, then claim again",
            output::shown(held.as_str())
        );
    }
    let mut claim = format!("{BIN} claim");
    if new {
        claim.push_str(" --new");
    }
    if exclusive {
        claim.push_str(" --exclusive");
    }
    format!(
        "release it first (`{BIN} release {}`), then `{claim} {target}`",
        safe_target(repo, held)
    )
}

/// The notes a claim of `plan` gets from this worktree's own store.
fn local_notes(plan: &Plan) -> Result<(Vec<FileRecord>, Vec<Diagnostic>), GraphError> {
    let mut store = plan.repo.store()?;
    plan.repo.observe(&mut store, now_ms())?;
    let state = store.coordinator().map_err(store_error)?;
    let sync = missing_versions(&plan.repo, &state, &plan.scope)?;
    let mut notes = claim_notes(
        &state,
        &plan.scope,
        &plan.label,
        &plan.target,
        plan.missing && !plan.new,
        plan.node,
    );
    if !sync.is_empty() {
        notes.push(sync_note(sync.len(), &plan.label, &plan.target));
    }
    Ok((sync, notes))
}

/// Warnings about the claimed thing itself: missing, or already done.
fn claim_notes(
    state: &Coordinator,
    scope: &Scope,
    label: &str,
    target: &str,
    missing: bool,
    node: Option<bool>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if missing {
        diagnostics.push(
            Diagnostic::warning(
                Code::CoordPathMissing,
                format!("{label} doesn't exist in any worktree"),
            )
            .with_hint(format!(
                "check the path; to create it, claim it with `{BIN} claim --new {target}`"
            )),
        );
    }
    if node.is_some_and(|done| done || state.done_mark(scope).is_some()) {
        diagnostics.push(
            Diagnostic::warning(Code::CoordNodeDone, format!("{label} is already done")).with_hint(
                format!("fine for editing its record; otherwise pick other work with `{BIN} next`"),
            ),
        );
    }
    diagnostics
}

fn sync_note(count: usize, label: &str, target: &str) -> Diagnostic {
    Diagnostic::warning(
        Code::CoordSyncNeeded,
        format!("{count} file(s) in {label} have unmerged versions this worktree lacks"),
    )
    .with_hint(format!("run `{BIN} sync {target}` before editing"))
}

/// A claim resolved and checked locally, before anything is granted.
struct Plan {
    repo: Repo,
    scope: Scope,
    label: String,
    missing: bool,
    node: Option<bool>,
    qid: Option<String>,
    node_key: Option<String>,
    mode: Mode,
    actor: String,
    target: String,
    new: bool,
    exclusive: bool,
    /// A host's claim of a canonical scope (`--mode`): the worker adds the
    /// notes about its own worktree, so none come from here.
    canonical: bool,
    /// The branch a branch-scoped path lease holds its scope on.
    branch: Option<String>,
}

/// Resolves and checks a claim: the scope, its mode, the actor. Nothing
/// is granted, locally or by an authority.
fn plan_claim(
    root: &Path,
    actor: Option<String>,
    target: &str,
    opts: &ClaimOptions,
) -> Result<Plan, GraphError> {
    {
        // Nothing hidden reaches a message, even a refusal's.
        refuse_hidden(target)?;
        let repo = Repo::open(root)?;
        let mut config = config::load(root)?;
        // Which paths are shared is committed authority: an uncommitted
        // edit can't share a path someone holds exclusively (orchestrator,
        // 2026-11-02, #15277).
        config.leases = committed_lease_policy(&repo)?;
        // A role or a turn is a name, never a path; read as a path under a
        // subfolder root it would quietly become `sub/run:x`.
        if opts.new && aliases_named(target) {
            let well_formed = ["role:", "run:"].iter().any(|p| {
                target
                    .strip_prefix(p)
                    .is_some_and(crate::msg::is_lower_name)
            });
            let hint = if well_formed && !repo.prefix.is_empty() && !aliases_named(&repo.prefix) {
                format!(
                    "claim the {} without --new (`{BIN} claim {target}`), or the path in this \
                     graph as `./{target}`",
                    if target.starts_with("run:") {
                        "turn"
                    } else {
                        "role"
                    }
                )
            } else if well_formed {
                format!("claim it without --new: `{BIN} claim {target}`")
            } else {
                "a role or a turn is `role:<name>` or `run:<name>`, lowercase; \
                 a path in a subfolder graph is written `./<path>`"
                    .to_string()
            };
            return Err(GraphError::usage(
                Code::MsgBadId,
                format!("{target} is a role or a turn, not a path"),
            )
            .with_hint(hint));
        }
        // A claim refuses rather than guess (a release of a turn taken
        // earlier still works).
        refuse_ambiguous(&config, target)?;
        let mut resolved = if opts.mode.is_some() {
            repo.canonical_target(target, opts.node.as_deref())?
        } else if opts.new {
            repo.path_target(target)?
        } else if opts.exact {
            repo.resolve(target)?
        } else {
            repo.resolve_claim_target(target)?
        };
        // A path that is a node's file holds that node: its lease is keyed
        // on the node's id too.
        // In whichever graph has the file as a node: one opened from
        // another graph is the same node.
        if resolved.qid.is_none() && !resolved.folder && !is_named_scope(&resolved.scope) {
            let other = repo.graph_of(&resolved.scope)?;
            let graph = other.as_ref().unwrap_or(&repo);
            if graph.node_file(&resolved.scope) {
                let config = config::load(&graph.root)?;
                let index = index::build(&graph.root, &config)?;
                resolved.qid = index
                    .nodes
                    .iter()
                    .find(|n| graph.scope_for(&n.path) == resolved.scope)
                    .map(|n| n.qid.clone());
                resolved.node_key = resolved
                    .qid
                    .as_deref()
                    .map(|q| graph.node_key(&resolved.scope, q));
            }
        }
        // The config's mode; a host's `--mode` can only make it stricter, so
        // nobody can ask for a shared lease on a node, a role, a turn or a
        // path the config keeps exclusive.
        let computed = repo.lease_mode(&config, &resolved, opts.exclusive);
        let mode = match opts.mode {
            Some(Mode::Exclusive) => Mode::Exclusive,
            _ => computed,
        };
        check_scope_text(&resolved.scope)?;
        if repo.node_folder_lease(&resolved)? {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "a lease can't hold a node folder wholesale",
            )
            .with_hint("claim the nodes you'll edit by id, or the files inside the folder"));
        }
        if repo.whole_graph(&resolved.scope) {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "a lease can't cover the whole graph or repository",
            )
            .with_hint("claim the nodes, files or folders you'll edit"));
        }
        let branch = lease_branch(&repo, &config.leases, &resolved, opts)?;
        let Target {
            scope,
            label,
            missing,
            node,
            qid,
            node_key,
            ..
        } = resolved;
        let named = named_actor(actor.as_ref());
        let actor = repo.actor(actor);
        crate::msg::check_actor(&actor)?;
        // Every holder is named to other actors, so it needs a mailbox's
        // name: no hidden characters to forge text with.
        crate::msg::check_actor_mailbox(&actor, named)?;
        Ok(Plan {
            repo,
            scope,
            label,
            missing,
            node,
            qid,
            node_key,
            mode,
            actor,
            target: target.to_string(),
            new: opts.new,
            exclusive: opts.exclusive,
            canonical: opts.mode.is_some(),
            branch,
        })
    }
}

/// Where `repo`'s path leases hold: `git config mdkg.leaseScope` in the
/// repository's own config (so a clone can choose without a tracked
/// change), else the committed `coordination.leases.scope`.
fn lease_scope(
    repo: &Repo,
    policy: &config::LeasePolicy,
) -> Result<config::LeaseScope, GraphError> {
    match mdkg_git::lease_scope(&repo.top) {
        None => Ok(policy.scope),
        Some(name) => config::LeaseScope::named(&name).ok_or_else(|| {
            GraphError::failure(
                Code::ConfigInvalid,
                "git config mdkg.leaseScope must be `repository` or `branch`",
            )
            .with_hint("set it with `git config mdkg.leaseScope branch`, or unset it")
        }),
    }
}

/// The branch the worktree at `top` has checked out, without
/// `refs/heads/`: `None` for a detached HEAD, or when git can't say.
pub(crate) fn checked_out_branch(top: &Path) -> Option<String> {
    let out = git::command(top)
        .args(["symbolic-ref", "-q", "HEAD"])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?;
    name.trim()
        .strip_prefix("refs/heads/")
        .filter(|b| !b.is_empty() && !hidden_characters(b))
        .map(str::to_string)
}

/// Whether a lease on `target` may hold its scope on one branch: a path,
/// never a node, a role, a turn, or anything holding a graph's files,
/// which settle on one branch whatever the claimant's.
fn branch_scopable(repo: &Repo, target: &Target) -> bool {
    target.qid.is_none()
        && target.node_key.is_none()
        && target.node.is_none()
        && !is_named_scope(&target.scope)
        && !target.scope.overlaps(&repo.graph_scope())
        && !target
            .scope
            .as_str()
            .split('/')
            .any(|s| s.eq_ignore_ascii_case(".mdkg"))
}

/// The branch a claim of `target` holds its scope on: the claimant
/// worktree's (or, for a host, the worker's `--branch`) where the
/// repository scopes path leases by branch, else none.
fn lease_branch(
    repo: &Repo,
    policy: &config::LeasePolicy,
    target: &Target,
    opts: &ClaimOptions,
) -> Result<Option<String>, GraphError> {
    if !branch_scopable(repo, target) {
        return Ok(None);
    }
    if lease_scope(repo, policy)? != config::LeaseScope::Branch {
        return Ok(None);
    }
    if opts.mode.is_some() {
        if let Some(b) = &opts.branch {
            refuse_hidden(b)?;
        }
        return Ok(opts.branch.clone());
    }
    Ok(checked_out_branch(&repo.top))
}

/// Grants a planned claim in the local store.
fn claim_local(plan: Plan) -> Result<Report<ClaimData>, GraphError> {
    let mut store = plan.repo.store()?;
    let now = now_ms();
    plan.repo.observe(&mut store, now)?;
    {
        let state = store.coordinator().map_err(store_error)?;
        claim_checks(&state, &plan)?;
    }
    // A turn isn't re-entrant: claiming one you hold is refused, not a
    // renewal, so two launchers acting as one actor never share it
    // (orchestrator, 2026-10-21). The coordinator refuses the second
    // claim inside its transaction; `renew` extends a turn.
    let turn = plan.scope.as_str().starts_with("run:");
    // Your own turn, as the refusing transaction saw it.
    let mut own_turn: Option<(LeaseId, u64)> = None;
    let mut capped = false;
    let outcome = store.execute_with(|c| {
        if turn {
            own_turn = own_lease(c, &plan.actor, &plan.scope).map(|l| (l.id.clone(), l.deadline));
        }
        let command = claim_command(c, &plan, now);
        if matches!(command, Command::Claim { .. }) && at_lease_cap(c, &plan.actor, now) {
            capped = true;
            // Rolls the transaction back; `capped` says why.
            return Err(mdkg_coord_sqlite::StoreError::Schema("capped".into()));
        }
        Ok(command)
    });
    if capped {
        return Err(lease_cap_error());
    }
    let outcome = outcome.map_err(store_error)?;
    let state = store.coordinator().map_err(store_error)?;
    let (lease, renewed) = claim_granted(&state, &plan, &outcome, own_turn.as_ref())?;
    claim_finish(&state, &plan, lease, renewed)
}

/// The checks a claim of `plan` makes against your own leases before
/// anything is granted.
fn claim_checks(state: &Coordinator, plan: &Plan) -> Result<(), GraphError> {
    let Plan {
        repo,
        scope,
        label,
        actor,
        target,
        new,
        exclusive,
        canonical,
        ..
    } = plan;
    let (target, new, exclusive, canonical) = (target.as_str(), *new, *exclusive, *canonical);
    // A lease an older build granted on a scope this one refuses
    // isn't renewed by claiming inside it: it lapses.
    if let Some(l) = covering_lease(state, actor, scope)
        && !repo.lease_still_granted(&l.scope)
    {
        return Err(GraphError::conflict(
            Code::CoordLeaseHeld,
            format!(
                "your lease {} covers this, on a scope this build doesn't lease",
                l.id
            ),
        )
        .with_hint(format!(
            "release it (`{BIN} release {}`) or let it lapse, then claim again",
            l.id
        )));
    }
    if let Some(l) = covering_lease(state, actor, scope)
        && exclusive
        && l.mode == Mode::Shared
    {
        return Err(GraphError::conflict(
            Code::CoordLeaseHeld,
            format!(
                "you hold {} on {} shared, so it can't become exclusive",
                l.id, l.scope
            ),
        )
        .with_hint(retry_hint(repo, &l.scope, canonical, target, new, true)));
    }
    // A wider claim over your own narrower lease: refused in every
    // mode, as the coordinator refuses it for exclusive leases.
    if covering_lease(state, actor, scope).is_none()
        && let Some(h) = state
            .holders(scope)
            .find(|l| l.owner == *actor && l.scope != *scope)
    {
        return Err(GraphError::conflict(
            Code::CoordLeaseHeld,
            format!("you hold {} on {}, inside {label}", h.id, h.scope),
        )
        .with_hint(narrower_hint(retry_hint(
            repo, &h.scope, canonical, target, new, exclusive,
        ))));
    }
    Ok(())
}

/// The command that grants `plan` from the current state `c`: a renewal
/// of your lease covering it, or a new claim. A turn is always a claim.
fn claim_command(c: &Coordinator, plan: &Plan, now: u64) -> Command {
    let turn = plan.scope.as_str().starts_with("run:");
    // The node held under another path, by anyone, isn't renewed
    // by a lease that happens to cover this one:
    // the claim goes to the coordinator, which refuses it.
    let elsewhere = plan
        .node_key
        .as_deref()
        .and_then(|k| c.node_holder(k))
        .is_some_and(|h| h.scope != plan.scope && h.deadline > now);
    // A lease you hold on another branch (your worktree switched) isn't
    // renewed for this one, nor a branch's for a claim on every branch
    // (a detached checkout): the claim asks for its own (Terra #21585 F2).
    let covering = covering_lease_where(c, &plan.actor, &plan.scope, |l| {
        l.holds_on(plan.branch.as_deref())
    })
    .filter(|_| !turn && !elsewhere);
    match covering {
        Some(l) => Command::Heartbeat {
            owner: plan.actor.clone(),
            lease: l.id.clone(),
            now,
        },
        None => Command::Claim {
            owner: plan.actor.clone(),
            scope: plan.scope.clone(),
            mode: plan.mode,
            now,
            request: None,
            node: plan.node_key.clone(),
            branch: plan.branch.clone(),
        },
    }
}

/// The lease a claim of `plan` was granted, and whether it was a renewal,
/// or the refusal as `claim` reports it. `own_turn` is your turn on the
/// scope as the refusing transaction saw it.
fn claim_granted(
    state: &Coordinator,
    plan: &Plan,
    outcome: &Outcome,
    own_turn: Option<&(LeaseId, u64)>,
) -> Result<(Lease, bool), GraphError> {
    let Plan {
        repo,
        scope,
        label,
        qid,
        node_key,
        mode,
        actor,
        target,
        new,
        exclusive,
        canonical,
        branch,
        ..
    } = plan;
    let (target, new, exclusive, canonical, mode) =
        (target.as_str(), *new, *exclusive, *canonical, *mode);
    let branch = branch.as_deref();
    let (lease, renewed) = match outcome {
        Outcome::Lease { lease } => (state.lease(lease).cloned(), false),
        Outcome::Deadline { .. } => (
            covering_lease_where(state, actor, scope, |l| l.holds_on(branch)).cloned(),
            true,
        ),
        Outcome::Refused {
            error: CoordError::Held(_),
        } => {
            if let Some((id, deadline)) = own_turn {
                return Err(GraphError::conflict(
                    Code::CoordLeaseHeld,
                    format!(
                        "{actor} already holds the turn {label} until {} ({id})",
                        rfc3339(*deadline)
                    ),
                )
                .with_hint(format!(
                    "a turn isn't re-entrant: if this process holds it, `{BIN} renew` \
                     extends it; otherwise another process acting as {actor} holds it, so \
                     pick other work or wait (release it only if that holder is gone)"
                )));
            }
            // The node held under another path first: a lease covering
            // this path can't explain the refusal then.
            let holder = node_key
                .as_deref()
                .and_then(|k| state.node_holder(k))
                .filter(|h| h.scope != *scope)
                .or_else(|| {
                    state
                        .blockers(scope, mode, branch)
                        .find(|l| l.owner != *actor || l.scope != *scope)
                });
            return Err(match holder {
                Some(h) if h.owner == *actor && !h.scope.overlaps(scope) => GraphError::conflict(
                    Code::CoordLeaseHeld,
                    format!("you hold {label} under another path ({})", h.id),
                )
                .at(h.scope.to_string(), None)
                .with_hint(format!(
                    "release that lease (`{BIN} release {}`), then claim it again",
                    h.id
                )),
                Some(h) if h.owner == *actor => GraphError::conflict(
                    Code::CoordLeaseHeld,
                    format!("you hold {} on {}, inside {label}", h.id, h.scope),
                )
                .with_hint(narrower_hint(retry_hint(
                    repo, &h.scope, canonical, target, new, exclusive,
                ))),
                Some(h) => {
                    // Its holder and scope are data, never prose:
                    // another actor chose them.
                    let mut d = Diagnostic::error(
                        Code::CoordLeaseHeld,
                        format!(
                            "{label} is held by another lease until {} ({})",
                            rfc3339(h.deadline),
                            h.id
                        ),
                    )
                    .at(h.scope.to_string(), None)
                    .held_by([h.owner.clone()])
                    .with_hint("pick other work, or wait for the lease to end");
                    if let Some(q) = qid {
                        d = d.for_node(q.clone());
                    }
                    GraphError::new(ErrorKind::Conflict, d)
                }
                None => GraphError::conflict(Code::CoordLeaseHeld, format!("{label} is held"))
                    .with_hint("pick other work, or wait for the lease to end"),
            });
        }
        Outcome::Refused { error } => return Err(refusal(error)),
        other => {
            return Err(GraphError::failure(
                Code::CoordStoreFailed,
                format!("unexpected outcome {other:?}"),
            ));
        }
    };
    let lease = lease
        .ok_or_else(|| GraphError::failure(Code::CoordStoreFailed, "the new lease is missing"))?;
    Ok((lease, renewed))
}

/// The report of a granted claim of `plan`: the lease, and the notes
/// this worktree adds.
fn claim_finish(
    state: &Coordinator,
    plan: &Plan,
    lease: Lease,
    renewed: bool,
) -> Result<Report<ClaimData>, GraphError> {
    let Plan {
        repo,
        scope,
        label,
        missing,
        node,
        actor,
        target,
        new,
        canonical,
        ..
    } = plan;
    let (target, canonical) = (target.as_str(), *canonical);
    // A host leaves the notes about the worktree to the worker.
    let sync = if canonical {
        Vec::new()
    } else {
        missing_versions(repo, state, scope)?
    };
    let mut diagnostics = if canonical {
        Vec::new()
    } else {
        claim_notes(state, scope, label, target, *missing && !*new, *node)
    };
    diagnostics.extend(co_hold_notices(state, &lease));
    // The shared-lease merge advisory (#16942): a co-holder's branch that conflicts with
    // yours under a shared lease, warned at the claim.
    if !canonical {
        let mut changes = Changes::default();
        diagnostics.extend(
            lease_merge_conflicts(repo, actor, state, &lease, &mut changes)
                .iter()
                .map(merge_conflict_notice),
        );
    }
    let mut record = lease_record_in(state, &lease);
    name_store_scopes(repo, [(record.scope.clone().as_str(), &mut record.node)]);
    let mut report = Report::ok(ClaimData {
        lease: record,
        renewed,
        sync: sync.clone(),
        claims: Vec::new(),
    });
    if !sync.is_empty() {
        diagnostics.push(sync_note(sync.len(), label, target));
    }
    if !diagnostics.is_empty() {
        report = report.with(diagnostics);
    }
    Ok(report)
}

pub fn claim(
    root: &Path,
    actor: Option<String>,
    target: &str,
    opts: ClaimOptions,
    json: bool,
) -> u8 {
    let report = claim_report_with(root, actor, target, opts)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    output::emit("claim", json, report, |d| {
        let verb = if d.renewed { "renewed" } else { "claimed" };
        let shared = if d.lease.mode == "shared" {
            " shared"
        } else {
            ""
        };
        // A host's answer, maybe an older build's: escaped.
        println!(
            "{}",
            output::shown(&format!(
                "{verb}{shared} {}{} as {} until {} ({})",
                d.lease.scope,
                NodeName::suffix(d.lease.node.as_ref()),
                d.lease.owner,
                d.lease.until,
                d.lease.id
            ))
        );
    })
}

/// `claim` of several targets: all or none, one lease per target, claimed
/// in the order given. Every target is resolved first, and one another
/// actor holds in this store refuses them all before any is granted. Then
/// every grant and renewal is applied in one store transaction, so a
/// refusal, a failed write or a crash between two of them leaves nothing
/// granted and nothing renewed.
fn claim_many_report(
    root: &Path,
    actor: Option<String>,
    targets: &[String],
    opts: &ClaimOptions,
) -> Result<Report<ClaimData>, GraphError> {
    if opts.mode.is_some() || opts.node.is_some() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "--mode and --node take one target",
        ));
    }
    let plans = targets
        .iter()
        .map(|t| plan_claim(root, actor.clone(), t, opts))
        .collect::<Result<Vec<Plan>, GraphError>>()?;
    // Beside a lease authority, one target at a time (#17108): its grants
    // can't join this store's transaction.
    if !matches!(authority::find(&plans[0].repo), Ok(None)) {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "with a lease authority configured, claim takes one target at a time",
        )
        .with_hint(format!(
            "claim each target on its own: `{BIN} claim <target>`"
        )));
    }
    let count = plans.len();
    let none_of = |e: GraphError, target: &str| {
        let mut d = *e.diagnostic;
        d.message = format!("{target}: {}", d.message);
        let undo = format!("none of the {count} targets was claimed");
        d.hint = Some(match d.hint.take() {
            Some(h) => format!("{h}; {undo}"),
            None => undo,
        });
        GraphError::new(e.kind, d)
    };
    // Two targets naming one lease (a turn twice, a node by two
    // spellings) or overlapping (a file and the folder around it, in
    // either order): refused up front, since a refusal's explanation is
    // read after the rollback, which no longer has the first one's grant.
    for (i, plan) in plans.iter().enumerate() {
        if let Some(other) = plans[..i].iter().find(|o| {
            o.scope.overlaps(&plan.scope)
                || (plan.node_key.is_some() && o.node_key == plan.node_key)
        }) {
            let (a, b) = (output::shown(&other.target), output::shown(&plan.target));
            let same = other.scope == plan.scope
                || (plan.node_key.is_some() && other.node_key == plan.node_key);
            let (why, fix) = if same {
                (
                    format!("{a} and {b} name the same lease"),
                    "name each target once",
                )
            } else if other.scope.contains(&plan.scope) {
                (
                    format!("{b} is inside {a}"),
                    "claim the folder or what's inside it, not both",
                )
            } else {
                (
                    format!("{a} is inside {b}"),
                    "claim the folder or what's inside it, not both",
                )
            };
            return Err(GraphError::usage(Code::UsageInvalidArguments, why)
                .with_hint(format!("{fix}; none of the {count} targets was claimed")));
        }
    }
    refuse_held(&plans)?;
    let mut store = plans[0].repo.store()?;
    let now = now_ms();
    plans[0].repo.observe(&mut store, now)?;
    // Each command is built from the state the ones before it left, inside
    // the transaction; the first from the state just read, as `claim`
    // checks it.
    let build = |c: &Coordinator, plan: &Plan| -> Result<Command, GraphError> {
        claim_checks(c, plan)?;
        let command = claim_command(c, plan, now);
        if matches!(command, Command::Claim { .. }) && at_lease_cap(c, &plan.actor, now) {
            return Err(lease_cap_error());
        }
        Ok(command)
    };
    let first = {
        let state = store.coordinator().map_err(store_error)?;
        build(&state, &plans[0]).map_err(|e| none_of(e, &plans[0].target))?
    };
    // A check that refuses a later target ends the transaction with a
    // command the coordinator always refuses (a renewal of no lease),
    // which rolls back every one before it.
    let mut stopped: Option<(usize, GraphError)> = None;
    let mut next = 1;
    let chain = store
        .execute_chain(&first, |c, last| {
            if last.error().is_some() || next == count {
                return None;
            }
            let i = next;
            next += 1;
            match build(c, &plans[i]) {
                Ok(command) => Some(command),
                Err(e) => {
                    stopped = Some((i, e));
                    Some(Command::Heartbeat {
                        owner: plans[i].actor.clone(),
                        lease: LeaseId(String::new()),
                        now,
                    })
                }
            }
        })
        .map_err(store_error)?;
    if let Some((i, e)) = stopped {
        return Err(none_of(e, &plans[i].target));
    }
    let state = store.coordinator().map_err(store_error)?;
    if let Some((i, outcome)) = chain
        .outcomes
        .iter()
        .enumerate()
        .find(|(_, o)| o.error().is_some())
    {
        let plan = &plans[i];
        // A turn you already hold, as `claim` reports it.
        let own_turn = plan
            .scope
            .as_str()
            .starts_with("run:")
            .then(|| own_lease(&state, &plan.actor, &plan.scope))
            .flatten()
            .map(|l| (l.id.clone(), l.deadline));
        let e = match claim_granted(&state, plan, outcome, own_turn.as_ref()) {
            Err(e) => e,
            Ok(_) => GraphError::failure(
                Code::CoordStoreFailed,
                format!("the claim of {} was refused", plan.target),
            ),
        };
        return Err(none_of(e, &plan.target));
    }
    if !chain.recorded || chain.outcomes.len() != count {
        return Err(GraphError::failure(
            Code::CoordStoreFailed,
            "the claims weren't recorded",
        ));
    }
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut claims = Vec::new();
    // Every target is held from here on: what fails now is a note, never
    // a refusal that reads as all or none.
    for (plan, outcome) in plans.iter().zip(&chain.outcomes) {
        let (lease, renewed) = claim_granted(&state, plan, outcome, None).map_err(|e| {
            GraphError::failure(
                Code::CoordStoreFailed,
                format!(
                    "all {count} targets were claimed, but the lease of {} couldn't be read: {}",
                    plan.target, e.diagnostic.message
                ),
            )
            .with_hint(format!("see them with `{BIN} status --mine`"))
        })?;
        let report = match claim_finish(&state, plan, lease.clone(), renewed) {
            Ok(report) => report,
            Err(e) => {
                let mut record = lease_record_in(&state, &lease);
                name_store_scopes(
                    &plan.repo,
                    [(record.scope.clone().as_str(), &mut record.node)],
                );
                Report::ok(ClaimData {
                    lease: record,
                    renewed,
                    sync: Vec::new(),
                    claims: Vec::new(),
                })
                .with([Diagnostic::warning(
                    Code::CoordStoreFailed,
                    format!(
                        "{} was claimed, but this worktree's notes couldn't be read: {}",
                        plan.target, e.diagnostic.message
                    ),
                )])
            }
        };
        for n in report.diagnostics {
            if !diagnostics.contains(&n) {
                diagnostics.push(n);
            }
        }
        if let Some(data) = report.data {
            claims.push(ClaimedTarget {
                target: plan.target.clone(),
                lease: data.lease,
                renewed: data.renewed,
                sync: data.sync,
            });
        }
    }
    let first = &claims[0];
    let data = ClaimData {
        lease: first.lease.clone(),
        renewed: first.renewed,
        sync: first.sync.clone(),
        claims,
    };
    Ok(Report::ok(data).with(diagnostics))
}

/// Refuses a claim of several targets when another actor's live lease in
/// this store blocks one, before any is granted. A target the lease
/// authority holds is left to the authority, which the claim then asks.
fn refuse_held(plans: &[Plan]) -> Result<(), GraphError> {
    let Some(first) = plans.first() else {
        return Ok(());
    };
    let local = matches!(authority::find(&first.repo), Ok(None));
    let store = first.repo.store()?;
    let state = store.coordinator().map_err(store_error)?;
    let now = now_ms();
    for plan in plans {
        if !local && !is_named_scope(&plan.scope) {
            continue;
        }
        if let Some(h) = state
            .blockers(&plan.scope, plan.mode, plan.branch.as_deref())
            .find(|l| l.owner != plan.actor && l.deadline > now)
        {
            let mut d = Diagnostic::error(
                Code::CoordLeaseHeld,
                format!(
                    "{}: {} is held by another lease until {} ({})",
                    plan.target,
                    plan.label,
                    rfc3339(h.deadline),
                    h.id
                ),
            )
            .at(h.scope.to_string(), None)
            .held_by([h.owner.clone()])
            .with_hint(format!(
                "none of the {} targets was claimed; pick other work, or wait for the lease to \
                 end",
                plans.len()
            ));
            if let Some(q) = &plan.qid {
                d = d.for_node(q.clone());
            }
            return Err(GraphError::new(ErrorKind::Conflict, d));
        }
    }
    Ok(())
}

/// `claim` of several targets; see [`claim_many_report`].
pub fn claim_many(
    root: &Path,
    actor: Option<String>,
    targets: &[String],
    opts: ClaimOptions,
    json: bool,
) -> u8 {
    let report = claim_many_report(root, actor, targets, &opts)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    output::emit("claim", json, report, |d| {
        for c in &d.claims {
            let verb = if c.renewed { "renewed" } else { "claimed" };
            let shared = if c.lease.mode == "shared" {
                " shared"
            } else {
                ""
            };
            println!(
                "{}",
                output::shown(&format!(
                    "{verb}{shared} {}{} as {} until {} ({})",
                    c.lease.scope,
                    NodeName::suffix(c.lease.node.as_ref()),
                    c.lease.owner,
                    c.lease.until,
                    c.lease.id
                ))
            );
        }
    })
}

/// The actor's most recent lease on exactly `scope`, if it has ended
/// (expired, released, or committed by `done`): releasing it again has
/// nothing left to do.
fn ended_lease<'a>(state: &'a Coordinator, actor: &str, scope: &Scope) -> Option<&'a Lease> {
    state
        .leases()
        .filter(|l| l.owner == actor && l.confirmed && l.scope == *scope)
        .last()
        .filter(|l| {
            matches!(
                l.status,
                Status::Expired | Status::Released | Status::Committed
            )
        })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseData {
    pub lease: LeaseRecord,
    /// With several targets: each one's lease, in the order given (`lease`
    /// above is the first's). Absent for one target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<ReleasedTarget>,
}

/// One target's lease in a `release` of several.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReleasedTarget {
    /// The target as given.
    pub target: String,
    /// The lease, released by this call or already ended before it.
    pub lease: LeaseRecord,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct HoldsData {
    /// Whether the actor holds an active lease covering the scope.
    pub holds: bool,
    /// That lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseRecord>,
}

/// Whether the actor holds a lease covering a canonical scope (relative to
/// the repository's top): read-only, for a worker's `done` and edits when a
/// lease authority holds its leases, and the host's answer to it.
pub(crate) fn holds_report(
    root: &Path,
    actor: Option<String>,
    scope: &str,
    node: Option<&str>,
) -> Result<Report<HoldsData>, GraphError> {
    holds_report_with(root, actor, scope, node, false, None)
}

/// [`holds_report`]; with `detail` (the `holds` command, not an edit
/// hook's check), the lease's age and its paths' state too, which cost
/// two git reads.
fn holds_report_with(
    root: &Path,
    actor: Option<String>,
    scope: &str,
    node: Option<&str>,
    detail: bool,
    asked: Option<&str>,
) -> Result<Report<HoldsData>, GraphError> {
    refuse_hidden(scope)?;
    let repo = Repo::open(root)?;
    // A bare node id would read as a top-level path and quietly answer no.
    if !scope.contains('/')
        && !repo.top.join(scope).exists()
        && let Ok(t) = repo.resolve(scope)
        && t.node.is_some()
    {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!("`{scope}` is a node id; `holds` takes a scope"),
        )
        // The file's path is data (`path`): a node file's name is anyone's.
        .with_hint("name the node's file (in `path`), relative to the repository's top")
        .at(t.scope.to_string(), None));
    }
    // With `--node`, the same checks a claim gets: the
    // node where this graph has it, and nobody holding it elsewhere.
    let target = repo.canonical_target(scope, node)?;
    let actor = repo.actor(actor);
    if !is_named_scope(&target.scope)
        && let Some(authority) = authority::find(&repo)?
    {
        let mut options = vec![("--as", Some(actor))];
        if let Some(q) = &target.qid {
            options.push(("--node", Some(q.clone())));
        }
        match asked {
            Some(b) => options.push(branch_flag(Some(b.to_string()))),
            None => options.extend(branch_option(&repo)?),
        }
        let mut report: Report<HoldsData> =
            authority.delegate(root, "holds", &[target.scope.to_string()], &options)?;
        // A host that can't take `--branch` judged the lease on its own
        // checkout: one it reports held on another branch is checked here.
        if let Some(d) = report.data.as_mut()
            && d.lease
                .as_ref()
                .is_some_and(|l| !held_on_this_branch(&repo, asked, l.branch.as_deref()))
        {
            d.holds = false;
            d.lease = None;
        }
        return Ok(report);
    }
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let elsewhere = target
        .node_key
        .as_deref()
        .and_then(|k| state.node_holder(k))
        .is_some_and(|h| h.scope != target.scope && h.deadline > now);
    // A lease this build wouldn't grant holds nothing for an edit: it
    // lapses.
    let here = std::cell::OnceCell::new();
    let lease = covering_lease_where(&state, &actor, &target.scope, |l| {
        l.holds_on(here.get_or_init(|| asking_branch(&repo, asked)).as_deref())
    })
    .filter(|l| l.deadline > now && !elsewhere && repo.lease_still_granted(&l.scope));
    let mut record = lease.map(|l| {
        if !detail {
            return lease_record_in(&state, l);
        }
        let path = (is_path_lease(l) && !is_named_scope(&l.scope)).then(|| {
            let mut changes = Changes::default();
            let clean = changes.strictly_clean(&repo.top, l);
            PathState {
                clean,
                // As status: no promise where another actor works (#18144).
                committed: clean == Some(true)
                    && owns_worktree(&repo, &actor)
                    && changes.committed_since(&repo.top, &l.scope, granted_at(&state, l), now)
                        == Some(true),
            }
        });
        with_path_state(lease_record_in(&state, l), &state, l, path, now)
    });
    if let Some(r) = record.as_mut() {
        let scope = r.scope.clone();
        name_store_scopes(&repo, [(scope.as_str(), &mut r.node)]);
    }
    Ok(Report::ok(HoldsData {
        holds: record.is_some(),
        lease: record,
    }))
}

/// Whether a lease held on `branch` (none: on every branch) holds in
/// `repo`'s checkout: a worktree switched to another branch, or detached,
/// claims again.
fn held_on_this_branch(repo: &Repo, asked: Option<&str>, branch: Option<&str>) -> bool {
    branch.is_none_or(|b| asking_branch(repo, asked).as_deref() == Some(b))
}

/// `holds`; `branch` is the hidden `--branch` a lease authority's client
/// names its checkout's branch with (empty: `--detached`).
pub fn holds(
    root: &Path,
    actor: Option<String>,
    scope: &str,
    node: Option<&str>,
    branch: Option<&str>,
    json: bool,
) -> u8 {
    if let Some(b) = branch
        && let Err(e) = refuse_hidden(b)
    {
        return output::emit("holds", json, Report::<HoldsData>::failed(e), |_| {});
    }
    let report = holds_report_with(root, actor, scope, node, true, branch)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    // The claim that takes it: `holds` names a scope from the repository's
    // top, `claim` a path from the graph root, so it's translated, and a
    // name a shell would read as more than a word is `<path>` (Terra
    // #17810).
    let claim = Repo::open(root).ok().map(|repo| {
        // `./` only where it's needed: a name with no `/` could read as a
        // node, and one starting with `-` as a flag.
        let target = safe_target(&repo, &Scope::new(scope));
        let target = match target.strip_prefix("./") {
            Some(rest) if rest.contains('/') && !rest.starts_with('-') => rest.to_owned(),
            _ if target.starts_with('-') => format!("./{target}"),
            _ => target,
        };
        format!("; `{} claim {target}` takes it", crate::BIN)
    });
    output::emit("holds", json, report, |d| match &d.lease {
        Some(l) => println!(
            "{}",
            output::shown(&format!(
                "held: {}{} ({}{}{})",
                l.scope,
                NodeName::suffix(l.node.as_ref()),
                l.id,
                l.age_ms
                    .map_or(String::new(), |ms| format!(", age {}", age_text(ms))),
                paths_text(l)
            ))
        ),
        // Still exit 0: a no is an answer, and lease authorities read
        // `data.holds` (e2e UX).
        None => println!(
            "{}",
            output::shown(&format!(
                "not held: {scope}{}",
                claim.as_deref().unwrap_or_default()
            ))
        ),
    })
}

/// The actor's exclusive lease on `leaf` (the node `node`, when given) at
/// the lease authority, or `None` when it holds none there (or holds it
/// only shared). An error when the authority can't be asked or understood.
pub(crate) fn authority_exclusive_hold(
    repo: &Repo,
    actor: &str,
    leaf: &Scope,
    node: Option<&str>,
) -> Result<Option<LeaseRecord>, GraphError> {
    let report = holds_report(&repo.root, Some(actor.to_string()), leaf.as_str(), node)?;
    if let (Some(kind), Some(d)) = (
        report.failure,
        report
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error),
    ) {
        return Err(GraphError::new(kind, d.clone()));
    }
    Ok(report
        .data
        .and_then(|d| d.lease.filter(|_| d.holds))
        .filter(|l| l.mode == "exclusive"))
}

/// Before `done` or an edit takes a node's local lease: with a lease
/// authority, the actor must hold the node there, exclusively, under the
/// path the authority has it; the edit itself stays local. Fails closed.
/// Returns the authority's lease, when there is one.
pub(crate) fn require_authority_hold(
    repo: &Repo,
    actor: &str,
    leaf: &Scope,
    node: Option<&str>,
) -> Result<Option<LeaseRecord>, GraphError> {
    if is_named_scope(leaf) || authority::find(repo)?.is_none() {
        return Ok(None);
    }
    let report = holds_report(&repo.root, Some(actor.to_string()), leaf.as_str(), node)?;
    if let (Some(kind), Some(d)) = (
        report.failure,
        report
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error),
    ) {
        return Err(GraphError::new(kind, d.clone()));
    }
    // Held, and exclusively: a shared lease never covers an edit.
    match report.data.and_then(|d| d.lease.filter(|_| d.holds)) {
        Some(l) if l.mode == "exclusive" => return Ok(Some(l)),
        Some(l) => {
            // Paths are data (`path`), and commands in hints name only ids:
            // a file or branch name is anyone's to choose.
            // By path, not the authority's id, which may name another
            // lease here.
            return Err(GraphError::failure(
                Code::CoordAuthorityFailed,
                format!("the lease authority says {actor} holds it only shared"),
            )
            .at(leaf.to_string(), None)
            .with_hint(format!(
                "release it (`{BIN} release {}`), then `{BIN} claim --exclusive {}`",
                safe_target(repo, &Scope::new(l.scope.as_str())),
                claim_ref(repo, node, leaf)
            )));
        }
        None => {}
    }
    Err(GraphError::failure(
        Code::CoordAuthorityFailed,
        format!("the lease authority says {actor} doesn't hold it"),
    )
    .at(leaf.to_string(), None)
    .with_hint(format!(
        "claim it first: `{BIN} claim {}`",
        claim_ref(repo, node, leaf)
    )))
}

/// How a hint names what to claim: the node's id, else the path.
fn claim_ref(repo: &Repo, node: Option<&str>, leaf: &Scope) -> String {
    node.map_or_else(|| safe_target(repo, leaf), output::display_ref)
}

/// A scope as a command in a hint names it, if every character is one no
/// shell reads specially; otherwise `<path>` (the path is in the
/// diagnostic's `path`): a file or branch name is anyone's to choose, and
/// a hint may be pasted into a shell.
fn safe_target(repo: &Repo, scope: &Scope) -> String {
    let t = repo.target_of(scope);
    if t.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | ':'))
    {
        t
    } else {
        "<path>".to_string()
    }
}

/// A lease id as the coordinator mints them: `lease-<digits>`, or
/// `lease-~<hex>` offline. Anything else that starts with `lease-` is a
/// node or a file when one by that name is here (`lease-plan`), and
/// otherwise a lease authority's id (whose ids start with `lease-`): name a
/// file not created yet as `./lease-notes.md`.
/// A node or path `release` names: by any handle `claim` takes (RC1 audit
/// U1), except a name shaped like a lease id (`lease-…`, an authority's
/// too), which only an exact id or path matches, so no title or prefix
/// stands in for a lease.
fn release_handle(repo: &Repo, target: &str) -> Result<Target, GraphError> {
    if target.starts_with("lease-") {
        repo.resolve(target)
    } else {
        repo.resolve_handle_target(target)
    }
}

fn is_lease_id(target: &str) -> bool {
    match target.strip_prefix("lease-") {
        Some(rest) => match rest.strip_prefix('~') {
            Some(hex) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
            None => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        },
        None => false,
    }
}

/// After an edit is recorded (`done`, goal edits): with a lease
/// authority, release there the node's own lease (`held`, as
/// [`require_authority_hold`] found it), by id, so `renew` doesn't keep it
/// forever. A folder lease covering the node stays: recording here
/// committed only the node, so the folder's other files may still hold
/// work. Best effort: a warning if it fails.
pub(crate) fn release_at_authority(
    repo: &Repo,
    actor: &str,
    leaf: &Scope,
    held: Option<&LeaseRecord>,
) -> Option<Diagnostic> {
    let held = held?;
    let scope = Scope::new(held.scope.as_str());
    if scope != *leaf {
        return Some(
            Diagnostic::new(
                Code::CoordAuthorityRemote,
                Severity::Info,
                format!(
                    "the lease authority still holds a folder lease ({}) that covers this node: \
                     release it when the folder's work is recorded",
                    output::shown(&held.id)
                ),
            )
            .at(scope.to_string(), None)
            .with_hint(format!(
                "`{BIN} release {}` (renew keeps it alive until then, if this build still \
                 grants it)",
                safe_target(repo, &scope)
            )),
        );
    }
    let warn = || {
        Diagnostic::warning(
            Code::CoordAuthorityRemote,
            format!(
                "recorded, but the lease authority may still hold its lease ({})",
                output::shown(&held.id)
            ),
        )
        .at(scope.to_string(), None)
        .with_hint(format!(
            "release it there: `{BIN} release {}`",
            safe_target(repo, &scope)
        ))
    };
    let authority = match authority::find(repo) {
        Ok(Some(a)) => a,
        Ok(None) => return None,
        Err(_) => return Some(warn()),
    };
    let released: Result<Report<ReleaseData>, GraphError> = authority.delegate(
        &repo.root,
        "release",
        std::slice::from_ref(&held.id),
        &[("--as", Some(actor.to_string()))],
    );
    match released {
        Ok(r) if r.failure.is_none() => None,
        _ => Some(warn()),
    }
}

/// What to release: a target as a person names it, or a canonical scope
/// (relative to the repository's top) as a lease authority's host is given,
/// with the `--branch` its client's checkout is on (empty: `--detached`).
#[derive(Debug, Clone, Copy)]
pub enum ReleaseTarget<'a> {
    Named(&'a str),
    Scope(&'a str, Option<&'a str>),
}

/// What `release` of one target takes: an active lease to release, or
/// your own lease on it that has already ended (nothing left to do).
enum Pick {
    Active(Lease),
    Ended(Lease),
}

/// Finds the lease `release` of `which` takes in `state`, without
/// releasing it: [`release_local`] and [`release_many_report`] share it.
fn release_pick(
    repo: &Repo,
    state: &Coordinator,
    actor: &str,
    which: ReleaseTarget<'_>,
) -> Result<Pick, GraphError> {
    let actor = actor.to_string();
    {
        let target = match which {
            ReleaseTarget::Named(t) | ReleaseTarget::Scope(t, _) => t,
        };
        let lease = if let ReleaseTarget::Scope(scope, asked) = which {
            let scope = repo.canonical_target(scope, None)?.scope;
            if own_lease(state, &actor, &scope).is_none()
                && let Some(l) = ended_lease(state, &actor, &scope)
            {
                return Ok(Pick::Ended(l.clone()));
            }
            own_lease_here(state, &actor, &scope, || asking_branch(repo, asked)).cloned()
        } else if is_lease_id(target) {
            let named = state.lease(&LeaseId(target.to_string())).cloned();
            // Your own lease that has already ended: nothing left to do.
            if let Some(l) = named.as_ref().filter(|l| {
                l.owner == actor
                    && matches!(
                        l.status,
                        Status::Expired | Status::Released | Status::Committed
                    )
            }) {
                return Ok(Pick::Ended(l.clone()));
            }
            named
        } else {
            // A node by any handle claim takes (RC1 audit U1); a path you
            // claimed with `--new` may still not exist, and a top-level one
            // then reads as an unknown node id.
            let scope = match release_handle(repo, target) {
                Ok(t) => t.scope,
                Err(e) => match repo.path_target(target) {
                    Ok(t)
                        if state
                            .leases()
                            .any(|l| l.owner == actor && l.scope == t.scope) =>
                    {
                        t.scope
                    }
                    _ => return Err(e),
                },
            };
            if let (None, Some(l)) = (
                own_lease(state, &actor, &scope),
                covering_lease(state, &actor, &scope),
            ) {
                return Err(GraphError::not_found(
                    Code::CoordNoLease,
                    format!("{target} is covered by your lease {} on {}", l.id, l.scope),
                )
                .with_hint(format!(
                    "release the whole lease with `{BIN} release {}`",
                    safe_target(repo, &l.scope)
                )));
            }
            // Your own lease that has already ended: nothing left to do.
            if own_lease(state, &actor, &scope).is_none()
                && let Some(l) = ended_lease(state, &actor, &scope)
            {
                return Ok(Pick::Ended(l.clone()));
            }
            own_lease_here(state, &actor, &scope, || checked_out_branch(&repo.top)).cloned()
        };
        let Some(lease) = lease else {
            return Err(GraphError::not_found(
                Code::CoordNoLease,
                "you hold no active lease on that",
            )
            .with_hint(format!("see your leases with `{} status`", crate::BIN)));
        };
        Ok(Pick::Active(lease))
    }
}

pub(crate) fn release_local(
    root: &Path,
    actor: Option<String>,
    which: ReleaseTarget<'_>,
) -> Result<Report<ReleaseData>, GraphError> {
    let repo = Repo::open(root)?;
    let actor = repo.actor(actor);
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let lease = match release_pick(&repo, &state, &actor, which)? {
        Pick::Ended(l) => {
            return Ok(Report::ok(ReleaseData {
                lease: lease_record(&l),
                releases: Vec::new(),
            }));
        }
        Pick::Active(l) => l,
    };
    let outcome = store
        .execute(&Command::Release {
            owner: actor.clone(),
            lease: lease.id.clone(),
            now,
        })
        .map_err(store_error)?;
    if let Some(error) = outcome.error() {
        return Err(refusal(error));
    }
    let state = store.coordinator().map_err(store_error)?;
    let released = state.lease(&lease.id).unwrap_or(&lease);
    Ok(Report::ok(ReleaseData {
        lease: lease_record(released),
        releases: Vec::new(),
    }))
}

/// The node or file `target` names for `release`, if any. A handle naming
/// several nodes refuses (exit 7), and so does a name that is both a lease
/// id and a node or file here: `release` of one target and of several
/// both refuse it (#18223).
fn release_thing(repo: &Repo, target: &str) -> Result<Option<Target>, GraphError> {
    let thing = match release_handle(repo, target) {
        Ok(t) => Some(t),
        Err(e) if e.kind == ErrorKind::Ambiguous => return Err(e),
        Err(_) => None,
    };
    if is_lease_id(target) && thing.is_some() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            format!("`{target}` is both a lease id and a node or file here"),
        )
        .with_hint(format!(
            "release the node or file as `./{target}`, or the lease by its scope"
        )));
    }
    Ok(thing)
}

/// `release` of one target, locally or through the lease authority.
fn release_report(
    root: &Path,
    actor: Option<String>,
    which: ReleaseTarget<'_>,
) -> Result<Report<ReleaseData>, GraphError> {
    let repo = Repo::open(root)?;
    let target = match which {
        ReleaseTarget::Named(t) | ReleaseTarget::Scope(t, _) => t,
    };
    // Its refusals quote the target: never include hidden characters.
    refuse_hidden(target)?;
    if let ReleaseTarget::Scope(_, Some(b)) = which {
        refuse_hidden(b)?;
    }
    // A role or a turn is named alone (`./role:x` is a path): always
    // local, whatever the authority's state.
    if !target.contains('/') && is_named_scope(&Scope::new(target)) {
        return release_local(root, actor, which);
    }
    let authority = authority::find(&repo)?;
    let ReleaseTarget::Named(target) = which else {
        // A canonical scope (as a host is asked): through the authority
        // when there is one, with the branch it was asked on.
        let scope = repo.canonical_target(target, None)?.scope;
        let branch = match which {
            ReleaseTarget::Scope(_, Some(b)) => Some(branch_flag(Some(b.to_string()))),
            _ => branch_option(&repo)?,
        };
        return match authority {
            Some(a) if !is_named_scope(&scope) => {
                let mut options = vec![
                    ("--scope", Some(scope.to_string())),
                    ("--as", Some(repo.actor(actor))),
                ];
                options.extend(branch);
                a.delegate(root, "release", &[], &options)
            }
            _ => release_local(root, actor, which),
        };
    };
    // A lease id, or a node (by any handle claim takes: RC1 audit U1)
    // or a path. A name shaped like a lease id, an authority's
    // `lease-remote-1` included, is read exactly, so no title can
    // shadow it; a handle naming several nodes refuses (exit 7).
    let thing = release_thing(&repo, target)?;
    let looks_like_id = is_lease_id(target) || (target.starts_with("lease-") && thing.is_none());
    let Some(authority) = authority else {
        return release_local(root, actor, which);
    };
    let actor = repo.actor(actor);
    if looks_like_id {
        // Ids are numbered by each store: one that names an active
        // lease of yours here, of any kind, is ambiguous with the
        // authority's. Anyone else's, or one that ended, can't be what
        // you meant (releasing it here would change nothing).
        let mut store = repo.store()?;
        repo.expire_due(&mut store, now_ms())?;
        let state = store.coordinator().map_err(store_error)?;
        if let Some(l) = state
            .lease(&LeaseId(target.to_string()))
            .filter(|l| l.owner == actor && l.status == Status::Active)
        {
            let hint = if is_role_or_turn(&l.scope) {
                format!(
                    "release a role or a turn by name (`{BIN} release {}`), and the \
                     authority's leases by their paths",
                    l.scope
                )
            } else {
                format!(
                    "release the authority's lease by its path (`{BIN} release <path>`); \
                 this worktree's own lease ends when it expires"
                )
                .to_string()
            };
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                format!(
                    "`{target}` is your lease on {} here ({}), and may also be a lease at \
                     the authority",
                    l.scope,
                    status_name(l.status)
                ),
            )
            .with_hint(hint));
        }
        return authority.delegate(
            root,
            "release",
            &[target.to_string()],
            &[("--as", Some(actor))],
        );
    }
    let scope = match thing {
        Some(t) => t.scope,
        None => repo.path_target(target)?.scope,
    };
    if is_named_scope(&scope) {
        return release_local(root, Some(actor), which);
    }
    let mut options = vec![("--scope", Some(scope.to_string())), ("--as", Some(actor))];
    options.extend(branch_option(&repo)?);
    authority.delegate(root, "release", &[], &options)
}

pub fn release(root: &Path, actor: Option<String>, which: ReleaseTarget<'_>, json: bool) -> u8 {
    let report = release_report(root, actor, which)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    output::emit("release", json, report, |d| {
        println!(
            "{}",
            output::shown(&format!("released {} ({})", d.lease.scope, d.lease.id))
        );
    })
}

/// `release` of several targets: all or none.
/// Every target is resolved first, as `release <target>` would, and one
/// that names no lease of yours refuses them all; then every lease is
/// released in one store transaction, which a refusal rolls back whole. A
/// target whose lease of yours already ended is listed, as `release`
/// lists it. Beside a lease authority, one target at a time, as `claim`.
fn release_many_report(
    root: &Path,
    actor: Option<String>,
    targets: &[String],
) -> Result<Report<ReleaseData>, GraphError> {
    for t in targets {
        refuse_hidden(t)?;
    }
    let repo = Repo::open(root)?;
    if !matches!(authority::find(&repo), Ok(None)) {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "with a lease authority configured, release takes one target at a time",
        )
        .with_hint(format!(
            "release each target on its own: `{BIN} release <target>`"
        )));
    }
    let count = targets.len();
    let none_of = |e: GraphError, target: &str| {
        let mut d = *e.diagnostic;
        d.message = format!("{target}: {}", d.message);
        let undo = format!("none of the {count} targets was released");
        d.hint = Some(match d.hint.take() {
            Some(h) => format!("{h}; {undo}"),
            None => undo,
        });
        GraphError::new(e.kind, d)
    };
    let actor = repo.actor(actor);
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let mut picks = Vec::with_capacity(count);
    for t in targets {
        // The checks `release <target>` makes before it picks: a role or a
        // turn is named alone, anything else may be ambiguous.
        if t.contains('/') || !is_named_scope(&Scope::new(t.as_str())) {
            release_thing(&repo, t).map_err(|e| none_of(e, t))?;
        }
        let pick = release_pick(&repo, &state, &actor, ReleaseTarget::Named(t))
            .map_err(|e| none_of(e, t))?;
        picks.push((t.as_str(), pick));
    }
    // Each active lease once, in the order given.
    let mut ids: Vec<LeaseId> = Vec::new();
    for (_, p) in &picks {
        if let Pick::Active(l) = p
            && !ids.contains(&l.id)
        {
            ids.push(l.id.clone());
        }
    }
    if let Some((first, rest)) = ids.split_first() {
        let release = |id: &LeaseId| Command::Release {
            owner: actor.clone(),
            lease: id.clone(),
            now,
        };
        let mut rest = rest.iter();
        let chain = store
            .execute_chain(&release(first), |_, last| {
                if last.error().is_some() {
                    None
                } else {
                    rest.next().map(release)
                }
            })
            .map_err(store_error)?;
        if let Some((i, error)) = chain
            .outcomes
            .iter()
            .enumerate()
            .find_map(|(i, o)| o.error().map(|e| (i, e)))
        {
            let target = picks
                .iter()
                .find(|(_, p)| matches!(p, Pick::Active(l) if l.id == ids[i]))
                .map_or("", |(t, _)| *t);
            return Err(none_of(refusal(error), target));
        }
    }
    let state = store.coordinator().map_err(store_error)?;
    let releases: Vec<ReleasedTarget> = picks
        .into_iter()
        .map(|(target, p)| {
            let (Pick::Active(l) | Pick::Ended(l)) = p;
            let lease = state.lease(&l.id).unwrap_or(&l);
            ReleasedTarget {
                target: target.to_string(),
                lease: lease_record(lease),
            }
        })
        .collect();
    Ok(Report::ok(ReleaseData {
        lease: releases[0].lease.clone(),
        releases,
    }))
}

/// `release` of several targets; see [`release_many_report`].
pub fn release_many(root: &Path, actor: Option<String>, targets: &[String], json: bool) -> u8 {
    let report = release_many_report(root, actor, targets)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    output::emit("release", json, report, |d| {
        for r in &d.releases {
            println!(
                "{}",
                output::shown(&format!("released {} ({})", r.lease.scope, r.lease.id))
            );
        }
    })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseManyData {
    /// Leases this run released, each in its own transaction.
    pub released: Vec<LeaseRecord>,
    /// Leases left as they are, each with the reason: `--committed` lists
    /// each path lease of yours it kept (`uncommitted`, `not_committed` or
    /// `unknown`); `--all` and `--clean` list none.
    pub skipped: Vec<KeptLease>,
    /// Leases whose release failed, read back from the store afterwards:
    /// `why: failed` ones are still held (as each record shows), and
    /// `why: uncertain` ones may be held or may have ended (the store
    /// couldn't be read back; the record is as it was before the run).
    /// Running the same command again retries them. A failed release the
    /// store shows ended is listed in `released`.
    pub failed: Vec<KeptLease>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct KeptLease {
    pub lease: LeaseRecord,
    pub why: KeptWhy,
    /// The reason, for people.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeptWhy {
    /// The release failed, and the store still holds the lease.
    Failed,
    /// The release failed and the store couldn't be read back: the lease
    /// may still be held, or may have ended. Claim its path again before
    /// editing it.
    Uncertain,
    /// Its paths have uncommitted, staged or ignored changes in this
    /// worktree (or open work git can't see: hidden index entries,
    /// submodules).
    Uncommitted,
    /// No commit of yours in this worktree touched its paths since you
    /// claimed it, claimed it again or renewed it.
    NotCommitted,
    /// Git couldn't say whether its paths are clean or committed.
    Unknown,
}

impl KeptWhy {
    /// The reason, for people, of a lease `--committed` kept.
    fn reason(self) -> &'static str {
        match self {
            KeptWhy::Failed => "the release failed",
            KeptWhy::Uncertain => "the release failed; whether the lease still holds is unknown",
            KeptWhy::Uncommitted => "its paths have uncommitted, staged or ignored changes",
            KeptWhy::NotCommitted => "no commit of yours touched its paths since you claimed it",
            KeptWhy::Unknown => "git couldn't say whether its paths are clean and committed",
        }
    }
}

/// The shapes `release` prints: one lease, or what `--all` did with each
/// of the actor's leases.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReleaseOutput {
    // Boxed: a lease record is large beside the list shape (serde and
    // the schema see through it).
    One(Box<ReleaseData>),
    Many(ReleaseManyData),
}

/// Which of the actor's leases `release` with no target takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseSelection {
    /// `--all`: every active lease.
    All,
    /// `--clean`: path leases with nothing uncommitted, not just claimed.
    Clean,
    /// `--committed`: path leases with nothing uncommitted that a commit of
    /// the holder's in this worktree touched since the claim.
    Committed,
}

/// `release --all`, `--clean` or `--committed` on this store's leases:
/// each release in its own transaction, as `release <id>` is, so a crash
/// or a failed write leaves the rest released or not, and running it
/// again finishes the job.
fn release_all_local(
    repo: &Repo,
    actor: &str,
    selection: ReleaseSelection,
) -> Result<Report<ReleaseManyData>, GraphError> {
    release_all_local_with(repo, actor, selection, None)
}

/// [`release_all_local`], with `just_made` the commit a post-commit hook
/// runs for.
fn release_all_local_with(
    repo: &Repo,
    actor: &str,
    selection: ReleaseSelection,
    just_made: Option<String>,
) -> Result<Report<ReleaseManyData>, GraphError> {
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let mut changes = Changes {
        just_made,
        ..Changes::default()
    };
    let mut skipped = Vec::new();
    let mine: Vec<Lease> = match selection {
        ReleaseSelection::All => state
            .leases()
            .filter(|l| l.owner == actor && l.status == Status::Active)
            .cloned()
            .collect(),
        // `--clean`: only path leases renew wouldn't extend (a file lease
        // protects uncommitted edits only, #11980): nothing uncommitted,
        // by the strict test (ignored files, hidden index entries and
        // submodules count as open work, #17070), in this worktree (or
        // the one that has the path), and not claimed in the first
        // quarter of the term. Never a node's, a role's or a turn's; when
        // git can't say, the lease is kept.
        ReleaseSelection::Clean => state
            .leases()
            .filter(|l| l.owner == actor && l.status == Status::Active)
            .filter(|l| {
                l.confirmed
                    && l.deadline > now
                    && is_path_lease(l)
                    && !just_claimed(&state, l, now)
                    && changes.strictly_clean(&repo.top, l) == Some(true)
            })
            .cloned()
            .collect(),
        // `--committed`: the holder asks, so
        // there's no first-quarter wait; a lease with no commit since its
        // claim is kept, with the reason.
        ReleaseSelection::Committed => {
            let mut due = Vec::new();
            for l in state.leases().filter(|l| {
                l.owner == actor
                    && l.confirmed
                    && l.status == Status::Active
                    && l.deadline > now
                    && is_path_lease(l)
                    && !is_named_scope(&l.scope)
            }) {
                let why = match changes.strictly_clean(&repo.top, l) {
                    Some(false) => Some(KeptWhy::Uncommitted),
                    None => Some(KeptWhy::Unknown),
                    Some(true) => {
                        match changes.committed_since(
                            &repo.top,
                            &l.scope,
                            granted_at(&state, l),
                            now,
                        ) {
                            Some(true) => None,
                            Some(false) => Some(KeptWhy::NotCommitted),
                            None => Some(KeptWhy::Unknown),
                        }
                    }
                };
                match why {
                    None => due.push(l.clone()),
                    Some(why) => skipped.push(KeptLease {
                        lease: lease_record_in(&state, l),
                        why,
                        reason: why.reason().to_string(),
                    }),
                }
            }
            due
        }
    };
    // No `?` from here: a release that landed is always reported.
    let mut released: Vec<LeaseId> = Vec::new();
    let mut errors: Vec<(Lease, GraphError)> = Vec::new();
    for lease in &mine {
        let result = store
            .execute(&Command::Release {
                owner: actor.to_string(),
                lease: lease.id.clone(),
                now,
            })
            .map_err(store_error)
            .and_then(|outcome| match outcome.error() {
                Some(error) => Err(refusal(error)),
                None => Ok(()),
            });
        match result {
            Ok(()) => released.push(lease.id.clone()),
            Err(e) => errors.push((lease.clone(), e)),
        }
    }
    // Leases as the store now holds them. A store error may come after the
    // write landed (#17077): a failed release the store shows ended is
    // released; one it shows active is held (`failed`); one it can't read
    // back is `uncertain`, reported as it was before the run.
    let after = store.coordinator().ok();
    let mut failed = Vec::new();
    let mut first_error: Option<GraphError> = None;
    for (lease, e) in errors {
        match after.as_ref().and_then(|s| s.lease(&lease.id)) {
            Some(held) if held.status != Status::Active => released.push(lease.id.clone()),
            Some(held) => {
                failed.push(KeptLease {
                    lease: lease_record(held),
                    why: KeptWhy::Failed,
                    reason: e.diagnostic.message.clone(),
                });
                first_error.get_or_insert(e);
            }
            None => {
                failed.push(KeptLease {
                    lease: lease_record(&lease),
                    why: KeptWhy::Uncertain,
                    reason: format!(
                        "the release failed and the store couldn't be read back, so whether \
                         the lease still holds is unknown: {}",
                        e.diagnostic.message
                    ),
                });
                first_error.get_or_insert(e);
            }
        }
    }
    let released = released
        .iter()
        .filter_map(|id| {
            after
                .as_ref()
                .and_then(|s| s.lease(id))
                .or_else(|| mine.iter().find(|l| &l.id == id))
        })
        .map(lease_record)
        .collect();
    let data = ReleaseManyData {
        released,
        skipped,
        failed,
    };
    Ok(match first_error {
        None => Report::ok(data),
        Some(e) => {
            let n = data.failed.len();
            Report {
                data: Some(data),
                diagnostics: vec![
                    Diagnostic {
                        message: format!(
                            "{n} lease(s) couldn't be released, the first: {}",
                            e.diagnostic.message
                        ),
                        ..*e.diagnostic
                    }
                    .with_hint("run the same command again to retry them"),
                ],
                failure: Some(e.kind),
            }
        }
    })
}

/// `release --all`; `release --clean`, the path leases whose paths have
/// no uncommitted changes and weren't just claimed (the launcher runs it
/// as the agent after every turn); or `release --committed`, the holder's
/// own call for the path leases it has committed and left clean. Beside a
/// lease authority, only this store's leases (roles, turns, profile C
/// nodes) are released here: the lease-authority contract has no call that
/// lists yours there, so those are released by path.
pub fn release_all(
    root: &Path,
    actor: Option<String>,
    selection: ReleaseSelection,
    json: bool,
) -> u8 {
    let report = (|| {
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        // `--committed` reads this worktree's own commits (its HEAD
        // reflog), so only its own actor, the one its folder names, runs
        // it (#16983, #17125).
        if selection == ReleaseSelection::Committed && !owns_worktree(&repo, &actor) {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                format!(
                    "`release --committed` reads this worktree's commits, so it releases only \
                     the leases of the actor this worktree is named after ({}), not {}",
                    output::shown(&repo.folder_actor()),
                    output::shown(&actor)
                ),
            )
            .with_hint("run it in your own worktree, as yourself"));
        }
        let report = release_all_local(&repo, &actor, selection)?;
        let beside = match authority::find(&repo) {
            Ok(None) => None,
            Ok(Some(_)) | Err(_) => Some(
                Diagnostic::warning(
                    Code::CoordAuthorityRemote,
                    "only this repository's own leases were released; the lease authority \
                     still holds yours there",
                )
                .with_hint(format!("release those by path: `{BIN} release <path>`")),
            ),
        };
        Ok::<_, GraphError>(report.with(beside))
    })()
    .unwrap_or_else(Report::failed)
    .with(authority::committed_key_warning(root));
    output::emit("release", json, report, |d| {
        if d.released.is_empty() && d.failed.is_empty() {
            match selection {
                ReleaseSelection::All => println!("no active leases to release"),
                ReleaseSelection::Clean => println!("no clean leases to release"),
                ReleaseSelection::Committed => println!("no committed leases to release"),
            }
        }
        for l in &d.released {
            println!(
                "{}",
                output::shown(&format!("released {} ({})", l.scope, l.id))
            );
        }
        for k in &d.failed {
            let what = match k.why {
                KeptWhy::Uncertain => "maybe released",
                _ => "not released",
            };
            println!(
                "{}",
                output::shown(&format!("{what} {} ({})", k.lease.scope, k.lease.id))
            );
            println!("  {}", output::shown(&k.reason));
        }
        for k in &d.skipped {
            println!(
                "{}",
                output::shown(&format!("kept {} ({})", k.lease.scope, k.lease.id))
            );
            println!("  {}", output::shown(&k.reason));
        }
    })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RenewData {
    /// The actor's active leases, each renewed for another full term.
    pub renewed: Vec<LeaseRecord>,
    /// Leases of the actor's that expired in the last 24 hours and weren't
    /// claimed again: renewing can't revive them, claiming again can.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expired: Vec<LeaseRecord>,
    /// Path leases whose path has no changes in this worktree, past the
    /// first quarter of their term (30 minutes by default): not renewed,
    /// so they lapse at their deadline. Claiming one again keeps it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_renewed: Vec<LeaseRecord>,
    /// Path leases with no changes in this worktree, still in the first
    /// quarter of their term: not renewed, and they keep their time. A
    /// renew once their path has changes extends them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<LeaseRecord>,
}

fn renew_local(
    root: &Path,
    actor: Option<String>,
    named_only: bool,
    only_turn: Option<&str>,
) -> Result<Report<RenewData>, GraphError> {
    {
        let repo = Repo::open(root)?;
        let named = named_actor(actor.as_ref());
        let actor = repo.actor(actor);
        // A name or a scope an older build stored unchecked isn't kept
        // alive: such leases lapse.
        crate::msg::check_actor_mailbox(&actor, named)?;
        let mut store = repo.store()?;
        let now = now_ms();
        // Only roles and turns (beside an authority): main doesn't matter.
        if named_only {
            repo.expire_due(&mut store, now)?;
        } else {
            repo.observe(&mut store, now)?;
        }
        let before = store.coordinator().map_err(store_error)?;
        // A file lease protects uncommitted edits only (decided 2026-10-30,
        // #11980; friction items 70 and 75): renew extends a path lease
        // while its path has changes in this worktree (or in the one that
        // has the path). A clean one, committed work included, keeps its
        // time and lapses at its deadline; claiming it again keeps it.
        // When git can't say, the lease is renewed, as before.
        let clean: Vec<LeaseId> = if named_only {
            Vec::new()
        } else {
            let mut changes = Changes::default();
            before
                .leases()
                .filter(|l| l.owner == actor && l.confirmed && l.status == Status::Active)
                // The strict test `release --clean` and status's `clean` use
                // (#17125): ignored drafts, hidden index entries and
                // submodules count as changes, so their lease is renewed.
                .filter(|l| is_path_lease(l) && changes.strictly_clean(&repo.top, l) == Some(true))
                .map(|l| l.id.clone())
                .collect()
        };
        let ids: Vec<LeaseId> = before
            .leases()
            .filter(|l| l.owner == actor && l.confirmed && l.status == Status::Active)
            .filter(|l| !clean.contains(&l.id))
            .filter(|l| !named_only || is_named_scope(&l.scope))
            .filter(|l| only_turn.is_none_or(|scope| l.scope.as_str() == scope))
            .filter(|l| repo.lease_still_granted(&l.scope))
            .map(|l| l.id.clone())
            .collect();
        for id in &ids {
            let outcome = store
                .execute(&Command::Heartbeat {
                    owner: actor.clone(),
                    lease: id.clone(),
                    now,
                })
                .map_err(store_error)?;
            if let Some(error) = outcome.error() {
                return Err(refusal(error));
            }
        }
        let state = store.coordinator().map_err(store_error)?;
        let renewed: Vec<LeaseRecord> = ids
            .iter()
            .filter_map(|id| state.lease(id))
            .map(|l| lease_record_in(&state, l))
            .collect();
        let expired: Vec<&Lease> = recently_expired(&state, Some(&actor), now)
            .into_iter()
            .filter(|l| !named_only || is_named_scope(&l.scope))
            .filter(|l| only_turn.is_none_or(|scope| l.scope.as_str() == scope))
            .collect();
        let mut diagnostics: Vec<Diagnostic> = ids
            .iter()
            .filter_map(|id| state.lease(id))
            .flat_map(|l| co_hold_notices(&state, l))
            .collect();
        // Leases an older build granted on scopes this one refuses: not
        // renewed, and said so, by id (their scopes are escaped in `status`).
        let lapsing: Vec<String> = state
            .leases()
            .filter(|l| l.owner == actor && l.confirmed && l.status == Status::Active)
            .filter(|l| !repo.lease_still_granted(&l.scope))
            .map(|l| l.id.to_string())
            .collect();
        if !lapsing.is_empty() {
            diagnostics.push(
                Diagnostic::warning(
                    Code::CoordNotActive,
                    format!(
                        "not renewed, so they'll lapse: {} (scopes this build doesn't lease)",
                        lapsing.join(", ")
                    ),
                )
                .with_hint(
                    "release them to work inside them now; claims inside them are refused until \
                     then, and claim nodes by id",
                ),
            );
        }
        if !expired.is_empty() {
            let scopes: Vec<String> = expired
                .iter()
                .map(|l| output::shown(l.scope.as_str()))
                .collect();
            diagnostics.push(
                Diagnostic::warning(
                    Code::CoordPastDeadline,
                    format!(
                        "{} lease(s) already expired: {}",
                        scopes.len(),
                        scopes.join(", ")
                    ),
                )
                .with_hint("claim them again to keep working on them"),
            );
        }
        let (kept, not_renewed): (Vec<&Lease>, Vec<&Lease>) = clean
            .iter()
            .filter_map(|id| state.lease(id))
            .partition(|l| just_claimed(&state, l, now));
        let kept: Vec<LeaseRecord> = kept
            .into_iter()
            .map(|l| lease_record_in(&state, l))
            .collect();
        let not_renewed: Vec<LeaseRecord> = not_renewed
            .into_iter()
            .map(|l| lease_record_in(&state, l))
            .collect();
        if !not_renewed.is_empty() {
            diagnostics.push(
                Diagnostic::warning(
                    Code::CoordNotActive,
                    format!(
                        "not renewed, so they'll lapse: {} (no changes in this worktree)",
                        not_renewed
                            .iter()
                            .map(|l| l.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .with_hint(format!(
                    "give them up now with `{BIN} release <path>`; to keep one while you edit it, \
                     claim it again"
                )),
            );
        }
        Ok(Report::ok(RenewData {
            renewed,
            expired: expired.into_iter().map(lease_record).collect(),
            not_renewed,
            kept,
        })
        .with(diagnostics))
    }
}

/// Renews every active lease the actor holds.
pub fn renew(root: &Path, actor: Option<String>, json: bool, local_turn: Option<&str>) -> u8 {
    let report = (|| {
        if let Some(scope) = local_turn {
            let name = scope.strip_prefix("run:").ok_or_else(|| {
                GraphError::usage(
                    Code::UsageInvalidArguments,
                    "--local-turn requires run:<actor>",
                )
            })?;
            mdkg_run::state::check_name(name).map_err(crate::run::graph_error)?;
            // Receiver holders have no authority-owned claims. This scoped
            // local heartbeat must never launch an external process: its
            // caller can cancel it at the verified run-lease deadline.
            return renew_local(root, actor, true, Some(scope));
        }
        let repo = Repo::open(root)?;
        // Roles and turns renew here, even when the authority can't be
        // found, trusted or reached; the rest renews through it, whatever
        // happened here.
        let authority = match authority::find(&repo) {
            Ok(None) => return renew_local(root, actor, false, None),
            Ok(Some(a)) => Ok(a),
            Err(e) => Err(e),
        };
        let local = renew_local(root, actor.clone(), true, None);
        let delegated: Result<Report<RenewData>, GraphError> = authority
            .and_then(|a| a.delegate(root, "renew", &[], &[("--as", Some(repo.actor(actor)))]));
        // Roles and turns not renewed here fail the command, whatever the
        // authority renewed: a launcher must never believe a turn renewed
        // that is running out.
        let (mine, local_diagnostics) = match local {
            Ok(r) => (r.data, r.diagnostics),
            Err(e) => {
                let mut failed = Report::failed(GraphError::new(
                    e.kind,
                    Diagnostic {
                        message: format!(
                            "your roles and turns couldn't be renewed here: {}",
                            e.diagnostic.message
                        ),
                        ..*e.diagnostic
                    },
                ));
                let mut remote: Vec<Diagnostic> = match delegated {
                    Ok(r) => {
                        let renewed: Vec<String> = r
                            .data
                            .iter()
                            .flat_map(|d| d.renewed.iter().map(|l| l.scope.clone()))
                            .collect();
                        let mut notes = r.diagnostics;
                        if !renewed.is_empty() {
                            notes.push(Diagnostic::warning(
                                Code::CoordAuthorityRemote,
                                format!(
                                    "the lease authority renewed: {}",
                                    output::shown(&renewed.join(", "))
                                ),
                            ));
                        }
                        notes
                    }
                    Err(a) => vec![*a.diagnostic],
                };
                for d in &mut remote {
                    d.severity = Severity::Warning;
                }
                failed = failed.with(remote);
                return Ok(failed);
            }
        };
        let kept: Vec<String> = mine
            .iter()
            .flat_map(|d| d.renewed.iter().map(|l| l.scope.clone()))
            .collect();
        let kept_note = (!kept.is_empty()).then(|| {
            Diagnostic::warning(
                Code::CoordAuthorityRemote,
                format!(
                    "your roles and turns were renewed here: {}",
                    kept.join(", ")
                ),
            )
        });
        Ok(match delegated {
            Ok(mut report) if report.data.is_some() => {
                if let (Some(d), Some(mine)) = (report.data.as_mut(), mine) {
                    d.renewed.extend(mine.renewed);
                    d.expired.extend(mine.expired);
                }
                report.with(local_diagnostics)
            }
            Ok(report) => report.with(local_diagnostics).with(kept_note),
            Err(e) => Report::failed(e).with(local_diagnostics).with(kept_note),
        })
    })()
    .unwrap_or_else(Report::failed)
    .with(authority::committed_key_warning(root));
    output::emit("renew", json, report, |d| {
        if d.renewed.is_empty() && d.not_renewed.is_empty() && d.kept.is_empty() {
            println!("no active leases to renew");
        }
        for l in &d.renewed {
            println!(
                "{}",
                output::shown(&format!("renewed {} until {} ({})", l.scope, l.until, l.id))
            );
        }
        for l in &d.not_renewed {
            println!(
                "{}",
                output::shown(&format!(
                    "not renewed {} (no changes here) until {} ({})",
                    l.scope, l.until, l.id
                ))
            );
        }
        for l in &d.kept {
            println!(
                "{}",
                output::shown(&format!(
                    "kept {} until {} ({}): just claimed, renewed once it has changes",
                    l.scope, l.until, l.id
                ))
            );
        }
    })
}

/// `mdkg lease`: asking about leases other actors hold.
#[derive(Debug, clap::Subcommand)]
pub enum LeaseCommand {
    /// Ask the actor holding a node or path to give it up: direct mail of
    /// kind `question` to the holder, copied to whoever you report to.
    ///
    /// The holder answers with a normal reply, and releasing stays its own
    /// act: nothing is released for it. The mail's context is
    /// `lease:<lease id>:<you>`, one conversation per lease and asker (asking
    /// again adds to it), and a data part names the path, you and the
    /// reason.
    #[command(after_help = examples!(
        "lease request src/parser.rs --reason \"a security fix needs one commit here\""
    ))]
    Request {
        /// The node id or path you need, as you'd claim it.
        target: String,
        /// Why you need it, for the holder.
        #[arg(long)]
        reason: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl LeaseCommand {
    pub fn name(&self) -> &'static str {
        match self {
            LeaseCommand::Request { .. } => "lease.request",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            LeaseCommand::Request { json, .. } => *json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LeaseRequestData {
    /// What was asked for, as a claim would lease it.
    pub scope: String,
    /// One request per lease in the way, each mailed to its holder.
    pub requests: Vec<LeaseRequestRecord>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LeaseRequestRecord {
    /// The lease in the way.
    pub lease: LeaseRecord,
    /// The `question` sent to its holder (and the requester's
    /// `reports_to`), in the context `lease:<lease id>:<requester>`.
    pub sent: crate::msg::SendData,
}

/// Who `actor` reports to, as the committed agent definitions say: `None`
/// for an actor that isn't an agent.
fn reports_to(root: &Path, actor: &str) -> Result<Option<String>, String> {
    use mdkg_run::orchestration::{agent, authority};
    let config = authority(root)
        .and_then(|a| agent::authority_config(&a))
        .map_err(|e| e.to_string())?;
    Ok(config
        .get("agents")
        .and_then(|agents| agents.get(actor))
        .and_then(|a| a.get("reports_to"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

fn lease_request_report(
    root: &Path,
    actor: Option<String>,
    target: &str,
    reason: &str,
) -> Result<Report<LeaseRequestData>, GraphError> {
    if reason.trim().is_empty() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "a lease request needs a --reason",
        )
        .with_hint("say what you need it for, so its holder can decide"));
    }
    let plan = plan_claim(root, actor.clone(), target, &ClaimOptions::default())?;
    if is_named_scope(&plan.scope) {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "a lease request takes a node or a path, not a role or a turn",
        )
        .with_hint("turns belong to the launcher; ask about a role by mail"));
    }
    let requester = plan.actor.clone();
    let mut store = plan.repo.store()?;
    let now = now_ms();
    plan.repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    // The leases a claim would be refused for: the node held under another
    // path, and every lease overlapping the scope that can't be held beside
    // one in the claim's mode.
    let mut blocking: Vec<&Lease> = Vec::new();
    let node_holder = plan.node_key.as_deref().and_then(|k| state.node_holder(k));
    for l in node_holder.into_iter().chain(state.blockers(
        &plan.scope,
        plan.mode,
        plan.branch.as_deref(),
    )) {
        if l.owner != requester && !blocking.iter().any(|b| b.id == l.id) {
            blocking.push(l);
        }
    }
    if blocking.is_empty() {
        return Err(GraphError::not_found(
            Code::CoordNoLease,
            format!("no one else's lease is in the way of {}", plan.label),
        )
        .with_hint(format!("claim it: `{BIN} claim {}`", plan.target)));
    }
    let mut diagnostics = Vec::new();
    let copy = match reports_to(root, &requester) {
        Ok(to) => to,
        Err(_) => {
            diagnostics.push(
                Diagnostic::warning(
                    Code::ConfigUnreadable,
                    "your agent definition couldn't be read, so nobody you report to was copied",
                )
                .with_hint(format!("`{BIN} doctor` checks the setup")),
            );
            None
        }
    };
    let mut requests = Vec::new();
    let mut failed: Option<GraphError> = None;
    for l in blocking {
        let record = lease_record_in(&state, l);
        let mut to = vec![l.owner.clone()];
        if let Some(c) = copy.as_ref().filter(|c| **c != l.owner && **c != requester) {
            to.push(c.clone());
        }
        // Names and the reason are data: the text names neither.
        let parts = serde_json::json!([
            {
                "kind": "text",
                "text": format!(
                    "Lease request: another actor needs {}, which your lease {} holds until {}. \
                     The data part says who and why. If you can, finish or commit what you're \
                     doing there and release it (`{BIN} release <path>`); reply either way. \
                     Releasing is yours to decide: nothing releases it for you.",
                    output::shown(l.scope.as_str()),
                    l.id,
                    record.until
                ),
            },
            {
                "kind": "data",
                "mediaType": "application/json",
                "data": {
                    "kind": "lease_request",
                    "lease": l.id.to_string(),
                    "path": l.scope.to_string(),
                    "requested": plan.scope.to_string(),
                    "requester": requester,
                    "reason": reason,
                },
            },
        ]);
        // One conversation per lease and asker: a mail context admits only
        // its members, so a second asker can't post into the first's.
        let sent = match crate::msg::send_direct(
            root,
            Some(requester.clone()),
            to,
            "question",
            &lease_request_context(&l.id, &requester),
            parts,
        ) {
            Ok(sent) => sent,
            // Mail already sent to other holders stands: say which lease
            // wasn't asked about, and go on.
            Err(e) => {
                let mut d = (*e.diagnostic).clone();
                d.severity = Severity::Warning;
                d.message = format!("{} wasn't asked about: {}", l.id, d.message);
                diagnostics.push(d);
                failed.get_or_insert(e);
                continue;
            }
        };
        diagnostics.extend(sent.diagnostics);
        let Some(sent) = sent.data else {
            continue;
        };
        requests.push(LeaseRequestRecord {
            lease: record,
            sent,
        });
    }
    if requests.is_empty()
        && let Some(e) = failed
    {
        return Err(e);
    }
    Ok(Report::ok(LeaseRequestData {
        scope: plan.scope.to_string(),
        requests,
    })
    .with(diagnostics))
}

/// `lease:<lease id>:<requester>`, kept to what a message id allows: any
/// other character (an `@` a mailbox may hold) becomes `_`, and it's cut at
/// 128 characters.
fn lease_request_context(lease: &LeaseId, requester: &str) -> String {
    format!("lease:{lease}:{requester}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect()
}

/// `mdkg lease request`: mails each holder in the way; never releases.
pub fn lease_request(
    root: &Path,
    actor: Option<String>,
    target: &str,
    reason: &str,
    json: bool,
) -> u8 {
    let report = lease_request_report(root, actor, target, reason)
        .unwrap_or_else(Report::failed)
        .with(authority_notice(root));
    output::emit("lease.request", json, report, |d| {
        for r in &d.requests {
            println!(
                "{}",
                output::shown(&format!(
                    "asked {} for {} ({}): sent {} #{} to {}",
                    r.lease.owner,
                    r.lease.scope,
                    r.lease.id,
                    r.sent.message.message_id,
                    r.sent.message.seq,
                    r.sent.message.to.join(", ")
                ))
            );
        }
    })
}

/// Runs `mdkg lease`.
pub fn lease(root: &Path, actor: Option<String>, cmd: &LeaseCommand) -> u8 {
    match cmd {
        LeaseCommand::Request {
            target,
            reason,
            json,
        } => lease_request(root, actor, target, reason, *json),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusData {
    /// Who this worktree acts as.
    pub actor: String,
    /// The branch work settles on: in ref mode, the graph ref (as
    /// `graph.ref`). Always present, as the published schema requires.
    pub main: String,
    /// The graph's head: in ref mode the graph ref work settles on and its
    /// tip; in checkout mode `ref` null and the worktree's `HEAD`. Absent
    /// only in checkout mode when `HEAD` names no commit yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<GraphStatus>,
    /// Ref mode: the read-only code base (`mdkg.mainRef`), when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_base: Option<String>,
    /// The coordinator store's own id, as `anchor` names it: stable for the
    /// store's life, across restores of its snapshots, and the same from
    /// every worktree of the repository. Null before anything has created
    /// the store.
    pub store_id: Option<String>,
    /// Active leases, everyone's; `mine` marks the actor's own.
    pub leases: Vec<LeaseRecord>,
    /// The actor's leases (everyone's with `--all`) that expired in the
    /// last 24 hours and weren't claimed again since.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expired: Vec<LeaseRecord>,
    /// Changesets not yet on main, in the order they must merge.
    pub queue: Vec<ChangesetRecord>,
    /// Temporary statuses (`review`, `blocked`) recorded with `update`, by
    /// file path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub work: Vec<WorkRecord>,
    /// Unacked mail in the actor's mailboxes (its own and the roles it
    /// holds), for the mailboxes that have any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mail: Vec<MailCount>,
}

/// The graph ref, as `status` reports it in ref mode (edd-01a18867).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GraphStatus {
    /// The graph ref's full name; null in checkout mode.
    #[serde(rename = "ref")]
    pub name: Option<String>,
    /// The commit read: the graph ref's tip, or the worktree's `HEAD`.
    pub tip: String,
    /// `mdkg.graph.remote`, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Graph edits found outside the graph ref, reported and never read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hand_edits: Vec<HandEditRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HandEditRecord {
    /// `checkout`, `branch` or `rescued`.
    pub source: String,
    /// A repository-relative path, a commit, or a rescued folder.
    pub at: String,
}

/// The commit `refname` names in `top`, or `None` when it names none (an
/// unborn `HEAD`, a ref not made yet). Only git's own "no such commit"
/// answer is `None`: a git that can't start or fails otherwise is an error.
pub(crate) fn commit_named(top: &Path, refname: &str) -> std::io::Result<Option<String>> {
    let out = git::command(top)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ])
        .run(None)?;
    if out.status.success() {
        return String::from_utf8(out.stdout)
            .map(|s| Some(s.trim_end().to_owned()))
            .map_err(std::io::Error::other);
    }
    // `--quiet` answers a name that resolves to no commit with exit 1 and
    // nothing on stderr.
    if out.status.code() == Some(1) && out.stderr.is_empty() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr.trim();
    Err(std::io::Error::other(if why.is_empty() {
        format!("git rev-parse {refname}: {}", out.status)
    } else {
        format!("git rev-parse {refname}: {why} ({})", out.status)
    }))
}

/// `status`'s graph block: in ref mode the graph ref and its tip, with the
/// `graph.hand_edits` notice when anything was edited outside it; in
/// checkout mode `ref` null and the worktree's `HEAD`, or none while
/// `HEAD` names no commit.
/// A git read of HEAD that fails is a warning, not an unborn `HEAD`.
pub(crate) fn graph_status(repo: &Repo) -> Option<GraphStatus> {
    let Some(mode) = repo.mode.as_ref() else {
        return match commit_named(&repo.top, "HEAD") {
            Ok(tip) => tip.map(|tip| GraphStatus {
                name: None,
                tip,
                remote: None,
                hand_edits: Vec::new(),
            }),
            Err(e) => {
                output::notice(mdkg_format::Diagnostic::warning(
                    Code::IoGitFailed,
                    format!("can't read the checkout's HEAD for graph: {e}"),
                ));
                None
            }
        };
    };
    let hand_edits: Vec<HandEditRecord> = graph_git::hand_edits(mode)
        .into_iter()
        .map(|h| HandEditRecord {
            source: h.source.into(),
            at: h.at,
        })
        .collect();
    if !hand_edits.is_empty() {
        output::notice(
            graph_git::graph_warning(
                "graph.hand_edits",
                format!(
                    "{} graph edit(s) outside the graph ref are reported and never read",
                    hand_edits.len()
                ),
            )
            .with_hint(graph_git::hand_edit_hint(None)),
        );
    }
    // graph.remote sync: approval, offline, a hook's refusal, conflicts.
    for note in crate::graph::sync::status_notes(&mode.graph) {
        output::notice(note);
    }
    Some(GraphStatus {
        name: Some(mode.graph.name.clone()),
        tip: mode.commit(),
        remote: mode.graph.remote.as_ref().map(|r| r.name.clone()),
        hand_edits,
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkRecord {
    pub scope: String,
    /// For a profile C scope (`node:<uuid>`): the node it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeName>,
    pub status: String,
    /// Who set it.
    pub actor: String,
    /// When it was set, RFC 3339 UTC.
    pub since: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MailCount {
    pub mailbox: String,
    pub unacked: u64,
}

/// What a `done` left behind: `actor`'s active path leases (never a role,
/// a turn or a node) after its latest `done` at or after `since_ms`, with
/// that done's time. `None` when there was no such done, or when the actor
/// holds a node: it has taken new work. The hooks' `lease.done` reads it.
pub(crate) fn held_after_done<'a>(
    state: &'a Coordinator,
    actor: &str,
    since_ms: u64,
    now: u64,
) -> Option<(u64, Vec<&'a Lease>)> {
    let mine: Vec<&Lease> = state.leases().filter(|l| l.owner == actor).collect();
    let done = mine
        .iter()
        .filter(|l| l.node.is_some() && l.status == Status::Committed)
        .filter_map(|l| l.committed_at)
        .filter(|at| *at >= since_ms)
        .max()?;
    let active = |l: &Lease| l.status == Status::Active && l.deadline > now;
    if mine.iter().any(|l| active(l) && l.node.is_some()) {
        return None;
    }
    let held = mine
        .into_iter()
        .filter(|l| active(l))
        .filter(|l| !is_role_or_turn(&l.scope))
        .collect();
    Some((done, held))
}

/// Whether anyone holds `owner`'s turn (`run:<owner>`) now.
fn in_a_turn(state: &Coordinator, owner: &str, now: u64) -> bool {
    state.leases().any(|l| {
        l.confirmed
            && l.status == Status::Active
            && l.deadline > now
            && l.scope
                .as_str()
                .strip_prefix("run:")
                .is_some_and(|name| name.eq_ignore_ascii_case(owner))
    })
}

/// A lease on a path: not a node's, a role's or a turn's. `renew` extends
/// one only while its path has changes in the holder's worktree.
fn is_path_lease(l: &Lease) -> bool {
    l.node.is_none() && !is_role_or_turn(&l.scope)
}

/// Whether `l` was claimed, claimed again or renewed in the first quarter
/// of its term (30 minutes of the default 2 hours, #11931): a clean one
/// isn't reported as lapsing then, nor marked idle. The store records no
/// grant time, so it's read from the deadline; renew never extends a
/// clean lease, so renewing can't keep one "just claimed". That reading
/// assumes every lease got the store's default term: a lease granted with
/// another term would have its first quarter misplaced.
fn just_claimed(state: &Coordinator, l: &Lease, now: u64) -> bool {
    let ttl = state.ttl();
    l.deadline.saturating_sub(ttl).saturating_add(ttl / 4) > now
}

/// When `l` was last claimed, claimed again or renewed, read from its
/// deadline as [`just_claimed`] reads it: the store records no grant time.
fn granted_at(state: &Coordinator, l: &Lease) -> u64 {
    l.deadline.saturating_sub(state.ttl())
}

/// The worktree's own commits, newest first, from its HEAD reflog: each
/// `commit:`, `commit (amend):` or `commit (initial):` entry (never a
/// fast-forward, merge, pull, rebase, cherry-pick or reset: what those
/// bring in is someone else's work, or the holder's replayed, #16988),
/// with the entry's time in milliseconds. `None` when git can't say.
fn own_commits(top: &Path) -> Option<Vec<OwnCommit>> {
    let out = git::command(top)
        .args([
            "reflog",
            "show",
            "--max-count=500",
            "--date=unix",
            "--format=%H %gd %gs",
            "HEAD",
        ])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commits = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let mut parts = line.splitn(3, ' ');
        let (Some(sha), Some(at), Some(subject)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(at) = at
            .strip_prefix("HEAD@{")
            .and_then(|t| t.strip_suffix('}'))
            .and_then(|t| t.parse::<u64>().ok())
            .map(|secs| secs.saturating_mul(1000))
        else {
            continue;
        };
        let own = ["commit:", "commit (amend):", "commit (initial):"]
            .iter()
            .any(|p| subject.starts_with(p));
        if own && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            // The reflog is newest first: the next entry is the HEAD an
            // amend replaced. One the window cut off is unknown.
            let replaced = subject
                .starts_with("commit (amend):")
                .then(|| {
                    lines
                        .peek()
                        .and_then(|l| l.split(' ').next())
                        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
                        .map(str::to_string)
                })
                .flatten();
            commits.push(OwnCommit {
                at,
                sha: sha.to_string(),
                replaced,
            });
        }
    }
    Some(commits)
}

/// One of a worktree's own commits in its HEAD reflog ([`own_commits`]).
#[derive(Debug, Clone)]
struct OwnCommit {
    /// When the reflog recorded it, in whole seconds as milliseconds.
    at: u64,
    sha: String,
    /// For an amend, the commit it replaced, when the reflog still has it.
    replaced: Option<String>,
}

/// The paths an amend of `replaced` into `sha` changed: those of `sha`'s
/// own change that also differ from `replaced`. A path the amended commit
/// carries over unchanged was committed by the earlier commit, not now.
fn amend_paths(top: &Path, replaced: &str, sha: &str) -> Option<Vec<Scope>> {
    let own = commit_paths(top, sha)?;
    let out = git::command(top)
        .args([
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--name-only",
            "-z",
            replaced,
            sha,
        ])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let changed: Vec<&[u8]> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    Some(
        own.into_iter()
            .filter(|p| changed.iter().any(|c| *c == p.as_str().as_bytes()))
            .collect(),
    )
}

/// The paths commit `sha`'s own change touches, relative to the top.
fn commit_paths(top: &Path, sha: &str) -> Option<Vec<Scope>> {
    let out = git::command(top)
        .args([
            "diff-tree",
            "-r",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-z",
            sha,
        ])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| Scope::new(String::from_utf8_lossy(p).into_owned()))
            .collect(),
    )
}

/// What the strict clean test reads in one worktree, once a command:
/// git status's untracked and ignored entries and changes, and the index's
/// entries with their tags. Paths are relative to the worktree's top, as
/// git spells them.
struct StrictRead {
    /// `git status` entries: (path, is a folder entry).
    status: Vec<(String, bool)>,
    /// `git ls-files -v -s` entries: (path, hidden, submodule), hidden
    /// meaning assume-unchanged, skip-worktree or a submodule.
    index: Vec<(String, bool, bool)>,
}

/// One `git status --porcelain=v1 -z` read in `top`, over `dir` (a folder
/// as git spelled it) or the whole worktree: (path, is a folder entry).
fn status_entries(top: &Path, dir: Option<&str>) -> Option<Vec<(String, bool)>> {
    let cmd = git::command(top).args([
        "status",
        "--porcelain=v1",
        "-z",
        "--no-renames",
        "--ignore-submodules=none",
    ]);
    let cmd = match dir {
        // Inside an untracked or ignored folder: each file by itself.
        Some(d) => cmd
            .args(["--untracked-files=all", "--ignored=traditional", "--"])
            .arg(format!(":(literal){d}")),
        // The whole worktree: such a folder as one entry, so nothing walks
        // a large ignored folder (`target/`) unless a lease is in it.
        None => cmd.args(["--untracked-files=normal", "--ignored=matching"]),
    };
    let out = cmd.run(None).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|e| e.len() > 3)
            .map(|e| {
                let path = String::from_utf8_lossy(&e[3..]).into_owned();
                let folder = path.ends_with('/');
                (path, folder)
            })
            .collect(),
    )
}

impl StrictRead {
    fn of(top: &Path) -> Option<StrictRead> {
        let status = status_entries(top, None)?;
        let files = git::command(top)
            .args(["ls-files", "-v", "-s", "-z"])
            .run(None)
            .ok()?;
        if !files.status.success() {
            return None;
        }
        // `ls-files -v -s`: `<tag> <mode> <object> <stage>\t<path>`. A
        // lowercase tag is assume-unchanged, `S` skip-worktree, and mode
        // 160000 a submodule.
        let index = files
            .stdout
            .split(|b| *b == 0)
            .filter_map(|e| {
                let tab = e.iter().position(|b| *b == b'\t')?;
                let tag = *e.first()?;
                let submodule = e.get(2..8) == Some(b"160000");
                let hidden = tag.is_ascii_lowercase() || tag == b'S' || submodule;
                Some((
                    String::from_utf8_lossy(&e[tab + 1..]).into_owned(),
                    hidden,
                    submodule,
                ))
            })
            .collect();
        Some(StrictRead { status, index })
    }
}

/// Whether `scope` has nothing uncommitted in the worktree at `top` by a
/// stricter test than [`Changes::of`] (#16983, #17070), for `release
/// --clean` and `--committed` and for status's and holds' `clean`: git
/// status with submodules and ignored files must find nothing under it (an
/// ignored draft is open work, #17073), and no index entry
/// under it may be assume-unchanged or skip-worktree (git status can't see
/// edits to those) or a submodule (edits inside one aren't this index's to
/// see). Every entry is matched to the scope by [`Scope`] alone, the
/// comparison claims use (any letter case, #18144): git is never asked to
/// match the scope. `None` when git can't say.
fn strictly_clean(top: &Path, scope: &Scope, read: &StrictRead) -> Option<bool> {
    let exists = || top.join(scope.as_str()).symlink_metadata().is_ok();
    let mut around = Vec::new();
    for (raw, folder) in &read.status {
        let path = Scope::new(raw.as_str());
        if scope.contains(&path) {
            return Some(false);
        }
        // An untracked or ignored folder around the scope: what's in it
        // is read file by file, below. A sibling's folder isn't open work
        // under the scope (#18021).
        if *folder && path.contains(scope) {
            around.push(raw.as_str());
        }
    }
    for dir in around {
        for (raw, folder) in status_entries(top, Some(dir))? {
            let path = Scope::new(raw.as_str());
            // A folder git won't list inside (an untracked nested
            // repository) is open work under the scope only when the
            // scope's path is there (#17125).
            if scope.contains(&path) || (folder && path.contains(scope) && exists()) {
                return Some(false);
            }
        }
    }
    // A hidden entry under the scope, or a submodule around it (the scope
    // is inside a submodule, whose edits this index can't see).
    // A hidden file above the scope (a sparse checkout's skipped file over
    // a `--new` path) isn't (#18265).
    let hidden = read.index.iter().any(|(raw, hidden, submodule)| {
        let path = Scope::new(raw.as_str());
        (*hidden && scope.contains(&path)) || (*submodule && path.contains(scope))
    });
    Some(!hidden)
}

/// The commit HEAD names in the worktree at `top`, if any.
fn head_commit(top: &Path) -> Option<String> {
    let out = git::command(top)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty() && sha.bytes().all(|b| b.is_ascii_hexdigit())).then_some(sha)
}

/// The files under `scope` that conflict when the commits `ours` and
/// `theirs` are merged, by `git merge-tree --write-tree` in `top` (git
/// 2.38 or later; it writes only objects, never a ref or the worktree).
/// `None` when git can't say: no common history, an older git, or an
/// error.
fn merge_tree_conflicts(
    top: &Path,
    ours: &str,
    theirs: &str,
    scope: &Scope,
) -> Option<Vec<String>> {
    if ours == theirs {
        return Some(Vec::new());
    }
    let out = git::command(top)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            ours,
            theirs,
        ])
        .run(None)
        .ok()?;
    match out.status.code() {
        Some(0) => return Some(Vec::new()),
        Some(1) => {}
        _ => return None,
    }
    let mut paths: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .skip(1)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .filter(|p| scope.overlaps(&Scope::new(p.clone())))
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Whether git config, in any scope, defines a merge driver
/// (`merge.<name>.driver`): then a merge-tree probe could run it, so the
/// advisory check doesn't run (#17101). Attributes are read
/// from the empty tree, and one naming an undefined driver falls back to
/// git's own merge, so no other configured command can run. True too when
/// git can't say.
fn merge_drivers_configured(top: &Path) -> bool {
    let Ok(out) = git::command(top)
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^merge\..*\.driver$",
        ])
        .run(None)
    else {
        return true;
    };
    // Exit 1: no such key.
    match out.status.code() {
        Some(1) => false,
        Some(0) => !out.stdout.is_empty(),
        _ => true,
    }
}

/// The shared-lease merge advisory (#16942): for `l`, a shared or
/// branch-held path lease of `actor`'s, each co-holder whose worktree is
/// found and whose branch head conflicts with this worktree's under the
/// lease.
/// Committed content only; a co-holder whose worktree isn't found, or a
/// merge git can't judge, is left out.
fn lease_merge_conflicts(
    repo: &Repo,
    actor: &str,
    state: &Coordinator,
    l: &Lease,
    changes: &mut Changes,
) -> Vec<MergeConflict> {
    // `git merge-tree --write-tree` writes objects: never under
    // `--read-only`, which changes nothing in git (#18694).
    if !co_holdable(l)
        || !is_path_lease(l)
        || is_named_scope(&l.scope)
        || mdkg_coord_sqlite::store_writes_forbidden()
    {
        return Vec::new();
    }
    let mut owners: Vec<&str> = co_holders(state, l)
        .into_iter()
        .map(|h| h.owner.as_str())
        .filter(|o| *o != actor)
        .collect();
    owners.sort_unstable();
    owners.dedup();
    if owners.is_empty() {
        return Vec::new();
    }
    let Some(ours) = head_commit(&repo.top) else {
        return Vec::new();
    };
    // Never run a configured merge driver to give advice (#17101).
    if merge_drivers_configured(&repo.top) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for owner in owners {
        let Some(top) = owner_top(repo, actor, owner, changes) else {
            continue;
        };
        let Some(theirs) = head_commit(&top) else {
            continue;
        };
        if let Some(paths) = merge_tree_conflicts(&repo.top, &ours, &theirs, &l.scope)
            && !paths.is_empty()
        {
            out.push(MergeConflict {
                owner: owner.to_string(),
                paths,
            });
        }
    }
    out
}

/// The `coord.merge_conflict` warning for one co-holder's conflicts under
/// a shared lease. The paths are locations and the owner a holder, never prose.
fn merge_conflict_notice(c: &MergeConflict) -> Diagnostic {
    let mut d = Diagnostic::warning(
        Code::CoordMergeConflict,
        format!(
            "a merge of a co-holder's branch with yours conflicts in {} file(s) under your shared \
             lease (the files are this warning's locations, the co-holder its holder)",
            c.paths.len()
        ),
    )
    .held_by([c.owner.clone()]);
    let mut paths = c.paths.iter();
    if let Some(first) = paths.next() {
        d = d.at(first.clone(), None);
    }
    for p in paths {
        d = d.also_at(p.clone(), None);
    }
    d.with_hint("tell them what you change, or merge their work first; nothing is refused")
}

/// What `status` and `holds` say about an active path lease's paths, read
/// in its owner's worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PathState {
    /// No changes, staged or not; `None` when git can't say or the
    /// owner's worktree isn't found.
    clean: Option<bool>,
    /// Clean, committed by the owner since it was granted, and judged in
    /// the owner's own worktree: the owner's `release --committed`
    /// releases it.
    committed: bool,
}

/// The worktree `owner`'s changes are judged in: this one for `actor`,
/// else the one named after the owner (an actor's default name), if
/// exactly one is.
fn owner_top(repo: &Repo, actor: &str, owner: &str, changes: &mut Changes) -> Option<PathBuf> {
    if owner == actor {
        return Some(repo.top.clone());
    }
    let named: Vec<&PathBuf> = changes
        .worktrees(&repo.top)
        .iter()
        .filter(|wt| {
            wt.file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(owner))
        })
        .collect();
    match named.as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

/// Each active path lease's [`PathState`], for every owner whose worktree
/// is found.
fn path_states(
    repo: &Repo,
    actor: &str,
    state: &Coordinator,
    now: u64,
) -> std::collections::HashMap<LeaseId, PathState> {
    let mut changes = Changes::default();
    let mut tops: std::collections::HashMap<String, Option<PathBuf>> =
        std::collections::HashMap::new();
    let mut out = std::collections::HashMap::new();
    for l in state.leases() {
        if !(l.confirmed && l.status == Status::Active && l.deadline > now && is_path_lease(l))
            || is_named_scope(&l.scope)
        {
            continue;
        }
        let top = tops
            .entry(l.owner.clone())
            .or_insert_with(|| owner_top(repo, actor, &l.owner, &mut changes))
            .clone();
        let Some(top) = top else {
            out.insert(l.id.clone(), PathState::default());
            continue;
        };
        let clean = changes.strictly_clean(&top, l);
        // `release --committed` runs only in the holder's own worktree
        // (#18021): no promise where another actor works.
        let committed = clean == Some(true)
            && (l.owner != actor || owns_worktree(repo, actor))
            && changes.committed_since(&top, &l.scope, granted_at(state, l), now) == Some(true);
        out.insert(l.id.clone(), PathState { clean, committed });
    }
    out
}

/// Whether `actor` is the one working in this worktree: the actor its
/// folder names (an actor's default name, as `status` finds another
/// owner's worktree). A lease authority's host acting `--as` a client, or
/// anyone else's `--as`, isn't.
/// What git's post-commit hook gives back (`lease.committed`): the path
/// leases `release --committed` releases, for the actor this worktree is
/// named after only, since it reads this worktree's own commits. The
/// scopes released; a lease whose release failed is left to lapse.
pub(crate) fn release_after_commit(root: &Path, actor: &str) -> Result<Vec<String>, GraphError> {
    let repo = Repo::open(root)?;
    if !owns_worktree(&repo, actor) {
        return Ok(Vec::new());
    }
    // HEAD is the commit just made: it counts after a claim in its second.
    let head = git::command(&repo.top)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .run(None)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty());
    let report = release_all_local_with(&repo, actor, ReleaseSelection::Committed, head)?;
    Ok(report
        .data
        .map(|d| d.released.into_iter().map(|l| l.scope).collect())
        .unwrap_or_default())
}

fn owns_worktree(repo: &Repo, actor: &str) -> bool {
    // Not `$MDKG_ACTOR`: a local authority wrapper inherits the client's
    // (#17066). An actor named otherwise keeps the first-quarter rule.
    repo.folder_actor().eq_ignore_ascii_case(actor)
}

/// The tracked paths with changes git can see in the worktree at `top`,
/// relative to the top: staged or unstaged. Untracked and ignored files
/// are [`strictly_clean`]'s to find, file by file: an untracked folder
/// listed as itself would make a missing path in it look dirty for a
/// sibling's sake (#18021). `None` when git can't say.
fn changed_paths(top: &Path) -> Option<Vec<Scope>> {
    let out = git::command(top)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=no",
            "--no-renames",
            "--ignore-submodules=all",
        ])
        .run(None)
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|e| e.len() > 3)
            .map(|e| Scope::new(String::from_utf8_lossy(&e[3..]).into_owned()))
            .collect(),
    )
}

/// Whether `l`'s path, anything in it, or a tracked file around it has
/// changes.
fn has_changes(l: &Lease, changed: &[Scope]) -> bool {
    changed.iter().any(|p| l.scope.overlaps(p))
}

/// Each worktree's changed paths, read once a command. A file lease
/// protects uncommitted edits only (decided 2026-10-30, #11980).
#[derive(Default)]
struct Changes {
    read: std::collections::HashMap<PathBuf, Option<Vec<Scope>>>,
    worktrees: Option<Vec<PathBuf>>,
    /// Each worktree's own commits ([`own_commits`]), read once.
    commits: std::collections::HashMap<PathBuf, Option<Vec<OwnCommit>>>,
    /// Each commit's paths ([`commit_paths`]), read once (#17074).
    paths: std::collections::HashMap<String, Option<Vec<Scope>>>,
    /// Each worktree's [`StrictRead`], read once.
    strict: std::collections::HashMap<PathBuf, Option<StrictRead>>,
    /// The commit git's post-commit hook runs for ([`release_after_commit`]).
    just_made: Option<String>,
}

impl Changes {
    /// Whether the worktree at `top` committed to `scope` since `since_ms`
    /// itself (#16988): one of its own commits ([`own_commits`]), recorded
    /// in a second that began at or after `since_ms` and not after `now`,
    /// whose own change touches the scope. Every entry in the window
    /// counts, in any order (#17074: a backdated commit can be recorded
    /// after a later one). A commit in the second the lease was granted
    /// doesn't count, so a claim made just after a commit keeps its
    /// protection; one dated in the future (a skewed clock) doesn't either.
    /// `None` when git can't say.
    fn committed_since(
        &mut self,
        top: &Path,
        scope: &Scope,
        since_ms: u64,
        now: u64,
    ) -> Option<bool> {
        let commits = self
            .commits
            .entry(top.to_path_buf())
            .or_insert_with(|| own_commits(top))
            .clone()?;
        for OwnCommit { at, sha, replaced } in commits {
            // The commit git's post-commit hook runs for was made now,
            // after every claim still active: the reflog's whole second
            // can't order it against a claim in that second.
            let at = if self.just_made.as_deref() == Some(sha.as_str()) {
                now
            } else {
                at
            };
            if at < since_ms || at > now {
                continue;
            }
            // An amend counts only what it changed: a path it carries
            // over was settled by the commit it replaced (Terra #21577 F2).
            let key = match &replaced {
                Some(r) => format!("{r}..{sha}"),
                None => sha.clone(),
            };
            let paths = self
                .paths
                .entry(key)
                .or_insert_with(|| match &replaced {
                    Some(r) => amend_paths(top, r, &sha),
                    None => commit_paths(top, &sha),
                })
                .as_deref()?;
            // A path the lease contains: never an ancestor file, which a
            // lease on a path beneath it (one about to replace the file
            // with a folder) hasn't been committed to (#17128).
            if paths.iter().any(|p| scope.contains(p)) {
                return Some(true);
            }
        }
        Some(false)
    }

    fn in_worktree(&mut self, top: &Path) -> Option<&[Scope]> {
        self.read
            .entry(top.to_path_buf())
            .or_insert_with(|| changed_paths(top))
            .as_deref()
    }

    /// Every worktree of the repository `top` belongs to.
    fn worktrees(&mut self, top: &Path) -> &[PathBuf] {
        self.worktrees
            .get_or_insert_with(|| git::worktrees(top).unwrap_or_default())
    }

    /// [`Self::of`] then [`strictly_clean`], in `top` and in the worktree
    /// `of` judged the path in when `top` lacks it (#17070): whether `l`'s
    /// paths are clean enough for `release --clean` and `--committed` and for
    /// `clean`.
    fn strictly_clean(&mut self, top: &Path, l: &Lease) -> Option<bool> {
        match self.of(top, l) {
            Some(false) => {}
            other => return other.map(|dirty| !dirty),
        }
        let rel = l.scope.as_str();
        let elsewhere = if top.join(rel).symlink_metadata().is_ok() {
            None
        } else {
            self.worktrees(top)
                .iter()
                .find(|wt| wt.as_path() != top && wt.join(rel).symlink_metadata().is_ok())
                .cloned()
        };
        let here = self.strictly_clean_in(top, &l.scope)?;
        match elsewhere {
            Some(wt) if here => self.strictly_clean_in(&wt, &l.scope),
            _ => Some(here),
        }
    }

    /// [`strictly_clean`] in the worktree at `top`, over its read once.
    fn strictly_clean_in(&mut self, top: &Path, scope: &Scope) -> Option<bool> {
        let read = self
            .strict
            .entry(top.to_path_buf())
            .or_insert_with(|| StrictRead::of(top))
            .as_ref()?;
        strictly_clean(top, scope, read)
    }

    /// Whether `l`'s path has tracked changes, judged in the worktree at
    /// `top`. A path `top` doesn't have is judged in the worktree that has
    /// it, as a claim finds it there (#11980); a path no worktree has (a
    /// `--new` file not written yet) has none. `None` when git can't say.
    fn of(&mut self, top: &Path, l: &Lease) -> Option<bool> {
        if has_changes(l, self.in_worktree(top)?) {
            return Some(true);
        }
        let rel = l.scope.as_str();
        if top.join(rel).symlink_metadata().is_ok() {
            return Some(false);
        }
        let elsewhere = self
            .worktrees(top)
            .iter()
            .find(|wt| wt.as_path() != top && wt.join(rel).symlink_metadata().is_ok())
            .cloned();
        match elsewhere {
            Some(wt) => self.in_worktree(&wt).map(|c| has_changes(l, c)),
            None => Some(false),
        }
    }
}

/// Path leases whose path has no changes in their owner's worktree, that
/// weren't just claimed, while no turn of the owner's runs: nothing is
/// working in them, renew won't keep them, and they block others until
/// they're released or lapse. `actor`'s worktree is this one; another
/// owner's is the worktree named after it (an actor's default name), and
/// an owner with no such worktree, or one git can't read, has none idle.
fn idle_leases(
    repo: &Repo,
    actor: &str,
    state: &Coordinator,
    now: u64,
) -> std::collections::HashSet<LeaseId> {
    let mut by_owner: std::collections::BTreeMap<&str, Vec<&Lease>> =
        std::collections::BTreeMap::new();
    for l in state.leases() {
        if l.confirmed
            && l.status == Status::Active
            && l.deadline > now
            && is_path_lease(l)
            && !just_claimed(state, l, now)
        {
            by_owner.entry(l.owner.as_str()).or_default().push(l);
        }
    }
    let mut changes = Changes::default();
    let mut idle = std::collections::HashSet::new();
    for (owner, leases) in by_owner {
        if in_a_turn(state, owner, now) {
            continue;
        }
        let Some(top) = owner_top(repo, actor, owner, &mut changes) else {
            continue;
        };
        for l in leases {
            // As renew and `release --clean` judge it (#17125).
            if changes.strictly_clean(&top, l) == Some(true) {
                idle.insert(l.id.clone());
            }
        }
    }
    idle
}

/// Whether an active lease is in the last eighth of its time.
fn expiring(state: &Coordinator, l: &Lease, now: u64) -> bool {
    l.deadline.saturating_sub(now) <= state.ttl() / 8
}

/// Leases that expired in the last 24 hours, by `actor` (anyone's when
/// `None`), and that their owner hasn't claimed again since.
fn recently_expired<'a>(state: &'a Coordinator, actor: Option<&str>, now: u64) -> Vec<&'a Lease> {
    let leases: Vec<&Lease> = state.leases().collect();
    leases
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.status == Status::Expired
                && l.confirmed
                && actor.is_none_or(|a| l.owner == a)
                && now.saturating_sub(l.deadline) < EXPIRED_SHOWN_MS
        })
        .filter(|(i, l)| {
            !leases[i + 1..]
                .iter()
                .any(|later| later.owner == l.owner && later.confirmed && later.scope == l.scope)
        })
        .map(|(_, l)| *l)
        .collect()
}

/// Whether the actor was named (`--as`, then `$MDKG_ACTOR`) rather than
/// taken from the worktree's folder name, resolved exactly as
/// [`Repo::actor`] resolves it: a blank `--as` hides `$MDKG_ACTOR`.
pub(crate) fn named_actor(flag: Option<&String>) -> bool {
    flag.cloned()
        .or_else(|| std::env::var("MDKG_ACTOR").ok())
        .is_some_and(|a| !a.trim().is_empty())
}

/// The deadline with the time left, so nobody has to compare a UTC time
/// with a local clock.
fn until_text(l: &LeaseRecord, now: u64) -> String {
    let mins = l.deadline_ms.saturating_sub(now).div_ceil(60_000);
    if l.expiring == Some(true) {
        format!(
            "{} (expires in {mins} min; `{BIN} renew` extends it)",
            l.until
        )
    } else if mins >= 60 {
        format!("{} ({}h {}m left)", l.until, mins / 60, mins % 60)
    } else {
        format!("{} ({mins} min left)", l.until)
    }
}

/// For commands that hand out work (`next`, `goal next`): a lease
/// authority that's configured but can't be trusted or read fails them, as
/// an unreadable store does: who holds what can't be known.
pub(crate) fn authority_usable(root: &Path) -> Result<(), GraphError> {
    match Repo::open(root) {
        Ok(repo) => authority::find(&repo).map(|_| ()),
        Err(_) => Ok(()),
    }
}

/// A warning for `status` when leases are held by a lease authority (or one
/// is configured but can't be used): the local store doesn't show them.
pub(crate) fn authority_notice(root: &Path) -> Option<Diagnostic> {
    let repo = Repo::open(root).ok()?;
    match authority::find(&repo) {
        Ok(None) => None,
        Ok(Some(a)) => Some(
            Diagnostic::warning(
                Code::CoordAuthorityRemote,
                format!(
                    "leases are held by the lease authority `{}`; this shows only the local store",
                    a.name()
                ),
            )
            .with_hint("claim, release and renew go through the authority"),
        ),
        Err(e) => {
            let mut d = *e.diagnostic;
            d.severity = Severity::Warning;
            Some(d)
        }
    }
}

/// Missing source objects are unmerged; a failed history check must never
/// become persisted false settlement evidence.
fn status_merged(
    repo: &Repo,
    state: &Coordinator,
    main: &str,
) -> std::io::Result<std::collections::BTreeSet<String>> {
    // A refresh may release the graph lock before this store snapshot was
    // read. Never project an older bound tip onto a newer coordinator.
    if let Some(mode) = &repo.mode {
        let current = git::commit_of(&repo.top, &mode.graph.name)?;
        if current != main {
            return Err(std::io::Error::other(
                "the bound graph tip changed before the coordinator snapshot was validated",
            ));
        }
    }
    let commits: std::collections::BTreeSet<String> = state
        .settlement_queue()
        .into_iter()
        .filter_map(|c| c.source.as_ref())
        .map(|s| s.commit.clone())
        .collect();
    let mut merged = std::collections::BTreeSet::new();
    for commit in commits {
        match git::is_ancestor_of(&repo.top, &commit, main) {
            Ok(true) => {
                merged.insert(commit);
            }
            Ok(false) => {}
            Err(error) => {
                // Batch-check reports an absent object with success and an
                // explicit "missing" line. Command/IO failures stay errors.
                let input = format!("{commit}\n");
                let probe = git::command(&repo.top)
                    .args(["cat-file", "--batch-check"])
                    .run(Some(input.as_bytes()))?;
                if !probe.status.success()
                    || !probe.stderr.is_empty()
                    || String::from_utf8_lossy(&probe.stdout).trim() != format!("{commit} missing")
                {
                    return Err(error);
                }
            }
        }
    }
    Ok(merged)
}

/// `status`: every active lease, the actor's first in text (only the
/// actor's with `mine`; JSON lists everyone's either way), the actor's
/// expired leases (everyone's with `all`), the settlement queue, temporary
/// statuses and unacked mail. With `observe` false (`--no-observe`, or
/// `--read-only`) what main holds is applied to the snapshot shown but not
/// recorded, so status writes nothing to the store.
pub fn status(
    root: &Path,
    actor: Option<String>,
    all: bool,
    mine: bool,
    observe: bool,
    json: bool,
) -> u8 {
    let observe = observe && !mdkg_coord_sqlite::store_writes_forbidden();
    let now = now_ms();
    let report = (|| -> Result<Report<StatusData>, GraphError> {
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        let path = git::store_path(&repo.top).map_err(git_error)?;
        unreachable_store(&path)?;
        let store = Store::open_read_only(&path).map_err(|e| store_error_in(e, Some(repo.command_root())))?;
        let had_store = store.is_some();
        let store_id = match &store {
            Some(store) => Some(store.store_id().map_err(store_error)?),
            None => None,
        };
        let mut state = match &store {
            Some(store) => store.coordinator().map_err(store_error)?,
            None => Coordinator::new(ttl_ms()),
        };
        let mail = match &store {
            Some(store) => crate::msg::unacked(store, &actor, now)?,
            None => Vec::new(),
        }
        .into_iter()
        .map(|(mailbox, unacked)| MailCount { mailbox, unacked })
        .collect();
        // No SQLite handle or transaction survives into Git, index reads or
        // output. Expiry is projected only; observation recording uses a
        // separate zero-wait connection after Git has finished.
        drop(store);
        let projection = match &repo.mode {
            Some(mode) => Some(mode.commit()),
            None => repo.observation_ref()?,
        };
        let mut observation_warning = None;
        let observation = if let Some(main) = projection {
            match status_merged(&repo, &state, &main) {
                Ok(merged) => {
                    let observation = mdkg_coord::Observation {
                        read_generation: state.generation(),
                        leaves: repo.graph_git().leaves_read_only_cached(
                            &main,
                            &repo.graph_scope().to_string(),
                            git::digest_cache_path(&path).as_deref(),
                        ).map_err(git_error)?,
                        merged,
                        prefix: Some(repo.graph_scope()),
                    };
                    state.apply(&Command::ObserveMain {
                        observation: observation.clone(),
                    });
                    Some(observation)
                }
                Err(error) => {
                    observation_warning = Some(Diagnostic::warning(
                        Code::CoordStoreFailed,
                        format!("observation deferred: history validation failed: {error}"),
                    ));
                    None
                }
            }
        } else {
            None
        };
        state.expire_due(now);
        // Open changesets whose commit still carries abandoned work.
        // A failed history check leaves status usable from its stored snapshot.
        let only = if observation_warning.is_none() {
            carried_by_open(&repo, &state)?
        } else {
            std::collections::BTreeMap::new()
        };
        let abandoned: Vec<&mdkg_coord::Changeset> = state
            .changesets()
            .filter(|c| c.state == Settlement::Abandoned && c.source.is_some())
            .collect();
        let mut carried: std::collections::BTreeMap<ChangesetId, Vec<&mdkg_coord::Changeset>> =
            std::collections::BTreeMap::new();
        for q in state.settlement_queue() {
            let Some(set) = q.source.as_ref().and_then(|s| only.get(&s.commit)) else {
                continue;
            };
            for a in &abandoned {
                if a.source.as_ref().is_some_and(|s| set.contains(&s.commit)) {
                    carried.entry(q.id).or_default().push(a);
                }
            }
        }
        let mut warnings: Vec<Diagnostic> = carried
            .into_iter()
            .filter_map(|(q, abandoned)| {
                let q = state.changeset(q)?;
                let ids: Vec<String> = abandoned.iter().map(|a| a.id.to_string()).collect();
                Some(
                    Diagnostic::warning(
                        Code::CoordContainsAbandoned,
                        // Its branch is another actor's choice: data
                        // (`queue[].branch`), never prose.
                        format!("{} contains abandoned {}", q.id, ids.join(", ")),
                    )
                    .with_hint(format!(
                        "drop those commits from its branch (a revert keeps them in its history), or run `{BIN} abandon {}`",
                        q.id
                    )),
                )
            })
            .collect();
        warnings.extend(observation_warning);
        if observe
            && had_store
            && let Some(observation) = &observation
        {
            let deferred = match Store::try_record_observation(&path, observation) {
                Ok(reason) => reason.map(str::to_owned),
                Err(error) => Some(error.to_string()),
            };
            if let Some(reason) = deferred {
                warnings.push(Diagnostic::warning(
                    Code::CoordStoreFailed,
                    format!("observation deferred: {reason}"),
                ));
            }
        }
        let idle = idle_leases(&repo, &actor, &state, now);
        let paths = path_states(&repo, &actor, &state, now);
        let mut changes = Changes::default();
        let leases = state
            .leases()
            .filter(|l| l.confirmed && l.status == Status::Active)
            .map(|l| {
                let merge_conflicts = if l.owner == actor && l.deadline > now {
                    lease_merge_conflicts(&repo, &actor, &state, l, &mut changes)
                } else {
                    Vec::new()
                };
                let record = LeaseRecord {
                    mine: Some(l.owner == actor),
                    expiring: Some(expiring(&state, l, now)),
                    idle: Some(idle.contains(&l.id)),
                    merge_conflicts,
                    ..lease_record_in(&state, l)
                };
                with_path_state(record, &state, l, paths.get(&l.id).copied(), now)
            })
            .collect();
        let expired = recently_expired(&state, (!all).then_some(actor.as_str()), now)
            .into_iter()
            .map(|l| LeaseRecord {
                mine: Some(l.owner == actor),
                ..lease_record(l)
            })
            .collect();
        let mut leases: Vec<LeaseRecord> = leases;
        let mut expired: Vec<LeaseRecord> = expired;
        let mut work: Vec<WorkRecord> = state
            .work_states()
            .filter(|(_, w)| w.status != "done")
            .map(|(scope, w)| WorkRecord {
                scope: scope.to_string(),
                node: None,
                status: w.status.clone(),
                actor: w.actor.clone(),
                since: rfc3339(w.at),
            })
            .collect();
        name_store_scopes(
            &repo,
            leases
                .iter_mut()
                .chain(expired.iter_mut())
                .map(|l| (l.scope.as_str(), &mut l.node))
                .chain(work.iter_mut().map(|w| (w.scope.as_str(), &mut w.node))),
        );
        let graph = graph_status(&repo);
        Ok(Report::ok(StatusData {
            actor,
            main: match &repo.mode {
                Some(mode) => mode.graph.name.clone(),
                None => main_ref(&repo.top),
            },
            code_base: repo
                .mode
                .as_ref()
                .and_then(|m| m.graph.main_ref_base.clone()),
            store_id,
            graph,
            leases,
            expired,
            queue: state
                .settlement_queue()
                .into_iter()
                .map(changeset_record)
                .collect(),
            work,
            mail,
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed)
    .with(authority_notice(root))
    .with(authority::committed_key_warning(root));
    output::emit("status", json, report, |d| {
        // Scopes, owners, files and states come from other actors, and
        // older stores held them unchecked: every line is escaped.
        let say = |line: String| println!("{}", output::shown(&line));
        let settling = d
            .graph
            .as_ref()
            .and_then(|g| g.name.as_deref())
            .unwrap_or(d.main.as_str());
        say(format!("acting as {}; settling on {}", d.actor, settling));
        // Yours first, then everyone else's, each in the store's order.
        let (yours, theirs): (Vec<&LeaseRecord>, Vec<&LeaseRecord>) =
            d.leases.iter().partition(|l| l.mine == Some(true));
        let shown: Vec<&LeaseRecord> = if mine {
            yours
        } else {
            yours.into_iter().chain(theirs).collect()
        };
        if shown.is_empty() {
            say(format!(
                "no active leases{}",
                if mine { " of yours" } else { "" }
            ));
        }
        for l in &shown {
            let shared = if l.shared_with.is_empty() {
                String::new()
            } else {
                format!(" shared with {}", l.shared_with.join(", "))
            };
            let idle = if l.idle == Some(true) {
                " (idle: no changes in its owner's worktree, no turn running)"
            } else {
                ""
            };
            let age = l.age_ms.map_or(String::new(), |ms| {
                format!("; age {}{}", age_text(ms), paths_text(l))
            });
            let branch = l
                .branch
                .as_ref()
                .map_or(String::new(), |b| format!(" on branch {b}"));
            say(format!(
                "lease {} {}{}{branch} by {}{shared} until {}{age}{idle}",
                l.id,
                l.scope,
                NodeName::suffix(l.node.as_ref()),
                l.owner,
                until_text(l, now)
            ));
            for c in &l.merge_conflicts {
                say(format!(
                    "  conflicts at merge with {}'s branch: {}",
                    c.owner,
                    c.paths.join(", ")
                ));
            }
        }
        let idle = shown.iter().filter(|l| l.idle == Some(true)).count();
        if idle > 0 {
            say(format!(
                "{idle} idle lease(s) block others for nothing: renew won't keep them, and their \
                 owners can give them up now with `{BIN} release <path>`"
            ));
        }
        let others = d.leases.len() - shown.len();
        if others > 0 {
            say(format!(
                "{others} more held by others (`{BIN} status` lists them)"
            ));
        }
        for l in &d.expired {
            say(format!(
                "expired {} {}{} by {} at {}; claim it again to keep it",
                l.id,
                l.scope,
                NodeName::suffix(l.node.as_ref()),
                l.owner,
                l.until
            ));
        }
        if d.queue.is_empty() {
            say("nothing waiting to merge".to_string());
        }
        for m in &d.mail {
            say(format!("mail: {} unacked for {}", m.unacked, m.mailbox));
        }
        for c in &d.queue {
            let after = if c.depends_on.is_empty() {
                String::new()
            } else {
                format!(" after {}", c.depends_on.join(", "))
            };
            say(format!(
                "{} {} on {}{after}: {}",
                c.id,
                c.state,
                c.branch.as_deref().unwrap_or("-"),
                c.files.join(", ")
            ));
        }
        for w in &d.work {
            say(format!(
                "{}{} {} by {} since {}",
                w.scope,
                NodeName::suffix(w.node.as_ref()),
                w.status,
                w.actor,
                w.since
            ));
        }
    })
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SyncData {
    /// Commits merged into this worktree's branch, in order.
    pub stacked: Vec<String>,
    /// Unmerged versions in the scope after syncing.
    pub files: Vec<FileRecord>,
    pub up_to_date: bool,
}

pub fn sync(root: &Path, _actor: Option<String>, target: Option<&str>, json: bool) -> u8 {
    let report = (|| -> Result<Report<SyncData>, GraphError> {
        let repo = Repo::open(root)?;
        let scope = match target {
            Some(t) => {
                refuse_ambiguous(&config::load(root)?, t)?;
                repo.resolve_handle_target(t)?.scope
            }
            None => repo.graph_scope(),
        };
        let mut store = repo.store()?;
        repo.observe(&mut store, now_ms())?;
        // Ref mode: every reader reads the graph ref, which holds every
        // published version, so there's nothing to bring into a checkout
        // (and a code branch never takes graph commits).
        if repo.mode.is_some() {
            return Ok(Report::ok(SyncData {
                stacked: Vec::new(),
                files: Vec::new(),
                up_to_date: true,
            })
            .with([graph_git::graph_info(
                "graph.nothing_to_sync",
                "the graph ref holds every published version: nothing to bring in",
            )]));
        }
        let state = store.coordinator().map_err(store_error)?;
        let missing = missing_versions(&repo, &state, &scope)?;
        let needed: Vec<(String, String)> = state
            .settlement_queue()
            .into_iter()
            .filter(|c| c.state == Settlement::Sequenced)
            .filter_map(|c| {
                c.source
                    .as_ref()
                    .map(|s| (s.commit.clone(), s.branch.clone()))
            })
            .filter(|(commit, _)| missing.iter().any(|f| f.commit.as_deref() == Some(commit)))
            .collect();
        let mut stacked: Vec<String> = Vec::new();
        for (commit, _branch) in needed {
            if !git::has_ref(&repo.top, &commit) {
                return Err(GraphError::conflict(
                    Code::CoordSyncFailed,
                    // Its branch is another actor's choice: `status` shows
                    // it as data (`queue[].branch`), never here.
                    format!("the version's commit {commit} is no longer in this repository"),
                )
                .with_hint(format!(
                    "if its branch was dropped, abandon its work: `{BIN} status` lists the \
                     changeset and branch (`{BIN} abandon <changeset>`)"
                )));
            }
            if let Err(e) = git::stack_on(&repo.top, &commit) {
                return Err(sync_failure(&repo, &commit, &stacked, &e));
            }
            stacked.push(commit);
        }
        let files = missing_versions(&repo, &state, &scope)?;
        let diagnostics: Vec<Diagnostic> = files
            .iter()
            .map(|f| {
                Diagnostic::warning(Code::CoordSyncNeeded, "unmerged version is still missing")
                    .at(f.path.clone(), None)
            })
            .collect();
        Ok(Report::ok(SyncData {
            stacked,
            up_to_date: files.is_empty(),
            files,
        })
        .with(diagnostics))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("sync", json, report, |d| {
        for c in &d.stacked {
            println!("stacked on {c}");
        }
        if d.up_to_date {
            println!("up to date");
        }
    })
}

/// Why stacking on `commit` failed, and what to do: a merge left in progress
/// needs resolving or aborting; a merge that never started (local changes in
/// the way) left the worktree as it was.
fn sync_failure(repo: &Repo, commit: &str, stacked: &[String], e: &std::io::Error) -> GraphError {
    let before = if stacked.is_empty() {
        String::new()
    } else {
        format!(" (already stacked on {})", stacked.join(", "))
    };
    match git::merge_conflicts(&repo.top) {
        Ok(Some(paths)) => GraphError::conflict(
            Code::CoordSyncFailed,
            format!(
                "stacking on {commit} conflicted, and a merge is in progress{before}: {}",
                // Paths another branch named: escape them for display.
                if paths.is_empty() {
                    "-".into()
                } else {
                    output::shown(&paths.join(", "))
                }
            ),
        )
        .with_hint("resolve the conflicts and commit, or run `git merge --abort`; then sync again"),
        _ => GraphError::conflict(
            Code::CoordSyncFailed,
            format!("stacking on {commit} failed{before}: {e}"),
        )
        .with_hint("commit or stash local changes, then sync again"),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AbandonData {
    /// Every changeset abandoned, including work built on it.
    pub abandoned: Vec<String>,
    /// Those abandoned because their commit contains abandoned work (a
    /// branch that synced on it), so merging them would bring it to main.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contained: Vec<String>,
}

/// Open changesets whose commit carries one of `commits` that main doesn't
/// have yet (`carried`, from [`carried_by_open`]). Abandoned work that
/// reached main anyway is main's now, not a branch's to carry.
fn containing<'a>(
    carried: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    state: &'a Coordinator,
    commits: &[String],
) -> Vec<&'a mdkg_coord::Changeset> {
    state
        .settlement_queue()
        .into_iter()
        .filter(|q| {
            q.source.as_ref().is_some_and(|s| {
                carried
                    .get(&s.commit)
                    .is_some_and(|only| commits.iter().any(|a| only.contains(a)))
            })
        })
        .collect()
}

/// For each open changeset's commit, the commits in its history that main
/// doesn't have. A commit git no longer has carries nothing; any other git
/// failure is reported, so the cascade and the warning never fail silently.
fn carried_by_open(
    repo: &Repo,
    state: &Coordinator,
) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>, GraphError> {
    // Ref mode: commits the graph ref lacks.
    let main = repo
        .mode
        .as_ref()
        .map_or_else(|| main_ref(&repo.top), |m| m.graph.name.clone());
    let open: std::collections::BTreeSet<String> = state
        .settlement_queue()
        .into_iter()
        .filter_map(|q| q.source.as_ref().map(|s| s.commit.clone()))
        .collect();
    let mut out = std::collections::BTreeMap::new();
    for c in open {
        let only = if git::has_ref(&repo.top, &c) {
            git::commits_not_on(&repo.top, &c, &main).map_err(git_error)?
        } else {
            std::collections::BTreeSet::new()
        };
        out.insert(c, only);
    }
    Ok(out)
}

/// Whether the graph ref holds a changeset: its commit is in the ref's
/// history, or every leaf it changes has its content there.
pub(crate) fn published(
    repo: &Repo,
    mode: &RefMode,
    c: &mdkg_coord::Changeset,
) -> Result<bool, GraphError> {
    if let Some(s) = &c.source
        && git::has_ref(&repo.top, &s.commit)
        && git::is_ancestor_of(&repo.top, &s.commit, &mode.graph.name).map_err(git_error)?
    {
        return Ok(true);
    }
    let seam = repo.graph_git();
    for (leaf, content) in &c.changes {
        let have = seam
            .leaves(&mode.graph.name, leaf.as_str())
            .map_err(git_error)?
            .remove(leaf);
        if have != *content {
            return Ok(false);
        }
    }
    Ok(!c.changes.is_empty())
}

fn parse_changeset(raw: &str) -> Result<ChangesetId, GraphError> {
    raw.strip_prefix("cs-")
        .and_then(|n| n.parse().ok())
        .map(ChangesetId)
        .ok_or_else(|| {
            GraphError::usage(Code::UsageUnknownChangeset, "that isn't a changeset id").with_hint(
                format!(
                    "changeset ids look like cs-3; see them with `{} status`",
                    crate::BIN
                ),
            )
        })
}

pub fn abandon(root: &Path, changeset: Option<&str>, branch: Option<&str>, json: bool) -> u8 {
    let report = (|| -> Result<Report<AbandonData>, GraphError> {
        if std::env::var_os(mdkg_run::profile::TURN_MARK).is_some_and(|mark| !mark.is_empty()) {
            return Err(GraphError::usage(
                Code::RunUnsupported,
                "abandon is refused in an agent turn",
            ));
        }
        let repo = Repo::open(root)?;
        let mut store = repo.store()?;
        repo.observe(&mut store, now_ms())?;
        let state = store.coordinator().map_err(store_error)?;
        let ids: Vec<ChangesetId> = match (changeset, branch) {
            (Some(raw), _) => vec![parse_changeset(raw)?],
            (None, Some(b)) => state
                .settlement_queue()
                .into_iter()
                .filter(|c| c.source.as_ref().is_some_and(|s| s.branch == b))
                .map(|c| c.id)
                .collect(),
            (None, None) => Vec::new(),
        };
        if ids.is_empty() {
            return Err(GraphError::not_found(
                Code::CoordUnknownChangeset,
                "no open changesets from that branch",
            )
            .with_hint(format!("see open changesets with `{} status`", crate::BIN)));
        }
        // Ref mode: a recorded changeset is published, and the graph ref
        // holding its commit or its content can't be taken back by
        // abandoning it (edd-01a18867, Settlement).
        if let Some(mode) = &repo.mode {
            for id in &ids {
                if let Some(c) = state.changeset(*id)
                    && published(&repo, mode, c)?
                {
                    return Err(graph_git::graph_error(
                        "coord.abandon_published",
                        format!(
                            "{id} is published on {}, so it can't be abandoned",
                            mode.graph.name
                        ),
                    )
                    .with_hint(
                        "change the node again (a later write), or revert it with a new write",
                    ));
                }
            }
        }
        // Every git read the cascade needs happens before the first write, so
        // a git failure can't leave the cascade half done.
        let carried = carried_by_open(&repo, &state)?;
        let mut abandoned: Vec<String> = Vec::new();
        for id in ids {
            let current = store.coordinator().map_err(store_error)?;
            if branch.is_some()
                && current
                    .changeset(id)
                    .is_some_and(|c| c.state == Settlement::Abandoned)
            {
                continue; // Dropped with an earlier one from the same branch.
            }
            match store
                .execute(&Command::Abandon { changeset: id })
                .map_err(store_error)?
            {
                Outcome::Abandoned { changesets } => {
                    abandoned.extend(changesets.iter().map(ToString::to_string));
                }
                Outcome::Refused { error } => return Err(refusal(&error)),
                _ => {}
            }
        }
        // History cascade (2026-10-19): a branch that synced on
        // abandoned work carries it, so its open changesets go too.
        let mut contained = Vec::new();
        loop {
            let state = store.coordinator().map_err(store_error)?;
            let commits: Vec<String> = abandoned
                .iter()
                .filter_map(|id| parse_changeset(id).ok())
                .filter_map(|id| state.changeset(id))
                .filter_map(|c| c.source.as_ref().map(|s| s.commit.clone()))
                .collect();
            let next: Vec<ChangesetId> = containing(&carried, &state, &commits)
                .into_iter()
                .map(|c| c.id)
                .collect();
            if next.is_empty() {
                break;
            }
            for id in next {
                if let Outcome::Abandoned { changesets } = store
                    .execute(&Command::Abandon { changeset: id })
                    .map_err(store_error)?
                {
                    for c in changesets {
                        contained.push(c.to_string());
                        abandoned.push(c.to_string());
                    }
                }
            }
        }
        Ok(Report::ok(AbandonData {
            abandoned,
            contained,
        }))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("abandon", json, report, |d| {
        println!("abandoned {}", d.abandoned.join(", "));
        if !d.contained.is_empty() {
            println!(
                "({}: their commit contains abandoned work)",
                d.contained.join(", ")
            );
        }
    })
}

/// Read-only coordinator state for `show` and `list`, if a store exists.
pub struct View {
    repo: Repo,
    state: Coordinator,
    now: u64,
    /// The ledger's presence and settle records, for settled reads (A29);
    /// none when they can't be read.
    ledger: Option<mdkg_coord_sqlite::entries::LedgerReads>,
}

impl View {
    /// The view, `None` when there's no store to read (not in git, or no
    /// coordinator yet), or an error when a store exists or may exist but
    /// can't be read,
    /// for example one a newer build upgraded. Commands that act on leases
    /// must fail on that error, never go on as if nobody held anything
    /// (orchestrator, 2026-10-21); read-only commands may warn and go on.
    pub fn open(root: &Path) -> Result<Option<Self>, GraphError> {
        // Not in git, or in a repository without a store: no coordinator.
        // A store that may be there but git refuses to open (a repository
        // owned by someone else, one it can't read, git missing): an
        // error, never "nobody holds anything".
        let repo = match Repo::open(root) {
            Ok(repo) => repo,
            // A skipped `.git` that may hold the store (Repo::open says so).
            Err(e) if e.diagnostic.code == Code::CoordStoreFailed => return Err(e),
            Err(e) => {
                let Some(entry) = store_may_exist(root) else {
                    return Ok(None);
                };
                return Err(GraphError::failure(
                    Code::CoordStoreFailed,
                    format!(
                        "git can't open the repository here ({}), and {} may hold a \
                         coordinator store",
                        e.diagnostic.message,
                        entry.display()
                    ),
                )
                .with_hint(
                    "check that git is installed and trusts this folder (git config \
                     safe.directory); repair a moved worktree (git worktree repair) or a \
                     `.git` git can't use (HEAD, objects or refs missing, a dangling link); \
                     or check GIT_CEILING_DIRECTORIES, which may hide the repository",
                ));
            }
        };
        let path = git::store_path(&repo.top).map_err(git_error)?;
        match path.try_exists() {
            Ok(false) => {
                unreachable_store(&path)?;
                return Ok(None);
            }
            Ok(true) => {}
            Err(e) => {
                return Err(GraphError::failure(
                    Code::CoordStoreFailed,
                    format!("can't read the coordinator store {}: {e}", path.display()),
                )
                .with_hint("check the permissions of the git folder's mdkg/ folder"));
            }
        }
        let store = Store::open(&path, ttl_ms()).map_err(store_error)?;
        upgraded(&store);
        let state = store.coordinator().map_err(store_error)?;
        let ledger = store.ledger_reads().ok();
        Ok(Some(Self {
            repo,
            state,
            now: now_ms(),
            ledger,
        }))
    }

    /// Settled reads (A29, "A done file that didn't settle reads as
    /// unsettled"): a node whose effective status is `done` and that has a
    /// ledger reads `done (unsettled)` unless it settled, so everything
    /// derived from done (dependency readiness, `next`, goal progress)
    /// counts it as not done; `done (unknown)` when that can't be told.
    pub(crate) fn mark_unsettled(&self, nodes: &mut [IndexNode]) {
        let done: Vec<usize> = (0..nodes.len())
            .filter(|i| nodes[*i].status.as_deref() == Some("done"))
            .collect();
        if done.is_empty() {
            return;
        }
        let Some(reads) = &self.ledger else {
            for i in done {
                if crate::entry::unknown_without_store(&nodes[i]) {
                    nodes[i].status = Some(crate::entry::UNKNOWN.into());
                }
            }
            return;
        };
        let marks = {
            let refs: Vec<(&IndexNode, Option<String>)> = done
                .iter()
                .map(|i| {
                    let leaf = self.repo.scope_for(&nodes[*i].path);
                    let mark = self.state.done_mark(&leaf).map(|c| c.to_string());
                    (&nodes[*i], mark)
                })
                .collect();
            crate::entry::unsettled(&self.repo, reads, &refs)
        };
        for (i, mark) in done.into_iter().zip(marks) {
            if let Some(mark) = mark {
                nodes[i].status = Some(mark.into());
            }
        }
    }

    /// The active, unexpired lease covering a node, if any. A role or a
    /// turn never holds a node, even one in a folder named like it.
    /// The node `qid`, when given, is held under its key too: a renamed
    /// copy's lease holds it.
    pub fn holder(&self, graph_path: &str, qid: Option<&str>) -> Option<&Lease> {
        let leaf = self.repo.scope_for(graph_path);
        // A key only matters when some lease holds one: reads stay cheap.
        let keyed = self.state.leases().any(|l| {
            l.node.is_some() && l.confirmed && l.status == Status::Active && l.deadline > self.now
        });
        let key = qid.filter(|_| keyed).map(|q| self.repo.node_key(&leaf, q));
        self.state.leases().find(|l| {
            !is_role_or_turn(&l.scope)
                && l.confirmed
                && l.status == Status::Active
                && l.deadline > self.now
                && (l.scope.contains(&leaf)
                    || key.as_deref().is_some_and(|k| {
                        l.node.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(k))
                    }))
        })
    }

    /// The effective status: a recorded `review` or `blocked`, else
    /// `done` (a `done` mark whose changeset hasn't been abandoned, or the
    /// file's status), else `progress` while a lease covers the node, else
    /// the file's status. A lease taken to edit a done node's record
    /// doesn't make it in progress again.
    pub fn effective_status(
        &self,
        graph_path: &str,
        qid: Option<&str>,
        file_status: Option<&str>,
    ) -> Option<String> {
        let leaf = self.repo.scope_for(graph_path);
        // A profile C node's file is its record: done there is done, over
        // a review or blocked state a crash left before done cleared it.
        if is_store_scope(&leaf) && file_status == Some("done") {
            return Some("done".into());
        }
        if let Some(w) = self.state.work(&leaf) {
            return Some(w.status.clone());
        }
        if self.state.done_mark(&leaf).is_some() || file_status == Some("done") {
            return Some("done".into());
        }
        if self.holder(graph_path, qid).is_some() {
            return Some("progress".into());
        }
        file_status.map(String::from)
    }

    pub fn record(&self, graph_path: &str, qid: Option<&str>) -> Option<CoordinationRecord> {
        let leaf = self.repo.scope_for(graph_path);
        let lease = self.holder(graph_path, qid).map(|l| LeaseBrief {
            id: l.id.to_string(),
            owner: l.owner.clone(),
            scope: l.scope.to_string(),
            until: rfc3339(l.deadline),
        });
        let pending = self
            .state
            .settlement_queue()
            .into_iter()
            .rev()
            .find(|c| c.changes.contains_key(&leaf))
            .map(|c| PendingBrief {
                changeset: c.id.to_string(),
                state: settlement_name(c.state).into(),
                branch: c.source.as_ref().map(|s| s.branch.clone()),
            });
        let record = CoordinationRecord { lease, pending };
        (!record.is_empty()).then_some(record)
    }
}

/// A workspace's node folder relative to its graph's root: its `path`
/// then its `mdkg_dir`, without `.` or empty segments (the root
/// workspace's is `.mdkg`). Enabled or not, as lease keys have always
/// counted it.
pub(crate) fn workspace_folder(w: &config::Workspace) -> String {
    let dir: Vec<&str> = w
        .path
        .split('/')
        .chain(w.mdkg_dir.split('/'))
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    dir.join("/")
}

/// The lease-key rule for the folder of the node file at `file`: of
/// `folders` (a graph's node folders, [`workspace_folder`]'s), the
/// innermost that holds it, so a file under a workspace nested in
/// another's folder keys in the nested one. Lease creation
/// ([`Repo::node_key`]) and the ledger's graph-ref lookup
/// (`entry::graph_ref_folder`) both call it, so a node's lease and the
/// gate's view of who holds it never disagree (Terra's code review
/// #10220, blocker 1, with the orchestrator's call).
pub(crate) fn innermost_node_folder<'a>(folders: &'a [Scope], file: &Scope) -> Option<&'a Scope> {
    folders
        .iter()
        .filter(|f| f.contains(file) && *f != file)
        .max_by_key(|f| f.as_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_and_git_caches_do_not_cross_trust_contexts() {
        // Files only: a minimal unborn repository needs no Git mutation.
        let root = std::env::temp_dir().join(format!("mdkg-trust-cache-{}", uuid::Uuid::now_v7()));
        for name in [".git/objects", ".git/refs"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            root.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let discovery = git::WorktreeTrust::default();
        let trusted = git::WorktreeTrust {
            common: Some(root.join(".git")),
            worktree: Some(root.clone()),
        };
        for bad in [
            git::WorktreeTrust {
                common: Some(root.join("wrong")),
                worktree: None,
            },
            git::WorktreeTrust {
                common: None,
                worktree: Some(root.join("wrong")),
            },
        ] {
            git::with_worktree_trust(&bad, || {
                git::with_worktree_trust(&discovery, || {
                    Repo::open(&root).unwrap();
                    assert_eq!(git::common_dir(&root).unwrap(), root.join(".git"));
                });
                // Neither cache may reuse the successful unpinned check.
                assert!(Repo::open(&root).is_err());
                assert!(git::common_dir(&root).is_err());
                git::with_worktree_trust(&trusted, || {
                    Repo::open(&root).unwrap();
                    assert_eq!(git::common_dir(&root).unwrap(), root.join(".git"));
                });
                assert!(Repo::open(&root).is_err());
                assert!(git::common_dir(&root).is_err());
            });
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A fresh git repository with one commit, in its own temp folder.
    fn git_repo(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "mdkg-coord-open-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("sub")).expect("dir");
        std::fs::write(dir.join("sub/x"), "x").expect("file");
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "x",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "git {args:?}");
        }
        std::fs::canonicalize(dir).expect("canonical")
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git {args:?}");
    }

    /// Tier 1 review of 186dee7: a worktree replaced at the same path by
    /// another repository's is checked afresh, so its commands reach the
    /// new repository's store, not the old one's.
    #[test]
    #[cfg(unix)]
    fn a_worktree_replaced_at_the_same_path_is_checked_again() {
        let (a, b) = (git_repo("wa"), git_repo("wb"));
        let w = a.with_file_name(format!(
            "{}-w",
            a.file_name().and_then(|n| n.to_str()).unwrap_or("a")
        ));
        let w_str = w.to_str().expect("utf-8").to_string();
        git_in(&a, &["worktree", "add", "-q", "-b", "wa", &w_str]);
        let w = std::fs::canonicalize(&w).expect("canonical");
        assert_eq!(Repo::open(&w).expect("a's worktree").top, w);
        assert_eq!(git::common_dir(&w).expect("common"), a.join(".git"));
        // Kept: a second open asks git nothing new and agrees.
        assert_eq!(Repo::open(&w).expect("again").top, w);
        // A's worktree goes; B's takes the same path.
        git_in(&a, &["worktree", "remove", "--force", &w_str]);
        git_in(&b, &["worktree", "add", "-q", "-b", "wb", &w_str]);
        let reopened = Repo::open(&w).expect("b's worktree");
        assert_eq!(reopened.top, w);
        assert_eq!(
            git::common_dir(&w).expect("common"),
            b.join(".git"),
            "the old repository's checkout was served"
        );
        // A `.git` file rewritten in place (the same inode) is checked
        // again: pointed at nothing, it's refused, not served.
        let entry = w.join(".git");
        assert_eq!(Repo::open(&w).expect("kept").top, w);
        std::fs::write(&entry, "gitdir: /nonexistent/mdkg-test\n").expect("rewrite");
        assert!(Repo::open(&w).is_err(), "a rewritten .git was served");
        for dir in [&w, &a, &b] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Tier 1 round 2 of f42ce03: a `.git` appearing between the root and
    /// the top it was found under is seen, valid or not.
    #[test]
    #[cfg(unix)]
    fn a_nearer_git_appearing_is_checked_again() {
        let a = git_repo("nest");
        let sub = a.join("sub");
        let first = Repo::open(&sub).expect("sub");
        assert_eq!(
            (first.top.clone(), first.prefix.as_str()),
            (a.clone(), "sub/")
        );
        // A repository made in the subfolder: it's its own top now.
        git_in(&sub, &["init", "-q"]);
        let inner = Repo::open(&sub).expect("inner");
        assert_eq!(
            (inner.top.clone(), inner.prefix.as_str()),
            (sub.clone(), "")
        );
        assert_eq!(git::common_dir(&sub).expect("common"), sub.join(".git"));
        // An unusable `.git` there instead: checked, so refused, not served.
        std::fs::remove_dir_all(sub.join(".git")).expect("rm");
        assert_eq!(Repo::open(&sub).expect("outer again").top, a);
        std::fs::write(sub.join(".git"), "gitdir: /nonexistent/mdkg-test\n").expect("bad");
        assert!(Repo::open(&sub).is_err(), "a cached outer top was served");
        std::fs::remove_dir_all(&a).ok();
    }

    /// Tier 1 round 2 of f42ce03: a `.git` file that names its git folder
    /// through a link follows the link when it's retargeted.
    #[test]
    #[cfg(unix)]
    fn a_retargeted_git_folder_link_is_checked_again() {
        let base = git_repo("link");
        let (gd_a, gd_b) = (base.join("a.git"), base.join("b.git"));
        let (wt_a, wt_b) = (base.join("wa"), base.join("wb"));
        for (wt, gd) in [(&wt_a, &gd_a), (&wt_b, &gd_b)] {
            std::fs::create_dir_all(wt).expect("dir");
            let gd = gd.to_str().expect("utf-8");
            git_in(wt, &["init", "-q", "--separate-git-dir", gd]);
        }
        let link = base.join("current");
        std::os::unix::fs::symlink(&gd_a, &link).expect("link");
        std::fs::write(wt_a.join(".git"), format!("gitdir: {}\n", link.display())).expect("file");
        let gd_a = std::fs::canonicalize(&gd_a).expect("a");
        let gd_b = std::fs::canonicalize(&gd_b).expect("b");
        Repo::open(&wt_a).expect("through a");
        assert_eq!(git::common_dir(&wt_a).expect("common"), gd_a);
        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&gd_b, &link).expect("relink");
        Repo::open(&wt_a).expect("through b");
        assert_eq!(
            git::common_dir(&wt_a).expect("common"),
            gd_b,
            "the old git folder's checkout was served"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// Review of d4444ef5, minor 3: every store a command opens keeps an
    /// acked body for the committed `coordination.messages.ack_grace_days`,
    /// not only the mail commands'; an invalid value falls back to the
    /// default here (the mail commands that ack or send refuse it).
    #[test]
    fn every_store_keeps_acked_bodies_for_the_configured_grace() {
        use mdkg_coord_sqlite::mail::{InboxQuery, Outgoing, Retention, Shown, To};
        let day: u64 = 24 * 60 * 60 * 1000;
        for (setting, days) in [("2", 2), ("\"2\"", 7)] {
            let root = git_repo("grace");
            std::fs::create_dir_all(root.join(".mdkg")).expect("dir");
            std::fs::write(
                root.join(config::CONFIG_PATH),
                format!(r#"{{"coordination": {{"messages": {{"ack_grace_days": {setting}}}}}}}"#),
            )
            .expect("config");
            // The grace is committed authority (e2e 0.7.1 B3).
            for args in [
                &["add", "."][..],
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "grace",
                ],
            ] {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .status()
                    .expect("git")
                    .success();
                assert!(ok, "git {args:?}");
            }
            let mut store = Repo::open(&root).expect("repo").store().expect("store");
            store
                .send(&Outgoing {
                    message_id: "p".into(),
                    context_id: None,
                    kind: "note".into(),
                    sender: "a".into(),
                    via: None,
                    sender_mailboxes: vec!["a".into()],
                    to: To::Mailboxes(vec!["b".into()]),
                    reference_ids: Vec::new(),
                    parts: serde_json::json!([{"kind": "text", "text": "x"}]),
                    retention: Retention::PurgeOnAck,
                    human_hold_days: 7,
                    now_ms: 0,
                })
                .expect("send");
            let inbox = store
                .inbox(&InboxQuery {
                    mailboxes: vec!["b".into()],
                    ..InboxQuery::default()
                })
                .expect("inbox");
            let shown: Vec<Shown> = inbox.messages.iter().map(Shown::of).collect();
            store.mark_shown(&shown, &["b".into()], 0).expect("shown");
            let acked = store
                .ack(&["p".into()], &["b".into()], "b", None, None, 0)
                .expect("ack");
            assert!(acked.purged.is_empty(), "{setting}");
            let body = |store: &mut Store, at: u64| store.message("p", at).expect("p").parts;
            assert!(body(&mut store, days * day - 1).is_some(), "{setting}");
            assert!(body(&mut store, days * day).is_none(), "{setting}");
            drop(store);
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// #53: a root is checked once per process, and only its own canonical
    /// path is served what was found; another root, a subfolder or a path
    /// that can't be canonicalized is checked afresh.
    #[test]
    #[cfg(unix)]
    fn an_open_repo_is_kept_only_for_its_own_root() {
        let (a, b) = (git_repo("a"), git_repo("b"));
        let first = Repo::open(&a).expect("a");
        assert_eq!((first.top.clone(), first.prefix.as_str()), (a.clone(), ""));
        // Another repository: its own top, never a's.
        let other = Repo::open(&b).expect("b");
        assert_eq!(other.top, b);
        // A subfolder of a: its own prefix, not the root's.
        let sub = Repo::open(&a.join("sub")).expect("sub");
        assert_eq!((sub.top.clone(), sub.prefix.as_str()), (a.clone(), "sub/"));
        // A link to a is the same real path: served what a's check found.
        let link = b.with_file_name(format!(
            "{}-link",
            b.file_name().and_then(|n| n.to_str()).unwrap_or("b")
        ));
        std::os::unix::fs::symlink(&a, &link).expect("link");
        let linked = Repo::open(&link).expect("link");
        assert_eq!(
            (linked.top.clone(), linked.root.clone()),
            (a.clone(), link.clone())
        );
        // A root that isn't there can't be canonicalized: checked, and refused.
        assert!(Repo::open(&a.join("missing")).is_err());
        // A failed check isn't kept: a repository made there afterwards opens.
        let late = a.with_file_name(format!(
            "{}-late",
            a.file_name().and_then(|n| n.to_str()).unwrap_or("a")
        ));
        std::fs::create_dir_all(&late).expect("dir");
        assert!(Repo::open(&late).is_err(), "not a repository yet");
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&late)
            .args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        let late = std::fs::canonicalize(&late).expect("canonical");
        assert_eq!(Repo::open(&late).expect("late").top, late);
        for dir in [&a, &b, &link, &late] {
            std::fs::remove_file(dir).ok();
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Round 1 of 5a0ae4f: a role or a turn covers only itself, so a claim
    /// of a scope inside one never renews it.
    #[test]
    fn a_role_or_turn_covers_only_itself() {
        let mut c = Coordinator::new(10);
        let s = Scope::new;
        for named in ["role:x", "run:x"] {
            c.claim_mode("a", &s(named), Mode::Exclusive, 0, None)
                .unwrap();
            assert!(covering_lease(&c, "a", &s(named)).is_some());
            let inside = Scope::new(format!("{named}/a"));
            assert!(covering_lease(&c, "a", &inside).is_none());
        }
    }

    /// Round 2 of 5a0ae4f: every hint's release target resolves back to
    /// exactly its lease, from any graph root.
    #[test]
    fn hints_spell_scopes_relative_to_the_graph_root() {
        let repo = |prefix: &str| Repo {
            root: PathBuf::from("/r").join(prefix.trim_end_matches('/')),
            top: PathBuf::from("/r"),
            prefix: prefix.to_string(),
            folders: std::cell::OnceCell::new(),
            store_work: std::cell::OnceCell::new(),
            committed: std::cell::OnceCell::new(),
            store_ids: Default::default(),
            mode: None,
        };
        let cases = [
            ("", "a/b", "./a/b"),
            ("", "task-1", "./task-1"),
            ("", "lease-7", "./lease-7"),
            ("", "role:x", "role:x"),
            ("sub/", "sub", "."),
            ("sub/", "sub/x", "./x"),
            ("sub/", "SUB/x", "./x"),
            ("sub/", "sub/role:x", "./role:x"),
            ("sub/", "subway/x", "../subway/x"),
            ("sub/", "d", "../d"),
            ("a/b/", "a", ".."),
            ("a/b/", "a/bc", "../bc"),
            ("a/b/", "x/y", "../../x/y"),
            ("a/b/", "run:x", "run:x"),
        ];
        for (prefix, scope, want) in cases {
            assert_eq!(
                repo(prefix).target_of(&Scope::new(scope)),
                want,
                "{prefix} {scope}"
            );
        }
    }

    /// Review round 2 of 1932559: an actor holding overlapping shared
    /// leases of its own (possible in a store 1932559 wrote) never lists
    /// itself as a co-holder.
    #[test]
    fn your_own_overlapping_leases_are_not_co_holders() {
        let mut c = Coordinator::new(10);
        let s = Scope::new;
        c.claim_mode("a", &s("src"), Mode::Shared, 0, None).unwrap();
        let inner = c
            .claim_mode("a", &s("src/lib.rs"), Mode::Shared, 0, None)
            .unwrap();
        let l = c.lease(&inner).unwrap().clone();
        assert!(lease_record_in(&c, &l).shared_with.is_empty());
        assert!(shared_notice(&c, &l).is_none());
        c.claim_mode("b", &s("src/lib.rs"), Mode::Shared, 1, None)
            .unwrap();
        assert_eq!(lease_record_in(&c, &l).shared_with, vec!["b".to_string()]);
        let notice = shared_notice(&c, &l).expect("b shares it");
        assert!(!notice.message.contains("a ("), "{}", notice.message);
    }
}
