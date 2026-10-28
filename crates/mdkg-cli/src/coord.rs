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
use mdkg_coord_sqlite::{Store, git};
use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::resolve::{Resolution, resolve_error, resolve_qid};
use mdkg_graph::{ErrorKind, GraphError, config, index};
use mdkg_index::resolve::{handle_error, resolve_noted};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::authority;
use crate::output::{self, CoordinationRecord, LeaseBrief, PendingBrief, Report, rfc3339};
use mdkg_format::program::BIN;

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

fn ttl_ms() -> u64 {
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

/// Says so when opening the store just upgraded it, with where the copy
/// of it as it was went.
fn upgraded(store: &Store) {
    if let Some(copy) = store.backup() {
        crate::output::notice(
            Diagnostic::warning(
                Code::CoordStoreUpgraded,
                format!(
                    "the coordinator store was upgraded to this build's schema; a copy of it as it \
                     was is at {}",
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
    /// Each store-folder node file's UUID, read once per command;
    /// `None` for a file that names no id.
    store_ids: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<String>>>,
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
                store_ids: Default::default(),
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
                store_ids: Default::default(),
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
        let on_disk = self.top.join(&spelled);
        // Node folders of this graph and of every graph above the scope.
        let mut folders: Vec<Scope> = self.node_folders().1.clone();
        let parts: Vec<&str> = spelled.split('/').collect();
        for i in 0..parts.len() {
            let dir = parts[..i].join("/");
            if self.top.join(&dir).join(config::CONFIG_PATH).is_file() {
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
        let files = git::files_under(&self.top, &spelled).map_err(git_error)?;
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
            folder: self.top.join(self.on_disk(scope.as_str())).is_dir(),
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
            root: self.top.join(dir),
            top: self.top.clone(),
            prefix: if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            },
            folders: std::cell::OnceCell::new(),
            store_work: std::cell::OnceCell::new(),
            store_ids: Default::default(),
        }
    }

    /// `rel` (from the repository's top) as the disk spells it: each
    /// existing segment matched in any letter case, the rest as given.
    fn on_disk(&self, rel: &str) -> String {
        let mut here = self.top.clone();
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
            if self.top.join(&dir).join(config::CONFIG_PATH).is_file() {
                let graph = self.nested(&dir);
                if graph.node_file(scope) {
                    return Ok(Some(graph));
                }
            }
        }
        Ok(None)
    }

    /// The node folders, by folder name (lowercase) and by scope: this
    /// graph's `.mdkg` and each workspace's (`.` is the graph root's own).
    /// A config that can't be read gives this graph's `.mdkg` alone.
    fn node_folders(&self) -> &(Vec<String>, Vec<Scope>) {
        self.folders.get_or_init(|| {
            let mut names = vec![".mdkg".to_string()];
            let mut folders = vec![self.graph_scope()];
            for w in config::load(&self.root)
                .map(|c| c.workspaces)
                .unwrap_or_default()
            {
                if let Some(name) = w.mdkg_dir.trim_matches('/').rsplit('/').next() {
                    names.push(name.to_lowercase());
                }
                let dir: Vec<&str> = w
                    .path
                    .split('/')
                    .chain(w.mdkg_dir.split('/'))
                    .filter(|p| !p.is_empty() && *p != ".")
                    .collect();
                folders.push(self.scope_for(&dir.join("/")));
            }
            (names, folders)
        })
    }

    /// The node folder holding `scope`, the innermost if folders nest.
    fn node_folder_of(&self, scope: &Scope) -> Option<&Scope> {
        self.node_folders()
            .1
            .iter()
            .filter(|f| f.contains(scope) && *f != scope)
            .max_by_key(|f| f.as_str().len())
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
        // SQLite would follow a dangling link and start an empty store
        // where it points: never write past one.
        let path = git::store_path(&self.top).map_err(git_error)?;
        unreachable_store(&path)?;
        let store = Store::open_for_repo(&self.top, ttl_ms()).map_err(store_error)?;
        upgraded(&store);
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
        let main = main_ref(&self.top);
        // A repository with no commit yet: main is unborn and holds
        // nothing, so there's nothing to observe, and work goes on before
        // the first commit. A missing main beside other branches is still
        // a misnamed one.
        if !git::has_ref(&self.top, &main) && git::no_commits(&self.top).map_err(git_error)? {
            return Ok(());
        }
        if !git::has_ref(&self.top, &main) {
            return Err(GraphError::failure(
                Code::IoGitFailed,
                format!("main branch {main} not found"),
            )
            .with_hint("set it with `git config mdkg.mainRef <branch>` (or $MDKG_MAIN_REF)"));
        }
        let prefix = self.graph_scope().to_string();
        match store.observe_main(&self.top, &main, &prefix) {
            Ok(_) => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }

    /// The actor a worktree names by default, its folder's name, with no
    /// look at `$MDKG_ACTOR` (a Codex hook in a shared daemon).
    pub(crate) fn folder_actor(&self) -> String {
        self.top
            .file_name()
            .map_or("agent".into(), |n| n.to_string_lossy().into_owned())
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
        refuse_hidden(target)?;
        let named = ["role:", "run:"].iter().any(|p| {
            target
                .get(..p.len())
                .is_some_and(|t| t.eq_ignore_ascii_case(p))
        });
        if named || target.starts_with("mdkg://") {
            return self.resolve(target);
        }
        let exists = self.root.join(target).exists()
            || self
                .in_other_worktree(&format!("{}{target}", self.prefix))
                .is_some();
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
        let missing = !self.top.join(rel).exists() && self.in_other_worktree(rel).is_none();
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
            folder: target.ends_with('/') || self.top.join(rel).is_dir(),
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
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let raw = Path::new(target);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            root.join(raw)
        };
        let resolved = true_path(&normalize(&joined));
        let rel = resolved.strip_prefix(&self.top).map_err(|_| outside())?;
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
fn unreachable_store(path: &Path) -> Result<(), GraphError> {
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
            "pick other work, or see who holds what with `{bin} status --all`"
        )),
        CoordError::CasFailed => Some("someone changed it first; run the command again".into()),
        CoordError::PastDeadline => Some("the lease ended; claim it again".into()),
        CoordError::NotOwner => Some(format!(
            "only the lease's owner can do that; see who holds what with `{bin} status --all`"
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
    /// Owners of other active leases overlapping a shared one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
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
        changeset: l.changeset.map(|c| c.to_string()),
        mine: None,
        expiring: None,
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Exclusive => "exclusive",
        Mode::Shared => "shared",
    }
}

/// Other active leases overlapping `l`, for a shared lease.
fn co_holders<'a>(state: &'a Coordinator, l: &'a Lease) -> Vec<&'a Lease> {
    if l.mode != Mode::Shared || l.status != Status::Active {
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

/// The `coord.lease_shared` notice for a shared lease with co-holders.
fn shared_notice(state: &Coordinator, l: &Lease) -> Option<Diagnostic> {
    let others = co_holders(state, l);
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
        let in_history = match &e.source {
            Some(s) => git::is_ancestor(&repo.top, &s.commit).unwrap_or(false),
            None => false,
        };
        if in_history {
            continue;
        }
        let path = git::disk_spelling(&repo.top, e.leaf.as_str());
        let have = git::file_digest(&repo.top, &path).map_err(git_error)?;
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

/// The actor's active lease on `scope` or on a folder containing it, so
/// claiming a file inside a folder you hold renews the folder.
pub(crate) fn covering_lease<'a>(
    state: &'a Coordinator,
    actor: &str,
    scope: &Scope,
) -> Option<&'a Lease> {
    own_lease(state, actor, scope).or_else(|| {
        state.leases().find(|l| {
            // A role or a turn covers only itself: never renewed by a claim
            // of a scope inside it (a node squatting a `role:x/...` folder).
            l.owner == actor
                && l.confirmed
                && l.status == Status::Active
                && l.scope.contains(scope)
                && !is_role_or_turn(&l.scope)
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
        let config = config::load(root)?;
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
            repo.resolve_handle_target(target)?
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
        })
    }
}

/// Grants a planned claim in the local store.
fn claim_local(plan: Plan) -> Result<Report<ClaimData>, GraphError> {
    let Plan {
        repo,
        scope,
        label,
        missing,
        node,
        qid,
        node_key,
        mode,
        actor,
        target,
        new,
        exclusive,
        canonical,
    } = plan;
    let target = target.as_str();
    {
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        {
            let state = store.coordinator().map_err(store_error)?;
            // A lease an older build granted on a scope this one refuses
            // isn't renewed by claiming inside it: it lapses.
            if let Some(l) = covering_lease(&state, &actor, &scope)
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
            if let Some(l) = covering_lease(&state, &actor, &scope)
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
                .with_hint(retry_hint(&repo, &l.scope, canonical, target, new, true)));
            }
            // A wider claim over your own narrower lease: refused in every
            // mode, as the coordinator refuses it for exclusive leases.
            if covering_lease(&state, &actor, &scope).is_none()
                && let Some(h) = state
                    .holders(&scope)
                    .find(|l| l.owner == actor && l.scope != scope)
            {
                return Err(GraphError::conflict(
                    Code::CoordLeaseHeld,
                    format!("you hold {} on {}, inside {label}", h.id, h.scope),
                )
                .with_hint(narrower_hint(retry_hint(
                    &repo, &h.scope, canonical, target, new, exclusive,
                ))));
            }
        }
        // A turn isn't re-entrant: claiming one you hold is refused, not a
        // renewal, so two launchers acting as one actor never share it
        // (orchestrator, 2026-10-21). The coordinator refuses the second
        // claim inside its transaction; `renew` extends a turn.
        let turn = scope.as_str().starts_with("run:");
        // Your own turn, as the refusing transaction saw it.
        let mut own_turn: Option<(LeaseId, u64)> = None;
        let mut capped = false;
        let outcome = store.execute_with(|c| {
            if turn {
                own_turn = own_lease(c, &actor, &scope).map(|l| (l.id.clone(), l.deadline));
            }
            // The node held under another path, by anyone, isn't renewed
            // by a lease that happens to cover this one:
            // the claim goes to the coordinator, which refuses it.
            let elsewhere = node_key
                .as_deref()
                .and_then(|k| c.node_holder(k))
                .is_some_and(|h| h.scope != scope && h.deadline > now);
            Ok(
                match covering_lease(c, &actor, &scope).filter(|_| !turn && !elsewhere) {
                    Some(l) => Command::Heartbeat {
                        owner: actor.clone(),
                        lease: l.id.clone(),
                        now,
                    },
                    None if at_lease_cap(c, &actor, now) => {
                        capped = true;
                        // Rolls the transaction back; `capped` says why.
                        return Err(mdkg_coord_sqlite::StoreError::Schema("capped".into()));
                    }
                    None => Command::Claim {
                        owner: actor.clone(),
                        scope: scope.clone(),
                        mode,
                        now,
                        request: None,
                        node: node_key.clone(),
                    },
                },
            )
        });
        if capped {
            return Err(lease_cap_error());
        }
        let outcome = outcome.map_err(store_error)?;
        let state = store.coordinator().map_err(store_error)?;
        let (lease, renewed) = match &outcome {
            Outcome::Lease { lease } => (state.lease(lease).cloned(), false),
            Outcome::Deadline { .. } => (covering_lease(&state, &actor, &scope).cloned(), true),
            Outcome::Refused {
                error: CoordError::Held(_),
            } => {
                if let Some((id, deadline)) = &own_turn {
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
                    .filter(|h| h.scope != scope)
                    .or_else(|| {
                        state
                            .blockers(&scope, mode)
                            .find(|l| l.owner != actor || l.scope != scope)
                    });
                return Err(match holder {
                    Some(h) if h.owner == actor && !h.scope.overlaps(&scope) => {
                        GraphError::conflict(
                            Code::CoordLeaseHeld,
                            format!("you hold {label} under another path ({})", h.id),
                        )
                        .at(h.scope.to_string(), None)
                        .with_hint(format!(
                            "release that lease (`{BIN} release {}`), then claim it again",
                            h.id
                        ))
                    }
                    Some(h) if h.owner == actor => GraphError::conflict(
                        Code::CoordLeaseHeld,
                        format!("you hold {} on {}, inside {label}", h.id, h.scope),
                    )
                    .with_hint(narrower_hint(retry_hint(
                        &repo, &h.scope, canonical, target, new, exclusive,
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
                        if let Some(q) = &qid {
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
        let lease = lease.ok_or_else(|| {
            GraphError::failure(Code::CoordStoreFailed, "the new lease is missing")
        })?;
        // A host leaves the notes about the worktree to the worker.
        let sync = if canonical {
            Vec::new()
        } else {
            missing_versions(&repo, &state, &scope)?
        };
        let mut diagnostics = if canonical {
            Vec::new()
        } else {
            claim_notes(&state, &scope, &label, target, missing && !new, node)
        };
        diagnostics.extend(shared_notice(&state, &lease));
        let mut record = lease_record_in(&state, &lease);
        name_store_scopes(&repo, [(record.scope.clone().as_str(), &mut record.node)]);
        let mut report = Report::ok(ClaimData {
            lease: record,
            renewed,
            sync: sync.clone(),
        });
        if !sync.is_empty() {
            diagnostics.push(sync_note(sync.len(), &label, target));
        }
        if !diagnostics.is_empty() {
            report = report.with(diagnostics);
        }
        Ok(report)
    }
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
        return authority.delegate(root, "holds", &[target.scope.to_string()], &options);
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
    let lease = covering_lease(&state, &actor, &target.scope)
        .filter(|l| l.deadline > now && !elsewhere && repo.lease_still_granted(&l.scope));
    let mut record = lease.map(|l| lease_record_in(&state, l));
    if let Some(r) = record.as_mut() {
        let scope = r.scope.clone();
        name_store_scopes(&repo, [(scope.as_str(), &mut r.node)]);
    }
    Ok(Report::ok(HoldsData {
        holds: record.is_some(),
        lease: record,
    }))
}

pub fn holds(
    root: &Path,
    actor: Option<String>,
    scope: &str,
    node: Option<&str>,
    json: bool,
) -> u8 {
    let report = holds_report(root, actor, scope, node)
        .unwrap_or_else(Report::failed)
        .with(authority::committed_key_warning(root));
    output::emit("holds", json, report, |d| match &d.lease {
        Some(l) => println!(
            "{}",
            output::shown(&format!(
                "held: {}{} ({})",
                l.scope,
                NodeName::suffix(l.node.as_ref()),
                l.id
            ))
        ),
        None => println!("not held"),
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
/// (relative to the repository's top) as a lease authority's host is given.
#[derive(Debug, Clone, Copy)]
pub enum ReleaseTarget<'a> {
    Named(&'a str),
    Scope(&'a str),
}

pub(crate) fn release_local(
    root: &Path,
    actor: Option<String>,
    which: ReleaseTarget<'_>,
) -> Result<Report<ReleaseData>, GraphError> {
    {
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        let state = store.coordinator().map_err(store_error)?;
        let target = match which {
            ReleaseTarget::Named(t) | ReleaseTarget::Scope(t) => t,
        };
        let lease = if let ReleaseTarget::Scope(scope) = which {
            let scope = repo.canonical_target(scope, None)?.scope;
            if own_lease(&state, &actor, &scope).is_none()
                && let Some(l) = ended_lease(&state, &actor, &scope)
            {
                return Ok(Report::ok(ReleaseData {
                    lease: lease_record(l),
                }));
            }
            own_lease(&state, &actor, &scope).cloned()
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
                return Ok(Report::ok(ReleaseData {
                    lease: lease_record(l),
                }));
            }
            named
        } else {
            // A node by any handle claim takes (RC1 audit U1); a path you
            // claimed with `--new` may still not exist, and a top-level one
            // then reads as an unknown node id.
            let scope = match release_handle(&repo, target) {
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
                own_lease(&state, &actor, &scope),
                covering_lease(&state, &actor, &scope),
            ) {
                return Err(GraphError::not_found(
                    Code::CoordNoLease,
                    format!("{target} is covered by your lease {} on {}", l.id, l.scope),
                )
                .with_hint(format!(
                    "release the whole lease with `{BIN} release {}`",
                    safe_target(&repo, &l.scope)
                )));
            }
            // Your own lease that has already ended: nothing left to do.
            if own_lease(&state, &actor, &scope).is_none()
                && let Some(l) = ended_lease(&state, &actor, &scope)
            {
                return Ok(Report::ok(ReleaseData {
                    lease: lease_record(l),
                }));
            }
            own_lease(&state, &actor, &scope).cloned()
        };
        let Some(lease) = lease else {
            return Err(GraphError::not_found(
                Code::CoordNoLease,
                "you hold no active lease on that",
            )
            .with_hint(format!("see your leases with `{} status`", crate::BIN)));
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
        }))
    }
}

pub fn release(root: &Path, actor: Option<String>, which: ReleaseTarget<'_>, json: bool) -> u8 {
    let report = (|| {
        let repo = Repo::open(root)?;
        let target = match which {
            ReleaseTarget::Named(t) | ReleaseTarget::Scope(t) => t,
        };
        // Its refusals quote the target: never include hidden characters.
        refuse_hidden(target)?;
        // A role or a turn is named alone (`./role:x` is a path): always
        // local, whatever the authority's state.
        if !target.contains('/') && is_named_scope(&Scope::new(target)) {
            return release_local(root, actor, which);
        }
        let authority = authority::find(&repo)?;
        let ReleaseTarget::Named(target) = which else {
            // A canonical scope (as a host is asked): through the authority
            // when there is one.
            let scope = repo.canonical_target(target, None)?.scope;
            return match authority {
                Some(a) if !is_named_scope(&scope) => a.delegate(
                    root,
                    "release",
                    &[],
                    &[
                        ("--scope", Some(scope.to_string())),
                        ("--as", Some(repo.actor(actor))),
                    ],
                ),
                _ => release_local(root, actor, which),
            };
        };
        // A lease id, or a node (by any handle claim takes: RC1 audit U1)
        // or a path. A name shaped like a lease id, an authority's
        // `lease-remote-1` included, is read exactly, so no title can
        // shadow it; a handle naming several nodes refuses (exit 7).
        let thing = match release_handle(&repo, target) {
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
        let looks_like_id =
            is_lease_id(target) || (target.starts_with("lease-") && thing.is_none());
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
        authority.delegate(
            root,
            "release",
            &[],
            &[("--scope", Some(scope.to_string())), ("--as", Some(actor))],
        )
    })()
    .unwrap_or_else(Report::failed)
    .with(authority::committed_key_warning(root));
    output::emit("release", json, report, |d| {
        println!(
            "{}",
            output::shown(&format!("released {} ({})", d.lease.scope, d.lease.id))
        );
    })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseManyData {
    /// Leases this run released, each in its own transaction.
    pub released: Vec<LeaseRecord>,
    /// Leases left as they are, each with the reason (`--all` keeps none;
    /// the list is there for narrower selections).
    pub skipped: Vec<KeptLease>,
    /// Leases whose release failed: they stay held, and running the same
    /// command again retries them.
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
    /// The release itself failed.
    Failed,
}

/// The shapes `release` prints: one lease, or what `--all` did with each
/// of the actor's leases.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReleaseOutput {
    One(ReleaseData),
    Many(ReleaseManyData),
}

/// `release --all` on this store's leases: each release in its own
/// transaction, as `release <id>` is, so a crash or a failed write leaves
/// the rest released or not, and running it again finishes the job.
fn release_all_local(repo: &Repo, actor: &str) -> Result<Report<ReleaseManyData>, GraphError> {
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let mine: Vec<Lease> = state
        .leases()
        .filter(|l| l.owner == actor && l.status == Status::Active)
        .cloned()
        .collect();
    let mut released = Vec::new();
    let mut failed = Vec::new();
    let mut first_error: Option<GraphError> = None;
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
            Err(e) => {
                failed.push(KeptLease {
                    lease: lease_record(lease),
                    why: KeptWhy::Failed,
                    reason: e.diagnostic.message.clone(),
                });
                first_error.get_or_insert(e);
            }
        }
    }
    // Released leases as the store now holds them; one it can no longer
    // read back is reported as it was.
    let after = store.coordinator().ok();
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
        skipped: Vec::new(),
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

/// `release --all`. Beside a lease authority, only this store's leases
/// (roles, turns, profile C nodes) are released here: the lease-authority
/// contract has no call that lists yours there, so those are released by
/// path.
pub fn release_all(root: &Path, actor: Option<String>, json: bool) -> u8 {
    let report = (|| {
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        let report = release_all_local(&repo, &actor)?;
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
            println!("no active leases to release");
        }
        for l in &d.released {
            println!(
                "{}",
                output::shown(&format!("released {} ({})", l.scope, l.id))
            );
        }
        for k in &d.failed {
            println!(
                "{}",
                output::shown(&format!("not released {} ({})", k.lease.scope, k.lease.id))
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
        let ids: Vec<LeaseId> = store
            .coordinator()
            .map_err(store_error)?
            .leases()
            .filter(|l| l.owner == actor && l.confirmed && l.status == Status::Active)
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
            .filter_map(|l| shared_notice(&state, l))
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
        Ok(Report::ok(RenewData {
            renewed,
            expired: expired.into_iter().map(lease_record).collect(),
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
        if d.renewed.is_empty() {
            println!("no active leases to renew");
        }
        for l in &d.renewed {
            println!(
                "{}",
                output::shown(&format!("renewed {} until {} ({})", l.scope, l.until, l.id))
            );
        }
    })
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusData {
    /// Who this worktree acts as.
    pub actor: String,
    /// The branch work settles on.
    pub main: String,
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

pub fn status(root: &Path, actor: Option<String>, all: bool, json: bool) -> u8 {
    let own_only = named_actor(actor.as_ref()) && !all;
    let now = now_ms();
    let report = (|| -> Result<Report<StatusData>, GraphError> {
        let repo = Repo::open(root)?;
        let actor = repo.actor(actor);
        let mut store = repo.store()?;
        repo.observe(&mut store, now)?;
        let state = store.coordinator().map_err(store_error)?;
        // Open changesets whose commit still carries abandoned work.
        let only = carried_by_open(&repo, &state)?;
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
        let warnings: Vec<Diagnostic> = carried
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
        let leases = state
            .leases()
            .filter(|l| l.confirmed && l.status == Status::Active)
            .map(|l| LeaseRecord {
                mine: Some(l.owner == actor),
                expiring: Some(expiring(&state, l, now)),
                ..lease_record_in(&state, l)
            })
            .collect();
        let expired = recently_expired(&state, (!all).then_some(actor.as_str()), now)
            .into_iter()
            .map(|l| LeaseRecord {
                mine: Some(l.owner == actor),
                ..lease_record(l)
            })
            .collect();
        let mail = crate::msg::unacked(&store, &actor, now)
            .into_iter()
            .map(|(mailbox, unacked)| MailCount { mailbox, unacked })
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
        Ok(Report::ok(StatusData {
            actor,
            main: main_ref(&repo.top),
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
        say(format!("acting as {}; settling on {}", d.actor, d.main));
        let shown: Vec<&LeaseRecord> = d
            .leases
            .iter()
            .filter(|l| !own_only || l.mine == Some(true))
            .collect();
        if shown.is_empty() {
            say(format!(
                "no active leases{}",
                if own_only { " of yours" } else { "" }
            ));
        }
        for l in &shown {
            let shared = if l.shared_with.is_empty() {
                String::new()
            } else {
                format!(" shared with {}", l.shared_with.join(", "))
            };
            say(format!(
                "lease {} {}{} by {}{shared} until {}",
                l.id,
                l.scope,
                NodeName::suffix(l.node.as_ref()),
                l.owner,
                until_text(l, now)
            ));
        }
        let others = d.leases.len() - shown.len();
        if others > 0 {
            say(format!(
                "{others} more held by others (`{BIN} status --all` lists them)"
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
    let main = main_ref(&repo.top);
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
        Ok(Some(Self {
            repo,
            state,
            now: now_ms(),
        }))
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
            store_ids: Default::default(),
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
