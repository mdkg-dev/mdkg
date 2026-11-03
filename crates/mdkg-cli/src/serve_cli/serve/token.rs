//! The loopback listener's bearer token (dec-01a18dd9): a 0600 file of
//! this user's, in a 0700 folder under the user's state folder, never in
//! the repository or its `.git`:
//! `<state>/mdkg/serve/tokens/<16 hex of sha256(<common>)>.token`.
//!
//! A turn's sandbox lists the worktree and the repository's `.git`, not
//! the user's state folder (A19), so a turn can't read the file **when a
//! sandbox restricts its reads**, which only `run.wrapper` provides, off
//! by default. An unsandboxed turn runs as the user and can read it, then
//! call the loopback listener, where no pid is checked, only that the
//! actor it names has no running turn: a stated residual
//! (the serve edd's 0.7.1 amendment), since such a turn can already open
//! the store and run the CLI directly. `serve` warns at a loopback start
//! when no `run.wrapper` is configured.
//!
//! The token is minted on the first loopback start, 32 random bytes as
//! hex, and kept until the file is removed. It never appears in a log line, a
//! diagnostic, an envelope or `serve.json`; only the file's path does.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mdkg_format::Digest;

use super::local::Token;

/// The token file for the repository whose git common dir is `common`.
pub fn path_for(state: &Path, common: &Path) -> PathBuf {
    let digest = Digest::of_bytes(common.as_os_str().as_bytes());
    state
        .join("mdkg")
        .join("serve")
        .join("tokens")
        .join(format!("{}.token", &digest.hex()[..16]))
}

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// Each folder from `<state>/mdkg` down to the tokens folder: created 0700
/// when missing, and the tokens folder must be a real folder of this
/// user's with no group or other bits.
fn private_folder(dir: &Path, uid: u32) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(refused(format!(
            "{} must be a folder of this user's",
            dir.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(meta.mode() & 0o700))?;
    }
    Ok(())
}

/// The token at `path`, minted when the file is missing. An existing file
/// must be a regular file of this user's with no group or other bits, and
/// hold at least 32 characters; it's refused otherwise, never repaired.
pub fn load_or_mint(path: &Path, uid: u32) -> io::Result<Token> {
    let dir = path
        .parent()
        .ok_or_else(|| refused("the token path has no folder".into()))?;
    private_folder(dir, uid)?;
    sweep(path, dir, uid)?;
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => match mint(path, dir)? {
            Some(token) => Ok(token),
            // Another start minted it first: theirs.
            None => load(path, &fs::symlink_metadata(path)?, uid),
        },
        Err(e) => Err(e),
        Ok(meta) => load(path, &meta, uid),
    }
}

/// The temp files a mint writes beside `path`, before the link.
fn temp_prefix(path: &Path) -> std::ffi::OsString {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp-");
    name
}

/// Removes the temp files a cut-off mint left beside `path`: regular
/// files of this user's with its temp prefix. Run before every load as
/// well as every mint, so a crash between the link and the temp file's
/// removal leaves no second name for the token, which would outlive a
/// removal of the token file (#12300). One that can't be removed is an
/// error, never skipped (orchestrator, on 39ce7ec7).
fn sweep(path: &Path, dir: &Path, uid: u32) -> io::Result<()> {
    let prefix = temp_prefix(path);
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        if name.as_bytes().starts_with(prefix.as_bytes())
            && entry
                .metadata()
                .is_ok_and(|m| m.file_type().is_file() && m.uid() == uid)
        {
            // A temp name that can't go fails the start, closed: left
            // behind, it would outlive a removal of the token file.
            match fs::remove_file(entry.path()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "{} is a leftover of the token and can't be removed: {e}",
                            entry.path().display()
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Mints the token at `path`, whole or not at all (#12110): written and
/// synced in a private temp file, then linked into place, which never
/// replaces a file, and the folder synced. A crash leaves a temp file,
/// removed at the next start ([`sweep`]), never a short token file. `None` when
/// another start's file got there first.
fn mint(path: &Path, dir: &Path) -> io::Result<Option<Token>> {
    let prefix = temp_prefix(path);
    let secret = mdkg_serve::random::hex(&mdkg_serve::random::bytes::<32>()?);
    let mut temp_name = prefix;
    temp_name.push(mdkg_serve::random::hex(&mdkg_serve::random::bytes::<8>()?));
    let temp = dir.join(temp_name);
    let written = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(secret.as_bytes())?;
        f.sync_all()?;
        fs::hard_link(&temp, path)
    })();
    let _ = fs::remove_file(&temp);
    match written {
        Ok(()) => {
            fs::File::open(dir)?.sync_all()?;
            Ok(Some(Token::new(secret)))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e),
    }
}

/// The token in the existing file at `path`, whose metadata is `meta`.
fn load(path: &Path, meta: &fs::Metadata, uid: u32) -> io::Result<Token> {
    if !meta.file_type().is_file() || meta.uid() != uid {
        return Err(refused(format!(
            "{} must be a regular file of this user's",
            path.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refused(format!(
            "{} is readable by others; run `chmod 600` on it, or remove it for a new token",
            path.display()
        )));
    }
    let mut text = String::new();
    fs::File::open(path)?.take(4096).read_to_string(&mut text)?;
    let secret = text.trim().to_owned();
    if secret.len() < 32 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(refused(format!(
            "{} doesn't hold a token; remove it for a new one",
            path.display()
        )));
    }
    Ok(Token::new(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mdkg-token-{}", uuid::Uuid::now_v7().simple()));
        fs::create_dir_all(&dir).expect("temp");
        dir
    }

    #[test]
    fn a_token_is_minted_once_0600_and_kept() {
        let state = temp();
        let uid = mdkg_serve::state::own_uid();
        let path = path_for(&state, Path::new("/r/.git"));
        assert!(path.starts_with(state.join("mdkg/serve/tokens")));
        let a = load_or_mint(&path, uid).expect("mint");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("dir")), 0o700);
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(text.len(), 64);
        let b = load_or_mint(&path, uid).expect("load");
        assert_eq!(format!("{a:?}"), "Token(..)", "Debug never shows it");
        drop((a, b));
        // Readable by others: refused, never repaired.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let e = load_or_mint(&path, uid).expect_err("open to others");
        assert!(
            !e.to_string().contains(text.trim()),
            "never names the token"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        fs::write(&path, "short").expect("write");
        assert!(load_or_mint(&path, uid).is_err());
        // Another repository gets another file.
        assert_ne!(path, path_for(&state, Path::new("/other/.git")));
        fs::remove_dir_all(&state).expect("cleanup");
    }

    #[test]
    fn a_mint_cut_off_leaves_no_short_token_and_the_next_one_cleans_up() {
        let state = temp();
        let uid = mdkg_serve::state::own_uid();
        let path = path_for(&state, Path::new("/r/.git"));
        let dir = path.parent().expect("dir").to_path_buf();
        fs::create_dir_all(&dir).expect("dir");
        // What a crash between the temp file's create and its write leaves.
        let mut name = path.file_name().expect("name").to_os_string();
        name.push(".tmp-0123456789abcdef");
        fs::write(dir.join(&name), "").expect("leftover");
        assert!(!path.exists(), "the token file itself is never partial");
        load_or_mint(&path, uid).expect("mint");
        assert_eq!(fs::read_to_string(&path).expect("token").len(), 64);
        let left: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, [path.file_name().expect("name").to_os_string()]);
        // A crash after the link, before the temp name went: the next
        // start's load removes the second name.
        let mut crashed = path.file_name().expect("name").to_os_string();
        crashed.push(".tmp-fedcba9876543210");
        fs::hard_link(&path, dir.join(&crashed)).expect("second name");
        load_or_mint(&path, uid).expect("load");
        assert!(!dir.join(&crashed).exists());
        // One that can't be removed (a folder made read-only) fails the
        // load, closed, rather than leaving the second name behind. Root
        // ignores the folder's mode, so the case runs only for others.
        if uid != 0 {
            fs::hard_link(&path, dir.join(&crashed)).expect("second name");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("read-only");
            let refused = load_or_mint(&path, uid);
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("writable");
            let e = refused.expect_err("a leftover that can't go");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            assert!(dir.join(&crashed).exists());
            load_or_mint(&path, uid).expect("load once it can go");
            assert!(!dir.join(&crashed).exists());
        }
        // A mint that finds the file already there keeps that one.
        let before = fs::read_to_string(&path).expect("token");
        assert_eq!(mint(&path, &dir).expect("raced").map(|_| ()), None);
        assert_eq!(fs::read_to_string(&path).expect("token"), before);
        fs::remove_dir_all(&state).expect("cleanup");
    }
}
