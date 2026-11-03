//! Where a person's keys live on their device, and their ids.
//!
//! Each person's folder is `${XDG_STATE_HOME:-$HOME/.local/state}/mdkg/
//! person/<name>/` (0700), beside the machine key's folder (S5). It holds
//! the file key (`key.ed25519`, 0600, only with the opt-in), a FIDO2
//! credential handle per graph (`fido2-<graph_id>.json`, none of it
//! secret). An enclave key lives in the keychain, under
//! [`enclave_label`], and its creation record beside the person-key lock
//! (per OS user, as the label is; #9869 D2, [`PersonDir::lock_folder`]). No key is ever in a repository, a node, the store, a
//! log or an environment variable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mdkg_format::{Code, Diagnostic};

use crate::cose::PublicKey;
use crate::lock::{Held, UserLock};

/// A key's id, sent as `Mdkg-Person` and stored as `persons.key_id`:
/// `mdkg:` and the base64url SHA-256 of the raw public key (65 bytes for
/// P-256, 32 for Ed25519), the form S5 gives machine keys: wire's
/// [`mdkg_wire::keys::fingerprint`].
pub fn fingerprint(key: &PublicKey) -> String {
    mdkg_wire::keys::fingerprint(key.as_bytes())
}

/// The name part of `human:<name>`, if it is safe as a folder name and a
/// keychain label: lowercase ASCII letters, digits, `-`, `_` and `.`, not
/// starting with `.`.
pub fn person_name(human: &str) -> Option<&str> {
    let name = human.strip_prefix("human:")?;
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
    ok.then_some(name)
}

/// The prefix of every enclave key's keychain label.
pub const ENCLAVE_LABEL_PREFIX: &str = "dev.mdkg.person.";

/// The keychain label of a person's enclave key on this device.
pub fn enclave_label(name: &str) -> String {
    format!("{ENCLAVE_LABEL_PREFIX}{name}")
}

/// The state folder: `$XDG_STATE_HOME`, or `$HOME/.local/state`.
pub fn state_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(v) if Path::new(&v).is_absolute() => Some(PathBuf::from(v)),
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")),
    }
}

/// A person's key folder under `state`: `<state>/mdkg/person/<name>`.
pub fn person_dir(state: &Path, name: &str) -> PathBuf {
    state.join("mdkg").join("person").join(name)
}

pub(crate) fn io_error(code: Code, what: &str, path: &Path, e: &std::io::Error) -> Box<Diagnostic> {
    Box::new(Diagnostic::error(
        code,
        format!("{what} {}: {}", path.display(), e.kind()),
    ))
}

fn unsafe_dir(path: &Path, why: &str) -> Box<Diagnostic> {
    Box::new(
        Diagnostic::error(Code::IoWriteFailed, format!("{} {why}", path.display())).with_hint(
            "a person's key folder must be yours, 0700, and not a link; remove it and enrol again",
        ),
    )
}

/// A person's key folder, checked (Terra #8056 5, #8294, #8765). Every
/// use holds the person-key lock ([`crate::lock::UserLock`]): opening,
/// publishing and reading take a [`Held`] lock, so no two mdkg processes
/// of this user ever work on a key at once.
/// - The state folder is resolved once, to a path with no links, and
///   every later use goes through that path.
/// - Every folder above it is root's or this user's, and one that others
///   may write is sticky, so no other user can rename or replace a folder
///   on the path.
/// - The state folder is this user's, writable by no one else, and not in
///   a git work tree; `mdkg`, `person` and `<name>` aren't work-tree roots
///   either. They are real folders, not links, owned by this user, and
///   `<name>` is 0700.
/// - On each open, the folders on the resolved path that this user owns
///   (from the state folder up to the first they don't), that first
///   folder too, and `mdkg`, `person` and `<name>` are synced, whoever
///   created them, so a key published or read here can't lose its path in
///   a crash. The one residual: that first folder is skipped when it
///   can't be opened for reading (a 0711 `/home`), where its entry for
///   this user's folder predates this user. Every read syncs the
///   person folder first, so a key is never returned before its name is
///   durable, whichever handle or process published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonDir {
    path: PathBuf,
}

/// A step of [`PersonDir::write_new`], where a test may stop it as a
/// crash would: nothing after the step runs, and nothing is cleaned up.
/// Outside tests each step is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    TempCreated,
    Written,
    Synced,
    Renamed,
}

#[cfg(test)]
thread_local! {
    pub(crate) static CRASH_AT: std::cell::Cell<Option<Step>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn crash(step: Step) -> Result<(), Box<Diagnostic>> {
    if CRASH_AT.with(|c| c.get()) == Some(step) {
        return Err(Box::new(Diagnostic::error(
            Code::IoWriteFailed,
            format!("crash at {step:?}"),
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn crash(_: Step) -> Result<(), Box<Diagnostic>> {
    Ok(())
}

impl PersonDir {
    /// Checks `name`'s folder under `state`, creating what's missing when
    /// `create`, under the held lock.
    pub fn open(
        state: &Path,
        name: &str,
        create: bool,
        _held: &Held,
    ) -> Result<Self, Box<Diagnostic>> {
        if person_name(&format!("human:{name}")).is_none() {
            return Err(Box::new(Diagnostic::error(
                Code::IoWriteFailed,
                format!("{name:?} isn't a person name a key folder can have"),
            )));
        }
        if create {
            fs::create_dir_all(state)
                .map_err(|e| io_error(Code::IoWriteFailed, "can't create", state, &e))?;
        }
        let state = &fs::canonicalize(state)
            .map_err(|e| io_error(Code::IoReadFailed, "can't resolve", state, &e))?;
        check_ancestors(state)?;
        check_state(state)?;
        for dir in state.ancestors() {
            refuse_repository(dir, state)?;
        }
        sync_owned_path(state)?;
        let mut path = state.to_path_buf();
        for (part, private) in [("mdkg", false), ("person", false), (name, true)] {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) => check_dir(&path, &meta, private)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                    make_dir(&path)?;
                    let meta = fs::symlink_metadata(&path)
                        .map_err(|e| io_error(Code::IoReadFailed, "can't read", &path, &e))?;
                    check_dir(&path, &meta, private)?;
                }
                Err(e) => return Err(io_error(Code::IoReadFailed, "can't read", &path, &e)),
            }
            // mdkg, person or <name> could be a work tree's root too.
            refuse_repository(&path, &path)?;
            sync_dir(&path)?;
        }
        Ok(Self { path })
    }

    /// The person-key lock's own folder, for the records whose scope is
    /// the OS user's, as the lock's is: the enclave key's creation record
    /// and its intent (#9869 D2). [`UserLock::hold`] checked it (this
    /// user's, 0700, reached without a link); this syncs it and the path
    /// to it as [`PersonDir::open`] does. It holds no secret, so unlike a
    /// state folder it may sit in a git work tree (a home kept in git).
    pub fn lock_folder(lock: &UserLock, _held: &Held) -> Result<Self, Box<Diagnostic>> {
        let path = lock.dir();
        sync_owned_path(&path)?;
        Ok(Self { path })
    }

    /// Whether `file` is in this folder, under the held lock; an error
    /// other than its absence is an error.
    pub fn exists(&self, _held: &Held, file: &str) -> Result<bool, Box<Diagnostic>> {
        let path = self.path.join(file);
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error(Code::IoReadFailed, "can't check", &path, &e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Publishes `file` in this folder, 0600, refusing to replace one that
    /// exists, under the held lock (#8674): it is written under a
    /// temporary name, synced, renamed to its name, and the folder is
    /// synced. A crash at any step leaves no key, with at most a temporary
    /// that the next create or read under the lock removes, or the whole
    /// key. On an error, only this call's own temporary is removed, never
    /// a published key.
    pub fn write_new(&self, _held: &Held, file: &str, bytes: &[u8]) -> Result<(), Box<Diagnostic>> {
        self.clear_temporaries(file);
        let path = self.path.join(file);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(Box::new(Diagnostic::error(
                    Code::IoWriteFailed,
                    format!("{} exists; a key is never replaced", path.display()),
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(Code::IoReadFailed, "can't check", &path, &e)),
        }
        let mut nonce = [0u8; 8];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce).map_err(
            |_| {
                Box::new(Diagnostic::error(
                    Code::IoWriteFailed,
                    "the system random source failed",
                ))
            },
        )?;
        let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let tmp = self.path.join(format!(".{file}.{hex}.partial"));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(nofollow());
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| io_error(Code::IoWriteFailed, "can't create", &tmp, &e))?;
        crash(Step::TempCreated)?;
        let fail = |e: std::io::Error| {
            let _ = fs::remove_file(&tmp);
            io_error(Code::IoWriteFailed, "can't write", &tmp, &e)
        };
        f.write_all(bytes).map_err(fail)?;
        crash(Step::Written)?;
        f.sync_all().map_err(fail)?;
        drop(f);
        crash(Step::Synced)?;
        fs::rename(&tmp, &path).map_err(fail)?;
        crash(Step::Renamed)?;
        sync_dir(&self.path)
    }

    /// Removes `file` under the held lock, then syncs the folder. A file
    /// already gone is fine, and the folder is synced then too, so a
    /// retry after a failed sync repairs the barrier (Terra #9855 5).
    pub fn remove(&self, _held: &Held, file: &str) -> Result<(), Box<Diagnostic>> {
        let path = self.path.join(file);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(Code::IoWriteFailed, "can't remove", &path, &e)),
        }
        sync_dir(&self.path)
    }

    /// Removes every temporary of `file` (exactly `.<file>.<16 lowercase
    /// hex>.partial`, never another name). Under the lock no creator is
    /// live, so each one is a crash's leftover.
    fn clear_temporaries(&self, file: &str) {
        let prefix = format!(".{file}.");
        let ours = |name: &str| {
            name.strip_prefix(&prefix)
                .and_then(|r| r.strip_suffix(".partial"))
                .is_some_and(|hex| {
                    hex.len() == 16
                        && hex
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
        };
        let Ok(entries) = fs::read_dir(&self.path) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(ours) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Reads `file`, which only its owner, this user, may read, under the
    /// held lock; any leftover temporary of it is removed first. The file
    /// is opened without following a link, and without blocking (so a
    /// FIFO is refused rather than waited on), and checked through the
    /// open descriptor, so the file checked is the file read.
    pub fn read_private(&self, _held: &Held, file: &str) -> Result<Vec<u8>, Box<Diagnostic>> {
        use std::io::Read;
        // A barrier before every read, whichever handle or process wrote
        // the key, so a key whose publication's folder sync failed is
        // never returned before its name is durable (Terra #9305, #9466).
        // Its cost: on macOS, File::sync_all is F_FULLFSYNC, a drive cache
        // flush of a few milliseconds to tens of milliseconds, paid once
        // per key read (Claude #9586); keys are read once per command or
        // request, not per signature.
        sync_dir(&self.path)?;
        self.clear_temporaries(file);
        let path = self.path.join(file);
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(nofollow() | rustix::fs::OFlags::NONBLOCK.bits() as i32);
        }
        let mut f = opts
            .open(&path)
            .map_err(|e| io_error(Code::IoReadFailed, "can't read", &path, &e))?;
        let meta = f
            .metadata()
            .map_err(|e| io_error(Code::IoReadFailed, "can't read", &path, &e))?;
        if !meta.is_file() {
            return Err(Box::new(Diagnostic::error(
                Code::IoReadFailed,
                format!("{} isn't a regular file", path.display()),
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid() || meta.mode() & 0o077 != 0 {
                return Err(Box::new(
                    Diagnostic::error(
                        Code::IoReadFailed,
                        format!("{} is readable by others, or isn't yours", path.display()),
                    )
                    .with_hint("make it 0600, or revoke the key and enrol a new one"),
                ));
            }
        }
        let mut out = Vec::new();
        f.read_to_end(&mut out)
            .map_err(|e| io_error(Code::IoReadFailed, "can't read", &path, &e))?;
        Ok(out)
    }
}

/// Syncs the folders on the resolved `dir` that this user owns, from
/// `dir` up to the first one they don't, and that one too, since it holds
/// the entry for the highest of theirs (Claude #9303): root-most first, so
/// a folder another process made, or whose sync failed before, is durable
/// after this. That first folder is skipped only if it can't be opened for
/// reading (a 0711 /home, or a root-owned 01733 sticky folder), where the
/// entry is taken to predate this user: the one residual, accepted on
/// Opus #9303 option A (#9326, #9645). Folders above it existed before
/// this user could make anything under them.
fn sync_owned_path(dir: &Path) -> Result<(), Box<Diagnostic>> {
    let mut owned: Vec<&Path> = Vec::new();
    for d in dir.ancestors() {
        // An error reading ownership is an error, never a boundary
        // (Terra #9347).
        if !owned_by_me(d)? {
            break;
        }
        owned.push(d);
    }
    let first_other = dir.ancestors().nth(owned.len());
    owned.reverse();
    if let Some(d) = first_other {
        // Only a folder that can't be opened for reading is skipped; one
        // that opens and then fails to sync is an error (Terra #9466).
        match open_dir(d) {
            Ok(f) => sync_open_dir(&f, d)
                .map_err(|e| io_error(Code::IoWriteFailed, "can't sync", d, &e))?,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(e) => return Err(io_error(Code::IoReadFailed, "can't open", d, &e)),
        }
    }
    for d in owned {
        sync_dir(d)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(unix)]
pub(crate) fn nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

/// Every folder above `path` (a resolved path, with no links) is root's
/// or this user's, and one that others may write is sticky, so no other
/// user can move or replace a folder on the path. `what` names `path` in
/// the refusal.
pub(crate) fn check_ancestors_of(path: &Path, what: &str) -> Result<(), Box<Diagnostic>> {
    #[cfg(unix)]
    for dir in path.ancestors().skip(1) {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(dir)
            .map_err(|e| io_error(Code::IoReadFailed, "can't read", dir, &e))?;
        if meta.uid() != 0 && meta.uid() != uid() {
            return Err(unsafe_dir(
                dir,
                &format!("is another user's, above the {what}"),
            ));
        }
        if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
            return Err(unsafe_dir(
                dir,
                &format!("is writable by others, above the {what}"),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, what);
    Ok(())
}

/// Whether this user owns `dir`; an error reading it is returned.
fn owned_by_me(dir: &Path) -> Result<bool, Box<Diagnostic>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(dir)
            .map(|m| m.uid() == uid())
            .map_err(|e| io_error(Code::IoReadFailed, "can't read", dir, &e))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(true)
    }
}

fn check_ancestors(state: &Path) -> Result<(), Box<Diagnostic>> {
    check_ancestors_of(state, "state folder")
}

/// A state folder inside a git work tree would put keys in a repository.
/// Refuses `dir` if it holds `.git`, failing closed: an error other than
/// "not found" refuses too (Terra #8624).
fn refuse_repository(dir: &Path, state: &Path) -> Result<(), Box<Diagnostic>> {
    let git = dir.join(".git");
    let found = match fs::symlink_metadata(&git) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(io_error(Code::IoReadFailed, "can't check", &git, &e)),
    };
    if found {
        return Err(Box::new(
            Diagnostic::error(
                Code::IoWriteFailed,
                format!(
                    "{} is inside the git work tree at {}; person keys never go in a repository",
                    state.display(),
                    dir.display()
                ),
            )
            .with_hint("set XDG_STATE_HOME to a folder outside any repository"),
        ));
    }
    Ok(())
}

/// The resolved state folder must be this user's and writable by no one
/// else.
fn check_state(state: &Path) -> Result<(), Box<Diagnostic>> {
    let meta =
        fs::metadata(state).map_err(|e| io_error(Code::IoReadFailed, "can't read", state, &e))?;
    if !meta.is_dir() {
        return Err(unsafe_dir(state, "isn't a folder"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != uid() {
            return Err(unsafe_dir(state, "isn't yours"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(unsafe_dir(state, "is writable by others"));
        }
    }
    Ok(())
}

fn check_dir(path: &Path, meta: &fs::Metadata, private: bool) -> Result<(), Box<Diagnostic>> {
    if meta.file_type().is_symlink() {
        return Err(unsafe_dir(path, "is a link"));
    }
    if !meta.is_dir() {
        return Err(unsafe_dir(path, "isn't a folder"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != uid() {
            return Err(unsafe_dir(path, "isn't yours"));
        }
        let others = if private { 0o077 } else { 0o022 };
        if meta.mode() & others != 0 {
            return Err(unsafe_dir(path, "is readable or writable by others"));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(())
}

/// Creates one folder, 0700, and syncs its parent so the entry lasts.
fn make_dir(path: &Path) -> Result<(), Box<Diagnostic>> {
    let mut b = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    match b.create(path) {
        Ok(()) => {}
        // Another process made it first; check_dir decides.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(io_error(Code::IoWriteFailed, "can't create", path, &e)),
    }
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}

pub(crate) fn sync_dir(dir: &Path) -> Result<(), Box<Diagnostic>> {
    sync_dir_raw(dir).map_err(|e| io_error(Code::IoWriteFailed, "can't sync", dir, &e))
}

#[cfg(test)]
thread_local! {
    /// A folder whose sync fails, and every folder synced, for tests.
    pub(crate) static FAIL_SYNC: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    pub(crate) static SYNCED: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn sync_dir_raw(dir: &Path) -> std::io::Result<()> {
    let f = open_dir(dir)?;
    sync_open_dir(&f, dir)
}

/// Opens a folder for syncing.
fn open_dir(dir: &Path) -> std::io::Result<Option<fs::File>> {
    #[cfg(unix)]
    {
        fs::File::open(dir).map(Some)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(None)
    }
}

/// Syncs a folder opened by [`open_dir`]; `dir` names it for the test
/// hook.
fn sync_open_dir(f: &Option<fs::File>, dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    {
        if FAIL_SYNC.with(|x| x.borrow().as_deref() == Some(dir)) {
            // PermissionDenied: the kind an open is skipped for, so a sync
            // error of that kind being skipped too fails a test (Terra
            // #9628).
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        SYNCED.with(|s| s.borrow_mut().push(dir.to_path_buf()));
    }
    #[cfg(not(test))]
    let _ = dir;
    match f {
        Some(f) => f.sync_all(),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_mdkg_and_base64url_sha256() {
        let fp = fingerprint(&PublicKey::Ed25519([0; 32]));
        assert!(fp.starts_with("mdkg:"));
        assert_eq!(fp.len(), "mdkg:".len() + 43);
        assert!(!fp.contains('=') && !fp.contains('+') && !fp.contains('/'));
        assert_ne!(fp, fingerprint(&PublicKey::Ed25519([1; 32])));
    }

    #[test]
    fn person_names_are_safe_path_parts() {
        assert_eq!(person_name("human:nick"), Some("nick"));
        assert_eq!(person_name("human:a.b-c_1"), Some("a.b-c_1"));
        for bad in [
            "nick",
            "human:",
            "human:../x",
            "human:a/b",
            "human:.x",
            "human:Ana",
        ] {
            assert_eq!(person_name(bad), None, "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn keys_are_published_once_and_read_only_when_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::testutil::TempDir::new("device");
        let (_l, held) = crate::testutil::held();
        let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
        let resolved = fs::canonicalize(tmp.path()).expect("resolve");
        assert_eq!(dir.path(), person_dir(&resolved, "nick"));
        dir.write_new(&held, "k", b"x").expect("write");
        assert!(dir.write_new(&held, "k", b"y").is_err(), "never replaced");
        assert_eq!(dir.read_private(&held, "k").expect("read"), b"x");
        let partials = || {
            fs::read_dir(dir.path())
                .expect("list")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
                .count()
        };
        assert_eq!(partials(), 0, "no temporary left behind");
        fs::set_permissions(dir.path().join("k"), fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(dir.read_private(&held, "k").is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(PersonDir::open(tmp.path(), "nick", false, &held).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_anywhere_under_the_state_folder_are_refused() {
        let (_l, held) = crate::testutil::held();
        use std::os::unix::fs::symlink;
        let tmp = crate::testutil::TempDir::new("links");
        let state = tmp.path().join("state");
        let elsewhere = tmp.path().join("checkout");
        fs::create_dir_all(&elsewhere).expect("dir");
        // The person folder, then the `person` folder, as a link to a
        // folder of this user's: the key must never land there.
        for link in ["mdkg/person/nick", "mdkg/person"] {
            let _ = fs::remove_dir_all(&state);
            let at = state.join(link);
            fs::create_dir_all(at.parent().expect("parent")).expect("dir");
            symlink(&elsewhere, &at).expect("link");
            let err = PersonDir::open(&state, "nick", true, &held).expect_err("refused");
            assert!(err.message.contains("is a link"), "{link}: {}", err.message);
        }
        // A key file that is a link isn't read.
        let _ = fs::remove_dir_all(&state);
        let dir = PersonDir::open(&state, "nick", true, &held).expect("dir");
        fs::write(elsewhere.join("secret"), b"s").expect("write");
        symlink(elsewhere.join("secret"), dir.path().join("k")).expect("link");
        assert!(dir.read_private(&held, "k").is_err());
        assert!(
            dir.write_new(&held, "k", b"x").is_err(),
            "a link is never replaced either"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_state_folder_others_can_write_is_refused() {
        let (_l, held) = crate::testutil::held();
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::testutil::TempDir::new("shared");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).expect("dir");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(PersonDir::open(&state, "nick", true, &held).is_err());
        assert!(
            PersonDir::open(tmp.path(), "../x", true, &held).is_err(),
            "a name, not a path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn competing_creators_publish_exactly_one_whole_file() {
        let tmp = crate::testutil::TempDir::new("race");
        let locks = crate::testutil::TempDir::new("race-locks");
        let lock = crate::lock::UserLock::at(&locks.path().join("l")).expect("lock");
        let state = tmp.path().to_path_buf();
        let threads: Vec<_> = (0u8..8)
            .map(|i| {
                let state = state.clone();
                let lock = lock.clone();
                std::thread::spawn(move || {
                    // Each creator takes the lock, as each process would.
                    let held = lock.hold().expect("hold");
                    let dir = PersonDir::open(&state, "nick", true, &held).expect("dir");
                    let bytes = vec![i; 64 * 1024];
                    dir.write_new(&held, "k", &bytes).is_ok().then_some(bytes)
                })
            })
            .collect();
        let winners: Vec<Vec<u8>> = threads
            .into_iter()
            .filter_map(|t| t.join().expect("join"))
            .collect();
        assert_eq!(winners.len(), 1, "one creator wins");
        let held = lock.hold().expect("hold");
        let dir = PersonDir::open(&state, "nick", false, &held).expect("dir");
        assert_eq!(
            dir.read_private(&held, "k").expect("read"),
            winners[0],
            "the winner's bytes, whole"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let tmp = crate::testutil::TempDir::new("fifo");
        let (_l, held) = crate::testutil::held();
        let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
        let fifo = dir.path().join("k");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(d.read_private(&held, "k").is_err());
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read_private returned instead of blocking on the FIFO");
        assert!(refused);
    }

    #[test]
    fn a_state_folder_in_a_repository_is_refused() {
        let (_l, held) = crate::testutil::held();
        let tmp = crate::testutil::TempDir::new("repo");
        let repo = tmp.path().join("checkout");
        fs::create_dir_all(repo.join(".git")).expect("dir");
        let err = PersonDir::open(&repo.join("state"), "nick", true, &held).expect_err("refused");
        assert!(err.message.contains("git work tree"), "{}", err.message);
        // A .git file (a worktree or submodule) counts too.
        let wt = tmp.path().join("worktree");
        fs::create_dir_all(&wt).expect("dir");
        fs::write(wt.join(".git"), b"gitdir: elsewhere").expect("write");
        assert!(PersonDir::open(&wt.join("deep/state"), "nick", true, &held).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_others_can_write_above_the_state_folder_is_refused() {
        let (_l, held) = crate::testutil::held();
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::testutil::TempDir::new("ancestor");
        let shared = tmp.path().join("shared");
        fs::create_dir_all(shared.join("state")).expect("dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        let err = PersonDir::open(&shared.join("state"), "nick", true, &held).expect_err("refused");
        assert!(
            err.message.contains("above the state folder"),
            "{}",
            err.message
        );
        // Sticky, as /tmp is: allowed.
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).expect("chmod");
        PersonDir::open(&shared.join("state"), "nick", true, &held).expect("sticky is fine");
    }

    #[test]
    fn a_work_tree_root_below_the_state_folder_is_refused() {
        let (_l, held) = crate::testutil::held();
        let tmp = crate::testutil::TempDir::new("repo-below");
        for part in ["mdkg", "mdkg/person", "mdkg/person/nick"] {
            let state = tmp.path().join(format!("s-{}", part.len()));
            fs::create_dir_all(state.join(part).join(".git")).expect("dir");
            #[cfg(unix)]
            if part.ends_with("nick") {
                use std::os::unix::fs::PermissionsExt;
                let nick = state.join(part);
                fs::set_permissions(&nick, fs::Permissions::from_mode(0o700)).expect("chmod");
            }
            let err = PersonDir::open(&state, "nick", true, &held).expect_err("refused");
            assert!(
                err.message.contains("git work tree"),
                "{part}: {}",
                err.message
            );
        }
    }

    #[test]
    fn missing_state_ancestors_are_created() {
        let (_l, held) = crate::testutil::held();
        let tmp = crate::testutil::TempDir::new("deep");
        let state = tmp.path().join("a/b/c/state");
        let dir = PersonDir::open(&state, "nick", true, &held).expect("dir");
        dir.write_new(&held, "k", b"x").expect("write");
        assert_eq!(dir.read_private(&held, "k").expect("read"), b"x");
    }

    #[test]
    fn the_sweep_touches_only_this_files_own_temporaries() {
        let tmp = crate::testutil::TempDir::new("sweep");
        let (_l, held) = crate::testutil::held();
        let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
        let keep = [
            ".k.backup.partial",
            ".k.0011223344556677.partial.bak",
            ".k.00112233445566ZZ.partial",
        ];
        let temp = dir.path().join(".k.0011223344556677.partial");
        for name in keep {
            fs::write(dir.path().join(name), b"other").expect("write");
        }
        // Under the lock no creator is live, so a temporary of the key,
        // however new, is a crash's: a refused create removes it, and so
        // does a read (Terra #8765).
        fs::write(&temp, b"key material").expect("write");
        fs::write(dir.path().join("k"), b"existing").expect("write");
        assert!(dir.write_new(&held, "k", b"x").is_err());
        assert!(!temp.exists(), "a refused create clears it");
        fs::write(&temp, b"key material").expect("write");
        assert!(dir.read_private(&held, "k").is_err(), "k isn't 0600 here");
        assert!(!temp.exists(), "a read clears it");
        for name in keep {
            assert!(dir.path().join(name).exists(), "{name} kept");
        }
    }

    /// Stops a publish at each step, as a crash would, and checks what is
    /// left: no key (a later create succeeds and clears the temporary) or
    /// the whole key.
    #[test]
    fn a_crash_at_each_step_leaves_no_key_or_the_whole_key() {
        for step in [
            Step::TempCreated,
            Step::Written,
            Step::Synced,
            Step::Renamed,
        ] {
            let tmp = crate::testutil::TempDir::new("crash");
            let (_l, held) = crate::testutil::held();
            let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
            CRASH_AT.with(|c| c.set(Some(step)));
            assert!(dir.write_new(&held, "k", b"whole key").is_err(), "{step:?}");
            CRASH_AT.with(|c| c.set(None));
            let dir = PersonDir::open(tmp.path(), "nick", false, &held).expect("reopen");
            if step == Step::Renamed {
                assert_eq!(dir.read_private(&held, "k").expect("read"), b"whole key");
                assert!(
                    dir.write_new(&held, "k", b"other").is_err(),
                    "never replaced"
                );
            } else {
                assert!(!dir.path().join("k").exists(), "{step:?}: no key");
                dir.write_new(&held, "k", b"retry").expect("retry");
                assert_eq!(dir.read_private(&held, "k").expect("read"), b"retry");
            }
            let left = fs::read_dir(dir.path())
                .expect("list")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
                .count();
            assert_eq!(left, 0, "{step:?}: no temporary left after the next use");
        }
    }

    /// Two openers, one creating the state path and one not, both leave
    /// every folder on it synced; a second open after the first syncs the
    /// whole path again rather than trusting the first.
    #[test]
    fn every_open_reaches_the_whole_path() {
        let tmp = crate::testutil::TempDir::new("deep-open");
        let (_l, held) = crate::testutil::held();
        let state = tmp.path().join("a/b/c/state");
        PersonDir::open(&state, "nick", true, &held).expect("creator");
        let dir = PersonDir::open(&state, "nick", false, &held).expect("opener");
        assert!(dir.path().ends_with("a/b/c/state/mdkg/person/nick"));
    }

    fn synced() -> Vec<PathBuf> {
        SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// An ancestor's sync fails: the open fails, and the next opener syncs
    /// the whole owned path and the first folder above it (Terra #9305,
    /// Claude #9303).
    #[test]
    fn a_failed_ancestor_sync_is_redone_by_the_next_opener() {
        let tmp = crate::testutil::TempDir::new("barrier");
        let (_l, held) = crate::testutil::held();
        let state = tmp.path().join("a/b/state");
        fs::create_dir_all(&state).expect("dir");
        let real = fs::canonicalize(&state).expect("real");
        let ancestor = real.parent().expect("b").to_path_buf();
        FAIL_SYNC.with(|f| *f.borrow_mut() = Some(ancestor.clone()));
        assert!(
            PersonDir::open(&state, "nick", true, &held).is_err(),
            "the barrier failed"
        );
        FAIL_SYNC.with(|f| *f.borrow_mut() = None);
        let _ = synced();
        PersonDir::open(&state, "nick", true, &held).expect("next opener");
        let done = synced();
        let owned: Vec<&Path> = real
            .ancestors()
            .take_while(|d| owned_by_me(d).expect("readable"))
            .collect();
        for dir in &owned {
            assert!(done.iter().any(|d| d == dir), "{} synced", dir.display());
        }
        // Run as root, every folder is "owned", and there is none above.
        if let Some(first_other) = real.ancestors().nth(owned.len()) {
            assert!(
                done.iter().any(|d| d == first_other),
                "the first folder above is synced"
            );
        }
        let nick = real.join("mdkg/person/nick");
        assert!(done.contains(&nick));
    }

    /// A publication whose folder sync fails leaves a key; a read through
    /// the same PersonDir syncs the folder before it returns the key.
    #[test]
    fn a_read_after_a_failed_publication_barrier_syncs_first() {
        let tmp = crate::testutil::TempDir::new("read-barrier");
        let (_l, held) = crate::testutil::held();
        let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
        FAIL_SYNC.with(|f| *f.borrow_mut() = Some(dir.path().to_path_buf()));
        assert!(
            dir.write_new(&held, "k", b"key").is_err(),
            "the folder sync failed"
        );
        FAIL_SYNC.with(|f| *f.borrow_mut() = None);
        let _ = synced();
        assert_eq!(dir.read_private(&held, "k").expect("read"), b"key");
        assert_eq!(
            synced(),
            vec![dir.path().to_path_buf()],
            "the barrier ran first"
        );
        // A second handle on the same folder syncs before its read too.
        let other = PersonDir::open(tmp.path(), "nick", false, &held).expect("dir");
        let _ = synced();
        let _ = other.read_private(&held, "k").expect("read");
        assert_eq!(synced(), vec![dir.path().to_path_buf()], "every read");
    }

    /// Removing a file that's already gone still syncs the folder, so a
    /// retry after a failed sync repairs the barrier (Terra #9855 5,
    /// #10099).
    #[test]
    fn a_removal_retry_syncs_the_folder() {
        let tmp = crate::testutil::TempDir::new("remove-retry");
        let (_l, held) = crate::testutil::held();
        let dir = PersonDir::open(tmp.path(), "nick", true, &held).expect("dir");
        dir.write_new(&held, "k", b"key").expect("write");
        FAIL_SYNC.with(|f| *f.borrow_mut() = Some(dir.path().to_path_buf()));
        assert!(dir.remove(&held, "k").is_err(), "the unlink's sync failed");
        FAIL_SYNC.with(|f| *f.borrow_mut() = None);
        let _ = synced();
        dir.remove(&held, "k").expect("already gone");
        assert_eq!(synced(), vec![dir.path().to_path_buf()], "synced anyway");
    }

    /// The first folder above opens but fails to sync: that's an error,
    /// not a skipped unreadable folder (Terra #9466).
    #[test]
    fn a_failed_sync_of_the_first_folder_above_is_an_error() {
        let tmp = crate::testutil::TempDir::new("above");
        let (_l, held) = crate::testutil::held();
        let real = fs::canonicalize(tmp.path()).expect("real");
        let owned = real
            .ancestors()
            .take_while(|d| owned_by_me(d).expect("readable"))
            .count();
        let Some(first_other) = real.ancestors().nth(owned) else {
            return; // run as root: nothing above to test
        };
        if fs::File::open(first_other).is_err() {
            return; // an unreadable folder above (a 0711 /home) is skipped, by design (Terra #9628)
        }
        FAIL_SYNC.with(|f| *f.borrow_mut() = Some(first_other.to_path_buf()));
        let err = PersonDir::open(tmp.path(), "nick", true, &held).expect_err("refused");
        FAIL_SYNC.with(|f| *f.borrow_mut() = None);
        assert!(err.message.contains("can't sync"), "{}", err.message);
    }
}
