//! Applying and rolling back a migration, so that a crash at any point
//! leaves the old graph or the new one, never a partially migrated graph.
//!
//! The graph changes in one atomic step: the new `.mdkg` folder is built
//! beside the old one and the two are exchanged with one `rename` call
//! (`RENAME_EXCHANGE` on Linux, `RENAME_SWAP` on macOS). Everything before
//! that step is invisible to readers, and everything after it is
//! bookkeeping that a re-run finishes. A journal in the git common
//! folder names the plan and the steps taken; each step finds out from the
//! world whether it already happened, so it can always be run again:
//!
//! 1. Backups: a `mdkg-pre-0.7` tag at the base commit, a snapshot of the
//!    coordinator store, and a copy of `.mdkg` outside the worktree.
//! 2. Records: the log records, written to the coordination plane.
//! 3. The commit, built from git objects alone (nothing visible yet).
//! 4. The swap: the new `.mdkg` replaces the old one, atomically.
//! 5. HEAD moves to the commit, only if it still names the base.
//! 6. The index is reset to the commit, and the old folder removed.
//!
//! Rollback runs the same steps backwards from the backups.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use mdkg_format::{Code, Digest};
use mdkg_graph::GraphError;
use serde::{Deserialize, Serialize};

use super::gitops::{self, Edit};
use super::plan::{Action, FileChange, Plan};
use crate::coord::{Repo, git_error};

/// The tag the base commit gets (a suffix is added when another migration
/// of this repository took it).
pub const TAG: &str = "mdkg-pre-0.7";

/// How far a migration got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Started,
    BackedUp,
    Recorded,
    Built,
    Swapped,
    Committed,
    Done,
    RollingBack,
    RolledBack,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Started => "started",
            Stage::BackedUp => "backed-up",
            Stage::Recorded => "recorded",
            Stage::Built => "built",
            Stage::Swapped => "swapped",
            Stage::Committed => "committed",
            Stage::Done => "done",
            Stage::RollingBack => "rolling-back",
            Stage::RolledBack => "rolled-back",
        }
    }
}

/// The migration's journal: what it set out to do and how far it got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub plan: String,
    pub base: String,
    /// The migration commit, once built.
    pub commit: Option<String>,
    /// The graph folder being replaced (absolute), and relative to the
    /// repository's top (`.mdkg/`): recovery reads both from here, never
    /// from the graph, whose config may already be 0.7.0's.
    pub graph_dir: PathBuf,
    #[serde(default)]
    pub rel_prefix: String,
    /// Where its replacement is built, beside it (absolute).
    pub staging: PathBuf,
    /// This migration's folder in the git common folder: the plan, the
    /// patch and the `.mdkg` copy.
    pub folder: PathBuf,
    /// The store snapshot, once taken.
    pub snapshot: Option<PathBuf>,
    /// The tag naming the base, and whether this migration created it.
    pub tag: Option<String>,
    pub tag_created: bool,
    /// Whether the patch outside the graph is applied too.
    pub rewrite_refs: bool,
    /// The digest of the old `.mdkg`'s full listing, taken just before the
    /// swap: after it, the old folder must still match, or a write raced.
    #[serde(default)]
    pub inventory: Option<String>,
    pub stage: Stage,
}

fn failure(code: Code, message: impl Into<String>) -> GraphError {
    GraphError::failure(code, message)
}

fn step_failed(what: &str, e: impl std::fmt::Display) -> GraphError {
    failure(Code::IoWriteFailed, format!("{what}: {e}"))
}

/// Syncs a file or folder, tolerating filesystems that can't.
fn sync_path(path: &Path) {
    if let Ok(f) = File::open(path) {
        f.sync_all().ok();
    }
}

/// Replaces the file at `path` with `bytes` only if it holds the content
/// whose digest is `expected`, with no window for an edit to be lost: the
/// new content is written beside it, the two are exchanged in one rename,
/// and the displaced file is checked. If it isn't what was expected (an
/// edit landed in between), the exchange is undone, keeping the edit, and
/// the migration stops (`migrate.dirty`).
fn replace_if(path: &Path, bytes: &[u8], expected: &str) -> Result<(), GraphError> {
    let fail = |e: io::Error| step_failed("replacing a patched file", e);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.migrate-{}", std::process::id()));
    let result = (|| -> Result<(), GraphError> {
        let mut f = File::create(&tmp).map_err(fail)?;
        f.write_all(bytes).map_err(fail)?;
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_file()
        {
            f.set_permissions(meta.permissions()).map_err(fail)?;
        }
        f.sync_all().map_err(fail)?;
        race_hook(path);
        exchange(&tmp, path).map_err(fail)?;
        let displaced = file_digest(&tmp).map_err(fail)?;
        if displaced.as_deref() == Some(expected) {
            return Ok(());
        }
        exchange(&tmp, path).map_err(fail)?;
        Err(failure(
            Code::MigrateDirty,
            format!("{} changed while the migration ran", path.display()),
        )
        .with_hint("undo that change, then run the same command again, or roll back"))
    })();
    fs::remove_file(&tmp).ok();
    result
}

/// The test hook: debug builds write `MDKG_MIGRATE_RACE_TEXT` to the file
/// `MDKG_MIGRATE_RACE_FILE` names just before it's replaced, as an editor
/// saving in that moment would.
fn race_hook(path: &Path) {
    if !cfg!(debug_assertions) {
        return;
    }
    if let (Ok(file), Ok(text)) = (
        std::env::var("MDKG_MIGRATE_RACE_FILE"),
        std::env::var("MDKG_MIGRATE_RACE_TEXT"),
    ) && fs::canonicalize(&file).ok() == fs::canonicalize(path).ok()
    {
        fs::write(path, text).ok();
    }
}

/// Writes `bytes` to `path` atomically: a temporary file beside it, synced,
/// then renamed over it, then the folder synced. A file that existed keeps
/// its permissions (an executable script stays executable).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_file()
        {
            f.set_permissions(meta.permissions())?;
        }
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_path(dir);
        Ok(())
    })();
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

impl Journal {
    pub fn path(folder_root: &Path) -> PathBuf {
        folder_root.join("journal.json")
    }

    pub fn load(folder_root: &Path) -> Result<Option<Self>, GraphError> {
        let path = Self::path(folder_root);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                failure(
                    Code::IoReadFailed,
                    format!(
                        "the migration journal {} can't be read: {e}",
                        path.display()
                    ),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failure(
                Code::IoReadFailed,
                format!(
                    "the migration journal {} can't be read: {e}",
                    path.display()
                ),
            )),
        }
    }

    pub fn save(&self, folder_root: &Path) -> Result<(), GraphError> {
        let bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        write_atomic(&Self::path(folder_root), &bytes)
            .map_err(|e| step_failed("writing the migration journal", e))
    }
}

/// Where this worktree's migrations live: `<git common dir>/mdkg/migrate/<key>`,
/// with the key derived from the worktree's path (one writer per worktree).
pub fn folder_root(repo: &Repo) -> Result<PathBuf, GraphError> {
    let common = mdkg_coord_sqlite::git::common_dir(&repo.top).map_err(git_error)?;
    let key = Digest::of_bytes(format!("{}\0{}", repo.top.display(), repo.prefix).as_bytes());
    Ok(common.join("mdkg").join("migrate").join(&key.hex()[..12]))
}

/// The short form of a plan hash used in folder names.
pub fn short(plan: &str) -> &str {
    let hex = plan.strip_prefix("sha256:").unwrap_or(plan);
    &hex[..hex.len().min(12)]
}

/// Holds the folder's lock for as long as it lives: one migrate at a time
/// per worktree. The OS releases it when the process dies.
pub struct Lock {
    _file: File,
}

pub fn lock(root: &Path) -> Result<Lock, GraphError> {
    fs::create_dir_all(root).map_err(|e| step_failed("creating the migration folder", e))?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("lock"))
        .map_err(|e| step_failed("opening the migration lock", e))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock { _file: file }),
        Err(fs::TryLockError::WouldBlock) => Err(GraphError::conflict(
            Code::MigrateInProgress,
            "another migrate is running in this worktree",
        )
        .with_hint("wait for it to finish")),
        Err(fs::TryLockError::Error(e)) => Err(step_failed("taking the migration lock", e)),
    }
}

/// Copies a folder tree: files (synced), folders and symbolic links (as
/// links); anything else is refused. Written to `dst.partial`, then
/// renamed to `dst`, so a copy that exists is complete.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    publish_copy(src, dst, |_| Ok(()))
}

fn copy_into(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir(dst)?;
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            let target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(not(unix))]
            return Err(io::Error::other(format!(
                "{} is a link, which this platform can't copy",
                from.display()
            )));
        } else if kind.is_dir() {
            copy_into(&from, &to)?;
            sync_path(&to);
        } else if kind.is_file() {
            fs::copy(&from, &to)?;
            File::open(&to)?.sync_all()?;
        } else {
            return Err(io::Error::other(format!(
                "{} is neither a file, a folder nor a link",
                from.display()
            )));
        }
    }
    Ok(())
}

/// The digest of a folder's whole listing: every entry's path and kind,
/// with each file's content digest and permissions and each link's
/// target, so any write, addition or removal inside shows.
pub fn inventory(dir: &Path) -> io::Result<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<u8>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let meta = fs::symlink_metadata(&path)?;
            let line = if meta.file_type().is_symlink() {
                format!("L {rel} {}", fs::read_link(&path)?.display())
            } else if meta.is_dir() {
                format!("D {rel}")
            } else {
                let digest = Digest::of_bytes(&fs::read(&path)?);
                #[cfg(unix)]
                let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
                #[cfg(not(unix))]
                let mode = u32::from(meta.permissions().readonly());
                format!("F {rel} {digest} {mode:o}")
            };
            out.extend_from_slice(line.as_bytes());
            out.push(0);
            if meta.is_dir() {
                walk(base, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(Digest::of_bytes(&out).to_string())
}

/// Builds `dst` as a copy of `src` changed by `edit`, in `dst.partial`
/// first, then renamed: a `dst` that exists is always complete.
fn publish_copy(
    src: &Path,
    dst: &Path,
    edit: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let partial = partial_of(dst);
    for stale in [&partial, dst] {
        if fs::symlink_metadata(stale).is_ok() {
            fs::remove_dir_all(stale)?;
        }
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_into(src, &partial)?;
    edit(&partial)?;
    sync_path(&partial);
    fs::rename(&partial, dst)?;
    if let Some(parent) = dst.parent() {
        sync_path(parent);
    }
    Ok(())
}

/// Removes the staging folder and its partial build, and their root when
/// it's this migration's own (`.mdkg.migrate-<plan>`).
fn remove_staging(journal: &Journal) -> io::Result<()> {
    for leftover in [journal.staging.clone(), partial_of(&journal.staging)] {
        if fs::symlink_metadata(&leftover).is_ok() {
            fs::remove_dir_all(&leftover)?;
        }
    }
    if let Some(root) = journal.staging.parent()
        && root.file_name().is_some_and(|n| {
            n.to_string_lossy() == format!(".mdkg.migrate-{}", short(&journal.plan))
        })
        && fs::symlink_metadata(root).is_ok()
    {
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

/// Where a folder is built before it's published.
pub fn partial_of(dst: &Path) -> PathBuf {
    let mut name = dst
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    dst.with_file_name(name)
}

/// The links inside a folder, by path relative to it. The migration
/// refuses a graph folder holding any: a staged write through a copied
/// link would land outside the staging folder, in the live graph or
/// beyond it.
pub fn links_in(dir: &Path) -> io::Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let path = entry.path();
                out.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            } else if kind.is_dir() {
                walk(base, &entry.path(), out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Exchanges two paths in one step: after it, each names what the other
/// did. Both must exist on the same filesystem.
pub fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE).map_err(io::Error::from)?;
    if let Some(parent) = a.parent() {
        sync_path(parent);
    }
    Ok(())
}

/// Digest of a file, or `None` when it's absent.
fn file_digest(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(Digest::of_bytes(&bytes).to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The file the new `.mdkg` carries from the moment it's staged until the
/// migration is tidied up: where it is says, exactly, whether the swap
/// happened (one rename moves it with its folder).
pub fn marker(plan: &str) -> String {
    format!(".migrate-{}", short(plan))
}

/// Whether the new graph is the one in place: its marker is.
fn swapped(journal: &Journal) -> bool {
    fs::symlink_metadata(journal.graph_dir.join(marker(&journal.plan))).is_ok()
}

/// Applies `changes` (paths relative to the repository's top, all under
/// `rel_prefix`) to the folder `dir`, which stands for `rel_prefix`.
fn apply_files(dir: &Path, rel_prefix: &str, changes: &[&FileChange]) -> io::Result<()> {
    for c in changes {
        let rel = c
            .path
            .strip_prefix(rel_prefix)
            .ok_or_else(|| io::Error::other(format!("{} is outside the graph folder", c.path)))?;
        let target = dir.join(rel);
        match c.action {
            Action::Write => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let content = c.content.as_deref().unwrap_or_default();
                let mut f = File::create(&target)?;
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
            }
            Action::Delete => match fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
    }
    Ok(())
}

/// What the coordinator store's log says about this migration's import
/// (lane 2: every store write appends a log record, and an import leaves
/// its head hash in `meta` as `import:<id>`), read from the store itself,
/// so a crash anywhere can't lose it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportState {
    /// The `log.import` marker's seq, and the head hash the import left.
    marker: u64,
    head_after: String,
    /// The store's head now.
    head_now: Option<(u64, String)>,
}

fn store_error(e: rusqlite::Error) -> GraphError {
    failure(Code::CoordStoreFailed, format!("reading the store: {e}"))
}

fn read_only(path: &Path) -> Result<rusqlite::Connection, GraphError> {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(store_error)
}

fn log_head(conn: &rusqlite::Connection) -> Result<Option<(u64, String)>, GraphError> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT seq, hash FROM log ORDER BY seq DESC LIMIT 1",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
    .map(|o| o.map(|(seq, hash)| (u64::try_from(seq).unwrap_or_default(), hash)))
    .map_err(store_error)
}

/// The import of `import_id` in the store at `path`, if it landed.
fn import_state(path: &Path, import_id: &str) -> Result<Option<ImportState>, GraphError> {
    use rusqlite::OptionalExtension;
    if fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let conn = read_only(path)?;
    // A store with no tables yet (an empty file) holds no import.
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |r| r.get(0),
        )
        .map_err(store_error)?;
    if !has_meta {
        return Ok(None);
    }
    let head_after: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [format!("import:{import_id}")],
            |r| r.get(0),
        )
        .optional()
        .map_err(store_error)?;
    let Some(head_after) = head_after else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare("SELECT seq, record FROM log WHERE record LIKE '%\"log.import\"%' ORDER BY seq")
        .map_err(store_error)?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(store_error)?
        .collect::<Result<_, _>>()
        .map_err(store_error)?;
    let marker = rows.into_iter().find_map(|(seq, text)| {
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        (v["kind"] == "log.import" && v["body"]["import_id"] == import_id)
            .then(|| u64::try_from(seq).unwrap_or_default())
    });
    let Some(marker) = marker else {
        return Err(failure(
            Code::CoordStoreFailed,
            "the store names this migration's import but its log has no marker for it",
        ));
    };
    Ok(Some(ImportState {
        marker,
        head_after,
        head_now: log_head(&conn)?,
    }))
}

/// The kinds of the records in `path`'s log with seq in `range`, counted
/// (`lease.claim ×2, msg.send`), for a refusal to name what it would lose.
fn kinds_in(path: &Path, from: u64, to: u64) -> Result<String, GraphError> {
    let conn = read_only(path)?;
    let mut stmt = conn
        .prepare("SELECT record FROM log WHERE seq >= ?1 AND seq <= ?2 ORDER BY seq")
        .map_err(store_error)?;
    let lo = i64::try_from(from).unwrap_or(i64::MAX);
    let hi = i64::try_from(to).unwrap_or(i64::MAX);
    let texts: Vec<String> = stmt
        .query_map([lo, hi], |r| r.get(0))
        .map_err(store_error)?
        .collect::<Result<_, _>>()
        .map_err(store_error)?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in texts {
        let kind = serde_json::from_str::<serde_json::Value>(&t)
            .ok()
            .and_then(|v| v["kind"].as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        *counts.entry(kind).or_default() += 1;
    }
    Ok(counts
        .into_iter()
        .map(|(k, n)| if n > 1 { format!("{k} ×{n}") } else { k })
        .collect::<Vec<_>>()
        .join(", "))
}

/// Whether rollback restores the store, and to which head: `None` when
/// the import never landed (or was already rolled back), so there's
/// nothing to take out; `Some(head)` when the store is exactly the snapshot
/// plus the import, restored only while its head is still `head`. Any
/// other write since the snapshot, before or after the import, refuses
/// the rollback (`--force` restores anyway: `Some("")`).
fn store_restore(
    store: &Path,
    snapshot: Option<&Path>,
    import_id: &str,
    force: bool,
) -> Result<Option<String>, GraphError> {
    let Some(state) = import_state(store, import_id)? else {
        return Ok(None);
    };
    let Some(snapshot) = snapshot else {
        return Err(failure(
            Code::MigrateStepFailed,
            "the store holds this migration's import, but there's no snapshot to restore",
        ));
    };
    if force {
        return Ok(Some(String::new()));
    }
    let snap_head = log_head(&read_only(snapshot)?)?.map_or(0, |(seq, _)| seq);
    let end = state.head_now.as_ref().map_or(0, |(seq, _)| *seq);
    let import_end = state.marker + {
        // The import's own records: from its marker to the head it left.
        let conn = read_only(store)?;
        let seq: Option<i64> = rusqlite::OptionalExtension::optional(conn.query_row(
            "SELECT seq FROM log WHERE hash = ?1",
            [&state.head_after],
            |r| r.get(0),
        ))
        .map_err(store_error)?;
        seq.and_then(|s| u64::try_from(s).ok())
            .map_or(0, |s| s.saturating_sub(state.marker))
    };
    let before = state.marker.saturating_sub(snap_head + 1);
    let after = end.saturating_sub(import_end);
    let head_ok = state.head_now.as_ref().map(|(_, h)| h) == Some(&state.head_after);
    if before == 0 && after == 0 && head_ok {
        return Ok(Some(state.head_after));
    }
    let mut lost = Vec::new();
    if before > 0 {
        lost.push(kinds_in(store, snap_head + 1, state.marker - 1)?);
    }
    if after > 0 {
        lost.push(kinds_in(store, import_end + 1, end)?);
    }
    Err(GraphError::conflict(
        Code::MigrateStoreChanged,
        format!(
            "the coordinator store has writes besides the migration's since its snapshot: {}",
            lost.join(", ")
        ),
    )
    .with_hint(
        "rolling back restores the store as it was before the migration, losing those \
         writes: stop every agent and pass --force to do it anyway",
    ))
}

/// Snapshots the coordinator store beside itself, with lane 2's
/// `snapshot`, so `store snapshots` lists the copy and `store restore`
/// takes it.
pub fn snapshot_store(store: &Path, plan: &str) -> Result<PathBuf, GraphError> {
    mdkg_coord_sqlite::snapshot(store, &format!("migrate-{}", short(plan)))
        .map_err(|e| step_failed("snapshotting the coordinator store", e))
}

/// Where the log records go. The coordination plane's log (lane 2) takes
/// them in one transaction, keyed by the plan, so writing twice is a no-op.
pub trait RecordSink {
    fn write(&mut self, plan: &Plan) -> Result<(), GraphError>;
}

/// Imports the records into the coordinator store's log (lane 2's
/// `import_records`): one transaction, keyed by the plan hash, so a
/// second write after a crash appends nothing.
pub struct StoreImport<'a> {
    pub repo: &'a Repo,
}

impl RecordSink for StoreImport<'_> {
    fn write(&mut self, plan: &Plan) -> Result<(), GraphError> {
        // Nothing to import: the store isn't touched (and has no snapshot
        // to roll back to).
        if plan.records.is_empty() {
            return Ok(());
        }
        // A record another migration of this repository already imported
        // (main's, when an unmerged branch migrates after it) is skipped by
        // its migrate_digest, which the store looks up inside the import's
        // own transaction; the import marker counts what was skipped. After
        // a rollback of that migration, the records are imported afresh.
        let records: Vec<mdkg_coord_sqlite::log::Imported> = plan
            .records
            .iter()
            .map(|r| mdkg_coord_sqlite::log::Imported {
                ts_ms: super::records::ts_ms(&r.ts),
                actor: r.actor.clone(),
                kind: serde_json::to_value(r.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                subject: r.subject.clone(),
                body: {
                    let mut m = match &r.body {
                        serde_json::Value::Object(m) => m.clone(),
                        other => {
                            let mut m = serde_json::Map::new();
                            m.insert("value".into(), other.clone());
                            m
                        }
                    };
                    // What the summary lines left in nodes cite: the log's own
                    // hashes (over seq and prev) aren't known at plan time.
                    m.insert("migrate_digest".into(), r.digest.clone().into());
                    m
                },
            })
            .collect();
        let mut store = self.repo.store()?;
        store
            .import_records(&plan.hash(), &records)
            .map_err(|e| step_failed("importing the log records", e))?;
        Ok(())
    }
}

/// The world a migration acts on.
pub struct Ctx<'a> {
    pub repo: &'a Repo,
    pub plan: &'a Plan,
    /// The graph folder relative to the repository's top (`.mdkg/`).
    pub rel_prefix: String,
    pub store: PathBuf,
    pub sink: &'a mut dyn RecordSink,
    /// Called after each stage is saved; a test stops the run here.
    pub after_stage: &'a mut dyn FnMut(Stage) -> Result<(), GraphError>,
}

fn moved_on(head: &str) -> GraphError {
    GraphError::conflict(
        Code::MigrateMovedOn,
        format!("HEAD is at {head}, neither the migration's base nor its commit"),
    )
    .with_hint("a commit landed in between; reset the branch to one of them, or roll back")
}

/// Runs the migration's remaining steps, from wherever its journal says it
/// got to. Every step checks whether it already happened.
pub fn advance(
    ctx: &mut Ctx<'_>,
    folder_root: &Path,
    journal: &mut Journal,
) -> Result<(), GraphError> {
    let top = ctx.repo.top.clone();
    let plan = ctx.plan;
    let save =
        |j: &mut Journal, stage: Stage, after: &mut dyn FnMut(Stage) -> Result<(), GraphError>| {
            j.stage = stage;
            j.save(folder_root)?;
            after(stage)
        };
    if journal.stage == Stage::Started {
        // 1. Backups.
        if journal.tag.is_none() {
            let (name, created) = take_tag(&top, &journal.base)?;
            journal.tag = Some(name);
            journal.tag_created = created;
            journal.save(folder_root)?;
        }
        if journal.snapshot.is_none() && fs::symlink_metadata(&ctx.store).is_ok() {
            journal.snapshot = Some(snapshot_store(&ctx.store, &journal.plan)?);
            journal.save(folder_root)?;
        }
        let copy = journal.folder.join("mdkg");
        if !copy.exists() {
            copy_tree(&journal.graph_dir, &copy).map_err(|e| step_failed("copying .mdkg", e))?;
        }
        save(journal, Stage::BackedUp, ctx.after_stage)?;
    }
    if journal.stage == Stage::BackedUp {
        // 2. Records: imported once (by the plan hash), whatever a crash
        // interrupted.
        ctx.sink.write(plan)?;
        save(journal, Stage::Recorded, ctx.after_stage)?;
    }
    if journal.stage == Stage::Recorded {
        // 3. The commit, from objects only.
        if journal.commit.is_none() {
            let edits: Vec<(String, Edit<'_>)> = plan
                .files
                .iter()
                .chain(plan.patch.iter().filter(|_| journal.rewrite_refs))
                .map(|c| {
                    let edit = match c.action {
                        Action::Write => {
                            Edit::Write(c.content.as_deref().unwrap_or_default().as_bytes())
                        }
                        Action::Delete => Edit::Delete,
                    };
                    (c.path.clone(), edit)
                })
                .collect();
            let tree = gitops::edited_tree(&top, &journal.base, &edits).map_err(git_error)?;
            let message = commit_message(plan, journal.rewrite_refs);
            let commit =
                gitops::commit_tree(&top, &tree, &journal.base, &message).map_err(git_error)?;
            journal.commit = Some(commit);
            journal.save(folder_root)?;
        }
        save(journal, Stage::Built, ctx.after_stage)?;
    }
    if journal.stage == Stage::Built {
        // 4. The swap.
        if swapped(journal) {
            // Swapped before the journal said so: the old folder is checked
            // as it would have been, and the new graph validated.
            raced(journal)?;
            validate_in_place(&ctx.repo.root, journal)?;
        } else {
            let commit = journal.commit.clone().unwrap_or_default();
            // The graph's changes, and with --rewrite-refs the patch's files
            // inside the graph folder too (the rest come after the swap).
            let changes: Vec<&FileChange> = plan
                .files
                .iter()
                .chain(
                    plan.patch
                        .iter()
                        .filter(|c| journal.rewrite_refs && c.path.starts_with(&ctx.rel_prefix)),
                )
                .collect();
            // Listed first, then checked against the base: an edit before
            // the listing fails the check, and one after it fails the
            // comparison after the swap.
            let before =
                inventory(&journal.graph_dir).map_err(|e| step_failed("reading .mdkg", e))?;
            unchanged_since_base(&top, &ctx.rel_prefix, &journal.graph_dir, &changes)?;
            let mark = marker(&journal.plan);
            publish_copy(&journal.graph_dir, &journal.staging, |staged| {
                apply_files(staged, &ctx.rel_prefix, &changes)?;
                let mut f = File::create(staged.join(&mark))?;
                f.write_all(journal.plan.as_bytes())?;
                f.sync_all()
            })
            .map_err(|e| step_failed("building the new .mdkg", e))?;
            // The staged folder must hold exactly the commit's files.
            let wrong = tree_mismatches(&top, &commit, &ctx.rel_prefix, &journal.staging, &mark)?;
            if !wrong.is_empty() {
                return Err(failure(
                    Code::MigrateStepFailed,
                    format!(
                        "the new .mdkg doesn't match the commit: {}",
                        wrong.join(", ")
                    ),
                ));
            }
            journal.inventory = Some(before);
            journal.save(folder_root)?;
            exchange(&journal.graph_dir, &journal.staging)
                .map_err(|e| step_failed("swapping .mdkg in", e))?;
            raced(journal)?;
            validate_in_place(&ctx.repo.root, journal)?;
        }
        save(journal, Stage::Swapped, ctx.after_stage)?;
    }
    if journal.stage == Stage::Swapped {
        // 5. The patch outside the graph, then HEAD.
        if journal.rewrite_refs {
            for c in plan
                .patch
                .iter()
                .filter(|c| !c.path.starts_with(&ctx.rel_prefix))
            {
                let path = top.join(&c.path);
                let now =
                    file_digest(&path).map_err(|e| step_failed("reading a patched file", e))?;
                // Written only over the content the plan read; already
                // written is fine; anything else is someone's edit.
                if now == c.after {
                    continue;
                }
                if now.as_deref() != Some(c.before.as_str()) {
                    return Err(failure(
                        Code::MigrateDirty,
                        format!("{} changed while the migration ran", c.path),
                    )
                    .with_hint("undo that change, then run the same --apply again, or roll back"));
                }
                replace_if(
                    &path,
                    c.content.as_deref().unwrap_or_default().as_bytes(),
                    &c.before,
                )?;
            }
        }
        let commit = journal.commit.clone().unwrap_or_default();
        let head = gitops::head(&top).map_err(git_error)?;
        if head == journal.base {
            gitops::move_head(&top, &commit, &journal.base, "mdkg migrate").map_err(git_error)?;
        } else if head != commit {
            return Err(moved_on(&head));
        }
        save(journal, Stage::Committed, ctx.after_stage)?;
    }
    if journal.stage == Stage::Committed {
        // 6. Tidy up.
        let commit = journal.commit.clone().unwrap_or_default();
        // The test hook: debug builds stage someone else's submodule pointer
        // just before the reset, as a concurrent `git add` might.
        if cfg!(debug_assertions)
            && let Ok(entry) = std::env::var("MDKG_MIGRATE_RACE_INDEX")
        {
            gitops::run(
                &top,
                &["update-index", "--add", "--cacheinfo", &entry],
                None,
            )
            .map_err(git_error)?;
        }
        // Or leave a path unmerged (stages 1 and 2), as a conflicted
        // `git apply --cached --3way` would.
        if cfg!(debug_assertions)
            && let Ok(path) = std::env::var("MDKG_MIGRATE_RACE_UNMERGED")
        {
            let oid = String::from_utf8_lossy(
                &gitops::run(&top, &["rev-parse", &format!(":{path}")], None).map_err(git_error)?,
            )
            .trim()
            .to_string();
            gitops::run(&top, &["update-index", "--force-remove", &path], None)
                .map_err(git_error)?;
            let info = format!("100644 {oid} 1\t{path}\n100644 {oid} 2\t{path}\n");
            gitops::run(
                &top,
                &["update-index", "--index-info"],
                Some(info.as_bytes()),
            )
            .map_err(git_error)?;
        }
        gitops::reset_index(&top, &journal.base, &commit).map_err(git_error)?;
        remove_staging(journal).map_err(|e| step_failed("removing the old .mdkg", e))?;
        // Last: until here the marker tells a resume the swap happened (and
        // HEAD names the commit from here on).
        match fs::remove_file(journal.graph_dir.join(marker(&journal.plan))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(step_failed("removing the swap marker", e)),
        }
        save(journal, Stage::Done, ctx.after_stage)?;
    }
    Ok(())
}

/// Validates the new graph where it now is, with the whole validator, as
/// `validate` runs it (lane 1's 0.7.0 reader, its templates, skills and
/// config as they really are), before HEAD moves. A graph that fails is
/// swapped back out, in one rename, so readers see the old graph again,
/// and the migration stops.
fn validate_in_place(root: &Path, journal: &Journal) -> Result<(), GraphError> {
    // The test hooks: debug builds break the new graph's config here, as a
    // planner bug would, or save a 0.6.0 node into it, as an editor might.
    if cfg!(debug_assertions) {
        if std::env::var_os("MDKG_MIGRATE_BREAK_STAGED").is_some() {
            fs::write(journal.graph_dir.join("config.json"), "not json")
                .map_err(|e| step_failed("the test hook", e))?;
        }
        if let Ok(text) = std::env::var("MDKG_MIGRATE_RACE_GRAPH") {
            fs::write(journal.graph_dir.join("work/task-90-raced.md"), text)
                .map_err(|e| step_failed("the test hook", e))?;
        }
    }
    let problem = match mdkg_graph::config::load(root)
        .and_then(|cfg| mdkg_graph::validate::validate(root, &cfg))
    {
        Ok(receipt) => receipt.errors.into_iter().next(),
        Err(e) => Some(*e.diagnostic),
    };
    let Some(d) = problem else {
        return Ok(());
    };
    let mut kept = String::new();
    if journal.staging.exists() {
        exchange(&journal.graph_dir, &journal.staging)
            .map_err(|e| step_failed("swapping the old .mdkg back", e))?;
        // The rejected graph, now at the staging path, would be deleted by
        // a retry or a rollback, with any edit saved into it while it was
        // in place (ignored files too): it's always kept, out of their way.
        let dest = journal.folder.join(format!(
            "kept-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        ));
        move_dir(&journal.staging, &dest)
            .map_err(|e| step_failed("keeping the rejected .mdkg", e))?;
        kept = format!(
            "; the rejected graph, with anything saved into it while it was in place, is kept in {}",
            dest.display()
        );
    }
    let at = d
        .path
        .as_ref()
        .map(|p| format!(" ({p})"))
        .unwrap_or_default();
    Err(failure(
        Code::MigrateStepFailed,
        format!(
            "the migrated graph doesn't validate: {} {}{at}{kept}",
            d.code.as_str(),
            d.message
        ),
    )
    .with_hint(
        "the old graph is back in place: roll back with `migrate --rollback`, and report the plan",
    ))
}

/// Moves a folder: one rename when it can, else a whole copy, then the
/// original removed.
fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_tree(from, to)?;
            fs::remove_dir_all(from)
        }
    }
}

/// The graph folder must be exactly the base before it's copied: nothing
/// under it that `git status` sees (staged, unstaged or untracked), and
/// every file the plan changes still as the plan read it.
fn unchanged_since_base(
    top: &Path,
    rel_prefix: &str,
    graph_dir: &Path,
    changes: &[&FileChange],
) -> Result<(), GraphError> {
    let mut changed = gitops::dirty_under(top, Some(rel_prefix)).map_err(git_error)?;
    for c in changes {
        let now = file_digest(&graph_dir.join(&c.path[rel_prefix.len()..]))
            .map_err(|e| step_failed("reading .mdkg", e))?;
        if now.as_deref() != Some(c.before.as_str()) {
            changed.push(c.path.clone());
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    changed.sort();
    changed.dedup();
    let shown: Vec<&str> = changed.iter().take(5).map(String::as_str).collect();
    Err(failure(
        Code::MigrateDirty,
        format!(
            "the graph folder changed since the migration started: {}",
            shown.join(", ")
        ),
    )
    .with_hint("commit or undo that change, then plan again, or roll back"))
}

/// Whether a rollback swaps the backup copy in, refusing one that would
/// discard work (`--force` skips the refusal). Judged from the world on
/// every run, a resumed one too:
/// - the graph is exactly the base's files: nothing to swap;
/// - it's exactly the migration commit's: swap the copy in;
/// - otherwise it was edited. If the new graph is in place (its marker is,
///   or HEAD names the commit), that's work done since the migration:
///   refused. Else the swap never happened, and the live graph, edits and
///   all, stays.
fn restore_needed(
    top: &Path,
    journal: &Journal,
    rel_prefix: &str,
    force: bool,
) -> Result<bool, GraphError> {
    let mark = marker(&journal.plan);
    let is = |commit: &str| -> Result<Vec<String>, GraphError> {
        tree_mismatches(top, commit, rel_prefix, &journal.graph_dir, &mark)
    };
    if is(&journal.base)?.is_empty() {
        return Ok(false);
    }
    let Some(commit) = journal.commit.as_deref() else {
        return Ok(false);
    };
    let wrong = is(commit)?;
    if wrong.is_empty() {
        return Ok(true);
    }
    let head = gitops::head(top).map_err(git_error)?;
    if !(swapped(journal) || head == commit) {
        return Ok(false);
    }
    if force {
        return Ok(true);
    }
    let shown: Vec<&str> = wrong.iter().take(5).map(String::as_str).collect();
    Err(failure(
        Code::MigrateDirty,
        format!(
            "the migrated graph changed since the migration: {}; rolling back would discard it",
            shown.join(", ")
        ),
    )
    .with_hint("commit or undo it first, or pass --force to roll back anyway"))
}

/// After the swap, the old folder (now at the staging path) must be exactly
/// as it was listed before: a write, an addition or a removal that raced
/// the swap would otherwise be lost with it. If anything changed, the swap
/// is undone and the migration stops.
fn raced(journal: &Journal) -> Result<(), GraphError> {
    let Some(before) = &journal.inventory else {
        return Err(failure(
            Code::MigrateStepFailed,
            "the journal has no listing of the old .mdkg to check the swap against",
        ));
    };
    if !journal.staging.exists() {
        // Tidied up already (a stage later than this one).
        return Ok(());
    }
    let after = inventory(&journal.staging).map_err(|e| step_failed("reading the old .mdkg", e))?;
    if after == *before {
        return Ok(());
    }
    exchange(&journal.graph_dir, &journal.staging)
        .map_err(|e| step_failed("swapping .mdkg back after a racing write", e))?;
    Err(failure(
        Code::MigrateDirty,
        ".mdkg changed while the migration ran (one writer per worktree)",
    )
    .with_hint("commit or undo that change, then run the same --apply again"))
}

/// Refuses when git's index holds anything but the base's tree or the
/// migration commit's: staged work (perhaps only in the index) that
/// resetting the index would discard.
fn index_is_ours(top: &Path, journal: &Journal) -> Result<(), GraphError> {
    if gitops::unmerged(top).map_err(git_error)? {
        return Err(failure(
            Code::MigrateDirty,
            "git's index has unmerged entries (a conflict someone is resolving)",
        )
        .with_hint("resolve or abort that conflict, then run the command again"));
    }
    let index = gitops::index_tree(top).map_err(git_error)?;
    let mut ours = vec![gitops::tree_of(top, &journal.base).map_err(git_error)?];
    if let Some(commit) = &journal.commit {
        ours.push(gitops::tree_of(top, commit).map_err(git_error)?);
    }
    if ours.contains(&index) {
        return Ok(());
    }
    Err(failure(
        Code::MigrateDirty,
        "git's index holds staged changes that are neither the base's nor the migration's",
    )
    .with_hint("commit or unstage them (git restore --staged), then run the command again"))
}

/// The paths where `dir` (standing for `rel_prefix`) and `commit`'s tree
/// disagree, judged from the files on disk, not git's index: a file the
/// commit has that's missing or different, or a file it doesn't have
/// (staged or not). Ignored files and the swap marker don't count.
fn tree_mismatches(
    top: &Path,
    commit: &str,
    rel_prefix: &str,
    dir: &Path,
    marker: &str,
) -> Result<Vec<String>, GraphError> {
    let entries = gitops::ls_tree(top, commit).map_err(git_error)?;
    let wanted: BTreeMap<String, &gitops::Entry> = entries
        .iter()
        .filter(|(p, e)| e.kind == "blob" && p.starts_with(rel_prefix))
        .map(|(p, e)| (p.clone(), e))
        .collect();
    let present: Vec<String> = files_in(dir)
        .map_err(|e| step_failed("reading .mdkg", e))?
        .into_iter()
        .filter(|rel| rel != marker)
        .map(|rel| format!("{rel_prefix}{rel}"))
        .collect();
    let ignored = gitops::ignored(top, &present).map_err(git_error)?;
    let present: Vec<String> = present
        .into_iter()
        // An ignored path the tree tracks is still compared.
        .filter(|p| !ignored.contains(p) || wanted.contains_key(p))
        .collect();
    let mut wrong: Vec<String> = wanted
        .keys()
        .filter(|p| !present.contains(p))
        .cloned()
        .collect();
    let mut known = Vec::new();
    for p in present {
        match wanted.get(&p) {
            Some(e) => known.push((p, e.oid.clone())),
            None => wrong.push(p),
        }
    }
    let paths: Vec<String> = known
        .iter()
        .map(|(p, _)| {
            dir.join(&p[rel_prefix.len()..])
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let oids = gitops::hash_paths(top, &paths).map_err(git_error)?;
    for ((p, want), oid) in known.into_iter().zip(oids) {
        if want != oid {
            wrong.push(p);
        }
    }
    wrong.sort();
    Ok(wrong)
}

/// Every file under `dir` (links count as files), by path relative to it.
fn files_in(dir: &Path) -> io::Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(base, &path, out)?;
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Tags the base `mdkg-pre-0.7`, or `mdkg-pre-0.7-<base>` when another
/// migration of this repository holds that name. Returns the tag and
/// whether this call created it.
fn take_tag(top: &Path, base: &str) -> Result<(String, bool), GraphError> {
    let exists = |name: &str| mdkg_coord_sqlite::git::has_ref(top, &format!("refs/tags/{name}"));
    for name in [
        TAG.to_string(),
        format!("{TAG}-{}", &base[..base.len().min(12)]),
    ] {
        let existed = exists(&name);
        if gitops::tag(top, &name, base).map_err(git_error)? {
            return Ok((name, !existed));
        }
    }
    Err(failure(
        Code::MigrateStepFailed,
        "the tags mdkg-pre-0.7 and its per-commit form both name other commits",
    ))
}

fn commit_message(plan: &Plan, rewrite_refs: bool) -> String {
    let stays = plan.nodes.iter().filter(|n| n.id.is_some()).count();
    let left = plan.nodes.iter().filter(|n| n.record.is_some()).count();
    let mut m = format!(
        "mdkg: migrate the graph to 0.7.0 (plan {})\n\n\
         {stays} nodes get UUIDv7 ids with their legacy_id; {left} checkpoints and \
         finished tests become log records; {} records in all. Rollback: \
         `mdkg migrate --rollback`.\n",
        plan.hash(),
        plan.records.len()
    );
    if rewrite_refs && !plan.patch.is_empty() {
        m.push_str(&format!(
            "\nAlso rewrites ids and 0.6.0 commands in {} instruction file(s) (--rewrite-refs).\n",
            plan.patch.len()
        ));
    }
    m
}

/// Rolls a migration back, from any stage: the old `.mdkg` back in place,
/// HEAD back to the base, the patch undone, the store restored.
pub fn roll_back(
    ctx: &mut Ctx<'_>,
    folder_root: &Path,
    journal: &mut Journal,
    force: bool,
) -> Result<(), GraphError> {
    let top = ctx.repo.top.clone();
    let plan = ctx.plan;
    if journal.stage == Stage::RolledBack {
        return Err(GraphError::conflict(
            Code::MigrateNotMigrated,
            "this migration was already rolled back",
        ));
    }
    // The store first, on every run (a resumed one too): refused, it
    // changes nothing.
    let restore_to = store_restore(
        &ctx.store,
        journal.snapshot.as_deref(),
        &journal.plan,
        force,
    )?;
    if journal.stage != Stage::RollingBack {
        // A finished migration's worktree must be as it left it.
        if journal.stage == Stage::Done {
            let dirty = gitops::dirty(&top).map_err(git_error)?;
            if !dirty.is_empty() {
                return Err(failure(
                    Code::MigrateDirty,
                    format!("the worktree has changes: {}", dirty.join(", ")),
                )
                .with_hint("commit or undo them first"));
            }
        }
        let head = gitops::head(&top).map_err(git_error)?;
        if head != journal.base && Some(&head) != journal.commit.as_ref() {
            return Err(moved_on(&head));
        }
        // Checked before RollingBack is saved, so a refusal leaves the run
        // where it was (an apply can still finish it).
        if !force {
            index_is_ours(&top, journal)?;
        }
        journal.stage = Stage::RollingBack;
        journal.save(folder_root)?;
        (ctx.after_stage)(Stage::RollingBack)?;
    } else if !force {
        index_is_ours(&top, journal)?;
    }
    // The old folder back in place, only where the new one was swapped in,
    // and never over work done since (decided afresh on every run). The
    // backup copy goes back exactly: a folder at the staging path is used
    // only when it lists just as the copy does (the old folder, left there
    // by a crash after the swap); else the copy is copied in afresh.
    if restore_needed(&top, journal, &ctx.rel_prefix, force)? {
        let copy = journal.folder.join("mdkg");
        if !copy.exists() {
            return Err(failure(
                Code::MigrateStepFailed,
                format!("the backup copy {} is missing", copy.display()),
            ));
        }
        let wanted = inventory(&copy).map_err(|e| step_failed("reading the backup copy", e))?;
        let staged = journal.staging.exists()
            && inventory(&journal.staging)
                .map_err(|e| step_failed("reading the staging folder", e))?
                == wanted;
        if !staged {
            publish_copy(&copy, &journal.staging, |_| Ok(()))
                .map_err(|e| step_failed("restoring .mdkg", e))?;
        }
        exchange(&journal.graph_dir, &journal.staging)
            .map_err(|e| step_failed("swapping .mdkg back", e))?;
    }
    // The patch undone (its files inside the graph folder came back with it).
    for c in plan
        .patch
        .iter()
        .filter(|c| !c.path.starts_with(&ctx.rel_prefix))
    {
        let path = top.join(&c.path);
        let now = file_digest(&path).map_err(|e| step_failed("reading a patched file", e))?;
        if now == c.after && now.as_ref() != Some(&c.before) {
            let old = gitops::run(
                &top,
                &["show", &format!("{}:{}", journal.base, c.path)],
                None,
            )
            .map_err(git_error)?;
            if let Some(after) = &c.after {
                replace_if(&path, &old, after)?;
            }
        }
    }
    // HEAD back.
    let head = gitops::head(&top).map_err(git_error)?;
    if let Some(commit) = journal.commit.as_deref()
        && head == commit
    {
        gitops::move_head(&top, &journal.base, commit, "mdkg migrate --rollback")
            .map_err(git_error)?;
    }
    if let Some(commit) = journal.commit.as_deref() {
        gitops::reset_index(&top, commit, &journal.base).map_err(git_error)?;
    }
    // The store as it was, if the import reached it.
    // Under the store's exclusive lock, lane 2's restore_if_head checks the
    // log still ends at the import's last record, so a write that landed
    // since the check above is refused, not lost (--force restores anyway).
    if let (Some(head), Some(snapshot)) = (&restore_to, &journal.snapshot) {
        // The test hook: debug builds have another writer append to the
        // store here, between the check and the restore.
        if cfg!(debug_assertions) && std::env::var_os("MDKG_MIGRATE_RACE_STORE").is_some() {
            ctx.repo
                .store()?
                .import_records("race-test", &[])
                .map_err(|e| step_failed("the test hook", e))?;
        }
        let restored = if head.is_empty() {
            mdkg_coord_sqlite::restore(&ctx.store, snapshot)
        } else {
            mdkg_coord_sqlite::restore_if_head(&ctx.store, snapshot, head)
        };
        restored.map_err(|e| match e {
            mdkg_coord_sqlite::StoreError::Restore(why) if why.contains("head") => {
                GraphError::conflict(
                    Code::MigrateStoreChanged,
                    format!("the coordinator store changed during the rollback: {why}"),
                )
                .with_hint(
                    "run --rollback again to see what changed, or stop every agent and pass --force",
                )
            }
            other => step_failed("restoring the coordinator store", other),
        })?;
    }
    remove_staging(journal).map_err(|e| step_failed("removing the migrated .mdkg", e))?;
    journal.stage = Stage::RolledBack;
    journal.save(folder_root)?;
    (ctx.after_stage)(Stage::RolledBack)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-migrate-apply-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("temp");
        dir
    }

    fn journal(graph_dir: PathBuf, staging: PathBuf, inventory: Option<String>) -> Journal {
        Journal {
            plan: "sha256:00".into(),
            base: "b".into(),
            commit: None,
            graph_dir,
            rel_prefix: ".mdkg/".into(),
            staging,
            folder: PathBuf::new(),
            snapshot: None,
            tag: None,
            tag_created: false,
            rewrite_refs: false,
            inventory,
            stage: Stage::Built,
        }
    }

    #[test]
    fn a_write_racing_the_swap_swaps_back() {
        let dir = temp("race");
        let (graph, staging) = (dir.join(".mdkg"), dir.join(".mdkg.migrate-x"));
        fs::create_dir_all(graph.join("work")).expect("graph");
        fs::write(graph.join("work/task-1.md"), "old").expect("old");
        fs::create_dir_all(&staging).expect("staging");
        fs::write(staging.join("new.md"), "new").expect("new");
        let before = inventory(&graph).expect("inventory");
        exchange(&graph, &staging).expect("swap");
        // Unchanged: fine.
        let j = journal(graph.clone(), staging.clone(), Some(before.clone()));
        assert!(raced(&j).is_ok());
        // A new file written into the old folder after the listing.
        fs::write(staging.join("work/task-3.md"), "racing").expect("race");
        let e = raced(&j).expect_err("raced");
        assert_eq!(e.code(), Code::MigrateDirty);
        // Swapped back: the racing write is in the live folder again.
        assert!(graph.join("work/task-3.md").exists());
        assert!(staging.join("new.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listings_see_content_permissions_and_links() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("inventory");
        fs::write(dir.join("a"), "x").expect("a");
        let one = inventory(&dir).expect("one");
        fs::set_permissions(dir.join("a"), fs::Permissions::from_mode(0o755)).expect("chmod");
        let two = inventory(&dir).expect("two");
        assert_ne!(one, two);
        std::os::unix::fs::symlink("a", dir.join("l")).expect("link");
        assert_ne!(inventory(&dir).expect("three"), two);
        assert_eq!(links_in(&dir).expect("links"), ["l"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_writes_keep_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("perm");
        let f = dir.join("s.sh");
        fs::write(&f, "a").expect("write");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o751)).expect("chmod");
        write_atomic(&f, b"b").expect("atomic");
        assert_eq!(fs::read(&f).expect("read"), b"b");
        assert_eq!(
            fs::metadata(&f).expect("meta").permissions().mode() & 0o777,
            0o751
        );
        fs::remove_dir_all(&dir).ok();
    }
}
