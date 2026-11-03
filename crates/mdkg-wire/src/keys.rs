//! Ed25519 key files: a machine's identity, and the coordinator host's TLS
//! key (S5).
//!
//! - A machine's key: `<state>/mdkg/machine/key.ed25519`, its id beside it
//!   in `id` (a UUIDv7);
//! - the host's TLS key: `<state>/mdkg/serve/tls/key.ed25519`, in a folder
//!   of its own, so `<state>/mdkg/serve` can hold `access.log` and serve's
//!   other files (orchestrator's call on Terra #8091);
//!
//! where `<state>` is `$XDG_STATE_HOME`, else `$HOME/.local/state`. A key
//! file holds the 32-byte seed and nothing else, mode 0600, in a 0700
//! folder the user owns that holds only key files. A key file that is a
//! link, or a file or folder that is someone else's or open to the group
//! or others, is refused, never repaired: something changed it, and a
//! person should look. Any other file in a key folder is refused too,
//! naming it, a `.DS_Store` that Finder leaves included: delete it. The
//! exceptions are the folder's own `.mint.lock` and a minting's
//! `.key.tmp-*` staging files, each a regular file of ours (#8494).
//!
//! The key folder's path is resolved by realpath first, and every step
//! after works on the real path, so no link on the way is followed twice.
//! Every folder on the real path must be the user's or root's and closed to
//! writes by others (a sticky folder such as `/tmp` excepted), so nobody
//! else can swap a folder on the way; the key folder itself is opened
//! without following links and must be the one checked, by device and
//! inode, and its files are listed and read through that handle.
//!
//! **Minting** (orchestrator's call on Terra #8369) runs under one lock
//! per key folder, `.mint.lock` inside it: a regular file of ours, opened
//! without following links or blocking, flocked exclusively for the whole
//! minting (and shared while a load reads). Under it, a folder already
//! holding its files is simply loaded. Otherwise any `.key.tmp-*` staging
//! file in it is a dead minting's and is removed, and each missing file is
//! written to a fresh staging file (`create_new`, 0600), synced and
//! renamed into place: a machine's id first, then its key, so an
//! interrupted minting leaves at most an id, which the next one completes.
//! A key without its id is damage, refused with how to recover, never
//! given a new id (Terra #6769). The key folder, and the parent of every
//! folder a minting makes, are synced through read-only handles; any
//! failure fails the minting. Before any caller uses a key folder, it, the
//! user's folders above it and the first folder above those are synced.
//! The private key never appears in `Debug` output, an error or a log.
//!
//! **Residual: access control lists.** The checks read owners and mode
//! bits, not ACLs. On macOS an ACL entry can grant another user access to
//! a 0700 folder or a 0600 file, set directly or inherited from a parent
//! folder's inheritable entries. Only the owner, root, or someone the
//! owner granted `writesecurity` can add one, so this trusts the user's
//! own account and root, and whoever they delegated to (orchestrator,
//! #6769 #3; Terra #8091).
//!
//! **Residuals under the stop rule** (orchestrator, #8330, #8369, #8614):
//! - Key material: every buffer this module holds a seed in is zeroized
//!   on drop and on every failure, with `zeroize` (approved, #10001):
//!   `read_private`'s buffer, sized so it never reallocates; `read_key`'s
//!   seed; a minting's staged bytes, generated in place; the copy
//!   `ServeKey::seed` gives the TLS signer; and the seed passed by value
//!   to `MachineKey::from_parts` or `ServeKey::from_seed`, whose caller
//!   keeps their own copy. ed25519-dalek zeroizes a `SigningKey` on drop.
//!   What zeroize can't reach: copies the compiler makes when it moves an
//!   array on the stack, and what ed25519-dalek and ring keep inside
//!   while a key is in use (orchestrator's call on Terra 1 #9945; Terra 7
//!   #10073; Terra 3 #10212, Terra 4 #10218).
//! - A machine folder whose key was deleted under an existing id looks
//!   like an interrupted first minting, and a fresh key is minted under
//!   that id; wire sees no enrolment record to tell them apart. An
//!   enrolled machine then fails every request with `bad_signature`:
//!   move the folder aside and enrol again with a new identity.
//! - An entry whose name isn't valid UTF-8 is read lossily, so a name like
//!   `.key.tmp-\xff` isn't checked or removed as staging and passes the
//!   keys-only check; every name mdkg makes is ASCII (#8636).
//! - On macOS every folder sync is `F_FULLFSYNC`, with no fallback; on a
//!   volume without it (some network or FAT volumes) every key folder
//!   open fails, closed (#8638).
//! - Key-folder state from unreleased builds is unsupported: a
//!   `.minting.lock` they left is refused with a hint to move the folder
//!   aside (#8646).
//! - Folders are made with mode 0700 through the umask. Under an umask
//!   such as 0777 a new key folder is mode 000, and opening it fails: it
//!   fails closed.
//! - A file of the user's own named `.key.tmp-*` inside a key folder is
//!   taken for a dead minting's staging file and removed.
//! - The lock is `flock`. Where the file system emulates it with
//!   per-process record locks (Linux NFS), threads of one process aren't
//!   kept apart; where locking is unsupported (`ENOLCK`), minting and
//!   loading fail. The state folder is expected on a local disk.
//! - Directory fsync failures propagate but have no fault-injected tests,
//!   and the folder-mode tests can't show a refusal when run as root.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use mdkg_format::Diagnostic;
use mdkg_ops::wire::{Signature, Signer, profile};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::b64;

/// `N` bytes from the operating system's generator.
pub fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut out = [0u8; N];
    fill_random(&mut out)?;
    Ok(out)
}

/// Fills `buf` from the operating system's generator, in place.
fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    File::open("/dev/urandom")?.read_exact(buf)
}

/// A key's fingerprint: `mdkg:` and the base64url (no padding) SHA-256 of
/// its public key bytes, for every key (S5, S41): an Ed25519 key's raw 32
/// bytes, a P-256 key's SEC1 uncompressed point (65 bytes).
pub fn fingerprint(public_key: &[u8]) -> String {
    format!("mdkg:{}", b64::encode_url(&Sha256::digest(public_key)))
}

/// `$XDG_STATE_HOME`, else `$HOME/.local/state`. Relative values are
/// ignored, as the XDG spec says.
pub fn state_dir() -> io::Result<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(p) = absolute(std::env::var_os("XDG_STATE_HOME")) {
        return Ok(p);
    }
    absolute(std::env::var_os("HOME"))
        .map(|h| h.join(".local").join("state"))
        .ok_or_else(|| io::Error::other("neither XDG_STATE_HOME nor HOME is an absolute path"))
}

/// The folder of a machine's key and id: `<state>/mdkg/machine`.
pub fn machine_dir(state: &Path) -> PathBuf {
    state.join("mdkg").join("machine")
}

/// serve's own folder: `<state>/mdkg/serve`, for `access.log` and the
/// like. The TLS key is in [`tls_dir`] below it.
pub fn serve_dir(state: &Path) -> PathBuf {
    state.join("mdkg").join("serve")
}

/// The folder of the host's TLS key, keys only: `<state>/mdkg/serve/tls`.
pub fn tls_dir(state: &Path) -> PathBuf {
    serve_dir(state).join("tls")
}

const KEY_FILE: &str = "key.ed25519";
const ID_FILE: &str = "id";
/// An id file holds a UUIDv7's 36 characters; anything longer is refused.
const ID_MAX: u64 = 64;

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{}: {why}", path.display()),
    )
}

/// A real folder (not a link), the user's, closed to the group and others.
fn check_private_dir(dir: &Path) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(dir)?;
    if !m.file_type().is_dir() {
        return Err(refuse(dir, "is not a folder"));
    }
    if m.uid() != euid() {
        return Err(refuse(dir, "is owned by another user"));
    }
    if m.mode() & 0o077 != 0 {
        return Err(refuse(
            dir,
            "is open to the group or others; it must be 0700",
        ));
    }
    Ok(m)
}

/// One existing folder on a key folder's real path: ours or root's, and
/// nobody else can write it, so nobody else can swap what lies below it
/// (Terra #6769, #8091). A sticky folder (`/tmp`) may be open: there, only
/// an entry's owner can rename or remove it. The real path has no links,
/// so one found here is a swap, refused.
fn check_ancestor(path: &Path) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    let uid = m.uid();
    if uid != euid() && uid != 0 {
        return Err(refuse(path, "is owned by another user"));
    }
    if m.file_type().is_symlink() {
        return Err(refuse(path, "became a link while it was being checked"));
    }
    if !m.file_type().is_dir() {
        return Err(refuse(path, "is not a folder"));
    }
    let sticky = m.mode() & 0o1000 != 0;
    if m.mode() & 0o022 != 0 && !sticky {
        return Err(refuse(
            path,
            "can be written by the group or others; `chmod go-w` it if only you should write there",
        ));
    }
    Ok(m)
}

/// Where a key folder really is (Terra #8091, orchestrator's call): `dir`
/// made absolute against the working directory, its deepest existing
/// ancestor resolved by realpath, and the missing rest appended as plain
/// names. Every later step works on this path, so no link on the way,
/// wherever it lives and whoever can repoint it, is followed again. Also
/// returns how many trailing components don't exist yet.
fn resolve(dir: &Path) -> io::Result<(PathBuf, usize)> {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let mut rest = Vec::new();
    let mut existing = abs.as_path();
    while !path_exists(existing)? {
        match existing.components().next_back() {
            Some(Component::Normal(n)) => rest.push(n.to_owned()),
            _ => return Err(refuse(dir, "has a missing part that isn't a plain name")),
        }
        existing = existing
            .parent()
            .ok_or_else(|| refuse(dir, "has no existing ancestor"))?;
    }
    let mut real = fs::canonicalize(existing)?;
    let missing = rest.len();
    for n in rest.into_iter().rev() {
        real.push(n);
    }
    Ok((real, missing))
}

/// Checks every existing folder of the real path `real`, from the root
/// down; the last `missing` components don't exist yet.
fn check_chain(real: &Path, missing: usize) -> io::Result<()> {
    let mut chain: Vec<&Path> = real.ancestors().skip(missing).collect();
    chain.reverse();
    for a in chain {
        check_ancestor(a)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The folders this thread synced, and the files it published (as
    /// their paths, in order), for tests of the barriers.
    static SYNCED: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn note_sync(_dir: &Path) {
    #[cfg(test)]
    SYNCED.with(|s| s.borrow_mut().push(_dir.to_path_buf()));
}

/// Syncs the open folder `f` at `dir` to stable storage: `sync_all`,
/// which is `F_FULLFSYNC` on macOS, where plain `fsync` leaves the drive's
/// cache unflushed (Terra #8494). A failure names the folder (#8500).
fn full_sync(f: &File, dir: &Path) -> io::Result<()> {
    f.sync_all().map_err(|e| {
        refuse(
            dir,
            &format!("couldn't be synced to make the key folder durable: {e}"),
        )
    })?;
    note_sync(dir);
    Ok(())
}

/// Syncs a staged file's contents to stable storage before it is
/// published, and notes it in the test record only once the sync
/// succeeded, so a failed sync is never recorded (Terra 3 #9650). The
/// ordering test checks the order of syncs and publications; it doesn't
/// check that this function calls `sync_all` (#9699, #9707).
fn sync_file(f: &File, path: &Path) -> io::Result<()> {
    f.sync_all()?;
    note_sync(path);
    Ok(())
}

/// Syncs a folder, so the entries made in it survive a crash.
fn sync_dir(dir: &Path) -> io::Result<()> {
    let f = File::open(dir).map_err(|e| {
        refuse(
            dir,
            &format!("can't be opened to make the key folder durable: {e}"),
        )
    })?;
    full_sync(&f, dir)
}

/// The barrier every caller passes before it accepts a key folder, not
/// only the one that minted it (Terra #8091): the folder and each
/// ancestor of ours above it are synced, so a minting whose own sync
/// failed, or that another process is still finishing, is durable before
/// its key is used. The first folder above that isn't ours is synced too,
/// since it holds the entry for our highest folder (Terra #8252: `/tmp`
/// holding `/tmp/state`), and a failure there fails the open. A clean
/// folder syncs at once. A folder the barrier can't open or sync (a home
/// folder at 0311, say) fails the open too: durability that can't be
/// established is not assumed (Terra #8330, orchestrator's call).
fn publish_barrier(kd: &KeyDir) -> io::Result<()> {
    // The key folder itself through its pinned handle, so the folder
    // synced is the one whose keys are read (Claude 5 #8306).
    full_sync(&kd.fd, &kd.real)?;
    let real = kd.real.as_path();
    for a in real.ancestors().skip(1) {
        let ours = fs::symlink_metadata(a)?.uid() == euid();
        let f = File::open(a).map_err(|e| {
            refuse(
                a,
                &format!("can't be opened to make the key folder durable: {e}"),
            )
        })?;
        full_sync(&f, a)?;
        if !ours {
            break;
        }
    }
    Ok(())
}

/// A checked key folder: its real path, for messages, and the handle
/// pinned to it, through which its files are listed and read, so the pin
/// covers every read (Claude 4 #8244).
#[derive(Debug)]
struct KeyDir {
    real: PathBuf,
    fd: File,
    /// The folder's lock, held while it is minted or read.
    lock: Option<File>,
}

impl KeyDir {
    fn path(&self, name: &str) -> PathBuf {
        self.real.join(name)
    }

    /// Whether `name` is a regular file of ours, empty if `empty`: the
    /// lock file, or a staging file, and not a link or a folder (#8330).
    /// An entry that vanished since it was listed is gone, not refused
    /// (#8487, #8500).
    fn is_plain_file(&self, name: &str, empty: bool) -> io::Result<bool> {
        let st = match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        Ok(
            rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile
                && (!empty || st.st_size == 0)
                && st.st_uid == euid(),
        )
    }

    fn has(&self, name: &str) -> io::Result<bool> {
        match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The names in it, `.` and `..` aside.
    fn entries(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in rustix::fs::Dir::read_from(&self.fd).map_err(io::Error::from)? {
            let e = e.map_err(io::Error::from)?;
            let n = e.file_name().to_string_lossy().into_owned();
            if n != "." && n != ".." {
                out.push(n);
            }
        }
        Ok(out)
    }
}

/// Opens the folder `dir` without following a link and checks it is the
/// one `seen` described, by device and inode (Terra #8091).
fn pin(dir: &Path, seen: &fs::Metadata) -> io::Result<File> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(dir)
        .map_err(|e| {
            // ELOOP on Linux, ENOTDIR on macOS for a link with
            // O_DIRECTORY.
            match fs::symlink_metadata(dir) {
                Ok(m) if m.file_type().is_symlink() => refuse(dir, "is a link"),
                _ => e,
            }
        })?;
    let m = f.metadata()?;
    if (m.dev(), m.ino()) != (seen.dev(), seen.ino()) {
        return Err(refuse(dir, "changed while it was being checked"));
    }
    Ok(f)
}

/// Reads the private file `name` in the pinned folder `dir`, through its
/// handle: no link, no FIFO, the user's, 0600 or tighter, at most `max`
/// bytes. Zeroized on drop, as on every failure.
fn read_private(dir: &KeyDir, name: &str, max: u64) -> io::Result<Zeroizing<Vec<u8>>> {
    use rustix::fs::OFlags;
    let path = dir.path(name);
    let path = path.as_path();
    let fd = rustix::fs::openat(
        &dir.fd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| {
        if e == rustix::io::Errno::LOOP {
            refuse(path, "is a link")
        } else {
            io::Error::from(e)
        }
    })?;
    let mut f = File::from(fd);
    let m = f.metadata()?;
    if !m.file_type().is_file() {
        return Err(refuse(path, "is not a regular file"));
    }
    if m.uid() != euid() {
        return Err(refuse(path, "is owned by another user"));
    }
    if m.mode() & 0o077 != 0 {
        return Err(refuse(
            path,
            "is open to the group or others; it must be 0600",
        ));
    }
    if m.len() > max {
        return Err(refuse(path, "is too long"));
    }
    // Room for every byte `take` lets through, so the buffer never moves
    // and leaves a copy behind; dropped, on any failure too, it is
    // zeroized.
    let mut out = Zeroizing::new(Vec::with_capacity(max as usize + 1));
    Read::by_ref(&mut f).take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(refuse(path, "is too long"));
    }
    Ok(out)
}

/// A file to mint into a key folder, zeroized on drop.
struct Minted {
    name: &'static str,
    bytes: Zeroizing<Vec<u8>>,
}

/// The key folder's lock file.
const LOCK_FILE: &str = ".mint.lock";
/// The prefix of a minting's staging files, inside the key folder.
const STAGING: &str = ".key.tmp-";

/// Opens the key folder's lock file through its pinned handle, making it
/// (0600) if `create`: no link, and no FIFO or device, since the open
/// doesn't block and the file must then be a regular file of ours.
/// `None` when it is missing and not to be made.
fn open_lock(kd: &KeyDir, create: bool) -> io::Result<Option<File>> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let path = kd.path(LOCK_FILE);
    let failed = |e: rustix::io::Errno| match e {
        rustix::io::Errno::LOOP => refuse(&path, "is a link"),
        e => refuse(&path, &format!("can't be opened: {e}")),
    };
    // Open what's there, else create it exclusively, and go round again
    // if another opener won: a plain O_CREAT racing another one can fail
    // with ENOENT on APFS.
    let mut tries = 0;
    let fd = loop {
        tries += 1;
        match rustix::fs::openat(&kd.fd, LOCK_FILE, flags, Mode::empty()) {
            Ok(fd) => break fd,
            Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(failed(e)),
        }
        match rustix::fs::openat(
            &kd.fd,
            LOCK_FILE,
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => break fd,
            Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOENT) if tries < 100 => {}
            Err(e) => return Err(failed(e)),
        }
    };
    let f = File::from(fd);
    let m = f.metadata()?;
    if !m.file_type().is_file() || m.uid() != euid() {
        return Err(refuse(&path, "is not a regular file of yours"));
    }
    Ok(Some(f))
}

fn damaged(real: &Path, lacking: &str) -> io::Error {
    refuse(
        real,
        &format!(
            "is damaged: {lacking} is missing. Nothing was changed. To start over with a new \
             identity, move this folder aside and run again; a new machine identity must be \
             enrolled again"
        ),
    )
}

/// Mints what the folder lacks, under its exclusive lock (orchestrator's
/// call on Terra #8369). `order` is the order files are written in; the
/// ones present must be a prefix of it, the trace of an interrupted
/// minting, and the rest are written. Anything else is damage.
fn mint_into(
    kd: &KeyDir,
    order: &[&str],
    mint: &dyn Fn() -> io::Result<Vec<Minted>>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut present = Vec::new();
    for n in order {
        present.push(kd.has(n)?);
    }
    let have = present.iter().take_while(|p| **p).count();
    if have == order.len() {
        return Ok(());
    }
    if present[have..].iter().any(|p| *p) {
        return Err(damaged(&kd.real, order[have]));
    }
    // Holding the lock, no other minting runs: staging files are dead.
    for n in kd.entries()? {
        if n.starts_with(STAGING) {
            match rustix::fs::unlinkat(&kd.fd, n.as_str(), AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(e) => {
                    return Err(refuse(
                        &kd.path(&n),
                        &format!("is a dead minting's staging file that couldn't be removed: {e}"),
                    ));
                }
            }
        }
    }
    // What an earlier, interrupted minting published must be durable
    // before anything is published after it (Terra #8646).
    if have > 0 {
        full_sync(&kd.fd, &kd.real)?;
    }
    let files = mint()?;
    for name in &order[have..] {
        let f = files
            .iter()
            .find(|f| f.name == *name)
            .ok_or_else(|| refuse(&kd.real, &format!("has nothing to mint for {name}")))?;
        let tmp = format!(
            "{STAGING}{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let fd = rustix::fs::openat(
            &kd.fd,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(io::Error::from)?;
        let mut out = File::from(fd);
        out.write_all(&f.bytes)?;
        sync_file(&out, &kd.path(&tmp))?;
        drop(out);
        rustix::fs::renameat(&kd.fd, tmp.as_str(), &kd.fd, *name).map_err(io::Error::from)?;
        note_sync(&kd.path(name));
        // Each file durable before the next, so a crash never keeps a
        // key whose id was lost (#8646).
        full_sync(&kd.fd, &kd.real)?;
    }
    Ok(())
}

/// Opens the key folder `dir` and returns it pinned, with its lock held:
/// exclusively while `mint` fills in what it lacks, shared for a plain
/// load. Missing folders, the key folder included, are made 0700 only
/// when minting, each checked as it appears and synced into its parent
/// (#8091). `order` lists the folder's files in the order they are
/// written; any other entry is refused (keys only, #8244).
fn open_key_dir(
    dir: &Path,
    order: &[&str],
    mint: Option<&dyn Fn() -> io::Result<Vec<Minted>>>,
) -> io::Result<KeyDir> {
    let (real, missing) = resolve(dir)?;
    check_chain(&real, missing)?;
    if missing > 0 {
        if mint.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: holds no key", real.display()),
            ));
        }
        let mut make: Vec<&Path> = real.ancestors().take(missing).collect();
        make.reverse();
        for m in make {
            match fs::DirBuilder::new().mode(0o700).create(m) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            check_ancestor(m)?;
            sync_dir(m.parent().unwrap_or(m))?;
        }
    }
    let seen = check_private_dir(&real)?;
    let mut kd = KeyDir {
        fd: pin(&real, &seen)?,
        real,
        lock: None,
    };
    let lock = open_lock(&kd, mint.is_some())?;
    if let Some(lock) = &lock {
        if mint.is_some() {
            lock.lock()?;
        } else {
            lock.lock_shared()?;
        }
    }
    kd.lock = lock;
    let real = kd.real.clone();
    for n in kd.entries()? {
        let allowed = order.contains(&n.as_str())
            || (n == LOCK_FILE && kd.is_plain_file(&n, true)?)
            || (n.starts_with(STAGING) && kd.is_plain_file(&n, false)?);
        if n == ".minting.lock" {
            // A pre-release build's lock file (orchestrator, #8646).
            return Err(refuse(
                &real,
                if order.contains(&ID_FILE) {
                    "holds .minting.lock, left by an unreleased build of mdkg; move this \
                     folder aside and run again (a new machine identity must be enrolled again)"
                } else {
                    "holds .minting.lock, left by an unreleased build of mdkg; move this \
                     folder aside and run again (a new TLS key is minted: confirm and update \
                     the pin on every machine that pinned the old one)"
                },
            ));
        }
        if !allowed {
            return Err(refuse(
                &real,
                &format!("holds {n}, which isn't a key file; a key folder holds only its keys"),
            ));
        }
    }
    if let Some(mint) = mint {
        mint_into(&kd, order, mint)?;
    }
    let mut present = Vec::new();
    for n in order {
        present.push(kd.has(n)?);
    }
    if present.iter().all(|p| !*p) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: holds no key", real.display()),
        ));
    }
    if let Some(i) = present.iter().position(|p| !*p) {
        return Err(damaged(&real, order[i]));
    }
    publish_barrier(&kd)?;
    Ok(kd)
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The 32 random bytes of a new key, to mint.
fn new_seed() -> io::Result<Minted> {
    // Generated in place, so no other copy is made.
    let mut bytes = Zeroizing::new(vec![0u8; 32]);
    fill_random(&mut bytes)?;
    Ok(Minted {
        name: KEY_FILE,
        bytes,
    })
}

/// Reads the key in `dir`, which [`open_key_dir`] checked.
fn read_key(dir: &KeyDir) -> io::Result<SigningKey> {
    let path = dir.path(KEY_FILE);
    let bytes = read_private(dir, KEY_FILE, 32)?;
    let mut seed = Zeroizing::new([0u8; 32]);
    if bytes.len() != seed.len() {
        return Err(refuse(&path, "is not a 32-byte Ed25519 seed"));
    }
    seed.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&seed))
}

/// A machine's identity: its id and Ed25519 key (S5). Signs requests as
/// RFC 9421's `keyid`.
pub struct MachineKey {
    id: String,
    key: SigningKey,
}

impl fmt::Debug for MachineKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachineKey")
            .field("id", &self.id)
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl MachineKey {
    /// The machine's identity in `dir` (see [`machine_dir`]), minted on
    /// first use: a fresh UUIDv7 id and a fresh key.
    pub fn load_or_mint(dir: &Path) -> io::Result<Self> {
        let mint = || {
            Ok(vec![
                new_seed()?,
                Minted {
                    name: ID_FILE,
                    bytes: Zeroizing::new(uuid::Uuid::now_v7().to_string().into_bytes()),
                },
            ])
        };
        let real = open_key_dir(dir, &[ID_FILE, KEY_FILE], Some(&mint))?;
        Self::read(&real)
    }

    /// The identity in `dir`, if one was minted; never mints.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let real = open_key_dir(dir, &[ID_FILE, KEY_FILE], None)?;
        Self::read(&real)
    }

    fn read(dir: &KeyDir) -> io::Result<Self> {
        Ok(Self {
            key: read_key(dir)?,
            id: read_id(dir)?,
        })
    }

    /// For tests and for keys held elsewhere (the turn proxy's copy).
    /// The `seed` passed in is zeroized; the caller's own copy is theirs.
    pub fn from_parts(id: impl Into<String>, mut seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Self { id: id.into(), key }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key())
    }
}

fn read_id(dir: &KeyDir) -> io::Result<String> {
    let path = dir.path(ID_FILE);
    let path = path.as_path();
    let mut bytes = read_private(dir, ID_FILE, ID_MAX)?;
    let text =
        String::from_utf8(std::mem::take(&mut *bytes)).map_err(|_| refuse(path, "is not text"))?;
    let id = text.trim();
    match uuid::Uuid::try_parse(id) {
        Ok(u) if u.get_version_num() == 7 && u.to_string() == id => Ok(id.to_owned()),
        _ => Err(refuse(path, "doesn't hold a UUIDv7 machine id")),
    }
}

impl Signer for MachineKey {
    fn key_id(&self) -> &str {
        &self.id
    }

    fn alg(&self) -> &'static str {
        profile::ALG
    }

    fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>> {
        Ok(Signature(self.key.sign(base).to_bytes().to_vec()))
    }
}

/// The coordinator host's TLS key (S5): one per host, shared by every
/// `serve` on it.
pub struct ServeKey {
    key: SigningKey,
}

impl fmt::Debug for ServeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeKey")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl ServeKey {
    /// The key in `dir` (see [`tls_dir`]), minted on first use.
    pub fn load_or_mint(dir: &Path) -> io::Result<Self> {
        let mint = || Ok(vec![new_seed()?]);
        let real = open_key_dir(dir, &[KEY_FILE], Some(&mint))?;
        Ok(Self {
            key: read_key(&real)?,
        })
    }

    /// The `seed` passed in is zeroized; the caller's own copy is theirs.
    pub fn from_seed(mut seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Self { key }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The 32-byte seed, for the TLS signer only; zeroized on drop.
    pub(crate) fn seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.key.to_bytes())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key())
    }
}

/// Ed25519 verification, strict (RFC 8032's checks, no malleable or
/// small-order keys): the primitive `serve`'s pipeline calls for machine
/// signatures.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ed25519;

impl Ed25519 {
    pub fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(public_key) else {
            return false;
        };
        if key.is_weak() {
            return false;
        }
        key.verify_strict(base, &ed25519_dalek::Signature::from_bytes(signature))
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn mints_once_then_loads_the_same_identity() {
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        let a = MachineKey::load_or_mint(&dir).unwrap();
        let b = MachineKey::load_or_mint(&dir).unwrap();
        let c = MachineKey::load(&dir).unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(a.public_key(), c.public_key());
        let m = fs::metadata(dir.join(KEY_FILE)).unwrap();
        assert_eq!(m.mode() & 0o777, 0o600);
        assert_eq!(m.len(), 32);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert!(uuid::Uuid::parse_str(a.id()).is_ok());
        // Its files and its lock, no staging file left behind.
        assert_eq!(names(&dir), [LOCK_FILE, ID_FILE, KEY_FILE]);
    }

    #[test]
    fn load_never_mints() {
        let t = TempDir::new();
        assert!(MachineKey::load(&machine_dir(t.path())).is_err());
        assert!(!machine_dir(t.path()).exists());
    }

    #[test]
    fn refuses_open_modes_links_and_bad_lengths() {
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        MachineKey::load_or_mint(&dir).unwrap();
        let key = dir.join(KEY_FILE);

        fs::set_permissions(&key, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(MachineKey::load(&dir).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(MachineKey::load(&dir).is_err());
        assert!(MachineKey::load_or_mint(&dir).is_err(), "never repaired");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let real = t.path().join("elsewhere");
        fs::rename(&key, &real).unwrap();
        std::os::unix::fs::symlink(&real, &key).unwrap();
        let err = MachineKey::load(&dir).unwrap_err();
        assert!(err.to_string().contains("link"), "{err}");
        fs::remove_file(&key).unwrap();

        File::create(&key).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(MachineKey::load(&dir).is_err());
    }

    #[test]
    fn a_linked_key_folder_is_used_by_its_real_path() {
        // Orchestrator's call on #8091: realpath first, then every step on
        // the real path; the real folder is held to the key-folder rules.
        let t = TempDir::new();
        let real = t.path().join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        fs::create_dir_all(t.path().join("mdkg")).unwrap();
        std::os::unix::fs::symlink(&real, machine_dir(t.path())).unwrap();
        MachineKey::load_or_mint(&machine_dir(t.path())).unwrap();
        assert!(real.join(KEY_FILE).exists());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(MachineKey::load(&machine_dir(t.path())).is_err());
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_key_without_its_id_is_refused_never_given_a_new_one() {
        // Terra #6769 #4: a new id for an existing key would be a new
        // identity for an old key.
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        let a = MachineKey::load_or_mint(&dir).unwrap();
        let seed = fs::read(dir.join(KEY_FILE)).unwrap();
        fs::remove_file(dir.join(ID_FILE)).unwrap();
        for err in [
            MachineKey::load_or_mint(&dir).unwrap_err(),
            MachineKey::load(&dir).unwrap_err(),
        ] {
            let msg = err.to_string();
            assert!(msg.contains("id is missing"), "{msg}");
            assert!(msg.contains("move this folder aside"), "{msg}");
        }
        assert_eq!(names(&dir), [LOCK_FILE, KEY_FILE]);
        assert_eq!(fs::read(dir.join(KEY_FILE)).unwrap(), seed);
        drop(a);
    }

    #[test]
    fn an_empty_folder_is_minted_whole() {
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        fs::create_dir_all(t.path().join("mdkg")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let k = MachineKey::load_or_mint(&dir).unwrap();
        assert_eq!(names(&dir), [LOCK_FILE, ID_FILE, KEY_FILE]);
        assert_eq!(MachineKey::load(&dir).unwrap().id(), k.id());
        // Nothing left beside it.
        assert_eq!(names(&t.path().join("mdkg")), ["machine"]);
    }

    #[test]
    fn concurrent_mints_agree_on_one_identity() {
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        let ids: Vec<(String, [u8; 32])> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let k = MachineKey::load_or_mint(&dir).unwrap();
                        (k.id().to_owned(), k.public_key())
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(names(&t.path().join("mdkg")), ["machine"]);
    }

    /// Writes `name` in `dir` as a minting would, 0600.
    fn put(dir: &Path, name: &str, bytes: &[u8]) {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    fn key_folder(t: &TempDir) -> PathBuf {
        let dir = machine_dir(t.path());
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)
            .unwrap();
        dir
    }

    #[test]
    fn a_crash_after_a_staged_write_is_cleaned_up() {
        // #8369: a minting died after writing a staging file.
        let t = TempDir::new();
        let dir = key_folder(&t);
        put(&dir, ".key.tmp-999-0", &[1u8; 32]);
        let k = MachineKey::load_or_mint(&dir).unwrap();
        assert_eq!(names(&dir), [LOCK_FILE, ID_FILE, KEY_FILE]);
        assert_ne!(fs::read(dir.join(KEY_FILE)).unwrap(), [1u8; 32]);
        assert_eq!(MachineKey::load(&dir).unwrap().id(), k.id());
    }

    #[test]
    fn each_minted_file_is_durable_before_the_next() {
        // Terra #8646: the folder is synced after the id and again after
        // the key, and before completing an interrupted minting.
        let t = TempDir::new();
        let dir = key_folder(&t);
        let count = |v: &[PathBuf]| v.iter().filter(|s| s.as_path() == dir).count();
        SYNCED.with(|s| s.borrow_mut().clear());
        let (real, _) = resolve(&dir).unwrap();
        assert_eq!(real, dir);
        MachineKey::load_or_mint(&dir).unwrap();
        let fresh = SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()));
        // Two in minting, one more in the publish barrier.
        assert!(count(&fresh) >= 3, "{fresh:?}");
        fs::remove_file(dir.join(KEY_FILE)).unwrap();
        MachineKey::load_or_mint(&dir).unwrap();
        let resumed = SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()));
        assert!(count(&resumed) >= 3, "{resumed:?}");
        // The order the fix rests on (#8708, #8731): each publication is
        // followed by a sync of the folder before anything else is
        // published, and the resumed minting syncs before it publishes.
        let synced_between = |v: &[PathBuf], from: usize, to: usize| {
            v[from + 1..to].iter().any(|s| s.as_path() == dir)
        };
        let at = |v: &[PathBuf], name: &str| v.iter().position(|s| *s == dir.join(name));
        let (i, k) = (at(&fresh, ID_FILE).unwrap(), at(&fresh, KEY_FILE).unwrap());
        assert!(i < k && synced_between(&fresh, i, k), "{fresh:?}");
        assert!(
            fresh[k + 1..].iter().any(|s| s.as_path() == dir),
            "{fresh:?}"
        );
        // Each file's staging copy is synced before it is published
        // (Terra 1 #9497).
        let staged_before = |v: &[PathBuf], from: usize, to: usize| {
            v[from..to].iter().any(|s| {
                s.parent() == Some(dir.as_path())
                    && s.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(STAGING))
            })
        };
        assert!(staged_before(&fresh, 0, i), "{fresh:?}");
        assert!(staged_before(&fresh, i + 1, k), "{fresh:?}");
        let k = at(&resumed, KEY_FILE).unwrap();
        assert!(staged_before(&resumed, 0, k), "{resumed:?}");
        assert!(at(&resumed, ID_FILE).is_none());
        assert!(
            resumed[..k].iter().any(|s| s.as_path() == dir),
            "{resumed:?}"
        );
        assert!(
            resumed[k + 1..].iter().any(|s| s.as_path() == dir),
            "{resumed:?}"
        );
    }

    #[test]
    fn a_pre_release_lock_file_is_refused_with_a_hint() {
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        MachineKey::load_or_mint(&dir).unwrap();
        put(&dir, ".minting.lock", b"");
        let err = MachineKey::load(&dir).unwrap_err().to_string();
        assert!(err.contains("unreleased build"), "{err}");
        assert!(err.contains("move this folder"), "{err}");
        assert!(err.contains("machine identity"), "{err}");
        // A TLS key folder's hint is about pins, not enrolment (#8731).
        let tls = tls_dir(t.path());
        ServeKey::load_or_mint(&tls).unwrap();
        put(&tls, ".minting.lock", b"");
        let err = ServeKey::load_or_mint(&tls).unwrap_err().to_string();
        assert!(err.contains("TLS key"), "{err}");
        assert!(err.contains("pin"), "{err}");
    }

    #[test]
    fn a_crash_between_the_id_and_the_key_is_finished() {
        // #8369: the id is written first, so an interrupted minting leaves
        // an id without a key, and the next one completes it under that
        // id. (A key without its id is damage: see
        // a_key_without_its_id_is_refused_never_given_a_new_one.)
        let t = TempDir::new();
        let dir = key_folder(&t);
        let id = uuid::Uuid::now_v7().to_string();
        put(&dir, ID_FILE, id.as_bytes());
        put(&dir, ".key.tmp-999-1", &[1u8; 32]);
        assert!(MachineKey::load(&dir).is_err(), "a load doesn't finish it");
        let k = MachineKey::load_or_mint(&dir).unwrap();
        assert_eq!(k.id(), id);
        assert_eq!(names(&dir), [LOCK_FILE, ID_FILE, KEY_FILE]);
        assert_eq!(MachineKey::load(&dir).unwrap().public_key(), k.public_key());
    }

    #[test]
    fn a_second_minter_waits_for_the_lock_then_loads() {
        // #8369: while one minting holds the lock, another blocks, then
        // loads what the first wrote instead of minting its own.
        let t = TempDir::new();
        let dir = key_folder(&t);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        lock.lock().unwrap();
        let second = std::thread::spawn({
            let dir = dir.clone();
            move || MachineKey::load_or_mint(&dir).map(|k| (k.id().to_owned(), k.public_key()))
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!second.is_finished(), "the second minter waits");
        let id = uuid::Uuid::now_v7().to_string();
        put(&dir, ID_FILE, id.as_bytes());
        put(&dir, KEY_FILE, &[4u8; 32]);
        drop(lock);
        let (got, pk) = second.join().unwrap().unwrap();
        assert_eq!(got, id);
        assert_eq!(pk, MachineKey::from_parts("x", [4u8; 32]).public_key());
    }

    #[test]
    fn an_ancestor_others_can_write_is_refused() {
        // #6769 #2: someone who can write an ancestor could swap the key
        // folder for a link into a place they read.
        let t = TempDir::new();
        let state = t.path().join("state");
        fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
        for mode in [0o770, 0o707, 0o777] {
            fs::set_permissions(&state, fs::Permissions::from_mode(mode)).unwrap();
            let err = MachineKey::load_or_mint(&machine_dir(&state)).unwrap_err();
            assert!(
                err.to_string().contains("written by the group or others"),
                "{err}"
            );
            assert!(err.to_string().contains("chmod go-w"), "{err}");
            assert_eq!(names(&state).len(), 0, "nothing created under it");
        }
        // A sticky folder, ours or root's, may be open (#8091's call).
        fs::set_permissions(&state, fs::Permissions::from_mode(0o1777)).unwrap();
        MachineKey::load_or_mint(&machine_dir(&state)).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        // Loading checks them too.
        fs::set_permissions(&state, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(MachineKey::load(&machine_dir(&state)).is_err());
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        // Folders it makes are 0700.
        let m = fs::metadata(state.join("mdkg")).unwrap();
        assert_eq!(m.mode() & 0o777, 0o700);
    }

    #[test]
    fn a_link_on_the_way_is_followed_and_both_sides_checked() {
        let t = TempDir::new();
        let real = t.path().join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        let link = t.path().join("state");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        MachineKey::load_or_mint(&machine_dir(&link)).unwrap();
        assert!(real.join("mdkg/machine").join(KEY_FILE).exists());
        // The target opened to others is refused through the link.
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(MachineKey::load(&machine_dir(&link)).is_err());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_link_through_a_folder_others_can_write_is_used_by_its_real_path() {
        // Terra #8091: our link `state` runs through `open/repoint`, a
        // link in a folder others can write, to a private folder. Every
        // step after resolving uses the real path, so repointing
        // `repoint` mid-call can't redirect it.
        let t = TempDir::new();
        let real = t.path().join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        let open = t.path().join("open");
        fs::DirBuilder::new().mode(0o700).create(&open).unwrap();
        std::os::unix::fs::symlink(&real, open.join("repoint")).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let state = t.path().join("state");
        std::os::unix::fs::symlink(open.join("repoint"), &state).unwrap();
        let (resolved, missing) = resolve(&machine_dir(&state)).unwrap();
        assert_eq!(resolved, real.join("mdkg").join("machine"));
        assert_eq!(missing, 2);
        assert_eq!(
            open_key_dir(&machine_dir(&state), &[KEY_FILE, ID_FILE], None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let k = MachineKey::load_or_mint(&machine_dir(&state)).unwrap();
        assert!(real.join("mdkg/machine").join(KEY_FILE).exists());
        assert_eq!(
            open_key_dir(&machine_dir(&state), &[KEY_FILE, ID_FILE], None)
                .unwrap()
                .real,
            real.join("mdkg").join("machine")
        );
        assert_eq!(MachineKey::load(&machine_dir(&state)).unwrap().id(), k.id());
        fs::set_permissions(&open, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_folder_swapped_after_its_check_is_refused() {
        let t = TempDir::new();
        let dir = t.path().join("k");
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let seen = check_private_dir(&dir).unwrap();
        pin(&dir, &seen).unwrap();
        fs::rename(&dir, t.path().join("k-old")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let err = pin(&dir, &seen).unwrap_err();
        assert!(err.to_string().contains("changed while"), "{err}");
        fs::remove_dir(&dir).unwrap();
        std::os::unix::fs::symlink(t.path().join("k-old"), &dir).unwrap();
        assert!(pin(&dir, &seen).unwrap_err().to_string().contains("link"));
    }

    #[test]
    fn a_relative_path_is_anchored_to_the_working_directory() {
        // Terra #8091 minor: relative paths resolve, nothing is made.
        let name = format!(
            "mdkg-wire-no-such-{}",
            b64::encode_url(&random::<6>().unwrap())
        );
        let rel = Path::new(&name).join("machine");
        let (real, missing) = resolve(&rel).unwrap();
        let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(real, cwd.join(&name).join("machine"));
        assert_eq!(missing, 2);
        assert!(MachineKey::load(&rel).is_err());
        assert!(!Path::new(&name).exists());
        assert!(resolve(&Path::new(&name).join("..").join("x")).is_err());
    }

    #[test]
    fn reads_go_through_the_pinned_folder() {
        // Claude 4 #8244: a folder swapped after the pin is not read.
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        let a = MachineKey::load_or_mint(&dir).unwrap();
        let kd = open_key_dir(&dir, &[KEY_FILE, ID_FILE], None).unwrap();
        fs::rename(&dir, t.path().join("mdkg/aside")).unwrap();
        let b = MachineKey::load_or_mint(&dir).unwrap();
        assert_ne!(a.id(), b.id());
        let read = MachineKey::read(&kd).unwrap();
        assert_eq!(read.id(), a.id());
        assert_eq!(read.public_key(), a.public_key());
    }

    #[test]
    fn the_barrier_syncs_the_pinned_folder_not_the_path() {
        // Claude 5 #8306: with the folder moved away after the pin, the
        // barrier still syncs the pinned one; by path it would fail.
        let t = TempDir::new();
        let dir = machine_dir(t.path());
        MachineKey::load_or_mint(&dir).unwrap();
        let kd = open_key_dir(&dir, &[KEY_FILE, ID_FILE], None).unwrap();
        fs::rename(&dir, t.path().join("mdkg/aside")).unwrap();
        SYNCED.with(|s| s.borrow_mut().clear());
        publish_barrier(&kd).unwrap();
        let got = SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()));
        assert_eq!(got.first(), Some(&kd.real));
        assert!(got.iter().any(|s| s == &t.path().join("mdkg")));
    }

    #[test]
    fn a_key_folder_holds_only_keys() {
        // #8244: serve's files go beside the TLS key folder, not in it.
        let t = TempDir::new();
        let dir = tls_dir(t.path());
        ServeKey::load_or_mint(&dir).unwrap();
        fs::write(dir.join("access.log"), b"x").unwrap();
        let err = ServeKey::load_or_mint(&dir).unwrap_err();
        assert!(err.to_string().contains("holds access.log"), "{err}");
        fs::remove_file(dir.join("access.log")).unwrap();
        // The lock file must be a regular, empty file of ours: not a
        // link, a folder or a file with content (#8330), nor a FIFO
        // (#8369), which the non-blocking open refuses rather than hangs on.
        fs::remove_file(dir.join(LOCK_FILE)).unwrap();
        std::os::unix::fs::symlink(dir.join(KEY_FILE), dir.join(LOCK_FILE)).unwrap();
        assert!(ServeKey::load_or_mint(&dir).is_err());
        fs::remove_file(dir.join(LOCK_FILE)).unwrap();
        fs::create_dir(dir.join(LOCK_FILE)).unwrap();
        assert!(ServeKey::load_or_mint(&dir).is_err());
        fs::remove_dir(dir.join(LOCK_FILE)).unwrap();
        fs::write(dir.join(LOCK_FILE), b"x").unwrap();
        assert!(ServeKey::load_or_mint(&dir).is_err());
        fs::remove_file(dir.join(LOCK_FILE)).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join(LOCK_FILE))
            .status()
            .unwrap();
        assert!(made.success());
        let err = ServeKey::load_or_mint(&dir).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        fs::remove_file(dir.join(LOCK_FILE)).unwrap();
        ServeKey::load_or_mint(&dir).unwrap();
    }

    #[test]
    fn every_open_passes_the_publish_barrier() {
        // #8091 items 2 and 3, #8244: the folders made on first use are
        // synced into their parents, and every later open, load included,
        // syncs the key folder and its parent before using the key.
        let t = TempDir::new();
        let state = t.path().join("state");
        let dir = machine_dir(&state);
        let synced = || SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()));
        synced();
        MachineKey::load_or_mint(&dir).unwrap();
        let first = synced();
        for p in [t.path(), state.as_path(), &state.join("mdkg"), &dir] {
            assert!(first.iter().any(|s| s == p), "{p:?} not synced: {first:?}");
        }
        let opens: [&dyn Fn() -> io::Result<()>; 2] =
            [&|| MachineKey::load(&dir).map(drop), &|| {
                MachineKey::load_or_mint(&dir).map(drop)
            }];
        for open in opens {
            open().unwrap();
            let again = synced();
            assert!(again.iter().any(|s| s == &dir), "{again:?}");
            assert!(again.iter().any(|s| s == &state.join("mdkg")), "{again:?}");
        }
        // #8252: the first folder above ours is synced too.
        MachineKey::load(&dir).unwrap();
        let first_not_ours = dir
            .ancestors()
            .find(|a| fs::symlink_metadata(a).unwrap().uid() != euid());
        // Run as root, every ancestor is ours and this has nothing to
        // check (Claude 5 #8371).
        if first_not_ours.is_none() {
            eprintln!("every ancestor is ours (root?): first-not-ours sync unchecked");
        }
        if let Some(top) = first_not_ours {
            let got = synced();
            assert!(got.iter().any(|s| s == top), "{top:?} not in {got:?}");
            let higher = top.parent().unwrap_or(top);
            assert!(higher == top || !got.iter().any(|s| s == higher));
        }
        // An ancestor the barrier can't open fails the open (#8330).
        // Root bypasses the mode, so the check holds only for others.
        fs::set_permissions(&state, fs::Permissions::from_mode(0o311)).unwrap();
        let loaded = MachineKey::load(&dir);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        if euid() != 0 {
            let err = loaded.unwrap_err();
            assert!(err.to_string().contains("durable"), "{err}");
        }
    }

    #[test]
    fn the_serve_key_is_minted_whole_too() {
        // #8091: in its own folder, beside serve's access.log.
        let t = TempDir::new();
        fs::create_dir_all(serve_dir(t.path())).unwrap();
        fs::set_permissions(serve_dir(t.path()), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(serve_dir(t.path()).join("access.log"), b"x\n").unwrap();
        let dir = tls_dir(t.path());
        let a = ServeKey::load_or_mint(&dir).unwrap();
        let b = ServeKey::load_or_mint(&dir).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(names(&dir), [LOCK_FILE, KEY_FILE]);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn signs_and_verifies_and_hides_the_key() {
        let k = MachineKey::from_parts("m-1", [7u8; 32]);
        assert_eq!(k.key_id(), "m-1");
        assert_eq!(k.alg(), "ed25519");
        let sig: [u8; 64] = k.sign(b"base").unwrap().0.try_into().unwrap();
        assert!(Ed25519.verify(&k.public_key(), b"base", &sig));
        assert!(!Ed25519.verify(&k.public_key(), b"basf", &sig));
        let mut bad = sig;
        bad[3] ^= 1;
        assert!(!Ed25519.verify(&k.public_key(), b"base", &bad));
        let shown = format!("{k:?}");
        assert!(!shown.contains("SigningKey"), "{shown}");
        assert!(shown.contains("mdkg:"), "{shown}");
    }

    #[test]
    fn weak_keys_never_verify() {
        // The identity point: every signature of the form (R = identity,
        // s = 0) "verifies" under a lax check.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let mut sig = [0u8; 64];
        sig[0] = 1;
        assert!(!Ed25519.verify(&identity, b"anything", &sig));
    }

    #[test]
    fn fingerprint_form() {
        let fp = fingerprint(&[0u8; 32]);
        assert!(fp.starts_with("mdkg:"));
        assert_eq!(fp.len(), 5 + 43);
        assert!(!fp.contains('='));
    }

    #[test]
    fn every_copy_of_a_key_is_zeroized_on_drop() {
        // The zeroize approval (#10001): the long-lived keys and every
        // transient copy keys.rs makes are zeroized when dropped.
        fn zeroized_on_drop<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        let t = TempDir::new();
        let dir = tls_dir(t.path());
        let key = ServeKey::load_or_mint(&dir).unwrap();
        zeroized_on_drop(&key.key);
        let seed = key.seed();
        zeroized_on_drop(&seed);
        let kd = open_key_dir(&dir, &[KEY_FILE], None).unwrap();
        let bytes = read_private(&kd, KEY_FILE, 32).unwrap();
        zeroized_on_drop(&bytes);
        assert_eq!(&bytes[..], &seed[..]);
        // The folder's lock goes with its handle.
        drop(kd);
        let minted = new_seed().unwrap();
        zeroized_on_drop(&minted.bytes);
        assert_eq!(minted.bytes.len(), 32);
        assert_ne!(&minted.bytes[..], &[0u8; 32][..]);
        let machine = MachineKey::from_parts("id", [5u8; 32]);
        zeroized_on_drop(&machine.key);
        // A key read back is the one minted, through the zeroized copies.
        assert_eq!(
            ServeKey::load_or_mint(&dir).unwrap().public_key(),
            key.public_key()
        );
    }
}
