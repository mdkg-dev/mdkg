//! The coordinator store: a command journal plus a state snapshot in SQLite.
//!
//! Every write runs in an `IMMEDIATE` transaction, so processes in different
//! worktrees take turns: each loads the state, applies one command, and
//! writes the journal entry, the new state and the lease events together.
//! WAL mode lets readers proceed while a writer holds the lock.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mdkg_coord::{Command, Coordinator, Ms, Observation, Outcome, Scope};
use rusqlite::backup::Backup;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::git;

/// "3" since every write appends a record to the `log` chain:
/// a binary that predates the log would change the state without a
/// record, so it must refuse the store. Opening a "1" or "2" store
/// upgrades it in place, adding the table and the chain's genesis record;
/// nothing it held changes.
///
/// "2" since leases carry a `mode`: a binary that predates modes
/// would drop it from the state it saves, turning shared leases exclusive
/// behind the journal's back. A lease or claim without a mode reads as
/// exclusive. There is no way back: an older binary refuses an upgraded
/// store.
const SCHEMA_VERSION: &str = "3";

/// Versions this binary upgrades in place on open.
const UPGRADES_FROM: &[&str] = &["1", "2"];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS journal (
    seq INTEGER PRIMARY KEY,
    command TEXT NOT NULL,
    outcome TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL,
    coordinator TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL,
    event TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS log (
    seq INTEGER PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    record TEXT NOT NULL
);
";

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The database was written by a newer, incompatible schema.
    Schema(String),
    /// A `meta` value can't be read (a damaged store).
    BadMeta(String),
    /// The state snapshot doesn't match the journal; run `rebuild`.
    Inconsistent {
        state_seq: i64,
        journal_seq: i64,
    },
    /// Replaying the journal gave a different outcome at this entry.
    ReplayDiverged(i64),
    /// The state snapshot differs from a replay of the journal.
    SnapshotDiffers,
    /// The copy taken before an upgrade couldn't be written: nothing was
    /// upgraded.
    Backup(String),
    /// A restore refused, or failed before changing anything.
    Restore(String),
    /// Opening would upgrade an existing store from `from` to `to`, and
    /// this process may not (see [`Upgrade`]): nothing was changed.
    UpgradeRefused {
        from: String,
        to: String,
    },
    /// The record chain can't be written or doesn't verify.
    Log(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "coordinator store: {e}"),
            StoreError::Io(e) => write!(f, "coordinator store: {e}"),
            StoreError::Json(e) => write!(f, "coordinator store: {e}"),
            StoreError::Schema(v) => write!(f, "coordinator store has unsupported schema {v}"),
            StoreError::BadMeta(v) => write!(f, "coordinator store has an unreadable {v}"),
            StoreError::Inconsistent {
                state_seq,
                journal_seq,
            } => write!(
                f,
                "coordinator state is at {state_seq} but the journal at {journal_seq}; rebuild it"
            ),
            StoreError::ReplayDiverged(seq) => {
                write!(f, "replaying the journal diverged at entry {seq}")
            }
            StoreError::SnapshotDiffers => write!(
                f,
                "the coordinator's state snapshot differs from a replay of its journal; run rebuild"
            ),
            StoreError::Backup(why) => write!(
                f,
                "couldn't copy the coordinator store before upgrading it, so it wasn't upgraded: {why}"
            ),
            StoreError::Restore(why) => write!(f, "coordinator store not restored: {why}"),
            StoreError::UpgradeRefused { from, to } => write!(
                f,
                "this build runs from a Cargo build folder (or its location can't be checked), \
                 so it won't upgrade the existing coordinator store from {from} to {to}; only an \
                 installed build upgrades a store"
            ),
            StoreError::Log(why) => write!(f, "coordinator log: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

/// Whether opening may upgrade an existing store's schema.
///
/// The 10-22 incident: a branch build run once inside a live repository
/// upgraded the store every worktree shares, after which the pinned build
/// refused it. So only an installed build upgrades a store by default; a
/// build running from a Cargo build folder (`cargo build`, `cargo run`,
/// `cargo test`) refuses, unless it's told to. Creating a new store is not
/// an upgrade, so tests in scratch repositories work as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade {
    /// Upgrade an older store, copying it first.
    Allow,
    /// Refuse with [`StoreError::UpgradeRefused`], changing nothing.
    Refuse,
}

/// Set by [`allow_store_upgrades`] (`--allow-store-upgrade`).
static UPGRADES_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Lets this process upgrade stores even from a Cargo build folder.
pub fn allow_store_upgrades() {
    UPGRADES_ALLOWED.store(true, Ordering::SeqCst);
}

/// Whether [`allow_store_upgrades`] was called, so a process can pass
/// `--allow-store-upgrade` on to the mdkg processes it starts.
pub fn store_upgrades_allowed() -> bool {
    UPGRADES_ALLOWED.load(Ordering::SeqCst)
}

impl Upgrade {
    /// This process's policy: allowed after [`allow_store_upgrades`] or
    /// for a binary outside any Cargo build folder, and refused otherwise,
    /// including when the binary's own path can't be read.
    pub fn for_this_process() -> Self {
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        if *INSTALLED.get_or_init(installed) || store_upgrades_allowed() {
            Upgrade::Allow
        } else {
            Upgrade::Refuse
        }
    }
}

/// Whether this binary is installed: outside any Cargo build folder, and
/// still where it was found once its folders are checked. A build folder
/// removed while they're read (`cargo clean`) shows no markers, but takes
/// the binary with it.
fn installed() -> bool {
    installed_at(|| std::env::current_exe().and_then(std::fs::canonicalize))
}

fn installed_at(mut here: impl FnMut() -> std::io::Result<PathBuf>) -> bool {
    let Ok(exe) = here() else {
        return false;
    };
    !in_build_folder(&exe) && here().is_ok_and(|again| again == exe && again.is_file())
}

/// Whether `exe` is a Cargo build of this code rather than an install
/// of it: it sits in the Cargo profile folder this crate was built in
/// (where its build script's `OUT_DIR` is), or in a folder Cargo marks as
/// its own. `cargo install`, cargo-dist and package managers copy the
/// binary out of both, wherever they put it (a workspace's own `.tools`
/// included).
pub fn in_build_folder(exe: &Path) -> bool {
    built_in(exe, profile_folder(Path::new(env!("OUT_DIR"))).as_deref()) || has_cargo_markers(exe)
}

/// The profile folder (`<target>/[<triple>/]<profile>`) a build script's
/// `OUT_DIR` (`<profile>/build/<crate>-<hash>/out`) sits in.
fn profile_folder(out_dir: &Path) -> Option<PathBuf> {
    let build = out_dir.ancestors().nth(2)?;
    (build.file_name()? == "build").then(|| build.parent().map(Path::to_path_buf))?
}

/// Whether `exe` is inside `profile`, compared as the filesystem resolves
/// them.
fn built_in(exe: &Path, profile: Option<&Path>) -> bool {
    profile.is_some_and(|dir| {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        exe.starts_with(dir)
    })
}

/// Whether a folder up to four above `exe` (`target/<profile>/`,
/// `target/<profile>/deps/`, `target/<triple>/<profile>/`) is a Cargo
/// build folder: Cargo marks its root with a `CACHEDIR.TAG` naming itself,
/// or with `.rustc_info.json`. A marker that can't be checked, for any
/// reason but its absence, counts as present.
pub fn has_cargo_markers(exe: &Path) -> bool {
    let absent = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    exe.ancestors().skip(1).take(4).any(|dir| {
        let info = match std::fs::metadata(dir.join(".rustc_info.json")) {
            Ok(m) => m.is_file(),
            Err(e) => !absent(&e),
        };
        info || match std::fs::read_to_string(dir.join("CACHEDIR.TAG")) {
            Ok(tag) => tag.contains("created by cargo"),
            Err(e) => !absent(&e),
        }
    })
}

/// What [`Store::execute_then`] did.
#[derive(Debug)]
pub struct Chained {
    pub first: Outcome,
    pub second: Option<Outcome>,
    /// Both were recorded; false when the second was refused, which left
    /// nothing recorded.
    pub recorded: bool,
}

/// What [`Store::execute_chain`] did: every outcome, in order.
#[derive(Debug)]
pub struct ChainOutcomes {
    pub outcomes: Vec<Outcome>,
    /// All were recorded; false when a later one was refused, which left
    /// nothing recorded.
    pub recorded: bool,
}

#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
    /// Device/inode checked around SQLite's open. No independent descriptor:
    /// closing one could release this process's SQLite POSIX locks.
    file: Option<(u64, u64)>,
    ttl: Ms,
    /// The copy this open took before upgrading the store, if it did.
    backup: Option<PathBuf>,
}

fn journal_seq(tx: &Connection) -> Result<i64, StoreError> {
    Ok(
        tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM journal", [], |r| {
            r.get(0)
        })?,
    )
}

/// Refuses a store another build has moved to a different schema since
/// this handle opened it: checked inside every transaction, so a process
/// that opened before an upgrade can't write a state that drops new fields.
pub(crate) fn check_schema(conn: &Connection) -> Result<(), StoreError> {
    let version: String =
        conn.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })?;
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(schema_error("schema", &version, SCHEMA_VERSION))
    }
}

/// The same for the mail tables' own version, which mail transactions also
/// check, so a process that opened before a mail upgrade writes nothing.
pub(crate) fn check_mail_schema(conn: &Connection) -> Result<(), StoreError> {
    check_schema(conn)?;
    let version: String =
        conn.query_row("SELECT value FROM meta WHERE key = 'msg_schema'", [], |r| {
            r.get(0)
        })?;
    if version == crate::mail::MAIL_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(schema_error(
            "msg_schema",
            &version,
            crate::mail::MAIL_SCHEMA_VERSION,
        ))
    }
}

/// This build's versions as a label (`2+msg3`).
pub fn build_label() -> String {
    label(SCHEMA_VERSION, Some(crate::mail::MAIL_SCHEMA_VERSION))
}

/// A store's versions as a label: the schema, and the mail tables' own
/// version when they exist (`2+msg3`).
fn label(schema: &str, mail: Option<&str>) -> String {
    match mail {
        Some(m) => format!("{schema}+msg{m}"),
        None => schema.to_string(),
    }
}

/// Reads one `meta` value, if the table and the key exist.
fn meta(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    let has_meta = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'")?
        .exists([])?;
    if !has_meta {
        return Ok(None);
    }
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

/// The upgrade opening this store makes, as `(from, to)` labels: none for a
/// new store, one at this build's versions, or one no build here reads.
fn upgrade_of(conn: &Connection) -> Result<Option<(String, String)>, StoreError> {
    let Some(schema) = meta(conn, "schema")? else {
        return Ok(None);
    };
    let mail = meta(conn, "msg_schema")?;
    let main_up = UPGRADES_FROM.contains(&schema.as_str());
    // No mail tables yet, or a version the mail upgrade moves on.
    let mail_up = match mail.as_deref() {
        None => true,
        Some(m) => crate::mail::UPGRADES_FROM.contains(&m),
    };
    if !(main_up || (schema == SCHEMA_VERSION && mail_up)) {
        return Ok(None);
    }
    Ok(Some((
        label(&schema, mail.as_deref()),
        label(SCHEMA_VERSION, Some(crate::mail::MAIL_SCHEMA_VERSION)),
    )))
}

/// The upgrade opening the store at `path` would make, read without
/// writing anything: through a read-only connection that doesn't
/// checkpoint a WAL a crashed writer left when it closes. A store that
/// doesn't exist yet needs none.
fn probe_upgrade(path: &Path) -> Result<Option<(String, String)>, StoreError> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > 0 => {}
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let probe = |flags: OpenFlags| -> Result<Option<(String, String)>, StoreError> {
        let probe = open_durable(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        probe.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        upgrade_of(&probe)
    };
    match probe(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        // A crashed writer's hot rollback journal (a store a restore left
        // in rollback mode) needs a writer to roll it back. That puts back
        // the store's last committed state, which is what it holds.
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
            if e.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK =>
        {
            probe(OpenFlags::SQLITE_OPEN_READ_WRITE)
        }
        other => other,
    }
}

/// Where the copies of the store at `path` go: beside it, as
/// `<name>.pre-<from>-to-<to>.sqlite`, with a number when that's taken.
fn backup_path(path: &Path, from: &str, to: &str) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            dir.join(format!("{name}.pre-{from}-to-{to}.sqlite"))
        } else {
            dir.join(format!("{name}.pre-{from}-to-{to}.{n}.sqlite"))
        };
        if std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
        n += 1;
    }
}

/// Opens a connection to a store, or a copy of one, that commits
/// durably (orchestrator, 2026-10-25, from lane 1): `synchronous=FULL`, so
/// a commit reported done survives power loss, not just a crash of the
/// process (`NORMAL` in WAL mode can lose the last commits), and on macOS
/// `fullfsync`, since a plain fsync there leaves data in the drive's
/// cache. The coordinator writes rarely, so the cost is a few ms a
/// command. Set on every connection, read-only ones included, so no path
/// opens one with weaker settings.
fn open_durable(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags)?;
    // Before anything else touches the file: every statement after this
    // waits for a busy store instead of failing at once. Callers that must
    // not wait (restore) set their own afterwards.
    conn.busy_timeout(BUSY_WAIT)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "fullfsync", true)?;
    conn.pragma_update(None, "checkpoint_fullfsync", true)?;
    Ok(conn)
}

/// How long a store connection waits for another process's lock.
const BUSY_WAIT: Duration = Duration::from_secs(30);

/// Moves the store to WAL mode, if it isn't there already. SQLite never
/// waits on its busy handler for a journal-mode change, so a plain
/// `PRAGMA journal_mode = WAL` fails at once while another process holds
/// the store: the mode is read first (a read, which does wait), and only a
/// store not yet in WAL is switched, retrying a busy refusal for as long
/// as a write would wait.
fn to_wal(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + BUSY_WAIT;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Syncs a file to the disk. A filesystem that can't (some network and FUSE
/// mounts reject macOS's F_FULLFSYNC as unsupported) is left unsynced
/// rather than failing the upgrade; I/O errors still fail it.
fn sync(file: &File) -> std::io::Result<()> {
    match file.sync_all() {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
            ) || (cfg!(target_vendor = "apple") && e.raw_os_error() == Some(45)) =>
        {
            // ENOTSUP (45 on Apple, where std doesn't call it unsupported):
            // what SMB mounts answer to F_FULLFSYNC.
            Ok(())
        }
        other => other,
    }
}

/// A store's own id, minted once and carried by its copies, so a copy
/// still matches the store after the repository moves.
fn mint_store_id(path: &Path) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!("{nanos}-{}-{}", std::process::id(), path.display());
    mdkg_format::Digest::of_bytes(seed.as_bytes()).hex()[..32].to_string()
}

/// Copies every page of `from` into `to` in one step, which writes `to` in
/// one transaction.
fn copy_all(from: &Connection, to: &mut Connection) -> Result<(), StoreError> {
    match Backup::new(from, to)?.step(-1)? {
        rusqlite::backup::StepResult::Done => Ok(()),
        other => Err(StoreError::Backup(format!("the copy stopped: {other:?}"))),
    }
}

/// An additive meta key: stores predating activity cursors start at zero.
/// Read without writing, so a receipt read cannot mutate the store.
pub(crate) fn restore_generation(conn: &Connection) -> Result<u64, StoreError> {
    meta(conn, "restore_generation")?
        .map(|v| {
            v.parse()
                .map_err(|_| StoreError::BadMeta(format!("restore_generation {v:?}")))
        })
        .transpose()
        .map(|v| v.unwrap_or(0))
}

/// Prepare a private SQLite temporary database, which SQLite deletes on close.
/// Stamp the next generation before importing any pages into the live store:
/// the restore's one backup transaction then lands content and generation
/// together, even if the process exits before post-copy tidying. The saved
/// backup remains untouched. A temporary database can spill pages to disk.
fn restore_image(source: &Connection, current: u64) -> Result<Connection, StoreError> {
    let mut image = open_durable(Path::new(""), OpenFlags::default())?;
    copy_all(source, &mut image)?;
    let next = current
        .max(restore_generation(&image)?)
        .checked_add(1)
        .ok_or_else(|| StoreError::Restore("restore generation is exhausted".into()))?;
    image.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('restore_generation', ?1)",
        [next.to_string()],
    )?;
    Ok(image)
}

/// Copies the store `conn` has open (at `path`, with no transaction of its
/// own open) with SQLite's backup API, syncs the copy and its folder, and
/// marks it with the store it came from. Written under a temporary name
/// of its own and published without replacing anything, so a copy that
/// exists is complete, and two copies taken at once (snapshots hold no
/// lock) each land whole under their own names.
fn back_up(conn: &Connection, path: &Path, from: &str, to: &str) -> Result<PathBuf, StoreError> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fail = |e: &dyn fmt::Display| StoreError::Backup(e.to_string());
    let of = std::fs::canonicalize(path).map_err(|e| fail(&e))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let partial = backup_path(path, from, to).with_extension(format!(
        "sqlite.partial-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let result = (|| -> Result<PathBuf, StoreError> {
        let mut copy = open_durable(
            &partial,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        copy_all(conn, &mut copy)?;
        // The source's WAL header came along: a copy that reads without
        // creating `-wal` and `-shm` beside it.
        let _: String = copy.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
        copy.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('backup_of', ?1)",
            [of.to_string_lossy()],
        )?;
        copy.close().map_err(|(_, e)| e)?;
        sync(&File::open(&partial)?)?;
        // Published under the first free name: a link fails rather than
        // replace a copy another process published there first.
        let target = loop {
            let target = backup_path(path, from, to);
            match std::fs::hard_link(&partial, &target) {
                Ok(()) => break target,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        };
        std::fs::remove_file(&partial).ok();
        // As SQLite does: a folder that can't be synced doesn't fail it.
        if let Some(dir) = target.parent()
            && let Ok(dir) = File::open(dir)
        {
            sync(&dir).ok();
        }
        Ok(target)
    })();
    if result.is_err() {
        std::fs::remove_file(&partial).ok();
    }
    result.map_err(|e| fail(&e))
}

/// Copies the store at `path` beside it, as an upgrade does, named
/// `<name>.pre-<its versions>-to-<label>.sqlite` and marked as its copy,
/// so `store snapshots` lists it and `store restore` takes it (`migrate`
/// takes one before it changes anything).
pub fn snapshot(path: &Path, label: &str) -> Result<PathBuf, StoreError> {
    let bad = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+');
    if label.is_empty() || label.chars().any(bad) {
        return Err(StoreError::Backup(format!(
            "a snapshot label is letters, digits, -, _ and +, not {label:?}"
        )));
    }
    let reader = open_durable(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let from = label_of(&reader)?;
    back_up(&reader, path, &from, label)
}

/// A store's versions as a label, read from its `meta`.
fn label_of(conn: &Connection) -> Result<String, StoreError> {
    Ok(label(
        &meta(conn, "schema")?.unwrap_or_default(),
        meta(conn, "msg_schema")?.as_deref(),
    ))
}

/// A copy of a store, as `store snapshots` lists it.
#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub schema: Option<String>,
    pub msg_schema: Option<String>,
    /// The store it was copied from.
    pub backup_of: Option<String>,
    /// The store's own id, as the copy carries it.
    pub store_id: Option<String>,
    pub bytes: u64,
    pub modified_ms: u64,
}

/// Whether a copy (marked `backup_of`, carrying `store_id`, at `copy`) is
/// one of the store at `path`: the same path, or, after the repository
/// moved, the same `store_id` with the copy beside the store (so a clone
/// made with `cp -a` doesn't take another clone's copies).
pub fn copy_of(path: &Path, copy: &Path, backup_of: Option<&str>, store_id: Option<&str>) -> bool {
    let Ok(of) = std::fs::canonicalize(path) else {
        return false;
    };
    if backup_of.is_some_and(|b| Path::new(b) == of) {
        return true;
    }
    let beside = std::fs::canonicalize(copy)
        .ok()
        .and_then(|c| c.parent().map(Path::to_path_buf))
        == of.parent().map(Path::to_path_buf);
    let ours = open_durable(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|c| meta(&c, "store_id").ok().flatten());
    beside && backup_of.is_some() && store_id.is_some() && store_id == ours.as_deref()
}

/// The copies taken of the store at `path`, oldest first.
pub fn backups(path: &Path) -> Result<Vec<BackupInfo>, StoreError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{name}.pre-");
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.starts_with(&prefix) || !file.ends_with(".sqlite") {
            continue;
        }
        let meta_fs = std::fs::symlink_metadata(entry.path())?;
        if !meta_fs.is_file() {
            continue;
        }
        let read = open_durable(
            &entry.path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        );
        let value = |key: &str| read.as_ref().ok().and_then(|c| meta(c, key).ok().flatten());
        found.push(BackupInfo {
            path: entry.path(),
            schema: value("schema"),
            msg_schema: value("msg_schema"),
            backup_of: value("backup_of"),
            store_id: value("store_id"),
            bytes: meta_fs.len(),
            modified_ms: meta_fs
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default(),
        });
    }
    found.sort_by_key(|b| (b.modified_ms, b.path.clone()));
    Ok(found)
}

/// What a restore did.
#[derive(Debug, Clone)]
pub struct Restored {
    /// The copy restored.
    pub from: PathBuf,
    /// The store's versions before and after (labels like `2+msg3`).
    pub replaced: String,
    pub restored: String,
    /// A copy of the store as it was just before, so a restore can be
    /// undone the same way.
    pub previous: PathBuf,
}

/// Restores the store at `path` from `copy`, one of its own copies:
/// refused while any process has the store open, for a copy of another
/// store, and for one a newer build wrote. The store as it was is copied
/// first; the restore itself is one transaction, so it lands whole or not
/// at all, and leaves no WAL behind.
pub fn restore(path: &Path, copy: &Path) -> Result<Restored, StoreError> {
    restore_checked(path, copy, None, || {})
}

/// [`restore`], refused unless the store's record chain still ends at
/// `expected_head` once the restore holds its exclusive lock, so nothing
/// that landed after that record is thrown away (`migrate --rollback`
/// passes its import's last record). A refusal restores nothing and
/// leaves the store in WAL mode, as it was.
pub fn restore_if_head(
    path: &Path,
    copy: &Path,
    expected_head: &str,
) -> Result<Restored, StoreError> {
    restore_checked(path, copy, Some(expected_head), || {})
}

/// The restore, running `after_switch` once the store has left WAL mode
/// (tests use it to act at that moment).
fn restore_checked(
    path: &Path,
    copy: &Path,
    expected_head: Option<&str>,
    after_switch: impl FnOnce(),
) -> Result<Restored, StoreError> {
    let refuse = |why: String| StoreError::Restore(why);
    let of = std::fs::canonicalize(path).map_err(|e| refuse(format!("{}: {e}", path.display())))?;
    let source = open_durable(copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| refuse(format!("{} can't be read: {e}", copy.display())))?;
    let ok: String = source
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| refuse(format!("{} can't be read: {e}", copy.display())))?;
    if ok != "ok" {
        return Err(refuse(format!("{} is damaged: {ok}", copy.display())));
    }
    let theirs = meta(&source, "store_id")?;
    match meta(&source, "backup_of")? {
        Some(b) if copy_of(path, copy, Some(&b), theirs.as_deref()) => {}
        Some(b) => {
            return Err(refuse(format!(
                "{} is a copy of {b}, not of this repository's store {}",
                copy.display(),
                of.display()
            )));
        }
        None => {
            return Err(refuse(format!(
                "{} isn't a copy mdkg took of a store",
                copy.display()
            )));
        }
    }
    let schema = meta(&source, "schema")?.unwrap_or_default();
    let mail = meta(&source, "msg_schema")?;
    if schema != SCHEMA_VERSION && !UPGRADES_FROM.contains(&schema.as_str()) {
        return Err(refuse(format!(
            "{} has schema {schema:?}, which this build doesn't read{}",
            copy.display(),
            match schema_error("schema", &schema, SCHEMA_VERSION) {
                StoreError::Schema(_) => " (a newer build wrote it)",
                _ => "",
            }
        )));
    }
    if let Some(m) = mail.as_deref()
        && !crate::mail::UPGRADES_FROM.contains(&m)
        && m != crate::mail::MAIL_SCHEMA_VERSION
    {
        return Err(refuse(format!(
            "{} has mail schema {m:?}, which this build doesn't read",
            copy.display()
        )));
    }

    let mut store = open_durable(path, OpenFlags::default())?;
    store.busy_timeout(Duration::ZERO)?;
    // Why the head no longer matches, if it doesn't.
    let head_moved = |conn: &Connection, expected: &str| -> Result<Option<String>, StoreError> {
        let has_log = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'log'")?
            .exists([])?;
        let head = if has_log {
            crate::log::head(conn)?
        } else {
            None
        };
        if head.as_ref().map(|(_, h)| h.as_str()) == Some(expected) {
            return Ok(None);
        }
        Ok(Some(format!(
            "the store's head moved: its chain ends at {}, not {expected}, so restoring would \
             drop what landed since",
            head.map_or_else(|| "nothing".to_string(), |(s, h)| format!("seq {s} ({h})"))
        )))
    };
    // A moved head is refused before anything changes, under a write lock
    // (in whatever journal mode the store is in).
    let original: String = store.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    let original = match original.to_ascii_lowercase().as_str() {
        m @ ("wal" | "delete" | "truncate" | "persist") => m.to_string(),
        _ => "wal".to_string(),
    };
    if let Some(expected) = expected_head {
        store.execute_batch("BEGIN IMMEDIATE")?;
        let checked = head_moved(&store, expected);
        store.execute_batch("ROLLBACK")?;
        if let Some(why) = checked? {
            return Err(refuse(why));
        }
    }
    // Leaving WAL mode needs every other connection closed (an idle one
    // still holds the WAL index), and folds any WAL left behind into the
    // file: the test that nothing else has the store open.
    let mode: Result<String, _> = store.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0));
    match mode.as_deref() {
        Ok("delete") => {}
        Ok(other) => {
            return Err(refuse(format!(
                "another process has {} open (an mdkg command, the MCP server or the \
                 launcher): stop it first (it stayed in {other} mode)",
                path.display()
            )));
        }
        Err(e) => {
            return Err(refuse(format!(
                "another process may have {} open (an mdkg command, the MCP server or \
                 the launcher): stop it first ({e})",
                path.display()
            )));
        }
    }
    after_switch();
    // From here until the copy lands, any refusal or failure leaves the
    // store as it was but in rollback mode: each one puts the store's own
    // journal mode back, and says so if it can't (the store is valid
    // either way).
    let prepared = (|| -> Result<(String, PathBuf), StoreError> {
        // Held until this connection closes: nothing opens it in between.
        store
            .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
            .map_err(|e| refuse(format!("couldn't lock {}: {e}", path.display())))?;
        // Checked again under the exclusive lock: a write may have landed
        // since.
        if let Some(expected) = expected_head
            && let Some(moved) = head_moved(&store, expected)?
        {
            return Err(refuse(moved));
        }
        let replaced = label(
            &meta(&store, "schema")?.unwrap_or_default(),
            meta(&store, "msg_schema")?.as_deref(),
        );
        let incoming = restore_image(&source, restore_generation(&store)?)?;
        let previous = back_up(&store, path, &replaced, "restore")
            .map_err(|e| refuse(format!("the store couldn't be copied first: {e}")))?;
        if let Err(e) = copy_all(&incoming, &mut store) {
            // One transaction, but a failure while SQLite finishes it (a
            // failed sync after the journal is cleared) can leave the copy
            // landed: so the store as it was stays, and the error says so.
            return Err(refuse(format!(
                "the copy failed ({e}), and may have landed in part or whole; the store as it \
                 was is at {}, which `store restore` puts back",
                previous.display()
            )));
        }
        Ok((replaced, previous))
    })();
    let (replaced, previous) = match prepared {
        Ok(done) => done,
        Err(e) => {
            // Through a fresh connection: in exclusive locking mode SQLite
            // reports the new mode at once but writes it only with the
            // next transaction, which never comes.
            drop(store);
            // And read back through another one: a switch SQLite can't
            // write (no room for its journal) can still report success.
            let back: Result<String, _> = open_durable(path, OpenFlags::default())
                .and_then(|c| {
                    c.busy_timeout(Duration::from_secs(5))?;
                    c.query_row(&format!("PRAGMA journal_mode = {original}"), [], |r| {
                        r.get::<_, String>(0)
                    })
                })
                .and_then(|_| {
                    open_durable(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?.query_row(
                        "PRAGMA journal_mode",
                        [],
                        |r| r.get(0),
                    )
                });
            return Err(match back {
                Ok(mode) if mode.eq_ignore_ascii_case(&original) => e,
                Ok(mode) => refuse(format!("{e}; and the store stayed in {mode} mode")),
                Err(why) => refuse(format!(
                    "{e}; and its journal mode couldn't be put back ({original}): {why}"
                )),
            });
        }
    };
    // Landed: what follows is tidying, which can't make it fail.
    let tidied = store
        .execute("DELETE FROM meta WHERE key = 'backup_of'", [])
        .is_ok();
    // Exclusive locking mode keeps the rollback journal (zeroed, so not
    // hot), with page images of the store as it was: removed while the
    // lock still keeps everyone else out.
    let journal = path.with_file_name(format!(
        "{}-journal",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    // Never a journal SQLite still needs: if that delete failed, it may be
    // hot, for the next opener to roll back.
    if tidied {
        std::fs::remove_file(journal).ok();
    }
    store.close().map_err(|(_, e)| e)?;
    Ok(Restored {
        from: copy.to_path_buf(),
        replaced,
        restored: label(&schema, mail.as_deref()),
        previous,
    })
}

/// A `key` version this build can't use: a newer one (`Schema`, which a
/// newer build reads), or a value no build wrote (`BadMeta`).
fn schema_error(key: &str, version: &str, ours: &str) -> StoreError {
    let newer = matches!(
        (version.parse::<u64>(), ours.parse::<u64>()),
        (Ok(v), Ok(ours)) if v > ours
    );
    match (newer, key) {
        (true, "schema") => StoreError::Schema(version.to_string()),
        (true, _) => StoreError::Schema(format!("msg {version}")),
        (false, _) => StoreError::BadMeta(format!("{key} {version:?}")),
    }
}

fn load(conn: &Connection, ttl: Ms) -> Result<(i64, Coordinator), StoreError> {
    check_schema(conn)?;
    let row: Option<(i64, String)> = conn
        .query_row("SELECT seq, coordinator FROM state WHERE id = 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let journal_seq = journal_seq(conn)?;
    let (state_seq, mut coordinator) = match row {
        Some((seq, json)) => (seq, serde_json::from_str::<Coordinator>(&json)?),
        None => (0, Coordinator::new(ttl)),
    };
    coordinator.upgrade_done_marks();
    if state_seq != journal_seq {
        return Err(StoreError::Inconsistent {
            state_seq,
            journal_seq,
        });
    }
    Ok((state_seq, coordinator))
}

impl Store {
    /// Opens the store at `path`, creating it if needed. `ttl` (the lease
    /// length) is fixed when the store is created; an existing store keeps
    /// its own. An older store is upgraded only as
    /// [`Upgrade::for_this_process`] allows.
    pub fn open(path: &Path, ttl: Ms) -> Result<Self, StoreError> {
        Self::open_with(path, ttl, Upgrade::for_this_process())
    }

    /// [`Store::open`] with an explicit upgrade policy.
    pub fn open_with(path: &Path, ttl: Ms, upgrade: Upgrade) -> Result<Self, StoreError> {
        Self::open_between(path, ttl, upgrade, || {})
    }

    /// [`Store::open_with`], running `between` after the first upgrade
    /// check and before the write lock: where another process can still
    /// act (tests use it to restore a store at that moment).
    fn open_between(
        path: &Path,
        ttl: Ms,
        upgrade: Upgrade,
        between: impl FnOnce(),
    ) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let refuse = upgrade == Upgrade::Refuse;
        // Refused before anything is written, and before this handle
        // touches the store: its close could checkpoint a WAL another
        // process left. Checked again under the write lock, since a
        // restore can swap an older store in until then.
        if refuse && let Some((from, to)) = probe_upgrade(path)? {
            return Err(StoreError::UpgradeRefused { from, to });
        }
        let before = crate::activity::identity_at(path)?;
        let mut conn = open_durable(path, OpenFlags::default())?;
        let file = crate::activity::identity_at(path)?;
        if before.is_some() && before != file {
            return Err(crate::activity::replaced());
        }
        between();
        // Every opener moves the store to WAL, which also keeps a restore
        // out while this handle is open (it can't leave WAL mode then). A
        // restore racing this open may be moved too, and then refused
        // below: the journal mode isn't the store's content, no table, row
        // or meta key changes, and every later opener does the same.
        to_wal(&conn)?;
        // A store from before ids gets one first, so the copy an upgrade
        // takes carries it. A refusing process never upgrades, so it mints
        // one under the write lock below instead, once allowed to go on.
        let known = |c: &Connection| -> Result<bool, StoreError> {
            Ok(meta(c, "schema")?
                .is_some_and(|v| v == SCHEMA_VERSION || UPGRADES_FROM.contains(&v.as_str())))
        };
        let mint = |c: &Connection| -> Result<(), StoreError> {
            if known(c)? && meta(c, "store_id")?.is_none() {
                c.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES ('store_id', ?1)",
                    [mint_store_id(path)],
                )?;
            }
            Ok(())
        };
        if !refuse {
            mint(&conn)?;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = upgrade_of(&tx)?;
        if refuse && let Some((from, to)) = pending {
            drop(tx);
            // Nothing was written, and closing doesn't checkpoint a WAL
            // another process left.
            conn.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .ok();
            return Err(StoreError::UpgradeRefused { from, to });
        }
        if refuse {
            mint(&tx)?;
        }
        // An upgrade first copies the store as it is, inside this same
        // write lock, and goes no further if the copy fails.
        let backup = match pending {
            Some((from, to)) => {
                // Read through a second connection: this one's write lock
                // keeps every other writer out, and it has written nothing.
                let reader = open_durable(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| StoreError::Backup(e.to_string()))?;
                Some(back_up(&reader, path, &from, &to)?)
            }
            None => None,
        };
        match Self::finish_open(tx, path, ttl) {
            Ok(ttl) => Ok(Self {
                conn,
                file,
                ttl,
                backup,
            }),
            Err(e) => {
                // Nothing changed: the copy would only pile up on retries.
                if let Some(copy) = &backup {
                    std::fs::remove_file(copy).ok();
                }
                Err(e)
            }
        }
    }

    /// Creates or upgrades the tables inside `tx`, and commits.
    fn finish_open(tx: Transaction<'_>, path: &Path, ttl: Ms) -> Result<Ms, StoreError> {
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch(crate::mail::MAIL_SCHEMA)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('msg_schema', ?1)",
            params![crate::mail::MAIL_SCHEMA_VERSION],
        )?;
        crate::mail::upgrade(&tx)?;
        let mail: String =
            tx.query_row("SELECT value FROM meta WHERE key = 'msg_schema'", [], |r| {
                r.get(0)
            })?;
        if mail != crate::mail::MAIL_SCHEMA_VERSION {
            return Err(schema_error(
                "msg_schema",
                &mail,
                crate::mail::MAIL_SCHEMA_VERSION,
            ));
        }
        let version: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()?;
        match version.as_deref() {
            None => {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema', ?1), ('ttl', ?2), \
                     ('store_id', ?3)",
                    params![SCHEMA_VERSION, ttl.to_string(), mint_store_id(path)],
                )?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(old) if UPGRADES_FROM.contains(&old) => {
                tx.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema'",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(other) => return Err(schema_error("schema", other, SCHEMA_VERSION)),
        }
        // A new or upgraded store opens its chain, after the journal it
        // already has.
        if version.as_deref() != Some(SCHEMA_VERSION) {
            let store_id = meta(&tx, "store_id")?
                .ok_or_else(|| StoreError::BadMeta("store_id (missing)".into()))?;
            crate::log::genesis(&tx, &store_id, journal_seq(&tx)?)?;
        }
        let ttl: String =
            tx.query_row("SELECT value FROM meta WHERE key = 'ttl'", [], |r| r.get(0))?;
        let ttl = ttl
            .parse()
            .map_err(|_| StoreError::BadMeta(format!("ttl {ttl}")))?;
        tx.commit()?;
        Ok(ttl)
    }

    /// The copy this open took before upgrading the store, if it did.
    pub fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }

    /// Opens the store a repository's worktrees share.
    /// As [`Store::open`], so upgrades follow this process's policy.
    pub fn open_for_repo(repo: &Path, ttl: Ms) -> Result<Self, StoreError> {
        Self::open(&git::store_path(repo)?, ttl)
    }

    pub fn ttl(&self) -> Ms {
        self.ttl
    }

    fn save(
        tx: &Transaction<'_>,
        seq: i64,
        command: &Command,
        outcome: &Outcome,
        coordinator: &mut Coordinator,
    ) -> Result<(), StoreError> {
        tx.execute(
            "INSERT INTO journal (seq, command, outcome) VALUES (?1, ?2, ?3)",
            params![
                seq,
                serde_json::to_string(command)?,
                serde_json::to_string(outcome)?
            ],
        )?;
        crate::log::append_command(tx, seq, command, outcome, coordinator)?;
        if let (
            Command::SetWork {
                leaf,
                status: Some(status),
                changeset: Some(id),
                ..
            },
            Outcome::Done,
        ) = (command, outcome)
            && status == "done"
        {
            let changeset = coordinator.changesets().find(|c| &c.id == id);
            let node = changeset
                .and_then(|c| coordinator.lease(&c.lease))
                .and_then(|l| l.node.as_deref())
                .unwrap_or(leaf.as_str());
            let revision = changeset
                .and_then(|c| c.changes.get(leaf))
                .and_then(Option::as_ref)
                .cloned()
                .unwrap_or_else(|| {
                    changeset.and_then(|c| c.source.as_ref()).map_or_else(
                        || format!("changeset:{id}"),
                        |s| format!("commit:{}", s.commit),
                    )
                });
            crate::activity::record_completion(tx, seq, leaf.as_str(), node, &revision)?;
        }
        for event in coordinator.drain_events() {
            tx.execute(
                "INSERT INTO events (seq, event) VALUES (?1, ?2)",
                params![seq, serde_json::to_string(&event)?],
            )?;
        }
        tx.execute(
            "INSERT INTO state (id, seq, coordinator) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET seq = excluded.seq, coordinator = excluded.coordinator",
            params![seq, serde_json::to_string(coordinator)?],
        )?;
        Ok(())
    }

    /// Applies one command and records it.
    pub fn execute(&mut self, command: &Command) -> Result<Outcome, StoreError> {
        self.execute_with(|_| Ok(command.clone()))
    }

    /// Builds a command from the locked, current state, then applies and
    /// records it in the same transaction. Nothing else can change the
    /// coordinator between `build` reading it and the command landing.
    pub fn execute_with(
        &mut self,
        build: impl FnOnce(&Coordinator) -> Result<Command, StoreError>,
    ) -> Result<Outcome, StoreError> {
        let ttl = self.ttl;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (seq, mut coordinator) = load(&tx, ttl)?;
        let command = build(&coordinator)?;
        // Every command observes main first; journaling the ones that
        // changed nothing would grow the journal by the graph's size per
        // command. Skipping them keeps replay exact, since state is equal.
        let before = matches!(command, Command::ObserveMain { .. }).then(|| coordinator.clone());
        let outcome = coordinator.apply(&command);
        if before.as_ref() == Some(&coordinator) {
            return Ok(outcome);
        }
        Self::save(&tx, seq + 1, &command, &outcome, &mut coordinator)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Applies `first` and then, when `then` builds one from the state and
    /// `first`'s outcome, a second command, in one transaction: both are
    /// recorded, or neither. A second command that is refused rolls the
    /// first back too ([`Chained::recorded`] false), so no other command can
    /// land between them and none of them is half done.
    pub fn execute_then(
        &mut self,
        first: &Command,
        then: impl FnOnce(&Coordinator, &Outcome) -> Option<Command>,
    ) -> Result<Chained, StoreError> {
        let mut then = Some(then);
        let mut outcomes = self.execute_chain(first, |c, o| then.take().and_then(|f| f(c, o)))?;
        let recorded = outcomes.recorded;
        let first = outcomes.outcomes.remove(0);
        Ok(Chained {
            first,
            second: outcomes.outcomes.pop(),
            recorded,
        })
    }

    /// Records one local done revision and clears its temporary status in
    /// one transaction. Repeating the revision retains its first fact position;
    /// this does not repair a done file whose command crashed before marking.
    pub fn complete_local(
        &mut self,
        leaf: &Scope,
        node: &str,
        actor: &str,
        revision: &str,
        now: Ms,
    ) -> Result<Chained, StoreError> {
        let first = Command::SetWork {
            leaf: leaf.clone(),
            status: Some("done".into()),
            changeset: None,
            actor: actor.into(),
            now,
        };
        let mut clear = Some(Command::SetWork {
            leaf: leaf.clone(),
            status: None,
            changeset: None,
            actor: actor.into(),
            now,
        });
        let mut batch = self.execute_chain_completed(
            &first,
            |_, o| {
                if o.error().is_none() {
                    clear.take()
                } else {
                    None
                }
            },
            Some((node, revision)),
        )?;
        Ok(Chained {
            first: batch.outcomes.remove(0),
            second: batch.outcomes.pop(),
            recorded: batch.recorded,
        })
    }

    /// Applies `first`, then each command `next` builds from the state and
    /// the last outcome, until it builds none, all in one transaction. Only
    /// `first` may be refused and still recorded (as `execute` records a
    /// refusal); any later refusal rolls every one of them back.
    pub fn execute_chain(
        &mut self,
        first: &Command,
        next: impl FnMut(&Coordinator, &Outcome) -> Option<Command>,
    ) -> Result<ChainOutcomes, StoreError> {
        self.execute_chain_completed(first, next, None)
    }

    fn execute_chain_completed(
        &mut self,
        first: &Command,
        mut next: impl FnMut(&Coordinator, &Outcome) -> Option<Command>,
        completion: Option<(&str, &str)>,
    ) -> Result<ChainOutcomes, StoreError> {
        let ttl = self.ttl;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut seq, mut coordinator) = load(&tx, ttl)?;
        let mut outcome = coordinator.apply(first);
        let first_seq = seq + 1;
        seq += 1;
        Self::save(&tx, seq, first, &outcome, &mut coordinator)?;
        let mut outcomes = vec![outcome.clone()];
        while let Some(command) = next(&coordinator, &outcome) {
            outcome = coordinator.apply(&command);
            outcomes.push(outcome.clone());
            if outcome.error().is_some() {
                // Dropping the transaction rolls every command back.
                return Ok(ChainOutcomes {
                    outcomes,
                    recorded: false,
                });
            }
            seq += 1;
            Self::save(&tx, seq, &command, &outcome, &mut coordinator)?;
        }
        if outcomes.iter().all(|o| o.error().is_none())
            && let Some((node, revision)) = completion
            && let Command::SetWork { leaf, .. } = first
        {
            crate::activity::record_completion(&tx, first_seq, leaf.as_str(), node, revision)?;
        }
        tx.commit()?;
        Ok(ChainOutcomes {
            outcomes,
            recorded: true,
        })
    }

    /// Reads `refname` under `prefix` and applies it as an observation of
    /// main, inside the write transaction.
    pub fn observe_main(
        &mut self,
        repo: &Path,
        refname: &str,
        prefix: &str,
    ) -> Result<Outcome, StoreError> {
        // The digest cache sits beside the store file (none for a store
        // without one, such as an in-memory store).
        let cache = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .and_then(|p| Path::new(p).parent())
            .map(|dir| dir.join(crate::digest_cache::FILE));
        self.execute_with(|coordinator| {
            // Open changesets whose commit main has merged: settled by
            // history. A commit git can't find (a dropped branch) isn't.
            let open: std::collections::BTreeSet<String> = coordinator
                .settlement_queue()
                .into_iter()
                .filter_map(|c| c.source.as_ref())
                .map(|s| s.commit.clone())
                .collect();
            let merged = open
                .into_iter()
                .filter(|commit| git::is_ancestor_of(repo, commit, refname).unwrap_or(false))
                .collect();
            Ok(Command::ObserveMain {
                observation: Observation {
                    read_generation: coordinator.generation(),
                    leaves: git::leaves_cached(repo, refname, prefix, cache.as_deref())?,
                    merged,
                    prefix: Some(Scope::new(prefix)),
                },
            })
        })
    }

    /// The current state, for queries. The state and the journal are read
    /// in one transaction, so a writer in another worktree can't land
    /// between them.
    pub fn coordinator(&self) -> Result<Coordinator, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let (_, coordinator) = load(&tx, self.ttl)?;
        tx.finish()?;
        Ok(coordinator)
    }

    /// Receipt baseline before a turn, bound to this store and log history.
    /// Opens no store and changes no coordinator state or expiry records.
    pub fn activity_cursor(&self) -> Result<crate::ActivityCursor, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let (seq, _) = load(&tx, self.ttl)?;
        let cursor = crate::activity::cursor(&tx, self.file, seq)?;
        tx.finish()?;
        Ok(cursor)
    }

    /// Native journal activity since `after`, and active leases at `now`,
    /// from one read transaction. Refuses a replaced history or an interval
    /// that exceeds its read limits, rather than reporting incomplete facts.
    pub fn activity_since(
        &self,
        after: &crate::ActivityCursor,
        actor: &str,
        now: Ms,
    ) -> Result<crate::ActivitySnapshot, StoreError> {
        self.activity_since_with(after, actor, now, || {})
    }

    fn activity_since_with(
        &self,
        after: &crate::ActivityCursor,
        actor: &str,
        now: Ms,
        between: impl FnOnce(),
    ) -> Result<crate::ActivitySnapshot, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let (seq, state) = load(&tx, self.ttl)?;
        between();
        let through = crate::activity::cursor(&tx, self.file, seq)?;
        let activity = crate::activity::read(&tx, &state, through, after, actor, now)?;
        tx.finish()?;
        Ok(activity)
    }

    /// Every journal entry in order.
    pub fn journal(&self) -> Result<Vec<(i64, Command, Outcome)>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        check_schema(&tx)?;
        let journal = journal_on(&tx)?;
        tx.finish()?;
        Ok(journal)
    }

    /// Replays the journal into a fresh coordinator, checking that every
    /// command gives its recorded outcome.
    pub fn replay(&self) -> Result<Coordinator, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        check_schema(&tx)?;
        let replayed = replay_on(&tx, self.ttl)?;
        tx.finish()?;
        Ok(replayed)
    }

    /// Checks that the stored state equals a replay of the journal, both
    /// read in one transaction.
    pub fn verify(&self) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let (_, stored) = load(&tx, self.ttl)?;
        let replayed = replay_on(&tx, self.ttl)?;
        tx.finish()?;
        if stored != replayed {
            return Err(StoreError::SnapshotDiffers);
        }
        Ok(())
    }

    /// Recreates the state snapshot from the journal (for a lost or damaged
    /// snapshot). The journal is read inside the write transaction, so no
    /// command can land between the replay and the save.
    pub fn rebuild(&mut self) -> Result<Coordinator, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let replayed = replay_on(&tx, self.ttl)?;
        let seq = journal_seq(&tx)?;
        tx.execute(
            "INSERT INTO state (id, seq, coordinator) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET seq = excluded.seq, coordinator = excluded.coordinator",
            params![seq, serde_json::to_string(&replayed)?],
        )?;
        tx.commit()?;
        Ok(replayed)
    }
}

/// What [`Store::verify_log`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogReport {
    pub store: String,
    /// Records in the chain.
    pub count: u64,
    /// The last record's hash.
    pub head: String,
    /// The journal entry the chain starts after.
    pub journal_from: i64,
}

impl Store {
    /// The store's own id (`store_id`), which its chain's genesis names.
    pub fn store_id(&self) -> Result<String, StoreError> {
        meta(&self.conn, "store_id")?
            .ok_or_else(|| StoreError::BadMeta("store_id (missing)".into()))
    }

    /// The chain's last record, as `(seq, hash)`.
    pub fn log_head(&self) -> Result<(u64, String), StoreError> {
        check_schema(&self.conn)?;
        crate::log::head(&self.conn)?.ok_or_else(|| StoreError::Log("the chain is empty".into()))
    }

    /// Every record's canonical text, in seq order, read in one
    /// transaction.
    pub fn log_texts(&self) -> Result<Vec<String>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        check_schema(&tx)?;
        let texts = crate::log::texts(&tx)?
            .into_iter()
            .map(|(_, _, t)| t)
            .collect();
        tx.finish()?;
        Ok(texts)
    }

    /// Compacts the chain at a milestone: appends a `log.checkpoint` that
    /// summarizes, in at most 150 words, every record since the last one
    /// and cites their first and last hashes. None when nothing came
    /// since the last checkpoint. Raw records are kept for verification.
    pub fn checkpoint(&mut self) -> Result<Option<crate::log::Checkpoint>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let made = crate::log::checkpoint(&tx)?;
        tx.commit()?;
        Ok(made)
    }

    /// The `log.import` marker an import wrote, as `(seq, hash)`, if
    /// `import_id` was imported.
    pub fn import_marker(&self, import_id: &str) -> Result<Option<(u64, String)>, StoreError> {
        check_schema(&self.conn)?;
        let mut stmt = self.conn.prepare(
            "SELECT record FROM log WHERE record LIKE '%\"kind\":\"log.import\"%' ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let (record, hash) = mdkg_log::record::Record::parse(&row?)
                .map_err(|e| StoreError::Log(e.to_string()))?;
            if record.kind == "log.import"
                && record
                    .body
                    .get("import_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(import_id)
            {
                return Ok(Some((record.seq, hash)));
            }
        }
        Ok(None)
    }

    /// Appends records from before the log (an import by `migrate`) in one
    /// transaction: a `log.import` marker and then the records, each with
    /// its own time and actor. Importing the same `import_id` again
    /// appends nothing, so a crashed import can simply run again.
    pub fn import_records(
        &mut self,
        import_id: &str,
        records: &[crate::log::Imported],
    ) -> Result<crate::log::ImportOutcome, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let outcome = crate::log::import(&tx, import_id, records)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Checks the chain: every record canonical, its stored seq and hash
    /// its own, linked from a genesis naming this store, ending where the
    /// store last noted its head, and exactly one record per journal entry
    /// after the genesis, in order, carrying that entry's command and
    /// outcome. Read in one transaction.
    pub fn verify_log(&self) -> Result<LogReport, StoreError> {
        self.verified_log().map(|(report, _)| report)
    }

    /// [`Store::verify_log`], also returning the canonical texts it
    /// verified, so a bundle and its receipt come from one read.
    pub fn verified_log(&self) -> Result<(LogReport, Vec<String>), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        check_schema(&tx)?;
        let rows = crate::log::texts(&tx)?;
        let journal = journal_rows(&tx)?;
        let store = meta(&tx, "store_id")?.unwrap_or_default();
        let noted = crate::log::noted_head(&tx)?;
        tx.finish()?;
        let bad = |why: String| StoreError::Log(why);
        let mut chain = Vec::with_capacity(rows.len());
        let mut texts = Vec::with_capacity(rows.len());
        for (seq, hash, text) in rows {
            let (record, computed) = mdkg_log::record::Record::parse(&text)
                .map_err(|e| bad(format!("seq {seq}: {e}")))?;
            if record.seq != seq || computed != hash {
                return Err(bad(format!(
                    "seq {seq}: the row's seq or hash doesn't match its record"
                )));
            }
            chain.push((record, computed));
            texts.push(text);
        }
        mdkg_log::bundle::check_chain(&store, &chain).map_err(|e| bad(e.to_string()))?;
        let (last, head) = chain
            .last()
            .map(|(r, h)| (r.seq, h.clone()))
            .unwrap_or_default();
        if noted.as_ref() != Some(&(last, head.clone())) {
            return Err(bad(format!(
                "the chain ends at seq {last}, but the store last wrote {}: records were \
                 dropped from its end",
                noted.map_or_else(|| "no head".to_string(), |(s, h)| format!("seq {s} ({h})"))
            )));
        }
        let journal_from = chain[0]
            .0
            .body
            .get("journal_seq")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| bad("the genesis has no journal_seq".into()))?;
        // Entries after the genesis, each matched by exactly one record of
        // a coordinator kind. Imported records (the `count` after a
        // `log.import`) and other kinds' bodies stay opaque.
        let mut entries = journal.iter().filter(|(seq, ..)| *seq > journal_from);
        let mut imported = 0u64;
        for (record, _) in &chain {
            if imported > 0 {
                imported -= 1;
                continue;
            }
            if record.kind == "log.import" {
                imported = record
                    .body
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                continue;
            }
            if !crate::log::COORDINATOR_KINDS.contains(&record.kind.as_str()) {
                continue;
            }
            let Some(j) = record.body.get("journal") else {
                return Err(bad(format!(
                    "seq {}: a {} record without its journal entry",
                    record.seq, record.kind
                )));
            };
            let Some((seq, command, outcome)) = entries.next() else {
                return Err(bad(format!(
                    "seq {}: records journal entry {j}, which the journal doesn't have",
                    record.seq
                )));
            };
            if j.as_i64() != Some(*seq)
                || record.body.get("command") != Some(command)
                || record.body.get("outcome") != Some(outcome)
            {
                return Err(bad(format!(
                    "seq {}: records journal entry {j}, but the journal's entry {seq} differs",
                    record.seq
                )));
            }
        }
        if let Some((seq, ..)) = entries.next() {
            return Err(bad(format!("journal entry {seq} has no record")));
        }
        Ok((
            LogReport {
                store,
                count: last,
                head,
                journal_from,
            },
            texts,
        ))
    }
}

/// Every journal entry as JSON, in order, for comparing with records.
fn journal_rows(
    conn: &Connection,
) -> Result<Vec<(i64, serde_json::Value, serde_json::Value)>, StoreError> {
    let mut stmt = conn.prepare("SELECT seq, command, outcome FROM journal ORDER BY seq")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, command, outcome) = row?;
        out.push((
            seq,
            serde_json::from_str(&command)?,
            serde_json::from_str(&outcome)?,
        ));
    }
    Ok(out)
}

fn journal_on(conn: &Connection) -> Result<Vec<(i64, Command, Outcome)>, StoreError> {
    {
        let mut stmt = conn.prepare("SELECT seq, command, outcome FROM journal ORDER BY seq")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (seq, command, outcome) = row?;
            entries.push((
                seq,
                serde_json::from_str(&command)?,
                serde_json::from_str(&outcome)?,
            ));
        }
        Ok(entries)
    }
}

fn replay_on(conn: &Connection, ttl: Ms) -> Result<Coordinator, StoreError> {
    let mut coordinator = Coordinator::new(ttl);
    for (seq, command, recorded) in journal_on(conn)? {
        if coordinator.apply(&command) != recorded {
            return Err(StoreError::ReplayDiverged(seq));
        }
        coordinator.drain_events();
    }
    Ok(coordinator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_snapshot_keeps_one_view_while_a_wal_writer_commits() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-activity-wal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        let store = Store::open_with(&path, 1_000, Upgrade::Allow).unwrap();
        let mut other = Store::open_with(&path, 1_000, Upgrade::Refuse).unwrap();
        let before = store.activity_cursor().unwrap();
        let facts = store
            .activity_since_with(&before, "alice", 1, || {
                assert!(matches!(
                    other
                        .execute(&Command::Claim {
                            owner: "alice".into(),
                            scope: Scope::new("a"),
                            mode: mdkg_coord::Mode::Exclusive,
                            now: 0,
                            request: None,
                            node: None,
                        })
                        .unwrap(),
                    Outcome::Lease { .. }
                ));
            })
            .unwrap();
        assert_eq!(facts.through, before);
        assert!(facts.claims.is_empty() && facts.leases.is_empty());
        let next = store.activity_since(&before, "alice", 1).unwrap();
        assert_eq!(next.claims.len(), 1);
        assert_eq!(next.leases.len(), 1);
        drop(other);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Review of fe4746c: a build folder removed while its markers are
    /// read (`cargo clean` racing the check) takes the binary with it, so
    /// the binary reads as not installed.
    #[test]
    fn a_vanishing_build_folder_is_not_an_install() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-installed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let target = dir.join("target");
        std::fs::create_dir_all(target.join("out")).unwrap();
        let exe = target.join("out/mdkg");
        std::fs::write(&exe, b"bin").unwrap();
        // Markers gone before the check, the binary gone by the recheck.
        let mut calls = 0;
        let racing = installed_at(|| {
            calls += 1;
            if calls == 2 {
                std::fs::remove_dir_all(&target).ok();
            }
            Ok(exe.clone())
        });
        assert!(!racing);
        // Moved: a different path the second time.
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let a = dir.join("bin/a");
        std::fs::write(&a, b"bin").unwrap();
        let mut flip = [a.clone(), dir.join("bin/b")].into_iter();
        assert!(!installed_at(|| Ok(flip.next().unwrap_or_default())));
        // Still there, outside any build folder: installed.
        assert!(installed_at(|| Ok(a.clone())));
        assert!(!installed_at(|| Err(std::io::Error::other("no exe"))));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of 9a2e736 and 527ae8c: provenance, not folder names or
    /// the workspace. A binary in the profile folder this crate was built
    /// in counts whatever the profile is called and whether or not Cargo's
    /// markers survive; an install doesn't, wherever it is.
    #[test]
    fn provenance_finds_builds_and_spares_installs() {
        let out = Path::new("/w/target/aarch64-apple-darwin/dist/build/mdkg-coord-sqlite-0123/out");
        let profile = profile_folder(out);
        assert_eq!(
            profile.as_deref(),
            Some(Path::new("/w/target/aarch64-apple-darwin/dist"))
        );
        assert_eq!(profile_folder(Path::new("/x/out")), None);
        assert_eq!(profile_folder(Path::new("/a/b/c/out")), None, "not build/");
        let p = profile.as_deref();
        assert!(built_in(
            Path::new("/w/target/aarch64-apple-darwin/dist/mdkg"),
            p
        ));
        assert!(built_in(
            Path::new("/w/target/aarch64-apple-darwin/dist/deps/t-01"),
            p
        ));
        assert!(!built_in(Path::new("/opt/mdkg/release/mdkg"), p));
        assert!(!built_in(
            Path::new("/w/target/aarch64-apple-darwin/distx/mdkg"),
            p
        ));
        assert!(
            !built_in(Path::new("/w/.tools/bin/mdkg"), p),
            "cargo install --root"
        );
        assert!(!built_in(
            Path::new("/w/target/aarch64-apple-darwin/dist/x"),
            None
        ));
        // This test binary is found by provenance alone, wherever
        // CARGO_TARGET_DIR puts it.
        let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert!(built_in(
            &exe,
            profile_folder(Path::new(env!("OUT_DIR"))).as_deref()
        ));
        // An install inside the workspace, beside the source: no markers,
        // not the profile folder.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let tools = std::fs::canonicalize(workspace)
            .unwrap()
            .join(".tools/bin/mdkg");
        assert!(!in_build_folder(&tools), "{tools:?}");
    }

    /// Every table's rows, for comparing a store's content.
    fn content(path: &Path) -> Vec<String> {
        let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut out = Vec::new();
        for t in tables {
            let mut stmt = c.prepare(&format!("SELECT * FROM {t}")).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    let cells: Vec<String> = (0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect();
                    Ok(format!("{t}: {}", cells.join(" | ")))
                })
                .unwrap();
            for row in rows {
                out.push(row.unwrap());
            }
        }
        out
    }

    /// Review of 527ae8c: a restore that swaps an older store in after the
    /// first check is caught under the write lock, and the refusal leaves
    /// the restored store's content as the restore left it.
    #[test]
    fn a_restore_racing_the_open_is_refused_and_left_alone() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-race-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
        // In WAL mode the open already keeps a restore out by now.
        let opened = Store::open_between(&path, 100, Upgrade::Refuse, || {
            let c = Connection::open(&path).unwrap();
            c.busy_timeout(std::time::Duration::ZERO).unwrap();
            assert!(
                c.query_row("PRAGMA journal_mode = DELETE", [], |r| r
                    .get::<_, String>(0))
                    .is_err()
            );
        });
        drop(opened.unwrap());
        // In rollback mode (an undone restore leaves it so) one can land.
        {
            let c = Connection::open(&path).unwrap();
            let _: String = c
                .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
                .unwrap();
        }
        let mut restored = None;
        let opened = Store::open_between(&path, 100, Upgrade::Refuse, || {
            let c = Connection::open(&path).unwrap();
            let mode: String = c
                .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "delete");
            c.execute("UPDATE meta SET value = '1' WHERE key = 'schema'", [])
                .unwrap();
            c.execute("DELETE FROM meta WHERE key = 'store_id'", [])
                .unwrap();
            drop(c);
            restored = Some(content(&path));
        });
        assert!(
            matches!(opened, Err(StoreError::UpgradeRefused { .. })),
            "{opened:?}"
        );
        assert_eq!(Some(content(&path)), restored, "untouched");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of 60cd287: while a refusing process has the store open, a
    /// restore can't replace it, even when the store was in rollback mode
    /// (an undone restore leaves it so).
    #[test]
    fn an_open_store_keeps_a_restore_out() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-held-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let copy = back_up(&reader, &path, "now", "test").unwrap();
        drop(reader);
        {
            let c = Connection::open(&path).unwrap();
            let _: String = c
                .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
                .unwrap();
        }
        let held = Store::open_with(&path, 100, Upgrade::Refuse).unwrap();
        assert!(matches!(restore(&path, &copy), Err(StoreError::Restore(_))));
        drop(held);
        restore(&path, &copy).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of 60cd287: one crash in rollback mode leaves a hot journal;
    /// a refusing process rolls it back (the store's last committed state)
    /// and opens a store at this schema.
    #[test]
    fn a_hot_journal_doesnt_lock_out_a_refusing_process() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-hot-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        let mut store = Store::open_with(&path, 100, Upgrade::Allow).unwrap();
        store
            .execute(&Command::Register {
                leaf: Scope::new("w/a"),
                content: "w/a@0".into(),
            })
            .unwrap();
        drop(store);
        let committed = content(&path);
        // A writer in rollback mode spills a big change to the file and
        // "crashes": the files are copied while its transaction is open.
        let crashed = dir.join("crashed.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            let _: String = c
                .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
                .unwrap();
            c.execute_batch("PRAGMA cache_size = 1; BEGIN IMMEDIATE;")
                .unwrap();
            let big = "x".repeat(4000);
            for i in 0..200 {
                c.execute(
                    "INSERT INTO events (seq, event) VALUES (?1, ?2)",
                    params![i, big],
                )
                .unwrap();
            }
            std::fs::copy(&path, &crashed).unwrap();
            std::fs::copy(
                path.with_file_name("coord.sqlite-journal"),
                dir.join("crashed.sqlite-journal"),
            )
            .unwrap();
            c.execute_batch("ROLLBACK;").unwrap();
        }
        assert!(dir.join("crashed.sqlite-journal").exists());
        let store = Store::open_with(&crashed, 100, Upgrade::Refuse).expect("opens");
        drop(store);
        assert_eq!(
            content(&crashed),
            committed,
            "rolled back to the last commit"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Orchestrator, 2026-10-25 (lane 1, room #63): every store connection
    /// commits durably, whatever the upgrade policy.
    #[test]
    fn store_connections_commit_durably() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-durable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        for upgrade in [Upgrade::Allow, Upgrade::Refuse] {
            let store = Store::open_with(&path, 100, upgrade).unwrap();
            let sync: i64 = store
                .conn
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            let full: i64 = store
                .conn
                .query_row("PRAGMA fullfsync", [], |r| r.get(0))
                .unwrap();
            assert_eq!((sync, full), (2, 1), "{upgrade:?}: FULL and fullfsync");
        }
        let read = open_durable(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let sync: i64 = read
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of ba8f2a6: every refusal after the journal-mode switch puts
    /// the store's mode back, or says it couldn't, and changes nothing.
    #[test]
    fn restore_refusals_put_the_journal_mode_back_or_say_so() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-restore-mode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let copy = back_up(&reader, &path, "now", "test").unwrap();
        drop(reader);
        let mode_of = |p: &Path| -> String {
            Connection::open(p)
                .unwrap()
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap()
        };
        let before = content(&path);
        // A reader takes a shared lock just after the switch: the lock is
        // refused, and the mode goes back (or the refusal says it didn't).
        let mut held = None;
        let refused = restore_checked(&path, &copy, None, || {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("BEGIN; SELECT * FROM meta;").unwrap();
            held = Some(c);
        });
        drop(held);
        match &refused {
            Err(StoreError::Restore(m)) => assert!(
                m.contains("lock") && (mode_of(&path) == "wal" || m.contains("journal mode")),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(content(&path), before, "unchanged");
        // The safety copy can't be written (the store's folder moved away
        // just after the switch): refused, and since the mode can't be put
        // back there either, the refusal says so.
        let moved = dir.with_extension("moved");
        let refused = restore_checked(&path, &copy, None, || {
            std::fs::rename(&dir, &moved).unwrap();
        });
        std::fs::rename(&moved, &dir).unwrap();
        match &refused {
            Err(StoreError::Restore(m)) => assert!(
                m.contains("couldn't be copied first") && m.contains("journal mode"),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(content(&path), before, "unchanged");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review of 0b5cee0: every store connection waits the full 30 s for a
    /// busy store (rusqlite's own default is 5 s), whatever it's opened as.
    #[test]
    fn every_store_connection_waits_thirty_seconds() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-busy-wait-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        let store = Store::open_with(&path, 100, Upgrade::Allow).unwrap();
        let wait = |c: &Connection| -> i64 {
            c.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(wait(&store.conn), 30_000);
        drop(store);
        for flags in [OpenFlags::default(), OpenFlags::SQLITE_OPEN_READ_ONLY] {
            assert_eq!(
                wait(&open_durable(&path, flags).unwrap()),
                30_000,
                "{flags:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
