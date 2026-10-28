//! What the launcher remembers per agent: its spec and its session. It
//! lives in the coordination plane (`<git common dir>/mdkg/run/`), shared by
//! every worktree and never committed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CODE_UNKNOWN_AGENT, RunError};
use crate::profile::{Harness, Role};

/// How an agent runs. Set on its first turn; later flags change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    /// The agent's actor id and mailbox.
    pub name: String,
    pub harness: Harness,
    pub role: Role,
    /// The folder the agent works in.
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort, checked against the selected harness's values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Cross-harness execution preset. This release supports auto only.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Harness-native overrides, replacing the preset where supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<String>,
    /// Recipient of deterministic turn receipts and decision questions.
    #[serde(default = "default_reports_to")]
    pub reports_to: String,
    /// Extra allowed commands for a Claude worker, in Claude's rule syntax
    /// (`Bash(npm test *)`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Branches besides `main` a worker may never move or commit to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protect: Vec<String>,
    /// For a reviewer: the commit or branch whose AGENTS.md gives the rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The worktree `cwd` is in and its git folder, pinned by the launcher
    /// the first time it runs the agent there (orchestrator, 2026-10-21).
    /// Every launcher git command for the agent names them, and a turn is
    /// refused when the checkout's `.git` no longer points there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<Pinned>,
}

fn default_mode() -> String {
    "auto".into()
}

fn default_reports_to() -> String {
    "orchestrator".into()
}

/// A worktree and its git folder, as the launcher found them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pinned {
    pub top: PathBuf,
    pub git_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub spec: AgentSpec,
    /// The harness's session id, once a turn has started one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Whether `session` was started in the user's own environment (mdkg
    /// 0.7.0), not in v0.1's private Codex or OpenCode folders, where the
    /// user's harness can't find it to resume. Unset, the agent's turn
    /// folder tells: only v0.1 made a private home there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_session: bool,
    /// Turns run so far.
    #[serde(default)]
    pub turns: u64,
    /// The highest mail sequence a turn has shown, so `--watch` isn't woken
    /// again by mail a failed turn left unacked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// Why the launcher refused the last commit for this worker, until one
    /// is made: the next prompt says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_refused: Option<String>,
}

/// Agent names become file names and lease scopes, so they stay plain.
pub fn check_name(name: &str) -> Result<(), RunError> {
    let ok = !name.is_empty()
        // The coordinator's `run:<name>` takes at most 120 (round 6).
        && name.len() <= 120
        && name
            .chars()
            // Lowercase: M1's `run:<name>` lease scope takes only lowercase.
            .all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ':')
            })
        && !name.starts_with('.')
        && !["human:", "role:", "run:"].iter().any(|p| {
            name.get(..p.len())
                .is_some_and(|n| n.eq_ignore_ascii_case(p))
        });
    if ok {
        Ok(())
    } else {
        // The same code as any other malformed name (a mailbox, a room, a
        // role): the orchestrator's D2, 2026-10-24. The rule goes in the
        // hint, and the name isn't quoted.
        Err(RunError::usage("msg.bad_id", "that isn't a valid agent name").with_hint(
            "an agent name is 1 to 120 of lowercase letters, digits, '-', '_', '.' and ':', and not human:*, role:* or run:*",
        ))
    }
}

/// The launcher's folder in the coordination plane.
pub fn run_dir(repo: &Path) -> Result<PathBuf, RunError> {
    let common = mdkg_coord_sqlite::git::common_dir(repo)
        .map_err(|e| RunError::io("can't find the git common dir", e))?;
    Ok(common.join("mdkg").join("run"))
}

pub(crate) fn file_name(name: &str) -> String {
    // ':' is legal in names (codex:pr19) but awkward in paths.
    name.replace(':', "%3A")
}

/// An install under way in a worktree (review of c99eb3b): written before
/// install looks for the agents working there, so an agent it couldn't
/// know of (one made, or moved there, meanwhile) still doesn't start a
/// turn in it.
///
/// Its install holds a lock on the file for as long as it runs, and the
/// system drops the lock with the process, Ctrl-C included: a marker whose
/// lock can be taken is dead, whatever its record says (review of
/// 08ff0f9, where a changed host name kept one alive for good). Past
/// `until_ms` it's dead too, should locks not work where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallMarker {
    pub record: LeaseRecord,
    /// The worktree's top, canonical.
    pub top: PathBuf,
    /// When it stops counting, in ms since the epoch.
    #[serde(default)]
    pub until_ms: u64,
}

/// How long a marker counts at most: a lease's two hours.
pub const MARKER_TTL_MS: u64 = 2 * 60 * 60 * 1000;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A marker this process holds: locked until dropped, then removed.
#[derive(Debug)]
pub struct HeldMarker {
    file: fs::File,
    pub path: PathBuf,
}

impl Drop for HeldMarker {
    fn drop(&mut self) {
        // Removed while still locked, so it's never seen unlocked and live.
        let _ = fs::remove_file(&self.path);
        let _ = self.file.unlock();
    }
}

/// Writes and locks this install's marker, after sweeping dead ones. It's
/// written under a name no reader looks at, locked, then renamed into
/// place, so every marker a reader finds is already locked while its
/// install runs.
pub fn mark_installing(dir: &Path, marker: &InstallMarker) -> std::io::Result<HeldMarker> {
    fs::create_dir_all(dir)?;
    sweep_markers(dir);
    let id = uuid::Uuid::now_v7().simple();
    let tmp = dir.join(format!(".install-{id}.tmp"));
    let path = dir.join(format!("install-{id}.installing"));
    let text = serde_json::to_vec(marker).map_err(std::io::Error::other)?;
    let held = (|| {
        fs::write(&tmp, &text)?;
        // Read and write: NFS emulates the lock with fcntl, whose exclusive
        // lock needs a writable file (review of 13b3e6e).
        let file = fs::OpenOptions::new().read(true).write(true).open(&tmp)?;
        match file.lock() {
            Ok(()) => {}
            // Where there are no locks, the deadline alone ends the marker.
            Err(e) if no_locks(&e) => {}
            Err(e) => return Err(e),
        }
        fs::rename(&tmp, &path)?;
        Ok(HeldMarker { file, path })
    })();
    if held.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    held
}

/// Whether a lock failed because the file system has none.
pub(crate) fn no_locks(e: &std::io::Error) -> bool {
    use rustix::io::Errno;
    e.kind() == std::io::ErrorKind::Unsupported
        || e.raw_os_error().is_some_and(|c| {
            [Errno::NOLCK, Errno::NOTSUP, Errno::OPNOTSUPP]
                .iter()
                .any(|n| n.raw_os_error() == c)
        })
}

/// Whether nobody holds `file`'s lock. Tested with a shared lock, so two
/// testers at once (a turn and a sweep) don't see each other as the
/// install (review of 13b3e6e). A lock that can't be tested counts as
/// held.
fn unlocked(file: &fs::File) -> bool {
    matches!(file.try_lock_shared(), Ok(()))
}

/// Whether the marker at `path` still counts: before its deadline, and its
/// lock held by its install.
fn marker_live(path: &Path, m: &InstallMarker) -> bool {
    if m.until_ms <= now_ms() {
        return false;
    }
    // Gone already: not live.
    fs::File::open(path).is_ok_and(|f| !unlocked(&f))
}

fn read_marker(path: &Path) -> Option<InstallMarker> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// Every marker file in the launcher's folder.
fn markers(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "installing"))
                .collect()
        })
        .unwrap_or_default()
}

/// How old a marker's temporary file must be before a sweep may take it:
/// one being written right now isn't locked yet.
const TMP_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Removes the markers of installs that are gone (or that can't be read),
/// and temporary ones they left half made (review of 13b3e6e).
pub fn sweep_markers(dir: &Path) {
    for path in markers(dir) {
        if !read_marker(&path).is_some_and(|m| marker_live(&path, &m)) {
            let _ = fs::remove_file(&path);
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(".install-") && name.ends_with(".tmp")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > TMP_GRACE);
        if old && fs::File::open(e.path()).is_ok_and(|f| unlocked(&f)) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Refuses a turn in `cwd` while a live install changes a worktree that
/// holds it, or that it holds.
pub fn not_installing(dir: &Path, cwd: &Path) -> Result<(), RunError> {
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for path in markers(dir) {
        let Some(m) = read_marker(&path) else {
            continue;
        };
        if (cwd.starts_with(&m.top) || m.top.starts_with(&cwd)) && marker_live(&path, &m) {
            return Err(RunError::new(
                crate::error::ErrorKind::Conflict,
                "coord.lease_held",
                format!(
                    "{} is installing in {}, where this agent works",
                    m.record.holder,
                    m.top.display()
                ),
            )
            .with_hint(format!(
                "{}; run the agent again once it's done (its marker is {}, and goes when that \
                 install ends)",
                crate::turn::INSTALLING,
                path.display()
            )));
        }
    }
    Ok(())
}

/// Who holds an agent's turn lease: the launcher process, so a later
/// launcher can tell a dead holder from a live one (orchestrator,
/// 2026-10-21). It lives in the launcher's folder, which no agent can write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub holder: String,
    pub host: String,
    pub pid: u32,
    /// The process's start time as `ps` prints it, so a reused pid isn't
    /// mistaken for the holder.
    pub started: String,
}

pub fn lease_record_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.lease", file_name(name)))
}

/// Serialize holder-record publication and conditional removal. The stable
/// lock is separate from the expiring coordinator lease: an old process may
/// resume after takeover, but cannot unlink a replacement's record.
pub fn lock_lease_record(dir: &Path, name: &str) -> Result<fs::File, RunError> {
    use std::os::unix::fs::OpenOptionsExt;
    check_name(name)?;
    fs::create_dir_all(dir).map_err(|e| RunError::io("can't create turn records", e))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(lease_record_path(dir, name).with_extension("lease-lock"))
        .map_err(|e| RunError::io("can't open turn record lock", e))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(RunError::failure(
            crate::error::CODE_IO,
            "turn record lock isn't a regular file",
        ));
    }
    file.lock()
        .map_err(|e| RunError::io("can't lock turn record", e))?;
    Ok(file)
}

/// Caller holds lock_lease_record through this comparison and removal.
pub fn remove_lease_record(dir: &Path, name: &str, holder: &str) {
    let path = lease_record_path(dir, name);
    let own = mdkg_coord_sqlite::git::read_capped(&path, 65536)
        .ok()
        .and_then(|b| serde_json::from_slice::<LeaseRecord>(&b).ok())
        .is_some_and(|r| r.holder == holder);
    if own {
        let _ = fs::remove_file(path);
    }
}

pub fn state_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.json", file_name(name)))
}

/// The folder for one agent's turn files.
pub fn turns_dir(dir: &Path, name: &str) -> PathBuf {
    dir.join(file_name(name))
}

pub fn load(dir: &Path, name: &str) -> Result<Option<AgentState>, RunError> {
    let path = state_path(dir, name);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            RunError::failure(
                crate::error::CODE_IO,
                format!("{} isn't a launcher state file: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RunError::io("can't read the agent's state", e)),
    }
}

/// Writes the state atomically, so a crash never leaves half a file.
pub fn save(dir: &Path, state: &AgentState) -> Result<(), RunError> {
    fs::create_dir_all(dir).map_err(|e| RunError::io("can't create the launcher folder", e))?;
    let path = state_path(dir, &state.spec.name);
    let tmp = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(state)
        .map_err(|e| RunError::failure(crate::error::CODE_IO, e.to_string()))?;
    text.push('\n');
    fs::write(&tmp, text).map_err(|e| RunError::io("can't write the agent's state", e))?;
    fs::rename(&tmp, &path).map_err(|e| RunError::io("can't write the agent's state", e))
}

/// Changes to an agent's spec from the command line.
#[derive(Debug, Clone, Default)]
pub struct SpecFlags {
    /// A complete committed definition, replacing optional fields too.
    pub definition: Option<AgentSpec>,
    pub harness: Option<Harness>,
    pub role: Option<Role>,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub sandbox: Option<String>,
    pub approvals: Option<String>,
    pub reports_to: Option<String>,
    pub allow: Vec<String>,
    pub protect: Vec<String>,
    pub base: Option<String>,
    /// Start a new session (`--fresh`).
    pub fresh: bool,
}

/// Claude Code's accepted `--effort` values.
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// Codex's accepted `model_reasoning_effort` values.
pub const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
/// Compatibility union for configuration whose harness is not resolved yet.
/// Once known, the harness must be checked with [`check_effort`].
pub const EFFORTS: &[&str] = CODEX_EFFORTS;

/// An effort the agent's harness takes. OpenCode takes none; its model's
/// variant sets it instead.
pub fn check_effort(harness: Harness, effort: Option<&str>) -> Result<(), RunError> {
    check_configured_effort(Some(harness), effort)
}

/// Validate a configuration field before its harness is necessarily known.
/// Reserved remote definitions may omit the harness; recognizing an effort
/// does not make those definitions executable.
pub fn check_configured_effort(
    harness: Option<Harness>,
    effort: Option<&str>,
) -> Result<(), RunError> {
    let Some(e) = effort else {
        return Ok(());
    };
    let accepted = match harness {
        Some(Harness::Claude) => CLAUDE_EFFORTS,
        Some(Harness::Codex) => CODEX_EFFORTS,
        None => EFFORTS,
        Some(Harness::OpenCode) => {
            return Err(RunError::usage(
                "usage.invalid_arguments",
                "OpenCode takes no effort setting",
            )
            .with_hint(format!(
                "name the model's variant with --model <provider>/<model>#<variant>; if there is no file definition, replace saved or inline settings with an effort-free definition: `{} agents add <name> --harness opencode --role <role>`; if .mdkg/agents/<name>.json exists, remove its effort field there, then commit that file in the graph checkout",
                mdkg_format::program::BIN
            )));
        }
    };
    if !accepted.contains(&e) {
        return Err(RunError::usage(
            "usage.invalid_arguments",
            format!("no effort {e:?}: it takes {}", accepted.join(", ")),
        ));
    }
    Ok(())
}

/// Apply flags to an agent's state. A new agent needs a harness and role.
/// Changing the harness or role starts a new session.
pub fn resolve(
    name: &str,
    existing: Option<AgentState>,
    flags: SpecFlags,
    default_cwd: &Path,
) -> Result<AgentState, RunError> {
    check_name(name)?;
    let definition = flags.definition.clone();
    let fresh = flags.fresh;
    // A move to OpenCode with no --effort drops the effort it had, which
    // OpenCode can't take; one asked for there is still refused (review
    // of 3d10456).
    let drop_effort = flags.harness == Some(Harness::OpenCode) && flags.effort.is_none();
    let mut state = resolve_spec(name, existing, flags, default_cwd)?;
    if let Some(spec) = definition {
        state = resolve_definition(state, spec, fresh)?;
    }
    if drop_effort {
        state.spec.effort = None;
    }
    check_execution(&state.spec)?;
    Ok(state)
}

/// Execution settings are checked for saved state as well as definitions.
pub(crate) fn check_execution(spec: &AgentSpec) -> Result<(), RunError> {
    check_effort(spec.harness, spec.effort.as_deref())?;
    check_native(
        spec.harness,
        spec.role,
        &spec.mode,
        spec.sandbox.as_deref(),
        spec.approvals.as_deref(),
    )?;
    check_name(&spec.reports_to)?;
    Ok(())
}

pub(crate) fn check_native(
    harness: Harness,
    role: Role,
    mode: &str,
    sandbox: Option<&str>,
    approvals: Option<&str>,
) -> Result<(), RunError> {
    if mode != "auto" {
        return Err(RunError::usage(
            "usage.invalid_arguments",
            "mode must be auto; other presets are not supported",
        ));
    }
    let error = |message| RunError::usage("usage.invalid_arguments", message);
    if role == Role::Reviewer && (sandbox.is_some() || approvals.is_some()) {
        return Err(error(
            "reviewers are read-only; sandbox and approvals overrides are not allowed",
        ));
    }
    if harness == Harness::Claude && sandbox.is_some() {
        return Err(error(
            "Claude has no native sandbox string override; use approvals for its permission mode",
        ));
    }
    if harness == Harness::OpenCode && (sandbox.is_some() || approvals.is_some()) {
        return Err(error(
            "OpenCode sandbox and approvals overrides are not supported; its user defaults apply",
        ));
    }
    Ok(())
}

/// Apply a complete committed definition to the latest machine-local state.
/// The caller holds the turn lease. Optional fields are replaced, including
/// fields the definition removed; session and progress remain machine-local.
pub fn resolve_definition(
    mut existing: AgentState,
    mut spec: AgentSpec,
    fresh: bool,
) -> Result<AgentState, RunError> {
    check_name(&spec.name)?;
    if spec.name != existing.spec.name {
        return Err(RunError::usage(
            CODE_UNKNOWN_AGENT,
            "the definition names another agent",
        ));
    }
    check_execution(&spec)?;
    let moved = spec.cwd != existing.spec.cwd;
    if fresh
        || moved
        || spec.harness != existing.spec.harness
        || spec.role != existing.spec.role
        || spec.mode != existing.spec.mode
        || spec.sandbox != existing.spec.sandbox
        || spec.approvals != existing.spec.approvals
    {
        existing.session = None;
        existing.user_session = false;
        existing.commit_refused = None;
    }
    spec.pinned = if moved {
        None
    } else {
        existing.spec.pinned.clone()
    };
    existing.spec = spec;
    Ok(existing)
}

fn resolve_spec(
    name: &str,
    existing: Option<AgentState>,
    flags: SpecFlags,
    default_cwd: &Path,
) -> Result<AgentState, RunError> {
    match existing {
        None => {
            let (Some(harness), Some(role)) = (flags.harness, flags.role) else {
                return Err(RunError::usage(
                    CODE_UNKNOWN_AGENT,
                    format!("no agent named {name} yet"),
                )
                .with_hint(
                    "start it with --harness claude|codex|opencode and --role worker|reviewer",
                ));
            };
            Ok(AgentState {
                spec: AgentSpec {
                    name: name.to_string(),
                    harness,
                    role,
                    cwd: flags.cwd.unwrap_or_else(|| default_cwd.to_path_buf()),
                    model: flags.model,
                    effort: flags.effort,
                    mode: flags.mode.unwrap_or_else(default_mode),
                    sandbox: flags.sandbox,
                    approvals: flags.approvals,
                    reports_to: flags.reports_to.unwrap_or_else(default_reports_to),
                    allow: flags.allow,
                    protect: flags.protect,
                    base: flags.base,
                    pinned: None,
                },
                session: None,
                user_session: false,
                turns: 0,
                cursor: None,
                commit_refused: None,
            })
        }
        Some(mut state) => {
            if let Some(h) = flags.harness
                && h != state.spec.harness
            {
                state.spec.harness = h;
                state.session = None;
                state.commit_refused = None;
            }
            if let Some(r) = flags.role
                && r != state.spec.role
            {
                state.spec.role = r;
                state.session = None;
                state.commit_refused = None;
            }
            if let Some(c) = flags.cwd {
                // Re-pinned from the common dir alone, which is safe; a
                // worktree that was recreated then works again.
                state.spec.pinned = None;
                // A new folder: Claude keeps sessions per folder, so the old
                // one couldn't be resumed there, and an old refusal is about
                // another worktree.
                if c != state.spec.cwd {
                    state.session = None;
                    state.commit_refused = None;
                }
                state.spec.cwd = c;
            }
            if flags.model.is_some() {
                state.spec.model = flags.model;
            }
            if flags.effort.is_some() {
                state.spec.effort = flags.effort;
            }
            if let Some(mode) = flags.mode {
                if mode != state.spec.mode {
                    state.session = None;
                    state.user_session = false;
                    state.commit_refused = None;
                }
                state.spec.mode = mode;
            }
            if let Some(sandbox) = flags.sandbox {
                if state.spec.sandbox.as_ref() != Some(&sandbox) {
                    state.session = None;
                    state.user_session = false;
                    state.commit_refused = None;
                }
                state.spec.sandbox = Some(sandbox);
            }
            if let Some(approvals) = flags.approvals {
                if state.spec.approvals.as_ref() != Some(&approvals) {
                    state.session = None;
                    state.user_session = false;
                    state.commit_refused = None;
                }
                state.spec.approvals = Some(approvals);
            }
            if let Some(reports_to) = flags.reports_to {
                state.spec.reports_to = reports_to;
            }
            if !flags.allow.is_empty() {
                state.spec.allow = flags.allow;
            }
            if !flags.protect.is_empty() {
                state.spec.protect = flags.protect;
            }
            if flags.base.is_some() {
                state.spec.base = flags.base;
            }
            if flags.fresh {
                state.session = None;
                state.user_session = false;
            }
            Ok(state)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_settings_round_trip_and_legacy_state_gets_auto_defaults() {
        let state = resolve(
            "worker",
            None,
            SpecFlags {
                harness: Some(Harness::Codex),
                role: Some(Role::Worker),
                sandbox: Some("read-only".into()),
                approvals: Some("never".into()),
                reports_to: Some("lead".into()),
                ..Default::default()
            },
            Path::new("/repo/worker"),
        )
        .unwrap();
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            serde_json::from_value::<AgentState>(json.clone()).unwrap(),
            state
        );
        let mut old = json;
        for key in ["mode", "sandbox", "approvals", "reports_to"] {
            old["spec"].as_object_mut().unwrap().remove(key);
        }
        let old: AgentState = serde_json::from_value(old).unwrap();
        assert_eq!(old.spec.mode, "auto");
        assert_eq!(old.spec.reports_to, "orchestrator");
        assert!(old.spec.sandbox.is_none() && old.spec.approvals.is_none());
    }

    #[test]
    fn native_definition_replacement_clears_overrides_and_permission_sessions() {
        let mut state = resolve(
            "worker",
            None,
            SpecFlags {
                harness: Some(Harness::Codex),
                role: Some(Role::Worker),
                sandbox: Some("read-only".into()),
                approvals: Some("never".into()),
                ..Default::default()
            },
            Path::new("/repo/worker"),
        )
        .unwrap();
        state.session = Some("old".into());
        state.turns = 3;
        state.cursor = Some(9);
        let mut spec = state.spec.clone();
        spec.reports_to = "lead".into();
        let retargeted = resolve_definition(state.clone(), spec.clone(), false).unwrap();
        assert_eq!(retargeted.session.as_deref(), Some("old"));
        for flags in [
            SpecFlags {
                sandbox: Some("workspace-write".into()),
                ..Default::default()
            },
            SpecFlags {
                approvals: Some("on-request".into()),
                ..Default::default()
            },
        ] {
            let changed = resolve(
                "worker",
                Some(state.clone()),
                flags,
                Path::new("/repo/worker"),
            )
            .unwrap();
            assert!(changed.session.is_none());
            assert_eq!((changed.turns, changed.cursor), (3, Some(9)));
        }
        spec.sandbox = None;
        spec.approvals = None;
        let cleared = resolve_definition(state, spec, false).unwrap();
        assert!(cleared.session.is_none());
        assert!(cleared.spec.sandbox.is_none() && cleared.spec.approvals.is_none());
        assert_eq!((cleared.turns, cleared.cursor), (3, Some(9)));
    }

    #[test]
    fn unsupported_native_settings_are_refused_in_saved_state_and_definitions() {
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for (sandbox, approvals) in [(Some("read-only"), None), (None, Some("never"))] {
                assert!(check_native(harness, Role::Reviewer, "auto", sandbox, approvals).is_err());
                let mut saved = resolve(
                    "reader",
                    None,
                    SpecFlags {
                        harness: Some(harness),
                        role: Some(Role::Reviewer),
                        ..Default::default()
                    },
                    Path::new("/repo"),
                )
                .unwrap();
                saved.spec.sandbox = sandbox.map(String::from);
                saved.spec.approvals = approvals.map(String::from);
                let saved: AgentState =
                    serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
                assert!(
                    resolve(
                        "reader",
                        Some(saved),
                        SpecFlags::default(),
                        Path::new("/repo")
                    )
                    .is_err()
                );
                assert!(crate::orchestration::agent::Definition::parse("reader", serde_json::json!({"schema":"mdkg.agent.v1", "harness":harness, "role":"reviewer", "sandbox":sandbox, "approvals":approvals})).is_err());
            }
        }
        assert!(
            check_native(
                Harness::Claude,
                Role::Worker,
                "auto",
                Some("read-only"),
                None
            )
            .is_err()
        );
        assert!(
            check_native(Harness::OpenCode, Role::Worker, "auto", None, Some("allow")).is_err()
        );
        assert!(check_native(Harness::Codex, Role::Worker, "other", None, None).is_err());
    }

    #[test]
    fn a_complete_definition_clears_removed_fields_without_losing_local_progress() {
        let mut original = resolve(
            "writer",
            None,
            SpecFlags {
                harness: Some(Harness::Claude),
                role: Some(Role::Worker),
                model: Some("old".into()),
                effort: Some("high".into()),
                allow: vec!["Bash(npm test *)".into()],
                base: Some("main".into()),
                ..Default::default()
            },
            Path::new("/repo/worker"),
        )
        .unwrap();
        original.session = Some("session".into());
        original.user_session = true;
        original.turns = 7;
        original.cursor = Some(91);
        let mut spec = original.spec.clone();
        spec.model = None;
        spec.effort = None;
        spec.allow.clear();
        spec.base = None;
        let updated = resolve(
            "writer",
            Some(original.clone()),
            SpecFlags {
                definition: Some(spec.clone()),
                ..Default::default()
            },
            Path::new("/unused"),
        )
        .unwrap();
        assert_eq!(updated.spec, spec);
        assert_eq!(
            (updated.turns, updated.cursor, updated.session.as_deref()),
            (7, Some(91), Some("session"))
        );
        for change in ["role", "harness", "cwd", "fresh"] {
            let mut target = spec.clone();
            match change {
                "role" => target.role = Role::Reviewer,
                "harness" => target.harness = Harness::Codex,
                "cwd" => target.cwd = "/repo/other".into(),
                _ => {}
            }
            let updated = resolve_definition(original.clone(), target, change == "fresh").unwrap();
            assert!(updated.session.is_none(), "{change}");
            assert!(!updated.user_session, "{change}");
            assert_eq!((updated.turns, updated.cursor), (7, Some(91)));
        }
    }

    #[test]
    fn ultra_effort_is_codex_only_and_harness_changes_revalidate_it() {
        let cwd = Path::new("/work");
        let codex = resolve(
            "astra",
            None,
            SpecFlags {
                harness: Some(Harness::Codex),
                role: Some(Role::Worker),
                model: Some("gpt-6-astra".into()),
                effort: Some("ultra".into()),
                ..Default::default()
            },
            cwd,
        )
        .expect("Codex ultra");
        assert_eq!(codex.spec.effort.as_deref(), Some("ultra"));
        assert!(resolve("astra", Some(codex.clone()), SpecFlags::default(), cwd).is_ok());
        assert!(
            resolve(
                "astra",
                Some(codex.clone()),
                SpecFlags {
                    harness: Some(Harness::Claude),
                    ..Default::default()
                },
                cwd
            )
            .is_err()
        );
        let opencode = resolve(
            "astra",
            Some(codex.clone()),
            SpecFlags {
                harness: Some(Harness::OpenCode),
                ..Default::default()
            },
            cwd,
        )
        .expect("OpenCode clears inherited effort");
        assert_eq!(opencode.spec.effort, None);
        let mut replacement = codex.spec.clone();
        assert!(resolve_definition(codex.clone(), replacement.clone(), false).is_ok());
        replacement.harness = Harness::Claude;
        assert!(resolve_definition(codex, replacement, false).is_err());
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            assert!(check_effort(Harness::Claude, Some(effort)).is_ok());
            assert!(check_effort(Harness::Codex, Some(effort)).is_ok());
            assert!(check_effort(Harness::OpenCode, Some(effort)).is_err());
        }
        assert!(check_effort(Harness::Claude, Some("ultra")).is_err());
        assert!(check_effort(Harness::OpenCode, Some("ultra")).is_err());
        assert!(check_effort(Harness::Codex, Some("ULTRA")).is_err());
    }

    #[test]
    fn saved_effort_is_checked_on_resume_without_overrides_and_at_execution() {
        let cwd = Path::new("/work");
        for (harness, invalid) in [
            (Harness::Claude, "ultra"),
            (Harness::Codex, "unknown"),
            (Harness::OpenCode, "low"),
        ] {
            let mut saved = resolve(
                "saved",
                None,
                SpecFlags {
                    harness: Some(harness),
                    role: Some(Role::Worker),
                    ..Default::default()
                },
                cwd,
            )
            .expect("valid initial state");
            saved.spec.effort = Some(invalid.into());
            let error = resolve("saved", Some(saved.clone()), SpecFlags::default(), cwd)
                .expect_err("invalid saved effort must not resume");
            assert!(error.message.contains("effort"), "{error}");
            let error = check_execution(&saved.spec).expect_err("execution also validates effort");
            assert!(error.message.contains("effort"), "{error}");
        }
    }

    #[test]
    fn unresolved_efforts_cover_every_supported_harness_value() {
        for efforts in [CLAUDE_EFFORTS, CODEX_EFFORTS] {
            for effort in efforts {
                assert!(EFFORTS.contains(effort), "missing {effort}");
                assert!(check_configured_effort(None, Some(effort)).is_ok());
            }
        }
    }

    #[test]
    fn max_effort_reaches_claude_and_codex_and_opencode_takes_none() {
        let flags = |h: Harness, e: &str| SpecFlags {
            harness: Some(h),
            role: Some(Role::Worker),
            effort: Some(e.into()),
            ..SpecFlags::default()
        };
        let cwd = Path::new("/w");
        for h in [Harness::Claude, Harness::Codex] {
            let a = resolve("a", None, flags(h, "max"), cwd).expect("max");
            assert_eq!(a.spec.effort.as_deref(), Some("max"));
            assert!(resolve("a", None, flags(h, "huge"), cwd).is_err());
        }
        let e = resolve("a", None, flags(Harness::OpenCode, "low"), cwd).expect_err("opencode");
        assert!(
            e.message.contains("OpenCode takes no effort"),
            "{}",
            e.message
        );
        // Invalid saved settings are refused even without explicit overrides.
        let mut saved = resolve(
            "a",
            None,
            SpecFlags {
                harness: Some(Harness::OpenCode),
                role: Some(Role::Worker),
                ..SpecFlags::default()
            },
            cwd,
        )
        .expect("made");
        saved.spec.effort = Some("low".into());
        let error = resolve("a", Some(saved.clone()), SpecFlags::default(), cwd)
            .expect_err("saved OpenCode effort");
        let hint = error.hint.expect("recovery hint");
        assert!(hint.contains("--model <provider>/<model>#<variant>"));
        assert!(hint.contains("agents add <name> --harness opencode --role <role>"));
        assert!(hint.contains("replace saved or inline settings"));
        assert!(hint.contains(
            "remove its effort field there, then commit that file in the graph checkout"
        ));
        // Moving it to Codex checks the effort it has.
        let moved = SpecFlags {
            harness: Some(Harness::Codex),
            ..SpecFlags::default()
        };
        let codex = resolve("a", Some(saved), moved, cwd).expect("to codex");
        // A Codex agent with an effort moves to OpenCode, which drops it;
        // naming one for OpenCode is refused (review of 3d10456).
        let to_oc = |effort: Option<&str>| SpecFlags {
            harness: Some(Harness::OpenCode),
            effort: effort.map(Into::into),
            ..SpecFlags::default()
        };
        let oc = resolve("a", Some(codex.clone()), to_oc(None), cwd).expect("to opencode");
        assert_eq!(oc.spec.effort, None);
        assert!(resolve("a", Some(codex), to_oc(Some("high")), cwd).is_err());
    }

    #[test]
    fn a_live_install_in_a_worktree_holds_off_turns_there_and_a_dead_one_doesnt() {
        // Review of c99eb3b: an agent install didn't know of. Review of
        // 08ff0f9: a marker whose holder can't be proven gone (another host
        // name, Ctrl-C) blocked turns for good; the lock decides now.
        let dir = std::env::temp_dir().join(format!(
            "mdkg-marker-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let top = dir.join("wt");
        fs::create_dir_all(top.join("sub")).expect("mkdir");
        let top = fs::canonicalize(&top).expect("canonical");
        let marker = InstallMarker {
            record: crate::turn::lease_record("nick@install-1".into()),
            top: top.clone(),
            until_ms: now_ms() + MARKER_TTL_MS,
        };
        let held = mark_installing(&dir, &marker).expect("mark");
        let e = not_installing(&dir, &top.join("sub")).expect_err("installing");
        assert!(crate::turn::install_holds(&e), "{e:?}");
        assert!(
            e.hint.as_deref().is_some_and(|h| h.contains(".installing")),
            "{e:?}"
        );
        assert!(not_installing(&dir, &dir.join("elsewhere")).is_ok());
        // Another sweep leaves a live one alone.
        sweep_markers(&dir);
        assert!(held.path.exists());
        drop(held);
        assert!(not_installing(&dir, &top).is_ok());
        assert!(markers(&dir).is_empty(), "removed when its install ends");
        // Left by an install that's gone (no lock), with a live-looking
        // record from a host name that changed since: dead, and swept.
        for until_ms in [now_ms() + MARKER_TTL_MS, 0] {
            let stale = InstallMarker {
                record: LeaseRecord {
                    host: "old-name.local".into(),
                    ..marker.record.clone()
                },
                until_ms,
                ..marker.clone()
            };
            let path = dir.join("install-left.installing");
            fs::write(&path, serde_json::to_vec(&stale).expect("j")).expect("w");
            assert!(not_installing(&dir, &top).is_ok());
            sweep_markers(&dir);
            assert!(!path.exists());
        }
        // A locked one past its deadline counts for nothing either.
        let late = mark_installing(
            &dir,
            &InstallMarker {
                until_ms: 1,
                ..marker.clone()
            },
        )
        .expect("mark");
        assert!(not_installing(&dir, &top).is_ok());
        drop(late);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_marker_stays_dead_while_another_tester_looks_and_old_temporaries_go() {
        // Review of 13b3e6e: testing by taking the lock exclusively made a
        // second tester (or a sweep) look like the install.
        let dir = std::env::temp_dir().join(format!(
            "mdkg-marker-tester-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let top = dir.join("wt");
        fs::create_dir_all(&top).expect("mkdir");
        let top = fs::canonicalize(&top).expect("canonical");
        let marker = InstallMarker {
            record: crate::turn::lease_record("nick@install-1".into()),
            top: top.clone(),
            until_ms: now_ms() + MARKER_TTL_MS,
        };
        let path = dir.join("install-left.installing");
        fs::write(&path, serde_json::to_vec(&marker).expect("j")).expect("w");
        let other = fs::File::open(&path).expect("open");
        other.lock_shared().expect("another tester");
        assert!(not_installing(&dir, &top).is_ok());
        drop(other);
        // A temporary file half made long ago goes; a fresh one stays.
        let (old, fresh) = (dir.join(".install-old.tmp"), dir.join(".install-new.tmp"));
        for f in [&old, &fresh] {
            fs::write(f, "{}").expect("w");
        }
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .and_then(|f| f.set_modified(long_ago))
            .expect("mtime");
        sweep_markers(&dir);
        assert!(!old.exists() && fresh.exists() && !path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_holder_ps_cant_be_asked_about_is_never_gone() {
        // Review of 08ff0f9: a failed `ps` counted as a dead holder. A pid
        // past any system's limit makes ps fail with a message.
        let mut r = crate::turn::lease_record("launcher@run-x".into());
        assert!(!crate::turn::holder_gone(&r), "this process is alive");
        r.pid = u32::MAX;
        assert!(!crate::turn::holder_gone(&r));
        // One that ran and ended is gone.
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("true");
        child.wait().expect("wait");
        r.pid = child.id();
        assert!(crate::turn::holder_gone(&r));
    }

    fn flags(h: Option<Harness>, r: Option<Role>) -> SpecFlags {
        SpecFlags {
            harness: h,
            role: r,
            ..SpecFlags::default()
        }
    }

    #[test]
    fn a_new_agent_needs_a_harness_and_a_role() {
        let e = resolve(
            "a",
            None,
            flags(Some(Harness::Claude), None),
            Path::new("/w"),
        )
        .expect_err("needs a role");
        assert_eq!(e.code, CODE_UNKNOWN_AGENT);
        let s = resolve(
            "a",
            None,
            flags(Some(Harness::Codex), Some(Role::Reviewer)),
            Path::new("/w"),
        )
        .expect("created");
        assert_eq!(s.spec.cwd, PathBuf::from("/w"));
        assert_eq!(s.session, None);
    }

    #[test]
    fn switching_harness_or_role_drops_the_session() {
        let mut s = resolve(
            "a",
            None,
            flags(Some(Harness::Claude), Some(Role::Worker)),
            Path::new("/w"),
        )
        .expect("created");
        s.session = Some("x".into());
        let same = resolve(
            "a",
            Some(s.clone()),
            flags(Some(Harness::Claude), Some(Role::Worker)),
            Path::new("/"),
        )
        .expect("kept");
        assert_eq!(same.session.as_deref(), Some("x"));
        let role = resolve(
            "a",
            Some(s.clone()),
            flags(None, Some(Role::Reviewer)),
            Path::new("/"),
        )
        .expect("changed");
        assert_eq!((role.spec.role, role.session), (Role::Reviewer, None));
        let other = resolve(
            "a",
            Some(s),
            flags(Some(Harness::Codex), None),
            Path::new("/"),
        )
        .expect("switched");
        assert_eq!(other.session, None);
    }

    #[test]
    fn passing_a_folder_drops_the_pin_and_no_flags_keep_it() {
        let mut st = resolve(
            "w",
            None,
            flags(Some(Harness::Claude), Some(Role::Worker)),
            Path::new("/a"),
        )
        .expect("new");
        st.spec.pinned = Some(Pinned {
            top: "/a".into(),
            git_dir: "/r/.git/worktrees/a".into(),
        });
        let kept =
            resolve("w", Some(st.clone()), SpecFlags::default(), Path::new("/a")).expect("kept");
        assert!(kept.spec.pinned.is_some());
        let moved = SpecFlags {
            cwd: Some("/a".into()),
            ..SpecFlags::default()
        };
        let moved = resolve("w", Some(st), moved, Path::new("/a")).expect("moved");
        assert_eq!(moved.spec.pinned, None);
    }

    #[test]
    fn a_new_folder_drops_the_session_and_an_old_refusal() {
        let mut st = resolve(
            "w",
            None,
            flags(Some(Harness::Claude), Some(Role::Worker)),
            Path::new("/a"),
        )
        .expect("new");
        st.session = Some("s-1".into());
        st.commit_refused = Some("a .env changed".into());
        let same = SpecFlags {
            cwd: Some("/a".into()),
            ..SpecFlags::default()
        };
        let kept = resolve("w", Some(st.clone()), same, Path::new("/a")).expect("same");
        assert_eq!(kept.session.as_deref(), Some("s-1"));
        let moved = SpecFlags {
            cwd: Some("/b".into()),
            ..SpecFlags::default()
        };
        let moved = resolve("w", Some(st), moved, Path::new("/a")).expect("moved");
        assert_eq!((moved.session, moved.commit_refused), (None, None));
    }

    #[test]
    fn every_harness_takes_every_role() {
        for h in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for r in [Role::Worker, Role::Reviewer] {
                resolve("a", None, flags(Some(h), Some(r)), Path::new("/w")).expect("runs");
            }
        }
    }

    #[test]
    fn names_are_plain_and_never_human() {
        for bad in [
            "",
            "human:nick",
            "Role:x",
            "run:a",
            "a/b",
            "../x",
            ".hidden",
            "a b",
            "Lane",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        for good in ["go-lane-b", "codex:pr19-4226245", "a.b_c"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn state_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!("mdkg-run-state-{}", uuid::Uuid::now_v7()));
        let s = resolve(
            "codex:pr1",
            None,
            flags(Some(Harness::Codex), Some(Role::Reviewer)),
            Path::new("/w"),
        )
        .expect("created");
        save(&dir, &s).expect("saved");
        assert!(state_path(&dir, "codex:pr1").ends_with("codex%3Apr1.json"));
        assert_eq!(load(&dir, "codex:pr1").expect("read"), Some(s));
        assert_eq!(load(&dir, "missing").expect("read"), None);
        let _ = fs::remove_dir_all(dir);
    }
}
