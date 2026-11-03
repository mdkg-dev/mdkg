//! `mdkg migrate --restore-history`: brings back into git the work history
//! a 0.7.0 migration moved to the coordinator store: its checkpoints and
//! finished tests, as 0.7 nodes, and its event logs, as they were.
//!
//! The source is git itself, not the store or a backup: the migration
//! commit's parent holds every file it deleted, exactly, on every clone
//! that has the history. A migration commit is any commit on HEAD's
//! history whose parent's config has no `graph_id` and whose own does, so
//! one merged by squash or rebase is found too. Each node comes back at the
//! path it had, with the id a migration that kept the history in git
//! would have minted for it, and its refs rewritten to the ids the graph's
//! nodes have now; a ref to a node the graph no longer has moves to its
//! body, as migrate does. Refs other nodes had to it, which 0.7.0 dropped,
//! stay dropped.
//!
//! - `--dry-run` lists what comes back and prints the plan's hash.
//! - `--apply <hash>` writes those files and commits them, in one commit
//!   on HEAD. Undo it with `git revert`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::Path;

use mdkg_format::{Code, Digest};
use mdkg_graph::{GraphError, config};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use super::classify::{Plane, classify};
use super::gather::{frontmatter_lines, subgraph_ids, workspace_folders};
use super::gitops::{self, Edit};
use super::plan::{self, Action, FileChange, History, NodeFile, PlanWarning, Source, TextFile};
use crate::coord::{Repo, git_error};

/// What a restore brings back, by kind.
#[derive(Debug, Default, Clone, Serialize, JsonSchema)]
pub struct Restored {
    /// The migration commits the files come from, newest first.
    pub migrations: Vec<String>,
    pub checkpoints: usize,
    pub check_results: usize,
    pub event_logs: usize,
}

/// A restore plan: every file it writes, each new.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub base: String,
    pub graph_id: String,
    pub restored: Restored,
    /// `before` is the file as the migration's parent held it.
    pub files: Vec<FileChange>,
    pub warnings: Vec<PlanWarning>,
}

impl RestorePlan {
    pub fn hash(&self) -> String {
        let value = json!({
            "hash_of": "mdkg-migrate-restore-history",
            "format": 1,
            "base": self.base,
            "graph_id": self.graph_id,
            "migrations": self.restored.migrations,
            "files": self.files.iter().map(|f| json!({
                "path": f.path, "before": f.before, "after": f.after,
            })).collect::<Vec<_>>(),
            "warnings": self.warnings,
        });
        Digest::of_record(&value)
            .unwrap_or_else(|_| Digest::of_bytes(value.to_string().as_bytes()))
            .to_string()
    }
}

fn unsupported(why: impl Into<String>) -> GraphError {
    GraphError::failure(Code::MigrateUnsupported, why)
}

pub fn show(top: &Path, commit: &str, path: &str) -> Option<Vec<u8>> {
    gitops::run(top, &["show", &format!("{commit}:{path}")], None).ok()
}

pub fn has_graph_id(bytes: Option<Vec<u8>>) -> bool {
    bytes
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v.get("graph_id").is_some())
}

/// The graph id a config names, if any.
fn graph_id_of(bytes: Option<Vec<u8>>) -> Option<String> {
    bytes
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("graph_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// The commits on `rev`'s history that migrated the graph `graph_id`
/// (when given) whose config is `config_rel`: each commit whose config
/// names that graph with a parent whose config names none, with that
/// parent, newest first. Every parent of a merge counts, and history isn't
/// simplified, so a migration merged in (with `--no-ff`, or beside another
/// branch's own migration) is found on every side it came by.
pub fn migration_commits(
    top: &Path,
    rev: &str,
    config_rel: &str,
    graph_id: Option<&str>,
) -> Result<Vec<(String, String)>, GraphError> {
    let out = gitops::run(
        top,
        &[
            "--literal-pathspecs",
            "log",
            "--full-history",
            "--format=%H %P",
            rev,
            "--",
            config_rel,
        ],
        None,
    )
    .map_err(git_error)?;
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let mut words = line.split_whitespace();
        let Some(commit) = words.next() else {
            continue;
        };
        let Some(id) = graph_id_of(show(top, commit, config_rel)) else {
            continue;
        };
        if graph_id.is_some_and(|g| g != id) {
            continue;
        }
        for parent in words {
            if graph_id_of(show(top, parent, config_rel)).is_some() {
                continue;
            }
            // The parent must be readable: a shallow clone lacks it.
            if gitops::tree_of(top, parent).is_err() {
                return Err(unsupported(format!(
                    "the migration commit {commit}'s parent isn't in this clone"
                ))
                .with_hint("fetch the whole history first: git fetch --unshallow"));
            }
            found.push((commit.to_string(), parent.to_string()));
        }
    }
    Ok(found)
}

/// What a restore's own commit says first.
const RESTORE_SUBJECT: &str = "mdkg: restore the work history the migration moved out of git";

/// The file a restore leaves in the migration folder while HEAD names its
/// commit but git's index may not: written before HEAD moves, removed once
/// the index has moved too. It holds `<commit> <parent>`.
const PENDING: &str = "restore-pending";

/// The file that lists the staged copies a restore of a plan makes, as
/// JSON (`{"plan": ..., "staged": [...]}`, so any path git allows fits):
/// only those are its own to replace or remove.
const STAGED: &str = "restore-staged";

/// What a restore's pending file says, read without changing anything.
enum Pending {
    /// No pending file.
    None,
    /// One that names no unfinished restore: HEAD moved on, or the index
    /// caught up.
    Stale,
    /// HEAD names the restore, and git's index is still its parent's tree.
    Repairable { commit: String, parent: String },
    /// HEAD names the restore, and git's index is neither tree: someone
    /// staged something on top.
    Blocked { commit: String },
}

fn pending(top: &Path, root_dir: &Path) -> Result<Pending, GraphError> {
    let Ok(text) = fs::read_to_string(root_dir.join(PENDING)) else {
        return Ok(Pending::None);
    };
    let mut words = text.split_whitespace();
    let (Some(commit), Some(parent)) = (words.next(), words.next()) else {
        return Ok(Pending::Stale);
    };
    let head = gitops::head(top).map_err(git_error)?;
    let index = gitops::index_tree(top).map_err(git_error)?;
    if head != commit || index == gitops::tree_of(top, commit).map_err(git_error)? {
        return Ok(Pending::Stale);
    }
    if index != gitops::tree_of(top, parent).map_err(git_error)? {
        return Ok(Pending::Blocked {
            commit: commit.to_string(),
        });
    }
    Ok(Pending::Repairable {
        commit: commit.to_string(),
        parent: parent.to_string(),
    })
}

/// Removes the pending file; one that can't be removed is an error, since
/// a later unstaging would be taken for an unfinished restore.
fn clear_pending(root_dir: &Path) -> Result<(), GraphError> {
    let path = root_dir.join(PENDING);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GraphError::failure(
            Code::IoWriteFailed,
            format!("removing {}: {e}", path.display()),
        )
        .with_hint("the restore is done: remove that file by hand")),
    }
}

/// A warning for a dry run in a worktree whose last restore didn't finish
/// moving git's index. Changes nothing: it runs without the lock.
pub fn unfinished_warning(root: &Path) -> Option<PlanWarning> {
    let repo = Repo::open(root).ok()?;
    let root_dir = super::apply::folder_root(&repo).ok()?;
    let message = match pending(&repo.top, &root_dir).ok()? {
        Pending::Repairable { commit, .. } => format!(
            "the restore {commit} is committed but git's index isn't updated: run \
             `migrate --restore-history --apply` with any plan hash before committing anything"
        ),
        Pending::Blocked { commit } => format!(
            "the restore {commit} is committed but git's index isn't updated, and holds \
             changes staged since: `git reset -q` (your files stay) puts the index at the \
             restore, then stage your changes again"
        ),
        Pending::None | Pending::Stale => return None,
    };
    Some(PlanWarning {
        code: "migrate.restore_unfinished".into(),
        path: None,
        message,
    })
}

/// Under the lock, settles a restore whose index update didn't happen (its
/// pending file says so): HEAD names the restore commit, but git's index
/// is still its parent's tree, so the next commit would delete what it
/// restored. Moves the index to the commit. Refuses when changes were
/// staged on top, which are the user's to sort out.
fn repair_index(top: &Path, root_dir: &Path) -> Result<bool, GraphError> {
    match pending(top, root_dir)? {
        Pending::None => Ok(false),
        Pending::Stale => clear_pending(root_dir).map(|()| false),
        Pending::Repairable { commit, parent } => {
            gitops::reset_index(top, &parent, &commit).map_err(git_error)?;
            clear_pending(root_dir)?;
            Ok(true)
        }
        Pending::Blocked { commit } => Err(GraphError::conflict(
            Code::MigrateDirty,
            format!(
                "the restore {commit} is committed but git's index isn't updated, and holds \
                 changes staged since"
            ),
        )
        .with_hint(
            "`git reset -q` (your files stay) puts the index at the restore; stage your \
             changes again after",
        )),
    }
}

/// The digest of the regular file at `path`; `None` for anything else (a
/// link, a folder) or nothing.
fn file_digest(path: &Path) -> Option<String> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|_| fs::read(path).ok())
        .map(|b| Digest::of_bytes(&b).to_string())
}

/// Where the restore of plan `hash` writes `rel` before it takes its
/// place, beside it.
fn staging_rel(rel: &str, hash: &str) -> String {
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let staged = format!(".{name}.mdkg-restore-{}", super::apply::short(hash));
    if dir.is_empty() {
        staged
    } else {
        format!("{dir}/{staged}")
    }
}

/// The staged copies the restore of `hash` listed as its own.
fn own_staged(root_dir: &Path, hash: &str) -> BTreeSet<String> {
    let Some(list) = fs::read(root_dir.join(STAGED))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return BTreeSet::new();
    };
    if list["plan"] != hash {
        return BTreeSet::new();
    }
    list["staged"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

/// Removes the staged copies the list names when it's another plan's than
/// `current`: each a regular file named for that plan, so only the list's
/// own. Then drops the list once none is left.
fn drop_other_staged(top: &Path, root_dir: &Path, current: Option<&str>) {
    let Some(list) = fs::read(root_dir.join(STAGED))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return;
    };
    let Some(plan) = list["plan"].as_str() else {
        return;
    };
    if Some(plan) == current {
        return;
    }
    let suffix = format!(".mdkg-restore-{}", super::apply::short(plan));
    let staged: Vec<String> = list["staged"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str())
        .filter(|s| s.ends_with(&suffix))
        .map(str::to_string)
        .collect();
    for s in &staged {
        let path = top.join(s);
        // Never through a folder that's a link (to outside the checkout).
        if plain_ancestors(top, s).is_ok() && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
        {
            fs::remove_file(&path).ok();
        }
    }
    forget_staged(top, root_dir, &staged);
}

/// Drops the list of staged copies once none is left: one whose removal
/// failed stays listed, so the next run still owns it.
fn forget_staged(top: &Path, root_dir: &Path, staged: &[String]) {
    if staged
        .iter()
        .all(|s| fs::symlink_metadata(top.join(s)).is_err())
    {
        fs::remove_file(root_dir.join(STAGED)).ok();
    }
}

/// The test hook: debug builds stop dead part way through writing a
/// staged copy (`MDKG_MIGRATE_RESTORE_CRASH_MID_FILE=1`).
fn mid_file_hook(out: &mut fs::File, bytes: &[u8]) {
    if cfg!(debug_assertions) && std::env::var_os("MDKG_MIGRATE_RESTORE_CRASH_MID_FILE").is_some() {
        out.write_all(&bytes[..bytes.len() / 2]).ok();
        std::process::exit(86);
    }
}

/// Writes `bytes` to `path`, which must not exist: in full at `staged`
/// first (one of this restore's own, so an earlier copy there is
/// replaced), then linked into place, so `path` never holds part of them.
fn write_new(path: &Path, staged: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match fs::remove_file(staged) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut out = fs::File::create_new(staged)?;
    mid_file_hook(&mut out, bytes);
    out.write_all(bytes).and_then(|()| out.sync_all())?;
    drop(out);
    // A link fails if `path` exists: never over a file someone made in
    // between. Where links aren't supported, nothing is written: a rename
    // can't refuse a file made after a check.
    let placed = fs::hard_link(staged, path);
    fs::remove_file(staged).ok();
    placed
}

/// Refuses a path whose folders, from the repository's top, aren't all
/// plain folders (a link would take the write elsewhere), or that exists.
fn plain_ancestors(top: &Path, rel: &str) -> Result<(), GraphError> {
    let mut at = top.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(GraphError::conflict(
                    Code::MigrateDirty,
                    format!(
                        "{} is a link or a file, where {rel} needs a folder",
                        at.display()
                    ),
                )
                .with_hint("move it out of the way and commit that first"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(GraphError::failure(
                    Code::IoReadFailed,
                    format!("reading {}: {e}", at.display()),
                ));
            }
        }
    }
    Ok(())
}

/// The test hooks: debug builds stop dead after writing this many files
/// (`MDKG_MIGRATE_RESTORE_CRASH_AFTER_WRITES=1`), or fail the index update
/// after HEAD moved (`MDKG_MIGRATE_RESTORE_FAIL_INDEX`), as a crash or a
/// held `index.lock` would.
fn restore_hook(written: usize) {
    if cfg!(debug_assertions)
        && std::env::var("MDKG_MIGRATE_RESTORE_CRASH_AFTER_WRITES").ok()
            == Some(written.to_string())
    {
        std::process::exit(86);
    }
}

/// Plans the restore for the graph at `root`.
pub fn plan_restore(root: &Path) -> Result<(Repo, RestorePlan), GraphError> {
    let repo = Repo::open(root)?;
    let top = repo.top.clone();
    let config_rel = format!("{}{}", repo.prefix, config::CONFIG_PATH);
    let raw: Value = fs::read(top.join(&config_rel))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            GraphError::failure(Code::ConfigUnreadable, "the graph's config can't be read")
        })?;
    let Some(graph_id) = raw
        .get("graph_id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(GraphError::not_found(
            Code::MigrateNotMigrated,
            "this graph isn't migrated yet: there's no history to bring back",
        )
        .with_hint("`migrate --dry-run` plans a migration that keeps the work history in git"));
    };
    let cfg = config::load(root)?;
    if cfg.work_committed == config::WorkCommitted::None {
        return Err(unsupported(
            "this graph keeps its work local (work.location \"local\"): its history would go into git",
        ));
    }
    let ws_folders: BTreeMap<String, String> = workspace_folders(&cfg, &repo.prefix)
        .into_iter()
        .map(|(ws, d)| (ws, format!("{d}/")))
        .collect();
    let folders: Vec<String> = ws_folders.values().cloned().collect();
    let ws_of = |path: &str| {
        ws_folders
            .iter()
            .find(|(_, f)| path.starts_with(f.as_str()))
            .map(|(ws, _)| ws.clone())
            .unwrap_or_default()
    };
    let mdkg_prefix = folders.first().cloned().unwrap_or_default();
    let root_folder = format!("{}.mdkg/", repo.prefix);
    let home_ws = ws_folders
        .iter()
        .find(|(_, f)| **f == root_folder)
        .or_else(|| ws_folders.iter().next())
        .map(|(ws, _)| ws.clone())
        .unwrap_or_default();
    let base = gitops::head(&top).map_err(git_error)?;
    let now = gitops::ls_tree_under(&top, &base, &folders).map_err(git_error)?;
    // The ids the graph's nodes have now, by `<workspace>:<legacy id>`.
    let known = committed_ids(&top, &now, &ws_of)?;

    let mut restored = Restored::default();
    // Path to (the file as the parent held it, the commit, its kind).
    let mut wanted: BTreeMap<String, (String, String, Plane)> = BTreeMap::new();
    let mut events: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut v2_uuids: BTreeMap<String, String> = BTreeMap::new();
    let mut parent_config: Option<String> = None;
    let mut warnings = Vec::new();
    for (commit, parent) in migration_commits(&top, "HEAD", &config_rel, Some(&graph_id))? {
        if !restored.migrations.contains(&commit) {
            restored.migrations.push(commit.clone());
        }
        let before = gitops::ls_tree_under(&top, &parent, &folders).map_err(git_error)?;
        let after = gitops::ls_tree_under(&top, &commit, &folders).map_err(git_error)?;
        let gone: Vec<(&String, &gitops::Entry)> = before
            .iter()
            .filter(|(p, e)| e.kind == "blob" && !after.contains_key(*p) && !now.contains_key(*p))
            .collect();
        let oids: Vec<String> = gone.iter().map(|(_, e)| e.oid.clone()).collect();
        let blobs = gitops::read_blobs(&top, &oids).map_err(git_error)?;
        if parent_config.is_none() {
            parent_config =
                show(&top, &parent, &config_rel).and_then(|b| String::from_utf8(b).ok());
        }
        for ((path, _), blob) in gone.into_iter().zip(blobs) {
            if wanted.contains_key(path) || events.contains_key(path) {
                continue; // A newer migration's copy came first.
            }
            let Ok(text) = String::from_utf8(blob) else {
                continue;
            };
            let in_events = folders
                .iter()
                .any(|f| path.starts_with(&format!("{f}work/events/")) && path.ends_with(".jsonl"));
            if in_events {
                events.insert(path.clone(), (text, commit.clone()));
                continue;
            }
            if !path.ends_with(".md") {
                continue;
            }
            let fields = frontmatter_lines(&text);
            let node_type = fields.get("type").copied().unwrap_or_default();
            let plane = classify(node_type, fields.get("status").copied());
            if !matches!(plane, Plane::Checkpoint | Plane::CheckResult) {
                continue;
            }
            let legacy = fields.get("id").copied().unwrap_or_default().to_string();
            if known.contains_key(&format!("{}:{legacy}", ws_of(path))) {
                warnings.push(PlanWarning {
                    code: "migrate.history_present".into(),
                    path: Some(path.clone()),
                    message: format!("the graph already has {legacy}, so it isn't restored"),
                });
                continue;
            }
            if let Some(uuid) = fields
                .get("node_id")
                .filter(|u| mdkg_format::ids::is_identity_uuid(u))
            {
                v2_uuids.insert(path.clone(), (*uuid).to_string());
            }
            wanted.insert(path.clone(), (text, commit.clone(), plane));
        }
    }

    // Planned as a migration that keeps its history in git would have
    // planned them, against the ids the graph has now.
    let parent_config = parent_config.unwrap_or_default();
    let v2 = serde_json::from_str::<Value>(&parent_config)
        .ok()
        .and_then(|v| v.get("schema_version").and_then(Value::as_i64))
        == Some(2);
    let source = Source {
        base: base.clone(),
        graph_id: graph_id.clone(),
        graph_id_minted: false,
        v2: v2.then(|| plan::V2Source {
            manifest: TextFile::default(),
            uuids: v2_uuids,
        }),
        config: TextFile {
            path: config_rel.clone(),
            content: parent_config,
            ws: String::new(),
        },
        nodes: wanted
            .iter()
            .map(|(path, (text, _, _))| NodeFile {
                path: path.clone(),
                content: text.clone(),
                ws: ws_of(path),
            })
            .collect(),
        mdkg_prefix,
        graph_prefix: repo.prefix.clone(),
        folders: ws_folders.clone(),
        home_ws,
        workspaces: cfg.workspaces.iter().map(|w| w.alias.clone()).collect(),
        subgraphs: subgraph_ids(root, &raw, true),
        history: History::Git,
        known_ids: known.clone(),
        ..Source::default()
    };
    let p = plan::plan(&source).map_err(|e| unsupported(e.message).at(e.path, None))?;
    let mut files = Vec::new();
    for f in p.files.into_iter().filter(|f| wanted.contains_key(&f.path)) {
        match wanted[&f.path].2 {
            Plane::Checkpoint => restored.checkpoints += 1,
            _ => restored.check_results += 1,
        }
        files.push(f);
    }
    // A node whose content needed no change is planned as no change:
    // it comes back as it was.
    for (path, (text, _, plane)) in &wanted {
        if files.iter().any(|f| &f.path == path) {
            continue;
        }
        match plane {
            Plane::Checkpoint => restored.checkpoints += 1,
            _ => restored.check_results += 1,
        }
        files.push(new_file(path, text));
    }
    for (path, (text, _)) in &events {
        restored.event_logs += 1;
        files.push(new_file(path, text));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    warnings.extend(
        p.warnings
            .into_iter()
            .filter(|w| w.path.as_ref().is_none_or(|p| wanted.contains_key(p))),
    );
    warnings.sort();
    warnings.dedup();
    Ok((
        repo,
        RestorePlan {
            base,
            graph_id,
            restored,
            files,
            warnings,
        },
    ))
}

/// Each committed node's id by `<workspace>:<legacy id>`, read from the
/// listing `entries` of HEAD's graph folders.
fn committed_ids(
    top: &Path,
    entries: &BTreeMap<String, gitops::Entry>,
    ws_of: &dyn Fn(&str) -> String,
) -> Result<BTreeMap<String, String>, GraphError> {
    let nodes: Vec<(&String, &gitops::Entry)> = entries
        .iter()
        .filter(|(p, e)| e.kind == "blob" && p.ends_with(".md"))
        .collect();
    let oids: Vec<String> = nodes.iter().map(|(_, e)| e.oid.clone()).collect();
    let blobs = gitops::read_blobs(top, &oids).map_err(git_error)?;
    let mut ids = BTreeMap::new();
    for ((path, _), blob) in nodes.into_iter().zip(blobs) {
        let Ok(text) = std::str::from_utf8(&blob) else {
            continue;
        };
        let fields = frontmatter_lines(text);
        if let (Some(id), Some(legacy)) = (fields.get("id"), fields.get("legacy_id")) {
            ids.insert(format!("{}:{legacy}", ws_of(path)), (*id).to_string());
        }
    }
    Ok(ids)
}

fn new_file(path: &str, text: &str) -> FileChange {
    let digest = Digest::of_bytes(text.as_bytes()).to_string();
    FileChange {
        path: path.to_string(),
        action: Action::Write,
        before: digest.clone(),
        after: Some(digest),
        content: Some(text.to_string()),
    }
}

/// Applies the restore plan with `hash`: writes its files and commits them
/// on HEAD. Returns the plan and the commit (`None` when there was nothing
/// to restore).
pub fn apply_restore(root: &Path, hash: &str) -> Result<(RestorePlan, Option<String>), GraphError> {
    let root_dir = super::apply::folder_root(&Repo::open(root)?)?;
    let _lock = super::apply::lock(&root_dir)?;
    let repaired = repair_index(&Repo::open(root)?.top, &root_dir)?;
    let (repo, p) = plan_restore(root)?;
    let top = repo.top.clone();
    let now = p.hash();
    // Copies an earlier run of another plan listed (one that finished but
    // couldn't remove them) go now.
    drop_other_staged(
        &top,
        &root_dir,
        (!p.files.is_empty()).then_some(now.as_str()),
    );
    if p.files.is_empty() {
        let head = repaired.then(|| gitops::head(&top).ok()).flatten();
        return Ok((p, head));
    }
    if now != hash {
        return Err(GraphError::conflict(
            Code::MigratePlanMismatch,
            format!("the restore plan is {now}, not the one given"),
        )
        .with_hint("run `migrate --restore-history --dry-run` again and review the new plan"));
    }
    // Staged work would be dropped by the index update after the commit.
    let index = gitops::index_tree(&top).map_err(git_error)?;
    if index != gitops::tree_of(&top, &p.base).map_err(git_error)? {
        return Err(GraphError::conflict(
            Code::MigrateDirty,
            "git's index holds staged changes (a submodule pointer counts)",
        )
        .with_hint("commit or unstage them (git restore --staged <path>) first"));
    }
    // Only this plan's own files may be there already, as it writes them
    // (an earlier run that stopped part way): each one whole, or a staged
    // copy this plan listed as its own before making it.
    let ours: BTreeMap<&str, &FileChange> = p.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let own = own_staged(&root_dir, &now);
    let dirty: Vec<String> = gitops::dirty(&top)
        .map_err(git_error)?
        .into_iter()
        .filter(|d| !own.contains(d))
        .filter(|d| {
            ours.get(d.as_str())
                .is_none_or(|f| file_digest(&top.join(d)) != f.after)
        })
        .collect();
    if !dirty.is_empty() {
        let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
        return Err(GraphError::conflict(
            Code::MigrateDirty,
            format!("the worktree has uncommitted changes: {}", shown.join(", ")),
        )
        .with_hint("commit or stash them first: the restore commits what it writes"));
    }
    let edits: Vec<(String, Edit<'_>)> = p
        .files
        .iter()
        .map(|f| {
            (
                f.path.clone(),
                Edit::Write(f.content.as_deref().unwrap_or_default().as_bytes()),
            )
        })
        .collect();
    let tree = gitops::edited_tree(&top, &p.base, &edits).map_err(git_error)?;
    let r = &p.restored;
    let message = format!(
        "{RESTORE_SUBJECT} (plan {now})\n\n\
         {} checkpoints and {} finished tests come back as nodes, and {} event logs as \
         they were, from the migration commit(s) {}. Undo with `git revert`.\n",
        r.checkpoints,
        r.check_results,
        r.event_logs,
        r.migrations.join(", ")
    );
    for f in &p.files {
        plain_ancestors(&top, &f.path)?;
    }
    // The staged copies are listed before any is made: one already there
    // that the list doesn't name is someone else's.
    let staged: Vec<String> = p.files.iter().map(|f| staging_rel(&f.path, &now)).collect();
    if let Some(theirs) = staged
        .iter()
        .find(|s| !own.contains(*s) && fs::symlink_metadata(top.join(s)).is_ok())
    {
        return Err(GraphError::conflict(
            Code::MigrateDirty,
            format!("{theirs} is in the way of the restore's staged copy"),
        )
        .with_hint("move it elsewhere first"));
    }
    let list = json!({ "plan": now, "staged": staged }).to_string();
    super::apply::write_atomic(&root_dir.join(STAGED), list.as_bytes()).map_err(|e| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("writing {}: {e}", root_dir.join(STAGED).display()),
        )
    })?;
    let commit = gitops::commit_tree(&top, &tree, &p.base, &message).map_err(git_error)?;
    let mut written = Vec::new();
    let result = (|| -> Result<(), GraphError> {
        for (f, staged) in p.files.iter().zip(&staged) {
            let path = top.join(&f.path);
            let staged = top.join(staged);
            if file_digest(&path) == f.after {
                // In place before a stop that left its staged copy.
                fs::remove_file(&staged).ok();
                continue;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    GraphError::failure(
                        Code::IoWriteFailed,
                        format!("creating {}: {e}", parent.display()),
                    )
                })?;
            }
            // Never over a file someone made in between (or a link).
            write_new(
                &path,
                &staged,
                f.content.as_deref().unwrap_or_default().as_bytes(),
            )
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    GraphError::conflict(Code::MigrateDirty, format!("writing {}: {e}", f.path))
                } else {
                    GraphError::failure(Code::IoWriteFailed, format!("writing {}: {e}", f.path))
                        .with_hint(
                            "the restore writes each file whole, then links it into place; \
                             a filesystem without hard links can't take it",
                        )
                }
            })?;
            written.push(path.clone());
            restore_hook(written.len());
        }
        let problem =
            match config::load(root).and_then(|cfg| mdkg_graph::validate::validate(root, &cfg)) {
                Ok(receipt) => receipt.errors.into_iter().next(),
                Err(e) => Some(*e.diagnostic),
            };
        if let Some(d) = problem {
            return Err(GraphError::failure(
                Code::MigrateStepFailed,
                format!(
                    "the graph with its history restored doesn't validate: {} {}{}",
                    d.code.as_str(),
                    d.message,
                    d.path.map(|p| format!(" ({p})")).unwrap_or_default()
                ),
            )
            .with_hint("nothing was committed; report the plan"));
        }
        Ok(())
    })();
    if let Err(e) = result {
        for path in written {
            fs::remove_file(path).ok();
        }
        for s in &staged {
            fs::remove_file(top.join(s)).ok();
        }
        forget_staged(&top, &root_dir, &staged);
        return Err(e);
    }
    forget_staged(&top, &root_dir, &staged);
    // Said before HEAD moves, so a run that stops after it finishes the
    // index update next time, and only then.
    super::apply::write_atomic(
        &root_dir.join(PENDING),
        format!("{commit} {}\n", p.base).as_bytes(),
    )
    .map_err(|e| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("writing {}: {e}", root_dir.join(PENDING).display()),
        )
    })?;
    gitops::move_head(&top, &commit, &p.base, "mdkg migrate --restore-history").map_err(|e| {
        fs::remove_file(root_dir.join(PENDING)).ok();
        git_error(e)
    })?;
    // If this fails, HEAD names the restore but the index doesn't: the next
    // run (`--apply` again) finishes it (`repair_index`).
    if cfg!(debug_assertions) && std::env::var_os("MDKG_MIGRATE_RESTORE_FAIL_INDEX").is_some() {
        return Err(GraphError::failure(
            Code::IoGitFailed,
            "the test hook failed the index update",
        ));
    }
    gitops::reset_index(&top, &p.base, &commit)
        .map_err(git_error)
        .map_err(|e| {
            e.with_hint(
                "the restore is committed but git's index isn't updated: run the same \
                 `migrate --restore-history --apply` again before committing anything",
            )
        })?;
    clear_pending(&root_dir)?;
    Ok((p, Some(commit)))
}
