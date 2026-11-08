//! Hardened Git commands, trusted worktrees and bounded subprocess I/O.
//!
//! Shared by the index and coordinator without opening coordinator state.
//! Request trust and cached checkout/executable identities live here once
//! per process, including callers using the coordinator's compatibility API.

use std::fs;
use std::io::{self, Read, Write as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub mod children;

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

/// A top folder bound by [`with_checkout`], with the check commands there
/// use; `None` once a fresh check of it, or of a folder inside it, failed
/// or placed it in another worktree. Reads of the top then check it
/// fresh, not through the shared cache, until a passing check whose top is
/// the bound top binds it again.
type Binding = (WorktreeTrust, PathBuf, Option<Checkout>);

thread_local! {
    /// Checks bound to this thread by [`with_checkout`], innermost last.
    static BOUND: std::cell::RefCell<Vec<Binding>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with every git command and object reader opened on this
/// thread for `checkout.top` using exactly this check, not whatever
/// the shared cache holds for that folder by then: another thread's check
/// can replace that entry between this thread's check and its read.
/// Bound for this trust context only; other folders, and other threads,
/// go through the cache as before. Nested scopes restore their parent on
/// exit.
///
/// The binding matches `checkout.top` exactly, so the reads in `f` must
/// name that folder. While it holds:
/// - if that path now leads somewhere else (it was replaced by a link),
///   git commands, object readers and [`common_dir`] naming it are refused
///   rather than run wherever it leads now;
/// - if it is gone, reads still use the bound check: git commands fail,
///   having no folder to run in, while [`common_dir`] returns the bound
///   common dir, and [`worktrees`] and [`main_ref`] read that common dir
///   and the environment as they are now.
///
/// A worktree check on this thread ([`check_worktree`], and the one inside
/// [`ready_to_write`]) updates the binding, comparing resolved paths:
/// - a passing check whose top is the bound top replaces the binding;
/// - a check of the top or a folder inside it that fails, or that places
///   the top in an enclosing worktree, ends it;
/// - any other check leaves it, including a failing check of a path that
///   now resolves outside the top. If the top itself now leads there,
///   reads naming it are refused, as above.
///
/// A refusal that comes after the check passed, such as [`ready_to_write`]
/// rejecting a branch name, leaves the replaced binding. Once a binding has
/// ended, reads of the top check fresh each time, not through the shared
/// cache, and are no longer refused for a link; a later passing check
/// whose top is the bound top binds it again.
pub fn with_checkout<T>(checkout: &Checkout, f: impl FnOnce() -> T) -> T {
    struct Unbind;
    impl Drop for Unbind {
        fn drop(&mut self) {
            BOUND.with(|b| b.borrow_mut().pop());
        }
    }
    BOUND.with(|b| {
        b.borrow_mut().push((
            WorktreeTrust::current(),
            checkout.top.clone(),
            Some(checkout.clone()),
        ))
    });
    let _unbind = Unbind;
    f()
}

/// The trusted worktree `dir` is in, checking it the first time.
fn checkout_for(dir: &Path) -> io::Result<Checkout> {
    let trust = WorktreeTrust::current();
    let bound = BOUND.with(|b| {
        let bound = b.borrow();
        if bound.is_empty() {
            return None;
        }
        let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        // Named as it was checked but no longer there (the folder became a
        // link elsewhere): refused, not read from wherever it leads now.
        bound
            .iter()
            .rev()
            .find(|(t, top, _)| *t == trust && (*top == real || top == dir))
            .map(|(_, top, c)| match c {
                Some(c) if *top == real => Ok(Some(c.clone())),
                Some(_) => Err(failure(format!(
                    "{}: it now resolves to {}, not the worktree checked for this read",
                    dir.display(),
                    real.display()
                ))),
                None => Ok(None),
            })
    });
    match bound {
        Some(Ok(Some(checkout))) => Ok(checkout),
        Some(Err(e)) => Err(e),
        // Ended by a fresh check: not the shared cache, which another
        // thread may have refilled with an older answer.
        Some(Ok(None)) => check_worktree_with_trust(dir, &trust),
        None => checked_worktree_with_trust(dir, &trust),
    }
}

/// The trusted worktree `dir` is in, for a read (decided 2026-10-31: cache
/// repeated checks within one process, for read paths only). Anything
/// immediately before a write calls [`check_worktree`] or
/// [`ready_to_write`] instead, which always check again.
///
/// - What's cached: the [`Checkout`] a check returned (its folders, object
///   format and the hooks its config defines), for this canonical folder
///   and trust context. A hook defined after the check changes nothing a
///   read does, because every command is hardened (`core.hooksPath`
///   is `/dev/null`, fsmonitor off, no optional locks) and switches off
///   each of [`HOOK_EVENTS`] by name, and the reads mdkg runs fire none of
///   them. A change to either keeps that true, or this cache isn't safe.
/// - For how long: this process, until [`fresh_reads`] clears it. A
///   long-running process (a supervisor's turns, a server's requests)
///   must call it at the start of each turn or request.
/// - What else invalidates it: every fresh check of the folder replaces
///   its entry, and a fresh check that fails removes it, with every entry
///   for a folder inside that worktree.
pub fn checked_worktree(dir: &Path) -> io::Result<Checkout> {
    checked_worktree_with_trust(dir, &WorktreeTrust::current())
}

/// [`checked_worktree`] with exactly these pins.
pub fn checked_worktree_with_trust(dir: &Path, trust: &WorktreeTrust) -> io::Result<Checkout> {
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let known = CHECKOUTS.lock().ok().and_then(|list| {
        list.iter()
            .find(|(d, t, _)| *d == real && t == trust)
            .map(|(_, _, c)| c.clone())
    });
    match known {
        Some(checkout) => Ok(checkout),
        None => check_worktree_with_trust(dir, trust),
    }
}

/// Forgets every check [`checked_worktree`] would reuse, so the next read
/// of each folder checks it again.
pub fn fresh_reads() {
    FRESH_READS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut list) = CHECKOUTS.lock() {
        list.clear();
    }
    if let Ok(mut list) = VALUES.lock() {
        list.clear();
    }
    if let Ok(mut list) = PARSED.lock() {
        list.clear();
    }
}

/// A file's identity and last change, `None` when it's missing: a value
/// read from git is kept only while every file it was read from has the
/// same stamp.
type FileStamp = Option<(u64, u64, u64, i64, i64, i64, i64)>;

/// The stamp of `path`, or `None` when it can't be stamped (unreadable, or
/// a link, whose target the stamp wouldn't see): then nothing is kept.
fn file_stamp(path: &Path) -> Option<FileStamp> {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => None,
        Ok(m) => Some(Some((
            m.dev(),
            m.ino(),
            m.size(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        ))),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Some(None)
        }
        Err(_) => None,
    }
}

/// What a kept value was read for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueKey {
    /// A key of a repository's own config file ([`common_config`]).
    Config { common: PathBuf, key: String },
    /// What `purpose` reads of a ref ([`has_ref`], [`cached_ref`]), seen
    /// from a worktree's git folder.
    Ref {
        git_dir: PathBuf,
        name: String,
        purpose: &'static str,
    },
}

/// One value read from git, with the stamps of every file it was read
/// from, taken before the read.
struct KeptValue {
    key: ValueKey,
    common: PathBuf,
    stamps: Vec<(PathBuf, FileStamp)>,
    value: Option<String>,
}

/// Values read from git (decided 2026-11-04, e-gitcache: f-tests step 5's
/// remainder). Like [`checked_worktree`]'s cache, for this process until
/// [`fresh_reads`]; and a value is used again only while every file it was
/// read from is unchanged, so a ref, HEAD or config written by anyone
/// (mdkg's writer or another git) is read again. [`ready_to_write`] drops
/// the repository's values outright.
static VALUES: Mutex<Vec<KeptValue>> = Mutex::new(Vec::new());

/// The stamps of `paths`, or `None` if one can't be stamped.
fn stamps_of(paths: Vec<PathBuf>) -> Option<Vec<(PathBuf, FileStamp)>> {
    paths
        .into_iter()
        .map(|p| file_stamp(&p).map(|s| (p, s)))
        .collect()
}

/// The value kept for `key`, if every file it was read from is unchanged.
fn kept_value(key: &ValueKey) -> Option<Option<String>> {
    let list = VALUES.lock().ok()?;
    let kept = list.iter().find(|k| k.key == *key)?;
    kept.stamps
        .iter()
        .all(|(path, stamp)| file_stamp(path).as_ref() == Some(stamp))
        .then(|| kept.value.clone())
}

fn keep_value(
    key: ValueKey,
    common: &Path,
    stamps: Vec<(PathBuf, FileStamp)>,
    value: Option<String>,
) {
    if let Ok(mut list) = VALUES.lock() {
        list.retain(|k| k.key != key);
        list.push(KeptValue {
            key,
            common: common.to_path_buf(),
            stamps,
            value,
        });
    }
}

/// Forgets every value kept for the repository whose common dir is
/// `common`.
fn forget_values(common: &Path) {
    if let Ok(mut list) = VALUES.lock() {
        list.retain(|k| k.common != common);
    }
}

/// Reads `key` through the value cache: the kept value while the files
/// `paths` names stamp the same, else `read`, kept when `paths` is `Some`,
/// every file stamps, and `read` says its answer is git's own (a git that
/// failed to run, timed out or erred answers nothing to keep). Stamped
/// before reading, so a change during the read is seen next time.
fn cached_value(
    key: ValueKey,
    common: &Path,
    paths: Option<Vec<PathBuf>>,
    read: impl FnOnce() -> (Option<String>, bool),
) -> Option<String> {
    if paths.is_some()
        && let Some(value) = kept_value(&key)
    {
        return value;
    }
    let stamps = paths.and_then(stamps_of);
    let (value, answered) = read();
    if answered && let Some(stamps) = stamps {
        keep_value(key, common, stamps, value.clone());
    }
    value
}

/// How many times [`fresh_reads`] has run in this process.
static FRESH_READS: AtomicU64 = AtomicU64::new(0);

/// How many times this process called [`fresh_reads`], for tests that
/// check a loop resets each turn or request.
pub fn fresh_reads_count() -> u64 {
    FRESH_READS.load(Ordering::Relaxed)
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
/// unless fed. The worktree is checked first, through the read cache
/// ([`checked_worktree`]), and then named outright; a worktree that fails
/// the check runs nothing. A write (staging, committing, merging, updating
/// a ref) calls [`ready_to_write`] or [`check_worktree`] first, which check
/// again and leave the fresh result for this command to use.
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

/// A git command against a common directory itself (`common_dir`'s
/// answer), with no work tree: for object reads such as resolving a SHA or
/// checking ancestry, which every worktree shares and no checkout's own
/// state may change (the ledger's SHA checks, edd-01a1878e A27 rule 5).
/// Hardened as [`command`] is, with `--git-dir` named outright and the
/// hooks the common config defines switched off, with every event's; a
/// common directory whose hooks can't be read runs nothing.
pub fn common_command(common: &Path) -> Git {
    let mut command = hardened(common);
    match config_hooks(common, common) {
        Ok(hooks) => {
            switch_off_hooks(&mut command, &hooks);
            command.arg("--git-dir").arg(common);
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

/// A hardened git command in the bare repository `git_dir`, with no
/// worktree check: for a repository mdkg writes and checks itself, such as
/// the push runner's private one (S38, orchestrator on #10729). It has
/// [`command`]'s hardening (no inherited `GIT_*` location, hooks off, no
/// lazy fetch, no replace objects, grafts or shallow file, and no
/// transport), with `--git-dir` named outright, and no system or global
/// config (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`), so only
/// the repository's own config, which its writer checks, applies (Terra 2
/// #10767). A relative `git_dir` runs nothing.
pub fn bare_command(git_dir: &Path) -> Git {
    let mut command = hardened(git_dir);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    switch_off_hooks(&mut command, &[]);
    command.arg("--git-dir").arg(git_dir);
    let refused = (!git_dir.is_absolute()).then(|| {
        failure(format!(
            "{} isn't an absolute git folder",
            git_dir.display()
        ))
    });
    Git { command, refused }
}

/// What [`Git::ssh_push`] sets through git's config environment, beside
/// the hooks [`switch_off_hooks`] switches off: S38's push settings.
const SSH_PUSH_CONFIG: [(&str, &str); 3] = [
    ("protocol.allow", "never"),
    ("protocol.ssh.allow", "always"),
    ("push.recurseSubmodules", "no"),
];

/// The inherited `GIT_*` and `SSH_*` variables `cmd` doesn't set itself:
/// each is removed for an ssh push.
fn inherited_git_and_ssh(
    cmd: &Command,
    inherited: impl Iterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    let set: Vec<&std::ffi::OsStr> = cmd.get_envs().map(|(k, _)| k).collect();
    inherited
        .filter(|k| {
            let b = k.as_encoded_bytes();
            (b.starts_with(b"GIT_") || b.starts_with(b"SSH_")) && !set.contains(&k.as_os_str())
        })
        .collect()
}

/// Appends one entry to git's config environment (`GIT_CONFIG_COUNT`,
/// `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`), after those set already.
fn add_config(cmd: &mut Command, key: &str, value: &str) {
    let count: usize = cmd
        .get_envs()
        .find(|(k, _)| *k == "GIT_CONFIG_COUNT")
        .and_then(|(_, v)| v?.to_str()?.parse().ok())
        .unwrap_or(0);
    cmd.env(format!("GIT_CONFIG_KEY_{count}"), key)
        .env(format!("GIT_CONFIG_VALUE_{count}"), value)
        .env("GIT_CONFIG_COUNT", (count + 1).to_string());
}

impl Git {
    /// Lets this one command push over ssh, and over nothing else, with
    /// S38's settings: `GIT_ALLOW_PROTOCOL=ssh` in place of `none`;
    /// `protocol.allow=never`, `protocol.ssh.allow=always` and
    /// `push.recurseSubmodules=no`; no system or global config
    /// (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`); every
    /// inherited `GIT_*` and `SSH_*` variable removed; and
    /// `GIT_SSH_COMMAND` set to `ssh_command`, which the caller builds
    /// (it names the identity and known hosts). Opt-in: no other command
    /// gets a transport. With `SSH_AUTH_SOCK` gone too, ssh reaches no
    /// agent: the identity must be a key file usable without one, and
    /// without a passphrase under `BatchMode`.
    pub fn ssh_push(mut self, ssh_command: &str) -> Self {
        let inherited = std::env::vars_os().map(|(k, _)| k);
        for k in inherited_git_and_ssh(&self.command, inherited) {
            self.command.env_remove(k);
        }
        for (key, value) in SSH_PUSH_CONFIG {
            add_config(&mut self.command, key, value);
        }
        self.command
            .env("GIT_ALLOW_PROTOCOL", "ssh")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GIT_SSH_COMMAND", ssh_command);
        self
    }
}

/// The inherited `GIT_*` variables a [`user_config_command`] or
/// [`network_command`] keeps: the user's own ssh setup.
const USER_SSH: &[&str] = &["GIT_SSH", "GIT_SSH_COMMAND", "GIT_SSH_VARIANT"];

/// Git as the user's own `git push` from the checkout `top` (its own git
/// folder `git_dir`, of the clone whose common folder is `common`) sees its
/// configuration: [`common_command`]'s hardening (bounded, outputs
/// drained, stdin closed, a process group of its own, no fsmonitor,
/// replace objects or grafts), with the user's setup given back:
///
/// - no hook switched off (`GIT_CONFIG_COUNT=0`; the hooks path stays
///   `/dev/null` until [`network_command`] names the user's);
/// - `GIT_ALLOW_PROTOCOL` removed, so the user's `protocol.*` config
///   decides;
/// - every inherited `GIT_*` variable cleared but `GIT_SSH`,
///   `GIT_SSH_COMMAND` and `GIT_SSH_VARIANT` (so `GIT_CONFIG_GLOBAL` and
///   friends don't apply);
/// - nothing can prompt: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=Never`,
///   `SSH_ASKPASS_REQUIRE=never`, `GIT_ASKPASS=false`.
///
/// Every read of remote configuration that graph sync's approval and
/// guard make goes through this one view, as the network command does, so
/// what's approved is what's contacted. The caller verifies `top` and
/// `git_dir` belong to `common` (graph sync's `checkout`).
pub fn user_config_command(common: &Path, git_dir: &Path, top: &Path) -> Git {
    let mut git = common_command(common)
        // Cancels `switch_off_hooks`'s entries: no hook is switched off.
        .env("GIT_CONFIG_COUNT", "0")
        .env_remove("GIT_ALLOW_PROTOCOL")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_ASKPASS", "false");
    for (key, _) in std::env::vars_os() {
        let b = key.as_encoded_bytes();
        if b.starts_with(b"GIT_")
            && !USER_SSH.iter().any(|k| key == *k)
            && !["GIT_TERMINAL_PROMPT", "GIT_ASKPASS", "GIT_CONFIG_COUNT"]
                .iter()
                .any(|k| key == *k)
        {
            git = git.env_remove(&key);
        }
    }
    // A later `-C` (absolute), `--git-dir` and `--work-tree` set where it
    // runs.
    git.arg("-C")
        .arg(top)
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(top)
}

/// A git command that talks to a remote for graph sync (edd-01a18867, G9):
/// [`user_config_command`], plus
///
/// - **hooks run**: the user's `core.hooksPath` (a relative one made
///   absolute against `top`, as a plain `git push` from there finds it;
///   `~/` and `%(prefix)/` left for git to expand), or the clone's `hooks`
///   folder, in place of `/dev/null`, so `pre-push` runs;
/// - with no ssh setup of the user's (`GIT_SSH`, `GIT_SSH_COMMAND` or
///   `core.sshCommand`), `ssh -o BatchMode=yes -o ConnectTimeout=15`;
/// - `transfer.fsckObjects=true`.
///
/// The user's value of a key is the last one git reads outside the
/// hardened runner's own `-c` (scope `command`), in the same view.
/// `common` must be canonical, as the caller's checkout check leaves it.
pub fn network_command(common: &Path, git_dir: &Path, top: &Path) -> io::Result<Git> {
    let hooks = network_hooks_path(common, git_dir, top)?;
    let mut git = user_config_command(common, git_dir, top);
    let user_ssh = USER_SSH[..2]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
        || user_value(common, git_dir, top, "core.sshCommand")?.is_some();
    if !user_ssh {
        git = git.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        );
    }
    // A later `-c` wins over the hardened runner's.
    Ok(git.args([
        "-c",
        &format!("core.hooksPath={hooks}"),
        "-c",
        "transfer.fsckObjects=true",
    ]))
}

/// The user's value of `key`, as [`network_command`] reads it: the last
/// one git reads in [`user_config_command`]'s view outside the hardened
/// runner's own `-c` (scope `command`), trimmed; `None` when unset or empty.
fn user_value(common: &Path, git_dir: &Path, top: &Path, key: &str) -> io::Result<Option<String>> {
    let out = user_config_command(common, git_dir, top)
        .args(["config", "--show-scope", "--get-all", key])
        .run(None)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(scope, _)| *scope != "command")
        .map(|(_, v)| v.trim().to_owned())
        .next_back()
        .filter(|v| !v.is_empty()))
}

/// The hooks folder [`network_command`] runs hooks from, as it passes it
/// to git: the user's `core.hooksPath` (a relative one made absolute
/// against `top`; `~/` and `%(prefix)/` left for git to expand), or the
/// clone's `hooks` folder.
pub fn network_hooks_path(common: &Path, git_dir: &Path, top: &Path) -> io::Result<String> {
    Ok(match user_value(common, git_dir, top, "core.hooksPath")? {
        Some(p) if Path::new(&p).is_absolute() || p.starts_with('~') || p.starts_with("%(") => p,
        Some(p) => top.join(p).to_string_lossy().into_owned(),
        None => common.join("hooks").to_string_lossy().into_owned(),
    })
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
    /// Reads only the repository's own config: no global or system file
    /// (`GIT_CONFIG_*` overrides are always cleared).
    pub fn own_config_only(mut self) -> Self {
        self.command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_SYSTEM");
        self
    }

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

    /// Sets one environment variable for this command only (a later
    /// `env` of the same key wins).
    pub fn env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        self.command.env(key, value);
        self
    }

    /// Removes one inherited environment variable for this command only.
    pub fn env_remove(mut self, key: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env_remove(key);
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

thread_local! {
    /// Git commands this thread started through [`run`].
    static RUNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many git commands this thread has started, for tests that check a
/// read is served from [`checked_worktree`]'s cache or the value cache.
#[doc(hidden)]
pub fn git_runs() -> u64 {
    RUNS.with(std::cell::Cell::get)
}

/// Runs `cmd` as [`Git::run`] describes.
fn run(mut cmd: Command, input: Option<&[u8]>) -> io::Result<std::process::Output> {
    RUNS.with(|n| n.set(n.get() + 1));
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let tracked = children::spawn(cmd.stdout(Stdio::piped()).stderr(Stdio::piped()))?;
    let pid = tracked.id();
    let (stdin, stdout, stderr) = {
        let mut child = tracked.child();
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    };
    let writer = match (input, stdin) {
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
    let stdout = collect(stdout.map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = collect(stderr.map(|p| Box::new(p) as Box<dyn Read + Send>));
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
        let waited = tracked.child().try_wait()?;
        match waited {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                stop(&mut tracked.child());
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
/// Always checks again: a failure also drops what [`checked_worktree`]
/// kept for the folder and for any folder inside its worktree.
pub fn check_worktree_with_trust(dir: &Path, trust: &WorktreeTrust) -> io::Result<Checkout> {
    let checked = check_fresh(dir, trust);
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if checked.is_err()
        && let Ok(mut list) = CHECKOUTS.lock()
    {
        list.retain(|(d, _, c)| *d != real && !real.starts_with(&c.top));
    }
    // This thread's bindings follow the fresh answer too.
    BOUND.with(|b| {
        for (t, top, bound) in b.borrow_mut().iter_mut() {
            if t != trust {
                continue;
            }
            match &checked {
                Ok(fresh) if *top == fresh.top => *bound = Some(fresh.clone()),
                // The folder, or one inside it, now belongs to a worktree
                // that contains the bound top: the binding no longer holds.
                Ok(fresh) if real.starts_with(&*top) && top.starts_with(&fresh.top) => {
                    *bound = None
                }
                Err(_) if real.starts_with(&*top) => *bound = None,
                _ => {}
            }
        }
    });
    checked
}

fn check_fresh(dir: &Path, trust: &WorktreeTrust) -> io::Result<Checkout> {
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

/// An object's identity, as `cat-file` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub oid: String,
    pub kind: String,
    pub size: u64,
}

/// One entry of a tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The mode as git writes it (`100644`, `100755`, `120000`, `40000`,
    /// `160000`).
    pub mode: String,
    /// The name, as bytes: git doesn't require UTF-8.
    pub name: Vec<u8>,
    pub oid: String,
}

/// One long-lived `git cat-file --batch-command` for a worktree's
/// objects, so a command's object and tree reads share one process
/// instead of starting one each (decided 2026-10-31). An object can't
/// change, so nothing it reads goes stale; its worktree is found through
/// [`command`], the read cache, so a caller that writes checks again
/// before writing ([`ready_to_write`]).
///
/// Each request is bounded by the same time limit as any git command:
/// past it, git's whole group is killed, and the reader refuses every
/// later request, as it does after any error that could leave its stream
/// out of step. An object is named with no newline or NUL, so a name read
/// from a tree can be refused: walk trees by each entry's OID. Writing a
/// request isn't under the limit, but a request is one short line, sent
/// only once the last one is answered, to a git whose output is always
/// drained. Dropping the reader kills its git; a long-running loop must
/// drop its readers each turn or request, when it calls [`fresh_reads`].
#[derive(Debug)]
pub struct ObjectReader {
    child: children::Tracked,
    stdin: Option<std::process::ChildStdin>,
    chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    limit: Duration,
    deadline: Instant,
    oid_bytes: usize,
    broken: bool,
}

impl ObjectReader {
    /// Starts the reader for the worktree `dir` is in.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let oid_bytes = match checkout_for(dir)?.empty_tree {
            EMPTY_TREE_SHA256 => 32,
            _ => 20,
        };
        let git = command(dir).args(["cat-file", "--batch-command"]);
        if let Some(refused) = git.refused {
            return Err(refused);
        }
        Self::spawned(git.command, oid_bytes, time_limit())
    }

    fn spawned(mut command: Command, oid_bytes: usize, limit: Duration) -> io::Result<Self> {
        let child = children::spawn(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )?;
        let (stdin, stdout) = {
            let mut child = child.child();
            (child.stdin.take(), child.stdout.take())
        };
        let Some(mut stdout) = stdout else {
            return Err(failure("cat-file has no output"));
        };
        // Bounded, so an object refused for its size buffers at most a
        // few chunks before git is killed; git waits while it's full.
        let (send, chunks) = mpsc::sync_channel(4);
        std::thread::spawn(move || {
            let mut chunk = vec![0; 64 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if send.send(Ok(chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        send.send(Err(e)).ok();
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            chunks,
            buf: Vec::new(),
            limit,
            deadline: Instant::now(),
            oid_bytes,
            broken: false,
        })
    }

    /// An object's identity, or `None` if there's no such object.
    pub fn info(&mut self, object: &str) -> io::Result<Option<ObjectInfo>> {
        self.request("info", object, |_, info| Ok(info))
    }

    /// An object's identity and contents, or `None` if there's no such
    /// object. One larger than `cap` bytes is an error.
    pub fn contents(
        &mut self,
        object: &str,
        cap: u64,
    ) -> io::Result<Option<(ObjectInfo, Vec<u8>)>> {
        self.request("contents", object, |reader, info| {
            if info.size > cap {
                return Err(failure(format!(
                    "object {} is {} bytes, over the {cap} allowed",
                    info.oid, info.size
                )));
            }
            let size = usize::try_from(info.size).map_err(|_| failure("object too large"))?;
            let data = reader.take(size)?;
            if reader.take(1)? != b"\n" {
                return Err(failure("cat-file's output is out of step"));
            }
            Ok((info, data))
        })
    }

    /// A tree's identity and entries, or `None` if there's no such object.
    /// Anything but a tree, or one larger than `cap` bytes, is an error.
    pub fn tree(
        &mut self,
        object: &str,
        cap: u64,
    ) -> io::Result<Option<(ObjectInfo, Vec<TreeEntry>)>> {
        let Some((info, data)) = self.contents(object, cap)? else {
            return Ok(None);
        };
        if info.kind != "tree" {
            return Err(failure(format!(
                "{} is a {}, not a tree",
                info.oid, info.kind
            )));
        }
        let entries = parse_tree(&data, self.oid_bytes)?;
        Ok(Some((info, entries)))
    }

    fn request<T>(
        &mut self,
        verb: &str,
        object: &str,
        body: impl FnOnce(&mut Self, ObjectInfo) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        if object.is_empty() || object.contains(['\n', '\0']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an object name can't be empty or hold a newline or NUL",
            ));
        }
        if self.broken {
            return Err(failure("this object reader stopped after an error"));
        }
        self.deadline = Instant::now() + self.limit;
        let answer = self.answer(verb, object, body);
        if answer.is_err() {
            self.stop();
        }
        answer
    }

    fn answer<T>(
        &mut self,
        verb: &str,
        object: &str,
        body: impl FnOnce(&mut Self, ObjectInfo) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(failure("cat-file has no input"));
        };
        stdin.write_all(format!("{verb} {object}\n").as_bytes())?;
        stdin.flush()?;
        let header = self.line()?;
        let header =
            String::from_utf8(header).map_err(|_| failure("unreadable cat-file header"))?;
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            return Ok(None);
        }
        let fields: Vec<&str> = header.split(' ').collect();
        let [oid, kind, size] = fields[..] else {
            return Err(failure("unexpected cat-file header"));
        };
        let info = ObjectInfo {
            oid: oid.to_string(),
            kind: kind.to_string(),
            size: size
                .parse()
                .map_err(|_| failure("unreadable object size"))?,
        };
        body(self, info).map(Some)
    }

    /// Waits, until the request's deadline, for more of git's output.
    fn fill(&mut self) -> io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        match self.chunks.recv_timeout(left) {
            Ok(chunk) => {
                self.buf.extend_from_slice(&chunk?);
                Ok(())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git didn't answer within {} ms", self.limit.as_millis()),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(failure("cat-file stopped")),
        }
    }

    /// The next line, without its newline.
    fn line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(at) = self.buf.iter().position(|&b| b == b'\n') {
                let line = self.buf[..at].to_vec();
                self.buf.drain(..=at);
                return Ok(line);
            }
            self.fill()?;
        }
    }

    /// The next `n` bytes.
    fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.buf.len() < n {
            self.fill()?;
        }
        Ok(self.buf.drain(..n).collect())
    }

    fn stop(&mut self) {
        self.broken = true;
        self.stdin = None;
        signal_group(self.child.id(), "-KILL");
        let mut child = self.child.child();
        child.kill().ok();
        child.wait().ok();
    }
}

impl Drop for ObjectReader {
    fn drop(&mut self) {
        if !self.broken {
            self.stop();
        }
    }
}

/// A tree object's entries: `<mode> <name>\0<oid bytes>`, repeated.
fn parse_tree(data: &[u8], oid_bytes: usize) -> io::Result<Vec<TreeEntry>> {
    let bad = || failure("a tree object can't be read");
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|&b| b == b' ').ok_or_else(bad)?;
        let mode = std::str::from_utf8(&rest[..space]).map_err(|_| bad())?;
        if mode.is_empty() || !mode.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        rest = &rest[space + 1..];
        let nul = rest.iter().position(|&b| b == 0).ok_or_else(bad)?;
        let name = rest[..nul].to_vec();
        rest = &rest[nul + 1..];
        if name.is_empty() || rest.len() < oid_bytes {
            return Err(bad());
        }
        let oid = rest[..oid_bytes]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        rest = &rest[oid_bytes..];
        entries.push(TreeEntry {
            mode: mode.to_string(),
            name,
            oid,
        });
    }
    Ok(entries)
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

/// `mdkg.leaseScope` in the repository's own config (never a worktree's
/// `config.worktree`, which an agent can write), trimmed, when set: where
/// this clone's path leases hold, without a tracked change.
pub fn lease_scope(dir: &Path) -> Option<String> {
    checkout_for(dir)
        .ok()
        .and_then(|c| common_config(&c.common, "mdkg.leaseScope"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The largest config file the value cache reads to look for includes.
const KEPT_CONFIG_LIMIT: u64 = 1 << 20;

/// A value from the repository's own config file only, kept while that
/// file is unchanged, unless it includes another file (which its stamp
/// can't see).
fn common_config(common: &Path, key: &str) -> Option<String> {
    let file = common.join("config");
    let paths = read_capped(&file, KEPT_CONFIG_LIMIT + 1)
        .ok()
        .filter(|text| {
            text.len() as u64 <= KEPT_CONFIG_LIMIT
                && !String::from_utf8_lossy(text)
                    .to_ascii_lowercase()
                    .contains("include")
        })
        .map(|_| vec![file]);
    let key_ = ValueKey::Config {
        common: common.to_path_buf(),
        key: key.to_string(),
    };
    cached_value(key_, common, paths, || read_common_config(common, key))
}

/// The value, and whether git answered (`config --get` exits 0, or 1 for
/// a key that isn't set): a git that couldn't run or failed otherwise
/// answers nothing to keep.
fn read_common_config(common: &Path, key: &str) -> (Option<String>, bool) {
    let mut c = hardened(common);
    c.args(["config", "--file"])
        .arg(common.join("config"))
        .args(["--get", key]);
    let Ok(out) = run(c, None) else {
        return (None, false);
    };
    match out.status.code() {
        Some(0) => match String::from_utf8(out.stdout) {
            Ok(v) => (Some(v.trim_end_matches(['\n', '\r']).to_string()), true),
            Err(_) => (None, false),
        },
        Some(1) => (None, true),
        _ => (None, false),
    }
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
    ref_storage_config_with(common, hardened(common), true)
}

/// Config snapshots git has parsed in this process, with its listing: the
/// listing depends on the bytes alone (no repository, global, system or
/// included config is read), so the same bytes are parsed once. Cleared by
/// [`fresh_reads`]; a handful at most are kept.
static PARSED: Mutex<Vec<(Vec<u8>, Vec<u8>)>> = Mutex::new(Vec::new());

const PARSED_KEPT: usize = 8;

fn parsed_listing(bytes: &[u8]) -> Option<Vec<u8>> {
    PARSED
        .lock()
        .ok()?
        .iter()
        .find(|(b, _)| b == bytes)
        .map(|(_, listing)| listing.clone())
}

fn keep_listing(bytes: &[u8], listing: &[u8]) {
    if let Ok(mut list) = PARSED.lock() {
        list.retain(|(b, _)| b != bytes);
        if list.len() >= PARSED_KEPT {
            list.remove(0);
        }
        list.push((bytes.to_vec(), listing.to_vec()));
    }
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

/// Parses a config file's snapshot, as `git config --list --null` gives
/// it, in an isolated Git that reads no repository, global, system or
/// included config (the same setup the ref-storage check uses). `dir` is
/// only the working folder; nothing in it is read. Fails closed on any
/// parse error.
pub fn parse_config_snapshot(dir: &Path, bytes: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(listing) = parsed_listing(bytes) {
        return Ok(listing);
    }
    let mut cmd = hardened(dir);
    cmd.env("GIT_DIR", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["config", "--file", "-", "--no-includes", "--null", "--list"]);
    let out = run(cmd, Some(bytes))?;
    if !out.status.success() {
        // Config values and Git's error text may be sensitive; never echo them.
        return Err(failure(format!(
            "git couldn't parse a config snapshot (exit {:?})",
            out.status.code()
        )));
    }
    keep_listing(bytes, &out.stdout);
    Ok(out.stdout)
}

#[cfg(test)]
fn ref_storage_config(common: &Path, cmd: Command) -> io::Result<()> {
    ref_storage_config_with(common, cmd, false)
}

/// [`ref_storage_config`], reusing a listing of the same bytes when `keep`.
fn ref_storage_config_with(common: &Path, mut cmd: Command, keep: bool) -> io::Result<()> {
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
    if keep && let Some(listing) = parsed_listing(&bytes) {
        return files_ref_storage(&listing);
    }
    let out = run(cmd, Some(&bytes))?;
    if !out.status.success() {
        // Config values and Git's error text may be sensitive; never echo them.
        return Err(failure(format!(
            "git couldn't parse {} for ref storage (exit {:?})",
            config.display(),
            out.status.code()
        )));
    }
    if keep {
        keep_listing(&bytes, &out.stdout);
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
    // What's written next changes refs or config: read them again after.
    forget_values(&checkout.common);
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
    let value = common_config(common, "core.worktree")?;
    fs::canonicalize(common.join(value)).ok()
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

/// Whether `refname` names a commit. A yes is kept while every file git
/// could have resolved it from is unchanged ([`ref_files`]); a no is asked
/// again each time.
pub fn has_ref(repo: &Path, refname: &str) -> bool {
    let Ok(checkout) = checkout_for(repo) else {
        return read_has_ref(repo, refname).unwrap_or(false);
    };
    let key = ValueKey::Ref {
        git_dir: checkout.git_dir.clone(),
        name: refname.to_string(),
        purpose: "has_ref",
    };
    let paths = ref_files(&checkout, refname);
    // Only a found commit is kept: `rev-parse --verify` exits 1 both for
    // a missing ref and for one whose commit can't be read now (an
    // alternate that's away), and the stamps see refs, not objects.
    cached_value(key, &checkout.common, paths, || {
        match read_has_ref(repo, refname) {
            Some(true) => (Some(String::new()), true),
            _ => (None, false),
        }
    })
    .is_some()
}

/// What `read` answers about the ref `name` in the worktree `repo` is in,
/// kept as [`has_ref`]'s answers are: while every file git could resolve
/// the name from is unchanged ([`ref_files`]), until [`fresh_reads`] or
/// [`ready_to_write`]. Only an `Ok` answer is kept; an error is read again
/// next time. `purpose` names what `read` reads, so two readers of one
/// ref never share an answer. A caller whose read depends on anything but
/// the ref (the objects it names aside) must not use this.
pub fn cached_ref<E>(
    repo: &Path,
    name: &str,
    purpose: &'static str,
    read: impl FnOnce() -> Result<String, E>,
) -> Result<String, E> {
    let Ok(checkout) = checkout_for(repo) else {
        return read();
    };
    let key = ValueKey::Ref {
        git_dir: checkout.git_dir.clone(),
        name: name.to_string(),
        purpose,
    };
    let paths = ref_files(&checkout, name);
    if paths.is_some()
        && let Some(Some(value)) = kept_value(&key)
    {
        return Ok(value);
    }
    let stamps = paths.and_then(stamps_of);
    let answer = read();
    if let (Ok(value), Some(stamps)) = (&answer, stamps) {
        keep_value(key, &checkout.common, stamps, Some(value.clone()));
    }
    answer
}

/// Every file git could resolve `name` from in this worktree: its HEAD,
/// the packed refs, and the loose ref each of git's lookup rules names, in
/// the worktree's git folder and the common one, with the ref a symbolic
/// one among them leads to. `None` (nothing is kept) for anything but a
/// plain ref name: an object name, an expression, a name all of hex
/// digits, a symbolic ref leading to another, or reftable storage.
fn ref_files(checkout: &Checkout, name: &str) -> Option<Vec<PathBuf>> {
    let plain = |name: &str| {
        (1..=200).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
            && !name.starts_with(['-', '/', '.'])
            && !name.ends_with(['/', '.'])
            && !name.contains("..")
            && !name.contains("//")
            && !name.contains("/.")
            && !name.bytes().all(|b| b.is_ascii_hexdigit())
            // Another worktree's refs, resolved through its own git folder
            // (or the primary's `<common>/HEAD`), which isn't stamped.
            && !name.starts_with("main-worktree/")
            && !name.starts_with("worktrees/")
    };
    if !plain(name) || fs::symlink_metadata(checkout.common.join("reftable")).is_ok() {
        return None;
    }
    let dirs = [&checkout.git_dir, &checkout.common];
    let named = |name: &str| -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for rule in ["", "refs/", "refs/tags/", "refs/heads/", "refs/remotes/"] {
            for dir in dirs {
                paths.push(dir.join(format!("{rule}{name}")));
            }
        }
        for dir in dirs {
            paths.push(dir.join(format!("refs/remotes/{name}/HEAD")));
        }
        paths
    };
    let mut paths = vec![
        checkout.git_dir.join("HEAD"),
        checkout.common.join("packed-refs"),
    ];
    paths.extend(named(name));
    // A symbolic ref leads to another: that one's files too, one step.
    let mut led = Vec::new();
    for path in &paths {
        let Some(target) = symbolic_target(path)? else {
            continue;
        };
        if !target.starts_with("refs/") || !plain(&target) {
            return None;
        }
        for dir in dirs {
            let file = dir.join(&target);
            if symbolic_target(&file)?.is_some() {
                return None;
            }
            led.push(file);
        }
    }
    paths.extend(led);
    Some(paths)
}

/// The ref a symbolic ref file names (`ref: refs/heads/x`): `Some(None)`
/// when `path` is missing, a folder, or a ref naming a commit, and `None`
/// when it can't be told (a link, or a file that can't be read whole).
fn symbolic_target(path: &Path) -> Option<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Some(None),
        Ok(m) if m.is_file() => {
            let text = read_capped(path, 4096).ok()?;
            let text = String::from_utf8(text).ok()?;
            Some(
                text.trim_end()
                    .strip_prefix("ref: ")
                    .map(|t| t.trim().to_string()),
            )
        }
        Ok(_) => None,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Some(None)
        }
        Err(_) => None,
    }
}

/// Whether `refname` names a commit, as git answers (`rev-parse --verify
/// --quiet` exits 0 or 1); `None` when git didn't answer (it couldn't run,
/// timed out, or failed otherwise).
fn read_has_ref(repo: &Path, refname: &str) -> Option<bool> {
    let out = command(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ])
        .run(None)
        .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
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
                        "ref_storage_tests::ref_storage_ignores_inherited_config_environment",
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
                "ref_storage_tests::ref_storage_uses_the_bounded_runner",
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

#[cfg(test)]
mod bare_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("mdkg-bare-{}-{id}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            Self(fs::canonicalize(dir).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn env_of<'a>(c: &'a Command, key: &str) -> Option<Option<&'a std::ffi::OsStr>> {
        c.get_envs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` pairs, in order.
    fn config_env(c: &Command) -> Vec<(String, String)> {
        let n: usize = env_of(c, "GIT_CONFIG_COUNT")
            .flatten()
            .and_then(|v| v.to_str()?.parse().ok())
            .unwrap_or(0);
        (0..n)
            .map(|i| {
                let get = |k: String| {
                    env_of(c, &k)
                        .flatten()
                        .and_then(|v| v.to_str())
                        .unwrap()
                        .to_owned()
                };
                (
                    get(format!("GIT_CONFIG_KEY_{i}")),
                    get(format!("GIT_CONFIG_VALUE_{i}")),
                )
            })
            .collect()
    }

    /// A commit in `repo`, as a refspec pushing it to `refs/heads/x`: git
    /// resolves the refspec before it connects.
    fn commit_refspec(repo: &Path) -> String {
        let tree = bare_command(repo).arg("mktree").run(Some(b"")).unwrap();
        let tree = String::from_utf8(tree.stdout).unwrap();
        let commit = bare_command(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .args(["commit-tree", tree.trim(), "-m", "m"])
            .run(None)
            .unwrap();
        assert!(commit.status.success(), "{commit:?}");
        format!(
            "{}:refs/heads/x",
            String::from_utf8(commit.stdout).unwrap().trim()
        )
    }

    /// Whether git refused `out`'s push for its transport.
    fn transport_refused(out: &std::process::Output) -> bool {
        let err = String::from_utf8_lossy(&out.stderr);
        !out.status.success() && err.contains("transport") && err.contains("not allowed")
    }

    fn bare_repo(s: &Scratch, name: &str) -> PathBuf {
        let dir = s.0.join(name);
        let out = bare_command(&s.0)
            .args(["init", "-q", "--bare"])
            .arg(&dir)
            .run(None)
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        dir
    }

    #[test]
    fn a_bare_command_keeps_the_hardening_and_has_no_transport() {
        let s = Scratch::new();
        let repo = bare_repo(&s, "r.git");
        let git = bare_command(&repo);
        assert_eq!(
            env_of(&git.command, "GIT_ALLOW_PROTOCOL"),
            Some(Some("none".as_ref()))
        );
        assert_eq!(
            env_of(&git.command, "GIT_NO_LAZY_FETCH"),
            Some(Some("1".as_ref()))
        );
        assert_eq!(env_of(&git.command, "GIT_SSH_COMMAND"), None);
        assert!(
            config_env(&git.command).contains(&("hook.pre-push.enabled".into(), "false".into()))
        );
        // Terra 2 #10767: no system or global config, whatever HOME says.
        assert_eq!(
            env_of(&git.command, "GIT_CONFIG_NOSYSTEM"),
            Some(Some("1".as_ref()))
        );
        assert_eq!(
            env_of(&git.command, "GIT_CONFIG_GLOBAL"),
            Some(Some("/dev/null".as_ref()))
        );
        let home = s.0.join("home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join(".gitconfig"), "[user]\n\tname = leaked\n").unwrap();
        let leaked = bare_command(&repo)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .args(["config", "--get", "user.name"])
            .run(None)
            .unwrap();
        assert!(!leaked.status.success(), "{leaked:?}");
        assert!(leaked.stdout.is_empty());
        let out = git
            .args(["rev-parse", "--is-bare-repository"])
            .run(None)
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
        // No transport: a push to a local path is refused.
        let dest = bare_repo(&s, "dest.git");
        let out = bare_command(&repo)
            .args(["push", "--quiet"])
            .arg(&dest)
            .arg(commit_refspec(&repo))
            .run(None)
            .unwrap();
        assert!(transport_refused(&out), "{out:?}");
        // A relative folder runs nothing.
        assert!(
            bare_command(Path::new("r.git"))
                .arg("status")
                .run(None)
                .is_err()
        );
    }

    #[test]
    fn an_ssh_push_allows_ssh_alone_with_s38s_settings() {
        let s = Scratch::new();
        let repo = bare_repo(&s, "r.git");
        let git = bare_command(&repo).ssh_push("ssh -F none");
        let c = &git.command;
        assert_eq!(env_of(c, "GIT_ALLOW_PROTOCOL"), Some(Some("ssh".as_ref())));
        assert_eq!(env_of(c, "GIT_CONFIG_NOSYSTEM"), Some(Some("1".as_ref())));
        assert_eq!(
            env_of(c, "GIT_CONFIG_GLOBAL"),
            Some(Some("/dev/null".as_ref()))
        );
        assert_eq!(
            env_of(c, "GIT_SSH_COMMAND"),
            Some(Some("ssh -F none".as_ref()))
        );
        let config = config_env(c);
        for (k, v) in SSH_PUSH_CONFIG {
            assert!(config.contains(&(k.to_owned(), v.to_owned())), "{k}");
        }
        // The hooks stay off: the push settings come after them.
        assert!(config.contains(&("hook.pre-push.enabled".into(), "false".into())));
        // A local path is still refused: only ssh is a transport.
        let dest = bare_repo(&s, "dest.git");
        let out = bare_command(&repo)
            .ssh_push("false")
            .args(["push", "--quiet"])
            .arg(&dest)
            .arg(commit_refspec(&repo))
            .run(None)
            .unwrap();
        assert!(transport_refused(&out), "{out:?}");
        // An ssh URL runs the given command, and nothing else does.
        let refspec = commit_refspec(&repo);
        let record = s.0.join("ssh-args");
        let fake = s.0.join("fake-ssh");
        fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$@\" > '{}'\nexit 1\n", record.display()),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let out = bare_command(&repo)
            .ssh_push(fake.to_str().unwrap())
            .args([
                "push",
                "--quiet",
                "ssh://git@host.invalid/srv/r.git",
                &refspec,
            ])
            .run(None)
            .unwrap();
        assert!(!out.status.success());
        let args = fs::read_to_string(&record).unwrap();
        assert!(args.contains("git@host.invalid"), "{args}");
        assert!(args.contains("git-receive-pack"), "{args}");
    }

    #[test]
    fn an_ssh_push_drops_inherited_git_and_ssh_variables_but_not_its_own() {
        let c = bare_command(Path::new("/r.git")).command;
        let inherited = [
            "GIT_DIR",
            "SSH_AUTH_SOCK",
            "GIT_SSH",
            "GIT_NO_LAZY_FETCH",
            "HOME",
            "PATH",
        ]
        .into_iter()
        .map(std::ffi::OsString::from);
        let dropped = inherited_git_and_ssh(&c, inherited);
        // GIT_DIR is already cleared by the hardening; GIT_NO_LAZY_FETCH is
        // the hardening's own setting and is kept.
        assert_eq!(dropped, ["SSH_AUTH_SOCK", "GIT_SSH"]);
    }

    #[test]
    fn the_existing_commands_are_unchanged() {
        // command(dir) and hardened() still forbid every transport and set
        // no ssh command (orchestrator on #10729: strictly additive).
        let s = Scratch::new();
        let h = hardened(&s.0);
        assert_eq!(
            env_of(&h, "GIT_ALLOW_PROTOCOL"),
            Some(Some("none".as_ref()))
        );
        for key in [
            "GIT_SSH_COMMAND",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert_eq!(env_of(&h, key), None, "{key}");
        }
        let out = Command::new(&git_program().0)
            .args(["init", "-q"])
            .arg(&s.0)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let git = command(&s.0);
        assert!(git.refused.is_none(), "{:?}", git.refused);
        assert_eq!(
            env_of(&git.command, "GIT_ALLOW_PROTOCOL"),
            Some(Some("none".as_ref()))
        );
        assert_eq!(env_of(&git.command, "GIT_SSH_COMMAND"), None);
        assert!(
            config_env(&git.command)
                .iter()
                .all(|(k, _)| k.starts_with("hook."))
        );
    }

    /// [`common_command`] (a-flow's ledger reads, A27 rule 5) keeps the
    /// hardening, names the common directory with `--git-dir`, switches off
    /// every event's hooks and those the common config names, and runs
    /// nothing when it can't read which hooks config names (Opus #11512).
    #[test]
    fn a_common_command_pins_the_git_dir_and_switches_off_every_hook() {
        let s = Scratch::new();
        let common = bare_repo(&s, "c.git");
        let set = bare_command(&common)
            .args(["config", "hook.mine.command", "true"])
            .run(None)
            .unwrap();
        assert!(set.status.success(), "{set:?}");
        let git = common_command(&common);
        assert!(git.refused.is_none(), "{:?}", git.refused);
        assert_eq!(
            env_of(&git.command, "GIT_ALLOW_PROTOCOL"),
            Some(Some("none".as_ref()))
        );
        let args: Vec<&std::ffi::OsStr> = git.command.get_args().collect();
        let at = args
            .iter()
            .position(|a| *a == "--git-dir")
            .expect("--git-dir");
        assert_eq!(args.get(at + 1).copied(), Some(common.as_os_str()));
        let config = config_env(&git.command);
        for name in HOOK_EVENTS.iter().copied().chain(["mine"]) {
            assert!(
                config.contains(&(format!("hook.{name}.enabled"), "false".into())),
                "{name}: {config:?}"
            );
        }
        assert!(
            config
                .iter()
                .all(|(k, v)| k.starts_with("hook.") && v == "false"),
            "{config:?}"
        );
        let out = git
            .args(["rev-parse", "--is-bare-repository"])
            .run(None)
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
        // A common config git can't read: which hooks it names is unknown,
        // so the command is refused and runs nothing.
        fs::write(common.join("config"), "[broken\n").unwrap();
        let git = common_command(&common);
        assert!(git.refused.is_some(), "an unreadable config ran");
        assert!(git.arg("rev-parse").run(None).is_err());
    }
}

#[cfg(test)]
mod read_cache_tests {
    use super::*;

    /// The cache is process-wide: these tests take turns with it.
    static SERIAL: Mutex<()> = Mutex::new(());

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repository with one commit and a linked worktree `wt`; returns
    /// the scratch folder and the worktree.
    fn linked() -> (Scratch, PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mdkg-read-cache-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = dir.join("repo");
        fs::create_dir(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        sh(&repo, &["worktree", "add", "-q", "-b", "side", "../wt"]);
        let wt = dir.join("wt");
        (Scratch(dir), wt)
    }

    /// Points the worktree's record at another folder, as a `.git` file
    /// copied from elsewhere would: a fresh check refuses it.
    fn break_link(wt: &Path) {
        let admin = check_fresh(wt, &WorktreeTrust::default()).unwrap().git_dir;
        fs::write(admin.join("gitdir"), "/elsewhere/.git\n").unwrap();
    }

    fn none() -> WorktreeTrust {
        WorktreeTrust::default()
    }

    /// e-gitcache: a kept ref read never outlives a change to the ref,
    /// whoever makes it.
    #[test]
    fn a_kept_ref_read_follows_every_ref_change() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            assert!(has_ref(&wt, "main"));
            let before = git_runs();
            assert!(has_ref(&wt, "main"));
            assert!(!has_ref(&wt, "topic"));
            assert!(!has_ref(&wt, "topic"));
            assert_eq!(git_runs(), before + 2, "main kept; a no asked again");
            // Made by another git: seen at once.
            sh(&wt, &["branch", "topic"]);
            assert!(has_ref(&wt, "topic"));
            // Packed, with no loose file left, then deleted.
            sh(&wt, &["pack-refs", "--all"]);
            assert!(has_ref(&wt, "topic"));
            sh(&wt, &["branch", "-D", "topic"]);
            assert!(!has_ref(&wt, "topic"));
            // Found again by another of git's rules.
            sh(&wt, &["tag", "topic"]);
            assert!(has_ref(&wt, "topic"));
            sh(&wt, &["update-ref", "-d", "refs/tags/topic"]);
            assert!(!has_ref(&wt, "topic"));
            // HEAD, followed through the branch it names.
            assert!(has_ref(&wt, "HEAD"));
            sh(&wt, &["checkout", "-q", "--orphan", "fresh"]);
            assert!(!has_ref(&wt, "HEAD"), "HEAD names an unborn branch");
            sh(&wt, &["commit", "-q", "--allow-empty", "-m", "second"]);
            assert!(has_ref(&wt, "HEAD"), "the branch HEAD names was born");
            // Another worktree's change to a shared ref.
            let primary = wt.parent().unwrap().join("repo");
            sh(&primary, &["branch", "shared"]);
            assert!(has_ref(&wt, "shared"));
        });
    }

    /// A ref's value read through [`cached_ref`] follows the ref: a commit
    /// on it, a reset, or a delete by another git is read at once, and an
    /// error is never kept.
    #[test]
    fn a_kept_ref_value_follows_the_ref() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let tip = |name: &str| {
                cached_ref(&wt, name, "test", || {
                    let out = command(&wt)
                        .args(["rev-parse", "--verify", "--quiet", name])
                        .run(None)
                        .map_err(|e| e.to_string())?;
                    if !out.status.success() {
                        return Err("missing".to_string());
                    }
                    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
                })
            };
            let first = tip("refs/heads/side").unwrap();
            let before = git_runs();
            assert_eq!(tip("refs/heads/side").unwrap(), first);
            assert_eq!(git_runs(), before, "kept");
            sh(&wt, &["commit", "-q", "--allow-empty", "-m", "next"]);
            let second = tip("refs/heads/side").unwrap();
            assert_ne!(second, first, "a commit is read at once");
            sh(&wt, &["update-ref", "refs/heads/side", &first]);
            assert_eq!(tip("refs/heads/side").unwrap(), first, "a reset too");
            // Packed: the loose file goes, the value stays.
            sh(&wt, &["pack-refs", "--all"]);
            assert_eq!(tip("refs/heads/side").unwrap(), first);
            assert!(tip("refs/heads/gone").is_err());
            let before = git_runs();
            assert!(tip("refs/heads/gone").is_err());
            assert_eq!(git_runs(), before + 1, "an error is read again");
            // Another purpose never shares the answer.
            let other = cached_ref(&wt, "refs/heads/side", "other", || {
                Ok::<_, String>("x".into())
            });
            assert_eq!(other.unwrap(), "x");
        });
    }

    /// A git that didn't answer (here: refused, as a timeout or a failed
    /// spawn would be) leaves nothing kept, so the next read asks again.
    #[test]
    fn a_read_git_didnt_answer_is_never_kept() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let checkout = checked_worktree(&wt).unwrap();
            let key = ValueKey::Ref {
                git_dir: checkout.git_dir.clone(),
                name: "main".into(),
                purpose: "has_ref",
            };
            let paths = ref_files(&checkout, "main");
            assert!(paths.is_some());
            let failed = cached_value(key.clone(), &checkout.common, paths.clone(), || {
                (None, false)
            });
            assert_eq!(failed, None);
            assert_eq!(kept_value(&key), None, "a failed read isn't kept");
            assert!(has_ref(&wt, "main"), "asked again, and found");
            // A key git reports unset is its answer, and kept.
            let common = checkout.common.clone();
            assert_eq!(read_common_config(&common, "mdkg.unset"), (None, true));
        });
    }

    /// A ref whose commit git can't read now (its object is away, as in an
    /// alternate that's unavailable) answers no, which is never kept: once
    /// the object is back, it's found.
    #[test]
    fn a_ref_whose_commit_is_unreadable_is_asked_again() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let common = common_dir(&wt).unwrap();
            let objects = common.join("objects");
            let away = common.join("objects-away");
            fs::rename(&objects, &away).unwrap();
            fs::create_dir(&objects).unwrap();
            assert!(!has_ref(&wt, "main"), "its commit can't be read");
            fs::remove_dir(&objects).unwrap();
            fs::rename(&away, &objects).unwrap();
            assert!(has_ref(&wt, "main"), "asked again, and found");
        });
    }

    /// Another worktree's refs (`main-worktree/HEAD`, `worktrees/<name>/HEAD`)
    /// resolve through files the cache doesn't stamp: never kept.
    #[test]
    fn another_worktrees_refs_are_never_kept() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let checkout = checked_worktree(&wt).unwrap();
            for name in ["main-worktree/HEAD", "worktrees/wt/HEAD"] {
                assert!(ref_files(&checkout, name).is_none(), "{name}");
            }
            assert!(has_ref(&wt, "main-worktree/HEAD"));
            let primary = wt.parent().unwrap().join("repo");
            sh(&primary, &["checkout", "-q", "--orphan", "unborn"]);
            assert!(!has_ref(&wt, "main-worktree/HEAD"), "seen at once");
        });
    }

    /// An object name, or anything but a plain ref name, is read every
    /// time: what names it isn't a file the cache can stamp.
    #[test]
    fn an_object_name_is_never_kept() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let out = command(&wt).args(["rev-parse", "HEAD"]).run(None).unwrap();
            let oid = String::from_utf8(out.stdout).unwrap().trim().to_string();
            for name in [oid.as_str(), &oid[..12], "HEAD~0", "main@{0}", "side^"] {
                let before = git_runs();
                has_ref(&wt, name);
                has_ref(&wt, name);
                assert_eq!(git_runs(), before + 2, "{name} was kept");
            }
        });
    }

    /// A config value is kept while the config file is unchanged, and
    /// never when it includes another file.
    #[test]
    fn a_kept_config_read_follows_the_config_file() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let common = common_dir(&wt).unwrap();
            assert_eq!(common_config(&common, "mdkg.mainRef"), None);
            let before = git_runs();
            assert_eq!(common_config(&common, "mdkg.mainRef"), None);
            assert_eq!(git_runs(), before, "kept");
            sh(&wt, &["config", "mdkg.mainRef", "trunk"]);
            assert_eq!(
                common_config(&common, "mdkg.mainRef").as_deref(),
                Some("trunk")
            );
            sh(&wt, &["config", "mdkg.mainRef", "trunk2"]);
            assert_eq!(
                common_config(&common, "mdkg.mainRef").as_deref(),
                Some("trunk2")
            );
            sh(&wt, &["config", "include.path", "/dev/null"]);
            let before = git_runs();
            common_config(&common, "mdkg.mainRef");
            common_config(&common, "mdkg.mainRef");
            assert_eq!(git_runs(), before + 2, "an include is never kept");
        });
    }

    /// mdkg's own writer, and the start of a turn or request, read every
    /// value again.
    #[test]
    fn a_write_check_and_fresh_reads_drop_kept_values() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            fresh_reads();
            let common = common_dir(&wt).unwrap();
            // Keys the write check itself doesn't read (it may keep those
            // again, under their stamps).
            assert!(has_ref(&wt, "main"));
            common_config(&common, "mdkg.unread");
            ready_to_write(&wt).unwrap();
            let before = git_runs();
            assert!(has_ref(&wt, "main"));
            common_config(&common, "mdkg.unread");
            assert_eq!(git_runs(), before + 2, "read again after a write check");
            fresh_reads();
            checked_worktree(&wt).unwrap();
            let before = git_runs();
            assert!(has_ref(&wt, "main"));
            assert_eq!(git_runs(), before + 1, "read again after fresh_reads");
        });
    }

    /// A config snapshot is parsed once per content: the same bytes reuse
    /// the listing, changed bytes are parsed again, and the ref-storage
    /// check reads a changed config at once.
    #[test]
    fn a_config_snapshot_is_parsed_once_per_content() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        fresh_reads();
        let one = b"[core]\n\tbare = false\n";
        let first = parse_config_snapshot(&wt, one).unwrap();
        let before = git_runs();
        assert_eq!(parse_config_snapshot(&wt, one).unwrap(), first);
        assert_eq!(git_runs(), before, "same bytes, kept");
        let two = b"[core]\n\tbare = true\n";
        assert_ne!(parse_config_snapshot(&wt, two).unwrap(), first);
        assert_eq!(git_runs(), before + 1);
        let common = common_dir(&wt).unwrap();
        require_files_ref_storage(&common).unwrap();
        let before = git_runs();
        require_files_ref_storage(&common).unwrap();
        assert_eq!(git_runs(), before, "unchanged config, kept");
        sh(&wt, &["config", "extensions.refStorage", "reftable"]);
        assert!(
            require_files_ref_storage(&common).is_err(),
            "changed: read again"
        );
    }

    #[test]
    fn a_read_reuses_the_check_and_a_fresh_check_sees_the_change() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        let first = checked_worktree_with_trust(&wt, &none()).unwrap();
        break_link(&wt);
        // Read paths keep what this process checked...
        assert_eq!(checked_worktree_with_trust(&wt, &none()).unwrap(), first);
        // ...a fresh check, as every write makes, doesn't...
        let refused = check_worktree_with_trust(&wt, &none()).unwrap_err();
        assert!(
            refused.to_string().contains("belongs to the worktree"),
            "{refused}"
        );
        // ...and its refusal drops the kept check, for reads too.
        assert!(checked_worktree_with_trust(&wt, &none()).is_err());
    }

    #[test]
    fn ready_to_write_checks_again_after_a_cached_read() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            ready_to_write(&wt).unwrap();
            checked_worktree(&wt).unwrap();
            break_link(&wt);
            assert!(checked_worktree(&wt).is_ok(), "the read is cached");
            let refused = ready_to_write(&wt).unwrap_err();
            assert!(
                refused.to_string().contains("belongs to the worktree"),
                "{refused}"
            );
        });
    }

    #[test]
    fn a_failed_check_of_a_subfolder_drops_its_worktree_too() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        let sub = wt.join("sub");
        fs::create_dir(&sub).unwrap();
        checked_worktree_with_trust(&wt, &none()).unwrap();
        break_link(&wt);
        assert!(check_worktree_with_trust(&sub, &none()).is_err());
        assert!(checked_worktree_with_trust(&wt, &none()).is_err());
    }

    fn head(dir: &Path) -> String {
        let out = command(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .run(None)
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_bound_check_outlasts_another_threads_stale_publish() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        let repo = s.0.join("repo");
        let at = wt.to_str().unwrap();
        // A thread checks the first worktree, then pauses before it
        // publishes; meanwhile that worktree moves away (keeping its admin
        // folder) and another branch is added at the same path.
        let stale = check_fresh(&wt, &none()).unwrap();
        sh(&repo, &["worktree", "move", at, "../moved"]);
        sh(&repo, &["worktree", "add", "-q", "-b", "other", at]);
        with_worktree_trust(&none(), || {
            let fresh = check_worktree(&wt).unwrap();
            assert_ne!(fresh.git_dir, stale.git_dir);
            let (checked, published) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    checked.wait();
                    // The paused thread publishes its older check last.
                    let mut list = CHECKOUTS.lock().unwrap();
                    list.retain(|(d, _, _)| *d != stale.top);
                    list.push((stale.top.clone(), none(), stale.clone()));
                    drop(list);
                    published.wait();
                });
                with_checkout(&fresh, || {
                    checked.wait();
                    published.wait();
                    assert_eq!(head(&wt), "other", "the bound check is used");
                });
            });
            // Unbound, the read takes what the cache holds: the race.
            assert_eq!(head(&wt), "side");
        });
        fresh_reads();
    }

    #[test]
    fn a_fresh_check_inside_a_binding_replaces_or_ends_it() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        let repo = s.0.join("repo");
        let at = wt.to_str().unwrap();
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&wt).unwrap();
            with_checkout(&bound, || {
                // The worktree is replaced under the binding; a write's
                // fresh check sees it, and the commands after it do too.
                sh(&repo, &["worktree", "move", at, "../moved"]);
                sh(&repo, &["worktree", "add", "-q", "-b", "other", at]);
                assert_eq!(head(&wt), "side", "still bound");
                ready_to_write(&wt).unwrap();
                assert_eq!(head(&wt), "other", "the fresh check replaced it");
                // A fresh check that fails ends the binding: commands check
                // again, and are refused.
                break_link(&wt);
                assert!(check_worktree(&wt).is_err());
                assert!(checkout_for(&wt).is_err(), "a failed check left it bound");
            });
        });
        fresh_reads();
    }

    #[test]
    fn a_bound_top_that_is_gone_keeps_the_bound_check() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&wt).unwrap();
            with_checkout(&bound, || {
                fs::rename(&wt, s.0.join("moved")).unwrap();
                // Reads that need only the check still get the bound one.
                assert_eq!(checkout_for(&wt).unwrap(), bound);
                assert_eq!(common_dir(&wt).unwrap(), bound.common);
                // A git command has no folder to run in.
                let out = command(&wt).args(["rev-parse", "HEAD"]).run(None).unwrap();
                assert!(!out.status.success());
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains(&*wt.to_string_lossy()),
                    "{out:?}"
                );
                fs::rename(s.0.join("moved"), &wt).unwrap();
            });
        });
        fresh_reads();
    }

    #[test]
    fn a_bound_top_swapped_for_a_link_is_refused() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        let repo = s.0.join("repo");
        let other = s.0.join("other");
        sh(&repo, &["worktree", "add", "-q", "-b", "other", "../other"]);
        let unpinned = none();
        let pinned = WorktreeTrust {
            common: Some(repo.join(".git")),
            worktree: Some(wt.clone()),
        };
        for trust in [unpinned, pinned] {
            with_worktree_trust(&trust, || {
                let bound = check_worktree(&wt).unwrap();
                let (checked, swapped) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        checked.wait();
                        // Another process moves the worktree away and
                        // leaves a link to a second one in its place.
                        fs::rename(&wt, s.0.join("moved")).unwrap();
                        std::os::unix::fs::symlink(&other, &wt).unwrap();
                        swapped.wait();
                    });
                    with_checkout(&bound, || {
                        checked.wait();
                        swapped.wait();
                        let refused = command(&wt)
                            .args(["rev-parse", "HEAD"])
                            .run(None)
                            .unwrap_err();
                        assert!(
                            refused.to_string().contains("not the worktree checked"),
                            "{refused}"
                        );
                    });
                });
                fs::remove_file(&wt).unwrap();
                fs::rename(s.0.join("moved"), &wt).unwrap();
            });
            fresh_reads();
        }
    }

    #[test]
    fn a_bound_folder_that_joins_an_outer_worktree_ends_its_binding() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, _wt) = linked();
        let repo = s.0.join("repo");
        let inner = repo.join("inner");
        fs::create_dir(&inner).unwrap();
        sh(&inner, &["init", "-q", "-b", "main"]);
        sh(&inner, &["commit", "-q", "--allow-empty", "-m", "inner"]);
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&inner).unwrap();
            assert_eq!(bound.top, inner);
            with_checkout(&bound, || {
                // Its own repository goes: git now places the folder in
                // the outer worktree, and a fresh check says so.
                fs::remove_dir_all(inner.join(".git")).unwrap();
                assert_eq!(check_worktree(&inner).unwrap().top, repo);
                assert_eq!(checkout_for(&inner).unwrap().top, repo, "still bound");
            });
        });
        fresh_reads();
    }

    #[test]
    fn a_check_below_a_bound_folder_that_joined_an_outer_worktree_ends_it() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, _wt) = linked();
        let repo = s.0.join("repo");
        let inner = repo.join("inner");
        fs::create_dir_all(inner.join("sub")).unwrap();
        sh(&inner, &["init", "-q", "-b", "main"]);
        sh(&inner, &["commit", "-q", "--allow-empty", "-m", "inner"]);
        let nested = inner.join("nested");
        fs::create_dir(&nested).unwrap();
        sh(&nested, &["init", "-q", "-b", "main"]);
        sh(&nested, &["commit", "-q", "--allow-empty", "-m", "nested"]);
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&inner).unwrap();
            with_checkout(&bound, || {
                // A repository nested inside the bound one leaves it bound.
                assert_eq!(check_worktree(&nested).unwrap().top, nested);
                // The bound folder's own repository goes. While still
                // bound, a read there gets the bound check, not a fresh
                // one, which would now find the outer worktree.
                fs::remove_dir_all(inner.join(".git")).unwrap();
                assert_eq!(checkout_for(&inner).unwrap(), bound, "unbound");
                // A check of a folder below it finds the outer worktree.
                assert_eq!(check_worktree(&inner.join("sub")).unwrap().top, repo);
                assert_eq!(checkout_for(&inner).unwrap().top, repo, "still bound");
            });
        });
        fresh_reads();
    }

    #[test]
    fn an_ended_binding_checks_fresh_not_the_shared_cache() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&wt).unwrap();
            with_checkout(&bound, || {
                break_link(&wt);
                assert!(check_worktree(&wt).is_err());
                // Another thread publishes the check it made before the
                // break.
                let mut list = CHECKOUTS.lock().unwrap();
                list.push((wt.clone(), none(), bound.clone()));
                drop(list);
                assert!(checkout_for(&wt).is_err(), "the stale publish was read");
            });
        });
        fresh_reads();
    }

    #[test]
    fn a_binding_covers_only_its_top_and_its_scope() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        let repo = s.0.join("repo");
        with_worktree_trust(&none(), || {
            let bound = check_worktree(&wt).unwrap();
            with_checkout(&bound, || {
                // Another folder still checks for itself.
                assert_eq!(head(&repo), "main");
                // Another trust context doesn't see the binding.
                let elsewhere = WorktreeTrust {
                    common: Some(s.0.clone()),
                    worktree: None,
                };
                with_worktree_trust(&elsewhere, || {
                    assert!(checkout_for(&wt).is_err());
                });
                assert_eq!(head(&wt), "side");
            });
            assert!(BOUND.with(|b| b.borrow().is_empty()), "unbound on exit");
        });
        fresh_reads();
    }

    #[test]
    fn fresh_reads_forgets_every_kept_check() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (_s, wt) = linked();
        checked_worktree_with_trust(&wt, &none()).unwrap();
        break_link(&wt);
        assert!(checked_worktree_with_trust(&wt, &none()).is_ok());
        fresh_reads();
        assert!(checked_worktree_with_trust(&wt, &none()).is_err());
    }

    #[test]
    fn a_kept_check_serves_only_its_own_trust_context() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (s, wt) = linked();
        checked_worktree_with_trust(&wt, &none()).unwrap();
        let elsewhere = WorktreeTrust {
            common: Some(s.0.clone()),
            worktree: None,
        };
        let refused = checked_worktree_with_trust(&wt, &elsewhere).unwrap_err();
        assert!(
            refused.to_string().contains("isn't the trusted one"),
            "{refused}"
        );
        let pinned = WorktreeTrust {
            common: Some(s.0.join("repo/.git")),
            worktree: Some(wt.clone()),
        };
        assert!(checked_worktree_with_trust(&wt, &pinned).unwrap().pinned);
        assert!(!checked_worktree_with_trust(&wt, &none()).unwrap().pinned);
    }
}

#[cfg(test)]
mod object_reader_tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn git_out(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        out.stdout
    }

    /// A repository in `format` with a file, a folder and a commit.
    fn repo(format: &str) -> Scratch {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mdkg-objects-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        git_out(
            &dir,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        );
        fs::write(dir.join("a.md"), "alpha\n").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.md"), "beta\n").unwrap();
        git_out(&dir, &["add", "."]);
        git_out(&dir, &["commit", "-q", "-m", "first"]);
        Scratch(dir)
    }

    fn trusted<T>(f: impl FnOnce() -> T) -> T {
        with_worktree_trust(&WorktreeTrust::default(), f)
    }

    #[test]
    fn reads_objects_and_trees_through_one_git_in_either_object_format() {
        for format in ["sha1", "sha256"] {
            let s = repo(format);
            trusted(|| {
                let mut reader = ObjectReader::open(&s.0).unwrap();
                let head = String::from_utf8(git_out(&s.0, &["rev-parse", "HEAD"])).unwrap();
                let info = reader.info("HEAD").unwrap().unwrap();
                assert_eq!(
                    (info.oid.as_str(), info.kind.as_str()),
                    (head.trim(), "commit")
                );
                let (blob, data) = reader.contents("HEAD:a.md", 1024).unwrap().unwrap();
                assert_eq!(
                    (blob.kind.as_str(), data.as_slice()),
                    ("blob", b"alpha\n".as_slice())
                );
                let (info, tree) = reader.tree("HEAD^{tree}", 4096).unwrap().unwrap();
                assert_eq!(info.kind, "tree");
                let listed = String::from_utf8(git_out(&s.0, &["ls-tree", "HEAD"])).unwrap();
                let expected: Vec<(String, String, String)> = listed
                    .lines()
                    .map(|l| {
                        let (meta, name) = l.split_once('\t').unwrap();
                        let mut f = meta.split(' ');
                        let mode = f.next().unwrap().trim_start_matches('0').to_string();
                        f.next();
                        (mode, name.to_string(), f.next().unwrap().to_string())
                    })
                    .collect();
                let got: Vec<(String, String, String)> = tree
                    .iter()
                    .map(|e| {
                        (
                            e.mode.clone(),
                            String::from_utf8(e.name.clone()).unwrap(),
                            e.oid.clone(),
                        )
                    })
                    .collect();
                assert_eq!(got, expected, "{format}");
                // A missing object leaves the reader usable.
                assert_eq!(reader.info("HEAD:nothing.md").unwrap(), None);
                assert_eq!(
                    reader.contents("HEAD:sub/b.md", 64).unwrap().unwrap().1,
                    b"beta\n"
                );
            });
        }
    }

    #[test]
    fn a_bad_name_is_refused_without_stopping_the_reader() {
        let s = repo("sha1");
        trusted(|| {
            let mut reader = ObjectReader::open(&s.0).unwrap();
            for name in ["", "HEAD\ninfo HEAD", "HEAD\0"] {
                let refused = reader.info(name).unwrap_err();
                assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{name:?}");
            }
            assert!(reader.info("HEAD").unwrap().is_some());
        });
    }

    #[test]
    fn an_object_over_the_cap_stops_the_reader_and_a_wrong_kind_doesnt() {
        let s = repo("sha1");
        trusted(|| {
            let mut reader = ObjectReader::open(&s.0).unwrap();
            assert!(reader.contents("HEAD:a.md", 3).is_err());
            let stopped = reader.info("HEAD").unwrap_err();
            assert!(
                stopped.to_string().contains("stopped after an error"),
                "a stopped reader answers nothing: {stopped}"
            );
            // A blob read whole where a tree was wanted leaves the stream
            // in step, so the reader goes on.
            let mut reader = ObjectReader::open(&s.0).unwrap();
            let refused = reader.tree("HEAD:a.md", 1024).unwrap_err();
            assert!(refused.to_string().contains("not a tree"), "{refused}");
            assert!(reader.info("HEAD").unwrap().is_some());
        });
    }

    #[test]
    fn a_git_that_doesnt_answer_is_killed_at_the_limit() {
        let mut silent = Command::new("/bin/sh");
        silent.args(["-c", "cat > /dev/null"]).process_group(0);
        let mut reader = ObjectReader::spawned(silent, 20, Duration::from_millis(200)).unwrap();
        let pid = reader.child.id();
        let started = Instant::now();
        let error = reader.info("HEAD").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            reader.child.child().try_wait().unwrap().is_some(),
            "{pid} still runs"
        );
        let refused = reader.info("HEAD").unwrap_err();
        assert!(
            refused.to_string().contains("stopped after an error"),
            "{refused}"
        );
    }

    #[test]
    fn an_answer_out_of_step_stops_the_reader() {
        // Each script answers its first request with a malformed frame.
        let oid = "a".repeat(40);
        // Whole frames must fail on parsing; only the truncated body
        // waits for the time limit.
        for (answer, truncated) in [
            (format!("{oid} blob x\\n"), false),
            (format!("{oid} blob\\n"), false),
            (format!("{oid} blob 3\\nab"), true),
            (format!("{oid} blob 3\\nabcX"), false),
            ("garbage\\n".to_string(), false),
        ] {
            // A generous limit for whole frames, so a slow machine can't
            // turn a parse failure into a timeout.
            let limit = Duration::from_millis(if truncated { 300 } else { 10_000 });
            let mut fake = Command::new("/bin/sh");
            fake.args([
                "-c",
                &format!("read line; printf '{answer}'; cat > /dev/null"),
            ])
            .process_group(0);
            let mut reader = ObjectReader::spawned(fake, 20, limit).unwrap();
            let failed = reader.contents("HEAD", 64).unwrap_err();
            assert_eq!(
                failed.kind() == io::ErrorKind::TimedOut,
                truncated,
                "{answer}: {failed}"
            );
            // Refused by the stopped reader itself, not by a second wait.
            let refused = reader.info("HEAD").unwrap_err();
            assert!(
                refused.to_string().contains("stopped after an error"),
                "{answer}: {refused}"
            );
        }
    }

    #[test]
    fn dropping_the_reader_ends_its_git() {
        let s = repo("sha1");
        let mut reader = trusted(|| ObjectReader::open(&s.0).unwrap());
        assert!(reader.info("HEAD").unwrap().is_some());
        let pid = reader.child.id();
        drop(reader);
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "cat-file {pid} outlived its reader");
    }

    #[test]
    fn a_tree_that_cant_be_read_is_an_error() {
        for data in [
            b"100644 a".as_slice(),
            b"100644 a\0short",
            b" a\0aaaaaaaaaaaaaaaaaaaa",
            b"10x644 a\0aaaaaaaaaaaaaaaaaaaa",
            b"100644 \0aaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(parse_tree(data, 20).is_err(), "{data:?}");
        }
        let entries = parse_tree(b"40000 d\0aaaaaaaaaaaaaaaaaaaa", 20).unwrap();
        assert_eq!(entries[0].mode, "40000");
        assert_eq!(entries[0].oid, "61".repeat(20));
    }
}
