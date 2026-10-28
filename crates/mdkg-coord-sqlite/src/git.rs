//! Reading git for settlement: what main holds, where a worktree is, and
//! checking a file out of another branch's commit.
//!
//! Leaves are repository-relative file paths, so a node file maps to a scope
//! one to one. Content hashes are `sha256:` over the file's bytes, the same
//! in a blob and in a worktree file when no git filters rewrite content.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read, Write as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use mdkg_coord::{Scope, Source};
use mdkg_format::Digest;

use crate::digest_cache::DigestCache;

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// The supported floor: global `--attr-source` arrived in Git 2.41.
pub const MIN_VERSION: (u64, u64, u64) = (2, 41, 0);

pub fn minimum_version() -> String {
    format!("{}.{}.{}", MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2)
}

pub fn upgrade_hint() -> String {
    format!(
        "install Git {} or later and put it first on PATH; on macOS use \
         `brew install git` (or `brew upgrade git`) and add `$(brew --prefix git)/bin` to PATH",
        minimum_version()
    )
}

/// Inherited variables that would point git somewhere else or change what
/// it reads or runs: cleared for every command (the MCP server and the
/// launcher run this code unsandboxed while agents edit the checkouts).
const CLEARED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_ATTR_SOURCE",
    "GIT_EXTERNAL_DIFF",
    "GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE",
    "GIT_DIFF_OPTS",
    "GIT_QUARANTINE_PATH",
    "GIT_EXEC_PATH",
];

/// How long one git command may run: a FIFO planted in the object store or
/// as HEAD must not hang the server. `$MDKG_GIT_TIMEOUT_MS` (tests).
fn time_limit() -> Duration {
    let ms = std::env::var("MDKG_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000);
    Duration::from_millis(ms)
}

/// The empty tree of each object format: attributes are read from it, so
/// no `.gitattributes` a checkout holds names a filter or merge driver.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// A worktree [`check_worktree`] trusted. Every later command for it runs
/// with these folders named outright, so nothing re-reads its `.git`, which
/// an agent could swap between commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The worktree's top folder.
    pub top: PathBuf,
    /// Its own git folder: the common dir, or `<common>/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// The repository's common git folder.
    pub common: PathBuf,
    empty_tree: &'static str,
    /// Hooks config defines (git 2.54 and later runs `hook.<name>.command`
    /// whatever `core.hooksPath` says): each is switched off.
    config_hooks: Vec<std::ffi::OsString>,
    /// Whether `$MDKG_TRUSTED_COMMON_DIR` pinned the repository (the
    /// launcher, the MCP server).
    pinned: bool,
}

/// Pins supplied by the caller. A shared daemon's environment is not the
/// trust context of an individual hook. An empty context discovers and checks
/// the real worktree; it does not skip the structural checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeTrust {
    pub common: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
}

thread_local! {
    // Synchronous request scope only: never inherited by another thread or
    // left installed after an unwind. No process environment is changed.
    static REQUEST_TRUST: std::cell::RefCell<Option<WorktreeTrust>> = const { std::cell::RefCell::new(None) };
}

impl WorktreeTrust {
    /// CLI/MCP default, unless this synchronous request supplied its context.
    pub fn current() -> Self {
        REQUEST_TRUST
            .with(|t| t.borrow().clone())
            .unwrap_or_else(|| Self {
                common: std::env::var_os("MDKG_TRUSTED_COMMON_DIR").map(PathBuf::from),
                worktree: std::env::var_os("MDKG_TRUSTED_WORKTREE").map(PathBuf::from),
            })
    }

    /// Carry the same context to a delegated mdkg/lease-authority process.
    /// In particular, remove stale daemon pins from a discovery request.
    pub fn apply_to(&self, command: &mut Command) {
        for (name, pin) in [
            ("MDKG_TRUSTED_COMMON_DIR", &self.common),
            ("MDKG_TRUSTED_WORKTREE", &self.worktree),
        ] {
            match pin {
                Some(pin) => {
                    command.env(name, pin);
                }
                None => {
                    command.env_remove(name);
                }
            }
        }
    }
}

/// Supply explicit trust for one synchronous operation and every nested
/// repository/store/Git read on this thread. This scope must surround use of
/// the repository, not merely its constructor. Async/spawned work must carry
/// a cloned context explicitly. Nested scopes restore their parent on exit.
pub fn with_worktree_trust<T>(trust: &WorktreeTrust, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<WorktreeTrust>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST_TRUST.with(|t| *t.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(REQUEST_TRUST.with(|t| t.replace(Some(trust.clone()))));
    f()
}

/// Checked folders and the worktree each is in, isolated by trust context.
static CHECKOUTS: Mutex<Vec<(PathBuf, WorktreeTrust, Checkout)>> = Mutex::new(Vec::new());

/// The trusted worktree `dir` is in, checking it the first time.
fn checkout_for(dir: &Path) -> io::Result<Checkout> {
    let trust = WorktreeTrust::current();
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let known = CHECKOUTS.lock().ok().and_then(|list| {
        list.iter()
            .find(|(d, t, _)| *d == real && *t == trust)
            .map(|(_, _, c)| c.clone())
    });
    match known {
        Some(checkout) => Ok(checkout),
        None => check_worktree_with_trust(dir, &trust),
    }
}

/// The git program, found on `$PATH` without its empty or relative entries
/// (which would find a `git` in whatever folder this runs in), and that
/// `$PATH` cleaned the same way for git's own children. The launcher's git
/// uses it too (audit, 2026-10-22).
pub fn git_program() -> &'static (PathBuf, std::ffi::OsString) {
    static GIT: OnceLock<(PathBuf, std::ffi::OsString)> = OnceLock::new();
    GIT.get_or_init(|| find_git(std::env::var_os("PATH").as_deref()))
}

/// [`git_program`] for the search path `path`.
pub fn find_git(path: Option<&std::ffi::OsStr>) -> (PathBuf, std::ffi::OsString) {
    let dirs: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let program = dirs
        .iter()
        .map(|d| d.join("git"))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        // Not found: spawning fails with "not found" as before.
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"));
    let path = std::env::join_paths(&dirs).unwrap_or_default();
    (program, path)
}

/// A git command in `dir` that no checkout can steer through hooks,
/// fsmonitor, replace objects, grafts, a shallow file, the commit-graph,
/// attributes, lazy fetches, a transport, or an inherited `GIT_*` location
/// (orchestrator, 2026-10-21, from the launcher's hardening). Not yet tied
/// to a worktree: only for finding and checking one.
fn hardened(dir: &Path) -> Command {
    let (program, path) = git_program();
    let mut c = Command::new(program);
    c.env("PATH", path);
    for var in CLEARED {
        c.env_remove(var);
    }
    // `/dev/null/none` can't be opened, which git reads as "none", with no
    // deprecation chatter (as `/dev/null` would print for grafts).
    c.env("GIT_GRAFT_FILE", "/dev/null/none")
        .env("GIT_SHALLOW_FILE", "/dev/null/none")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "none")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .arg("--no-replace-objects")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.commitGraph=false",
            "-c",
            "core.attributesFile=/dev/null",
        ])
        .arg("-C")
        .arg(dir)
        .stdin(Stdio::null())
        .process_group(0);
    c
}

/// A git command for the worktree `dir` is in, run in `dir`. Its only way
/// to run is [`Git::run`]: bounded in time, outputs drained, stdin closed
/// unless fed. The worktree is checked first ([`check_worktree`]) and then
/// named outright; a worktree that fails the check runs nothing.
///
/// Attributes come from the empty tree (`--attr-source`, git 2.41 or
/// later, which older git refuses rather than ignores), so no filter or
/// merge driver a checkout names ever runs; our commits ignore eol/text
/// attributes (orchestrator, 2026-10-21). The repository's own
/// `info/attributes` and config are trusted like the rest of its git
/// folder.
#[derive(Debug)]
pub struct Git {
    command: Command,
    refused: Option<io::Error>,
}

/// The events git runs hooks for: each is switched off by name, for git
/// that switches hooks off per event.
pub const HOOK_EVENTS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

/// A hardened git command in `dir`.
pub fn command(dir: &Path) -> Git {
    let mut command = hardened(dir);
    match checkout_for(dir) {
        Ok(checkout) => {
            command
                .env("GIT_COMMON_DIR", &checkout.common)
                .arg(format!("--attr-source={}", checkout.empty_tree));
            switch_off_hooks(&mut command, &checkout.config_hooks);
            command
                .arg("--git-dir")
                .arg(&checkout.git_dir)
                .arg("--work-tree")
                .arg(&checkout.top);
            Git {
                command,
                refused: None,
            }
        }
        Err(e) => Git {
            command,
            refused: Some(e),
        },
    }
}

/// Switches off the hooks config defines (`names`, from [`hook_names`]) and
/// every event's, for `cmd`. Through the environment, so no name (even one
/// holding `=`) escapes its key; every event too (git 2.55's switch).
pub fn switch_off_hooks(cmd: &mut Command, names: &[std::ffi::OsString]) {
    let names = names
        .iter()
        .map(std::ffi::OsString::as_os_str)
        .chain(HOOK_EVENTS.iter().map(std::ffi::OsStr::new));
    let mut count = 0;
    for name in names {
        let mut key = std::ffi::OsString::from("hook.");
        key.push(name);
        key.push(".enabled");
        cmd.env(format!("GIT_CONFIG_KEY_{count}"), key)
            .env(format!("GIT_CONFIG_VALUE_{count}"), "false");
        count += 1;
    }
    cmd.env("GIT_CONFIG_COUNT", count.to_string());
}

/// Signals git's whole process group (git and anything it started).
fn signal_group(pid: u32, signal: &str) {
    for kill in ["/bin/kill", "/usr/bin/kill"] {
        if Path::new(kill).exists() {
            Command::new(kill)
                .args([signal, "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok();
            return;
        }
    }
}

impl Git {
    pub fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command.args(args);
        self
    }

    /// Runs to the end within the time limit, feeding `input` to stdin
    /// when given and collecting both outputs on threads. At the limit, the
    /// whole group gets SIGTERM (so git removes its lock files), then
    /// SIGKILL.
    pub fn run(self, input: Option<&[u8]>) -> io::Result<std::process::Output> {
        if let Some(refused) = self.refused {
            return Err(refused);
        }
        run(self.command, input)
    }
}

fn read_output(pipe: Option<Box<dyn Read + Send>>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut out)?;
    }
    Ok(out)
}

/// Runs `cmd` as [`Git::run`] describes.
fn run(mut cmd: Command, input: Option<&[u8]>) -> io::Result<std::process::Output> {
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let pid = child.id();
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        }
        _ => None,
    };
    let collect = |pipe: Option<Box<dyn Read + Send>>| {
        let (send, recv) = mpsc::channel();
        std::thread::spawn(move || {
            send.send(read_output(pipe)).ok();
        });
        recv
    };
    let stdout = collect(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = collect(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + time_limit();
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("git didn't finish within {} ms", time_limit().as_millis()),
        )
    };
    let stop = |child: &mut std::process::Child| {
        signal_group(pid, "-TERM");
        let grace = Instant::now() + Duration::from_secs(2);
        while Instant::now() < grace {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        signal_group(pid, "-KILL");
        child.kill().ok();
        child.wait().ok();
    };
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                stop(&mut child);
                return Err(timed_out());
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    // Something git started may still hold its outputs open: reading
    // them is bounded by the same limit, with a moment's grace for a git
    // that finished right at it.
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1))
    };
    let (Ok(stdout), Ok(stderr)) = (
        stdout.recv_timeout(remaining()),
        stderr.recv_timeout(remaining()),
    ) else {
        signal_group(pid, "-KILL");
        return Err(timed_out());
    };
    // A writer cut short by git exiting early is told by git's own
    // status and output, not by the broken pipe.
    if let Some(w) = writer
        && let Ok(Err(e)) = w.join()
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(e);
    }
    Ok(std::process::Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// `O_NONBLOCK`, so opening a FIFO planted where a file belongs can't hang.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "mips", target_arch = "mips64")
))]
const O_NONBLOCK: i32 = 0o200;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "sparc", target_arch = "sparc64")
))]
const O_NONBLOCK: i32 = 0x4000;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const O_NONBLOCK: i32 = 0x0004;

/// A small regular file's text (the first 4 KiB), read without blocking on
/// a FIFO planted in its place.
pub fn read_small(path: &Path) -> io::Result<String> {
    // Strict: a path that isn't UTF-8 is an error, never a look-alike.
    String::from_utf8(read_capped(path, 4096)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A small file's text, decoded lossily, for ref names only: bytes that
/// aren't UTF-8 (git allows them in ref names) read as U+FFFD, which the
/// ASCII rule refuses.
fn read_ref_names(path: &Path, cap: u64) -> io::Result<String> {
    // One byte past the cap tells a file of exactly `cap` bytes from one
    // cut short, which is never read as the whole list.
    let bytes = read_capped(path, cap + 1)?;
    if bytes.len() as u64 > cap {
        return Err(failure(format!(
            "{} is too long to read whole",
            path.display()
        )));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Up to `cap` bytes of a regular file, without blocking on a FIFO.
pub fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure(format!("{} isn't a regular file", path.display())));
    }
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// What git makes of `dir`: its git folder, common dir and top folder
/// (canonical), and the object format. Also refuses git older than 2.41,
/// which has no `--attr-source`.
fn locate(dir: &Path) -> io::Result<(PathBuf, PathBuf, PathBuf, String)> {
    let mut c = hardened(dir);
    // Read lazily, so the SHA-1 tree passes in a SHA-256 repository here.
    c.arg(format!("--attr-source={EMPTY_TREE_SHA1}")).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
        "--show-toplevel",
        "--show-object-format",
    ]);
    let out = run(c, None)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if stderr.contains("--attr-source") {
            return Err(failure(format!(
                "mdkg needs Git {} or later (this Git has no --attr-source)",
                minimum_version()
            )));
        }
        return Err(failure(format!("git rev-parse: {}", stderr.trim())));
    }
    let text = String::from_utf8(out.stdout).map_err(|e| failure(e.to_string()))?;
    let fields: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(&text)
        .split('\n')
        .collect();
    let [git_dir, common, top, format] = fields[..] else {
        return Err(failure(format!(
            "{}: git's view of this folder can't be read (a newline in a path?)",
            dir.display()
        )));
    };
    Ok((
        fs::canonicalize(git_dir)?,
        fs::canonicalize(common)?,
        fs::canonicalize(top)?,
        format.to_string(),
    ))
}

/// Checks that `dir` is in a real worktree of its repository, so git can't
/// be steered elsewhere, and remembers it for every later [`command`]:
/// - seen from its top folder, the worktree is the same one (no
///   `core.worktree` lending another checkout);
/// - its git folder is the common dir, reached through a `.git` folder (or,
///   for a submodule or a separate git dir, a `.git` file), or
///   `<common>/worktrees/<name>`, reached through a plain `.git` file that
///   its own `gitdir` file names back.
///
/// With `$MDKG_TRUSTED_COMMON_DIR` set (an absolute path, for the launcher
/// and the MCP server to set from their own root), the common dir must be
/// that one, a `.git` link is refused, and a `.git` file reaching the
/// common dir needs the common dir's `core.worktree` to name this top.
pub fn check_worktree(dir: &Path) -> io::Result<Checkout> {
    check_worktree_with_trust(dir, &WorktreeTrust::current())
}

/// Check using exactly these pins, without reading daemon environment.
pub fn check_worktree_with_trust(dir: &Path, trust: &WorktreeTrust) -> io::Result<Checkout> {
    let refuse = |why: String| failure(format!("{}: {why}", dir.display()));
    let (git_dir, common, top, format) = locate(dir)?;
    if fs::canonicalize(dir).ok().as_ref() != Some(&top) {
        let (again, _, again_top, _) = locate(&top)?;
        if again != git_dir || again_top != top {
            return Err(refuse(format!(
                "git reads it as part of {} through {}, which that folder's own .git doesn't name",
                top.display(),
                git_dir.display()
            )));
        }
    }
    let common_pinned = match &trust.common {
        None => false,
        Some(pin) => {
            let pin = pin.clone();
            if !pin.is_absolute() {
                return Err(refuse(
                    "$MDKG_TRUSTED_COMMON_DIR must be an absolute path".into(),
                ));
            }
            let pin = fs::canonicalize(&pin).unwrap_or(pin);
            if pin != common {
                return Err(refuse(format!(
                    "its git folder {} isn't the trusted one {}",
                    common.display(),
                    pin.display()
                )));
            }
            true
        }
    };
    // The launcher and the MCP server may pin the worktree itself: a
    // folder whose `.git` was removed can't then fall through to another.
    let worktree_pinned = &trust.worktree;
    // Either pin makes the stricter rules apply (a worktree pinned alone
    // still refuses a `.git` reaching the common dir).
    let pinned = common_pinned || worktree_pinned.is_some();
    if let Some(pin) = worktree_pinned {
        let pin = pin.clone();
        if !pin.is_absolute() {
            return Err(refuse(
                "$MDKG_TRUSTED_WORKTREE must be an absolute path".into(),
            ));
        }
        let pin = fs::canonicalize(&pin).unwrap_or(pin);
        if pin != top {
            return Err(refuse(format!(
                "git places it in the worktree {}, not the trusted one {}",
                top.display(),
                pin.display()
            )));
        }
    }
    let empty_tree = match format.as_str() {
        "sha1" => EMPTY_TREE_SHA1,
        "sha256" => EMPTY_TREE_SHA256,
        other => return Err(refuse(format!("unknown object format {other}"))),
    };
    let entry = top.join(".git");
    let meta = fs::symlink_metadata(&entry)
        .map_err(|e| refuse(format!("no .git in {} ({e})", top.display())))?;
    let kind = meta.file_type();
    let trusted = if git_dir == common {
        if kind.is_dir() || kind.is_symlink() {
            (kind.is_dir() || !pinned) && fs::canonicalize(&entry).ok() == Some(common.clone())
        } else if kind.is_file() {
            !pinned || worktree_setting(&common).as_ref() == Some(&top)
        } else {
            false
        }
    } else if git_dir.parent() == Some(&common.join("worktrees")) {
        if !kind.is_file() {
            return Err(refuse(format!(
                "{} isn't a plain file, as a linked worktree's .git is",
                entry.display()
            )));
        }
        // Git writes the link back absolute, or relative to the git folder
        // (`worktree.useRelativePaths`); only the last part may be a link.
        let back = read_small(&git_dir.join("gitdir"))
            .map_err(|e| refuse(format!("{} has no link back ({e})", git_dir.display())))?;
        let back = git_dir.join(back.trim_end_matches(['\n', '\r']));
        let back = back
            .parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .zip(back.file_name())
            .map(|(p, name)| p.join(name));
        if back.as_ref() != Some(&entry) {
            return Err(refuse(format!(
                "its git folder {} belongs to the worktree at {} (moved without `git worktree \
                 repair`, or a .git copied from another worktree)",
                git_dir.display(),
                back.as_deref()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new("?"))
                    .display()
            )));
        }
        true
    } else {
        false
    };
    if !trusted {
        return Err(refuse(format!(
            "its .git reaches {}, which isn't a worktree of {}{}",
            git_dir.display(),
            common.display(),
            if pinned { " this command may use" } else { "" }
        )));
    }
    // A folder inside another worktree of this repository (one whose own
    // `.git` was removed) would act on the worktree git found above it.
    // Read from the worktrees' own records, which an agent can rewrite:
    // `$MDKG_TRUSTED_WORKTREE` is what makes this certain.
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for (_, other) in linked_worktrees(&common) {
        let Some(other) = other else { continue };
        if other != top && other.starts_with(&top) && real.starts_with(&other) {
            return Err(refuse(format!(
                "it's inside the worktree {}, whose .git is missing or unusable",
                other.display()
            )));
        }
    }
    let config_hooks = config_hooks(&git_dir, &common).map_err(|e| refuse(e.to_string()))?;
    let checkout = Checkout {
        top,
        git_dir,
        common,
        empty_tree,
        config_hooks,
        pinned,
    };
    // Under the folder and the top, replacing what an earlier check left.
    if let Ok(mut list) = CHECKOUTS.lock() {
        list.retain(|(d, _, _)| *d != real && *d != checkout.top);
        list.push((real, trust.clone(), checkout.clone()));
        list.push((checkout.top.clone(), trust.clone(), checkout.clone()));
    }
    Ok(checkout)
}

/// The repository's linked worktrees: each one's git folder, and its top
/// folder as its `gitdir` file names it (canonical), read directly so a
/// FIFO or a damaged record in one worktree's folder can't stall or fail
/// another's commands (an unreadable one is left without a top).
fn linked_worktrees(common: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let Ok(entries) = fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let admin = e.path();
            let top = read_small(&admin.join("gitdir")).ok().and_then(|back| {
                let back = admin.join(back.trim_end_matches(['\n', '\r']));
                back.parent().and_then(|p| fs::canonicalize(p).ok())
            });
            (admin, top)
        })
        .collect()
}

/// Whether `top` really is the worktree the git folder `admin` records:
/// its `.git` is a plain file whose `gitdir:` names `admin` back (an agent
/// can rewrite its own record to claim any folder).
fn links_back(top: &Path, admin: &Path) -> bool {
    let entry = top.join(".git");
    if !fs::symlink_metadata(&entry).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(text) = read_small(&entry) else {
        return false;
    };
    let Some(target) = text.trim_end_matches(['\n', '\r']).strip_prefix("gitdir: ") else {
        return false;
    };
    let named = fs::canonicalize(top.join(target)).ok();
    named.is_some() && named == fs::canonicalize(admin).ok()
}

/// The branch work settles on: `$MDKG_MAIN_REF`, then `mdkg.mainRef` in
/// the repository's own config (never a worktree's `config.worktree`,
/// which an agent can write), then `main`; as a branch name, without
/// `refs/heads/`.
pub fn main_ref(dir: &Path) -> String {
    let name = std::env::var("MDKG_MAIN_REF").ok().or_else(|| {
        checkout_for(dir)
            .ok()
            .and_then(|c| common_config(&c.common, "mdkg.mainRef"))
    });
    let name = name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "main".into());
    let name = name.trim();
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

/// A value from the repository's own config file only.
fn common_config(common: &Path, key: &str) -> Option<String> {
    let mut c = hardened(common);
    c.args(["config", "--file"])
        .arg(common.join("config"))
        .args(["--get", key]);
    let out = run(c, None).ok().filter(|o| o.status.success())?;
    String::from_utf8(out.stdout)
        .ok()
        .map(|v| v.trim_end_matches(['\n', '\r']).to_string())
}

/// Ref guards need file-backed refs. `common` must already be the caller's
/// trusted common directory; this does not establish repository identity.
///
/// Git 2.41 lacks `rev-parse --show-ref-format`. Parse only a bounded snapshot
/// of common/config, with includes disabled and no global or worktree config.
/// Only a successful complete parse with no refStorage key defaults to files.
/// Every explicit value must be exactly `files`, even in duplicate entries.
/// The config itself must be a regular file, never a symlink.
pub fn require_files_ref_storage(common: &Path) -> io::Result<()> {
    ref_storage_config(common, hardened(common))
}

const REF_CONFIG_LIMIT: u64 = 1 << 20;

fn read_ref_config(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure(format!("{} isn't a regular file", path.display())));
    }
    // Check and read the same descriptor: a path replacement cannot redirect
    // the snapshot. Ancestors remain covered by the caller's common-dir trust.
    let mut bytes = Vec::new();
    file.take(REF_CONFIG_LIMIT + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn ref_storage_config(common: &Path, mut cmd: Command) -> io::Result<()> {
    if !common.is_absolute() {
        return Err(failure(
            "ref storage needs an absolute trusted common directory",
        ));
    }
    let config = common.join("config");
    let bytes = read_ref_config(&config).map_err(|e| {
        failure(format!(
            "can't read {} for ref storage: {e}",
            config.display()
        ))
    })?;
    if bytes.len() as u64 > REF_CONFIG_LIMIT || bytes.contains(&0) {
        return Err(failure(format!(
            "{} is too large or contains NUL; ref storage can't be verified",
            config.display()
        )));
    }
    // --get in Git 2.41 can turn read failures into 'key absent'. --list
    // checks the entire parse. Feeding the snapshot avoids a reopen race and
    // bounds normal Git output by the capped input (includes are disabled).
    // Git discovers and reads repository config before parsing --no-includes.
    // An explicit non-repository GIT_DIR prevents discovery while --file -
    // still permits this config-only command (including on Git 2.41).
    cmd.env("GIT_DIR", "/dev/null")
        // Git may consult these during startup even with an explicit --file.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["config", "--file", "-", "--no-includes", "--null", "--list"]);
    let out = run(cmd, Some(&bytes))?;
    if !out.status.success() {
        // Config values and Git's error text may be sensitive; never echo them.
        return Err(failure(format!(
            "git couldn't parse {} for ref storage (exit {:?})",
            config.display(),
            out.status.code()
        )));
    }
    files_ref_storage(&out.stdout)
}

fn files_ref_storage(list: &[u8]) -> io::Result<()> {
    if list.is_empty() {
        return Ok(());
    }
    let list = list
        .strip_suffix(&[0])
        .ok_or_else(|| failure("git returned an incomplete ref-storage config listing"))?;
    for entry in list.split(|b| *b == 0) {
        let (key, value) = match entry.iter().position(|b| *b == b'\n') {
            Some(i) => (&entry[..i], &entry[i + 1..]),
            None => (entry, &[][..]),
        };
        if key.is_empty() {
            return Err(failure(
                "git returned an invalid ref-storage config listing",
            ));
        }
        if key == b"extensions.refstorage" && value != b"files" {
            return Err(failure(
                "the repository must use files ref storage; other or ambiguous formats are refused",
            ));
        }
    }
    Ok(())
}

/// The ref a HEAD file names (`ref: refs/heads/x`), if it names one.
fn head_ref(git_dir: &Path) -> io::Result<Option<String>> {
    Ok(ref_file(&git_dir.join("HEAD"), 4096)?
        .and_then(|text| text.trim_end().strip_prefix("ref: ").map(str::to_string)))
}

/// A ref-name file's text: none when it's missing; any other trouble
/// (too long, not a regular file) is an error, so a held branch is never
/// dropped unseen.
fn ref_file(path: &Path, cap: u64) -> io::Result<Option<String>> {
    match read_ref_names(path, cap) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(failure(format!("{} can't be read: {e}", path.display()))),
    }
}

/// The branches a worktree holds, as git's own guard counts them: the one
/// HEAD names, one being bisected (`BISECT_START`, a bare name), one being
/// rebased (`rebase-merge/head-name`, `rebase-apply/head-name`), whose
/// HEAD is detached meanwhile, and those a `rebase --update-refs` will
/// move (`rebase-merge/update-refs`: a ref, then its old and new values).
/// A list too long to read whole fails closed.
fn held_refs(git_dir: &Path) -> io::Result<Vec<String>> {
    let mut held: Vec<String> = head_ref(git_dir)?.into_iter().collect();
    if let Some(name) = ref_file(&git_dir.join("BISECT_START"), 4096)? {
        let name = name.trim();
        if !name.is_empty() {
            held.push(if name.starts_with("refs/") {
                name.to_string()
            } else {
                format!("refs/heads/{name}")
            });
        }
    }
    for state in ["rebase-merge", "rebase-apply"] {
        if let Some(name) = ref_file(&git_dir.join(state).join("head-name"), 4096)? {
            let name = name.trim();
            if name.starts_with("refs/") {
                held.push(name.to_string());
            }
        }
    }
    const CAP: u64 = 1 << 20;
    // No list is no rebase; one that can't be read, or read whole, refuses.
    let list = ref_file(&git_dir.join("rebase-merge").join("update-refs"), CAP)?;
    if let Some(list) = list {
        held.extend(
            list.lines()
                .step_by(3)
                .map(str::trim)
                .filter(|r| r.starts_with("refs/"))
                .map(str::to_string),
        );
    }
    Ok(held)
}

/// Before staging, committing or merging: checks the worktree again (what
/// config defines may have changed since), and refuses where a rewritten
/// HEAD would land the work: a ref that isn't a branch (a tag), a branch
/// another worktree has checked out, and, when the repository is pinned
/// (the launcher, the MCP server), main from a linked worktree. A HEAD
/// rewritten after this check remains a race; guarding the later update
/// requires `update-ref` with the expected old value.
pub fn ready_to_write(worktree: &Path) -> io::Result<()> {
    let checkout = check_worktree(worktree)?;
    let refuse = |why: String| failure(format!("{}: {why}", worktree.display()));
    let out = command(worktree)
        .args(["symbolic-ref", "-q", "HEAD"])
        .run(None)?;
    let mine = match out.status.code() {
        Some(0) => line(out.stdout)?,
        // Detached: a commit moves no ref.
        Some(1) => return Ok(()),
        _ => {
            return Err(refuse(format!(
                "its HEAD can't be read: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    };
    let Some(branch) = mine.strip_prefix("refs/heads/") else {
        return Err(refuse(format!(
            "its HEAD names {mine}, which isn't a branch"
        )));
    };
    // A disk can fold Unicode names (APFS: `taſk` is `task`, decomposed
    // `é` is `é`) whatever the config says, and no crate here normalizes
    // them: branch names must be ASCII, on every disk (orchestrator,
    // 2026-10-21).
    let ascii_only = |name: &str| -> io::Result<()> {
        if name.is_ascii() {
            Ok(())
        } else {
            Err(refuse(format!(
                "branch names must be ASCII; {name} isn't (rename it with git)"
            )))
        }
    };
    ascii_only(branch)?;
    // Other worktrees' HEADs are read from their files below, which the
    // reftable format leaves as stubs.
    require_files_ref_storage(&checkout.common)?;
    // Spelled exactly as git lists it: on a case-insensitive disk
    // `refs/heads/MAIN` would write through to main.
    let listed = line(git(
        worktree,
        &["for-each-ref", "--format=%(refname)", &mine],
    )?)?;
    if listed != mine {
        // Every branch git doesn't list is refused, a branch with no
        // commits yet included: a disk that folds names (APFS folds Unicode
        // case and normalization, `taſk` is `task`) would let a look-alike
        // that git lists nowhere write through to another branch.
        let any_case = line(git(
            worktree,
            &[
                "for-each-ref",
                "--ignore-case",
                "--format=%(refname)",
                &mine,
            ],
        )?)?;
        let stored = if listed.is_empty() { any_case } else { listed };
        let hint = match stored.lines().count() {
            // ASCII and listed in no case: truly a branch with no commits.
            0 => " (a branch with no commits yet needs a commit first, made with git)".to_string(),
            1 => format!(" ({stored}; rename the branch to match)"),
            _ => String::new(),
        };
        return Err(refuse(format!(
            "its HEAD names {mine}, which isn't a branch spelled as git lists it{hint}"
        )));
    }
    // Letter case counts where the disk may fold it: core.ignorecase, or a
    // platform whose disks fold by default (a cloned or moved repository's
    // config can be wrong). Only elsewhere with ignorecase off do
    // `Feature` and `feature` stay apart.
    let folds = common_config(&checkout.common, "core.ignorecase").is_some_and(|v| v == "true")
        || cfg!(any(target_vendor = "apple", target_os = "windows"));
    let same_name = |a: &str, b: &str| {
        if folds {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let same = |other: &str| same_name(other, &mine);
    // Another branch that is this one on a folding disk: committing would
    // write through to it.
    if folds {
        let all = git(
            worktree,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        )?;
        let all = String::from_utf8_lossy(&all);
        if let Some(other) = all
            .lines()
            .find(|r| *r != mine && r.eq_ignore_ascii_case(&mine))
        {
            return Err(refuse(format!(
                "its branch {branch} collides with {other} on this disk, which doesn't tell \
                 letter case apart (rename one with git)"
            )));
        }
    }
    let main = main_ref(worktree);
    ascii_only(&main)?;
    if checkout.pinned && same_name(branch, &main) {
        return Err(refuse(format!(
            "its HEAD is on {branch}, the branch work settles on, which the launcher's and \
             the MCP server's commands never commit to directly"
        )));
    }
    // Every other worktree's HEAD: each linked one's, and the main
    // checkout's unless the repository is bare (git's own rule).
    let linked = checkout.git_dir != checkout.common;
    let mut others: Vec<PathBuf> = linked_worktrees(&checkout.common)
        .into_iter()
        .map(|(admin, _)| admin)
        .filter(|admin| fs::canonicalize(admin).ok().as_ref() != Some(&checkout.git_dir))
        .collect();
    let bare = common_config(&checkout.common, "core.bare").is_some_and(|v| v == "true");
    if linked && !bare {
        others.push(checkout.common.clone());
    }
    let mut held = Vec::new();
    for admin in &others {
        for r in held_refs(admin).map_err(|e| refuse(e.to_string()))? {
            let name = r.strip_prefix("refs/heads/").unwrap_or(&r);
            if !name.is_ascii() {
                let holder = if *admin == checkout.common {
                    "the main checkout".to_string()
                } else {
                    format!(
                        "the worktree {}",
                        admin.file_name().unwrap_or_default().to_string_lossy()
                    )
                };
                return Err(refuse(format!(
                    "branch names must be ASCII; {name}, held by {holder}, isn't (rename it \
                     with git there)"
                )));
            }
            held.push(r);
        }
    }
    if held.iter().any(|r| same(r)) {
        return Err(refuse(format!(
            "its branch {branch} is checked out, bisected or rebased in another worktree"
        )));
    }
    Ok(())
}

/// The names of the hooks config defines for this git folder, in any scope.
fn config_hooks(git_dir: &Path, common: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let mut c = hardened(git_dir);
    c.env("GIT_COMMON_DIR", common)
        .arg("--git-dir")
        .arg(git_dir)
        .args(["config", "--name-only", "-z", "--get-regexp", HOOK_KEYS]);
    let out = run(c, None)?;
    // 1 is "none set"; anything else unread fails closed.
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(failure(format!(
                "can't read which hooks config defines: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(hook_names(&out.stdout))
}

/// The config keys that define a hook, for `git config --name-only -z
/// --get-regexp`.
pub const HOOK_KEYS: &str = r"^hook\..*\.(command|event|enabled)$";

/// The hook names in `git config --name-only -z --get-regexp` output for
/// [`HOOK_KEYS`], sorted and without repeats.
pub fn hook_names(listed: &[u8]) -> Vec<std::ffi::OsString> {
    // Names as bytes: one that isn't UTF-8 is switched off all the same.
    use std::os::unix::ffi::OsStringExt;
    let mut names: Vec<std::ffi::OsString> = listed
        .split(|b| *b == 0 || *b == b'\n')
        .filter_map(|key| key.strip_prefix(b"hook."))
        .filter_map(|rest| {
            let dot = rest.iter().rposition(|b| *b == b'.')?;
            Some(std::ffi::OsString::from_vec(rest[..dot].to_vec()))
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The common dir's `core.worktree` (a submodule's), resolved against it.
fn worktree_setting(common: &Path) -> Option<PathBuf> {
    let mut c = hardened(common);
    c.args(["config", "--file"])
        .arg(common.join("config"))
        .args(["--get", "core.worktree"]);
    let out = run(c, None).ok().filter(|o| o.status.success())?;
    let value = String::from_utf8(out.stdout).ok()?;
    fs::canonicalize(common.join(value.trim_end_matches(['\n', '\r']))).ok()
}

/// Runs git in `dir` and returns its stdout.
fn git(dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = command(dir).args(args).run(None)?;
    if !out.status.success() {
        return Err(failure(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn line(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.trim_end().to_string())
        .map_err(|e| failure(e.to_string()))
}

/// The git common directory, shared by every worktree, of the trusted
/// worktree `repo` is in.
pub fn common_dir(repo: &Path) -> io::Result<PathBuf> {
    Ok(checkout_for(repo)?.common)
}

/// The top folder of the worktree `dir` is in, as git finds it, unchecked
/// ([`check_worktree`] checks it).
pub fn toplevel(dir: &Path) -> io::Result<PathBuf> {
    let mut c = hardened(dir);
    c.args(["rev-parse", "--show-toplevel"]);
    let out = run(c, None)?;
    if !out.status.success() {
        return Err(failure(format!(
            "git rev-parse --show-toplevel: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    fs::canonicalize(line(out.stdout)?)
}

/// The top folder of every checked-out worktree of the repository, the
/// primary one first, skipping worktrees whose folder is gone. A bare
/// repository's own entry is its git folder, not a checkout, so it's left
/// out.
pub fn worktrees(repo: &Path) -> io::Result<Vec<PathBuf>> {
    // From the records directly, never `git worktree list`, so one
    // worktree's damaged record can't stall another's commands.
    let common = checkout_for(repo)?.common;
    let mut tops = Vec::new();
    if common_config(&common, "core.bare").is_none_or(|v| v != "true") {
        let main = worktree_setting(&common).or_else(|| {
            (common.file_name().is_some_and(|n| n == ".git"))
                .then(|| common.parent().map(Path::to_path_buf))
                .flatten()
        });
        tops.extend(main);
    }
    tops.extend(
        linked_worktrees(&common)
            .into_iter()
            .filter_map(|(admin, top)| top.filter(|t| links_back(t, &admin))),
    );
    Ok(tops)
}

/// The local time zone's offset from UTC in minutes, as git reports it in
/// the committer identity (`+0200`, `-0500`). 0 if git can't say.
pub fn local_offset_minutes(dir: &Path) -> i64 {
    let Ok(ident) = git(dir, &["var", "GIT_COMMITTER_IDENT"]).and_then(line) else {
        return 0;
    };
    let Some(tz) = ident.rsplit(' ').next() else {
        return 0;
    };
    let (sign, digits) = match tz.split_at_checked(1) {
        Some(("+", d)) => (1, d),
        Some(("-", d)) => (-1, d),
        _ => return 0,
    };
    match (
        digits.get(..2).and_then(|h| h.parse::<i64>().ok()),
        digits.get(2..4).and_then(|m| m.parse::<i64>().ok()),
    ) {
        (Some(h), Some(m)) => sign * (h * 60 + m),
        _ => 0,
    }
}

/// Whether `refname` names a commit.
pub fn has_ref(repo: &Path, refname: &str) -> bool {
    command(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ])
        .run(None)
        .is_ok_and(|o| o.status.success())
}

/// Where the coordinator database lives for a repository.
pub fn store_path(repo: &Path) -> io::Result<PathBuf> {
    Ok(common_dir(repo)?.join("mdkg").join("coord.sqlite"))
}

/// The worktree's current commit and branch.
pub fn head(worktree: &Path) -> io::Result<Source> {
    Ok(Source {
        commit: line(git(worktree, &["rev-parse", "HEAD"])?)?,
        branch: line(git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?)?,
    })
}

/// The file's spelling on disk, matching `path` in any case: the first
/// tracked or untracked match, or `path` itself if there is none.
pub fn disk_spelling(worktree: &Path, path: &str) -> String {
    files_under(worktree, path)
        .ok()
        .and_then(|files| {
            files
                .into_iter()
                .find(|f| Scope::new(f.as_str()) == Scope::new(path))
        })
        .unwrap_or_else(|| path.to_string())
}

/// Whether `path` is tracked and its file, staged or not, is as `HEAD`
/// has it: nothing about it is left to commit.
pub fn unchanged_since_head(worktree: &Path, path: &str) -> io::Result<bool> {
    let spec = format!(":(literal){path}");
    let tracked = !git(worktree, &["ls-files", "-z", "--full-name", "--", &spec])?.is_empty();
    if !tracked {
        return Ok(false);
    }
    let status = command(worktree)
        .args(["diff", "--quiet", "HEAD", "--", &spec])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!("can't tell whether {path} is committed"))),
    }
}

/// Whether `a` and `b` hold the same content under `path` (a file or a
/// folder, matched as scopes match ASCII case).
pub fn same_under(repo: &Path, a: &str, b: &str, path: &str) -> io::Result<bool> {
    let spec = format!(":(icase,literal){path}");
    let status = command(repo)
        .args(["diff", "--quiet", "--no-renames", a, b, "--", &spec])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!("can't compare {a} and {b} under {path}"))),
    }
}

/// Whether `tip` has a commit that `base` lacks and that changed `path`.
/// Every side of every merge is walked (`--full-history`): git's default
/// simplification would hide an edit on main's side behind a merge that
/// took the other side's content whole. A merge whose content under
/// `path` is one of its parents' changed nothing itself and doesn't count;
/// what its other side brought still does.
pub fn touched_between(repo: &Path, base: &str, tip: &str, path: &str) -> io::Result<bool> {
    let spec = format!(":(icase,literal){path}");
    let range = format!("{base}..{tip}");
    let listed = git(
        repo,
        &[
            "rev-list",
            "--full-history",
            "--parents",
            &range,
            "--",
            &spec,
        ],
    )?;
    for line in String::from_utf8_lossy(&listed).lines() {
        let mut ids = line.split_whitespace();
        let Some(commit) = ids.next() else {
            continue;
        };
        let parents: Vec<&str> = ids.collect();
        if parents.len() < 2 {
            return Ok(true);
        }
        let mut takes_a_side = false;
        for parent in &parents {
            if same_under(repo, parent, commit, path)? {
                takes_a_side = true;
                break;
            }
        }
        if !takes_a_side {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the repository has no commit at all yet: no branch, tag or
/// other ref names anything, and HEAD names no commit (a fresh `git
/// init`; a detached HEAD whose branches were deleted still has commits).
pub fn no_commits(repo: &Path) -> io::Result<bool> {
    if !git(repo, &["for-each-ref", "--count=1", "--format=%(refname)"])?.is_empty() {
        return Ok(false);
    }
    let head = command(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .run(None)?;
    Ok(!head.status.success())
}

/// The commit `refname` names.
pub fn commit_of(repo: &Path, refname: &str) -> io::Result<String> {
    line(git(
        repo,
        &["rev-parse", "--verify", &format!("{refname}^{{commit}}")],
    )?)
}

/// The content hash of a worktree file, or `None` if it doesn't exist.
pub fn file_digest(worktree: &Path, path: &str) -> io::Result<Option<String>> {
    match fs::read(worktree.join(path)) {
        Ok(bytes) => Ok(Some(Digest::of_bytes(&bytes).to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Stages and commits exactly `paths` (other staged changes stay staged).
/// Returns false when they had nothing to commit.
pub fn commit_paths(worktree: &Path, paths: &[String], message: &str) -> io::Result<bool> {
    // No paths means nothing to commit, never "everything staged".
    if paths.is_empty() {
        return Ok(false);
    }
    // The files inside these scopes, matched as scopes are (in any case,
    // never as globs), named exactly for git. A deletion already staged
    // (`git rm`, `git mv`) is committed too, though `git add` can't name it.
    let (present, deleted) = files_in(worktree, paths)?;
    for p in paths {
        let scope = Scope::new(p.as_str());
        let found = present
            .iter()
            .chain(&deleted)
            .any(|f| scope.contains(&Scope::new(f.as_str())));
        if !found {
            return Err(failure(format!(
                "{p} matches no file git can see (is it ignored?)"
            )));
        }
    }
    let literal = |f: &String| format!(":(literal){f}");
    let staged: Vec<String> = present.iter().map(literal).collect();
    let specs: Vec<String> = present.iter().chain(&deleted).map(literal).collect();
    let paths = &specs;
    // The index entries for these paths before staging, so a failed commit
    // can put the index back exactly, including anything staged on purpose.
    let mut list = vec!["ls-files", "--stage", "-z", "--"];
    list.extend(paths.iter().map(String::as_str));
    let saved = git(worktree, &list)?;
    // Before anything is staged, so a refusal leaves the index as it was.
    ready_to_write(worktree)?;
    if !staged.is_empty() {
        let mut add = vec!["add", "-A", "--"];
        add.extend(staged.iter().map(String::as_str));
        git(worktree, &add)?;
    }
    // No textconv or external diff: their output must not decide.
    let mut diff = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--cached",
        "--quiet",
        "--",
    ];
    diff.extend(paths.iter().map(String::as_str));
    let unchanged = command(worktree).args(&diff).run(None)?.status.success();
    if unchanged {
        return Ok(false);
    }
    let mut commit = vec![
        "commit",
        "--quiet",
        "--no-verify",
        "--no-gpg-sign",
        "-m",
        message,
        "--",
    ];
    commit.extend(paths.iter().map(String::as_str));
    // Again just before the commit, so what config defines or HEAD names
    // can't change in between unseen.
    if let Err(e) = ready_to_write(worktree) {
        if let Err(r) = restore_index(worktree, paths, &saved) {
            return Err(failure(format!(
                "{e}; restoring the index also failed: {r}"
            )));
        }
        return Err(e);
    }
    let head = |rev: &str| {
        git(worktree, &["rev-parse", "--verify", "--quiet", rev])
            .ok()
            .and_then(|b| line(b).ok())
    };
    let before = head("HEAD");
    if let Err(e) = git(worktree, &commit) {
        // Cut short after our commit landed (a time limit hit while git
        // tidied up): HEAD moved on by one commit from where it was, and
        // holds these paths as staged.
        let landed = before.is_some() && head("HEAD^1") == before && head("HEAD") != before && {
            let mut same = vec!["diff", "--cached", "--quiet", "HEAD", "--"];
            same.extend(paths.iter().map(String::as_str));
            command(worktree)
                .args(&same)
                .run(None)
                .is_ok_and(|o| o.status.success())
        };
        if landed {
            return Ok(true);
        }
        if let Err(r) = restore_index(worktree, paths, &saved) {
            return Err(failure(format!(
                "{e}; restoring the index also failed: {r}"
            )));
        }
        return Err(e);
    }
    Ok(true)
}

/// Puts the index entries for `paths` back to `saved` (`ls-files --stage
/// -z` output): entries added since are dropped, saved ones written back.
fn restore_index(worktree: &Path, paths: &[String], saved: &[u8]) -> io::Result<()> {
    // -f: the entry may differ from both HEAD and the worktree (staged on
    // purpose, then edited), which plain rm refuses.
    let mut rm = vec![
        "rm",
        "--cached",
        "-f",
        "--quiet",
        "-r",
        "--ignore-unmatch",
        "--",
    ];
    rm.extend(paths.iter().map(String::as_str));
    git(worktree, &rm)?;
    if saved.is_empty() {
        return Ok(());
    }
    let out = command(worktree)
        .args(["update-index", "-z", "--index-info"])
        .run(Some(saved))?;
    if !out.status.success() {
        return Err(failure(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    Ok(())
}

/// Pathspecs narrowing a listing to the top folders of `scopes`, so a
/// command doesn't list the whole repository. Git folds only ASCII case, so a
/// top folder with other letters means no narrowing (everything is listed).
fn narrowing(scopes: &[Scope]) -> Vec<String> {
    let mut tops: BTreeSet<String> = BTreeSet::new();
    for s in scopes {
        let top = s.as_str().split('/').next().unwrap_or_default();
        // The Kelvin sign folds to `k` for scopes but not for git, so a `k`
        // means no narrowing either.
        if top.is_empty() || !top.is_ascii() || top.contains(['k', 'K']) {
            return Vec::new();
        }
        tops.insert(format!(":(icase,literal){top}"));
    }
    tops.into_iter().collect()
}

/// NUL-separated names from git, as text. A name that isn't UTF-8 can't be
/// named back to git exactly, so it's reported rather than mangled.
fn names(out: &[u8]) -> Vec<Result<String, String>> {
    out.split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8(r.to_vec()).map_err(|_| String::from_utf8_lossy(r).into_owned()))
        .collect()
}

/// Files inside `scopes`: those in the index or the worktree (`present`, not
/// ignored), and those whose deletion is already staged (`deleted`), matched
/// as scopes match: in any case (with Unicode folding), segment by segment,
/// never as globs. Refuses two files that differ only in case, which scopes
/// can't tell apart, and names that aren't UTF-8.
fn files_in(worktree: &Path, scopes: &[String]) -> io::Result<(Vec<String>, Vec<String>)> {
    let scopes: Vec<Scope> = scopes.iter().map(|s| Scope::new(s.as_str())).collect();
    let narrow = narrowing(&scopes);
    let mut list = vec![
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--full-name",
        "--",
    ];
    list.extend(narrow.iter().map(String::as_str));
    let mut gone = vec![
        "diff",
        "--cached",
        "--name-only",
        "-z",
        "--no-renames",
        "--diff-filter=D",
        "--",
    ];
    gone.extend(narrow.iter().map(String::as_str));
    let mut seen: BTreeMap<Scope, String> = BTreeMap::new();
    let mut take = |raw: Vec<Result<String, String>>, deleted: bool| -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for name in raw {
            let f = match name {
                Ok(f) => f,
                Err(lossy) => {
                    if scopes
                        .iter()
                        .any(|s| s.contains(&Scope::new(lossy.as_str())))
                    {
                        return Err(failure(format!(
                            "{lossy} has a name that isn't UTF-8, which mdkg can't name to git; rename it"
                        )));
                    }
                    continue;
                }
            };
            let leaf = Scope::new(f.as_str());
            if !scopes.iter().any(|s| s.contains(&leaf)) {
                continue;
            }
            match seen.get(&leaf) {
                Some(other) if *other == f => continue,
                Some(other) if deleted => {
                    return Err(failure(format!(
                        "a rename of {f} to {other}, differing only in case, is staged; \
                         commit it with `git commit` first, then try again"
                    )));
                }
                Some(other) => {
                    return Err(failure(format!(
                        "{other} and {f} differ only in case; mdkg treats them as one file, \
                         so rename one with `git mv`"
                    )));
                }
                None => {
                    seen.insert(leaf, f.clone());
                    out.push(f);
                }
            }
        }
        out.sort();
        Ok(out)
    };
    let present = take(names(&git(worktree, &list)?), false)?;
    let deleted = take(names(&git(worktree, &gone)?), true)?;
    Ok((present, deleted))
}

/// Tracked and untracked (not ignored) files inside `path`, relative to the
/// worktree's top folder, matched as scopes match (see [`files_in`]).
pub fn files_under(worktree: &Path, path: &str) -> io::Result<Vec<String>> {
    Ok(files_in(worktree, &[path.to_string()])?.0)
}

/// Files changed on this branch since it left `base`, plus uncommitted and
/// untracked ones, relative to the worktree's top folder.
pub fn changed_since(worktree: &Path, base: &str) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let range = format!("{base}...HEAD");
    for args in [
        vec!["diff", "--name-only", "-z", range.as_str()],
        vec!["diff", "--name-only", "-z", "HEAD"],
        vec![
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
    ] {
        for name in git(worktree, &args)?
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
        {
            out.insert(String::from_utf8_lossy(name).into_owned());
        }
    }
    Ok(out)
}

/// Brings `commit` into the worktree's branch (`git merge`), so work that
/// builds on another branch's unmerged version keeps its history. This is
/// `sync` for editing: copying the file alone loses the ancestry, and the
/// later merge to main then conflicts. Worktrees share one object store, so
/// another local branch's commit needs no fetch.
pub fn stack_on(worktree: &Path, commit: &str) -> io::Result<()> {
    ready_to_write(worktree)?;
    let out = command(worktree)
        .args([
            "merge",
            "--no-edit",
            "--no-verify",
            "--no-gpg-sign",
            "--no-verify-signatures",
            commit,
        ])
        .run(None)?;
    if out.status.success() {
        // A shallow history can make git report "Already up to date"
        // without merging: only a merge that took counts.
        if !is_ancestor(worktree, commit)? {
            return Err(failure(format!(
                "git merge {commit} reported success, but HEAD doesn't contain it (a shallow \
                 clone? `git fetch --unshallow`)"
            )));
        }
        return Ok(());
    }
    // A conflict is reported on stdout, a refusal to start on stderr.
    let said = [out.stdout, out.stderr]
        .iter()
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    Err(failure(format!("git merge {commit}: {said}")))
}

/// Whether `commit` is already in the history of the worktree's HEAD.
pub fn is_ancestor(worktree: &Path, commit: &str) -> io::Result<bool> {
    is_ancestor_of(worktree, commit, "HEAD")
}

/// Whether `commit` is in the history of `refname` (itself included).
pub fn is_ancestor_of(repo: &Path, commit: &str, refname: &str) -> io::Result<bool> {
    let status = command(repo)
        .args(["merge-base", "--is-ancestor", commit, refname])
        .run(None)?
        .status;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(format!(
            "can't tell whether {commit} is in {refname}"
        ))),
    }
}

/// The commits in `commit`'s history that `base` doesn't have.
pub fn commits_not_on(repo: &Path, commit: &str, base: &str) -> io::Result<BTreeSet<String>> {
    let out = git(repo, &["rev-list", commit, "--not", base, "--"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Files with unresolved conflicts, when a merge is in progress.
pub fn merge_conflicts(worktree: &Path) -> io::Result<Option<Vec<String>>> {
    if !has_ref(worktree, "MERGE_HEAD") {
        return Ok(None);
    }
    let out = git(worktree, &["diff", "--name-only", "-z", "--diff-filter=U"])?;
    Ok(Some(
        out.split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect(),
    ))
}

/// Writes `path` in the worktree from `commit` (or deletes it if the commit
/// doesn't have it), without touching the index or history. For reading
/// another branch's version; to edit on top of it, use [`stack_on`].
pub fn checkout_file(worktree: &Path, commit: &str, path: &str) -> io::Result<()> {
    let spec = format!("{commit}:{path}");
    let exists = command(worktree)
        .args(["cat-file", "-e", &spec])
        .run(None)?
        .status
        .success();
    if exists {
        git(
            worktree,
            &["restore", "--source", commit, "--worktree", "--", path],
        )
        .map(drop)
    } else {
        match fs::remove_file(worktree.join(path)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Every file under `prefix` on `refname`, with its content hash.
pub fn leaves(repo: &Path, refname: &str, prefix: &str) -> io::Result<BTreeMap<Scope, String>> {
    leaves_cached(repo, refname, prefix, None)
}

/// [`leaves`], taking the hashes of blobs it has seen before from the
/// digest cache at `cache` (see `digest_cache`), and hashing only the rest.
/// The result is the same with or without the cache.
pub fn leaves_cached(
    repo: &Path,
    refname: &str,
    prefix: &str,
    cache: Option<&Path>,
) -> io::Result<BTreeMap<Scope, String>> {
    let listing = git(
        repo,
        &[
            "ls-tree",
            "-r",
            "-l",
            "-z",
            "--full-tree",
            refname,
            "--",
            prefix,
        ],
    )?;
    let mut paths: Vec<(String, String)> = Vec::new();
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8(record.to_vec()).map_err(|e| failure(e.to_string()))?;
        let (meta, path) = record
            .split_once('\t')
            .ok_or_else(|| failure(format!("unexpected ls-tree record {record:?}")))?;
        // `-l` pads the size column with spaces.
        let mut fields = meta.split_whitespace();
        if let (Some(_mode), Some("blob"), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            let size = size
                .parse()
                .map_err(|_| failure(format!("unexpected ls-tree record {record:?}")))?;
            sizes.insert(oid.to_string(), size);
            paths.push((path.to_string(), oid.to_string()));
        }
    }
    // Scopes ignore case, so two files that differ only in case would be one
    // leaf, and one would hide the other. Refuse instead.
    let mut seen: BTreeMap<Scope, &str> = BTreeMap::new();
    for (path, _) in &paths {
        if let Some(other) = seen.insert(Scope::new(path.as_str()), path) {
            return Err(failure(format!(
                "on {refname}, {other} and {path} differ only in case; mdkg treats them as \
                 one file, so rename one with `git mv` on a branch and merge it to {refname}"
            )));
        }
    }
    let wanted: BTreeMap<&str, u64> = sizes
        .iter()
        .map(|(oid, size)| (oid.as_str(), *size))
        .collect();
    let mut cache = cache.and_then(DigestCache::open);
    let (known, rows) = cache
        .as_ref()
        .and_then(|c| c.lookup(&wanted))
        .unwrap_or_default();
    let missing: BTreeSet<&str> = wanted
        .keys()
        .copied()
        .filter(|oid| !known.contains_key(*oid))
        .collect();
    let mut digests = blob_digests(repo, &missing)?;
    digests.extend(known);
    // Rewritten only when main brought new blobs or dropped some.
    if let Some(cache) = cache.as_mut()
        && (!missing.is_empty() || rows != wanted.len())
    {
        let rows: Vec<(&str, u64, &str)> = wanted
            .iter()
            .filter_map(|(oid, size)| digests.get(*oid).map(|d| (*oid, *size, d.as_str())))
            .collect();
        cache.replace(&rows);
    }
    paths
        .into_iter()
        .map(|(path, oid)| {
            let digest = digests
                .get(&oid)
                .cloned()
                .ok_or_else(|| failure(format!("missing blob {oid}")))?;
            Ok((Scope::new(path), digest))
        })
        .collect()
}

/// Hashes blobs through one `git cat-file --batch`.
fn blob_digests(repo: &Path, oids: &BTreeSet<&str>) -> io::Result<HashMap<String, String>> {
    if oids.is_empty() {
        return Ok(HashMap::new());
    }
    let input: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
    let out = command(repo)
        .args(["cat-file", "--batch"])
        .run(Some(input.as_bytes()))?;
    if !out.status.success() {
        return Err(failure(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    let mut digests = HashMap::new();
    let mut rest = out.stdout.as_slice();
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| failure("truncated cat-file header"))?;
        let header = String::from_utf8_lossy(&rest[..end]).to_string();
        let mut fields = header.split(' ');
        let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(failure(format!("unexpected cat-file header {header:?}")));
        };
        if kind != "blob" {
            return Err(failure(format!("{oid} is a {kind}, not a blob")));
        }
        let size: usize = size.parse().map_err(|_| failure("bad blob size"))?;
        let body = rest
            .get(end + 1..end + 1 + size)
            .ok_or_else(|| failure("truncated blob"))?;
        digests.insert(oid.to_string(), Digest::of_bytes(body).to_string());
        rest = rest.get(end + 2 + size..).unwrap_or_default();
    }
    Ok(digests)
}

#[cfg(test)]
mod ref_storage_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("mdkg-ref-storage-{}-{id}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            Self(fs::canonicalize(dir).unwrap())
        }

        fn config(&self, bytes: impl AsRef<[u8]>) {
            fs::write(self.0.join("config"), bytes).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn child_passed_once(out: std::process::Output) {
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("test result: ok. 1 passed; 0 failed;"),
            "child did not execute exactly one test: {out:?}"
        );
    }

    #[test]
    fn ref_storage_accepts_absence_and_only_explicit_files() {
        let dir = Scratch::new();
        for body in [
            "",
            "[core]\n bare\n",
            "[Extensions]\n refStorage = files\n",
            "[extensions]\n refStorage = files\n refStorage = files\n",
        ] {
            dir.config(body);
            require_files_ref_storage(&dir.0).unwrap();
        }
    }

    #[test]
    fn ref_storage_refuses_unsupported_and_ambiguous_values() {
        let dir = Scratch::new();
        for value in [
            "reftable",
            "unknown",
            "",
            "FILES",
            "\"files \"",
            "\"files\\n\"",
            "files\n refStorage = reftable",
            "reftable\n refStorage = files",
        ] {
            dir.config(format!("[extensions]\n refStorage = {value}\n"));
            assert!(require_files_ref_storage(&dir.0).is_err(), "{value:?}");
        }
        dir.config("[extensions]\n refStorage\n");
        assert!(require_files_ref_storage(&dir.0).is_err());
    }

    #[test]
    fn ref_storage_ignores_includes_and_other_config_scopes() {
        let dir = Scratch::new();
        let init = hardened(&dir.0).args(["init", "--quiet"]).output().unwrap();
        assert!(init.status.success(), "{init:?}");
        let common = dir.0.join(".git");
        let mut config = fs::read_to_string(common.join("config")).unwrap();
        config.push_str("[extensions]\nworktreeConfig = true\n");
        fs::write(common.join("config"), &config).unwrap();
        let other = common.join("other.cfg");
        fs::write(&other, "[extensions]\nrefStorage = reftable\n[malformed\n").unwrap();
        fs::write(
            common.join("config.worktree"),
            "[extensions]\nrefStorage = reftable\n",
        )
        .unwrap();
        // Prove the conflicting worktree config is active for an ordinary read.
        let lookup = hardened(&dir.0)
            .args(["config", "--get", "extensions.refStorage"])
            .output()
            .unwrap();
        assert!(lookup.status.success(), "{lookup:?}");
        assert_eq!(lookup.stdout, b"reftable\n");
        config
            .push_str("[include]\npath = other.cfg\n[includeIf \"gitdir:**\"]\npath = other.cfg\n");
        fs::write(common.join("config"), config).unwrap();
        let mut command = hardened(&common);
        command
            .env("GIT_CONFIG_GLOBAL", &other)
            .env("GIT_CONFIG_SYSTEM", &other);
        ref_storage_config(&common, command).unwrap();
    }

    #[test]
    fn ref_storage_ignores_inherited_config_environment() {
        const PROBE: &str = "MDKG_REF_STORAGE_CONFIG_ENV_TEST";
        if std::env::var_os(PROBE).is_some() {
            assert_eq!(
                std::env::var("GIT_CONFIG_KEY_0").unwrap(),
                "extensions.refStorage"
            );
            assert_eq!(std::env::var("GIT_CONFIG_VALUE_0").unwrap(), "reftable");
            let dir = Scratch::new();
            dir.config("");
            // Construct hardened() only after inheriting the hostile values.
            let command = hardened(&dir.0);
            // --file alone could mask valid overrides, so verify removal too.
            for name in ["GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"] {
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                        .map(|(_, value)| value),
                    Some(None),
                    "{name} must be removed from the inherited environment"
                );
            }
            ref_storage_config(&dir.0, command).unwrap();
            return;
        }
        for (count, parameters) in [
            ("1", "'extensions.refStorage=reftable'"),
            // Malformed controls also exercise cleanup before Git startup.
            ("not-a-number", "malformed parameters"),
        ] {
            child_passed_once(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "git::ref_storage_tests::ref_storage_ignores_inherited_config_environment",
                        "--test-threads=1",
                    ])
                    .env(PROBE, "1")
                    .env("GIT_CONFIG", "/nonexistent-mdkg-fixture-config")
                    .env("GIT_CONFIG_PARAMETERS", parameters)
                    .env("GIT_CONFIG_COUNT", count)
                    .env("GIT_CONFIG_KEY_0", "extensions.refStorage")
                    .env("GIT_CONFIG_VALUE_0", "reftable")
                    .output()
                    .unwrap(),
            );
        }
    }

    #[test]
    fn ref_storage_refuses_symlinked_configs_before_spawning_git() {
        let common = Scratch::new();
        let worker = Scratch::new();
        let target = worker.0.join("config");
        let relative = Path::new("..")
            .join(worker.0.file_name().unwrap())
            .join("config");
        for link_target in [&target, &relative] {
            std::os::unix::fs::symlink(link_target, common.0.join("config")).unwrap();
            for body in [
                "",
                "[extensions]\nrefStorage = files\n",
                "[extensions]\nrefStorage = reftable\n",
            ] {
                worker.config(body);
                assert!(
                    require_files_ref_storage(&common.0).is_err(),
                    "followed {link_target:?}"
                );
                // A missing Git would report a spawn error if the read passed.
                let error = ref_storage_config(&common.0, Command::new(common.0.join("no-git")))
                    .unwrap_err();
                assert!(error.to_string().starts_with("can't read "), "{error}");
                assert_eq!(fs::read_to_string(&target).unwrap(), body);
            }
            fs::remove_file(common.0.join("config")).unwrap();
        }
    }

    #[test]
    fn ref_storage_requires_a_readable_regular_complete_config() {
        let dir = Scratch::new();
        assert!(require_files_ref_storage(&dir.0).is_err(), "missing config");
        let config = dir.0.join("config");
        fs::create_dir(&config).unwrap();
        assert!(
            require_files_ref_storage(&dir.0).is_err(),
            "directory config"
        );
        fs::remove_dir(&config).unwrap();
        dir.config("[core]\nrepositoryformatversion = 0\n");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read mode 000; when access is denied it must not mean absent.
        if fs::File::open(&config).is_err() {
            assert!(
                require_files_ref_storage(&dir.0).is_err(),
                "unreadable config"
            );
        }
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        for body in [
            b"[extensions]\nrefStorage = files\n[malformed\n".to_vec(),
            b"[core]\nbare = false\0\n".to_vec(),
            vec![b' '; REF_CONFIG_LIMIT as usize + 1],
        ] {
            dir.config(body);
            assert!(require_files_ref_storage(&dir.0).is_err());
        }
    }

    #[test]
    fn ref_storage_refuses_spawn_and_command_failures() {
        let dir = Scratch::new();
        dir.config("");
        assert!(ref_storage_config(&dir.0, Command::new(dir.0.join("no-git"))).is_err());
        let mut command = Command::new("/usr/bin/false");
        command.process_group(0);
        assert!(ref_storage_config(&dir.0, command).is_err());
        for output in [
            b"extensions.refstorage\nfiles".as_slice(),
            b"\0",
            b"core.bare\0\0",
        ] {
            assert!(files_ref_storage(output).is_err());
        }
    }

    #[test]
    fn ref_storage_refuses_a_fifo_without_waiting_for_a_writer() {
        let dir = Scratch::new();
        assert!(
            Command::new("mkfifo")
                .arg(dir.0.join("config"))
                .status()
                .unwrap()
                .success()
        );
        assert!(require_files_ref_storage(&dir.0).is_err());
    }

    #[test]
    fn ref_storage_uses_the_bounded_runner() {
        const PROBE: &str = "MDKG_REF_STORAGE_TIMEOUT_TEST";
        if std::env::var_os(PROBE).is_some() {
            let dir = Scratch::new();
            dir.config("");
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "while :; do :; done"]).process_group(0);
            let error = ref_storage_config(&dir.0, command).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            return;
        }
        // Set the timeout in a child; never mutate process-global env while
        // other tests are running on their four test threads.
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git::ref_storage_tests::ref_storage_uses_the_bounded_runner",
                "--test-threads=1",
            ])
            .env(PROBE, "1")
            .env("MDKG_GIT_TIMEOUT_MS", "20")
            .output()
            .unwrap();
        child_passed_once(out);
    }

    #[test]
    fn ref_storage_does_not_accept_partial_pipe_reads() {
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture pipe failure"))
            }
        }
        // A valid partial record without refStorage must not become success
        // when reading the rest of the listing fails.
        let pipe = io::Cursor::new(b"core.bare\0").chain(FailedRead);
        assert!(read_output(Some(Box::new(pipe))).is_err());
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn nested_request_trust_restores_on_return_and_unwind() {
        let parent = WorktreeTrust {
            common: Some("/parent/common".into()),
            worktree: None,
        };
        with_worktree_trust(&parent, || {
            with_worktree_trust(&WorktreeTrust::default(), || {
                assert_eq!(WorktreeTrust::current(), WorktreeTrust::default());
            });
            assert_eq!(WorktreeTrust::current(), parent);
            let panicked = std::panic::catch_unwind(|| {
                with_worktree_trust(&WorktreeTrust::default(), || panic!("test unwind"));
            });
            assert!(panicked.is_err());
            assert_eq!(WorktreeTrust::current(), parent);
        });
    }

    #[test]
    fn explicit_context_is_thread_local() {
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            for name in ["/first", "/second"] {
                let barrier = &barrier;
                s.spawn(move || {
                    let trust = WorktreeTrust {
                        common: Some(name.into()),
                        worktree: None,
                    };
                    with_worktree_trust(&trust, || {
                        barrier.wait();
                        assert_eq!(WorktreeTrust::current(), trust);
                    });
                });
            }
        });
    }

    #[test]
    fn a_delegated_child_receives_only_the_request_pins() {
        let mut child = Command::new("unused");
        child.env("MDKG_TRUSTED_COMMON_DIR", "/stale");
        child.env("MDKG_TRUSTED_WORKTREE", "/stale");
        WorktreeTrust::default().apply_to(&mut child);
        for name in ["MDKG_TRUSTED_COMMON_DIR", "MDKG_TRUSTED_WORKTREE"] {
            assert_eq!(child.get_envs().find(|(k, _)| *k == name).unwrap().1, None);
        }
        let trust = WorktreeTrust {
            common: Some("/explicit".into()),
            worktree: None,
        };
        trust.apply_to(&mut child);
        assert_eq!(
            child
                .get_envs()
                .find(|(k, _)| *k == "MDKG_TRUSTED_COMMON_DIR")
                .unwrap()
                .1,
            Some(std::ffi::OsStr::new("/explicit"))
        );
    }
}
