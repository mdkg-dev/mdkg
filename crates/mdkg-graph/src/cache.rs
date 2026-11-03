//! The graph cache: an mdkg-private, read-only copy of the graph tip's
//! graph folders at `<common>/mdkg/graph/cache/`, with a manifest beside it
//! (`cache.json`: the commit, and per path the oid, mode, size, mtime,
//! ctime and inode). edd-01a18867, "The graph cache".
//!
//! - **Derived.** Nothing in it is ever the only copy of anything. Outside
//!   a write session, a file that differs from the manifest is a hand edit:
//!   it's copied to `<common>/mdkg/graph/rescued/<utc>/<path>`, synced,
//!   before anything replaces it.
//! - **Read-only.** Outside a write session files are 0444 and folders
//!   0555. Readers verify stat, ctime included: nothing resets ctime, so a
//!   file rewritten with its mtime restored still shows. A suspect file is
//!   hashed again.
//! - **Write sessions** (under the exclusive graph lock) record every
//!   operation write in `session.json` before making it: the request id,
//!   op and actor, each target path and each temporary file.
//!   [`Session::write`] and [`Session::delete`] check the target's
//!   preimage, then replace it by rename. Anything the record doesn't
//!   account for is never guessed at: it's kept, non-importable, as
//!   *unclassified* (orchestrator, on Terra #6375), and restored.
//! - **Refused or failed ops** restore their own writes with no rescue
//!   ([`Session::abort`]). After a crash, the next refresh keeps the
//!   session's differing files under `rescued/<request-id>/`, marked
//!   unfinished, and its unaccounted drift as unclassified; neither is a
//!   hand edit, and `graph import` refuses both ([`kept`]).
//! - **Confined.** Every path is checked to be relative, with no `..`,
//!   and every folder on the way to it is checked to be a real folder,
//!   never a link; files are opened with `O_NOFOLLOW`. A link or a folder
//!   where a file belongs is never followed or emptied: its contents are
//!   kept first. (A swap racing between the check and the open is the
//!   stated residual of one trust domain.)
//! - **Bounded.** A file is at most 64 MiB, and a refresh writes at most
//!   1 GiB, checked from the tree's sizes before anything is replaced.
//!   Nothing is ever truncated.
//! - Authority reads never use the cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::refwrite::{self, Change, GraphLock};

/// The largest file the cache reads, writes or keeps.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// The most a refresh writes.
pub const MAX_REFRESH_BYTES: u64 = 1024 * 1024 * 1024;
/// The largest session record.
pub const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;

/// One cached file's manifest record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub oid: String,
    /// The git mode: `100644` or `100755`.
    pub mode: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub ino: u64,
}

/// The cache's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// The commit whose tree the cache holds.
    pub commit: String,
    /// The graph-owned folders it holds.
    pub paths: Vec<String>,
    pub files: BTreeMap<String, Stat>,
}

/// How a cache file differs from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    Modified,
    Added,
    Deleted,
    /// Not a regular file: a link, a FIFO, a device.
    NotAFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// Repository-relative.
    pub path: String,
    pub kind: DriftKind,
}

/// What a refresh did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Refreshed {
    /// Files that differed from the manifest.
    pub drift: Vec<Drift>,
    /// Hand edits, rescued (`graph.cache_edit`), when there were any.
    pub rescued: Option<PathBuf>,
    /// Incomplete rescues a crash left whose files aren't all proven
    /// redundant, kept as visible `orphan-…` folders (orchestrator #8066).
    pub orphaned: Vec<PathBuf>,
    /// Files written or removed to reach the commit.
    pub updated: usize,
    /// A session that didn't finish (a crash). The caller reports
    /// `graph.session_discarded` unless the replay before this refresh
    /// published that request.
    pub session: Option<Ended>,
}

/// A write session's record, `<common>/mdkg/graph/session.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The request id.
    pub id: String,
    pub op: String,
    pub actor: String,
    /// Every target path, recorded before it was written.
    pub paths: Vec<String>,
    /// Every temporary file, recorded before it was created.
    pub temps: Vec<String>,
    /// What the session wrote at each path (its blob id, `None` for a
    /// delete), recorded before the write: after a crash, a declared file
    /// that differs from it is someone else's edit.
    pub written: BTreeMap<String, Option<String>>,
    /// Recovery progress: every folder already kept, unfinished and
    /// unclassified, each recorded before anything it holds is replaced.
    pub kept: Vec<String>,
    pub unclassified: Vec<String>,
}

/// A crashed session, as a refresh ended it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub record: SessionRecord,
    /// Every folder holding its target files that differ from the commit
    /// refreshed to, marked unfinished: an earlier recovery's and this
    /// one's. Empty when none differ.
    pub kept: Vec<PathBuf>,
    /// Every folder holding drift the record doesn't account for,
    /// unclassified: an earlier recovery's and this one's. Empty when there
    /// was none.
    pub unclassified: Vec<PathBuf>,
}

/// What [`Session::finish`] or [`Session::abort`] set aside.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetAside {
    /// Every folder files were set aside in, as unclassified: files the
    /// session record doesn't account for, or whose content isn't what the
    /// op wrote. Empty when nothing was set aside.
    pub unclassified: Vec<PathBuf>,
}

/// The marker every kept session folder holds. `graph import` refuses a
/// folder that has it (`graph.import_unfinished_op`).
pub const MARKER: &str = "UNFINISHED.json";

/// Why a folder was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptKind {
    /// A session's own target files (a crash before publication).
    Unfinished,
    /// Drift a session record didn't account for: never guessed at.
    Unclassified,
}

/// A kept folder's marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub kind: KeptKind,
    pub record: SessionRecord,
}

/// The marker of a kept session folder, if `folder` is one.
pub fn kept(folder: &Path) -> Result<Option<Kept>> {
    let bytes = match read_regular(&folder.join(MARKER), MAX_RECORD_BYTES) {
        Ok(Some((bytes, _))) => bytes,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e),
    };
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| invalid(format!("{MARKER}: {e}")))?;
    let kind = match v.get("kind").and_then(Value::as_str) {
        Some("unclassified") => KeptKind::Unclassified,
        _ => KeptKind::Unfinished,
    };
    parse_record(&v)
        .map(|record| Some(Kept { kind, record }))
        .ok_or_else(|| invalid(format!("{MARKER} is malformed")))
}

/// The session record of a kept folder, of either kind, if `folder` is
/// one. `graph import` refuses such a folder.
pub fn unfinished(folder: &Path) -> Result<Option<SessionRecord>> {
    Ok(kept(folder)?.map(|k| k.record))
}

pub type Result<T> = std::result::Result<T, refwrite::Error>;

fn invalid(message: impl Into<String>) -> refwrite::Error {
    refwrite::Error::Invalid(message.into())
}

fn cache_edit(message: impl std::fmt::Display) -> refwrite::Error {
    invalid(format!("graph.cache_edit: {message}"))
}

/// One clone's graph cache.
#[derive(Debug, Clone)]
pub struct Cache {
    repo: PathBuf,
    common: PathBuf,
    root: PathBuf,
    refresh_limit: u64,
}

impl Cache {
    /// The cache of the clone `repo` (any checkout of it) belongs to.
    pub fn open(repo: &Path) -> Result<Self> {
        let repo = repo.canonicalize()?;
        let common = mdkg_git::common_dir(&repo)?;
        let root = refwrite::graph_dir(&common)?.join("cache");
        Ok(Self {
            repo,
            common,
            root,
            refresh_limit: MAX_REFRESH_BYTES,
        })
    }

    /// Lowers the most one refresh may write (default [`MAX_REFRESH_BYTES`]);
    /// it can't be raised. For tests and constrained hosts.
    pub fn with_refresh_limit(mut self, bytes: u64) -> Self {
        self.refresh_limit = bytes.min(MAX_REFRESH_BYTES);
        self
    }

    /// The cache folder: graph files sit at their repository paths in it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn graph(&self) -> Result<PathBuf> {
        Ok(refwrite::graph_dir(&self.common)?)
    }

    /// The manifest, or `None` before the first refresh.
    pub fn manifest(&self) -> Result<Option<Manifest>> {
        let Some((bytes, _)) = read_regular(&self.graph()?.join("cache.json"), 256 << 20)? else {
            return Ok(None);
        };
        let v: Value =
            serde_json::from_slice(&bytes).map_err(|e| invalid(format!("cache.json: {e}")))?;
        parse_manifest(&v)
            .map(Some)
            .ok_or_else(|| invalid("cache.json is malformed"))
    }

    fn save(&self, manifest: &Manifest) -> Result<()> {
        let files: serde_json::Map<String, Value> = manifest
            .files
            .iter()
            .map(|(p, s)| {
                (
                    p.clone(),
                    json!({"oid": s.oid, "mode": s.mode, "size": s.size,
                        "mtime_ns": s.mtime_ns.to_string(), "ctime_ns": s.ctime_ns.to_string(),
                        "ino": s.ino}),
                )
            })
            .collect();
        let value =
            json!({"v": 1, "commit": manifest.commit, "paths": manifest.paths, "files": files});
        let bytes = serde_json::to_vec(&value).map_err(|e| invalid(e.to_string()))?;
        crate::intent::write_durably(&self.graph()?, "cache.json", &bytes)?;
        Ok(())
    }

    /// The record of a session that hasn't ended, if any.
    pub fn session_record(&self) -> Result<Option<SessionRecord>> {
        let Some((bytes, _)) = read_regular(&self.graph()?.join("session.json"), MAX_RECORD_BYTES)?
        else {
            return Ok(None);
        };
        let v: Value =
            serde_json::from_slice(&bytes).map_err(|e| invalid(format!("session.json: {e}")))?;
        parse_record(&v)
            .map(Some)
            .ok_or_else(|| invalid("session.json is malformed or names an unsafe path"))
    }

    fn write_record(&self, record: &SessionRecord) -> Result<()> {
        let bytes =
            serde_json::to_vec(&record_json(record, None)).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid(
                "the session record is over 4 MiB: split the request",
            ));
        }
        crate::intent::write_durably(&self.graph()?, "session.json", &bytes)?;
        Ok(())
    }

    fn remove_record(&self) -> Result<()> {
        let dir = self.graph()?;
        match fs::remove_file(dir.join("session.json")) {
            Ok(()) => Ok(File::open(&dir)?.sync_all()?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every file that differs from the manifest, anywhere in the cache:
    /// stat first (ctime included), then a content hash of each suspect.
    /// Before the first refresh, every file present is `Added`.
    pub fn diff_against_manifest(&self) -> Result<Vec<Drift>> {
        let manifest = self.manifest()?.unwrap_or_default();
        self.diff(&manifest)
    }

    /// Whether the cache matches its manifest (a reader's check).
    pub fn verify(&self) -> Result<bool> {
        Ok(self.diff_against_manifest()?.is_empty())
    }

    fn diff(&self, manifest: &Manifest) -> Result<Vec<Drift>> {
        let mut drift = Vec::new();
        let mut seen = BTreeSet::new();
        for (path, meta) in walk(&self.root)? {
            seen.insert(path.clone());
            if !meta.file_type().is_file() {
                drift.push(Drift {
                    path,
                    kind: DriftKind::NotAFile,
                });
                continue;
            }
            match manifest.files.get(&path) {
                None => drift.push(Drift {
                    path,
                    kind: DriftKind::Added,
                }),
                Some(stat) if same_stat(stat, &meta) => {}
                Some(stat) => match read_in(&self.root, &path)? {
                    Some((bytes, meta)) => {
                        if self.oid(&bytes)? != stat.oid || git_mode(&meta) != stat.mode {
                            drift.push(Drift {
                                path,
                                kind: DriftKind::Modified,
                            });
                        }
                    }
                    None => drift.push(Drift {
                        path,
                        kind: DriftKind::Deleted,
                    }),
                },
            }
        }
        for path in manifest.files.keys() {
            if !seen.contains(path) {
                drift.push(Drift {
                    path: path.clone(),
                    kind: DriftKind::Deleted,
                });
            }
        }
        drift.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(drift)
    }

    /// Copies every drifted file that still exists to a new
    /// `<common>/mdkg/graph/rescued/<utc>-<id>-hand/files/<path>`, synced,
    /// and returns that folder (`None` when nothing drifted). The folder's
    /// `DRIFT.json` lists every drifted path and how, deletions included.
    /// A file over 64 MiB refuses the rescue, so nothing replaces it.
    pub fn rescue(&self, drift: &[Drift]) -> Result<Option<PathBuf>> {
        self.copy_out("hand", None, drift, &mut |_| Ok(()))
    }

    /// Copies drifted regular files into a new folder of their own, one
    /// per attempt and kind (`rescued/<utc>-<rand>-<label>/`): files under
    /// `files/`, so no copied path can meet the metadata beside it, the
    /// drift list in `DRIFT.json`, and the session marker, if any, in
    /// `UNFINISHED.json`. Each copy is a synced temporary file renamed
    /// into place; every folder on the way is created as a real folder,
    /// never through a link; everything is synced before it returns.
    /// `note` gets the folder's final path before anything is written, so
    /// a session records it first (Opus #8195).
    fn copy_out(
        &self,
        label: &str,
        marker: Option<Value>,
        drift: &[Drift],
        note: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<Option<PathBuf>> {
        if drift.is_empty() {
            return Ok(None);
        }
        // Everything is read first: an unexpected error stops here, before
        // anything is written or, later, removed.
        let mut copies = Vec::new();
        let mut list = Vec::new();
        for d in drift {
            let mut entry = json!({"path": d.path, "kind": format!("{:?}", d.kind).to_lowercase()});
            match confined(&self.root, &d.path) {
                Ok(()) => {}
                // A path under a replaced folder: listed, never followed.
                Err(refwrite::Error::Invalid(m)) if m.starts_with("graph.cache_edit") => {
                    entry["unreadable"] = json!("under a link or file where a folder belongs");
                    list.push(entry);
                    continue;
                }
                Err(e) => return Err(e),
            }
            let file = self.root.join(&d.path);
            match fs::symlink_metadata(&file) {
                Ok(m) if m.file_type().is_symlink() => {
                    // A link is recorded as data, never followed.
                    // A target that isn't UTF-8 can't be recorded exactly:
                    // refused before anything is removed (orchestrator
                    // #7290, on Terra #7173 and #7266).
                    let target = fs::read_link(&file)?;
                    let Some(target) = target.to_str() else {
                        return Err(invalid(format!(
                            "{} is a link whose target isn't UTF-8; move it out of the graph cache, then refresh again",
                            d.path
                        )));
                    };
                    entry["link"] = json!(target);
                }
                Ok(m) if m.is_file() => {
                    // Over 64 MiB fails here, before anything replaces it.
                    if let Some((bytes, meta)) = read_in(&self.root, &d.path)? {
                        entry["mode"] = json!(git_mode(&meta));
                        copies.push((d.path.clone(), bytes));
                    }
                }
                Ok(_) => {
                    entry["unreadable"] = json!("not a regular file");
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            list.push(entry);
        }
        let graph = self.graph()?;
        let base = graph.join("rescued");
        ensure_dir(&base)?;
        let name = format!(
            "{}-{}-{label}",
            utc_now(),
            &mdkg_format::Uuid7::now().to_string()[24..]
        );
        // Built under a hidden name and renamed into place only when
        // complete: a crash leaves no visible, half-classified folder.
        let partial = base.join(format!(".{name}.partial"));
        let folder = base.join(&name);
        note(&folder)?;
        // 0700 whatever the umask, so a later recovery can always read
        // and judge it (Terra #8060).
        for dir in [&partial, &partial.join("files")] {
            fs::create_dir(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let files = partial.join("files");
        if let Some(mut marker) = marker {
            marker["drift"] = json!(list);
            let bytes = serde_json::to_vec_pretty(&marker).map_err(|e| invalid(e.to_string()))?;
            crate::intent::write_durably(&partial, MARKER, &bytes)?;
        }
        let listing = serde_json::to_vec_pretty(&json!({"drift": list}))
            .map_err(|e| invalid(e.to_string()))?;
        crate::intent::write_durably(&partial, "DRIFT.json", &listing)?;
        for (path, bytes) in copies {
            let to = files.join(&path);
            let parent = to.parent().unwrap_or(&files).to_path_buf();
            // Real folders only: never through a link planted on the way.
            ensure_dir(&parent)?;
            let file_name = to
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            crate::intent::write_durably(&parent, &file_name, &bytes)?;
            sync_up(&parent, &partial)?;
        }
        File::open(&partial)?.sync_all()?;
        fs::rename(&partial, &folder)?;
        File::open(&base)?.sync_all()?;
        File::open(&graph)?.sync_all()?;
        Ok(Some(folder))
    }

    /// Keeps a session's files in a new `rescued/<utc>-<rand>-<kind>-<id>/`
    /// with the marker.
    fn keep(
        &self,
        record: &SessionRecord,
        kind: KeptKind,
        drift: &[Drift],
        note: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<Option<PathBuf>> {
        let label = match kind {
            KeptKind::Unfinished => "unfinished",
            KeptKind::Unclassified => "unclassified",
        };
        self.copy_out(
            &format!("{label}-{}", record.id),
            Some(record_json(record, Some(label))),
            drift,
            note,
        )
    }

    /// Brings the cache to `commit`'s `paths` (the graph-owned folders).
    /// First it checks the whole cache against the manifest: with a
    /// crashed session's record, its target files that differ from
    /// `commit` are kept as unfinished, its temporary files dropped, and
    /// any other drift kept as unclassified; with no record, drift is a
    /// hand edit and rescued. Only then does it rewrite what the trees
    /// change (or everything, on first use or a changed folder set).
    /// Needs the exclusive graph lock.
    pub fn refresh(&self, lock: &GraphLock, commit: &str, paths: &[String]) -> Result<Refreshed> {
        self.exclusive(lock, "a graph cache refresh")?;
        let mut paths = paths.to_vec();
        paths.sort();
        let old = self.manifest()?;
        let manifest_or_empty = old.clone().unwrap_or_default();
        let record = self.session_record()?;
        let wanted = self.listing(commit, &paths)?;
        let mut out = Refreshed {
            // Incomplete rescues first: removed only when proven
            // redundant, else kept visible (orchestrator #8066).
            orphaned: self.sweep_partials()?,
            drift: self.diff(&manifest_or_empty)?,
            ..Refreshed::default()
        };
        match &record {
            // A crashed session, first recovery or a retry after a crash
            // in one: the same classification every time, and every folder
            // kept is in the record before anything it holds is replaced.
            Some(record) => {
                // The record as read may not be durable yet (its folder
                // sync can have failed): made durable before anything is
                // replaced (Terra #8228).
                self.write_record(record)?;
                // Every folder an earlier attempt recorded is made durable
                // before it's relied on as already kept (Terra #9366).
                for folder in record.kept.iter().chain(&record.unclassified) {
                    let folder = Path::new(folder);
                    if fs::symlink_metadata(folder).is_ok_and(|m| m.is_dir()) {
                        File::open(folder)?.sync_all()?;
                        if let Some(parent) = folder.parent() {
                            File::open(parent)?.sync_all()?;
                        }
                    }
                }
                let mut progress = record.clone();
                let mut differing = Vec::new();
                let mut unaccounted = Vec::new();
                for d in &out.drift {
                    if record.temps.contains(&d.path) {
                        // Ours only if it holds the output recorded for
                        // its own target; anything else is unclassified.
                        if !self.holds_recorded_output(record, &d.path)? {
                            unaccounted.push(d.clone());
                        }
                        continue;
                    }
                    // Already in a folder an earlier attempt kept, byte
                    // for byte: nothing new to keep.
                    if self.already_kept(record, &d.path)? {
                        continue;
                    }
                    if !record.paths.contains(&d.path) {
                        unaccounted.push(d.clone());
                        continue;
                    }
                    let state = self.state(&d.path)?;
                    // Not what the session wrote (content or mode): someone
                    // else's edit.
                    if record.written.contains_key(&d.path)
                        && state != self.recorded(record, &manifest_or_empty, &d.path)
                    {
                        unaccounted.push(d.clone());
                        continue;
                    }
                    // Target files the commit already holds, content and
                    // mode (a replay published them), need no copy.
                    let target = wanted
                        .get(&d.path)
                        .map(|(mode, oid, _)| (oid.clone(), mode.clone()));
                    if state != target {
                        differing.push(d.clone());
                    }
                }
                // Each folder is in the record before anything is written
                // into it (Opus #8195).
                self.keep(record, KeptKind::Unfinished, &differing, &mut |folder| {
                    progress.kept.push(folder.to_string_lossy().into_owned());
                    self.write_record(&progress)
                })?;
                self.keep(
                    record,
                    KeptKind::Unclassified,
                    &unaccounted,
                    &mut |folder| {
                        progress
                            .unclassified
                            .push(folder.to_string_lossy().into_owned());
                        self.write_record(&progress)
                    },
                )?;
                // Only folders that exist: a name recorded before a crash
                // that came ahead of its build isn't reported (Terra #9366).
                let present = |f: &String| {
                    let p = PathBuf::from(f);
                    fs::symlink_metadata(&p)
                        .is_ok_and(|m| m.is_dir())
                        .then_some(p)
                };
                out.session = Some(Ended {
                    record: record.clone(),
                    kept: progress.kept.iter().filter_map(present).collect(),
                    unclassified: progress.unclassified.iter().filter_map(present).collect(),
                });
            }
            None => out.rescued = self.rescue(&out.drift)?,
        }
        let _open = Writable::open(&self.root)?;
        let incremental = old
            .as_ref()
            .filter(|m| m.paths == paths && !m.commit.is_empty());
        let (mut manifest, touched) =
            match incremental.map(|m| (m, self.changed(&m.commit, commit, &paths))) {
                Some((m, Ok(changed))) => {
                    let mut touched = changed;
                    touched.extend(out.drift.iter().map(|d| d.path.clone()));
                    (m.clone(), Some(touched))
                }
                _ => (Manifest::default(), None),
            };
        let targets: Vec<String> = match &touched {
            Some(t) => t.iter().cloned().collect(),
            None => wanted.keys().cloned().collect(),
        };
        let total: u64 = targets
            .iter()
            .filter_map(|p| wanted.get(p).map(|(_, _, size)| *size))
            .sum();
        // Before anything is removed.
        if total > self.refresh_limit {
            return Err(invalid(format!(
                "the refresh would write {total} bytes, over its {} byte limit",
                self.refresh_limit
            )));
        }
        if touched.is_none() {
            // Whole rebuild: everything that differed is kept or rescued
            // above, so the rest is derived.
            self.reset()?;
        }
        let blobs: Vec<(String, String, String)> = targets
            .iter()
            .filter_map(|p| {
                wanted
                    .get(p)
                    .map(|(m, o, _)| (p.clone(), o.clone(), m.clone()))
            })
            .collect();
        for path in &targets {
            remove_entry(&self.root, path)?;
            manifest.files.remove(path);
            out.updated += 1;
        }
        self.write_blobs(&mut manifest, blobs)?;
        manifest.commit = commit.to_owned();
        manifest.paths = paths;
        prune_empty(&self.root)?;
        drop(_open);
        self.save(&manifest)?;
        self.remove_record()?;
        Ok(out)
    }

    /// Opens a write session for request `id`: writes its record, then
    /// makes the folders writable until it ends. Needs the exclusive graph
    /// lock and a refreshed cache with no unfinished session.
    pub fn session<'a>(
        &'a self,
        lock: &'a GraphLock,
        id: &str,
        op: &str,
        actor: &str,
    ) -> Result<Session<'a>> {
        self.exclusive(lock, "a graph cache session")?;
        if self.session_record()?.is_some() {
            return Err(invalid(
                "an unfinished session's files are in the cache: refresh it first",
            ));
        }
        if !valid_id(id) {
            return Err(invalid(
                "request ID must be 1..120 ASCII letters, digits, '-' or '_'",
            ));
        }
        let manifest = self
            .manifest()?
            .ok_or_else(|| invalid("the graph cache hasn't been built"))?;
        let record = SessionRecord {
            id: id.into(),
            op: op.into(),
            actor: actor.into(),
            paths: Vec::new(),
            temps: Vec::new(),
            written: BTreeMap::new(),
            kept: Vec::new(),
            unclassified: Vec::new(),
        };
        self.write_record(&record)?;
        let writable = Writable::open(&self.root)?;
        Ok(Session {
            cache: self,
            manifest,
            record,
            _writable: writable,
        })
    }

    fn exclusive(&self, lock: &GraphLock, what: &str) -> Result<()> {
        lock.require_exclusive(&self.common, what)
    }

    /// A cache path's blob id and git mode now (`None`: absent). A file
    /// that can't be read as a regular file reads as an empty id, which
    /// matches nothing.
    fn state(&self, path: &str) -> Result<Option<(String, String)>> {
        Ok(match read_in(&self.root, path) {
            Ok(Some((bytes, meta))) => Some((self.oid(&bytes)?, git_mode(&meta))),
            Ok(None) => None,
            Err(refwrite::Error::Invalid(m)) if m.starts_with("graph.cache_edit") => {
                Some((String::new(), String::new()))
            }
            Err(e) => return Err(e),
        })
    }

    /// What the session recorded at `path`: its output's blob id and the
    /// mode a session write gives it (the manifest's, else `100644`).
    fn recorded(
        &self,
        record: &SessionRecord,
        manifest: &Manifest,
        path: &str,
    ) -> Option<(String, String)> {
        let mode = manifest
            .files
            .get(path)
            .map_or_else(|| "100644".to_owned(), |s| s.mode.clone());
        record
            .written
            .get(path)
            .cloned()
            .flatten()
            .map(|oid| (oid, mode))
    }

    /// Whether a recorded temporary file holds one of the session's
    /// recorded outputs, so it's provably the session's own.
    fn holds_recorded_output(&self, record: &SessionRecord, path: &str) -> Result<bool> {
        // Only the output recorded for the temp's own target (Terra
        // #7173): `<dir>/.mdkg-session-<name>-<rand>.tmp` is `<dir>/<name>`'s.
        let Some(target) = temp_target(path) else {
            return Ok(false);
        };
        Ok(match self.state(path)? {
            Some((oid, _)) => {
                record.written.get(&target).and_then(Option::as_deref) == Some(oid.as_str())
            }
            None => true,
        })
    }

    /// Whether a folder in the record already holds `path` byte for byte.
    fn already_kept(&self, record: &SessionRecord, path: &str) -> Result<bool> {
        // A link or anything else that isn't a regular file is simply not
        // already kept: it goes on to be recorded as data (Terra #8228).
        let Ok(Some((now, meta))) = read_in(&self.root, path) else {
            return Ok(false);
        };
        let mode = git_mode(&meta);
        // Only a real folder directly in `rescued/` (Terra #10216): a
        // recorded name that is now a link is never relied on.
        let base = self.graph()?.join("rescued");
        for folder in record.kept.iter().chain(&record.unclassified) {
            let folder = Path::new(folder);
            if folder.parent() == Some(base.as_path())
                && kept_copy_matches(folder, path, &now, &mode)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Settles incomplete rescues a crash left in `rescued/`. Only names
    /// in this module's generated form are touched (orchestrator #8573):
    /// one whose every file is proven redundant (byte- and mode-identical
    /// to the cache file now, or to a completed rescue's copy) is removed;
    /// any other is renamed to a visible `orphan-<name>` and returned.
    fn sweep_partials(&self) -> Result<Vec<PathBuf>> {
        let base = self.graph()?.join("rescued");
        let entries = match fs::read_dir(&base) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut orphaned = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(inner) = name.to_str().and_then(generated_partial) else {
                continue;
            };
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() {
                continue;
            }
            if self.partial_redundant(&base, &path, inner) {
                make_writable(&path).ok();
                fs::remove_dir_all(&path)?;
            } else {
                fs::rename(&path, base.join(format!("orphan-{inner}")))?;
            }
            File::open(&base)?.sync_all()?;
        }
        // Every orphan still present is reported, so one renamed by a
        // refresh that then failed is never lost from the report (Terra
        // #9366); it stays until someone settles it.
        for entry in fs::read_dir(&base)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| n.starts_with("orphan-"))
                && fs::symlink_metadata(entry.path())?.is_dir()
            {
                orphaned.push(entry.path());
            }
        }
        orphaned.sort();
        Ok(orphaned)
    }

    /// Whether every file in a partial rescue is proven redundant. Any
    /// doubt (an unreadable entry, a link, a missing drift list) is no.
    fn partial_redundant(&self, base: &Path, partial: &Path, inner: &str) -> bool {
        let files = partial.join("files");
        // A copy kept elsewhere counts only in a rescue of the same kind
        // and session, marker and all (Terra #10216): an unfinished
        // session's partial is never settled by a hand rescue.
        let label = rescue_label(inner);
        let marked = fs::symlink_metadata(partial.join(MARKER)).is_ok();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.is_empty() {
                files.clone()
            } else {
                files.join(&rel)
            };
            let Ok(entries) = fs::read_dir(&dir) else {
                // No files folder at all: nothing was copied yet.
                return rel.is_empty() && fs::symlink_metadata(&files).is_err();
            };
            for entry in entries {
                let Ok(entry) = entry else { return false };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    return false;
                };
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                    return false;
                };
                if meta.is_dir() {
                    stack.push(child);
                    continue;
                }
                if !meta.is_file() {
                    return false;
                }
                let Ok(bytes) = fs::read(entry.path()) else {
                    return false;
                };
                let Some(mode) = drift_mode(partial, &child) else {
                    return false;
                };
                let in_cache = matches!(
                    read_in(&self.root, &child),
                    Ok(Some((now, m))) if now == bytes && git_mode(&m) == mode
                );
                // Only a distinct, real completed rescue counts: never the
                // partial itself through a link (Terra #9366).
                let in_rescue = !in_cache
                    && fs::read_dir(base).is_ok_and(|rescues| {
                        rescues.flatten().any(|r| {
                            r.file_name()
                                .to_str()
                                .is_some_and(|n| !n.starts_with('.') && rescue_label(n) == label)
                                && r.path() != partial
                                && (!marked
                                    || fs::symlink_metadata(r.path().join(MARKER))
                                        .is_ok_and(|m| m.is_file()))
                                && fs::symlink_metadata(r.path()).is_ok_and(|m| m.is_dir())
                                && kept_copy_matches(&r.path(), &child, &bytes, &mode)
                        })
                    });
                if !in_cache && !in_rescue {
                    return false;
                }
            }
        }
        // The drift list's entries without a copy are metadata (a link's
        // target, a deletion): each must still hold in the cache, or the
        // partial is its only record (Terra #9366).
        let list = partial.join("DRIFT.json");
        if fs::symlink_metadata(&list).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
            // No list yet: written before any copy, so nothing was kept.
            return fs::read_dir(&files).map_or(true, |mut e| e.next().is_none());
        }
        // A list that isn't a regular file (a link) proves nothing (Terra
        // #10216).
        let Some(bytes) = read_real(&list) else {
            return false;
        };
        let Ok(list) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        let Some(entries) = list.get("drift").and_then(Value::as_array) else {
            return false;
        };
        entries.iter().all(|e| {
            let Some(path) = e.get("path").and_then(Value::as_str) else {
                return false;
            };
            // Each entry needs a positive proof of its own kind (Terra
            // #10216): a deletion holds only while the cache path is
            // provably absent, never because a copied file's folder has
            // its name or a read failed.
            let here = self.root.join(path);
            if e.get("kind").and_then(Value::as_str) == Some("deleted") {
                return fs::symlink_metadata(&here)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
            }
            if let Some(target) = e.get("link").and_then(Value::as_str) {
                return fs::symlink_metadata(&here).is_ok_and(|m| m.file_type().is_symlink())
                    && fs::read_link(&here).is_ok_and(|t| t.as_os_str() == target);
            }
            // A file entry: its copy, a regular file the walk above proved.
            fs::symlink_metadata(files.join(path)).is_ok_and(|m| m.is_file())
        })
    }

    /// Removes the whole cache folder's contents.
    fn reset(&self) -> Result<()> {
        if fs::symlink_metadata(&self.root).is_ok() {
            remove_entry_any(&self.root)?;
        }
        ensure_dir(&self.root)?;
        Ok(())
    }

    /// Paths under `paths` that differ between two commits.
    fn changed(&self, old: &str, new: &str, paths: &[String]) -> Result<BTreeSet<String>> {
        if !full_oid(old) || !full_oid(new) {
            return Err(invalid("a cache refresh needs full commit ids"));
        }
        let mut args = vec![
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            "--end-of-options",
            old,
            new,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let out = self.git(&args, None)?;
        out.split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| {
                let path = String::from_utf8(r.to_vec())
                    .map_err(|_| invalid("a graph path isn't UTF-8"))?;
                check_rel(&path)?;
                Ok(path)
            })
            .collect()
    }

    /// Regular files under `paths` at `commit`: path → (mode, oid, size).
    /// A file over 64 MiB is refused.
    fn listing(
        &self,
        commit: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, (String, String, u64)>> {
        if !full_oid(commit) {
            return Err(invalid("a cache listing needs a full commit id"));
        }
        let mut args = vec![
            "ls-tree",
            "-r",
            "-z",
            "-l",
            "--full-tree",
            "--end-of-options",
            commit,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let out = self.git(&args, None)?;
        let mut files = BTreeMap::new();
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| invalid("ls-tree record"))?;
            let header = String::from_utf8_lossy(&record[..tab]);
            let fields: Vec<&str> = header.split_whitespace().collect();
            let path = std::str::from_utf8(&record[tab + 1..])
                .map_err(|_| invalid("a graph path isn't UTF-8"))?;
            if let [mode @ ("100644" | "100755"), "blob", oid, size] = fields.as_slice() {
                check_rel(path)?;
                let size: u64 = size.parse().map_err(|_| invalid("ls-tree size"))?;
                if size > MAX_FILE_BYTES {
                    return Err(invalid(format!("{path} is over 64 MiB")));
                }
                files.insert(
                    path.to_owned(),
                    ((*mode).to_owned(), (*oid).to_owned(), size),
                );
            }
        }
        Ok(files)
    }

    /// Blob bodies, in order, in `cat-file --batch` calls of at most
    /// 64 MiB each.
    fn cat(&self, oids: &[&str]) -> Result<Vec<Vec<u8>>> {
        let mut bodies = Vec::new();
        let mut chunk: Vec<&str> = Vec::new();
        let mut size = 0u64;
        let sizes = self.sizes(oids)?;
        for (oid, n) in oids.iter().zip(sizes) {
            if n > MAX_FILE_BYTES {
                return Err(invalid(format!("blob {oid} is over 64 MiB")));
            }
            if !chunk.is_empty() && size + n > MAX_FILE_BYTES {
                bodies.extend(self.cat_batch(&chunk)?);
                chunk.clear();
                size = 0;
            }
            chunk.push(oid);
            size += n;
        }
        if !chunk.is_empty() {
            bodies.extend(self.cat_batch(&chunk)?);
        }
        Ok(bodies)
    }

    fn sizes(&self, oids: &[&str]) -> Result<Vec<u64>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let out = self.git(
            &["cat-file", "--batch-check=%(objecttype) %(objectsize)"],
            Some(input.as_bytes()),
        )?;
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .map_err(|_| invalid("cat-file output"))?
            .lines()
            .collect();
        if lines.len() != oids.len() {
            return Err(invalid("cat-file answered a different count"));
        }
        lines
            .iter()
            .map(|l| match l.split_once(' ') {
                Some(("blob", n)) => n.parse().map_err(|_| invalid("cat-file size")),
                _ => Err(invalid(format!("cat-file: {l}"))),
            })
            .collect()
    }

    fn cat_batch(&self, oids: &[&str]) -> Result<Vec<Vec<u8>>> {
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let out = self.git(&["cat-file", "--batch"], Some(input.as_bytes()))?;
        let mut bodies = Vec::new();
        let mut at = 0;
        for oid in oids {
            let nl = out[at..]
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| invalid("cat-file header"))?
                + at;
            let header = String::from_utf8_lossy(&out[at..nl]);
            let fields: Vec<&str> = header.split(' ').collect();
            let [found, "blob", size] = fields.as_slice() else {
                return Err(invalid(format!("cat-file: {header}")));
            };
            if found != oid {
                return Err(invalid("cat-file answered out of order"));
            }
            let size: usize = size.parse().map_err(|_| invalid("cat-file size"))?;
            let start = nl + 1;
            let end = start + size;
            if out.get(end) != Some(&b'\n') {
                return Err(invalid("cat-file body truncated"));
            }
            bodies.push(out[start..end].to_vec());
            at = end + 1;
        }
        Ok(bodies)
    }

    /// Writes blobs `(path, oid, git mode)` read-only into the cache, and
    /// records them in `manifest`.
    fn write_blobs(
        &self,
        manifest: &mut Manifest,
        blobs: Vec<(String, String, String)>,
    ) -> Result<()> {
        let bodies = self.cat(&blobs.iter().map(|b| b.1.as_str()).collect::<Vec<_>>())?;
        for ((path, oid, mode), bytes) in blobs.into_iter().zip(bodies) {
            let meta = place(&self.root, &path, &bytes, &mode, None, &mut |_| Ok(()))?;
            manifest.files.insert(path, stat(&meta, oid, mode));
        }
        Ok(())
    }

    /// The git blob id of `bytes`, without writing an object.
    fn oid(&self, bytes: &[u8]) -> Result<String> {
        let out = self.git(&["hash-object", "--no-filters", "--stdin"], Some(bytes))?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    fn git(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let out = mdkg_git::command(&self.repo).args(args).run(input)?;
        if !out.status.success() {
            return Err(refwrite::Error::Git {
                operation: args[0].into(),
                detail: String::from_utf8_lossy(&out.stderr).trim().into(),
            });
        }
        Ok(out.stdout)
    }
}

/// A write session over the cache. Folders stay writable until it's
/// dropped. Write through [`Session::write`] and [`Session::delete`], so
/// every operation write is in the record before it happens.
#[derive(Debug)]
pub struct Session<'a> {
    cache: &'a Cache,
    manifest: Manifest,
    record: SessionRecord,
    _writable: Writable,
}

impl Session<'_> {
    /// The cache folder.
    pub fn root(&self) -> &Path {
        &self.cache.root
    }

    /// The commit the session started from.
    pub fn base(&self) -> &str {
        &self.manifest.commit
    }

    /// The session's record, as persisted.
    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    fn in_graph(&self, path: &str) -> Result<()> {
        check_rel(path)?;
        if !self
            .manifest
            .paths
            .iter()
            .any(|p| path.starts_with(&format!("{p}/")))
        {
            return Err(invalid(format!("{path} is outside the graph folders")));
        }
        Ok(())
    }

    /// The blob id and git mode a path should hold now: what the session
    /// wrote, else the manifest's (`None`: absent).
    fn expected(&self, path: &str) -> Option<(String, String)> {
        let mode = self
            .manifest
            .files
            .get(path)
            .map_or_else(|| "100644".to_owned(), |s| s.mode.clone());
        match self.record.written.get(path) {
            Some(w) => w.clone().map(|oid| (oid, mode)),
            None => self
                .manifest
                .files
                .get(path)
                .map(|s| (s.oid.clone(), s.mode.clone())),
        }
    }

    /// The blob id and git mode a path holds now.
    fn current(&self, path: &str) -> Result<Option<(String, String)>> {
        Ok(match read_in(&self.cache.root, path)? {
            Some((bytes, meta)) => Some((self.cache.oid(&bytes)?, git_mode(&meta))),
            None => None,
        })
    }

    /// Refuses a path whose current content or mode isn't what the session
    /// expects (`graph.cache_edit`): someone edited it during the session.
    fn check_preimage(&self, path: &str, expected: &Option<(String, String)>) -> Result<()> {
        if self.current(path)? != *expected {
            return Err(cache_edit(format!(
                "{path} changed during the write session"
            )));
        }
        Ok(())
    }

    /// Adds to the record, persisting it before the in-memory copy
    /// changes, so a failed write is never acknowledged.
    fn record_more(
        &mut self,
        path: Option<&str>,
        temp: Option<&str>,
        written: Option<Option<String>>,
    ) -> Result<()> {
        let mut next = self.record.clone();
        if let Some(p) = path
            && !next.paths.iter().any(|x| x == p)
        {
            next.paths.push(p.to_owned());
        }
        if let Some(t) = temp
            && !next.temps.iter().any(|x| x == t)
        {
            next.temps.push(t.to_owned());
        }
        if let (Some(p), Some(w)) = (path, written) {
            next.written.insert(p.to_owned(), w);
        }
        if next != self.record {
            self.cache.write_record(&next)?;
            self.record = next;
        }
        Ok(())
    }

    /// A file's bytes. A file the manifest lists is checked by the hash of
    /// the very bytes returned (`graph.cache_edit` when it differs, or
    /// when it's missing); a file it doesn't list reads as absent unless
    /// the session wrote it, and refuses when someone else created it.
    pub fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        self.in_graph(path)?;
        let current = read_in(&self.cache.root, path)?;
        let found = match &current {
            Some((bytes, meta)) => Some((self.cache.oid(bytes)?, git_mode(meta))),
            None => None,
        };
        if found != self.expected(path) {
            return Err(cache_edit(format!("{path} differs from the graph tip")));
        }
        Ok(current.map(|(b, _)| b))
    }

    /// Replaces `path` with `bytes`: checks its preimage, records the
    /// path, a temporary file and the output's blob id, writes the
    /// temporary file (synced), checks the preimage again immediately
    /// before renaming it over the path (under the exclusive lock; the
    /// microseconds between that check and the rename are the stated
    /// residual), and syncs the folder. A changed preimage is refused
    /// (`graph.cache_edit`) and never overwritten.
    pub fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.in_graph(path)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(format!("{path} would be over 64 MiB")));
        }
        let expected = self.expected(path);
        self.check_preimage(path, &expected)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let temp = format!(
            "{dir}/.mdkg-session-{name}-{}.tmp",
            &mdkg_format::Uuid7::now().to_string()[24..]
        );
        let oid = self.cache.oid(bytes)?;
        self.record_more(Some(path), None, Some(Some(oid)))?;
        let mode = self
            .manifest
            .files
            .get(path)
            .map_or_else(|| "100644".to_owned(), |s| s.mode.clone());
        let root = self.cache.root.clone();
        place(&root, path, bytes, &mode, Some(&temp), &mut |at| match at {
            Placed::Created => self.record_more(None, Some(&temp), None),
            Placed::BeforeRename => self.check_preimage(path, &expected),
        })?;
        Ok(())
    }

    /// Deletes `path`: checks its preimage, records the deletion, checks
    /// the preimage again immediately before removing it, and syncs the
    /// folder.
    pub fn delete(&mut self, path: &str) -> Result<()> {
        self.in_graph(path)?;
        let expected = self.expected(path);
        self.check_preimage(path, &expected)?;
        self.record_more(Some(path), None, Some(None))?;
        self.check_preimage(path, &expected)?;
        remove_entry(&self.cache.root, path)?;
        sync_parent(&self.cache.root, path)?;
        Ok(())
    }

    /// Declares a path the op writes by other means, after checking its
    /// preimage. Trust is per path (a stated residual, orchestrator on
    /// #6791): the cache is single-writer by contract, so whatever bytes
    /// the declared path holds when [`Session::changes`] runs are the op's,
    /// and a concurrent edit to it by the same user is that user's own. Prefer [`Session::write`]: a writer's own temporary
    /// files aren't in the record, so after a crash they're kept as
    /// unclassified, and the output isn't recorded, so it can't be
    /// checked.
    pub fn will_write(&mut self, path: &str) -> Result<()> {
        self.in_graph(path)?;
        if !self.record.paths.iter().any(|p| p == path) {
            let expected = self.expected(path);
            self.check_preimage(path, &expected)?;
        }
        self.record_more(Some(path), None, None)
    }
    /// Every file that differs from the manifest, declared or not.
    pub fn touched(&self) -> Result<Vec<Drift>> {
        self.cache.diff(&self.manifest)
    }

    /// Files that differ from the manifest but that the record doesn't
    /// account for: an unleased edit by the op, or someone else's edit.
    pub fn undeclared(&self) -> Result<Vec<Drift>> {
        Ok(self
            .touched()?
            .into_iter()
            .filter(|d| {
                !self.record.paths.contains(&d.path) && !self.record.temps.contains(&d.path)
            })
            .collect())
    }

    /// The declared paths that changed, as refwrite changes. Refuses the
    /// op (`graph.unleased_edit`) when anything the record doesn't
    /// account for changed.
    pub fn changes(&self) -> Result<Vec<Change>> {
        // Every recorded output first, not only the paths still differing
        // from the manifest: one edited back to its old bytes would
        // otherwise drop out of the op silently.
        for path in self.record.written.keys() {
            if self.cache.state(path)? != self.cache.recorded(&self.record, &self.manifest, path) {
                return Err(cache_edit(format!(
                    "{path} isn't what the write session wrote"
                )));
            }
        }
        let mut out = Vec::new();
        for d in self.touched()? {
            if self.record.temps.contains(&d.path) {
                continue;
            }
            if !self.record.paths.contains(&d.path) {
                return Err(invalid(format!(
                    "graph.unleased_edit: {} changed, but the write session didn't record it",
                    d.path
                )));
            }
            match d.kind {
                DriftKind::Deleted => {
                    if self
                        .record
                        .written
                        .get(&d.path)
                        .is_some_and(Option::is_some)
                    {
                        return Err(cache_edit(format!(
                            "{} was deleted, but the write session wrote it",
                            d.path
                        )));
                    }
                    out.push(Change::Delete { path: d.path });
                }
                DriftKind::Modified | DriftKind::Added => {
                    let (bytes, _) = read_in(&self.cache.root, &d.path)?
                        .ok_or_else(|| cache_edit(format!("{} vanished", d.path)))?;
                    // Only the session's own output is published.
                    if let Some(written) = self.record.written.get(&d.path)
                        && *written != Some(self.cache.oid(&bytes)?)
                    {
                        return Err(cache_edit(format!(
                            "{} isn't what the write session wrote",
                            d.path
                        )));
                    }
                    out.push(Change::Put {
                        path: d.path,
                        bytes,
                    });
                }
                DriftKind::NotAFile => {
                    return Err(cache_edit(format!("{} isn't a regular file", d.path)));
                }
            }
        }
        // Every requested path is a preimage, unchanged ones included
        // (orchestrator #8013, #6819 item 16): an unchanged one goes in as
        // the no-op it is, so a rebuild still checks it, and an all-no-op
        // session publishes nothing (`graph.write_noop`).
        let mut unchanged: Vec<&String> = self
            .record
            .paths
            .iter()
            .filter(|p| !out.iter().any(|c| c.path() == p.as_str()))
            .collect();
        unchanged.sort();
        unchanged.dedup();
        for path in unchanged {
            match read_in(&self.cache.root, path)? {
                Some((bytes, _)) => out.push(Change::Put {
                    path: path.clone(),
                    bytes,
                }),
                None => out.push(Change::Delete { path: path.clone() }),
            }
        }
        Ok(out)
    }

    /// Sets aside, as unclassified, every drifted path the record doesn't
    /// account for and each declared path whose content isn't what the
    /// session wrote, then restores those paths from the manifest. Returns
    /// the paths so handled.
    fn set_aside(&mut self) -> Result<(Option<PathBuf>, BTreeSet<String>)> {
        let mut aside = Vec::new();
        let mut temps = Vec::new();
        for d in self.touched()? {
            if self.record.temps.contains(&d.path) {
                // Ours only if it holds an output the session recorded.
                if self.cache.holds_recorded_output(&self.record, &d.path)? {
                    temps.push(d.path);
                } else {
                    aside.push(d);
                }
            } else {
                // Not declared, or declared but not what the session wrote
                // (content or mode).
                let undeclared = !self.record.paths.contains(&d.path);
                let not_ours = self.record.written.contains_key(&d.path)
                    && self.cache.state(&d.path)?
                        != self.cache.recorded(&self.record, &self.manifest, &d.path);
                if undeclared || not_ours {
                    aside.push(d);
                }
            }
        }
        for t in temps {
            remove_entry(&self.cache.root, &t)?;
        }
        // Where they go is in the record before anything is written there
        // or restored (Opus #8195).
        let cache = self.cache;
        let record = self.record.clone();
        let kept = cache.keep(&record, KeptKind::Unclassified, &aside, &mut |folder| {
            self.note_unclassified(folder)
        })?;
        let paths: BTreeSet<String> = aside.into_iter().map(|d| d.path).collect();
        self.restore(&paths)?;
        Ok((kept, paths))
    }

    /// Records a folder just kept, before anything it holds is replaced.
    fn note_unclassified(&mut self, folder: &Path) -> Result<()> {
        let mut next = self.record.clone();
        next.unclassified
            .push(folder.to_string_lossy().into_owned());
        self.cache.write_record(&next)?;
        self.record = next;
        Ok(())
    }

    /// Puts `paths` back as the manifest has them.
    fn restore(&mut self, paths: &BTreeSet<String>) -> Result<()> {
        let mut blobs = Vec::new();
        for path in paths {
            remove_entry(&self.cache.root, path)?;
            if let Some(stat) = self.manifest.files.remove(path) {
                blobs.push((path.clone(), stat.oid, stat.mode));
            }
        }
        self.cache.write_blobs(&mut self.manifest, blobs)
    }

    /// Ends the session after a publish. `commit` is the commit whose tree
    /// the declared paths now hold (the prepared commit). Each declared
    /// path is checked against `commit`'s tree by the bytes actually read;
    /// one that differs is set aside as unclassified and rewritten from
    /// the commit, never trusted. Undeclared drift is set aside the same
    /// way. The manifest moves to `commit` only when it was built on the
    /// manifest's commit; otherwise it stays, so the next refresh catches
    /// up on everything between.
    pub fn finish(mut self, commit: &str) -> Result<SetAside> {
        let (unclassified, _) = self.set_aside()?;
        let mut unclassified: Vec<PathBuf> = unclassified.into_iter().collect();
        let wanted = self.cache.listing(commit, &self.manifest.paths)?;
        let mut differing = Vec::new();
        let mut rewrite = Vec::new();
        for path in self.record.paths.clone() {
            let target = wanted.get(&path);
            match (read_in_file(&self.cache.root, &path), target) {
                (Ok(Some((bytes, meta, file))), Some((mode, oid, _)))
                    if self.cache.oid(&bytes)? == *oid && git_mode(&meta) == *mode =>
                {
                    // Through the very descriptor hashed, never the path.
                    file.set_permissions(fs::Permissions::from_mode(file_mode(mode)))?;
                    let after = file.metadata()?;
                    self.manifest
                        .files
                        .insert(path.clone(), stat(&after, oid.clone(), mode.clone()));
                }
                (Ok(None), None) => {
                    self.manifest.files.remove(&path);
                }
                (current, _) => {
                    if !matches!(current, Ok(None)) {
                        drop(current);
                        differing.push(Drift {
                            path: path.clone(),
                            kind: DriftKind::Modified,
                        });
                    }
                    rewrite.push((path.clone(), target.cloned()));
                }
            }
        }
        if !differing.is_empty() {
            // Keep before anything is removed.
            // Recorded before the rewrite, so a crash after it leaves the
            // folder reported (Terra #7173).
            let cache = self.cache;
            let record = self.record.clone();
            if let Some(folder) =
                cache.keep(&record, KeptKind::Unclassified, &differing, &mut |folder| {
                    self.note_unclassified(folder)
                })?
            {
                unclassified.push(folder);
            }
        }
        let mut blobs = Vec::new();
        for (path, target) in rewrite {
            remove_entry(&self.cache.root, &path)?;
            self.manifest.files.remove(&path);
            if let Some((mode, oid, _)) = target {
                blobs.push((path, oid, mode));
            }
        }
        self.cache.write_blobs(&mut self.manifest, blobs)?;
        prune_empty(&self.cache.root)?;
        let parent = self
            .cache
            .git(
                &["rev-parse", "--verify", "--quiet", &format!("{commit}^1")],
                None,
            )
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim().to_owned());
        if parent.as_deref() == Some(self.manifest.commit.as_str()) {
            self.manifest.commit = commit.to_owned();
        }
        self.cache.save(&self.manifest)?;
        self.cache.remove_record()?;
        Ok(SetAside { unclassified })
    }

    /// Ends a refused or failed session (a lease refusal, a gate failure,
    /// an unleased path, any error before publication): restores what the
    /// session wrote, with no rescue, and removes the record. Anything the
    /// record doesn't account for, and any declared file whose content
    /// isn't what the session wrote, is set aside as unclassified first.
    pub fn abort(mut self) -> Result<SetAside> {
        let (unclassified, aside) = self.set_aside()?;
        let unclassified: Vec<PathBuf> = unclassified.into_iter().collect();
        let mine: BTreeSet<String> = self
            .record
            .paths
            .iter()
            .filter(|p| !aside.contains(*p))
            .cloned()
            .collect();
        self.restore(&mine)?;
        prune_empty(&self.cache.root)?;
        self.cache.save(&self.manifest)?;
        self.cache.remove_record()?;
        Ok(SetAside { unclassified })
    }
}

/// Makes the cache's folders writable while held; read-only again when
/// dropped.
#[derive(Debug)]
struct Writable {
    root: PathBuf,
}

impl Writable {
    fn open(root: &Path) -> Result<Self> {
        ensure_dir(root)?;
        make_writable(root)?;
        Ok(Self {
            root: root.to_owned(),
        })
    }
}

impl Drop for Writable {
    fn drop(&mut self) {
        set_dir_modes(&self.root, 0o555).ok();
    }
}

fn full_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A normalized relative path: no leading `/`, no empty, `.` or `..`
/// component, no NUL or backslash.
fn check_rel(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\0', '\\'])
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(invalid(format!("{path:?} isn't a safe graph path")));
    }
    Ok(())
}

/// Checks that `root` and every folder between it and `rel` is a real
/// folder, never a link.
fn confined(root: &Path, rel: &str) -> Result<()> {
    check_rel(rel)?;
    let mut at = root.to_path_buf();
    let meta = fs::symlink_metadata(&at)?;
    if !meta.is_dir() {
        return Err(cache_edit("the cache folder was replaced"));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(cache_edit(format!(
                    "{} isn't a folder (a link or file replaced it)",
                    at.strip_prefix(root).unwrap_or(&at).display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// A regular file's whole content and stat, read through one descriptor
/// opened with `O_NOFOLLOW`. `None` when absent. A link, folder or other
/// non-file is an error, never followed; so is a file over `cap`.
fn read_regular(path: &Path, cap: u64) -> Result<Option<(Vec<u8>, fs::Metadata)>> {
    Ok(read_regular_file(path, cap)?.map(|(b, m, _)| (b, m)))
}

/// [`read_regular`], also returning the descriptor read.
fn read_regular_file(path: &Path, cap: u64) -> Result<Option<(Vec<u8>, fs::Metadata, File)>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            return Err(cache_edit(format!("{} is a link", path.display())));
        }
        Err(e) => return Err(e.into()),
    };
    let before = file.metadata()?;
    // A hard link could make a chmod or a rescue reach a file outside.
    if before.is_file() && before.nlink() != 1 {
        return Err(cache_edit(format!(
            "{} has more than one link",
            path.display()
        )));
    }
    if !before.is_file() {
        return Err(cache_edit(format!(
            "{} isn't a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    (&mut file).take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid(format!(
            "{} is over the {} MiB limit",
            path.display(),
            cap >> 20
        )));
    }
    let after = file.metadata()?;
    if after.len() != bytes.len() as u64
        || after.ctime_nsec() != before.ctime_nsec()
        || after.ctime() != before.ctime()
    {
        return Err(cache_edit(format!(
            "{} changed while it was read",
            path.display()
        )));
    }
    Ok(Some((bytes, after, file)))
}

/// [`read_regular`] of a cache path, confined.
fn read_in(root: &Path, rel: &str) -> Result<Option<(Vec<u8>, fs::Metadata)>> {
    confined(root, rel)?;
    read_regular(&root.join(rel), MAX_FILE_BYTES)
}

/// [`read_regular_file`] of a cache path, confined.
fn read_in_file(root: &Path, rel: &str) -> Result<Option<(Vec<u8>, fs::Metadata, File)>> {
    confined(root, rel)?;
    read_regular_file(&root.join(rel), MAX_FILE_BYTES)
}

/// The `<name>` of `.<name>.partial` when it's exactly this module's
/// generated form, `<utc>-<12 hex>-<label>` with `label` one of `hand`,
/// `unfinished-<id>` or `unclassified-<id>`; `None` for anything else.
fn generated_partial(entry: &str) -> Option<&str> {
    let inner = entry.strip_prefix('.')?.strip_suffix(".partial")?;
    let (utc, rest) = inner.split_once('-')?;
    let (rand, label) = rest.split_once('-')?;
    let utc_ok = utc.len() == 16
        && utc.bytes().enumerate().all(|(i, b)| match i {
            8 => b == b'T',
            15 => b == b'Z',
            _ => b.is_ascii_digit(),
        });
    let rand_ok = rand.len() == 12
        && rand
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    let label_ok = label == "hand"
        || label
            .strip_prefix("unfinished-")
            .or_else(|| label.strip_prefix("unclassified-"))
            .is_some_and(valid_id);
    (utc_ok && rand_ok && label_ok).then_some(inner)
}

/// A rescue name's label (`hand`, `unfinished-<id>`, `unclassified-<id>`),
/// after its time and random parts.
fn rescue_label(name: &str) -> Option<&str> {
    name.splitn(3, '-').nth(2)
}

/// A regular file's bytes; `None` for a link, anything else or an error.
fn read_real(path: &Path) -> Option<Vec<u8>> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    fs::read(path).ok()
}

/// The git mode a rescue folder's `DRIFT.json` records for `path`.
fn drift_mode(folder: &Path, path: &str) -> Option<String> {
    let bytes = read_real(&folder.join("DRIFT.json"))?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("drift")?.as_array()?.iter().find_map(|e| {
        (e.get("path")?.as_str()? == path)
            .then(|| e.get("mode")?.as_str().map(str::to_owned))
            .flatten()
    })
}

/// Whether a rescue folder holds `path` with these bytes and this mode.
fn kept_copy_matches(folder: &Path, path: &str, bytes: &[u8], mode: &str) -> bool {
    // Every folder on the way is a real folder, never a link, the rescue
    // itself included (Terra #10216).
    if !fs::symlink_metadata(folder).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let mut dir = folder.join("files");
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            return false;
        }
        dir = dir.join(part);
    }
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let copy = folder.join("files").join(path);
    fs::symlink_metadata(&copy).is_ok_and(|m| m.is_file())
        && fs::read(&copy).is_ok_and(|b| b == bytes)
        && drift_mode(folder, path).as_deref() == Some(mode)
}

/// Creates `dir` (and its missing parents) as real folders.
fn ensure_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(cache_edit(format!("{} isn't a folder", dir.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                ensure_dir(parent)?;
            }
            match fs::create_dir(dir) {
                Ok(()) => Ok(()),
                // Created meanwhile: it must still be a real folder, never
                // a link. A swap after this check is a stated same-user
                // residual (orchestrator, on #6791).
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::symlink_metadata(dir)?.is_dir() {
                        Ok(())
                    } else {
                        Err(cache_edit(format!("{} isn't a folder", dir.display())))
                    }
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Writes `bytes` at a cache path with the git mode's permissions: to a
/// new temporary file (named `temp`, or a private one), synced, renamed
/// over the path, with the folder synced. Missing folders are created as
/// real folders; a link or file where a folder belongs is refused.
/// The points in [`place`] where the caller acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    /// The temporary file was just created by this call.
    Created,
    /// The file is written; the rename comes next.
    BeforeRename,
}

fn place(
    root: &Path,
    rel: &str,
    bytes: &[u8],
    mode: &str,
    temp: Option<&str>,
    step: &mut dyn FnMut(Placed) -> Result<()>,
) -> Result<fs::Metadata> {
    confined(root, rel)?;
    let target = root.join(rel);
    let dir = target.parent().unwrap_or(root).to_path_buf();
    ensure_dir(&dir)?;
    confined(root, rel)?;
    let temp = match temp {
        Some(t) => root.join(t),
        None => dir.join(format!(
            ".mdkg-cache-{}.tmp",
            &mdkg_format::Uuid7::now().to_string()[24..]
        )),
    };
    // Only a temporary file this call created is ever removed.
    let mut created = false;
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temp)?;
        created = true;
        // Ownership is this very creation (orchestrator #7290): the
        // caller records the temporary file only now.
        step(Placed::Created).map_err(io::Error::other)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        // Through the descriptor written, never the path.
        f.set_permissions(fs::Permissions::from_mode(file_mode(mode)))?;
        // A folder or other non-file where the file belongs is never
        // replaced: the caller keeps its contents and removes it first.
        if let Ok(m) = fs::symlink_metadata(&target)
            && m.is_dir()
        {
            return Err(io::Error::other(format!(
                "{rel}: a folder is where a file belongs"
            )));
        }
        Ok(())
    })();
    // Immediately before the rename: the caller's last preimage check.
    let result = result
        .map_err(refwrite::Error::from)
        .and_then(|()| step(Placed::BeforeRename));
    let result = result.and_then(|()| {
        fs::rename(&temp, &target)?;
        File::open(&dir)?.sync_all()?;
        Ok(())
    });
    if result.is_err() && created {
        fs::remove_file(&temp).ok();
    }
    result?;
    Ok(fs::symlink_metadata(&target)?)
}

/// Removes a cache path: a file or link itself (never its target), or a
/// folder whole. Callers keep or rescue a folder's contents first.
fn remove_entry(root: &Path, rel: &str) -> Result<()> {
    confined(root, rel)?;
    remove_entry_any(&root.join(rel))
}

fn remove_entry_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            make_writable(path)?;
            fs::remove_dir_all(path)?;
            Ok(())
        }
        Ok(_) => Ok(fs::remove_file(path)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn sync_parent(root: &Path, rel: &str) -> Result<()> {
    let dir = root.join(rel);
    let dir = dir.parent().unwrap_or(root);
    if dir.is_dir() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Syncs `dir` and every folder above it up to `base`.
fn sync_up(dir: &Path, base: &Path) -> Result<()> {
    let mut at = Some(dir);
    while let Some(d) = at {
        File::open(d)?.sync_all()?;
        if d == base {
            break;
        }
        at = d.parent();
    }
    Ok(())
}

fn make_writable(root: &Path) -> io::Result<()> {
    set_dir_modes(root, 0o755)
}

fn set_dir_modes(root: &Path, mode: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Ok(());
    }
    // Writable on the way down, so children can be listed and changed.
    if mode & 0o200 != 0 {
        fs::set_permissions(root, fs::Permissions::from_mode(mode))?;
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            set_dir_modes(&entry.path(), mode)?;
        }
    }
    if mode & 0o200 == 0 {
        fs::set_permissions(root, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Removes empty folders below `root`.
fn prune_empty(root: &Path) -> io::Result<()> {
    fn walk(dir: &Path) -> io::Result<bool> {
        let mut empty = true;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if walk(&entry.path())? {
                    fs::remove_dir(entry.path())?;
                } else {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        Ok(empty)
    }
    if fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        walk(root)?;
    }
    Ok(())
}

/// Every entry below `root` that isn't a folder (links included, never
/// followed), as repository-relative paths.
fn walk(root: &Path) -> Result<Vec<(String, fs::Metadata)>> {
    let mut out = Vec::new();
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(cache_edit("the cache folder was replaced")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    }
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("a cache path isn't UTF-8"))?;
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                stack.push((entry.path(), path));
            } else {
                out.push((path, meta));
            }
        }
    }
    Ok(out)
}

fn same_stat(stat: &Stat, meta: &fs::Metadata) -> bool {
    stat.size == meta.size()
        && stat.ino == meta.ino()
        && stat.mtime_ns == nanos(meta.mtime(), meta.mtime_nsec())
        && stat.ctime_ns == nanos(meta.ctime(), meta.ctime_nsec())
        && stat.mode == git_mode(meta)
}

fn stat(meta: &fs::Metadata, oid: String, mode: String) -> Stat {
    Stat {
        oid,
        mode,
        size: meta.size(),
        mtime_ns: nanos(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: nanos(meta.ctime(), meta.ctime_nsec()),
        ino: meta.ino(),
    }
}

fn nanos(secs: i64, nsec: i64) -> i128 {
    i128::from(secs) * 1_000_000_000 + i128::from(nsec)
}

fn git_mode(meta: &fs::Metadata) -> String {
    if meta.mode() & 0o100 != 0 {
        "100755".into()
    } else {
        "100644".into()
    }
}

fn file_mode(git_mode: &str) -> u32 {
    if git_mode == "100755" { 0o555 } else { 0o444 }
}

/// The target a session temp name belongs to.
fn temp_target(temp: &str) -> Option<String> {
    let (dir, file) = temp.rsplit_once('/').unwrap_or(("", temp));
    let rest = file.strip_prefix(".mdkg-session-")?.strip_suffix(".tmp")?;
    let (name, _) = rest.rsplit_once('-')?;
    Some(if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    })
}

fn record_json(r: &SessionRecord, kind: Option<&str>) -> Value {
    let mut v = json!({"v": 1, "id": r.id, "op": r.op, "actor": r.actor,
        "paths": r.paths, "temps": r.temps, "written": r.written,
        "kept": r.kept, "unclassified": r.unclassified});
    if let Some(kind) = kind {
        v["kind"] = json!(kind);
    }
    v
}

/// A record whose id and paths are as safe as a new session's.
fn parse_record(v: &Value) -> Option<SessionRecord> {
    if v.get("v")?.as_u64()? != 1 {
        return None;
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let list = |k: &str| -> Option<Vec<String>> {
        match v.get(k) {
            None => Some(Vec::new()),
            Some(a) => a
                .as_array()?
                .iter()
                .map(|p| {
                    let p = p.as_str()?;
                    check_rel(p).ok().map(|()| p.to_owned())
                })
                .collect(),
        }
    };
    // A folder list; a single string or null from an older record.
    let folders = |k: &str| -> Option<Vec<String>> {
        match v.get(k) {
            None | Some(Value::Null) => Some(Vec::new()),
            Some(Value::String(f)) => Some(vec![f.clone()]),
            Some(a) => a
                .as_array()?
                .iter()
                .map(|f| f.as_str().map(str::to_owned))
                .collect(),
        }
    };
    let record = SessionRecord {
        id: s("id")?,
        op: s("op")?,
        actor: s("actor")?,
        paths: list("paths")?,
        temps: list("temps")?,
        written: match v.get("written") {
            None => BTreeMap::new(),
            Some(w) => w
                .as_object()?
                .iter()
                .map(|(k, x)| {
                    check_rel(k).ok()?;
                    let oid = match x {
                        Value::Null => None,
                        Value::String(o) => Some(o.clone()),
                        _ => return None,
                    };
                    Some((k.clone(), oid))
                })
                .collect::<Option<_>>()?,
        },
        kept: folders("kept")?,
        unclassified: folders("unclassified")?,
    };
    valid_id(&record.id).then_some(record)
}

fn parse_manifest(v: &Value) -> Option<Manifest> {
    if v.get("v")?.as_u64()? != 1 {
        return None;
    }
    let mut files = BTreeMap::new();
    for (path, s) in v.get("files")?.as_object()? {
        check_rel(path).ok()?;
        let text = |k: &str| s.get(k).and_then(Value::as_str).map(str::to_owned);
        files.insert(
            path.clone(),
            Stat {
                oid: text("oid")?,
                mode: text("mode")?,
                size: s.get("size")?.as_u64()?,
                mtime_ns: text("mtime_ns")?.parse().ok()?,
                ctime_ns: text("ctime_ns")?.parse().ok()?,
                ino: s.get("ino")?.as_u64()?,
            },
        );
    }
    let commit = v.get("commit")?.as_str()?.to_owned();
    // A commit id or nothing: never a value git could read as an option.
    if !commit.is_empty() && !full_oid(&commit) {
        return None;
    }
    Some(Manifest {
        commit,
        paths: v
            .get("paths")?
            .as_array()?
            .iter()
            .map(|p| p.as_str().map(str::to_owned))
            .collect::<Option<_>>()?,
        files,
    })
}

/// `YYYYMMDDTHHMMSSZ`, now.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
