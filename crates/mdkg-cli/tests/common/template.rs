//! Fixture templates. nextest runs every test in its own process, so a
//! fixture's setup can't be shared in memory. A template is the part of a
//! fixture that doesn't depend on where it lives (the first commit, a
//! migration), built once for each build of mdkg and of the test, and
//! copied into each test. Templates are kept in this build's target
//! folder ([`root`]) with their git folder named [`GIT_DIR`], so the
//! worktree never holds a nested repository (mdkg refuses a turn in one,
//! `run.cwd_outside`); only a test's own copy gets a `.git`. Anything that
//! records the fixture's own path in a database (a worktree, `install`)
//! stays in the test.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Where the build ran, kept in the template (and never copied) so a copy
/// can name its own folder wherever the build named that one.
const ORIGIN: &str = ".mdkg-template-origin";

/// A template's git folder, renamed from `.git` (orchestrator, after
/// #13438): never a repository while it sits in the worktree.
pub const GIT_DIR: &str = "git-dir";

/// Copies the template `name` into the empty folder `dest`, building it
/// first with `build` (into an empty folder) if there's none for this key
/// yet. The key covers the name, the mdkg binary, the test binary (so an
/// edit to `build` makes a new template) and the contents of `inputs`.
///
/// A copy names `dest` wherever a text file the build wrote named the
/// build's folder (migrate's journal does). Any other file that names it
/// (a database, its log, git's index) can't be rewritten safely, so the
/// copy refuses it: that step belongs in the test.
///
/// `name` must be unique to one test binary: making a template removes
/// that name's older templates.
pub fn copy(name: &str, inputs: &[&Path], build: impl FnOnce(&Path), dest: &Path) {
    let binaries = [
        PathBuf::from(env!("CARGO_BIN_EXE_mdkg")),
        std::env::current_exe().expect("test binary"),
    ];
    let root = root();
    fs::create_dir_all(&root).expect("template root");
    let ready = root.join(format!("{name}-{:016x}", key(name, &binaries, inputs)));
    if !ready.is_dir() {
        // Built outside the worktree, where its `.git` may exist, then
        // copied in with that folder renamed, and renamed into place, so a
        // concurrent test sees either no template or a whole one. The
        // first rename wins.
        // Only this user may add to it (a hook, say) while it builds.
        let built = super::temp::private(&format!("mdkg-template-{name}"));
        build(&built);
        let canonical = fs::canonicalize(&built).expect("build folder");
        // A copy restores any top-level `git-dir` as `.git`, so the name
        // is the template's own.
        if fs::symlink_metadata(built.join(GIT_DIR)).is_ok() {
            let left = match fs::remove_dir_all(&built) {
                Ok(()) => String::new(),
                Err(e) => format!(" (and {} is left: {e})", built.display()),
            };
            panic!(
                "the template {name} wrote {GIT_DIR}, a name templates reserve for their git folder{left}"
            );
        }
        if fs::symlink_metadata(built.join(".git")).is_ok() {
            fs::rename(built.join(".git"), built.join(GIT_DIR)).expect("rename the git folder");
        }
        fs::write(
            built.join(ORIGIN),
            format!("{}\n{}\n", built.display(), canonical.display()),
        )
        .expect("origin");
        let staging =
            super::temp::create_in(&root, &format!(".{name}"), &NEXT).expect("template staging");
        copy_all(&built, &staging).expect("stage template");
        fs::remove_dir_all(&built).ok();
        if fs::rename(&staging, &ready).is_ok() {
            prune(&root, name, &ready);
        } else {
            assert!(ready.is_dir(), "template {} wasn't made", ready.display());
            fs::remove_dir_all(&staging).ok();
        }
    }
    copy_tree(&ready, dest).expect("copy template");
    if fs::symlink_metadata(dest.join(GIT_DIR)).is_ok() {
        fs::rename(dest.join(GIT_DIR), dest.join(".git")).expect("restore the git folder");
    }
    let origin = fs::read_to_string(ready.join(ORIGIN)).expect("template origin");
    let dest = fs::canonicalize(dest).expect("destination");
    renamed(dest.as_os_str().as_encoded_bytes(), origin.lines(), &dest);
}

/// Where templates are kept: this build's target folder, which holds no
/// nested repository because each template's git folder is [`GIT_DIR`].
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-templates")
}

/// The template key: its name, each binary's size and modification time,
/// and every path, kind and content under `inputs`.
pub fn key(name: &str, binaries: &[PathBuf], inputs: &[&Path]) -> u64 {
    let mut key = DefaultHasher::new();
    name.hash(&mut key);
    for binary in binaries {
        let meta = fs::metadata(binary).expect("binary");
        binary.hash(&mut key);
        meta.len().hash(&mut key);
        meta.modified().expect("binary mtime").hash(&mut key);
    }
    for input in inputs {
        hash_tree(input, input, &mut key);
    }
    key.finish()
}

/// Rewrites every text file under `dir` that names one of the `origins` to
/// name `to` instead; any other file that names one is refused.
pub fn renamed<'a>(to: &[u8], origins: impl Iterator<Item = &'a str>, dir: &Path) {
    let mut origins: Vec<&str> = origins.filter(|o| !o.is_empty()).collect();
    // The longest first: one origin may be inside another (`/tmp` and
    // `/private/tmp` on macOS).
    origins.sort_by_key(|o| std::cmp::Reverse(o.len()));
    for entry in fs::read_dir(dir).expect("copied folder") {
        let path = entry.expect("entry").path();
        let kind = fs::symlink_metadata(&path).expect("metadata").file_type();
        if kind.is_dir() {
            renamed(to, origins.iter().copied(), &path);
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let bytes = fs::read(&path).expect("copied file");
        let mut text = bytes.clone();
        for origin in &origins {
            text = replace(&text, origin.as_bytes(), to);
        }
        if text != bytes {
            // Only text can change length safely: a binary file's
            // checksums and offsets would silently break.
            assert!(
                std::str::from_utf8(&bytes).is_ok_and(|t| !t.contains('\0')),
                "{} names the template's own folder; build that part in the test",
                path.display()
            );
            fs::write(&path, text).expect("rewrite copied file");
        }
    }
}

/// Fails, naming the file, if any file or link target under `dir` names
/// `outside`, as its bytes or JSON-escaped, or if any symbolic link is
/// absolute or has `..`, wherever it points: a template's links must be
/// relative and stay below their own folder. A build
/// calls it with the building test's own folder (its HOME, its harness
/// scripts): a copy can't rewrite those, and they're gone once that test
/// ends.
pub fn names_none(dir: &Path, outside: &Path) {
    let raw = outside.as_os_str().as_encoded_bytes().to_vec();
    let mut needles = vec![raw.clone()];
    if let Some(text) = outside.to_str() {
        let json = serde_json::to_string(text).expect("JSON path");
        let escaped = json.as_bytes()[1..json.len() - 1].to_vec();
        if escaped != raw {
            needles.push(escaped);
        }
    }
    unnamed(dir, outside, &needles);
}

fn unnamed(dir: &Path, outside: &Path, needles: &[Vec<u8>]) {
    let refuse = |path: &Path| {
        panic!(
            "{} names the building test's folder {}; the template build may write only inside its own folder",
            path.display(),
            outside.display()
        )
    };
    for entry in fs::read_dir(dir).expect("template folder") {
        let path = entry.expect("entry").path();
        let kind = fs::symlink_metadata(&path).expect("metadata").file_type();
        let bytes = if kind.is_dir() {
            unnamed(&path, outside, needles);
            continue;
        } else if kind.is_symlink() {
            let target = fs::read_link(&path).expect("link target");
            // Where a link with `..` or an absolute target ends depends on
            // the links it passes through, so a template may hold only
            // links that stay below their own folder: relative, with no
            // `..`. None does today.
            if !target.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            }) {
                panic!(
                    "{} links to {}; a template's links must stay below their own folder",
                    path.display(),
                    target.display()
                );
            }
            target.as_os_str().as_encoded_bytes().to_vec()
        } else if kind.is_file() {
            fs::read(&path).expect("template file")
        } else {
            continue;
        };
        if needles
            .iter()
            .any(|n| bytes.windows(n.len()).any(|w| w == n.as_slice()))
        {
            refuse(&path);
        }
    }
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.windows(from.len()).position(|w| w == from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Removes this name's other templates once they're an hour old, best
/// effort: a newer one may still be in use by a concurrent test.
fn prune(root: &Path, name: &str, keep: &Path) {
    let prefix = format!("{name}-");
    // `<name>-` and the 16 hex digits of a key: not a longer name.
    let ours = |file: &str| {
        file.strip_prefix(&prefix)
            .is_some_and(|k| k.len() == 16 && k.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        let file_name = entry.file_name();
        if path != keep && stale && ours(&file_name.to_string_lossy()) {
            fs::remove_dir_all(&path).ok();
        }
    }
}

/// Hashes every path under `dir` (relative to `top`), its kind and contents.
fn hash_tree(top: &Path, dir: &Path, key: &mut DefaultHasher) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("template input")
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        path.strip_prefix(top).expect("under top").hash(key);
        let kind = fs::symlink_metadata(&path).expect("metadata").file_type();
        if kind.is_dir() {
            hash_tree(top, &path, key);
        } else if kind.is_symlink() {
            fs::read_link(&path).expect("link").hash(key);
        } else {
            fs::read(&path).expect("input file").hash(key);
        }
    }
}

/// Copies a folder's contents, keeping file modes and symbolic links,
/// without the template's origin record.
fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    copy_dir(src, dest, false)
}

/// [`copy_tree`], keeping the origin record.
fn copy_all(src: &Path, dest: &Path) -> io::Result<()> {
    copy_dir(src, dest, true)
}

fn copy_dir(src: &Path, dest: &Path, origin: bool) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        // The origin record sits at the top only.
        if !origin && entry.file_name() == ORIGIN {
            continue;
        }
        let target = dest.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_dir(&entry.path(), &target, origin)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
