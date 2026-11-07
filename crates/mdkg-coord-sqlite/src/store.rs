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

/// "4" since the flow tables (edd-01a1878e: message links to nodes,
/// supersession and expiry, the ledger's entries): a binary that predates
/// them would send, ack and finish work around the flow rules (a `done`
/// skipping its node's gate), so it must refuse the store. Opening a "3"
/// store upgrades it in place, copying it first and adding the tables;
/// nothing it held changes, and its chain goes on.
///
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
const SCHEMA_VERSION: &str = "4";

/// Versions this binary upgrades in place on open.
const UPGRADES_FROM: &[&str] = &["1", "2", "3"];

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
    /// A reader found an upgradeable store and leaves it for a writer.
    ReadUpgradeRequired {
        from: String,
        to: String,
    },
    /// A read-only handle cannot roll back a hot SQLite journal.
    ReadRecoveryRequired(rusqlite::Error),
    /// A reader found no store, and creates none: only a writer does.
    ReadAbsent,
    /// The record chain can't be written or doesn't verify.
    Log(String),
    /// This process forbade store writes ([`forbid_store_writes`],
    /// `--read-only`), and the command needed one: nothing was written.
    WritesForbidden,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => {
                write!(f, "coordinator store: {e}")?;
                if matches!(e, rusqlite::Error::SqliteFailure(code, _)
                    if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
                {
                    write!(
                        f,
                        " (bounded lock wait; SQLite does not report the lock holder)"
                    )?;
                }
                Ok(())
            }
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
            StoreError::ReadUpgradeRequired { from, to } => write!(
                f,
                "the coordinator store needs an upgrade from {from} to {to}; this read left it unchanged"
            ),
            // Store errors name no command (#17360): one copied from here
            // would act on the current folder's repository. The CLI's hint
            // says what to run and with which --root.
            StoreError::ReadRecoveryRequired(error) => write!(
                f,
                "coordinator store needs SQLite journal recovery before this read: {error}; this read left it unchanged"
            ),
            StoreError::ReadAbsent => {
                f.write_str("coordinator store is absent, and this read creates none")
            }
            StoreError::Log(why) => write!(f, "coordinator log: {why}"),
            StoreError::WritesForbidden => f.write_str(WRITES_FORBIDDEN),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        if is_writes_forbidden(&e) {
            return StoreError::WritesForbidden;
        }
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
    /// Refuse upgrades with [`StoreError::UpgradeRefused`]. Native SQLite
    /// rollback recovery and WAL-index cache maintenance may still occur.
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

/// What a refused write says, and the marker [`open_durable`] puts in the
/// SQLite error it returns, so `?` turns it into
/// [`StoreError::WritesForbidden`].
const WRITES_FORBIDDEN: &str = "this command would write the coordinator store, and --read-only forbids that; nothing was written";

#[cfg(not(test))]
static WRITES_FORBIDDEN_HERE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static WRITES_FORBIDDEN_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Forbids every coordinator store write for the rest of this process
/// (`--read-only`): opening a store for writing, creating, upgrading,
/// recovering, restoring or copying one, and recording an observation are
/// refused with [`StoreError::WritesForbidden`] before anything is
/// written. Read-only opens ([`Store::open_read_only`],
/// [`Store::open_raw_read_only`]) are unaffected; they never checkpoint.
/// There is no way back: a reader can't turn writes on again.
pub fn forbid_store_writes() {
    #[cfg(not(test))]
    WRITES_FORBIDDEN_HERE.store(true, Ordering::SeqCst);
    #[cfg(test)]
    WRITES_FORBIDDEN_HERE.with(|f| f.set(true));
}

/// Whether [`forbid_store_writes`] was called in this process (in tests,
/// on this thread).
pub fn store_writes_forbidden() -> bool {
    #[cfg(not(test))]
    return WRITES_FORBIDDEN_HERE.load(Ordering::SeqCst);
    #[cfg(test)]
    return WRITES_FORBIDDEN_HERE.with(std::cell::Cell::get);
}

/// Refuses a store write once [`forbid_store_writes`] was called.
pub(crate) fn may_write() -> Result<(), StoreError> {
    if store_writes_forbidden() {
        Err(StoreError::WritesForbidden)
    } else {
        Ok(())
    }
}

fn writes_forbidden_error() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
        Some(WRITES_FORBIDDEN.to_string()),
    )
}

fn is_writes_forbidden(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(code, Some(message))
        if code.extended_code == rusqlite::ffi::SQLITE_READONLY && message == WRITES_FORBIDDEN)
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
    /// Both were recorded; false when the second was refused, or the
    /// check before commit refused ([`Chained::checked`]), which left
    /// nothing recorded.
    pub recorded: bool,
    /// False when the check run before the transaction commits (a
    /// [`BeforeCommit`], such as a node's settle) refused it.
    pub checked: bool,
}

/// What [`Store::execute_chain`] did: every outcome, in order.
#[derive(Debug)]
pub struct ChainOutcomes {
    pub outcomes: Vec<Outcome>,
    /// All were recorded; false when a later one was refused, or the
    /// check before commit refused, which left nothing recorded.
    pub recorded: bool,
    /// False when the check before commit refused.
    pub checked: bool,
}

/// A check run inside a write transaction once every command in it has
/// applied, before it commits, over the same connection: `done`'s settle
/// (A29 step 4), which evaluates the gate and appends `ledger.settle`
/// there. `Ok(false)` refuses: the transaction rolls back and nothing is
/// recorded; an error rolls it back too.
pub type BeforeCommit<'h> = &'h mut dyn FnMut(&Connection) -> Result<bool, StoreError>;

#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
    /// Device/inode checked around SQLite's open. No independent descriptor:
    /// closing one could release this process's SQLite POSIX locks.
    file: Option<(u64, u64)>,
    pub(crate) ttl: Ms,
    /// The copy this open took before upgrading the store, if it did.
    backup: Option<PathBuf>,
    /// Whether that copy settled for a plain fsync ([`PLAIN_FSYNC_ENV`]).
    backup_weak: bool,
    /// Whether this open created the store and settled for a plain fsync
    /// there ([`PLAIN_FSYNC_ENV`]).
    created_weak: bool,
    /// How long an acked `purge_on_ack` body is kept
    /// ([`Store::set_ack_grace_days`]); 0 purges it at the ack.
    pub(crate) ack_grace_ms: u64,
    /// Mail reads write nothing ([`Store::set_pure_reads`]): always for a
    /// store opened read-only.
    pub(crate) pure_reads: bool,
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

type TableColumns = std::collections::BTreeMap<String, (String, bool, i64)>;

fn table_columns(conn: &Connection, table: &str) -> Result<TableColumns, StoreError> {
    Ok(conn
        .prepare("SELECT name, upper(type), \"notnull\", pk FROM pragma_table_info(?1)")?
        .query_map([table], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Require the recorded version's structure, and validate later known objects
/// if present: the upgrade will reuse them instead of replacing their shape.
fn check_coordinator_structure(conn: &Connection, schema: &str) -> Result<(), StoreError> {
    let mail = meta(conn, "msg_schema")?;
    let mail = match mail.as_deref() {
        None => 0,
        Some(version)
            if version == crate::mail::MAIL_SCHEMA_VERSION
                || crate::mail::UPGRADES_FROM.contains(&version) =>
        {
            version.parse::<u8>().map_err(|_| {
                schema_error("msg_schema", version, crate::mail::MAIL_SCHEMA_VERSION)
            })?
        }
        Some(version) => {
            return Err(schema_error(
                "msg_schema",
                version,
                crate::mail::MAIL_SCHEMA_VERSION,
            ));
        }
    };
    // Build the reference in memory: validation never runs schema DDL on
    // the candidate database. These are the definitions used by creation.
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA)?;
    reference.execute_batch(crate::mail::MAIL_SCHEMA)?;
    reference.execute_batch(crate::ledger::LEDGER_SCHEMA)?;
    let mut tables = vec![
        ("meta", true),
        ("journal", true),
        ("state", true),
        ("events", true),
    ];
    tables.push(("log", matches!(schema, "3" | "4")));
    tables.extend(
        [
            "msg_links",
            "msg_meta",
            "msg_deferred",
            "entries",
            "entry_events",
        ]
        .map(|table| (table, schema == "4")),
    );
    tables.extend(
        [
            "msg_rooms",
            "msg_members",
            "msg_messages",
            "msg_bodies",
            "msg_deliveries",
        ]
        .map(|table| (table, mail > 0)),
    );
    tables.push(("msg_decisions", mail >= 5));
    for (table, required_table) in tables {
        let kind: Option<String> = conn
            .query_row(
                "SELECT type FROM sqlite_master WHERE name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if kind.is_none() && !required_table {
            continue;
        }
        if kind.as_deref() != Some("table") {
            return Err(StoreError::BadMeta(format!(
                "schema {schema}: missing table {table}"
            )));
        }
        let required = table_columns(&reference, table)?;
        let actual = table_columns(conn, table)?;
        for (column, shape) in required {
            let required_column = match (table, column.as_str()) {
                ("msg_messages", "request_digest") => mail >= 2,
                ("msg_deliveries", "acked_via") => mail >= 3,
                ("msg_deliveries", "shown_ms") => mail >= 4,
                _ => true,
            };
            if !actual.contains_key(&column) && !required_column {
                continue;
            }
            if actual.get(&column) != Some(&shape) {
                return Err(StoreError::BadMeta(format!(
                    "schema {schema}: incompatible column {table}.{column}"
                )));
            }
        }
    }
    Ok(())
}

/// Only an empty database may acquire a coordinator schema. Existing objects
/// must already name a supported coordinator version before any DDL or journal
/// mode change, and again inside the initialization/upgrade write transaction.
fn check_initializable(conn: &Connection) -> Result<(), StoreError> {
    if let Some(schema) = meta(conn, "schema")? {
        return if schema == SCHEMA_VERSION || UPGRADES_FROM.contains(&schema.as_str()) {
            check_coordinator_structure(conn, &schema)
        } else {
            Err(schema_error("schema", &schema, SCHEMA_VERSION))
        };
    }
    if conn.prepare("SELECT 1 FROM sqlite_master")?.exists([])? {
        return Err(StoreError::BadMeta(
            "schema (missing in a nonempty database)".into(),
        ));
    }
    Ok(())
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
    if !store_file_present(path)? {
        return Ok(None);
    }
    let probe = |flags: OpenFlags| -> Result<Option<(String, String)>, StoreError> {
        let probe = open_durable(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        probe.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        check_initializable(&probe)?;
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

/// The versions the store at `path` holds, as a label (`3+msg5`), to
/// compare with [`build_label`]; `None` for a store that doesn't exist yet
/// or has no schema. Read only, as [`probe_upgrade`] reads, but never
/// through a writer: a hot rollback journal is an error here.
pub fn stored_label(path: &Path) -> Result<Option<String>, StoreError> {
    if !store_file_present(path)? {
        return Ok(None);
    }
    let probe = open_durable(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    probe.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    let Some(schema) = meta(&probe, "schema")? else {
        return Ok(None);
    };
    Ok(Some(label(&schema, meta(&probe, "msg_schema")?.as_deref())))
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
pub(crate) fn open_durable(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    open_durable_wait(path, flags, BUSY_WAIT)
}

/// A missing/zero regular file is absent; an unreachable store link needs
/// repair, not initialization. Never let SQLite inspect a zero-page artifact:
/// its read-only pager may otherwise delete an existing WAL.
pub(crate) fn store_file_present(path: &Path) -> std::io::Result<bool> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for ancestor in path.ancestors() {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(metadata) if metadata.is_symlink() => {
                        if let Err(error) = std::fs::metadata(ancestor) {
                            return Err(std::io::Error::new(
                                error.kind(),
                                format!(
                                    "coordinator store has an unreachable link at {}; restore its target before retrying",
                                    ancestor.display()
                                ),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "coordinator store is not a regular file",
        ));
    }
    Ok(metadata.len() > 0)
}

fn open_durable_wait(
    path: &Path,
    flags: OpenFlags,
    wait: Duration,
) -> rusqlite::Result<Connection> {
    // Every writable open of a store file, or of a copy beside it, comes
    // through here. A private temporary database (the empty path) holds
    // nothing of the store's.
    if !flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY)
        && !path.as_os_str().is_empty()
        && store_writes_forbidden()
    {
        return Err(writes_forbidden_error());
    }
    if flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        match store_file_present(path) {
            Ok(true) => {}
            Ok(false) => return Err(rusqlite::Error::InvalidPath(path.to_path_buf())),
            Err(error) => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(error.to_string()),
                ));
            }
        }
    }
    let conn = Connection::open_with_flags(path, flags)?;
    // Before anything else touches the file: every statement after this
    // waits for a busy store instead of failing at once. Callers that must
    // not wait (restore) set their own afterwards.
    conn.busy_timeout(wait)?;
    durable_settings(&conn)?;
    Ok(conn)
}

fn durable_settings(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "fullfsync", true)?;
    conn.pragma_update(None, "checkpoint_fullfsync", true)?;
    Ok(())
}

/// How long a store connection waits for another process's lock.
const BUSY_WAIT: Duration = Duration::from_secs(30);

/// Polling status must fail promptly even when an exclusive locker prevents reads.
const READ_BUSY_WAIT: Duration = Duration::from_secs(5);

fn read_only_error(error: StoreError) -> StoreError {
    match error {
        StoreError::Sqlite(error)
            if matches!(&error, rusqlite::Error::SqliteFailure(code, _)
                if matches!(code.extended_code, rusqlite::ffi::SQLITE_READONLY_RECOVERY | rusqlite::ffi::SQLITE_READONLY_ROLLBACK)) =>
        {
            StoreError::ReadRecoveryRequired(error)
        }
        other => other,
    }
}

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

/// The documented override for file systems with no full sync barrier:
/// `MDKG_STORE_PLAIN_FSYNC=1` lets a backup copy settle for a plain
/// `fsync` where `F_FULLFSYNC` (or `fsync` itself) is refused as
/// unsupported. A plain macOS `fsync` can leave data in the drive's cache,
/// so this is a weaker guarantee, and an upgrade that relied on it records
/// it in the upgraded store (`meta.upgrade_barrier = "fsync"`) and says so.
pub const PLAIN_FSYNC_ENV: &str = "MDKG_STORE_PLAIN_FSYNC";

#[cfg(not(test))]
fn plain_fsync_allowed() -> bool {
    std::env::var_os(PLAIN_FSYNC_ENV).is_some_and(|v| v == "1")
}

#[cfg(test)]
thread_local! {
    static PLAIN_FSYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn plain_fsync_allowed() -> bool {
    PLAIN_FSYNC.with(std::cell::Cell::get)
}

/// Syncs a backup copy, or its folder, with the full barrier, refusing
/// every failure: a copy an upgrade relies on is durable or the upgrade
/// doesn't start (edd-01a1878e, Storage; Terra's reviews of
/// e821ea6e..a1412008, finding 3, and of 070dc919..0bf4a12a, finding 6).
/// Where the full barrier isn't offered (`F_FULLFSYNC` on an SMB mount
/// answers ENOTSUP; some file systems answer EINVAL) it refuses, unless
/// [`PLAIN_FSYNC_ENV`] accepts a plain `fsync`. Returns whether it settled
/// for the plain one.
fn sync(file: &File, step: &'static str) -> std::io::Result<bool> {
    let unsupported = |e: &std::io::Error| {
        matches!(
            e.kind(),
            std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
        ) || (cfg!(target_vendor = "apple") && e.raw_os_error() == Some(45))
    };
    match injected_io(step).and_then(|()| file.sync_all()) {
        Ok(()) => Ok(false),
        Err(e) if unsupported(&e) && plain_fsync_allowed() => {
            rustix::fs::fsync(file).map_err(std::io::Error::from)?;
            Ok(true)
        }
        Err(e) if unsupported(&e) => Err(std::io::Error::new(
            e.kind(),
            format!(
                "this file system offers no full sync barrier ({e}), so it can't be made \
                 durable; {PLAIN_FSYNC_ENV}=1 accepts a plain fsync, a weaker guarantee mdkg \
                 reports"
            ),
        )),
        Err(e) => Err(e),
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

#[cfg(test)]
thread_local! {
    /// A step at which this thread's next store open fails, for the
    /// upgrade's fault-injection tests.
    static FAIL_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static PAUSE_CREATION: std::cell::RefCell<Option<(&'static str, PathBuf)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn injected(step: &'static str) -> Result<(), StoreError> {
    PAUSE_CREATION.with(|hook| {
        if let Some((phase, ready)) = hook.borrow().as_ref()
            && *phase == step
        {
            std::fs::write(ready, b"ready").expect("creation checkpoint");
            loop {
                std::thread::park();
            }
        }
    });
    if FAIL_AT.with(|f| f.get()) == Some(step) {
        return Err(StoreError::Io(std::io::Error::other(format!(
            "injected failure at {step}"
        ))));
    }
    Ok(())
}

#[cfg(not(test))]
fn injected(_step: &'static str) -> Result<(), StoreError> {
    Ok(())
}

/// Makes the store's entry in its folder, and the folder's entry in its
/// parent (the folder may be new: `open_between` creates it, Terra #16445),
/// durable. Returns whether a sync settled for a plain `fsync`.
fn sync_store_entries(dir: &Path) -> Result<bool, StoreError> {
    let mut weak = sync_entry(dir, "store_folder_sync")?;
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        weak |= sync_entry(parent, "store_parent_sync")?;
    }
    Ok(weak)
}

/// Syncs one folder with the full barrier, as [`sync`] does: where the
/// file system refuses it (an SMB mount's `F_FULLFSYNC`), only
/// [`PLAIN_FSYNC_ENV`] settles for a plain `fsync`, and says so (Terra
/// #16506). A test's injected failure replaces the sync's own result, so
/// a caller that dropped the result would be caught.
fn sync_entry(dir: &Path, step: &'static str) -> Result<bool, StoreError> {
    Ok(File::open(dir).and_then(|folder| sync(&folder, step))?)
}

#[cfg(test)]
thread_local! {
    /// Every sync on this thread fails as unsupported (no full barrier
    /// anywhere), so a test can also inject a failure at another step.
    static NO_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`injected`] for a sync: the step fails as a file system that offers
/// no full barrier would, with an unsupported error, so a test shows it
/// is refused, or, with the override, settled for a plain fsync.
#[cfg(test)]
fn injected_io(step: &'static str) -> std::io::Result<()> {
    if FAIL_AT.with(|f| f.get()) == Some(step) || NO_BARRIER.with(std::cell::Cell::get) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("injected unsupported sync at {step}"),
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn injected_io(_step: &'static str) -> std::io::Result<()> {
    Ok(())
}

/// Copies the store `conn` has open (at `path`, with no transaction of its
/// own open) with SQLite's backup API, syncs the copy and its folder, and
/// marks it with the store it came from. Written under a temporary name
/// of its own and published without replacing anything, so a copy that
/// exists is complete, and two copies taken at once (snapshots hold no
/// lock) each land whole under their own names.
fn back_up(conn: &Connection, path: &Path, from: &str, to: &str) -> Result<PathBuf, StoreError> {
    back_up_noting(conn, path, from, to).map(|(copy, _)| copy)
}

/// Syncs a copy's folder ([`sync`]), saying whether it settled for a plain
/// `fsync`.
fn sync_folder(dir: &Path, step: &'static str) -> Result<bool, StoreError> {
    File::open(dir)
        .and_then(|d| sync(&d, step))
        .map_err(|e| StoreError::Backup(format!("can't sync the copy's folder: {e}")))
}

/// The proof beside a copy (`<copy>.synced`) that its final folder
/// barrier held: the copy's own generation (a random id written into its
/// `meta.backup_generation` before it was published), with its
/// inode, length and modification time, so a proof never vouches for
/// another file under the same name, a replacement on a file system that
/// reuses inodes included (Terra's code review #8466, with the
/// orchestrator's call). Device numbers change across reboots/remounts and
/// are not durable identity. A proof goes whenever its copy does
/// ([`remove_copy`]).
fn proof_path(copy: &Path) -> PathBuf {
    let mut name = copy.as_os_str().to_owned();
    name.push(".synced");
    PathBuf::from(name)
}

/// Removes a copy and, in the same step, its proof.
fn remove_copy(copy: &Path) {
    std::fs::remove_file(copy).ok();
    std::fs::remove_file(proof_path(copy)).ok();
}

fn proof_of(copy: &Path, generation: &str) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(copy)?;
    if !m.is_file() {
        return Err(std::io::Error::other("a copy is a file"));
    }
    Ok(format!(
        "mdkg-copy-synced gen={generation} ino={} len={} mtime={}.{}\n",
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec()
    ))
}

fn proof_matches(found: &str, expected: &str) -> bool {
    if found == expected {
        return true;
    }
    // Old proofs persisted the device as well. Ignore only that decimal
    // field; every stable field, including generation and inode, must match.
    let Some((prefix, rest)) = found.split_once(" dev=") else {
        return false;
    };
    let Some((device, suffix)) = rest.split_once(' ') else {
        return false;
    };
    !device.is_empty()
        && device.bytes().all(|b| b.is_ascii_digit())
        && device.parse::<u64>().is_ok()
        && format!("{prefix} {suffix}") == expected
}

/// Writes `copy`'s proof, new (never through a link or over another
/// file), synced, with its folder.
fn write_proof(copy: &Path, generation: &str) -> Result<(), StoreError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text = proof_of(copy, generation)?;
    let proof = proof_path(copy);
    // A proof left by an earlier copy of this name can't vouch for this
    // one (another generation): it goes, so this copy isn't left weak
    // (Opus's code review #8408).
    if std::fs::symlink_metadata(&proof).is_ok() {
        std::fs::remove_file(&proof)?;
    }
    let written = (|| -> Result<(), StoreError> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&proof)?;
        file.write_all(text.as_bytes())?;
        if sync(&file, "proof_sync")? {
            return Err(StoreError::Backup(
                "the proof settled for a plain fsync".into(),
            ));
        }
        let dir = proof.parent().unwrap_or(Path::new("."));
        if sync_folder(dir, "proof_folder")? {
            return Err(StoreError::Backup(
                "the proof settled for a plain fsync".into(),
            ));
        }
        Ok(())
    })();
    if written.is_err() {
        std::fs::remove_file(&proof).ok();
    }
    written
}

/// A copy's barrier as its readers take it: its own `backup_barrier`
/// mark, lifted only by a proof that matches the copy and its generation
/// ([`write_proof`]). A copy from before proofs, with no mark, reads as
/// it always did.
fn copy_barrier(copy: &Path, marked: Option<String>, generation: Option<&str>) -> Option<String> {
    let proven = || {
        let Some(generation) = generation.filter(|g| !g.is_empty()) else {
            return false;
        };
        let Ok(meta) = std::fs::symlink_metadata(proof_path(copy)) else {
            return false;
        };
        meta.is_file()
            && meta.len() < 512
            && std::fs::read_to_string(proof_path(copy))
                .ok()
                .zip(proof_of(copy, generation).ok())
                .is_some_and(|(found, want)| proof_matches(&found, &want))
    };
    match marked {
        Some(m) if m == "fsync" && proven() => None,
        m => m,
    }
}

/// [`back_up`], saying whether a sync settled for a plain `fsync`
/// ([`PLAIN_FSYNC_ENV`]).
fn back_up_noting(
    conn: &Connection,
    path: &Path,
    from: &str,
    to: &str,
) -> Result<(PathBuf, bool), StoreError> {
    may_write()?;
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
    let mut weak = false;
    // This copy's own generation, which its proof must name.
    let generation = mdkg_format::Digest::of_bytes(
        format!("{}-{}", mdkg_format::Uuid7::now(), partial.display()).as_bytes(),
    )
    .hex()[..32]
        .to_string();
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
        // Born marked weak, and the copy itself never loses the mark: the
        // marker is this copy's own (never one the source inherited from
        // a weak restore; Opus's code review #6264, finding 1), and a copy
        // reads as fully synced only beside the proof written once the
        // final folder barrier held ([`copy_barrier`]). So a lost write, a
        // reader listing it before that barrier, or a removal that a crash
        // undoes, leaves a copy marked weak, never strong (Terra's code
        // reviews #6874, finding 3, and #7598, finding 1, with the
        // orchestrator's call).
        copy.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('backup_barrier', 'fsync')",
            [],
        )?;
        copy.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('backup_generation', ?1)",
            [&generation],
        )?;
        copy.close().map_err(|(_, e)| e)?;
        weak |= sync(&File::open(&partial)?, "copy_sync")?;
        // The folder's barrier, tried before the publish (which also makes
        // the partial's own name durable; Terra's code review #6273,
        // finding 1).
        let dir = partial.parent().unwrap_or(Path::new("."));
        weak |= sync_folder(dir, "copy_folder_barrier")?;
        // Published under the first free name: a link fails rather than
        // replace a copy another process published there first. Nothing is
        // written to the copy after this (Terra's code review #6874,
        // findings 1 and 4).
        let target = loop {
            let target = backup_path(path, from, to);
            match std::fs::hard_link(&partial, &target) {
                Ok(()) => break target,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        };
        std::fs::remove_file(&partial).ok();
        // The copy's name, and the partial's removal, are durable only once
        // the folder is synced. A copy whose folder can't be synced isn't
        // one to rely on: its name is removed again, and should a crash
        // undo that removal, the copy is still marked weak (Terra's code
        // reviews #6874, findings 1 and 2, and #7598, finding 1).
        let synced =
            injected("copy_folder_sync").and_then(|()| sync_folder(dir, "copy_folder_published"));
        match synced {
            Ok(later_weak) => weak |= later_weak,
            Err(e) => {
                remove_copy(&target);
                return Err(e);
            }
        }
        // Every barrier held: the proof beside it lifts the mark. A proof
        // that can't be written leaves the copy weak, never failed.
        if !weak && write_proof(&target, &generation).is_err() {
            weak = true;
        }
        Ok(target)
    })();
    if result.is_err() {
        std::fs::remove_file(&partial).ok();
    }
    result.map(|copy| (copy, weak)).map_err(|e| fail(&e))
}

/// Copies the store at `path` beside it, as an upgrade does, named
/// `<name>.pre-<its versions>-to-<label>.sqlite` and marked as its copy,
/// so `store snapshots` lists it and `store restore` takes it (`migrate`
/// takes one before it changes anything).
pub fn snapshot(path: &Path, label: &str) -> Result<PathBuf, StoreError> {
    may_write()?;
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
    /// `fsync` when the copy was synced with a plain fsync only, under
    /// [`PLAIN_FSYNC_ENV`]: a weaker guarantee than the full barrier.
    pub barrier: Option<String>,
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
            barrier: copy_barrier(
                &entry.path(),
                value("backup_barrier"),
                value("backup_generation").as_deref(),
            ),
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
    /// Whether that copy settled for a plain fsync ([`PLAIN_FSYNC_ENV`]).
    pub previous_weak: bool,
    /// `fsync` when the restored copy itself was synced that way.
    pub from_barrier: Option<String>,
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
    may_write()?;
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
    let from_barrier = copy_barrier(
        copy,
        meta(&source, "backup_barrier")?,
        meta(&source, "backup_generation")?.as_deref(),
    );
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
    let prepared = (|| -> Result<(String, PathBuf, bool), StoreError> {
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
        let (previous, previous_weak) = back_up_noting(&store, path, &replaced, "restore")
            .map_err(|e| refuse(format!("the store couldn't be copied first: {e}")))?;
        if let Err(e) = injected("restore_write").and_then(|()| copy_all(&incoming, &mut store)) {
            // One transaction, but a failure while SQLite finishes it (a
            // failed sync after the journal is cleared) can leave the copy
            // landed: so the store as it was stays, and the error says so,
            // and how far that copy can be trusted (Terra's code review
            // #6273, finding 2).
            let weak = if previous_weak {
                " (synced with a plain fsync only: a power loss can lose it, so take a \
                 snapshot of whichever store you keep, without MDKG_STORE_PLAIN_FSYNC, once \
                 this is settled)"
            } else {
                ""
            };
            return Err(refuse(format!(
                "the copy failed ({e}), and may have landed in part or whole; the store as it \
                 was is at {}{weak}, which `store restore` puts back",
                previous.display()
            )));
        }
        Ok((replaced, previous, previous_weak))
    })();
    let (replaced, previous, previous_weak) = match prepared {
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
        .execute(
            "DELETE FROM meta WHERE key IN ('backup_of', 'backup_barrier', 'backup_generation')",
            [],
        )
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
        previous_weak,
        from_barrier,
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

/// The coordinator state as `conn`'s transaction reads it: the lease
/// state an entry op's class is judged on (A27 rule 4).
pub(crate) fn coordinator_in(conn: &Connection) -> Result<Coordinator, StoreError> {
    Ok(load(conn, 0)?.1)
}

pub(crate) fn load(conn: &Connection, ttl: Ms) -> Result<(i64, Coordinator), StoreError> {
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
    /// Records an already-read observation on a fresh, zero-wait connection.
    /// Never creates/upgrades/repairs a store, expires leases, or runs Git.
    /// `Some(reason)` means the observation was deferred without a write.
    pub fn try_record_observation(
        path: &Path,
        observation: &Observation,
    ) -> Result<Option<&'static str>, StoreError> {
        may_write()?;
        let attempt = || -> Result<Option<&'static str>, StoreError> {
            let mut conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.busy_timeout(Duration::ZERO)?;
            conn.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )?;
            durable_settings(&conn)?;
            conn.pragma_update(None, "wal_autocheckpoint", 0)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            check_mail_schema(&tx)?;
            let ttl: String =
                tx.query_row("SELECT value FROM meta WHERE key = 'ttl'", [], |r| r.get(0))?;
            let ttl = ttl.parse().map_err(|_| StoreError::BadMeta("ttl".into()))?;
            let (seq, mut state) = load(&tx, ttl)?;
            if state.generation() != observation.read_generation {
                return Ok(Some("coordinator changed"));
            }
            let before = state.clone();
            let command = Command::ObserveMain {
                observation: observation.clone(),
            };
            let outcome = state.apply(&command);
            if matches!(outcome, Outcome::Refused { .. }) {
                return Ok(Some("coordinator changed"));
            }
            if state == before {
                return Ok(None);
            }
            Self::save(&tx, seq + 1, &command, &outcome, &mut state)?;
            tx.commit()?;
            Ok(None)
        };
        match attempt() {
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Ok(Some("store busy"))
            }
            Err(StoreError::Schema(_) | StoreError::BadMeta(_)) => Ok(Some("schema not current")),
            other => other,
        }
    }

    /// Opens an existing coordinator for queries only. Never creates,
    /// upgrades, repairs, changes journal mode or checkpoints the store.
    /// A missing or zero-object store reads as absent. Hot journals require a
    /// writer and return `ReadRecoveryRequired`. Each lock wait is at most 5 s.
    /// SQLite may maintain its WAL-index cache files during a read.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, StoreError> {
        Self::read_only_inner(path).map_err(read_only_error)
    }

    /// A guarded read handle without version validation, for diagnostics and
    /// migrations inspecting older schemas. Missing/uninitialized stores are
    /// absent; SQLite may maintain WAL-index caches, never logical contents.
    pub fn open_raw_read_only(path: &Path) -> Result<Option<Connection>, StoreError> {
        let read = || -> Result<Option<Connection>, StoreError> {
            if !store_file_present(path)? {
                return Ok(None);
            }
            let conn = open_durable_wait(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                READ_BUSY_WAIT,
            )?;
            conn.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )?;
            if !conn.prepare("SELECT 1 FROM sqlite_master")?.exists([])? {
                return Ok(None);
            }
            Ok(Some(conn))
        };
        read().map_err(read_only_error)
    }

    /// Event readers share the same absence, schema and journal validation.
    pub(crate) fn open_read_only_connection(path: &Path) -> Result<Option<Connection>, StoreError> {
        Ok(Self::open_read_only(path)?.map(|store| store.conn))
    }

    fn read_only_inner(path: &Path) -> Result<Option<Self>, StoreError> {
        let before = crate::activity::identity_at(path)?;
        let Some(conn) = Self::open_raw_read_only(path)? else {
            return Ok(None);
        };
        let file = crate::activity::identity_at(path)?;
        if before != file {
            return Err(crate::activity::replaced());
        }
        let tx = conn.unchecked_transaction()?;
        if let Some((from, to)) = upgrade_of(&tx)? {
            return Err(StoreError::ReadUpgradeRequired { from, to });
        }
        check_mail_schema(&tx)?;
        let ttl: String =
            tx.query_row("SELECT value FROM meta WHERE key = 'ttl'", [], |r| r.get(0))?;
        let ttl = ttl
            .parse()
            .map_err(|_| StoreError::BadMeta(format!("ttl {ttl}")))?;
        tx.finish()?;
        Ok(Some(Self {
            conn,
            file,
            ttl,
            backup: None,
            backup_weak: false,
            created_weak: false,
            ack_grace_ms: 0,
            pure_reads: true,
        }))
    }

    /// Keep the public name absent until all initial schema and metadata are
    /// committed in one private rollback-mode database. Concurrent creators
    /// publish without replacing each other's stores; unpublished crash debris
    /// is private and is never opened as the coordinator.
    /// Returns whether this open created the store with a plain `fsync`
    /// only ([`PLAIN_FSYNC_ENV`]), a weaker guarantee than the full barrier.
    fn create_if_missing(path: &Path, ttl: Ms) -> Result<bool, StoreError> {
        may_write()?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if path.try_exists()? {
            // A store found published may follow a creation whose entry
            // syncs failed: they are made durable again before any write is
            // acknowledged (Terra #16575). Idempotent. Only a creation
            // reports a weaker barrier: this open made nothing.
            sync_store_entries(dir)?;
            return Ok(false);
        }
        let staging = dir.join(format!(".mdkg-store-{}", mdkg_format::Uuid7::now()));
        let mut folder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            folder.mode(0o700);
        }
        folder.create(&staging)?;
        let temporary = staging.join("coord.sqlite");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let result = (|| -> Result<bool, StoreError> {
            drop(options.open(&temporary)?);
            injected("creation_file")?;
            let mut conn = open_durable(&temporary, OpenFlags::default())?;
            conn.pragma_update(None, "journal_mode", "DELETE")?;
            injected("creation_opened")?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut committing = false;
            Self::finish_open(tx, path, ttl, false, &mut committing)?;
            conn.close().map_err(|(_, error)| error)?;
            // The full barrier, or with the override a plain fsync, which
            // the open reports (Terra #16506).
            let weak = sync(&File::open(&temporary)?, "creation_sync")?;
            match rustix::fs::renameat_with(
                rustix::fs::CWD,
                &temporary,
                rustix::fs::CWD,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(std::io::Error::from(error).into()),
            }
            injected("creation_published")?;
            Ok(sync_store_entries(dir)? | weak)
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

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
        // Before the folder or the store is made.
        may_write()?;
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            // Folders made here are the owner's only, as the store files
            // are (e2e N4); an existing folder keeps its mode.
            let mut folder = std::fs::DirBuilder::new();
            folder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                folder.mode(0o700);
            }
            folder.create(dir)?;
        }
        let created_weak = Self::create_if_missing(path, ttl)?;
        let refuse = upgrade == Upgrade::Refuse;
        // Refused before anything is written, and before this handle
        // touches the store: its close could checkpoint a WAL another
        // process left. Checked again under the write lock, since a
        // restore can swap an older store in until then.
        let pending = probe_upgrade(path)?;
        if refuse && let Some((from, to)) = pending {
            return Err(StoreError::UpgradeRefused { from, to });
        }
        let before = crate::activity::identity_at(path)?;
        let mut conn = open_durable(path, OpenFlags::default())?;
        // Test-only: foreign keys on, so a deferred violation can make the
        // upgrade's own COMMIT fail (`deferred_fk`, below). Outside any
        // transaction, where the pragma takes effect.
        #[cfg(test)]
        if FAIL_AT.with(|f| f.get()) == Some("deferred_fk") {
            conn.pragma_update(None, "foreign_keys", true)?;
        }
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
        injected("creation_wal")?;
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
        let upgrading = pending.clone();
        let backup = match pending {
            Some((from, to)) => {
                // Read through a second connection: this one's write lock
                // keeps every other writer out, and it has written nothing.
                let reader = open_durable(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| StoreError::Backup(e.to_string()))?;
                Some(back_up_noting(&reader, path, &from, &to)?)
            }
            None => None,
        };
        let weak = backup.as_ref().is_some_and(|(_, weak)| *weak);
        let backup = backup.map(|(copy, _)| copy);
        let mut committing = false;
        match Self::finish_open(tx, path, ttl, weak, &mut committing) {
            Ok(ttl) => Ok(Self {
                conn,
                file,
                ttl,
                backup,
                backup_weak: weak,
                created_weak,
                ack_grace_ms: 0,
                pure_reads: false,
            }),
            Err(e) => {
                // The copy goes only when the upgrade failed before its
                // COMMIT was issued: then nothing changed, and the copy
                // would only pile up on retries. An error from COMMIT or
                // after it leaves the outcome unknown, whatever a readback
                // shows (after a failed WAL sync a commit frame can be on
                // disk while the live index still reads schema 3), so the
                // copy stays (edd-01a1878e, Storage; Terra's fourth
                // review, finding 9).
                if let Some(copy) = &backup
                    && upgrading.is_some()
                    && !committing
                {
                    remove_copy(copy);
                }
                Err(e)
            }
        }
    }

    /// Creates or upgrades the tables inside `tx`, and commits. `committing`
    /// is set just before COMMIT is issued, so a caller can tell a failure
    /// before it (nothing changed) from one whose outcome is unknown.
    fn finish_open(
        tx: Transaction<'_>,
        path: &Path,
        ttl: Ms,
        weak_copy: bool,
        committing: &mut bool,
    ) -> Result<Ms, StoreError> {
        check_initializable(&tx)?;
        tx.execute_batch(SCHEMA)?;
        injected("creation_schema")?;
        tx.execute_batch(crate::mail::MAIL_SCHEMA)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('msg_schema', ?1)",
            params![crate::mail::MAIL_SCHEMA_VERSION],
        )?;
        crate::mail::upgrade(&tx)?;
        // Schema 4's flow tables, for a new store and for one this open
        // upgrades (after its copy).
        crate::ledger::create(&tx)?;
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
        // already has; a chain a "3" store already has goes on (genesis
        // writes nothing over a head).
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
        if weak_copy {
            // The copy this upgrade relies on settled for a plain fsync
            // (PLAIN_FSYNC_ENV): the store says so for as long as it lives.
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('upgrade_barrier', 'fsync')",
                [],
            )?;
        }
        injected("before_commit")?;
        *committing = true;
        // COMMIT itself failing, with nothing written (the store reads
        // back as it was, yet the outcome counts as unknown).
        injected("commit_failed")?;
        // Test-only: a real COMMIT error, from a deferred foreign key that
        // fails at COMMIT, with SQLite rolling the upgrade back. It does
        // not simulate a failed WAL sync after a commit frame (Terra's
        // code review #5973, r2 #8); `after_commit` stands in for that.
        #[cfg(test)]
        if FAIL_AT.with(|f| f.get()) == Some("deferred_fk") {
            tx.execute_batch(
                "CREATE TABLE fk_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE fk_child (p INTEGER REFERENCES fk_parent (id)
                     DEFERRABLE INITIALLY DEFERRED);
                 INSERT INTO fk_child (p) VALUES (1);",
            )?;
        }
        tx.commit()?;
        // A commit that wrote its frame and then failed (a WAL sync)
        // reports an error although the upgrade may stand.
        injected("after_commit")?;
        Ok(ttl)
    }

    /// The copy this open took before upgrading the store, if it did.
    pub fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }

    /// Whether [`Store::backup`]'s copy was synced with a plain fsync only
    /// ([`PLAIN_FSYNC_ENV`]; the upgraded store records it as
    /// `meta.upgrade_barrier`).
    pub fn backup_weak(&self) -> bool {
        self.backup_weak
    }

    /// Whether this open created the store with a plain `fsync` only, where
    /// the file system refused the full barrier and [`PLAIN_FSYNC_ENV`]
    /// accepted the weaker guarantee (Terra #16506).
    pub fn created_weak(&self) -> bool {
        self.created_weak
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
        self.execute_then_checked(first, then, None)
    }

    /// [`Store::execute_then`], running `check` before the transaction
    /// commits when both commands were recorded (and only then).
    pub fn execute_then_checked(
        &mut self,
        first: &Command,
        then: impl FnOnce(&Coordinator, &Outcome) -> Option<Command>,
        check: Option<BeforeCommit<'_>>,
    ) -> Result<Chained, StoreError> {
        let mut then = Some(then);
        let mut outcomes = self.execute_chain_completed(
            first,
            |c, o| then.take().and_then(|f| f(c, o)),
            None,
            check,
        )?;
        let recorded = outcomes.recorded;
        let first = outcomes.outcomes.remove(0);
        Ok(Chained {
            first,
            second: outcomes.outcomes.pop(),
            recorded,
            checked: outcomes.checked,
        })
    }

    /// Runs `check` (a node's settle) alone in its own write transaction,
    /// applying no command: commits what it wrote when it passes, and rolls
    /// it back when it refuses (`Ok(false)`) or fails. For `done`'s rerun
    /// on a node whose done file main already holds, which needs no mark
    /// and no work state, only the settle (Terra's code review #10631,
    /// finding 1).
    pub fn check_alone(&mut self, check: BeforeCommit<'_>) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !check(&tx)? {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
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
        self.complete_local_checked(leaf, node, actor, revision, now, None)
    }

    /// [`Store::complete_local`], running `check` (the node's settle)
    /// before the transaction commits, once the completion is recorded.
    pub fn complete_local_checked(
        &mut self,
        leaf: &Scope,
        node: &str,
        actor: &str,
        revision: &str,
        now: Ms,
        check: Option<BeforeCommit<'_>>,
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
            check,
        )?;
        Ok(Chained {
            first: batch.outcomes.remove(0),
            second: batch.outcomes.pop(),
            recorded: batch.recorded,
            checked: batch.checked,
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
        self.execute_chain_completed(first, next, None, None)
    }

    fn execute_chain_completed(
        &mut self,
        first: &Command,
        mut next: impl FnMut(&Coordinator, &Outcome) -> Option<Command>,
        completion: Option<(&str, &str)>,
        check: Option<BeforeCommit<'_>>,
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
                    checked: true,
                });
            }
            seq += 1;
            Self::save(&tx, seq, &command, &outcome, &mut coordinator)?;
        }
        let all = outcomes.iter().all(|o| o.error().is_none());
        if all
            && let Some((node, revision)) = completion
            && let Command::SetWork { leaf, .. } = first
        {
            crate::activity::record_completion(&tx, first_seq, leaf.as_str(), node, revision)?;
        }
        // The check sees everything this transaction wrote; a refusal
        // drops the transaction, which rolls every command back.
        if all
            && let Some(check) = check
            && !check(&tx)?
        {
            return Ok(ChainOutcomes {
                outcomes,
                recorded: false,
                checked: false,
            });
        }
        tx.commit()?;
        Ok(ChainOutcomes {
            outcomes,
            recorded: true,
            checked: true,
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

    /// Coordinator state and API event-stream head from one read transaction.
    /// This read has no expiry, observation, purge or journal effects.
    pub fn coordinator_with_cursor(&self) -> Result<(Coordinator, String), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        check_schema(&tx)?;
        let (_, coordinator) = load(&tx, self.ttl)?;
        let cursor = crate::events::head_in(&tx)?.encode();
        tx.finish()?;
        Ok((coordinator, cursor))
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
    fn dangling_store_links_refuse_initialization_advice_on_every_read() {
        use std::os::unix::fs::symlink;
        for parent in [false, true] {
            let dir = std::env::temp_dir()
                .join(format!("mdkg-dangling-read-{}", mdkg_format::Uuid7::now()));
            let folder = dir.join("mdkg");
            let path = folder.join("coord.sqlite");
            drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
            let graph = "01a185ac-560e-76e0-91af-2c49dfc29b81";
            let mut reader = crate::events::Reader::open(&path, graph, None).unwrap();
            let missing = dir.join("missing-target");
            let link = if parent { &folder } else { &path };
            if parent {
                std::fs::remove_dir_all(&folder).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            symlink(&missing, link).unwrap();
            for error in [
                Store::open_read_only(&path).unwrap_err(),
                crate::events::head(&path).unwrap_err(),
            ] {
                assert!(error.to_string().contains("restore"), "{error}");
                assert!(!error.to_string().contains("initialize"), "{error}");
            }
            for error in [
                crate::events::Reader::open(&path, graph, None).unwrap_err(),
                reader.poll(1).unwrap_err(),
            ] {
                let crate::events::ReadError::Store(error) = error else {
                    panic!("{error:?}");
                };
                assert!(error.to_string().contains("restore"), "{error}");
            }
            assert!(std::fs::symlink_metadata(link).unwrap().is_symlink());
            assert!(!missing.exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn malformed_current_tables_are_refused_before_schema_ddl() {
        for sql in [
            "DROP TABLE journal; CREATE VIEW journal AS SELECT 0 AS seq, '' AS command, '' AS outcome;",
            "DROP TABLE journal; CREATE TABLE journal(seq INTEGER PRIMARY KEY, command INTEGER, outcome TEXT NOT NULL);",
        ] {
            let dir =
                std::env::temp_dir().join(format!("mdkg-structure-{}", mdkg_format::Uuid7::now()));
            let path = dir.join("coord.sqlite");
            drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(sql).unwrap();
            drop(conn);
            let before = std::fs::read(&path).unwrap();
            for upgrade in [Upgrade::Allow, Upgrade::Refuse] {
                assert!(matches!(
                    Store::open_with(&path, 100, upgrade),
                    Err(StoreError::BadMeta(_))
                ));
            }
            assert_eq!(std::fs::read(&path).unwrap(), before);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn version_specific_structure_keeps_supported_upgrade_paths() {
        for version in ["1", "2", "3"] {
            let dir = std::env::temp_dir().join(format!(
                "mdkg-legacy-structure-{}",
                mdkg_format::Uuid7::now()
            ));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("coord.sqlite");
            schema_3_at(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE meta SET value=?1 WHERE key='schema'", [version])
                .unwrap();
            if version != "3" {
                conn.execute_batch(
                    "DROP TABLE log; DELETE FROM meta WHERE key IN ('store_id','log_head');",
                )
                .unwrap();
            }
            drop(conn);
            let store = Store::open_with(&path, 999, Upgrade::Allow).unwrap();
            assert_eq!(store.ttl(), 1000);
            assert!(store.backup().is_some());
            drop(store);
            assert!(Store::open_read_only(&path).unwrap().is_some());
            std::fs::remove_dir_all(dir).unwrap();
        }
        for version in ["1", "2", "3", "4"] {
            let dir = std::env::temp_dir().join(format!(
                "mdkg-legacy-mail-shape-{}",
                mdkg_format::Uuid7::now()
            ));
            let path = dir.join("coord.sqlite");
            drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE meta SET value=?1 WHERE key='msg_schema'", [version])
                .unwrap();
            conn.execute_batch("DROP TABLE msg_decisions;").unwrap();
            if version == "1" {
                conn.execute_batch("ALTER TABLE msg_messages DROP COLUMN request_digest;")
                    .unwrap();
            }
            if matches!(version, "1" | "2") {
                conn.execute_batch("ALTER TABLE msg_deliveries DROP COLUMN acked_via;")
                    .unwrap();
            }
            if version != "4" {
                conn.execute_batch("ALTER TABLE msg_deliveries DROP COLUMN shown_ms;")
                    .unwrap();
            }
            drop(conn);
            let store = Store::open_with(&path, 999, Upgrade::Allow).unwrap();
            assert_eq!(store.ttl(), 100);
            assert!(store.backup().is_some());
            drop(store);
            assert!(Store::open_read_only(&path).unwrap().is_some());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn upgrades_refuse_incompatible_future_objects_before_changing_versions() {
        for version in ["1", "4"] {
            let dir = std::env::temp_dir()
                .join(format!("mdkg-future-shape-{}", mdkg_format::Uuid7::now()));
            let path = dir.join("coord.sqlite");
            drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE meta SET value=?1 WHERE key='msg_schema'", [version])
                .unwrap();
            if version == "1" {
                let schema: String = conn
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE name='msg_messages'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                conn.execute_batch("DROP TABLE msg_messages;").unwrap();
                conn.execute_batch(
                    &schema.replace("request_digest TEXT NOT NULL", "request_digest INTEGER"),
                )
                .unwrap();
            } else {
                conn.execute_batch("DROP TABLE msg_decisions; CREATE TABLE msg_decisions(seq INTEGER PRIMARY KEY, provenance INTEGER);").unwrap();
            }
            drop(conn);
            assert!(matches!(
                Store::open_with(&path, 100, Upgrade::Allow),
                Err(StoreError::BadMeta(_))
            ));
            let conn = Connection::open(&path).unwrap();
            assert_eq!(meta(&conn, "msg_schema").unwrap().as_deref(), Some(version));
            drop(conn);
            assert!(matches!(
                Store::open_with(&path, 100, Upgrade::Allow),
                Err(StoreError::BadMeta(_))
            ));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn foreign_refusals_preserve_committed_data_through_native_recovery() {
        fn logical(conn: &Connection) -> (Vec<(String, String)>, Vec<String>) {
            let schema = conn.prepare("SELECT name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name")
                .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap();
            let rows = conn
                .prepare("SELECT value FROM foreign_data ORDER BY value")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (schema, rows)
        }
        for mode in ["hot", "wal"] {
            for upgrade in [Upgrade::Allow, Upgrade::Refuse] {
                let dir = std::env::temp_dir().join(format!(
                    "mdkg-foreign-journal-{}",
                    mdkg_format::Uuid7::now()
                ));
                std::fs::create_dir(&dir).unwrap();
                let source = dir.join("source.sqlite");
                let path = dir.join("coord.sqlite");
                let writer = Connection::open(&source).unwrap();
                if mode == "wal" {
                    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
                }
                writer.execute_batch("CREATE TABLE foreign_data(value TEXT); INSERT INTO foreign_data VALUES ('preserve'); CREATE UNIQUE INDEX entries_request ON foreign_data(value); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO meta VALUES('schema','4'),('msg_schema','5'),('ttl','100');").unwrap();
                let expected = logical(&writer);
                if mode == "hot" {
                    writer
                        .execute_batch("PRAGMA cache_size=1; BEGIN IMMEDIATE;")
                        .unwrap();
                    for seq in 0..200 {
                        writer
                            .execute(
                                "INSERT INTO foreign_data VALUES(?1)",
                                [format!("uncommitted{seq}{}", "x".repeat(4000))],
                            )
                            .unwrap();
                    }
                    std::fs::copy(&source, &path).unwrap();
                    std::fs::copy(
                        dir.join("source.sqlite-journal"),
                        dir.join("coord.sqlite-journal"),
                    )
                    .unwrap();
                    writer.execute_batch("ROLLBACK;").unwrap();
                } else {
                    std::fs::copy(&source, &path).unwrap();
                    std::fs::copy(dir.join("source.sqlite-wal"), dir.join("coord.sqlite-wal"))
                        .unwrap();
                    std::fs::write(dir.join("coord.sqlite-shm"), vec![0; 32768]).unwrap();
                }
                drop(writer);
                assert!(
                    matches!(
                        Store::open_with(&path, 100, upgrade),
                        Err(StoreError::BadMeta(_))
                    ),
                    "{mode}"
                );
                let reader =
                    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
                assert_eq!(logical(&reader), expected, "{mode}");
                drop(reader);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn read_only_store_refuses_directories_and_zero_length_special_files() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-special-store-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        let fifo = dir.join("coord.sqlite");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::metadata(&fifo).unwrap().len(), 0);
        assert!(Store::open_read_only(&fifo).is_err());
        assert!(Store::open_read_only(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creating_through_a_link_does_not_narrow_the_target_folder() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir =
            std::env::temp_dir().join(format!("mdkg-linked-parent-{}", mdkg_format::Uuid7::now()));
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("mdkg");
        symlink(&target, &link).unwrap();
        drop(Store::open_with(&link.join("coord.sqlite"), 100, Upgrade::Refuse).unwrap());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_bare_store_filename_does_not_narrow_its_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "MDKG_TEST_BARE_STORE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            drop(Store::open_with(Path::new("coord.sqlite"), 100, Upgrade::Refuse).unwrap());
            assert!(
                Store::open_read_only(Path::new("coord.sqlite"))
                    .unwrap()
                    .is_some()
            );
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("mdkg-bare-store-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .current_dir(&dir)
            .env(CHILD, "1")
            .args([
                "--exact",
                "store::tests::a_bare_store_filename_does_not_narrow_its_working_directory",
                "--test-threads=1",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// e2e N4: the folders an open creates for the store (`.git/mdkg`)
    /// are 0700; an existing one keeps its mode.
    #[cfg(unix)]
    #[test]
    fn a_created_store_folder_is_the_owners_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("mdkg-store-folder-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("mdkg/coord.sqlite");
        drop(Store::open_with(&path, 100, Upgrade::Allow).unwrap());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("mdkg")), 0o700);
        assert_eq!(mode(&dir), 0o755);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `--read-only` (forbid_store_writes): every way this crate writes a
    /// store is refused before it writes, a missing store's folder isn't
    /// made, and read-only opens still work. The store's bytes and its
    /// WAL's content are unchanged.
    #[test]
    fn forbidden_writes_are_refused_before_anything_is_written() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-forbid-writes-{}", mdkg_format::Uuid7::now()));
        let path = dir.join("mdkg").join("coord.sqlite");
        let mut writer = Store::open_with(&path, 100, Upgrade::Allow).unwrap();
        writer
            .execute(&Command::Register {
                leaf: Scope::new("work/a"),
                content: "work/a@0".into(),
            })
            .unwrap();
        drop(writer);
        let bytes = || {
            let mut wal = path.as_os_str().to_owned();
            wal.push("-wal");
            (
                std::fs::read(&path).unwrap(),
                std::fs::read(&wal).unwrap_or_default(),
            )
        };
        let before = bytes();
        assert!(!store_writes_forbidden());
        forbid_store_writes();
        assert!(store_writes_forbidden());
        let forbidden = |result: Result<(), StoreError>| {
            assert!(
                matches!(result, Err(StoreError::WritesForbidden)),
                "{result:?}"
            );
        };
        for upgrade in [Upgrade::Allow, Upgrade::Refuse] {
            forbidden(Store::open_with(&path, 100, upgrade).map(drop));
        }
        let elsewhere = dir.join("other").join("coord.sqlite");
        forbidden(Store::open_with(&elsewhere, 100, Upgrade::Allow).map(drop));
        assert!(!dir.join("other").exists(), "no folder made");
        forbidden(Store::try_record_observation(&path, &Observation::default()).map(drop));
        forbidden(snapshot(&path, "manual").map(drop));
        forbidden(restore(&path, &path).map(drop));
        forbidden(
            open_durable(&path, OpenFlags::default())
                .map(drop)
                .map_err(StoreError::from),
        );
        forbidden(
            open_durable(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map(drop)
                .map_err(StoreError::from),
        );
        assert!(
            StoreError::WritesForbidden
                .to_string()
                .contains("--read-only")
        );
        let reader = Store::open_read_only(&path).unwrap().unwrap();
        assert!(
            reader
                .coordinator()
                .unwrap()
                .leaf(&Scope::new("work/a"))
                .is_some()
        );
        drop(reader);
        assert!(stored_label(&path).unwrap().is_some());
        assert!(bytes() == before, "the store is unchanged");
        let names: Vec<_> = std::fs::read_dir(dir.join("mdkg"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| {
                !matches!(
                    n.as_str(),
                    "coord.sqlite" | "coord.sqlite-wal" | "coord.sqlite-shm"
                )
            })
            .collect();
        assert!(names.is_empty(), "no copies or staging: {names:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_only_zero_byte_store_preserves_every_sidecar() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-empty-sidecars-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        std::fs::write(&path, b"").unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            std::fs::write(dir.join(format!("coord.sqlite{suffix}")), suffix).unwrap();
        }
        let snapshot = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), std::fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();
        assert!(Store::open_read_only(&path).unwrap().is_none());
        assert!(Store::open_raw_read_only(&path).unwrap().is_none());
        assert!(stored_label(&path).unwrap().is_none());
        assert!(open_durable(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).is_err());
        assert!(
            crate::mail::read_decisions(&path, 0, 10, &[])
                .unwrap()
                .is_empty()
        );
        assert_eq!(snapshot(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_only_empty_creation_artifacts_are_absent_without_writes() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-empty-create-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        for header in [false, true] {
            drop(Connection::open(&path).unwrap());
            if header {
                let conn = Connection::open(&path).unwrap();
                conn.pragma_update(None, "journal_mode", "WAL").unwrap();
                drop(conn);
            }
            let bytes = std::fs::read(&path).unwrap();
            assert!(Store::open_read_only(&path).unwrap().is_none());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            drop(Store::open_with(&path, 100, Upgrade::Refuse).unwrap());
            assert!(Store::open_read_only(&path).unwrap().is_some());
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn first_creation_survives_sigkill_and_keeps_private_modes() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command as ProcessCommand;
        use std::time::Instant;

        const CHILD: &str = "MDKG_TEST_STORE_CREATION_CHILD";
        const PHASE: &str = "MDKG_TEST_STORE_CREATION_PHASE";
        const PHASES: [(&str, bool); 8] = [
            ("creation_file", false),
            ("creation_opened", false),
            ("creation_schema", false),
            ("before_commit", false),
            ("after_commit", false),
            ("creation_published", true),
            ("creation_wal", true),
            ("creation_complete", true),
        ];
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let phase = std::env::var(PHASE).unwrap();
            let phase = PHASES.iter().find(|(name, _)| *name == phase).unwrap().0;
            PAUSE_CREATION.with(|hook| *hook.borrow_mut() = Some((phase, dir.join("ready"))));
            let _store =
                Store::open_with(&dir.join(".git/mdkg/coord.sqlite"), 100, Upgrade::Refuse)
                    .unwrap();
            injected("creation_complete").unwrap();
            panic!("creation checkpoint was not reached");
        }

        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for (phase, published) in PHASES {
            let dir = std::env::temp_dir()
                .join(format!("mdkg-create-kill-{}", mdkg_format::Uuid7::now()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join(".git/mdkg/coord.sqlite");
            let ready = dir.join("ready");
            let mut child = Child(
                ProcessCommand::new("/bin/sh")
                    .args(["-c", "umask 022; exec \"$@\"", "store-create-test"])
                    .arg(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "store::tests::first_creation_survives_sigkill_and_keeps_private_modes",
                        "--test-threads=1",
                    ])
                    .env(CHILD, &dir)
                    .env(PHASE, phase)
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while !ready.exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "child exited before {phase}"
                );
                assert!(Instant::now() < deadline, "creation did not reach {phase}");
                std::thread::sleep(Duration::from_millis(5));
            }
            for entry in std::fs::read_dir(path.parent().unwrap()).unwrap() {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();
                if metadata.is_dir() {
                    assert_eq!(metadata.mode() & 0o777, 0o700, "{phase}");
                    for child in std::fs::read_dir(entry.path()).unwrap() {
                        assert_eq!(
                            child.unwrap().metadata().unwrap().mode() & 0o777,
                            0o600,
                            "{phase}"
                        );
                    }
                } else {
                    assert_eq!(metadata.mode() & 0o777, 0o600, "{phase}");
                }
            }
            child.0.kill().unwrap();
            assert_eq!(child.0.wait().unwrap().signal(), Some(9));
            assert_eq!(path.exists(), published, "{phase}");
            let before = std::fs::read(&path).ok();
            assert_eq!(
                Store::open_read_only(&path).unwrap().is_some(),
                published,
                "{phase}"
            );
            assert_eq!(std::fs::read(&path).ok(), before, "read modified {phase}");
            let mut store = Store::open_with(&path, 100, Upgrade::Refuse).unwrap();
            store
                .execute(&Command::Register {
                    leaf: Scope::new("work/a"),
                    content: "work/a@0".into(),
                })
                .unwrap();
            let identity = store.store_id().unwrap();
            drop(store);
            let contents = content(&path);
            let store = Store::open_with(&path, 999, Upgrade::Refuse).unwrap();
            assert_eq!(store.ttl(), 100);
            assert_eq!(store.store_id().unwrap(), identity);
            drop(store);
            assert_eq!(content(&path), contents);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn concurrent_first_creators_publish_one_identity_and_keep_existing_data() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-create-race-{}", mdkg_format::Uuid7::now()));
        let path = dir.join("coord.sqlite");
        let barrier = std::sync::Barrier::new(2);
        let identities = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let mut store = Store::open_with(&path, 100, Upgrade::Refuse).unwrap();
                        store
                            .execute(&Command::Register {
                                leaf: Scope::new("work/a"),
                                content: "work/a@0".into(),
                            })
                            .unwrap();
                        store.store_id().unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(identities[0], identities[1]);
        let before = content(&path);
        assert!(Store::open_read_only(&path).unwrap().is_some());
        drop(Store::open_with(&path, 100, Upgrade::Refuse).unwrap());
        assert_eq!(content(&path), before);
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".mdkg-store-")
        }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_only_nonempty_unknown_schemas_are_not_hidden_as_absent() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-unknown-create-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE foreign_data(value TEXT); INSERT INTO foreign_data VALUES ('preserve');",
        )
        .unwrap();
        drop(conn);
        let bytes = std::fs::read(&path).unwrap();
        assert!(Store::open_read_only(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writers_refuse_unknown_nonempty_schemas_without_changing_any_files() {
        for upgrade in [Upgrade::Allow, Upgrade::Refuse] {
            for marker in ["no_meta", "missing", "1", "2", "3", "4"] {
                let dir = std::env::temp_dir()
                    .join(format!("mdkg-foreign-writer-{}", mdkg_format::Uuid7::now()));
                std::fs::create_dir(&dir).unwrap();
                let path = dir.join("coord.sqlite");
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(
                    "CREATE TABLE foreign_data(value TEXT); \
                     INSERT INTO foreign_data VALUES ('preserve'); \
                     CREATE UNIQUE INDEX entries_request ON foreign_data(value);",
                )
                .unwrap();
                if marker != "no_meta" {
                    conn.execute_batch(
                        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
                    )
                    .unwrap();
                    if marker != "missing" {
                        conn.execute("INSERT INTO meta VALUES('schema',?1)", [marker])
                            .unwrap();
                        conn.execute_batch(
                            "INSERT INTO meta VALUES('msg_schema','5'),('ttl','100');",
                        )
                        .unwrap();
                    }
                }
                drop(conn);
                let before = std::fs::read(&path).unwrap();
                let error = Store::open_with(&path, 100, upgrade).unwrap_err();
                assert!(matches!(error, StoreError::BadMeta(_)), "{error}");
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
                let conn =
                    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
                assert!(
                    conn.prepare("SELECT 1 FROM sqlite_master WHERE name='entries_request'")
                        .unwrap()
                        .exists([])
                        .unwrap()
                );
                assert_eq!(
                    conn.query_row("SELECT value FROM foreign_data", [], |r| r
                        .get::<_, String>(0))
                        .unwrap(),
                    "preserve"
                );
                drop(conn);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn read_only_hot_rollback_preserves_data_and_explains_writer_recovery() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-read-recovery-{}", mdkg_format::Uuid7::now()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source.sqlite");
        let crashed = dir.join("crashed.sqlite");
        let mut store = Store::open_with(&source, 100, Upgrade::Refuse).unwrap();
        store
            .execute(&Command::Register {
                leaf: Scope::new("work/a"),
                content: "work/a@0".into(),
            })
            .unwrap();
        drop(store);
        let committed = content(&source);
        std::fs::copy(&source, &crashed).unwrap();
        let mut events =
            crate::events::Reader::open(&crashed, "01a185ac-560e-76e0-91af-2c49dfc29b81", None)
                .unwrap();
        let writer = Connection::open(&source).unwrap();
        writer
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        writer
            .execute_batch("PRAGMA cache_size=1; BEGIN IMMEDIATE;")
            .unwrap();
        for seq in 0..200 {
            writer
                .execute(
                    "INSERT INTO events(seq,event) VALUES (?1,?2)",
                    params![seq, "x".repeat(4000)],
                )
                .unwrap();
        }
        std::fs::copy(&source, &crashed).unwrap();
        let journal = dir.join("crashed.sqlite-journal");
        std::fs::copy(dir.join("source.sqlite-journal"), &journal).unwrap();
        writer.execute_batch("ROLLBACK;").unwrap();
        drop(writer);
        let before = std::fs::read(&crashed).unwrap();
        let rollback = std::fs::read(&journal).unwrap();
        let error = Store::open_read_only(&crashed).unwrap_err();
        assert!(
            matches!(error, StoreError::ReadRecoveryRequired(_)),
            "{error}"
        );
        // Store errors name no command; the CLI's hint does (#17360).
        assert!(!error.to_string().contains("msg inbox"), "{error}");
        assert!(matches!(
            crate::events::head(&crashed).unwrap_err(),
            StoreError::ReadRecoveryRequired(_)
        ));
        for error in [
            crate::events::Reader::open(&crashed, "01a185ac-560e-76e0-91af-2c49dfc29b81", None)
                .unwrap_err(),
            events.poll(1).unwrap_err(),
        ] {
            assert!(matches!(
                error,
                crate::events::ReadError::Store(StoreError::ReadRecoveryRequired(_))
            ));
        }
        assert_eq!(std::fs::read(&crashed).unwrap(), before);
        assert_eq!(std::fs::read(&journal).unwrap(), rollback);
        drop(Store::open_with(&crashed, 100, Upgrade::Refuse).unwrap());
        assert_eq!(content(&crashed), committed);
        assert!(Store::open_read_only(&crashed).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stored_label_reads_a_store_without_writing_it() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-stored-label-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        assert_eq!(stored_label(&path).unwrap(), None);
        assert!(!path.exists(), "a missing store stays missing");
        drop(Store::open_with(&path, 1_000, Upgrade::Allow).unwrap());
        assert_eq!(stored_label(&path).unwrap(), Some(build_label()));
        // A newer build's versions read as they are, for a supervisor to see
        // that the store moved under it.
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
            .unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            stored_label(&path).unwrap(),
            Some(format!("99+msg{}", crate::mail::MAIL_SCHEMA_VERSION))
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

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
        let reader = Store::open_read_only(&path).unwrap().unwrap();
        assert_eq!(wait(&reader.conn), 5_000);
        drop(reader);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_held_read_only_snapshot_allows_a_writer_to_commit() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-status-wal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("coord.sqlite");
        let mut writer = Store::open_with(&path, 100, Upgrade::Allow).unwrap();
        let reader = Store::open_read_only(&path).unwrap().unwrap();
        let tx = reader.conn.unchecked_transaction().unwrap();
        let (seq, before) = load(&tx, reader.ttl).unwrap();
        writer.conn.busy_timeout(Duration::ZERO).unwrap();
        writer
            .execute(&Command::Register {
                leaf: Scope::new("new"),
                content: "new".into(),
            })
            .unwrap();
        assert_eq!(
            load(&tx, reader.ttl).unwrap(),
            (seq, before.clone()),
            "one snapshot while the writer commits"
        );
        tx.finish().unwrap();
        assert_ne!(reader.coordinator().unwrap(), before);
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A schema 3 store, as a 0.7.0 build left it, at `path`.
    fn schema_3_at(path: &Path) {
        drop(Store::open_with(path, 1_000, Upgrade::Allow).expect("new store"));
        let conn = Connection::open(path).expect("conn");
        conn.execute("UPDATE meta SET value = '3' WHERE key = 'schema'", [])
            .expect("schema 3");
        // Without the tables schema 4 adds, so a test sees a failed
        // upgrade leave none behind (Terra's code review #6273, finding 4).
        conn.execute_batch(&schema_4_tables().iter().fold(String::new(), |mut sql, t| {
            sql.push_str(&format!("DROP TABLE {t};"));
            sql
        }))
        .expect("schema 4 tables dropped");
    }

    /// The tables schema 4 adds to a schema 3 store.
    fn schema_4_tables() -> [&'static str; 5] {
        [
            "msg_links",
            "msg_meta",
            "msg_deferred",
            "entries",
            "entry_events",
        ]
    }

    fn tables_present(path: &Path, names: &[&str]) -> Vec<String> {
        let conn = Connection::open(path).expect("conn");
        names
            .iter()
            .filter(|n| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [n],
                    |r| r.get::<_, i64>(0),
                )
                .expect("count")
                    > 0
            })
            .map(|n| n.to_string())
            .collect()
    }

    fn copies_beside(path: &Path) -> Vec<PathBuf> {
        let dir = path.parent().expect("dir");
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().contains(".pre-3+msg5-to-4+msg5"))
            .filter(|p| !p.to_string_lossy().ends_with(".synced"))
            .collect();
        out.sort();
        out
    }

    fn schema_at(path: &Path) -> String {
        let conn = Connection::open(path).expect("conn");
        meta(&conn, "schema").expect("meta").unwrap_or_default()
    }

    /// First creation on a file system with no full barrier (an SMB
    /// mount's F_FULLFSYNC): refused with the override named, nothing
    /// published; with the override, created and reported weak, once
    /// (Terra #16506).
    #[test]
    fn a_first_store_without_a_full_barrier_needs_the_override_and_says_so() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-store-create-barrier-{}",
            mdkg_format::Uuid7::now()
        ));
        std::fs::create_dir(&dir).expect("dir");
        let path = dir.join(".git/mdkg/coord.sqlite");
        NO_BARRIER.with(|f| f.set(true));
        let refused = Store::open_with(&path, 1_000, Upgrade::Refuse);
        NO_BARRIER.with(|f| f.set(false));
        let Err(error) = refused else {
            panic!("no barrier and no override is refused")
        };
        let text = error.to_string();
        assert!(text.contains(PLAIN_FSYNC_ENV), "{text}");
        assert!(text.contains("creation_sync"), "{text}");
        assert!(!path.exists(), "nothing published");
        NO_BARRIER.with(|f| f.set(true));
        PLAIN_FSYNC.with(|f| f.set(true));
        let created = Store::open_with(&path, 1_000, Upgrade::Refuse);
        PLAIN_FSYNC.with(|f| f.set(false));
        NO_BARRIER.with(|f| f.set(false));
        let created = created.expect("created with the override");
        assert!(created.created_weak());
        drop(created);
        let again = Store::open_with(&path, 1_000, Upgrade::Refuse).expect("reopen");
        assert!(!again.created_weak(), "only the creating open reports it");
        drop(again);
        let full = dir.join("full/coord.sqlite");
        let strong = Store::open_with(&full, 1_000, Upgrade::Refuse).expect("full barrier");
        assert!(!strong.created_weak());
        drop(strong);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_store_reports_a_failed_sync_of_its_folder_entry() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-store-parent-sync-{}",
            mdkg_format::Uuid7::now()
        ));
        std::fs::create_dir(&dir).expect("dir");
        let path = dir.join(".git/mdkg/coord.sqlite");
        FAIL_AT.with(|f| f.set(Some("store_parent_sync")));
        let failed = Store::open_with(&path, 1_000, Upgrade::Refuse);
        let Err(error) = failed else {
            panic!("an unsynced folder entry is an error")
        };
        assert!(error.to_string().contains("store_parent_sync"), "{error}");
        // Terra #16575: the store is published now; a retry syncs the
        // entries again before acknowledging anything, so it fails the same
        // way while the sync does.
        assert!(path.exists(), "published before the failed sync");
        for step in ["store_parent_sync", "store_folder_sync"] {
            FAIL_AT.with(|f| f.set(Some(step)));
            let retry = Store::open_with(&path, 1_000, Upgrade::Refuse);
            let Err(error) = retry else {
                panic!("a retry syncs the entries again ({step})")
            };
            assert!(error.to_string().contains(step), "{error}");
        }
        FAIL_AT.with(|f| f.set(None));
        drop(Store::open_with(&path, 1_000, Upgrade::Refuse).expect("published store opens"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_upgrade_stops_when_its_copy_or_its_folder_cant_be_made_durable() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-copy-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        schema_3_at(&path);
        FAIL_AT.with(|f| f.set(Some("copy_folder_sync")));
        let failed = Store::open_with(&path, 1_000, Upgrade::Allow);
        FAIL_AT.with(|f| f.set(None));
        assert!(failed.is_err());
        assert_eq!(schema_at(&path), "3", "nothing upgraded");
        drop(Store::open_with(&path, 1_000, Upgrade::Allow).expect("retry"));
        assert_eq!(schema_at(&path), "4");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sync_the_file_system_doesnt_offer_stops_the_upgrade() {
        // The copy's sync and its folder's, each refused as unsupported (no
        // full barrier): without the override, neither settles for a plain
        // fsync, and the upgrade doesn't start.
        for step in ["copy_sync", "copy_folder_barrier", "copy_folder_published"] {
            let dir = std::env::temp_dir().join(format!(
                "mdkg-store-unsupported-{step}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("dir");
            let path = dir.join("coord.sqlite");
            schema_3_at(&path);
            FAIL_AT.with(|f| f.set(Some(step)));
            let failed = Store::open_with(&path, 1_000, Upgrade::Allow);
            FAIL_AT.with(|f| f.set(None));
            assert!(failed.is_err(), "{step}");
            assert_eq!(schema_at(&path), "3", "{step}: nothing upgraded");
            // No copy is left at any step, published or partial (Opus's
            // #6817, minor 2; Terra's #6874, finding 2).
            assert!(copies_beside(&path).is_empty(), "{step}: no copy left");
            drop(Store::open_with(&path, 1_000, Upgrade::Allow).expect("retry"));
            assert_eq!(schema_at(&path), "4");
            let conn = Connection::open(&path).expect("conn");
            assert_eq!(meta(&conn, "upgrade_barrier").expect("meta"), None);
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn the_plain_fsync_override_upgrades_and_records_the_weaker_guarantee() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        schema_3_at(&path);
        FAIL_AT.with(|f| f.set(Some("copy_sync")));
        PLAIN_FSYNC.with(|f| f.set(true));
        let opened = Store::open_with(&path, 1_000, Upgrade::Allow);
        PLAIN_FSYNC.with(|f| f.set(false));
        FAIL_AT.with(|f| f.set(None));
        let opened = opened.expect("upgraded with the override");
        // The open says so, for the CLI's upgrade notice.
        assert!(opened.backup_weak());
        drop(opened);
        assert_eq!(schema_at(&path), "4");
        let conn = Connection::open(&path).expect("conn");
        assert_eq!(
            meta(&conn, "upgrade_barrier").expect("meta").as_deref(),
            Some("fsync")
        );
        drop(conn);
        assert_eq!(copies_beside(&path).len(), 1);
        // So does the copy itself, as `store snapshots` lists it.
        let listed = backups(&path).expect("backups");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].barrier.as_deref(), Some("fsync"));
        // A snapshot under the override is marked; one with the full
        // barrier isn't. Restoring the weak copy says both the copy it
        // read and the one it took first.
        FAIL_AT.with(|f| f.set(Some("copy_sync")));
        PLAIN_FSYNC.with(|f| f.set(true));
        let weak = snapshot(&path, "weak").expect("weak snapshot");
        PLAIN_FSYNC.with(|f| f.set(false));
        FAIL_AT.with(|f| f.set(None));
        let full = snapshot(&path, "full").expect("full snapshot");
        let by_path = |p: &Path| {
            backups(&path)
                .expect("backups")
                .into_iter()
                .find(|b| b.path == p)
                .expect("listed")
                .barrier
        };
        assert_eq!(by_path(&weak).as_deref(), Some("fsync"));
        assert_eq!(by_path(&full), None);
        // The full copy is still marked in itself: only its proof, written
        // once the final folder barrier held, lifts the mark, and a proof
        // of another file doesn't (Terra's #7598, finding 1).
        let marked = Connection::open(&full).expect("conn");
        assert_eq!(
            meta(&marked, "backup_barrier").expect("meta").as_deref(),
            Some("fsync")
        );
        drop(marked);
        let proof = std::fs::read_to_string(proof_path(&full)).expect("proof");
        std::fs::write(proof_path(&full), proof.replace("len=", "len=1")).expect("tamper");
        assert_eq!(by_path(&full).as_deref(), Some("fsync"));
        std::fs::write(proof_path(&full), &proof).expect("restore proof");
        assert_eq!(by_path(&full), None);
        let restored = restore(&path, &weak).expect("restore");
        assert_eq!(restored.from_barrier.as_deref(), Some("fsync"));
        assert!(
            !restored.previous_weak,
            "the copy first took the full barrier"
        );
        // The weak copy's marker doesn't stay in the live store, and a
        // full-barrier copy taken after it isn't labelled weak (Opus's code
        // review #6264, finding 1; Terra's #6273, finding 3).
        let conn = Connection::open(&path).expect("conn");
        assert_eq!(meta(&conn, "backup_barrier").expect("meta"), None);
        drop(conn);
        let after = snapshot(&path, "after").expect("full snapshot");
        assert_eq!(by_path(&after), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Terra's code review #6273, finding 1: a copy whose folder only a
    /// plain fsync could sync, before its publish or after, is marked.
    #[test]
    fn a_folder_that_settles_for_a_plain_fsync_marks_the_copy() {
        for step in ["copy_folder_barrier", "copy_folder_published"] {
            let dir = std::env::temp_dir().join(format!(
                "mdkg-store-weak-folder-{step}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("dir");
            let path = dir.join("coord.sqlite");
            drop(Store::open_with(&path, 1_000, Upgrade::Allow).expect("store"));
            FAIL_AT.with(|f| f.set(Some(step)));
            PLAIN_FSYNC.with(|f| f.set(true));
            let copy = snapshot(&path, "weak");
            PLAIN_FSYNC.with(|f| f.set(false));
            FAIL_AT.with(|f| f.set(None));
            // Weak before the publish or after it: the copy keeps its own
            // mark, with no proof beside it (Terra's #7598, finding 1,
            // with the orchestrator's call).
            let copy = copy.expect("snapshot");
            let listed = backups(&path).expect("backups");
            let found = listed.iter().find(|b| b.path == copy).expect("listed");
            assert_eq!(found.barrier.as_deref(), Some("fsync"), "{step}");
            assert!(!proof_path(&copy).exists(), "{step}");
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// Terra's code review #6273, finding 2: a restore whose write fails
    /// after a weak first copy says that copy is weak.
    #[test]
    fn a_failed_restore_says_its_first_copy_is_weak() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-store-weak-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        drop(Store::open_with(&path, 1_000, Upgrade::Allow).expect("store"));
        let copy = snapshot(&path, "full").expect("snapshot");
        NO_BARRIER.with(|f| f.set(true));
        PLAIN_FSYNC.with(|f| f.set(true));
        FAIL_AT.with(|f| f.set(Some("restore_write")));
        let failed = restore(&path, &copy);
        FAIL_AT.with(|f| f.set(None));
        PLAIN_FSYNC.with(|f| f.set(false));
        NO_BARRIER.with(|f| f.set(false));
        match failed {
            Err(StoreError::Restore(m)) => {
                assert!(m.contains("the copy failed"), "{m}");
                assert!(m.contains("plain fsync only"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_ambiguous_commit_keeps_the_copy_and_a_clean_failure_removes_it() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-ambiguous-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        schema_3_at(&path);
        // Before the commit: the store is as it was, so the copy goes.
        FAIL_AT.with(|f| f.set(Some("before_commit")));
        assert!(Store::open_with(&path, 1_000, Upgrade::Allow).is_err());
        FAIL_AT.with(|f| f.set(None));
        assert_eq!(schema_at(&path), "3");
        assert!(copies_beside(&path).is_empty());
        // Its proof went with it, and the retry's copy, under the same
        // name, reads fully synced (Opus's #8408; Terra's #8466).
        let names = |p: &Path| -> Vec<String> {
            std::fs::read_dir(p.parent().expect("dir"))
                .expect("dir")
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".synced"))
                .collect()
        };
        assert!(names(&path).is_empty(), "no orphan proof");
        let retried = Store::open_with(&path, 1_000, Upgrade::Allow).expect("retry");
        assert!(!retried.backup_weak());
        drop(retried);
        let listed = backups(&path).expect("backups");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].barrier, None, "fully synced");
        // A proof of another generation never vouches for a copy.
        let copy = listed[0].path.clone();
        let proof = std::fs::read_to_string(proof_path(&copy)).expect("proof");
        let generation = proof
            .split(' ')
            .find_map(|w| w.strip_prefix("gen="))
            .expect("gen")
            .to_string();
        std::fs::write(
            proof_path(&copy),
            proof.replace(&generation, "0123456789abcdef"),
        )
        .expect("other generation");
        assert_eq!(
            backups(&path).expect("backups")[0].barrier.as_deref(),
            Some("fsync")
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("dir");
        schema_3_at(&path);
        // After the commit frame: the store may read back upgraded, so the
        // copy stays.
        FAIL_AT.with(|f| f.set(Some("after_commit")));
        assert!(Store::open_with(&path, 1_000, Upgrade::Allow).is_err());
        FAIL_AT.with(|f| f.set(None));
        assert_eq!(schema_at(&path), "4");
        assert_eq!(copies_beside(&path).len(), 1, "the recovery copy stays");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_proofs_survive_device_changes_without_trusting_other_copy_metadata() {
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!(
            "mdkg-store-proof-reboot-{}",
            mdkg_format::Uuid7::now()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        drop(Store::open(&path, 1_000).expect("store"));
        let copy = snapshot(&path, "reboot").expect("snapshot");
        let conn = Connection::open(&copy).expect("copy");
        let generation = meta(&conn, "backup_generation").unwrap().unwrap();
        drop(conn);
        let file = std::fs::metadata(&copy).unwrap();
        let legacy = format!(
            "mdkg-copy-synced gen={generation} dev={} ino={} len={} mtime={}.{}\n",
            file.dev() ^ 1,
            file.ino(),
            file.len(),
            file.mtime(),
            file.mtime_nsec()
        );
        std::fs::write(proof_path(&copy), &legacy).unwrap();
        let barrier = || {
            backups(&path)
                .unwrap()
                .into_iter()
                .find(|b| b.path == copy)
                .unwrap()
                .barrier
        };
        assert_eq!(
            barrier(),
            None,
            "reboot must not weaken a fully synced copy"
        );
        assert_eq!(
            std::fs::read_to_string(proof_path(&copy)).unwrap(),
            legacy,
            "read-only listing must not rewrite legacy proofs"
        );
        let stable = proof_of(&copy, &generation).unwrap();
        assert!(
            !stable.contains(" dev="),
            "new proofs must not persist device numbers"
        );
        for proof in [&legacy, &stable] {
            for altered in [
                proof.replace(&generation, "other-generation"),
                proof.replace(
                    &format!("ino={}", file.ino()),
                    &format!("ino={}", file.ino() ^ 1),
                ),
                proof.replace(
                    &format!("len={}", file.len()),
                    &format!("len={}", file.len() + 1),
                ),
                proof.replace("mtime=", "mtime=1"),
            ] {
                std::fs::write(proof_path(&copy), altered).unwrap();
                assert_eq!(barrier().as_deref(), Some("fsync"));
            }
            std::fs::write(proof_path(&copy), proof).unwrap();
            assert_eq!(barrier(), None);
        }
        std::fs::write(proof_path(&copy), legacy).unwrap();
        assert_eq!(
            restore(&path, &copy).unwrap().from_barrier,
            None,
            "restore must recognize the old proof after a device change"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_commit_keeps_the_copy_even_when_the_store_reads_back_unchanged() {
        let dir =
            std::env::temp_dir().join(format!("mdkg-store-commit-failed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        schema_3_at(&path);
        FAIL_AT.with(|f| f.set(Some("commit_failed")));
        assert!(Store::open_with(&path, 1_000, Upgrade::Allow).is_err());
        FAIL_AT.with(|f| f.set(None));
        assert_eq!(schema_at(&path), "3", "the readback shows schema 3");
        assert_eq!(copies_beside(&path).len(), 1, "yet the copy stays");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_commit_sqlite_itself_refuses_rolls_back_and_keeps_the_copy() {
        let dir = std::env::temp_dir().join(format!("mdkg-store-fk-commit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("coord.sqlite");
        schema_3_at(&path);
        // A deferred foreign key fails at COMMIT: SQLite's own error, not
        // an injected one before the call.
        FAIL_AT.with(|f| f.set(Some("deferred_fk")));
        let e = Store::open_with(&path, 1_000, Upgrade::Allow).expect_err("COMMIT fails");
        FAIL_AT.with(|f| f.set(None));
        assert!(e.to_string().contains("FOREIGN KEY"), "{e}");
        // Rolled back whole: schema 3, and nothing the transaction wrote.
        assert_eq!(schema_at(&path), "3");
        let conn = Connection::open(&path).expect("open");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('fk_parent', 'fk_child')",
                [],
                |r| r.get(0),
            )
            .expect("tables");
        assert_eq!(tables, 0);
        drop(conn);
        // Nor any table schema 4 adds (the fixture has none of them).
        assert_eq!(
            tables_present(&path, &schema_4_tables()),
            Vec::<String>::new()
        );
        // An error from COMMIT counts as unknown: the copy stays.
        assert_eq!(copies_beside(&path).len(), 1, "the copy stays");
        // And the next open upgrades as usual.
        Store::open_with(&path, 1_000, Upgrade::Allow).expect("upgrades");
        assert_eq!(schema_at(&path), SCHEMA_VERSION);
        assert_eq!(tables_present(&path, &schema_4_tables()).len(), 5);
        std::fs::remove_dir_all(&dir).ok();
    }
}
