//! Migrating a branch cut before main migrated, so that it merges into the
//! migrated main with conflicts only in its own edits.
//!
//! When main (`mdkg.mainRef`, or `main`; the local branch or a
//! remote-tracking one, so a main migrated on GitHub and fetched counts)
//! is already migrated, the branch's plan is made against it:
//! - every graph file the branch hasn't changed since it forked from main
//!   comes out exactly as main has it now (or goes, where main has no such
//!   file), so git sees no change between the two;
//! - every node the branch changed, that main has too (by workspace and
//!   a legacy id both had at the fork), gets main's id, whatever path or
//!   `created` it has now;
//! - its own nodes get minted ids, as in any migration, even where main
//!   made another node with the same legacy id after the fork (a warning
//!   says so, and the branch's node leaves that handle to main's: it has
//!   no `legacy_id`);
//! - nodes main made after the fork come as main has them, so that what
//!   main's nodes refer to is there;
//! - the work history goes where main's migration put it: in git, unless
//!   main's migration commit moved checkpoints, finished tests, event logs
//!   or progress notes out of it. Records main's migration imported are
//!   planned again (the import skips each one the store already has), so
//!   a clone whose store never saw main's migration gets them too.
//!
//! The alternative, merging the migrated main into the 0.6 branch and
//! resolving, makes every node the branch touched conflict with main's
//! rewrite of it, so it isn't offered.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mdkg_format::Code;
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::classify::{Plane, classify};
use super::gather::frontmatter_lines;
use super::gitops;
use super::plan::{History, MOVED_SECTION, PlanWarning, Source};
use super::records::Record;
use super::restore::{has_graph_id, migration_commits, show};
use crate::coord::{Repo, git_error};

/// What a plan made against a migrated main took from it (part of the
/// plan, so of its hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Against {
    /// The ref the plan was made against (`refs/heads/main`).
    #[serde(rename = "ref")]
    pub reference: String,
    /// Its commit, and the commit that migrated it.
    pub tip: String,
    pub migration: String,
    /// Where this branch forked from it.
    pub fork: String,
    /// Graph files the branch hasn't changed since the fork, taken as
    /// main has them.
    pub adopted: usize,
    /// Nodes main made after the fork, added as main has them.
    pub added: usize,
    /// Nodes the branch changed that main has too (the same legacy id at
    /// the fork), given main's ids.
    pub shared: usize,
    /// Where this branch's work history goes: where main's migration put
    /// it (a `--work-history` that says otherwise is refused).
    pub work_history: History,
    /// Whether `--against` named the ref.
    pub explicit: bool,
    /// Records main's migration made that this plan imports again, for a
    /// store that doesn't have them (0 when it does, or when main kept its
    /// history in git).
    #[serde(default)]
    pub replayed: usize,
}

/// The branch's plan inputs, changed to follow main.
pub struct Follow {
    pub against: Against,
    pub graph_id: String,
    /// By `<workspace>:<legacy id>`, main's id for each node it has that
    /// was already there at the fork.
    pub ids: BTreeMap<String, String>,
    /// By path, main's content for each unchanged file (`None`: main has
    /// none there).
    pub adopt: BTreeMap<String, Option<String>>,
    /// By path, each node main made after the fork.
    pub added: BTreeMap<String, String>,
    /// By `<workspace>:<legacy id>`, main's ids of the nodes in `added`.
    pub added_ids: BTreeMap<String, String>,
    /// This branch's nodes whose legacy id main gave another node after
    /// the fork, by `<workspace>:<legacy id>`, with main's id for it.
    pub collisions: BTreeMap<String, String>,
    /// Said before the plan's own.
    pub warnings: Vec<PlanWarning>,
    /// The commit main's migration migrated, and that migration's plan
    /// hash, when its commit message still says it.
    pub migration_parent: String,
    pub main_plan: Option<String>,
    /// The record digests main's nodes cite (their moved-section and
    /// summary lines).
    pub cited: BTreeSet<String>,
}

fn rev(top: &Path, name: &str) -> Option<String> {
    gitops::run(
        top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
        None,
    )
    .ok()
    .and_then(|b| String::from_utf8(b).ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

fn is_ancestor(top: &Path, a: &str, b: &str) -> bool {
    gitops::run(top, &["merge-base", "--is-ancestor", a, b], None).is_ok()
}

/// The migrated main this branch would merge into (the main branch or a
/// remote-tracking copy of it), if any.
/// Copies that have diverged count too: main is migrated either way.
pub fn migrated_main_of(repo: &Repo, source: &Source) -> Result<Option<String>, GraphError> {
    migrated_main_at(&repo.top, &source.config.path)
}

/// [`migrated_main_of`] for the graph whose config is at `config_rel`
/// in `top`, without gathering the graph.
pub fn migrated_main_at(top: &Path, config_rel: &str) -> Result<Option<String>, GraphError> {
    Ok(migrated_copies(top, config_rel, None)?
        .into_iter()
        .next()
        .map(|(name, _)| name))
}

/// The migrated copies of main, by name and commit: `explicit`, or the
/// main branch and its remote-tracking copies, whichever are migrated.
fn migrated_copies(
    top: &Path,
    config_rel: &str,
    explicit: Option<&str>,
) -> Result<Vec<(String, String)>, GraphError> {
    let mut names = Vec::new();
    match explicit {
        Some(r) => names.push(r.to_string()),
        None => {
            let main = crate::coord::main_ref(top);
            names.push(format!("refs/heads/{main}"));
            let out = gitops::run(
                top,
                &["for-each-ref", "--format=%(refname)", "refs/remotes"],
                None,
            )
            .map_err(git_error)?;
            for r in String::from_utf8_lossy(&out).lines() {
                // `refs/remotes/<remote>/<main>`, where main may have
                // slashes of its own (`release/0.7`).
                if r.strip_prefix("refs/remotes/")
                    .and_then(|rest| rest.split_once('/'))
                    .is_some_and(|(_, branch)| branch == main)
                {
                    names.push(r.to_string());
                }
            }
        }
    }
    let head = rev(top, "HEAD").unwrap_or_default();
    let mut migrated: Vec<(String, String)> = Vec::new();
    for name in names {
        let Some(tip) = rev(top, &name) else {
            if explicit.is_some() {
                return Err(GraphError::not_found(
                    Code::MigrateUnsupported,
                    format!("--against {name} names no commit"),
                ));
            }
            continue;
        };
        if tip == head || !has_graph_id(show(top, &tip, config_rel)) {
            if explicit.is_some() {
                return Err(GraphError::failure(
                    Code::MigrateUnsupported,
                    format!("--against {name} isn't migrated"),
                )
                .with_hint("leave --against out to migrate the branch on its own"));
            }
            continue;
        }
        if !migrated.iter().any(|(_, t)| *t == tip) {
            migrated.push((name, tip));
        }
    }
    Ok(migrated)
}

/// The migrated main this branch should follow, if any: the newest of its
/// migrated copies. Refuses when two of them have diverged.
fn migrated_main(
    top: &Path,
    config_rel: &str,
    explicit: Option<&str>,
) -> Result<Option<(String, String)>, GraphError> {
    let migrated = migrated_copies(top, config_rel, explicit)?;
    let Some(newest) = migrated
        .iter()
        .find(|(_, t)| migrated.iter().all(|(_, o)| is_ancestor(top, o, t)))
        .cloned()
    else {
        if migrated.is_empty() {
            return Ok(None);
        }
        let listed: Vec<&str> = migrated.iter().map(|(n, _)| n.as_str()).collect();
        return Err(GraphError::conflict(
            Code::MigrateUnsupported,
            format!(
                "main's migrated copies have diverged: {}",
                listed.join(", ")
            ),
        )
        .with_hint("pass --against <ref> to say which one this branch merges into"));
    };
    Ok(Some(newest))
}

/// The plan hash a migration commit's message names (`(plan sha256:...)`),
/// if a squash or a reword kept it.
fn plan_of(top: &Path, commit: &str) -> Option<String> {
    let out = gitops::run(top, &["log", "-1", "--format=%B", commit], None).ok()?;
    let text = String::from_utf8_lossy(&out);
    let at = text.find("(plan sha256:")? + "(plan ".len();
    let hash: String = text[at..].chars().take_while(|c| *c != ')').collect();
    (hash.len() == "sha256:".len() + 64).then_some(hash)
}

/// Whether this repository's store already holds the import of plan
/// `hash` (its migration ran here). Read only: no store, or one that can't
/// be read, holds nothing.
fn store_has_import(top: &Path, hash: &str) -> bool {
    let Ok(path) = mdkg_coord_sqlite::git::store_path(top) else {
        return false;
    };
    mdkg_coord_sqlite::Store::open_raw_read_only(&path)
        .ok()
        .flatten()
        .is_some_and(|c| {
            c.query_row(
                "SELECT 1 FROM meta WHERE key = ?1",
                [format!("import:{hash}")],
                |_| Ok(()),
            )
            .is_ok()
        })
}

/// Every migrate_digest this repository's store's log holds: one read of
/// the log. Read only; no store, or one that can't be read, holds none.
fn store_digests(top: &Path) -> BTreeSet<String> {
    let Ok(path) = mdkg_coord_sqlite::git::store_path(top) else {
        return BTreeSet::new();
    };
    let Ok(Some(conn)) = mdkg_coord_sqlite::Store::open_raw_read_only(&path) else {
        return BTreeSet::new();
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT record FROM log WHERE instr(record, '\"migrate_digest\":') > 0")
    else {
        return BTreeSet::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return BTreeSet::new();
    };
    rows.filter_map(|row| {
        let v: Value = serde_json::from_str(&row.ok()?).ok()?;
        v["body"]["migrate_digest"].as_str().map(str::to_string)
    })
    .collect()
}

/// A folder of its own under the system's temporary folder: made here,
/// never one that was already there.
fn own_temp_dir() -> std::io::Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    for n in 0..100 {
        let dir =
            std::env::temp_dir().join(format!("mdkg-replay-{}-{nanos}-{n}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free temporary folder name"))
}

/// The records main's migration put in the store, for a store that
/// doesn't hold them: a clone that never ran that migration would
/// otherwise have none of the history it moved out of git. They're
/// planned again from the commit it migrated, as main's migration planned
/// them; this branch's own records are planned from its own nodes. The
/// import skips each one the store already has.
///
/// The commit is checked out in a repository of its own that borrows this
/// one's objects (an alternates file) and copies its branches, in a folder
/// this makes and removes: nothing in this repository changes, so a dry
/// run stays read only, and a crash leaves only that temporary folder.
/// The branches are there for refs to nodes on other branches, as main's
/// migration had them.
///
/// A replay can't promise main's exact records: a child graph or a
/// submodule that isn't in git, or branches that have moved since, can
/// change one. Every record main's nodes cite is checked for, and a plan
/// missing any is refused.
pub fn replay(repo: &Repo, f: &Follow) -> Result<Vec<Record>, GraphError> {
    // This store ran main's migration: its import is here, by the plan
    // hash the commit message names. Otherwise main's records are replayed
    // whole, and each is imported unless the store holds that one (holding
    // the records main's nodes cite says nothing of the rest, such as its
    // event logs).
    if f.against.work_history != History::Store
        || f.main_plan
            .as_deref()
            .is_some_and(|h| store_has_import(&repo.top, h))
    {
        return Ok(Vec::new());
    }
    let dir = own_temp_dir().map_err(|e| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("making a temporary folder to replay main's migration: {e}"),
        )
    })?;
    let result = (|| -> Result<Vec<Record>, GraphError> {
        let common = mdkg_coord_sqlite::git::common_dir(&repo.top).map_err(git_error)?;
        let tree = dir.join("tree");
        let heads = gitops::run(
            &repo.top,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
            ],
            None,
        )
        .map_err(git_error)?;
        // A repository of its own, written out rather than made by git (the
        // hardened git runs only inside a repository): HEAD at the commit,
        // this one's branches, and its objects borrowed through an
        // alternates file, not copied.
        let mut packed = String::new();
        for line in String::from_utf8_lossy(&heads).lines() {
            if let Some((oid, name)) = line.split_once(' ') {
                packed.push_str(&format!("{oid} {name}\n"));
            }
        }
        // Its object format is this repository's (sha256 needs saying).
        let format = gitops::run(&repo.top, &["rev-parse", "--show-object-format"], None)
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_else(|_| "sha1".into());
        let config = match format.as_str() {
            "sha1" => "[core]\n\trepositoryformatversion = 0\n\tbare = false\n".to_string(),
            other => format!(
                "[core]\n\trepositoryformatversion = 1\n\tbare = false\n\
                 [extensions]\n\tobjectformat = {other}\n"
            ),
        };
        let git = tree.join(".git");
        let written = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(git.join("objects/info"))?;
            std::fs::create_dir_all(git.join("refs/heads"))?;
            std::fs::create_dir_all(git.join("refs/tags"))?;
            std::fs::write(git.join("config"), &config)?;
            std::fs::write(git.join("HEAD"), format!("{}\n", f.migration_parent))?;
            std::fs::write(git.join("packed-refs"), packed)?;
            std::fs::write(
                git.join("objects/info/alternates"),
                format!("{}\n", common.join("objects").display()),
            )
        })();
        written.map_err(|e| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("writing the repository main's migration is replayed in: {e}"),
            )
        })?;
        // Git in that repository is trusted as that repository, pinned
        // exactly, whatever the launcher or the MCP server pinned for this
        // one (which would refuse it): it's the one this run just wrote.
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let trust = mdkg_coord_sqlite::git::WorktreeTrust {
            common: Some(canonical(&git)),
            worktree: Some(canonical(&tree)),
        };
        mdkg_coord_sqlite::git::with_worktree_trust(&trust, || {
            gitops::run(&tree, &["read-tree", "--reset", "-u", "HEAD"], None).map_err(git_error)?;
            let (_, mut source) =
                super::gather::gather(&tree.join(&repo.prefix), true, Some(History::Store))?;
            source.graph_id = f.graph_id.clone();
            let p = super::plan::plan(&source).map_err(|e| {
                GraphError::failure(
                    Code::MigrateUnsupported,
                    format!(
                        "planning main's migration again, for the records it made: {}",
                        e.message
                    ),
                )
                .at(e.path, None)
            })?;
            Ok(p.records)
        })
    })();
    // Only the folder this made.
    std::fs::remove_dir_all(&dir).ok();
    let records = result?;
    // A cited record the replay doesn't make is still fine if the store
    // holds it (one an earlier migration here imported).
    let made: BTreeSet<&str> = records.iter().map(|r| r.digest.as_str()).collect();
    let unmade: BTreeSet<String> = f
        .cited
        .iter()
        .filter(|d| !made.contains(d.as_str()))
        .cloned()
        .collect();
    let missing = if unmade.is_empty() {
        0
    } else {
        let held = store_digests(&repo.top);
        unmade.iter().filter(|d| !held.contains(*d)).count()
    };
    // Main's nodes would cite records this store never gets: refused, not
    // applied with a gap (orchestrator, 2026-10-29).
    if missing > 0 {
        return Err(GraphError::conflict(
            Code::MigrateUnsupported,
            format!(
                "{missing} record(s) main's nodes cite didn't come out the same when main's \
                 migration was planned again (a child graph or submodule outside git, or \
                 branches that moved since), so this store can't get them"
            ),
        )
        .with_hint(
            "migrate this branch where main's migration ran: that store has main's records",
        ));
    }
    Ok(records)
}

/// Whether `line` is one a store-mode migration leaves where it moved a
/// section to the log (`Progress notes moved to the work log at
/// migration: review record, migration digest sha256:...`), not prose
/// that quotes it.
fn moved_line(line: &str) -> bool {
    ["Progress notes ", "The review ledger "]
        .iter()
        .any(|what| {
            line.strip_prefix(what)
                .and_then(|rest| rest.strip_prefix(MOVED_SECTION))
                .and_then(|rest| rest.strip_prefix(": review record, migration digest sha256:"))
                .and_then(|hex| hex.strip_suffix('.'))
                .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

/// Blobs under `folders` in `commit`, by path.
fn blobs(
    top: &Path,
    commit: &str,
    folders: &[String],
) -> Result<BTreeMap<String, String>, GraphError> {
    Ok(gitops::ls_tree_under(top, commit, folders)
        .map_err(git_error)?
        .into_iter()
        .filter(|(_, e)| e.kind == "blob")
        .map(|(p, e)| (p, e.oid))
        .collect())
}

/// Plans `source` (this branch's) against a migrated main, when there is
/// one: `None` when main isn't migrated. `history` is the operator's
/// `--work-history`, refused unless it's where main's migration put it.
pub fn follow(
    repo: &Repo,
    source: &Source,
    explicit: Option<&str>,
    history: Option<History>,
) -> Result<Option<Follow>, GraphError> {
    let top = &repo.top;
    let config_rel = source.config.path.clone();
    let Some((reference, tip)) = migrated_main(top, &config_rel, explicit)? else {
        return Ok(None);
    };
    let Some((migration, parent)) = migration_commits(top, &tip, &config_rel, None)?
        .into_iter()
        .next()
    else {
        return Err(GraphError::failure(
            Code::MigrateUnsupported,
            format!("{reference} is migrated, but no commit on its history migrated it"),
        ));
    };
    let fork = gitops::run(top, &["merge-base", "HEAD", &tip], None)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            GraphError::failure(
                Code::MigrateUnsupported,
                format!("this branch shares no history with {reference}"),
            )
        })?;
    if is_ancestor(top, &migration, "HEAD") {
        return Err(GraphError::conflict(
            Code::MigrateAlreadyMigrated,
            format!("this branch already has {reference}'s migration commit {migration}"),
        ));
    }
    let graph_id = show(top, &tip, &config_rel)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("graph_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| source.graph_id.clone());

    let ws_folders: Vec<(String, String)> = source
        .folders
        .iter()
        .map(|(ws, f)| (ws.clone(), f.clone()))
        .collect();
    let mut folders: Vec<String> = ws_folders.iter().map(|(_, f)| f.clone()).collect();
    // The config, which may lie outside every workspace folder's listing.
    folders.push(config_rel.clone());
    let ws_of = |path: &str| {
        ws_folders
            .iter()
            .find(|(_, f)| path.starts_with(f.as_str()))
            .map(|(ws, _)| ws.clone())
            .unwrap_or_default()
    };
    let at_fork = blobs(top, &fork, &folders)?;
    let at_head = blobs(top, "HEAD", &folders)?;
    let at_tip = blobs(top, &tip, &folders)?;

    // Main's work history: in git unless its migration moved some of it
    // out: deleted a checkpoint, a finished test or an event log, or left
    // a node a line saying its notes moved to the work log.
    let before = blobs(top, &parent, &folders)?;
    let after = blobs(top, &migration, &folders)?;
    let gone: Vec<String> = before
        .iter()
        .filter(|(p, _)| !after.contains_key(*p))
        .filter(|(p, _)| p.ends_with(".md") || p.ends_with(".jsonl"))
        .map(|(_, oid)| oid.clone())
        .collect();
    let events_gone = before
        .keys()
        .any(|p| p.ends_with(".jsonl") && !after.contains_key(p));
    let changed_after: Vec<String> = after
        .iter()
        .filter(|(p, oid)| p.ends_with(".md") && before.get(*p) != Some(*oid))
        .map(|(_, oid)| oid.clone())
        .collect();
    let history_gone = gitops::read_blobs(top, &gone)
        .map_err(git_error)?
        .iter()
        .filter_map(|b| std::str::from_utf8(b).ok())
        .any(|text| {
            let f = frontmatter_lines(text);
            matches!(
                classify(
                    f.get("type").copied().unwrap_or(""),
                    f.get("status").copied()
                ),
                Plane::Checkpoint | Plane::CheckResult
            )
        });
    let sections_moved = !history_gone
        && !events_gone
        && gitops::read_blobs(top, &changed_after)
            .map_err(git_error)?
            .iter()
            .filter_map(|b| std::str::from_utf8(b).ok())
            .any(|text| text.lines().any(moved_line));
    // Store mode stays store mode: a `--restore-history` on main since
    // brings back checkpoints, tests and event logs, but its nodes still
    // cite the records of the sections that moved, which only the store
    // holds.
    let main_history = if history_gone || events_gone || sections_moved {
        History::Store
    } else {
        History::Git
    };
    // A branch whose history went elsewhere than main's would bring back
    // what main's migration moved out, or move out what main kept.
    if let Some(h) = history.filter(|h| *h != main_history) {
        return Err(GraphError::conflict(
            Code::MigrateUnsupported,
            format!(
                "{reference}'s migration put the work history in {}, not {}",
                main_history.as_str(),
                h.as_str()
            ),
        )
        .with_hint(match main_history {
            History::Store => {
                "leave --work-history out; to bring the history back into git, run \
                 `migrate --restore-history` once this branch has merged"
            }
            History::Git => "leave --work-history out",
        }));
    }

    // Main's nodes now, by workspace and legacy id.
    let tip_nodes: Vec<(&String, &String)> =
        at_tip.iter().filter(|(p, _)| p.ends_with(".md")).collect();
    let texts = gitops::read_blobs(
        top,
        &tip_nodes
            .iter()
            .map(|(_, o)| (*o).clone())
            .collect::<Vec<_>>(),
    )
    .map_err(git_error)?;
    let mut main_ids = BTreeMap::new();
    let mut tip_text: BTreeMap<String, String> = BTreeMap::new();
    for ((path, _), blob) in tip_nodes.into_iter().zip(texts) {
        let Ok(text) = String::from_utf8(blob) else {
            continue;
        };
        let f = frontmatter_lines(&text);
        if let (Some(id), Some(legacy)) = (f.get("id"), f.get("legacy_id")) {
            main_ids.insert(format!("{}:{legacy}", ws_of(path)), (*id).to_string());
        }
        tip_text.insert(path.clone(), text);
    }
    // A legacy id is one node on both sides only if it was one at the
    // fork: 0.6 numbered ids in sequence, so main and the branch each
    // made their own task-20 after it as often as not.
    let fork_nodes: Vec<String> = at_fork
        .iter()
        .filter(|(p, _)| p.ends_with(".md"))
        .map(|(_, o)| o.clone())
        .collect();
    let fork_paths: Vec<&String> = at_fork.keys().filter(|p| p.ends_with(".md")).collect();
    let mut at_fork_keys = BTreeSet::new();
    for (path, blob) in fork_paths
        .into_iter()
        .zip(gitops::read_blobs(top, &fork_nodes).map_err(git_error)?)
    {
        if let Some(id) = std::str::from_utf8(&blob)
            .ok()
            .and_then(|t| frontmatter_lines(t).get("id").map(|id| id.to_string()))
        {
            at_fork_keys.insert(format!("{}:{id}", ws_of(path)));
        }
    }
    let mut ids: BTreeMap<String, String> = main_ids
        .iter()
        .filter(|(k, _)| at_fork_keys.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Every graph file the branch hasn't changed since the fork comes out
    // as main has it.
    let mut adopt = BTreeMap::new();
    let mut wanted = Vec::new();
    for (path, oid) in &at_head {
        if at_fork.get(path) != Some(oid) {
            continue;
        }
        match at_tip.get(path) {
            Some(_) => wanted.push(path.clone()),
            None => {
                adopt.insert(path.clone(), None);
            }
        }
    }
    let missing: Vec<String> = wanted
        .iter()
        .filter(|p| !tip_text.contains_key(*p))
        .filter_map(|p| at_tip.get(p).cloned())
        .collect();
    let mut extra: BTreeMap<String, String> = wanted
        .iter()
        .filter(|p| !tip_text.contains_key(*p))
        .cloned()
        .zip(
            gitops::read_blobs(top, &missing)
                .map_err(git_error)?
                .into_iter()
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        )
        .collect();
    for path in wanted {
        let text = tip_text.get(&path).cloned().or_else(|| extra.remove(&path));
        adopt.insert(path, text);
    }
    // Nodes main has at a path this branch doesn't come too, so that the
    // nodes taken from main find what they refer to; the merge then adds
    // each on both sides alike. They're the nodes main made after the
    // fork, and the ones main moved that this branch left as they were;
    // one this branch changed or deleted stays its own.
    let head_keys: BTreeMap<String, &str> = source
        .nodes
        .iter()
        .filter_map(|n| {
            let id = frontmatter_lines(&n.content).get("id")?.to_string();
            Some((format!("{}:{id}", n.ws), n.path.as_str()))
        })
        .collect();
    let branch_uuids: BTreeMap<&str, &String> = source
        .v2
        .iter()
        .flat_map(|v2| v2.uuids.iter().map(|(p, u)| (u.as_str(), p)))
        .collect();
    let mut added = BTreeMap::new();
    let mut added_ids = BTreeMap::new();
    for (path, text) in &tip_text {
        if at_head.contains_key(path) {
            continue;
        }
        let f = frontmatter_lines(text);
        let key = f
            .get("legacy_id")
            .map(|legacy| format!("{}:{legacy}", ws_of(path)));
        // An identity v2 node this branch holds by its UUID is this
        // branch's to carry, wherever main has it now, unless the branch
        // left it as it was (and main moved it).
        let uuid = f
            .get("id")
            .and_then(|id| id.get(id.len().checked_sub(36)?..));
        if let Some(theirs) = uuid.and_then(|u| branch_uuids.get(u))
            && adopt.get(*theirs) != Some(&None)
        {
            continue;
        }
        let comes = match &key {
            Some(k) if at_fork_keys.contains(k) => head_keys
                .get(k)
                .is_some_and(|p| adopt.get(*p) == Some(&None)),
            _ => !at_fork.contains_key(path),
        };
        if !comes {
            continue;
        }
        if let (Some(k), Some(id)) = (key, f.get("id")) {
            added_ids.insert(k, id.to_string());
        }
        added.insert(path.clone(), text.clone());
    }
    let changed: BTreeSet<&str> = source
        .nodes
        .iter()
        .filter(|n| !adopt.contains_key(&n.path))
        .map(|n| n.path.as_str())
        .collect();
    let mut shared = 0;
    let mut warnings = Vec::new();
    let mut collisions = BTreeMap::new();
    for n in source
        .nodes
        .iter()
        .filter(|n| changed.contains(n.path.as_str()))
    {
        let Some(id) = frontmatter_lines(&n.content)
            .get("id")
            .map(|id| id.to_string())
        else {
            continue;
        };
        let key = format!("{}:{id}", n.ws);
        // An identity v2 node is one node wherever its UUID is, even one
        // main got by a squash or a cherry-pick, with no shared history.
        let same_uuid = |theirs: &str| {
            source
                .v2
                .as_ref()
                .and_then(|v2| v2.uuids.get(&n.path))
                .is_some_and(|uuid| theirs.ends_with(&format!("-{uuid}")))
        };
        if ids.contains_key(&key) {
            shared += 1;
        } else if let Some(theirs) = main_ids.get(&key).filter(|t| same_uuid(t)) {
            ids.insert(key.clone(), theirs.clone());
            shared += 1;
        } else if let Some(theirs) = main_ids.get(&key) {
            collisions.insert(key.clone(), theirs.clone());
            warnings.push(PlanWarning {
                code: "migrate.legacy_id_collision".into(),
                path: Some(n.path.clone()),
                message: format!(
                    "{id} is a node this branch made after it forked from {reference}, \
                     which made a different {id} ({theirs}): it gets its own id, and refs \
                     to {id} in this branch's nodes name it, but it has no legacy_id, so \
                     after the merge the handle {id} names main's"
                ),
            });
        }
    }
    let main_plan = plan_of(top, &migration);
    let mut cited = BTreeSet::new();
    for text in tip_text.values() {
        for part in text.split("migration digest ").skip(1) {
            let digest: String = part.chars().take("sha256:".len() + 64).collect();
            if digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64 {
                cited.insert(digest);
            }
        }
    }
    Ok(Some(Follow {
        against: Against {
            reference,
            tip,
            migration,
            fork,
            adopted: adopt.len(),
            added: added.len(),
            shared,
            work_history: main_history,
            explicit: explicit.is_some(),
            replayed: 0,
        },
        graph_id,
        ids,
        adopt,
        added,
        added_ids,
        collisions,
        warnings,
        main_plan,
        migration_parent: parent,
        cited,
    }))
}

#[cfg(test)]
mod tests {
    use super::moved_line;

    #[test]
    fn only_a_generated_line_says_a_section_moved() {
        let digest = "0".repeat(64);
        assert!(moved_line(&format!(
            "Progress notes moved to the work log at migration: review record, migration digest sha256:{digest}."
        )));
        assert!(moved_line(&format!(
            "The review ledger moved to the work log at migration: review record, migration digest sha256:{digest}."
        )));
        // Prose that quotes the phrase isn't one.
        assert!(!moved_line(
            "Our notes say they were moved to the work log at migration, which is wrong."
        ));
        assert!(!moved_line(
            "Progress notes moved to the work log at migration: review record, migration digest sha256:abc."
        ));
    }
}
