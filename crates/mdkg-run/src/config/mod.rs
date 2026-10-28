//! Scoped configuration edits, independent of CLI dispatch.
//!
//! Preparation is read-only. Apply requires the caller's effective-config
//! validation before any write. Project leases and graph-repair gates belong
//! to that caller. Directory-relative operations pin the checked folders.
//! Cooperating setters transfer an advisory lock to the staged replacement;
//! editors that ignore file locks are outside this serialization boundary.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mdkg_format::settings::{Input, Layer, MAX_BYTES, Origin, Paths, edit};
use serde::Serialize;
use serde_json::Value;

use crate::error::{ErrorKind, RunError};
use crate::install::at::{self, Folder, Links, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    User,
    Local,
}

impl Scope {
    pub fn layer(self) -> Layer {
        match self {
            Self::Project => Layer::Project,
            Self::User => Layer::User,
            Self::Local => Layer::Local,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Receipt {
    pub path: PathBuf,
    pub scope: Scope,
    pub changed: bool,
}

/// A post-publication sync failure must not be reported as an untouched file.
#[derive(Debug)]
pub struct Failure {
    pub error: RunError,
    pub published: bool,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Failure {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    bytes: Vec<u8>,
    id: (u64, u64),
    mode: u32,
}

#[derive(Debug)]
pub struct Prepared {
    path: PathBuf,
    scope: Scope,
    anchor: PathBuf,
    tree: Tree,
    root_id: (u64, u64),
    parent: PathBuf,
    parent_id: Option<(u64, u64)>,
    name: OsString,
    before: Option<Snapshot>,
    candidate: Input,
    bytes: Vec<u8>,
    changed: bool,
}

fn invalid(message: &str) -> RunError {
    RunError::usage("config.invalid", message)
}
fn raced() -> RunError {
    RunError::new(
        ErrorKind::Conflict,
        "coord.cas_failed",
        "configuration or its folder changed; prepare the edit again",
    )
}
fn io_error(write: bool, error: impl std::fmt::Display) -> RunError {
    RunError::failure(
        if write {
            "io.write_failed"
        } else {
            "io.read_failed"
        },
        format!("configuration operation failed: {error}"),
    )
}
fn input_error(error: mdkg_format::settings::Error) -> RunError {
    let mut out = invalid(&error.to_string());
    out.path = error.origin.path.map(|p| p.to_string_lossy().into_owned());
    out
}
fn id(folder: &Folder) -> io::Result<(u64, u64)> {
    let stat = rustix::fs::fstat(&folder.fd)?;
    #[allow(clippy::unnecessary_cast)]
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

fn snapshot(file: &File) -> Result<Snapshot, RunError> {
    let meta = file.metadata().map_err(|e| io_error(false, e))?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(invalid(
            "configuration must be a regular file no larger than 1 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(false, e))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("configuration exceeds the 1 MiB limit"));
    }
    Ok(Snapshot {
        bytes,
        id: (meta.dev(), meta.ino()),
        mode: meta.mode() & 0o777,
    })
}

fn read(folder: &Folder, name: &std::ffi::OsStr, update: bool) -> Result<Option<File>, RunError> {
    match if update {
        at::open_file_for_update(&folder.fd, name)
    } else {
        at::open_file(&folder.fd, name)
    } {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(false, e)),
    }
}

fn lock(file: &File, exclusive: bool) -> Result<(), RunError> {
    match if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    } {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(raced()),
        Err(e) => Err(io_error(true, e)),
    }
}

fn target(paths: &Paths, scope: Scope) -> Result<(PathBuf, PathBuf), RunError> {
    let path = match scope {
        Scope::Project => paths.project.clone(),
        Scope::User => paths
            .user
            .clone()
            .ok_or_else(|| invalid("no absolute user configuration directory is available"))?,
        Scope::Local => {
            let repo = paths
                .repository
                .as_ref()
                .ok_or_else(|| invalid("machine-local settings need a Git checkout"))?;
            let path = repo.common.join("mdkg/config.local.json");
            if paths.local.as_ref() != Some(&path) {
                return Err(invalid(
                    "local configuration path does not match the repository",
                ));
            }
            path
        }
    };
    if !path.is_absolute() {
        return Err(invalid("configuration paths must be absolute"));
    }
    let anchor = match scope {
        Scope::Project => {
            if !path.ends_with(".mdkg/config.json") {
                return Err(invalid("project settings need .mdkg/config.json"));
            }
            path.parent()
                .and_then(Path::parent)
                .expect("checked project path")
                .to_path_buf()
        }
        Scope::Local => paths
            .repository
            .as_ref()
            .expect("checked repository")
            .common
            .clone(),
        Scope::User => {
            if !path.ends_with("mdkg/config.json") {
                return Err(invalid("user settings need mdkg/config.json"));
            }
            let mut ancestor = path.parent().expect("absolute config path");
            loop {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(_) => break ancestor.to_path_buf(),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        ancestor = ancestor
                            .parent()
                            .ok_or_else(|| invalid("configuration has no existing ancestor"))?
                    }
                    Err(e) => return Err(io_error(false, e)),
                }
            }
        }
    };
    Ok((path, anchor))
}

impl Prepared {
    /// Prepare one layer only. The caller builds the typed value with
    /// `settings::edit::value` or supplies an already parsed JSON value.
    pub fn new(paths: &Paths, scope: Scope, key: &str, value: Value) -> Result<Self, RunError> {
        let (path, anchor) = target(paths, scope)?;
        let tree = Tree::open(&anchor).map_err(|e| io_error(false, e))?;
        let root_id = id(&tree
            .folder(Path::new(""), false, Links::Never)
            .map_err(|e| io_error(false, e))?)
        .map_err(|e| io_error(false, e))?;
        let relative = path
            .strip_prefix(&anchor)
            .map_err(|_| invalid("configuration escaped its scope"))?;
        let parent = relative
            .parent()
            .expect("config file has parent")
            .to_path_buf();
        let name = relative
            .file_name()
            .expect("config has file name")
            .to_os_string();
        let folder = match tree.folder(&parent, false, Links::Never) {
            Ok(folder) => Some(folder),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_error(false, e)),
        };
        let parent_id = folder
            .as_ref()
            .map(id)
            .transpose()
            .map_err(|e| io_error(false, e))?;
        let before = folder
            .as_ref()
            .map(|dir| read(dir, &name, false))
            .transpose()?
            .flatten()
            .as_ref()
            .map(snapshot)
            .transpose()?;
        if scope == Scope::Project && before.is_none() {
            return Err(invalid("project configuration is absent; run init first"));
        }
        let origin = Origin::file(scope.layer(), &path);
        let input = match &before {
            Some(snapshot) => Input::parse(origin, &snapshot.bytes),
            None => Input::new(origin, serde_json::json!({})),
        }
        .map_err(|error| {
            let mut error = input_error(error);
            error.kind = ErrorKind::Failure;
            error
        })?;
        let candidate = edit::set(&input, key, value).map_err(input_error)?;
        let changed = candidate.value != input.value;
        let mut bytes = if changed {
            serde_json::to_vec_pretty(&candidate.value).map_err(|e| io_error(true, e))?
        } else {
            Vec::new()
        };
        if changed {
            bytes.push(b'\n');
        }
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("edited configuration exceeds the 1 MiB limit"));
        }
        Ok(Self {
            path,
            scope,
            anchor,
            tree,
            root_id,
            parent,
            parent_id,
            name,
            before,
            candidate,
            bytes,
            changed,
        })
    }

    pub fn candidate(&self) -> &Input {
        &self.candidate
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_folder(&self, folder: &Folder) -> Result<(), RunError> {
        let now = Tree::open(&self.anchor)
            .and_then(|tree| tree.folder(Path::new(""), false, Links::Never))
            .and_then(|folder| id(&folder));
        if now.ok() != Some(self.root_id) {
            return Err(raced());
        }
        let named = self
            .tree
            .folder(&self.parent, false, Links::Never)
            .map_err(|_| raced())?;
        let expected = id(folder).map_err(|e| io_error(false, e))?;
        if id(&named).map_err(|e| io_error(false, e))? != expected {
            return Err(raced());
        }
        if self.parent_id.is_some_and(|before| before != expected) {
            return Err(raced());
        }
        Ok(())
    }

    pub fn apply(
        self,
        validate: impl FnOnce(&Input) -> Result<(), RunError>,
    ) -> Result<Receipt, Failure> {
        self.apply_with(validate, |_| Ok(()))
    }

    fn apply_with(
        self,
        validate: impl FnOnce(&Input) -> Result<(), RunError>,
        hook: impl Fn(Checkpoint) -> io::Result<()>,
    ) -> Result<Receipt, Failure> {
        let mut published = false;
        let result = (|| {
            validate(&self.candidate)?;
            hook(Checkpoint::Validated).map_err(|e| io_error(true, e))?;
            let named_root = Tree::open(&self.anchor)
                .and_then(|tree| tree.folder(Path::new(""), false, Links::Never))
                .and_then(|folder| id(&folder));
            if named_root.ok() != Some(self.root_id) {
                return Err(raced());
            }
            let folder = self
                .tree
                .folder(&self.parent, self.parent_id.is_none(), Links::Never)
                .map_err(|e| io_error(true, e))?;
            self.check_folder(&folder)?;
            let original = read(&folder, &self.name, self.changed)?;
            if let Some(file) = &original {
                lock(file, self.changed)?;
            }
            let current = original.as_ref().map(snapshot).transpose()?;
            if current != self.before {
                return Err(raced());
            }
            if !self.changed {
                return Ok(Receipt {
                    path: self.path.clone(),
                    scope: self.scope,
                    changed: false,
                });
            }
            if self.parent_id.is_none() {
                for parent in self.parent.ancestors() {
                    let dir = self
                        .tree
                        .folder(parent, false, Links::Never)
                        .map_err(|e| io_error(true, e))?;
                    rustix::fs::fsync(&dir.fd).map_err(|e| io_error(true, e))?;
                }
            }
            let name = OsString::from(format!(
                ".mdkg-config-{}.tmp",
                uuid::Uuid::now_v7().simple()
            ));
            let mut staged = at::create_file(&folder.fd, &name).map_err(|e| io_error(true, e))?;
            let mut cleanup = Stage {
                folder: &folder,
                name: &name,
                published: false,
            };
            lock(&staged, true)?;
            staged
                .write_all(&self.bytes)
                .map_err(|e| io_error(true, e))?;
            if let Some(before) = &self.before {
                staged
                    .set_permissions(std::fs::Permissions::from_mode(before.mode))
                    .map_err(|e| io_error(true, e))?;
            }
            staged.sync_all().map_err(|e| io_error(true, e))?;
            hook(Checkpoint::Staged).map_err(|e| io_error(true, e))?;
            self.check_folder(&folder)?;
            let again = read(&folder, &self.name, false)?
                .as_ref()
                .map(snapshot)
                .transpose()?;
            if again != self.before {
                return Err(raced());
            }
            if self.before.is_none() {
                at::rename_noreplace(&folder.fd, &name, &folder.fd, &self.name).map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        raced()
                    } else {
                        io_error(true, e)
                    }
                })?;
            } else {
                at::rename(&folder.fd, &name, &folder.fd, &self.name)
                    .map_err(|e| io_error(true, e))?;
            }
            published = true;
            cleanup.published = true;
            hook(Checkpoint::Published).map_err(|e| io_error(true, e))?;
            rustix::fs::fsync(&folder.fd).map_err(|e| io_error(true, e))?;
            Ok(Receipt {
                path: self.path.clone(),
                scope: self.scope,
                changed: true,
            })
        })();
        result.map_err(|mut error| {
            if error.path.is_none() {
                error.path = Some(self.path.to_string_lossy().into_owned());
            }
            if published {
                error.message = format!(
                    "configuration was published; durability was not confirmed: {}",
                    error.message
                );
            }
            Failure { error, published }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checkpoint {
    Validated,
    Staged,
    Published,
}

#[derive(Debug)]
struct Stage<'a> {
    folder: &'a Folder,
    name: &'a std::ffi::OsStr,
    published: bool,
}
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = at::unlink(&self.folder.fd, self.name);
        }
    }
}

#[cfg(test)]
mod tests;
