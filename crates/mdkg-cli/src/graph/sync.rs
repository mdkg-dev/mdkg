//! `mdkg graph remote approve`, `graph sync`, `graph conflicts` and
//! `graph resolve`: graph.remote sync for a person (edd-01a18867, section
//! 4). The engine is `mdkg_graph::sync`; this module adds the person
//! check, the protected list, the secret scanner and the output.
//!
//! **Who.** Each of these commands is refused inside an agent's turn
//! (`graph.remote_unapproved` for approve, `graph.sync_in_turn` for the
//! rest): the same outside-turn check delegated decisions use
//! (`mdkg_run::approval::outside_turn`: the turn mark, live launcher turns
//! of this clone, and the process's ancestry). Until person keys land
//! (d-s9), that check is the boundary.

use std::path::Path;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_graph::graphref::{self, GraphRef};
use mdkg_graph::sync::guard::{ScanResult, Scanner};
use mdkg_graph::sync::{self, Approval, Guards, Options, Who};
use schemars::JsonSchema;
use serde::Serialize;

use crate::coord::graph_git::{graph_error, graph_info, graph_warning};
use crate::output::{self, Report};

/// `mdkg graph remote` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum RemoteCommand {
    /// Approve graph.remote's destination, so graph pushes may go there
    /// (G23). Shows the remote, its remoteRef and its effective fetch and
    /// push destinations, and pins them in <git common dir>/mdkg/graph/remote.json.
    /// Only a person runs it, outside any agent turn; configuration alone
    /// never pushes. Run it again after changing the remote's URL or
    /// remoteRef.
    #[command(after_help = examples!("graph remote approve", "graph remote approve --dry-run --json"))]
    Approve {
        /// Show what would be pinned; pin nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The graph sync record of one conflict.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictRecord {
    /// The sync merge commit that took the remote's version.
    pub merge: String,
    pub path: String,
    /// The local version's blob (`null`: the local side deleted it).
    pub local: Option<String>,
    /// The remote version's blob (`null`: the remote deleted it).
    pub remote: Option<String>,
    /// When it was recorded, in Unix seconds.
    pub time: u64,
}

impl From<&sync::Conflict> for ConflictRecord {
    fn from(c: &sync::Conflict) -> Self {
        Self {
            merge: c.merge.clone(),
            path: c.path.clone(),
            local: c.local.clone(),
            remote: c.remote.clone(),
            time: c.time,
        }
    }
}

/// `graph remote approve`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ApproveData {
    pub remote: String,
    pub remote_ref: String,
    /// The effective fetch destinations, normalized (host and path).
    pub fetch: Vec<String>,
    /// The effective push destinations, normalized.
    pub push: Vec<String>,
    /// The graph remote is the code's own remote (the personal setup).
    pub code_remote: bool,
    /// What was approved before, when it differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Written (false on `--dry-run`).
    pub pinned: bool,
}

/// `graph sync`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SyncData {
    pub remote: String,
    pub remote_ref: String,
    /// The destination is approved and unchanged: pushes may go there.
    pub approved: bool,
    /// The local graph tip before and after.
    pub before: String,
    pub after: String,
    /// The remote's graph tip after the sync (`null`: it has none).
    pub remote_tip: Option<String>,
    pub pushed: bool,
    /// The local graph holds commits the remote doesn't.
    pub ahead: bool,
    /// Paths a merge took the remote's version of (G12).
    pub conflicts: Vec<ConflictRecord>,
}

/// `graph conflicts`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictsData {
    pub conflicts: Vec<ConflictRecord>,
}

/// `graph resolve`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ResolveData {
    /// The conflicted paths settled, in one graph commit.
    pub paths: Vec<String>,
    /// `local` or `remote`.
    pub took: String,
    /// The graph commit that published the choice; `null` when the graph
    /// already held it.
    pub commit: Option<String>,
    /// Conflict entries cleared.
    pub cleared: usize,
}

/// Which side `graph resolve` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Take {
    Local,
    Remote,
}

fn from_ref(e: graphref::Error) -> GraphError {
    graph_error(e.code, e.message)
}

/// The clone's graph settings, in ref mode only.
fn ref_mode(root: &Path) -> Result<(std::path::PathBuf, GraphRef), GraphError> {
    let top = crate::coord::Repo::open(root)?.top;
    let graph = graphref::resolve(&top).map_err(from_ref)?.ok_or_else(|| {
        graph_error(
            "graph.unsupported",
            format!(
                "graph sync needs ref mode; this clone is in checkout mode (`{} install --setup personal` or `--setup professional --graph-remote <name>`)",
                mdkg_format::program::BIN
            ),
        )
    })?;
    Ok((top, graph))
}

/// Ends this command's graph binding, which holds the shared graph lock
/// while the cache is at the tip: a sync's fast-forward or merge and a
/// resolution take the exclusive lock, which this process's own shared one
/// would block forever (Terra #17687). Neither reads the cache; the next
/// command binds again and catches its cache up.
fn unbind() {
    crate::coord::graph_git::unbind();
}

/// Refuses inside an agent's turn, with `code`.
pub(crate) fn outside_turn(
    top: &Path,
    graph: &GraphRef,
    code: &'static str,
) -> Result<(), GraphError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| top.to_path_buf());
    let actor = std::env::var("MDKG_ACTOR")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_default();
    mdkg_run::approval::outside_turn(
        &graph.common,
        &actor,
        &[top, &cwd],
        std::env::var_os(mdkg_run::profile::TURN_MARK).is_some(),
    )
    .map_err(|why| {
        graph_error(
            code,
            format!("{why}; a person runs this outside any agent turn, in their own terminal"),
        )
    })
}

/// The protected branch names sync's destination guard refuses as
/// `remoteRef`: `main`, `master`, `mainRef`'s, every committed agent
/// definition's `protect` list, and GitHub's protection cache.
pub fn protected(top: &Path, graph: &GraphRef) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(authority) = graphref::authority(top) {
        for def in authority.definitions.values() {
            if let Some(list) = def.value.get("protect").and_then(|v| v.as_array()) {
                names.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_owned));
            }
        }
    }
    names.extend(crate::hook::git::github_protected(&graph.common));
    mdkg_run::profile::protected_refs(
        &mdkg_run::profile::ProtectedRefs {
            main_ref: graph.main_ref.as_deref(),
            ..Default::default()
        },
        &names,
    )
}

/// `secret.scan`'s scanner, the one git's hooks run.
#[derive(Debug)]
pub struct HookScanner;

impl Scanner for HookScanner {
    fn scan(&self, common: &Path, commits: &[String]) -> Result<ScanResult, String> {
        let scanned =
            crate::hook::secret::scan(Path::new("git"), common, commits).map_err(|e| e.0)?;
        Ok(ScanResult {
            finding: scanned.finding.map(|f| (f.commit, f.path)),
            unscanned: scanned.skipped,
        })
    }
}

/// The agent definition check strict validation runs, as the consumers
/// parse definitions.
fn definition_check(name: &str, value: &serde_json::Value) -> Result<(), String> {
    mdkg_run::orchestration::agent::Definition::parse(name, value.clone())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// One sync, for the launcher's timers or a person.
pub fn run_once(
    top: &Path,
    graph: &GraphRef,
    who: Who,
    push: bool,
    accept_rewrite: bool,
) -> Result<sync::Outcome, graphref::Error> {
    let protected = protected(top, graph);
    let net = sync::net::Git::new(&graph.common, top);
    // R13: pending write intents are replayed, with the store's record,
    // before sync moves the local graph.
    let replay = |writer: &mdkg_graph::refwrite::RefWriter,
                  lock: &mdkg_graph::refwrite::GraphLock|
     -> Result<(), graphref::Error> {
        crate::coord::graph_git::replay_intents(top, writer, lock)
            .map_err(|e| graphref::Error::new("graph.intents", e.to_string()))
    };
    sync::run(
        top,
        graph,
        &net,
        &Guards {
            protected: &protected,
            scanner: Some(&HookScanner),
            definitions: &definition_check,
            replay: Some(&replay),
        },
        Options {
            who,
            push,
            accept_rewrite,
        },
    )
}

pub fn approve(root: &Path, dry_run: bool) -> Result<Report<ApproveData>, GraphError> {
    let (top, graph) = ref_mode(root)?;
    outside_turn(&top, &graph, "graph.remote_unapproved")?;
    graph.remote.as_ref().ok_or_else(|| {
        graph_error(
            "graph.remote_invalid",
            format!(
                "no mdkg.graph.remote is set: this clone's graph is local only (`{} install --setup personal` or `--setup professional --graph-remote <name>`)",
                mdkg_format::program::BIN
            ),
        )
    })?;
    let now = sync::current(&top, &graph).map_err(from_ref)?;
    let base = sync::net::checkout(&graph.common, &top).map_err(from_ref)?;
    let protected = protected(&top, &graph);
    sync::guard::check_remote_ref(
        &graph.common,
        &now.remote,
        &now.remote_ref,
        graph.main_ref.as_deref(),
        &protected,
    )
    .map_err(from_ref)?;
    let code_remote = sync::guard::check_destination(
        &graph.common,
        &base,
        &now.remote,
        &now,
        graph.main_ref.as_deref(),
    )
    .map_err(from_ref)?;
    let previous = match sync::approval(&top, &graph).map_err(from_ref)? {
        Approval::Changed(why) => Some(why),
        _ => None,
    };
    if !dry_run {
        sync::approve(&top, &graph, &protected).map_err(from_ref)?;
    }
    Ok(Report::ok(ApproveData {
        remote: now.remote,
        remote_ref: now.remote_ref,
        fetch: now.fetch,
        push: now.push,
        code_remote,
        previous,
        pinned: !dry_run,
    }))
}

pub fn sync_now(
    root: &Path,
    no_push: bool,
    accept_rewrite: bool,
) -> Result<Report<SyncData>, GraphError> {
    let (top, graph) = ref_mode(root)?;
    outside_turn(&top, &graph, "graph.sync_in_turn")?;
    unbind();
    if graph.remote.is_none() {
        return Err(graph_error(
            "graph.remote_invalid",
            "no mdkg.graph.remote is set: this clone's graph is local only",
        ));
    }
    let out = run_once(&top, &graph, Who::Person, !no_push, accept_rewrite).map_err(from_ref)?;
    let mut notes = Vec::new();
    if let Some(backup) = &out.backup {
        notes.push(graph_info(
            "graph.remote_rewritten",
            format!(
                "adopted the rewritten {} from {}, with this clone's own changes since re-applied; the local graph from before is kept at {backup}",
                out.remote_ref, out.remote
            ),
        ));
    }
    for c in &out.conflicts {
        notes.push(graph_warning(
            "graph.sync_conflict",
            format!(
                "{}: the remote's version was taken; the local one is {} in {} (`{}` keeps it)",
                output::shown_path(&c.path),
                c.local
                    .as_deref()
                    .map_or("deleted", |o| o.get(..12).unwrap_or(o)),
                c.merge.get(..12).unwrap_or(&c.merge),
                resolve_command(&c.path, &c.merge)
            ),
        ));
    }
    Ok(Report::ok(SyncData {
        remote: out.remote,
        remote_ref: out.remote_ref,
        approved: out.approval == Approval::Approved,
        before: out.before,
        after: out.after,
        remote_tip: out.remote_tip,
        pushed: out.pushed,
        ahead: out.ahead,
        conflicts: out.conflicts.iter().map(ConflictRecord::from).collect(),
    })
    .with(notes))
}

pub(crate) fn tip(graph: &GraphRef) -> Option<String> {
    mdkg_git::common_command(&graph.common)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", graph.name),
        ])
        .run(None)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

pub fn conflicts(root: &Path) -> Result<Report<ConflictsData>, GraphError> {
    let (_, graph) = ref_mode(root)?;
    let list = sync::conflicts(&graph.common, tip(&graph).as_deref()).map_err(from_ref)?;
    Ok(Report::ok(ConflictsData {
        conflicts: list.iter().map(ConflictRecord::from).collect(),
    }))
}

/// `actor` is the command's `--as`, else the worktree's actor: the one
/// whose own lease doesn't block the resolution. Several paths settle
/// together, in one commit (a remote rename and a local edit are two
/// paths of one change), and the result passes the same strict
/// validation a sync's merge does before the graph moves (DECISION 18
/// (V)): a duplicate id or a broken definition is refused, and every
/// conflict stays open.
pub fn resolve(
    root: &Path,
    paths: &[String],
    take: Take,
    merge: Option<&str>,
    actor: Option<String>,
) -> Result<Report<ResolveData>, GraphError> {
    if let Some(m) = merge
        && !(m.len() >= 7 && m.len() <= 64 && m.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "--merge takes a merge commit's id, at least 7 hex digits",
        ));
    }
    let merge = merge.map(str::to_ascii_lowercase);
    let (top, graph) = ref_mode(root)?;
    outside_turn(&top, &graph, "graph.sync_in_turn")?;
    unbind();
    // Held from reading the entries to clearing them, so no sync in
    // between records or drops one (design review, Astra 2).
    let sync_lock = sync::lock(&graph.common).map_err(from_ref)?;
    let list = sync::conflicts(&graph.common, tip(&graph).as_deref()).map_err(from_ref)?;
    // Each path as the conflict records it: one typed as `graph
    // conflicts` shows it, hidden characters escaped, settles the same
    // conflict as the original (Terra #19906).
    let mut picked: Vec<&sync::Conflict> = paths
        .iter()
        .map(|p| select(&list, p, merge.as_deref()))
        .collect::<Result<_, _>>()?;
    picked.sort_by(|a, b| (&a.path, &a.merge).cmp(&(&b.path, &b.merge)));
    picked.dedup_by(|a, b| a.path == b.path && a.merge == b.merge);
    let paths: Vec<&String> = picked.iter().map(|c| &c.path).collect();
    let writer = graph
        .writer(&top, mdkg_graph::refwrite::LockMode::Local)
        .map_err(from_ref)?;
    let base = writer.snapshot().map_err(|e| from_ref(e.into()))?;
    let mut entries = Vec::new();
    let mut changes = Vec::new();
    for (path, c) in paths.iter().zip(&picked) {
        let oid = match take {
            Take::Local => c.local.clone(),
            Take::Remote => c.remote.clone(),
        };
        let bytes = match &oid {
            Some(oid) => {
                let out = mdkg_git::common_command(&graph.common)
                    .args(["cat-file", "blob", oid])
                    .run(None)
                    .map_err(|e| GraphError::failure(Code::IoGitFailed, e.to_string()))?;
                if !out.status.success() {
                    return Err(graph_error(
                        "graph.git",
                        format!("the {take:?} version {oid} isn't in this clone"),
                    ));
                }
                Some(out.stdout)
            }
            None => None,
        };
        let held = writer.read(&base, path).map_err(|e| from_ref(e.into()))?;
        not_leased(
            &top,
            actor.clone(),
            path,
            [held.as_deref(), bytes.as_deref()],
        )?;
        if held != bytes {
            changes.push(match bytes {
                Some(bytes) => mdkg_graph::refwrite::Change::Put {
                    path: (*path).clone(),
                    bytes,
                },
                None => mdkg_graph::refwrite::Change::Delete {
                    path: (*path).clone(),
                },
            });
        }
        entries.push(((*path).clone(), c.merge.clone()));
    }
    let commit = if changes.is_empty() {
        None
    } else {
        let shown: Vec<String> = paths.iter().map(|p| output::shown_path(p)).collect();
        let request = mdkg_graph::refwrite::Request {
            id: format!("sync-resolve-{}", mdkg_format::Uuid7::now()),
            base: Some(base),
            changes,
            message: format!("resolve sync conflict on {}", shown.join(", ")),
            trailers: vec![],
        };
        let lock = writer
            .lock(mdkg_graph::refwrite::LockKind::Exclusive)
            .map_err(|e| from_ref(e.into()))?;
        let prepared = writer
            .prepare(&lock, &request)
            .map_err(|e| from_ref(e.into()))?;
        let folder = graph.folder("").map_err(from_ref)?;
        let graph_id = prepared
            .parent
            .as_deref()
            .map(|p| writer.graph_id_at(p))
            .transpose()
            .map_err(|e| from_ref(e.into()))?
            .flatten()
            .unwrap_or_default();
        if let Err(e) = sync::validate_tip(
            &top,
            &folder,
            &graph_id,
            &prepared.commit,
            &definition_check,
        ) {
            writer.unpin(&lock, &prepared).ok();
            return Err(from_ref(e).with_hint(
                "nothing was published and the conflicts stay open; settle the paths that \
                 belong together (a rename's old and new path) in one `graph resolve`"
                    .to_owned(),
            ));
        }
        let receipt = writer
            .publish(&lock, &prepared)
            .map_err(|e| from_ref(e.into()))?;
        writer
            .unpin(&lock, &prepared)
            .map_err(|e| from_ref(e.into()))?;
        Some(receipt.commit)
    };
    // All in one write (Terra #19565): a retry after a crash here selects
    // the same entries and publishes nothing new.
    let cleared = sync::clear_conflicts(&graph.common, &sync_lock, &entries).map_err(from_ref)?;
    let took = match take {
        Take::Local => "local",
        Take::Remote => "remote",
    };
    Ok(Report::ok(ResolveData {
        paths: entries.into_iter().map(|(p, _)| p).collect(),
        took: took.into(),
        commit,
        cleared,
    }))
}

/// The one open conflict on `path` a resolution settles (DECISION 20,
/// Terra #19640): the only one, or the one in `merge` (a commit id
/// prefix). With several and no `merge`, it refuses and lists them, so a
/// retry after an uncertain outcome never settles an older conflict in
/// place of the one the person chose.
fn select<'a>(
    list: &'a [sync::Conflict],
    path: &str,
    merge: Option<&str>,
) -> Result<&'a sync::Conflict, GraphError> {
    let mut open: Vec<&sync::Conflict> = list.iter().filter(|c| c.path == path).collect();
    if open.is_empty() {
        // Not recorded as typed: the escaped form `graph conflicts` shows
        // for a path with hidden characters. An exact match always wins.
        open = list
            .iter()
            .filter(|c| c.path != path && output::shown_path(&c.path) == path)
            .collect();
    }
    let shown = output::shown_path(path);
    let found: Vec<&sync::Conflict> = match merge {
        Some(m) => open
            .iter()
            .copied()
            .filter(|c| c.merge.starts_with(m))
            .collect(),
        None => open.clone(),
    };
    match (found.as_slice(), merge) {
        ([c], _) => Ok(c),
        ([], None) => Err(GraphError::not_found(
            Code::NodeNotFound,
            format!("no recorded sync conflict for {shown}"),
        )),
        ([], Some(m)) => Err(GraphError::not_found(
            Code::NodeNotFound,
            format!("no recorded sync conflict for {shown} in merge {m}"),
        )),
        (several, _) => {
            let merges: Vec<String> = several
                .iter()
                .map(|c| c.merge[..c.merge.len().min(12)].to_owned())
                .collect();
            Err(GraphError::usage(
                Code::UsageInvalidArguments,
                format!(
                    "{shown} has {} open sync conflicts (merges {}); name one with --merge",
                    several.len(),
                    merges.join(", ")
                ),
            )
            .with_hint(format!(
                "`{} graph conflicts` lists them; settle the newest first",
                mdkg_format::program::BIN
            )))
        }
    }
}

/// Refuses a resolution of a node someone else holds a lease on (Terra
/// #17687): a worker's lease protects the graph version it works from,
/// and a person's resolution would overwrite it. The lease is looked up
/// by path and by the node's id in either version. A store that can't be
/// read refuses too, as every lease-checking command does.
fn not_leased<'a>(
    top: &Path,
    actor: Option<String>,
    path: &str,
    versions: impl IntoIterator<Item = Option<&'a [u8]>>,
) -> Result<(), GraphError> {
    let Some(view) = crate::coord::View::open(top)? else {
        return Ok(());
    };
    let actor = crate::coord::Repo::open(top)?.actor(actor);
    let ids: Vec<Option<String>> = versions
        .into_iter()
        .flatten()
        .flat_map(frontmatter_ids)
        .map(Some)
        .chain([None])
        .collect();
    for id in &ids {
        if let Some(lease) = view.holder(path, id.as_deref())
            && lease.owner != actor
        {
            let owner = lease.owner.clone();
            let mut e = GraphError::conflict(
                Code::CoordLeaseHeld,
                format!(
                    "{} is leased by {owner}; resolve it once their lease ends, or ask them to",
                    output::shown_path(path)
                ),
            );
            *e.diagnostic = e.diagnostic.as_ref().clone().held_by([owner]);
            return Err(e);
        }
    }
    Ok(())
}

/// The ids a node file's front matter may name: what the format's parser
/// reads (either lexical rule), and a tolerant scan beside it (CRLF lines,
/// spaces around the key), so a lookup never misses a valid spelling
/// (Terra #17986). Extra candidates only mean extra lease lookups.
fn frontmatter_ids(bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for lex in [
        mdkg_format::frontmatter::Lexical::Js,
        mdkg_format::frontmatter::Lexical::Ascii,
    ] {
        if let Ok(parsed) = mdkg_format::frontmatter::parse_with(text, "node.md", lex)
            && let Some(id) = parsed.frontmatter.get("id").and_then(|v| v.as_str())
        {
            ids.push(id.trim().trim_matches(['"', '\'']).trim().to_owned());
        }
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
    if lines.next().map(str::trim_end) == Some("---") {
        for line in lines {
            if line.trim_end() == "---" {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.trim() == "id"
            {
                ids.push(value.trim().trim_matches(['"', '\'']).trim().to_owned());
            }
        }
    }
    ids.retain(|id| !id.is_empty());
    ids.sort();
    ids.dedup();
    ids
}

/// Text output for `graph sync`.
pub fn print_sync(d: &SyncData) {
    let short = |s: &str| s.get(..12).unwrap_or(s).to_owned();
    if d.before == d.after {
        println!("graph {}: unchanged", short(&d.after));
    } else {
        println!("graph {} -> {}", short(&d.before), short(&d.after));
    }
    let remote = d.remote_tip.as_deref().map_or("none".into(), short);
    println!("remote {} {}: {remote}", d.remote, d.remote_ref);
    if d.pushed {
        println!("pushed");
    } else if d.ahead {
        println!("ahead of the remote; not pushed");
    }
}

/// Text output for `graph remote approve`.
pub fn print_approve(d: &ApproveData) {
    println!("graph remote: {} ({})", d.remote, d.remote_ref);
    for f in &d.fetch {
        println!("  fetch: {f}");
    }
    for p in &d.push {
        println!("  push:  {p}");
    }
    if d.code_remote {
        // Nothing records the setup, so a person reads this before
        // approving (design review minor 3).
        println!(
            "  this is your code's own remote: the graph ref goes where your code goes, with the \
             code's history in a personal (full-shape) graph. In a company repository, name a \
             graph remote of its own instead"
        );
    }
    if let Some(p) = &d.previous {
        println!("  changed since the last approval: {p}");
    }
    println!(
        "{}",
        if d.pinned {
            "approved"
        } else {
            "dry run: nothing pinned"
        }
    );
}

/// Text output for `graph conflicts`.
pub fn print_conflicts(d: &ConflictsData) {
    if d.conflicts.is_empty() {
        println!("no sync conflicts");
    }
    for c in &d.conflicts {
        println!(
            "{}  local={} remote={}  merge {}",
            output::shown_path(&c.path),
            c.local.as_deref().unwrap_or("none"),
            c.remote.as_deref().unwrap_or("none"),
            c.merge.get(..12).unwrap_or(&c.merge)
        );
    }
}

/// Diagnostics `status` adds for graph sync: offline, a hook's refusal,
/// unresolved conflicts, an unapproved or changed destination.
pub fn status_notes(graph: &GraphRef) -> Vec<Diagnostic> {
    let mut notes = Vec::new();
    let Some(remote) = &graph.remote else {
        return notes;
    };
    // The checkout this command was started in, where sync would run git.
    let from = crate::coord::graph_git::bound()
        .map(|m| m.checkout.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    match sync::approval(&from, graph) {
        Ok(Approval::Approved) => {}
        Ok(Approval::Unapproved) => notes.push(graph_info(
            "graph.remote_unapproved",
            format!(
                "graph remote {} isn't approved, so nothing is pushed; a person runs `{} graph remote approve`",
                remote.name,
                mdkg_format::program::BIN
            ),
        )),
        Ok(Approval::Changed(why)) => notes.push(graph_warning(
            "graph.remote_changed",
            format!(
                "graph remote {}'s destination changed since it was approved ({why}); sync is stopped until a person approves it again",
                remote.name
            ),
        )),
        Err(e) => notes.push(graph_warning(e.code, e.message)),
    }
    if let Ok(state) = sync::State::load(&graph.common) {
        if let Some(r) = &state.refused {
            // The latch of a push whose outcome is unknown says so; only a
            // recorded refusal names a hook (Terra #17986).
            let why = if r.code == sync::UNRECORDED {
                r.shown()
            } else {
                format!("a hook refused the last graph push ({})", r.shown())
            };
            notes.push(graph_warning(
                "graph.push_refused",
                format!(
                    "{why}; background sync waits for a person's `{} graph sync`",
                    mdkg_format::program::BIN
                ),
            ));
        } else if let Some(f) = &state.last_error {
            notes.push(graph_warning(
                // The stored code is ours; anything else reads as a git error.
                code_of(&f.code),
                format!(
                    "graph sync failing since {} ({} in a row): {}",
                    time_text(f.time),
                    state.attempts,
                    f.shown()
                ),
            ));
        }
    }
    match sync::conflicts(&graph.common, tip(graph).as_deref()) {
        Ok(list) if !list.is_empty() => notes.push(graph_warning(
            "graph.sync_conflict",
            format!(
                "{} unresolved graph sync conflict(s); `{} graph conflicts` lists them",
                list.len(),
                mdkg_format::program::BIN
            ),
        )),
        Ok(_) => {}
        // Shown, never read as none (Terra #19873), under the listing
        // error's own code: no conflict is known yet (Terra #19929).
        Err(e) => notes.push(graph_warning(
            e.code,
            format!("the graph sync conflicts can't be listed: {}", e.message),
        )),
    }
    notes
}

fn code_of(code: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "graph.remote_unreachable",
        "graph.remote_auth",
        "graph.remote_rewritten",
        "graph.remote_changed",
        "graph.remote_is_code",
        "graph.remote_invalid",
        "graph.secret",
        "graph.secret_scan_unavailable",
        "graph.push_large",
        "graph.shape",
        "graph.overlapping_workspace",
        "graph.sync_invalid",
        "graph.sync_contended",
        "graph.identity_unset",
    ];
    KNOWN
        .iter()
        .find(|k| **k == code)
        .copied()
        .unwrap_or("graph.git")
}

/// Unix seconds as UTC text.
pub fn time_text(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The command that keeps the local version of `path` from `merge`, safe
/// to paste (Terra #19803, #19859): options first, then `--`, then the
/// path always single-quoted, so `$(...)`, a quote, a space, a leading `-`
/// or `=` is passed as typed. Hidden characters are escaped, as `graph
/// conflicts` shows them, and `graph resolve` accepts that form for a
/// path recorded with them (Terra #19906).
fn resolve_command(path: &str, merge: &str) -> String {
    format!(
        "{} graph resolve --take local --merge {} -- '{}'",
        mdkg_format::program::BIN,
        merge.get(..12).unwrap_or(merge),
        output::shown_path(path).replace('\'', "'\"'\"'")
    )
}

#[cfg(test)]
mod tests {
    use super::frontmatter_ids;

    #[test]
    fn the_suggested_resolve_command_quotes_the_path() {
        let merge = "0123456789abcdef0123456789abcdef01234567";
        let cmd = super::resolve_command(".mdkg/$(touch marker) it's.txt", merge);
        assert!(
            cmd.ends_with(
                " graph resolve --take local --merge 0123456789ab -- '.mdkg/$(touch marker) it'\"'\"'s.txt'"
            ),
            "{cmd}"
        );
        for path in ["-team/.mdkg/work/n.md", "=x/.mdkg/n.md"] {
            let cmd = super::resolve_command(path, merge);
            assert!(cmd.ends_with(&format!(" -- '{path}'")), "{cmd}");
        }
        // A hidden character is shown escaped, as `graph conflicts` lists
        // it and `graph resolve` accepts it.
        let cmd = super::resolve_command(".mdkg/a\u{200d}b\nc.md", merge);
        assert!(cmd.ends_with(" -- '.mdkg/a\\u{200d}b\\u{a}c.md'"), "{cmd}");
        // What a POSIX shell passes for the quoted path is the path.
        let path = "-x/.mdkg/$(touch m) it's.txt";
        let cmd = super::resolve_command(path, merge);
        let quoted = cmd.split(" -- ").nth(1).expect("path word");
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("printf '%s' {quoted}")])
            .output()
            .expect("sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout), path);
    }

    #[test]
    fn every_valid_spelling_of_the_id_is_found() {
        let id = "task-01a1a432-0827-7632-851d-9a4435c3ef63";
        for text in [
            format!("---\nid: {id}\ntype: task\n---\nbody\n"),
            format!("---\r\nid: {id}\r\ntype: task\r\n---\r\nbody\r\n"),
            format!("---\nid :  {id}  \n---\n"),
            format!("---\n  id: \"{id}\"\n---\n"),
        ] {
            assert_eq!(frontmatter_ids(text.as_bytes()), [id], "{text:?}");
        }
        assert!(frontmatter_ids(b"no front matter\nid: x\n").is_empty());
    }
}
