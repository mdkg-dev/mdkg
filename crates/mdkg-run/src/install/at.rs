//! Folders opened by handle, for writes made by install and init.
//!
//! Install checks each path before it writes, but a launched agent can swap
//! a folder for a link in between. So a write never goes by path: each
//! folder is opened one name at a time from the project root's handle,
//! never through a link, and the file is created, renamed or removed
//! relative to its folder's handle. A link swapped in after the check
//! can't redirect it.
//!
//! The links install allows on purpose are followed by the same rules the
//! checks apply: in a skill mirror, only a mirror root linked to the other
//! mirror root (that root is then opened by handle in its place); anywhere
//! else (a config file, AGENTS.md), a link whose target is in the project
//! and outside any `.git` folder.
//!
//! They're followed once, when install plans a file: its [`Landing`] (the
//! real folder and name the path leads to) is recorded, and the file's
//! content is read there. Writes and removals then go to that landing by a
//! walk that follows no link at all, and only once the path still leads
//! there (review of d0a0f84): a link swapped or retargeted after the plan
//! can neither redirect a write nor mix one file's content into another.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

/// The two skill mirrors.
pub(super) const MIRRORS: [[&str; 2]; 2] = [[".agents", "skills"], [".claude", "skills"]];

/// Links followed in one resolution at most, as the system's own limit.
const MAX_HOPS: usize = 40;

/// Whether a walk may follow a link at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Links {
    /// By the rules above.
    Allowed,
    /// Never (install's own scratch folders).
    Never,
}

/// The project root, by handle.
#[derive(Debug)]
pub struct Tree {
    fd: OwnedFd,
    canon: PathBuf,
}

/// A folder in the project, by handle, and its real path from the root.
#[derive(Debug)]
pub struct Folder {
    /// The opened directory, for directory-relative file operations.
    pub fd: OwnedFd,
    pub(super) rel: PathBuf,
}

/// Where a path in the project leads: its real folder from the root, with
/// no link on the way (the part that doesn't exist yet as named), the
/// file's name there, and whether a link at the end led there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Landing {
    pub(super) folder: PathBuf,
    pub(super) name: OsString,
    pub(super) linked: bool,
}

/// What a walk does with a folder that isn't there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Make {
    /// Fails (not found).
    No,
    /// Makes it, with the usual mode.
    Yes,
    /// Stops there, and returns the names left (a file where a folder goes
    /// counts as missing: install removes it first).
    Partial,
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::other(format!("{} {why}", path.display()))
}

/// Which mirror a path is in, if any.
pub(super) fn mirror_of(parts: &[OsString]) -> Option<usize> {
    MIRRORS.iter().position(|m| {
        parts.len() >= 2
            && m.iter()
                .zip(parts)
                .all(|(a, b)| b.as_os_str().eq_ignore_ascii_case(a))
    })
}

fn parts_of(rel: &Path) -> io::Result<Vec<OsString>> {
    rel.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| match c {
            Component::Normal(n) => Ok(n.to_os_string()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} isn't a plain path in the project", rel.display()),
            )),
        })
        .collect()
}

fn is_git(name: &OsStr) -> bool {
    name.eq_ignore_ascii_case(".git")
}

fn is_link(dir: &OwnedFd, name: &OsStr) -> bool {
    rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|s| FileType::from_raw_mode(s.st_mode).is_symlink())
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

impl Tree {
    pub fn open(root: &Path) -> io::Result<Self> {
        let canon = std::fs::canonicalize(root)?;
        let fd = rustix::fs::open(
            &canon,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { fd, canon })
    }

    /// The folder `rel` names; `create` makes the missing ones, with the
    /// usual mode.
    pub fn folder(&self, rel: &Path, create: bool, links: Links) -> io::Result<Folder> {
        let mut hops = 0;
        let make = if create { Make::Yes } else { Make::No };
        Ok(self.walk(parts_of(rel)?, make, links, &mut hops)?.0)
    }

    /// Pins a subtree without following links. Later walks start there,
    /// even if its name is replaced in the parent directory.
    pub fn subtree(&self, rel: &Path, create: bool) -> io::Result<Self> {
        let folder = self.folder(rel, create, Links::Never)?;
        Ok(Self {
            fd: folder.fd,
            canon: self.canon.join(folder.rel),
        })
    }

    fn walk(
        &self,
        mut todo: Vec<OsString>,
        make: Make,
        links: Links,
        hops: &mut usize,
    ) -> io::Result<(Folder, Vec<OsString>)> {
        // Set once a mirror root was followed to the other root: no link
        // after that.
        let mut joined = false;
        'restart: loop {
            let mirror = mirror_of(&todo);
            let mut cur = rustix::fs::openat(&self.fd, ".", dir_flags(), Mode::empty())?;
            let mut rel = PathBuf::new();
            let mut made = false;
            let mut i = 0;
            while i < todo.len() {
                let name = todo[i].clone();
                if is_git(&name) {
                    return Err(refused(&rel.join(&name), "is a .git folder"));
                }
                match rustix::fs::openat(&cur, name.as_os_str(), dir_flags(), Mode::empty()) {
                    Ok(fd) => {
                        cur = fd;
                        rel.push(&name);
                        made = false;
                        i += 1;
                    }
                    Err(Errno::NOENT) if make == Make::Partial => {
                        return Ok((Folder { fd: cur, rel }, todo.split_off(i)));
                    }
                    Err(Errno::NOTDIR) if make == Make::Partial && !is_link(&cur, &name) => {
                        return Ok((Folder { fd: cur, rel }, todo.split_off(i)));
                    }
                    Err(Errno::NOENT) if make == Make::Yes && !made => {
                        match rustix::fs::mkdirat(
                            &cur,
                            name.as_os_str(),
                            Mode::from_raw_mode(0o777),
                        ) {
                            Ok(()) | Err(Errno::EXIST) => made = true,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    Err(_) if is_link(&cur, &name) => {
                        let at = rel.join(&name);
                        if links == Links::Never {
                            return Err(refused(
                                &at,
                                "is a link, which install never follows here",
                            ));
                        }
                        let target = self.target(&cur, &rel, &name)?;
                        // In a mirror, only the mirror root itself, onto the
                        // other mirror root.
                        if let Some(m) = mirror {
                            let other = MIRRORS[1 - m];
                            let onto_other = target.len() == 2
                                && other
                                    .iter()
                                    .zip(&target)
                                    .all(|(a, b)| b.as_os_str().eq_ignore_ascii_case(a));
                            if i != 1 || joined || !onto_other {
                                return Err(refused(
                                    &at,
                                    "is a link in a skill mirror, which install never follows",
                                ));
                            }
                            joined = true;
                        }
                        *hops += 1;
                        if *hops > MAX_HOPS {
                            return Err(Errno::LOOP.into());
                        }
                        let rest = todo.split_off(i + 1);
                        todo = target;
                        todo.extend(rest);
                        continue 'restart;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok((Folder { fd: cur, rel }, Vec::new()));
        }
    }

    /// Where the link `name` in the folder `rel` leads, relative to the
    /// root: refused outside the project, into a `.git` folder, or dangling.
    fn target(&self, dir: &OwnedFd, rel: &Path, name: &OsStr) -> io::Result<Vec<OsString>> {
        let at = rel.join(name);
        let link = rustix::fs::readlinkat(dir, name, Vec::new())?;
        let link = Path::new(OsStr::from_bytes(link.as_bytes()));
        let real = std::fs::canonicalize(self.canon.join(rel).join(link))
            .map_err(|_| refused(&at, "is a dangling link"))?;
        let inside = real
            .strip_prefix(&self.canon)
            .map_err(|_| refused(&at, "is a link outside the project"))?;
        let parts = parts_of(inside)?;
        if parts.iter().any(|p| is_git(p)) {
            return Err(refused(&at, "leads into a .git folder"));
        }
        Ok(parts)
    }

    /// Where `rel` leads now, making nothing. With `follow`, a link at the
    /// end is followed by the same rules (never in a mirror), as a write
    /// lands; without, the name itself, as a removal takes it.
    pub(super) fn locate(&self, rel: &Path, follow: bool) -> io::Result<Landing> {
        let mut parts = parts_of(rel)?;
        let mut hops = 0;
        let mut linked = false;
        loop {
            let Some(name) = parts.pop() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the project root isn't a file",
                ));
            };
            let mut whole = parts.clone();
            whole.push(name.clone());
            let (folder, missing) = self.walk(parts, Make::Partial, Links::Allowed, &mut hops)?;
            let mut real = folder.rel.clone();
            for m in missing.iter().chain([&name]) {
                if is_git(m) {
                    return Err(refused(&real.join(m), "is a .git folder"));
                }
                real.push(m);
            }
            real.pop();
            if !missing.is_empty() || !follow || !is_link(&folder.fd, &name) {
                return Ok(Landing {
                    folder: real,
                    name,
                    linked,
                });
            }
            let at = folder.rel.join(&name);
            if mirror_of(&whole).is_some() {
                return Err(refused(
                    &at,
                    "is a link in a skill mirror, which install never follows",
                ));
            }
            hops += 1;
            if hops > MAX_HOPS {
                return Err(Errno::LOOP.into());
            }
            parts = self.target(&folder.fd, &folder.rel, &name)?;
            linked = true;
        }
    }

    /// The landing's folder, by a walk that follows no link: exactly the
    /// folder recorded, or an error. `None` when it's gone and `create` is
    /// off.
    pub(super) fn exact(&self, at: &Landing, create: bool) -> io::Result<Option<Folder>> {
        let mut hops = 0;
        let make = if create { Make::Yes } else { Make::No };
        match self.walk(parts_of(&at.folder)?, make, Links::Never, &mut hops) {
            Ok((f, _)) => Ok(Some(f)),
            Err(e) if not_found(&e) || e.kind() == io::ErrorKind::NotADirectory => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The landing's bytes and permission bits, `None` when there's no file
    /// (or a folder) there. Never through a link.
    pub(super) fn read(&self, at: &Landing) -> io::Result<Option<(Vec<u8>, Option<u32>)>> {
        Ok(self.read_linked(at)?.map(|(bytes, mode, _)| (bytes, mode)))
    }

    /// [`Tree::read`], with how many names the file has (from the opened
    /// file itself): a hard link elsewhere could be a secret's.
    pub(super) fn read_linked(&self, at: &Landing) -> io::Result<Option<Linked>> {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt;
        let Some(folder) = self.exact(at, false)? else {
            return Ok(None);
        };
        let mut f = match open_file(&folder.fd, &at.name) {
            Ok(f) => f,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::IsADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let meta = f.metadata()?;
        Ok(Some((bytes, Some(meta.mode() & 0o777), meta.nlink())))
    }

    /// Which file a landing names, by the file system's identity rather
    /// than the spelling (a case alias on APFS is one file): the file's
    /// device and inode, or for one not there yet its folder's and the name
    /// (review of 13b3e6e). `None` when its folder isn't there either.
    pub(super) fn identity(&self, at: &Landing) -> Option<FileId> {
        let folder = self.exact(at, false).ok()??;
        match rustix::fs::statat(&folder.fd, at.name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => Some(FileId::Existing(device(&s), s.st_ino)),
            Err(_) => {
                let d = rustix::fs::fstat(&folder.fd).ok()?;
                Some(FileId::New(device(&d), d.st_ino, at.name.clone()))
            }
        }
    }

    /// The landing's permission bits, if a file is there.
    /// `st_mode` is `u16` on macOS and `u32` on Linux.
    #[allow(clippy::unnecessary_cast)]
    pub(super) fn mode(&self, at: &Landing) -> Option<u32> {
        let folder = self.exact(at, false).ok()??;
        rustix::fs::statat(&folder.fd, at.name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .filter(|s| FileType::from_raw_mode(s.st_mode).is_file())
            .map(|s| s.st_mode as u32 & 0o777)
    }

    /// Makes a private folder (0700) right in the root, for staging.
    pub(super) fn private_dir(&self, name: &OsStr) -> io::Result<Folder> {
        rustix::fs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700))?;
        let fd = rustix::fs::openat(&self.fd, name, dir_flags(), Mode::empty())?;
        Ok(Folder {
            fd,
            rel: PathBuf::from(name),
        })
    }

    /// Removes the folder `rel` and everything in it, following no link.
    pub(super) fn remove_tree(&self, rel: &Path) -> io::Result<()> {
        match self.parent(rel)? {
            Some((folder, name)) => remove_tree_at(&folder.fd, &name),
            None => Ok(()),
        }
    }

    /// Removes the folder `rel` if it's empty.
    pub(super) fn remove_empty_dir(&self, rel: &Path) -> io::Result<()> {
        let Some((folder, name)) = self.parent(rel)? else {
            return Err(io::ErrorKind::NotFound.into());
        };
        rustix::fs::unlinkat(&folder.fd, name.as_os_str(), AtFlags::REMOVEDIR).map_err(Into::into)
    }

    /// The folder holding `rel`, and its name; `None` when the folder is
    /// gone.
    fn parent(&self, rel: &Path) -> io::Result<Option<(Folder, OsString)>> {
        let mut parts = parts_of(rel)?;
        let Some(name) = parts.pop() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "install never removes the project root",
            ));
        };
        let mut hops = 0;
        match self.walk(parts, Make::No, Links::Allowed, &mut hops) {
            Ok((f, _)) => Ok(Some((f, name))),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Removes `name` in `dir`: a folder with everything in it, anything else
/// (a link included) as itself.
pub(super) fn remove_tree_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let fd = match rustix::fs::openat(dir, name, dir_flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => {
            return match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(s) if !FileType::from_raw_mode(s.st_mode).is_dir() => {
                    rustix::fs::unlinkat(dir, name, AtFlags::empty()).map_err(Into::into)
                }
                _ => Err(e.into()),
            };
        }
    };
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&fd)? {
        let n = OsStr::from_bytes(entry?.file_name().to_bytes()).to_os_string();
        if n != "." && n != ".." {
            names.push(n);
        }
    }
    for n in names {
        let folder = rustix::fs::statat(&fd, n.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|s| FileType::from_raw_mode(s.st_mode).is_dir());
        if folder {
            remove_tree_at(&fd, &n)?;
        } else {
            match rustix::fs::unlinkat(&fd, n.as_os_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    match rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A new private file `name` in `dir` (0600), never through a link.
pub fn create_file(dir: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    Ok(File::from(fd))
}

/// The file `name` in `dir`, for reading, never through a link. Opened
/// without blocking, so a FIFO put there can't hang install, and refused
/// unless it's a regular file (a folder is `IsADirectory`); then reads
/// block as usual (review of 08ff0f9).
pub fn open_file(dir: &OwnedFd, name: &OsStr) -> io::Result<File> {
    open_regular(dir, name, OFlags::RDONLY)
}

/// A regular file opened read/write for a held advisory update lock. It is
/// neither created nor truncated, and links and special files are refused.
pub fn open_file_for_update(dir: &OwnedFd, name: &OsStr) -> io::Result<File> {
    open_regular(dir, name, OFlags::RDWR)
}

fn open_regular(dir: &OwnedFd, name: &OsStr, access: OFlags) -> io::Result<File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let kind = FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode);
    if kind.is_dir() {
        return Err(io::ErrorKind::IsADirectory.into());
    }
    if !kind.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it isn't a regular file",
        ));
    }
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK)?;
    Ok(File::from(fd))
}

/// What an entry in a folder is, never through a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Dir,
    Link,
    /// Anything else.
    Other,
}

/// A file's device: `i32` on macOS, `u64` on Linux.
#[allow(clippy::unnecessary_cast)]
fn device(s: &rustix::fs::Stat) -> u64 {
    s.st_dev as u64
}

/// A file's bytes, permission bits and number of names (hard links).
pub(super) type Linked = (Vec<u8>, Option<u32>, u64);

/// A file by the file system's identity ([`Tree::identity`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum FileId {
    Existing(u64, u64),
    /// Not there yet: its folder's device and inode, and its name.
    New(u64, u64, OsString),
}

/// The entries of `folder`, sorted by name; a link is listed as a link,
/// never followed (review of 08ff0f9).
pub(super) fn entries(folder: &Folder) -> io::Result<Vec<(OsString, Kind)>> {
    let mut out = Vec::new();
    for entry in rustix::fs::Dir::read_from(&folder.fd)? {
        let n = OsStr::from_bytes(entry?.file_name().to_bytes()).to_os_string();
        if n == "." || n == ".." {
            continue;
        }
        let kind = match rustix::fs::statat(&folder.fd, n.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => {
                let t = FileType::from_raw_mode(s.st_mode);
                if t.is_file() {
                    Kind::File
                } else if t.is_dir() {
                    Kind::Dir
                } else if t.is_symlink() {
                    Kind::Link
                } else {
                    Kind::Other
                }
            }
            Err(_) => Kind::Other,
        };
        out.push((n, kind));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The folder `name` in `folder`, by handle, never through a link: a walk
/// down a tree opens each folder once, from its parent (review of
/// 13b3e6e, where every level walked again from the root).
pub(super) fn child(folder: &Folder, name: &OsStr) -> io::Result<Folder> {
    if is_git(name) {
        return Err(refused(&folder.rel.join(name), "is a .git folder"));
    }
    let fd = rustix::fs::openat(&folder.fd, name, dir_flags(), Mode::empty())?;
    Ok(Folder {
        fd,
        rel: folder.rel.join(name),
    })
}

pub fn rename(from: &OwnedFd, a: &OsStr, to: &OwnedFd, b: &OsStr) -> io::Result<()> {
    rustix::fs::renameat(from, a, to, b).map_err(Into::into)
}

/// Publishes a staged entry without replacing an existing entry, in one
/// operation (RENAME_NOREPLACE on Linux, RENAME_EXCL on macOS). No fallback
/// to a replacing rename or a hard link is permitted.
pub fn rename_noreplace(from: &OwnedFd, a: &OsStr, to: &OwnedFd, b: &OsStr) -> io::Result<()> {
    rustix::fs::renameat_with(from, a, to, b, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(Into::into)
}

pub fn unlink(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    rustix::fs::unlinkat(dir, name, AtFlags::empty()).map_err(Into::into)
}
