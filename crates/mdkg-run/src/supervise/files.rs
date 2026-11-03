//! Private folders under the run folder, and files replaced whole, for the
//! supervisor's records ([`super::registry`]) and stop requests
//! ([`super::stop`]). The same checks as the progress folder's: every
//! folder from Git's common directory down is a directory this user owns
//! that no one else can write.

use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};

use crate::RunError;

pub(crate) fn flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

/// `<run dir>/<name>`, checked, and created when `create` is set and the
/// store's `mdkg` folder exists. `None` when it doesn't exist and isn't
/// created.
pub(crate) fn folder(
    run_dir: &Path,
    name: &str,
    create: bool,
) -> Result<Option<PathBuf>, RunError> {
    let state = run_dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "mdkg"));
    let common = state.and_then(Path::parent).filter(|_| {
        run_dir.is_absolute()
            && run_dir.file_name().is_some_and(|n| n == "run")
            && !run_dir
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    });
    let common = common.ok_or_else(|| {
        RunError::failure(
            "io.read_failed",
            "supervisor records require the Git common directory's mdkg/run folder",
        )
    })?;
    let state = common.join("mdkg");
    let path = run_dir.join(name);
    let uid = rustix::process::geteuid().as_raw();
    for (i, p) in [common, state.as_path(), run_dir, path.as_path()]
        .into_iter()
        .enumerate()
    {
        let metadata = match fs::symlink_metadata(p) {
            Ok(m) => m,
            // Never creates the store's own folder.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && i > 1 => {
                if !create {
                    return Ok(None);
                }
                match fs::DirBuilder::new().mode(0o700).create(p) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(RunError::io("can't create a supervisor folder", e)),
                }
                fs::symlink_metadata(p)
                    .map_err(|e| RunError::io("can't inspect a supervisor folder", e))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(RunError::io("can't inspect a supervisor folder", e)),
        };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(RunError::failure(
                "io.read_failed",
                format!(
                    "{} isn't a private folder this user owns; supervisor records can't be trusted",
                    p.display()
                ),
            ));
        }
    }
    Ok(Some(path))
}

/// Replaces `path` with `bytes`: a 0600 temp file beside it, synced, then
/// renamed over it, and the folder synced.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let write = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(flags())
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if write.is_err() {
        fs::remove_file(&tmp).ok();
    }
    write
}

/// Milliseconds since the epoch as `YYYY-MM-DDTHH:MM:SS.sssZ`.
pub fn timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// A scratch Git common directory with its `mdkg/run` folder, removed on drop.
#[cfg(test)]
pub(crate) struct Scratch(PathBuf);

#[cfg(test)]
impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mdkg-{name}-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(path.join("common/mdkg/run")).expect("scratch");
        Self(path)
    }

    pub(crate) fn run_dir(&self) -> PathBuf {
        self.0.join("common/mdkg/run")
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_793_361_928_894), "2026-10-30T12:05:28.894Z");
        assert_eq!(timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_folder_is_created_private_and_a_shared_one_is_refused() {
        let scratch = Scratch::new("supervise-files");
        let run = scratch.run_dir();
        assert_eq!(folder(&run, "stop", false).unwrap(), None);
        let made = folder(&run, "stop", true).unwrap().unwrap();
        assert_eq!(fs::metadata(&made).unwrap().mode() & 0o777, 0o700);
        replace(&made.join("a.json"), b"{}").unwrap();
        assert_eq!(fs::read(made.join("a.json")).unwrap(), b"{}");
        fs::set_permissions(&made, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(folder(&run, "stop", false).is_err());
        assert!(folder(Path::new("relative/mdkg/run"), "stop", false).is_err());
    }
}
