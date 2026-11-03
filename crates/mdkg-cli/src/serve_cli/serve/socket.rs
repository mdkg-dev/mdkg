//! The graph's local socket (API v1 A15; dec-01a18dd9):
//! `<common>/mdkg/local.sock`, its own name, so a later signed socket
//! (`api.sock`, the networked design) can't be mistaken for it. Mode 0600,
//! in its folder of mode 0700, which the `serve` lock has already made
//! private. When that path is longer than the platform allows (104 bytes
//! on macOS, 108 on Linux, the terminating NUL included), the socket goes
//! in `${XDG_RUNTIME_DIR:-$TMPDIR}/mdkg-<uid>/<16 hex of sha256(path)>.sock`,
//! in a 0700 folder of this user's. `serve.json` names whichever is used;
//! clients never build the path themselves.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use mdkg_format::Digest;

/// The longest socket path, in bytes, without its NUL.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const MAX_PATH: usize = 103;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
pub const MAX_PATH: usize = 107;

pub const SOCKET: &str = "local.sock";

/// The folder short sockets go in: `$XDG_RUNTIME_DIR` when set to an
/// absolute path, else `$TMPDIR`, else the system's temp folder.
pub fn runtime_dir(xdg_runtime: Option<&Path>, tmpdir: Option<&Path>) -> PathBuf {
    xdg_runtime
        .filter(|p| p.is_absolute())
        .or(tmpdir.filter(|p| p.is_absolute()))
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

/// Where the socket for `mdkg_dir` (`<common>/mdkg`) goes.
pub fn path_for(mdkg_dir: &Path, runtime: &Path, uid: u32) -> PathBuf {
    let long = mdkg_dir.join(SOCKET);
    if long.as_os_str().len() <= MAX_PATH {
        return long;
    }
    let digest = Digest::of_bytes(long.as_os_str().as_bytes());
    let name = &digest.hex()[..16];
    runtime
        .join(format!("mdkg-{uid}"))
        .join(format!("{name}.sock"))
}

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// Makes the fallback folder ready: created 0700 when missing; an existing
/// one must be a real folder of this user, and loses any group or other
/// bits it has.
fn private_folder(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(refused(format!("{} isn't a folder", dir.display())));
    }
    if meta.uid() != uid {
        return Err(refused(format!(
            "{} belongs to another user",
            dir.display()
        )));
    }
    let mode = meta.mode() & 0o7777;
    if mode != mode & 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

/// Binds the socket at `path`, mode 0600. A socket file a crashed `serve`
/// left there is removed first; anything else at the path is refused. The
/// caller holds the `serve` lock, so no live `serve` owns the old file.
/// `fallback`: `path` is the short fallback, whose folder this makes
/// private; `<common>/mdkg` the `serve` lock has already made private.
pub fn bind(path: &Path, uid: u32, fallback: bool) -> io::Result<UnixListener> {
    let dir = path
        .parent()
        .ok_or_else(|| refused("the socket path has no folder".into()))?;
    if path.as_os_str().len() > MAX_PATH {
        return Err(refused(format!(
            "the socket path {} is longer than {MAX_PATH} bytes",
            path.display()
        )));
    }
    if fallback {
        private_folder(dir, uid)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid => fs::remove_file(path)?,
        Ok(_) => {
            return Err(refused(format!(
                "{} exists and isn't a socket of this user's",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    // The folder is private, so nobody else can reach the socket between
    // the bind and this.
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        // Short: macOS's own temp folder is already 50 bytes long.
        let dir = PathBuf::from("/tmp").join(format!(
            "mdkg-sock-{tag}-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        ));
        fs::create_dir_all(&dir).expect("temp");
        dir
    }

    fn uid() -> u32 {
        mdkg_serve::state::own_uid()
    }

    #[test]
    fn a_short_path_is_used_as_it_is() {
        let p = path_for(Path::new("/r/.git/mdkg"), Path::new("/run/u"), 501);
        assert_eq!(p, PathBuf::from("/r/.git/mdkg/local.sock"));
    }

    #[test]
    fn a_long_path_falls_back_to_a_short_one_in_a_private_folder() {
        let long = PathBuf::from("/").join("x".repeat(120)).join(".git/mdkg");
        let p = path_for(&long, Path::new("/run/user/501"), 501);
        let name = p.file_name().expect("name").to_string_lossy().into_owned();
        assert_eq!(p.parent(), Some(Path::new("/run/user/501/mdkg-501")));
        assert_eq!(name.len(), 16 + ".sock".len());
        assert!(name.ends_with(".sock"));
        assert!(p.as_os_str().len() <= MAX_PATH);
        // The same long path always gives the same short one.
        assert_eq!(p, path_for(&long, Path::new("/run/user/501"), 501));
        assert_ne!(
            p,
            path_for(&long.join("other"), Path::new("/run/user/501"), 501)
        );
    }

    #[test]
    fn the_runtime_folder_prefers_xdg_then_tmpdir() {
        let (x, t) = (Path::new("/run/u"), Path::new("/tmp/t"));
        assert_eq!(runtime_dir(Some(x), Some(t)), x);
        assert_eq!(runtime_dir(None, Some(t)), t);
        assert_eq!(runtime_dir(Some(Path::new("rel")), Some(t)), t);
        assert_eq!(runtime_dir(None, None), std::env::temp_dir());
    }

    #[test]
    fn the_socket_is_0600_and_a_stale_one_is_replaced() {
        let dir = temp("bind");
        let p = dir.join(SOCKET);
        let l = bind(&p, uid(), false).expect("bind");
        let mode = fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(l);
        // A crashed serve's socket is still there: bound again over it.
        assert!(p.exists());
        let _l = bind(&p, uid(), false).expect("bind again");
        // Anything else at the path is refused, never removed.
        fs::remove_file(&p).expect("rm");
        fs::write(&p, "x").expect("plain file");
        assert!(bind(&p, uid(), false).is_err());
        assert!(p.exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_fallback_folder_is_made_private() {
        let dir = temp("fb");
        let folder = dir.join(format!("mdkg-{}", uid()));
        fs::create_dir(&folder).expect("folder");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("chmod");
        let _l = bind(&folder.join("ab.sock"), uid(), true).expect("bind");
        let mode = fs::metadata(&folder).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
