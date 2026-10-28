//! `mdkg migrate`: brings a 0.6.0 graph onto 0.7.0, migrating identities
//! and separating knowledge from work history.
//!
//! - `--dry-run` prints the plan and its hash, and writes the full plan and
//!   the `--rewrite-refs` patch for review.
//! - `--apply <hash>` applies exactly that plan in one commit, after
//!   backing up the graph, the store and the base commit.
//! - `--rollback` reverts it from the backups.
//! - `--status` shows how far the last migration got.

pub mod apply;
mod classify;
mod gather;
mod gitops;
mod mint;
pub mod plan;
mod records;
mod rewrite;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::coord::{Repo, git_error};
use crate::output::{self, Report};
use apply::{Ctx, Journal, Stage, StoreImport};
use plan::{Action, FileChange, Plan, PlanWarning};

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "apply", "rollback", "status"])))]
pub struct MigrateArgs {
    /// Print the plan and its hash, and write the full plan for review;
    /// change nothing in the graph.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply the plan with this hash (from --dry-run) in one commit, or
    /// finish one that stopped part way.
    #[arg(long, value_name = "HASH")]
    pub apply: Option<String>,
    /// Revert the migration: the old graph, commit and store come back.
    #[arg(long)]
    pub rollback: bool,
    /// Show how far the last migration got.
    #[arg(long)]
    pub status: bool,
    /// With --apply: also apply the patch to AGENTS.md, CLAUDE.md, skills
    /// and scripts that --dry-run wrote for review.
    #[arg(long, requires = "apply")]
    pub rewrite_refs: bool,
    /// With --rollback: restore the store even though it changed since the
    /// migration (the changes since are lost).
    #[arg(long, requires = "rollback")]
    pub force: bool,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// How many of each.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Counts {
    /// Nodes that stay, with new ids.
    pub nodes: usize,
    /// Of those, tier C nodes kept as plain nodes.
    pub tier_c: usize,
    /// Checkpoints that became log records.
    pub checkpoints: usize,
    /// Finished tests that became check results.
    pub check_results: usize,
    /// Every log record: those, plus legacy events and review records.
    pub records: usize,
    pub legacy_events: usize,
    pub review_records: usize,
    pub files_written: usize,
    pub files_deleted: usize,
    /// Files outside the graph the patch changes.
    pub patch_files: usize,
}

/// The backups an apply takes.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Backups {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_snapshot: Option<String>,
    pub graph_copy: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MigrateData {
    /// The plan's hash.
    pub plan: String,
    /// planned (dry run), or the migration's stage: done, rolled-back, or
    /// where it stopped.
    pub stage: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub graph_id: String,
    pub counts: Counts,
    /// Changes inside the graph.
    pub files: Vec<FileChange>,
    /// Changes outside it (applied only with --rewrite-refs).
    pub patch: Vec<FileChange>,
    pub warnings: Vec<PlanWarning>,
    /// The full plan, with every new file's content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
    /// The --rewrite-refs patch, as a unified diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backups: Option<Backups>,
}

/// A plan as it's kept on disk: the plan, and every new file's content.
#[derive(Debug, Serialize, Deserialize)]
struct StoredPlan {
    hash: String,
    plan: Plan,
    contents: BTreeMap<String, String>,
}

fn store_plan(path: &Path, p: &Plan, hash: &str) -> Result<(), GraphError> {
    let contents = p
        .files
        .iter()
        .chain(&p.patch)
        .filter_map(|f| Some((f.path.clone(), f.content.clone()?)))
        .collect();
    let stored = StoredPlan {
        hash: hash.to_string(),
        plan: p.clone(),
        contents,
    };
    let bytes = serde_json::to_vec(&stored).unwrap_or_default();
    apply::write_atomic(path, &bytes)
        .map_err(|e| GraphError::failure(Code::IoWriteFailed, format!("writing the plan: {e}")))
}

fn load_plan(path: &Path, hash: &str) -> Result<Plan, GraphError> {
    let bad = |why: String| {
        GraphError::failure(
            Code::MigrateStepFailed,
            format!("the stored plan {} {why}", path.display()),
        )
    };
    let bytes = fs::read(path).map_err(|e| bad(format!("can't be read: {e}")))?;
    let stored: StoredPlan =
        serde_json::from_slice(&bytes).map_err(|e| bad(format!("can't be read: {e}")))?;
    let mut p = stored.plan;
    for f in p.files.iter_mut().chain(p.patch.iter_mut()) {
        f.content = stored.contents.get(&f.path).cloned();
    }
    if p.hash() != hash {
        return Err(bad("doesn't match its hash".into()));
    }
    Ok(p)
}

fn counts(p: &Plan) -> Counts {
    use records::RecordKind;
    let kind = |k: RecordKind| p.records.iter().filter(|r| r.kind == k).count();
    Counts {
        nodes: p.nodes.iter().filter(|n| n.id.is_some()).count(),
        tier_c: p.nodes.iter().filter(|n| n.plane == "tier-c").count(),
        checkpoints: kind(RecordKind::LegacyCheckpoint),
        check_results: kind(RecordKind::CheckResult),
        records: p.records.len(),
        legacy_events: kind(RecordKind::LegacyEvent),
        review_records: kind(RecordKind::ReviewRecord),
        files_written: p.files.iter().filter(|f| f.action == Action::Write).count(),
        files_deleted: p
            .files
            .iter()
            .filter(|f| f.action == Action::Delete)
            .count(),
        patch_files: p.patch.len(),
    }
}

fn data(
    p: &Plan,
    hash: &str,
    stage: &str,
    folder: Option<&Path>,
    journal: Option<&Journal>,
) -> MigrateData {
    let file = |name: &str| {
        folder
            .map(|f| f.join(name))
            .filter(|f| f.exists())
            .map(|f| f.display().to_string())
    };
    MigrateData {
        plan: hash.to_string(),
        stage: stage.to_string(),
        base: p.base.clone(),
        commit: journal.and_then(|j| j.commit.clone()),
        graph_id: p.graph_id.clone(),
        counts: counts(p),
        files: p.files.clone(),
        patch: p.patch.clone(),
        warnings: p.warnings.clone(),
        plan_file: file("plan.json"),
        patch_file: file("rewrite-refs.patch"),
        backups: journal.map(|j| Backups {
            tag: j.tag.clone(),
            store_snapshot: j.snapshot.as_ref().map(|s| s.display().to_string()),
            graph_copy: j.folder.join("mdkg").display().to_string(),
        }),
    }
}

fn print(d: &MigrateData) {
    let c = &d.counts;
    println!("migrate plan {} ({})", d.plan, d.stage);
    println!("  base {}, graph_id {}", d.base, d.graph_id);
    if let Some(commit) = &d.commit {
        println!("  commit {commit}");
    }
    println!(
        "  nodes: {} keep their place with new ids ({} tier C kept as plain nodes)",
        c.nodes, c.tier_c
    );
    println!(
        "  to the log: {} records ({} checkpoints, {} check results, {} legacy events, {} review records)",
        c.records, c.checkpoints, c.check_results, c.legacy_events, c.review_records
    );
    println!(
        "  files: {} rewritten, {} deleted; patch outside the graph: {} file(s)",
        c.files_written, c.files_deleted, c.patch_files
    );
    let mut by_code: BTreeMap<&str, usize> = BTreeMap::new();
    for w in &d.warnings {
        *by_code.entry(w.code.as_str()).or_default() += 1;
    }
    for (code, n) in by_code {
        println!("  warning {code}: {n}");
    }
    // Key conversions change what a node holds: each one is listed.
    for w in d
        .warnings
        .iter()
        .filter(|w| w.code == "migrate.key_converted")
    {
        println!("    {}", w.message);
    }
    if let Some(f) = &d.plan_file {
        println!("  full plan: {f}");
    }
    if let Some(f) = &d.patch_file {
        println!("  patch: {f}");
    }
    if let Some(b) = &d.backups {
        if let Some(t) = &b.tag {
            println!("  backup tag: {t}");
        }
        if let Some(s) = &b.store_snapshot {
            println!("  backup store: {s}");
        }
        println!("  backup graph: {}", b.graph_copy);
    }
    if d.stage == "planned" {
        println!("apply with: mdkg migrate --apply {}", d.plan);
    }
}

fn emit(json: bool, report: Report<MigrateData>) -> u8 {
    output::emit("migrate", json, report, print)
}

pub fn run(root: &Path, args: &MigrateArgs) -> u8 {
    let result = if args.dry_run {
        dry_run(root)
    } else if let Some(hash) = &args.apply {
        apply_plan(root, hash, args.rewrite_refs)
    } else if args.rollback {
        rollback(root, args.force)
    } else {
        status(root)
    };
    match result {
        Ok(r) => emit(args.json, r),
        Err(e) => emit(args.json, Report::failed(e)),
    }
}

/// The single graph folder this migration replaces, relative to the
/// repository's top (`.mdkg/` or `sub/.mdkg/`). 0.7.0 migrates a graph of
/// one workspace, `.mdkg` at the graph's root; it reads and writes any
/// other in its 0.6 form, and a migration that keeps workspaces is planned
/// for 0.7.1.
fn graph_folder(repo: &Repo, root: &Path) -> Result<(String, PathBuf), GraphError> {
    let v2 = mdkg_graph::identity_v2::is_identity_v2(root);
    let cfg = if v2 {
        mdkg_graph::config::load_identity_v2(root)?
    } else {
        mdkg_graph::config::load(root)?
    };
    let enabled: Vec<_> = cfg.workspaces.iter().filter(|w| w.enabled).collect();
    // What to do meanwhile: 0.7.0 reads and writes a 0.6 graph as it is,
    // but refuses an identity v2 one everywhere but here.
    let meanwhile = if v2 {
        "keep using mdkg 0.6.x on this graph: it's in identity format v2, which \
         0.7.0's other commands refuse"
    } else {
        "keep using this graph as it is: 0.7.0 reads and writes it in its 0.6 form"
    };
    let unsupported = |why: String| {
        GraphError::failure(Code::MigrateUnsupported, why).with_hint(format!(
            "mdkg 0.7.0 migrates only a graph of one workspace, .mdkg at the graph's \
             root. Until a migration that keeps workspaces (planned for 0.7.1), \
             {meanwhile}. See docs/migrating-from-0.6.md"
        ))
    };
    match enabled.as_slice() {
        [ws] if Path::new(&ws.path) == Path::new(".") && ws.mdkg_dir == ".mdkg" => {}
        [_] => {
            return Err(unsupported(
                "the graph's workspace isn't .mdkg at the graph root".into(),
            ));
        }
        _ => {
            return Err(unsupported(format!(
                "the graph has {} enabled workspaces",
                enabled.len()
            )));
        }
    }
    let dir = root.join(".mdkg");
    let meta = fs::symlink_metadata(&dir)
        .map_err(|e| GraphError::failure(Code::IoReadFailed, format!("reading .mdkg: {e}")))?;
    if !meta.is_dir() {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            ".mdkg is a link or a file, not a folder",
        ));
    }
    let links = apply::links_in(&dir)
        .map_err(|e| GraphError::failure(Code::IoReadFailed, format!("reading .mdkg: {e}")))?;
    if !links.is_empty() {
        let shown: Vec<&str> = links.iter().take(5).map(String::as_str).collect();
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            format!(
                "the graph folder holds symbolic links: {}",
                shown.join(", ")
            ),
        )
        .with_hint("replace each link with the file or folder it names, commit, and plan again"));
    }
    Ok((format!("{}.mdkg/", repo.prefix), dir))
}

/// Refuses a graph that's already on 0.7.0.
fn not_migrated(p: &plan::Source) -> Result<(), GraphError> {
    let migrated = (!p.graph_id_minted && p.v2.is_none())
        || p.nodes.iter().any(|n| {
            n.content
                .lines()
                .take_while(|l| !l.is_empty())
                .any(|l| l.starts_with("legacy_id:"))
        });
    if migrated {
        return Err(GraphError::conflict(
            Code::MigrateAlreadyMigrated,
            "this graph already has a graph_id or migrated nodes",
        )
        .with_hint("there's nothing to migrate; `migrate --status` shows the last migration"));
    }
    Ok(())
}

fn dry_run(root: &Path) -> Result<Report<MigrateData>, GraphError> {
    let (repo, source) = gather::gather(root, true)?;
    not_migrated(&source)?;
    let (rel_prefix, _) = graph_folder(&repo, root)?;
    let p = plan::plan(&source).map_err(plan_error)?;
    check_inside(&p, &rel_prefix)?;
    let hash = p.hash();
    let folder = apply::folder_root(&repo)?.join(apply::short(&hash));
    fs::create_dir_all(&folder).map_err(|e| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("creating {}: {e}", folder.display()),
        )
    })?;
    store_plan(&folder.join("plan.json"), &p, &hash)?;
    write_patch(&repo, &p, &folder)?;
    Ok(Report::ok(data(&p, &hash, "planned", Some(&folder), None)))
}

fn plan_error(e: plan::PlanError) -> GraphError {
    GraphError::failure(Code::MigrateUnsupported, e.message).at(e.path, None)
}

/// Every change inside the graph lies in its folder.
fn check_inside(p: &Plan, rel_prefix: &str) -> Result<(), GraphError> {
    if let Some(f) = p.files.iter().find(|f| !f.path.starts_with(rel_prefix)) {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            format!("{} lies outside the graph folder", f.path),
        ));
    }
    Ok(())
}

/// Writes the `--rewrite-refs` patch as one unified diff.
fn write_patch(repo: &Repo, p: &Plan, folder: &Path) -> Result<(), GraphError> {
    if p.patch.is_empty() {
        return Ok(());
    }
    let mut text = String::new();
    for f in &p.patch {
        let old = fs::read(repo.top.join(&f.path)).unwrap_or_default();
        let new = f.content.as_deref().unwrap_or_default();
        text.push_str(&gitops::diff(&repo.top, &f.path, &old, new.as_bytes()).map_err(git_error)?);
    }
    apply::write_atomic(&folder.join("rewrite-refs.patch"), text.as_bytes())
        .map_err(|e| GraphError::failure(Code::IoWriteFailed, format!("writing the patch: {e}")))
}

/// The test hook: debug builds stop dead after the stage this names, as a
/// crash would (`MDKG_MIGRATE_CRASH_AFTER=swapped`).
fn crash_hook() -> impl FnMut(Stage) -> Result<(), GraphError> {
    let at = if cfg!(debug_assertions) {
        std::env::var("MDKG_MIGRATE_CRASH_AFTER").ok()
    } else {
        None
    };
    move |stage| {
        if at.as_deref() == Some(stage.as_str()) {
            std::process::exit(86);
        }
        Ok(())
    }
}

fn store_path(repo: &Repo) -> Result<PathBuf, GraphError> {
    mdkg_coord_sqlite::git::store_path(&repo.top).map_err(git_error)
}

fn apply_plan(
    root: &Path,
    hash: &str,
    rewrite_refs: bool,
) -> Result<Report<MigrateData>, GraphError> {
    let repo = Repo::open(root)?;
    let root_dir = apply::folder_root(&repo)?;
    let _lock = apply::lock(&root_dir)?;
    let folder = root_dir.join(apply::short(hash));
    let journal = Journal::load(&root_dir)?;
    let unfinished = journal
        .as_ref()
        .filter(|j| !matches!(j.stage, Stage::Done | Stage::RolledBack));
    let (p, mut journal) = match unfinished {
        // A rollback that stopped finishes as a rollback, never as an apply.
        Some(j) if j.stage == Stage::RollingBack => {
            return Err(GraphError::conflict(
                Code::MigrateInProgress,
                format!("the migration with plan {} is being rolled back", j.plan),
            )
            .with_hint("finish undoing it with `migrate --rollback`, then plan again"));
        }
        Some(j) if j.plan != hash => {
            return Err(GraphError::conflict(
                Code::MigrateInProgress,
                format!(
                    "the migration with plan {} stopped at {}",
                    j.plan,
                    j.stage.as_str()
                ),
            )
            .with_hint(format!(
                "finish it with `migrate --apply {}`, or undo it with `migrate --rollback`",
                j.plan
            )));
        }
        Some(j) => {
            if rewrite_refs != j.rewrite_refs {
                return Err(GraphError::usage(
                    Code::UsageInvalidArguments,
                    "finish the migration with the same --rewrite-refs choice it started with",
                ));
            }
            (load_plan(&folder.join("plan.json"), hash)?, j.clone())
        }
        None => {
            if let Some(j) = &journal
                && j.stage == Stage::Done
                && j.plan == hash
            {
                let p = load_plan(&folder.join("plan.json"), hash)?;
                return Ok(Report::ok(data(&p, hash, "done", Some(&folder), Some(j))));
            }
            let (_, source) = gather::gather(root, true)?;
            not_migrated(&source)?;
            let (rel_prefix, graph_dir) = graph_folder(&repo, root)?;
            let p = plan::plan(&source).map_err(plan_error)?;
            check_inside(&p, &rel_prefix)?;
            let now = p.hash();
            if now != hash {
                return Err(GraphError::conflict(
                    Code::MigratePlanMismatch,
                    format!("the graph's plan is {now}, not the one given"),
                )
                .with_hint("run `migrate --dry-run` again and review the new plan"));
            }
            // Staged changes git status doesn't show (a submodule pointer,
            // with submodules ignored) would be dropped by the index reset
            // after the commit: the index must be exactly the base's tree.
            let index = gitops::index_tree(&repo.top).map_err(git_error)?;
            if index != gitops::tree_of(&repo.top, &p.base).map_err(git_error)? {
                return Err(GraphError::conflict(
                    Code::MigrateDirty,
                    "git's index holds staged changes (a submodule pointer counts)",
                )
                .with_hint("commit or unstage them (git restore --staged <path>) first"));
            }
            let dirty = gitops::dirty(&repo.top).map_err(git_error)?;
            if !dirty.is_empty() {
                let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
                return Err(GraphError::conflict(
                    Code::MigrateDirty,
                    format!("the worktree has uncommitted changes: {}", shown.join(", ")),
                )
                .with_hint("commit or stash them first: the migration commits the whole graph"));
            }
            // The records go to the coordinator store, and history leaves
            // git only once they're there: a store this build can't open
            // (or upgrade) stops the migration before anything changes.
            if !p.records.is_empty() {
                repo.store().map_err(|e| {
                    let mut e = e;
                    e.diagnostic.hint = Some(
                        "the migration's log records go to the coordinator store; run it \
                         with a build that can open the store (an installed mdkg, or \
                         --allow-store-upgrade on a scratch copy)"
                            .into(),
                    );
                    e
                })?;
            }
            fs::create_dir_all(&folder).map_err(|e| {
                GraphError::failure(
                    Code::IoWriteFailed,
                    format!("creating {}: {e}", folder.display()),
                )
            })?;
            // The new graph is built one level down, in a root of its own
            // beside the graph (`.mdkg.migrate-<plan>/.mdkg`), so it can be
            // validated as a graph before the swap.
            let staging = graph_dir
                .with_file_name(format!(".mdkg.migrate-{}", apply::short(hash)))
                .join(".mdkg");
            // An earlier attempt's copy (rolled back since) is of an older
            // folder: this attempt takes its own.
            for stale in [
                folder.join("mdkg"),
                folder.join("mdkg.partial"),
                staging.parent().map(Path::to_path_buf).unwrap_or_default(),
            ] {
                if fs::symlink_metadata(&stale).is_ok() {
                    fs::remove_dir_all(&stale).map_err(|e| {
                        GraphError::failure(
                            Code::IoWriteFailed,
                            format!("clearing {}: {e}", stale.display()),
                        )
                    })?;
                }
            }
            store_plan(&folder.join("plan.json"), &p, hash)?;
            write_patch(&repo, &p, &folder)?;
            let j = Journal {
                plan: hash.to_string(),
                base: p.base.clone(),
                commit: None,
                graph_dir: graph_dir.clone(),
                rel_prefix: rel_prefix.clone(),
                staging,
                folder: folder.clone(),
                snapshot: None,
                tag: None,
                tag_created: false,
                rewrite_refs,
                inventory: None,
                stage: Stage::Started,
            };
            j.save(&root_dir)?;
            (p, j)
        }
    };
    let store = store_path(&repo)?;
    let rel_prefix = journal.rel_prefix.clone();
    let mut sink = StoreImport { repo: &repo };
    let mut hook = crash_hook();
    let mut ctx = Ctx {
        repo: &repo,
        plan: &p,
        rel_prefix,
        store,
        sink: &mut sink,
        after_stage: &mut hook,
    };
    apply::advance(&mut ctx, &root_dir, &mut journal)?;
    Ok(Report::ok(data(
        &p,
        hash,
        journal.stage.as_str(),
        Some(&folder),
        Some(&journal),
    )))
}

fn rollback(root: &Path, force: bool) -> Result<Report<MigrateData>, GraphError> {
    let repo = Repo::open(root)?;
    let root_dir = apply::folder_root(&repo)?;
    let _lock = apply::lock(&root_dir)?;
    let Some(mut journal) = Journal::load(&root_dir)? else {
        return Err(GraphError::not_found(
            Code::MigrateNotMigrated,
            "this worktree has no migration to roll back",
        ));
    };
    let rel_prefix = journal.rel_prefix.clone();
    let p = load_plan(&journal.folder.join("plan.json"), &journal.plan)?;
    let store = store_path(&repo)?;
    let mut sink = StoreImport { repo: &repo };
    let mut hook = crash_hook();
    let mut ctx = Ctx {
        repo: &repo,
        plan: &p,
        rel_prefix,
        store,
        sink: &mut sink,
        after_stage: &mut hook,
    };
    apply::roll_back(&mut ctx, &root_dir, &mut journal, force)?;
    let hash = journal.plan.clone();
    Ok(Report::ok(data(
        &p,
        &hash,
        journal.stage.as_str(),
        Some(&journal.folder),
        Some(&journal),
    )))
}

fn status(root: &Path) -> Result<Report<MigrateData>, GraphError> {
    let repo = Repo::open(root)?;
    let root_dir = apply::folder_root(&repo)?;
    let Some(journal) = Journal::load(&root_dir)? else {
        return Err(GraphError::not_found(
            Code::MigrateNotMigrated,
            "this worktree has no migration",
        )
        .with_hint("start with `migrate --dry-run`"));
    };
    let p = load_plan(&journal.folder.join("plan.json"), &journal.plan)?;
    let mut report = Report::ok(data(
        &p,
        &journal.plan,
        journal.stage.as_str(),
        Some(&journal.folder),
        Some(&journal),
    ));
    if !matches!(journal.stage, Stage::Done | Stage::RolledBack) {
        report = report.with([Diagnostic::warning(
            Code::MigrateInProgress,
            format!("the migration stopped at {}", journal.stage.as_str()),
        )
        .with_hint(format!(
            "finish it with `migrate --apply {}`, or undo it with `migrate --rollback`",
            journal.plan
        ))]);
    }
    Ok(report)
}
