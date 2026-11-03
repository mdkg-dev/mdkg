//! The graph ref: mdkg's only source of authority (edd-01a18867).
//!
//! - [`resolve`] reads the clone-local keys from the common git config file
//!   only (never `config.worktree`, the global or system files, or the
//!   environment, except `$MDKG_MAIN_REF` for [`GraphRef::main_ref_base`]).
//! - [`authority`] and [`authority_at`] read committed config and agent
//!   definitions as git objects at one pinned commit: no checkout, cache
//!   or index.
//! - [`graph_paths`] and [`design_paths`] name the graph-owned folders.

use mdkg_format::program::BIN;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::{GraphError, committed, refwrite};

/// An error with a stable `graph.*` code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new("graph.io", e.to_string())
    }
}
impl From<refwrite::Error> for Error {
    fn from(e: refwrite::Error) -> Self {
        Self::new(e.code(), e.to_string())
    }
}
impl From<GraphError> for Error {
    fn from(e: GraphError) -> Self {
        Self::new("graph.authority_invalid", e.to_string())
    }
}

/// `graph.remote`'s settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The git remote's name (`mdkg.graph.remote`).
    pub name: String,
    /// The full ref on the remote (`mdkg.graph.remoteRef`, default the
    /// graph ref's name).
    pub remote_ref: String,
    /// The destination install pinned (`<common>/mdkg/graph/remote.json`),
    /// `None` until pinned.
    pub pinned: Option<Destination>,
}

/// A remote's normalized effective destinations, as pinned.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Destination {
    pub fetch: Vec<String>,
    pub push: Vec<String>,
}

/// The clone's graph-ref settings. `None` from [`resolve`] is checkout mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRef {
    /// The common git folder.
    pub common: PathBuf,
    /// The graph ref's full name.
    pub name: String,
    pub remote: Option<Remote>,
    /// `mdkg.mainRef` as configured, normalized to a full ref: the config
    /// value only. Anchoring, integration and the protected list read this.
    pub main_ref: Option<String>,
    /// `$MDKG_MAIN_REF` when set, else [`GraphRef::main_ref`]: worktree
    /// bases and reports only, never authority.
    pub main_ref_base: Option<String>,
    /// `mdkg.graph.sync.pushDelay`, default 120 s.
    pub push_delay: Duration,
    /// `mdkg.graph.sync.pullInterval`, default 900 s; zero means only at
    /// `agents up` start and on `mdkg graph sync`.
    pub pull_interval: Duration,
    /// `mdkg.graph.publishDesign`; unset is off (install writes it per
    /// setup).
    pub publish_design: bool,
    /// `mdkg.follow.fetch`.
    pub follow_fetch: bool,
    /// The graph ids the clone's identity records name: each exact key
    /// `mdkg.graph.<graph_id>.ref` whose value is this graph ref.
    pub graph_ids: Vec<String>,
    /// Each recorded id's graph root (`mdkg.graph.<graph_id>.root`,
    /// repository-relative; absent is the repository root). An id is
    /// accepted only for the root it was recorded against.
    pub graph_roots: BTreeMap<String, String>,
    /// Ids recorded only the 0.7.0 way (`.worktree`, no `.ref` or
    /// `.root`): readable at the nested root holding them, and kept by
    /// every writer wherever the graph held them (Terra #8551).
    pub legacy_ids: Vec<String>,
    /// Each recorded id's graph folder name, when it isn't `.mdkg`
    /// (`mdkg.graph.<graph_id>.dir`, orchestrator #8321).
    pub graph_dirs: BTreeMap<String, String>,
    /// Each explicitly bound root's effective folder, as `dir_for` judges
    /// it from every record there, partial `.root` records included (Terra
    /// #10216); a conflict is kept as its message, so any change shows.
    root_folders: BTreeMap<String, String>,
}

/// A graph root as records hold it: no leading, trailing or doubled `/`,
/// no `.`; the repository root is `""`.
pub fn normalize_root(root: &str) -> String {
    root.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// A graph root as given by a caller: normalized, and refused when it
/// leaves the repository (`..`) or holds a backslash, which 0.7.1 doesn't
/// support in a graph-root folder name (orchestrator #7846).
pub fn checked_root(graph_root: &str) -> Result<String, Error> {
    // Repository-relative only (Terra #7885): an absolute root never
    // becomes a relative one.
    if graph_root.starts_with('/') || graph_root.contains('\0') {
        return Err(Error::new(
            "graph.config_invalid",
            format!("graph root {graph_root:?} must be relative to the repository"),
        ));
    }
    let root = normalize_root(graph_root);
    if root.split('/').any(|c| c == "..") {
        return Err(Error::new(
            "graph.config_invalid",
            format!("graph root {graph_root:?} leaves the repository"),
        ));
    }
    if root.contains('\\') {
        return Err(Error::new(
            "graph.config_invalid",
            format!(
                "graph root {graph_root:?} has a backslash in a folder name, which mdkg doesn't support; rename the folder"
            ),
        ));
    }
    Ok(root)
}

/// The root a graph id's record binds it to.
fn record_root(keys: &BTreeMap<String, String>, id: &str) -> String {
    keys.get(&format!("mdkg.graph.{id}.root"))
        .map(|r| normalize_root(r))
        .unwrap_or_default()
}

pub const DEFAULT_PUSH_DELAY: Duration = Duration::from_secs(120);
pub const DEFAULT_PULL_INTERVAL: Duration = Duration::from_secs(900);

impl GraphRef {
    /// A writer of this graph ref. On an existing graph it checks the
    /// tip's `graph_id` against the clone's record ([`GraphRef::check_identity`])
    /// and binds the writer to it, so every prepare and publish refuses
    /// another graph and rechecks the clone's config. With no graph ref yet
    /// it refuses: use [`GraphRef::creator`].
    pub fn writer(
        &self,
        repo: &Path,
        lock_mode: refwrite::LockMode,
    ) -> Result<refwrite::RefWriter, Error> {
        self.writer_in(repo, lock_mode, "")
    }

    /// [`GraphRef::writer`] bound to the graph at `graph_root` (a nested
    /// workspace or subgraph registered at that root), so a clone whose
    /// only registered graph is nested can write it.
    pub fn writer_in(
        &self,
        repo: &Path,
        lock_mode: refwrite::LockMode,
        graph_root: &str,
    ) -> Result<refwrite::RefWriter, Error> {
        checked_root(graph_root)?;
        check_all_folders(&common_config(repo, &self.common)?)?;
        let writer =
            refwrite::RefWriter::open(repo, &self.name, self.main_ref.as_deref(), lock_mode)?
                .trusted()
                .with_dirs(self.root_dirs())
                .with_graph_root(graph_root);
        let Some(tip) = writer.tip()? else {
            return Err(Error::new(
                "graph.ref_missing",
                format!(
                    "{} doesn't exist; create it with GraphRef::creator",
                    self.name
                ),
            ));
        };
        // Only the graph id: a bad definition must not block the write
        // that fixes it.
        let graph_id = writer.graph_id_at(&tip)?.unwrap_or_default();
        self.check_identity_at(&graph_id, graph_root)?;
        Ok(writer
            .with_graph_id(&graph_id)
            .with_registered(self.others(&graph_id))
            .with_legacy(self.legacy_ids.clone()))
    }

    /// Each explicitly bound root's effective graph folder name, from every
    /// record at that root (Terra #10216).
    pub(crate) fn root_dirs(&self) -> BTreeMap<String, String> {
        self.root_folders.clone()
    }

    /// The graph folder name at `graph_root`, as every authority read
    /// settles it ([`dir_for`]): `.mdkg` unless a record there names
    /// another; records that disagree are an error.
    pub fn folder(&self, graph_root: &str) -> Result<String, Error> {
        match self.root_folders.get(&normalize_root(graph_root)) {
            None => Ok(committed::DEFAULT_DIR.to_owned()),
            Some(conflict) if conflict.starts_with("conflict: ") => Err(Error::new(
                "graph.identity_mismatch",
                conflict.trim_start_matches("conflict: ").to_owned(),
            )),
            Some(dir) => Ok(dir.clone()),
        }
    }

    /// The registered graphs other than `graph_id`, as `(id, root)`.
    pub(crate) fn others(&self, graph_id: &str) -> Vec<(String, String)> {
        self.graph_roots
            .iter()
            .filter(|(id, _)| id.as_str() != graph_id)
            .map(|(id, root)| (id.clone(), root.clone()))
            .collect()
    }

    /// A writer that creates the graph ref (init and install) for a new
    /// graph `graph_id` at `graph_root`, recoverable across a crash
    /// (orchestrator, on Terra #6791):
    /// - `graph_id` must be a UUID that no record names yet;
    /// - a durable creation intent (`<common>/mdkg/graph/creating.json`)
    ///   is written before the create, and the writer refuses a created
    ///   graph that doesn't carry `graph_id`;
    /// - after the create publishes, [`GraphRef::complete_creation`] writes
    ///   the identity records and drops the intent;
    /// - a rerun after a crash between the two finds the ref carrying the
    ///   intended id, adopts it (writes the records, drops the intent) and
    ///   returns a writer bound to it, so the create request's rerun
    ///   returns its original receipt.
    ///
    /// Any other existing ref is refused (`graph.ref_exists`), and so is a
    /// second creation while one is pending; [`GraphRef::abandon_creation`]
    /// retires an abandoned one. The intent is read, written and completed
    /// under the exclusive graph lock (orchestrator #7290), which this call
    /// takes and releases itself: call it without holding the lock.
    pub fn creator(
        &self,
        repo: &Path,
        lock_mode: refwrite::LockMode,
        graph_id: &str,
        graph_root: &str,
    ) -> Result<refwrite::RefWriter, Error> {
        self.creator_dir(
            repo,
            lock_mode,
            graph_id,
            graph_root,
            committed::DEFAULT_DIR,
        )
    }

    /// [`GraphRef::creator`] for a graph whose folder under its root is
    /// `dir` (a 0.7.0 workspace's `mdkg_dir`); completion records it as
    /// `mdkg.graph.<id>.dir` when it isn't `.mdkg` (orchestrator #8321).
    pub fn creator_dir(
        &self,
        repo: &Path,
        lock_mode: refwrite::LockMode,
        graph_id: &str,
        graph_root: &str,
        dir: &str,
    ) -> Result<refwrite::RefWriter, Error> {
        let root = checked_root(graph_root)?;
        // The committed reader's rules, so what's created can be read
        // (Terra #9366).
        let dir = committed::check_dir(dir).map_err(|e| {
            Error::new(
                "graph.config_invalid",
                format!("{dir:?} isn't a graph folder name: {e}"),
            )
        })?;
        let mut dirs = self.root_dirs();
        if dir != committed::DEFAULT_DIR {
            dirs.insert(root.clone(), dir.clone());
        }
        if mdkg_format::Uuid7::parse(graph_id).is_err() || graph_id.to_ascii_lowercase() != graph_id
        {
            return Err(Error::new(
                "graph.config_invalid",
                format!("{graph_id:?} isn't a graph id"),
            ));
        }
        // One clone: the writer's repository shares this graph ref's
        // common folder (Terra #8377).
        if mdkg_git::common_dir(repo)?.canonicalize()? != self.common.canonicalize()? {
            return Err(Error::new(
                "graph.identity_mismatch",
                "the creator's repository isn't the clone this graph ref belongs to",
            ));
        }
        check_all_folders(&common_config(repo, &self.common)?)?;
        let lock = refwrite::GraphLock::exclusive(&self.common, lock_mode)?;
        let intent = read_creation(&self.common)?;
        // The folder is part of a creation's identity (Terra #9366).
        let ours = intent
            .as_ref()
            .is_some_and(|(i, r, n)| i == graph_id && *r == root && *n == self.name)
            && read_creation_dir(&self.common)? == dir;
        // A new id must be absent from every record, whatever ref or
        // checkout it names (orchestrator #7290).
        let prefix = format!("mdkg.graph.{graph_id}.");
        if !ours
            && common_config(repo, &self.common)?
                .keys()
                .any(|k| k.starts_with(&prefix))
        {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!("graph {graph_id} is already recorded in this clone"),
            ));
        }
        let writer =
            refwrite::RefWriter::open(repo, &self.name, self.main_ref.as_deref(), lock_mode)?
                .trusted()
                .with_dirs(dirs)
                .with_graph_root(&root);
        if let Some(tip) = writer.tip()? {
            // Adoption: the create published before a crash.
            if ours && writer.graph_id_at(&tip)?.as_deref() == Some(graph_id) {
                self.complete_locked(repo, &lock)?;
                drop(lock);
                let graph = resolve(repo)?.ok_or_else(|| {
                    Error::new("graph.ref_missing", "the graph ref is no longer configured")
                })?;
                return graph.writer_in(repo, lock_mode, &root);
            }
            return Err(Error::new(
                "graph.ref_exists",
                format!("{} already exists; use GraphRef::writer", self.name),
            ));
        }
        // A root another recorded graph is bound to is refused (Terra
        // #8754).
        // Any `.root` record counts, a partial one (no `.ref`) included
        // (Terra #9366).
        let keys = common_config(repo, &self.common)?;
        let bound_elsewhere = keys.iter().find_map(|(k, v)| {
            let id = k.strip_prefix("mdkg.graph.")?.strip_suffix(".root")?;
            (normalize_root(v) == root && id != graph_id).then(|| id.to_owned())
        });
        if let Some(other) = bound_elsewhere.or_else(|| {
            self.graph_roots
                .iter()
                .find(|(id, r)| **r == root && id.as_str() != graph_id)
                .map(|(id, _)| id.clone())
        }) {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!("graph {other} is already recorded at {root:?}"),
            ));
        }
        // One creation at a time (Opus #7225): the intent is created only
        // where none exists, so a second creator is refused instead of
        // replacing it. A rerun of this very creation reuses its own.
        if let Some((i, r, n)) = &intent
            && !ours
        {
            return Err(Error::new(
                "graph.ref_exists",
                format!(
                    "another creation is pending (graph {i} at {r:?} on {n}); rerun it, or remove {}",
                    refwrite::graph_dir(&self.common)?
                        .join("creating.json")
                        .display()
                ),
            ));
        }
        if !ours {
            let value = serde_json::json!({"graph_id": graph_id, "root": root, "ref": self.name, "dir": dir});
            create_intent(
                &refwrite::graph_dir(&self.common)?,
                &serde_json::to_vec(&value).map_err(|e| Error::new("graph.io", e.to_string()))?,
            )?;
        }
        Ok(writer
            .with_creating(graph_id)
            .with_registered(self.others(graph_id))
            .with_legacy(self.legacy_ids.clone()))
    }

    /// Finishes a creation: once the graph ref carries the intended id,
    /// writes its identity records (`mdkg.graph.<id>.ref` and, for a nested
    /// root, `.root`) to the common config and drops the creation intent.
    /// The pending creation's `(graph id, root)`, if one is recorded: an
    /// `init` that failed before it published resumes with these rather
    /// than minting a new id (Opus #8345).
    /// Returns `(graph id, root, folder)`; the folder is part of the
    /// creation (Terra #9366).
    pub fn pending_creation(&self) -> Result<Option<(String, String, String)>, Error> {
        let dir = read_creation_dir(&self.common)?;
        Ok(read_creation(&self.common)?
            .filter(|(_, _, name)| *name == self.name)
            .map(|(id, root, _)| (id, root, dir)))
    }

    /// Refuses a repository that isn't this graph ref's clone (Terra
    /// #9366): recovery never inspects one clone and changes another.
    fn same_clone(&self, repo: &Path) -> Result<(), Error> {
        if mdkg_git::common_dir(repo)?.canonicalize()? != self.common.canonicalize()? {
            return Err(Error::new(
                "graph.identity_mismatch",
                "the repository isn't the clone this graph ref belongs to",
            ));
        }
        Ok(())
    }

    /// Takes the exclusive graph lock itself: call it without holding it.
    pub fn complete_creation(&self, repo: &Path) -> Result<(), Error> {
        self.same_clone(repo)?;
        let lock = refwrite::GraphLock::exclusive(&self.common, refwrite::LockMode::Local)?;
        self.complete_locked(repo, &lock)
    }

    /// Retires a pending creation that never published (an abandoned
    /// `init`), under the exclusive graph lock. One whose graph ref already
    /// carries its id is completed instead, never dropped.
    pub fn abandon_creation(&self, repo: &Path) -> Result<(), Error> {
        self.same_clone(repo)?;
        let lock = refwrite::GraphLock::exclusive(&self.common, refwrite::LockMode::Local)?;
        let Some((graph_id, root, name)) = read_creation(&self.common)? else {
            return Ok(());
        };
        // The pending creation's own graph folder (orchestrator #8321).
        let dirs = std::iter::once((root.clone(), read_creation_dir(&self.common)?)).collect();
        let writer = refwrite::RefWriter::open(repo, &name, None, refwrite::LockMode::Local)?
            .with_dirs(dirs)
            .with_graph_root(&root);
        if let Some(tip) = writer.tip()?
            && writer.graph_id_at(&tip)?.as_deref() == Some(graph_id.as_str())
        {
            return self.complete_locked(repo, &lock);
        }
        let dir = refwrite::graph_dir(&self.common)?;
        std::fs::remove_file(dir.join("creating.json"))?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }

    fn complete_locked(&self, repo: &Path, lock: &refwrite::GraphLock) -> Result<(), Error> {
        lock.require_exclusive(&self.common, "completing a graph creation")?;
        let Some((graph_id, root, name)) = read_creation(&self.common)? else {
            // No intent: success only when already complete. A graph ref
            // whose root graph no record names fails closed (Opus #8328).
            if let Some(current) = resolve(repo)? {
                let writer = refwrite::RefWriter::open(
                    repo,
                    &current.name,
                    None,
                    refwrite::LockMode::Local,
                )?;
                if let Some(tip) = writer.tip()?
                    && let Some(id) = writer.graph_id_at(&tip)?
                    && !current.graph_ids.contains(&id)
                {
                    return Err(Error::new(
                        "graph.identity_unrecorded",
                        format!(
                            "{} holds graph {id}, but no creation intent or record names it; run `{BIN} install`",
                            current.name
                        ),
                    ));
                }
            }
            return Ok(());
        };
        // The configuration as it is now, under the lock (orchestrator
        // #7290): a changed graph ref keeps the intent, never acknowledged.
        let current = resolve(repo)?.ok_or_else(|| {
            Error::new(
                "graph.ref_missing",
                "the clone no longer has a graph ref configured; the creation intent is kept",
            )
        })?;
        if name != self.name || current.name != name {
            return Err(Error::new(
                "graph.identity_mismatch",
                "the creation intent names another graph ref than the one configured; the intent is kept",
            ));
        }
        // The pending creation's own graph folder (orchestrator #8321).
        let dirs = std::iter::once((root.clone(), read_creation_dir(&self.common)?)).collect();
        let writer = refwrite::RefWriter::open(repo, &self.name, None, refwrite::LockMode::Local)?
            .with_dirs(dirs)
            .with_graph_root(&root);
        let tip = writer.tip()?.ok_or_else(|| {
            Error::new("graph.ref_missing", format!("{} wasn't created", self.name))
        })?;
        if writer.graph_id_at(&tip)?.as_deref() != Some(graph_id.as_str()) {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!("{} doesn't carry graph {graph_id}", self.name),
            ));
        }
        let file = self.common.join("config");
        let set = |key: String, value: &str| -> Result<(), Error> {
            let out = mdkg_git::command(repo)
                .arg("config")
                .arg("--file")
                .arg(&file)
                .args([key.as_str(), value])
                .run(None)?;
            if !out.status.success() {
                return Err(Error::new(
                    "graph.io",
                    format!("couldn't record {key} (exit {:?})", out.status.code()),
                ));
            }
            Ok(())
        };
        // `.dir` and `.root` first: the id is only ever registered with its
        // folder and root (orchestrator #8321).
        let dir = read_creation_dir(&self.common)?;
        if dir != committed::DEFAULT_DIR {
            set(format!("mdkg.graph.{graph_id}.dir"), &dir)?;
        }
        // `.root` first: the id is only ever registered with its root
        // (Opus #7225); a crash between the two leaves an unregistered id.
        if !root.is_empty() {
            set(format!("mdkg.graph.{graph_id}.root"), &root)?;
        }
        set(format!("mdkg.graph.{graph_id}.ref"), &self.name)?;
        // The records are durable before the intent goes.
        std::fs::File::open(&file)?.sync_all()?;
        std::fs::File::open(&self.common)?.sync_all()?;
        let dir = refwrite::graph_dir(&self.common)?;
        match std::fs::remove_file(dir.join("creating.json")) {
            Ok(()) => std::fs::File::open(&dir)?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Refuses a committed `graph_id` the clone's identity records don't
    /// name (`graph.identity_mismatch`), and any graph when the clone has
    /// no record (`graph.identity_unrecorded`): install and adoption write
    /// the record first.
    pub fn check_identity(&self, graph_id: &str) -> Result<(), Error> {
        self.check_identity_at(graph_id, "")
    }

    /// [`GraphRef::check_identity`] for the graph at `graph_root`: the id
    /// must be recorded against that root (orchestrator, on Terra #6651),
    /// so another recorded graph's config moved there is refused.
    pub fn check_identity_at(&self, graph_id: &str, graph_root: &str) -> Result<(), Error> {
        if self.graph_ids.is_empty() {
            return Err(Error::new(
                "graph.identity_unrecorded",
                format!(
                    "this clone records no graph identity for {}; run `{BIN} install`",
                    self.name
                ),
            ));
        }
        let root = normalize_root(graph_root);
        // One graph per root (Terra #6917): two ids bound to one root,
        // such as two records with no `.root`, let either replace the other.
        let at_root: Vec<&String> = self
            .graph_roots
            .iter()
            .filter(|(_, r)| **r == root)
            .map(|(id, _)| id)
            .collect();
        if at_root.len() > 1 {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!(
                    "this clone records several graphs at {root:?} ({}); a record without `mdkg.graph.<id>.root` binds the repository root, so run `{BIN} install` or `{BIN} doctor --repair` to record each graph's root",
                    at_root
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if self.graph_roots.get(graph_id).is_some_and(|r| *r == root) {
            return Ok(());
        }
        Err(Error::new(
            "graph.identity_mismatch",
            format!(
                "{} holds graph {graph_id} at {:?}, but this clone doesn't record that graph at that root{}",
                self.name,
                root,
                if !root.is_empty() && self.graph_roots.get(graph_id).is_some_and(String::is_empty)
                {
                    format!(
                        "; a record without `mdkg.graph.<id>.root` binds the repository root, so run `{BIN} install` or `{BIN} doctor --repair` to record this graph's root"
                    )
                } else {
                    String::new()
                }
            ),
        ))
    }

    /// [`GraphRef::check_identity_at`] that accepts a clone with no record
    /// yet: only for `init` and install, which create or adopt the graph
    /// at `graph_root` and then write the record.
    pub fn check_identity_or_unrecorded(
        &self,
        graph_id: &str,
        graph_root: &str,
    ) -> Result<(), Error> {
        let root = normalize_root(graph_root);
        if self.graph_ids.is_empty() {
            return Ok(());
        }
        if self.graph_roots.get(graph_id).is_some_and(|r| *r == root) {
            // One graph per root here too (Terra #7776).
            return self.check_identity_at(graph_id, &root);
        }
        Err(Error::new(
            "graph.identity_mismatch",
            format!(
                "{} holds graph {graph_id}, but this clone records {}",
                self.name,
                self.graph_ids.join(", ")
            ),
        ))
    }
}

/// Writes `creating.json` only where none exists: a synced temporary file
/// hard-linked into place (atomic, and refused when the name is taken),
/// then the folder synced.
fn create_intent(dir: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = dir.join(format!(
        ".creating.{}-{}.tmp",
        std::process::id(),
        &mdkg_format::Uuid7::now().to_string()[24..]
    ));
    // Only a temporary file this call created is ever removed (Terra
    // #8377).
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        created = true;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::hard_link(&tmp, dir.join("creating.json"))
    })();
    if created {
        std::fs::remove_file(&tmp).ok();
    }
    match result {
        Ok(()) => {
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::new(
            "graph.ref_exists",
            "another creation started at the same time; rerun it",
        )),
        Err(e) => Err(e.into()),
    }
}

/// The pending creation's graph folder name (`.mdkg` when none is
/// recorded).
pub(crate) fn read_creation_dir(common: &Path) -> Result<String, Error> {
    let path = refwrite::graph_dir(common)?.join("creating.json");
    let bytes = match mdkg_git::read_capped(&path, 64 * 1024) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(committed::DEFAULT_DIR.to_owned());
        }
        Err(e) => return Err(e.into()),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::new("graph.config_invalid", format!("creating.json: {e}")))?;
    Ok(value
        .get("dir")
        .and_then(Value::as_str)
        .map(normalize_root)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| committed::DEFAULT_DIR.to_owned()))
}

/// The creation intent, `(graph id, root, graph ref)`, if one is pending.
pub(crate) fn read_creation(common: &Path) -> Result<Option<(String, String, String)>, Error> {
    let path = refwrite::graph_dir(common)?.join("creating.json");
    let bytes = match mdkg_git::read_capped(&path, 64 * 1024) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::new("graph.config_invalid", format!("creating.json: {e}")))?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    match (field("graph_id"), field("root"), field("ref")) {
        (Some(id), Some(root), Some(name)) => Ok(Some((id, normalize_root(&root), name))),
        _ => Err(Error::new(
            "graph.config_invalid",
            "creating.json is malformed",
        )),
    }
}

/// The clone's graph-ref settings, or `None` in checkout mode (no
/// `mdkg.graph.ref`).
pub fn resolve(repo: &Path) -> Result<Option<GraphRef>, Error> {
    let common = mdkg_git::common_dir(repo)?;
    let keys = common_config(repo, &common)?;
    resolve_from(repo, common, &keys)
}

/// [`resolve`] from a config snapshot already read, so one lookup reads
/// the common config once (orchestrator #7376).
fn resolve_from(
    repo: &Path,
    common: PathBuf,
    keys: &BTreeMap<String, String>,
) -> Result<Option<GraphRef>, Error> {
    let Some(name) = keys.get("mdkg.graph.ref") else {
        return Ok(None);
    };
    let name = full_ref(name, "mdkg.graph.ref")?;
    let main_ref = keys
        .get("mdkg.mainref")
        .map(|v| main_ref_name(repo, v))
        .transpose()?;
    if main_ref.as_deref() == Some(name.as_str()) {
        return Err(Error::new(
            "graph.ref_invalid",
            "mdkg.graph.ref must differ from mdkg.mainRef",
        ));
    }
    let main_ref_base = match std::env::var("MDKG_MAIN_REF") {
        Ok(v) if !v.is_empty() => Some(main_ref_name(repo, &v)?),
        _ => main_ref.clone(),
    };
    let remote = match keys.get("mdkg.graph.remote") {
        None => None,
        Some(remote) => {
            if remote.is_empty() || remote.contains(['/', ' ', '\0']) || remote.starts_with('-') {
                return Err(Error::new(
                    "graph.remote_invalid",
                    format!("mdkg.graph.remote {remote:?} isn't a remote name"),
                ));
            }
            let remote_ref = match keys.get("mdkg.graph.remoteref") {
                Some(r) => full_ref(r, "mdkg.graph.remoteRef")?,
                None => name.clone(),
            };
            if !remote_ref.starts_with("refs/heads/") {
                return Err(Error::new(
                    "graph.remote_invalid",
                    "mdkg.graph.remoteRef must be under refs/heads/",
                ));
            }
            Some(Remote {
                name: remote.clone(),
                remote_ref,
                pinned: pinned(&common)?,
            })
        }
    };
    let seconds = |key: &str, default: Duration| -> Result<Duration, Error> {
        match keys.get(key) {
            None => Ok(default),
            Some(v) => v.parse::<u64>().map(Duration::from_secs).map_err(|_| {
                Error::new(
                    "graph.config_invalid",
                    format!("{key} must be a whole number of seconds"),
                )
            }),
        }
    };
    let push_delay = seconds("mdkg.graph.sync.pushdelay", DEFAULT_PUSH_DELAY)?;
    let pull_interval = seconds("mdkg.graph.sync.pullinterval", DEFAULT_PULL_INTERVAL)?;
    if push_delay.is_zero() {
        return Err(Error::new(
            "graph.config_invalid",
            "mdkg.graph.sync.pushDelay must be positive",
        ));
    }
    let publish_design = flag(keys, "mdkg.graph.publishdesign")?;
    let follow_fetch = flag(keys, "mdkg.follow.fetch")?;
    // The record is the exact key `mdkg.graph.<graph_id>.ref`, naming
    // this graph ref.
    let mut graph_ids: Vec<String> = keys
        .iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("mdkg.graph.")?.strip_suffix(".ref")?;
            (mdkg_format::Uuid7::parse(id).is_ok() && full_ref(v, "record").ok()? == name)
                .then(|| id.to_owned())
        })
        .collect();
    graph_ids.sort();
    graph_ids.dedup();
    let graph_roots = graph_ids
        .iter()
        .map(|id| (id.clone(), record_root(keys, id)))
        .collect();
    let has = |id: &str, suffix: &str| keys.contains_key(&format!("mdkg.graph.{id}.{suffix}"));
    let mut legacy_ids: Vec<String> = keys
        .keys()
        .filter_map(|k| k.strip_prefix("mdkg.graph.")?.strip_suffix(".worktree"))
        .filter(|id| !has(id, "ref") && !has(id, "root"))
        .map(str::to_owned)
        .collect();
    legacy_ids.sort();
    let graph_dirs = graph_ids
        .iter()
        .filter_map(|id| {
            let dir = normalize_root(keys.get(&format!("mdkg.graph.{id}.dir"))?);
            (!dir.is_empty()).then(|| (id.clone(), dir))
        })
        .collect();
    let mut root_folders = BTreeMap::new();
    for id in explicit_ids(keys) {
        let root = record_root(keys, &id);
        root_folders.entry(root).or_insert_with_key(|root| {
            dir_for(keys, root).unwrap_or_else(|e| format!("conflict: {}", e.message))
        });
    }
    Ok(Some(GraphRef {
        common,
        name,
        remote,
        main_ref,
        main_ref_base,
        push_delay,
        pull_interval,
        publish_design,
        follow_fetch,
        graph_ids,
        graph_roots,
        legacy_ids,
        graph_dirs,
        root_folders,
    }))
}

fn flag(keys: &BTreeMap<String, String>, key: &str) -> Result<bool, Error> {
    match keys.get(key).map(|v| v.to_ascii_lowercase()) {
        None => Ok(false),
        Some(v) if matches!(v.as_str(), "on" | "true" | "yes" | "1") => Ok(true),
        Some(v) if matches!(v.as_str(), "off" | "false" | "no" | "0") => Ok(false),
        Some(_) => Err(Error::new(
            "graph.config_invalid",
            format!("{key} must be on or off"),
        )),
    }
}

/// Every `mdkg.*` key of `<common>/config` alone, keys lowercased as git
/// compares them (a later value wins).
fn common_config(_repo: &Path, common: &Path) -> Result<BTreeMap<String, String>, Error> {
    let file = common.join("config");
    let snapshot = config_snapshot(&file)?;
    // Parsed by an isolated Git that reads no repository, global, system or
    // included config, even at startup; a parse error fails closed.
    let parsed = mdkg_git::parse_config_snapshot(common, &snapshot).map_err(|e| {
        Error::new(
            "graph.config_invalid",
            format!("can't parse {}: {e}", file.display()),
        )
    })?;
    let mut keys = BTreeMap::new();
    for record in parsed.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // Filtered before decoding: another tool's non-UTF-8 value (a
        // legacy-encoded `user.name`) never blocks resolution (Terra #7173).
        let (key, value) = match record.iter().position(|b| *b == b'\n') {
            Some(at) => (&record[..at], &record[at + 1..]),
            None => (record, &b""[..]),
        };
        if !key.to_ascii_lowercase().starts_with(b"mdkg.") {
            continue;
        }
        let (Ok(key), Ok(value)) = (std::str::from_utf8(key), std::str::from_utf8(value)) else {
            return Err(Error::new(
                "graph.config_invalid",
                "mdkg config isn't UTF-8",
            ));
        };
        let key = key.to_ascii_lowercase();
        // A key given twice with different values is a conflict, never
        // the last one silently (Terra #7807): two records for one graph
        // must refuse.
        if let Some(had) = keys.get(&key)
            && had != value
        {
            return Err(Error::new(
                "graph.config_invalid",
                format!("{key} is set twice with different values; keep one"),
            ));
        }
        keys.insert(key, value.to_owned());
    }
    Ok(keys)
}

/// The common config file's bytes, read once through a descriptor opened
/// with `O_NOFOLLOW` (D12's rule): a regular file of at most 1 MiB, never
/// a link. Git then parses that snapshot, never the path again.
fn config_snapshot(file: &Path) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    const CAP: u64 = 1024 * 1024;
    let bad = |why: &str| Error::new("graph.config_invalid", format!("{}: {why}", file.display()));
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(bad("the common config is missing"));
        }
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            return Err(bad("the config must not be a link"));
        }
        Err(e) => return Err(e.into()),
    };
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(bad("the config must be a regular file"));
    }
    if std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
        return Err(bad("the config must have exactly one link"));
    }
    let mut bytes = Vec::new();
    f.take(CAP + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CAP {
        return Err(bad("the config is over 1 MiB"));
    }
    Ok(bytes)
}

/// A configured graph ref as the authority reader names it: a short name
/// is under `refs/heads/`, never a same-named tag (#16207).
pub fn graph_ref_name(value: &str) -> Result<String, Error> {
    full_ref(value, "mdkg.graph.ref")
}

/// A graph or remote ref as a full name: a short name is under
/// `refs/heads/`.
fn full_ref(value: &str, key: &str) -> Result<String, Error> {
    let name = if value.starts_with("refs/") {
        value.to_owned()
    } else {
        format!("refs/heads/{value}")
    };
    if !valid_ref(&name) || name.starts_with("refs/mdkg/") {
        return Err(Error::new(
            "graph.ref_invalid",
            format!("{key} {value:?} isn't a valid ref name"),
        ));
    }
    Ok(name)
}

/// `mainRef` as a full ref: a full name as is; a bare name is
/// `refs/heads/<x>` when that exists, then `refs/remotes/<x>` when that
/// does, else `refs/heads/<x>`.
fn main_ref_name(repo: &Path, value: &str) -> Result<String, Error> {
    if value.starts_with("refs/") {
        if !valid_ref(value) {
            return Err(Error::new(
                "graph.ref_invalid",
                format!("mdkg.mainRef {value:?} isn't a valid ref name"),
            ));
        }
        return Ok(value.to_owned());
    }
    let local = format!("refs/heads/{value}");
    let remote = format!("refs/remotes/{value}");
    if !valid_ref(&local) {
        return Err(Error::new(
            "graph.ref_invalid",
            format!("mdkg.mainRef {value:?} isn't a valid ref name"),
        ));
    }
    if !mdkg_git::has_ref(repo, &local) && mdkg_git::has_ref(repo, &remote) {
        return Ok(remote);
    }
    Ok(local)
}

/// A conservative `check-ref-format` without running git.
fn valid_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name
            .bytes()
            .all(|b| b > 0x20 && b != 0x7f && !b"~^:?*[\\".contains(&b))
        && name
            .split('/')
            .all(|c| !c.is_empty() && !c.starts_with('.'))
}

/// The pinned destination, `<common>/mdkg/graph/remote.json`
/// (`{"fetch": [...], "push": [...]}`), if install wrote one.
fn pinned(common: &Path) -> Result<Option<Destination>, Error> {
    let path = common.join("mdkg").join("graph").join("remote.json");
    let bytes = match mdkg_git::read_capped(&path, 64 * 1024) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::new("graph.remote_invalid", format!("remote.json: {e}")))?;
    let list = |key: &str| -> Result<Vec<String>, Error> {
        value
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new("graph.remote_invalid", format!("remote.json needs {key}")))?
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::new("graph.remote_invalid", "remote.json URLs must be strings")
                })
            })
            .collect()
    };
    Ok(Some(Destination {
        fetch: list("fetch")?,
        push: list("push")?,
    }))
}

/// Writes the pinned destination, atomically and fsynced.
pub fn pin_destination(common: &Path, destination: &Destination) -> Result<(), Error> {
    let dir = refwrite::graph_dir(common)?;
    let value = serde_json::json!({"fetch": destination.fetch, "push": destination.push});
    crate::intent::write_durably(
        &dir,
        "remote.json",
        &serde_json::to_vec_pretty(&value).map_err(|e| Error::new("graph.io", e.to_string()))?,
    )?;
    Ok(())
}

/// Committed config and definitions at one pinned graph commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Authority {
    /// Where the commit came from, captured by the same resolution:
    /// `graph:<graph ref>@<sha12>` in ref mode, `checkout:<rev>@<sha12>` in
    /// checkout mode, or `commit:<sha12>` from [`authority_at`].
    pub source: String,
    /// The graph ref read, in ref mode.
    pub graph_ref: Option<String>,
    /// The pinned commit: use it for the whole operation.
    pub commit: String,
    pub graph_id: String,
    /// The committed `.mdkg/config.json`.
    pub config: Value,
    /// `.mdkg/agents/*.json`, then inline `agents.<name>` entries of the
    /// config that no file replaces (`defaults` excluded).
    pub definitions: BTreeMap<String, Def>,
}

/// One agent definition, as committed.
#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    /// The blob it was read from (the config's, when inline).
    pub blob: String,
    pub value: Value,
    pub inline: bool,
}

/// A definition file's cap, as `mdkg_run::orchestration::agent` reads it.
pub const DEFINITION_CAP: u64 = 1024 * 1024;

/// The authority at the graph tip. In checkout mode it's the HEAD commit of
/// the registered graph checkout (`mdkg.graph.<id>.worktree`), else the
/// invoking checkout's HEAD.
pub fn authority(repo: &Path) -> Result<Authority, Error> {
    authority_in(repo, "")
}

/// [`authority`] of the graph whose root is `graph_root`, a
/// repository-relative folder (`""` for the repository root): its
/// `<graph_root>/.mdkg/config.json` and `<graph_root>/.mdkg/agents/`.
pub fn authority_in(repo: &Path, graph_root: &str) -> Result<Authority, Error> {
    let root = checked_root(graph_root)?;
    // One snapshot of the common config for the whole lookup (orchestrator
    // #7376): the mode and the checkout records come from the same read.
    let common = mdkg_git::common_dir(repo)?;
    let keys = common_config(repo, &common)?;
    authority_with(repo, &root, &common, &keys, Read::Whole)
}

/// [`authority_in`] from a config snapshot already read.
/// How much of an authority snapshot a read parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// The config and every agent definition.
    Whole,
    /// The config only, for runtime settings (the mail and lease
    /// settings): an unrelated invalid definition doesn't refuse them
    /// (orchestrator, 2026-11-02, #15433). Commit selection and identity
    /// checks are the same.
    ConfigOnly,
}

fn authority_with(
    repo: &Path,
    graph_root: &str,
    common: &Path,
    keys: &BTreeMap<String, String>,
    read: Read,
) -> Result<Authority, Error> {
    let dir = dir_for(keys, graph_root)?;
    match resolve_from(repo, common.to_owned(), keys)? {
        Some(graph) => {
            let tip = tip(repo, &graph)?;
            let mut found = read_at_dir(repo, &tip, graph_root, &dir, read)?;
            if let Err(e) = graph.check_identity_at(&found.graph_id, graph_root) {
                // A 0.7.0 nested record (`.worktree` only, no `.ref` or
                // `.root`) binds the nested root its graph sits at, never a
                // copy of the root graph (orchestrator #7376, #7556).
                // An explicit binding at this root always wins (orchestrator
                // #8573).
                let root = normalize_root(graph_root);
                // Any `.root` record at this root counts, a partial one
                // with no `.ref` included (Terra #9366).
                let partial = keys.iter().any(|(k, v)| {
                    k.starts_with("mdkg.graph.")
                        && k.ends_with(".root")
                        && normalize_root(v) == root
                });
                if graph.graph_roots.values().any(|r| *r == root)
                    || partial
                    || !legacy_nested(repo, &tip, keys, &found.graph_id, graph_root)?
                {
                    return Err(e);
                }
            }
            found.source = format!("graph:{}@{}", graph.name, &tip[..12]);
            found.graph_ref = Some(graph.name);
            Ok(found)
        }
        None => {
            let (name, commit, snapshot) =
                checkout_head(repo, graph_root, &dir, common, keys, read)?;
            // The registered checkout's snapshot was read once already.
            let mut found = match snapshot {
                Some(found) => found,
                None => {
                    // The invoking HEAD (no checkout records): a graph id
                    // explicitly bound to another root is refused here too
                    // (Terra #8686).
                    let found = read_at_dir(repo, &commit, graph_root, &dir, read)?;
                    let root = normalize_root(graph_root);
                    let id = &found.graph_id;
                    let bound_elsewhere = keys
                        .get(&format!("mdkg.graph.{id}.root"))
                        .map(|r| normalize_root(r) != root)
                        .unwrap_or_else(|| {
                            keys.contains_key(&format!("mdkg.graph.{id}.ref")) && !root.is_empty()
                        });
                    if !id.is_empty() && bound_elsewhere {
                        return Err(Error::new(
                            "graph.identity_mismatch",
                            format!("graph {id} is recorded at another root than {root:?}"),
                        ));
                    }
                    found
                }
            };
            found.source = format!("checkout:{name}@{}", &commit[..12]);
            Ok(found)
        }
    }
}

/// Why a checkout has no authority to read (decided 2026-10-29: option
/// (a) of the hook question).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// The invoking checkout has no commit yet.
    Unborn,
    /// Its HEAD commits no graph config at the root asked for.
    NoGraphConfig,
}

/// [`authority_in`]'s answer for callers (such as git hooks) that must
/// tell "this checkout has no graph" from a failure.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    Found(Authority),
    /// Only in checkout mode (no `mdkg.graph.ref`), with no registered
    /// graph checkout (no `mdkg.graph.<id>.worktree` record), when HEAD is
    /// unborn or commits neither `<root>/.mdkg/config.json` nor
    /// `<root>/.mdkg/graph.json`. Every other state is an error.
    Absent(Absent),
}

/// [`authority_in`], with the one authority-absent outcome typed:
/// [`Lookup::Absent`] for an unborn or unregistered checkout with no
/// committed graph config. Every other failure stays an error.
pub fn lookup_in(repo: &Path, graph_root: &str) -> Result<Lookup, Error> {
    lookup_with(repo, graph_root, Read::Whole)
}

/// [`lookup_in`] reading only the config: the same commit, identity and
/// absence rules, with no agent definitions parsed (they're left empty).
pub fn lookup_config_in(repo: &Path, graph_root: &str) -> Result<Lookup, Error> {
    lookup_with(repo, graph_root, Read::ConfigOnly)
}

fn lookup_with(repo: &Path, graph_root: &str, read: Read) -> Result<Lookup, Error> {
    // The root is checked before any probe: a bad spelling is an error,
    // never absence. One config snapshot for the whole lookup (Terra
    // #8516, #8551).
    let root = checked_root(graph_root)?;
    let common = mdkg_git::common_dir(repo)?;
    let keys = common_config(repo, &common)?;
    // The folder at this root is settled before any absence is decided
    // (Terra #9705).
    let dir = dir_for(&keys, &root)?;
    if resolve_from(repo, common.clone(), &keys)?.is_none() {
        // A checkout record anywhere registers the clone; a `.root` record
        // only for the root it names (Opus #8729), so a partial record is a
        // refusal at its own root and never hides absence elsewhere.
        let registered = keys.iter().any(|(k, v)| {
            k.starts_with("mdkg.graph.")
                && (k.ends_with(".worktree") || (k.ends_with(".root") && normalize_root(v) == root))
        });
        if !registered {
            let Some(head) = rev(repo, "HEAD")? else {
                // Unborn only when proven: HEAD is a symbolic ref to a
                // branch that doesn't exist yet (Terra #6964). A HEAD
                // naming a missing or unreadable commit is an error.
                return if unborn(repo)? {
                    Ok(Lookup::Absent(Absent::Unborn))
                } else {
                    Err(Error::new("graph.io", "HEAD names a commit git can't read"))
                };
            };
            // Every folder on the way to the graph folder must be a real
            // folder at HEAD: a committed link or file there is an error,
            // never "no graph" (Terra #7885).
            let mut path = String::new();
            let graph = if root.is_empty() {
                dir
            } else {
                format!("{root}/{dir}")
            };
            for part in graph.split('/') {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(part);
                match head_entry(repo, &head, &path)? {
                    None => return Ok(Lookup::Absent(Absent::NoGraphConfig)),
                    Some(kind) if kind == "tree" => {}
                    Some(kind) => {
                        return Err(Error::new(
                            "graph.config_invalid",
                            format!("{path} is a {kind} at HEAD, not a folder"),
                        ));
                    }
                }
            }
            let config = head_entry(repo, &head, &format!("{graph}/config.json"))?;
            let v2 = head_entry(repo, &head, &format!("{graph}/graph.json"))?;
            if config.is_none() && v2.is_none() {
                return Ok(Lookup::Absent(Absent::NoGraphConfig));
            }
        }
    }
    authority_with(repo, &root, &common, &keys, read).map(Lookup::Found)
}

/// The type of HEAD's entry at `path` (`blob`, `tree`, `commit`), or `None`
/// when a successful listing shows none. Any git failure is an error,
/// never absence.
fn head_entry(repo: &Path, head: &str, path: &str) -> Result<Option<String>, Error> {
    let out = mdkg_git::command(repo)
        .args([
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            "--full-tree",
            "--end-of-options",
            head,
            "--",
            path,
        ])
        .run(None)?;
    if !out.status.success() {
        return Err(Error::new(
            "graph.io",
            format!(
                "git couldn't list {path} at HEAD (exit {:?})",
                out.status.code()
            ),
        ));
    }
    let Some(entry) = out.stdout.split(|b| *b == 0).find(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let entry = String::from_utf8_lossy(entry);
    let kind = entry.split_whitespace().nth(1).unwrap_or_default();
    Ok(Some(kind.to_owned()))
}

/// The graph ref's tip, refusing a missing or symbolic ref
/// (`graph.ref_missing`, `graph.ref_invalid`).
pub fn tip(repo: &Path, graph: &GraphRef) -> Result<String, Error> {
    let symbolic = mdkg_git::command(repo)
        .args(["symbolic-ref", "-q", &graph.name])
        .run(None)?;
    if symbolic.status.success() {
        return Err(Error::new(
            "graph.ref_invalid",
            format!("{} must not be a symbolic ref", graph.name),
        ));
    }
    rev(repo, &graph.name)?.ok_or_else(|| {
        Error::new(
            "graph.ref_missing",
            format!(
                "the graph ref {} doesn't exist; run `{BIN} install`",
                graph.name
            ),
        )
    })
}

/// Whether HEAD is a symbolic ref to a branch with no ref yet.
fn unborn(repo: &Path) -> Result<bool, Error> {
    let out = mdkg_git::command(repo)
        .args(["symbolic-ref", "-q", "HEAD"])
        .run(None)?;
    match out.status.code() {
        Some(0) => {}
        // Detached: not unborn.
        Some(1) => return Ok(false),
        code => {
            return Err(Error::new(
                "graph.io",
                format!("git couldn't read HEAD (exit {code:?})"),
            ));
        }
    }
    // The exact name: only git's terminating newline is removed, and a
    // name that isn't UTF-8 is refused rather than altered (Terra #7885).
    let mut raw = out.stdout;
    if raw.last() == Some(&b'\n') {
        raw.pop();
    }
    let branch = String::from_utf8(raw)
        .map_err(|_| Error::new("graph.io", "HEAD names a branch whose name isn't UTF-8"))?;
    // Only a branch can be unborn; HEAD naming any other missing ref is a
    // broken checkout.
    if !branch.starts_with("refs/heads/") {
        return Ok(false);
    }
    // Listed with ref paranoia, and any warning (git reports a torn or
    // garbage ref only on stderr, exit 0) is an error (Opus #7843).
    let out = mdkg_git::command(repo)
        .env("GIT_REF_PARANOIA", "1")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "--end-of-options",
            branch.as_str(),
        ])
        .run(None)?;
    if !out.status.success() || !out.stderr.is_empty() {
        return Err(Error::new(
            "graph.io",
            format!(
                "git couldn't list {branch} cleanly (exit {:?})",
                out.status.code()
            ),
        ));
    }
    // A descendant like `<branch>/x` isn't the branch itself.
    let exists = out
        .stdout
        .split(|b| *b == b'\n')
        .any(|line| line == branch.as_bytes());
    Ok(!exists)
}

/// The commit `name` resolves to, or `None` only when the name provably
/// doesn't exist (Terra #7266): a name that exists but whose object git
/// can't read as a commit is an error, never absence.
fn rev(repo: &Path, name: &str) -> Result<Option<String>, Error> {
    let peeled = format!("{name}^{{commit}}");
    let out = mdkg_git::command(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            peeled.as_str(),
        ])
        .run(None)?;
    match out.status.code() {
        Some(0) => return Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
        Some(1) => {}
        code => {
            return Err(Error::new(
                "graph.io",
                format!("git couldn't resolve {name} (exit {code:?})"),
            ));
        }
    }
    let out = mdkg_git::command(repo)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options", name])
        .run(None)?;
    match out.status.code() {
        Some(1) => Ok(None),
        Some(0) => Err(Error::new(
            "graph.io",
            format!("{name} names an object git can't read as a commit"),
        )),
        code => Err(Error::new(
            "graph.io",
            format!("git couldn't resolve {name} (exit {code:?})"),
        )),
    }
}

/// The registered graph checkout's HEAD for the graph at `graph_root`.
/// Each `mdkg.graph.<id>.worktree` record names a checkout; the one used
/// is the checkout whose committed graph at `graph_root` holds that
/// record's id. Records of one checkout under several ids (a root and
/// nested graphs) are one candidate. With no record, the invoking
/// checkout's HEAD is read, as 0.7.0 did. It fails closed, never guessing
/// between checkouts: among others, `graph.ref_missing` when no candidate
/// matches and `graph.identity_mismatch` when several do (or a record is
/// partial, or names no valid worktree).
fn checkout_head(
    repo: &Path,
    graph_root: &str,
    dir: &str,
    common: &Path,
    keys: &BTreeMap<String, String>,
    read: Read,
) -> Result<(String, String, Option<Authority>), Error> {
    let root = normalize_root(graph_root);
    let records: Vec<(String, String)> = keys
        .iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("mdkg.graph.")?.strip_suffix(".worktree")?;
            Some((id.to_owned(), v.clone()))
        })
        .collect();
    // A `.root` binding with no `.worktree` record is a partial install or
    // repair: a lookup at that root is refused, never read as the invoking
    // HEAD (Terra #6917). Lookups at other roots aren't affected (Opus
    // #7736): the partial record can't bind them.
    for (key, value) in keys.iter() {
        if let Some(id) = key
            .strip_prefix("mdkg.graph.")
            .and_then(|k| k.strip_suffix(".root"))
            && normalize_root(value) == root
            && !records.iter().any(|(r, _)| r == id)
        {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!(
                    "graph {id} has a root record but no checkout record; run `{BIN} install` or `{BIN} doctor --repair`"
                ),
            ));
        }
    }
    if records.is_empty() {
        // A clone that used a graph ref (an identity `.ref` record) and has
        // no graph checkout yet, as a rollback can leave it, never reads
        // the invoking HEAD: that checkout's `.mdkg` is whatever the code
        // branch last held (#16207).
        if let Some((key, name)) = keys.iter().find(|(k, _)| {
            k.strip_prefix("mdkg.graph.")
                .and_then(|k| k.strip_suffix(".ref"))
                .is_some_and(|id| mdkg_format::Uuid7::parse(id).is_ok())
        }) {
            return Err(Error::new(
                "graph.checkout_missing",
                format!(
                    "checkout mode, and this clone used the graph ref {name} ({key}) but registers \
                     no graph checkout, so its graph isn't read from this checkout's HEAD; run \
                     `{BIN} install` to make and register one"
                ),
            ));
        }
        let commit = rev(repo, "HEAD")?.ok_or_else(|| {
            Error::new(
                "graph.ref_missing",
                "HEAD names no commit to read the graph from",
            )
        })?;
        return Ok(("HEAD".to_owned(), commit, None));
    }
    // Explicit records bind the root their `.root` names. A legacy 0.7.0
    // record (no `.root`) binds whichever root its checkout holds its id
    // at: the repository root when the root config carries it, else the
    // nested root that does (orchestrator #6947, checkout mode only; G19).
    // An id with `.root` or `.ref` is explicitly bound (a `.ref` without
    // `.root` binds the repository root); only a `.worktree`-only record is
    // 0.7.0's legacy shape (Terra #8551).
    let explicit = |id: &str| {
        keys.contains_key(&format!("mdkg.graph.{id}.root"))
            || keys.contains_key(&format!("mdkg.graph.{id}.ref"))
    };
    // One graph per root (Terra #7776): two ids explicitly bound to this
    // root would let either replace the other in a shared checkout.
    let bound_here: Vec<&String> = records
        .iter()
        .map(|(id, _)| id)
        .filter(|id| explicit(id) && record_root(keys, id) == root)
        .collect();
    if bound_here.len() > 1 {
        return Err(Error::new(
            "graph.identity_mismatch",
            format!(
                "this clone records several graphs at {root:?} ({}); run `{BIN} doctor --repair`",
                bound_here
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    // Candidates are the checkouts with a record that can bind this root,
    // each read once and by its committed graph id alone (orchestrator
    // #6859). A checkout explicitly recorded only for other roots isn't a
    // candidate here (Opus #6894, orchestrator #6947).
    // An explicit binding at this root always wins: legacy records are
    // candidates only where no id is explicitly bound (orchestrator #8573,
    // Terra #8754).
    let relevant = |id: &str| {
        if explicit(id) {
            record_root(keys, id) == root
        } else {
            bound_here.is_empty()
        }
    };
    let mut admins: Vec<&String> = records
        .iter()
        .filter(|(id, _)| relevant(id))
        .map(|(_, a)| a)
        .collect();
    admins.sort();
    admins.dedup();
    let mut matches = Vec::new();
    for admin in admins {
        if admin.is_empty() || admin.contains(['/', '\\']) || admin == ".." {
            return Err(Error::new(
                "graph.identity_mismatch",
                format!("a graph checkout record names no valid worktree ({admin:?})"),
            ));
        }
        let mine: Vec<&String> = records
            .iter()
            .filter(|(id, a)| a == admin && relevant(id))
            .map(|(id, _)| id)
            .collect();
        let bound_here = mine
            .iter()
            .any(|id| explicit(id) && record_root(keys, id) == root);
        let name = format!("worktrees/{admin}/HEAD");
        // Any relevant candidate that can't be read refuses the whole
        // lookup (orchestrator, on Terra #6651): authority never falls
        // through to another registered graph.
        registered(common, admin)?;
        let commit = rev(repo, &name)?.ok_or_else(|| gone(admin))?;
        let graph_id = if bound_here {
            committed_graph_id(repo, &commit, graph_root, dir)?
        } else {
            // Legacy records only: a checkout with no graph at this root
            // simply isn't one.
            committed_graph_id_if_present(repo, &commit, graph_root, dir)?
        };
        let Some(graph_id) = graph_id else { continue };
        let Some(id) = mine.iter().find(|id| ***id == graph_id) else {
            continue;
        };
        let matched = if explicit(id) {
            record_root(keys, id) == root
        } else {
            // A legacy record whose checkout holds its id at the root is
            // bound to the root, never to a nested copy (no moved config).
            root.is_empty()
                || committed_graph_id_if_present(repo, &commit, "", &dir_for(keys, "")?)?.as_deref()
                    != Some(id.as_str())
        };
        if matched {
            matches.push((name, commit));
        }
    }
    match matches.len() {
        // Definitions load for the single match only, and an error there
        // is reported as itself.
        1 => {
            let (name, commit) = matches.remove(0);
            let found = read_at_dir(repo, &commit, graph_root, dir, read)?;
            Ok((name, commit, Some(found)))
        }
        0 => Err(Error::new(
            "graph.ref_missing",
            format!(
                "no registered graph checkout holds a graph recorded at {root:?}; run `{BIN} install` to register it"
            ),
        )),
        _ => Err(Error::new(
            "graph.identity_mismatch",
            "several registered graph checkouts hold this graph",
        )),
    }
}

/// The `graph_id` a commit's config at `graph_root` names, read without
/// any definition.
fn committed_graph_id(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Option<String>, Error> {
    let file = committed::config_file_dir(repo, commit, graph_root, dir)?;
    let config = parse_object(&file.path, &file.bytes)?;
    // Only the identity fields: the full config is validated for the
    // single match (orchestrator #7628). Three cases (orchestrator #8608):
    // a legacy config with no graph_id is no candidate; a graph_id that's
    // present but malformed fails closed; an unreadable config fails
    // closed (above).
    match crate::config::read_identity(&config) {
        Ok((_, Some(id))) => Ok(Some(id.to_string())),
        Ok((_, None)) => Ok(None),
        Err(e) => Err(Error::new(
            "graph.config_invalid",
            format!("{}: {}", file.path, e),
        )),
    }
}

/// Whether `id` is a 0.7.0 nested graph at `graph_root` in `commit`: its
/// only record is `.worktree`, the root is nested, and the commit's root
/// graph doesn't carry the id.
fn legacy_nested(
    repo: &Path,
    commit: &str,
    keys: &BTreeMap<String, String>,
    id: &str,
    graph_root: &str,
) -> Result<bool, Error> {
    let root = normalize_root(graph_root);
    let has = |suffix: &str| keys.contains_key(&format!("mdkg.graph.{id}.{suffix}"));
    if root.is_empty() || id.is_empty() || !has("worktree") || has("ref") || has("root") {
        return Ok(false);
    }
    Ok(
        committed_graph_id_if_present(repo, commit, "", &dir_for(keys, "")?)?.as_deref()
            != Some(id),
    )
}

/// [`committed_graph_id`], or `None` when the commit has no graph config
/// at `graph_root`.
fn committed_graph_id_if_present(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Option<String>, Error> {
    if committed::config_file_if_present_dir(repo, commit, graph_root, dir)?.is_none() {
        return Ok(None);
    }
    committed_graph_id(repo, commit, graph_root, dir)
}

/// The graph folder's name at `graph_root`: the `.dir` of the graph
/// explicitly bound there (orchestrator #8321), else `.mdkg`.
fn dir_for(keys: &BTreeMap<String, String>, graph_root: &str) -> Result<String, Error> {
    let root = normalize_root(graph_root);
    // Every graph explicitly bound to this root, each with its effective
    // folder: a missing or empty `.dir` is `.mdkg` (orchestrator #9719,
    // Opus #9671). Differing folders are a conflict, never settled by key
    // order (Opus #9270).
    let mut dirs: Vec<(String, String)> = explicit_ids(keys)
        .into_iter()
        .filter(|id| record_root(keys, id) == root)
        .map(|id| {
            let dir = keys
                .get(&format!("mdkg.graph.{id}.dir"))
                .map(|d| normalize_root(d))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| committed::DEFAULT_DIR.to_owned());
            (id, dir)
        })
        .collect();
    dirs.sort_by(|a, b| a.1.cmp(&b.1));
    if let [(first_id, first), .., (last_id, last)] = dirs.as_slice()
        && first != last
    {
        // `doctor --repair` only adds missing records, so the fix is named
        // exactly (Terra #9705): the whole stale record, since removing
        // only its `.root` would rebind it at the repository root (Opus
        // #10313).
        return Err(Error::new(
            "graph.identity_mismatch",
            format!(
                "graphs {first_id} ({first}) and {last_id} ({last}) are both recorded at {root:?} (a graph with no `.dir` record uses the default {default}); remove the stale graph's whole record by hand: `git config --remove-section mdkg.graph.<id>`",
                default = committed::DEFAULT_DIR
            ),
        ));
    }
    Ok(dirs
        .pop()
        .map(|(_, d)| d)
        .unwrap_or_else(|| committed::DEFAULT_DIR.to_owned()))
}

/// Every id explicitly bound to a root (a `.root` or `.ref` record), less
/// those whose `.ref` names another graph ref (Terra #9366).
fn explicit_ids(keys: &BTreeMap<String, String>) -> Vec<String> {
    let mut ids: Vec<String> = keys
        .keys()
        .filter_map(|k| {
            let rest = k.strip_prefix("mdkg.graph.")?;
            rest.strip_suffix(".root")
                .or_else(|| rest.strip_suffix(".ref"))
        })
        .filter(|id| {
            match (
                keys.get("mdkg.graph.ref"),
                keys.get(&format!("mdkg.graph.{id}.ref")),
            ) {
                (Some(active), Some(own)) => {
                    full_ref(active, "mdkg.graph.ref").ok() == full_ref(own, "record").ok()
                }
                _ => true,
            }
        })
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Refuses while any recorded root has conflicting folder records
/// (orchestrator #9719): a writer never starts beside one.
fn check_all_folders(keys: &BTreeMap<String, String>) -> Result<(), Error> {
    let mut roots: Vec<String> = explicit_ids(keys)
        .iter()
        .map(|id| record_root(keys, id))
        .collect();
    roots.sort();
    roots.dedup();
    for root in roots {
        dir_for(keys, &root)?;
    }
    Ok(())
}

fn gone(admin: &str) -> Error {
    Error::new(
        "graph.ref_missing",
        format!("the registered graph checkout {admin} is gone; run `{BIN} doctor`"),
    )
}

/// A registered worktree whose admin folder and checkout still point at
/// each other: `<common>/worktrees/<admin>/gitdir` names `<checkout>/.git`,
/// and that `.git` file names the admin folder back.
fn registered(common: &Path, admin: &str) -> Result<(), Error> {
    let admin_dir = common.join("worktrees").join(admin);
    let read = |p: &Path| -> Option<String> {
        let meta = std::fs::symlink_metadata(p).ok()?;
        if !meta.is_file() {
            return None;
        }
        // Line endings only: a path may end in a space (Terra #6994).
        mdkg_git::read_small(p)
            .ok()
            .map(|s| s.trim_end_matches(['\n', '\r']).to_owned())
    };
    // Either link may be relative (git 2.48's worktree.useRelativePaths):
    // the forward one from the admin folder, the back one from the folder
    // holding the checkout's `.git` file.
    let gitdir = read(&admin_dir.join("gitdir")).ok_or_else(|| gone(admin))?;
    let dot_git = admin_dir.join(&gitdir);
    // Only a checkout's `.git` file is ever opened: the record must name
    // one, and it must point back with git's own `gitdir: ` form.
    if dot_git.file_name().is_none_or(|n| n != ".git") {
        return Err(gone(admin));
    }
    let back = read(&dot_git).ok_or_else(|| gone(admin))?;
    let back = back.strip_prefix("gitdir: ").ok_or_else(|| gone(admin))?;
    let checkout = dot_git.parent().ok_or_else(|| gone(admin))?;
    let same =
        std::fs::canonicalize(checkout.join(back)).ok() == std::fs::canonicalize(&admin_dir).ok();
    if !same {
        return Err(gone(admin));
    }
    Ok(())
}

/// The authority at `commit`, a full commit id.
pub fn authority_at(repo: &Path, commit: &str) -> Result<Authority, Error> {
    authority_at_in(repo, commit, "")
}

/// [`authority_at`] of the graph whose root is `graph_root`.
pub fn authority_at_in(repo: &Path, commit: &str, graph_root: &str) -> Result<Authority, Error> {
    authority_at_dir(repo, commit, graph_root, committed::DEFAULT_DIR)
}

/// [`authority_at_in`] for a graph whose folder under its root is `dir`
/// (`mdkg.graph.<id>.dir`, orchestrator #8321).
pub fn authority_at_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Authority, Error> {
    read_at_dir(repo, commit, graph_root, dir, Read::Whole)
}

/// [`authority_at`] reading only the config: no agent definition is read
/// (they're left empty), for callers that need only the settings and the
/// graph id (orchestrator #15706).
pub fn config_at(repo: &Path, commit: &str) -> Result<Authority, Error> {
    config_at_dir(repo, commit, "", committed::DEFAULT_DIR)
}

/// [`config_at`] of the graph at `graph_root`, in folder `dir`.
pub fn config_at_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
) -> Result<Authority, Error> {
    read_at_dir(repo, commit, graph_root, dir, Read::ConfigOnly)
}

fn read_at_dir(
    repo: &Path,
    commit: &str,
    graph_root: &str,
    dir: &str,
    read: Read,
) -> Result<Authority, Error> {
    // A config-only read never looks at `agents/`: one oversized, linked
    // or malformed definition file can't refuse it (Terra #15629).
    let (config_file, files) = match read {
        Read::Whole => {
            committed::authority_files_dir(repo, commit, graph_root, dir, DEFINITION_CAP)?
        }
        Read::ConfigOnly => (
            committed::config_file_dir(repo, commit, graph_root, dir)?,
            Vec::new(),
        ),
    };
    let config = parse_object(&config_file.path, &config_file.bytes)?;
    // The same format checks as the committed reader's.
    crate::config::from_snapshot(config.clone())?;
    let graph_id = config
        .get("graph_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut definitions = BTreeMap::new();
    // Config-only leaves the definitions empty, inline ones included, so
    // none can stand in for a file definition it didn't read.
    let inline = config
        .get("agents")
        .and_then(Value::as_object)
        .filter(|_| read == Read::Whole);
    if let Some(inline) = inline {
        for (name, value) in inline {
            if name != "defaults" {
                definitions.insert(
                    name.clone(),
                    Def {
                        blob: config_file.blob.clone(),
                        value: value.clone(),
                        inline: true,
                    },
                );
            }
        }
    }
    for file in files {
        let name = file
            .path
            .rsplit('/')
            .next()
            .and_then(|n| n.strip_suffix(".json"))
            .unwrap_or_default();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let value = parse_object(&file.path, &file.bytes)?;
        definitions.insert(
            name.to_owned(),
            Def {
                blob: file.blob,
                value,
                inline: false,
            },
        );
    }
    Ok(Authority {
        source: format!("commit:{}", commit.get(..12).unwrap_or(commit)),
        graph_ref: None,
        commit: commit.to_owned(),
        graph_id,
        config,
        definitions,
    })
}

/// A committed JSON object, through the settings parser: duplicate keys,
/// a non-object or too deep a value are refused
/// (`graph.authority_invalid`), as the file reader refuses them.
fn parse_object(path: &str, bytes: &[u8]) -> Result<Value, Error> {
    let origin = mdkg_format::settings::Origin::file(mdkg_format::settings::Layer::Project, path);
    mdkg_format::settings::Input::parse(origin, bytes)
        .map(|input| input.value)
        .map_err(|e| Error::new("graph.authority_invalid", e.to_string()))
}

/// Scope mdkg-git's worktree trust for one synchronous operation, from a
/// caller's existing pins (for example `mdkg_coord_sqlite::git::WorktreeTrust`'s
/// `common` and `worktree`), so authority reads in a hook honour the
/// hook's pins, not a daemon's ambient ones. The coordinator's git module
/// re-exports mdkg-git's trust (orchestrator, 2026-10-30), so a scope set
/// through either is the same one.
pub fn with_git_trust<T>(
    common: Option<PathBuf>,
    worktree: Option<PathBuf>,
    f: impl FnOnce() -> T,
) -> T {
    mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust { common, worktree }, f)
}

/// The graph-owned folders, repository-relative with no trailing `/`: the
/// root `.mdkg`, each workspace's `<path>/<mdkg_dir>`, and each declared
/// subgraph's `<source_path>/.mdkg`. A workspace whose
/// `mdkg_dir` is `.` or empty, or whose folder holds or sits inside
/// another's, is refused (`graph.overlapping_workspace`). Whether a folder
/// overlaps code in `mainRef`'s tree is the sync guard's check.
pub fn graph_paths(config: &Config) -> Result<Vec<String>, Error> {
    graph_paths_dir(config, committed::DEFAULT_DIR)
}

/// [`graph_paths`] for a root graph whose folder is `dir` (orchestrator
/// #8321).
pub fn graph_paths_dir(config: &Config, dir: &str) -> Result<Vec<String>, Error> {
    let dir = committed::check_dir(dir)?;
    let mut paths: Vec<(String, String)> = vec![("root".into(), dir.clone())];
    for ws in &config.workspaces {
        // The root workspace's folder is the graph folder itself (Terra
        // #10216), never an implicit `.mdkg` beside it.
        if ws.path.split('/').all(|c| c.is_empty() || c == ".") {
            continue;
        }
        let dir = ws.mdkg_dir.trim_matches('/');
        if dir.is_empty()
            || dir
                .split('/')
                .any(|c| c == "." || c.is_empty() || c == "..")
        {
            return Err(Error::new(
                "graph.overlapping_workspace",
                format!(
                    "workspace {} declares mdkg_dir {:?}: a graph folder must be a dedicated folder",
                    ws.alias, ws.mdkg_dir
                ),
            ));
        }
        let mut parts: Vec<&str> = ws
            .path
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        if parts.contains(&"..") {
            return Err(Error::new(
                "graph.overlapping_workspace",
                format!("workspace {} leaves the repository", ws.alias),
            ));
        }
        parts.extend(dir.split('/'));
        let joined = parts.join("/");
        if !paths.iter().any(|(_, p)| *p == joined) {
            paths.push((ws.alias.clone(), joined));
        }
    }
    for source in &config.subgraph_sources {
        let joined = format!("{source}/.mdkg");
        if !paths.iter().any(|(_, p)| *p == joined) {
            paths.push((format!("subgraph {source}"), joined));
        }
    }
    for (a, pa) in &paths {
        for (b, pb) in &paths {
            if pa != pb && pb.starts_with(&format!("{pa}/")) {
                return Err(Error::new(
                    "graph.overlapping_workspace",
                    format!("workspace {b}'s graph folder {pb} is inside {a}'s {pa}"),
                ));
            }
        }
    }
    let mut out: Vec<String> = paths.into_iter().map(|(_, p)| p).collect();
    out.sort();
    Ok(out)
}

/// The graph-owned design folders (`<graph folder>/design`), for
/// `publish-design`.
pub fn design_paths(config: &Config) -> Result<Vec<String>, Error> {
    Ok(graph_paths(config)?
        .into_iter()
        .map(|p| format!("{p}/design"))
        .collect())
}
