//! `mdkg migrate`: brings a 0.6.0 graph onto 0.7.0, migrating identities
//! and separating knowledge from work history.
//!
//! - `--dry-run` prints the plan and its hash, and writes the full plan and
//!   the `--rewrite-refs` patch for review.
//! - `--apply <hash>` applies exactly that plan in one commit, after
//!   backing up the graph, the store and the base commit.
//! - `--rollback` reverts it from the backups.
//! - `--status` shows how far the last migration got.
//! - `--restore-history` with `--dry-run` or `--apply <hash>` brings back
//!   into git the work history a migration moved to the store.

// Migrating a branch against an already-migrated main: cut from 0.7.1
// (decided 2026-10-28). Only `migrated_main_of` is used.
#[allow(dead_code)]
mod against;
pub mod apply;
mod classify;
mod gather;
mod gitops;
mod mint;
pub mod plan;
mod records;
mod restore;
mod rewrite;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BIN;
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
    /// With --dry-run or --apply: where the work history goes. `git` keeps
    /// checkpoints, finished tests, event logs and progress sections in
    /// git; `store` moves them to the coordinator store's log, as 0.7.0
    /// did. Default: `git` when the graph commits its work (its config's
    /// `work.location`, which is `git` for every 0.6 graph).
    #[arg(long, value_name = "WHERE", value_enum)]
    pub work_history: Option<WorkHistory>,
    /// With --dry-run or --apply: on a graph already migrated, bring back
    /// into git, in one commit, the checkpoints, finished tests and event
    /// logs the migration moved to the store, read from the migration
    /// commit's parent.
    #[arg(long, conflicts_with_all = ["rollback", "status", "work_history", "rewrite_refs"])]
    pub restore_history: bool,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// `--work-history`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkHistory {
    Git,
    Store,
}

impl From<WorkHistory> for plan::History {
    fn from(w: WorkHistory) -> Self {
        match w {
            WorkHistory::Git => plan::History::Git,
            WorkHistory::Store => plan::History::Store,
        }
    }
}

/// How many of each.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct Counts {
    /// Nodes that stay, with new ids.
    pub nodes: usize,
    /// Of those, checkpoints and finished tests kept in git as nodes.
    pub kept_in_git: usize,
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
    /// Where the work history goes: `git` (it stays) or `store` (it
    /// becomes log records and leaves git). The plan's hash covers it.
    pub work_history: String,
    /// Why the work history goes there.
    pub work_history_reason: String,
    /// The workspaces the migration converts, each in place; the hash
    /// covers them.
    pub workspaces: Vec<plan::PlanWorkspace>,
    /// What the plan took from the migrated main it was made against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<against::Against>,
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
    /// With --restore-history: what comes back into git, and from where.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<restore::Restored>,
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

/// Refuses to carry on a plan made against a migrated main by an earlier
/// build: that mode is cut from 0.7.1, and its records may not be safe to
/// import. `--status` still reads it, and `--rollback` undoes it.
fn resumable(p: &Plan) -> Result<(), GraphError> {
    match &p.against {
        None => Ok(()),
        Some(a) => Err(GraphError::conflict(
            Code::MigrateUnsupported,
            format!(
                "this unfinished migration was planned against {}, which this release \
                 doesn't carry out",
                a.reference
            ),
        )
        .with_hint(
            "undo it with `migrate --rollback`, then see the guide's \"Branches that merge \
             later\"",
        )),
    }
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
        kept_in_git: p
            .nodes
            .iter()
            .filter(|n| n.id.is_some() && matches!(n.plane.as_str(), "checkpoint" | "check-result"))
            .count(),
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
        work_history: p.history.as_str().to_string(),
        work_history_reason: p.history_reason.clone(),
        workspaces: p.workspaces.clone(),
        against: p.against.clone(),
        counts: counts(p),
        files: p.files.clone(),
        patch: p.patch.clone(),
        warnings: {
            let mut w = p.warnings.clone();
            // A path recorded before its move that a crash stopped isn't
            // there: the next run moves the graph there, or records anew.
            for kept in journal
                .into_iter()
                .flat_map(|j| &j.kept)
                .filter(|k| Path::new(k).exists())
            {
                w.push(plan::PlanWarning {
                    code: "migrate.rejected_kept".into(),
                    path: None,
                    message: format!(
                        "a graph that didn't validate, with anything saved into it while it \
                         was in place, is kept in {kept}: check it for edits to bring back"
                    ),
                });
            }
            w
        },
        plan_file: file("plan.json"),
        patch_file: file("rewrite-refs.patch"),
        backups: journal.map(|j| Backups {
            tag: j.tag.clone(),
            store_snapshot: j.snapshot.as_ref().map(|s| s.display().to_string()),
            graph_copy: j.folder.join("mdkg").display().to_string(),
        }),
        restore: None,
    }
}

fn restore_data(p: &restore::RestorePlan, stage: &str, commit: Option<String>) -> MigrateData {
    MigrateData {
        plan: p.hash(),
        stage: stage.to_string(),
        base: p.base.clone(),
        commit,
        graph_id: p.graph_id.clone(),
        work_history: plan::History::Git.as_str().to_string(),
        work_history_reason: "restoring it to git".into(),
        workspaces: Vec::new(),
        against: None,
        counts: Counts {
            kept_in_git: p.restored.checkpoints + p.restored.check_results,
            files_written: p.files.len(),
            ..Counts::default()
        },
        files: p.files.clone(),
        patch: Vec::new(),
        warnings: p.warnings.clone(),
        plan_file: None,
        patch_file: None,
        backups: None,
        restore: Some(p.restored.clone()),
    }
}

/// Text a repository or its config supplies (workspace names and folders,
/// paths, refs), escaped for the terminal (Terra #15894).
fn shown(s: &str) -> String {
    crate::output::shown_path(s)
}

/// The report's first lines: plan, stage, base, graph id and commit. A
/// `--status` reads them from the journal, which a worker can edit, so
/// each is escaped (Terra #15997).
fn print_head(what: &str, d: &MigrateData) {
    println!("{what} plan {} ({})", shown(&d.plan), shown(&d.stage));
    println!("  base {}, graph_id {}", shown(&d.base), shown(&d.graph_id));
    if let Some(commit) = &d.commit {
        println!("  commit {}", shown(commit));
    }
}

fn print_restore(d: &MigrateData, r: &restore::Restored) {
    print_head("restore-history", d);
    if r.migrations.is_empty() {
        println!("  no migration commit on HEAD's history moved anything out of git");
    } else {
        println!(
            "  from the migration commit(s) {}",
            shown(&r.migrations.join(", "))
        );
    }
    println!(
        "  back into git: {} checkpoints and {} finished tests as nodes, {} event logs as they were",
        r.checkpoints, r.check_results, r.event_logs
    );
    for f in &d.files {
        println!("    {}", crate::output::shown_path(&f.path));
    }
    for w in &d.warnings {
        println!(
            "  warning {}: {}",
            w.code,
            crate::output::shown_path(&w.message)
        );
    }
    if d.stage == "planned" && !d.files.is_empty() {
        println!(
            "apply with: {} migrate --restore-history --apply {}",
            crate::BIN,
            shown(&d.plan)
        );
    }
}

fn print(d: &MigrateData) {
    if let Some(r) = &d.restore {
        return print_restore(d, r);
    }
    let c = &d.counts;
    print_head("migrate", d);
    println!(
        "  nodes: {} keep their place with new ids ({} tier C kept as plain nodes)",
        c.nodes, c.tier_c
    );
    if d.workspaces.len() > 1 {
        let listed: Vec<String> = d
            .workspaces
            .iter()
            .map(|w| {
                format!(
                    "{} ({}, {} nodes)",
                    shown(&w.alias),
                    shown(&w.folder),
                    w.nodes
                )
            })
            .collect();
        println!("  workspaces, each kept in place: {}", listed.join(", "));
    }
    let why = if d.work_history_reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", shown(&d.work_history_reason))
    };
    if d.work_history == "git" {
        println!(
            "  work history: stays in git{why}: {} checkpoints and finished tests stay \
             nodes; progress sections and event logs stay as they are",
            c.kept_in_git
        );
    } else {
        println!("  work history: moves to the coordinator store's log, out of git{why}");
    }
    if let Some(a) = &d.against {
        println!(
            "  against {} ({}, migrated in {}): this branch forked at {}",
            shown(&a.reference),
            shown(&a.tip),
            shown(&a.migration),
            shown(&a.fork)
        );
        println!(
            "    {} graph files it hasn't changed since come out as main has them, and {} \
             nodes main made since are added; {} nodes it changed that were there at the \
             fork keep main's ids; the rest are its own. Records main's migration moved to \
             the log are imported here too, once. Merge main after this: only the branch's \
             own edits can conflict",
            a.adopted, a.added, a.shared
        );
    }
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
    // A kept graph may hold someone's edit: each one is named.
    for w in d
        .warnings
        .iter()
        .filter(|w| w.code == "migrate.rejected_kept")
    {
        println!("    {}", crate::output::shown_path(&w.message));
    }
    // Key conversions change what a node holds: each one is listed.
    for w in d
        .warnings
        .iter()
        .filter(|w| w.code == "migrate.key_converted")
    {
        println!("    {}", crate::output::shown_path(&w.message));
    }
    if let Some(f) = &d.plan_file {
        println!("  full plan: {}", shown(f));
    }
    if let Some(f) = &d.patch_file {
        println!("  patch: {}", shown(f));
    }
    if let Some(b) = &d.backups {
        if let Some(t) = &b.tag {
            println!("  backup tag: {}", shown(t));
        }
        if let Some(s) = &b.store_snapshot {
            println!("  backup store: {}", shown(s));
        }
        println!("  backup graph: {}", shown(&b.graph_copy));
    }
    if d.stage == "planned" {
        println!(
            "apply with: {} migrate --apply {}{}",
            crate::BIN,
            shown(&d.plan),
            history_flag(d)
        );
    }
}

/// The options an apply of this plan needs: none for the defaults.
fn history_flag(d: &MigrateData) -> String {
    let mut flags = String::new();
    if d.work_history_reason == "chosen with --work-history" {
        flags.push_str(&format!(" --work-history {}", shown(&d.work_history)));
    }
    flags
}

fn emit(json: bool, report: Report<MigrateData>) -> u8 {
    output::emit("migrate", json, report, print)
}

pub fn run(root: &Path, args: &MigrateArgs) -> u8 {
    let history = args.work_history.map(plan::History::from);
    let result = if args.restore_history {
        match &args.apply {
            Some(hash) => restore::apply_restore(root, hash).map(|(p, commit)| {
                let stage = if commit.is_some() {
                    "done"
                } else {
                    "nothing-to-restore"
                };
                Report::ok(restore_data(&p, stage, commit))
            }),
            None => restore::plan_restore(root).map(|(_, p)| {
                let stage = if p.files.is_empty() {
                    "nothing-to-restore"
                } else {
                    "planned"
                };
                let mut data = restore_data(&p, stage, None);
                // Not the plan's (nor its hash's): the apply finishes it first.
                data.warnings.extend(restore::unfinished_warning(root));
                Report::ok(data)
            }),
        }
    } else if args.dry_run {
        dry_run(root, history)
    } else if let Some(hash) = &args.apply {
        apply_plan(root, hash, args.rewrite_refs, history)
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

/// The graph folders this migration replaces, one per enabled workspace:
/// relative to the repository's top (`.mdkg/`, `aux/.mdkg/`), and
/// absolute. The workspace at the graph's root comes first, then the rest
/// in alias order. An identity v2 graph must have one workspace, since its
/// reader doesn't say which workspace a node is in.
fn graph_folders(repo: &Repo, root: &Path) -> Result<Vec<(String, PathBuf)>, GraphError> {
    let v2 = mdkg_graph::identity_v2::is_identity_v2(root);
    let cfg = if v2 {
        mdkg_graph::config::load_identity_v2(root)?
    } else {
        mdkg_graph::config::load(root)?
    };
    let dirs = gather::workspace_folders(&cfg, &repo.prefix);
    if dirs.is_empty() {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            "the graph has no enabled workspace",
        ));
    }
    if v2 && dirs.len() > 1 {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            format!(
                "the identity v2 graph has {} enabled workspaces",
                dirs.len()
            ),
        )
        .with_hint(
            "migrate migrates an identity v2 graph of one workspace; keep using mdkg 0.6.x \
             on this graph, since 0.7's other commands refuse identity format v2. See \
             docs/migrating-from-0.6.md",
        ));
    }
    let home = format!("{}.mdkg", repo.prefix);
    let mut ordered: Vec<&(String, String)> = dirs.iter().filter(|(_, d)| *d == home).collect();
    ordered.extend(dirs.iter().filter(|(_, d)| *d != home));
    let mut out = Vec::new();
    for (alias, rel) in ordered {
        let rel_prefix = format!("{rel}/");
        if let Some((other, _)) = dirs
            .iter()
            .find(|(a, d)| a != alias && rel_prefix.starts_with(&format!("{d}/")))
        {
            return Err(GraphError::failure(
                Code::MigrateUnsupported,
                format!("workspace {alias}'s folder {rel} is inside workspace {other}'s"),
            ));
        }
        let dir = repo.top.join(rel);
        let meta = fs::symlink_metadata(&dir)
            .map_err(|e| GraphError::failure(Code::IoReadFailed, format!("reading {rel}: {e}")))?;
        if !meta.is_dir() {
            return Err(GraphError::failure(
                Code::MigrateUnsupported,
                format!("{rel} is a link or a file, not a folder"),
            ));
        }
        let links = apply::links_in(&dir)
            .map_err(|e| GraphError::failure(Code::IoReadFailed, format!("reading {rel}: {e}")))?;
        if !links.is_empty() {
            let shown: Vec<&str> = links.iter().take(5).map(String::as_str).collect();
            return Err(GraphError::failure(
                Code::MigrateUnsupported,
                format!(
                    "the graph folder {rel} holds symbolic links: {}",
                    shown.join(", ")
                ),
            )
            .with_hint(
                "replace each link with the file or folder it names, commit, and plan again",
            ));
        }
        out.push((rel_prefix, dir));
    }
    Ok(out)
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

/// The graph's source. A branch whose main is already migrated is refused:
/// migrating it against main is not supported by this release.
fn source_for(
    root: &Path,
    history: Option<plan::History>,
) -> Result<(Repo, plan::Source), GraphError> {
    let (repo, source) = gather::gather(root, true, history)?;
    not_migrated(&source)?;
    if let Some(main) = against::migrated_main_of(&repo, &source)? {
        return Err(GraphError::conflict(
            Code::MigrateUnsupported,
            format!(
                "{main} is already migrated: this release can't migrate a branch cut \
                 before main's migration"
            ),
        )
        .with_hint(branch_cut_hint()));
    }
    Ok((repo, source))
}

/// How to go on with a branch cut before main's migration.
fn branch_cut_hint() -> String {
    format!(
        "merge main into this branch first, taking main's file for each node that conflicts \
         and redoing this branch's change in it; a node only this branch has must be made \
         again on 0.7 (`{} new`)",
        crate::BIN
    )
}

/// What `migrate` would say about this checkout's graph with no
/// `graph_id`, for `doctor` (e2e 0.7.1 B14): a branch cut before main's
/// migration is refused, so it gets that guidance, not `migrate`.
pub(crate) fn unmigrated_hint(repo: &Repo) -> String {
    let config_rel = format!("{}{}", repo.prefix, mdkg_graph::config::CONFIG_PATH);
    match against::migrated_main_at(&repo.top, &config_rel) {
        Ok(Some(main)) => format!(
            "{main} is already migrated, so `{BIN} migrate` refuses this branch: {}",
            branch_cut_hint()
        ),
        _ => format!("`{BIN} migrate` moves a 0.6.0 graph to 0.7 (dry run first)"),
    }
}

fn dry_run(root: &Path, history: Option<plan::History>) -> Result<Report<MigrateData>, GraphError> {
    let (repo, source) = source_for(root, history)?;
    let folders = graph_folders(&repo, root)?;
    let p = plan::plan(&source).map_err(plan_error)?;
    check_inside(&p, &folders)?;
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

/// Every change inside the graph lies in one of its folders.
fn check_inside(p: &Plan, folders: &[(String, PathBuf)]) -> Result<(), GraphError> {
    if let Some(f) = p
        .files
        .iter()
        .find(|f| !folders.iter().any(|(rel, _)| f.path.starts_with(rel)))
    {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            format!("{} lies outside the graph's folders", f.path),
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
    history: Option<plan::History>,
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
            let p = load_plan(&folder.join("plan.json"), hash)?;
            resumable(&p)?;
            (p, j.clone())
        }
        None => {
            if let Some(j) = &journal
                && j.stage == Stage::Done
                && j.plan == hash
            {
                let p = load_plan(&folder.join("plan.json"), hash)?;
                return Ok(Report::ok(data(&p, hash, "done", Some(&folder), Some(j))));
            }
            let (_, source) = source_for(root, history)?;
            let graph_folders = graph_folders(&repo, root)?;
            let p = plan::plan(&source).map_err(plan_error)?;
            check_inside(&p, &graph_folders)?;
            let now = p.hash();
            if now != hash {
                return Err(GraphError::conflict(
                    Code::MigratePlanMismatch,
                    format!(
                        "the graph's plan is {now}, not the one given (work history: {})",
                        p.history.as_str()
                    ),
                )
                .with_hint(
                    "run `migrate --dry-run` again and review the new plan; a plan made \
                     with --work-history needs the same option on --apply",
                ));
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
            // Each folder's new graph is built one level down, in a root of
            // its own beside it (`.mdkg.migrate-<plan>/.mdkg`).
            let mut folders: Vec<apply::Folder> = graph_folders
                .iter()
                .map(|(rel_prefix, graph_dir)| apply::Folder {
                    graph_dir: graph_dir.clone(),
                    rel_prefix: rel_prefix.clone(),
                    staging: apply::staging_for(graph_dir, hash),
                    inventory: None,
                })
                .collect();
            let first = folders.remove(0);
            let mut j = Journal {
                plan: hash.to_string(),
                base: p.base.clone(),
                commit: None,
                graph_dir: first.graph_dir,
                rel_prefix: first.rel_prefix,
                staging: first.staging,
                folder: folder.clone(),
                snapshot: None,
                tag: None,
                tag_created: false,
                rewrite_refs,
                inventory: None,
                extra: folders,
                rejecting: false,
                kept: Vec::new(),
                stage: Stage::Started,
            };
            // An earlier attempt's copies (rolled back since) are of older
            // folders: this attempt takes its own.
            let mut stale = Vec::new();
            for (i, f) in j.folders().iter().enumerate() {
                stale.push(j.copy_of(i));
                stale.push(apply::partial_of(&j.copy_of(i)));
                stale.push(
                    f.staging
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default(),
                );
            }
            for stale in stale {
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
            j.stage = Stage::Started;
            j.save(&root_dir)?;
            (p, j)
        }
    };
    let store = store_path(&repo)?;
    let mut sink = StoreImport { repo: &repo };
    let mut hook = crash_hook();
    let mut ctx = Ctx {
        repo: &repo,
        plan: &p,
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
    let p = load_plan(&journal.folder.join("plan.json"), &journal.plan)?;
    let store = store_path(&repo)?;
    let mut sink = StoreImport { repo: &repo };
    let mut hook = crash_hook();
    let mut ctx = Ctx {
        repo: &repo,
        plan: &p,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfinished_plan_made_against_main_is_not_carried_on() {
        let mut p =
            plan::plan(&plan::tests::source(vec![plan::tests::task(1, "", "")])).expect("plan");
        assert!(resumable(&p).is_ok());
        p.against = Some(against::Against {
            reference: "refs/heads/main".into(),
            tip: "a".into(),
            migration: "b".into(),
            fork: "c".into(),
            adopted: 0,
            added: 0,
            shared: 0,
            work_history: plan::History::Store,
            explicit: false,
            replayed: 0,
        });
        let e = resumable(&p).expect_err("refused");
        assert_eq!(e.diagnostic.code, Code::MigrateUnsupported);
        assert!(
            e.diagnostic
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("migrate --rollback")),
            "{:?}",
            e.diagnostic.hint
        );
    }
}
