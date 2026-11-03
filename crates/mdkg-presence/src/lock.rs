//! The person-key lock: one per OS user (orchestrator, #8674).
//!
//! Every enclave creator and opener, every FIDO2 credential creation
//! (makeCredential and the handle's save, [`crate::fido2::enrol_credential`]),
//! and every file-key or FIDO2-handle writer and reader holds it,
//! exclusively, for the whole operation. A key is therefore never seen
//! half-made, two creators never race, and an enclave label never holds
//! two keys. Signing doesn't take it: an enclave signature or a FIDO2
//! assertion uses a key already made, and two can run at once.
//!
//! It is `person.lock` in a folder under the OS user's home directory
//! from the user database (getpwuid), never `$HOME`, so every process of
//! one user shares it whatever its environment: `Library/Application
//! Support/dev.mdkg` on macOS (which, unlike `Library/Caches`, macOS
//! never purges), `.local/state/mdkg-lock` elsewhere.
//!
//! The path is pinned (Terra #9305, #9326): the home is resolved once, and
//! it and every folder above it must be root's or this user's, and sticky
//! if others may write it. From a descriptor of the home, each component
//! is opened with `O_NOFOLLOW`, so no link anywhere on the path is
//! followed, and each must be this user's and writable by no one else;
//! the lock folder itself is 0700. The lock file is opened from the lock
//! folder's descriptor, without following a link and without
//! blocking, and must be a regular file. After the lock is taken, the
//! name must still be the locked file (Claude #9255): a waiter whose file
//! was removed or replaced before it got the lock takes it again, on the
//! file the name now holds. A lock file deleted while someone holds it is
//! not caught: the next process would lock a new file. That is why the
//! folder is one macOS never purges, and it is a residual of deleting
//! files in a 0700 folder by hand.
//!
//! The passwd home path is trusted (orchestrator, #9466 call): it is
//! the trust root, a named residual. Someone who controls the path to it
//! can usually read or replace what is under it, but not always: if the
//! home is reached through a link they control, they could point it at
//! one of this user's private folders and then another, splitting the
//! lock between processes without access to the keychain or a shared
//! state folder (Terra #9624). No lock check can fix that either, since
//! every check runs on the path the link gives. The one check kept:
//! the resolved home is a folder owned by this user. So a home that is
//! legitimately root's (an SFTP chroot, a managed or kiosk account) is
//! refused, on purpose, with a hint saying so.
//!
//! A FIDO2 credential creation waiting on the person's touch keeps the
//! lock, with no timeout: meanwhile every other person-key creation or
//! load of this user waits too. A long wait there is the person, not a
//! hang.

use std::fs;
use std::path::{Path, PathBuf};

use mdkg_format::{Code, Diagnostic};

use crate::device;

/// The lock file's name in its folder.
pub const LOCK_FILE: &str = "person.lock";

fn refused(path: &Path, why: &str) -> Box<Diagnostic> {
    Box::new(
        Diagnostic::error(Code::IoWriteFailed, format!("{} {why}", path.display()))
            .with_hint("the person-key lock folder must be yours, 0700, under folders only you or root can change"),
    )
}

/// A lock folder: a trusted, resolved base and the components below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserLock {
    base: PathBuf,
    parts: Vec<String>,
}

/// The held lock; released when dropped.
#[derive(Debug)]
pub struct Held {
    _file: fs::File,
}

impl UserLock {
    /// This OS user's lock folder, created if missing.
    pub fn for_user() -> Result<Self, Box<Diagnostic>> {
        let (home, parts) = user_location()?;
        Self::under(&home, &parts)
    }

    /// The lock folder `dir`, under its parent as the base. Tests use
    /// their own; everything else uses [`UserLock::for_user`].
    pub fn at(dir: &Path) -> Result<Self, Box<Diagnostic>> {
        let (parent, name) = match (dir.parent(), dir.file_name().and_then(|n| n.to_str())) {
            (Some(p), Some(n)) => (p, n),
            _ => return Err(refused(dir, "isn't a folder path")),
        };
        fs::create_dir_all(parent)
            .map_err(|e| device::io_error(Code::IoWriteFailed, "can't create", parent, &e))?;
        Self::under(parent, &[name])
    }

    /// `parts` under `base`, created and checked now, and pinned again on
    /// every [`UserLock::hold`].
    fn under(base: &Path, parts: &[&str]) -> Result<Self, Box<Diagnostic>> {
        let base = fs::canonicalize(base)
            .map_err(|e| device::io_error(Code::IoReadFailed, "can't resolve", base, &e))?;
        let lock = Self {
            base,
            parts: parts.iter().map(|p| (*p).to_owned()).collect(),
        };
        #[cfg(unix)]
        lock.open_folder()?;
        Ok(lock)
    }

    /// The lock folder's path, for messages and tests.
    pub fn dir(&self) -> PathBuf {
        self.parts
            .iter()
            .fold(self.base.clone(), |p, part| p.join(part))
    }

    /// Opens the lock folder from the base's descriptor, one component at
    /// a time with `O_NOFOLLOW`, creating missing ones 0700, and checks
    /// each through its descriptor.
    #[cfg(unix)]
    fn open_folder(&self) -> Result<std::os::fd::OwnedFd, Box<Diagnostic>> {
        use rustix::fs::{Mode, OFlags};
        let first = self.parts.first().map(String::as_str).unwrap_or("");
        device::check_ancestors_of(&self.base.join(first), "person-key lock folder")?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let io = |path: &Path, e: rustix::io::Errno| {
            device::io_error(
                Code::IoReadFailed,
                "can't open",
                path,
                &std::io::Error::from(e),
            )
        };
        let mut fd =
            rustix::fs::open(&self.base, flags, Mode::empty()).map_err(|e| io(&self.base, e))?;
        let mut path = self.base.clone();
        for (i, part) in self.parts.iter().enumerate() {
            path.push(part);
            let next = match rustix::fs::openat(&fd, part.as_str(), flags, Mode::empty()) {
                Ok(f) => f,
                Err(rustix::io::Errno::NOENT) => {
                    match rustix::fs::mkdirat(&fd, part.as_str(), Mode::from_raw_mode(0o700)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(io(&path, e)),
                    }
                    rustix::fs::openat(&fd, part.as_str(), flags, Mode::empty())
                        .map_err(|e| io(&path, e))?
                }
                Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                    return Err(refused(&path, "is a link or not a folder"));
                }
                Err(e) => return Err(io(&path, e)),
            };
            let st = rustix::fs::fstat(&next).map_err(|e| io(&path, e))?;
            if st.st_uid != device::uid() {
                return Err(refused(&path, "isn't yours"));
            }
            let mode = st.st_mode as u32;
            let last = i + 1 == self.parts.len();
            if last && mode & 0o077 != 0 {
                return Err(refused(&path, "is open to others; make it 0700"));
            }
            if mode & 0o022 != 0 {
                return Err(refused(&path, "is writable by others"));
            }
            fd = next;
        }
        Ok(fd)
    }

    /// Takes the lock, waiting for any other holder.
    pub fn hold(&self) -> Result<Held, Box<Diagnostic>> {
        for _ in 0..5 {
            if let Some(file) = self.lock_once()? {
                return Ok(Held { _file: file });
            }
        }
        Err(refused(
            &self.dir().join(LOCK_FILE),
            "keeps being replaced while it is locked",
        ))
    }

    /// Whether another open of the lock file holds it now, by a
    /// non-blocking attempt, for tests that must observe the lock held
    /// (Terra #10099). flock conflicts between separate opens, in one
    /// process too. Plain std, so every platform's tests build (Terra
    /// #10276).
    #[cfg(test)]
    pub(crate) fn is_held(&self) -> bool {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(self.dir().join(LOCK_FILE))
            .expect("the lock file");
        match file.try_lock() {
            Ok(()) => false,
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(e)) => panic!("try_lock: {e}"),
        }
    }

    /// Locks the file the name holds; `None` if the name no longer holds
    /// it once locked.
    #[cfg(unix)]
    fn lock_once(&self) -> Result<Option<fs::File>, Box<Diagnostic>> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        let dir = self.open_folder()?;
        let path = self.dir().join(LOCK_FILE);
        let io = |e: rustix::io::Errno| {
            device::io_error(
                Code::IoWriteFailed,
                "can't open",
                &path,
                &std::io::Error::from(e),
            )
        };
        // macOS can answer ENOENT to an O_CREAT open that races another
        // creating the same name; that is spurious, so it is retried.
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let mut tries = 0;
        let fd = loop {
            match rustix::fs::openat(&dir, LOCK_FILE, flags, Mode::from_raw_mode(0o600)) {
                Err(rustix::io::Errno::NOENT) if tries < 20 => {
                    tries += 1;
                    std::thread::yield_now();
                }
                other => break other.map_err(io)?,
            }
        };
        let file = fs::File::from(fd);
        let meta = file
            .metadata()
            .map_err(|e| device::io_error(Code::IoReadFailed, "can't read", &path, &e))?;
        if !meta.is_file() {
            return Err(refused(&path, "isn't a regular file"));
        }
        file.lock()
            .map_err(|e| device::io_error(Code::IoWriteFailed, "can't lock", &path, &e))?;
        // The name must still be the locked file (Claude #9255).
        let held = rustix::fs::fstat(&file).map_err(io)?;
        let named = rustix::fs::statat(&dir, LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW);
        Ok(named
            .is_ok_and(|n| n.st_dev == held.st_dev && n.st_ino == held.st_ino)
            .then_some(file))
    }

    #[cfg(not(unix))]
    fn lock_once(&self) -> Result<Option<fs::File>, Box<Diagnostic>> {
        let dir = self.dir();
        fs::create_dir_all(&dir)
            .map_err(|e| device::io_error(Code::IoWriteFailed, "can't create", &dir, &e))?;
        let path = dir.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| device::io_error(Code::IoWriteFailed, "can't open", &path, &e))?;
        file.lock()
            .map_err(|e| device::io_error(Code::IoWriteFailed, "can't lock", &path, &e))?;
        Ok(Some(file))
    }
}

/// Where [`UserLock::for_user`] puts the lock: the resolved passwd home,
/// and the components below it.
fn user_location() -> Result<(PathBuf, [&'static str; 3]), Box<Diagnostic>> {
    #[cfg(target_os = "macos")]
    let parts = ["Library", "Application Support", "dev.mdkg"];
    #[cfg(not(target_os = "macos"))]
    let parts = [".local", "state", "mdkg-lock"];
    Ok((user_home()?, parts))
}

/// The OS user's home directory from the user database, resolved.
#[cfg(unix)]
fn user_home() -> Result<PathBuf, Box<Diagnostic>> {
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .filter(|u| u.dir.is_absolute())
        .ok_or_else(|| {
            Box::new(Diagnostic::error(
                Code::IoReadFailed,
                "this user has no home directory in the user database",
            ))
        })?;
    resolve_home(&user.dir)
}

/// The passwd home `dir`, resolved, and checked to be a folder this user
/// owns (#9466 call).
#[cfg(unix)]
fn resolve_home(dir: &Path) -> Result<PathBuf, Box<Diagnostic>> {
    let home = fs::canonicalize(dir)
        .map_err(|e| device::io_error(Code::IoReadFailed, "can't resolve", dir, &e))?;
    check_home(&home)?;
    Ok(home)
}

/// The resolved home must be a folder this user owns (#9466 call); the
/// path to it is trusted, a named residual.
#[cfg(unix)]
fn check_home(home: &Path) -> Result<(), Box<Diagnostic>> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(home)
        .map_err(|e| device::io_error(Code::IoReadFailed, "can't read", home, &e))?;
    // Its own message and hint: the lock folder isn't what's wrong
    // (Claude #9610), and nothing was written.
    let home_refused = |why: &str| {
        Box::new(
            Diagnostic::error(Code::IoReadFailed, format!("{} {why}", home.display())).with_hint(
                "the home directory the user database names must be a folder you own; a root-owned \
                 home (an SFTP chroot, a managed account) is refused on purpose",
            ),
        )
    };
    if !meta.is_dir() {
        return Err(home_refused("the home directory isn't a folder"));
    }
    if meta.uid() != device::uid() {
        return Err(home_refused("the home directory isn't yours"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn user_home() -> Result<PathBuf, Box<Diagnostic>> {
    Err(Box::new(Diagnostic::error(
        Code::ApiUnsupported,
        "person keys need a Unix user database on this platform",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[cfg(unix)]
    #[test]
    fn the_lock_folder_must_be_private_and_under_trusted_folders() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("lock-dir");
        // A fresh folder is made 0700 and accepted, whatever the umask.
        let ok = tmp.path().join("ok/locks");
        UserLock::at(&ok).expect("fresh").hold().expect("hold");
        // Open to the group: refused.
        let open = tmp.path().join("open");
        fs::create_dir_all(&open).expect("dir");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o750)).expect("chmod");
        assert!(
            UserLock::at(&open)
                .expect_err("refused")
                .message
                .contains("0700")
        );
        // Under a folder others may write (and not sticky): refused.
        let shared = tmp.path().join("shared");
        fs::create_dir_all(shared.join("locks")).expect("dir");
        fs::set_permissions(shared.join("locks"), fs::Permissions::from_mode(0o700))
            .expect("chmod");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        let err = UserLock::at(&shared.join("locks")).expect_err("refused");
        assert!(
            err.message.contains("above the person-key lock folder"),
            "{}",
            err.message
        );
        // A link on the path is never followed, even to a good folder.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&ok, &link).expect("link");
        let err = UserLock::at(&link).expect_err("refused");
        assert!(err.message.contains("is a link"), "{}", err.message);
        // Repointed after the check: the next hold refuses, rather than
        // locking whatever the link names now.
        let real = tmp.path().join("real");
        let lock = UserLock::at(&real).expect("real");
        fs::remove_dir_all(&real).expect("rm");
        std::os::unix::fs::symlink(&ok, &real).expect("swap in a link");
        assert!(lock.hold().is_err(), "a link swapped in later is refused");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_lock_name_is_refused_without_blocking() {
        let tmp = TempDir::new("lock-fifo");
        let lock = UserLock::at(&tmp.path().join("locks")).expect("dir");
        let made = std::process::Command::new("mkfifo")
            .arg(lock.dir().join(LOCK_FILE))
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(lock.hold().is_err());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("hold returned instead of blocking on the FIFO")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_user_lock_is_under_the_passwd_home() {
        // The location for_user uses, checked against the user database
        // independently, so a regression to $HOME or the old cache folder
        // fails here (Terra #9347).
        let (base, parts) = user_location().expect("location");
        let home = nix::unistd::User::from_uid(nix::unistd::Uid::current())
            .expect("lookup")
            .expect("user")
            .dir;
        assert_eq!(base, fs::canonicalize(home).expect("home"));
        #[cfg(target_os = "macos")]
        assert_eq!(parts, ["Library", "Application Support", "dev.mdkg"]);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(parts, [".local", "state", "mdkg-lock"]);
        assert!(!parts.contains(&"Caches"), "never a purgeable cache");
    }

    #[test]
    fn the_lock_excludes_a_second_holder() {
        let tmp = TempDir::new("lock-excl");
        let lock = UserLock::at(&tmp.path().join("locks")).expect("dir");
        let held = lock.hold().expect("hold");
        let (tx, rx) = std::sync::mpsc::channel();
        let other = lock.clone();
        let t = std::thread::spawn(move || {
            let _h = other.hold().expect("hold");
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "waits"
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("then holds");
        t.join().expect("join");
    }

    /// A waiter whose file is replaced while it waits takes the lock
    /// again, on the file the name now holds (Claude #9255).
    #[cfg(unix)]
    #[test]
    fn a_waiter_whose_file_is_replaced_locks_the_new_one() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new("lock-gone");
        let lock = UserLock::at(&tmp.path().join("locks")).expect("dir");
        let path = lock.dir().join(LOCK_FILE);
        let held = lock.hold().expect("hold");
        let waiter = {
            let lock = lock.clone();
            std::thread::spawn(move || {
                let h = lock.hold().expect("hold");
                h._file.metadata().expect("meta").ino()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        // The waiter is blocked on the old file; a cleaner replaces it.
        fs::remove_file(&path).expect("rm");
        fs::write(&path, b"").expect("replaced");
        let new = fs::symlink_metadata(&path).expect("meta").ino();
        drop(held);
        assert_eq!(
            waiter.join().expect("join"),
            new,
            "the waiter locked the new file"
        );
    }

    #[test]
    fn is_held_observes_a_holder() {
        let tmp = TempDir::new("lock-held");
        let lock = UserLock::at(&tmp.path().join("l")).expect("lock");
        let held = lock.hold().expect("hold");
        assert!(lock.is_held());
        drop(held);
        assert!(!lock.is_held());
    }

    /// Eight holders at once, as eight processes of one user would: each
    /// gets the lock in turn, none sees the file missing (macOS can answer
    /// ENOENT to racing O_CREAT opens; that is retried).
    #[test]
    fn concurrent_holders_all_get_the_lock() {
        let tmp = TempDir::new("lock-many");
        let lock = UserLock::at(&tmp.path().join("l")).expect("lock");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let l = lock.clone();
                std::thread::spawn(move || l.hold().map(|_| ()).map_err(|d| d.message.clone()))
            })
            .collect();
        for t in threads {
            t.join().expect("join").expect("each holder gets the lock");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_home_that_isnt_this_users_folder_is_refused() {
        let tmp = TempDir::new("home-check");
        check_home(tmp.path()).expect("a folder of ours");
        // Through resolve_home, which user_home calls with the passwd home:
        // a folder of ours resolves, a file and / are refused (Claude #9610).
        assert_eq!(
            resolve_home(tmp.path()).expect("ours"),
            fs::canonicalize(tmp.path()).expect("real")
        );
        let f = tmp.path().join("g");
        fs::write(&f, b"").expect("write");
        let err = resolve_home(&f).expect_err("a file");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("home directory"))
        );
        if device::uid() != 0 {
            assert!(resolve_home(Path::new("/")).is_err(), "root's home");
        }
        let file = tmp.path().join("f");
        fs::write(&file, b"").expect("write");
        assert!(
            check_home(&file)
                .expect_err("refused")
                .message
                .contains("isn't a folder")
        );
        // A folder owned by root (when not running as root).
        if device::uid() != 0 {
            let err = check_home(Path::new("/")).expect_err("refused");
            assert!(err.message.contains("isn't yours"), "{}", err.message);
        }
    }
}
