//! `graph.remote` sync (edd-01a18867, section 4): the graph ref is pulled
//! from and pushed to an optional `mdkg.graph.remote`, which may be a
//! private repository other than the code's.
//!
//! - **Approval (G23).** Configuration alone never pushes. A person's
//!   `mdkg graph remote approve` pins the remote, its `remoteRef` and its
//!   effective destinations in `<common>/mdkg/graph/remote.json`
//!   ([`approve`]); pushes go only to that destination while it's
//!   unchanged ([`approval`]).
//! - **The algorithm** ([`run`]): fetch `remoteRef` through one explicit
//!   refspec, take a fast-forward or merge it (validated strictly), CAS
//!   the local graph ref under the graph lock, then push with a lease, in
//!   a bounded loop. A merge's conflicting paths take the remote's version
//!   (G12), named in `Mdkg-Conflict` trailers and recorded in
//!   `conflicts.json` before the local CAS.
//! - **State** ([`State`], `sync.json`): the last accepted remote tip, so a
//!   rewritten remote stays refused across restarts; the last success and
//!   error; and a hook's refusal, which stops background sync until a
//!   person syncs.
//! - **Cadence (G13)** is [`Timer`]'s: the launcher pushes `pushDelay`
//!   after the last local change, coalesced, and pulls every
//!   `pullInterval` and when it starts. Never per turn or inline after a
//!   write.
//!
//! The guards are in [`guard`], the network commands in [`net`].

pub mod guard;
pub mod net;

use mdkg_format::program::BIN;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::committed::{self, DefinitionCheck};
use crate::graphref::{Destination, Error, GraphRef};
use crate::refwrite::{self, GraphLock, LockKind, LockMode, RefWriter};
use guard::{Scanner, git, git_ok, short};
use net::{Net, Pushed};

/// How many fetch-merge-push rounds a sync makes before it gives up on a
/// remote that keeps moving (`graph.sync_contended`).
pub const ATTEMPTS: u32 = 8;

/// `s` safe to print on one line: control characters escaped.
pub(crate) fn printable(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn graph_dir(common: &Path) -> Result<PathBuf, Error> {
    Ok(refwrite::graph_dir(common)?)
}

/// Reads a small JSON file in the graph folder; `None` when it's absent.
/// Held while one sync, approval or resolution reads and writes sync's
/// metadata (`sync.json`, `conflicts.json`, `remote.json`): an exclusive
/// lock on `<common>/mdkg/graph/sync.lock`, so a timer's sync and a
/// person's never interleave and erase each other's records (design
/// review, Astra 2). Taken before the graph lock, never while holding it.
#[derive(Debug)]
pub struct SyncLock {
    _file: std::fs::File,
}

/// Blocks until [`SyncLock`] is held. Not re-entrant: a caller holding it
/// passes it on instead of locking again.
pub fn lock(common: &Path) -> Result<SyncLock, Error> {
    let dir = common.join("mdkg").join("graph");
    let io = |e: std::io::Error| {
        Error::new(
            "graph.git",
            format!("the sync lock in {}: {e}", dir.display()),
        )
    };
    std::fs::create_dir_all(&dir).map_err(io)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("sync.lock"))
        .map_err(io)?;
    file.lock().map_err(io)?;
    Ok(SyncLock { _file: file })
}

fn read_json(common: &Path, name: &str) -> Result<Option<Value>, Error> {
    let path = common.join("mdkg").join("graph").join(name);
    let bytes = match mdkg_git::read_capped(&path, 1 << 20) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::new("graph.io", format!("{name}: {e}")))
}

fn write_json(common: &Path, name: &str, value: &Value) -> Result<(), Error> {
    let dir = graph_dir(common)?;
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|e| Error::new("graph.io", e.to_string()))?;
    crate::intent::write_durably(&dir, name, &bytes)?;
    Ok(())
}

// ---------------------------------------------------------------- approval

/// Whether pushes may go to the graph remote (G23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Pinned, and the destination is still the pinned one.
    Approved,
    /// Never approved: nothing is pushed.
    Unapproved,
    /// The remote, its `remoteRef` or a destination changed since it was
    /// pinned: no network write until a person approves it again. The text
    /// says what changed.
    Changed(String),
}

impl Approval {
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Approved => None,
            Self::Unapproved => Some("graph.remote_unapproved"),
            Self::Changed(_) => Some("graph.remote_changed"),
        }
    }
}

/// What approving would pin: the remote, its `remoteRef` and its
/// effective destinations now.
/// `repo` is the checkout sync runs from ([`net::checkout`]), against
/// which relative local URLs resolve.
pub fn current(repo: &Path, graph: &GraphRef) -> Result<Destination, Error> {
    let common = &graph.common;
    let base = net::checkout(common, repo)?;
    let remote = graph
        .remote
        .as_ref()
        .ok_or_else(|| Error::new("graph.remote_invalid", "no mdkg.graph.remote is set"))?;
    let mut d = guard::destinations(common, &base, &remote.name)?;
    d.setup = guard::setup(common, &base, &remote.name)?;
    d.remote = remote.name.clone();
    d.remote_ref = remote.remote_ref.clone();
    Ok(d)
}

/// Compares the pinned destination with [`current`].
pub fn approval(repo: &Path, graph: &GraphRef) -> Result<Approval, Error> {
    let remote = graph
        .remote
        .as_ref()
        .ok_or_else(|| Error::new("graph.remote_invalid", "no mdkg.graph.remote is set"))?;
    let Some(pinned) = &remote.pinned else {
        return Ok(Approval::Unapproved);
    };
    let now = current(repo, graph)?;
    let mut changed = Vec::new();
    if pinned.remote != now.remote {
        changed.push(format!(
            "the remote was {}, now {}",
            pinned.remote, now.remote
        ));
    }
    if pinned.remote_ref != now.remote_ref {
        changed.push(format!(
            "remoteRef was {}, now {}",
            pinned.remote_ref, now.remote_ref
        ));
    }
    if pinned.fetch != now.fetch {
        changed.push("its fetch destination changed".into());
    }
    if pinned.push != now.push {
        changed.push("its push destination changed".into());
    }
    if pinned.setup != now.setup {
        // Names only: values (an ssh command, a helper) are never printed.
        let keys: std::collections::BTreeSet<&String> = pinned
            .setup
            .keys()
            .chain(now.setup.keys())
            .filter(|k| pinned.setup.get(*k) != now.setup.get(*k))
            .collect();
        changed.push(format!(
            "the git setup a push runs with changed ({})",
            keys.into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(if changed.is_empty() {
        Approval::Approved
    } else {
        Approval::Changed(changed.join("; "))
    })
}

/// Pins [`current`] in `remote.json` (`mdkg graph remote approve`). The
/// caller checks the person: an agent's turn never approves. The guards a
/// push would run on the destination run first, so a code remote is never
/// approved. Returns what was pinned.
pub fn approve(repo: &Path, graph: &GraphRef, protected: &[String]) -> Result<Destination, Error> {
    let common = &graph.common;
    let _held = lock(common)?;
    let d = current(repo, graph)?;
    let base = net::checkout(common, repo)?;
    guard::check_remote_ref(
        common,
        &d.remote,
        &d.remote_ref,
        graph.main_ref.as_deref(),
        protected,
    )?;
    guard::check_destination(common, &base, &d.remote, &d, graph.main_ref.as_deref())?;
    crate::graphref::pin_destination(common, &d)?;
    // A new destination's history is its own: what was accepted from the
    // old one doesn't carry over, and a refusal there doesn't either.
    let mut state = State::load(common)?;
    if state.destination.as_deref() != Some(&key(&d)) {
        state = State {
            destination: Some(key(&d)),
            ..State::default()
        };
        state.save(common)?;
    } else if state.missing && !state.recreate {
        // The remote graph ref was deleted (DECISION 17, orchestrator
        // 2026-11-04): approving again is the person's choice to publish
        // the local graph there anew, so a sync may create the absent ref.
        // The accepted tip stays (Terra #19459): if the ref is back when
        // the sync runs, at any tip, the rewrite guard still applies, and
        // the permission ends.
        state.recreate = true;
        state.save(common)?;
    }
    Ok(d)
}

/// The key `sync.json` holds its state under: the pinned destination.
fn key(d: &Destination) -> String {
    format!(
        "{} {} fetch={} push={}",
        d.remote,
        d.remote_ref,
        d.fetch.join(","),
        d.push.join(",")
    )
}

// ------------------------------------------------------------------- state

/// A recorded failure: its code and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub time: u64,
    /// The binary that wrote it, as its own suggestions name it: `None`
    /// in a state written before this was recorded.
    pub bin: Option<String>,
}

impl Failure {
    /// The message as shown now: a command it suggests (`` `<bin> ...` ``,
    /// with the exact name that wrote it, any subcommand: `graph sync`,
    /// `install`, `doctor --repair`) names the binary running now, which
    /// may differ (Terra #20017, #20181). Only that name is replaced, so a
    /// remote's or a hook's text keeps its own backticks and commands
    /// (Terra #20048).
    pub fn shown(&self) -> String {
        match &self.bin {
            Some(bin) if bin.as_str() != BIN.as_str() => self
                .message
                .replace(&format!("`{bin} "), &format!("`{} ", BIN.as_str())),
            _ => self.message.clone(),
        }
    }
}

/// `<common>/mdkg/graph/sync.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The destination this state belongs to ([`key`]).
    pub destination: Option<String>,
    /// The last remote tip this clone accepted: a remote whose tip doesn't
    /// descend from it was rewritten.
    pub accepted: Option<String>,
    /// When a sync last finished.
    pub last_ok: Option<u64>,
    /// The last failure, cleared by a success.
    pub last_error: Option<Failure>,
    /// Failed syncs in a row.
    pub attempts: u32,
    /// A hook refused a push: background sync stops until a person syncs.
    pub refused: Option<Failure>,
    /// The remote graph ref was found deleted after a tip was accepted
    /// (DECISION 17): only approving again lets a push recreate it.
    pub missing: bool,
    /// A person approved again while [`State::missing`]: a sync may create
    /// the absent ref. Ends when the ref is seen again or a push creates
    /// it; the accepted tip is kept throughout (Terra #19459).
    pub recreate: bool,
}

fn failure(v: &Value) -> Option<Failure> {
    Some(Failure {
        code: v.get("code")?.as_str()?.to_owned(),
        message: v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        time: v.get("time")?.as_u64()?,
        bin: v.get("bin").and_then(Value::as_str).map(str::to_owned),
    })
}

fn failure_json(f: &Option<Failure>) -> Value {
    match f {
        Some(f) => json!({"code": f.code, "message": f.message, "time": f.time, "bin": f.bin}),
        None => Value::Null,
    }
}

impl State {
    pub fn load(common: &Path) -> Result<Self, Error> {
        let Some(v) = read_json(common, "sync.json")? else {
            return Ok(Self::default());
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            destination: s("destination"),
            accepted: s("accepted").filter(|a| hex(a)),
            last_ok: v.get("last_ok").and_then(Value::as_u64),
            last_error: v.get("last_error").and_then(failure),
            attempts: v
                .get("attempts")
                .and_then(Value::as_u64)
                .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
            refused: v.get("refused").and_then(failure),
            missing: v.get("missing").and_then(Value::as_bool).unwrap_or(false),
            recreate: v.get("recreate").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn save(&self, common: &Path) -> Result<(), Error> {
        write_json(
            common,
            "sync.json",
            &json!({
                "destination": self.destination,
                "accepted": self.accepted,
                "last_ok": self.last_ok,
                "last_error": failure_json(&self.last_error),
                "attempts": self.attempts,
                "refused": failure_json(&self.refused),
                "missing": self.missing,
                "recreate": self.recreate,
            }),
        )
    }
}

/// The strict check every graph tree sync or `graph resolve` would write
/// passes before the graph ref moves (DECISION 18 (V)): the committed
/// graph at `commit` loads strictly, its authority and agent definitions
/// included, so a duplicate id or a broken definition is refused, never
/// published.
pub fn validate_tip(
    repo: &Path,
    folder: &str,
    graph_id: &str,
    commit: &str,
    definitions: &DefinitionCheck<'_>,
) -> Result<(), Error> {
    committed::load_strict_dir_with(repo, commit, "", folder, graph_id, definitions)
        .map(|_| ())
        .map_err(|e| {
            Error::new(
                "graph.sync_invalid",
                format!(
                    "the candidate graph tip {} fails validation: {e}",
                    short(commit)
                ),
            )
        })
}

/// The merge base of two commits, if they share history.
fn merge_base(common: &Path, a: &str, b: &str) -> Result<Option<String>, Error> {
    let out = git(common, &["merge-base", a, b])?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty()))
}

fn hex(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// --------------------------------------------------------------- conflicts

/// One path a sync merge took the remote's version of (G12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The merge commit.
    pub merge: String,
    pub path: String,
    /// The local version's blob, `None` when the local side deleted it.
    pub local: Option<String>,
    /// The remote version's blob, `None` when the remote deleted it.
    pub remote: Option<String>,
    pub time: u64,
}

fn conflicts_file(common: &Path) -> Result<Vec<Conflict>, Error> {
    let Some(v) = read_json(common, "conflicts.json")? else {
        return Ok(Vec::new());
    };
    let list = v
        .get("conflicts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<Conflict> = Vec::new();
    for c in &list {
        let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_owned);
        let (Some(merge), Some(path)) = (s("merge").filter(|m| hex(m)), s("path")) else {
            continue;
        };
        // One entry per path and merge (Terra #19736): a sync that crashed
        // after recording can make the same merge again and record it
        // twice; the first record stands.
        if out.iter().any(|o| o.path == path && o.merge == merge) {
            continue;
        }
        out.push(Conflict {
            merge,
            path,
            local: s("local").filter(|o| hex(o)),
            remote: s("remote").filter(|o| hex(o)),
            time: c.get("time").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    Ok(out)
}

/// `found` added to the recorded conflicts, but no second entry for a
/// path and merge already recorded.
fn record_conflicts(common: &Path, found: &[Conflict]) -> Result<(), Error> {
    let mut all = conflicts_file(common)?;
    for c in found {
        if !all.iter().any(|o| o.path == c.path && o.merge == c.merge) {
            all.push(c.clone());
        }
    }
    save_conflicts(common, &all)
}

fn save_conflicts(common: &Path, list: &[Conflict]) -> Result<(), Error> {
    let list: Vec<Value> = list
        .iter()
        .map(|c| {
            json!({"merge": c.merge, "path": c.path, "local": c.local, "remote": c.remote, "time": c.time})
        })
        .collect();
    write_json(common, "conflicts.json", &json!({ "conflicts": list }))
}

/// The recorded conflicts whose merge commit is in the graph's history
/// (`tip`): a merge whose local CAS lost never published, so its entries
/// are dropped from what's reported, and so are entries whose merge commit
/// git has since pruned (Terra #19800): such a merge was never reachable.
pub fn conflicts(common: &Path, tip: Option<&str>) -> Result<Vec<Conflict>, Error> {
    let Some(tip) = tip else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut shallow: Option<bool> = None;
    for c in conflicts_file(common)? {
        let exists = git(
            common,
            &["cat-file", "-e", &format!("{}^{{commit}}", c.merge)],
        )?
        .status
        .success();
        if !exists {
            // git never prunes a commit the tip reaches, so a missing merge
            // was never published, unless the history is shallow: then it
            // may be behind the boundary, still open (Terra #19820).
            // Read from `<common>/shallow` itself: mdkg's git runner turns
            // the shallow file off (GIT_SHALLOW_FILE), so git won't say.
            let is_shallow = *shallow.get_or_insert_with(|| {
                std::fs::metadata(common.join("shallow")).is_ok_and(|m| m.len() > 0)
            });
            if is_shallow {
                return Err(Error::new(
                    "graph.git",
                    format!(
                        "the merge {} of a recorded sync conflict on {} isn't in this clone, \
                         whose history is shallow, so whether it's still open is unknown; \
                         fetch the whole history (`git fetch --unshallow`) and try again",
                        short(&c.merge),
                        c.path
                    ),
                ));
            }
            continue;
        }
        if is_ancestor(common, &c.merge, tip)? {
            out.push(c);
        }
    }
    Ok(out)
}

/// Removes the one entry `mdkg graph resolve` settled: `path`'s conflict
/// in the merge `merge`. An earlier conflict on the same path, from
/// another merge, stays listed (Terra #17687); how many went.
/// The caller holds [`SyncLock`] from reading the entry to clearing it.
pub fn clear_conflict(
    common: &Path,
    held: &SyncLock,
    path: &str,
    merge: &str,
) -> Result<usize, Error> {
    clear_conflicts(common, held, &[(path.to_owned(), merge.to_owned())])
}

/// Removes every entry one `mdkg graph resolve` settled, each a `(path,
/// merge)`, in one durable write (Terra #19565): a crash or failed write
/// leaves all of them listed, so a retry selects the same entries, never
/// an older conflict on a path whose newest one is gone. How many went.
pub fn clear_conflicts(
    common: &Path,
    _held: &SyncLock,
    entries: &[(String, String)],
) -> Result<usize, Error> {
    let all = conflicts_file(common)?;
    let kept: Vec<Conflict> = all
        .iter()
        .filter(|c| {
            !entries
                .iter()
                .any(|(path, merge)| c.path == *path && c.merge == *merge)
        })
        .cloned()
        .collect();
    let gone = all.len() - kept.len();
    if gone > 0 {
        save_conflicts(common, &kept)?;
    }
    Ok(gone)
}

// --------------------------------------------------------------------- run

/// The `refused` code of the latch written before a push: a push whose
/// outcome no later write recorded.
pub const UNRECORDED: &str = "graph.push_unrecorded";

/// Who asked for a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    /// A person's `mdkg graph sync`: clears a hook's refusal. Like the
    /// launcher, it neither pulls nor pushes with no approval (DECISION 11).
    Person,
    /// The launcher's timers: only an approved, unchanged destination, and
    /// nothing after a hook refused a push.
    Launcher,
}

/// What a sync is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub who: Who,
    /// Push too, when the local graph is ahead.
    pub push: bool,
    /// A person accepts a rewritten remote (`--accept-rewrite`).
    pub accept_rewrite: bool,
}

/// A caller's replay of pending graph write intents ([`Guards::replay`]).
pub type Replay<'a> = dyn Fn(&RefWriter, &GraphLock) -> Result<(), Error> + 'a;

/// What a sync checks against beside the clone's own config.
pub struct Guards<'a> {
    /// Protected branch names (short): the definitions' `protect` lists and
    /// GitHub's protection cache, as the hooks read them.
    pub protected: &'a [String],
    /// The secret scanner; `None` refuses every push.
    pub scanner: Option<&'a dyn Scanner>,
    /// The consumers' agent definition check, for strict validation.
    pub definitions: &'a DefinitionCheck<'a>,
    /// Replays pending graph write intents under the exclusive lock before
    /// sync moves the local graph (R13, Terra #18237); the caller asks its
    /// store what was recorded. `None` replays nothing.
    pub replay: Option<&'a Replay<'a>>,
}

impl std::fmt::Debug for Guards<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guards")
            .field("protected", &self.protected)
            .field("scanner", &self.scanner.is_some())
            .field("replay", &self.replay.is_some())
            .finish_non_exhaustive()
    }
}

/// What a sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub remote: String,
    pub remote_ref: String,
    pub approval: Approval,
    /// The local graph tip before and after.
    pub before: String,
    pub after: String,
    /// The remote tip after the sync (`None`: it has no graph ref).
    pub remote_tip: Option<String>,
    /// This sync pushed `after` to the remote.
    pub pushed: bool,
    /// The local graph has commits the remote hasn't (no push was made).
    pub ahead: bool,
    /// Paths a merge took the remote's version of.
    pub conflicts: Vec<Conflict>,
    /// The ref the local graph was kept under when a person accepted a
    /// rewrite (DECISION 9).
    pub backup: Option<String>,
}

fn is_ancestor(common: &Path, a: &str, b: &str) -> Result<bool, Error> {
    let out = git(common, &["merge-base", "--is-ancestor", a, b])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::new(
            "graph.git",
            format!(
                "can't compare {} and {}: {}",
                short(a),
                short(b),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )),
    }
}

fn rev(common: &Path, name: &str) -> Result<Option<String>, Error> {
    let out = git(
        common,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    )?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| hex(s)))
}

/// Records a failure in the state and returns it.
fn failed(common: &Path, state: &mut State, e: Error) -> Error {
    state.last_error = Some(Failure {
        code: e.code.to_owned(),
        message: e.message.clone(),
        time: now(),
        bin: Some(BIN.as_str().to_owned()),
    });
    state.attempts = state.attempts.saturating_add(1);
    if let Err(save) = state.save(common) {
        return Error::new(e.code, format!("{} (and sync.json: {save})", e.message));
    }
    e
}

/// One sync: pull, and with `options.push`, push (edd-01a18867, Sync, "The
/// algorithm"). `repo` is the checkout of the clone sync runs from
/// ([`net::checkout`]); `net` runs from the same one. The local graph moves
/// only by a compare-and-swap under the exclusive graph lock, which is
/// never held across the network.
pub fn run(
    repo: &Path,
    graph: &GraphRef,
    net: &dyn Net,
    guards: &Guards<'_>,
    options: Options,
) -> Result<Outcome, Error> {
    let common = graph.common.as_path();
    let remote = graph
        .remote
        .as_ref()
        .ok_or_else(|| Error::new("graph.remote_invalid", "no mdkg.graph.remote is set"))?;
    let _held = lock(common)?;
    let approval = approval(repo, graph)?;
    let base = net::checkout(common, repo)?;
    let mut state = State::load(common)?;
    match (&approval, options.who) {
        (Approval::Changed(why), _) => {
            return Err(Error::new(
                "graph.remote_changed",
                format!(
                    "graph remote {}'s destination changed since it was approved ({why}); a \
                     person checks it and runs `{BIN} graph remote approve` again",
                    remote.name
                ),
            ));
        }
        // Not a pull either, a person's included (DECISION 11,
        // 2026-11-04): what an unapproved remote holds (definitions,
        // protect lists) is authority an agent may have seeded.
        (Approval::Unapproved, _) => {
            return Err(Error::new(
                "graph.remote_unapproved",
                format!(
                    "graph remote {} was never approved, so nothing is pulled or pushed; a \
                     person runs `{BIN} graph remote approve`, which shows the destination",
                    remote.name
                ),
            ));
        }
        _ => {}
    }
    let pinned = remote.pinned.as_ref().map(key);
    if pinned.is_some() && state.destination != pinned {
        state = State {
            destination: pinned.clone(),
            ..State::default()
        };
    }
    if options.who == Who::Launcher
        && let Some(r) = &state.refused
    {
        let why = if r.code == UNRECORDED {
            r.shown()
        } else {
            format!("a hook refused the last graph push ({})", r.shown())
        };
        return Err(Error::new(
            "graph.push_refused",
            format!("{why}; background sync waits for a person's `{BIN} graph sync`"),
        ));
    }
    let may_push = options.push && approval == Approval::Approved;
    let main_ref = graph.main_ref.as_deref();
    guard::check_remote_ref(
        common,
        &remote.name,
        &remote.remote_ref,
        main_ref,
        guards.protected,
    )
    .map_err(|e| failed(common, &mut state, e))?;
    let mine = guard::destinations(common, &base, &remote.name)?;
    let is_code = guard::check_destination(common, &base, &remote.name, &mine, main_ref)
        .map_err(|e| failed(common, &mut state, e))?;
    let main_tip = match main_ref {
        Some(m) => rev(common, m)?,
        None => None,
    };
    let writer = graph.writer(repo, LockMode::Local)?;
    let folder = graph.folder("")?;
    let tracking = net::tracking_ref(&remote.name, &remote.remote_ref);
    let mut conflicts = Vec::new();
    // The tip accepted before a rewrite a person accepts (DECISION 9):
    // set once, kept across attempts, so a retry adopts the rewrite too.
    let mut rewritten: Option<String> = None;
    let mut backup: Option<String> = None;
    for attempt in 1..=ATTEMPTS {
        let r = match net.remote_tip(&remote.name, &remote.remote_ref) {
            Ok(Some(_)) => {
                // A remote ref that exists again ends the deleted state and
                // any approval to recreate it, so the rewrite guard applies
                // to it. Consumed durably as soon as the ref is seen, before
                // the fetch: a failed or interrupted fetch never leaves the
                // permission for a later deletion (Terra #19640). A write
                // that fails here leaves the old sync.json, permission
                // included (a Known limit: two faults at once).
                if state.missing || state.recreate {
                    state.missing = false;
                    state.recreate = false;
                    state.save(common)?;
                }
                Some(
                    net.fetch(&remote.name, &remote.remote_ref, &tracking)
                        .map_err(|e| failed(common, &mut state, e))?,
                )
            }
            Ok(None) => None,
            Err(e) => return Err(failed(common, &mut state, e)),
        };
        // A deleted remote graph ref can't be adopted, and pushing would
        // recreate it with all the history it removed (DECISION 17):
        // refused, --accept-rewrite and an adoption retry included (Terra
        // #19459), until a person approves the destination again.
        if r.is_none()
            && !state.recreate
            && let Some(accepted) = state.accepted.clone()
        {
            state.missing = true;
            let e = Error::new(
                "graph.remote_rewritten",
                format!(
                    "{} on {} was deleted after this clone accepted {}; to publish the \
                     local graph there again, a person runs `{BIN} graph remote approve`",
                    remote.remote_ref,
                    remote.name,
                    short(&accepted)
                ),
            );
            return Err(failed(common, &mut state, e));
        }
        // An adoption that already happened is read from history, never
        // from sync.json alone (DECISION 18 (R), Terra #19140). Every
        // rewrite is refused until adopted, and an adoption parents the
        // local graph on the remote's tip alone, so against a rewritten
        // remote the local graph took it in when it no longer holds the
        // accepted tip, or shares a commit with the remote that the
        // accepted history lacks; even if the state write after the move
        // was lost. Their merge base becomes the accepted tip, and the
        // attempt goes on as a plain sync from it.
        if let (Some(accepted), Some(r)) = (state.accepted.clone(), &r)
            && rewritten.is_none()
            && !is_ancestor(common, &accepted, r)?
            && let Some(l) = writer.tip()?
            && let Some(b) = merge_base(common, &l, r)?
            && (!is_ancestor(common, &accepted, &l)? || !is_ancestor(common, &b, &accepted)?)
        {
            state.accepted = Some(b);
            state.save(common)?;
        }
        // Once a person accepted this rewrite, later attempts adopt it;
        // the old accepted tip stays recorded until the local graph moves,
        // so a failed adoption still refuses the rewrite (Terra #18960).
        if let Some(accepted) = state.accepted.clone()
            && rewritten.is_none()
            && let Some(r) = &r
            && *r != accepted
            && !is_ancestor(common, &accepted, r)?
        {
            if options.accept_rewrite && options.who == Who::Person {
                rewritten = Some(accepted);
            } else {
                let e = Error::new(
                    "graph.remote_rewritten",
                    format!(
                        "{} on {} no longer descends from {}, the tip this clone last \
                         accepted; a person checks it and runs `{BIN} graph sync \
                         --accept-rewrite`",
                        remote.remote_ref,
                        remote.name,
                        short(&accepted)
                    ),
                );
                return Err(failed(common, &mut state, e));
            }
        }
        let l = writer.tip()?.ok_or_else(|| {
            Error::new("graph.ref_missing", format!("{} doesn't exist", graph.name))
        })?;
        // A remote ref sharing history with the code base isn't a graph
        // ref, unless it's the personal full-shape graph on the code's own
        // remote, cut from main (G20).
        if let (Some(r), Some(main)) = (&r, &main_tip) {
            let full = !guard::graph_only_tree(common, &l, &prefixes(repo, &l, &folder)?)?;
            if !(is_code && full) {
                guard::unrelated_to_code(common, r, main)
                    .map_err(|e| failed(common, &mut state, e))?;
            }
        }
        let graph_id = writer.graph_id_at(&l)?.unwrap_or_default();
        let validate =
            |commit: &str| validate_tip(repo, &folder, &graph_id, commit, guards.definitions);
        let adopt = rewritten.as_deref().zip(r.as_deref());
        let m = match &r {
            // A rewrite a person accepted (DECISION 9 (a),
            // 2026-11-04): the local graph is kept under a backup ref, and
            // the new tip is `r` with only the local changes since the old
            // accepted tip re-applied, parented on `r` alone, so nothing
            // the rewrite removed goes out again, even when `r` is an
            // ancestor of the local graph (a remote moved back).
            Some(r) if adopt.is_some() => {
                let (old, _) = adopt.unwrap_or_default();
                // Adoption leaves the merges that recorded conflicts, which
                // would hide them: they're settled first (Terra #18960).
                let open = self::conflicts(common, Some(&l))?;
                if !open.is_empty() {
                    let e = Error::new(
                        "graph.sync_conflict",
                        format!(
                            "{} unresolved sync conflict(s) would be hidden by adopting the \
                             rewrite; settle them with `{BIN} graph resolve` first",
                            open.len()
                        ),
                    );
                    return Err(failed(common, &mut state, e));
                }
                if backup.is_none() {
                    backup = Some(
                        keep_backup(common, &remote.name, &l)
                            .map_err(|e| failed(common, &mut state, e))?,
                    );
                }
                let base = git(common, &["merge-base", &l, old])?;
                let base = base
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&base.stdout).trim().to_owned());
                match base {
                    // No local change since the old accepted tip.
                    Some(b) if b == l => {
                        validate(r).map_err(|e| failed(common, &mut state, e))?;
                        r.clone()
                    }
                    Some(b) => {
                        let (m, found) = merge(
                            common,
                            &l,
                            r,
                            Some(&b),
                            Parents::RemoteOnly,
                            &remote.name,
                            &remote.remote_ref,
                            &validate,
                        )
                        .map_err(|e| failed(common, &mut state, e))?;
                        if !found.is_empty() {
                            record_conflicts(common, &found)?;
                        }
                        conflicts = found;
                        m
                    }
                    // Nothing shared with what was accepted: take `r`.
                    None => {
                        validate(r).map_err(|e| failed(common, &mut state, e))?;
                        r.clone()
                    }
                }
            }
            None => l.clone(),
            Some(r) if is_ancestor(common, r, &l)? => l.clone(),
            Some(r) if is_ancestor(common, &l, r)? => {
                validate(r).map_err(|e| failed(common, &mut state, e))?;
                r.clone()
            }
            Some(r) => {
                let (m, found) = merge(
                    common,
                    &l,
                    r,
                    None,
                    Parents::Both,
                    &remote.name,
                    &remote.remote_ref,
                    &validate,
                )
                .map_err(|e| failed(common, &mut state, e))?;
                if !found.is_empty() {
                    // Recorded, fsynced, before the local CAS (Terra major 14).
                    record_conflicts(common, &found)?;
                }
                conflicts = found;
                m
            }
        };
        // The remote tip this sync takes in is accepted durably before the
        // local graph moves (design review, Astra 3): a crash between the
        // two leaves acceptance ahead of the local graph, so a remote
        // reset behind it is still refused as a rewrite, never re-pushed.
        // An adopted rewrite is accepted only after the local graph moves:
        // accepted first, a crash before the move would leave the old
        // local history merged with `r` by the next sync, re-publishing
        // what the rewrite removed; this way the next sync sees the
        // rewrite again and a person's `--accept-rewrite` finds `r`
        // already in the local graph.
        let adopting = adopt.is_some();
        if !adopting
            && let Some(r) = &r
            && state.accepted.as_deref() != Some(r.as_str())
        {
            state.accepted = Some(r.clone());
            state.save(common)?;
        }
        if m != l {
            let lock = writer.lock(LockKind::Exclusive)?;
            // A replay that publishes moves the tip off `l`, so the
            // compare-and-swap below fails and the attempt starts again.
            if let Some(replay) = guards.replay {
                replay(&writer, &lock).map_err(|e| failed(common, &mut state, e))?;
            }
            let moved = match &backup {
                // Off the local history (DECISION 9): only with it kept.
                Some(kept) if adopting && !is_ancestor(common, &l, &m)? => {
                    if rev(common, kept)?.as_deref() != Some(l.as_str()) {
                        // The local graph moved since it was kept: keep it again.
                        backup = None;
                        drop(lock);
                        continue;
                    }
                    writer.replace(&lock, &l, &m, kept)?
                }
                _ => writer.advance(&lock, &l, &m)?,
            };
            if !moved {
                drop(lock);
                continue;
            }
        }
        if adopting && let Some(r) = &r {
            state.accepted = Some(r.clone());
            state.save(common)?;
            // Adopted: a retry after a stale push syncs from `r`, never
            // adopts again from the old accepted tip (Terra #19140).
            rewritten = None;
        }
        let done = |state: &mut State, pushed: bool, ahead: bool, remote_tip: Option<String>| {
            state.last_ok = Some(now());
            state.last_error = None;
            state.attempts = 0;
            if options.who == Who::Person && (pushed || !ahead || !may_push) {
                state.refused = None;
            }
            state.save(common)?;
            Ok(Outcome {
                remote: remote.name.clone(),
                remote_ref: remote.remote_ref.clone(),
                approval: approval.clone(),
                before: l.clone(),
                after: m.clone(),
                remote_tip,
                pushed,
                ahead,
                conflicts: conflicts.clone(),
                backup: backup.clone(),
            })
        };
        if r.as_deref() == Some(m.as_str()) {
            return done(&mut state, false, false, r);
        }
        if !may_push {
            return done(&mut state, false, true, r);
        }
        // The personal setup (the graph remote is the code's): what the
        // destination already holds is neither sent, sized nor scanned
        // again, so a first push doesn't walk all of the code's history
        // (design review, Opus 1). Only what the destination itself
        // reports counts (its branches, by `ls-remote`), never a tracking
        // ref, which may come from another URL or be stale (Terra #18297);
        // read on each attempt that pushes, so a retry never trusts heads
        // an earlier attempt saw (Terra #18368); and only commits this
        // clone has, checked in one batch.
        let held = if is_code {
            let heads = net
                .heads(&remote.name)
                .map_err(|e| failed(common, &mut state, e))?;
            guard::present(common, &heads)?
        } else {
            Vec::new()
        };
        let known: Vec<String> = r.iter().cloned().chain(held).collect();
        let outgoing = guard::outgoing(common, &m, &known)?;
        let checked = (|| {
            if !is_code {
                guard::graph_only(
                    common,
                    &outgoing,
                    &prefixes(repo, &m, &folder)?,
                    main_tip.as_deref(),
                )?;
            }
            guard::size(common, &m, &known)?;
            guard::scan(common, guards.scanner, &outgoing)
        })();
        checked.map_err(|e| failed(common, &mut state, e))?;
        // The latch, durable before the push starts (Terra #17687): a hook
        // refusal is recorded by the write after the push, and if that
        // write fails, this one still stops background sync until a
        // person's. A push whose outcome can't be written fails closed the
        // same way, a crash mid-push included.
        let prior = state.refused.replace(Failure {
            code: UNRECORDED.to_owned(),
            message: "a graph push started and its outcome wasn't recorded".to_owned(),
            time: now(),
            bin: Some(BIN.as_str().to_owned()),
        });
        if let Err(e) = state.save(common) {
            state.refused = prior;
            return Err(failed(common, &mut state, e));
        }
        let pushed = net.push(&remote.name, &m, &remote.remote_ref, r.as_deref());
        // Only a definite outcome lifts the latch: the remote took it, or
        // its lease didn't hold. A failed push may be a hook's refusal git
        // reported as `fatal:` (Terra #17986), so it stays latched, with
        // the failure as the reason, until a person syncs.
        match &pushed {
            Ok(Pushed::Accepted | Pushed::Stale) => state.refused = prior,
            Ok(Pushed::Refused(_)) => {}
            Err(e) => {
                state.refused = Some(Failure {
                    code: UNRECORDED.to_owned(),
                    message: format!(
                        "the last graph push failed ({}: {}), and whether a hook refused it is \
                         unknown",
                        e.code,
                        printable(&e.message)
                    ),
                    time: now(),
                    bin: Some(BIN.as_str().to_owned()),
                });
            }
        }
        match pushed {
            Ok(Pushed::Accepted) => {
                state.accepted = Some(m.clone());
                state.missing = false;
                state.recreate = false;
                return done(&mut state, true, false, Some(m.clone()));
            }
            Ok(Pushed::Stale) => {
                state.save(common)?;
                backoff(attempt);
            }
            Ok(Pushed::Refused(why)) => {
                let e = Error::new(
                    "graph.push_refused",
                    format!(
                        "a hook refused the graph push ({}); nothing retries around it, and \
                         background sync waits for a person's `{BIN} graph sync`",
                        printable(&why)
                    ),
                );
                state.refused = Some(Failure {
                    code: e.code.to_owned(),
                    message: printable(&why),
                    time: now(),
                    bin: Some(BIN.as_str().to_owned()),
                });
                return Err(failed(common, &mut state, e));
            }
            Err(e) => return Err(failed(common, &mut state, e)),
        }
    }
    let e = Error::new(
        "graph.sync_contended",
        format!(
            "the graph remote kept moving through {ATTEMPTS} attempts; local writes are \
             unaffected, and the next sync tries again"
        ),
    );
    Err(failed(common, &mut state, e))
}

/// Keeps `tip` under `refs/mdkg/backup/<remote>/<unix seconds>` (a
/// numeric suffix when that exists), before an accepted rewrite replaces
/// the local graph's history. Returns the ref.
fn keep_backup(common: &Path, remote: &str, tip: &str) -> Result<String, Error> {
    let stamp = now();
    for n in 0..100u32 {
        let name = if n == 0 {
            format!("refs/mdkg/backup/{remote}/{stamp}")
        } else {
            format!("refs/mdkg/backup/{remote}/{stamp}-{n}")
        };
        let zero = "0".repeat(tip.len());
        let out = mdkg_git::common_command(common)
            .args([
                "-c",
                "core.fsync=objects,reference",
                "update-ref",
                &name,
                tip,
                &zero,
            ])
            .run(None)
            .map_err(|e| Error::new("graph.git", format!("update-ref: {e}")))?;
        if out.status.success() {
            return Ok(name);
        }
        if rev(common, &name)?.is_none() {
            return Err(Error::new(
                "graph.git",
                format!(
                    "can't keep the local graph under {name}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
    }
    Err(Error::new(
        "graph.git",
        format!("can't name a backup ref for {remote}"),
    ))
}

/// 0.2 s · 2^attempt with jitter, at most 5 s.
fn backoff(attempt: u32) {
    let base = 200u64.saturating_mul(1 << attempt.min(5)).min(5_000);
    let jitter = u64::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos()),
    ) % (base / 2 + 1);
    std::thread::sleep(Duration::from_millis((base / 2 + jitter).min(5_000)));
}

/// The graph-owned folders at `commit`.
fn prefixes(repo: &Path, commit: &str, folder: &str) -> Result<Vec<String>, Error> {
    let authority = crate::graphref::config_at_dir(repo, commit, "", folder)?;
    let config = crate::config::from_snapshot(authority.config)
        .map_err(|e| Error::new("graph.authority_invalid", e.to_string()))?;
    crate::graphref::graph_paths_dir(&config, folder)
}

/// One `ls-tree` entry: `(mode, oid)`.
fn entry(common: &Path, commit: &str, path: &str) -> Result<Option<(String, String)>, Error> {
    let out = git_ok(
        common,
        &["ls-tree", "-z", "--full-tree", commit, "--", path],
    )?;
    for e in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let e = String::from_utf8_lossy(e);
        let Some((meta, name)) = e.split_once('\t') else {
            continue;
        };
        if name != path {
            continue;
        }
        let mut f = meta.split_whitespace();
        let (mode, kind, oid) = (f.next(), f.next(), f.next());
        if kind == Some("blob")
            && let (Some(mode), Some(oid)) = (mode, oid)
        {
            return Ok(Some((mode.to_owned(), oid.to_owned())));
        }
    }
    Ok(None)
}

fn paths(common: &Path, commit: &str) -> Result<std::collections::BTreeSet<String>, Error> {
    let out = git_ok(
        common,
        &["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// The paths `to` changed since `base`.
fn changed_since(
    common: &Path,
    base: &str,
    to: &str,
) -> Result<std::collections::BTreeSet<String>, Error> {
    {
        let out = git_ok(
            common,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-only",
                base,
                to,
            ],
        )?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }
}

/// How a merge's commit is parented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parents {
    /// `[l, r]`: an ordinary sync merge.
    Both,
    /// `[r]` alone: local changes re-applied onto a rewritten remote
    /// (DECISION 9), so none of `l`'s history goes out with them.
    RemoteOnly,
}

/// Merges `l` and `r` with no checkout (Sync, "The merge"):
/// `merge-tree --write-tree`; each conflicted path takes `r`'s version,
/// deletions included, and no path that neither side holds survives (a
/// directory/file conflict's renamed copy). The merge commit has parents
/// `[l, r]` (or `[r]` alone, [`Parents::RemoteOnly`]) and an
/// `Mdkg-Conflict` trailer per conflicted path. A merge that fails strict
/// validation takes `r`'s version of every path both sides changed; if
/// that fails too, it takes `r`'s version of every path `l` changed since
/// the base, which is `r`'s whole tree (DECISION 6, 2026-11-04: `r`
/// was strictly valid when published), recording each local version as a
/// conflict that `graph resolve` restores. `base` is the merge base, or
/// `None` for `l` and `r`'s own.
#[allow(clippy::too_many_arguments)]
fn merge(
    common: &Path,
    l: &str,
    r: &str,
    base: Option<&str>,
    parents: Parents,
    remote: &str,
    remote_ref: &str,
    validate: &dyn Fn(&str) -> Result<(), Error>,
) -> Result<(String, Vec<Conflict>), Error> {
    let base = match base {
        Some(b) => b.to_owned(),
        None => {
            let out = git(common, &["merge-base", l, r])?;
            if !out.status.success() {
                return Err(Error::new(
                    "graph.sync_invalid",
                    format!(
                        "the local graph and {remote_ref} on {remote} share no history; they \
                         aren't one graph's"
                    ),
                ));
            }
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
    };
    let out = git(
        common,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--merge-base",
            &base,
            l,
            r,
        ],
    )?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(Error::new(
            "graph.git",
            format!(
                "merge-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let mut fields = out.stdout.split(|b| *b == 0);
    let tree = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
    let mut conflicted: Vec<String> = Vec::new();
    if out.status.code() == Some(1) {
        for f in fields {
            if f.is_empty() {
                break;
            }
            let p = String::from_utf8_lossy(f).into_owned();
            if !conflicted.contains(&p) {
                conflicted.push(p);
            }
        }
    }
    // `from`: the merged tree, or (the third try) `r`'s own tree, so
    // nothing git carried across a rename survives (Terra #18960).
    let attempt = |from: &str, take_remote: &[String]| -> Result<(String, Vec<Conflict>), Error> {
        let tree = rebuild(common, from, l, r, take_remote)?;
        let mut message = match parents {
            Parents::Both => format!("mdkg: sync merge of {remote_ref} from {remote}\n"),
            Parents::RemoteOnly => format!(
                "mdkg: local graph changes re-applied onto rewritten {remote_ref} from {remote}\n"
            ),
        };
        let mut found = Vec::new();
        let mut trailers = String::new();
        for p in take_remote {
            let (local_entry, their_entry) = (entry(common, l, p)?, entry(common, r, p)?);
            // A mode-only change is a conflict too (Terra #18960 minor).
            if local_entry == their_entry {
                continue;
            }
            let local = local_entry.map(|e| e.1);
            let theirs = their_entry.map(|e| e.1);
            trailers.push_str(&format!(
                "Mdkg-Conflict: {} local={} remote={}\n",
                printable(p),
                local.as_deref().unwrap_or("none"),
                theirs.as_deref().unwrap_or("none")
            ));
            found.push((p.clone(), local, theirs));
        }
        if !trailers.is_empty() {
            message.push('\n');
            message.push_str(&trailers);
        }
        let mut args = vec!["-c", "core.fsync=objects,reference", "commit-tree", &tree];
        if parents == Parents::Both {
            args.extend(["-p", l]);
        }
        args.extend(["-p", r]);
        let commit = mdkg_git::common_command(common)
            .args(&args)
            .run(Some(message.as_bytes()))
            .map_err(|e| Error::new("graph.git", format!("commit-tree: {e}")))?;
        if !commit.status.success() {
            let err = String::from_utf8_lossy(&commit.stderr);
            let code = if err.contains("identity") || err.contains("user.email") {
                "graph.identity_unset"
            } else {
                "graph.git"
            };
            return Err(Error::new(code, format!("commit-tree: {}", err.trim())));
        }
        let commit = String::from_utf8_lossy(&commit.stdout).trim().to_owned();
        let time = now();
        let found = found
            .into_iter()
            .map(|(path, local, remote)| Conflict {
                merge: commit.clone(),
                path,
                local,
                remote,
                time,
            })
            .collect();
        Ok((commit, found))
    };
    let first = attempt(&tree, &conflicted)?;
    if validate(&first.0).is_ok() {
        return Ok(first);
    }
    let ours = changed_since(common, &base, l)?;
    let theirs = changed_since(common, &base, r)?;
    let mut wider: Vec<String> = ours.intersection(&theirs).cloned().collect();
    for p in &conflicted {
        if !wider.contains(p) {
            wider.push(p.clone());
        }
    }
    let second = attempt(&tree, &wider)?;
    if validate(&second.0).is_ok() {
        return Ok(second);
    }
    // The third try (DECISION 6): `r`'s version of everything `l` changed.
    // A rename on one side and an edit on the other can leave a duplicate
    // id no narrower take fixes.
    let mut all = wider;
    for p in ours {
        if !all.contains(&p) {
            all.push(p);
        }
    }
    let r_tree = git_ok(common, &["rev-parse", &format!("{r}^{{tree}}")])?;
    let r_tree = String::from_utf8_lossy(&r_tree).trim().to_owned();
    let third = attempt(&r_tree, &all)?;
    validate(&third.0)?;
    Ok(third)
}

/// `tree` with each of `take_remote` set to `r`'s version (or removed), and
/// every path neither `l` nor `r` holds removed.
fn rebuild(
    common: &Path,
    tree: &str,
    l: &str,
    r: &str,
    take_remote: &[String],
) -> Result<String, Error> {
    let dir = graph_dir(common)?;
    let index = dir.join(format!("sync-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&index);
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _gone = Gone(index.clone());
    let with_index = |args: &[&str], input: Option<&[u8]>| -> Result<Vec<u8>, Error> {
        let out = mdkg_git::common_command(common)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .run(input)
            .map_err(|e| Error::new("graph.git", format!("git {}: {e}", args[0])))?;
        if !out.status.success() {
            return Err(Error::new(
                "graph.git",
                format!(
                    "git {} failed: {}",
                    args[0],
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(out.stdout)
    };
    with_index(&["read-tree", tree], None)?;
    let held: std::collections::BTreeSet<String> = paths(common, l)?
        .union(&paths(common, r)?)
        .cloned()
        .collect();
    let zero = "0".repeat(tree.len());
    let mut info = Vec::new();
    for p in paths(common, tree)?.difference(&held) {
        info.extend_from_slice(format!("0 {zero}\t{p}").as_bytes());
        info.push(0);
    }
    for p in take_remote {
        match entry(common, r, p)? {
            Some((mode, oid)) => info.extend_from_slice(format!("{mode} {oid}\t{p}").as_bytes()),
            None => info.extend_from_slice(format!("0 {zero}\t{p}").as_bytes()),
        }
        info.push(0);
    }
    if !info.is_empty() {
        with_index(&["update-index", "-z", "--index-info"], Some(&info))?;
    }
    let out = with_index(&["write-tree"], None)?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

// ------------------------------------------------------------------- timer

/// What the launcher's sync timer asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Fetch and take the remote's changes.
    Pull,
    /// Fetch, merge if needed, and push.
    Push,
}

/// The first wait after a failed or refused sync; each further failure
/// doubles it, up to [`BACKOFF_MAX`].
pub const BACKOFF_MIN: Duration = Duration::from_secs(30);
/// The longest wait between failed or refused syncs.
pub const BACKOFF_MAX: Duration = Duration::from_secs(900);

/// The launcher's push and pull cadence (G13, option A;
/// DECISION 19 (T2)). One pull-due rule: a pull is due when the current
/// approved destination has never been pulled from successfully, or
/// `pull_interval` after its last successful pull (zero: only that first
/// one). A push is due `push_delay` after the last change of the local
/// tip, each change restarting the delay so one push carries them all; a
/// pull that leaves the graph ahead starts the delay too
/// ([`Timer::ahead`]). Only a success is recorded ([`Timer::succeeded`]);
/// a failure or refusal records nothing and only backs off
/// ([`Timer::failed`]), and a changed interval is applied to the recorded
/// last pull, never clearing it.
#[derive(Debug, Clone)]
pub struct Timer {
    push_delay: Duration,
    pull_interval: Duration,
    tip: Option<String>,
    push_at: Option<Instant>,
    /// The destination last pulled from successfully, and when.
    last_pull: Option<(Option<Destination>, Instant)>,
    /// No sync before this, after a failure or refusal.
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl Timer {
    /// A timer whose first [`Timer::due`] is a pull, at `now`.
    pub fn new(
        push_delay: Duration,
        pull_interval: Duration,
        _now: Instant,
        tip: Option<String>,
    ) -> Self {
        Self {
            push_delay,
            pull_interval,
            tip,
            push_at: None,
            last_pull: None,
            retry_at: None,
            backoff: Duration::ZERO,
        }
    }

    /// Whether a sync is due at `now`, given the local tip and the
    /// approved destination (`remote.json`'s pin; `None`: none) now. A
    /// changed tip restarts the push delay. A due push is taken: call
    /// [`Timer::succeeded`] or [`Timer::failed`] after the sync.
    pub fn due(
        &mut self,
        now: Instant,
        tip: Option<&str>,
        destination: Option<&Destination>,
    ) -> Option<Due> {
        if tip != self.tip.as_deref() {
            self.tip = tip.map(str::to_owned);
            self.push_at = Some(now + self.push_delay);
        }
        if self.retry_at.is_some_and(|t| now < t) {
            return None;
        }
        if self.push_at.is_some_and(|t| now >= t) {
            self.push_at = None;
            return Some(Due::Push);
        }
        let pull = match &self.last_pull {
            Some((pulled, at)) if pulled.as_ref() == destination => {
                !self.pull_interval.is_zero() && now >= *at + self.pull_interval
            }
            // Never pulled from this destination.
            _ => true,
        };
        pull.then_some(Due::Pull)
    }

    /// A sync `due` asked for succeeded at `now`, with `destination`
    /// approved: a push fetches first, so either counts as a pull.
    pub fn succeeded(&mut self, now: Instant, destination: Option<&Destination>) {
        self.last_pull = Some((destination.cloned(), now));
        self.retry_at = None;
        self.backoff = Duration::ZERO;
    }

    /// A sync `due` asked for failed or was refused at `now`: nothing is
    /// recorded, a push stays due, and no sync runs for the backoff.
    pub fn failed(&mut self, due: Due, now: Instant) {
        self.backoff = (self.backoff * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
        let retry = now + self.backoff;
        self.retry_at = Some(retry);
        if due == Due::Push {
            self.push_at.get_or_insert(retry);
        }
    }

    /// A sync left the local graph ahead of the remote: a push is due
    /// `push_delay` from `now`, unless one is already set.
    pub fn ahead(&mut self, now: Instant) {
        self.push_at.get_or_insert(now + self.push_delay);
    }

    /// The cadence as configured now (the launcher reads the settings on
    /// each tick). A changed pull interval applies from the last
    /// successful pull, which it never clears (zero: no pull after the
    /// first); a changed push delay reschedules a waiting push the new
    /// delay from `now`.
    pub fn set_cadence(&mut self, push_delay: Duration, pull_interval: Duration, now: Instant) {
        if push_delay != self.push_delay {
            self.push_delay = push_delay;
            if self.push_at.is_some() {
                self.push_at = Some(now + push_delay);
            }
        }
        self.pull_interval = pull_interval;
    }

    /// A sync moved the local tip to `tip` (a pull): not a local change, so
    /// it doesn't start the push delay.
    pub fn moved(&mut self, tip: Option<&str>) {
        self.tip = tip.map(str::to_owned);
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_MIN, Due, Failure, Timer};
    use crate::graphref::Destination;
    use std::time::{Duration, Instant};

    #[test]
    fn a_saved_failure_names_the_binary_running_now() {
        let running = mdkg_format::program::name();
        let f = |bin: Option<&str>, message: &str| Failure {
            code: "graph.remote_rewritten".into(),
            message: message.into(),
            time: 1,
            bin: bin.map(str::to_owned),
        };
        // A remote ref with a backtick before the suggestion, and a hook's
        // own command: only the writer's exact `<bin> graph ` changes.
        let saved = "refs/heads/graph`east moved; a person runs \
                     `mdkg-gone graph sync --accept-rewrite`; the hook said `other graph x`";
        assert_eq!(
            f(Some("mdkg-gone"), saved).shown(),
            format!(
                "refs/heads/graph`east moved; a person runs \
                 `{running} graph sync --accept-rewrite`; the hook said `other graph x`"
            )
        );
        // A suggestion other than `graph`: a replayed write that needs
        // `install` or `doctor --repair` (Terra #20181).
        assert_eq!(
            f(
                Some("mdkg-gone"),
                "run `mdkg-gone install` (or migrate), or `mdkg-gone doctor --repair`"
            )
            .shown(),
            format!("run `{running} install` (or migrate), or `{running} doctor --repair`")
        );
        // Written by the binary running now, or before `bin` was saved:
        // as written.
        assert_eq!(f(Some(running), saved).shown(), saved);
        assert_eq!(f(None, saved).shown(), saved);
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn dest(url: &str) -> Destination {
        Destination {
            remote: "graph".into(),
            remote_ref: "refs/mdkg/graph".into(),
            fetch: vec![url.into()],
            push: vec![url.into()],
            ..Default::default()
        }
    }

    /// Runs `due` and records a success for whatever it asked for.
    fn tick(t: &mut Timer, now: Instant, tip: &str, d: Option<&Destination>) -> Option<Due> {
        let due = t.due(now, Some(tip), d);
        if due.is_some() {
            t.succeeded(now, d);
        }
        due
    }

    /// Terra #19782: recording a conflict already recorded (a retry after
    /// a crash before the ref moved) persists no second entry.
    #[test]
    fn recording_a_conflict_twice_persists_one_entry() {
        let common = std::env::temp_dir().join(format!(
            "mdkg-record-twice-{}-{}",
            std::process::id(),
            mdkg_format::Uuid7::now()
        ));
        std::fs::create_dir_all(&common).expect("dir");
        let c = super::Conflict {
            merge: "a".repeat(40),
            path: ".mdkg/notes.txt".into(),
            local: None,
            remote: None,
            time: 1,
        };
        super::record_conflicts(&common, std::slice::from_ref(&c)).expect("first");
        super::record_conflicts(&common, std::slice::from_ref(&c)).expect("again");
        let raw: serde_json::Value = serde_json::from_slice(
            &std::fs::read(common.join("mdkg/graph/conflicts.json")).expect("read"),
        )
        .expect("json");
        assert_eq!(raw["conflicts"].as_array().map(Vec::len), Some(1), "{raw}");
        std::fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn the_timer_pulls_at_start_then_every_interval() {
        let t0 = Instant::now();
        let d = dest("a");
        let mut t = Timer::new(secs(120), secs(900), t0, Some("a".into()));
        assert_eq!(tick(&mut t, t0, "a", Some(&d)), Some(Due::Pull));
        assert_eq!(tick(&mut t, t0 + secs(899), "a", Some(&d)), None);
        assert_eq!(tick(&mut t, t0 + secs(900), "a", Some(&d)), Some(Due::Pull));
    }

    #[test]
    fn changes_are_coalesced_into_one_push_after_the_delay() {
        let t0 = Instant::now();
        let d = dest("a");
        let mut t = Timer::new(secs(120), secs(900), t0, Some("a".into()));
        assert_eq!(tick(&mut t, t0, "a", Some(&d)), Some(Due::Pull));
        assert_eq!(tick(&mut t, t0 + secs(10), "b", Some(&d)), None);
        assert_eq!(
            tick(&mut t, t0 + secs(100), "c", Some(&d)),
            None,
            "a change restarts the delay"
        );
        assert_eq!(tick(&mut t, t0 + secs(219), "c", Some(&d)), None);
        assert_eq!(tick(&mut t, t0 + secs(220), "c", Some(&d)), Some(Due::Push));
        assert_eq!(
            tick(&mut t, t0 + secs(400), "c", Some(&d)),
            None,
            "one push carries them all"
        );
    }

    #[test]
    fn a_changed_pull_interval_applies_from_the_last_pull() {
        let t0 = Instant::now();
        let d = dest("a");
        let mut t = Timer::new(secs(120), secs(900), t0, Some("a".into()));
        assert_eq!(tick(&mut t, t0, "a", Some(&d)), Some(Due::Pull));
        t.set_cadence(secs(120), secs(60), t0 + secs(10));
        assert_eq!(tick(&mut t, t0 + secs(59), "a", Some(&d)), None);
        assert_eq!(
            tick(&mut t, t0 + secs(60), "a", Some(&d)),
            Some(Due::Pull),
            "from the last pull, not from the change"
        );
        // Lengthening the interval doesn't push a due pull away, and
        // changing it back and forth never resets it (DECISION 19 (T2)).
        t.set_cadence(secs(120), secs(100), t0 + secs(150));
        t.set_cadence(secs(120), secs(90), t0 + secs(151));
        assert_eq!(tick(&mut t, t0 + secs(152), "a", Some(&d)), Some(Due::Pull));
        t.set_cadence(secs(120), Duration::ZERO, t0 + secs(160));
        assert_eq!(
            tick(&mut t, t0 + secs(5000), "a", Some(&d)),
            None,
            "zero: only the first pull"
        );
        // A shorter push delay applies to the next change, and to a push
        // already waiting.
        t.set_cadence(secs(30), Duration::ZERO, t0 + secs(5000));
        assert_eq!(tick(&mut t, t0 + secs(5001), "b", Some(&d)), None);
        assert_eq!(
            tick(&mut t, t0 + secs(5031), "b", Some(&d)),
            Some(Due::Push)
        );
        t.ahead(t0 + secs(6000));
        t.set_cadence(secs(5), Duration::ZERO, t0 + secs(6001));
        assert_eq!(
            tick(&mut t, t0 + secs(6006), "b", Some(&d)),
            Some(Due::Push)
        );
    }

    #[test]
    fn a_failed_or_refused_sync_records_nothing_and_backs_off() {
        let t0 = Instant::now();
        let d = dest("a");
        // A zero interval: the first pull is retried until one succeeds.
        let mut t = Timer::new(secs(120), Duration::ZERO, t0, Some("a".into()));
        assert_eq!(t.due(t0, Some("a"), Some(&d)), Some(Due::Pull));
        t.failed(Due::Pull, t0);
        assert_eq!(t.due(t0 + BACKOFF_MIN - secs(1), Some("a"), Some(&d)), None);
        assert_eq!(
            t.due(t0 + BACKOFF_MIN, Some("a"), Some(&d)),
            Some(Due::Pull)
        );
        t.failed(Due::Pull, t0 + BACKOFF_MIN);
        let second = t0 + BACKOFF_MIN * 3;
        assert_eq!(
            t.due(second - secs(1), Some("a"), Some(&d)),
            None,
            "the wait doubles"
        );
        assert_eq!(t.due(second, Some("a"), Some(&d)), Some(Due::Pull));
        t.succeeded(second, Some(&d));
        assert_eq!(t.due(second + secs(9999), Some("a"), Some(&d)), None);
        // A failed push stays due after the backoff, and isn't dropped.
        assert_eq!(t.due(second + secs(10000), Some("b"), Some(&d)), None);
        let push = second + secs(10120);
        assert_eq!(t.due(push, Some("b"), Some(&d)), Some(Due::Push));
        t.failed(Due::Push, push);
        assert_eq!(
            t.due(push + BACKOFF_MIN, Some("b"), Some(&d)),
            Some(Due::Push)
        );
    }

    #[test]
    fn a_newly_approved_destination_is_pulled_from_at_once() {
        let t0 = Instant::now();
        let (a, b) = (dest("a"), dest("b"));
        let mut t = Timer::new(secs(120), secs(900), t0, Some("a".into()));
        // Unapproved: refused, so nothing is recorded.
        assert_eq!(t.due(t0, Some("a"), None), Some(Due::Pull));
        t.failed(Due::Pull, t0);
        // Approved during the backoff: due once the backoff ends.
        assert_eq!(t.due(t0 + secs(1), Some("a"), Some(&a)), None);
        assert_eq!(
            tick(&mut t, t0 + BACKOFF_MIN, "a", Some(&a)),
            Some(Due::Pull)
        );
        assert_eq!(tick(&mut t, t0 + secs(100), "a", Some(&a)), None);
        // Another destination approved: its last pull is none.
        assert_eq!(tick(&mut t, t0 + secs(101), "a", Some(&b)), Some(Due::Pull));
        assert_eq!(tick(&mut t, t0 + secs(102), "a", Some(&b)), None);
    }

    #[test]
    fn a_pull_that_moves_the_tip_starts_no_push_and_ahead_does() {
        let t0 = Instant::now();
        let d = dest("a");
        let mut t = Timer::new(secs(120), secs(0), t0, Some("a".into()));
        assert_eq!(tick(&mut t, t0, "a", Some(&d)), Some(Due::Pull));
        t.moved(Some("r"));
        assert_eq!(
            tick(&mut t, t0 + secs(500), "r", Some(&d)),
            None,
            "a zero interval pulls only at start"
        );
        t.ahead(t0 + secs(500));
        assert_eq!(tick(&mut t, t0 + secs(620), "r", Some(&d)), Some(Due::Push));
    }
}
