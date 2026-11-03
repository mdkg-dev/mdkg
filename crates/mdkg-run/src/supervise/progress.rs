//! Durable watch progress. One record per agent, independent of lifetime state.
//!
//! Callers keep the stable sidecar lock through a watch and drain all work
//! before releasing it for exec. An interrupted admission is never replayed.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, FileExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mdkg_format::program::BIN;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::Progress;
use crate::{RunError, state};

const SCHEMA: &str = "mdkg.supervisor-progress.v3";
const PREVIOUS_SCHEMA: &str = "mdkg.supervisor-progress.v2";
const LEGACY_SCHEMA: &str = "mdkg.supervisor-progress.v1";
const CAP: u64 = 256 * 1024;
// Room for one maximum-sized admission, including 1000 public message IDs
// of 128 bytes each, while keeping ample headroom under the full file cap.
const INLINE_CAP: usize = 144 * 1024;
const CODE: &str = "run.progress_invalid";
const INTERRUPTED: &str = "run.turn_interrupted";
// Dot-prefixed names cannot be agent turn-log directories.
const STORE_ID: &str = ".store-id";

/// The legacy device number can change across a reboot or remount. A nonce
/// separates independent stores; the inode also rejects a copied common dir
/// carrying the same nonce. An inode-preserving restore remains indistinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum StoreIdentity {
    Legacy([u64; 2]),
    Stable(StableStore),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StableStore {
    #[serde(
        serialize_with = "serialize_uuid",
        deserialize_with = "deserialize_uuid"
    )]
    id: Uuid,
    inode: u64,
}

fn serialize_uuid<S: serde::Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

fn deserialize_uuid<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
    let text = String::deserialize(deserializer)?;
    Uuid::parse_str(&text).map_err(serde::de::Error::custom)
}

impl StoreIdentity {
    fn inode(self) -> u64 {
        match self {
            Self::Legacy([_, inode]) => inode,
            Self::Stable(store) => store.inode,
        }
    }

    fn matches(self, current: Self) -> bool {
        match self {
            Self::Legacy(_) => self.inode() == current.inode(),
            Self::Stable(_) => self == current,
        }
    }
}

/// Metadata about an admitted turn, never its prompt, reply, or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub id: String,
    /// Agent lifetime turn number, distinct from the watch's `Progress::ran`.
    pub number: u64,
    /// None records an unborn checkout; omission is still corruption.
    #[serde(deserialize_with = "required_nullable")]
    pub start_head: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub session: Option<String>,
    pub mail_ids: Vec<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub cursor: Option<u64>,
}

/// A queue dispatch admission, never a completed model turn or synthetic mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receiver {
    pub attempt_id: String,
    pub session: String,
    pub through: u64,
}

/// A snapshot is safe for non-blocking doctor/status reads. The lock, not a
/// PID in this record, decides whether a writer owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    schema: String,
    /// Run-store nonce and common-directory inode; legacy records have dev/ino.
    store: StoreIdentity,
    pub graph_id: String,
    pub agent: String,
    pub watch_id: String,
    /// Hash of the launcher's effective invocation, excluding its build.
    pub invocation: String,
    pub revision: u64,
    #[serde(deserialize_with = "required_progress")]
    pub progress: Progress,
    #[serde(deserialize_with = "required_nullable")]
    pub current: Option<Turn>,
    #[serde(deserialize_with = "required_nullable")]
    pub current_receiver: Option<Receiver>,
    /// Explicitly resumed turns, retained until a turn consumes their context.
    pub recovery: Vec<Turn>,
    pub receiver_recovery: Vec<Receiver>,
    /// Explicit recovery permits an edited invocation, while an unchanged
    /// invocation must continue the saved budget instead of starting over.
    pub resume_pending: bool,
    /// Latest full archived context. Its record retains any earlier reference.
    /// Store only the hash; paths are derived from it inside the progress folder.
    #[serde(deserialize_with = "required_nullable")]
    pub context_archive: Option<String>,
    pub finished: bool,
}

/// The old record cannot contain a receiver admission. Decode it strictly
/// before adding empty receiver fields; never default a missing v2 admission.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRecord {
    schema: String,
    store: [u64; 2],
    graph_id: String,
    agent: String,
    watch_id: String,
    invocation: String,
    revision: u64,
    #[serde(deserialize_with = "required_progress")]
    progress: Progress,
    #[serde(deserialize_with = "required_nullable")]
    current: Option<Turn>,
    recovery: Vec<Turn>,
    resume_pending: bool,
    #[serde(deserialize_with = "required_nullable")]
    context_archive: Option<String>,
    finished: bool,
}

fn decode(bytes: &[u8]) -> Result<Record, RunError> {
    #[derive(Deserialize)]
    struct Version {
        schema: String,
    }
    let version: Version =
        serde_json::from_slice(bytes).map_err(|_| invalid("corrupt supervisor progress record"))?;
    if version.schema != LEGACY_SCHEMA {
        return serde_json::from_slice(bytes)
            .map_err(|_| invalid("corrupt supervisor progress record"));
    }
    let old: LegacyRecord = serde_json::from_slice(bytes)
        .map_err(|_| invalid("corrupt legacy supervisor progress record"))?;
    debug_assert_eq!(old.schema, LEGACY_SCHEMA);
    Ok(Record {
        schema: PREVIOUS_SCHEMA.into(),
        store: StoreIdentity::Legacy(old.store),
        graph_id: old.graph_id,
        agent: old.agent,
        watch_id: old.watch_id,
        invocation: old.invocation,
        revision: old.revision,
        progress: old.progress,
        current: old.current,
        current_receiver: None,
        recovery: old.recovery,
        receiver_recovery: Vec::new(),
        resume_pending: old.resume_pending,
        context_archive: old.context_archive,
        finished: old.finished,
    })
}

// Missing and explicit null have different meanings in a durable record. In
// particular, an omitted `current` must never authorize a fresh admission.
fn required_nullable<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

fn required_progress<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Progress, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        ran: u64,
        #[serde(deserialize_with = "required_nullable")]
        seen: Option<u64>,
        check_inbox: bool,
    }
    let fields = Fields::deserialize(deserializer)?;
    Ok(Progress {
        ran: fields.ran,
        seen: fields.seen,
        check_inbox: fields.check_inbox,
    })
}

fn invalid(message: impl Into<String>) -> RunError {
    RunError::failure(CODE, message)
}

fn hex(s: &str, lengths: &[usize]) -> bool {
    lengths.contains(&s.len()) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn valid_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|id| !id.is_nil())
}

fn valid_mail_id(id: &str) -> bool {
    // Public message IDs include imported launcher replies (import:<digest>),
    // not only UUIDs. Rooms are contexts, never message IDs. These are IDs of
    // already-admitted messages: credential-shape policy stays at mail ingress.
    !id.is_empty()
        && id.len() <= 128
        && !id
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("room:"))
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn check_turn(turn: &Turn) -> Result<(), RunError> {
    if !valid_id(&turn.id)
        || turn.number == 0
        || turn.start_head.as_ref().is_some_and(|head| {
            !hex(head, &[40, 64]) || head.bytes().any(|b| b.is_ascii_uppercase())
        })
        || turn.mail_ids.len() > 1000
        || turn.mail_ids.iter().any(|id| !valid_mail_id(id))
        || turn
            .session
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
    {
        return Err(invalid("invalid interrupted-turn metadata"));
    }
    Ok(())
}

fn check_receiver(receiver: &Receiver) -> Result<(), RunError> {
    if !valid_id(&receiver.attempt_id) || !valid_id(&receiver.session) || receiver.through == 0 {
        return Err(invalid("invalid interrupted-receiver metadata"));
    }
    Ok(())
}

impl Record {
    fn next_revision(&mut self) -> Result<(), RunError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("progress revision exhausted"))?;
        Ok(())
    }

    fn resume_turn(&mut self) -> Result<Option<Turn>, RunError> {
        let Some(turn) = self.current.take() else {
            return Ok(None);
        };
        self.progress.ran = self
            .progress
            .ran
            .checked_add(1)
            .ok_or_else(|| invalid("turn budget exhausted"))?;
        self.progress.seen = self.progress.seen.max(turn.cursor);
        self.progress.check_inbox = true;
        self.recovery.push(turn.clone());
        Ok(Some(turn))
    }

    fn resume_receiver(&mut self) -> Result<Option<Receiver>, RunError> {
        let Some(receiver) = self.current_receiver.take() else {
            return Ok(None);
        };
        self.progress.ran = self
            .progress
            .ran
            .checked_add(1)
            .ok_or_else(|| invalid("dispatch budget exhausted"))?;
        self.progress.seen = self.progress.seen.max(Some(receiver.through));
        self.progress.check_inbox = true;
        self.receiver_recovery.push(receiver.clone());
        Ok(Some(receiver))
    }

    fn encode(&self) -> Result<Vec<u8>, RunError> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| invalid("can't encode supervisor progress"))?;
        if bytes.len() as u64 > CAP {
            return Err(invalid("supervisor progress exceeds its size limit"));
        }
        Ok(bytes)
    }

    fn require_inline_capacity(&self) -> Result<(), RunError> {
        let bytes = serde_json::to_vec(&(&self.recovery, &self.receiver_recovery))
            .map_err(|_| invalid("can't encode supervisor recovery context"))?;
        if bytes.len() > INLINE_CAP {
            return Err(invalid("inline recovery context exceeds its size limit; archive it before another turn")
                .with_hint(format!("inspect the saved context, then run {BIN} agents resume {} --archive-context; the full record is preserved for the next prompt", self.agent)));
        }
        Ok(())
    }

    fn require_continuation_capacity(&self) -> Result<(), RunError> {
        if self.resume_pending {
            let mut continued = self.clone();
            continued.next_revision()?;
            continued.finished = false;
            continued.resume_pending = false;
            continued.encode().map_err(|error| error.with_hint(format!(
                "preserve the saved context with {BIN} agents resume {} --archive-context before continuing", self.agent
            )))?;
        }
        Ok(())
    }

    fn validate(&self, graph_id: Uuid, agent: &str) -> Result<(), RunError> {
        let version_matches_store = match self.store {
            StoreIdentity::Legacy(_) => self.schema == PREVIOUS_SCHEMA,
            StoreIdentity::Stable(_) => self.schema == SCHEMA,
        };
        if !version_matches_store
            || matches!(self.store, StoreIdentity::Stable(store) if store.id.is_nil())
            || graph_id.is_nil()
            || self.graph_id != graph_id.to_string()
            || self.agent != agent
            || !valid_id(&self.watch_id)
            || !hex(&self.invocation, &[64])
            || self.revision == 0
            || self
                .context_archive
                .as_ref()
                .is_some_and(|hash| !archive_hash(hash))
            || (self.finished && (self.current.is_some() || self.current_receiver.is_some()))
            || (self.current.is_some() && self.current_receiver.is_some())
            || (self.resume_pending && !self.finished)
        {
            return Err(invalid("corrupt or foreign supervisor progress record"));
        }
        state::check_name(&self.agent)?;
        for turn in self.current.iter().chain(&self.recovery) {
            check_turn(turn)?;
        }
        for receiver in self.current_receiver.iter().chain(&self.receiver_recovery) {
            check_receiver(receiver)?;
        }
        Ok(())
    }

    fn check_context_archive(&self, path: &Path) -> Result<Option<PathBuf>, RunError> {
        let Some(hash) = &self.context_archive else {
            return Ok(None);
        };
        let archive = archive_path(path, hash);
        self.check_archive_bytes(&archive, hash).map_err(|error| {
            error.with_hint(format!(
                "restore the original recovery archive at {} as a private, owned regular file with one link before continuing; keep the progress record unchanged",
                archive.display()
            ))
        })?;
        Ok(Some(archive))
    }

    fn check_archive_bytes(&self, archive: &Path, hash: &str) -> Result<(), RunError> {
        let bytes =
            read_bytes(archive)?.ok_or_else(|| invalid("recovery context archive is missing"))?;
        if format!("{:x}", Sha256::digest(&bytes)) != hash {
            return Err(invalid("recovery context archive does not match its hash"));
        }
        let old = decode(&bytes).map_err(|_| invalid("corrupt recovery context archive"))?;
        old.validate(
            self.graph_id
                .parse()
                .map_err(|_| invalid("invalid graph identity"))?,
            &self.agent,
        )?;
        Ok(())
    }

    pub fn store_id(&self) -> String {
        store_id(self.store)
    }

    fn check_store(&self, current: StoreIdentity) -> Result<(), RunError> {
        if !self.store.matches(current) {
            return Err(invalid(format!(
                "foreign supervisor progress store: recorded {}, current {}",
                self.store_id(), store_id(current)
            ))
            .with_hint(format!(
                "inspect the saved turn and both store identities; to retain the old record and resume explicitly, run {BIN} agents resume {} --store {}",
                self.agent, store_id(current)
            )));
        }
        Ok(())
    }

    pub fn require_ready(&self) -> Result<(), RunError> {
        if let Some(turn) = &self.current {
            return Err(RunError::failure(
                INTERRUPTED,
                format!(
                    "{} has an interrupted turn {}; automatic execution is refused",
                    self.agent, turn.id
                ),
            )
            .with_hint(format!(
                "inspect the turn with {BIN} agents status, then run {BIN} agents resume {}",
                self.agent
            )));
        }
        if let Some(receiver) = &self.current_receiver {
            return Err(RunError::failure(INTERRUPTED, format!(
                "{} has an interrupted receiver dispatch {}; automatic dispatch is refused",
                self.agent, receiver.attempt_id
            )).with_hint(format!(
                "inspect the dispatch with {BIN} agents status, then run {BIN} agents resume {}; the dispatch is charged conservatively and is not resent",
                self.agent
            )));
        }
        Ok(())
    }
}

fn archive_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn archive_path(record: &Path, hash: &str) -> PathBuf {
    // The digest covers the agent and graph as well as its context. Keep the
    // filename bounded even when the encoded agent name is near NAME_MAX.
    record.with_file_name(format!("recovery-{hash}"))
}

fn folder(run_dir: &Path, create: bool) -> Result<Option<PathBuf>, RunError> {
    folder_with_sync(run_dir, create, |path| File::open(path)?.sync_all())
}

fn folder_with_sync(
    run_dir: &Path,
    create: bool,
    mut sync: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<Option<PathBuf>, RunError> {
    let state = run_dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "mdkg"));
    let common = state.and_then(Path::parent).filter(|_| {
        run_dir.is_absolute()
            && run_dir.file_name().is_some_and(|n| n == "run")
            && !run_dir.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
    });
    let common =
        common.ok_or_else(|| invalid("progress requires the git common mdkg/run folder"))?;
    let state = common.join("mdkg");
    let path = run_dir.join("progress");
    for (i, p) in [common, state.as_path(), run_dir, path.as_path()]
        .into_iter()
        .enumerate()
    {
        let metadata = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && i > 0 => {
                if !create {
                    return Ok(None);
                }
                // Progress is the first writer for a receiver without saved
                // launcher state. Create each private parent only after the
                // existing parent has passed the ownership/topology checks.
                match fs::DirBuilder::new().mode(0o700).create(p) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(RunError::io("can't create progress folder", e)),
                }
                fs::symlink_metadata(p)
                    .map_err(|e| RunError::io("can't inspect progress folder", e))?
            }
            Err(e) => return Err(RunError::io("can't inspect progress folder", e)),
        };
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid(format!(
                "unsafe supervisor progress folder: {}",
                p.display()
            ))
            .with_hint("supervisor progress requires private directories, including Git's common directory; use a private clone instead of a group-writable repository"));
        }
    }
    // A prior creator may have died (or failed its sync) after any mkdir.
    // Seeing these directories does not prove their parent entries are durable.
    if create {
        for parent in [common, state.as_path(), run_dir, path.as_path()] {
            sync(parent).map_err(|e| RunError::io("can't sync the progress folder", e))?;
        }
    }
    Ok(Some(path))
}

fn checked_file(file: &File) -> Result<(), RunError> {
    let m = file
        .metadata()
        .map_err(|e| RunError::io("can't inspect progress file", e))?;
    if !m.is_file()
        || m.nlink() != 1
        || m.len() > CAP
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(invalid("unsafe supervisor progress file"));
    }
    Ok(())
}

/// The lock file holds the PID of the writer that holds it, so a writer it
/// refuses can name it. It's a record, not proof: an older build holds the
/// lock without writing one, and a writer that crashed leaves its own.
/// Best effort, since it's only for that message. Status snapshots only
/// read the lock and never write it.
fn record_holder(lock: &File) {
    let _ = lock
        .set_len(0)
        .and_then(|()| lock.write_all_at(format!("{}\n", std::process::id()).as_bytes(), 0));
}

/// The PID last recorded in the lock: digits ending in a newline, so a
/// torn or partial write names nobody. An older build leaves it empty.
fn holder(lock: &File) -> Option<u32> {
    let mut buf = [0; 16];
    let read = lock.read_at(&mut buf, 0).ok()?;
    let digits = std::str::from_utf8(&buf[..read]).ok()?.strip_suffix('\n')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|pid| *pid > 0)
}

/// How a busy refusal names the recorded holder, saying so when no such
/// process is running: an older build may hold the lock under a PID
/// recorded earlier.
fn holder_note(pid: u32) -> String {
    let gone = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|p| rustix::process::test_kill_process(p) == Err(rustix::io::Errno::SRCH));
    if gone {
        format!(": the last recorded holder, process {pid}, isn't running")
    } else {
        format!(": the last recorded holder is process {pid}")
    }
}

fn flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

fn physical_identity(run_dir: &Path) -> Result<[u64; 2], RunError> {
    let common = run_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("progress requires the git common mdkg/run folder"))?;
    let metadata = fs::symlink_metadata(common)
        .map_err(|e| RunError::io("can't inspect progress store identity", e))?;
    Ok([metadata.dev(), metadata.ino()])
}

fn store_identity(run_dir: &Path, create: bool) -> Result<StoreIdentity, RunError> {
    let physical = physical_identity(run_dir)?;
    let id = if create {
        Some(ensure_store_id(run_dir)?)
    } else {
        read_store_id(run_dir)?
    };
    Ok(match id {
        Some(id) => StoreIdentity::Stable(StableStore {
            id,
            inode: physical[1],
        }),
        None => StoreIdentity::Legacy(physical),
    })
}

fn store_id(store: StoreIdentity) -> String {
    match store {
        StoreIdentity::Legacy([device, inode]) => format!("{device:016x}:{inode:016x}"),
        StoreIdentity::Stable(store) => format!("{}:{:016x}", store.id, store.inode),
    }
}

fn read_store_id(run_dir: &Path) -> Result<Option<Uuid>, RunError> {
    let Some(bytes) = read_bytes(&run_dir.join(STORE_ID))? else {
        return Ok(None);
    };
    let id = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .and_then(|text| Uuid::parse_str(text).ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(|| invalid("invalid supervisor progress store identity"))?;
    Ok(Some(id))
}

fn ensure_store_id(run_dir: &Path) -> Result<Uuid, RunError> {
    if read_store_id(run_dir)?.is_none() {
        let temporary = run_dir.join(format!(".store-id.{}.tmp", Uuid::now_v7()));
        let publish = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(flags())
                .open(&temporary)?;
            writeln!(file, "{}", Uuid::now_v7())?;
            file.sync_all()?;
            // Other agents initialize the same store under different locks.
            // Readers must see a complete nonce, and the first publisher wins.
            match rustix::fs::renameat_with(
                rustix::fs::CWD,
                &temporary,
                rustix::fs::CWD,
                run_dir.join(STORE_ID),
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
                Err(error) => Err(error.into()),
            }
        })();
        let _ = fs::remove_file(&temporary);
        publish.map_err(|e| RunError::io("can't publish progress store identity", e))?;
    }
    let id = read_store_id(run_dir)?
        .ok_or_else(|| invalid("supervisor progress store identity disappeared"))?;
    // Also heal an earlier publisher's failed directory barrier before any
    // record is durably bound to this nonce.
    sync_saved(&run_dir.join(STORE_ID), |_| Ok(()))
        .map_err(|e| RunError::io("can't sync progress store identity", e))?;
    Ok(id)
}

// Only a stable lock owner may remove its own abandoned write files. Other
// agents, unexpected files, links and directories are left untouched.
fn clean_temps(path: &Path) -> Result<(), RunError> {
    let dir = path.parent().expect("progress directory");
    let prefix = format!(
        "{}.",
        path.file_stem().expect("record name").to_string_lossy()
    );
    for entry in fs::read_dir(dir).map_err(|e| RunError::io("can't read progress folder", e))? {
        let entry = entry.map_err(|e| RunError::io("can't read progress entry", e))?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|s| s.strip_prefix(&prefix))
            .and_then(|s| s.strip_suffix(".tmp"))
            .filter(|s| valid_id(s))
        else {
            continue;
        };
        let temp = dir.join(format!("{prefix}{id}.tmp"));
        let metadata = fs::symlink_metadata(&temp)
            .map_err(|e| RunError::io("can't inspect progress temporary file", e))?;
        if metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == rustix::process::geteuid().as_raw()
        {
            fs::remove_file(temp)
                .map_err(|e| RunError::io("can't remove progress temporary file", e))?;
        }
    }
    Ok(())
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, RunError> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags())
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't read supervisor progress", e)),
    };
    checked_file(&file)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io("can't read supervisor progress", e))?;
    if bytes.len() as u64 > CAP {
        return Err(invalid("supervisor progress exceeds its size limit"));
    }
    Ok(Some(bytes))
}

fn read_record(path: &Path, graph_id: Uuid, agent: &str) -> Result<Option<Record>, RunError> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    let record = decode(&bytes)
        .map_err(|_| invalid("corrupt supervisor progress JSON; preserved for inspection"))?;
    record.validate(graph_id, agent)?;
    Ok(Some(record))
}

fn read(path: &Path, graph_id: Uuid, agent: &str) -> Result<Option<Record>, RunError> {
    let record = read_record(path, graph_id, agent)?;
    if let Some(record) = &record {
        record.check_context_archive(path)?;
    }
    Ok(record)
}

/// Progress-only actors remain discoverable even before runtime state exists.
/// Validate the private hierarchy before enumerating display candidates.
pub fn actors(run_dir: &Path) -> Result<Vec<String>, RunError> {
    let Some(folder) = folder(run_dir, false)? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in fs::read_dir(folder).map_err(|e| RunError::io("can't list progress", e))? {
        let entry = entry.map_err(|e| RunError::io("can't list progress", e))?;
        if let Some(stem) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.strip_suffix(".json"))
        {
            let name = stem.replace("%3A", ":");
            state::check_name(&name)?;
            if state::file_name(&name) != stem {
                return Err(invalid("invalid progress filename"));
            }
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Non-mutating and non-blocking, including when another process owns the watch.
pub fn inspect(run_dir: &Path, graph_id: Uuid, agent: &str) -> Result<Option<Record>, RunError> {
    let Some(view) = inspect_recovery(run_dir, graph_id, agent)? else {
        return Ok(None);
    };
    view.record.check_store(view.store)?;
    if let Some(error) = view.archive_problem {
        return Err(error);
    }
    Ok(Some(view.record))
}

/// Display-only inspection may expose a foreign store's metadata, but never
/// authorizes its use. Archive damage is reported beside the saved metadata;
/// corrupt or unrelated graph/agent records still refuse.
#[derive(Debug)]
pub struct RecoveryView {
    pub record: Record,
    store: StoreIdentity,
    /// Expected path for display, even when missing or damaged. Check
    /// `archive_problem` before using it; execution uses [`Locked::context_archive_path`].
    pub context_archive: Option<PathBuf>,
    pub archive_problem: Option<RunError>,
}

impl RecoveryView {
    pub fn current_store_id(&self) -> String {
        store_id(self.store)
    }

    pub fn require_current_store(&self) -> Result<(), RunError> {
        self.record.check_store(self.store)
    }
}

pub fn inspect_recovery(
    run_dir: &Path,
    graph_id: Uuid,
    agent: &str,
) -> Result<Option<RecoveryView>, RunError> {
    state::check_name(agent)?;
    let Some(folder) = folder(run_dir, false)? else {
        return Ok(None);
    };
    let path = folder.join(format!("{}.json", state::file_name(agent)));
    let Some(record) = read_record(&path, graph_id, agent)? else {
        return Ok(None);
    };
    Ok(Some(RecoveryView {
        context_archive: record
            .context_archive
            .as_ref()
            .map(|hash| archive_path(&path, hash)),
        archive_problem: record.check_context_archive(&path).err(),
        record,
        store: store_identity(run_dir, false)?,
    }))
}

/// A diagnostic snapshot. A held sidecar means a watch is active; an admission
/// in that snapshot is not evidence of interruption. This is display-only and
/// never grants permission to execute or recover work.
#[derive(Debug)]
pub struct StatusView {
    pub recovery: RecoveryView,
    pub active: bool,
    /// Keep validated metadata visible even if its lock is missing or damaged.
    pub lock_problem: Option<RunError>,
}

/// Inspect an existing lock without creating files or waiting for a writer.
/// If it is free, keep it through the read so a just-completed admission cannot
/// be mistaken for an interrupted one from an older snapshot.
pub fn inspect_status(
    run_dir: &Path,
    graph_id: Uuid,
    agent: &str,
) -> Result<Option<StatusView>, RunError> {
    state::check_name(agent)?;
    let Some(folder) = folder(run_dir, false)? else {
        return Ok(None);
    };
    let path = folder.join(format!("{}.lock", state::file_name(agent)));
    let lock: Result<_, RunError> = (|| {
        let lock = fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags())
            .open(path)
            .map_err(|e| RunError::io("can't inspect supervisor progress lock", e))?;
        checked_file(&lock)?;
        let active = match lock.try_lock() {
            Ok(()) => false,
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(e)) => {
                return Err(RunError::io("can't inspect supervisor progress lock", e));
            }
        };
        Ok((lock, active))
    })();
    let active = lock.as_ref().is_ok_and(|(_, active)| *active);
    let view = inspect_recovery(run_dir, graph_id, agent)?;
    // Reading the record is complete before the successful lock is dropped.
    let lock_problem = lock.err();
    Ok(view.map(|recovery| StatusView {
        recovery,
        active,
        lock_problem,
    }))
}

/// Receipt for the caller to display after explicit recovery. The archive
/// holds the exact old record; turn context also remains in active progress.
#[derive(Debug)]
pub struct Resumed {
    /// Retained interruption context for display; it may predate this operation.
    pub recovery_note: Option<Turn>,
    pub receiver_note: Option<Receiver>,
    pub retired: bool,
    pub already_resumed: bool,
    pub archive: Option<PathBuf>,
}

/// Owns one stable lock inode. Never unlink this sidecar on release: another
/// process may already have opened it. The replaceable JSON is not the lock.
#[derive(Debug)]
pub struct Locked {
    lock: File,
    run_dir: PathBuf,
    path: PathBuf,
    graph_id: Uuid,
    store: StoreIdentity,
    agent: String,
    record: Option<Record>,
    poisoned: bool,
    /// Finished and unlocked before the process ends ([`Locked::release`]).
    released: bool,
    #[cfg(test)]
    pub(super) fail_write: Option<WriteStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteStep {
    Written,
    Synced,
    Renamed,
    DirectorySynced,
}

impl Drop for Locked {
    /// Clears the recorded holder while the lock is still held, so the next
    /// holder, if it records none, isn't named by this one's PID.
    fn drop(&mut self) {
        if !self.released {
            let _ = self.lock.set_len(0);
        }
    }
}

impl Locked {
    pub fn open(run_dir: &Path, graph_id: Uuid, agent: &str) -> Result<Self, RunError> {
        Self::open_inner(run_dir, graph_id, agent, false)
    }

    /// Only explicit operator recovery may open a foreign store. All normal
    /// mutation methods still check the binding; opening never rebinds it.
    pub fn open_for_resume(run_dir: &Path, graph_id: Uuid, agent: &str) -> Result<Self, RunError> {
        Self::open_inner(run_dir, graph_id, agent, true)
    }

    fn open_inner(
        run_dir: &Path,
        graph_id: Uuid,
        agent: &str,
        recovery: bool,
    ) -> Result<Self, RunError> {
        state::check_name(agent)?;
        if graph_id.is_nil() {
            return Err(invalid("progress requires a graph identity"));
        }
        let dir = folder(run_dir, true)?.ok_or_else(|| invalid("missing progress folder"))?;
        let path = dir.join(format!("{}.json", state::file_name(agent)));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(flags())
            .open(path.with_extension("lock"))
            .map_err(|e| RunError::io("can't open supervisor progress lock", e))?;
        checked_file(&lock)?;
        // A status/doctor snapshot briefly owns this same lock. Give that
        // read time to finish, without waiting indefinitely for a live watch.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        let held = holder(&lock).map(holder_note).unwrap_or_default();
                        let error = invalid(format!("{agent}'s supervisor progress is busy{held}"));
                        return Err(if recovery {
                            error.with_hint(format!(
                                "stop the running agents up before full recovery; to clear only a paused loop-guard thread while it is running, use `{BIN} agents resume {agent} --thread <thread>` (copy the thread from agents status)"
                            ))
                        } else {
                            error
                        });
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(RunError::io("can't lock supervisor progress", error));
                }
            }
        }
        record_holder(&lock);
        let store = store_identity(run_dir, true)?;
        let record = read(&path, graph_id, agent)?;
        if !recovery && let Some(record) = &record {
            record.check_store(store)?;
        }
        if record.as_ref().is_none_or(|r| r.store.matches(store)) {
            clean_temps(&path)?;
        }
        let mut locked = Self {
            lock,
            run_dir: run_dir.into(),
            path,
            graph_id,
            store,
            agent: agent.into(),
            record,
            poisoned: false,
            released: false,
            #[cfg(test)]
            fail_write: None,
        };
        if !recovery
            && let Some(record) = &locked.record
            && matches!(record.store, StoreIdentity::Legacy(_))
            && record.store.matches(store)
        {
            let mut migrated = record.clone();
            migrated.next_revision()?;
            migrated.schema = SCHEMA.into();
            migrated.store = store;
            // Keep admissions, budgets and recovery archives intact. A failed
            // migration stops execution and is retried on the next open.
            locked.persist(migrated).map_err(|error| error.with_hint(format!(
                "inspect the saved context, then run {BIN} agents resume {agent} --archive-context if the legacy record has no room for its new store identity"
            )))?;
        }
        Ok(locked)
    }

    pub fn record(&self) -> Option<&Record> {
        self.record.as_ref()
    }

    /// Validated archive path for the next turn's recovery prompt, under the
    /// watch's existing lock. A damaged archive never reaches execution.
    pub fn context_archive_path(&self) -> Result<Option<PathBuf>, RunError> {
        self.healthy()?;
        self.record
            .as_ref()
            .map(|record| record.check_context_archive(&self.path))
            .transpose()
            .map(Option::flatten)
    }

    pub fn current_store_id(&self) -> String {
        store_id(self.store)
    }

    fn healthy(&self) -> Result<(), RunError> {
        self.healthy_inner(false)
    }

    fn healthy_inner(&self, recovery: bool) -> Result<(), RunError> {
        if self.released {
            return Err(invalid("supervisor progress was released"));
        }
        if self.poisoned {
            return Err(invalid(
                "progress write failed; stop and reopen before continuing",
            ));
        }
        folder(&self.run_dir, false)?.ok_or_else(|| invalid("progress folder disappeared"))?;
        if store_identity(&self.run_dir, false)? != self.store {
            return Err(invalid("supervisor progress store was replaced"));
        }
        let held = self
            .lock
            .metadata()
            .map_err(|e| RunError::io("can't inspect progress lock", e))?;
        let at = fs::symlink_metadata(self.path.with_extension("lock"))
            .map_err(|e| RunError::io("progress lock disappeared", e))?;
        if !at.is_file() || at.dev() != held.dev() || at.ino() != held.ino() {
            return Err(invalid("supervisor progress lock was replaced"));
        }
        if read(&self.path, self.graph_id, &self.agent)? != self.record {
            return Err(invalid("supervisor progress changed outside its owner"));
        }
        if !recovery && let Some(record) = &self.record {
            record.check_store(self.store)?;
        }
        Ok(())
    }

    /// A matching unfinished invocation resumes its budget after a crash.
    /// An exec token additionally pins the logical watch identity. A different
    /// invocation never overwrites unfinished or interrupted progress.
    pub fn start(&mut self, invocation: &str, watch: Option<Uuid>) -> Result<&Record, RunError> {
        self.healthy()?;
        if let Some(record) = &self.record {
            record.require_ready()?;
            if record.resume_pending && record.invocation == invocation {
                if watch.is_some_and(|id| id.to_string() != record.watch_id) {
                    return Err(invalid(
                        "foreign supervisor invocation; existing progress was preserved",
                    ));
                }
                let mut continued = record.clone();
                continued.next_revision()?;
                continued.finished = false;
                continued.resume_pending = false;
                self.persist(continued)?;
                return Ok(self.record.as_ref().expect("saved"));
            }
            if !record.finished {
                if record.invocation != invocation
                    || watch.is_some_and(|id| id.to_string() != record.watch_id)
                {
                    return Err(invalid(
                        "foreign supervisor invocation; existing progress was preserved",
                    ).with_hint(format!(
                        "inspect the saved watch, then run {BIN} agents resume {} to retire it while retaining recovery context", self.agent
                    )));
                }
                return Ok(self.record.as_ref().expect("checked"));
            }
        }
        if watch.is_some() {
            return Err(invalid("exec progress is missing or already finished"));
        }
        let recovery = self
            .record
            .as_ref()
            .map(|r| r.recovery.clone())
            .unwrap_or_default();
        let context_archive = self.record.as_ref().and_then(|r| r.context_archive.clone());
        let receiver_recovery = self
            .record
            .as_ref()
            .map(|r| r.receiver_recovery.clone())
            .unwrap_or_default();
        self.persist(Record {
            schema: SCHEMA.into(),
            store: self.store,
            graph_id: self.graph_id.to_string(),
            agent: self.agent.clone(),
            watch_id: Uuid::now_v7().to_string(),
            invocation: invocation.into(),
            revision: 1,
            progress: Progress::default(),
            current: None,
            current_receiver: None,
            recovery,
            receiver_recovery,
            resume_pending: false,
            context_archive,
            finished: false,
        })?;
        Ok(self.record.as_ref().expect("saved"))
    }

    fn next(&self) -> Result<Record, RunError> {
        self.healthy()?;
        let mut next = self
            .record
            .clone()
            .ok_or_else(|| invalid("supervisor progress has not started"))?;
        if next.finished {
            return Err(invalid("supervisor progress is already finished"));
        }
        next.next_revision()?;
        Ok(next)
    }

    fn advance(record: &mut Record, progress: Progress) -> Result<(), RunError> {
        if progress.ran < record.progress.ran || progress.seen < record.progress.seen {
            return Err(invalid("supervisor progress cannot move backwards"));
        }
        record.progress = progress;
        Ok(())
    }

    pub fn checkpoint(&mut self, progress: Progress) -> Result<(), RunError> {
        let mut next = self.next()?;
        next.require_ready()?;
        Self::advance(&mut next, progress)?;
        self.persist(next)
    }

    /// Persist under the turn lease, before any harness admission or effect.
    pub fn begin_turn(&mut self, turn: Turn) -> Result<(), RunError> {
        let mut next = self.next()?;
        next.require_ready()?;
        check_turn(&turn)?;
        next.current = Some(turn);
        Self::admission_capacity(&next)?;
        self.persist(next)
    }

    fn receiver_admission(&self, receiver: Receiver) -> Result<Record, RunError> {
        let mut next = self.next()?;
        next.require_ready()?;
        check_receiver(&receiver)?;
        next.current_receiver = Some(receiver);
        Self::admission_capacity(&next)?;
        Ok(next)
    }

    /// Refuse invalid metadata or full recovery context before reserving a
    /// queue wake. This writes nothing and does not authorize a spawn.
    pub(crate) fn check_receiver_admission(&self, receiver: &Receiver) -> Result<(), RunError> {
        self.receiver_admission(receiver.clone()).map(|_| ())
    }

    /// Under the receiver's lease and final route check, immediately before
    /// spawning the queue command. Rechecks admission even after preflight;
    /// admission failure forbids spawning it.
    pub fn begin_receiver(&mut self, receiver: Receiver) -> Result<(), RunError> {
        let next = self.receiver_admission(receiver)?;
        self.persist(next)
    }

    /// Persist the exact dispatch result before lease release. The queue owner
    /// supplies NotSent only when non-delivery is proven; an uncertain spawn is
    /// DeliveryUnknown. No receiver delivery consumes a model's recovery note.
    /// Completion stores the caller's mail-wait cursor (`progress.seen`)
    /// without deriving it from the queue reservation's `through`. Explicit
    /// resume instead conservatively raises `seen` to at least `through` and
    /// rechecks the inbox. Any error must stop the supervisor and forbid re-exec.
    pub fn complete_receiver(
        &mut self,
        receiver: &Receiver,
        delivery: crate::receiver::route::Delivery,
        progress: Progress,
    ) -> Result<(), RunError> {
        let mut next = self.next()?;
        if next.current_receiver.as_ref() != Some(receiver) {
            return Err(invalid("receiver completion does not match its admission"));
        }
        let charged = u64::from(delivery != crate::receiver::route::Delivery::NotSent);
        if next.progress.ran.checked_add(charged) != Some(progress.ran) {
            return Err(invalid(
                "receiver completion has an incorrect dispatch budget charge",
            ));
        }
        next.current_receiver = None;
        Self::advance(&mut next, progress)?;
        self.persist(next)
    }

    /// Record a billing retry's session before that attempt can start. The
    /// logical turn, its original HEAD, mail and budget charge stay the same.
    pub fn attempt_session(&mut self, session: Option<String>) -> Result<(), RunError> {
        let mut next = self.next()?;
        let turn = next
            .current
            .as_mut()
            .ok_or_else(|| invalid("no admitted turn for a session update"))?;
        turn.session = session;
        check_turn(turn)?;
        Self::admission_capacity(&next)?;
        self.persist(next)
    }

    fn admission_capacity(next: &Record) -> Result<(), RunError> {
        // Admission must leave room for recovery after a crash, including
        // counter growth and the retained context of earlier interruptions.
        let mut resumed = next.clone();
        resumed.next_revision()?;
        resumed.resume_turn()?;
        resumed.resume_receiver()?;
        resumed.finished = true;
        resumed.resume_pending = true;
        // Explicit recovery may follow a move with a wider inode number.
        // Reserve the largest stable representation before admission.
        resumed.store = StoreIdentity::Stable(StableStore {
            id: Uuid::from_u128(u128::MAX),
            inode: u64::MAX,
        });
        resumed.require_inline_capacity()?;
        resumed.encode()?;
        // An unchanged invocation must also be able to reopen this budget.
        resumed.require_continuation_capacity()
    }

    /// Persist before releasing the turn lease or admitting the next turn.
    /// Only forget recovery context when this turn actually consumed it.
    pub fn complete_turn(
        &mut self,
        progress: Progress,
        consumed_recovery: bool,
    ) -> Result<(), RunError> {
        let mut next = self.next()?;
        if next.current.take().is_none() {
            return Err(invalid("no admitted turn to complete"));
        }
        if next.progress.ran.checked_add(1) != Some(progress.ran) {
            return Err(invalid(
                "an admitted turn must consume exactly one turn of the watch budget",
            ));
        }
        Self::advance(&mut next, progress)?;
        if consumed_recovery {
            next.recovery.clear();
            next.receiver_recovery.clear();
            next.context_archive = None;
        }
        self.persist(next)
    }

    /// Call only after showing the interrupted metadata to the operator.
    /// One atomic transition clears admission and retains the next prompt's
    /// context and retires this watch. A repeated resume is a no-op. Rebinding
    /// additionally requires the exact current store ID and archives old bytes.
    pub fn resume(&mut self, store: Option<&str>) -> Result<Resumed, RunError> {
        self.resume_with(store, false, |_, _| Ok(()))
    }

    /// Explicitly preserve the complete old record and replace inline context
    /// with its archive reference. Never deletes earlier records or archives.
    pub fn archive_context(&mut self, store: Option<&str>) -> Result<Resumed, RunError> {
        self.resume_with(store, true, |_, _| Ok(()))
    }

    fn resume_with(
        &mut self,
        store: Option<&str>,
        archive_context: bool,
        mut at: impl FnMut(bool, WriteStep) -> std::io::Result<()>,
    ) -> Result<Resumed, RunError> {
        self.healthy_inner(true)?;
        let Some(mut next) = self.record.clone() else {
            return Err(invalid("no supervisor progress to resume"));
        };
        // Before the first writer creates the nonce, read-only status can
        // only display the current physical identity. Accept that exact
        // confirmation too; never accept an arbitrary legacy saved identity.
        let confirmed = match store {
            Some(id) if id == self.current_store_id() => true,
            Some(id) => id == store_id(StoreIdentity::Legacy(physical_identity(&self.run_dir)?)),
            None => false,
        };
        let foreign = !next.store.matches(self.store);
        if foreign && !confirmed {
            return Err(next.check_store(self.store).expect_err("foreign"));
        }
        if store.is_some() && !confirmed {
            return Err(invalid(format!(
                "store confirmation does not match current {}",
                self.current_store_id()
            )));
        }
        let archive_inline = archive_context
            && (foreign
                || !next.finished
                || !next.recovery.is_empty()
                || !next.receiver_recovery.is_empty());
        if next.finished && next.store == self.store && !archive_inline {
            // A previous attempt may have renamed this record but failed or
            // crashed before syncing its directory. A successful retry must
            // finish that durability boundary even when no fields change.
            if let Err(error) = sync_saved(&self.path, |step| at(false, step)) {
                self.poisoned = true;
                return Err(RunError::io(
                    "can't sync resumed supervisor progress",
                    error,
                ));
            }
            return Ok(Resumed {
                recovery_note: next.recovery.last().cloned(),
                receiver_note: next.receiver_recovery.last().cloned(),
                retired: false,
                already_resumed: next.resume_pending,
                archive: None,
            });
        }
        next.next_revision()?;
        let turn = next.resume_turn()?;
        let receiver = next.resume_receiver()?;
        next.resume_pending |= !next.finished;
        next.finished = true;
        next.schema = SCHEMA.into();
        next.store = self.store;
        let preserved = if foreign || archive_inline {
            let bytes = read_bytes(&self.path)?
                .ok_or_else(|| invalid("progress disappeared before recovery"))?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            if archive_inline {
                next.recovery.clear();
                next.receiver_recovery.clear();
                next.context_archive = Some(hash.clone());
            }
            Some((hash, bytes))
        } else {
            None
        };
        // Preflight everything before publishing even the preserved copy.
        next.validate(self.graph_id, &self.agent)?;
        next.encode()?;
        next.require_continuation_capacity()?;
        let archive = if let Some((hash, bytes)) = preserved {
            match self.archive_with(&hash, &bytes, |step| at(true, step)) {
                Ok(path) => Some(path),
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            }
        } else {
            None
        };
        self.persist_inner(next, true, |step| at(false, step))?;
        Ok(Resumed {
            recovery_note: turn,
            receiver_note: receiver,
            retired: true,
            already_resumed: false,
            archive,
        })
    }

    fn archive_with(
        &self,
        hash: &str,
        bytes: &[u8],
        mut at: impl FnMut(WriteStep) -> std::io::Result<()>,
    ) -> Result<PathBuf, RunError> {
        let path = archive_path(&self.path, hash);
        if let Some(saved) = read_bytes(&path)? {
            if saved != bytes {
                return Err(invalid("recovery archive differs from the saved progress"));
            }
            // Existence alone does not prove the prior attempt synced it.
            sync_saved(&path, &mut at)
                .map_err(|e| RunError::io("can't sync recovery archive", e))?;
        } else {
            self.write_bytes(&path, bytes, at)
                .map_err(|e| RunError::io("can't preserve old supervisor progress", e))?;
        }
        Ok(path)
    }

    /// A supervisor that drained and re-execed itself for a reloaded
    /// definition continues its own unfinished watch under the reloaded
    /// invocation, keeping its count and cursor. Only the predecessor's
    /// watch id adopts, and never over an unresolved turn or dispatch.
    pub fn adopt(&mut self, invocation: &str, watch: &str) -> Result<(), RunError> {
        let mut next = self.next()?;
        next.require_ready()?;
        if next.watch_id != watch || next.resume_pending {
            return Err(invalid(
                "foreign supervisor invocation; existing progress was preserved",
            ));
        }
        if next.invocation == invocation {
            return Ok(());
        }
        next.invocation = invocation.into();
        self.persist(next)
    }

    pub fn finish(&mut self) -> Result<(), RunError> {
        if self.released {
            return Ok(());
        }
        let mut next = self.next()?;
        next.require_ready()?;
        next.resume_pending = false;
        next.finished = true;
        self.persist(next)
    }

    /// Finishes the watch and gives up its lock now, before this process
    /// ends: `agents stop` drained the agent, and another `agents up` may
    /// start it while this one runs its other agents. Every later write
    /// is refused; [`Locked::finish`] succeeds without writing.
    pub fn release(&mut self) -> Result<(), RunError> {
        if self.released {
            return Ok(());
        }
        self.finish()?;
        let _ = self.lock.set_len(0);
        self.lock
            .unlock()
            .map_err(|e| RunError::io("can't unlock supervisor progress", e))?;
        self.released = true;
        Ok(())
    }

    pub fn released(&self) -> bool {
        self.released
    }

    fn persist(&mut self, next: Record) -> Result<(), RunError> {
        #[cfg(test)]
        let fault = self.fail_write.take();
        self.persist_with(next, |_step| {
            #[cfg(test)]
            if fault == Some(_step) {
                return Err(std::io::Error::other("injected progress write failure"));
            }
            Ok(())
        })
    }

    fn persist_with(
        &mut self,
        next: Record,
        at: impl FnMut(WriteStep) -> std::io::Result<()>,
    ) -> Result<(), RunError> {
        self.persist_inner(next, false, at)
    }

    fn persist_inner(
        &mut self,
        next: Record,
        recovery: bool,
        at: impl FnMut(WriteStep) -> std::io::Result<()>,
    ) -> Result<(), RunError> {
        self.healthy_inner(recovery)?;
        next.validate(self.graph_id, &self.agent)?;
        next.check_store(self.store)?;
        next.check_context_archive(&self.path)?;
        let bytes = next.encode()?;
        if let Err(e) = self.write_bytes(&self.path, &bytes, at) {
            self.poisoned = true;
            return Err(RunError::io(
                "can't durably write supervisor progress; execution stopped",
                e,
            ));
        }
        self.record = Some(next);
        Ok(())
    }

    fn write_bytes(
        &self,
        path: &Path,
        bytes: &[u8],
        mut at: impl FnMut(WriteStep) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let tmp = self.path.with_extension(format!("{}.tmp", Uuid::now_v7()));
        let write = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(flags())
                .open(&tmp)?;
            file.write_all(bytes)?;
            at(WriteStep::Written)?;
            file.sync_all()?;
            at(WriteStep::Synced)?;
            fs::rename(&tmp, path)?;
            at(WriteStep::Renamed)?;
            File::open(self.path.parent().expect("progress directory"))?.sync_all()?;
            at(WriteStep::DirectorySynced)
        })();
        if write.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write
    }
}

fn sync_saved(
    path: &Path,
    mut at: impl FnMut(WriteStep) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags())
        .open(path)?;
    file.sync_all()?;
    at(WriteStep::Synced)?;
    File::open(path.parent().expect("progress directory"))?.sync_all()?;
    at(WriteStep::DirectorySynced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::process::Command;

    const INVOCATION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const GRAPH: &str = "01900000-0000-7000-8000-000000000001";
    const AGENT: &str = "codex:worker";

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let top = std::env::temp_dir().join(format!("mdkg-progress-{}", Uuid::now_v7()));
            fs::create_dir_all(top.join("common/mdkg/run")).unwrap();
            Self(top)
        }
        fn dir(&self) -> PathBuf {
            self.0.join("common/mdkg/run")
        }
        fn open(&self) -> Locked {
            Locked::open(&self.dir(), GRAPH.parse().unwrap(), AGENT).unwrap()
        }
        fn path(&self) -> PathBuf {
            self.dir().join("progress/codex%3Aworker.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn turn() -> Turn {
        Turn {
            id: Uuid::now_v7().to_string(),
            number: 7,
            start_head: Some("a".repeat(40)),
            session: Some("session-before-interruption".into()),
            mail_ids: vec![Uuid::now_v7().to_string(), Uuid::now_v7().to_string()],
            cursor: Some(17),
        }
    }
    fn snapshot(f: &Fixture) -> Record {
        inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap()
    }

    fn receiver() -> Receiver {
        Receiver {
            attempt_id: Uuid::now_v7().to_string(),
            session: Uuid::now_v7().to_string(),
            through: 23,
        }
    }

    #[test]
    fn writer_open_waits_for_a_brief_status_snapshot_but_not_a_live_watch() {
        use std::time::{Duration, Instant};

        let f = Fixture::new();
        let mut saved = f.open();
        saved.start(INVOCATION, None).unwrap();
        drop(saved);
        let bytes = fs::read(f.path()).unwrap();
        for recovery in [false, true] {
            // The same read-only descriptor and exclusive lock held by
            // inspect_status while it reads and validates the JSON snapshot.
            let reader = File::open(f.path().with_extension("lock")).unwrap();
            reader.try_lock().unwrap();
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    drop(reader);
                });
                let writer = if recovery {
                    Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                } else {
                    Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                }
                .expect("a brief diagnostic read must not reject a writer");
                assert_eq!(writer.record().unwrap(), &snapshot(&f));
            });
        }
        let _live_watch = f.open();
        let start = Instant::now();
        let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
        assert!(error.message.contains("busy"));
        assert!(
            error.message.ends_with(&format!(
                ": the last recorded holder is process {}",
                std::process::id()
            )),
            "{}",
            error.message
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
    }

    #[test]
    fn busy_lock_from_an_older_build_names_no_process() {
        let f = Fixture::new();
        let _live_watch = f.open();
        // A build from before the holder was recorded leaves the lock empty.
        fs::OpenOptions::new()
            .write(true)
            .open(f.path().with_extension("lock"))
            .unwrap()
            .set_len(0)
            .unwrap();
        let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
        assert!(
            error.message.ends_with("progress is busy"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_released_lock_names_no_one_and_a_stale_or_partial_record_says_so() {
        let f = Fixture::new();
        let lock = f.path().with_extension("lock");
        drop(f.open());
        assert_eq!(
            fs::read(&lock).unwrap(),
            b"",
            "released, the record is cleared"
        );
        // An older build holds the lock and records nothing, over a record
        // a crashed writer left: an exited process's PID.
        let older = File::open(&lock).unwrap();
        older.try_lock().unwrap();
        let mut exited = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = exited.id();
        exited.wait().unwrap();
        let busy = |record: &str| {
            fs::write(&lock, record).unwrap();
            Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap_err()
                .message
        };
        assert!(
            busy(&format!("{pid}\n")).ends_with(&format!(
                ": the last recorded holder, process {pid}, isn't running"
            )),
            "{}",
            busy(&format!("{pid}\n"))
        );
        // A torn or partial record names nobody.
        for partial in [
            format!("{pid}"),
            "12\x003\n".into(),
            "\n".into(),
            "0\n".into(),
        ] {
            assert!(busy(&partial).ends_with("progress is busy"), "{partial:?}");
        }
    }

    #[test]
    fn status_distinguishes_active_admissions_without_creating_or_changing_files() {
        let f = Fixture::new();
        assert!(
            inspect_status(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap()
                .is_none()
        );
        assert!(!f.dir().join("progress").exists());
        let mut saved = f.open();
        saved.start(INVOCATION, None).unwrap();
        let admitted = turn();
        saved.begin_turn(admitted.clone()).unwrap();
        let bytes = fs::read(f.path()).unwrap();
        let lock_bytes = fs::read(f.path().with_extension("lock")).unwrap();
        assert_eq!(lock_bytes, format!("{}\n", std::process::id()).into_bytes());
        let view = inspect_status(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap();
        // A snapshot reads the recorded holder's lock without writing it.
        assert_eq!(
            fs::read(f.path().with_extension("lock")).unwrap(),
            lock_bytes
        );
        assert!(view.active);
        assert_eq!(view.recovery.record.current, Some(admitted));
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        drop(saved);
        let view = inspect_status(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap();
        assert!(!view.active);
        assert_eq!(
            view.recovery.record.require_ready().unwrap_err().code,
            INTERRUPTED
        );
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        let lock = f.path().with_extension("lock");
        fs::remove_file(&lock).unwrap();
        let view = inspect_status(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap();
        assert!(view.lock_problem.is_some());
        assert!(!view.active);
        assert!(view.recovery.record.current.is_some());
        assert!(!lock.exists());
    }

    #[test]
    fn group_finalization_crash_and_one_failed_finish_keep_completed_counts() {
        const CHILD: &str = "MDKG_TEST_GROUP_FINISH";
        if let Some(dir) = std::env::var_os(CHILD) {
            let mut first = Locked::open(Path::new(&dir), GRAPH.parse().unwrap(), "a").unwrap();
            let mut second = Locked::open(Path::new(&dir), GRAPH.parse().unwrap(), "b").unwrap();
            first.finish().unwrap();
            let fault = std::env::var("MDKG_TEST_FINISH_FAULT").unwrap();
            second.fail_write = match fault.as_str() {
                "written" => Some(WriteStep::Written),
                "synced" => Some(WriteStep::Synced),
                "renamed" => Some(WriteStep::Renamed),
                "directory" => Some(WriteStep::DirectorySynced),
                "crash" => None,
                _ => panic!("unknown fault"),
            };
            if second.fail_write.is_some() {
                assert!(second.finish().is_err());
                assert!(
                    second
                        .finish()
                        .unwrap_err()
                        .message
                        .contains("stop and reopen")
                );
            }
            // No Rust destructors: a real process death between finalizations,
            // or immediately following the one injected failed finish write.
            std::process::exit(73);
        }
        for fault in ["crash", "written", "synced", "renamed", "directory"] {
            let f = Fixture::new();
            for name in ["a", "b"] {
                let mut saved = Locked::open(&f.dir(), GRAPH.parse().unwrap(), name).unwrap();
                saved.start(INVOCATION, None).unwrap();
                saved
                    .checkpoint(Progress {
                        ran: 1,
                        seen: Some(17),
                        check_inbox: true,
                    })
                    .unwrap();
            }
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "supervise::progress::tests::group_finalization_crash_and_one_failed_finish_keep_completed_counts", "--test-threads=1"])
                .env(CHILD, f.dir()).env("MDKG_TEST_FINISH_FAULT", fault)
                .status().unwrap();
            assert_eq!(status.code(), Some(73));
            let first = Locked::open(&f.dir(), GRAPH.parse().unwrap(), "a").unwrap();
            let second = Locked::open(&f.dir(), GRAPH.parse().unwrap(), "b").unwrap();
            assert!(first.record().unwrap().finished);
            assert_eq!(
                second.record().unwrap().finished,
                matches!(fault, "renamed" | "directory")
            );
            for saved in [&first, &second] {
                let record = saved.record().unwrap();
                assert_eq!(record.progress.ran, 1);
                assert_eq!(record.progress.seen, Some(17));
                assert!(record.current.is_none() && record.current_receiver.is_none());
            }
        }
    }

    #[test]
    fn receiver_completion_charges_delivery_without_consuming_model_context() {
        use crate::receiver::route::Delivery::*;
        for delivery in [Queued, DeliveryUnknown, NotSent] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            p.begin_turn(turn()).unwrap();
            p.resume(None).unwrap();
            p.start(INVOCATION, None).unwrap();
            let prior = p.record().unwrap().recovery.clone();
            let attempt = receiver();
            p.begin_receiver(attempt.clone()).unwrap();
            let before = fs::read(f.path()).unwrap();
            assert!(p.begin_turn(turn()).is_err());
            assert!(p.begin_receiver(receiver()).is_err());
            assert!(p.attempt_session(None).is_err());
            assert!(p.complete_turn(Progress::default(), true).is_err());
            let next = Progress {
                ran: 1 + u64::from(delivery != NotSent),
                seen: Some(29),
                check_inbox: true,
            };
            assert!(p.complete_receiver(&receiver(), delivery, next).is_err());
            assert!(
                p.complete_receiver(
                    &attempt,
                    delivery,
                    Progress {
                        ran: next.ran + 1,
                        ..next
                    }
                )
                .is_err()
            );
            assert_eq!(fs::read(f.path()).unwrap(), before);
            p.complete_receiver(&attempt, delivery, next).unwrap();
            assert_eq!(snapshot(&f).progress, next);
            assert_eq!(snapshot(&f).recovery, prior);
            assert!(snapshot(&f).current_receiver.is_none());
            let completed = fs::read(f.path()).unwrap();
            assert!(p.complete_receiver(&attempt, delivery, next).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), completed);
        }
    }

    #[test]
    fn interrupted_receiver_requires_explicit_resume_and_keeps_exact_metadata() {
        let f = Fixture::new();
        let attempt = receiver();
        let mut p = f.open();
        let watch = p.start(INVOCATION, None).unwrap().watch_id.clone();
        p.begin_receiver(attempt.clone()).unwrap();
        drop(p);
        let mut p = f.open();
        let error = p.start(INVOCATION, None).unwrap_err();
        assert_eq!(error.code, INTERRUPTED);
        assert!(error.message.contains(&attempt.attempt_id));
        assert!(error.hint.unwrap().contains("agents resume"));
        assert!(p.checkpoint(Progress::default()).is_err());
        assert!(p.finish().is_err());
        let resumed = p.resume(None).unwrap();
        assert!(resumed.recovery_note.is_none());
        assert_eq!(resumed.receiver_note, Some(attempt.clone()));
        let bytes = fs::read(f.path()).unwrap();
        let again = p.resume(None).unwrap();
        assert!(!again.retired);
        assert_eq!(again.receiver_note, Some(attempt.clone()));
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        let record = p.start(INVOCATION, None).unwrap();
        assert_eq!(record.watch_id, watch);
        assert_eq!(
            record.progress,
            Progress {
                ran: 1,
                seen: Some(23),
                check_inbox: true
            }
        );
        assert!(record.recovery.is_empty());
        assert_eq!(record.receiver_recovery, std::slice::from_ref(&attempt));
        p.resume(None).unwrap();
        let edited = p.start(&"b".repeat(64), None).unwrap();
        assert_ne!(edited.watch_id, watch);
        assert_eq!(edited.progress.ran, 0);
        assert_eq!(edited.receiver_recovery, [attempt]);
    }

    #[test]
    fn receiver_write_faults_preserve_admission_or_completed_charge_after_reopen() {
        use WriteStep::*;
        for step in [Written, Synced, Renamed, DirectorySynced] {
            for operation in ["admit", "complete", "resume"] {
                let f = Fixture::new();
                let mut p = f.open();
                p.start(INVOCATION, None).unwrap();
                let attempt = receiver();
                if operation != "admit" {
                    p.begin_receiver(attempt.clone()).unwrap();
                }
                let failed = if operation == "resume" {
                    p.resume_with(None, false, |archive, at| {
                        assert!(!archive);
                        if at == step {
                            Err(std::io::Error::other("one failed write"))
                        } else {
                            Ok(())
                        }
                    })
                    .map(|_| ())
                } else {
                    p.fail_write = Some(step);
                    if operation == "admit" {
                        p.begin_receiver(attempt.clone())
                    } else {
                        p.complete_receiver(
                            &attempt,
                            crate::receiver::route::Delivery::Queued,
                            Progress {
                                ran: 1,
                                seen: Some(23),
                                check_inbox: true,
                            },
                        )
                    }
                };
                assert!(failed.is_err());
                assert!(p.finish().unwrap_err().message.contains("stop and reopen"));
                drop(p);
                let changed = matches!(step, Renamed | DirectorySynced);
                let saved = snapshot(&f);
                let pending = if operation == "admit" {
                    changed
                } else {
                    !changed
                };
                assert_eq!(saved.current_receiver, pending.then_some(attempt.clone()));
                assert!(saved.current.is_none());
                assert_eq!(
                    saved.progress.ran,
                    u64::from(operation != "admit" && changed)
                );
                let mut p = f.open();
                if pending {
                    assert_eq!(p.start(INVOCATION, None).unwrap_err().code, INTERRUPTED);
                    p.resume(None).unwrap();
                    assert_eq!(p.record().unwrap().receiver_recovery, [attempt]);
                    assert_eq!(p.record().unwrap().progress.ran, 1);
                } else {
                    p.start(INVOCATION, None).unwrap();
                }
            }
        }
    }

    #[test]
    fn receiver_metadata_and_conflicting_admissions_are_refused_without_writes() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let before = fs::read(f.path()).unwrap();
        for field in ["attempt_id", "session", "through"] {
            let mut invalid = receiver();
            match field {
                "attempt_id" => invalid.attempt_id = Uuid::nil().to_string(),
                "session" => invalid.session = "not-a-session-uuid".into(),
                _ => invalid.through = 0,
            }
            assert!(p.begin_receiver(invalid).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), before);
        }
        p.begin_receiver(receiver()).unwrap();
        let good = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        for field in ["attempt_id", "session", "through", "both", "finished"] {
            let mut bad = good.clone();
            match field {
                "both" => bad["current"] = serde_json::to_value(turn()).unwrap(),
                "finished" => bad["finished"] = true.into(),
                _ => {
                    bad["current_receiver"]
                        .as_object_mut()
                        .unwrap()
                        .remove(field);
                }
            }
            let bytes = serde_json::to_vec(&bad).unwrap();
            fs::write(f.path(), &bytes).unwrap();
            assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn legacy_records_and_archives_preserve_turns_without_defaulting_receiver_fields() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let interrupted = turn();
        p.begin_turn(interrupted.clone()).unwrap();
        let mut old = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        old["schema"] = LEGACY_SCHEMA.into();
        old["store"] = serde_json::json!(physical_identity(&f.dir()).unwrap());
        old.as_object_mut().unwrap().remove("current_receiver");
        old.as_object_mut().unwrap().remove("receiver_recovery");
        let bytes = serde_json::to_vec(&old).unwrap();
        fs::write(f.path(), &bytes).unwrap();
        assert_eq!(snapshot(&f).current, Some(interrupted.clone()));
        assert_eq!(fs::read(f.path()).unwrap(), bytes); // reads never migrate in place
        let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
        assert_eq!(p.start(INVOCATION, None).unwrap_err().code, INTERRUPTED);
        let archive = p.archive_context(None).unwrap().archive.unwrap();
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert_eq!(p.context_archive_path().unwrap(), Some(archive));
        assert_eq!(snapshot(&f).schema, SCHEMA);
        p.start(INVOCATION, None).unwrap();
        drop(p);
        // A version downgrade cannot make a receiver admission disappear.
        old["current_receiver"] = serde_json::to_value(receiver()).unwrap();
        fs::write(f.path(), serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
    }

    #[test]
    fn receiver_context_capacity_refuses_admission_and_archive_restores_capacity() {
        for mixed in [false, true] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            let mut record = p.next().unwrap();
            if mixed {
                record.recovery = vec![turn()];
            }
            let attempt = receiver();
            loop {
                let mut candidate = record.clone();
                candidate.receiver_recovery.push(attempt.clone());
                if candidate.require_inline_capacity().is_err() {
                    break;
                }
                record = candidate;
            }
            p.persist(record).unwrap();
            let before = fs::read(f.path()).unwrap();
            let error = p.begin_receiver(attempt.clone()).unwrap_err();
            assert!(error.hint.unwrap().contains("--archive-context"));
            assert_eq!(fs::read(f.path()).unwrap(), before);
            let archive = p.archive_context(None).unwrap().archive.unwrap();
            assert_eq!(fs::read(&archive).unwrap(), before);
            assert!(snapshot(&f).receiver_recovery.is_empty());
            assert!(snapshot(&f).recovery.is_empty());
            p.start(INVOCATION, None).unwrap();
            p.begin_receiver(attempt).unwrap();
        }
    }

    #[test]
    fn progress_survives_reopen_without_resetting_budget_or_failed_mail_cursor() {
        let f = Fixture::new();
        let expected = Progress {
            ran: 2,
            seen: Some(17),
            check_inbox: false,
        };
        let mut p = f.open();
        let watch: Uuid = p.start(INVOCATION, None).unwrap().watch_id.parse().unwrap();
        p.checkpoint(expected).unwrap();
        drop(p);
        let mut reopened = f.open();
        assert_eq!(
            reopened.start(INVOCATION, Some(watch)).unwrap().progress,
            expected
        );
        assert_eq!(
            reopened.start(INVOCATION, None).unwrap().watch_id,
            watch.to_string()
        );
        assert!(reopened.start(&"b".repeat(64), None).is_err());
        assert!(reopened.start(INVOCATION, Some(Uuid::now_v7())).is_err());
        assert_eq!(snapshot(&f).progress, expected);
        assert!(reopened.checkpoint(Progress::default()).is_err());
    }

    #[test]
    fn resume_preserves_context_across_crash_and_is_idempotent() {
        let f = Fixture::new();
        let interrupted = turn();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        p.begin_turn(interrupted.clone()).unwrap();
        drop(p);
        let mut reopened = f.open();
        let blocked = reopened.start(INVOCATION, None).unwrap_err();
        assert_eq!(
            mdkg_format::Code::parse(&blocked.code),
            Some(mdkg_format::Code::RunTurnInterrupted)
        );
        assert!(blocked.message.contains("interrupted turn"));
        assert!(blocked.hint.unwrap().contains("agents resume codex:worker"));
        assert_eq!(
            reopened.resume(None).unwrap().recovery_note,
            Some(interrupted.clone())
        );
        let saved = fs::read(f.path()).unwrap();
        assert!(!reopened.resume(None).unwrap().retired);
        assert_eq!(fs::read(f.path()).unwrap(), saved);
        assert_eq!(reopened.record().unwrap().progress.ran, 1);
        drop(reopened);
        let mut p = f.open();
        assert_eq!(
            p.record().unwrap().recovery,
            std::slice::from_ref(&interrupted)
        );
        // Natural completion may start a new budget, keeping the prompt note.
        p.start(INVOCATION, None).unwrap();
        p.finish().unwrap();
        p.start(INVOCATION, None).unwrap();
        assert_eq!(p.record().unwrap().recovery, [interrupted]);
        p.begin_turn(turn()).unwrap();
        p.complete_turn(
            Progress {
                ran: 1,
                seen: Some(18),
                check_inbox: true,
            },
            true,
        )
        .unwrap();
        assert!(snapshot(&f).recovery.is_empty());
        assert!(snapshot(&f).current.is_none());
    }

    #[test]
    fn public_mail_ids_and_unborn_heads_survive_admission_and_recovery() {
        for head in [None, Some("a".repeat(40)), Some("b".repeat(64))] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            let mut admitted = turn();
            admitted.start_head = head;
            admitted.mail_ids = vec![
                format!("import:{}", "a".repeat(32)),
                "Message_7.note:reply-2".into(),
                "x".repeat(128),
                Uuid::now_v7().to_string(),
            ];
            p.begin_turn(admitted.clone()).unwrap();
            drop(p);
            assert_eq!(snapshot(&f).current, Some(admitted.clone()));
            let mut p = f.open();
            assert!(p.start(INVOCATION, None).is_err());
            assert_eq!(
                p.resume(None).unwrap().recovery_note,
                Some(admitted.clone())
            );
            let record = p.start(INVOCATION, None).unwrap();
            assert_eq!(record.recovery, [admitted]);
            assert_eq!(record.progress.ran, 1);
        }
    }

    #[test]
    fn invalid_mail_ids_or_heads_never_publish_an_admission() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let before = fs::read(f.path()).unwrap();
        for id in [
            "",
            "room:test",
            "ROOM:test",
            "space here",
            "line\nbreak",
            "slash/id",
            "é",
            &"x".repeat(129),
        ] {
            let mut bad = turn();
            bad.mail_ids = vec![id.into()];
            assert!(p.begin_turn(bad).is_err(), "{id:?}");
            assert_eq!(fs::read(f.path()).unwrap(), before);
        }
        for head in [
            "",
            "unborn",
            &"a".repeat(39),
            &"z".repeat(40),
            &"B".repeat(64),
        ] {
            let mut bad = turn();
            bad.start_head = Some(head.into());
            assert!(p.begin_turn(bad).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), before);
        }
    }

    #[test]
    fn loading_noncanonical_heads_refuses_without_rewriting() {
        for recovered in [false, true] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            p.begin_turn(turn()).unwrap();
            if recovered {
                p.resume(None).unwrap();
            }
            drop(p);
            let mut stored: serde_json::Value =
                serde_json::from_slice(&fs::read(f.path()).unwrap()).unwrap();
            let turn = if recovered {
                &mut stored["recovery"][0]
            } else {
                &mut stored["current"]
            };
            turn["start_head"] = serde_json::Value::String("aB".repeat(20));
            let bytes = serde_json::to_vec(&stored).unwrap();
            fs::write(f.path(), &bytes).unwrap();
            assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn retry_session_update_keeps_the_logical_admission_and_validates_before_write() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut admitted = turn();
        p.begin_turn(admitted.clone()).unwrap();
        for session in [Some("retry-session".into()), None] {
            p.attempt_session(session.clone()).unwrap();
            admitted.session = session;
            assert_eq!(snapshot(&f).current, Some(admitted.clone()));
            assert_eq!(snapshot(&f).progress.ran, 0);
        }
        let before = fs::read(f.path()).unwrap();
        assert!(p.attempt_session(Some("\n".into())).is_err());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        drop(p);
        let mut p = f.open();
        assert!(p.start(INVOCATION, None).is_err());
        assert_eq!(p.resume(None).unwrap().recovery_note, Some(admitted));
    }

    #[test]
    fn a_maximum_sized_admission_can_be_recovered_then_archived() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut admitted = turn();
        admitted.start_head = Some("a".repeat(64));
        admitted.session = Some("\"".repeat(4096));
        admitted.mail_ids = vec!["x".repeat(128); 1000];
        admitted.number = u64::MAX;
        admitted.cursor = Some(u64::MAX);
        p.begin_turn(admitted.clone()).unwrap();
        drop(p);
        let mut p = f.open();
        p.resume(None).unwrap();
        p.start(INVOCATION, None).unwrap();
        assert!(
            p.begin_turn(admitted.clone())
                .unwrap_err()
                .hint
                .unwrap()
                .contains("--archive-context")
        );
        p.archive_context(None).unwrap();
        assert_eq!(p.start(INVOCATION, None).unwrap().progress.ran, 1);
        p.begin_turn(admitted).unwrap();
    }

    #[test]
    fn admission_reserves_room_for_interrupted_turn_recovery() {
        for wide_counters in [false, true] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            let mut ready = p.next().unwrap();
            // Several resumed interruptions still need to reach the next prompt.
            let mut prior = turn();
            prior.mail_ids = vec![GRAPH.into(); 620];
            prior.session = Some("s".repeat(2048));
            ready.recovery = vec![prior; 4];
            ready.progress.ran = 4;
            ready.progress.seen = Some(17);
            let mut admitted = turn();
            admitted.mail_ids = vec![GRAPH.into(); 1000];
            if wide_counters {
                ready.revision = 98;
                ready.progress.ran = 9;
                ready.progress.seen = None;
                admitted.cursor = Some(u64::MAX);
            }
            let mut recovery = ready.recovery.clone();
            recovery.push(admitted.clone());
            let padding = INLINE_CAP + 1
                - serde_json::to_vec(&(&recovery, Vec::<Receiver>::new()))
                    .unwrap()
                    .len();
            admitted
                .session
                .as_mut()
                .unwrap()
                .push_str(&"s".repeat(padding));
            assert!(admitted.session.as_ref().unwrap().len() <= 4096);
            p.persist(ready).unwrap();
            let before = fs::read(f.path()).unwrap();
            let error = p.begin_turn(admitted.clone()).unwrap_err();
            assert!(error.message.contains("size limit"), "{error:?}");
            assert!(
                error
                    .hint
                    .unwrap()
                    .contains("agents resume codex:worker --archive-context")
            );
            assert_eq!(fs::read(f.path()).unwrap(), before);
            assert!(snapshot(&f).current.is_none());

            // The exact recovery boundary is accepted, without dropping any
            // earlier context, and leaves headroom for the active record.
            let session = admitted.session.as_mut().unwrap();
            session.truncate(session.len() - 1);
            // One byte over the recovery boundary still refuses, without
            // poisoning the owner or changing its authoritative record.
            let mut too_large = admitted.clone();
            too_large.session.as_mut().unwrap().push('s');
            assert!(p.begin_turn(too_large).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), before);
            p.begin_turn(admitted.clone()).unwrap();
            drop(p);
            let mut p = f.open();
            assert_eq!(
                p.resume(None).unwrap().recovery_note,
                Some(admitted.clone())
            );
            assert_eq!(snapshot(&f).recovery.last(), Some(&admitted));
            assert_eq!(
                snapshot(&f).progress.ran,
                if wide_counters { 10 } else { 5 }
            );
            assert_eq!(snapshot(&f).progress.seen, admitted.cursor);
            assert!(fs::metadata(f.path()).unwrap().len() <= CAP);
            let recovered = snapshot(&f).progress;
            assert_eq!(p.start(INVOCATION, None).unwrap().progress, recovered);
            assert!(fs::metadata(f.path()).unwrap().len() <= CAP);
            assert_eq!(
                serde_json::to_vec(&(&snapshot(&f).recovery, &snapshot(&f).receiver_recovery))
                    .unwrap()
                    .len(),
                INLINE_CAP
            );
        }
    }

    #[test]
    fn admission_refuses_exhausted_recovery_counters_before_writing() {
        for revision in [None, Some(u64::MAX - 1), Some(u64::MAX - 2)] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            let mut ready = p.next().unwrap();
            if let Some(revision) = revision {
                ready.revision = revision;
            } else {
                ready.progress.ran = u64::MAX;
            }
            p.persist(ready).unwrap();
            let before = fs::read(f.path()).unwrap();
            let error = p.begin_turn(turn()).unwrap_err();
            assert!(error.message.contains("exhausted"), "{error:?}");
            assert_eq!(fs::read(f.path()).unwrap(), before);
            p.finish().unwrap();
            assert!(snapshot(&f).current.is_none());
        }
    }

    #[test]
    fn completion_failure_preserves_an_authoritative_record_and_poisoned_writer_stops() {
        for failed in [
            WriteStep::Written,
            WriteStep::Synced,
            WriteStep::Renamed,
            WriteStep::DirectorySynced,
        ] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            let interrupted = turn();
            p.begin_turn(interrupted.clone()).unwrap();
            let mut done = p.next().unwrap();
            done.current = None;
            done.progress.ran = 1;
            assert!(
                p.persist_with(done, |step| {
                    if step == failed {
                        Err(std::io::Error::other("one failed write"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert!(
                p.checkpoint(Progress::default())
                    .unwrap_err()
                    .message
                    .contains("stop and reopen")
            );
            let observed = snapshot(&f);
            if matches!(failed, WriteStep::Written | WriteStep::Synced) {
                assert_eq!(observed.current, Some(interrupted));
                assert_eq!(observed.progress.ran, 0);
            } else {
                assert!(observed.current.is_none());
                assert_eq!(observed.progress.ran, 1);
            }
        }
    }

    #[test]
    fn failed_admission_write_never_authorizes_a_turn() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut admitted = p.next().unwrap();
        admitted.current = Some(turn());
        assert!(
            p.persist_with(admitted, |_| Err(std::io::Error::other("write failed")))
                .is_err()
        );
        assert!(p.begin_turn(turn()).is_err());
        assert!(snapshot(&f).current.is_none());
        assert_eq!(snapshot(&f).progress.ran, 0);
    }

    #[test]
    fn process_crash_after_save_or_one_failed_write_never_resets_progress() {
        const CHILD: &str = "MDKG_TEST_PROGRESS_CRASH";
        if let Some(dir) = std::env::var_os(CHILD) {
            let mut p = Locked::open(Path::new(&dir), GRAPH.parse().unwrap(), AGENT).unwrap();
            p.start(INVOCATION, None).unwrap();
            p.checkpoint(Progress {
                ran: 2,
                seen: Some(17),
                check_inbox: false,
            })
            .unwrap();
            if std::env::var_os("MDKG_TEST_PROGRESS_FAIL").is_some() {
                p.begin_turn(turn()).unwrap();
                let mut done = p.next().unwrap();
                done.current = None;
                done.progress.ran = 3;
                assert!(
                    p.persist_with(done, |_| Err(std::io::Error::other(
                        "failed completion write"
                    )))
                    .is_err()
                );
            }
            std::process::exit(73);
        }
        for failure in [false, true] {
            let f = Fixture::new();
            let mut child = Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "supervise::progress::tests::process_crash_after_save_or_one_failed_write_never_resets_progress", "--test-threads=1"])
                .env(CHILD, f.dir()).env_remove("MDKG_TEST_PROGRESS_FAIL");
            if failure {
                child.env("MDKG_TEST_PROGRESS_FAIL", "1");
            }
            assert_eq!(child.status().unwrap().code(), Some(73));
            let mut p = f.open();
            assert_eq!(
                p.record().unwrap().progress,
                Progress {
                    ran: 2,
                    seen: Some(17),
                    check_inbox: false
                }
            );
            assert_eq!(p.start(INVOCATION, None).is_err(), failure);
        }
    }

    #[test]
    fn corrupt_or_foreign_records_are_preserved_and_refused() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let good = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        let mut cases = vec![Vec::new(), b"{".to_vec()];
        for (key, value) in [
            (
                "schema",
                serde_json::json!("mdkg.supervisor-progress.unknown"),
            ),
            ("agent", serde_json::json!("someone-else")),
            ("graph_id", serde_json::json!(Uuid::now_v7().to_string())),
            ("watch_id", serde_json::json!(Uuid::nil().to_string())),
            ("revision", serde_json::json!(0)),
            ("resume_pending", serde_json::json!(true)),
            ("unexpected", serde_json::json!(true)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            cases.push(serde_json::to_vec(&bad).unwrap());
        }
        for bytes in cases {
            fs::write(f.path(), &bytes).unwrap();
            let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
            assert_eq!(
                mdkg_format::Code::parse(&error.code),
                Some(mdkg_format::Code::RunProgressInvalid)
            );
            assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn absent_nullable_fields_never_clear_an_admission_or_mail_cursor() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        p.begin_turn(turn()).unwrap();
        let good = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        for (object, field) in [
            ("", "current"),
            ("", "current_receiver"),
            ("", "receiver_recovery"),
            ("", "context_archive"),
            ("/progress", "seen"),
            ("/current", "session"),
            ("/current", "start_head"),
            ("/current", "cursor"),
        ] {
            let mut bad = good.clone();
            bad.pointer_mut(object)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            let bytes = serde_json::to_vec(&bad).unwrap();
            fs::write(f.path(), &bytes).unwrap();
            assert!(
                Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err(),
                "{object}/{field}"
            );
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn copied_record_from_another_clone_is_foreign_even_with_same_graph_and_agent() {
        let first = Fixture::new();
        let second = Fixture::new();
        let mut p = first.open();
        p.start(INVOCATION, None).unwrap();
        drop(second.open());
        fs::copy(first.path(), second.path()).unwrap();
        assert!(
            Locked::open(&second.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap_err()
                .message
                .contains("foreign")
        );
        assert_eq!(snapshot(&first), p.record().unwrap().clone());
    }

    #[test]
    fn legacy_device_change_migrates_without_resetting_progress_or_interruption() {
        for schema in [PREVIOUS_SCHEMA, LEGACY_SCHEMA] {
            for harness in ["claude", "codex", "opencode"] {
                let f = Fixture::new();
                let agent = format!("{harness}:worker");
                let path = f
                    .dir()
                    .join(format!("progress/{}.json", state::file_name(&agent)));
                let open = || Locked::open(&f.dir(), GRAPH.parse().unwrap(), &agent).unwrap();
                let mut p = open();
                p.start(INVOCATION, None).unwrap();
                p.checkpoint(Progress {
                    ran: 3,
                    seen: Some(12),
                    check_inbox: false,
                })
                .unwrap();
                p.begin_turn(turn()).unwrap();
                let mut expected = p.record().unwrap().clone();
                let mut saved = serde_json::to_value(&expected).unwrap();
                drop(p);
                fs::remove_file(f.dir().join(STORE_ID)).unwrap();
                let common = fs::metadata(f.0.join("common")).unwrap();
                saved["store"] = serde_json::json!([common.dev() ^ 1, common.ino()]);
                saved["schema"] = schema.into();
                if schema == LEGACY_SCHEMA {
                    saved.as_object_mut().unwrap().remove("current_receiver");
                    saved.as_object_mut().unwrap().remove("receiver_recovery");
                }
                fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
                let before = fs::read(&path).unwrap();
                // Status recognizes the unchanged store without creating the
                // nonce or changing old progress. Only the lock owner migrates.
                assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), &agent).is_ok());
                assert_eq!(fs::read(&path).unwrap(), before);
                assert!(!f.dir().join(STORE_ID).exists());
                let mut p = open();
                expected.store = p.store;
                expected.next_revision().unwrap();
                assert_eq!(p.record().unwrap(), &expected);
                assert_eq!(p.start(INVOCATION, None).unwrap_err().code, INTERRUPTED);
                assert_ne!(fs::read(&path).unwrap(), before);
                let migrated = fs::read(&path).unwrap();
                drop(p);
                drop(open());
                assert_eq!(fs::read(&path).unwrap(), migrated);
            }
        }
    }

    #[test]
    fn legacy_other_inode_refuses_and_preserves_the_record_with_recovery_hint() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut saved = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        let common = fs::metadata(f.0.join("common")).unwrap();
        saved["store"] = serde_json::json!([common.dev(), common.ino() ^ 1]);
        saved["schema"] = PREVIOUS_SCHEMA.into();
        let bytes = serde_json::to_vec(&saved).unwrap();
        fs::write(f.path(), &bytes).unwrap();
        let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
        assert!(error.message.contains("foreign supervisor progress store"));
        assert!(error.hint.unwrap().contains("--store"));
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
    }

    #[test]
    fn first_upgrade_recovery_accepts_the_store_confirmation_from_read_only_status() {
        for legacy in [false, true] {
            let (f, _, interrupted) = foreign_record();
            if legacy {
                let mut saved: serde_json::Value =
                    serde_json::from_slice(&fs::read(f.path()).unwrap()).unwrap();
                let [device, inode] = physical_identity(&f.dir()).unwrap();
                saved["store"] = serde_json::json!([device, inode ^ 1]);
                saved["schema"] = PREVIOUS_SCHEMA.into();
                fs::write(f.path(), serde_json::to_vec(&saved).unwrap()).unwrap();
            }
            fs::remove_file(f.dir().join(STORE_ID)).unwrap();
            let view = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap()
                .unwrap();
            let confirmation = view.current_store_id();
            assert!(!f.dir().join(STORE_ID).exists());
            let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
            assert_ne!(p.current_store_id(), confirmation);
            p.resume(Some(&confirmation)).unwrap();
            assert_eq!(p.record().unwrap().store, p.store);
            assert_eq!(p.record().unwrap().recovery, [interrupted]);
        }
    }

    #[test]
    fn legacy_record_at_the_file_cap_can_archive_before_expanding_its_identity() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut legacy = p.next().unwrap();
        legacy.schema = PREVIOUS_SCHEMA.into();
        legacy.store = StoreIdentity::Legacy(physical_identity(&f.dir()).unwrap());
        let mut old = turn();
        old.mail_ids = vec![GRAPH.into(); 1000];
        old.session = Some("s".repeat(4096));
        legacy.recovery = vec![old; 6];
        legacy.recovery.push(turn());
        let padding = CAP as usize - legacy.encode().unwrap().len();
        let session = legacy
            .recovery
            .last_mut()
            .unwrap()
            .session
            .as_mut()
            .unwrap();
        session.push_str(&"s".repeat(padding));
        assert!(session.len() <= 4096);
        p.persist(legacy).unwrap();
        drop(p);
        let before = fs::read(f.path()).unwrap();
        assert_eq!(before.len() as u64, CAP);
        let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
        assert!(error.hint.unwrap().contains("--archive-context"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
        let archive = p.archive_context(None).unwrap().archive.unwrap();
        assert_eq!(fs::read(archive).unwrap(), before);
        assert_eq!(p.record().unwrap().store, p.store);
        drop(p);
        assert!(f.open().start(INVOCATION, None).is_ok());
    }

    #[test]
    fn stable_store_round_trips_and_rejects_uuid_or_inode_changes() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let record = p.record().unwrap().clone();
        let StoreIdentity::Stable(current) = record.store else {
            panic!("new records must use stable store identities");
        };
        assert_eq!(read_store_id(&f.dir()).unwrap(), Some(current.id));
        let bytes = fs::read(f.path()).unwrap();
        drop(p);
        assert_eq!(f.open().record(), Some(&record));
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        assert_eq!(record.schema, SCHEMA);
        for foreign in [
            StableStore {
                id: Uuid::now_v7(),
                ..current
            },
            StableStore {
                inode: current.inode ^ 1,
                ..current
            },
        ] {
            let mut other = record.clone();
            other.store = StoreIdentity::Stable(foreign);
            let bytes = other.encode().unwrap();
            fs::write(f.path(), &bytes).unwrap();
            assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn versioned_store_shapes_cannot_be_mixed() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let stable = serde_json::to_value(p.record().unwrap()).unwrap();
        drop(p);
        for version in [SCHEMA, PREVIOUS_SCHEMA] {
            let mut invalid = stable.clone();
            invalid["schema"] = version.into();
            if version == SCHEMA {
                invalid["store"] = serde_json::json!(physical_identity(&f.dir()).unwrap());
            }
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(f.path(), &bytes).unwrap();
            assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn whole_common_directory_copy_is_foreign_even_with_copied_store_uuid() {
        let first = Fixture::new();
        let second = Fixture::new();
        let mut p = first.open();
        p.start(INVOCATION, None).unwrap();
        drop(second.open());
        fs::copy(first.dir().join(STORE_ID), second.dir().join(STORE_ID)).unwrap();
        fs::copy(first.path(), second.path()).unwrap();
        assert_eq!(
            read_store_id(&first.dir()).unwrap(),
            read_store_id(&second.dir()).unwrap()
        );
        assert!(Locked::open(&second.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
    }

    #[test]
    fn concurrent_store_initializers_converge_without_partial_identity_reads() {
        let f = Fixture::new();
        let barrier = std::sync::Barrier::new(8);
        let stores = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        store_identity(&f.dir(), true).unwrap()
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(stores.iter().all(|store| *store == stores[0]));
        assert_eq!(
            fs::metadata(f.dir().join(STORE_ID)).unwrap().mode() & 0o777,
            0o600
        );
        assert!(fs::read_dir(f.dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn store_identity_leaves_the_store_id_agents_turn_directory_available() {
        let f = Fixture::new();
        let agent = "store-id";
        state::check_name(agent).unwrap();
        let mut p = Locked::open(&f.dir(), GRAPH.parse().unwrap(), agent).unwrap();
        p.start(INVOCATION, None).unwrap();
        let turns = state::turns_dir(&f.dir(), agent);
        fs::create_dir_all(&turns).unwrap();
        assert!(turns.is_dir());
        assert!(read_store_id(&f.dir()).unwrap().is_some());
    }

    #[test]
    fn damaged_or_linked_store_identity_is_preserved_and_refused() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let bytes = fs::read(f.path()).unwrap();
        drop(p);
        let id = f.dir().join(STORE_ID);
        for malformed in ["", "not a UUID\n", "00000000-0000-0000-0000-000000000000\n"] {
            fs::write(&id, malformed).unwrap();
            assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read_to_string(&id).unwrap(), malformed);
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
        fs::remove_file(&id).unwrap();
        let target = f.0.join("identity-target");
        fs::write(&target, "preserve").unwrap();
        symlink(&target, &id).unwrap();
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "preserve");
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
    }

    fn foreign_record() -> (Fixture, Vec<u8>, Turn) {
        let first = Fixture::new();
        let second = Fixture::new();
        let mut p = first.open();
        p.start(INVOCATION, None).unwrap();
        let interrupted = turn();
        p.begin_turn(interrupted.clone()).unwrap();
        let mut bytes = b"\n ".to_vec();
        bytes.extend(fs::read(first.path()).unwrap());
        bytes.extend(b" \n");
        drop(second.open());
        fs::write(second.path(), &bytes).unwrap();
        (second, bytes, interrupted)
    }

    #[test]
    fn archiving_full_inline_context_restores_admission_without_deleting_history() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut ready = p.next().unwrap();
        let mut old = turn();
        old.mail_ids = vec![GRAPH.into(); 1000];
        old.session = Some("s".repeat(4096));
        ready.recovery = vec![old; 5];
        ready.progress.ran = 5;
        // Older valid records over the inline limit remain readable for repair.
        p.persist(ready).unwrap();
        assert!(
            serde_json::to_vec(&p.record().unwrap().recovery)
                .unwrap()
                .len()
                > INLINE_CAP
        );
        let bytes = fs::read(f.path()).unwrap();
        let error = p.begin_turn(turn()).unwrap_err();
        assert!(error.hint.unwrap().contains("--archive-context"));
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        let result = p.archive_context(None).unwrap();
        let archive = result.archive.unwrap();
        assert_eq!(p.context_archive_path().unwrap(), Some(archive.clone()));
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert!(p.record().unwrap().recovery.is_empty());
        assert!(p.record().unwrap().context_archive.is_some());
        let view = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap();
        assert_eq!(view.context_archive, Some(archive.clone()));
        let archived = fs::read(f.path()).unwrap();
        assert!(p.archive_context(None).unwrap().already_resumed);
        assert_eq!(fs::read(f.path()).unwrap(), archived);
        drop(p);
        let mut p = f.open();
        assert_eq!(p.start(INVOCATION, None).unwrap().progress.ran, 5);
        p.begin_turn(turn()).unwrap();
        p.complete_turn(
            Progress {
                ran: 6,
                ..Progress::default()
            },
            false,
        )
        .unwrap();
        assert!(p.record().unwrap().context_archive.is_some());
        p.finish().unwrap();
        p.start(INVOCATION, None).unwrap();
        assert!(p.record().unwrap().context_archive.is_some());
        p.begin_turn(turn()).unwrap();
        p.complete_turn(
            Progress {
                ran: 1,
                ..Progress::default()
            },
            true,
        )
        .unwrap();
        assert!(p.record().unwrap().context_archive.is_none());
        assert_eq!(fs::read(archive).unwrap(), bytes);
    }

    #[test]
    fn resume_preflights_continuation_of_an_older_record_at_the_file_cap() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let mut ready = p.next().unwrap();
        ready.revision = 98;
        let mut old = turn();
        old.mail_ids = vec![GRAPH.into(); 1000];
        old.session = Some("s".repeat(4096));
        ready.recovery = vec![old; 6];
        ready.recovery.push(turn());
        let padding = CAP as usize - ready.encode().unwrap().len();
        let session = ready.recovery.last_mut().unwrap().session.as_mut().unwrap();
        session.push_str(&"s".repeat(padding));
        assert!(session.len() <= 4096);
        p.persist(ready).unwrap();
        let before = fs::read(f.path()).unwrap();
        assert_eq!(before.len() as u64, CAP);
        let error = p.resume(None).unwrap_err();
        assert!(error.hint.unwrap().contains("--archive-context"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        p.archive_context(None).unwrap();
        assert_eq!(p.start(INVOCATION, None).unwrap().revision, 100);
    }

    #[test]
    fn continuation_write_failures_preserve_the_recovered_budget() {
        for failed in [
            WriteStep::Written,
            WriteStep::Synced,
            WriteStep::Renamed,
            WriteStep::DirectorySynced,
        ] {
            let f = Fixture::new();
            let mut p = f.open();
            let watch = p.start(INVOCATION, None).unwrap().watch_id.clone();
            p.checkpoint(Progress {
                ran: 3,
                seen: Some(99),
                check_inbox: false,
            })
            .unwrap();
            let interrupted = turn();
            p.begin_turn(interrupted.clone()).unwrap();
            p.resume(None).unwrap();
            let expected = p.record().unwrap().progress;
            let mut continued = p.record().unwrap().clone();
            continued.next_revision().unwrap();
            continued.finished = false;
            continued.resume_pending = false;
            assert!(
                p.persist_with(continued, |step| {
                    if step == failed {
                        Err(std::io::Error::other("failed continuation write"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            drop(p);
            let mut p = f.open();
            assert_eq!(
                p.record().unwrap().resume_pending,
                matches!(failed, WriteStep::Written | WriteStep::Synced)
            );
            let resumed = p.start(INVOCATION, None).unwrap();
            assert_eq!(resumed.progress, expected);
            assert_eq!(resumed.watch_id, watch);
            assert_eq!(resumed.recovery, [interrupted]);
        }
    }

    #[test]
    fn repeated_context_archives_form_a_bounded_chain_and_keep_all_original_bytes() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let first_turn = turn();
        p.begin_turn(first_turn.clone()).unwrap();
        let first_bytes = fs::read(f.path()).unwrap();
        let first = p.archive_context(None).unwrap().archive.unwrap();
        let first_hash = p.record().unwrap().context_archive.clone().unwrap();
        assert_eq!(p.record().unwrap().progress.ran, 1);
        assert!(p.archive_context(None).unwrap().already_resumed);
        assert_eq!(p.start(INVOCATION, None).unwrap().progress.ran, 1);
        let second_turn = turn();
        p.begin_turn(second_turn.clone()).unwrap();
        let second_bytes = fs::read(f.path()).unwrap();
        let second = p.archive_context(None).unwrap().archive.unwrap();
        let old: Record = serde_json::from_slice(&fs::read(&second).unwrap()).unwrap();
        assert_eq!(old.context_archive, Some(first_hash));
        assert_eq!(old.current, Some(second_turn));
        let older: Record = serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
        assert_eq!(older.current, Some(first_turn));
        assert!(p.record().unwrap().recovery.is_empty());
        assert_eq!(p.record().unwrap().progress.ran, 2);
        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), first_bytes);
        assert_eq!(fs::read(second).unwrap(), second_bytes);
        assert!(fs::metadata(f.path()).unwrap().len() < 2048);
    }

    #[test]
    fn archived_context_must_be_present_safe_and_authentic_before_start() {
        for damage in [
            "missing", "hash", "symlink", "hardlink", "mode", "agent", "graph", "pointer",
        ] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            p.begin_turn(turn()).unwrap();
            let archive = p.archive_context(None).unwrap().archive.unwrap();
            let mut record = p.record().unwrap().clone();
            drop(p);
            match damage {
                "missing" => fs::remove_file(&archive).unwrap(),
                "hash" => fs::write(&archive, "{}").unwrap(),
                "symlink" => {
                    let target = f.0.join("saved-archive");
                    fs::rename(&archive, &target).unwrap();
                    symlink(target, &archive).unwrap();
                }
                "hardlink" => fs::hard_link(&archive, f.0.join("linked-archive")).unwrap(),
                "mode" => fs::set_permissions(&archive, fs::Permissions::from_mode(0o666)).unwrap(),
                "agent" | "graph" => {
                    let mut old: Record =
                        serde_json::from_slice(&fs::read(&archive).unwrap()).unwrap();
                    if damage == "agent" {
                        old.agent = "other".into();
                    } else {
                        old.graph_id = Uuid::now_v7().to_string();
                    }
                    let bytes = old.encode().unwrap();
                    let hash = format!("{:x}", Sha256::digest(&bytes));
                    fs::write(archive_path(&f.path(), &hash), bytes).unwrap();
                    record.context_archive = Some(hash);
                }
                _ => {
                    record.context_archive = Some("../not-an-archive".into());
                }
            }
            fs::write(f.path(), record.encode().unwrap()).unwrap();
            let before = fs::read(f.path()).unwrap();
            assert!(
                Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err(),
                "{damage}"
            );
            assert!(
                Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err(),
                "{damage}"
            );
            assert!(
                inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err(),
                "{damage}"
            );
            let view = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT);
            if damage == "pointer" {
                assert!(view.is_err());
            } else {
                let view = view.unwrap().unwrap();
                assert_eq!(view.record, record);
                assert_eq!(view.current_store_id(), record.store_id());
                let path = view.context_archive.unwrap();
                let error = view.archive_problem.unwrap();
                assert!(error.hint.unwrap().contains(&path.display().to_string()));
            }
            assert_eq!(fs::read(f.path()).unwrap(), before);
        }
    }

    #[test]
    fn archive_context_faults_never_clear_inline_without_the_complete_archive() {
        for archive_write in [false, true] {
            for failed in [
                WriteStep::Written,
                WriteStep::Synced,
                WriteStep::Renamed,
                WriteStep::DirectorySynced,
            ] {
                let f = Fixture::new();
                let mut p = f.open();
                p.start(INVOCATION, None).unwrap();
                let interrupted = turn();
                p.begin_turn(interrupted.clone()).unwrap();
                let bytes = fs::read(f.path()).unwrap();
                assert!(
                    p.resume_with(None, true, |archive, step| {
                        if archive == archive_write && step == failed {
                            Err(std::io::Error::other("failed archive-context write"))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
                );
                assert!(p.archive_context(None).is_err());
                drop(p);
                let mut p = f.open();
                let published = !archive_write
                    && matches!(failed, WriteStep::Renamed | WriteStep::DirectorySynced);
                assert_eq!(p.record().unwrap().context_archive.is_some(), published);
                assert_eq!(p.record().unwrap().current.is_none(), published);
                let retry = p.archive_context(None).unwrap();
                assert_eq!(retry.already_resumed, published);
                assert_eq!(p.record().unwrap().progress.ran, 1);
                assert!(p.record().unwrap().recovery.is_empty());
                let view = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                    .unwrap()
                    .unwrap();
                let archive = view.context_archive.unwrap();
                assert_eq!(fs::read(&archive).unwrap(), bytes);
                let archived: Record = serde_json::from_slice(&fs::read(archive).unwrap()).unwrap();
                assert_eq!(archived.current, Some(interrupted));
                assert!(p.archive_context(None).unwrap().already_resumed);
            }
        }
    }

    #[test]
    fn archiving_foreign_context_still_requires_the_exact_store_confirmation() {
        let (f, bytes, interrupted) = foreign_record();
        let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
        for token in [None, Some("*")] {
            assert!(p.archive_context(token).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
        let current = p.current_store_id();
        let archived = p.archive_context(Some(&current)).unwrap();
        assert_eq!(archived.recovery_note, Some(interrupted));
        assert_eq!(fs::read(archived.archive.unwrap()).unwrap(), bytes);
        assert!(p.record().unwrap().context_archive.is_some());
        assert!(p.record().unwrap().recovery.is_empty());
        assert_eq!(p.record().unwrap().store_id(), current);
    }

    #[test]
    fn explicit_resume_retires_an_unfinished_invocation_without_losing_context() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let interrupted = turn();
        p.begin_turn(interrupted.clone()).unwrap();
        p.resume(None).unwrap();
        p.start(INVOCATION, None).unwrap();
        p.checkpoint(Progress {
            ran: 5,
            seen: Some(99),
            check_inbox: false,
        })
        .unwrap();
        let changed = "b".repeat(64);
        let error = p.start(&changed, None).unwrap_err();
        assert!(error.hint.unwrap().contains("agents resume codex:worker"));
        let result = p.resume(None).unwrap();
        assert!(result.retired);
        assert!(result.recovery_note.is_none());
        assert!(result.archive.is_none());
        let before = fs::read(f.path()).unwrap();
        assert!(!p.resume(None).unwrap().retired);
        assert_eq!(fs::read(f.path()).unwrap(), before);
        drop(p);
        let mut p = f.open();
        let old_watch = p.record().unwrap().watch_id.clone();
        let next = p.start(&changed, None).unwrap();
        assert_ne!(next.watch_id, old_watch);
        assert_eq!(next.progress, Progress::default());
        assert_eq!(next.invocation, changed);
        assert_eq!(next.recovery, [interrupted]);
    }

    #[test]
    fn resumed_same_invocation_preserves_budget_cursor_and_watch_identity() {
        let f = Fixture::new();
        let mut p = f.open();
        let watch = p.start(INVOCATION, None).unwrap().watch_id.clone();
        p.checkpoint(Progress {
            ran: 3,
            seen: Some(99),
            check_inbox: false,
        })
        .unwrap();
        let interrupted = turn();
        p.begin_turn(interrupted.clone()).unwrap();
        drop(p);
        let mut p = f.open();
        p.resume(None).unwrap();
        let expected = Progress {
            ran: 4,
            seen: Some(99),
            check_inbox: true,
        };
        assert_eq!(p.record().unwrap().progress, expected);
        let before = fs::read(f.path()).unwrap();
        let retry = p.resume(None).unwrap();
        assert!(!retry.retired);
        assert!(retry.already_resumed);
        assert_eq!(retry.recovery_note, Some(interrupted.clone()));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        drop(p);
        let mut p = f.open();
        assert!(p.start(INVOCATION, Some(Uuid::now_v7())).is_err());
        let continued = p.start(INVOCATION, Some(watch.parse().unwrap())).unwrap();
        assert_eq!(continued.watch_id, watch);
        assert_eq!(continued.progress, expected);
        assert!(!continued.finished && !continued.resume_pending);
        assert_eq!(continued.recovery, [interrupted]);
        // A between-turn recovery also preserves an intentionally parked cursor.
        p.checkpoint(Progress {
            check_inbox: false,
            ..expected
        })
        .unwrap();
        p.resume(None).unwrap();
        let continued = p.start(INVOCATION, None).unwrap();
        assert_eq!(
            continued.progress,
            Progress {
                check_inbox: false,
                ..expected
            }
        );
        assert_eq!(continued.watch_id, watch);
        // Natural completion is the distinct boundary for another fresh watch.
        p.finish().unwrap();
        assert!(!p.resume(None).unwrap().already_resumed);
        assert_eq!(
            p.start(INVOCATION, None).unwrap().progress,
            Progress::default()
        );
    }

    #[test]
    fn foreign_recovery_requires_exact_confirmation_and_preserves_original_bytes() {
        let (f, bytes, interrupted) = foreign_record();
        let view = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT)
            .unwrap()
            .unwrap();
        let current = view.current_store_id();
        let recorded = view.record.store_id();
        // Archive suffixes cannot alias another agent's .json record.
        let neighbor = format!("{AGENT}.recovery-{:x}", Sha256::digest(&bytes));
        let mut other = Locked::open(&f.dir(), GRAPH.parse().unwrap(), &neighbor).unwrap();
        other.start(INVOCATION, None).unwrap();
        let other_path = f
            .path()
            .parent()
            .unwrap()
            .join(format!("{}.json", state::file_name(&neighbor)));
        let other_bytes = fs::read(&other_path).unwrap();
        let error = view.require_current_store().unwrap_err();
        assert!(error.message.contains(&current) && error.message.contains(&recorded));
        assert!(
            error
                .hint
                .unwrap()
                .contains(&format!("agents resume {AGENT} --store {current}"))
        );
        assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
        assert!(p.start(INVOCATION, None).is_err());
        assert!(p.checkpoint(Progress::default()).is_err());
        assert!(p.begin_turn(turn()).is_err());
        assert!(
            p.complete_turn(
                Progress {
                    ran: 1,
                    ..Progress::default()
                },
                true
            )
            .is_err()
        );
        for confirmation in [
            None,
            Some(recorded.as_str()),
            Some("wrong"),
            Some(&format!(" {current}")),
            Some(&current[..current.len() - 1]),
            Some("*"),
        ] {
            assert!(p.resume(confirmation).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
            assert_eq!(fs::read_dir(f.path().parent().unwrap()).unwrap().count(), 4);
        }
        let uppercase = current.to_uppercase();
        if uppercase != current {
            assert!(p.resume(Some(&uppercase)).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
        let result = p.resume(Some(&current)).unwrap();
        assert_eq!(result.recovery_note, Some(interrupted.clone()));
        assert!(result.retired);
        let archive = result.archive.unwrap();
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert_eq!(fs::read(&other_path).unwrap(), other_bytes);
        other.healthy().unwrap();
        assert_eq!(fs::metadata(&archive).unwrap().mode() & 0o777, 0o600);
        let rebound = fs::read(f.path()).unwrap();
        assert!(p.resume(Some("wrong")).is_err());
        assert_eq!(fs::read(f.path()).unwrap(), rebound);
        let retry = p.resume(Some(&current)).unwrap();
        assert!(!retry.retired && retry.already_resumed);
        assert_eq!(retry.recovery_note, Some(interrupted.clone()));
        drop(p);
        let mut p = f.open();
        let resumed = p.record().unwrap();
        assert!(resumed.current.is_none() && resumed.finished);
        assert_eq!(resumed.recovery, std::slice::from_ref(&interrupted));
        assert_eq!(resumed.progress.ran, 1);
        assert_eq!(resumed.store_id(), current);
        p.start(&"b".repeat(64), None).unwrap();
        assert_eq!(p.record().unwrap().recovery, [interrupted]);
        assert_eq!(fs::read(archive).unwrap(), bytes);
    }

    #[test]
    fn foreign_recovery_write_faults_preserve_state_and_retry_only_once() {
        for archive in [true, false] {
            for failed in [
                WriteStep::Written,
                WriteStep::Synced,
                WriteStep::Renamed,
                WriteStep::DirectorySynced,
            ] {
                let (f, bytes, interrupted) = foreign_record();
                let mut p =
                    Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
                let current = p.current_store_id();
                assert!(
                    p.resume_with(Some(&current), false, |is_archive, step| {
                        if is_archive == archive && step == failed {
                            Err(std::io::Error::other("failed recovery write"))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
                );
                assert!(
                    p.resume(Some(&current))
                        .unwrap_err()
                        .message
                        .contains("stop and reopen")
                );
                let changed =
                    !archive && matches!(failed, WriteStep::Renamed | WriteStep::DirectorySynced);
                let observed = inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                    .unwrap()
                    .unwrap();
                assert_eq!(observed.record.finished, changed);
                assert_eq!(observed.record.current.is_none(), changed);
                if !changed {
                    assert_eq!(fs::read(f.path()).unwrap(), bytes);
                }
                drop(p);
                let mut p =
                    Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
                // Reuse a pre-existing complete archive after a failed sync.
                let resumed = p.resume(Some(&current)).unwrap();
                assert_eq!(resumed.retired, !changed);
                assert_eq!(p.record().unwrap().recovery, [interrupted]);
                assert_eq!(p.record().unwrap().progress.ran, 1);
                assert!(!p.resume(Some(&current)).unwrap().retired);
                let path = archive_path(&f.path(), &format!("{:x}", Sha256::digest(&bytes)));
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn recovery_archives_never_replace_existing_unexpected_files() {
        for linked in [false, true] {
            let (f, bytes, _) = foreign_record();
            let path = archive_path(&f.path(), &format!("{:x}", Sha256::digest(&bytes)));
            let target = f.0.join("archive-target");
            fs::write(&target, "preserve target").unwrap();
            if linked {
                symlink(&target, &path).unwrap();
            } else {
                fs::write(&path, "preserve archive").unwrap();
            }
            let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
            let current = p.current_store_id();
            assert!(p.resume(Some(&current)).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
            assert_eq!(fs::read_to_string(&target).unwrap(), "preserve target");
            if linked {
                assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
            } else {
                assert_eq!(fs::read_to_string(&path).unwrap(), "preserve archive");
            }
        }
    }

    #[test]
    fn context_archive_does_not_replace_an_existing_different_file() {
        for linked in [false, true] {
            let f = Fixture::new();
            let mut p = f.open();
            p.start(INVOCATION, None).unwrap();
            p.begin_turn(turn()).unwrap();
            let before = fs::read(f.path()).unwrap();
            let path = archive_path(&f.path(), &format!("{:x}", Sha256::digest(&before)));
            let target = f.0.join("archive-target");
            fs::write(&target, "preserve target").unwrap();
            if linked {
                symlink(&target, &path).unwrap();
            } else {
                fs::write(&path, "preserve archive").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            assert!(p.archive_context(None).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), before);
            assert_eq!(fs::read_to_string(&target).unwrap(), "preserve target");
            if linked {
                assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
            } else {
                assert_eq!(fs::read_to_string(&path).unwrap(), "preserve archive");
            }
        }
    }

    #[test]
    fn long_encoded_agent_names_can_archive_and_continue() {
        let f = Fixture::new();
        let agent = format!("a{}", ":".repeat(70));
        assert_eq!(state::file_name(&agent).len(), 211);
        let mut p = Locked::open(&f.dir(), GRAPH.parse().unwrap(), &agent).unwrap();
        assert!(p.context_archive_path().unwrap().is_none());
        p.start(INVOCATION, None).unwrap();
        p.begin_turn(turn()).unwrap();
        let original = fs::read(&p.path).unwrap();
        let archive = p.archive_context(None).unwrap().archive.unwrap();
        assert_eq!(archive.file_name().unwrap().len(), 73);
        assert_eq!(fs::read(&archive).unwrap(), original);
        assert_eq!(p.context_archive_path().unwrap(), Some(archive.clone()));
        drop(p);
        let mut p = Locked::open(&f.dir(), GRAPH.parse().unwrap(), &agent).unwrap();
        assert_eq!(p.start(INVOCATION, None).unwrap().progress.ran, 1);
        p.begin_turn(turn()).unwrap();
        fs::remove_file(&archive).unwrap();
        assert!(p.context_archive_path().is_err());
        assert!(
            inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), &agent)
                .unwrap()
                .unwrap()
                .archive_problem
                .is_some()
        );
    }

    #[test]
    fn resume_retry_syncs_an_already_rebound_record_before_success() {
        let (f, _, interrupted) = foreign_record();
        let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
        let store = p.current_store_id();
        assert!(
            p.resume_with(Some(&store), false, |archive, step| {
                if !archive && step == WriteStep::Renamed {
                    Err(std::io::Error::other("directory sync never reached"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        drop(p);
        let before = fs::read(f.path()).unwrap();
        let mut p = f.open();
        assert!(
            p.resume_with(None, false, |archive, step| {
                assert!(!archive);
                if step == WriteStep::DirectorySynced {
                    Err(std::io::Error::other("retry sync failed"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert!(
            p.resume(None)
                .unwrap_err()
                .message
                .contains("stop and reopen")
        );
        drop(p);
        let mut p = f.open();
        let mut synced = Vec::new();
        let result = p
            .resume_with(None, false, |archive, step| {
                assert!(!archive);
                synced.push(step);
                Ok(())
            })
            .unwrap();
        assert!(!result.retired);
        assert_eq!(synced, [WriteStep::Synced, WriteStep::DirectorySynced]);
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert_eq!(p.record().unwrap().recovery, [interrupted]);
        assert_eq!(p.record().unwrap().progress.ran, 1);
    }

    #[test]
    fn recovery_crash_after_archive_or_rebind_or_one_failed_write_keeps_context() {
        const CHILD: &str = "MDKG_TEST_RECOVERY_CRASH";
        if let Some(dir) = std::env::var_os(CHILD) {
            let fault = std::env::var("MDKG_TEST_RECOVERY_FAULT").unwrap();
            let archive_context = std::env::var("MDKG_TEST_ARCHIVE_CONTEXT").unwrap() == "true";
            let mut p =
                Locked::open_for_resume(Path::new(&dir), GRAPH.parse().unwrap(), AGENT).unwrap();
            let store = p.current_store_id();
            let result = p.resume_with(Some(&store), archive_context, |archive, step| {
                if step == WriteStep::Renamed
                    && ((fault == "archive" && archive) || (fault == "active" && !archive))
                {
                    std::process::exit(73);
                }
                if fault == "write" && archive && step == WriteStep::Written {
                    Err(std::io::Error::other("failed archive write before crash"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            std::process::exit(73);
        }
        for (fault, archive_context) in ["archive", "active", "write"]
            .into_iter()
            .flat_map(|fault| [false, true].map(|archive_context| (fault, archive_context)))
        {
            let (f, bytes, interrupted) = foreign_record();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "supervise::progress::tests::recovery_crash_after_archive_or_rebind_or_one_failed_write_keeps_context", "--test-threads=1"])
                .env(CHILD, f.dir()).env("MDKG_TEST_RECOVERY_FAULT", fault)
                .env("MDKG_TEST_ARCHIVE_CONTEXT", archive_context.to_string())
                .status().unwrap();
            assert_eq!(status.code(), Some(73));
            let mut p = Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap();
            assert_eq!(p.record().unwrap().finished, fault == "active");
            let store = p.current_store_id();
            let result = if archive_context {
                p.archive_context(Some(&store))
            } else {
                p.resume(Some(&store))
            }
            .unwrap();
            assert_eq!(result.retired, fault != "active");
            if archive_context {
                assert!(p.record().unwrap().recovery.is_empty());
                assert!(p.record().unwrap().context_archive.is_some());
            } else {
                assert_eq!(p.record().unwrap().recovery, [interrupted]);
            }
            assert_eq!(p.record().unwrap().progress.ran, 1);
            let archive = archive_path(&f.path(), &format!("{:x}", Sha256::digest(&bytes)));
            assert_eq!(fs::read(archive).unwrap(), bytes);
        }
    }

    #[test]
    fn recovery_open_never_bypasses_record_validation() {
        let (f, _, _) = foreign_record();
        for bytes in [b"{".to_vec(), b"{}".to_vec()] {
            fs::write(f.path(), &bytes).unwrap();
            assert!(Locked::open_for_resume(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert!(inspect_recovery(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
        let (f, bytes, _) = foreign_record();
        for (graph, agent) in [(Uuid::now_v7(), AGENT), (GRAPH.parse().unwrap(), "other")] {
            // Wrong agent also gets its own name's file; contents remain foreign.
            let other = f
                .path()
                .parent()
                .unwrap()
                .join(format!("{}.json", state::file_name(agent)));
            fs::write(&other, &bytes).unwrap();
            assert!(Locked::open_for_resume(&f.dir(), graph, agent).is_err());
            assert_eq!(fs::read(other).unwrap(), bytes);
        }
    }

    #[test]
    fn existing_folder_is_synced_again_after_its_creators_failed_sync() {
        for failure in 0..4 {
            let f = Fixture::new();
            fs::remove_dir_all(f.dir().parent().unwrap()).unwrap();
            assert!(
                inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                    .unwrap()
                    .is_none()
            );
            assert!(!f.dir().parent().unwrap().exists());
            let mut call = 0;
            assert!(
                folder_with_sync(&f.dir(), true, |path| {
                    if call == failure {
                        return Err(std::io::Error::other("parent sync failed"));
                    }
                    call += 1;
                    File::open(path)?.sync_all()
                })
                .is_err()
            );
            for path in [
                f.dir().parent().unwrap().to_path_buf(),
                f.dir(),
                f.dir().join("progress"),
            ] {
                assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o700);
            }
            let mut synced = Vec::new();
            folder_with_sync(&f.dir(), true, |path| {
                synced.push(path.to_path_buf());
                File::open(path)?.sync_all()
            })
            .unwrap();
            assert_eq!(
                synced,
                [
                    f.0.join("common"),
                    f.0.join("common/mdkg"),
                    f.dir(),
                    f.dir().join("progress")
                ]
            );
            assert!(f.open().start(INVOCATION, None).is_ok());
        }
    }

    #[test]
    fn missing_record_is_not_confused_with_crash_debris_and_reads_create_nothing() {
        let f = Fixture::new();
        assert!(
            inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                .unwrap()
                .is_none()
        );
        assert!(!f.dir().join("progress").exists());
        let mut p = f.open();
        fs::write(f.dir().join("progress/interrupted.tmp"), b"{").unwrap();
        assert!(p.start(INVOCATION, Some(Uuid::now_v7())).is_err());
        p.start(INVOCATION, None).unwrap();
        assert_eq!(snapshot(&f).progress, Progress::default());
    }

    #[test]
    fn opening_cleans_only_owned_crash_temps_under_the_stable_lock() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let abandoned = f.path().with_extension(format!("{}.tmp", Uuid::now_v7()));
        fs::write(&abandoned, "interrupted write").unwrap();
        let dir = abandoned.parent().unwrap();
        let other = dir.join(format!("other.{}.tmp", Uuid::now_v7()));
        let similar = dir.join(format!("codex%3Aworker.more.{}.tmp", Uuid::now_v7()));
        let unexpected = f.path().with_extension("not-a-uuid.tmp");
        let linked = f.path().with_extension(format!("{}.tmp", Uuid::now_v7()));
        let missing = f.0.join("missing-target");
        symlink(&missing, &linked).unwrap();
        let directory = f.path().with_extension(format!("{}.tmp", Uuid::now_v7()));
        fs::create_dir(&directory).unwrap();
        let hardlinked = f.path().with_extension(format!("{}.tmp", Uuid::now_v7()));
        fs::hard_link(&abandoned, &hardlinked).unwrap();
        for path in [&other, &similar, &unexpected] {
            fs::write(path, "leave alone").unwrap();
        }
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert!(abandoned.exists());
        drop(p);
        // Hard links are not treated as abandoned writes, even under the lock.
        drop(f.open());
        assert!(abandoned.exists());
        fs::remove_file(hardlinked).unwrap();
        let p = f.open();
        assert!(!abandoned.exists());
        for path in [&other, &similar, &unexpected] {
            assert_eq!(fs::read_to_string(path).unwrap(), "leave alone");
        }
        assert!(fs::symlink_metadata(linked).unwrap().is_symlink());
        assert!(!missing.exists());
        assert!(directory.is_dir());
        assert_eq!(snapshot(&f), p.record().unwrap().clone());
    }

    #[test]
    fn linked_folders_and_locks_are_refused_without_touching_their_targets() {
        for folder in [false, true] {
            for existing in [false, true] {
                let f = Fixture::new();
                drop(f.open());
                let link = if folder {
                    let path = f.dir().join("progress");
                    fs::remove_dir_all(&path).unwrap();
                    path
                } else {
                    let path = f.path().with_extension("lock");
                    fs::remove_file(&path).unwrap();
                    path
                };
                let target = f.0.join("link-target");
                if existing {
                    if folder {
                        fs::create_dir(&target).unwrap();
                    } else {
                        fs::write(&target, "preserve lock target").unwrap();
                    }
                }
                symlink(&target, &link).unwrap();
                assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
                assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
                if existing && folder {
                    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
                } else if existing {
                    assert_eq!(fs::read_to_string(&target).unwrap(), "preserve lock target");
                } else {
                    assert!(!target.exists());
                }
            }
        }
    }

    #[test]
    fn writable_record_shared_repository_and_hardlinked_lock_are_refused() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        drop(p);
        let before = fs::read(f.path()).unwrap();
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o664)).unwrap();
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let alias = f.0.join("lock-alias");
        fs::hard_link(f.path().with_extension("lock"), &alias).unwrap();
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert_eq!(fs::metadata(&alias).unwrap().nlink(), 2);
        assert_eq!(fs::read(f.path()).unwrap(), before);
        fs::remove_file(alias).unwrap();
        fs::set_permissions(f.0.join("common"), fs::Permissions::from_mode(0o775)).unwrap();
        let error = Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).unwrap_err();
        assert!(error.message.contains("unsafe supervisor progress folder"));
        assert!(error.hint.unwrap().contains("use a private clone"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
    }

    #[test]
    fn stable_lock_survives_record_replacement_and_snapshot_does_not_block() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        for ran in 0..3 {
            p.checkpoint(Progress {
                ran,
                ..Progress::default()
            })
            .unwrap();
            assert!(
                Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT)
                    .unwrap_err()
                    .message
                    .contains("busy")
            );
            assert_eq!(snapshot(&f).progress.ran, ran);
        }
        let sidecar = f.path().with_extension("lock");
        let inode = fs::metadata(&sidecar).unwrap().ino();
        drop(p);
        let _next = f.open();
        assert_eq!(fs::metadata(sidecar).unwrap().ino(), inode);
    }

    #[test]
    fn unsafe_folder_file_or_replaced_lock_is_refused() {
        let f = Fixture::new();
        let mut p = f.open();
        p.start(INVOCATION, None).unwrap();
        let folder = f.dir().join("progress");
        assert_eq!(fs::metadata(&folder).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(p.begin_turn(turn()).is_err());
        assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        let sidecar = f.path().with_extension("lock");
        fs::rename(&sidecar, folder.join("old-lock")).unwrap();
        fs::write(&sidecar, "").unwrap();
        assert!(
            p.begin_turn(turn())
                .unwrap_err()
                .message
                .contains("lock was replaced")
        );
        drop(p);
        let saved = folder.join("saved");
        fs::rename(f.path(), &saved).unwrap();
        symlink(&saved, f.path()).unwrap();
        assert!(inspect(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
        assert!(Locked::open(&f.dir(), GRAPH.parse().unwrap(), AGENT).is_err());
    }
}
