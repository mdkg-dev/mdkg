//! Local process build receipts and executable replacement detection.
//!
//! A record is live only while its publisher holds its file lock. No PID
//! probe is needed, and a record left by a killed process is ignored.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::RunError;

const SCHEMA: &str = "mdkg.process-build.v1";
const CAP: u64 = 65536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub version: String,
    pub commit: String,
    pub dirty: Option<bool>,
    pub built: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Launcher,
    Mcp,
}

/// An executable's identity and change times. Following a symlink here is
/// deliberate: replacing either the link's target or the file is an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

impl Fingerprint {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let m = fs::metadata(path)?;
        if !m.is_file() || m.mode() & 0o111 == 0 {
            return Err(std::io::Error::other(
                "the executable isn't an executable regular file",
            ));
        }
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        })
    }
}

/// Observe both the actual binary and its invocation path. In an npm install
/// those are the payload in .bin_real and the JavaScript launcher; a change
/// to either requires draining the old process before re-exec.
#[derive(Debug)]
pub struct BinaryWatch {
    paths: Vec<(PathBuf, Fingerprint)>,
}

impl BinaryWatch {
    pub fn new(executable: &Path, invocation: &Path) -> std::io::Result<Self> {
        let mut paths = vec![(executable.to_path_buf(), Fingerprint::read(executable)?)];
        if invocation != executable {
            paths.push((invocation.to_path_buf(), Fingerprint::read(invocation)?));
        }
        Ok(Self { paths })
    }

    /// A missing/replaced/non-executable path counts as changed. The caller
    /// drains first, then handles an unavailable replacement as an exec error.
    pub fn changed(&self) -> bool {
        self.paths
            .iter()
            .any(|(p, before)| !Fingerprint::read(p).is_ok_and(|now| now == *before))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub schema: String,
    pub pid: u32,
    pub kind: Kind,
    pub executable: PathBuf,
    pub fingerprint: Fingerprint,
    pub build: Build,
    pub agents: Vec<String>,
}

/// Keeps the build record live until Drop, which removes only its own file.
#[derive(Debug)]
pub struct Held {
    file: File,
    path: PathBuf,
}

impl Drop for Held {
    fn drop(&mut self) {
        // Closing only this descriptor leaves a fork/dup copy holding the lock.
        // End the receipt's lifetime even when unsafe cleanup must be skipped.
        let _ = self.file.unlock();
        let Some(run) = self.path.parent().and_then(Path::parent) else {
            return;
        };
        if !matches!(folder(run, false), Ok(Some(_))) {
            return;
        }
        let same = self
            .file
            .metadata()
            .ok()
            .zip(fs::symlink_metadata(&self.path).ok())
            .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino());
        if same {
            fs::remove_file(&self.path).ok();
        }
    }
}

fn directory(path: &Path, metadata: &fs::Metadata, uid: u32) -> Result<(), RunError> {
    let problem = if !metadata.is_dir() {
        "isn't a plain build record folder"
    } else if metadata.uid() != uid {
        "is owned by another user; process builds cannot be trusted"
    } else if metadata.mode() & 0o022 != 0 {
        "is writable by group or other users; process builds cannot be trusted"
    } else {
        return Ok(());
    };
    Err(RunError::failure(
        "io.read_failed",
        format!("{} {problem}", path.display()),
    ))
}

fn folder(run_dir: &Path, create: bool) -> Result<Option<PathBuf>, RunError> {
    // The trusted anchor is the Git common directory. Validate every state
    // directory below it: a writable parent could replace a private child.
    let state = run_dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "mdkg"));
    let common = state
        .and_then(Path::parent)
        .filter(|_| {
            run_dir.is_absolute()
                && run_dir.file_name().is_some_and(|n| n == "run")
                && !run_dir
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        })
        .ok_or_else(|| {
            RunError::failure(
                "io.read_failed",
                "build records require the Git common directory's mdkg/run folder",
            )
        })?;
    let state = common.join("mdkg");
    let path = run_dir.join("builds");
    let uid = rustix::process::geteuid().as_raw();
    for (i, p) in [common, state.as_path(), run_dir, path.as_path()]
        .into_iter()
        .enumerate()
    {
        let metadata = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && i > 0 => {
                // Diagnostics never initialize the store directory. Only
                // registry children of an existing store may be created.
                if !create || i == 1 {
                    return Ok(None);
                }
                match fs::DirBuilder::new().mode(0o700).create(p) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(RunError::io("can't create build records", e)),
                };
                fs::symlink_metadata(p)
                    .map_err(|e| RunError::io("can't inspect new build folder", e))?
            }
            Err(e) => return Err(RunError::io("can't inspect build folder", e)),
        };
        directory(p, &metadata, uid)?;
    }
    Ok(Some(path))
}

/// Publish a locked receipt for this process, outside committed agent state.
/// `run_dir` is the path returned by [`crate::state::run_dir`].
/// A missing store directory returns `None` without creating it.
pub fn publish(
    run_dir: &Path,
    kind: Kind,
    executable: &Path,
    build: Build,
    agents: Vec<String>,
) -> Result<Option<Held>, RunError> {
    let Some(dir) = folder(run_dir, true)? else {
        return Ok(None);
    };
    let process = Process {
        schema: SCHEMA.into(),
        pid: std::process::id(),
        kind,
        executable: executable.to_path_buf(),
        fingerprint: Fingerprint::read(executable)
            .map_err(|e| RunError::io("can't inspect running executable", e))?,
        build,
        agents,
    };
    let bytes = serde_json::to_vec(&process)
        .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
    if bytes.len() as u64 > CAP {
        return Err(RunError::failure(
            "io.write_failed",
            "build record is over 64 KiB",
        ));
    }
    let name = format!("{}-{}", std::process::id(), uuid::Uuid::now_v7());
    let temporary = dir.join(format!("{name}.tmp"));
    let path = dir.join(format!("{name}.json"));
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(&temporary)
        .map_err(|e| RunError::io("can't create build record", e))?;
    let mut write = || -> std::io::Result<()> {
        file.lock()?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
    };
    if let Err(e) = (write)() {
        fs::remove_file(temporary).ok();
        return Err(RunError::io("can't publish build record", e));
    }
    Ok(Some(Held { file, path }))
}

#[derive(Debug, Default)]
pub struct Live {
    pub processes: Vec<Process>,
    pub problems: Vec<String>,
}

fn read_live(path: &Path) -> Result<Option<Process>, RunError> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|e| RunError::io("can't open process build record", e))?;
    let m = file
        .metadata()
        .map_err(|e| RunError::io("can't inspect process build record", e))?;
    if !m.is_file()
        || m.nlink() != 1
        || m.len() > CAP
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(RunError::failure(
            "io.read_failed",
            "process build record isn't an owned, protected regular file of at most 64 KiB",
        ));
    }
    match file.try_lock_shared() {
        Ok(()) => return Ok(None),
        Err(fs::TryLockError::WouldBlock) => (),
        Err(fs::TryLockError::Error(e)) => {
            return Err(RunError::io("can't check process build lock", e));
        }
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io("can't read process build record", e))?;
    if bytes.len() as u64 > CAP {
        return Err(RunError::failure(
            "io.read_failed",
            "process build record is over 64 KiB",
        ));
    }
    let process: Process = serde_json::from_slice(&bytes)
        .map_err(|_| RunError::failure("io.read_failed", "invalid process build record"))?;
    if process.schema != SCHEMA {
        return Err(RunError::failure(
            "io.read_failed",
            "unknown process build record schema",
        ));
    }
    Ok(Some(process))
}

/// Read-only live inventory. Broken entries remain visible as problems;
/// they cannot suppress other valid process receipts. Dead records are kept
/// on disk and ignored, so doctor itself never cleans or mutates state.
pub fn live(run_dir: &Path) -> Result<Live, RunError> {
    let Some(dir) = folder(run_dir, false)? else {
        return Ok(Live::default());
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Live::default()),
        Err(e) => return Err(RunError::io("can't list process build records", e)),
    };
    let mut result = Live::default();
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.path().extension().is_some_and(|e| e == "json") => {
                paths.push(entry.path())
            }
            Ok(_) => (),
            Err(e) => result
                .problems
                .push(format!("can't list a process build record: {e}")),
        }
    }
    paths.sort();
    for path in paths {
        match read_live(&path) {
            Ok(Some(process)) => result.processes.push(process),
            Ok(None) => (),
            Err(e) => result
                .problems
                .push(format!("{}: {}", path.display(), e.message)),
        }
    }
    Ok(result)
}

impl Live {
    pub fn warnings(&self, current: &Build) -> Vec<String> {
        let mut warnings = self.problems.clone();
        for process in &self.processes {
            if process.build != *current {
                warnings.push(format!("mixed mdkg builds: {:?} pid {} runs {} at {} (built {}), this process is {} at {} (built {})", process.kind, process.pid, process.build.version, process.build.commit, process.build.built, current.version, current.commit, current.built));
            }
            if !Fingerprint::read(&process.executable).is_ok_and(|now| now == process.fingerprint) {
                warnings.push(format!(
                    "{:?} pid {} has an executable changed on disk: {}",
                    process.kind,
                    process.pid,
                    process.executable.display()
                ));
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }
    fn scratch() -> Scratch {
        let d = std::env::temp_dir().join(format!("mdkg-builds-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&d).expect("scratch");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(d.join("common"))
            .expect("common");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(d.join("common/mdkg"))
            .expect("existing store folder");
        Scratch(d)
    }
    impl Scratch {
        fn run(&self) -> PathBuf {
            self.0.join("common/mdkg/run")
        }
    }
    fn build(commit: &str) -> Build {
        Build {
            version: "0.7.0".into(),
            commit: commit.into(),
            dirty: Some(false),
            built: "sample-build".into(),
        }
    }
    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").expect("binary");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mode");
    }

    #[test]
    fn live_build_receipts_follow_locks_and_ignore_dead_records() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let run = d.run();
        let held = publish(
            &run,
            Kind::Launcher,
            &bin,
            build("first"),
            vec!["worker".into()],
        )
        .expect("publish")
        .expect("existing store");
        let records = live(&run).expect("live");
        assert_eq!(records.processes.len(), 1);
        assert!(records.warnings(&build("first")).is_empty());
        assert!(records.warnings(&build("second"))[0].contains("mixed mdkg builds"));
        let dead = run.join("builds/dead.json");
        fs::copy(&held.path, &dead).expect("stale record");
        assert_eq!(live(&run).expect("live").processes.len(), 1);
        drop(held);
        assert!(live(&run).expect("live").processes.is_empty());
        assert!(dead.is_file(), "read-only inventory keeps stale records");
    }

    #[test]
    fn cleanup_leaves_a_replacement_file_alone() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let held = publish(&d.run(), Kind::Mcp, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        let path = held.path.clone();
        fs::rename(&path, d.0.join("moved")).expect("move");
        fs::write(&path, "replacement").expect("file");
        drop(held);
        assert_eq!(
            fs::read_to_string(path).expect("replacement"),
            "replacement"
        );
    }

    #[test]
    fn malformed_and_linked_records_do_not_hide_valid_live_builds() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let held = publish(&d.run(), Kind::Mcp, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        let bad = d.run().join("builds/bad.json");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&bad)
            .expect("bad");
        file.lock().expect("lock");
        file.write_all(b"{").expect("write");
        symlink(&bin, d.run().join("builds/link.json")).expect("link");
        let result = live(&d.run()).expect("live");
        assert_eq!(result.processes.len(), 1);
        assert_eq!(result.problems.len(), 2);
        drop(held);
    }

    #[test]
    fn observing_absent_records_does_not_create_any_folder() {
        let d = scratch();
        let run = d.run();
        fs::remove_dir(run.parent().expect("store folder")).expect("remove empty store folder");
        assert!(live(&run).expect("absent").processes.is_empty());
        assert!(!run.exists());
        assert!(!run.parent().expect("store folder").exists());
        fs::create_dir_all(run.parent().expect("state parent")).expect("state parent");
        symlink(&d.0, &run).expect("link");
        assert!(live(&run).is_err());
    }

    #[test]
    fn publishing_without_a_store_folder_creates_nothing() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let run = d.run();
        let store = run.parent().expect("store folder");
        fs::remove_dir(store).expect("remove empty store folder");
        for kind in [Kind::Mcp, Kind::Launcher] {
            assert!(
                publish(&run, kind, &bin, build("first"), vec![])
                    .expect("optional receipt")
                    .is_none()
            );
            assert!(!store.exists(), "a receipt must not initialize the store");
        }
    }

    #[test]
    fn binary_watch_detects_replacement_retargeting_and_missing_files() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let link = d.0.join("mdkg-next");
        symlink(&bin, &link).expect("link");
        let watch = BinaryWatch::new(&bin, &link).expect("watch");
        assert!(!watch.changed());
        let next = d.0.join("next");
        executable(&next);
        fs::rename(next, &bin).expect("replace");
        assert!(watch.changed());
        let watch = BinaryWatch::new(&bin, &link).expect("watch");
        let other = d.0.join("other");
        executable(&other);
        fs::remove_file(&link).expect("remove");
        symlink(&other, &link).expect("retarget");
        assert!(watch.changed());
        let watch = BinaryWatch::new(&other, &link).expect("watch");
        fs::remove_file(other).expect("remove");
        assert!(watch.changed());
    }

    #[test]
    fn npm_payload_and_wrapper_changes_are_both_observed() {
        let d = scratch();
        let bin = d.0.join("payload");
        let wrapper = d.0.join("launcher");
        executable(&bin);
        executable(&wrapper);
        let watch = BinaryWatch::new(&bin, &wrapper).expect("watch");
        fs::write(&wrapper, "#!/bin/sh\nexit 1\n").expect("replace wrapper");
        assert!(watch.changed());
        let watch = BinaryWatch::new(&bin, &wrapper).expect("watch");
        fs::write(&bin, "#!/bin/sh\nexit 2\n").expect("replace payload");
        assert!(watch.changed());
    }

    #[test]
    fn a_running_build_warns_when_its_binary_is_replaced_on_disk() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let _held = publish(&d.run(), Kind::Launcher, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        let replacement = d.0.join("replacement");
        executable(&replacement);
        fs::rename(replacement, &bin).expect("replace");
        let warnings = live(&d.run()).expect("live").warnings(&build("first"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("executable changed on disk"));
    }

    #[test]
    fn hardlinked_and_oversized_records_are_reported_without_reading_their_content() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let _held = publish(&d.run(), Kind::Launcher, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        let unrelated = d.0.join("unrelated");
        fs::write(&unrelated, "keep").expect("file");
        fs::hard_link(&unrelated, d.run().join("builds/hardlink.json")).expect("link");
        fs::write(d.run().join("builds/large.json"), vec![0; CAP as usize + 1]).expect("large");
        let result = live(&d.run()).expect("live");
        assert_eq!(result.processes.len(), 1);
        assert_eq!(result.problems.len(), 2);
        assert_eq!(fs::read_to_string(unrelated).expect("unrelated"), "keep");
    }

    #[test]
    fn foreign_directory_ownership_is_refused() {
        let d = scratch();
        let common = d.0.join("common");
        let metadata = fs::symlink_metadata(&common).expect("metadata");
        let error = directory(&common, &metadata, metadata.uid().wrapping_add(1))
            .expect_err("foreign directory");
        assert!(error.message.contains("owned by another user"));
        // Exercise the public reader against a real foreign trust anchor
        // when the test itself isn't running as root. No write is attempted.
        if rustix::process::geteuid().as_raw() != 0 {
            assert!(
                live(Path::new("/mdkg/run"))
                    .expect_err("root-owned anchor")
                    .message
                    .contains("owned by another user")
            );
        }
    }

    #[test]
    fn every_registry_directory_rejects_symlinks() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let run = d.run();
        let _held = publish(&run, Kind::Mcp, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        for path in [
            d.0.join("common"),
            d.0.join("common/mdkg"),
            run.clone(),
            run.join("builds"),
        ] {
            let moved = d.0.join(format!("moved-{}", uuid::Uuid::now_v7()));
            fs::rename(&path, &moved).expect("move");
            symlink(&moved, &path).expect("symlink");
            assert!(live(&run).is_err(), "{}", path.display());
            assert!(publish(&run, Kind::Mcp, &bin, build("second"), vec![]).is_err());
            fs::remove_file(&path).expect("remove link");
            fs::rename(moved, path).expect("restore");
        }
    }

    #[test]
    fn writable_receipts_are_not_trusted_and_unsafe_cleanup_is_skipped() {
        let d = scratch();
        let bin = d.0.join("mdkg");
        executable(&bin);
        let run = d.run();
        let held = publish(&run, Kind::Mcp, &bin, build("first"), vec![])
            .expect("publish")
            .expect("existing store");
        let path = held.path.clone();
        // Model a child retaining the same open file description before exec.
        let inherited = held
            .file
            .try_clone()
            .expect("duplicate publisher descriptor");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("mode");
        let inventory = live(&run).expect("inventory");
        assert!(inventory.processes.is_empty());
        assert_eq!(inventory.problems.len(), 1);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        fs::set_permissions(run.join("builds"), fs::Permissions::from_mode(0o777)).expect("mode");
        drop(held);
        assert!(path.exists(), "drop must not touch an unsafe registry");
        fs::set_permissions(run.join("builds"), fs::Permissions::from_mode(0o700)).expect("mode");
        assert!(live(&run).expect("stale inventory").processes.is_empty());
        assert!(path.exists(), "read-only inventory keeps stale records");
        drop(inherited);
    }
}
