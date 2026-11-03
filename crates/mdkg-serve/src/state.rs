//! The `serve` lock and `serve.json` (the serve edd's Process model).
//!
//! `serve` takes `<common>/mdkg/serve.lock` exclusively before it opens a
//! listener, so a second `serve` for the repository is refused
//! (`api.serve_running`, R3), timers never run twice and the socket has
//! one owner. The OS releases the lock on a crash. The store's lease-mode
//! switch takes the same lock, with the lease-mode lock (S37); a `serve`
//! started during a switch waits for it (SC55). Local commands never take
//! the lock.
//!
//! `<common>/mdkg` is the socket's folder, so `serve` keeps it private
//! (S4). Every check and open is anchored on one descriptor of that folder,
//! opened without following a link, so nothing can be swapped between a
//! check and its use (Terra 1 #6697). Lock files are opened without
//! following links or blocking, and must be regular files of this user
//! with one link, so a planted FIFO can't hang a start and a hard link to
//! the store can't be truncated by a stamp.
//!
//! The lock is checked to be the file its path names when it is taken,
//! and again on the 10 s timer ([`ServeLock::still_held`]). An unlinked or
//! replaced `serve.lock` lets a second `serve` lock a new file beside it
//! until this one's next check, which then stops this one (Terra 2
//! #7280).
//!
//! Residual (orchestrator, #6716 answer (a), #6822): mode `0700` doesn't
//! remove macOS ACL entries. Those can be granted by the owner or root,
//! inherited from a parent folder, or changed by someone delegated
//! `writesecurity`. A user with delete or write on `<common>/mdkg` through
//! one can replace the lock file between timer ticks. The inode checks
//! notice a replacement still in place at a check, not one undone before
//! it: renaming the locked file aside, letting a switch lock a new one,
//! then renaming the original back escapes them (Terra 3 #7009).
//!
//! After it listens, `serve` locks `serve.live`, stamps it with its serve
//! id, and writes `serve.json` (0600) for discovery on the host, removing
//! it on a clean stop. Only `serve` ever takes `serve.live`, never the
//! lease-mode switch, so `serve.json` is running only while `serve.live`
//! is held and its stamp names the file's serve id; otherwise it is stale,
//! or held by another.

use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};

pub const DIR: &str = "mdkg";
pub const LOCK_FILE: &str = "serve.lock";
pub const LIVE_FILE: &str = "serve.live";
pub const STATE_FILE: &str = "serve.json";
pub const STATE_SCHEMA: &str = "mdkg.serve.state.v1";
/// The lease-mode lock (S37): delegations hold it shared, the switch
/// exclusively.
pub const LEASE_MODE_LOCK: &str = "lease-mode.lock";

/// `<common>/mdkg`, where the store and serve's files live.
pub fn mdkg_dir(common: &Path) -> PathBuf {
    common.join(DIR)
}

/// This process's effective uid.
pub fn own_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn other(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

/// A file's `(dev, ino)`; the field types differ by platform.
#[allow(clippy::unnecessary_cast)]
fn file_id(st: &rustix::fs::Stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

/// A verified descriptor of `<common>/mdkg`.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
    path: PathBuf,
    /// The folder's `(dev, ino)` when opened, so a renamed and recreated
    /// folder is noticed ([`Dir::still_named`]).
    id: (u64, u64),
}

impl Dir {
    /// Opens `<common>/mdkg` without following a link, and refuses it
    /// unless it is a real folder of this user. With `private`, it is
    /// created 0700 if missing, and an existing one only ever loses bits:
    /// it is set to its own mode, read from this descriptor, `& 0700`, so
    /// the group's and others' bits go and none is added (orchestrator's
    /// amended #6822, #7076). The coordinator store creates the folder with
    /// the default mode, so a 0755 one is usual; a 0500 one stays 0500.
    pub fn open(common: &Path, private: bool) -> io::Result<Self> {
        let parent = rustix::fs::open(
            common,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut created = false;
        if private {
            // Only we may replace names in <common>: it must be ours, and
            // writable by no one else unless sticky (orchestrator #7478).
            // A <common> another user can write is a compromised
            // repository anyway.
            let pst = rustix::fs::fstat(&parent)?;
            let pmode = pst.st_mode as u32;
            // The hint names the repair that fits, its path quoted for a
            // shell (Terra 6 #8208).
            if pst.st_uid != own_uid() {
                return Err(other(format!(
                    "{} belongs to another user; run serve as its owner",
                    common.display()
                )));
            }
            if pmode & 0o022 != 0 && pmode & 0o1000 == 0 {
                return Err(other(format!(
                    "{} must be writable by no one else; run `chmod go-w {}`",
                    common.display(),
                    shell_quoted(&common.display().to_string())
                )));
            }
            match rustix::fs::mkdirat(&parent, DIR, Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    created = true;
                    // Set by name before the read-only open, so a umask of
                    // 0777 can't strand a folder we made unopenable; the
                    // parent is trusted, so the name is ours (#7478).
                    // A failed chmod would strand a folder of the umask's
                    // mode, 000 under 0777: it is removed again (Terra 4
                    // #8518).
                    // Removing a folder only succeeds while it is empty,
                    // so another process's lock file inside keeps it.
                    if let Err(e) = chmod_dir(&parent, DIR) {
                        let e = io::Error::from(e);
                        if rustix::fs::unlinkat(&parent, DIR, AtFlags::REMOVEDIR).is_ok() {
                            // Durable, as the lock file's (Terra 2 #8870),
                            // and a failed sync said (Terra 1 #8926).
                            return Err(with_sync(e, sync_dir(&parent)));
                        }
                        return Err(e);
                    }
                }
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let path = mdkg_dir(common);
        let fd = rustix::fs::openat(
            &parent,
            DIR,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| unopenable_dir(&parent, &path, e))?;
        let mut st = rustix::fs::fstat(&fd)?;
        if st.st_uid != own_uid() {
            return Err(other(format!("{} belongs to another user", path.display())));
        }
        if created {
            // Just made: its mode is the umask's, so set it exactly.
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
            st = rustix::fs::fstat(&fd)?;
        }
        // Exactly `mode & 0700`: the group's and others' bits and the
        // special bits (setuid, setgid, sticky) go, none is added (#7478).
        let mode = st.st_mode as u32 & 0o7777;
        if private && mode != mode & 0o700 {
            rustix::fs::fchmod(&fd, Mode::from_raw_mode((mode & 0o700) as _))?;
            st = rustix::fs::fstat(&fd)?;
        }
        Ok(Self {
            fd,
            path,
            id: file_id(&st),
        })
    }

    /// Whether `<common>/mdkg` still names this folder: not renamed away,
    /// replaced, or made a link (orchestrator #6822).
    pub fn still_named(&self) -> io::Result<bool> {
        match rustix::fs::statat(rustix::fs::CWD, &self.path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(now) => Ok(file_id(&now) == self.id),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `name` in this folder: never through a link, never blocking
    /// (a FIFO is refused, not waited on), and only a regular file of this
    /// user with one link. `create` makes it 0600 if missing, whatever
    /// the umask, so no later open is locked out of it (Terra 3 #6773).
    fn file(&self, name: &str, create: bool, write: bool) -> io::Result<File> {
        let mut flags = OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        flags |= if write { OFlags::RDWR } else { OFlags::RDONLY };
        let mode = Mode::from_raw_mode(0o600);
        // Errors name the file, so a refused open says where (#7439).
        let named = |e: rustix::io::Errno| {
            let e = io::Error::from(e);
            io::Error::new(e.kind(), format!("{}: {e}", self.path.join(name).display()))
        };
        let fd = if create {
            match rustix::fs::openat(&self.fd, name, flags | OFlags::CREATE | OFlags::EXCL, mode) {
                Ok(fd) => {
                    // A failed chmod would strand a file of the umask's
                    // mode, 000 under 0777, that the next start can't
                    // open: it is removed again (Terra 4 #8518), but only
                    // while unheld and still at its name (#8759).
                    if let Err(e) = chmod_file(&fd, mode) {
                        let synced = self.discard_new(name, fd);
                        return Err(with_sync(named(e), synced));
                    }
                    fd
                }
                Err(rustix::io::Errno::EXIST) => {
                    rustix::fs::openat(&self.fd, name, flags, mode).map_err(named)?
                }
                Err(e) => return Err(named(e)),
            }
        } else {
            rustix::fs::openat(&self.fd, name, flags, mode).map_err(named)?
        };
        let st = rustix::fs::fstat(&fd)?;
        let ok = FileType::from_raw_mode(st.st_mode as _) == FileType::RegularFile
            && st.st_uid == own_uid()
            && st.st_nlink == 1;
        if !ok {
            return Err(other(format!(
                "{} isn't a regular file of this user with one link",
                self.path.join(name).display()
            )));
        }
        Ok(File::from(fd))
    }

    /// Removes `name`, which this call just created as `fd`, after a
    /// failed chmod: only if nobody else holds its lock and the name still
    /// names that inode. Another process may have opened, locked and
    /// validated the new file in between; its lock is then live, and the
    /// file stays (Terra 4 #8759, orchestrator #8777).
    /// A failed removal's sync is returned, for the caller to report.
    fn discard_new(&self, name: &str, fd: OwnedFd) -> io::Result<()> {
        let file = File::from(fd);
        if file.try_lock().is_err() {
            return Ok(());
        }
        let (Ok(mine), Ok(named)) = (
            rustix::fs::fstat(&file),
            rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW),
        ) else {
            return Ok(());
        };
        if (mine.st_dev, mine.st_ino) == (named.st_dev, named.st_ino)
            && rustix::fs::unlinkat(&self.fd, name, AtFlags::empty()).is_ok()
        {
            // Durable, so a crash can't bring the entry back (Terra 2
            // #8870).
            return sync_dir(&self.fd);
        }
        Ok(())
    }

    /// `name`'s file, or `None` when it doesn't exist.
    fn existing(&self, name: &str, write: bool) -> io::Result<Option<File>> {
        match self.file(name, false, write) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `name` is locked by someone, probed with a shared try-lock
    /// released at once. An exclusive start in the same instant can be
    /// refused once; it never waits.
    fn held(&self, name: &str) -> io::Result<bool> {
        let Some(f) = self.existing(name, false)? else {
            return Ok(false);
        };
        match f.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// `serve.live`'s holder and stamp, read through one descriptor, so
    /// both describe the same file.
    fn live(&self) -> io::Result<Option<(bool, Vec<u8>)>> {
        let Some(mut f) = self.existing(LIVE_FILE, false)? else {
            return Ok(None);
        };
        let held = match f.try_lock_shared() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(e)) => return Err(e),
        };
        let mut stamp = Vec::new();
        Read::by_ref(&mut f).take(1024).read_to_end(&mut stamp)?;
        Ok(Some((held, stamp)))
    }

    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(mut f) = self.existing(name, false)? else {
            return Ok(None);
        };
        let mut out = Vec::new();
        Read::by_ref(&mut f).take(64 * 1024).read_to_end(&mut out)?;
        Ok(Some(out))
    }
}

/// Why the lock wasn't taken.
#[derive(Debug)]
pub enum LockError {
    /// Another `serve` holds it: `api.serve_running`.
    Held,
    /// A lease-mode switch held it for longer than the start would wait.
    Switching,
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held => f.write_str("another serve holds this repository's serve lock"),
            LockError::Switching => f.write_str("a lease-mode switch still holds the serve lock"),
            LockError::Io(e) => write!(f, "can't take the serve lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// The held `serve` lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct ServeLock {
    dir: Dir,
    file: File,
    /// `serve.live`, once this `serve` has announced itself.
    live: OnceLock<File>,
    /// Held across a whole [`ServeState::write`], so concurrent writers in
    /// this process announce once and rename one file at a time (Terra 1
    /// #7077). True once [`ServeState::remove`] ran: a writer that queued
    /// behind it writes nothing (Terra 6 #8208).
    writing: Mutex<bool>,
    /// This start's serve id, minted when the lock is taken: fresh at
    /// every start, the same through boot-id rotations (Terra 7 #7089).
    serve_id: String,
}

impl ServeLock {
    /// Takes `<common>/mdkg/serve.lock` exclusively, without waiting.
    /// Once locked, the path must still name the locked file (#6716): one
    /// unlinked or replaced between the open and the lock is opened again,
    /// a few times, then refused.
    pub fn acquire(common: &Path) -> Result<Self, LockError> {
        let dir = Dir::open(common, true).map_err(LockError::Io)?;
        let lock = Self::acquire_in(dir, &mut |d| {
            d.file(LOCK_FILE, true, true)
                .map_err(|e| unopenable_lock(d, e))
        })?;
        lock.sweep_temps();
        Ok(lock)
    }

    /// Removes temp files a crashed `serve` left between creating them
    /// and renaming them into place (`.serve.live.<pid>.tmp`,
    /// `.serve.json.<pid>.tmp`). Only the lock holder runs this, so no
    /// live `serve` is mid-rename (#7073).
    fn sweep_temps(&self) {
        let Ok(dir) = rustix::fs::Dir::read_from(&self.dir.fd) else {
            return;
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_temp_name(&name) {
                continue;
            }
            // Only a regular file of this user, never a link or a folder
            // someone left under a matching name (Terra 1 #7461).
            let ours = rustix::fs::statat(&self.dir.fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|st| {
                    FileType::from_raw_mode(st.st_mode as _) == FileType::RegularFile
                        && st.st_uid == own_uid()
                });
            if ours {
                let _ = rustix::fs::unlinkat(&self.dir.fd, name.as_str(), AtFlags::empty());
            }
        }
    }

    /// [`ServeLock::acquire`] in an open folder; tests replace `open` to
    /// swap the file between the open and the lock.
    fn acquire_in(
        dir: Dir,
        open: &mut dyn FnMut(&Dir) -> io::Result<File>,
    ) -> Result<Self, LockError> {
        for _ in 0..3 {
            let file = open(&dir).map_err(LockError::Io)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(LockError::Held),
                Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
            }
            if names(&dir, LOCK_FILE, &file).map_err(LockError::Io)? {
                return Ok(Self {
                    dir,
                    file,
                    live: OnceLock::new(),
                    writing: Mutex::new(false),
                    serve_id: mint_serve_id().map_err(LockError::Io)?,
                });
            }
        }
        Err(LockError::Io(io::Error::other(
            "serve.lock keeps being replaced while it is taken",
        )))
    }

    /// Takes the lock, waiting up to `wait` while a lease-mode switch
    /// holds it (SC55). A holder that isn't a switch is another `serve`,
    /// refused at once.
    pub fn acquire_after_switch(common: &Path, wait: Duration) -> Result<Self, LockError> {
        let start = Instant::now();
        loop {
            match Self::acquire(common) {
                Err(LockError::Held) if switching(common).map_err(LockError::Io)? => {
                    if start.elapsed() >= wait {
                        return Err(LockError::Switching);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => return other,
            }
        }
    }

    /// The serve id this start writes to `serve.json` and stamps on
    /// `serve.live`.
    pub fn serve_id(&self) -> &str {
        &self.serve_id
    }

    pub fn dir(&self) -> &Dir {
        &self.dir
    }

    /// Whether this `serve` still holds what others see: `<common>/mdkg`
    /// is still the folder it opened (orchestrator #6822), `serve.lock`
    /// still names the locked file, and once announced `serve.live` still
    /// names its locked file (Terra 3 #6773). Anyone who can remove names
    /// there could otherwise let a second `serve` or a switch lock new
    /// files beside it. The 10 s timer calls this and stops `serve`, with
    /// an error logged, when it fails.
    pub fn still_held(&self) -> io::Result<bool> {
        Ok(self.dir.still_named()?
            && names(&self.dir, LOCK_FILE, &self.file)?
            && match self.live.get() {
                Some(live) => names(&self.dir, LIVE_FILE, live)?,
                None => true,
            })
    }
}

/// A fresh serve id: a random (version 4) UUID.
fn mint_serve_id() -> io::Result<String> {
    let mut b = crate::random::bytes::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crate::random::hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// Whether `name` is one of `serve`'s temp names: `.<file>.<pid>.tmp` or
/// `.<file>.<pid>.<n>.tmp`, with decimal pid and counter.
fn is_temp_name(name: &str) -> bool {
    [LIVE_FILE, STATE_FILE].iter().any(|f| {
        name.strip_prefix(&format!(".{f}."))
            .and_then(|r| r.strip_suffix(".tmp"))
            .is_some_and(|mid| {
                // Only names a writer can make: a pid (a positive pid_t,
                // so 1 to i32::MAX) and, in the current form, a counter (a
                // u64), each written without leading zeroes (Terra 6
                // #8208, Terra 4 #8518, #8759).
                let canonical = |p: &str, min: u64, max: u64| {
                    p.parse::<u64>()
                        .is_ok_and(|n| (min..=max).contains(&n) && n.to_string() == p)
                };
                let pid = |p: &str| canonical(p, 1, i32::MAX as u64);
                match mid.split_once('.') {
                    None => pid(mid),
                    Some((p, n)) => pid(p) && canonical(n, 0, u64::MAX),
                }
            })
    })
}

/// A lock file that exists but can't be opened read-write: a failed
/// chmod left it so (a stated residual; orchestrator #8879, Terra 2
/// #8870). The refusal names the file and its one repair.
fn unopenable_lock(dir: &Dir, e: io::Error) -> io::Error {
    if e.kind() != io::ErrorKind::PermissionDenied {
        return e;
    }
    // Only a regular file of ours, with one link, that its owner can't
    // read and write: not a chmod that failed on a usable file, nor a
    // FIFO, a foreign file or a link, which their own refusals say
    // (Terra 3 #8943).
    let st = match rustix::fs::statat(&dir.fd, LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        // A folder that opens but that its owner can't search (0600,
        // left by a crash before its chmod) denies even the stat: the
        // folder is what needs repair (Terra 1 #9025).
        Err(rustix::io::Errno::ACCESS) => {
            let folder = rustix::fs::fstat(&dir.fd).map(|d| mode_bits(&d));
            if folder.is_ok_and(|m| m & 0o100 == 0) {
                let named = match dir.path().to_str() {
                    Some(p) => p.to_owned(),
                    None => format!(
                        "the folder shown as {} (a path that isn't valid UTF-8, so shown \
                         approximately)",
                        dir.path().display()
                    ),
                };
                return io::Error::new(e.kind(), dir_hint(&named, own_uid(), own_uid(), &e));
            }
            return e;
        }
        Err(_) => return e,
    };
    let mode = st.st_mode as u32;
    if FileType::from_raw_mode(st.st_mode as _) != FileType::RegularFile
        || st.st_uid != own_uid()
        || st.st_nlink != 1
        || mode & 0o600 == 0o600
    {
        return e;
    }
    let folder_mode = rustix::fs::fstat(&dir.fd)
        .map(|d| mode_bits(&d))
        .unwrap_or(0);
    let read_only = folder_mode & 0o200 == 0;
    io::Error::new(
        e.kind(),
        lock_repair(&dir.path().join(LOCK_FILE), &e, read_only),
    )
}

/// What to check before a manual repair: a running `serve`, or a
/// lease-mode switch, holds the same lock (orchestrator #8948).
fn quiescent() -> String {
    let bin = mdkg_format::program::BIN;
    format!(
        "once neither `{bin} serve` nor `{bin} store lease-mode` is running for this \
         repository (`pgrep -fl {bin}` lists its processes)"
    )
}

/// The lock file's repair, naming it and never as a pasteable command:
/// the display of a path that isn't valid UTF-8 is lossy, so it is only
/// a label there, and the folder is named instead (orchestrator #8948,
/// Terra 3 #8943).
fn lock_repair(path: &Path, e: &io::Error, folder_read_only: bool) -> String {
    let named = match path.to_str() {
        Some(p) => format!("{p} can't be opened ({e})"),
        None => format!(
            "the lock file {LOCK_FILE} in the folder shown as {} (a path that isn't valid \
             UTF-8, so shown approximately) can't be opened ({e})",
            path.parent().unwrap_or(path).display()
        ),
    };
    let first = if folder_read_only {
        "first give its folder back its owner's write bit, which this folder lacks, then "
    } else {
        ""
    };
    format!("{named}; {}, {first}remove that file", quiescent())
}

/// A full barrier on a folder: `F_FULLFSYNC` on macOS, where a plain
/// fsync leaves the drive's cache (Terra 1 #8926), and fsync elsewhere.
fn sync_dir(fd: &OwnedFd) -> io::Result<()> {
    #[cfg(test)]
    tests::SYNCS.with(|n| n.set(n.get() + 1));
    // On the descriptor itself, never a duplicate: at the descriptor limit
    // a dup fails with EMFILE after the rename, though the barrier would
    // succeed (Terra 5 #9039).
    // EINTR is retried, as `File::sync_all` did (Terra 1 #9126). A test's
    // fault stands in for one attempt's result, so the retry itself is
    // what it exercises (Terra 3 #9195).
    loop {
        #[cfg(test)]
        let injected = tests::SYNC_FAULT.with(|f| f.borrow_mut().pop_front());
        #[cfg(not(test))]
        let injected: Option<rustix::io::Errno> = None;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let barrier = || rustix::fs::fcntl_fullfsync(fd);
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let barrier = || rustix::fs::fsync(fd);
        let synced = match injected {
            Some(e) => Err(e),
            None => barrier(),
        };
        match synced {
            Err(rustix::io::Errno::INTR) => continue,
            r => return r.map_err(io::Error::from),
        }
    }
}

/// `e`, saying so when the removal that followed it may not be durable.
fn with_sync(e: io::Error, synced: io::Result<()>) -> io::Error {
    match synced {
        Ok(()) => e,
        Err(s) => io::Error::new(
            e.kind(),
            format!("{e}; the removal that followed may not survive a crash: {s}"),
        ),
    }
}

/// `<common>/mdkg` that exists but can't be opened: a failed mode-set
/// left it so (a stated residual, as the lock file's; orchestrator
/// #8930, Terra 1 #8926). The refusal names the folder and its one
/// repair; the folder may hold the store, so it is never removed.
fn unopenable_dir(parent: &OwnedFd, path: &Path, e: rustix::io::Errno) -> io::Error {
    let e = io::Error::from(e);
    if e.kind() != io::ErrorKind::PermissionDenied {
        return e;
    }
    // Inspected through the parent's descriptor, without following a
    // link: a folder another user owns (root, say) can't be repaired by
    // its mode, so that is said instead (Terra 3 #9022).
    let Ok(st) = rustix::fs::statat(parent, DIR, AtFlags::SYMLINK_NOFOLLOW) else {
        return e;
    };
    let named = match path.to_str() {
        Some(p) => p.to_owned(),
        None => format!(
            "the folder shown as {} (a path that isn't valid UTF-8, so shown approximately)",
            path.display()
        ),
    };
    io::Error::new(e.kind(), dir_hint(&named, st.st_uid, own_uid(), &e))
}

/// The unopenable folder's hint: its owner's repair, or, for a folder
/// another user owns, that it isn't this user's. Named, never as a
/// pasteable command, as the lock file's (#8948).
fn dir_hint(named: &str, owner: u32, me: u32, e: &io::Error) -> String {
    if owner != me {
        return format!(
            "{named} can't be opened ({e}): it belongs to another user (uid {owner}), so its \
             mode is not this user's to repair; its owner must give it back"
        );
    }
    format!(
        "{named} can't be opened ({e}); {}, restore that folder's mode to 0700",
        quiescent()
    )
}

/// The fresh entries' chmods, with a fault to inject in tests.
fn chmod_file(fd: &OwnedFd, mode: Mode) -> rustix::io::Result<()> {
    #[cfg(test)]
    if let Some(fault) = tests::FAULT.with(|f| f.borrow_mut().take()) {
        return fault();
    }
    rustix::fs::fchmod(fd, mode)
}

fn chmod_dir(parent: &OwnedFd, name: &str) -> rustix::io::Result<()> {
    #[cfg(test)]
    if let Some(fault) = tests::FAULT.with(|f| f.borrow_mut().take()) {
        return fault();
    }
    rustix::fs::chmodat(parent, name, Mode::from_raw_mode(0o700), AtFlags::empty())
}

/// `s` as one shell word: in single quotes, each `'` closed, escaped and
/// reopened.
fn shell_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A temp file's name for `file`, unique per call: the pid and a counter,
/// so no two writes ever share one (Terra 1 #7077). The start's sweep
/// removes any a crash left.
fn temp_name(file: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(".{file}.{}.{n}.tmp", std::process::id())
}

/// Whether `name` in `dir` names `file`'s inode.
fn names(dir: &Dir, name: &str, file: &File) -> io::Result<bool> {
    let held = rustix::fs::fstat(file.as_fd())?;
    match rustix::fs::statat(&dir.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(now) => Ok(now.st_ino == held.st_ino && now.st_dev == held.st_dev),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether a lease-mode switch is in progress: its lock is held
/// exclusively. Delegations hold it shared, which this doesn't count.
pub fn switching(common: &Path) -> io::Result<bool> {
    let dir = match Dir::open(common, false) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(f) = dir.existing(LEASE_MODE_LOCK, false)? else {
        return Ok(false);
    };
    match f.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Whether some process holds `serve.lock` now: a `serve`, or a switch.
pub fn lock_held(common: &Path) -> io::Result<bool> {
    match Dir::open(common, false) {
        Ok(dir) => dir.held(LOCK_FILE),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `serve.json`: `mdkg.serve.state.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeState {
    pub schema: String,
    pub serve_id: String,
    pub boot_id: String,
    pub pid: u32,
    /// RFC 3339, UTC.
    pub started_at: String,
    /// The socket's path (`<common>/mdkg/local.sock` for serve-local), or
    /// its short fallback.
    pub socket: String,
    pub build: String,
    /// The TCP address, when `--listen` gave one.
    pub tcp: Option<String>,
    /// The loopback token file's path, never the token, when `--listen`
    /// gave an address: a client attaching later finds it here (#12110).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
}

/// What `serve status` and `doctor` find on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// No `serve.json`.
    None,
    /// The `serve` that `serve.json` names is live.
    Running(ServeState),
    /// A `serve.json` whose `serve` is gone.
    Stale(ServeState),
    /// `serve.live` is held, by a `serve` other than the one this file
    /// names: one that started and hasn't written its file yet.
    HeldByOther(ServeState),
    /// A `serve.json` that doesn't parse, or names another schema.
    Unreadable,
}

impl ServeState {
    /// Announces this `serve`: locks `serve.live` and stamps it with the
    /// serve id (once), then writes `serve.json` by temp file, fsync,
    /// rename and folder fsync, mode 0600, all inside the locked folder.
    /// A boot-id rotation calls this again to rewrite the file. Once
    /// [`ServeState::remove`] ran, this writes nothing, however the two
    /// were queued: a clean stop leaves no `serve.json` behind.
    pub fn write(&self, lock: &ServeLock) -> io::Result<()> {
        let removed = lock.writing.lock().unwrap_or_else(|e| e.into_inner());
        if *removed {
            return Ok(());
        }
        if self.serve_id != lock.serve_id {
            // A start that reused a crashed serve's id would make that
            // serve's serve.json read as running (Terra 7 #7089).
            return Err(other("serve.json must carry this start's serve_id".into()));
        }
        self.announce(lock)?;
        let dir = &lock.dir;
        let tmp = temp_name(STATE_FILE);
        let json = serde_json::to_vec(self).map_err(io::Error::other)?;
        let fd = rustix::fs::openat(
            &dir.fd,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let placed = (|| {
            // The mode at creation honours the umask; set it exactly.
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o600))?;
            let mut f = File::from(fd);
            f.write_all(&json)?;
            f.sync_all()?;
            rustix::fs::renameat(&dir.fd, tmp.as_str(), &dir.fd, STATE_FILE)?;
            sync_dir(&dir.fd)?;
            Ok::<_, io::Error>(())
        })();
        if placed.is_err() {
            // A failed write leaves no temp file behind (Terra 1 #7487).
            let _ = rustix::fs::unlinkat(&dir.fd, tmp.as_str(), AtFlags::empty());
        }
        placed
    }

    /// Puts a new `serve.live` in place, already locked and stamped: a
    /// temp file is stamped, locked and renamed over the old one, so no
    /// reader ever sees this `serve`'s lock on another's stamp. A crashed
    /// `serve`'s file is unlocked, so it reads stale until the rename, and
    /// after it the stamp names this `serve`, which `serve.json` doesn't
    /// yet (Terra 3 #6773). Once per `serve`.
    fn announce(&self, lock: &ServeLock) -> io::Result<()> {
        if lock.live.get().is_some() {
            return Ok(());
        }
        let dir = &lock.dir;
        if dir.held(LIVE_FILE)? {
            return Err(other("serve.live is held by another process".into()));
        }
        let tmp = temp_name(LIVE_FILE);
        let _ = rustix::fs::unlinkat(&dir.fd, tmp.as_str(), AtFlags::empty());
        let live = dir.file(&tmp, true, true)?;
        let placed = (|| {
            live.try_lock().map_err(|e| match e {
                TryLockError::WouldBlock => other("a new serve.live is locked".into()),
                TryLockError::Error(e) => e,
            })?;
            // Emptied first: should the unlink above have failed and an
            // old temp file been reopened, none of its bytes can follow
            // the stamp (Terra 1 #7077).
            live.set_len(0)?;
            live.write_all_at(self.serve_id.as_bytes(), 0)?;
            live.sync_all()?;
            rustix::fs::renameat(&dir.fd, tmp.as_str(), &dir.fd, LIVE_FILE)?;
            sync_dir(&dir.fd)?;
            Ok::<_, io::Error>(())
        })();
        if let Err(e) = placed {
            let _ = rustix::fs::unlinkat(&dir.fd, tmp.as_str(), AtFlags::empty());
            return Err(e);
        }
        let _ = lock.live.set(live);
        Ok(())
    }

    /// Removes `serve.json` on a clean stop, after any write in flight and
    /// before any that comes later, which then writes nothing.
    pub fn remove(lock: &ServeLock) -> io::Result<()> {
        // After any write in flight, never between its steps (Opus #7484);
        // and no write follows it (Terra 6 #8208).
        let mut removed = lock.writing.lock().unwrap_or_else(|e| e.into_inner());
        *removed = true;
        match rustix::fs::unlinkat(&lock.dir.fd, STATE_FILE, AtFlags::empty()) {
            // Synced like the publish, so a crash can't bring it back
            // (Opus #9032).
            // Already gone, perhaps by a removal whose sync failed: synced
            // anyway, so a retry makes it durable (Terra 1 #9126).
            Ok(()) | Err(rustix::io::Errno::NOENT) => sync_dir(&lock.dir.fd),
            Err(e) => Err(e.into()),
        }
    }

    fn parse(bytes: &[u8]) -> Result<ServeState, ()> {
        serde_json::from_slice::<ServeState>(bytes)
            .ok()
            .filter(|s| s.schema == STATE_SCHEMA)
            .ok_or(())
    }

    /// Reads `serve.json` and tells whether its `serve` is the live one.
    /// The file is read again after the probe; when it changed in between
    /// (a start or a stop raced the probe), the whole check runs again.
    pub fn find(common: &Path) -> io::Result<Found> {
        let dir = match Dir::open(common, false) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Found::None),
            Err(e) => return Err(e),
        };
        for _ in 0..3 {
            let state = match dir.read(STATE_FILE)? {
                None => return Ok(Found::None),
                Some(bytes) => match Self::parse(&bytes) {
                    Ok(s) => s,
                    Err(()) => return Ok(Found::Unreadable),
                },
            };
            let found = match dir.live()? {
                None | Some((false, _)) => Found::Stale(state.clone()),
                Some((true, stamp)) if stamp == state.serve_id.as_bytes() => {
                    Found::Running(state.clone())
                }
                Some((true, _)) => Found::HeldByOther(state.clone()),
            };
            let again = dir.read(STATE_FILE)?.map(|b| Self::parse(&b));
            if again == Some(Ok(state)) {
                return Ok(found);
            }
        }
        Ok(Found::Unreadable)
    }
}

/// A stat's mode as u32. `st_mode` is u16 on macOS and u32 on Linux, so the
/// cast is needed on one and redundant on the other; inside a closure clippy
/// no longer sees the `mode_t` alias that keeps it quiet elsewhere (Ubuntu
/// CI, 0.7.1).
#[allow(clippy::unnecessary_cast)]
fn mode_bits(st: &rustix::fs::Stat) -> u32 {
    st.st_mode as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// One state test at a time: the FIFO test spawns `mkfifo`, whose
    /// child holds every open descriptor between its fork and exec, so a
    /// lock another test just dropped could still read as held.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-serve-state-{}-{}",
            std::process::id(),
            crate::random::hex(&crate::random::bytes::<6>().expect("urandom"))
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The state a start writes, with that start's serve id.
    fn state(lock: &ServeLock) -> ServeState {
        ServeState {
            schema: STATE_SCHEMA.into(),
            serve_id: lock.serve_id().into(),
            boot_id: "00112233445566778899aabbccddeeff".into(),
            pid: std::process::id(),
            started_at: "2026-10-29T18:00:00Z".into(),
            socket: "/tmp/x/api.sock".into(),
            build: "test".into(),
            tcp: None,
            token_file: None,
        }
    }

    fn mode_of(p: &Path) -> u32 {
        // Special bits too (setuid, setgid, sticky): #7478.
        fs::metadata(p).expect("meta").permissions().mode() & 0o7777
    }

    #[test]
    fn a_second_serve_is_refused_and_the_lock_frees_on_drop() {
        let _serial = serial();
        let common = temp();
        let first = ServeLock::acquire(&common).expect("first");
        assert!(matches!(ServeLock::acquire(&common), Err(LockError::Held)));
        assert!(lock_held(&common).expect("probe"));
        assert!(first.still_held().expect("identity"));
        drop(first);
        assert!(!lock_held(&common).expect("probe"));
        ServeLock::acquire(&common).expect("after release");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn serve_json_is_0600_running_while_live_stale_after() {
        let _serial = serial();
        let common = temp();
        assert_eq!(ServeState::find(&common).expect("find"), Found::None);
        let lock = ServeLock::acquire(&common).expect("lock");
        let first = state(&lock);
        first.write(&lock).expect("write");
        assert_eq!(mode_of(&mdkg_dir(&common).join(STATE_FILE)), 0o600);
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Running(first.clone())
        );
        // A crash: the locks go, the file stays.
        drop(lock);
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Stale(first.clone())
        );
        // The next start replaces it, a rotation rewrites it, and a clean
        // stop removes it.
        let lock = ServeLock::acquire(&common).expect("relock");
        let mut next = state(&lock);
        assert_ne!(next.serve_id, first.serve_id, "a fresh id per start");
        next.write(&lock).expect("rewrite");
        next.boot_id = "ffeeddccbbaa99887766554433221100".into();
        next.write(&lock).expect("rotation");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Running(next)
        );
        ServeState::remove(&lock).expect("remove");
        assert_eq!(ServeState::find(&common).expect("find"), Found::None);
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_foreign_file_is_unreadable() {
        let _serial = serial();
        let common = temp();
        fs::create_dir_all(mdkg_dir(&common)).expect("dir");
        fs::write(mdkg_dir(&common).join(STATE_FILE), b"{\"schema\":\"x\"}").expect("w");
        assert_eq!(ServeState::find(&common).expect("find"), Found::Unreadable);
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn an_existing_folder_only_ever_loses_bits() {
        // Orchestrator's amended #6822 (#7076): the store makes the folder
        // with the default mode, so 0755 is tightened to 0700; nothing is
        // ever added, so 0500 stays 0500.
        let _serial = serial();
        let common = temp();
        let dir = mdkg_dir(&common);
        fs::create_dir_all(&dir).expect("dir");
        for (from, to) in [
            (0o755, 0o700),
            (0o775, 0o700),
            (0o750, 0o700),
            (0o700, 0o700),
            (0o2700, 0o700),
            (0o2755, 0o700),
        ] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(from)).expect("chmod");
            drop(ServeLock::acquire(&common).expect("serve starts"));
            assert_eq!(mode_of(&dir), to, "{from:o}");
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        // Its existing lock file still opens; the folder isn't widened.
        drop(ServeLock::acquire(&common));
        assert_eq!(mode_of(&dir), 0o500, "owner write is never added");
        // 0555 loses only the others' bits (#7461).
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("chmod");
        drop(ServeLock::acquire(&common));
        assert_eq!(mode_of(&dir), 0o500);
        // Without its lock file, a 0500 folder refuses to start, naming
        // the file (#7439). Root overrides the mode, so not as root
        // (Terra 6 #8208).
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm lock");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        if own_uid() != 0 {
            match ServeLock::acquire(&common) {
                Err(LockError::Io(e)) => assert!(e.to_string().contains(LOCK_FILE), "{e}"),
                other => panic!("{other:?}"),
            }
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::remove_dir_all(&common).ok();
        // Made by serve, whatever the umask.
        let common = temp();
        let old = rustix::process::umask(Mode::from_raw_mode(0o077));
        drop(ServeLock::acquire(&common).expect("lock"));
        rustix::process::umask(old);
        assert_eq!(mode_of(&mdkg_dir(&common)), 0o700);
        fs::remove_dir_all(&common).ok();
        // Not a folder: refused.
        let common = temp();
        fs::write(mdkg_dir(&common), b"x").expect("file");
        assert!(matches!(ServeLock::acquire(&common), Err(LockError::Io(_))));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn every_start_has_a_fresh_serve_id_and_writes_only_its_own() {
        // Terra 7 #7089: discovery tells starts apart by serve id, so a
        // restart can't reuse a crashed serve's id; rotations keep it.
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        let crashed = state(&lock);
        crashed.write(&lock).expect("write");
        drop(lock);
        let lock = ServeLock::acquire(&common).expect("restart");
        assert_ne!(lock.serve_id(), crashed.serve_id);
        assert_eq!(lock.serve_id().len(), 36);
        assert!(crashed.write(&lock).is_err(), "the old id is refused");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Stale(crashed),
            "nothing was written"
        );
        let mut mine = state(&lock);
        mine.write(&lock).expect("write");
        mine.boot_id = "ffeeddccbbaa99887766554433221100".into();
        mine.write(&lock).expect("a rotation keeps the id");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Running(mine)
        );
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn concurrent_first_writes_announce_once_and_stay_running() {
        // Terra 1 #7077: two threads writing serve.json at once on one
        // lock publish exactly one locked serve.live, the one held.
        let _serial = serial();
        let common = temp();
        for _ in 0..20 {
            let lock = ServeLock::acquire(&common).expect("lock");
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| state(&lock).write(&lock).expect("write"));
                }
            });
            assert_eq!(
                ServeState::find(&common).expect("find"),
                Found::Running(state(&lock))
            );
            assert!(
                lock.still_held().expect("probe"),
                "the held live file is the named one"
            );
            let leftovers = fs::read_dir(mdkg_dir(&common))
                .expect("dir")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .count();
            assert_eq!(leftovers, 0);
        }
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn serve_refuses_a_parent_others_can_write_and_a_fresh_folder_survives_umask_0777() {
        // Orchestrator #7478: <common> must be ours and writable by no one
        // else unless sticky; and a folder serve makes under umask 0777
        // still opens and ends up 0700.
        let _serial = serial();
        for (mode, ok) in [
            (0o777, false),
            (0o775, false),
            (0o1777, true),
            (0o755, true),
        ] {
            let common = temp();
            fs::set_permissions(&common, fs::Permissions::from_mode(mode)).expect("chmod");
            let r = ServeLock::acquire(&common);
            assert_eq!(r.is_ok(), ok, "{mode:o}: {r:?}");
            fs::set_permissions(&common, fs::Permissions::from_mode(0o700)).expect("chmod");
            fs::remove_dir_all(&common).ok();
        }
        let common = temp();
        let old = rustix::process::umask(Mode::from_raw_mode(0o777));
        let r = ServeLock::acquire(&common);
        rustix::process::umask(old);
        r.expect("a fresh folder under umask 0777");
        assert_eq!(mode_of(&mdkg_dir(&common)), 0o700);
        fs::remove_dir_all(&common).ok();
    }

    const RELEASED: &str = "the probe blocked on the FIFO until released";

    /// The released probe's failure, carrying what it answered, so a test
    /// can check the reader got its byte (Terra 1 #8967).
    fn released(answer: bool) -> String {
        format!("{RELEASED}, answering {answer}")
    }

    /// Runs `probe` on a detached thread and waits at most 2 s for its
    /// answer. If it hasn't answered, the FIFO's write end is opened
    /// without blocking, which releases a probe blocked opening it as a
    /// reader; one byte is written and the writer held until the probe
    /// answers, at most 2 s, then dropped, so a probe reading to the end
    /// sees end of file. The answer shows the probe returned; the thread is never
    /// joined, since its end can wait on a thread-local destructor, so
    /// this never hangs (#7974, #8010, Terra 6 #8235, Terra 4 #8518).
    fn bounded_fifo_probe<F>(fifo: &Path, probe: F) -> Result<bool, String>
    where
        F: FnOnce() -> bool + Send + 'static,
    {
        use std::os::unix::fs::OpenOptionsExt;
        let (tx, rx) = std::sync::mpsc::channel();
        // Detached: its answer shows the probe returned, and nothing waits
        // on the thread's end, which a thread-local destructor can delay
        // however long (Terra 4 #8518).
        drop(std::thread::spawn(move || {
            let _ = tx.send(probe());
        }));
        if let Ok(v) = rx.recv_timeout(Duration::from_secs(2)) {
            return Ok(v);
        }
        let writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits() as i32)
            .open(fifo);
        match writer {
            Ok(mut w) => {
                // A handshake, not a timing guess (Terra 2 #8805): the
                // writer is held, with one byte for the reader, until the
                // probe answers. A reader blocked opening the FIFO re-checks
                // for a writer when it wakes (BSD's fifo_open), so it must
                // find one; a reader that wants end of file gets it once
                // the writer goes, in the second wait.
                use std::io::Write;
                let _ = w.write_all(b"x");
                if let Ok(v) = rx.recv_timeout(Duration::from_secs(2)) {
                    return Err(released(v));
                }
                drop(w);
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(v) => Err(released(v)),
                    Err(_) => Err("the probe stayed blocked after its release".into()),
                }
            }
            Err(e) => Err(format!("the probe blocked elsewhere ({e})")),
        }
    }

    #[test]
    fn the_fifo_probe_helper_fails_fast_for_a_blocked_probe() {
        // #8010: exercise the timeout path with deliberately blocked
        // probes: one that opens the FIFO and reads it to the end, and one
        // stuck elsewhere. Neither hangs the test.
        let _serial = serial();
        let common = temp();
        let fifo = common.join("probe.fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let reader = fifo.clone();
        let t = Instant::now();
        let r = bounded_fifo_probe(&fifo, move || {
            let mut byte = [0u8; 1];
            fs::File::open(&reader)
                .and_then(|mut f| f.read_exact(&mut byte))
                .is_ok()
        });
        // Released by the handshake and answered, whatever the scheduling
        // (Terra 6 #8235, Terra 2 #8805).
        assert_eq!(
            r,
            Err(released(true)),
            "released, and the reader got its byte"
        );
        // A reader that wants end of file: released by the handshake,
        // answered once the writer is dropped (Terra 1 #8887).
        let to_eof = fifo.clone();
        let r = bounded_fifo_probe(&fifo, move || {
            let mut text = String::new();
            fs::File::open(&to_eof)
                .and_then(|mut f| f.read_to_string(&mut text))
                .is_ok_and(|_| text == "x")
        });
        assert_eq!(
            r,
            Err(released(true)),
            "released, and the reader got its byte"
        );
        let r = bounded_fifo_probe(&fifo, || {
            std::thread::sleep(Duration::from_secs(3600));
            true
        });
        assert!(
            r.as_ref()
                .is_err_and(|e| e.starts_with("the probe blocked elsewhere")),
            "{r:?}"
        );
        // A probe whose thread-local destructor never ends still answers
        // at once: nothing waits on the thread (Terra 4 #8518).
        struct Stuck;
        impl Drop for Stuck {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        thread_local!(static STUCK: Stuck = const { Stuck });
        let quick = Instant::now();
        let r = bounded_fifo_probe(&fifo, || {
            STUCK.with(|_| {});
            true
        });
        assert_eq!(r, Ok(true));
        assert!(quick.elapsed() < Duration::from_secs(1));
        // Three released probes of about 2 s, 4 s and 2 s, plus a little.
        assert!(t.elapsed() < Duration::from_secs(14));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn the_no_key_check_passes() {
        // Opus #7891, Terra 5 #7897: tools/check_no_key.py re-derives
        // enroll.rs's NO_KEY and checks it is a canonical prime-order
        // point; running it here makes the check part of the suite. It
        // spawns a child, so it holds the state tests' mutex: a child holds
        // every descriptor between its fork and exec.
        let _serial = serial();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/check_no_key.py");
        match std::process::Command::new("python3")
            .arg("-I")
            .arg(&script)
            .output()
        {
            Ok(out) => assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            // No Python on this machine: said so, and a failure under CI,
            // so a runner can't lose the check unnoticed (Opus #8139). The
            // harness shows the line only with `--show-output` (or on a
            // failure); libtest has no runtime skip (Terra 2 #8457).
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "python3 is missing under CI: {} can't run",
                    script.display()
                );
                eprintln!("skipped: no python3 to run {}", script.display());
            }
            Err(e) => panic!("python3: {e}"),
        }
    }

    #[test]
    fn a_write_queued_behind_the_removal_writes_nothing() {
        // Terra 6 #8208: a boot-id rotation's rewrite that takes the lock
        // after a clean stop's removal leaves no serve.json behind.
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        state(&lock).write(&lock).expect("write");
        // The removal took the lock first; the rewrite comes after it.
        ServeState::remove(&lock).expect("remove");
        state(&lock)
            .write(&lock)
            .expect("writes nothing, without error");
        assert!(!mdkg_dir(&common).join(STATE_FILE).exists());
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_write_racing_the_removal_never_leaves_serve_json() {
        // Terra 4 #8518: a rewrite and a clean stop's removal released
        // together, many times. Whichever takes the lock first, the file
        // is gone at the end: a write that released the lock before its
        // rename would sometimes leave it.
        let _serial = serial();
        let common = temp();
        for round in 0..40 {
            let lock = ServeLock::acquire(&common).expect("lock");
            state(&lock).write(&lock).expect("write");
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    start.wait();
                    state(&lock).write(&lock).expect("write or nothing");
                });
                s.spawn(|| {
                    start.wait();
                    ServeState::remove(&lock).expect("remove");
                });
            });
            assert!(
                !mdkg_dir(&common).join(STATE_FILE).exists(),
                "round {round}"
            );
            drop(lock);
        }
        fs::remove_dir_all(&common).ok();
    }

    /// A fault for the next fresh entry's chmod on this thread: it runs
    /// in the chmod's place, so it can also act as another process would
    /// between the creation and the chmod (Terra 4 #8759).
    type Fault = Box<dyn FnOnce() -> rustix::io::Result<()>>;
    thread_local!(pub(super) static FAULT: std::cell::RefCell<Option<Fault>> =
        const { std::cell::RefCell::new(None) });
    // Folder syncs this thread made, and the results of its next sync
    // attempts, in order, before the real barrier runs.
    thread_local!(pub(super) static SYNCS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) });
    type Faults = std::collections::VecDeque<rustix::io::Errno>;
    thread_local!(pub(super) static SYNC_FAULT: std::cell::RefCell<Faults> =
        const { std::cell::RefCell::new(Faults::new()) });

    fn sync_faults(faults: &[rustix::io::Errno]) {
        SYNC_FAULT.with(|f| *f.borrow_mut() = faults.iter().copied().collect());
    }

    fn inject(fault: impl FnOnce() -> rustix::io::Result<()> + 'static) {
        FAULT.with(|f| *f.borrow_mut() = Some(Box::new(fault)));
    }

    #[test]
    fn a_failed_chmod_removes_what_it_created_unless_another_holds_it() {
        // Terra 4 #8759, orchestrator #8777.
        let _serial = serial();
        // The folder: removed when its chmod fails.
        let common = temp();
        inject(|| Err(rustix::io::Errno::PERM));
        assert!(ServeLock::acquire(&common).is_err());
        assert!(!mdkg_dir(&common).exists(), "the new folder is gone");
        // A folder something was put in meanwhile stays: rmdir refuses.
        let dir = mdkg_dir(&common);
        let inside = dir.clone();
        inject(move || {
            fs::write(inside.join("other"), b"").expect("write");
            Err(rustix::io::Errno::PERM)
        });
        assert!(ServeLock::acquire(&common).is_err());
        assert!(dir.join("other").exists(), "not emptied, not removed");
        fs::remove_file(dir.join("other")).expect("rm");
        drop(ServeLock::acquire(&common).expect("a clean start"));
        // The lock file: removed when its chmod fails and nobody holds it.
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm lock");
        inject(|| Err(rustix::io::Errno::PERM));
        assert!(ServeLock::acquire(&common).is_err());
        assert!(!dir.join(LOCK_FILE).exists(), "the new lock file is gone");
        // Another process opened, locked and validated it before the
        // chmod failed: its live lock stays where it is.
        let held = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (path, keep) = (dir.join(LOCK_FILE), held.clone());
        inject(move || {
            let f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("the other process's open");
            f.try_lock().expect("the other process's lock");
            *keep.borrow_mut() = Some(f);
            Err(rustix::io::Errno::PERM)
        });
        assert!(ServeLock::acquire(&common).is_err());
        let other = held.borrow_mut().take().expect("held");
        let (named, locked) = (
            fs::metadata(dir.join(LOCK_FILE)).expect("still there"),
            other.metadata().expect("meta"),
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(named.ino(), locked.ino(), "the held inode keeps its name");
        drop(other);
        // The name moved to another inode before the chmod failed: the
        // replacement stays (Terra 2 #8870).
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm lock");
        let lock_path = dir.join(LOCK_FILE);
        let aside = dir.join("aside");
        inject(move || {
            fs::rename(&lock_path, &aside).expect("move the new inode aside");
            fs::write(&lock_path, b"replacement").expect("a replacement");
            Err(rustix::io::Errno::PERM)
        });
        assert!(ServeLock::acquire(&common).is_err());
        assert_eq!(
            fs::read(dir.join(LOCK_FILE)).expect("still there"),
            b"replacement",
            "the replacement keeps its name"
        );
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_cleanup_removal_is_synced_and_a_failed_sync_is_said() {
        // Terra 1 #8926: both cleanup removals pass a full barrier, and a
        // barrier that fails is reported with the chmod's error.
        let _serial = serial();
        let common = temp();
        let syncs = || SYNCS.with(|n| n.get());
        // The folder.
        let before = syncs();
        inject(|| Err(rustix::io::Errno::PERM));
        let quiet = |r: Result<ServeLock, LockError>| match r {
            // The real barrier succeeded: no uncertain removal reported
            // (Terra 3 #9022).
            Err(LockError::Io(e)) => assert!(!e.to_string().contains("may not survive"), "{e}"),
            other => panic!("{other:?}"),
        };
        quiet(ServeLock::acquire(&common));
        assert_eq!(syncs(), before + 1, "the folder's removal is synced");
        inject(|| Err(rustix::io::Errno::PERM));
        sync_faults(&[rustix::io::Errno::IO]);
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => {
                assert!(e.to_string().contains("may not survive a crash"), "{e}")
            }
            other => panic!("{other:?}"),
        }
        // The lock file.
        drop(ServeLock::acquire(&common).expect("a clean start"));
        let dir = mdkg_dir(&common);
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm lock");
        let before = syncs();
        inject(|| Err(rustix::io::Errno::PERM));
        quiet(ServeLock::acquire(&common));
        assert_eq!(syncs(), before + 1, "the lock file's removal is synced");
        inject(|| Err(rustix::io::Errno::PERM));
        sync_faults(&[rustix::io::Errno::IO]);
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => {
                assert!(e.to_string().contains("may not survive a crash"), "{e}")
            }
            other => panic!("{other:?}"),
        }
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn the_lock_hint_is_only_for_an_unusable_file_and_names_a_read_only_folder() {
        // Terra 3 #8943 minors 3 and 4.
        let _serial = serial();
        if own_uid() == 0 {
            return;
        }
        let common = temp();
        drop(ServeLock::acquire(&common).expect("a first start"));
        let dir = mdkg_dir(&common);
        let lock = dir.join(LOCK_FILE);
        // A usable file whose chmod fails, kept because another process
        // holds it, gets the chmod's error, not the hint: the mode filter
        // decides (Opus #8992).
        fs::remove_file(&lock).expect("rm");
        let held = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (path, keep) = (lock.clone(), held.clone());
        inject(move || {
            let f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("the other process's open");
            f.try_lock().expect("its lock");
            *keep.borrow_mut() = Some(f);
            Err(rustix::io::Errno::ACCESS)
        });
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => assert!(!e.to_string().contains(&quiescent()), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(lock.exists(), "kept for its holder");
        drop(held.borrow_mut().take());
        // A 0400 file with a second link: the link filter keeps the hint
        // away (Opus #8992).
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o400)).expect("chmod");
        let link = dir.join("second-link");
        fs::hard_link(&lock, &link).expect("link");
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => assert!(!e.to_string().contains(&quiescent()), "{e}"),
            other => panic!("{other:?}"),
        }
        fs::remove_file(&link).expect("rm link");
        fs::remove_file(&lock).expect("rm");
        // A 0400 lock file in a 0500 folder: the folder's write bit first.
        drop(ServeLock::acquire(&common).expect("a clean start"));
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o400)).expect("chmod");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => {
                assert!(e.to_string().contains("owner's write bit"), "{e}")
            }
            other => panic!("{other:?}"),
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_folder_another_user_owns_is_said_not_given_a_mode_repair() {
        // Terra 3 #9022: a root-owned folder can't be fixed by this user.
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        let other = dir_hint("/r/.git/mdkg", 0, 501, &e);
        assert!(other.contains("belongs to another user (uid 0)"), "{other}");
        assert!(!other.contains("0700"), "{other}");
        let mine = dir_hint("/r/.git/mdkg", 501, 501, &e);
        assert!(
            mine.contains("restore that folder's mode to 0700"),
            "{mine}"
        );
    }

    #[test]
    fn a_path_that_isnt_utf8_is_only_a_label() {
        // Orchestrator #8948, Terra 3 #8943 item 2.
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"/tmp/r\xffepo/mdkg/serve.lock",
        ));
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        let text = lock_repair(path, &e, false);
        assert!(text.contains("isn't valid UTF-8"), "{text}");
        assert!(text.contains("in the folder shown as"), "{text}");
        let rest = text.replace(&path.parent().unwrap_or(path).display().to_string(), "");
        assert!(!rest.contains("rm "), "{text}");
    }

    #[test]
    fn a_clean_stops_removal_is_synced() {
        // Opus #9123: remove syncs the folder after the unlink, and a
        // failed sync fails it with serve.json already gone.
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        state(&lock).write(&lock).expect("write");
        let before = SYNCS.with(|n| n.get());
        ServeState::remove(&lock).expect("remove");
        assert_eq!(SYNCS.with(|n| n.get()), before + 1, "the removal is synced");
        drop(lock);
        let lock = ServeLock::acquire(&common).expect("lock");
        state(&lock).write(&lock).expect("write");
        sync_faults(&[rustix::io::Errno::IO]);
        ServeState::remove(&lock).expect_err("the sync failed");
        assert!(!mdkg_dir(&common).join(STATE_FILE).exists());
        // Its retry finds the file gone and still syncs (Terra 1 #9126).
        let before = SYNCS.with(|n| n.get());
        ServeState::remove(&lock).expect("the retry");
        assert_eq!(SYNCS.with(|n| n.get()), before + 1);
        drop(lock);
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn an_interrupted_sync_is_retried_and_a_hard_error_after_it_fails() {
        // Terra 3 #9195: EINTR is retried at the attempt, and an error
        // after it is reported.
        let _serial = serial();
        let common = temp();
        let dir = rustix::fs::open(
            &common,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open");
        sync_faults(&[rustix::io::Errno::INTR]);
        sync_dir(&dir).expect("EINTR, then the real barrier");
        assert!(SYNC_FAULT.with(|f| f.borrow().is_empty()));
        sync_faults(&[rustix::io::Errno::INTR, rustix::io::Errno::INTR]);
        sync_dir(&dir).expect("two EINTRs, then the real barrier");
        sync_faults(&[rustix::io::Errno::INTR, rustix::io::Errno::IO]);
        let e = sync_dir(&dir).expect_err("EIO after the retry");
        assert_eq!(e.raw_os_error(), Some(rustix::io::Errno::IO.raw_os_error()));
        assert!(SYNC_FAULT.with(|f| f.borrow().is_empty()));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn an_unopenable_folder_names_its_repair() {
        // Orchestrator #8930 on Terra 1 #8926: a folder a failed
        // mode-set left 000 refuses the start, naming it and the chmod.
        let _serial = serial();
        if own_uid() == 0 {
            return;
        }
        let common = temp();
        let dir = mdkg_dir(&common);
        fs::create_dir_all(&dir).expect("dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => {
                let text = e.to_string();
                assert!(
                    text.contains("restore that folder's mode to 0700"),
                    "{text}"
                );
                let shown = dir.display().to_string();
                assert!(text.contains(&shown), "{text}");
                // The path itself may contain any word (Terra 1 #9025).
                let rest = text.replace(&shown, "");
                assert!(!rest.contains("chmod "), "no pasteable command: {text}");
            }
            other => panic!("{other:?}"),
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_folder_its_owner_cant_search_gets_the_folder_hint() {
        // Terra 1 #9025: <common>/mdkg at 0600 opens, but the lock file
        // in it can't be reached; the folder's repair is named.
        let _serial = serial();
        if own_uid() == 0 {
            return;
        }
        let common = temp();
        drop(ServeLock::acquire(&common).expect("a first start"));
        let dir = mdkg_dir(&common);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o600)).expect("chmod");
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => assert!(
                e.to_string().contains("restore that folder's mode to 0700"),
                "{e}"
            ),
            other => panic!("{other:?}"),
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn an_unopenable_lock_file_names_its_repair() {
        // Orchestrator #8879 on Terra 2 #8870: a lock file left 0400 by a
        // failed chmod refuses the start, naming the file and the command.
        let _serial = serial();
        if own_uid() == 0 {
            return; // root opens it anyway
        }
        let common = temp();
        drop(ServeLock::acquire(&common).expect("a first start"));
        let lock = mdkg_dir(&common).join(LOCK_FILE);
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o400)).expect("chmod");
        match ServeLock::acquire(&common) {
            Err(LockError::Io(e)) => {
                let text = e.to_string();
                assert!(text.contains(&quiescent()), "{text}");
                assert!(text.ends_with("remove that file"), "{text}");
                let shown = lock.display().to_string();
                assert!(text.contains(&shown), "{text}");
                let rest = text.replace(&shown, "");
                assert!(!rest.contains("rm "), "no pasteable command: {text}");
            }
            other => panic!("{other:?}"),
        }
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_failed_write_removes_its_temp_file() {
        // Terra 6 #8208: a rename that fails (a folder holds the name)
        // leaves no temp file.
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        let dir = mdkg_dir(&common);
        fs::create_dir(dir.join(STATE_FILE)).expect("a folder named serve.json");
        state(&lock).write(&lock).expect_err("the rename fails");
        let left: Vec<String> = fs::read_dir(&dir)
            .expect("read")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".serve.json."))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn the_parent_hint_names_the_repair_that_fits() {
        // Terra 6 #8208: a path with a space is quoted for the shell.
        assert_eq!(shell_quoted("/a b/it's"), "'/a b/it'\\''s'");
    }

    #[test]
    fn a_crashed_serves_temp_files_are_swept_at_start() {
        // #7073: a crash between creating a temp file and its rename
        // leaves it; the next start removes it.
        let _serial = serial();
        let common = temp();
        drop(ServeLock::acquire(&common).expect("folder"));
        let dir = mdkg_dir(&common);
        let swept = [
            ".serve.live.99999.tmp",
            ".serve.json.99999.tmp",
            ".serve.live.99999.7.tmp",
            ".serve.json.99999.12.tmp",
        ];
        for f in swept {
            fs::write(dir.join(f), b"").expect("leftover");
        }
        // Names serve never generates, a folder and a link under its
        // pattern, are kept (Terra 1 #7461).
        let kept = [
            "keep.tmp",
            ".serve.live.foo.tmp",
            ".serve.json..tmp",
            ".serve.json.1.2.3.tmp",
            // Not a name a writer makes: out of range, or a leading zero
            // (Terra 6 #8208).
            ".serve.json.123.18446744073709551616.tmp",
            ".serve.json.4294967296.1.tmp",
            ".serve.live.4294967296.tmp",
            ".serve.json.0123.1.tmp",
            ".serve.json.123.01.tmp",
            // No process has pid 0, or one past pid_t (Terra 4 #8518,
            // #8759).
            ".serve.json.4294967295.1.tmp",
            ".serve.live.2147483648.tmp",
            ".serve.json.0.1.tmp",
            ".serve.live.0.tmp",
        ];
        for f in kept {
            fs::write(dir.join(f), b"").expect("other");
        }
        fs::create_dir(dir.join(".serve.live.5.tmp")).expect("folder");
        std::os::unix::fs::symlink(common.join("x"), dir.join(".serve.json.6.tmp")).expect("link");
        let lock = ServeLock::acquire(&common).expect("start");
        for f in swept {
            assert!(!dir.join(f).exists(), "{f}");
        }
        for f in kept {
            assert!(dir.join(f).exists(), "only serve's own: {f}");
        }
        assert!(dir.join(".serve.live.5.tmp").is_dir());
        assert!(fs::symlink_metadata(dir.join(".serve.json.6.tmp")).is_ok());
        drop(lock);
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_restrictive_umask_leaves_no_unusable_file() {
        // Terra 3 #6773: under umask 0777 the files serve creates are
        // still 0600, so discovery and the next start can open them.
        let _serial = serial();
        let common = temp();
        drop(ServeLock::acquire(&common).expect("make the folder"));
        let old = rustix::process::umask(Mode::from_raw_mode(0o777));
        let made = (|| {
            let lock = ServeLock::acquire(&common)?;
            state(&lock).write(&lock).map_err(LockError::Io)?;
            Ok::<_, LockError>(lock)
        })();
        rustix::process::umask(old);
        let lock = made.expect("serve under umask 0777");
        let expected = state(&lock);
        let dir = mdkg_dir(&common);
        for f in [LOCK_FILE, LIVE_FILE, STATE_FILE] {
            assert_eq!(mode_of(&dir.join(f)), 0o600, "{f}");
        }
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Running(expected)
        );
        drop(lock);
        ServeLock::acquire(&common).expect("the next start");
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_replaced_folder_or_live_file_is_noticed() {
        // Orchestrator #6822 and Terra 3 #6773: still_held checks the
        // folder's identity and serve.live's inode, not only serve.lock.
        let _serial = serial();
        let common = temp();
        let dir = mdkg_dir(&common);
        let lock = ServeLock::acquire(&common).expect("lock");
        state(&lock).write(&lock).expect("write");
        assert!(lock.still_held().expect("probe"));
        fs::remove_file(dir.join(LIVE_FILE)).expect("rm live");
        assert!(!lock.still_held().expect("probe"), "serve.live removed");
        drop(lock);

        let lock = ServeLock::acquire(&common).expect("lock");
        let moved = common.join("mdkg-old");
        fs::rename(&dir, &moved).expect("rename away");
        assert!(!lock.still_held().expect("probe"), "folder renamed");
        // Another serve can take a lock in the new folder; this one knows.
        let other = ServeLock::acquire(&common).expect("a new folder");
        assert!(other.still_held().expect("probe"));
        assert!(!lock.still_held().expect("probe"));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_new_serve_never_shows_as_the_crashed_one() {
        // Terra 3 #6773: between taking serve.live and writing serve.json,
        // discovery must not report the crashed serve's file as running.
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        let crashed = state(&lock);
        crashed.write(&lock).expect("write");
        drop(lock);
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Stale(crashed.clone())
        );
        let lock = ServeLock::acquire(&common).expect("relock");
        let new = state(&lock);
        // Announced, serve.json not yet rewritten: never Running.
        new.announce(&lock).expect("announce");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::HeldByOther(crashed)
        );
        new.write(&lock).expect("write");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Running(new)
        );
        drop(lock);
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn the_switch_probe_finds_no_switch_without_the_folder() {
        let _serial = serial();
        let common = temp();
        assert!(!switching(&common).expect("absent folder"));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn links_and_fifos_are_refused() {
        let _serial = serial();
        let common = temp();
        let dir = mdkg_dir(&common);
        drop(ServeLock::acquire(&common).expect("lock"));
        // A symbolic link in place of the lock file.
        let target = common.join("elsewhere");
        fs::write(&target, b"keep").expect("target");
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm");
        std::os::unix::fs::symlink(&target, dir.join(LOCK_FILE)).expect("link");
        assert!(matches!(ServeLock::acquire(&common), Err(LockError::Io(_))));
        // A hard link to another file (the store, say) is never truncated.
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm");
        fs::hard_link(&target, dir.join(LOCK_FILE)).expect("hard link");
        assert!(matches!(ServeLock::acquire(&common), Err(LockError::Io(_))));
        assert_eq!(fs::read(&target).expect("read"), b"keep");
        fs::remove_file(dir.join(LOCK_FILE)).expect("rm");
        // A FIFO in place of the lease-mode lock: the probe doesn't hang.
        let fifo = dir.join(LEASE_MODE_LOCK);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        // Bounded: should the probe block on the FIFO (a regression that
        // drops O_NONBLOCK), the test fails rather than hanging every state
        // test (#7009, #7262, #7974, #8010).
        let probe_common = common.clone();
        assert_eq!(
            bounded_fifo_probe(&fifo, move || switching(&probe_common).is_err()),
            Ok(true),
            "the FIFO is refused without blocking"
        );
        fs::remove_file(&fifo).expect("rm fifo");
        // A link in place of the folder.
        let other = temp();
        fs::remove_dir_all(&dir).expect("rm dir");
        std::os::unix::fs::symlink(&other, &dir).expect("dir link");
        assert!(matches!(ServeLock::acquire(&common), Err(LockError::Io(_))));
        fs::remove_dir_all(&common).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn a_lock_replaced_before_it_is_taken_is_opened_again() {
        let _serial = serial();
        // #6716: the path must name the locked file at start, not only on
        // the 10 s timer.
        let common = temp();
        let dir = mdkg_dir(&common);
        drop(ServeLock::acquire(&common).expect("make the folder"));
        let replace = |d: &Dir| {
            let f = d.file(LOCK_FILE, true, true)?;
            fs::remove_file(dir.join(LOCK_FILE))?;
            fs::write(dir.join(LOCK_FILE), b"")?;
            fs::set_permissions(dir.join(LOCK_FILE), fs::Permissions::from_mode(0o600))?;
            Ok(f)
        };
        let mut calls = 0;
        let lock = ServeLock::acquire_in(Dir::open(&common, true).expect("dir"), &mut |d| {
            calls += 1;
            if calls == 1 {
                replace(d)
            } else {
                d.file(LOCK_FILE, true, true)
            }
        })
        .expect("taken on the second open");
        assert_eq!(calls, 2);
        assert!(lock.still_held().expect("probe"));
        drop(lock);
        // Replaced every time: refused, never a lock on an orphan.
        let always =
            ServeLock::acquire_in(Dir::open(&common, true).expect("dir"), &mut |d| replace(d));
        assert!(matches!(always, Err(LockError::Io(_))));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn an_unlinked_lock_is_noticed() {
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        fs::remove_file(mdkg_dir(&common).join(LOCK_FILE)).expect("rm");
        assert!(!lock.still_held().expect("probe"));
        let second = ServeLock::acquire(&common).expect("a new file");
        assert!(second.still_held().expect("probe"));
        assert!(!lock.still_held().expect("probe"));
        fs::remove_dir_all(&common).ok();
    }

    /// A lease-mode switch as S37 runs it: both locks, exclusively.
    fn hold_switch(common: &Path) -> (File, ServeLock) {
        let dir = Dir::open(common, true).expect("dir");
        let mode = dir.file(LEASE_MODE_LOCK, true, true).expect("mode lock");
        mode.try_lock().expect("mode lock free");
        (mode, ServeLock::acquire(common).expect("serve lock"))
    }

    #[test]
    fn a_start_during_a_switch_waits_for_it() {
        let _serial = serial();
        let common = temp();
        let (mode, held) = hold_switch(&common);
        assert!(switching(&common).expect("probe"));
        let c = common.clone();
        let start = std::thread::spawn(move || {
            ServeLock::acquire_after_switch(&c, Duration::from_secs(10))
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        drop(mode);
        start
            .join()
            .expect("thread")
            .expect("started after the switch");
        // A switch that outlasts the wait.
        let (mode, held) = hold_switch(&common);
        let r = ServeLock::acquire_after_switch(&common, Duration::from_millis(100));
        assert!(matches!(r, Err(LockError::Switching)));
        drop((mode, held));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_second_serve_is_refused_at_once_while_a_delegation_runs() {
        let _serial = serial();
        let common = temp();
        let dir = Dir::open(&common, true).expect("dir");
        // A delegation holds the lease-mode lock shared: not a switch.
        let delegation = dir.file(LEASE_MODE_LOCK, true, true).expect("f");
        delegation.try_lock_shared().expect("shared");
        let _first = ServeLock::acquire(&common).expect("first");
        let t = Instant::now();
        let r = ServeLock::acquire_after_switch(&common, Duration::from_secs(10));
        assert!(matches!(r, Err(LockError::Held)));
        assert!(t.elapsed() < Duration::from_secs(1));
        fs::remove_dir_all(&common).ok();
    }

    #[test]
    fn a_crashed_serve_is_stale_whoever_holds_the_lock_and_whatever_its_pid() {
        let _serial = serial();
        let common = temp();
        let lock = ServeLock::acquire(&common).expect("lock");
        // The crashed serve's pid is reused by a live process (ours).
        let crashed = state(&lock);
        crashed.write(&lock).expect("write");
        drop(lock);
        // A switch holds serve.lock after the crash; serve.live is free.
        let (mode, held) = hold_switch(&common);
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::Stale(crashed.clone())
        );
        drop((mode, held));
        // A new serve that hasn't written its file yet.
        let lock = ServeLock::acquire(&common).expect("lock");
        let new = state(&lock);
        new.write(&lock).expect("write");
        fs::write(
            mdkg_dir(&common).join(STATE_FILE),
            serde_json::to_vec(&crashed).expect("j"),
        )
        .expect("old file back");
        assert_eq!(
            ServeState::find(&common).expect("find"),
            Found::HeldByOther(crashed)
        );
        drop(lock);
        fs::remove_dir_all(&common).ok();
    }
}
