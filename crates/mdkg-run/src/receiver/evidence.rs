//! Independent observation evidence. Begin is lock-free and precedes route
//! work; completion/reset take the actor's route lock. Nothing here is pruned.
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{RunError, hooks, invalid, read_json};

// Bounded overlapping observations. Exhaustion creates a permanent overflow
// latch, so later completion of all active slots cannot hide a dropped hook.
const SLOTS: u128 = 64;

#[derive(Debug)]
pub(super) struct Marker {
    path: PathBuf,
    file: fs::File,
}

impl Marker {
    // The open descriptor prevents inode reuse after reset. A stale ticket
    // must never remove evidence belonging to a newly reused slot.
    pub(super) fn check(&self) -> Result<(), RunError> {
        let held = self
            .file
            .metadata()
            .map_err(|e| RunError::io("can't inspect receiver marker", e))?;
        let current = fs::symlink_metadata(&self.path)
            .map_err(|e| RunError::io("receiver marker was reset", e))?;
        if !current.is_file() || current.dev() != held.dev() || current.ino() != held.ino() {
            return Err(invalid("receiver marker was replaced"));
        }
        Ok(())
    }
}

fn folder(common: &Path, actor: &str) -> PathBuf {
    hooks::dir(common)
        .join("receiver")
        .join(format!("{}.pending", crate::state::file_name(actor)))
}

fn observed_folder(pending: &Path) -> PathBuf {
    pending.with_extension("observed")
}

fn observed_path(marker: &Path) -> Result<PathBuf, RunError> {
    Ok(observed_folder(
        marker
            .parent()
            .ok_or_else(|| invalid("receiver evidence has no folder"))?,
    )
    .join(
        marker
            .file_name()
            .ok_or_else(|| invalid("receiver evidence has no name"))?,
    ))
}

pub(super) fn begin(common: &Path, actor: &str) -> Result<Marker, RunError> {
    let dir = folder(common, actor);
    directory(&dir)?;
    for slot in 1..=SLOTS {
        let path = dir.join(Uuid::from_u128(slot).to_string());
        match create_marker(&path) {
            Ok(file) => {
                sync_marker(&file, &dir)?;
                return Ok(Marker { path, file });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => {
                return Err(RunError::io(
                    "can't persist pending receiver observation",
                    e,
                ));
            }
        }
    }
    let overflow = dir.join("overflow");
    match create_marker(&overflow) {
        Ok(file) => sync_marker(&file, &dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(RunError::io("can't persist receiver overflow", e)),
    }
    Err(invalid(
        "receiver observation slots exhausted; explicit resume required",
    ))
}

fn directory(path: &Path) -> Result<(), RunError> {
    fs::create_dir_all(path).map_err(|e| RunError::io("can't create receiver evidence", e))?;
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(invalid("receiver evidence isn't a directory"));
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), RunError> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| RunError::io("can't sync receiver evidence", e))
}

fn create_marker(path: &Path) -> std::io::Result<fs::File> {
    // Presence alone refuses admission, including a zero-byte or partial file.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
}

fn sync_marker(file: &fs::File, dir: &Path) -> Result<(), RunError> {
    file.sync_all()
        .map_err(|e| RunError::io("can't sync receiver marker", e))?;
    sync_dir(dir)?;
    sync_dir(
        dir.parent()
            .ok_or_else(|| invalid("receiver evidence has no parent"))?,
    )
}

/// The general thread-status registry is pruned. Keep this independent
/// working-Thread witness until completion/reset, independent of pruning.
pub(super) fn persist_thread(marker: &Marker, thread: &hooks::Thread) -> Result<(), RunError> {
    use std::io::Write;
    let path = observed_path(&marker.path)?;
    let dir = path
        .parent()
        .ok_or_else(|| invalid("receiver evidence has no folder"))?;
    directory(dir)?;
    let bytes =
        serde_json::to_vec(thread).map_err(|_| invalid("can't encode receiver thread witness"))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(&path)
        .map_err(|e| RunError::io("can't persist receiver thread witness", e))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| RunError::io("can't persist receiver thread witness", e))?;
    sync_dir(dir)?;
    sync_dir(
        dir.parent()
            .ok_or_else(|| invalid("receiver evidence has no parent"))?,
    )
}

pub(super) fn finish(marker: &Marker) -> Result<(), RunError> {
    marker.check()?;
    let path = &marker.path;
    let observed = observed_path(path)?;
    fs::remove_file(&observed)
        .map_err(|e| RunError::io("can't complete receiver thread witness", e))?;
    sync_dir(
        observed
            .parent()
            .ok_or_else(|| invalid("receiver evidence has no folder"))?,
    )?;
    fs::remove_file(path).map_err(|e| RunError::io("can't complete receiver observation", e))?;
    sync_dir(
        path.parent()
            .ok_or_else(|| invalid("receiver evidence has no folder"))?,
    )?;
    Ok(())
}

pub(super) fn pending(common: &Path, actor: &str) -> Result<bool, RunError> {
    let dir = folder(common, actor);
    Ok(nonempty(&dir)? || nonempty(&observed_folder(&dir))?)
}

fn nonempty(dir: &Path) -> Result<bool, RunError> {
    match fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(RunError::io("can't inspect receiver evidence", e)),
        Ok(m) if m.is_dir() => (),
        Ok(_) => return Err(invalid("receiver evidence isn't a directory")),
    }
    let mut entries =
        fs::read_dir(dir).map_err(|e| RunError::io("can't read receiver evidence", e))?;
    entries
        .next()
        .transpose()
        .map(|e| e.is_some())
        .map_err(|e| RunError::io("can't read receiver evidence", e))
}

/// Only an explicit full reset clears abandoned evidence, after the reset
/// route has persisted. Never recursively remove unknown content or links.
pub(super) fn reset(common: &Path, actor: &str) -> Result<(), RunError> {
    let dir = folder(common, actor);
    for folder in [&dir, &observed_folder(&dir)] {
        directory(folder)?;
        for entry in
            fs::read_dir(folder).map_err(|e| RunError::io("can't list receiver evidence", e))?
        {
            let entry = entry.map_err(|e| RunError::io("can't read receiver evidence", e))?;
            if !entry.file_type().is_ok_and(|t| t.is_file())
                || entry
                    .file_name()
                    .to_str()
                    .is_none_or(|s| s != "overflow" && Uuid::parse_str(s).is_err())
            {
                return Err(invalid(
                    "receiver pending evidence is not a regular observation marker",
                ));
            }
            fs::remove_file(entry.path())
                .map_err(|e| RunError::io("can't reset receiver evidence", e))?;
        }
        sync_dir(folder)?;
    }
    Ok(())
}

/// Strict counterpart to the best-effort status listing. The stable shared
/// lock covers the callback (only a spawn, never a child wait). Missing,
/// unreadable, linked, oversized or malformed Codex records cannot disappear
/// from routing evidence. Lock order is receiver route, then thread registry.
pub(super) fn with_threads<T>(
    common: &Path,
    f: impl FnOnce(&[hooks::Thread]) -> Result<T, RunError>,
) -> Result<T, RunError> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(hooks::dir(common).join("threads.lock"))
        .map_err(|e| RunError::io("can't lock receiver thread evidence", e))?;
    if !lock.metadata().is_ok_and(|m| m.is_file()) {
        return Err(invalid("thread evidence lock isn't a regular file"));
    }
    lock.lock()
        .map_err(|e| RunError::io("can't lock receiver thread evidence", e))?;
    let all = read_threads(common)?;
    f(&all)
}

/// Diagnostics never create a lock or wait for a hook/dispatch to finish.
pub(super) fn snapshot_lock(path: &Path) -> Result<fs::File, RunError> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|e| RunError::io("receiver busy: snapshot lock unavailable", e))?;
    if !lock.metadata().is_ok_and(|m| m.is_file()) {
        return Err(invalid("receiver busy: snapshot lock isn't a regular file"));
    }
    match lock.try_lock_shared() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(invalid("receiver busy")),
        Err(fs::TryLockError::Error(e)) => {
            Err(RunError::io("receiver busy: can't read snapshot lock", e))
        }
    }
}

pub(super) fn snapshot_threads(common: &Path) -> Result<Vec<hooks::Thread>, RunError> {
    let _lock = snapshot_lock(&hooks::dir(common).join("threads.lock"))?;
    read_threads(common)
}

fn read_threads(common: &Path) -> Result<Vec<hooks::Thread>, RunError> {
    let dir = hooks::dir(common).join("threads");
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return Err(invalid("receiver thread registry is missing or unreadable"));
    }
    let mut all = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|e| RunError::io("can't list receiver thread evidence", e))?
    {
        let entry = entry.map_err(|e| RunError::io("can't read receiver thread evidence", e))?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("codex-")
            || !name.to_string_lossy().ends_with(".json")
        {
            continue;
        }
        let value = read_json(&entry.path())?
            .ok_or_else(|| invalid("receiver thread evidence disappeared"))?;
        let t: hooks::Thread = serde_json::from_value(value)
            .map_err(|_| invalid("receiver thread evidence is malformed"))?;
        if t.harness != "codex"
            || Uuid::parse_str(&t.session).is_err()
            || name != format!("codex-{}.json", t.session).as_str()
            || !t.cwd.is_absolute()
        {
            return Err(invalid(
                "receiver thread evidence has inconsistent identity",
            ));
        }
        all.push(t);
        if all.len() > 4096 {
            return Err(invalid("receiver thread registry is too large"));
        }
    }
    Ok(all)
}
