//! Worktree-free writes of the graph ref (edd-01a18867, "Worktree-free
//! writes").
//!
//! A write runs in two steps, so the store can record it in between:
//! - [`RefWriter::prepare`] writes the blobs, trees and commit on the
//!   current tip, and pins the commit under `refs/mdkg/pending/<id>` so `gc`
//!   can't take it. No graph ref moves.
//! - [`RefWriter::publish`] compare-and-swaps the graph ref to it, or
//!   rebuilds it on a newer tip when the touched paths are unchanged there.
//!   [`RefWriter::unpin`] then drops the pending ref.
//!
//! CAS is the publication boundary. The graph lock ([`GraphLock`]) only
//! serializes mdkg's own writers, so they don't retry each other's CAS.
//! Request ids make a rerun idempotent: a request's commit carries
//! `[<id> <fingerprint>]` in its subject, looked up in `base..tip` only.
//!
//! Local plumbing is hardened: hooks off, no global or system config, and
//! `core.fsync=objects,reference`. The commit identity alone is read from
//! the user's own git configuration (`git var GIT_COMMITTER_IDENT`).

use mdkg_format::program::BIN;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::OnceLock;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Git {
        operation: String,
        detail: String,
    },
    Invalid(String),
    /// A touched path changed on the graph since the request's base.
    Stale {
        path: String,
    },
    RequestIdReused(String),
    /// The graph ref kept moving.
    RetryLimit,
    /// No committer identity is configured.
    Identity(String),
    /// The graph ref doesn't exist and the request has a base.
    Missing(String),
    /// A request that creates the graph ref found it already there.
    Exists(String),
    /// The graph tip holds another graph than the expected one.
    GraphIdentity(String),
    /// The request's content is what the tip already holds: nothing to
    /// write. Callers treat it as success with no commit.
    ///
    /// A no-op isn't recorded durably (orchestrator #7002): it has no
    /// receipt, and a retry of the same request id recomputes against the
    /// current tip. If nothing changed since, it's a no-op again. If a
    /// touched path changed, the request's preimages refuse the retry
    /// ([`Error::Stale`]) rather than applying it over that change. A
    /// create is never a no-op: with no graph ref, a request that changes
    /// nothing is refused.
    NoChange,
    /// The op needs this clone's exclusive OS lock, or the filesystem
    /// can't give one.
    LockRequired(String),
}

impl Error {
    /// The stable diagnostic code (edd-01a18867).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "graph.io",
            Self::Git { .. } => "graph.git",
            Self::Invalid(_) => "graph.write_invalid",
            Self::Stale { .. } => "graph.write_stale",
            Self::RequestIdReused(_) => "graph.request_reused",
            Self::RetryLimit => "graph.write_contended",
            Self::Identity(_) => "graph.identity_unset",
            Self::Missing(_) => "graph.ref_missing",
            Self::Exists(_) => "graph.ref_exists",
            Self::GraphIdentity(_) => "graph.identity_mismatch",
            Self::NoChange => "graph.write_noop",
            Self::LockRequired(_) => "graph.lock_required",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Git { operation, detail } => write!(f, "git {operation}: {detail}"),
            Self::Invalid(s) => write!(f, "invalid ref write: {s}"),
            Self::Stale { path } => write!(f, "the graph changed {path} since the request's base"),
            Self::RequestIdReused(id) => {
                write!(f, "request ID reused with different payload: {id}")
            }
            Self::RetryLimit => write!(f, "graph ref kept moving; retry limit reached"),
            // The detail carries the fix: it depends on the global file
            // read (`identity_error`).
            Self::Identity(detail) => write!(f, "no git identity for graph commits: {detail}"),
            Self::Missing(name) => write!(f, "the graph ref {name} doesn't exist"),
            Self::Exists(name) => write!(
                f,
                "the graph ref {name} already exists: a request that creates it can't publish"
            ),
            Self::GraphIdentity(detail) => write!(f, "{detail}"),
            Self::NoChange => write!(f, "the graph already holds this content; nothing to write"),
            // Each message is a whole sentence (Terra #7300).
            Self::LockRequired(what) => write!(f, "graph lock refused: {what}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// How the graph lock is taken. There's one mode: every write path holds
/// a real OS lock (orchestrator, on Terra #6321: no lockless CAS-only
/// mode). A common folder on a network filesystem, where OS locks can't be
/// trusted, is refused ([`Error::LockRequired`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// The OS lock at `<common>/mdkg/graph/lock`.
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// A cache refresh, a write session, a publish, a sync's local CAS.
    Exclusive,
    /// A read once the cache is at the tip.
    Shared,
}

/// The graph lock, `<common>/mdkg/graph/lock`: an OS lock the kernel drops
/// when its holder dies. The exclusive lock lasts one whole write session
/// (the op's logic, hashing, the store transaction, the publish): tens of
/// milliseconds for most ops, up to a few seconds for a `done` with a large
/// ledger. It's never held across the network.
///
/// **Lock order (G22):** take the graph lock first, then the store's write
/// transaction, never the other way round. Store writes that run without
/// a graph lock (a-flow's settling transaction among them) must not take
/// one while their transaction is open; they read graph content as git
/// objects ([`crate::graphref::authority`]), which needs no lock.
#[derive(Debug)]
pub struct GraphLock {
    common: PathBuf,
    kind: LockKind,
    // Held for its lock.
    _file: File,
}

impl GraphLock {
    /// Blocks until the exclusive lock is held.
    pub fn exclusive(common: &Path, mode: LockMode) -> Result<Self> {
        Self::take(common, mode, LockKind::Exclusive)
    }

    /// Blocks until a shared lock is held.
    pub fn shared(common: &Path, mode: LockMode) -> Result<Self> {
        Self::take(common, mode, LockKind::Shared)
    }

    /// The exclusive lock if nobody holds the lock now, else `None`.
    pub fn try_exclusive(common: &Path, _mode: LockMode) -> Result<Option<Self>> {
        let file = open_lock(common)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                common: canonical(common)?,
                kind: LockKind::Exclusive,
                _file: file,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// The shared lock if no writer holds the lock now, else `None`.
    pub fn try_shared(common: &Path, _mode: LockMode) -> Result<Option<Self>> {
        let file = open_lock(common)?;
        match file.try_lock_shared() {
            Ok(()) => Ok(Some(Self {
                common: canonical(common)?,
                kind: LockKind::Shared,
                _file: file,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn take(common: &Path, _mode: LockMode, kind: LockKind) -> Result<Self> {
        let file = open_lock(common)?;
        match kind {
            LockKind::Exclusive => file.lock()?,
            LockKind::Shared => file.lock_shared()?,
        }
        Ok(Self {
            common: canonical(common)?,
            kind,
            _file: file,
        })
    }

    pub fn kind(&self) -> LockKind {
        self.kind
    }

    /// Refuses a lock that isn't this clone's exclusive graph lock (`what`
    /// names the op that needs it).
    pub fn require_exclusive(&self, common: &Path, what: &str) -> Result<()> {
        if self.kind != LockKind::Exclusive || self.common != common.canonicalize()? {
            return Err(Error::LockRequired(format!(
                "{what} needs this clone's exclusive graph lock"
            )));
        }
        Ok(())
    }

    /// The common git folder the lock belongs to.
    pub fn common(&self) -> &Path {
        &self.common
    }
}

fn canonical(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize()
}

/// `<common>/mdkg/graph`, created private when missing, with each new
/// folder synced into its parent.
pub(crate) fn graph_dir(common: &Path) -> io::Result<PathBuf> {
    let mdkg = common.join("mdkg");
    let dir = mdkg.join("graph");
    for (d, parent) in [(&mdkg, common), (&dir, mdkg.as_path())] {
        private_dir(d, parent)?;
    }
    Ok(dir)
}

/// Creates `dir` private when missing, refuses a link or file there, and
/// syncs it into `parent` every time: a sync that failed once is redone
/// on the next call instead of being skipped because the folder exists.
pub(crate) fn private_dir(dir: &Path, parent: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;
    // Everything through descriptors, never by path (orchestrator #9415):
    // the parent opened as a folder, the folder made in it and opened with
    // O_NOFOLLOW, its owner checked, then fchmod on that descriptor. A
    // no-follow open that fails changes nothing.
    let parent_fd = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let name = dir
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no folder name", dir.display())))?;
    let created = match rustix::fs::mkdirat(&parent_fd, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(e) => return Err(e.into()),
    };
    let fd = rustix::fs::openat(
        &parent_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        // One just made that a restrictive umask left unreadable is
        // removed again (only an empty folder, never through a link), so
        // a retry under a sane umask makes it afresh (Terra #10216).
        if created && e == rustix::io::Errno::ACCESS {
            rustix::fs::unlinkat(&parent_fd, name, rustix::fs::AtFlags::REMOVEDIR).ok();
        }
        io::Error::other(format!(
            "{} must be a folder its owner can open, not a link ({e}); check the umask",
            dir.display()
        ))
    })?;
    let handle = File::from(fd);
    let meta = handle.metadata()?;
    // Ours: owned by this process's user (Claude #10147), so a folder
    // another user made is refused here rather than failing later. Root
    // also accepts one owned like its parent, as before (a container
    // running as root over a checkout another uid owns, Opus #10313), but
    // never a third user's (Opus #10511).
    let parent_uid = File::from(parent_fd).metadata()?.uid();
    let euid = rustix::process::geteuid();
    let owner_ok = meta.uid() == euid.as_raw() || (euid.is_root() && meta.uid() == parent_uid);
    if !meta.is_dir() || !owner_ok {
        return Err(io::Error::other(format!(
            "{} must be a folder owned by this user",
            dir.display()
        )));
    }
    let mode = meta.mode() & 0o777;
    if created {
        // Ours, just made: exactly 0700, whatever the umask took away.
        if mode != 0o700 {
            handle.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
    } else {
        // An existing folder keeps its owner bits: one the owner can't
        // use is refused, never widened; group and other bits are removed.
        // A swap after this check is a stated residual (item 9).
        if mode & 0o700 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} isn't readable, writable and searchable by its owner (mode {mode:o})",
                    dir.display()
                ),
            ));
        }
        if mode & 0o077 != 0 {
            handle.set_permissions(std::fs::Permissions::from_mode(mode & 0o700))?;
        }
    }
    File::open(parent)?.sync_all()
}

/// The filesystems the graph lock trusts, by name (BSD `f_fstypename`).
#[cfg_attr(
    not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")),
    allow(dead_code)
)]
// FAT and exFAT can't hard-link, which graph creation needs: refused
// (orchestrator #8398).
const LOCAL_BY_NAME: [&str; 5] = ["apfs", "hfs", "ufs", "zfs", "tmpfs"];

/// The filesystems the graph lock trusts, by Linux `f_type` magic. An
/// allowlist: network, FUSE, host-shared, eCryptfs and unknown filesystems
/// are refused rather than trusted.
#[cfg_attr(
    any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
    allow(dead_code)
)]
const LOCAL_BY_MAGIC: [(u32, &str); 12] = [
    (0xEF53, "ext2/ext3/ext4"),
    (0x5846_5342, "xfs"),
    (0x9123_683E, "btrfs"),
    (0x0102_1994, "tmpfs"),
    (0x2FC1_2FC1, "zfs"),
    (0xF2F5_2010, "f2fs"),
    (0x794C_7630, "overlayfs"), // containers
    (0x5265_4973, "reiserfs"),
    (0x3153_464A, "jfs"),
    (0xCA45_1A4E, "bcachefs"),
    (0x7366_746E, "ntfs3"),
    (0x0000_482B, "hfsplus"),
];

#[cfg_attr(
    not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")),
    allow(dead_code)
)]
fn untrusted_by_name(name: &str) -> Option<String> {
    (!LOCAL_BY_NAME.contains(&name)).then(|| name.to_owned())
}

#[cfg_attr(
    any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
    allow(dead_code)
)]
fn untrusted_by_magic(kind: u32) -> Option<String> {
    (!LOCAL_BY_MAGIC.iter().any(|(m, _)| *m == kind)).then(|| format!("{kind:#x}"))
}

/// The trusted filesystems, for the refusal.
fn trusted_list() -> String {
    if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )) {
        LOCAL_BY_NAME.join(", ")
    } else {
        let names: Vec<&str> = LOCAL_BY_MAGIC.iter().map(|(_, n)| *n).collect();
        names.join(", ")
    }
}

/// `Some(name)` unless the filesystem is a known local one, where OS
/// locks work.
fn network_filesystem(file: &File) -> io::Result<Option<String>> {
    let stat = rustix::fs::fstatfs(file).map_err(io::Error::from)?;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let name: String = stat
            .f_fstypename
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8 as char)
            .collect();
        Ok(untrusted_by_name(&name))
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        // Compared as u32: f_type is signed 32-bit on some targets, where
        // a widening cast would sign-extend the large magics.
        Ok(untrusted_by_magic(stat.f_type as u32))
    }
}

/// Every graph root a path lies under: what precedes each of its `.mdkg`
/// components, so a graph nested inside another graph's folder is checked
/// as well (Terra #7776).
fn graph_roots_of(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    parts
        .iter()
        .enumerate()
        .filter(|(_, p)| **p == ".mdkg")
        .map(|(at, _)| parts[..at].join("/"))
        .collect()
}

fn open_lock(common: &Path) -> Result<File> {
    let dir = graph_dir(common)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(dir.join("lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid("graph lock must be a regular file with one link"));
    }
    // The opened lock file's own filesystem and the graph folder's, on one
    // device: a lock file bind-mounted from elsewhere, or a local lock over
    // a shared folder, is refused. Other layouts are a stated residual
    // (orchestrator, on #6791).
    let folder = File::open(&dir)?;
    if folder.metadata()?.dev() != metadata.dev() {
        return Err(Error::LockRequired(
            "the lock file is on another device than the graph folder".into(),
        ));
    }
    if let Some(fs) = network_filesystem(&folder)?.or(network_filesystem(&file)?) {
        return Err(Error::LockRequired(format!(
            "the lock is on a {fs} filesystem, where OS locks can't be trusted. Keep the clone on a supported local filesystem: {}. Network, FUSE and host-shared mounts (NFS, SMB, sshfs, virtiofs, 9p) and encrypted layers such as eCryptfs are refused; overlayfs over a local disk, as containers use, is fine.",
            trusted_list()
        )));
    }
    Ok(file)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    commit: String,
}
impl Snapshot {
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// A snapshot a journal recorded (crate-only).
    pub(crate) fn recorded(commit: String) -> Self {
        Self { commit }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Put { path: String, bytes: Vec<u8> },
    Delete { path: String },
}
impl Change {
    pub fn path(&self) -> &str {
        match self {
            Self::Put { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// One write. Reuse this exact value after an ambiguous result, including
/// its id and base: the id is caller-supplied (`--request-id`, an MCP call
/// id, a turn-scoped id), so a rerun finds its pending or published commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: String,
    /// The tip the op read. `None` creates the graph ref (an orphan first
    /// commit, published by a create-CAS).
    pub base: Option<Snapshot>,
    pub changes: Vec<Change>,
    /// One line, after `mdkg: ` in the subject, such as `new task-1`.
    pub message: String,
    /// `Key: value` trailers, such as `Mdkg-Actor` and `Mdkg-Clone`.
    pub trailers: Vec<(String, String)>,
}

/// A prepared commit, pinned under `refs/mdkg/pending/<id>` and not yet
/// published. Every field is plain text, so an intent can hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub id: String,
    /// The request's base.
    pub base: Option<String>,
    /// The tip the commit was built on (its only parent).
    pub parent: Option<String>,
    pub commit: String,
    pub tree: String,
    /// The touched paths: what the commit changes.
    pub paths: Vec<String>,
    /// Every path the request named, unchanged ones included: each must
    /// still hold its preimage when the commit is rebuilt on a newer tip.
    pub preimages: Vec<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The published commit: the prepared one, or its rebuild on a newer tip.
    pub commit: String,
    pub head: String,
    /// An earlier attempt already published this request.
    pub already_applied: bool,
    /// The commit was rebuilt on a newer tip.
    pub rebuilt: bool,
    /// An already-applied request whose content a later commit has since
    /// replaced (a sync, say): it succeeded, and was overwritten afterwards.
    pub superseded: bool,
    pub retries: usize,
}

#[derive(Debug, Clone)]
pub struct RefWriter {
    repo: PathBuf,
    common: PathBuf,
    graph_ref: String,
    main_ref: Option<String>,
    lock_mode: LockMode,
    /// Graph-owned path prefixes, each ending in `/`.
    prefixes: Vec<String>,
    zero: String,
    identity: OnceLock<(String, String)>,
    /// The graph id every tip written on must hold; `None` only for a
    /// writer that creates the graph.
    graph_id: Option<String>,
    /// The graph root the bound id belongs to (`""`: the repository
    /// root): its `<root>/.mdkg/config.json` holds the id.
    graph_root: String,
    /// Other registered graphs, `(id, root)`: a write must keep each one's
    /// id where the tip holds it (no identity transfer in 0.7.1).
    registered: Vec<(String, String)>,
    /// 0.7.0 graphs (`.worktree`-only records): a write keeps each one's id
    /// wherever the parent held it.
    legacy: Vec<String>,
    /// Each recorded root's graph folder name, where it isn't `.mdkg`
    /// (orchestrator #8321).
    dirs: std::collections::BTreeMap<String, String>,
    /// For a creator: the id the created graph must carry.
    creating: Option<String>,
    /// Built by [`crate::graphref::GraphRef::writer`] or `creator`: every
    /// prepare and publish rechecks the clone's common config.
    trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    mode: String,
    kind: String,
    oid: String,
}
type Tree = BTreeMap<Vec<u8>, Entry>;
type Edit = (Vec<u8>, Option<Entry>);

/// Fault-injection points, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Blob,
    Tree,
    Commit,
    Pinned,
    BeforeCas,
    Published,
}

const PENDING: &str = "refs/mdkg/pending/";
const SUBJECT: &str = "mdkg: ";

impl RefWriter {
    /// `graph_ref` and `main_ref` are full ref names. The graph ref must be
    /// a branch (`refs/heads/...`): never a tag, a remote-tracking ref or
    /// any other namespace, never `main` or `master`, `main_ref` or its
    /// symbolic target, never a branch with an upstream (a tracking
    /// branch), and never a case-folded alias of another branch. It needn't
    /// exist yet, but it must not be symbolic or checked out on a branch in
    /// any worktree. Every prepare and publish rechecks all of this, the
    /// checkout immediately before the update. The only refs written are
    /// the graph ref and `refs/mdkg/pending/<id>`. `main_ref` is optional
    /// and never written. Paths are limited to `.mdkg/` until
    /// [`RefWriter::with_paths`] names the graph-owned set.
    ///
    /// A writer of an existing graph needs [`RefWriter::with_graph_id`];
    /// [`crate::graphref::GraphRef::writer`] sets it from the clone's
    /// identity record.
    pub(crate) fn open(
        repo: &Path,
        graph_ref: &str,
        main_ref: Option<&str>,
        lock_mode: LockMode,
    ) -> Result<Self> {
        let short = graph_ref.strip_prefix("refs/heads/").unwrap_or_default();
        if short.is_empty()
            || ["main", "master"]
                .iter()
                .any(|p| short.eq_ignore_ascii_case(p))
            || main_ref
                .is_some_and(|m| m.eq_ignore_ascii_case(graph_ref) || !m.starts_with("refs/"))
        {
            return Err(invalid(
                "the graph ref must be a branch (refs/heads/...) other than main, master and the main ref",
            ));
        }
        let repo = repo.canonicalize()?;
        let common = mdkg_git::common_dir(&repo)?;
        let mut writer = Self {
            repo,
            common,
            graph_ref: graph_ref.into(),
            main_ref: main_ref.map(Into::into),
            lock_mode,
            prefixes: vec![".mdkg/".into()],
            zero: String::new(),
            identity: OnceLock::new(),
            graph_id: None,
            graph_root: String::new(),
            registered: Vec::new(),
            legacy: Vec::new(),
            dirs: std::collections::BTreeMap::new(),
            creating: None,
            trusted: false,
        };
        for name in std::iter::once(graph_ref).chain(main_ref) {
            writer.git(&["check-ref-format", name], None)?;
        }
        let format = writer.git(&["rev-parse", "--show-object-format"], None)?;
        writer.zero = match text(&format)?.trim() {
            "sha256" => "0".repeat(64),
            _ => "0".repeat(40),
        };
        writer.writable()?;
        Ok(writer)
    }

    /// Requires every tip this writer prepares on or publishes to hold
    /// `graph_id` in its committed `.mdkg/config.json`
    /// (`graph.identity_mismatch` otherwise).
    /// Binds the writer's identity to the graph at `graph_root` (crate-only).
    pub(crate) fn with_graph_root(mut self, graph_root: &str) -> Self {
        self.graph_root = crate::graphref::normalize_root(graph_root);
        // A nested graph's own folder is the default write scope, never
        // the root graph's (Terra #7173); `with_paths` widens it.
        let dir = self.dir_of(&self.graph_root).to_owned();
        if !self.graph_root.is_empty() || dir != ".mdkg" {
            self.prefixes = vec![if self.graph_root.is_empty() {
                format!("{dir}/")
            } else {
                format!("{}/{dir}/", self.graph_root)
            }];
        }
        self
    }

    /// The other registered graphs a write must keep (crate-only).
    pub(crate) fn with_registered(mut self, registered: Vec<(String, String)>) -> Self {
        self.registered = registered;
        self
    }

    /// Each recorded root's graph folder name (crate-only).
    pub(crate) fn with_dirs(mut self, dirs: std::collections::BTreeMap<String, String>) -> Self {
        self.dirs = dirs;
        self
    }

    /// The graph folder name at `root`.
    fn dir_of(&self, root: &str) -> &str {
        self.dirs.get(root).map_or(".mdkg", String::as_str)
    }

    /// The 0.7.0 graphs a write must keep (crate-only).
    pub(crate) fn with_legacy(mut self, legacy: Vec<String>) -> Self {
        self.legacy = legacy;
        self
    }

    /// A creator whose created graph must carry `graph_id` (crate-only).
    pub(crate) fn with_creating(mut self, graph_id: &str) -> Self {
        self.creating = Some(graph_id.to_owned());
        self
    }

    pub(crate) fn with_graph_id(mut self, graph_id: &str) -> Self {
        self.graph_id = Some(graph_id.to_owned());
        self
    }

    /// Marks a writer the graph ref's resolution built (crate-only).
    pub(crate) fn trusted(mut self) -> Self {
        self.trusted = true;
        self
    }

    /// Rechecks the clone's common config: the graph ref and main ref must
    /// still be the ones this writer was built from, and its graph id must
    /// still be recorded (orchestrator, on Terra #6577: no caller-supplied
    /// refs).
    fn config_unchanged(&self) -> Result<()> {
        if !self.trusted {
            return Ok(());
        }
        let resolved = crate::graphref::resolve(&self.repo)
            .map_err(|e| invalid(format!("{}: {}", e.code, e.message)))?
            .ok_or_else(|| invalid("the clone no longer has a graph ref configured"))?;
        if resolved.name != self.graph_ref || resolved.main_ref != self.main_ref {
            return Err(invalid(
                "the graph ref or main ref changed in the clone's config; open a new writer",
            ));
        }
        if let Some(id) = &self.graph_id
            && resolved.check_identity_at(id, &self.graph_root).is_err()
        {
            return Err(Error::GraphIdentity(format!(
                "this clone no longer records graph {id}"
            )));
        }
        // A creator's own intent must still be there, unchanged, at every
        // prepare and right before the CAS (orchestrator #8573): an
        // abandoned or replaced creation never publishes.
        if let Some(id) = &self.creating {
            let intent = crate::graphref::read_creation(&self.common)
                .map_err(|e| invalid(format!("{}: {}", e.code, e.message)))?;
            // A completed creation (its id now recorded at its root) has
            // no intent any more, and its create's rerun still returns the
            // receipt.
            let completed = resolved
                .graph_roots
                .get(id)
                .is_some_and(|r| *r == self.graph_root);
            // Its folder too (Terra #10216): a creator for `.old` never
            // publishes once the intent was replaced by one for `.new`.
            let ours = intent
                .as_ref()
                .is_some_and(|(i, r, n)| i == id && *r == self.graph_root && *n == self.graph_ref)
                && crate::graphref::read_creation_dir(&self.common)
                    .map_err(|e| invalid(format!("{}: {}", e.code, e.message)))?
                    == self.dir_of(&self.graph_root);
            if !ours && !completed {
                return Err(Error::GraphIdentity(format!(
                    "the creation intent for graph {id} was removed or replaced; run the creation again"
                )));
            }
        }
        // The other graphs' bindings, as recorded now (orchestrator #6947),
        // for a creator too (Terra #7776).
        if let Some(id) = self.graph_id.as_ref().or(self.creating.as_ref()) {
            let mut now = resolved.others(id);
            now.sort();
            let mut had = self.registered.clone();
            had.sort();
            if now != had {
                return Err(Error::GraphIdentity(
                    "the clone's graph identity records changed; open a new writer".into(),
                ));
            }
            // Folder records and 0.7.0 records too (Terra #9366): a write
            // never lands in a folder authority no longer reads, or deletes
            // a graph recorded since. A creator's own folder isn't recorded
            // until completion.
            let mut dirs_now = resolved.root_dirs();
            let mut dirs_had = self.dirs.clone();
            if self.creating.is_some() {
                dirs_now.remove(&self.graph_root);
                dirs_had.remove(&self.graph_root);
            }
            let mut legacy_now = resolved.legacy_ids.clone();
            legacy_now.sort();
            let mut legacy_had = self.legacy.clone();
            legacy_had.sort();
            if dirs_now != dirs_had || legacy_now != legacy_had {
                return Err(Error::GraphIdentity(
                    "the clone's graph folder or 0.7.0 records changed; open a new writer".into(),
                ));
            }
        }
        Ok(())
    }

    /// Limits writes to these graph-owned prefixes (from
    /// [`crate::graphref::graph_paths`]), each a normalized folder.
    pub fn with_paths(mut self, prefixes: &[String]) -> Result<Self> {
        let mut out = Vec::new();
        for prefix in prefixes {
            let p = if prefix.ends_with('/') {
                prefix.clone()
            } else {
                format!("{prefix}/")
            };
            normalized(p.trim_end_matches('/'))?;
            out.push(p);
        }
        if out.is_empty() {
            return Err(invalid("no graph-owned paths"));
        }
        self.prefixes = out;
        Ok(self)
    }

    pub fn graph_ref(&self) -> &str {
        &self.graph_ref
    }

    /// The common git folder.
    pub fn common(&self) -> &Path {
        &self.common
    }

    pub fn lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// The graph lock for this repository.
    pub fn lock(&self, kind: LockKind) -> Result<GraphLock> {
        GraphLock::take(&self.common, self.lock_mode, kind)
    }

    /// The graph tip, or `None` while the graph ref doesn't exist.
    pub fn tip(&self) -> Result<Option<String>> {
        self.resolve(&self.graph_ref)
    }

    /// The graph tip, pinned. Fails while the graph ref doesn't exist.
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            commit: self
                .tip()?
                .ok_or_else(|| Error::Missing(self.graph_ref.clone()))?,
        })
    }

    /// Moves the graph ref from `old` to `new`, an existing commit that
    /// descends from `old`, by a compare-and-swap under the exclusive graph
    /// lock: graph sync's local step (edd-01a18867, Sync, "The
    /// algorithm"). `new` must hold this writer's graph. `false` when the
    /// ref no longer holds `old` (a lost race: the caller tries again).
    pub fn advance(&self, lock: &GraphLock, old: &str, new: &str) -> Result<bool> {
        self.held(lock)?;
        self.config_unchanged()?;
        self.writable()?;
        if !full_oid(old) || !full_oid(new) || self.head(new)? != new {
            return Err(invalid("advance takes two full commit object IDs"));
        }
        let ancestor = self.run(&["merge-base", "--is-ancestor", old, new], None)?;
        if !ancestor.status.success() {
            return Err(invalid(format!(
                "{new} doesn't descend from the graph tip {old}"
            )));
        }
        self.same_graph(Some(new))?;
        self.update_ref(&self.graph_ref, new, old, "mdkg: graph sync")
    }

    /// Moves the graph ref from `old` to `new`, which need not descend from
    /// it, by the same compare-and-swap as [`RefWriter::advance`]: graph
    /// sync adopting a rewritten remote a person accepted (DECISION 9).
    /// `kept` must be a ref under `refs/mdkg/backup/` that holds `old`
    /// already, so the history the move leaves is never lost. `false` when
    /// the ref no longer holds `old`.
    pub fn replace(&self, lock: &GraphLock, old: &str, new: &str, kept: &str) -> Result<bool> {
        self.held(lock)?;
        self.config_unchanged()?;
        self.writable()?;
        if !full_oid(old) || !full_oid(new) || self.head(new)? != new {
            return Err(invalid("replace takes two full commit object IDs"));
        }
        if !kept.starts_with("refs/mdkg/backup/") || self.resolve(kept)?.as_deref() != Some(old) {
            return Err(invalid(format!(
                "replacing the graph tip {old} needs a backup ref holding it, not {kept}"
            )));
        }
        self.same_graph(Some(new))?;
        self.update_ref(
            &self.graph_ref,
            new,
            old,
            "mdkg: graph sync adopts a rewrite",
        )
    }

    /// Restore a persisted request's base after process restart. Only a full
    /// commit object ID is accepted; moving ref names would change its meaning.
    pub fn snapshot_at(&self, commit: &str) -> Result<Snapshot> {
        if !full_oid(commit) || self.head(commit)? != commit {
            return Err(invalid("snapshot requires a full commit object ID"));
        }
        Ok(Snapshot {
            commit: commit.into(),
        })
    }

    /// Read a regular blob from the pinned commit, never from checkout files.
    pub fn read(&self, snapshot: &Snapshot, path: &str) -> Result<Option<Vec<u8>>> {
        self.validate_path(path)?;
        match self.entry(Some(&snapshot.commit), path)? {
            None => Ok(None),
            Some(e) if e.kind == "blob" && e.mode != "120000" => {
                Ok(Some(self.git(&["cat-file", "blob", &e.oid], None)?))
            }
            _ => Err(invalid("read path is not a regular blob")),
        }
    }

    /// Lock, prepare, publish and unpin in one call, for a caller with no
    /// store record to make in between.
    pub fn write(&self, request: &Request) -> Result<Receipt> {
        let lock = self.lock(LockKind::Exclusive)?;
        let prepared = self.prepare(&lock, request)?;
        let receipt = self.publish(&lock, &prepared)?;
        self.unpin(&lock, &prepared)?;
        Ok(receipt)
    }

    /// [`RefWriter::write`] with a fault hook at every step.
    #[cfg(test)]
    pub(crate) fn write_hooked(
        &self,
        request: &Request,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Receipt> {
        let lock = self.lock(LockKind::Exclusive)?;
        let prepared = self.prepare_with(&lock, request, hook)?;
        let receipt = self.publish_with(&lock, &prepared, hook)?;
        self.unpin(&lock, &prepared)?;
        Ok(receipt)
    }

    /// Builds the request's commit on the current tip and pins it. Moves no
    /// graph ref. A rerun with the same request returns the same prepared
    /// commit (pending) or the published one.
    pub fn prepare(&self, lock: &GraphLock, request: &Request) -> Result<Prepared> {
        self.prepare_with(lock, request, &mut |_| Ok(()))
    }

    pub(crate) fn prepare_with(
        &self,
        lock: &GraphLock,
        request: &Request,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Prepared> {
        self.held(lock)?;
        self.validate_request(request)?;
        self.config_unchanged()?;
        self.writable()?;
        let fingerprint = self.fingerprint(request)?;
        let paths: Vec<String> = request.changes.iter().map(|c| c.path().into()).collect();
        let base = request.base.as_ref().map(|b| b.commit.clone());
        if let Some((commit, found)) = self.receipt(&request.id)? {
            if found != fingerprint {
                return Err(Error::RequestIdReused(request.id.clone()));
            }
            // While the graph's history still holds it, even if a later
            // commit replaced its content; a rewound graph gets a fresh
            // prepare.
            if let Some(tip) = self.tip()?
                && self.exists(&commit)?
                && self.ancestor(&commit, &tip)?
                && self.carries_request(request, &commit)?
            {
                // Only on the graph a bound writer is bound to; a creator's
                // rerun of its own create reads, writes nothing.
                if self.graph_id.is_some() {
                    self.same_graph(Some(&tip))?;
                }
                // A pin left by a crash after a rebuilt publish is done with.
                let pending = format!("{PENDING}{}", request.id);
                if let Some(pinned) = self.resolve(&pending)? {
                    self.git(&["update-ref", "--no-deref", "-d", &pending, &pinned], None)?;
                }
                return self.prepared_from(&commit, request, fingerprint, paths);
            }
        }
        let pending = format!("{PENDING}{}", request.id);
        if let Some(commit) = self.resolve(&pending)? {
            // Only on the graph a bound writer is bound to (Terra #7173).
            if self.graph_id.is_some() {
                self.same_graph(self.tip()?.as_deref())?;
            }
            return self.pending_prepared(&commit, request, &fingerprint, paths);
        }
        let head = self.tip()?;
        match (&base, &head) {
            (Some(b), Some(h)) => {
                // A base from unrelated/replaced history is never a safe preimage.
                if !self.ancestor(b, h)? {
                    return Err(invalid("request base is not an ancestor of the graph"));
                }
            }
            (Some(_), None) => return Err(Error::Missing(self.graph_ref.clone())),
            (None, Some(_)) | (None, None) => {}
        }
        if let Some(h) = &head
            && let Some(commit) =
                self.applied(base.as_deref(), h, &request.id, &fingerprint, &mut |c| {
                    self.carries_request(request, c)
                })?
        {
            if self.graph_id.is_some() {
                self.same_graph(Some(h))?;
            }
            return self.prepared_from(&commit, request, fingerprint, paths);
        }
        if base.is_none() && head.is_some() {
            return Err(Error::Exists(self.graph_ref.clone()));
        }
        // Lookups above write nothing; everything below builds on the tip.
        self.same_graph(head.as_deref())?;
        let mut edits = Vec::new();
        for change in &request.changes {
            let before = self.entry(base.as_deref(), change.path())?;
            let current = self.entry(head.as_deref(), change.path())?;
            if before != current {
                return Err(Error::Stale {
                    path: change.path().into(),
                });
            }
            if current
                .as_ref()
                .is_some_and(|e| e.kind != "blob" || e.mode == "120000")
            {
                return Err(invalid("only regular files may be replaced or deleted"));
            }
            let entry = match change {
                Change::Put { bytes, .. } => {
                    let oid = self.git(
                        &["hash-object", "-w", "--no-filters", "--stdin"],
                        Some(bytes),
                    )?;
                    hook(Step::Blob)?;
                    Some(Entry {
                        mode: current.map_or_else(|| "100644".into(), |e| e.mode),
                        kind: "blob".into(),
                        oid: text(&oid)?.trim().into(),
                    })
                }
                Change::Delete { .. } => None,
            };
            // A put of the same bytes, or a delete of an absent file, changes
            // nothing: it's left out, so the commit's diff is the request's.
            let unchanged = match (&entry, self.entry(head.as_deref(), change.path())?) {
                (Some(new), Some(old)) => new.oid == old.oid && new.mode == old.mode,
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                edits.push((change.path().as_bytes().to_vec(), entry));
            }
        }
        if edits.is_empty() {
            // A create that writes nothing creates no graph (Terra #6994).
            if head.is_none() {
                return Err(invalid(
                    "the request changes nothing, so it can't create the graph",
                ));
            }
            return Err(Error::NoChange);
        }
        let preimages = paths;
        let paths: Vec<String> = edits
            .iter()
            .map(|(p, _)| String::from_utf8_lossy(p).into_owned())
            .collect();
        let old_tree = head.as_ref().map(|h| format!("{h}^{{tree}}"));
        let tree = self.rebuild(old_tree.as_deref(), &edits, hook)?;
        let message = commit_message(request, &fingerprint, &self.preimage_digest(&preimages)?);
        let parents: Vec<&str> = head.iter().map(String::as_str).collect();
        let commit = self.commit(&tree, &parents, &message)?;
        self.candidate_identity(&commit)?;
        hook(Step::Commit)?;
        if !self.update_ref(&pending, &commit, &self.zero, "mdkg pending")? {
            // Pinned meanwhile by a rerun of the same request.
            let other = self
                .resolve(&pending)?
                .ok_or_else(|| invalid("pending ref vanished"))?;
            return self.pending_prepared(&other, request, &fingerprint, preimages);
        }
        hook(Step::Pinned)?;
        Ok(Prepared {
            id: request.id.clone(),
            base,
            parent: head,
            commit,
            tree,
            paths,
            preimages,
            fingerprint,
        })
    }

    /// Publishes a prepared commit: CAS from its parent, or a rebuild on a
    /// newer tip whose touched paths still match the parent's. Returns the
    /// earlier publish when the request is already in `base..tip`. Leaves
    /// the pending ref for [`RefWriter::unpin`].
    pub fn publish(&self, lock: &GraphLock, prepared: &Prepared) -> Result<Receipt> {
        self.publish_with(lock, prepared, &mut |_| Ok(()))
    }

    pub(crate) fn publish_with(
        &self,
        lock: &GraphLock,
        prepared: &Prepared,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Receipt> {
        self.held(lock)?;
        self.config_unchanged()?;
        self.writable()?;
        for path in &prepared.paths {
            self.validate_path(path)?;
        }
        // A pinned record's paths must be exactly its pinned commit's
        // changed paths before they're used to recognize a publication
        // (orchestrator #9415): an edited (emptied) list never matches a
        // cherry-pick that dropped the change.
        if let Some(pinned) = self.resolve(&format!("{PENDING}{}", prepared.id))? {
            // Against the pinned commit's real parent (Terra #10216): a
            // record naming the pin itself as its parent would diff to
            // nothing and match any marker-only cherry-pick.
            let parent = self.resolve(&format!("{pinned}^1"))?;
            if parent != prepared.parent {
                return Err(invalid(format!(
                    "the prepared record for {} names another parent than its pinned commit's; it's kept",
                    prepared.id
                )));
            }
            let mut changed = self.changed_paths(parent.as_deref(), &pinned)?;
            changed.sort();
            let mut paths = prepared.paths.clone();
            paths.sort();
            if pinned != prepared.commit || changed != paths {
                return Err(invalid(format!(
                    "the prepared record for {} doesn't match its pinned commit; it's kept",
                    prepared.id
                )));
            }
        }
        // The record is verified before anything is published from it; an
        // already-applied request (found by receipt or history, a wider
        // squash included) is recognized first (Terra #8228).
        let mut bound = None;
        for retry in 0..16 {
            let head = self.tip()?;
            if let Some(h) = &head {
                // A receipt finds the publish from any base.
                if let Some((commit, found)) = self.receipt(&prepared.id)? {
                    if found != prepared.fingerprint {
                        return Err(Error::RequestIdReused(prepared.id.clone()));
                    }
                    if self.exists(&commit)?
                        && self.ancestor(&commit, h)?
                        && self.carries_prepared(prepared, &commit)?
                    {
                        if self.graph_id.is_some() {
                            self.same_graph(Some(h))?;
                        }
                        let superseded = !self.carries_prepared(prepared, h)?;
                        return Ok(Receipt {
                            commit,
                            head: h.clone(),
                            already_applied: true,
                            rebuilt: false,
                            superseded,
                            retries: retry,
                        });
                    }
                }
                if let Some(base) = &prepared.base
                    && !self.ancestor(base, h)?
                {
                    return Err(invalid("request base is not an ancestor of the graph"));
                }
                if let Some(commit) = self.applied(
                    prepared.base.as_deref(),
                    h,
                    &prepared.id,
                    &prepared.fingerprint,
                    &mut |c| self.carries_prepared(prepared, c),
                )? {
                    // Only on the graph a bound writer is bound to.
                    if self.graph_id.is_some() {
                        self.same_graph(Some(h))?;
                    }
                    self.record_receipt(&prepared.id, &commit, &prepared.fingerprint)?;
                    return Ok(Receipt {
                        commit,
                        head: h.clone(),
                        already_applied: true,
                        rebuilt: false,
                        superseded: !self.carries_prepared(prepared, h)?,
                        retries: retry,
                    });
                }
            }
            let bound = match bound {
                Some(b) => b,
                None => *bound.insert(self.verify_prepared(prepared)?),
            };
            // A create is a create-CAS only: never layered onto a graph
            // someone else created meanwhile.
            if prepared.parent.is_none() && head.is_some() {
                return Err(Error::Exists(self.graph_ref.clone()));
            }
            // The lookups above write nothing; a publish needs the graph.
            self.same_graph(head.as_deref())?;
            let (commit, rebuilt) = if head == prepared.parent {
                (prepared.commit.clone(), false)
            } else if !bound {
                // An intent an earlier build prepared carries no preimage
                // digest, so it's never rebuilt on a newer tip: published
                // only onto its own parent, else refused (orchestrator
                // #8234).
                return Err(invalid(
                    "this intent was prepared by an earlier mdkg and the graph moved since; run the request again",
                ));
            } else {
                let h = head
                    .as_deref()
                    .ok_or_else(|| Error::Missing(self.graph_ref.clone()))?;
                // A create is a create-CAS only: never layered onto a graph
                // someone else created meanwhile.
                if prepared.parent.is_none() {
                    return Err(Error::Exists(self.graph_ref.clone()));
                }
                if let Some(parent) = &prepared.parent
                    && !self.ancestor(parent, h)?
                {
                    return Err(invalid(
                        "the graph ref was rewritten under a prepared write",
                    ));
                }
                (self.rebuild_on(prepared, h, hook)?, true)
            };
            hook(Step::BeforeCas)?;
            // Under the lock, immediately before the update: the checkout,
            // the pin naming this very prepared commit (a new publication
            // needs one), and the candidate's graph identity.
            self.not_checked_out()?;
            if self
                .resolve(&format!("{PENDING}{}", prepared.id))?
                .as_deref()
                != Some(prepared.commit.as_str())
            {
                return Err(invalid(
                    "a prepared commit is published only while its pending ref names it",
                ));
            }
            self.candidate_identity(&commit)?;
            // The clone's config once more, under the lock, right before
            // the update. A `git config` write inside this last window is a
            // stated residual of one trust domain (orchestrator, on #6791).
            self.config_unchanged()?;
            let old = head.as_deref().unwrap_or(&self.zero);
            if self.update_ref(&self.graph_ref, &commit, old, "mdkg refwrite")? {
                hook(Step::Published)?;
                self.record_receipt(&prepared.id, &commit, &prepared.fingerprint)?;
                return Ok(Receipt {
                    head: commit.clone(),
                    commit,
                    already_applied: false,
                    rebuilt,
                    superseded: false,
                    retries: retry,
                });
            }
            // Lost a race with a writer that doesn't take the lock: every
            // path is checked again on the next tip.
        }
        Err(Error::RetryLimit)
    }

    /// The commit that published this request, if `base..tip` holds one.
    pub fn published(&self, prepared: &Prepared) -> Result<Option<String>> {
        match self.tip()? {
            None => Ok(None),
            Some(h) => self.applied(
                prepared.base.as_deref(),
                &h,
                &prepared.id,
                &prepared.fingerprint,
                &mut |c| self.carries_prepared(prepared, c),
            ),
        }
    }

    /// Drops a request's pending ref (after a publish, or when the store
    /// didn't record it). An absent pending ref is fine. Under the
    /// exclusive graph lock only, so a pin never goes between a publish's
    /// pin check and its CAS (orchestrator #7181).
    pub fn unpin(&self, lock: &GraphLock, prepared: &Prepared) -> Result<()> {
        self.held(lock)?;
        let pending = format!("{PENDING}{}", prepared.id);
        match self.resolve(&pending)? {
            None => Ok(()),
            Some(c) if c == prepared.commit => {
                self.git(&["update-ref", "--no-deref", "-d", &pending, &c], None)?;
                Ok(())
            }
            Some(_) => Err(invalid("the pending ref names another commit")),
        }
    }

    /// Drops the pending ref `id` if it still names `commit` (a pin with
    /// no intent, left by a crash between the pin and the intent write).
    /// Under the exclusive graph lock only, as [`RefWriter::unpin`].
    /// Returns whether the pin was dropped: a pin is kept, and the request
    /// reported unpublished, when its only receipt names a commit no
    /// longer on the graph (orchestrator #8586).
    pub fn unpin_orphan(&self, lock: &GraphLock, id: &str, commit: &str) -> Result<bool> {
        self.held(lock)?;
        let pending = format!("{PENDING}{id}");
        // A receipt proves publication only while its commit is reachable
        // from the tip (a rewind or reset can strand it).
        let receipt = self.receipt(id)?;
        let live = match (&receipt, self.tip()?) {
            (Some((c, _)), Some(tip)) => self.exists(c)? && self.ancestor(c, &tip)?,
            _ => false,
        };
        // A publish whose receipt write failed (or whose receipt went
        // stale): recover the receipt before the pin goes, so a rerun from
        // any base still finds it.
        let mut recovered = false;
        if !live
            && let Some((found_id, fingerprint)) =
                marker(&self.subject(commit)?).map(|(i, f)| (i.to_owned(), f.to_owned()))
            && found_id == id
        {
            let parent = self.resolve(&format!("{commit}^1"))?;
            let changed = self.changed_paths(parent.as_deref(), commit)?;
            let prepared = Prepared {
                id: id.to_owned(),
                base: parent.clone(),
                preimages: changed.clone(),
                paths: changed,
                tree: String::new(),
                parent,
                commit: commit.to_owned(),
                fingerprint: fingerprint.clone(),
            };
            if let Some(published) = self.published(&prepared)? {
                self.record_receipt(id, &published, &fingerprint)?;
                recovered = true;
            }
        }
        if receipt.is_some() && !live && !recovered {
            return Ok(false);
        }
        if self.resolve(&pending)?.as_deref() == Some(commit) {
            self.git(&["update-ref", "--no-deref", "-d", &pending, commit], None)?;
        }
        Ok(true)
    }

    /// Every pending ref: `(request id, commit)`.
    pub fn pending(&self) -> Result<Vec<(String, String)>> {
        let out = self.git(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)",
                PENDING,
            ],
            None,
        )?;
        let mut list = Vec::new();
        for line in text(&out)?.lines() {
            if let Some((name, oid)) = line.split_once('\0')
                && let Some(id) = name.strip_prefix(PENDING)
            {
                list.push((id.into(), oid.into()));
            }
        }
        Ok(list)
    }

    fn held(&self, lock: &GraphLock) -> Result<()> {
        if lock.kind != LockKind::Exclusive || lock.common != self.common.canonicalize()? {
            return Err(invalid(
                "a graph write needs this repository's exclusive graph lock",
            ));
        }
        Ok(())
    }

    fn pending_prepared(
        &self,
        commit: &str,
        request: &Request,
        fingerprint: &str,
        paths: Vec<String>,
    ) -> Result<Prepared> {
        let subject = self.subject(commit)?;
        match marker(&subject) {
            Some((id, found)) if id == request.id && found == fingerprint => {
                self.prepared_from(commit, request, fingerprint.into(), paths)
            }
            _ => Err(Error::RequestIdReused(request.id.clone())),
        }
    }

    fn prepared_from(
        &self,
        commit: &str,
        request: &Request,
        fingerprint: String,
        paths: Vec<String>,
    ) -> Result<Prepared> {
        let parent = self.resolve(&format!("{commit}^1"))?;
        let tree = text(&self.git(&["rev-parse", &format!("{commit}^{{tree}}")], None)?)?
            .trim()
            .to_owned();
        // The paths the commit really changes (a no-op put isn't one); the
        // request's whole list stays as the preimages.
        // A squash or cherry-pick that also changed other graph files is
        // still this request's publish: only the request's own paths count
        // (Terra #8228).
        let paths: Vec<String> = self
            .changed_paths(parent.as_deref(), commit)?
            .into_iter()
            .filter(|p| paths.contains(p))
            .collect();
        let preimages = request
            .changes
            .iter()
            .map(|c| c.path().to_owned())
            .collect();
        Ok(Prepared {
            id: request.id.clone(),
            base: request.base.as_ref().map(|b| b.commit.clone()),
            parent,
            commit: commit.into(),
            tree,
            paths,
            preimages,
            fingerprint,
        })
    }

    fn rebuild_on(
        &self,
        prepared: &Prepared,
        head: &str,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<String> {
        // Every path the request named, unchanged ones included, must still
        // hold its preimage on the newer tip.
        for path in prepared.preimages.iter().chain(&prepared.paths) {
            if self.entry(prepared.parent.as_deref(), path)? != self.entry(Some(head), path)? {
                return Err(Error::Stale { path: path.clone() });
            }
        }
        let mut edits = Vec::new();
        for path in &prepared.paths {
            edits.push((
                path.as_bytes().to_vec(),
                self.entry(Some(&prepared.commit), path)?,
            ));
        }
        let tree = self.rebuild(Some(&format!("{head}^{{tree}}")), &edits, hook)?;
        let raw = self.git(&["cat-file", "commit", &prepared.commit], None)?;
        let message = raw
            .windows(2)
            .position(|w| w == b"\n\n")
            .map(|i| &raw[i + 2..])
            .ok_or_else(|| invalid("prepared commit has no message"))?;
        // The prepared commit stays in history as the second parent, so
        // settlement finds the source the store recorded by ancestry.
        let rebuilt = self.commit(&tree, &[head, &prepared.commit], text(message)?)?;
        self.candidate_identity(&rebuilt)?;
        Ok(rebuilt)
    }

    /// A candidate commit must keep the graph's identity: a bound writer's
    /// graph id, and for a create, some graph id.
    fn candidate_identity(&self, commit: &str) -> Result<()> {
        let found = self.graph_id_at(commit)?;
        let expected = self.graph_id.as_ref().or(self.creating.as_ref());
        match (expected, &found) {
            (Some(expected), Some(found)) if expected == found => {}
            (None, Some(_)) => {}
            (expected, found) => {
                return Err(Error::GraphIdentity(format!(
                    "the write would leave graph {} where {} belongs",
                    found.as_deref().unwrap_or("(none)"),
                    expected.map_or("a graph id", String::as_str)
                )));
            }
        }
        // Every other registered graph keeps its id wherever the commit's
        // parent held it, nested ones included.
        let parent = self.resolve(&format!("{commit}^1"))?;
        // Every graph root the write touches may only hold an id recorded
        // at that root (orchestrator #6947): a moved or unrecorded config
        // is refused here, not left for the next authority read.
        let mut roots: Vec<String> = self
            .changed_paths(parent.as_deref(), commit)?
            .iter()
            .flat_map(|p| {
                // A recorded custom graph folder counts too.
                let mut roots = graph_roots_of(p);
                for (root, dir) in &self.dirs {
                    let folder = if root.is_empty() {
                        format!("{dir}/")
                    } else {
                        format!("{root}/{dir}/")
                    };
                    if p.starts_with(&folder) {
                        roots.push(root.clone());
                    }
                }
                roots
            })
            .collect();
        roots.sort();
        roots.dedup();
        let touched = roots.clone();
        for root in roots {
            let Some(id) = self.graph_id_at_root(commit, &root)? else {
                continue;
            };
            // An unbound creator's own root holds whatever id it creates,
            // as the check above allows.
            let own = (expected.is_none_or(|e| *e == id) && self.graph_root == root)
                || self
                    .registered
                    .iter()
                    .any(|(r, at)| *r == id && *at == root);
            if !own {
                return Err(Error::GraphIdentity(format!(
                    "the write would publish graph {id} at {root:?}, where this clone records no such graph; run `{BIN} install` (or migrate) to record it there"
                )));
            }
        }
        for (id, root) in &self.registered {
            let Some(parent) = &parent else { continue };
            if self.graph_id_at_root(parent, root)?.as_deref() == Some(id.as_str())
                && self.graph_id_at_root(commit, root)?.as_deref() != Some(id.as_str())
            {
                return Err(Error::GraphIdentity(format!(
                    "the write would change or remove registered graph {id} at {root:?}"
                )));
            }
        }
        // A 0.7.0 graph (readable through its `.worktree` record) keeps its
        // id at every root the write touches where the parent held it
        // (Terra #8551).
        if let Some(parent) = &parent {
            for root in &touched {
                if let Some(id) = self.graph_id_at_root(parent, root)?
                    && self.legacy.contains(&id)
                    && self.graph_id_at_root(commit, root)?.as_deref() != Some(id.as_str())
                {
                    return Err(Error::GraphIdentity(format!(
                        "the write would change or remove graph {id} at {root:?}, which this clone records the 0.7.0 way; migrate it first"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether `commit` names a commit object here (a pruned one doesn't).
    fn exists(&self, commit: &str) -> Result<bool> {
        Ok(self.resolve(commit)?.is_some())
    }

    fn fingerprint(&self, request: &Request) -> Result<String> {
        let mut bytes = Vec::new();
        let mut field = |value: &[u8]| {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value);
        };
        // The payload only, not the base: a rerun from a newer base with
        // the same payload is the same request.
        field(b"mdkg-refwrite-v3");
        field(self.graph_ref.as_bytes());
        field(request.message.as_bytes());
        for (key, value) in &request.trailers {
            field(key.as_bytes());
            field(value.as_bytes());
        }
        for change in &request.changes {
            field(change.path().as_bytes());
            match change {
                Change::Put { bytes, .. } => {
                    field(b"put");
                    field(bytes);
                }
                Change::Delete { .. } => field(b"delete"),
            }
        }
        Ok(text(&self.git(&["hash-object", "--stdin"], Some(&bytes))?)?
            .trim()
            .into())
    }

    /// The commit in `base..head` (all of `head` without a base) whose
    /// subject carries this request's marker and that `carries` confirms
    /// holds the request's change. A marker on a commit that doesn't hold
    /// the change (a copied subject, a conflict-resolved cherry-pick)
    /// proves nothing and is skipped. The same id with another
    /// fingerprint is a reuse error.
    fn applied(
        &self,
        base: Option<&str>,
        head: &str,
        id: &str,
        fingerprint: &str,
        carries: &mut dyn FnMut(&str) -> Result<bool>,
    ) -> Result<Option<String>> {
        let range = base.map_or_else(|| head.to_owned(), |b| format!("{b}..{head}"));
        // The commit that published the request on the graph's own line
        // first; a rebuild's prepared side commit carries the marker too.
        for first_parent in [true, false] {
            let mut args = vec!["log", "--no-show-signature", "--fixed-strings"];
            if first_parent {
                args.push("--first-parent");
            }
            let grep = format!("--grep=[{id} ");
            args.extend([grep.as_str(), "--format=%H%x00%s%x00", range.as_str(), "--"]);
            // Filter output, not ancestry: a marker in another commit's body
            // may match too, so check subjects below.
            let log = self.git(&args, None)?;
            let fields: Vec<_> = log.split(|&b| b == 0).collect();
            for pair in fields.chunks_exact(2) {
                let subject = String::from_utf8_lossy(pair[1]);
                if let Some((found_id, found)) = marker(&subject)
                    && found_id == id
                {
                    if found != fingerprint {
                        return Err(Error::RequestIdReused(id.into()));
                    }
                    let commit = text(pair[0])?.trim();
                    if carries(commit)? {
                        return Ok(Some(commit.into()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Whether `commit` holds exactly what the prepared commit holds at
    /// every touched path.
    fn carries_prepared(&self, prepared: &Prepared, commit: &str) -> Result<bool> {
        for path in &prepared.paths {
            if self.entry(Some(commit), path)? != self.entry(Some(&prepared.commit), path)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `commit` holds the request's changes.
    fn carries_request(&self, request: &Request, commit: &str) -> Result<bool> {
        for change in &request.changes {
            let found = self.entry(Some(commit), change.path())?;
            let ok = match change {
                Change::Delete { .. } => found.is_none(),
                Change::Put { bytes, .. } => {
                    let oid = self.git(&["hash-object", "--no-filters", "--stdin"], Some(bytes))?;
                    let oid = text(&oid)?.trim().to_owned();
                    found.is_some_and(|e| e.kind == "blob" && e.oid == oid)
                }
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// A digest of the preimage set, order-free: git's object id of the
    /// sorted, NUL-separated paths.
    fn preimage_digest(&self, preimages: &[String]) -> Result<String> {
        let mut sorted: Vec<&str> = preimages.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut bytes = Vec::new();
        for p in sorted {
            bytes.extend_from_slice(p.as_bytes());
            bytes.push(0);
        }
        let out = self.git(&["hash-object", "--stdin"], Some(&bytes))?;
        Ok(text(&out)?.trim().to_owned())
    }

    /// Checks a prepared record (from a journal, say) against its commit,
    /// so an edited record can't rewind the graph: the commit's marker,
    /// first parent, tree and changed paths must all match, and a pending
    /// ref for the request must name it.
    ///
    /// Returns whether the commit carries the preimage digest; one an
    /// earlier build prepared has none (Terra #8228).
    fn verify_prepared(&self, prepared: &Prepared) -> Result<bool> {
        let bad = |what: &str| {
            invalid(format!(
                "the prepared record doesn't match its commit: {what}"
            ))
        };
        if !full_oid(&prepared.commit) {
            return Err(bad("commit id"));
        }
        let subject = self.subject(&prepared.commit)?;
        if marker(&subject) != Some((prepared.id.as_str(), prepared.fingerprint.as_str())) {
            return Err(bad("marker"));
        }
        // Every preimage is a valid graph path (so no NUL can merge two
        // into one digest entry, Terra #8228), every changed path is a
        // preimage, and the set is the one the commit was prepared with.
        for path in prepared.preimages.iter().chain(&prepared.paths) {
            self.validate_path(path).map_err(|_| bad("preimages"))?;
        }
        if prepared
            .paths
            .iter()
            .any(|p| !prepared.preimages.contains(p))
        {
            return Err(bad("preimages"));
        }
        let body = self.git(&["cat-file", "commit", &prepared.commit], None)?;
        // The first one, after the subject: the one this writer wrote.
        let digest = text(&body)?
            .split_once("\n\n")
            .map_or("", |(_, message)| message)
            .lines()
            .find_map(|l| l.strip_prefix(PREIMAGES))
            .map(str::to_owned);
        let bound = digest.is_some();
        if let Some(digest) = digest
            && digest != self.preimage_digest(&prepared.preimages)?
        {
            return Err(bad("preimages"));
        }
        let parents = self.git(
            &["rev-list", "--parents", "-n", "1", &prepared.commit],
            None,
        )?;
        let parents: Vec<String> = text(&parents)?
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect();
        if parents.first() != prepared.parent.as_ref() || parents.len() > 2 {
            return Err(bad("parent"));
        }
        let tree = self.git(
            &["rev-parse", &format!("{}^{{tree}}", prepared.commit)],
            None,
        )?;
        if text(&tree)?.trim() != prepared.tree {
            return Err(bad("tree"));
        }
        let mut args = vec![
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            "--no-commit-id",
        ];
        match &prepared.parent {
            Some(p) => args.extend([p.as_str(), prepared.commit.as_str()]),
            None => args.extend(["--root", prepared.commit.as_str()]),
        }
        let changed = self.git(&args, None)?;
        let mut changed: Vec<String> = changed
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        changed.sort();
        let mut paths = prepared.paths.clone();
        paths.sort();
        if changed != paths {
            return Err(bad("paths"));
        }
        if let Some(pinned) = self.resolve(&format!("{PENDING}{}", prepared.id))?
            && pinned != prepared.commit
        {
            return Err(bad("pending ref"));
        }
        Ok(bound)
    }

    /// The paths `commit` changes against `parent` (all, for a root).
    fn changed_paths(&self, parent: Option<&str>, commit: &str) -> Result<Vec<String>> {
        let mut args = vec![
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            "--no-commit-id",
        ];
        match parent {
            Some(p) => args.extend([p, commit]),
            None => args.extend(["--root", commit]),
        }
        let out = self.git(&args, None)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect())
    }

    /// `<common>/mdkg/graph/receipts`.
    fn receipts(&self) -> io::Result<PathBuf> {
        let graph = graph_dir(&self.common)?;
        let dir = graph.join("receipts");
        private_dir(&dir, &graph)?;
        Ok(dir)
    }

    /// The receipt of a published request: `(commit, fingerprint)`.
    fn receipt(&self, id: &str) -> Result<Option<(String, String)>> {
        let dir = self.receipts()?;
        let path = dir.join(format!("{id}.json"));
        let bytes = match mdkg_git::read_capped(&path, 64 * 1024) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // A receipt is relied on only once it's durable: one whose earlier
        // folder sync failed is synced now, before any caller drops an
        // intent or a pin because of it (Terra #7480).
        File::open(&path)?.sync_all()?;
        File::open(&dir)?.sync_all()?;
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| invalid(format!("receipt {id}: {e}")))?;
        let field = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        match (field("commit"), field("fingerprint")) {
            (Some(c), Some(f)) => Ok(Some((c, f))),
            _ => Err(invalid(format!("receipt {id} is malformed"))),
        }
    }

    /// Records a published request durably, so a rerun from any base finds
    /// it (`graph.request_reused` when its payload differs).
    fn record_receipt(&self, id: &str, commit: &str, fingerprint: &str) -> Result<()> {
        let value = serde_json::json!({"id": id, "commit": commit, "fingerprint": fingerprint});
        crate::intent::write_durably(
            &self.receipts()?,
            &format!("{id}.json"),
            &serde_json::to_vec(&value).map_err(|e| invalid(e.to_string()))?,
        )?;
        Ok(())
    }

    fn subject(&self, commit: &str) -> Result<String> {
        let out = self.git(
            &[
                "log",
                "-1",
                "--no-show-signature",
                "--format=%s",
                commit,
                "--",
            ],
            None,
        )?;
        Ok(text(&out)?.trim_end().to_owned())
    }

    fn tree(&self, oid: &str) -> Result<Tree> {
        let raw = self.git(&["ls-tree", "--full-tree", "-z", oid], None)?;
        let mut tree = Tree::new();
        for record in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|&b| b == b'\t')
                .ok_or_else(|| invalid("ls-tree record"))?;
            let mut fields = text(&record[..tab])?.split(' ');
            let mode = fields.next().ok_or_else(|| invalid("tree mode"))?.into();
            let kind = fields.next().ok_or_else(|| invalid("tree kind"))?.into();
            let oid = fields.next().ok_or_else(|| invalid("tree oid"))?.into();
            tree.insert(record[tab + 1..].to_vec(), Entry { mode, kind, oid });
        }
        Ok(tree)
    }

    fn entry(&self, commit: Option<&str>, path: &str) -> Result<Option<Entry>> {
        let Some(commit) = commit else {
            return Ok(None);
        };
        let mut tree = commit.to_owned();
        let parts: Vec<_> = path.as_bytes().split(|&b| b == b'/').collect();
        for (i, name) in parts.iter().enumerate() {
            match self.tree(&tree)?.remove(*name) {
                Some(entry) if i + 1 == parts.len() => return Ok(Some(entry)),
                Some(entry) if entry.kind == "tree" => tree = entry.oid,
                // A file where a folder was: nothing is at the path below it.
                Some(_) => return Ok(None),
                None => return Ok(None),
            }
        }
        Err(invalid("empty path"))
    }

    fn rebuild(
        &self,
        old: Option<&str>,
        edits: &[Edit],
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<String> {
        let mut tree = old
            .map(|oid| self.tree(oid))
            .transpose()?
            .unwrap_or_default();
        let mut nested: BTreeMap<Vec<u8>, Vec<Edit>> = BTreeMap::new();
        for (path, entry) in edits {
            if let Some(slash) = path.iter().position(|&b| b == b'/') {
                nested
                    .entry(path[..slash].to_vec())
                    .or_default()
                    .push((path[slash + 1..].to_vec(), entry.clone()));
            } else if let Some(entry) = entry {
                tree.insert(path.clone(), entry.clone());
            } else {
                tree.remove(path);
            }
        }
        for (name, edits) in nested {
            let old = tree.get(&name);
            if old.is_some_and(|e| e.kind != "tree") {
                return Err(invalid("path crosses a non-tree entry"));
            }
            let oid = self.rebuild(old.map(|e| e.oid.as_str()), &edits, hook)?;
            if self.tree(&oid)?.is_empty() {
                tree.remove(&name);
            } else {
                tree.insert(
                    name,
                    Entry {
                        mode: "040000".into(),
                        kind: "tree".into(),
                        oid,
                    },
                );
            }
        }
        let mut input = Vec::new();
        for (name, entry) in tree {
            input.extend_from_slice(
                format!("{} {} {}\t", entry.mode, entry.kind, entry.oid).as_bytes(),
            );
            input.extend_from_slice(&name);
            input.push(0);
        }
        let oid = self.git(&["mktree", "-z"], Some(&input))?;
        hook(Step::Tree)?;
        Ok(text(&oid)?.trim().into())
    }

    /// The user's committer identity, read once from their own git config
    /// (global included) with hooks off. Never guessed from the host name.
    /// Inherited identity overrides (`GIT_AUTHOR_*`, `GIT_COMMITTER_*`,
    /// `EMAIL`) and system-config switches are ignored. `GIT_CONFIG_GLOBAL`
    /// is honoured when it names a file, since it is where Nix,
    /// home-manager and CI setups keep the user's global config; a
    /// `/dev/null` there (a hardened child's) is ignored, so the identity
    /// comes from `$HOME`'s config as it would without it. A relative
    /// path is read as the user's own `git` run in this process's folder
    /// reads it: from that worktree's top, or the folder itself outside
    /// one (reviews of 5fa1bd43 and f58c2058).
    fn identity(&self) -> Result<&(String, String)> {
        if let Some(found) = self.identity.get() {
            return Ok(found);
        }
        let found = commit_identity(&self.repo)?;
        Ok(self.identity.get_or_init(|| found))
    }

    fn commit(&self, tree: &str, parents: &[&str], message: &str) -> Result<String> {
        let (name, email) = self.identity()?.clone();
        let mut args = vec!["commit-tree", "--no-gpg-sign", tree];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        let out = self
            .command(&args)
            .env("GIT_AUTHOR_NAME", &name)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", &name)
            .env("GIT_COMMITTER_EMAIL", &email)
            .run(Some(message.as_bytes()))?;
        if !out.status.success() {
            return Err(git_error(&args, &out));
        }
        Ok(text(&out.stdout)?.trim().into())
    }

    fn ancestor(&self, a: &str, b: &str) -> Result<bool> {
        let args = ["merge-base", "--is-ancestor", a, b];
        let out = self.run(&args, None)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_error(&args, &out)),
        }
    }

    fn head(&self, name: &str) -> Result<String> {
        self.resolve(name)?
            .ok_or_else(|| invalid(format!("{name} doesn't name a commit")))
    }

    /// The commit a name resolves to, or `None` only when it provably
    /// names none (Terra #7266): a ref or HEAD naming a missing or
    /// unreadable object, or `<commit>^1` of a missing commit, is an
    /// error, never absence.
    fn resolve(&self, name: &str) -> Result<Option<String>> {
        let target = format!("{name}^{{commit}}");
        let args = [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            target.as_str(),
        ];
        let out = self.run(&args, None)?;
        match out.status.code() {
            Some(0) => return Ok(Some(text(&out.stdout)?.trim().into())),
            Some(1) => {}
            _ => return Err(git_error(&args, &out)),
        }
        // `<base>^<n>`: absent only when the base itself is a readable
        // commit (a root commit has no parent).
        if let Some((base, _)) = name.split_once('^') {
            return match self.resolve(base)? {
                Some(_) => Ok(None),
                None => Err(invalid(format!("{base} doesn't name a readable commit"))),
            };
        }
        // A bare object id names no other thing: no object is absence
        // (a pruned commit).
        if full_oid(name) {
            // Present but not a commit is an error, never absence (Opus
            // #8328).
            let out = self.run(&["cat-file", "-e", name], None)?;
            if out.status.success() {
                return Err(invalid(format!("{name} isn't a commit")));
            }
            return Ok(None);
        }
        // Unpeeled, a name that exists yields its object id even when the
        // object is missing; only a name that doesn't exist exits 1.
        let raw = ["rev-parse", "--verify", "--quiet", "--end-of-options", name];
        let out = self.run(&raw, None)?;
        match out.status.code() {
            Some(1) => Ok(None),
            Some(0) => Err(invalid(format!(
                "{name} names {}, which git can't read as a commit",
                text(&out.stdout)?.trim()
            ))),
            _ => Err(git_error(&raw, &out)),
        }
    }

    /// Whether `name` is a symbolic ref, and its target.
    fn symbolic(&self, name: &str) -> Result<Option<String>> {
        let args = ["symbolic-ref", "-q", name];
        let out = self.run(&args, None)?;
        match out.status.code() {
            Some(0) => Ok(Some(text(&out.stdout)?.trim().into())),
            Some(1) => Ok(None),
            _ => Err(git_error(&args, &out)),
        }
    }

    fn writable(&self) -> Result<()> {
        // --no-deref in CAS is a second guard against a symbolic target;
        // symbolic refs and graph branch checkouts are refused before objects.
        if self.symbolic(&self.graph_ref)?.is_some() {
            return Err(invalid("graph ref must not be symbolic"));
        }
        if let Some(main) = &self.main_ref
            && self
                .symbolic(main)?
                .is_some_and(|t| t.eq_ignore_ascii_case(&self.graph_ref))
        {
            return Err(invalid("the main ref is a symbolic alias of the graph ref"));
        }
        let short = &self.graph_ref["refs/heads/".len()..];
        for key in ["remote", "merge"] {
            // The tracking config as ordinary git sees it for this user,
            // inherited GIT_CONFIG_GLOBAL, GIT_CONFIG_SYSTEM and
            // GIT_CONFIG_NOSYSTEM included (orchestrator #7181, reversing
            // #6819 item 21); a read or parse failure fails closed.
            let key = format!("branch.{short}.{key}");
            // Inline config inherited from the environment
            // (GIT_CONFIG_COUNT/KEY_n/VALUE_n, GIT_CONFIG_PARAMETERS) is
            // passed through for git to apply, never scrubbed (orchestrator
            // #8234).
            let mut command = mdkg_git::command(&self.repo);
            for (name, value) in std::env::vars_os() {
                let n = name.to_string_lossy();
                if n == "GIT_CONFIG_COUNT"
                    || n == "GIT_CONFIG_PARAMETERS"
                    || n.starts_with("GIT_CONFIG_KEY_")
                    || n.starts_with("GIT_CONFIG_VALUE_")
                {
                    command = command.env(&name, &value);
                }
            }
            let out = command.args(["config", "--get", &key]).run(None)?;
            match out.status.code() {
                Some(1) => {}
                Some(0) => {
                    return Err(invalid(
                        "the graph branch has an upstream: a tracking branch can't be the graph ref",
                    ));
                }
                _ => {
                    return Err(invalid(format!(
                        "git couldn't read {key} (exit {:?})",
                        out.status.code()
                    )));
                }
            }
        }
        let branches = self.git(
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
            None,
        )?;
        if text(&branches)?
            .lines()
            .any(|r| r != self.graph_ref && r.eq_ignore_ascii_case(&self.graph_ref))
        {
            return Err(invalid(
                "another branch differs from the graph ref only in case",
            ));
        }
        self.not_checked_out()
    }

    /// Refuses a graph branch checked out in any worktree.
    fn not_checked_out(&self) -> Result<()> {
        let worktrees = self.git(&["worktree", "list", "--porcelain", "-z"], None)?;
        if worktrees.split(|&b| b == 0).any(|field| {
            field
                .strip_prefix(b"branch ")
                .is_some_and(|b| b.eq_ignore_ascii_case(self.graph_ref.as_bytes()))
        }) {
            return Err(invalid("graph ref is checked out; use a detached view"));
        }
        Ok(())
    }

    /// The `graph_id` committed at `commit`, if its root config names one.
    pub fn graph_id_at(&self, commit: &str) -> Result<Option<String>> {
        self.graph_id_at_root(commit, &self.graph_root.clone())
    }

    /// The `graph_id` committed at `commit` for the graph at `root`.
    pub fn graph_id_at_root(&self, commit: &str, root: &str) -> Result<Option<String>> {
        let dir = self.dir_of(root);
        let config = if root.is_empty() {
            format!("{dir}/config.json")
        } else {
            format!("{root}/{dir}/config.json")
        };
        let Some(entry) = self.entry(Some(commit), &config)? else {
            return Ok(None);
        };
        if entry.kind != "blob" {
            return Ok(None);
        }
        let size = self.git(&["cat-file", "-s", &entry.oid], None)?;
        if text(&size)?.trim().parse::<u64>().unwrap_or(u64::MAX) > 1024 * 1024 {
            return Err(invalid("the committed graph config is over 1 MiB"));
        }
        let bytes = self.git(&["cat-file", "blob", &entry.oid], None)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("the committed graph config: {e}")))?;
        Ok(value
            .get("graph_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned))
    }

    /// Refuses a tip whose graph isn't the expected one. A writer with no
    /// expected id may only create the graph.
    fn same_graph(&self, tip: Option<&str>) -> Result<()> {
        let Some(tip) = tip else {
            return Ok(());
        };
        let Some(expected) = &self.graph_id else {
            return Err(Error::GraphIdentity(format!(
                "{} exists, and this writer has no graph identity to check it against",
                self.graph_ref
            )));
        };
        let found = self.graph_id_at(tip)?;
        if found.as_deref() != Some(expected.as_str()) {
            return Err(Error::GraphIdentity(format!(
                "{} holds graph {}, not this clone's {expected}",
                self.graph_ref,
                found.as_deref().unwrap_or("(none)")
            )));
        }
        Ok(())
    }

    /// CAS `name` from `old` (the zero id: absent) to `new`. `false` when the
    /// ref no longer holds `old`; any other failure is an error.
    fn update_ref(&self, name: &str, new: &str, old: &str, reason: &str) -> Result<bool> {
        let args = ["update-ref", "--no-deref", "-m", reason, name, new, old];
        let out = self.run(&args, None)?;
        if out.status.success() {
            return Ok(true);
        }
        // A moved ref means a lost race. A stale Git lock or other failure at
        // the same value is a real error, never a reason to remove a lock file.
        let now = self.resolve(name)?;
        if now.as_deref().unwrap_or(&self.zero) != old {
            return Ok(false);
        }
        Err(git_error(&args, &out))
    }

    fn validate_path(&self, path: &str) -> Result<()> {
        normalized(path)?;
        if !self.prefixes.iter().any(|p| path.starts_with(p.as_str())) {
            return Err(invalid(format!(
                "{path} is outside the graph-owned folders"
            )));
        }
        Ok(())
    }

    fn validate_request(&self, request: &Request) -> Result<()> {
        if request.id.is_empty()
            || request.id.len() > 120
            || !request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid(
                "request ID must be 1..120 ASCII letters, digits, '-' or '_'",
            ));
        }
        if request.changes.is_empty()
            || request.message.is_empty()
            || request.message.contains(['\0', '\n', '\r'])
        {
            return Err(invalid("empty batch, or a message that isn't one line"));
        }
        for (key, value) in &request.trailers {
            if key.is_empty()
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || value.contains(['\0', '\n', '\r'])
            {
                return Err(invalid("a trailer must be `Key: one line`"));
            }
        }
        let mut paths = BTreeSet::new();
        for change in &request.changes {
            self.validate_path(change.path())?;
            if !paths.insert(change.path()) {
                return Err(invalid("duplicate operation path"));
            }
        }
        for path in &paths {
            for (i, _) in path.match_indices('/') {
                if paths.contains(&path[..i]) {
                    return Err(invalid("overlapping operation paths"));
                }
            }
        }
        Ok(())
    }

    /// A hardened git command: no hooks, no global or system config, no
    /// inherited redirection, objects and refs fsynced.
    fn command(&self, args: &[&str]) -> mdkg_git::Git {
        mdkg_git::command(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args([
                "-c",
                "core.fsync=objects,reference",
                "-c",
                "core.fsyncMethod=fsync",
                "-c",
                "user.useConfigOnly=true",
            ])
            .args(args)
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output> {
        Ok(self.command(args).run(input)?)
    }

    fn git(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let out = self.run(args, input)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(git_error(args, &out))
        }
    }
}

/// `mdkg: <message> [<id> <fingerprint>]`, a blank line, then the trailers.
fn commit_message(request: &Request, fingerprint: &str, preimages: &str) -> String {
    let mut message = format!(
        "{SUBJECT}{} [{} {fingerprint}]\n\n",
        request.message, request.id
    );
    // Binds the prepared record's preimage set to its commit (Terra
    // #7173): a journal edit that drops a preimage fails verification.
    // First, so the caller's trailers stay last and can't stand in for it.
    message.push_str(&format!("{PREIMAGES}{preimages}\n"));
    for (key, value) in &request.trailers {
        message.push_str(&format!("{key}: {value}\n"));
    }
    message
}

/// The trailer that carries a prepared commit's preimage digest.
const PREIMAGES: &str = "Mdkg-Preimages: ";

/// The `(id, fingerprint)` of an mdkg subject's marker.
fn marker(subject: &str) -> Option<(&str, &str)> {
    let rest = subject.strip_prefix(SUBJECT)?.strip_suffix(']')?;
    let (_, inner) = rest.rsplit_once(" [")?;
    let (id, fingerprint) = inner.split_once(' ')?;
    (!id.is_empty() && full_oid(fingerprint)).then_some((id, fingerprint))
}

fn full_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where git reads a relative `GIT_CONFIG_GLOBAL` from when run in this
/// process's folder: git moves to the worktree's top before it reads
/// config, and stays put outside a worktree. The folder's own worktree,
/// discovered with no pins: a pin (`MDKG_TRUSTED_WORKTREE`) names the
/// worktree mdkg writes, which needn't be the one the user runs in
/// (review of a5a90190).
fn relative_global_base() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    mdkg_git::checked_worktree_with_trust(&cwd, &mdkg_git::WorktreeTrust::default())
        .map_or(cwd, |checkout| checkout.top)
}

/// Whether an inherited `GIT_CONFIG_GLOBAL` names the user's global config:
/// an absolute path other than `/dev/null`.
fn global_config_honoured(path: &Path) -> bool {
    path.is_absolute() && path != Path::new("/dev/null")
}

/// Checks the same configured identity graph commits use, without opening a
/// writer, acquiring a lock or writing repository state. Setup callers can
/// refuse a missing identity before making any local changes.
pub fn check_commit_identity(repo: &Path) -> Result<()> {
    commit_identity(repo).map(|_| ())
}

fn commit_identity(repo: &Path) -> Result<(String, String)> {
    let global = std::env::var_os("GIT_CONFIG_GLOBAL")
        .map(PathBuf::from)
        .map(
            |path| match path.is_relative() && !path.as_os_str().is_empty() {
                true => relative_global_base().join(path),
                false => path,
            },
        );
    let honoured = global.clone().filter(|path| global_config_honoured(path));
    let mut command = mdkg_git::command(repo);
    // The user's git configuration, not inherited overrides.
    for var in [
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "EMAIL",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
    ] {
        command = command.env_remove(var);
    }
    if let Some(path) = &honoured {
        command = command.env("GIT_CONFIG_GLOBAL", path);
    }
    let failed = |why: &str| identity_error(why, global.as_deref(), honoured.is_some());
    let out = command
        .args([
            "-c",
            "user.useConfigOnly=true",
            "var",
            "GIT_COMMITTER_IDENT",
        ])
        .run(None)?;
    if !out.status.success() {
        // git's last line says what's missing; the lines before it are
        // its own advice, which names `--global` even where that's not
        // the file read here.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("git found none");
        return Err(failed(why));
    }
    let ident = text(&out.stdout)?.trim();
    let (name, rest) = ident
        .split_once(" <")
        .ok_or_else(|| failed("unparsable identity"))?;
    let (email, _) = rest
        .split_once('>')
        .ok_or_else(|| failed("unparsable identity"))?;
    if name.trim().is_empty() || email.trim().is_empty() {
        return Err(failed("empty name or email"));
    }
    Ok((name.into(), email.into()))
}

/// [`Error::Identity`] with the fix for the global file the identity read
/// used: `--global` writes where `GIT_CONFIG_GLOBAL` points when it was
/// honoured, and an ignored one would take the hinted write away, so the
/// hint then names `$HOME`'s file outright.
fn identity_error(why: &str, global: Option<&Path>, honoured: bool) -> Error {
    let fix = match global {
        Some(path) if !honoured => format!(
            "set one with `git config --file ~/.gitconfig user.email <you@example.com>` and `user.name` (GIT_CONFIG_GLOBAL={} is ignored here)",
            path.display()
        ),
        Some(path) => format!(
            "set one with `git config --global user.email <you@example.com>` and `user.name` (it writes {})",
            path.display()
        ),
        None => {
            "set one with `git config --global user.email <you@example.com>` and `user.name`".into()
        }
    };
    Error::Identity(format!("{why}; {fix}"))
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| invalid("Git returned non-UTF-8 metadata"))
}
fn git_error(args: &[&str], out: &Output) -> Error {
    Error::Git {
        operation: args[0].into(),
        detail: String::from_utf8_lossy(&out.stderr).trim().into(),
    }
}
/// A normalized, repository-relative path that git can store.
fn normalized(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\0', '\\'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git"))
    {
        return Err(invalid(format!(
            "{path:?} isn't a normalized repository path"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
