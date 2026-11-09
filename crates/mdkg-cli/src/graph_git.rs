//! Ref mode's graph access (edd-01a18867, "Worktree-free writes"): the
//! binding of a command to the graph cache, the `GraphGit` seam for
//! graph-file git, the guard that refuses a raw git call on a graph path,
//! and the write path (prepare, record, publish).
//!
//! - **Binding.** In ref mode (`mdkg.graph.ref` set) a command runs against
//!   the graph cache, `<common>/mdkg/graph/cache/`, never a checkout.
//!   [`bind`] checks the graph ref and its identity, replays pending
//!   intents, brings the cache to the tip, and hands the command the
//!   cache as its graph root. `Repo::open` of that root is the ref-mode
//!   `Repo`: `root` is the cache, `top` the invoking checkout (code paths,
//!   the store, worktrees).
//! - **`GraphGit`.** Every graph-file git question goes through
//!   [`GraphGit`], bound to the cache and its manifest commit C. Checkout
//!   mode forwards to today's `git::…(&repo.top, …)`.
//! - **The guard.** Writers import [`git`], which forwards to
//!   `mdkg_coord_sqlite::git` but refuses (`graph.checkout_read`) a call
//!   whose path is under a graph folder while ref mode is bound: a panic in
//!   debug builds, so tests catch a missed site, and an error in release.
//! - **Writes.** [`prepare_record_publish`] runs one op as a write session:
//!   the exclusive lock, replay, refresh and observation, the op's edits,
//!   the touched-path check, prepare (commit, pending ref, intent), the
//!   store record, publish. A refusal before publication restores the
//!   session's files; a crash is finished or discarded by the next replay.

// `main.rs` (the bind call) and `write.rs` (the writers' call sites) are
// held by other lanes for now; until they call in, parts of this are
// reached only from tests.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mdkg_coord::{Scope, Source};
use mdkg_coord_sqlite::Store;
use mdkg_coord_sqlite::git as raw;
use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::GraphError;
use mdkg_graph::cache::{self, Cache, Drift, DriftKind};
use mdkg_graph::graphref::{self, GraphRef};
use mdkg_graph::intent::{Intent, Journal, Replayed};
use mdkg_graph::refwrite::{self, GraphLock, LockKind, LockMode, Prepared, Receipt, RefWriter};

/// A `graph.*` error. The code is the edd's; until the registry has it,
/// the nearest registered code carries it, and the message names it.
pub(crate) fn graph_error(code: &'static str, message: impl Into<String>) -> GraphError {
    let message = message.into();
    match Code::parse(code) {
        // The graph moved under the write: a coordination conflict (exit
        // 5, re-read and retry), not a failure (orchestrator on #17713).
        Some(c @ (Code::GraphWriteStale | Code::GraphWriteContended)) => {
            GraphError::conflict(c, message)
        }
        Some(c) => GraphError::failure(c, message),
        None => GraphError::failure(Code::IoGitFailed, format!("{code}: {message}")),
    }
}

/// A `graph.*` note, as [`graph_error`] spells it.
pub(crate) fn graph_info(code: &'static str, message: impl Into<String>) -> Diagnostic {
    let message = message.into();
    match Code::parse(code) {
        Some(c) => Diagnostic::new(c, Severity::Info, message),
        None => Diagnostic::new(
            Code::IoGitFailed,
            Severity::Info,
            format!("{code}: {message}"),
        ),
    }
}

/// A `graph.*` warning, as [`graph_error`] spells it.
pub(crate) fn graph_warning(code: &'static str, message: impl Into<String>) -> Diagnostic {
    let message = message.into();
    match Code::parse(code) {
        Some(c) => Diagnostic::warning(c, message),
        None => Diagnostic::warning(Code::IoGitFailed, format!("{code}: {message}")),
    }
}

/// Whether `e` carries `code`: as its registered code, or named in its
/// message while the registry lacks it ([`graph_error`]).
#[cfg(test)]
pub(crate) fn has_code(e: &GraphError, code: &str) -> bool {
    e.diagnostic.code.as_str() == code || e.diagnostic.message.contains(code)
}

fn from_ref(e: graphref::Error) -> GraphError {
    graph_error(e.code, e.message)
}

fn from_write(e: refwrite::Error) -> GraphError {
    let code = e.code();
    let error = graph_error(code, e.to_string());
    match code {
        "graph.write_stale" => {
            error.with_hint("the graph changed under your lease: re-read the node and retry")
        }
        "graph.write_contended" => error.with_hint("the graph ref kept moving: run it again"),
        _ => error,
    }
}

fn io_error(e: io::Error) -> GraphError {
    graph_error("graph.io", e.to_string())
}

/// One command's ref-mode binding.
#[derive(Debug)]
pub(crate) struct RefMode {
    pub(crate) graph: GraphRef,
    /// The invoking checkout's top folder (code paths, the store).
    pub(crate) checkout: PathBuf,
    pub(crate) cache: Cache,
    /// The graph-owned folders, repository-relative (`.mdkg`, ...).
    pub(crate) paths: Vec<String>,
    /// The graph id the tip holds.
    pub(crate) graph_id: String,
    /// The manifest commit C the cache holds.
    commit: Mutex<String>,
    /// The shared lock a reader holds while the cache is at the tip; a
    /// write session gives it up for the exclusive one.
    shared: Mutex<Option<GraphLock>>,
}

impl RefMode {
    /// The cache folder, which is the graph root.
    pub(crate) fn root(&self) -> &Path {
        self.cache.root()
    }

    /// The commit the cache holds (C).
    pub(crate) fn commit(&self) -> String {
        self.commit.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn set_commit(&self, commit: &str) {
        if let Ok(mut c) = self.commit.lock() {
            *c = commit.to_owned();
        }
    }

    /// Whether repository-relative `rel` lies in a graph folder (as scopes
    /// match: ASCII case ignored).
    pub(crate) fn holds(&self, rel: &str) -> bool {
        let scope = Scope::new(rel.trim_start_matches("./"));
        self.paths
            .iter()
            .any(|p| Scope::new(p.as_str()).contains(&scope))
    }

    fn writer(&self) -> Result<RefWriter, GraphError> {
        self.graph
            .writer(&self.checkout, LockMode::Local)
            .map_err(from_ref)?
            .with_paths(&self.paths)
            .map_err(from_write)
    }
}

// Per thread: a command runs on one thread (MCP runs each call as its own
// process), and tests bind different fixtures side by side.
thread_local! {
    static BOUND: std::cell::RefCell<Option<Arc<RefMode>>> = const { std::cell::RefCell::new(None) };
}

/// The bound ref mode, if any.
pub(crate) fn bound() -> Option<Arc<RefMode>> {
    BOUND.with(|b| b.borrow().clone())
}

/// The bound ref mode when `root` is its cache (the graph root `bind`
/// handed the command).
pub(crate) fn for_root(root: &Path) -> Option<Arc<RefMode>> {
    let mode = bound()?;
    let same = root == mode.root()
        || std::fs::canonicalize(root)
            .ok()
            .zip(std::fs::canonicalize(mode.root()).ok())
            .is_some_and(|(a, b)| a == b);
    same.then_some(mode)
}

/// Ends the binding (a long-running process between commands; tests).
pub(crate) fn unbind() {
    BOUND.with(|b| b.borrow_mut().take());
}

/// Binds this command to the graph ref when the clone is in ref mode, and
/// returns the graph root it runs against (the cache); `None` in checkout
/// mode. Refuses a missing, symbolic or checked-out graph ref, and a tip
/// whose graph id this clone doesn't record. Pending intents are replayed
/// and the cache is brought to the tip (rescuing hand edits first).
pub(crate) fn bind(checkout: &Path) -> Result<Option<PathBuf>, GraphError> {
    bind_with(checkout, false)
}

/// Status never replays an intent or writes the coordinator. The derived
/// cache can still refresh without opening a coordinator connection.
pub(crate) fn bind_status(checkout: &Path) -> Result<Option<PathBuf>, GraphError> {
    bind_with(checkout, true)
}

/// Status shares one five-second budget across its read lock and a cache
/// refresh's exclusive lock. Existing writers retain their lock policy.
fn bind_lock(
    writer: &RefWriter,
    kind: LockKind,
    status: bool,
    deadline: std::time::Instant,
) -> Result<GraphLock, GraphError> {
    if !status {
        return writer.lock(kind).map_err(from_write);
    }
    loop {
        let lock = match kind {
            LockKind::Shared => GraphLock::try_shared(writer.common(), writer.lock_mode()),
            LockKind::Exclusive => GraphLock::try_exclusive(writer.common(), writer.lock_mode()),
        }
        .map_err(from_write)?;
        if let Some(lock) = lock {
            return Ok(lock);
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(graph_error(
                "graph.lock_required",
                format!(
                    "status graph lock wait exceeded 5 s at {} (holder unknown)",
                    writer.common().join("mdkg/graph/lock").display()
                ),
            ));
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(20)));
    }
}

fn bind_with(checkout: &Path, status: bool) -> Result<Option<PathBuf>, GraphError> {
    // The same checked open every command makes (#53: once per command),
    // then one read of `mdkg.graph.ref` from the common config file. Not
    // in a repository git can open, or no graph ref named: checkout mode,
    // and the command goes on and reports what it finds. Only a clone in
    // ref mode pays for the full resolve.
    let repo = match crate::coord::Repo::open(checkout) {
        Ok(repo) => repo,
        // A refused open (a skipped `.git` that may hold the store, an
        // untrusted worktree link, an old git) can still be a ref-mode
        // clone git opens: never fall back to its checkout's `.mdkg` then
        // (Terra #10293). The probe uses g-core's own git layer, and runs
        // only on this path, so a normal open costs nothing more (#53).
        Err(e) => {
            return match probe_graph_ref(checkout) {
                Some(false) => Ok(None),
                // Named, or the config can't be read: fail closed (Terra
                // #10551). Outside any repository git opens (`None`), the
                // command goes on as before.
                Some(true) => Err(e),
                None => Ok(None),
            };
        }
    };
    let Ok(common) = raw::common_dir(&repo.top) else {
        return Ok(None);
    };
    if !names_graph_ref(&repo.top, &common) {
        return Ok(None);
    }
    let Some(graph) = graphref::resolve(checkout).map_err(from_ref)? else {
        return Ok(None);
    };
    if let Some(step) = migrating(&graph.common) {
        return Err(graph_error(
            "graph.migrating",
            format!("a migration to the graph ref is part-way through (step {step})"),
        )
        .with_hint(format!("run `{} install` to finish it", crate::BIN)));
    }
    let top = raw::toplevel(checkout).map_err(io_error)?;
    let tip = graphref::tip(&top, &graph).map_err(from_ref)?;
    // Ref mode keeps the root graph at `.mdkg` (#16095, orchestrator #19410).
    crate::config_cmd::root_graph_folder(&top, &graph, Some(&tip))?;
    // Only the config and graph id are used here: an agent definition is
    // checked by the entry gate's full read where a command needs it, and
    // never blocks coordination (orchestrator #15706).
    let authority = graphref::config_at(&top, &tip).map_err(from_ref)?;
    graph
        .check_identity(&authority.graph_id)
        .map_err(from_ref)?;
    let config = mdkg_graph::config::from_snapshot(authority.config)?;
    let paths = graphref::graph_paths(&config).map_err(from_ref)?;
    let cache = Cache::open(&top).map_err(from_write)?;
    let mode = Arc::new(RefMode {
        graph,
        checkout: top,
        cache,
        paths,
        graph_id: authority.graph_id,
        commit: Mutex::new(String::new()),
        shared: Mutex::new(None),
    });
    let writer = mode.writer()?;
    // A cache at the tip, with nothing to replay or end, is read under the
    // shared lock; anything else takes the exclusive one to catch up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let shared = bind_lock(&writer, LockKind::Shared, status, deadline)?;
    let pending = Journal::open(&mode.graph.common)
        .and_then(|j| j.pending())
        .map_err(io_error)?;
    let current = mode
        .cache
        .manifest()
        .map_err(from_write)?
        .is_some_and(|m| m.commit == tip && m.paths == sorted(&mode.paths))
        && mode.cache.session_record().map_err(from_write)?.is_none()
        && pending.is_empty()
        && mode.cache.verify().map_err(from_write)?;
    if current {
        mode.set_commit(&tip);
        if let Ok(mut s) = mode.shared.lock() {
            *s = Some(shared);
        }
    } else {
        drop(shared);
        let lock = bind_lock(&writer, LockKind::Exclusive, status, deadline)?;
        catch_up(&mode, &writer, &lock, None, status)?;
    }
    let root = mode.root().to_path_buf();
    BOUND.with(|b| *b.borrow_mut() = Some(mode));
    Ok(Some(root))
}

/// After a refused open: whether the repository git finds from `dir`
/// names a graph ref (`Some(true)` also when its config can't be read),
/// `None` when git opens no repository there. Plain git, not the checked
/// command builder (whose check just refused), with no inherited `GIT_*`
/// variable, no hooks, and includes never followed, so a relative include
/// can't make it fail open.
fn probe_graph_ref(dir: &Path) -> Option<bool> {
    // A folder that doesn't exist holds no repository; checked before
    // spawning, so a git that can't start for any other reason (no git,
    // no processes or descriptors left) fails closed (Terra #10802).
    if !dir.is_dir() {
        return None;
    }
    let (program, path) = raw::git_program();
    let git = |args: &[&std::ffi::OsStr]| {
        let mut c = std::process::Command::new(program);
        c.env("PATH", path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        for (key, _) in std::env::vars_os() {
            let k = key.to_string_lossy();
            if k.starts_with("GIT_") && k != "GIT_TERMINAL_PROMPT" && k != "GIT_OPTIONAL_LOCKS" {
                c.env_remove(&key);
            }
        }
        c.current_dir(dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args);
        bounded(c)
    };
    // A git that doesn't answer in time (a FIFO planted in the git
    // folder), or can't start, is unknown: fail closed, as the refused
    // open already did. One that answers it's not a repository: checkout
    // mode.
    // Plain `--git-common-dir` (every git has it; `--path-format` needs
    // 2.31, and an old git is one reason an open is refused): a relative
    // answer is relative to `dir`, where git ran.
    let common = match git(&["rev-parse".as_ref(), "--git-common-dir".as_ref()]) {
        Ok(out) if out.status.success() => out,
        Ok(_) => return None,
        Err(Bounded::NoStart | Bounded::TimedOut) => return Some(true),
    };
    let common = String::from_utf8_lossy(&common.stdout).trim().to_owned();
    if common.is_empty() || common.starts_with('-') {
        // A repository git opened but whose answer makes no sense: unknown.
        return Some(true);
    }
    let common = dir.join(common);
    let config = common.join("config");
    // `config --file` reads a missing file as empty (exit 1, "unset"): a
    // common folder without its config file (an old git's answer from a
    // subfolder, relative to the top) is unknown, so fail closed (Terra
    // #10861).
    if !std::fs::symlink_metadata(&config).is_ok_and(|m| m.is_file()) {
        return Some(true);
    }
    let named = git(&[
        "config".as_ref(),
        "--no-includes".as_ref(),
        "--file".as_ref(),
        config.as_os_str(),
        "--get".as_ref(),
        "mdkg.graph.ref".as_ref(),
    ]);
    // Unset (exit 1) is checkout mode; anything else, an unreadable
    // config included, counts as named.
    Some(!matches!(named, Ok(o) if o.status.code() == Some(1)))
}

/// Why [`bounded`] has no output.
enum Bounded {
    /// The command couldn't start (no such folder, no git).
    NoStart,
    /// It didn't finish within the time limit, and was killed.
    TimedOut,
}

/// How long one probe git call may run: it runs only after an open was
/// refused, which may itself have waited out the full git time limit (a
/// planted FIFO), so it gets a few seconds, or `$MDKG_GIT_TIMEOUT_MS` when
/// that is shorter (Terra #10725).
const PROBE_LIMIT_MS: u64 = 5_000;

/// Runs `cmd` with no input, within the probe's time limit; one that
/// doesn't finish in time is killed.
fn bounded(mut cmd: std::process::Command) -> Result<std::process::Output, Bounded> {
    use std::io::Read as _;
    let limit = std::env::var("MDKG_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(PROBE_LIMIT_MS)
        .min(PROBE_LIMIT_MS);
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| Bounded::NoStart)?;
    let mut stdout = child.stdout.take().ok_or(Bounded::NoStart)?;
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).ok();
        send.send(out).ok();
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(limit);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = recv
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr: Vec::new(),
                });
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                return Err(Bounded::TimedOut);
            }
        }
    }
}

/// Whether the common config file sets `mdkg.graph.ref` (includes never
/// followed: the key lives in that file only). A file git can't read
/// counts as naming one, so the full resolve reports why.
fn names_graph_ref(_top: &Path, common: &Path) -> bool {
    mdkg_git::own_config_value(common, "mdkg.graph.ref").map_or(true, |value| value.is_some())
}

fn sorted(paths: &[String]) -> Vec<String> {
    let mut p = paths.to_vec();
    p.sort();
    p
}

/// `mdkg.graph.migrating`, read from the common config.
fn migrating(common: &Path) -> Option<String> {
    mdkg_git::own_config_value(common, "mdkg.graph.migrating")
        .ok()
        .flatten()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Under the exclusive lock: replays pending intents, then brings the
/// cache to the tip, reporting what each step found. `store` is the
/// command's store when it has one open; otherwise one is opened only if
/// an intent is pending.
fn catch_up(
    mode: &RefMode,
    writer: &RefWriter,
    lock: &GraphLock,
    store: Option<&mut Store>,
    status: bool,
) -> Result<String, GraphError> {
    let journal = Journal::open(&mode.graph.common).map_err(io_error)?;
    let pending = journal.pending().map_err(io_error)?;
    if status && !pending.is_empty() {
        // Replay only needs the coordinator's reservations. Inspect them
        // through a reader, even while a real writer owns the WAL lock.
        let path = raw::store_path(&mode.checkout).map_err(io_error)?;
        crate::coord::unreachable_store(&path)?;
        let store = Store::open_read_only(&path)
            .map_err(|e| crate::coord::store_error_in(e, Some(&mode.checkout)))?;
        let mut reserved = 0;
        for intent in &pending {
            let recorded = match &store {
                Some(store) => recorded(store, intent)?,
                None => intent.leases.is_empty(),
            };
            if recorded {
                reserved += 1;
            }
        }
        return Err(
            graph_error(
                "graph.intents",
                format!(
                    "{} graph write intent(s) await replay ({reserved} recorded); status left them pending",
                    pending.len()
                ),
            )
            .with_hint(format!(
                "run `{} renew` to recover the graph, then retry status",
                mdkg_format::program::BIN
            )),
        );
    }
    let mut published: BTreeSet<String> = BTreeSet::new();
    if !pending.is_empty() {
        let mut opened;
        let store = match store {
            Some(s) => s,
            None => {
                opened = Store::open_for_repo(&mode.checkout, crate::coord::ttl_ms())
                    .map_err(crate::coord::store_error)?;
                &mut opened
            }
        };
        let replayed = journal
            .replay(writer, lock, |intent| recorded(store, intent))
            .map_err(from_write)?;
        for r in replayed {
            report_replay(&journal, &r);
            if let Replayed::Published { id, .. } = r {
                published.insert(id);
            }
        }
    }
    let tip = graphref::tip(&mode.checkout, &mode.graph).map_err(from_ref)?;
    let refreshed = mode
        .cache
        .refresh(lock, &tip, &mode.paths)
        .map_err(from_write)?;
    if !refreshed.drift.is_empty()
        && let Some(folder) = &refreshed.rescued
    {
        let named: BTreeSet<&str> = refreshed
            .session
            .iter()
            .flat_map(|s| s.record.paths.iter().map(String::as_str))
            .collect();
        let edited: Vec<&str> = refreshed
            .drift
            .iter()
            .map(|d| d.path.as_str())
            .filter(|p| !named.contains(p))
            .collect();
        crate::output::notice(
            graph_warning(
                "graph.cache_edit",
                format!(
                    "the graph cache held edits that no write session made ({}); they were \
                     set aside and the cache rebuilt",
                    edited.join(", ")
                ),
            )
            .with_hint(hand_edit_hint(Some(folder))),
        );
    }
    if let Some(ended) = &refreshed.session {
        report_aside(&cache::SetAside {
            unclassified: ended.unclassified.clone(),
        });
    }
    if let Some(ended) = refreshed.session
        && !published.contains(&ended.record.id)
    {
        crate::output::notice(
            graph_warning(
                "graph.session_discarded",
                format!(
                    "`{}` (request {}) didn't finish, so nothing it changed was published",
                    ended.record.op, ended.record.id
                ),
            )
            .with_hint("run the command again"),
        );
    }
    mode.set_commit(&tip);
    Ok(tip)
}

/// Whether the store recorded a changeset whose source is the intent's
/// prepared commit: the reservation a replay publishes.
///
/// A write that holds no lease (an anchor) has no store reservation: its
/// journaled intent is the commit point, so replay publishes it (Terra
/// #10293).
fn recorded(store: &Store, intent: &Intent) -> Result<bool, GraphError> {
    if intent.leases.is_empty() {
        return Ok(true);
    }
    let state = store.coordinator().map_err(crate::coord::store_error)?;
    let commit = &intent.prepared.commit;
    Ok(state
        .changesets()
        .any(|c| c.source.as_ref().is_some_and(|s| &s.commit == commit)))
}

/// Replays this clone's pending graph write intents under `lock` (R13),
/// for graph sync before it moves the local graph (Terra #18237), as a
/// command's catch-up does: the store is opened only when an intent is
/// pending, and what each replay did is reported.
pub(crate) fn replay_intents(
    checkout: &Path,
    writer: &RefWriter,
    lock: &GraphLock,
) -> Result<(), GraphError> {
    let journal = Journal::open(writer.common()).map_err(io_error)?;
    if journal.pending().map_err(io_error)?.is_empty() {
        return Ok(());
    }
    let store = Store::open_for_repo(checkout, crate::coord::ttl_ms())
        .map_err(crate::coord::store_error)?;
    let replayed = journal
        .replay(writer, lock, |intent| recorded(&store, intent))
        .map_err(from_write)?;
    let failed = replayed
        .iter()
        .filter(|r| matches!(r, Replayed::Failed { .. }))
        .count();
    for r in &replayed {
        report_replay(&journal, r);
    }
    // A write that couldn't be replayed now is tried again before the
    // graph moves, never left behind a sync's move (Terra #18984).
    if failed > 0 {
        return Err(graph_error(
            "graph.intents",
            format!(
                "{failed} pending graph write(s) couldn't be replayed; the sync waits for them"
            ),
        ));
    }
    Ok(())
}

fn stale_intent_hint(journal: &Journal, id: &str) -> String {
    format!(
        "the prepared request is kept at {} and cannot be published over the newer change; \
         keep it for recovery and inspect the newer graph change",
        journal.dir().join(format!("{id}.json")).display()
    )
}

fn pending_request(common: &Path, id: &str) -> Result<Option<Journal>, GraphError> {
    let journal = Journal::open(common).map_err(io_error)?;
    if journal.load(id).map_err(io_error)?.is_some() {
        Ok(Some(journal))
    } else {
        Ok(None)
    }
}

fn report_replay(journal: &Journal, r: &Replayed) {
    let op = |id: &str| {
        journal
            .load(id)
            .ok()
            .flatten()
            .map(|i| i.message)
            .unwrap_or_else(|| "a write".into())
    };
    match r {
        Replayed::Published { id, receipt } => crate::output::notice(graph_info(
            "graph.intents",
            format!(
                "recovered: published request {id} ({}) as {}",
                op(id),
                &receipt.commit[..receipt.commit.len().min(12)]
            ),
        )),
        Replayed::Discarded { .. } | Replayed::Unpinned { .. } => {}
        Replayed::Stale { id, path } => crate::output::notice(
            graph_warning(
                "graph.intent_stale",
                format!(
                    "request {id} was recorded but not published, and the graph has changed \
                     {path} since"
                ),
            )
            .with_hint(stale_intent_hint(journal, id)),
        ),
        Replayed::Failed { id, error } => crate::output::notice(graph_warning(
            "graph.intents",
            format!("request {id} is still pending: {error}"),
        )),
    }
}

/// One seam for graph-file git (edd-01a18867, `GraphGit`): the same
/// questions the 0.7.0 write path asked `git::…(&repo.top, …)`, answered
/// from the cache and its manifest commit C in ref mode.
pub(crate) enum GraphGit<'a> {
    Checkout { top: &'a Path },
    Ref { mode: &'a RefMode },
}

impl GraphGit<'_> {
    /// Files inside `path` (matched as scopes match), repository-relative:
    /// in ref mode, every file the cache holds there, the session's new
    /// files included.
    pub(crate) fn files_under(&self, path: &str) -> io::Result<Vec<String>> {
        match self {
            Self::Checkout { top } => raw::files_under(top, path),
            Self::Ref { mode } => {
                let scope = Scope::new(path);
                let mut out = Vec::new();
                for folder in &mode.paths {
                    let f = Scope::new(folder.as_str());
                    if !f.overlaps(&scope) {
                        continue;
                    }
                    walk(mode.root(), folder, &mut |rel| {
                        if scope.contains(&Scope::new(rel)) {
                            out.push(rel.to_owned());
                        }
                    })?;
                }
                out.sort();
                Ok(out)
            }
        }
    }

    /// The content hash of a graph file, or `None` when it's absent.
    pub(crate) fn file_digest(&self, path: &str) -> io::Result<Option<String>> {
        match self {
            Self::Checkout { top } => raw::file_digest(top, path),
            Self::Ref { mode } => raw::file_digest(mode.root(), path),
        }
    }

    /// The file's spelling as the graph holds it, matching in any case.
    pub(crate) fn disk_spelling(&self, path: &str) -> String {
        match self {
            Self::Checkout { top } => raw::disk_spelling(top, path),
            Self::Ref { .. } => self
                .files_under(path)
                .ok()
                .and_then(|f| {
                    f.into_iter()
                        .find(|f| Scope::new(f.as_str()) == Scope::new(path))
                })
                .unwrap_or_else(|| path.to_owned()),
        }
    }

    /// Whether `path` is in C and its cache file is as C has it.
    pub(crate) fn unchanged_since_head(&self, path: &str) -> io::Result<bool> {
        match self {
            Self::Checkout { top } => raw::unchanged_since_head(top, path),
            Self::Ref { mode } => {
                let commit = mode.commit();
                let blob = raw::command(&mode.checkout)
                    .args(["rev-parse", "--verify", "--quiet"])
                    .arg(format!("{commit}:{path}"))
                    .run(None)?;
                if !blob.status.success() {
                    return Ok(false);
                }
                let file = mode.root().join(path);
                if !file.is_file() {
                    return Ok(false);
                }
                let hashed = raw::command(&mode.checkout)
                    .args(["hash-object", "--no-filters", "--"])
                    .arg(&file)
                    .run(None)?;
                Ok(hashed.status.success()
                    && String::from_utf8_lossy(&hashed.stdout).trim()
                        == String::from_utf8_lossy(&blob.stdout).trim())
            }
        }
    }

    /// Every file under `path` at `rev` with its content hash.
    pub(crate) fn leaves(&self, rev: &str, path: &str) -> io::Result<BTreeMap<Scope, String>> {
        raw::leaves(self.repo(), rev, path)
    }

    pub(crate) fn leaves_read_only_cached(
        &self,
        rev: &str,
        path: &str,
        cache: Option<&Path>,
    ) -> io::Result<BTreeMap<Scope, String>> {
        raw::leaves_read_only_cached(self.repo(), rev, path, cache)
    }

    pub(crate) fn same_under(&self, a: &str, b: &str, path: &str) -> io::Result<bool> {
        raw::same_under(self.repo(), a, b, path)
    }

    pub(crate) fn touched_between(&self, base: &str, tip: &str, path: &str) -> io::Result<bool> {
        raw::touched_between(self.repo(), base, tip, path)
    }

    pub(crate) fn commit_of(&self, refname: &str) -> io::Result<String> {
        raw::commit_of(self.repo(), refname)
    }

    /// The graph content's commit: the checkout's HEAD, or C on the graph
    /// ref.
    pub(crate) fn head(&self) -> io::Result<Source> {
        match self {
            Self::Checkout { top } => raw::head(top),
            Self::Ref { mode } => Ok(Source {
                commit: mode.commit(),
                branch: mode.graph.name.clone(),
            }),
        }
    }

    /// The ref settlement observes: the graph ref, or main.
    pub(crate) fn settle_ref(&self) -> String {
        match self {
            Self::Checkout { top } => raw::main_ref(top),
            Self::Ref { mode } => mode.graph.name.clone(),
        }
    }

    fn repo(&self) -> &Path {
        match self {
            Self::Checkout { top } => top,
            Self::Ref { mode } => &mode.checkout,
        }
    }
}

/// Where a hand edit was found (edd-01a18867, "Hand edits are only
/// reported").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandEdit {
    /// `checkout` (the invoking checkout differs from its own HEAD),
    /// `branch` (the branch has commits since the code base that touch a
    /// graph folder) or `rescued` (cache edits set aside).
    pub(crate) source: &'static str,
    /// A repository-relative path, a commit, or a rescued folder.
    pub(crate) at: String,
}

/// The one hint for a graph edit no mdkg write made, wherever it's
/// reported (`status`, `doctor`, a read that sets cache edits aside):
/// `folder` is where set-aside files are, when there's one.
pub(crate) fn hand_edit_hint(folder: Option<&Path>) -> String {
    edit_hint(folder, Rescue::HandEdit)
}

/// [`hand_edit_hint`] for files set aside as unclassified.
fn unclassified_hint(folder: &Path) -> String {
    edit_hint(Some(folder), Rescue::Unclassified)
}

/// What a folder under `<common>/mdkg/graph/rescued/` holds, by its
/// marker: the one classification `status` and `doctor` share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rescue {
    /// Cache edits no write made (no marker): a hand edit.
    HandEdit,
    /// Drift a write session didn't account for, or a marker that can't
    /// be read: listed, never importable.
    Unclassified,
    /// A write's own copies, kept after it stopped before publishing.
    Unfinished,
}

/// Every rescue folder with its kind, sorted.
pub(crate) fn rescued(common: &Path) -> Vec<(Rescue, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(common.join("mdkg/graph/rescued")) else {
        return Vec::new();
    };
    let mut out: Vec<(Rescue, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| {
            let kind = match cache::kept(&p) {
                Ok(None) => Rescue::HandEdit,
                Ok(Some(k)) if k.kind == cache::KeptKind::Unfinished => Rescue::Unfinished,
                _ => Rescue::Unclassified,
            };
            (kind, p)
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// The advice for one kind of rescue folder (or, with `HandEdit`, for a
/// checkout or branch edit too). A node's change is published only by
/// `node save`, so an edit goes only after it succeeds (Terra #15956);
/// each source goes its own way, since deleting a tracked checkout file
/// would be a new edit and a branch commit isn't a file (Terra #16136);
/// an unfinished write is rerun, never saved by hand, so its own steps
/// (`done`'s settlement) run (Terra #16139).
pub(crate) fn edit_hint(folder: Option<&Path>, kind: Rescue) -> String {
    let bin = mdkg_format::program::BIN;
    let at = folder.map_or_else(String::new, |f| {
        format!(
            "the files are at {}; ",
            crate::output::shown_path(&f.display().to_string())
        )
    });
    let remove = "remove a rescue folder once nothing in it is wanted: status and doctor report \
                  it until it's gone";
    match kind {
        Rescue::HandEdit => format!(
            "{at}mdkg never reads them. To keep a node's change, run `{bin} node edit <id>`, \
             make it in that copy, then `{bin} node save <id>`. Only after the save succeeds, \
             clean up where the edit was: a rescued copy is deleted; a changed checkout file \
             is put back in both the index and the worktree with `git restore --staged \
             --worktree -- <path>` (an untracked one deleted); and a branch commit's graph \
             paths, and only those, are taken back to the code base in a new commit (`git \
             checkout <code base> -- <graph paths>`, or `git rm` for paths it added), so the \
             commit's other changes stay; status keeps listing that commit while it's on the \
             branch and not in the local code base: until a merge commit reaches the code base \
             here, and after a squash or rebase merge until the branch is rebased onto it. Keep any other rescued file: a later \
             release's importer publishes it, except in an `-unclassified-` folder, whose \
             files no importer takes (compare them with the graph). Then {remove}. `{bin} status --json` lists \
             every hand edit under graph.hand_edits"
        ),
        Rescue::Unclassified => format!(
            "{at}mdkg never reads them. These are unclassified, so no importer takes them: \
             compare them with the graph, and {remove}. `{bin} status --json` lists them under \
             graph.hand_edits"
        ),
        Rescue::Unfinished => format!(
            "{at}these are the copies of an mdkg write that stopped before it published (its \
             UNFINISHED.json names the op); mdkg never reads them. Run that command again \
             rather than saving its files by hand, so its own steps run; then {remove}"
        ),
    }
}

/// Every hand edit ref mode reports and never reads: `.mdkg/**` changes
/// in the invoking checkout against its own HEAD, the branch's commits
/// since the code base that touch a graph folder, and rescued cache
/// edits. Unfinished ops' kept copies aren't hand edits.
pub(crate) fn hand_edits(mode: &RefMode) -> Vec<HandEdit> {
    let mut out = Vec::new();
    let mut status = raw::command(&mode.checkout);
    status = status.args([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
    ]);
    for p in &mode.paths {
        status = status.arg(format!(":(literal){p}"));
    }
    if let Ok(o) = status.run(None)
        && o.status.success()
    {
        for record in o.stdout.split(|b| *b == 0).filter(|r| r.len() > 3) {
            out.push(HandEdit {
                source: "checkout",
                at: String::from_utf8_lossy(&record[3..]).into_owned(),
            });
        }
    }
    if let Some(base) = &mode.graph.main_ref_base {
        let mut log = raw::command(&mode.checkout)
            .args(["rev-list", "--max-count=20"])
            .arg(format!("{base}..HEAD"))
            .arg("--");
        for p in &mode.paths {
            log = log.arg(format!(":(literal){p}"));
        }
        if let Ok(o) = log.run(None)
            && o.status.success()
        {
            for commit in String::from_utf8_lossy(&o.stdout).lines() {
                out.push(HandEdit {
                    source: "branch",
                    at: commit.to_owned(),
                });
            }
        }
    }
    // An op's own unfinished copies aren't hand edits; files set aside
    // as unclassified, or a folder whose marker can't be read, are listed.
    out.extend(
        rescued(&mode.graph.common)
            .into_iter()
            .filter(|(kind, _)| *kind != Rescue::Unfinished)
            .map(|(_, f)| HandEdit {
                source: "rescued",
                at: f.display().to_string(),
            }),
    );
    out
}

/// Graph paths changed since the code base (`mainRef`'s tree against the
/// graph tip), repository-relative; `None` when no code base resolves.
pub(crate) fn changed_paths(mode: &RefMode) -> io::Result<Option<BTreeSet<String>>> {
    let Some(base) = &mode.graph.main_ref_base else {
        return Ok(None);
    };
    if raw::commit_of(&mode.checkout, base).is_err() {
        return Ok(None);
    }
    let mut diff = raw::command(&mode.checkout)
        .args(["diff", "--name-only", "-z", "--no-renames"])
        .arg(base)
        .arg(mode.commit())
        .arg("--");
    for p in &mode.paths {
        diff = diff.arg(format!(":(literal){p}"));
    }
    let out = diff.run(None)?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git diff: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect(),
    ))
}

/// Regular files under `rel` in `root`, repository-relative.
fn walk(root: &Path, rel: &str, found: &mut dyn FnMut(&str)) -> io::Result<()> {
    let dir = root.join(rel);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(root, &child, found)?;
        } else if kind.is_file() {
            found(&child);
        }
    }
    Ok(())
}

/// `mdkg_coord_sqlite::git`, with the ref-mode guard: a call whose path
/// argument lies in a graph folder is refused (`graph.checkout_read`)
/// while ref mode is bound, since the checkout's copy (absent, or main's
/// stale one) is never the graph. Graph-file questions go through
/// [`GraphGit`] instead. Every other function forwards unchanged.
pub(crate) mod git {
    pub(crate) use mdkg_coord_sqlite::git::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::path::Path;

    use mdkg_coord::Scope;
    use mdkg_coord_sqlite::git as raw;

    /// Refuses `path` (repository-relative) when ref mode is bound and it
    /// names a graph folder or a file in one.
    pub(crate) fn guard(path: &str) -> io::Result<()> {
        let Some(mode) = super::bound() else {
            return Ok(());
        };
        let scope = Scope::new(path.trim_start_matches("./"));
        if !mode
            .paths
            .iter()
            .any(|p| Scope::new(p.as_str()).overlaps(&scope))
        {
            return Ok(());
        }
        let message = format!(
            "graph.checkout_read: {path} is a graph path, and ref mode never reads the \
             checkout's copy (use GraphGit)"
        );
        if cfg!(debug_assertions) {
            panic!("{message}");
        }
        Err(io::Error::other(message))
    }

    pub(crate) fn files_under(worktree: &Path, path: &str) -> io::Result<Vec<String>> {
        guard(path)?;
        raw::files_under(worktree, path)
    }

    pub(crate) fn file_digest(worktree: &Path, path: &str) -> io::Result<Option<String>> {
        guard(path)?;
        raw::file_digest(worktree, path)
    }

    pub(crate) fn disk_spelling(worktree: &Path, path: &str) -> String {
        if guard(path).is_err() {
            return path.to_owned();
        }
        raw::disk_spelling(worktree, path)
    }

    pub(crate) fn unchanged_since_head(worktree: &Path, path: &str) -> io::Result<bool> {
        guard(path)?;
        raw::unchanged_since_head(worktree, path)
    }

    pub(crate) fn same_under(repo: &Path, a: &str, b: &str, path: &str) -> io::Result<bool> {
        guard(path)?;
        raw::same_under(repo, a, b, path)
    }

    pub(crate) fn touched_between(
        repo: &Path,
        base: &str,
        tip: &str,
        path: &str,
    ) -> io::Result<bool> {
        guard(path)?;
        raw::touched_between(repo, base, tip, path)
    }

    pub(crate) fn leaves(
        repo: &Path,
        refname: &str,
        prefix: &str,
    ) -> io::Result<BTreeMap<Scope, String>> {
        guard(prefix)?;
        raw::leaves(repo, refname, prefix)
    }

    pub(crate) fn commit_paths(
        worktree: &Path,
        paths: &[String],
        message: &str,
    ) -> io::Result<bool> {
        for p in paths {
            guard(p)?;
        }
        raw::commit_paths(worktree, paths, message)
    }

    pub(crate) fn checkout_file(worktree: &Path, commit: &str, path: &str) -> io::Result<()> {
        guard(path)?;
        raw::checkout_file(worktree, commit, path)
    }

    pub(crate) fn changed_since(worktree: &Path, base: &str) -> io::Result<BTreeSet<String>> {
        if super::bound().is_some() {
            guard(".mdkg")?;
        }
        raw::changed_since(worktree, base)
    }
}

// A test's injected failure, on this thread only.
#[cfg(test)]
thread_local! {
    static FAIL_AFTER: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

/// The write path's fault steps (`session`, `prepared`, `recorded`,
/// `published`), as [`crate::write::fault`] runs them.
fn fault(step: &str) -> Result<(), GraphError> {
    #[cfg(test)]
    if FAIL_AFTER.with(|f| *f.borrow() == Some(step)) {
        return Err(GraphError::failure(
            Code::IoWriteFailed,
            format!("a write failed after `{step}` (injected)"),
        ));
    }
    crate::write::fault(step)
}

// The command's request id (`--request-id`, else minted), and how many
// publishes it has made: a command that publishes more than once (`done
// --checkpoint` creates, then settles) suffixes the later ones, so a rerun
// with the same id finds each of them (R41).
thread_local! {
    static REQUEST: std::cell::RefCell<(Option<String>, u32)> = const { std::cell::RefCell::new((None, 0)) };
}

/// Sets the command's request id (`--request-id`).
pub(crate) fn set_request_id(id: Option<String>) {
    REQUEST.with(|r| *r.borrow_mut() = (id, 0));
}

/// The request id of this command's next publish: the command's id for
/// its first, then `<id>-2`, `<id>-3`; minted once when none was given.
pub(crate) fn next_request_id() -> String {
    REQUEST.with(|r| {
        let mut r = r.borrow_mut();
        let base =
            r.0.get_or_insert_with(|| mdkg_format::Uuid7::now().to_string())
                .clone();
        r.1 += 1;
        if r.1 == 1 {
            base
        } else {
            format!("{base}-{}", r.1)
        }
    })
}

/// What an op's edits come to, for prepare and record.
/// What [`record_graph_file`] did: the caller's value, the published
/// commit and its changesets (`None` when nothing changed), and the
/// authority-hold release note.
pub(crate) type RecordedFile<T> = (T, Made, Option<Diagnostic>);

/// A recorded write's commit and changesets, `None` when nothing changed.
pub(crate) type Made = Option<(String, Vec<String>)>;

/// One graph file changed under an exact exclusive lease on it, as
/// `agents add` and `config set` change theirs: the lease is taken when the
/// actor holds none (and released again when nothing changed, or the write
/// fails), and `decide` reads the file and returns the bytes to write
/// (`None`: leave it) with its own value. In ref mode (`root` is the bound
/// cache root) the file is read in the cache at the tip under the exclusive
/// graph lock and published on the graph ref; in a checkout-mode git
/// checkout it's written in place and committed on the checkout's branch
/// (a refresh inside `.worktrees/graph` lands on the graph branch, never
/// leaving it dirty), and put back if the commit fails before it's made.
/// Returns that value, the commit and its changesets (`None` when nothing
/// changed), and the authority-hold release note.
pub(crate) fn record_graph_file<T>(
    root: &Path,
    actor: Option<String>,
    rel: &str,
    message: &str,
    op: &str,
    decide: impl FnOnce(&Path) -> Result<(Option<Vec<u8>>, T), GraphError>,
) -> Result<RecordedFile<T>, GraphError> {
    use crate::coord::{self, Repo, now_ms, store_error};
    use mdkg_coord::{Command, Mode, Outcome};

    crate::graph_repair::require_writable(root)?;
    let repo = Repo::open(root)?;
    let mode = for_root(&repo.root);
    crate::anchor::refuse_linked_folders(&repo.root, rel)?;
    let scope = repo.leaf(rel)?;
    coord::check_scope_text(&scope)?;
    let actor = repo.actor(actor);
    crate::msg::check_actor_mailbox(&actor, true)?;
    let held = coord::require_authority_hold(&repo, &actor, &scope, None)?;
    let mut store = repo.store()?;
    let now = now_ms();
    repo.observe(&mut store, now)?;
    let state = store.coordinator().map_err(store_error)?;
    let existing = coord::covering_lease(&state, &actor, &scope);
    let taken = existing.is_none();
    let lease = if let Some(lease) = existing {
        if lease.scope != scope || lease.mode != Mode::Exclusive {
            return Err(GraphError::conflict(
                Code::CoordLeaseHeld,
                format!("changing {rel} needs an exclusive lease on that file"),
            )
            .with_hint("release the covering or shared lease, then retry"));
        }
        lease.id.clone()
    } else {
        let mut capped = false;
        let outcome = store.execute_with(|state| {
            if coord::at_lease_cap(state, &actor, now) {
                capped = true;
                return Err(mdkg_coord_sqlite::StoreError::Schema("capped".into()));
            }
            Ok(Command::Claim {
                owner: actor.clone(),
                scope: scope.clone(),
                mode: Mode::Exclusive,
                now,
                request: None,
                node: None,
                branch: None,
            })
        });
        if capped {
            return Err(coord::lease_cap_error());
        }
        match outcome.map_err(store_error)? {
            Outcome::Lease { lease } => lease,
            Outcome::Refused { error } => return Err(coord::refusal(&error)),
            _ => {
                return Err(GraphError::failure(
                    Code::IoWriteFailed,
                    format!("the lease on {rel} wasn't granted"),
                ));
            }
        }
    };
    let release = |store: &mut Store| -> Result<(), GraphError> {
        let outcome = store
            .execute(&Command::Release {
                owner: actor.clone(),
                lease: lease.clone(),
                now,
            })
            .map_err(store_error)?;
        if let Outcome::Refused { error } = outcome {
            return Err(coord::refusal(&error));
        }
        Ok(())
    };
    let Some(mode) = mode else {
        let out = record_checkout_file(
            &repo, &mut store, &actor, rel, &scope, &lease, message, now, decide,
        );
        let out = match out {
            Ok((value, None)) => {
                if taken {
                    release(&mut store)?;
                }
                (value, None)
            }
            Ok((value, Some(made))) => (value, Some(made)),
            // A made commit keeps the lease (its error says so).
            Err((e, true)) => return Err(e),
            Err((e, false)) => {
                if taken {
                    let _ = release(&mut store);
                }
                return Err(e);
            }
        };
        // As `config set`: the authority's lease is released only when the
        // change was committed; a no-op leaves the actor holding it.
        let note = out
            .1
            .as_ref()
            .and_then(|_| coord::release_at_authority(&repo, &actor, &scope, held.as_ref()));
        return Ok((out.0, out.1, note));
    };
    let graph = mode.graph.name.clone();
    let mut changesets = Vec::new();
    let wrote = prepare_record_publish(
        &mode,
        &mut store,
        (&next_request_id(), op, &actor),
        |w, _| {
            // The cache is at the tip, under the exclusive lock; nothing is
            // written there but through `w`.
            let (bytes, value) = decide(&repo.root.join(rel))?;
            if let Some(bytes) = &bytes {
                w.write(scope.as_str(), bytes)?;
            }
            Ok((
                Publish {
                    message: message.to_owned(),
                    leases: vec![lease.to_string()],
                    allowed: vec![scope.clone()],
                    trailers: Vec::new(),
                },
                value,
            ))
        },
        |store, prepared| {
            let source = Source {
                commit: prepared.commit.clone(),
                branch: graph.clone(),
            };
            changesets = crate::write::record_changesets(
                &repo,
                store,
                &actor,
                &[(lease.to_string(), scope.clone())],
                &[],
                &source,
                now,
                None,
                None,
            )?;
            Ok(())
        },
    );
    let out = match wrote {
        Ok(Wrote::Done(value, Some(written))) => {
            (value, Some((written.receipt.commit, changesets)))
        }
        Ok(Wrote::Done(value, None)) => {
            // Nothing changed: no changeset holds the lease, and only one
            // this call took is released.
            if taken {
                release(&mut store)?;
            }
            (value, None)
        }
        // Request ids are minted per call, so a replay of this one can't
        // be found here.
        Ok(Wrote::Already(_)) => {
            if taken {
                let _ = release(&mut store);
            }
            return Err(GraphError::failure(
                Code::IoWriteFailed,
                "this change's request was already published",
            ));
        }
        Err(e) => {
            if taken {
                let _ = release(&mut store);
            }
            return Err(e);
        }
    };
    let note = out
        .1
        .as_ref()
        .and_then(|_| coord::release_at_authority(&repo, &actor, &scope, held.as_ref()));
    Ok((out.0, out.1, note))
}

/// [`record_graph_file`] in checkout mode: `rel` written in place under the
/// lease, then committed and recorded; the earlier bytes are put back when
/// the commit fails before it's made (a made commit is never undone).
/// Whether to commit is judged against `HEAD`'s copy, not the working
/// copy, so a retry after a stop between the write and the commit still
/// commits (Terra #19620). A staged change of the path is refused, never
/// replaced. On `Err`, the flag says a commit was made (the caller then
/// keeps the lease, and the error names the commit).
#[allow(clippy::too_many_arguments)]
fn record_checkout_file<T>(
    repo: &crate::coord::Repo,
    store: &mut Store,
    actor: &str,
    rel: &str,
    scope: &Scope,
    lease: &mdkg_coord::LeaseId,
    message: &str,
    now: u64,
    decide: impl FnOnce(&Path) -> Result<(Option<Vec<u8>>, T), GraphError>,
) -> Result<(T, Made), (GraphError, bool)> {
    let before_commit = |e: GraphError| (e, false);
    let path = repo.root.join(rel);
    let failed = |e: std::io::Error| {
        GraphError::failure(Code::IoWriteFailed, format!("can't write {rel}: {e}"))
    };
    // Read only under the lease, never through a link.
    let before = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Some(
            std::fs::read(&path)
                .map_err(failed)
                .map_err(before_commit)?,
        ),
        Ok(_) => {
            return Err(before_commit(GraphError::usage(
                Code::IoWriteFailed,
                format!("{rel} isn't a regular file"),
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(before_commit(failed(e))),
    };
    let git = |args: &[&str]| {
        raw::command(&repo.top)
            .args(["--literal-pathspecs"])
            .args(args)
            .run(None)
            .map_err(io_error)
    };
    // A staged version of its own would be replaced by the commit's
    // `git add`: refused, so it's never lost.
    let staged = git(&["diff", "--cached", "--quiet", "HEAD", "--", scope.as_str()])
        .map_err(before_commit)?;
    if staged.status.code() != Some(0) {
        return Err(before_commit(
            GraphError::conflict(
                Code::IoWriteFailed,
                format!("{rel} has staged changes, which this commit would replace"),
            )
            .with_hint(format!(
                "commit them, or unstage them (`git restore --staged -- {rel}`), then run it again"
            )),
        ));
    }
    let head = git(&["show", &format!("HEAD:{}", scope.as_str())]).map_err(before_commit)?;
    let head = head.status.success().then_some(head.stdout);
    let (bytes, value) = decide(&path).map_err(before_commit)?;
    let Some(bytes) = bytes else {
        return Ok((value, None));
    };
    if before.as_deref() != Some(bytes.as_slice()) {
        staged_write(&path, &bytes)
            .map_err(failed)
            .map_err(before_commit)?;
    }
    // History already holds these bytes: nothing to commit, and no old
    // commit is reported as this one's.
    if head.as_deref() == Some(bytes.as_slice()) {
        return Ok((value, None));
    }
    let recorded = crate::write::commit_and_record(
        repo,
        store,
        actor,
        &[(lease.to_string(), scope.clone())],
        &[],
        message,
        now,
        None,
        None,
    );
    match recorded {
        Ok(r) => Ok((value, Some((r.commit, r.changesets)))),
        Err((error, true)) => {
            let commit = raw::head(&repo.top).map(|h| h.commit).unwrap_or_default();
            Err((
                error.with_hint(format!(
                    "{rel} was committed ({commit}) but the coordinator didn't record it; \
                     your lease on it is kept so the commit stays protected: release it once \
                     the branch lands (`{} release {rel}`)",
                    mdkg_format::program::BIN
                )),
                true,
            ))
        }
        Err((error, false)) => {
            let restored = match &before {
                Some(old) => staged_write(&path, old),
                None => std::fs::remove_file(&path),
            };
            if let Err(restore) = restored {
                return Err(before_commit(GraphError::failure(
                    Code::IoWriteFailed,
                    format!(
                        "{}; putting {rel} back failed: {restore}",
                        error.diagnostic.message
                    ),
                )));
            }
            Err(before_commit(error))
        }
    }
}

/// Writes `bytes` to `path` through a temporary file staged a folder up
/// (beside the file's own folder), so a stop never leaves a stray file in
/// that folder, where a skill's mirror would copy it (Terra #19620).
fn staged_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(parent)?;
    let stage = parent.parent().unwrap_or(parent);
    let name = format!(
        ".{}.{}.mdkg-tmp",
        parent.file_name().unwrap_or_default().to_string_lossy(),
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let tmp = stage.join(name);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    result
}

pub(crate) struct Publish {
    /// The commit subject after `mdkg: `, such as `update task-1`.
    pub(crate) message: String,
    /// The request's leases, as the store names them.
    pub(crate) leases: Vec<String>,
    /// Scopes the op may change: its leased scopes, and files it created.
    pub(crate) allowed: Vec<Scope>,
    /// Extra trailers (`done`'s `Mdkg-Settle`).
    pub(crate) trailers: Vec<(String, String)>,
}

/// Reports files a session set aside because no write of its own made
/// them (someone else's edit, or a stray of the op's).
/// Where set-aside cache files are, for a hint: the folder escaped, so a
/// name holding a newline can't print a line of its own (orchestrator
/// #15911).
fn report_aside(aside: &cache::SetAside) {
    for folder in &aside.unclassified {
        crate::output::notice(
            graph_warning(
                "graph.cache_edit",
                "graph cache files no write session made were set aside, and restored",
            )
            .with_hint(unclassified_hint(folder)),
        );
    }
}

/// An open write session, as the op's edit step sees it.
pub(crate) struct Writing<'s, 'c> {
    session: &'s mut cache::Session<'c>,
}

impl Writing<'_, '_> {
    /// Declares a repository-relative graph path before the op writes or
    /// deletes it, so a crash keeps it as an unfinished op, never a hand
    /// edit.
    pub(crate) fn declare(&mut self, rel: &str) -> Result<(), GraphError> {
        self.session.will_write(rel).map_err(from_write)
    }

    /// Writes a repository-relative graph path in the session: recorded,
    /// written through a recorded temporary file, and checked at finish.
    pub(crate) fn write(&mut self, rel: &str, bytes: &[u8]) -> Result<(), GraphError> {
        self.session.write(rel, bytes).map_err(from_write)
    }

    /// Deletes a repository-relative graph path in the session.
    pub(crate) fn delete(&mut self, rel: &str) -> Result<(), GraphError> {
        self.session.delete(rel).map_err(from_write)
    }

    /// The commit the session started from (C).
    pub(crate) fn base(&self) -> &str {
        self.session.base()
    }
}

/// What a write came to.
#[derive(Debug)]
pub(crate) enum Wrote<T> {
    /// The op ran: its value, and what it published (`None` when it
    /// changed nothing).
    Done(T, Option<Written>),
    /// An earlier run with this request id already published (R41): its
    /// receipt, and nothing more is applied.
    Already(Written),
}

/// The commit an earlier run of request `id` published on the graph ref:
/// the subject's `[<id> <fingerprint>]` marker.
pub(crate) fn published_request(mode: &RefMode, id: &str) -> Result<Option<String>, GraphError> {
    let out = raw::command(&mode.checkout)
        .args(["log", "--fixed-strings", "--format=%H%x00%s"])
        .arg(format!("--grep=[{id} "))
        .arg(&mode.graph.name)
        .arg("--")
        .run(None)
        .map_err(io_error)?;
    if !out.status.success() {
        return Err(graph_error(
            "graph.git",
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    let marker = format!(" [{id} ");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let (commit, subject) = line.split_once('\0')?;
            let (_, rest) = subject.rsplit_once(&marker)?;
            let fingerprint = rest.strip_suffix(']')?;
            (subject.starts_with("mdkg: ")
                && !fingerprint.is_empty()
                && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| commit.to_owned())
        }))
}

/// The outcome of a published write.
#[derive(Debug, Clone)]
pub(crate) struct Written {
    /// The prepared commit: the store's changesets name it.
    pub(crate) prepared: String,
    pub(crate) receipt: Receipt,
}

/// Ref mode's write path (edd-01a18867, steps 1 to 8):
/// 1. the exclusive lock, and replay;
/// 2. refresh to the tip, and observe the graph ref in the store;
/// 3. a write session, in which `edit` makes the op's changes in the cache
///    (declaring each path first) and says what they come to;
/// 4. every touched path must be allowed, or the op is refused
///    (`graph.unleased_edit`) and the stray files rescued;
/// 5. prepare the commit on C, pin it, and journal the intent;
/// 6. `record` the request's changesets in one store transaction, with
///    source `{commit: P, branch: graph.ref}`;
/// 7. publish;
/// 8. finish: the manifest at P, the session closed, the intent gone.
///
/// A failure before 6 commits restores the session's files and leaves
/// nothing behind; after it, the next replay finishes the publish. `None`
/// when the op changed nothing.
pub(crate) fn prepare_record_publish<T>(
    mode: &RefMode,
    store: &mut Store,
    request: (&str, &str, &str),
    edit: impl FnOnce(&mut Writing<'_, '_>, &mut Store) -> Result<(Publish, T), GraphError>,
    record: impl FnOnce(&mut Store, &Prepared) -> Result<(), GraphError>,
) -> Result<Wrote<T>, GraphError> {
    let (id, op, actor) = request;
    let writer = mode.writer()?;
    if let Ok(mut s) = mode.shared.lock() {
        s.take();
    }
    let lock = writer.lock(LockKind::Exclusive).map_err(from_write)?;
    catch_up(mode, &writer, &lock, Some(store), false)?;
    // As `Repo::observe` does: the graph root's `.mdkg` (prefix "").
    store
        .observe_main(&mode.checkout, &mode.graph.name, ".mdkg")
        .map_err(crate::coord::store_error)?;
    // A rerun with the same request id (R41): replay above finished a
    // recorded one; one in the graph's history returns its receipt, and a
    // stale one stays refused until recovered.
    if let Some(commit) = published_request(mode, id)? {
        let head = graphref::tip(&mode.checkout, &mode.graph).map_err(from_ref)?;
        return Ok(Wrote::Already(Written {
            prepared: commit.clone(),
            receipt: Receipt {
                commit,
                head,
                already_applied: true,
                rebuilt: false,
                superseded: false,
                retries: 0,
            },
        }));
    }
    if let Some(journal) = pending_request(&mode.graph.common, id)? {
        return Err(graph_error(
            "graph.intent_stale",
            format!("request {id} is recorded but couldn't be published: the graph changed since"),
        )
        .with_hint(stale_intent_hint(&journal, id)));
    }
    let mut session = mode
        .cache
        .session(&lock, id, op, actor)
        .map_err(from_write)?;
    let refuse = |session: cache::Session<'_>, e: GraphError| -> GraphError {
        match session.abort() {
            Ok(aside) => {
                report_aside(&aside);
                e
            }
            Err(undo) => e.with_hint(format!(
                "and the session's files couldn't be restored ({undo}); run `{} renew` to recover them",
                mdkg_format::program::BIN
            )),
        }
    };
    let (publish, value) = match edit(
        &mut Writing {
            session: &mut session,
        },
        store,
    )
    .and_then(|r| fault("session").map(|()| r))
    {
        Ok(r) => r,
        Err(e) => return Err(refuse(session, e)),
    };
    let touched = match session.touched() {
        Ok(t) => t,
        Err(e) => return Err(refuse(session, from_write(e))),
    };
    if touched.is_empty() {
        report_aside(&session.abort().map_err(from_write)?);
        return Ok(Wrote::Done(value, None));
    }
    let stray: Vec<Drift> = touched
        .iter()
        .filter(|d| {
            !publish
                .allowed
                .iter()
                .any(|s| s.contains(&Scope::new(d.path.as_str())))
        })
        .cloned()
        .collect();
    if !stray.is_empty() {
        let rescued = mode.cache.rescue(&stray).ok().flatten();
        let names: Vec<&str> = stray.iter().map(|d| d.path.as_str()).collect();
        let mut e = graph_error(
            "graph.unleased_edit",
            format!(
                "`{op}` changed graph files outside its leases ({}), so nothing was recorded or \
                 published",
                names.join(", ")
            ),
        );
        if let Some(folder) = rescued {
            e = e.with_hint(format!("the files were set aside in {}", folder.display()));
        }
        return Err(refuse(session, e));
    }
    if touched.iter().any(|d| d.kind == DriftKind::NotAFile) {
        return Err(refuse(
            session,
            graph_error("graph.write_invalid", "a graph path isn't a regular file"),
        ));
    }
    let changes = match session.changes() {
        Ok(c) => c,
        Err(e) => return Err(refuse(session, from_write(e))),
    };
    let base = match writer.snapshot_at(session.base()) {
        Ok(b) => b,
        Err(e) => return Err(refuse(session, from_write(e))),
    };
    let clone = match store.store_id() {
        Ok(c) => c,
        Err(e) => return Err(refuse(session, crate::coord::store_error(e))),
    };
    let journal = match Journal::open(&mode.graph.common) {
        Ok(j) => j,
        Err(e) => return Err(refuse(session, io_error(e))),
    };
    let mut trailers = vec![
        ("Mdkg-Actor".to_owned(), actor.to_owned()),
        ("Mdkg-Clone".to_owned(), clone),
    ];
    trailers.extend(publish.trailers.iter().cloned());
    let req = refwrite::Request {
        id: id.to_owned(),
        base: Some(base),
        changes,
        message: publish.message.clone(),
        trailers,
    };
    let prepared = match writer.prepare(&lock, &req) {
        Ok(p) => p,
        Err(e) => return Err(refuse(session, from_write(e))),
    };
    let intent = Intent {
        prepared: prepared.clone(),
        message: publish.message.clone(),
        actor: actor.to_owned(),
        leases: publish.leases.clone(),
        op: op.to_owned(),
    };
    let unprepare = |e: GraphError| -> GraphError {
        journal.finish(&writer, &lock, &intent).ok();
        e
    };
    if let Err(e) = journal
        .record(&intent)
        .map_err(io_error)
        .and_then(|()| fault("prepared"))
    {
        return Err(refuse(session, unprepare(e)));
    }
    if let Err(e) = record(store, &prepared) {
        return Err(refuse(session, unprepare(e)));
    }
    // Recorded: from here the store holds the reservation, so a failure
    // leaves the intent for a later replay to publish; status only reports it.
    let pending = |e: GraphError| {
        e.with_hint(format!(
            "the change is recorded; run `{} renew` to publish it (or report it stale); status leaves the intent pending",
            mdkg_format::program::BIN
        ))
    };
    fault("recorded").map_err(pending)?;
    let receipt = writer
        .publish(&lock, &prepared)
        .map_err(from_write)
        .map_err(pending)?;
    fault("published").map_err(pending)?;
    journal
        .finish(&writer, &lock, &intent)
        .map_err(from_write)?;
    report_aside(&session.finish(&prepared.commit).map_err(from_write)?);
    mode.set_commit(&prepared.commit);
    Ok(Wrote::Done(
        value,
        Some(Written {
            prepared: prepared.commit,
            receipt,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_coord::{Command, Mode, Outcome};
    use std::process::Command as Process;

    const GRAPH: &str = "refs/heads/mdkg-graph";
    const TASK: &str = "task-01a18900-0000-7000-8000-000000000001";

    /// A set-aside folder's newline stays escaped in the hint (#15911).
    #[test]
    fn a_set_aside_folder_prints_escaped() {
        let hint = unclassified_hint(Path::new("/repo/.git/mdkg/aside/a\nrun this forged step"));
        assert!(
            !hint
                .lines()
                .any(|line| line.trim_start().starts_with("run this forged step")),
            "{hint}"
        );
        assert!(hint.contains("a\\u{a}run this forged step"), "{hint}");
    }

    #[test]
    fn pending_request_refuses_an_unopenable_journal() {
        let clone = Clone::new();
        let not_a_directory = clone.dir.join("not-a-directory");
        std::fs::write(&not_a_directory, b"file").expect("file");
        let error = pending_request(&not_a_directory, "request-1")
            .expect_err("a journal that cannot be opened must refuse the request");
        assert!(has_code(&error, "graph.io"), "{error:?}");
    }

    /// The graph moving under a write is a conflict (exit 5): stale and
    /// contended alike, whichever way the error is made; other graph
    /// errors stay failures (orchestrator on #17713).
    #[test]
    fn stale_and_contended_writes_exit_5() {
        let stale = refwrite::Error::Stale {
            path: ".mdkg/work/x.md".into(),
        };
        for e in [
            from_write(stale),
            from_write(refwrite::Error::RetryLimit),
            graph_error("graph.write_stale", "moved"),
            graph_error("graph.write_contended", "kept moving"),
        ] {
            assert_eq!(e.exit_code(), 5, "{e:?}");
        }
        assert_eq!(from_write(refwrite::Error::NoChange).exit_code(), 4);
        assert_eq!(graph_error("graph.io", "disk").exit_code(), 4);
    }

    #[test]
    fn stale_intent_hint_names_the_kept_file_and_newer_change() {
        let clone = Clone::new();
        let journal = Journal::open(&clone.dir.join(".git")).expect("journal");
        let hint = stale_intent_hint(&journal, "request-1");
        assert!(hint.contains(&journal.dir().join("request-1.json").display().to_string()));
        assert!(hint.contains("cannot be published over the newer change"));
        assert!(hint.contains("keep it for recovery and inspect the newer graph change"));
    }

    /// A clone in ref mode: a code commit on `trunk`, and a graph ref
    /// holding a 0.7 config and one task, with no `.mdkg` in the checkout.
    struct Clone {
        dir: PathBuf,
        graph_id: String,
    }

    impl Drop for Clone {
        fn drop(&mut self) {
            unbind();
            Process::new("chmod")
                .args(["-R", "u+w"])
                .arg(&self.dir)
                .status()
                .ok();
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    impl Clone {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("mdkg-graphgit-{}", mdkg_format::Uuid7::now()));
            std::fs::create_dir_all(&dir).expect("fixture");
            let dir = dir.canonicalize().expect("canonical");
            let graph_id = uuid::Uuid::now_v7().hyphenated().to_string();
            let clone = Self { dir, graph_id };
            clone.git(&["init", "-q", "-b", "trunk"]);
            clone.git(&["config", "user.name", "Graph Test"]);
            clone.git(&["config", "user.email", "graph@example.invalid"]);
            std::fs::write(clone.dir.join("code.rs"), "fn main() {}\n").expect("code");
            clone.git(&["add", "code.rs"]);
            clone.git(&["commit", "-q", "-m", "code"]);
            let config = serde_json::json!({
                "schema_version": mdkg_graph::config::V07_SCHEMA_VERSION,
                "format_version": mdkg_graph::config::FORMAT_VERSION_07,
                "graph_id": clone.graph_id,
                "work": {"location": "git"},
            });
            // As init creates a graph: the graph ref configured, a
            // creator for the new id, then the identity records.
            clone.git(&["config", "mdkg.graph.ref", GRAPH]);
            let graph = graphref::resolve(&clone.dir)
                .expect("resolve")
                .expect("ref mode");
            let writer = graph
                .creator(&clone.dir, LockMode::Local, &clone.graph_id, "")
                .expect("creator");
            writer
                .write(&refwrite::Request {
                    id: "seed".into(),
                    base: None,
                    changes: vec![
                        refwrite::Change::Put {
                            path: ".mdkg/config.json".into(),
                            bytes: serde_json::to_vec_pretty(&config).expect("json"),
                        },
                        refwrite::Change::Put {
                            path: format!(".mdkg/work/{TASK}.md"),
                            bytes: node("todo").into_bytes(),
                        },
                    ],
                    message: "seed".into(),
                    trailers: vec![],
                })
                .expect("seed");
            graph
                .complete_creation(&clone.dir)
                .expect("identity records");
            clone
        }

        fn git(&self, args: &[&str]) -> String {
            let mut cmd = Process::new("git");
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(key);
                }
            }
            let out = cmd
                .current_dir(&self.dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        fn tip(&self) -> String {
            self.git(&["rev-parse", GRAPH])
        }

        fn store(&self) -> Store {
            Store::open_for_repo(&self.dir, 60 * 60 * 1000).expect("store")
        }
    }

    fn node(status: &str) -> String {
        format!(
            "---\nid: {TASK}\ntype: task\ntitle: A\nstatus: {status}\ncreated: \
             2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
        )
    }

    fn leaf() -> String {
        format!(".mdkg/work/{TASK}.md")
    }

    /// Claims the task's leaf, as `record` needs a lease to commit under.
    fn claim(store: &mut Store) -> String {
        // As every command does: observe the graph ref first, so the
        // lease's base is the graph's content.
        let mode = bound().expect("bound");
        store
            .observe_main(&mode.checkout, &mode.graph.name, ".mdkg")
            .expect("observe");
        match store
            .execute(&Command::Claim {
                owner: "tester".into(),
                scope: Scope::new(leaf()),
                mode: Mode::Exclusive,
                now: 1,
                request: None,
                node: None,
                branch: None,
            })
            .expect("claim")
        {
            Outcome::Lease { lease } => lease.to_string(),
            other => panic!("claim: {other:?}"),
        }
    }

    /// The op's edit: the task's status, written in the cache.
    fn set_status<'a>(
        status: &'a str,
    ) -> impl FnOnce(&mut Writing<'_, '_>, &mut Store) -> Result<(Publish, ()), GraphError> + 'a
    {
        move |w, _| {
            w.write(&leaf(), node(status).as_bytes())?;
            Ok((
                Publish {
                    message: format!("update {TASK}"),
                    leases: vec![],
                    allowed: vec![Scope::new(leaf())],
                    trailers: vec![],
                },
                (),
            ))
        }
    }

    /// Records the request's changeset under `lease`, sourced at P.
    fn commit_under(
        lease: String,
        status: &'static str,
    ) -> impl FnOnce(&mut Store, &Prepared) -> Result<(), GraphError> {
        move |store, prepared| {
            let mut changes = mdkg_coord::Changes::new();
            changes.insert(
                Scope::new(leaf()),
                Some(mdkg_format::Digest::of_bytes(node(status).as_bytes()).to_string()),
            );
            let outcome = store
                .execute(&Command::Commit {
                    owner: "tester".into(),
                    lease: mdkg_coord::LeaseId(lease),
                    changes,
                    source: Some(Source {
                        commit: prepared.commit.clone(),
                        branch: GRAPH.into(),
                    }),
                    now: 2,
                    landed: false,
                })
                .map_err(crate::coord::store_error)?;
            match outcome {
                Outcome::Committed { .. } => Ok(()),
                other => Err(GraphError::failure(
                    Code::CoordStoreFailed,
                    format!("{other:?}"),
                )),
            }
        }
    }

    #[test]
    fn a_folder_outside_git_binds_nothing() {
        let dir = std::env::temp_dir().join(format!("mdkg-nogit-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(bind(&dir).expect("bind"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_mode_binds_nothing() {
        let clone = Clone::new();
        clone.git(&["config", "--unset", "mdkg.graph.ref"]);
        assert_eq!(bind(&clone.dir).expect("bind"), None);
        assert!(bound().is_none());
    }

    #[test]
    fn a_bound_repo_reads_the_graph_tip_from_the_cache() {
        let clone = Clone::new();
        assert!(
            !clone.dir.join(".mdkg").exists(),
            "the checkout holds no graph"
        );
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        assert!(root.join(leaf()).is_file());
        let repo = crate::coord::Repo::open(&root).expect("repo");
        assert_eq!(repo.top, clone.dir);
        assert_eq!(repo.root, root);
        let seam = repo.graph_git();
        assert_eq!(seam.files_under(".mdkg/work").expect("files"), vec![leaf()]);
        assert_eq!(seam.disk_spelling(&leaf().to_uppercase()), leaf());
        assert!(seam.unchanged_since_head(&leaf()).expect("unchanged"));
        assert_eq!(
            seam.file_digest(&leaf()).expect("digest"),
            Some(mdkg_format::Digest::of_bytes(node("todo").as_bytes()).to_string())
        );
        let head = seam.head().expect("head");
        assert_eq!(head.commit, clone.tip());
        assert_eq!(head.branch, GRAPH);
        assert_eq!(seam.settle_ref(), GRAPH);
        // Code paths stay in the checkout.
        assert_eq!(repo.disk_base("code.rs"), clone.dir.as_path());
        assert_eq!(repo.disk_base(&leaf()), root.as_path());
        // Observation reads the graph ref.
        let mut store = clone.store();
        repo.observe(&mut store, 1).expect("observe");
        let state = store.coordinator().expect("state");
        assert!(state.leaves().contains_key(&Scope::new(leaf())));
    }

    #[test]
    fn a_graph_id_the_clone_does_not_record_is_refused() {
        let clone = Clone::new();
        clone.git(&[
            "config",
            "--unset",
            &format!("mdkg.graph.{}.ref", clone.graph_id),
        ]);
        let other = uuid::Uuid::now_v7().hyphenated().to_string();
        clone.git(&["config", &format!("mdkg.graph.{other}.ref"), GRAPH]);
        let e = bind(&clone.dir).expect_err("refused");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.identity_mismatch"),
            "{e:?}"
        );
        assert!(bound().is_none());
    }

    #[test]
    #[should_panic(expected = "graph.checkout_read")]
    fn a_raw_git_call_on_a_graph_path_is_refused_in_ref_mode() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        // A code path is fine.
        git::files_under(&clone.dir, "code.rs").expect("code path");
        let _ = git::files_under(&clone.dir, ".mdkg/work");
    }

    #[test]
    fn a_raw_git_call_is_unguarded_in_checkout_mode() {
        let clone = Clone::new();
        assert!(
            git::files_under(&clone.dir, ".mdkg/work")
                .expect("files")
                .is_empty()
        );
    }

    #[test]
    fn a_write_prepares_records_then_publishes() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let lease = claim(&mut store);
        let before = clone.tip();
        let Wrote::Done((), written) = prepare_record_publish(
            &mode,
            &mut store,
            ("req-1", "update", "tester"),
            set_status("progress"),
            commit_under(lease, "progress"),
        )
        .expect("write") else {
            panic!("a first run is done, not already applied");
        };
        let written = written.expect("published");
        let tip = clone.tip();
        assert_ne!(tip, before);
        assert_eq!(written.receipt.commit, tip);
        assert_eq!(clone.git(&["rev-parse", &format!("{tip}^")]), before);
        let body = clone.git(&["show", &format!("{tip}:{}", leaf())]);
        assert!(body.contains("status: progress"));
        let message = clone.git(&["log", "-1", "--format=%B", GRAPH]);
        assert!(
            message.starts_with(&format!("mdkg: update {TASK}")),
            "{message}"
        );
        assert!(message.contains("Mdkg-Actor: tester"), "{message}");
        assert!(message.contains("Mdkg-Clone: "), "{message}");
        // The store's changeset names P on the graph ref; nothing pending
        // is left behind.
        let state = store.coordinator().expect("state");
        assert!(state.changesets().any(|c| {
            c.source
                .as_ref()
                .is_some_and(|s| s.commit == written.prepared && s.branch == GRAPH)
        }));
        assert!(
            Journal::open(&mode.graph.common)
                .expect("journal")
                .pending()
                .expect("pending")
                .is_empty()
        );
        assert!(
            clone
                .git(&["for-each-ref", "refs/mdkg/pending/"])
                .is_empty()
        );
        assert!(mode.cache.session_record().expect("record").is_none());
        assert_eq!(mode.commit(), written.prepared);
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("status: progress")
        );
        // The checkout is untouched.
        assert!(!clone.dir.join(".mdkg").exists());
        assert!(clone.git(&["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn a_rerun_with_the_same_request_id_returns_the_original_receipt() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let lease = claim(&mut store);
        let Wrote::Done((), first) = prepare_record_publish(
            &mode,
            &mut store,
            ("req-r41", "update", "tester"),
            set_status("progress"),
            commit_under(lease.clone(), "progress"),
        )
        .expect("write") else {
            panic!("first run");
        };
        let first = first.expect("published");
        let tip = clone.tip();
        let changesets = store.coordinator().expect("state").changesets().count();
        // The same request again: nothing is edited, recorded or published.
        let again = prepare_record_publish(
            &mode,
            &mut store,
            ("req-r41", "update", "tester"),
            |_, _| -> Result<(Publish, ()), GraphError> { panic!("the op doesn't run again") },
            |_, _| panic!("nothing is recorded again"),
        )
        .expect("rerun");
        let Wrote::Already(receipt) = again else {
            panic!("already applied");
        };
        assert_eq!(receipt.receipt.commit, first.receipt.commit);
        assert!(receipt.receipt.already_applied);
        assert_eq!(clone.tip(), tip);
        assert_eq!(
            store.coordinator().expect("state").changesets().count(),
            changesets
        );
    }

    #[test]
    fn a_rerun_after_a_failed_publish_finds_the_replayed_request() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let lease = claim(&mut store);
        {
            let _fault = FaultAfter::set("recorded");
            prepare_record_publish(
                &mode,
                &mut store,
                ("req-r41b", "new", "tester"),
                set_status("blocked"),
                commit_under(lease, "blocked"),
            )
            .expect_err("injected");
        }
        // The rerun's replay publishes the recorded request, and the rerun
        // itself applies nothing more (C28).
        let again = prepare_record_publish(
            &mode,
            &mut store,
            ("req-r41b", "new", "tester"),
            |_, _| -> Result<(Publish, ()), GraphError> { panic!("no second node") },
            |_, _| panic!("nothing is recorded again"),
        )
        .expect("rerun");
        assert!(matches!(again, Wrote::Already(_)));
        assert!(
            clone
                .git(&["show", &format!("{GRAPH}:{}", leaf())])
                .contains("status: blocked")
        );
        assert_eq!(clone.git(&["rev-list", "--count", GRAPH]), "2");
    }

    #[test]
    fn a_refused_op_restores_its_files_and_publishes_nothing() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let before = clone.tip();
        let e = prepare_record_publish(
            &mode,
            &mut store,
            ("req-2", "done", "tester"),
            set_status("done"),
            |_, _| {
                Err(GraphError::failure(
                    Code::CoordStoreFailed,
                    "the gate failed",
                ))
            },
        )
        .expect_err("refused");
        assert!(e.diagnostic.message.contains("the gate failed"));
        assert_eq!(clone.tip(), before);
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("status: todo")
        );
        assert!(mode.cache.session_record().expect("record").is_none());
        assert!(mode.cache.verify().expect("verify"));
        assert!(
            Journal::open(&mode.graph.common)
                .expect("journal")
                .pending()
                .expect("pending")
                .is_empty()
        );
        assert!(
            clone
                .git(&["for-each-ref", "refs/mdkg/pending/"])
                .is_empty()
        );
        // Nothing was rescued: a refused session's edits are never hand edits.
        assert!(!mode.graph.common.join("mdkg/graph/rescued").exists());
    }

    #[test]
    fn an_edit_outside_the_leases_is_refused_and_rescued() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let before = clone.tip();
        let e = prepare_record_publish(
            &mode,
            &mut store,
            ("req-3", "update", "tester"),
            |w, _| {
                w.write(".mdkg/config.json", b"{}")?;
                Ok((
                    Publish {
                        message: "update".into(),
                        leases: vec![],
                        allowed: vec![Scope::new(leaf())],
                        trailers: vec![],
                    },
                    (),
                ))
            },
            |_, _| panic!("nothing is recorded"),
        )
        .expect_err("refused");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.unleased_edit"),
            "{e:?}"
        );
        assert_eq!(clone.tip(), before);
        assert_ne!(
            std::fs::read(root.join(".mdkg/config.json")).expect("config"),
            b"{}"
        );
        let rescued = mode.graph.common.join("mdkg/graph/rescued");
        assert!(rescued.is_dir(), "the stray file is rescued");
    }

    #[test]
    fn a_failure_after_the_record_is_published_by_the_next_command() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let lease = claim(&mut store);
        let before = clone.tip();
        let e = {
            let _fault = FaultAfter::set("recorded");
            prepare_record_publish(
                &mode,
                &mut store,
                ("req-4", "update", "tester"),
                set_status("blocked"),
                commit_under(lease, "blocked"),
            )
            .expect_err("injected")
        };
        assert!(
            e.diagnostic
                .hint
                .as_deref()
                .unwrap_or("")
                .contains("recorded"),
            "{e:?}"
        );
        assert_eq!(clone.tip(), before, "nothing published yet");
        drop(store);
        unbind();
        // The next command replays the intent the store recorded.
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let tip = clone.tip();
        assert_ne!(tip, before);
        assert!(
            clone
                .git(&["show", &format!("{tip}:{}", leaf())])
                .contains("status: blocked")
        );
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("status: blocked")
        );
        let mode = bound().expect("bound");
        assert!(mode.cache.session_record().expect("record").is_none());
        assert!(
            Journal::open(&mode.graph.common)
                .expect("journal")
                .pending()
                .expect("pending")
                .is_empty()
        );
    }

    #[test]
    fn a_failure_before_the_record_leaves_nothing() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let before = clone.tip();
        // A session that wrote and journaled, then failed before the store
        // recorded anything: its files, intent and pending ref all go.
        {
            let _fault = FaultAfter::set("prepared");
            prepare_record_publish(
                &mode,
                &mut store,
                ("req-5", "update", "tester"),
                set_status("blocked"),
                |_, _| panic!("not reached"),
            )
            .expect_err("injected");
        }
        assert!(mode.cache.session_record().expect("record").is_none());
        assert!(
            clone
                .git(&["for-each-ref", "refs/mdkg/pending/"])
                .is_empty()
        );
        assert_eq!(clone.tip(), before);
        drop(store);
        unbind();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        assert_eq!(clone.tip(), before);
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("status: todo")
        );
    }

    #[test]
    fn status_reports_the_graph_ref_and_hand_edits_without_reading_them() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let status = crate::coord::graph_status(&repo).expect("ref mode");
        assert_eq!(status.name.as_deref(), Some(GRAPH));
        assert_eq!(status.tip, clone.tip());
        assert!(status.hand_edits.is_empty());
        // A stale .mdkg written into the checkout is reported, not read.
        std::fs::create_dir_all(clone.dir.join(".mdkg/work")).expect("dir");
        std::fs::write(clone.dir.join(leaf()), node("done")).expect("hand edit");
        let status = crate::coord::graph_status(&repo).expect("ref mode");
        assert_eq!(status.hand_edits.len(), 1, "{:?}", status.hand_edits);
        assert_eq!(status.hand_edits[0].source, "checkout");
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("status: todo")
        );
    }

    #[test]
    fn changed_paths_compare_the_code_base_with_the_graph_tip() {
        let clone = Clone::new();
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        assert_eq!(changed_paths(&mode).expect("changed"), None, "no code base");
        unbind();
        clone.git(&["config", "mdkg.mainRef", "trunk"]);
        bind(&clone.dir).expect("bind");
        let mode = bound().expect("bound");
        let changed = changed_paths(&mode).expect("changed").expect("code base");
        assert!(changed.contains(&leaf()), "{changed:?}");
        assert!(changed.contains(".mdkg/config.json"), "{changed:?}");
        assert!(!changed.contains("code.rs"), "{changed:?}");
    }

    #[test]
    fn a_published_changeset_is_held_by_the_graph_ref() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let mode = bound().expect("bound");
        let mut store = clone.store();
        let lease = claim(&mut store);
        prepare_record_publish(
            &mode,
            &mut store,
            ("req-6", "update", "tester"),
            set_status("progress"),
            commit_under(lease, "progress"),
        )
        .expect("write");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        repo.observe(&mut store, 3).expect("observe");
        let state = store.coordinator().expect("state");
        let c = state
            .changesets()
            .find(|c| c.scope == Scope::new(leaf()))
            .expect("changeset");
        assert!(crate::coord::published(&repo, &mode, c).expect("published"));
    }

    #[test]
    fn decision_grants_read_the_node_at_the_graph_tip() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        assert_eq!(repo.graph_git().head().expect("head").commit, clone.tip());
        assert_eq!(
            repo.disk_base(&leaf()).join(leaf()),
            root.join(leaf()),
            "the node's bytes are the cache's"
        );
    }

    /// The writers this lane has moved onto the seam import the guarded
    /// facade, never `mdkg_coord_sqlite::git` itself, so a graph path they
    /// pass by mistake is refused instead of read from the checkout.
    #[test]
    fn converted_writers_use_only_the_guarded_git() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for file in [
            "coord.rs",
            "authority_decision.rs",
            "init.rs",
            "node_edit.rs",
        ] {
            let text = std::fs::read_to_string(src.join(file)).expect("source");
            let raw = text.lines().any(|l| {
                let l = l.trim();
                !l.starts_with("//")
                    && (l.contains("mdkg_coord_sqlite::git")
                        || (l.starts_with("use mdkg_coord_sqlite::{") && l.contains("git")))
            });
            assert!(!raw, "{file} imports the raw git module");
        }
    }

    fn task_node(root: &Path) -> mdkg_graph::IndexNode {
        let config = mdkg_graph::config::load(root).expect("config");
        let index = mdkg_graph::index::build(root, &config).expect("index");
        index
            .nodes
            .iter()
            .find(|n| n.id == TASK)
            .cloned()
            .expect("task")
    }

    #[test]
    fn node_edit_writes_an_excluded_copy_outside_git() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let edited = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        assert_eq!(edited.copy, clone.dir.join(".mdkg-edit").join(leaf()));
        assert!(!edited.copy.starts_with(clone.dir.join(".git")));
        assert_eq!(
            std::fs::read_to_string(&edited.copy).expect("copy"),
            node("todo")
        );
        assert!(!edited.kept);
        let exclude =
            std::fs::read_to_string(clone.dir.join(".git/info/exclude")).expect("exclude");
        assert!(
            exclude.lines().any(|l| l == node_edit::EXCLUDE_LINE),
            "{exclude}"
        );
        assert!(
            clone.git(&["status", "--porcelain"]).is_empty(),
            "the copy is excluded"
        );
        // The node's lease is the actor's.
        let state = clone.store().coordinator().expect("state");
        assert!(
            state
                .leases()
                .any(|l| l.owner == "tester" && l.scope == Scope::new(leaf()))
        );
    }

    #[test]
    fn node_edit_keeps_a_modified_copy_and_discard_starts_over() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let first = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let notes = format!("{}\nmy notes\n", node("todo"));
        std::fs::write(&first.copy, &notes).expect("notes");
        // Interrupted, then run again: the notes survive.
        let again = node_edit::edit(&repo, "tester", &task, None, false).expect("again");
        assert!(again.kept);
        assert_eq!(again.preimage, first.preimage);
        assert_eq!(std::fs::read_to_string(&again.copy).expect("copy"), notes);
        let fresh = node_edit::edit(&repo, "tester", &task, None, true).expect("discard");
        assert!(!fresh.kept);
        assert_eq!(
            std::fs::read_to_string(&fresh.copy).expect("copy"),
            node("todo")
        );
    }

    #[test]
    fn node_edit_refuses_a_linked_copy_folder() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let elsewhere = clone.dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("target");
        std::os::unix::fs::symlink(&elsewhere, clone.dir.join(".mdkg-edit")).expect("link");
        let e = node_edit::edit(&repo, "tester", &task, None, false).expect_err("refused");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.edit_path_unsafe"),
            "{e:?}"
        );
        // A link on a parent folder inside it, too.
        std::fs::remove_file(clone.dir.join(".mdkg-edit")).expect("unlink");
        std::fs::create_dir_all(clone.dir.join(".mdkg-edit")).expect("dir");
        std::os::unix::fs::symlink(&elsewhere, clone.dir.join(".mdkg-edit/.mdkg")).expect("link");
        let e = node_edit::edit(&repo, "tester", &task, None, false).expect_err("refused");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.edit_path_unsafe"),
            "{e:?}"
        );
        assert!(std::fs::read_dir(&elsewhere).expect("dir").next().is_none());
    }

    /// The actor's active leases in the clone's store.
    fn leases_of(clone: &Clone, actor: &str) -> usize {
        let state = clone.store().coordinator().expect("state");
        state
            .leases()
            .filter(|l| l.owner == actor && l.status == mdkg_coord::lease::Status::Active)
            .count()
    }

    /// e2e N2: a local work node (ref-mode init's default for tasks) is
    /// refused by node edit and node save before any lease, naming its
    /// file; refusals of the copy folder leave no lease either.
    #[test]
    fn node_edit_refuses_local_work_and_leaves_no_lease() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let mut task = task_node(&root);
        let local = clone.dir.join(".git/mdkg/work/root/task-local.md");
        task.path = local.display().to_string();
        let e = node_edit::edit(&repo, "tester", &task, None, false).expect_err("local");
        assert!(e.diagnostic.message.contains("local work"), "{e:?}");
        let hint = e.diagnostic.hint.as_deref().expect("hint");
        assert!(hint.contains(&task.path), "{hint}");
        let e = node_edit::save(&repo, "tester", &task, "save-l", None, None).expect_err("local");
        assert!(e.diagnostic.message.contains("local work"), "{e:?}");
        assert_eq!(leases_of(&clone, "tester"), 0);
        // An unsafe copy folder is refused before the lease too.
        let task = task_node(&root);
        let elsewhere = clone.dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("target");
        std::os::unix::fs::symlink(&elsewhere, clone.dir.join(".mdkg-edit")).expect("link");
        let e = node_edit::edit(&repo, "tester", &task, None, false).expect_err("refused");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.edit_path_unsafe"),
            "{e:?}"
        );
        assert_eq!(leases_of(&clone, "tester"), 0);
    }

    /// e2e N2: a content save refused after it took the lease fresh
    /// releases it; one under `node edit`'s lease keeps that lease.
    #[test]
    fn node_save_refused_releases_only_a_lease_it_took() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let text = node("todo");
        let stale = Some("0".repeat(40));
        let content = Some((text.as_bytes(), stale.as_deref()));
        node_edit::save(&repo, "other", &task, "save-s1", None, content).expect_err("stale");
        assert_eq!(leases_of(&clone, "other"), 0);
        node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        node_edit::save(&repo, "tester", &task, "save-s2", None, content).expect_err("stale");
        assert_eq!(leases_of(&clone, "tester"), 1);
    }

    /// Terra #16768 (orchestrator's rule): a save releases only a lease it
    /// acquired itself; a lease held before it started (node edit's, a
    /// claim made since an earlier save, a broader claim) stays held.
    #[test]
    fn a_save_releases_only_a_lease_it_acquired() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let release_all = |actor: &str| {
            let state = clone.store().coordinator().expect("state");
            let ids: Vec<_> = state
                .leases()
                .filter(|l| l.owner == actor && l.status == mdkg_coord::lease::Status::Active)
                .map(|l| l.id.clone())
                .collect();
            for id in ids {
                clone
                    .store()
                    .execute(&Command::Release {
                        owner: actor.into(),
                        lease: id,
                        now: crate::coord::now_ms(),
                    })
                    .expect("release");
            }
        };
        let claim = |scope: &str| {
            clone
                .store()
                .execute(&Command::Claim {
                    owner: "tester".into(),
                    scope: Scope::new(scope),
                    mode: Mode::Exclusive,
                    now: crate::coord::now_ms(),
                    request: None,
                    node: None,
                    branch: None,
                })
                .expect("claim");
        };
        // Unchanged copy: node edit's lease, held before the save, stays.
        node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let saved = node_edit::save(&repo, "tester", &task, "same-1", None, None).expect("save");
        assert_eq!(saved.commit, None);
        assert_eq!(leases_of(&clone, "tester"), 1);
        release_all("tester");
        // A published content save, then its retry by request id: the
        // retry's own fresh lease is released.
        let text = format!("{}\nnotes\n", node("todo"));
        let edited = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let content = Some((text.as_bytes(), edited.preimage.as_deref()));
        let first = node_edit::save(&repo, "tester", &task, "pub-1", None, content).expect("save");
        assert!(first.commit.is_some());
        assert_eq!(leases_of(&clone, "tester"), 0);
        let again = node_edit::save(&repo, "tester", &task, "pub-1", None, content).expect("retry");
        assert_eq!(again.commit, first.commit);
        assert_eq!(leases_of(&clone, "tester"), 0);
        // A claim taken after the save, for new work, survives a retry.
        claim(&task.path);
        let again = node_edit::save(&repo, "tester", &task, "pub-1", None, content).expect("retry");
        assert_eq!(again.commit, first.commit);
        assert_eq!(leases_of(&clone, "tester"), 1);
        release_all("tester");
        // A folder claim covering the node is kept.
        claim(".mdkg/work");
        let task = task_node(&root);
        node_edit::edit(&repo, "tester", &task, None, true).expect("edit");
        node_edit::save(&repo, "tester", &task, "same-2", None, None).expect("save");
        assert_eq!(leases_of(&clone, "tester"), 1);
    }

    /// Terra #16757: a release that fails is reported, with the lease to
    /// release by hand.
    #[test]
    fn a_failed_release_is_named_in_the_refusal() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let text = node("todo");
        let stale = Some("0".repeat(40));
        let content = Some((text.as_bytes(), stale.as_deref()));
        node_edit::RELEASE_FAILS.with(|f| f.set(true));
        let e = node_edit::save(&repo, "other", &task, "save-f", None, content).expect_err("stale");
        let hint = e.diagnostic.hint.as_deref().expect("hint");
        assert!(hint.contains("may still be held"), "{hint}");
        assert!(hint.contains("release lease-"), "{hint}");
        assert!(hint.contains("--as=other`"), "{hint}");
        assert_eq!(leases_of(&clone, "other"), 1);
        // Terra #16828: an actor with a space is one quoted shell word.
        let state = clone.store().coordinator().expect("state");
        let held = state
            .leases()
            .find(|l| l.owner == "other" && l.status == mdkg_coord::lease::Status::Active)
            .expect("lease")
            .id
            .clone();
        clone
            .store()
            .execute(&Command::Release {
                owner: "other".into(),
                lease: held,
                now: crate::coord::now_ms(),
            })
            .expect("release");
        node_edit::RELEASE_FAILS.with(|f| f.set(true));
        let e =
            node_edit::save(&repo, "Pat Work", &task, "save-g", None, content).expect_err("stale");
        let hint = e.diagnostic.hint.as_deref().expect("hint");
        assert!(hint.contains("--as='Pat Work'`"), "{hint}");
    }

    /// Terra #16851: a replay is read-only even after an intervening save:
    /// an edit based on that later save keeps its record and copy, whether
    /// its copy differs from the replayed save or changes back to it.
    #[test]
    fn a_replay_after_a_later_save_touches_nothing() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let a = format!("{}\nA\n", node("todo"));
        let b = format!("{}\nB\n", node("todo"));
        let task = task_node(&root);
        let e1 = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let r1_content = Some((a.as_bytes(), e1.preimage.as_deref()));
        let r1 = node_edit::save(&repo, "tester", &task, "r1", None, r1_content).expect("r1");
        let task = task_node(&root);
        let e2 = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let r2_content = Some((b.as_bytes(), e2.preimage.as_deref()));
        let r2 = node_edit::save(&repo, "tester", &task, "r2", None, r2_content).expect("r2");
        assert_ne!(r1.commit, r2.commit);
        // An edit based on B, its copy modified.
        let task = task_node(&root);
        let e3 = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let c = format!("{}\nC\n", node("todo"));
        std::fs::write(&e3.copy, &c).expect("write copy");
        // Terra #16863/#16875: the replay neither renews the edit's lease
        // nor takes one of its own. The store's event head (any lease write,
        // a heartbeat included, appends an event) and its leases are
        // unchanged, whatever the clock says.
        let store_state = || {
            let (state, cursor) = clone.store().coordinator_with_cursor().expect("state");
            let mut all: Vec<_> = state
                .leases()
                .map(|l| (l.id.0.clone(), format!("{:?}", l.status), l.deadline))
                .collect();
            all.sort();
            (cursor, all)
        };
        let before = store_state();
        let again =
            node_edit::save(&repo, "tester", &task, "r1", None, r1_content).expect("replay");
        assert_eq!(again.commit, r1.commit);
        assert_eq!(std::fs::read_to_string(&e3.copy).expect("copy"), c);
        assert_eq!(store_state(), before);
        // Its copy changed back to A: a copy-based replay of r1 still
        // touches nothing.
        std::fs::write(&e3.copy, &a).expect("write copy");
        let again = node_edit::save(&repo, "tester", &task, "r1", None, None).expect("replay");
        assert_eq!(again.commit, r1.commit);
        assert_eq!(std::fs::read_to_string(&e3.copy).expect("copy"), a);
        assert_eq!(store_state(), before);
        // The edit's record survived: its copy saves as a new request.
        let r3 = node_edit::save(&repo, "tester", &task, "r3", None, None).expect("r3");
        assert!(r3.commit.is_some() && r3.commit != r2.commit);
    }

    /// Terra #16840: publishing one node under a folder claim would commit
    /// (end) the whole claim, so it's refused with nothing published; the
    /// folder claim and the copy stay.
    #[test]
    fn a_folder_claim_survives_a_save_in_it() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        clone
            .store()
            .execute(&Command::Claim {
                owner: "tester".into(),
                scope: Scope::new(".mdkg/work"),
                mode: Mode::Exclusive,
                now: crate::coord::now_ms(),
                request: None,
                node: None,
                branch: None,
            })
            .expect("claim");
        let task = task_node(&root);
        let edited = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let text = format!("{}\nnotes\n", node("todo"));
        std::fs::write(&edited.copy, &text).expect("write copy");
        let before = clone.git(&["rev-parse", GRAPH]);
        let e = node_edit::save(&repo, "tester", &task, "f-1", None, None).expect_err("refused");
        assert!(
            e.diagnostic.message.contains(".mdkg/work"),
            "{}",
            e.diagnostic.message
        );
        assert_eq!(clone.git(&["rev-parse", GRAPH]), before);
        assert_eq!(leases_of(&clone, "tester"), 1);
        let state = clone.store().coordinator().expect("state");
        assert!(state.leases().any(|l| l.owner == "tester"
            && l.scope == Scope::new(".mdkg/work")
            && l.status == mdkg_coord::lease::Status::Active));
        assert_eq!(std::fs::read_to_string(&edited.copy).expect("copy"), text);
        let hint = e.diagnostic.hint.as_deref().expect("hint");
        assert!(hint.contains("your copy is kept"), "{hint}");
        // Terra #16872: a content save (`--stdin`) has no copy to keep.
        let content = Some((text.as_bytes(), edited.preimage.as_deref()));
        let e = node_edit::save(&repo, "tester", &task, "f-2", None, content).expect_err("refused");
        let hint = e.diagnostic.hint.as_deref().expect("hint");
        assert!(hint.contains("nothing was saved"), "{hint}");
        assert_eq!(clone.git(&["rev-parse", GRAPH]), before);
        assert_eq!(leases_of(&clone, "tester"), 1);
    }

    /// Terra #16828: a replay of a published save leaves an edit begun
    /// after it alone: its record and modified copy stay, and saving that
    /// copy under a new request id publishes it.
    #[test]
    fn a_replay_leaves_a_later_edit_alone() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let text = format!("{}\nfirst\n", node("todo"));
        let edited = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let content = Some((text.as_bytes(), edited.preimage.as_deref()));
        let first = node_edit::save(&repo, "tester", &task, "r1", None, content).expect("save");
        assert!(first.commit.is_some());
        // A later edit, its copy modified.
        let task = task_node(&root);
        let later = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let newer = format!("{}\nfirst\nsecond\n", node("todo"));
        std::fs::write(&later.copy, &newer).expect("write copy");
        // The delayed retry of r1 returns its receipt and changes nothing.
        let again = node_edit::save(&repo, "tester", &task, "r1", None, content).expect("retry");
        assert_eq!(again.commit, first.commit);
        assert_eq!(std::fs::read_to_string(&later.copy).expect("copy"), newer);
        assert_eq!(leases_of(&clone, "tester"), 1);
        let second = node_edit::save(&repo, "tester", &task, "r2", None, None).expect("save copy");
        assert!(second.commit.is_some() && second.commit != first.commit);
    }

    #[test]
    fn node_save_publishes_the_copy_and_removes_it() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let dir = clone.dir.join("scratch");
        std::fs::create_dir_all(&dir).expect("dir");
        let edited = node_edit::edit(&repo, "tester", &task, Some(&dir), false).expect("edit");
        assert!(edited.copy.starts_with(&dir));
        let body = format!("{}\nprogress notes\n", node("todo"));
        std::fs::write(&edited.copy, &body).expect("notes");
        let before = clone.tip();
        let saved =
            node_edit::save(&repo, "tester", &task, "save-1", Some(&dir), None).expect("save");
        let tip = clone.tip();
        assert_ne!(tip, before);
        assert_eq!(saved.commit.as_deref(), Some(tip.as_str()));
        assert!(
            clone
                .git(&["show", &format!("{tip}:{}", leaf())])
                .contains("progress notes")
        );
        assert!(!edited.copy.exists());
        assert!(
            std::fs::read_to_string(root.join(leaf()))
                .expect("cache")
                .contains("progress notes")
        );
    }

    #[test]
    fn node_save_is_refused_as_stale_and_keeps_the_copy() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        // The default folder, then a `--dir` one: the hint's recovery
        // command names that folder (Terra #17973).
        let custom = clone.dir.join("edits");
        std::fs::create_dir_all(&custom).expect("edit folder");
        for (round, dir) in [(0, None), (1, Some(custom.as_path()))] {
            let edited = node_edit::edit(&repo, "tester", &task, dir, true).expect("edit");
            std::fs::write(&edited.copy, format!("{}\nmine\n", node("todo"))).expect("notes");
            // Someone publishes a change to the node meantime (as a sync
            // would).
            let writer = bound().expect("bound").writer().expect("writer");
            writer
                .write(&refwrite::Request {
                    id: format!("other-{round}"),
                    base: Some(writer.snapshot().expect("tip")),
                    changes: vec![refwrite::Change::Put {
                        path: leaf(),
                        bytes: node(["blocked", "todo"][round]).into_bytes(),
                    }],
                    message: "sync".into(),
                    trailers: vec![],
                })
                .expect("other write");
            let tip = clone.tip();
            let e = node_edit::save(&repo, "tester", &task, &format!("save-{round}"), dir, None)
                .expect_err("stale");
            assert!(
                crate::coord::graph_git::has_code(&e, "graph.write_stale"),
                "{e:?}"
            );
            // A conflict, re-read and retry (exit 5), not a failure (4).
            assert_eq!(e.exit_code(), 5, "{e:?}");
            // The hint says to move the edits out before `--discard`
            // overwrites the copy, names it, and gives the whole command:
            // root, actor, node and folder (e2e UX).
            let hint = e.diagnostic.hint.as_deref().expect("hint");
            assert!(hint.contains("move your edits out of it first"), "{hint}");
            assert!(
                hint.contains(&*edited.copy.file_name().expect("name").to_string_lossy()),
                "{hint}"
            );
            assert!(
                hint.contains(&format!("--as=tester node edit {} ", task.id)),
                "{hint}"
            );
            assert!(hint.contains(" --discard`"), "{hint}");
            assert_eq!(
                hint.contains(&format!("--dir={}", custom.display())),
                dir.is_some(),
                "{hint}"
            );
            assert_eq!(clone.tip(), tip);
            assert!(
                std::fs::read_to_string(&edited.copy)
                    .expect("copy")
                    .contains("mine")
            );
        }
    }

    #[test]
    fn node_save_takes_content_with_its_preimage() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let body = format!("{}\nfrom MCP\n", node("todo"));
        // Content alone carries no preimage: refused.
        let e = node_edit::save(
            &repo,
            "tester",
            &task,
            "save-3",
            None,
            Some((body.as_bytes(), None)),
        )
        .expect_err("no preimage");
        assert!(e.diagnostic.message.contains("preimage"), "{e:?}");
        // The preimage the content was based on.
        let pre = clone.git(&["rev-parse", &format!("{GRAPH}:{}", leaf())]);
        node_edit::save(
            &repo,
            "tester",
            &task,
            "save-3",
            None,
            Some((body.as_bytes(), Some(pre.as_str()))),
        )
        .expect("save");
        assert!(
            clone
                .git(&["show", &format!("{GRAPH}:{}", leaf())])
                .contains("from MCP")
        );
        assert!(!clone.dir.join(".mdkg-edit").join(leaf()).exists());
        // Content based on a preimage the graph has moved past is stale.
        let later = format!("{}\nlater\n", node("todo"));
        let e = node_edit::save(
            &repo,
            "tester",
            &task,
            "save-4",
            None,
            Some((later.as_bytes(), Some(pre.as_str()))),
        )
        .expect_err("stale");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.write_stale"),
            "{e:?}"
        );
    }

    #[test]
    fn a_crash_between_copy_and_record_fails_closed() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        // An unmodified copy of A.
        let first = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        // Nothing but the copy sits beside it: the preimage is mdkg's own.
        let beside: Vec<_> = std::fs::read_dir(first.copy.parent().expect("parent"))
            .expect("dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(beside.len(), 1, "{beside:?}");
        // The graph moves to B.
        let writer = bound().expect("bound").writer().expect("writer");
        writer
            .write(&refwrite::Request {
                id: "move".into(),
                base: Some(writer.snapshot().expect("tip")),
                changes: vec![refwrite::Change::Put {
                    path: leaf(),
                    bytes: node("blocked").into_bytes(),
                }],
                message: "sync".into(),
                trailers: vec![],
            })
            .expect("move");
        unbind();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        // A refresh wrote B to the copy, then crashed before its record:
        // the record still says A.
        std::fs::write(&first.copy, node("blocked")).expect("copy B");
        let again = node_edit::edit(&repo, "tester", &task, None, false).expect("again");
        assert!(again.kept, "a copy the record doesn't describe is kept");
        assert_eq!(again.preimage, first.preimage, "with the old preimage");
        // Saving it can't pass as current: refused as stale.
        let tip = clone.tip();
        let e = node_edit::save(&repo, "tester", &task, "save-5", None, None).expect_err("stale");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.write_stale"),
            "{e:?}"
        );
        assert_eq!(clone.tip(), tip);
    }

    #[test]
    fn a_save_reusing_a_request_id_keeps_the_newer_copy() {
        use crate::coord::node_edit;
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let repo = crate::coord::Repo::open(&root).expect("repo");
        let task = task_node(&root);
        let first = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let saved = format!("{}\nfirst save\n", node("todo"));
        std::fs::write(&first.copy, &saved).expect("notes");
        let one = node_edit::save(&repo, "tester", &task, "save-r", None, None).expect("save");
        // A retry of that same save (same id, same bytes) is the receipt.
        let pre = first.preimage.clone().expect("preimage");
        let again = node_edit::save(
            &repo,
            "tester",
            &task,
            "save-r",
            None,
            Some((saved.as_bytes(), Some(pre.as_str()))),
        )
        .expect("retry");
        assert_eq!(again.commit, one.commit);
        // Edited again, then saved under the used id: refused, nothing
        // applied, and the newer copy and its record kept.
        let second = node_edit::edit(&repo, "tester", &task, None, false).expect("edit");
        let newer = format!("{}\nnewer notes\n", node("todo"));
        std::fs::write(&second.copy, &newer).expect("newer");
        let tip = clone.tip();
        let e = node_edit::save(&repo, "tester", &task, "save-r", None, None).expect_err("reused");
        assert!(has_code(&e, "graph.request_reused"), "{e:?}");
        assert_eq!(clone.tip(), tip);
        assert_eq!(std::fs::read_to_string(&second.copy).expect("kept"), newer);
        // The record is kept too: a save with a fresh id publishes it.
        node_edit::save(&repo, "tester", &task, "save-r2", None, None).expect("fresh id");
        assert!(
            clone
                .git(&["show", &format!("{GRAPH}:{}", leaf())])
                .contains("newer notes")
        );
    }

    /// The refused-open probe asks git only what every supported git
    /// answers: an old git is one reason an open is refused (Terra #10725).
    #[test]
    fn the_probe_needs_no_new_git_option() {
        let src =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/graph_git.rs"))
                .expect("source");
        let start = src.find("fn probe_graph_ref").expect("probe");
        let end = src[start..].find("\n}\n").expect("end") + start;
        let probe: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!probe.contains("--path-format"), "{probe}");
        assert!(probe.contains("\"--git-common-dir\""), "{probe}");
    }

    #[test]
    fn a_relative_common_dir_resolves_against_the_folder() {
        let clone = Clone::new();
        // From the top, git answers `.git`, relative.
        assert_eq!(probe_graph_ref(&clone.dir), Some(true));
        clone.git(&["config", "--unset", "mdkg.graph.ref"]);
        assert_eq!(probe_graph_ref(&clone.dir), Some(false));
        // A folder that doesn't exist holds no repository.
        assert_eq!(probe_graph_ref(&clone.dir.join("missing/nested")), None);
        // A repository whose common folder has no config file can't say,
        // so the probe fails closed rather than reading "unset".
        std::fs::rename(
            clone.dir.join(".git/config"),
            clone.dir.join(".git/config.away"),
        )
        .expect("move config");
        assert_eq!(probe_graph_ref(&clone.dir), Some(true));
        std::fs::rename(
            clone.dir.join(".git/config.away"),
            clone.dir.join(".git/config"),
        )
        .expect("restore config");
        let outside =
            std::env::temp_dir().join(format!("mdkg-nogit-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir_all(&outside).expect("dir");
        assert_eq!(probe_graph_ref(&outside), None);
        std::fs::remove_dir_all(&outside).ok();
    }

    /// Every `graph.*` or `coord.*` code the lane's files name is in the
    /// registry, so none reaches the `io.git_failed` fallback.
    #[test]
    fn every_code_named_is_registered() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for file in [
            "graph_git.rs",
            "node_edit.rs",
            "coord.rs",
            "init.rs",
            "read.rs",
            "anchor.rs",
            "work/plan.rs",
        ] {
            let text = std::fs::read_to_string(src.join(file)).expect("source");
            for (i, _) in text.match_indices('"') {
                let rest = &text[i + 1..];
                let Some(end) = rest.find('"') else { continue };
                let word = &rest[..end];
                let named = (word.starts_with("graph.") || word.starts_with("coord."))
                    && word.len() > 6
                    && word
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_');
                if named && !word.ends_with(".rs") && !word.ends_with(".sqlite") {
                    assert!(
                        Code::parse(word).is_some(),
                        "{file}: {word} isn't registered"
                    );
                }
            }
        }
    }

    /// A stale anchor intent at the chain's seq (the graph changed the
    /// anchor after the intent was recorded, and no record came since)
    /// doesn't refuse every rerun: the retry adds the graph's tip to the
    /// request id, and that publishes (g-launcher #13274).
    #[test]
    fn a_stale_anchor_intent_at_the_same_seq_retries_with_the_tip() {
        let clone = Clone::new();
        let root = bind(&clone.dir).expect("bind").expect("ref mode");
        let mut store = clone.store();
        claim(&mut store);
        {
            let _fault = FaultAfter::set("recorded");
            crate::anchor::write_anchor(&root, &mut store, false).expect_err("fault");
        }
        let journal = Journal::open(&clone.dir.join(".git")).expect("journal");
        let pending = journal.pending().expect("pending");
        assert_eq!(pending.len(), 1, "{pending:?}");
        let first = pending[0].id().to_owned();
        let (store_id, _) = first
            .strip_prefix("anchor-")
            .and_then(|r| r.rsplit_once('-'))
            .expect("anchor-<store>-<seq>");
        let rel = format!("{}/{store_id}.json", crate::anchor::ANCHOR_DIR);
        // The graph changes the anchor meantime, to a valid one (the same
        // anchor at another time, canonical, as another clone's would be):
        // the intent is stale (Terra #18107).
        let prepared = clone.git(&["show", &format!("{}:{rel}", pending[0].prepared.commit)]);
        let mut value: serde_json::Value = serde_json::from_str(&prepared).expect("anchor");
        value["ts"] = "2026-01-01T00:00:00.000Z".into();
        let foreign = format!("{}\n", serde_json::to_string(&value).expect("json"));
        assert_ne!(foreign.trim_end(), prepared.trim_end());
        let writer = bound().expect("bound").writer().expect("writer");
        writer
            .write(&refwrite::Request {
                id: "elsewhere".into(),
                base: Some(writer.snapshot().expect("tip")),
                changes: vec![refwrite::Change::Put {
                    path: rel.clone(),
                    bytes: foreign.clone().into_bytes(),
                }],
                message: "another anchor".into(),
                trailers: vec![],
            })
            .expect("other write");
        let tip = clone.tip();
        let report = crate::anchor::write_anchor(&root, &mut store, false).expect("retried");
        assert!(report.data.is_some());
        let after = clone.tip();
        assert_ne!(after, tip, "published under the new id");
        let text = clone.git(&["show", &format!("{after}:{rel}")]);
        assert_ne!(text, foreign.trim_end(), "{text}");
        assert!(
            clone
                .git(&["log", "-1", "--format=%B", &after])
                .contains("anchor"),
            "{after}"
        );
        // The stale one is kept for recovery, as before.
        assert!(journal.load(&first).expect("load").is_some());
    }

    /// Fails the write path after `step`, on this thread, while held.
    struct FaultAfter;
    impl FaultAfter {
        fn set(step: &'static str) -> Self {
            FAIL_AFTER.with(|f| *f.borrow_mut() = Some(step));
            Self
        }
    }
    impl Drop for FaultAfter {
        fn drop(&mut self) {
            FAIL_AFTER.with(|f| *f.borrow_mut() = None);
        }
    }
}
