use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use mdkg_coord::{Changes, Command, LeaseId, Mode, Outcome, Scope, Settlement, Status};
use mdkg_coord_sqlite::{Store, git};
use mdkg_format::{Code, Digest, NodeId};
use mdkg_graph::{GraphError, config, index};
use serde::{Deserialize, Serialize};

use super::plan::{self, Plan, bad, read};
use super::{LocalizeData, data};
use crate::coord::{self, Repo, git_error, now_ms, store_error};
use crate::migrate::apply::{copy_tree, exchange, inventory, write_atomic};

const MESSAGE: &str = "mdkg: localize work definitions";
const PLAN_MISMATCH: &str = "HEAD does not match the localize plan; nothing recorded";
const CAP: u64 = 1 << 30;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    plan: Plan,
    commit: Option<String>,
    #[serde(default)]
    commit_pending: bool,
    statuses_copied: bool,
    complete: bool,
}

fn write_error(e: impl std::fmt::Display) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, format!("localize: {e}"))
}

fn hash_key(hash: &str) -> Result<&str, GraphError> {
    let h = hash
        .strip_prefix("sha256:")
        .ok_or_else(|| bad("pass the full sha256 plan hash from --dry-run"))?;
    if h.len() != 64
        || !h
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(bad("pass the full sha256 plan hash from --dry-run"));
    }
    Ok(h)
}

/// Check each directory below the already checked common directory. This
/// refuses linked metadata instead of creating files through it.
fn directories(common: &Path, relative: &Path) -> Result<PathBuf, GraphError> {
    let mut path = common.to_path_buf();
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            return Err(bad("invalid localize metadata path"));
        };
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => {
                File::open(path.parent().unwrap_or(common))
                    .and_then(|f| f.sync_all())
                    .map_err(write_error)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(write_error(e)),
        }
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            return Err(bad("localize metadata must use plain directories"));
        }
    }
    Ok(path)
}

fn lock(path: &Path) -> Result<File, GraphError> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(write_error)?;
    let file = File::from(fd);
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(bad("the localize lock isn't a regular file"));
    }
    file.try_lock()
        .map_err(|_| bad("another localize is running for this checkout"))?;
    Ok(file)
}

fn save(folder: &Path, journal: &Journal) -> Result<(), GraphError> {
    let bytes = serde_json::to_vec(journal).map_err(write_error)?;
    write_atomic(&folder.join("journal.json"), &bytes).map_err(write_error)
}

fn marker(p: &Plan) -> String {
    format!(".localize-{}", p.hash().trim_start_matches("sha256:"))
}

fn swapped(p: &Plan) -> bool {
    read(&p.root.join(".mdkg").join(marker(p)), 128).is_ok_and(|b| b == p.hash().as_bytes())
}

fn inventory_is(path: &Path, expected: &str) -> Result<(), GraphError> {
    plan::plain_tree(path)?;
    if inventory(path).map_err(git_error)? != expected {
        return Err(bad(
            "the graph changed since localize planned it; no changed files will be overwritten",
        ));
    }
    Ok(())
}

fn publish(path: &Path, content: &[u8]) -> Result<(), GraphError> {
    if fs::symlink_metadata(path).is_ok() {
        return if read(path, CAP)? == content {
            Ok(())
        } else {
            Err(bad(
                "a local work file differs from the plan; it was not overwritten",
            ))
        };
    }
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let fd = rustix::fs::open(
            &tmp,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(write_error)?;
        let mut file = File::from(fd);
        file.write_all(content)
            .and_then(|()| file.sync_all())
            .map_err(write_error)?;
        match fs::hard_link(&tmp, path) {
            Ok(()) => (),
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists && read(path, CAP)? == content => {
            }
            Err(e) => return Err(write_error(e)),
        }
        File::open(
            path.parent()
                .ok_or_else(|| bad("local work has no parent"))?,
        )
        .and_then(|f| f.sync_all())
        .map_err(write_error)
    })();
    fs::remove_file(&tmp).ok();
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Prepared,
    Copied,
    Swapped,
}

/// Filesystem stages are independently repeatable. `after` is a private
/// test seam for interruptions; production always supplies a no-op.
fn materialize(
    p: &Plan,
    folder: &Path,
    after: &mut dyn FnMut(Step) -> Result<(), GraphError>,
) -> Result<(), GraphError> {
    let graph = p.root.join(".mdkg");
    let backup = folder.join("backup.mdkg");
    let stage = folder.join("staging.mdkg");
    if !backup.exists() {
        inventory_is(&graph, &p.inventory)?;
        copy_tree(&graph, &backup).map_err(write_error)?;
    }
    inventory_is(&backup, &p.inventory)?;
    if !swapped(p) {
        inventory_is(&graph, &p.inventory)?;
        if fs::symlink_metadata(&stage).is_ok() {
            plan::plain_tree(&stage)?;
            fs::remove_dir_all(&stage).map_err(write_error)?;
        }
        copy_tree(&backup, &stage).map_err(write_error)?;
        for n in &p.nodes {
            let rel = Path::new(&n.path)
                .strip_prefix(".mdkg")
                .map_err(write_error)?;
            fs::remove_file(stage.join(rel)).map_err(write_error)?;
        }
        write_atomic(&stage.join("config.json"), p.config_after.as_bytes()).map_err(write_error)?;
        write_atomic(&stage.join(marker(p)), p.hash().as_bytes()).map_err(write_error)?;
        after(Step::Prepared)?;
        for n in &p.nodes {
            publish(
                &p.local_work.join(format!("{}.md", n.id)),
                n.content.as_bytes(),
            )?;
        }
        after(Step::Copied)?;
        inventory_is(&graph, &p.inventory)?;
        exchange(&graph, &stage).map_err(write_error)?;
        File::open(folder)
            .and_then(|f| f.sync_all())
            .map_err(write_error)?;
        if let Err(error) = inventory_is(&stage, &p.inventory) {
            exchange(&graph, &stage).map_err(write_error)?;
            return Err(error);
        }
        after(Step::Swapped)?;
    }
    let cfg = config::load(&p.root)?;
    if cfg.work_committed != config::WorkCommitted::None {
        return Err(bad("the prepared graph isn't local"));
    }
    if read(&p.root.join(config::CONFIG_PATH), 4 << 20)? != p.config_after.as_bytes() {
        return Err(bad(
            "the localize config changed after the swap; it was not committed",
        ));
    }
    index::build(&p.root, &cfg)?;
    plan::validate(&p.root)?;
    Ok(())
}

struct Held {
    git: Vec<(String, Scope)>,
    taken: Vec<String>,
}

fn release(store: &mut Store, actor: &str, leases: &[String]) {
    for id in leases {
        store
            .execute(&Command::Release {
                owner: actor.into(),
                lease: LeaseId(id.clone()),
                now: now_ms(),
            })
            .ok();
    }
}

fn take(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    p: &Plan,
    held: &mut Held,
) -> Result<(), GraphError> {
    let mut paths: Vec<(&str, Option<String>)> = p
        .nodes
        .iter()
        .map(|n| (n.path.as_str(), Some(format!("root:{}", n.id))))
        .collect();
    paths.push((config::CONFIG_PATH, None));
    for (path, node) in paths {
        let scope = repo.scope_for(path);
        coord::check_scope_text(&scope)?;
        coord::require_authority_hold(repo, actor, &scope, node.as_deref())?;
        let state = store.coordinator().map_err(store_error)?;
        if !coord::missing_versions(repo, &state, &scope)?.is_empty() {
            return Err(bad("sync the graph's pending versions before localizing"));
        }
        let key = node.as_deref().map(|q| repo.node_key(&scope, q));
        if state.leases().any(|l| {
            l.status == Status::Active
                && l.confirmed
                && l.deadline > now_ms()
                && (l.scope.overlaps(&scope)
                    || key.as_deref().is_some_and(|k| l.node.as_deref() == Some(k)))
                && (l.owner != actor || l.scope != scope || l.mode != Mode::Exclusive)
        }) {
            return Err(bad(
                "a graph file is leased elsewhere or under a wider scope; release that lease before localizing",
            ));
        }
        let exact = state.leases().find(|l| {
            l.owner == actor
                && l.scope == scope
                && l.status == Status::Active
                && l.confirmed
                && l.deadline > now_ms()
        });
        let id = if let Some(l) = exact {
            store
                .execute(&Command::Heartbeat {
                    owner: actor.into(),
                    lease: l.id.clone(),
                    now: now_ms(),
                })
                .map_err(store_error)?;
            l.id.to_string()
        } else {
            if state.leaf(&scope).is_none()
                && let Some(content) =
                    git::file_digest(&repo.top, scope.as_str()).map_err(git_error)?
            {
                store
                    .execute(&Command::Register {
                        leaf: scope.clone(),
                        content,
                    })
                    .map_err(store_error)?;
            }
            match store
                .execute(&Command::Claim {
                    owner: actor.into(),
                    scope: scope.clone(),
                    mode: Mode::Exclusive,
                    now: now_ms(),
                    request: None,
                    node: key,
                })
                .map_err(store_error)?
            {
                Outcome::Lease { lease } => {
                    held.taken.push(lease.to_string());
                    lease.to_string()
                }
                Outcome::Refused { error } => return Err(coord::refusal(&error)),
                _ => return Err(bad("the localize lease wasn't granted")),
            }
        };
        held.git.push((id, scope));
    }
    Ok(())
}

fn copy_statuses(repo: &Repo, store: &mut Store, actor: &str, p: &Plan) -> Result<(), GraphError> {
    let state = store.coordinator().map_err(store_error)?;
    for n in &p.nodes {
        let Some(status) = state.work(&repo.scope_for(&n.path)) else {
            continue;
        };
        let id = NodeId::parse(&n.id).map_err(|_| bad("invalid work id"))?;
        let target = Scope::new(format!("node:{}", id.uuid()));
        if state.work(&target).is_none() {
            let result = store
                .execute(&Command::SetWork {
                    leaf: target,
                    status: Some(status.status.clone()),
                    changeset: None,
                    actor: actor.into(),
                    now: now_ms(),
                })
                .map_err(store_error)?;
            if let Outcome::Refused { error } = result {
                return Err(coord::refusal(&error));
            }
        }
    }
    Ok(())
}

fn commit_message(p: &Plan) -> String {
    format!("{MESSAGE}\n\nLocalize-plan: {}", p.hash())
}

/// Check the complete raw tree delta, using exact paths (not pathspecs).
/// Renames are disabled by the caller, so only the planned deletions and
/// one config modification can appear. Preserve the config's file mode.
fn delta_matches(raw: &[u8], config: &str, removals: &BTreeSet<String>, blob: &str) -> bool {
    let Some(raw) = raw.strip_suffix(b"\0") else {
        return false;
    };
    let mut remaining = removals.clone();
    let mut config_seen = false;
    let mut records = raw.split(|b| *b == 0);
    while let Some(header) = records.next() {
        let (Ok(header), Some(path)) = (std::str::from_utf8(header), records.next()) else {
            return false;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            return false;
        };
        let Some(header) = header.strip_prefix(':') else {
            return false;
        };
        let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
        let [old_mode, new_mode, old_oid, new_oid, status] = fields.as_slice() else {
            return false;
        };
        if !matches!(*old_mode, "100644" | "100755")
            || [old_oid, new_oid]
                .iter()
                .any(|oid| oid.len() != blob.len() || !oid.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return false;
        }
        if path == config {
            if config_seen || *status != "M" || new_mode != old_mode || *new_oid != blob {
                return false;
            }
            config_seen = true;
        } else if *status != "D"
            || *new_mode != "000000"
            || !new_oid.bytes().all(|b| b == b'0')
            || !remaining.remove(path)
        {
            return false;
        }
    }
    config_seen && remaining.is_empty()
}

/// Bind recovery to the immutable commit's parent, message AND exact tree
/// delta. A message-preserving amend cannot supply different changes.
fn expected_commit(repo: &Repo, p: &Plan, commit: &str) -> Result<bool, GraphError> {
    let output = git::command(&repo.top)
        .args(["log", "-1", "--format=%P%n%B", commit, "--"])
        .run(None)
        .map_err(git_error)?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !text.split_once('\n').is_some_and(|(parents, message)| {
            parents == p.base && message.trim_end() == commit_message(p)
        })
    {
        return Ok(false);
    }
    // No -w: compute the repository-format blob id without writing objects.
    let object = git::command(&repo.top)
        .args(["hash-object", "-t", "blob", "--stdin", "--no-filters"])
        .run(Some(p.config_after.as_bytes()))
        .map_err(git_error)?;
    let blob = String::from_utf8_lossy(&object.stdout);
    let blob = blob.trim();
    if !object.status.success()
        || !matches!(blob.len(), 40 | 64)
        || !blob.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(false);
    }
    let delta = git::command(&repo.top)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--raw",
            "-r",
            "-z",
            "--no-renames",
            "--no-abbrev",
            "--no-ext-diff",
            "--no-textconv",
            &p.base,
            commit,
            "--",
        ])
        .run(None)
        .map_err(git_error)?;
    let removals = p
        .nodes
        .iter()
        .map(|n| format!("{}{}", repo.prefix, n.path))
        .collect();
    Ok(delta.status.success()
        && delta_matches(
            &delta.stdout,
            &format!("{}{}", repo.prefix, config::CONFIG_PATH),
            &removals,
            blob,
        ))
}

/// The Git commit already exists. Finish its coordinator records without
/// asking Git to stage the work paths, which the commit has removed.
fn record_existing(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    leases: &[(String, Scope)],
    p: &Plan,
    commit: &str,
) -> Result<(), GraphError> {
    let source = git::head(&repo.top).map_err(git_error)?;
    if source.commit != commit || !expected_commit(repo, p, commit)? {
        return Err(bad(PLAN_MISMATCH));
    }
    // These digests come from committed blobs, independently of working
    // files that can be restored or removed after an amend.
    let committed =
        git::leaves(&repo.top, commit, &format!("{}.mdkg", repo.prefix)).map_err(git_error)?;
    for (lease, scope) in leases {
        let planned = if *scope == repo.scope_for(config::CONFIG_PATH) {
            Some(Digest::of_bytes(p.config_after.as_bytes()).to_string())
        } else {
            None
        };
        let content = committed.get(scope).cloned();
        if content != planned {
            return Err(bad(PLAN_MISMATCH));
        }
        let changes = Changes::from([(scope.clone(), content)]);
        // A previous attempt may have recorded some or all of the scopes.
        if store
            .coordinator()
            .map_err(store_error)?
            .changesets()
            .any(|c| {
                c.scope == *scope
                    && c.source.as_ref().is_some_and(|s| s.commit == commit)
                    && c.changes == changes
                    && !matches!(c.state, Settlement::Abandoned | Settlement::Conflicted)
            })
        {
            continue;
        }
        match store
            .execute(&Command::Commit {
                owner: actor.into(),
                lease: LeaseId(lease.clone()),
                changes,
                source: Some(source.clone()),
                now: now_ms(),
                landed: false,
            })
            .map_err(store_error)?
        {
            Outcome::Committed { .. } => (),
            Outcome::Refused { error } => return Err(coord::refusal(&error)),
            _ => return Err(bad("the localize commit wasn't recorded")),
        }
    }
    Ok(())
}

pub fn apply(repo: &Repo, actor: Option<String>, hash: &str) -> Result<LocalizeData, GraphError> {
    apply_after_commit(repo, actor, hash, &mut || Ok(()))
}

/// Private fault seam at the commit/journal boundary; no environment flag
/// or production input can enable it.
fn apply_after_commit(
    repo: &Repo,
    actor: Option<String>,
    hash: &str,
    after_commit: &mut dyn FnMut() -> Result<(), GraphError>,
) -> Result<LocalizeData, GraphError> {
    crate::graph_repair::require_writable(&repo.root)?;
    let hex = hash_key(hash)?;
    let common = git::common_dir(&repo.top).map_err(git_error)?;
    let root = fs::canonicalize(&repo.root).map_err(git_error)?;
    let root_key = Digest::of_bytes(root.as_os_str().as_encoded_bytes()).to_string();
    let relative = PathBuf::from("mdkg/localize").join(root_key.trim_start_matches("sha256:"));
    let base = directories(&common, &relative)?;
    let _guard = lock(&base.join("lock"))?;
    let folder = directories(&base, Path::new(hex))?;
    let journal_path = folder.join("journal.json");
    let mut journal = if journal_path.try_exists().map_err(git_error)? {
        serde_json::from_slice::<Journal>(&read(&journal_path, CAP)?)
            .map_err(|_| bad("the localize journal is unreadable"))?
    } else {
        let p = plan::build(repo)?;
        if p.hash() != hash {
            return Err(bad(
                "the localize plan changed; run --dry-run and review it again",
            ));
        }
        if p.already_local {
            return Ok(data(&p, true, None, None));
        }
        Journal {
            plan: p,
            commit: None,
            commit_pending: false,
            statuses_copied: false,
            complete: false,
        }
    };
    journal.plan.check(repo)?;
    if journal.plan.hash() != hash {
        return Err(bad("the localize journal doesn't match the requested plan"));
    }
    if journal.complete {
        if config::load(&root)?.work_committed != config::WorkCommitted::None {
            return Err(bad(
                "this conversion was completed, but the checkout is no longer local",
            ));
        }
        return Ok(data(
            &journal.plan,
            true,
            journal.commit.clone(),
            Some(folder.join("backup.mdkg").to_string_lossy().into_owned()),
        ));
    }
    let p = journal.plan.clone();
    git::ready_to_write(&repo.top).map_err(git_error)?;
    let unmerged = git::command(&repo.top)
        .args(["ls-files", "-u", "-z"])
        .run(None)
        .map_err(git_error)?;
    if !unmerged.status.success() || !unmerged.stdout.is_empty() {
        return Err(bad("resolve the index conflicts before localizing"));
    }
    let head = git::head(&repo.top).map_err(git_error)?.commit;
    if head != p.base {
        // A crash can follow the commit but precede saving its id.
        if (journal.commit.as_deref() != Some(head.as_str())
            && !(journal.commit.is_none() && journal.commit_pending && swapped(&p)))
            || !expected_commit(repo, &p, &head)?
        {
            return Err(bad(PLAN_MISMATCH));
        }
        journal.commit = Some(head.clone());
    }
    // Validate the same-filesystem requirement before publishing anything.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(root.join(".mdkg")).map_err(git_error)?.dev()
            != fs::metadata(&folder).map_err(git_error)?.dev()
        {
            return Err(bad(
                "localize requires .mdkg and the git common directory on one filesystem",
            ));
        }
    }
    let local_rel = p.local_work.strip_prefix(&common).map_err(write_error)?;
    directories(&common, local_rel)?;
    let _work_lock = crate::write::store_work_lock(&p.local_work)?;
    save(&folder, &journal)?;
    let actor = repo.actor(actor);
    crate::msg::check_actor_mailbox(&actor, true)?;
    let mut store = repo.store()?;
    repo.observe(&mut store, now_ms())?;
    if store.coordinator().map_err(store_error)?.leases().any(|l| {
        l.scope.as_str().starts_with("run:")
            && l.owner != actor
            && l.confirmed
            && l.status == Status::Active
            && l.deadline > now_ms()
    }) {
        return Err(bad("stop other agent turns before localizing the graph"));
    }
    let mut held = Held {
        git: Vec::new(),
        taken: Vec::new(),
    };
    let result = (|| {
        take(repo, &mut store, &actor, &p, &mut held)?;
        if !journal.statuses_copied {
            copy_statuses(repo, &mut store, &actor, &p)?;
            journal.statuses_copied = true;
            save(&folder, &journal)?;
        }
        if journal.commit.is_none() || swapped(&p) {
            materialize(&p, &folder, &mut |_| Ok(()))?;
        } else {
            if read(&root.join(config::CONFIG_PATH), 4 << 20)? != p.config_after.as_bytes()
                || p.nodes.iter().any(|n| root.join(&n.path).exists())
            {
                return Err(bad("the localized graph changed after its commit"));
            }
            index::build(&root, &config::load(&root)?)?;
        }
        // Recheck authority immediately before the commit, after the copy.
        let before_commit = git::head(&repo.top).map_err(git_error)?.commit;
        if before_commit != p.base && journal.commit.as_deref() != Some(before_commit.as_str()) {
            return Err(bad("HEAD moved while localize ran; it was not committed"));
        }
        inventory_is(&folder.join("staging.mdkg"), &p.inventory)?;
        let state = store.coordinator().map_err(store_error)?;
        for (id, scope) in &held.git {
            if !state.lease(&LeaseId(id.clone())).is_some_and(|l| {
                l.owner == actor
                    && l.confirmed
                    && l.status == Status::Active
                    && l.deadline > now_ms()
            }) {
                return Err(bad(
                    "a localize lease expired before the commit; resume to acquire it again",
                ));
            }
            coord::require_authority_hold(repo, &actor, scope, None)?;
        }
        if journal.commit.is_none() {
            journal.commit_pending = true;
            save(&folder, &journal)?;
            let paths = held
                .git
                .iter()
                .map(|(_, scope)| scope.to_string())
                .collect::<Vec<_>>();
            if !git::commit_paths(&repo.top, &paths, &commit_message(&p)).map_err(git_error)? {
                return Err(bad("the planned localize changes weren't committed"));
            }
            let current = git::head(&repo.top).map_err(git_error)?.commit;
            if current == p.base || !expected_commit(repo, &p, &current)? {
                return Err(bad(PLAN_MISMATCH));
            }
            after_commit()?;
            journal.commit = Some(current);
            save(&folder, &journal)?;
        }
        let commit = journal
            .commit
            .as_deref()
            .ok_or_else(|| bad("the localize commit is missing"))?;
        record_existing(repo, &mut store, &actor, &held.git, &p, commit)?;
        // The displaced tree stays in Git metadata beside the backup. No
        // temporary source copy can accidentally enter a later git add.
        let mark = root.join(".mdkg").join(marker(&p));
        if fs::symlink_metadata(&mark).is_ok() {
            fs::remove_file(&mark).map_err(write_error)?;
        }
        File::open(root.join(".mdkg"))
            .and_then(|f| f.sync_all())
            .map_err(write_error)?;
        journal.complete = true;
        journal.commit_pending = false;
        save(&folder, &journal)?;
        Ok(data(
            &p,
            true,
            journal.commit.clone(),
            Some(folder.join("backup.mdkg").to_string_lossy().into_owned()),
        ))
    })();
    release(&mut store, &actor, &held.taken);
    result.map_err(|e| e.with_hint("the backup and local files are retained; repeat the same work localize --apply hash to finish, or follow the documented rollback"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TASK: &str = "task-0192f3a0-0000-7000-8000-000000000001";
    const GOAL: &str = "goal-0192f3a0-0000-7000-8000-000000000002";

    struct Fixture {
        root: PathBuf,
        folder: PathBuf,
        p: Plan,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("mdkg-localize-unit-{}", uuid::Uuid::now_v7()));
            for path in [
                ".mdkg/work",
                ".git/objects",
                ".git/refs",
                ".git/mdkg/work/root",
                ".git/mdkg/localize/test",
            ] {
                fs::create_dir_all(root.join(path)).expect("fixture folder");
            }
            fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("fixture HEAD");
            fs::write(
                root.join(".git/config"),
                "[core]\nrepositoryformatversion = 0\nbare = false\n",
            )
            .expect("fixture Git config");
            let root = fs::canonicalize(root).expect("fixture root");
            let config = json!({"schema_version":3,"format_version":"0.7",
                "graph_id":"0192f3a0-0000-7000-8000-000000000000",
                "work":{"location":"git"},"workspaces":{"root":{}}})
            .to_string();
            fs::write(root.join(config::CONFIG_PATH), &config).expect("config");
            let content = format!(
                "---\nid: {TASK}\ntype: task\ntitle: Keep this work\nstatus: todo\npriority: 1\ncreated: 2026-10-27T00:00:00.000Z\nupdated: 2026-10-27T00:00:00.000Z\n---\n\n# Overview\nExact work bytes.\n"
            );
            fs::write(root.join(".mdkg/work/original.md"), &content).expect("task");
            let p = Plan {
                root: root.clone(),
                base: "a".repeat(40),
                inventory: inventory(&root.join(".mdkg")).expect("inventory"),
                local_work: root.join(".git/mdkg/work/root"),
                config_after: plan::local_config(&config).expect("local config"),
                config_before: config,
                nodes: vec![plan::Move {
                    id: TASK.into(),
                    path: ".mdkg/work/original.md".into(),
                    content,
                }],
                already_local: false,
            };
            Self {
                folder: root.join(".git/mdkg/localize/test"),
                root,
                p,
            }
        }
    }

    #[test]
    fn localize_config_keeps_options_and_removes_only_the_storage_alias() {
        let input = json!({"work":{"committed":"definitions","status_enum":["todo","done"]},
            "projections":["hooks:claude"],"agents":{"x":{"model":"model"}},"x-user":true});
        let output: serde_json::Value = serde_json::from_str(
            &plan::local_config(&input.to_string()).expect("valid localize fixture"),
        )
        .expect("valid localize fixture");
        let mut expected = input;
        expected["work"]
            .as_object_mut()
            .expect("valid localize fixture")
            .remove("committed");
        expected["work"]["location"] = json!("local");
        assert_eq!(output, expected);
        let output: serde_json::Value = serde_json::from_str(
            &plan::local_config("{\"schema_version\":3}").expect("valid localize fixture"),
        )
        .expect("valid localize fixture");
        assert_eq!(output["work"]["location"], "local");
    }

    #[test]
    fn localize_filesystem_stages_resume_without_changing_ids_or_bytes() {
        for stop in [Step::Prepared, Step::Copied, Step::Swapped] {
            let f = Fixture::new();
            let error = materialize(&f.p, &f.folder, &mut |at| {
                if at == stop {
                    Err(bad("simulated interruption"))
                } else {
                    Ok(())
                }
            })
            .expect_err("interrupt");
            assert!(error.diagnostic.message.contains("simulated"));
            materialize(&f.p, &f.folder, &mut |_| Ok(())).expect("resume");
            assert!(!f.root.join(&f.p.nodes[0].path).exists());
            assert_eq!(
                fs::read(f.p.local_work.join(format!("{TASK}.md")))
                    .expect("valid localize fixture"),
                f.p.nodes[0].content.as_bytes()
            );
            assert_eq!(
                fs::read_to_string(f.root.join(config::CONFIG_PATH))
                    .expect("valid localize fixture"),
                f.p.config_after
            );
            inventory_is(&f.folder.join("backup.mdkg"), &f.p.inventory).expect("backup unchanged");
            assert!(swapped(&f.p));
        }
    }

    #[test]
    fn localize_never_overwrites_existing_or_changed_work() {
        let f = Fixture::new();
        let target = f.p.local_work.join(format!("{TASK}.md"));
        fs::write(&target, "someone else's local work").expect("valid localize fixture");
        assert!(materialize(&f.p, &f.folder, &mut |_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(&target).expect("valid localize fixture"),
            "someone else's local work"
        );
        assert!(f.root.join(&f.p.nodes[0].path).is_file());
        fs::remove_file(&target).expect("valid localize fixture");
        materialize(&f.p, &f.folder, &mut |at| {
            if at == Step::Swapped {
                Err(bad("stop"))
            } else {
                Ok(())
            }
        })
        .expect_err("interrupted localize");
        let revised = f.p.nodes[0].content.clone() + "\nNew work after the swap.\n";
        fs::write(&target, &revised).expect("valid localize fixture");
        materialize(&f.p, &f.folder, &mut |_| Ok(())).expect("already swapped");
        assert_eq!(
            fs::read_to_string(&target).expect("valid localize fixture"),
            revised
        );
    }

    #[test]
    fn localize_rechecks_the_source_before_the_atomic_swap() {
        let f = Fixture::new();
        let source = f.root.join(&f.p.nodes[0].path);
        let revised = f.p.nodes[0].content.clone() + "\nA concurrent source edit.\n";
        let error = materialize(&f.p, &f.folder, &mut |at| {
            if at == Step::Copied {
                fs::write(&source, &revised).expect("valid localize fixture");
            }
            Ok(())
        })
        .expect_err("changed source");
        assert!(error.diagnostic.message.contains("changed"));
        assert_eq!(
            fs::read_to_string(&source).expect("valid localize fixture"),
            revised
        );
        assert!(!swapped(&f.p));
    }

    #[test]
    fn localize_checks_design_references_to_the_prospective_local_nodes() {
        let f = Fixture::new();
        let cfg = config::load(&f.root).expect("valid localize fixture");
        let graph = index::build(&f.root, &cfg).expect("valid localize fixture");
        plan::preview(&graph, &f.p.local_work).expect("work without incoming design ref");
        let goal = format!(
            "---\nid: {GOAL}\ntype: goal\ntitle: A goal\nstatus: todo\ngoal_state: paused\ngoal_condition: Finished\npriority: 1\nrefs: [{TASK}]\ncreated: 2026-10-27T00:00:00.000Z\nupdated: 2026-10-27T00:00:00.000Z\n---\n\n# Overview\nGoal.\n"
        );
        fs::write(f.root.join(".mdkg/work/goal.md"), goal).expect("valid localize fixture");
        let graph = index::build(&f.root, &cfg).expect("valid localize fixture");
        let error = plan::preview(&graph, &f.p.local_work).expect_err("incoming ref");
        assert_eq!(error.diagnostic.code, Code::EdgePrivateTarget);
    }

    #[test]
    fn localize_checks_body_links_using_validation_resolution() {
        let f = Fixture::new();
        let cfg = config::load(&f.root).expect("config");
        let goal = format!(
            "---\nid: {GOAL}\ntype: goal\ntitle: A goal\nstatus: todo\ngoal_state: paused\ngoal_condition: Finished\npriority: 1\ncreated: 2026-10-27T00:00:00.000Z\nupdated: 2026-10-27T00:00:00.000Z\n---\n\n# Overview\n"
        );
        let graph_id = "0192f3a0-0000-7000-8000-000000000000";
        for target in [
            TASK.to_string(),
            TASK.replacen("task-", "bug-", 1),
            format!("mdkg://{graph_id}/{TASK}"),
        ] {
            fs::write(
                f.root.join(".mdkg/work/goal.md"),
                format!("{goal}[work]({target})\n"),
            )
            .expect("goal body link");
            let graph = index::build(&f.root, &cfg).expect("index");
            let error = plan::preview(&graph, &f.p.local_work).expect_err("incoming body link");
            assert_eq!(error.diagnostic.code, Code::EdgePrivateTarget);
            assert_eq!(
                error.diagnostic.node.as_deref(),
                Some(format!("root:{GOAL}").as_str())
            );
            assert!(error.diagnostic.message.contains(TASK));
        }
        for body in [
            format!("`[work]({TASK})`\n"),
            format!("```md\n[work]({TASK})\n```\n"),
            format!("[foreign](mdkg://0192f3a0-0000-7000-8000-000000000099/{TASK})\n"),
        ] {
            fs::write(f.root.join(".mdkg/work/goal.md"), format!("{goal}{body}"))
                .expect("non-local body link");
            let graph = index::build(&f.root, &cfg).expect("index");
            plan::preview(&graph, &f.p.local_work).expect("not an incoming body link");
        }
        fs::remove_file(f.root.join(".mdkg/work/goal.md")).expect("remove goal");
        let work_path = f.root.join(&f.p.nodes[0].path);
        fs::write(
            &work_path,
            format!("{}[self]({TASK})\n", f.p.nodes[0].content),
        )
        .expect("work body link");
        let graph = index::build(&f.root, &cfg).expect("index");
        plan::preview(&graph, &f.p.local_work).expect("work can link to work");
    }

    #[cfg(unix)]
    #[test]
    fn localize_refuses_linked_metadata_and_work_files() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let link = f.root.join(".mdkg/work/link.md");
        symlink(f.root.join(&f.p.nodes[0].path), &link).expect("valid localize fixture");
        assert!(plan::plain_tree(&f.root.join(".mdkg")).is_err());
        let target = f.p.local_work.join(format!("{TASK}.md"));
        symlink(f.root.join(&f.p.nodes[0].path), &target).expect("valid localize fixture");
        assert!(publish(&target, b"changed").is_err());
        assert_eq!(
            fs::read_to_string(f.root.join(&f.p.nodes[0].path)).expect("valid localize fixture"),
            f.p.nodes[0].content
        );
    }

    #[test]
    fn localize_plan_hashes_are_content_bound_and_path_components_are_strict() {
        let mut f = Fixture::new();
        let hash = f.p.hash();
        assert!(hash_key(&hash).is_ok());
        f.p.nodes[0].content.push('x');
        assert_ne!(hash, f.p.hash());
        for hash in [
            "",
            "../other",
            "sha256:../other",
            "sha256:AA",
            "sha256:1234",
        ] {
            assert!(hash_key(hash).is_err());
        }
        assert!(directories(&f.root, Path::new("../outside")).is_err());
    }

    #[test]
    fn localize_requires_the_exact_planned_tree_delta() {
        let config = "nested/.mdkg/config.json";
        let work = "nested/.mdkg/work/Task [one].md";
        let removals = BTreeSet::from([work.to_string()]);
        for size in [40, 64] {
            let old = "1".repeat(size);
            let blob = "2".repeat(size);
            let zero = "0".repeat(size);
            let config_change = format!(":100644 100644 {old} {blob} M\0{config}\0");
            let deletion = format!(":100644 000000 {old} {zero} D\0{work}\0");
            let valid = format!("{config_change}{deletion}");
            assert!(delta_matches(valid.as_bytes(), config, &removals, &blob));
            assert!(delta_matches(
                format!("{deletion}{config_change}").as_bytes(),
                config,
                &removals,
                &blob
            ));
            assert!(delta_matches(
                config_change.as_bytes(),
                config,
                &BTreeSet::new(),
                &blob
            ));
            for invalid in [
                String::new(),
                config_change.clone(),                    // missing removal
                deletion.clone(),                         // missing config
                valid.trim_end_matches('\0').to_string(), // truncated record
                format!("{valid}{deletion}"),             // duplicate removal
                format!("{valid}{config_change}"),        // duplicate config
                format!("{valid}:000000 100644 {zero} {old} A\0extra.txt\0"),
                config_change.replace(&blob, &old) + &deletion, // wrong config blob
                config_change.replacen("100644 100644", "100644 100755", 1) + &deletion,
                valid.replace(work, "nested/.mdkg/work/task [one].md"), // exact Git spelling
                valid.replace(" D\0", " M\0"),
                valid.replace(" D\0", " R100\0"),
                valid.replace(&zero, &old), // deletion has a new blob
                valid.replace(&format!("{old} {zero}"), &format!("not-an-id {zero}")),
            ] {
                assert!(
                    !delta_matches(invalid.as_bytes(), config, &removals, &blob),
                    "accepted {invalid:?}"
                );
            }
        }
    }

    /// Git-writing acceptance test: compiled in restricted turns, run by
    /// the central runner. The child isolates all library-spawned Git and
    /// store operations without changing the parallel test environment.
    #[test]
    fn localize_git_commit_resumes_after_the_commit_journal_write_fails() {
        const TEST: &str =
            "work::apply::tests::localize_git_commit_resumes_after_the_commit_journal_write_fails";
        if std::env::var("MDKG_LOCALIZE_GIT_TEST").as_deref() != Ok(TEST) {
            let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .env_clear()
                .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .env("MDKG_LEASE_TTL_MS", "120000")
                .env("MDKG_LOCALIZE_GIT_TEST", TEST)
                .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
                .output()
                .expect("isolated Git-backed test");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && text.contains("1 passed"),
                "{text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        fn git(root: &Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .expect("fixture Git");
            assert!(output.status.success(), "{args:?}: {output:?}");
            String::from_utf8(output.stdout)
                .expect("Git text")
                .trim()
                .into()
        }
        for tamper in ["none", "hash", "extra", "missing removal", "config blob"] {
            let f = Fixture::new();
            git(&f.root, &["init", "-q", "-b", "main"]);
            git(&f.root, &["add", ".mdkg"]);
            git(&f.root, &["commit", "-qm", "shared graph"]);
            git(&f.root, &["checkout", "-qb", "convert"]);
            let repo = Repo::open(&f.root).expect("fixture repo");
            let p = plan::build(&repo).expect("plan");
            let hash = p.hash();
            let common = git::common_dir(&repo.top).expect("common");
            let root_key = Digest::of_bytes(p.root.as_os_str().as_encoded_bytes()).to_string();
            let folder = common
                .join("mdkg/localize")
                .join(root_key.trim_start_matches("sha256:"))
                .join(hash.trim_start_matches("sha256:"));
            let blocked_write = folder.join(format!(".journal.json.tmp-{}", std::process::id()));
            let before: usize = git(&f.root, &["rev-list", "--count", "HEAD"])
                .parse()
                .expect("commit count");
            let failure = apply_after_commit(&repo, Some("localizer".into()), &hash, &mut || {
                // Fail exactly the next atomic journal write, after Git
                // has committed and before its id can be persisted.
                fs::create_dir(&blocked_write).expect("block journal temp file");
                Ok(())
            })
            .expect_err("one failed write");
            assert_eq!(failure.diagnostic.code, Code::IoWriteFailed);
            fs::remove_dir(&blocked_write).expect("clear one failed write");
            let committed = git(&f.root, &["rev-parse", "HEAD"]);
            assert_ne!(committed, p.base);
            assert_eq!(
                git(&f.root, &["rev-list", "--count", "HEAD"]),
                (before + 1).to_string()
            );
            let journal_before = read(&folder.join("journal.json"), CAP).expect("journal");
            let pending: Journal = serde_json::from_slice(&journal_before).expect("journal JSON");
            assert!(pending.commit.is_none() && pending.commit_pending && !pending.complete);
            // Reopen as a new CLI invocation would, after work.location
            // and the graph directory have changed.
            let retry_repo = Repo::open(&f.root).expect("reopened local repo");
            if tamper != "none" {
                match tamper {
                    "extra" => {
                        fs::write(f.root.join("extra.txt"), "unplanned committed file\n")
                            .expect("extra file");
                        git(&f.root, &["add", "extra.txt"]);
                    }
                    "missing removal" => {
                        fs::write(f.root.join(&p.nodes[0].path), &p.nodes[0].content)
                            .expect("restore work into commit");
                        git(&f.root, &["add", &p.nodes[0].path]);
                    }
                    "config blob" => {
                        fs::write(
                            f.root.join(config::CONFIG_PATH),
                            p.config_after.clone() + "\n",
                        )
                        .expect("different config blob");
                        git(&f.root, &["add", config::CONFIG_PATH]);
                    }
                    _ => (),
                }
                if tamper == "hash" {
                    git(
                        &f.root,
                        &[
                            "commit",
                            "--amend",
                            "--no-verify",
                            "--no-gpg-sign",
                            "-qm",
                            &format!("{MESSAGE}\n\nLocalize-plan: sha256:{}", "0".repeat(64)),
                        ],
                    );
                } else {
                    git(
                        &f.root,
                        &[
                            "commit",
                            "--amend",
                            "--no-edit",
                            "--no-verify",
                            "--no-gpg-sign",
                            "-q",
                        ],
                    );
                    assert_eq!(
                        git(&f.root, &["log", "-1", "--format=%B"]),
                        commit_message(&p)
                    );
                    assert_eq!(git(&f.root, &["log", "-1", "--format=%P"]), p.base);
                    // Hide the amendment from working-file checks. The
                    // committed tree, alone, must decide recovery.
                    match tamper {
                        "extra" => {
                            fs::remove_file(f.root.join("extra.txt")).expect("restore working tree")
                        }
                        "missing removal" => fs::remove_file(f.root.join(&p.nodes[0].path))
                            .expect("remove working copy"),
                        "config blob" => {
                            fs::write(f.root.join(config::CONFIG_PATH), &p.config_after)
                                .expect("restore working config")
                        }
                        _ => unreachable!("known tamper mode"),
                    }
                }
                let foreign = git(&f.root, &["rev-parse", "HEAD"]);
                let error = apply(&retry_repo, Some("localizer".into()), &hash)
                    .expect_err("commit differs from the plan");
                assert_eq!(error.diagnostic.message, PLAN_MISMATCH, "{tamper}");
                assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), foreign);
                assert_eq!(
                    read(&folder.join("journal.json"), CAP).expect("retained journal"),
                    journal_before
                );
                assert_eq!(
                    retry_repo
                        .store()
                        .expect("store")
                        .coordinator()
                        .expect("state")
                        .changesets()
                        .count(),
                    0,
                    "{tamper} recorded changes"
                );
                continue;
            }
            let store = retry_repo.store().expect("store");
            let records = store.coordinator().expect("state").changesets().count();
            drop(store);
            assert_eq!(records, 0, "the fault precedes coordinator recording");
            let done = apply(&retry_repo, Some("localizer".into()), &hash)
                .expect("resume without committing removed paths");
            assert_eq!(done.commit.as_deref(), Some(committed.as_str()));
            assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), committed);
            assert_eq!(
                git(&f.root, &["rev-list", "--count", "HEAD"]),
                (before + 1).to_string()
            );
            assert_eq!(
                retry_repo
                    .store()
                    .expect("store")
                    .coordinator()
                    .expect("state")
                    .changesets()
                    .count(),
                p.nodes.len() + 1
            );
            let mut finished: Journal =
                serde_json::from_slice(&read(&folder.join("journal.json"), CAP).expect("journal"))
                    .expect("journal JSON");
            assert!(finished.complete && !finished.commit_pending);
            assert!(!swapped(&p));
            assert_eq!(
                fs::read(p.local_work.join(format!("{TASK}.md"))).expect("local work"),
                f.p.nodes[0].content.as_bytes()
            );
            // A failure persisting completion can leave these records in
            // place. Recording-only recovery must not duplicate them.
            finished.complete = false;
            finished.commit_pending = true;
            save(&folder, &finished).expect("incomplete final journal");
            apply(
                &Repo::open(&f.root).expect("reopen"),
                Some("localizer".into()),
                &hash,
            )
            .expect("resume previously recorded commit");
            assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), committed);
            assert_eq!(
                retry_repo
                    .store()
                    .expect("store")
                    .coordinator()
                    .expect("state")
                    .changesets()
                    .count(),
                p.nodes.len() + 1
            );
        }
    }
}
