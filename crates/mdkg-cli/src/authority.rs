//! The external lease authority (orchestrator, 2026-10-21): when the user
//! has configured one for a repository, file and node leases are held
//! there instead of the local store, so they can be held across machines.
//! `claim`, `release` and `renew` resolve locally and then ask it with
//! canonical scopes; `done` and edits ask it (`holds`) whether the actor
//! holds the node.
//! mdkg names no product; the authority is any command that speaks the
//! same JSON envelope.
//!
//! Where it's configured: never in the repository, whose committed config
//! anyone with a PR could change. The file is
//! `${XDG_CONFIG_HOME:-$HOME/.config}/mdkg/lease-authority/<key>.json`,
//! where `<key>` is the hex SHA-256 of the canonical git common dir (as
//! realpath(3) gives it: `git rev-parse --path-format=absolute
//! --git-common-dir`, then `realpath`), as UTF-8 with no trailing slash. It
//! holds `{"common_dir", "command": [argv...], "timeout_ms"}`, and the
//! command's first word must be an absolute path outside the repository.
//!
//! Trust model: the file, and the `mdkg/` and `lease-authority/` folders
//! above it, must be real (no symlinks), owned by the user running
//! mdkg and not group- or world-writable, and the file must name this
//! repository's common dir. That keeps out other users and sandboxed agents,
//! whose write roots don't include the user's config folder; it doesn't
//! stop an unsandboxed process running as the user, which could run any
//! command anyway. Which file is read depends on mdkg's environment
//! (`XDG_CONFIG_HOME`, `HOME`), so whatever starts mdkg for agents (an
//! MCP server, the launcher) must take that environment from the user, not
//! from an agent.
//!
//! The authority runs in its own process group, and the whole group is
//! killed at the timeout and as soon as it has answered: it must not leave
//! background work running. A Ctrl-C to mdkg doesn't reach that group
//! (a known limit): the timeout still bounds it.
//!
//! Roles and turns (`role:*`, `run:*`) are never delegated: mailboxes and
//! turns are per node and per store (orchestrator, 2026-10-21).
//!
//! It fails closed: a file that breaks a rule, a command that can't start,
//! times out, exits without a valid envelope or answers for another command
//! refuses with `coord.authority_failed`, and nothing is granted locally.
//! The authority may still have acted, so the refusal says so.

use std::fs;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mdkg_coord_sqlite::git;
use mdkg_format::{Code, Diagnostic, Digest, Severity};
use mdkg_graph::{ErrorKind, GraphError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::coord::Repo;
use crate::output::{self, Report};

/// The most of the authority's file or output read.
const MAX_FILE: u64 = 64 * 1024;
const MAX_OUTPUT: u64 = 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// A configured authority.
#[derive(Debug)]
pub(crate) struct Authority {
    command: Vec<String>,
    /// The program as checked (links resolved): what runs, so a link can't
    /// be swapped between the check and the start.
    program: PathBuf,
    timeout: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityFile {
    common_dir: String,
    command: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn failed(message: impl Into<String>) -> GraphError {
    GraphError::failure(Code::CoordAuthorityFailed, message)
}

/// The user's config folder: `$XDG_CONFIG_HOME` when it's an absolute
/// path, else the home folder's `.config` (`$HOME`, or the account's home
/// when `$HOME` is unset, so a process with a bare environment still finds
/// the same file).
fn config_home() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    xdg.or_else(|| {
        std::env::home_dir()
            .filter(|p| p.is_absolute())
            .map(|h| h.join(".config"))
    })
}

/// The authority file for the repository whose common dir is `common`.
fn file_for(home: &Path, common: &str) -> PathBuf {
    let key = Digest::of_bytes(common.as_bytes());
    home.join("mdkg")
        .join("lease-authority")
        .join(format!("{}.json", key.hex()))
}

/// The user id this process runs as: a new pipe is owned by it. std has no
/// `geteuid`, and this crate allows no `unsafe`.
fn current_uid() -> Result<u32, GraphError> {
    let (reader, _writer) = std::io::pipe().map_err(|e| {
        failed(format!(
            "can't check who owns the lease authority file: {e}"
        ))
    })?;
    fs::File::from(OwnedFd::from(reader))
        .metadata()
        .map(|m| m.uid())
        .map_err(|e| {
            failed(format!(
                "can't check who owns the lease authority file: {e}"
            ))
        })
}

/// Why `meta` can't be trusted, if it can't: a symlink, the wrong kind, not
/// the user's, or writable by others.
fn untrusted(meta: &fs::Metadata, dir: bool, uid: u32) -> Option<&'static str> {
    if meta.file_type().is_symlink() {
        Some("is a symlink")
    } else if dir && !meta.is_dir() {
        Some("isn't a folder")
    } else if !dir && !meta.is_file() {
        Some("isn't a regular file")
    } else if meta.uid() != uid {
        Some("isn't owned by you")
    } else if meta.mode() & 0o022 != 0 {
        Some("is writable by others")
    } else {
        None
    }
}

fn refused(path: &Path, dir: bool, reason: &str) -> GraphError {
    let what = if dir { "folder" } else { "file" };
    failed(format!(
        "the lease authority {what} {} {reason}, so it isn't used",
        path.display()
    ))
    .with_hint("only you may write the lease authority config: chmod go-w, no symlinks")
}

/// Checks one folder or file on the way, without following a link: `None`
/// when nothing is there (a dangling link is something, and refused).
fn checked(path: &Path, dir: bool, uid: u32) -> Result<Option<fs::Metadata>, GraphError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed(format!("can't read {}: {e}", path.display()))),
    };
    match untrusted(&meta, dir, uid) {
        Some(reason) => Err(refused(path, dir, reason)),
        None => Ok(Some(meta)),
    }
}

/// The repository's configured authority, if any. An error when one is
/// configured but can't be trusted or read: the caller refuses.
pub(crate) fn find(repo: &Repo) -> Result<Option<Authority>, GraphError> {
    let Some(home) = config_home() else {
        return Ok(None);
    };
    let common = git::common_dir(&repo.top)
        .map_err(|e| failed(format!("can't find the repository's git folder: {e}")))?;
    let common_text = common.to_string_lossy().into_owned();
    let path = file_for(&home, &common_text);
    let mdkg = home.join("mdkg");
    let folder = mdkg.join("lease-authority");
    // Each level, outermost first, without following links: nothing there
    // means no authority; anything there (a dangling link too) must pass.
    let absent = |p: &Path| matches!(fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    if absent(&mdkg) {
        return Ok(None);
    }
    let uid = current_uid()?;
    checked(&mdkg, true, uid)?;
    if absent(&folder) {
        return Ok(None);
    }
    checked(&folder, true, uid)?;
    let Some(before) = checked(&path, false, uid)? else {
        return Ok(None);
    };
    let file =
        fs::File::open(&path).map_err(|e| failed(format!("can't read {}: {e}", path.display())))?;
    // The file opened is the one checked, and still trusted: no swap in
    // between.
    let opened = file
        .metadata()
        .map_err(|e| failed(format!("can't read {}: {e}", path.display())))?;
    if opened.dev() != before.dev() || opened.ino() != before.ino() {
        return Err(failed(format!(
            "the lease authority file {} changed while it was read",
            path.display()
        )));
    }
    if let Some(reason) = untrusted(&opened, false, uid) {
        return Err(refused(&path, false, reason));
    }
    let mut text = String::new();
    file.take(MAX_FILE + 1)
        .read_to_string(&mut text)
        .map_err(|e| failed(format!("can't read {}: {e}", path.display())))?;
    if text.len() as u64 > MAX_FILE {
        return Err(failed(format!("{} is too large", path.display())));
    }
    let parsed: AuthorityFile = serde_json::from_str(&text).map_err(|e| {
        failed(format!(
            "{} isn't a lease authority file: {e}",
            path.display()
        ))
        .with_hint("it holds {\"common_dir\", \"command\": [argv...], \"timeout_ms\"}")
    })?;
    if parsed.common_dir != common_text {
        return Err(failed(format!(
            "{} names another repository's git folder",
            path.display()
        )));
    }
    if parsed.command.is_empty() || parsed.command.iter().any(String::is_empty) {
        return Err(failed(format!(
            "{} needs a command: a non-empty list of non-empty arguments",
            path.display()
        )));
    }
    // An absolute program outside the repository: never one found on a
    // PATH or under the graph, which an agent or a PR could plant.
    let program = Path::new(&parsed.command[0]);
    if !program.is_absolute() {
        return Err(failed(format!(
            "{}: the command must start with an absolute path, not `{}`",
            path.display(),
            parsed.command[0]
        ))
        .with_hint("write the program's full path (see `command -v <name>`)"));
    }
    let real = fs::canonicalize(program).map_err(|e| {
        failed(format!(
            "{}: can't find the command {}: {e}",
            path.display(),
            program.display()
        ))
    })?;
    // Inside the git folder or any worktree of the repository (a bare
    // repository's parent is fine).
    let mut inside = vec![common.clone()];
    inside.extend(
        git::worktrees(&repo.top)
            .unwrap_or_default()
            .into_iter()
            .map(|w| fs::canonicalize(&w).unwrap_or(w)),
    );
    inside.push(fs::canonicalize(&repo.top).unwrap_or_else(|_| repo.top.clone()));
    if inside.iter().any(|dir| real.starts_with(dir)) {
        return Err(failed(format!(
            "{}: the command {} is inside the repository",
            path.display(),
            real.display()
        ))
        .with_hint("install the authority outside the repository and its worktrees"));
    }
    let timeout_ms = parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(1..=600_000).contains(&timeout_ms) {
        return Err(failed(format!(
            "{}: timeout_ms must be 1 to 600000",
            path.display()
        )));
    }
    Ok(Some(Authority {
        command: parsed.command,
        program: real,
        timeout: Duration::from_millis(timeout_ms),
    }))
}

/// Kills the authority's whole process group: a wrapper's background job
/// mustn't finish a claim after mdkg gave up on it.
fn kill_group(pid: u32) {
    for kill in ["/bin/kill", "/usr/bin/kill"] {
        if !Path::new(kill).exists() {
            continue;
        }
        Command::new(kill)
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        return;
    }
}

impl Authority {
    /// The command's name, for messages.
    pub(crate) fn name(&self) -> &str {
        &self.command[0]
    }

    /// Runs `command <subcommand> <positionals> <option value>... --json`
    /// (no shell, stdin closed, its own process group, killed at the
    /// timeout and once it has answered) from the graph root and returns
    /// its envelope as this command's report. Anything but a valid envelope
    /// for `command` with a matching exit code refuses. A positional or an
    /// option's value starting with `-` is refused first: it could steer the
    /// authority.
    pub(crate) fn delegate<T: DeserializeOwned>(
        &self,
        root: &Path,
        command: &str,
        positionals: &[String],
        options: &[(&str, Option<String>)],
    ) -> Result<Report<T>, GraphError> {
        let name = self.name();
        let values = positionals
            .iter()
            .chain(options.iter().filter_map(|(_, v)| v.as_ref()));
        if let Some(a) = values.clone().find(|a| a.starts_with('-') || a.is_empty()) {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                format!("`{a}` can't be passed to the lease authority"),
            )
            .with_hint("a path or name starting with `-` can't go through a lease authority"));
        }
        let mut args: Vec<String> = positionals.to_vec();
        for (flag, value) in options {
            args.push((*flag).to_string());
            if let Some(v) = value {
                args.push(v.clone());
            }
        }
        let may_have_acted = "nothing was granted locally; the authority may have acted, so \
             check it there (and release what it granted)";
        let mut invocation = Command::new(&self.program);
        git::WorktreeTrust::current().apply_to(&mut invocation);
        let mut child = invocation
            .arg0(name)
            .args(&self.command[1..])
            .arg(command)
            .args(&args)
            .arg("--json")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| failed(format!("can't run the lease authority `{name}`: {e}")))?;
        let pid = child.id();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| failed("the lease authority's output can't be read"))?;
        let (send, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let read = (&mut stdout)
                .take(MAX_OUTPUT + 1)
                .read_to_end(&mut out)
                .map(|_| out);
            send.send(read).ok();
        });
        let deadline = Instant::now() + self.timeout;
        let timed_out = || {
            failed(format!(
                "the lease authority `{name}` didn't answer within {} ms",
                self.timeout.as_millis()
            ))
            .with_hint(may_have_acted)
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    kill_group(pid);
                    child.kill().ok();
                    child.wait().ok();
                    return Err(timed_out());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    kill_group(pid);
                    return Err(failed(format!("the lease authority `{name}` failed: {e}")));
                }
            }
        };
        // Something it left behind may still hold its output open: the
        // timeout covers reading too.
        let out = match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(read) => read
                .map_err(|e| failed(format!("the lease authority's output can't be read: {e}")))?,
            Err(_) => {
                kill_group(pid);
                return Err(timed_out());
            }
        };
        kill_group(pid);
        if out.len() as u64 > MAX_OUTPUT {
            return Err(
                failed(format!("the lease authority `{name}` said too much"))
                    .with_hint(may_have_acted),
            );
        }
        let code = status.code().ok_or_else(|| {
            failed(format!(
                "the lease authority `{name}` was killed by a signal"
            ))
            .with_hint(may_have_acted)
        })?;
        report_from(&out, code, command).map_err(|why| {
            failed(format!(
                "the lease authority `{name}` gave no valid {command} answer (exit {code}): {why}"
            ))
            .with_hint(may_have_acted)
        })
    }
}

/// Reads an authority's envelope back into a report.
fn report_from<T: DeserializeOwned>(
    out: &[u8],
    code: i32,
    command: &str,
) -> Result<Report<T>, String> {
    let envelope: Value = serde_json::from_slice(out).map_err(|e| format!("not JSON: {e}"))?;
    let schema = envelope.get("schema").and_then(Value::as_str);
    if schema != Some(output::schema_id(command).as_str()) {
        return Err(format!("schema is {schema:?}"));
    }
    let ok = envelope
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or("no `ok`")?;
    let failure = match code {
        0 => None,
        1 => Some(ErrorKind::Usage),
        2 => Some(ErrorKind::Validation),
        3 => Some(ErrorKind::NotFound),
        4 => Some(ErrorKind::Failure),
        5 => Some(ErrorKind::Conflict),
        6 => Some(ErrorKind::Timeout),
        7 => Some(ErrorKind::Ambiguous),
        _ => return Err("an exit code outside 0 to 7".into()),
    };
    if ok != failure.is_none() {
        return Err("`ok` disagrees with the exit code".into());
    }
    let diagnostics = envelope
        .get("diagnostics")
        .and_then(Value::as_array)
        .ok_or("no `diagnostics`")?
        .iter()
        .map(diagnostic_from)
        .collect::<Result<Vec<_>, _>>()?;
    let has_error = diagnostics.iter().any(|d| d.severity == Severity::Error);
    if has_error == ok {
        return Err("`ok` disagrees with the diagnostics".into());
    }
    let data = match envelope.get("data") {
        None | Some(Value::Null) if ok => return Err("no `data`".into()),
        None | Some(Value::Null) => None,
        Some(d) => match serde_json::from_value::<T>(d.clone()) {
            Ok(d) => Some(d),
            Err(e) if ok => return Err(format!("`data`: {e}")),
            Err(_) => None,
        },
    };
    Ok(Report {
        data,
        diagnostics,
        failure,
    })
}

/// One diagnostic of the authority's. A code this build doesn't know (a
/// newer authority's) comes through as `coord.authority_note` with the
/// original code in the message, at its own severity.
fn diagnostic_from(v: &Value) -> Result<Diagnostic, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let severity = match s("severity") {
        Some("error") => Severity::Error,
        Some("warning") => Severity::Warning,
        Some("info") => Severity::Info,
        other => return Err(format!("unknown severity {other:?}")),
    };
    let message = s("message").ok_or("a diagnostic without a message")?;
    let raw = s("code").ok_or("a diagnostic without a code")?;
    let mut d = match Code::parse(raw) {
        Some(code) => Diagnostic::new(code, severity, message),
        None => Diagnostic::new(
            Code::CoordAuthorityNote,
            severity,
            format!("{raw}: {message}"),
        ),
    };
    d.path = s("path").map(String::from);
    d.line = v
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|l| usize::try_from(l).ok());
    d.node = s("node").map(String::from);
    // A host's prose (an older build's named holders and scopes in it):
    // escape hidden characters before displaying it.
    d.message = crate::output::shown_path(&d.message);
    d.hint = s("hint").map(crate::output::shown_path);
    // Who holds a lease, as the host says: data, as it was there.
    d.holders = v
        .get("holders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    for r in v
        .get("related")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = r
            .get("path")
            .and_then(Value::as_str)
            .ok_or("a related location without a path")?;
        let line = r
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|l| usize::try_from(l).ok());
        d = d.also_at(path, line);
    }
    Ok(d)
}

/// A warning when the committed config names a lease authority: it's
/// ignored, since anyone with a PR could change it.
pub(crate) fn committed_key_warning(root: &Path) -> Option<Diagnostic> {
    let text = fs::read_to_string(root.join(mdkg_graph::config::CONFIG_PATH)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.pointer("/coordination/lease_authority")?;
    let place = config_home().map_or_else(
        || "the user's config folder".to_string(),
        |h| h.join("mdkg").join("lease-authority").display().to_string(),
    );
    Some(
        Diagnostic::warning(
            Code::CoordAuthorityIgnored,
            "coordination.lease_authority in the committed config is ignored",
        )
        .at(mdkg_graph::config::CONFIG_PATH, None)
        .with_hint(format!(
            "a lease authority is configured per user, in {place}/; remove the key"
        )),
    )
}
