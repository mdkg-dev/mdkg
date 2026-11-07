//! The head's tree, written out from its objects into a private temporary
//! folder, so the adapters read exactly the commit: never the checkout,
//! whose uncommitted edits, filters, ignored files or sparse entries could
//! differ from it.
//!
//! - Blobs are written as stored (`cat-file --batch`); submodules are left
//!   out (the decision engine forces full for a changed gitlink at either
//!   endpoint, even when a rule or containing package could map its path).
//! - Nothing is written through a symlink, and nothing can alias: a tree
//!   with a symlink, a path git would refuse to check out (`..`, `.git`, an
//!   empty segment), or two paths (or folders) that differ only in case
//!   fails the write, and the run is full. After the write, the folder must
//!   hold exactly the tree's files and folders, which also catches names a
//!   disk treats as equal in other ways (Unicode normalization).
//! - The folder is private to the user (0700) and goes away when the value
//!   is dropped.
//! - It goes in the first of [`places`] that takes a new folder: the
//!   temporary folder, else a per-user cache, else `$CARGO_TARGET_DIR`
//!   (a sandboxed reviewer's one writable folder). Each place skipped is
//!   reported with why, and the report names the place used, so a run
//!   that couldn't write anywhere says so instead of going full silently.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use super::git::{Repo, TreeEntry};

/// Where a snapshot can go, in the order tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// The system temporary folder (`$TMPDIR`).
    Temp,
    /// The user's cache: `$XDG_CACHE_HOME/mdkg/affected`, else
    /// `~/Library/Caches/mdkg/affected` on macOS and
    /// `~/.cache/mdkg/affected` elsewhere.
    UserCache,
    /// `$CARGO_TARGET_DIR/mdkg-affected`: the build folder a sandbox
    /// grants a reviewer that runs tests.
    CargoTargetDir,
}

/// A place a snapshot didn't go, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Skipped {
    pub place: Place,
    pub detail: String,
}

/// Each place with the folder it names in this environment (`None` when
/// it names none: a variable unset or not absolute).
pub fn places() -> Vec<(Place, Option<PathBuf>)> {
    let var = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let cache = var("XDG_CACHE_HOME").or_else(|| {
        var("HOME").map(|h| {
            if cfg!(target_os = "macos") {
                h.join("Library").join("Caches")
            } else {
                h.join(".cache")
            }
        })
    });
    vec![
        (Place::Temp, Some(std::env::temp_dir())),
        (
            Place::UserCache,
            cache.map(|c| c.join("mdkg").join("affected")),
        ),
        (
            Place::CargoTargetDir,
            var("CARGO_TARGET_DIR").map(|t| t.join("mdkg-affected")),
        ),
    ]
}

/// More than this many files, or bytes, isn't written: the run is full.
pub const MAX_FILES: usize = 400_000;
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct Tree {
    /// The written tree's top folder (canonical): `<base>/tree`.
    pub root: PathBuf,
    /// An empty private folder beside it, for the tools' own caches:
    /// `<base>/cache`.
    pub cache: PathBuf,
    /// The private folder holding both (canonical).
    base: PathBuf,
    /// The same folder as first named, before symlinks in the temporary
    /// folder's own path were resolved (`/var` for `/private/var`, say).
    named: PathBuf,
    /// Where it went.
    pub place: Place,
}

impl Tree {
    /// `text` with the temporary folder's path replaced by `<head>`, so a
    /// tool's error reads the same on every run.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (dir, name) in [
            (self.root.clone(), "<head>"),
            (self.named.join("tree"), "<head>"),
            (self.base.clone(), "<scratch>"),
            (self.named.clone(), "<scratch>"),
        ] {
            if let Some(d) = dir.to_str() {
                out = out.replace(d, name);
            }
        }
        out
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn check_path(path: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(path)
        .map_err(|_| "a path in the head's tree isn't UTF-8".to_string())?;
    for segment in text.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.eq_ignore_ascii_case(".git")
        {
            return Err(format!(
                "the head's tree has a path git won't check out: {text}"
            ));
        }
    }
    Ok(text)
}

fn fresh_dir(parent: &Path) -> Result<(PathBuf, PathBuf), String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = parent.join(format!("mdkg-affected-{}-{nanos}-{n}", std::process::id()));
    // A new folder (not `create_dir_all`), private to the user.
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let root = std::fs::canonicalize(&dir).map_err(|e| {
        let _ = std::fs::remove_dir(&dir);
        format!("can't resolve {}: {e}", dir.display())
    })?;
    Ok((root, dir))
}

/// A new private folder in the first place that takes one; each place
/// before it goes in `skipped`.
fn place_dir(
    places: &[(Place, Option<PathBuf>)],
    skipped: &mut Vec<Skipped>,
) -> Result<(PathBuf, PathBuf, Place), String> {
    for (place, parent) in places {
        let made = match parent {
            None => Err("not set".to_string()),
            // The temporary folder exists already; the others are made
            // as needed (only the snapshot itself has to be private).
            Some(p) if *place == Place::Temp => fresh_dir(p),
            Some(p) => std::fs::create_dir_all(p)
                .map_err(|e| format!("can't create {}: {e}", p.display()))
                .and_then(|()| fresh_dir(p)),
        };
        match made {
            Ok((root, named)) => return Ok((root, named, *place)),
            Err(detail) => skipped.push(Skipped {
                place: *place,
                detail,
            }),
        }
    }
    Err(format!(
        "no folder could hold the head's snapshot ({})",
        skipped
            .iter()
            .map(|s| format!("{}: {}", place_name(s.place), s.detail))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

fn place_name(place: Place) -> &'static str {
    match place {
        Place::Temp => "the temporary folder",
        Place::UserCache => "the user cache",
        Place::CargoTargetDir => "$CARGO_TARGET_DIR",
    }
}

/// Writes `entries` (the head's tree) into a new private folder in the
/// first of `places` that takes one. The places passed over go in
/// `skipped`, whether or not the write then succeeds.
pub fn write(
    repo: &Repo,
    entries: &[TreeEntry],
    places: &[(Place, Option<PathBuf>)],
    skipped: &mut Vec<Skipped>,
) -> Result<Tree, String> {
    let files: Vec<&TreeEntry> = entries.iter().filter(|e| e.mode != "160000").collect();
    if files.len() > MAX_FILES {
        return Err(format!(
            "the head's tree has {} files, more than {MAX_FILES}",
            files.len()
        ));
    }
    let total: u64 = files.iter().map(|e| e.size.unwrap_or(0)).sum();
    if total > MAX_BYTES {
        return Err(format!(
            "the head's tree holds {total} bytes, more than {MAX_BYTES}"
        ));
    }
    // Every path and every folder above it, by its case-folded name.
    let mut names: BTreeMap<String, &str> = BTreeMap::new();
    let mut folders = BTreeSet::new();
    let mut paths = Vec::with_capacity(files.len());
    for e in &files {
        let path = check_path(&e.path)?;
        match e.mode.as_str() {
            "100644" | "100755" => {}
            "120000" => {
                return Err(format!(
                    "the head's tree has a symlink ({path}); affected doesn't write or follow symlinks"
                ));
            }
            other => return Err(format!("{path} has an unexpected mode {other}")),
        }
        let prefixes = path
            .match_indices('/')
            .map(|(i, _)| &path[..i])
            .chain(std::iter::once(path));
        for prefix in prefixes {
            let folded = prefix.to_lowercase();
            match names.get(&folded) {
                Some(seen) if *seen != prefix => {
                    return Err(format!(
                        "the head's tree has paths that differ only in case ({seen} and {prefix}); a case-insensitive disk can't keep them apart"
                    ));
                }
                Some(_) if prefix == path => {
                    return Err(format!("the head's tree lists {path} twice"));
                }
                _ => {
                    names.insert(folded, prefix);
                }
            }
            if prefix != path {
                folders.insert(prefix);
            }
        }
        paths.push(path);
    }
    if let Some(clash) = paths.iter().find(|p| folders.contains(*p)) {
        return Err(format!(
            "the head's tree has {clash} as both a file and a folder"
        ));
    }
    let (base, named, place) = place_dir(places, skipped)?;
    let tree = Tree {
        root: base.join("tree"),
        cache: base.join("cache"),
        base,
        named,
        place,
    };
    for dir in [&tree.root, &tree.cache] {
        std::fs::create_dir(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let oids: Vec<String> = files.iter().map(|e| e.oid.clone()).collect();
    repo.each_blob(&oids, |i, blob| {
        let dest = tree.root.join(paths[i]);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_entry(&dest, blob).map_err(|e| std::io::Error::other(format!("{}: {e}", paths[i])))
    })
    .map_err(|e| format!("can't write the head's tree: {e}"))?;
    let (wrote_files, wrote_folders) =
        count(&tree.root).map_err(|e| format!("can't read back the head's tree: {e}"))?;
    if wrote_files != paths.len() || wrote_folders != folders.len() {
        return Err(format!(
            "the written tree holds {wrote_files} files in {wrote_folders} folders, but the head has {} in {}; the disk merged names it treats as equal",
            paths.len(),
            folders.len()
        ));
    }
    Ok(tree)
}

/// The files and folders under `root` (not counting it), without following
/// symlinks.
fn count(root: &Path) -> std::io::Result<(usize, usize)> {
    let (mut files, mut folders) = (0, 0);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                folders += 1;
                stack.push(entry.path());
            } else {
                files += 1;
            }
        }
    }
    Ok((files, folders))
}

/// The tracked paths, for the adapters' discovery (submodules included:
/// they're paths in the tree, though nothing is written for them).
pub fn paths(entries: &[TreeEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_string))
        .collect()
}

fn write_entry(dest: &Path, blob: &[u8]) -> std::io::Result<()> {
    // `create_new`: a path written twice fails instead of overwriting.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    std::io::Write::write_all(&mut f, blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_git_wont_check_out() {
        for bad in [
            &b"../x"[..],
            b"a/../b",
            b".git/config",
            b"a/.GIT/x",
            b"a//b",
            b"./a",
            b"bad\xff",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(check_path(b".github/x.yml").unwrap(), ".github/x.yml");
        assert_eq!(check_path(b"a/.gitignore").unwrap(), "a/.gitignore");
    }

    /// A place no one can create in (a file where its folder should be, so
    /// root is refused too), and a writable one after it.
    #[cfg(unix)]
    #[test]
    fn an_unwritable_place_falls_back_to_the_next_and_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let top =
            std::env::temp_dir().join(format!("mdkg-affected-place-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let locked = top.join("locked");
        std::fs::write(&locked, "a file, not a folder\n").unwrap();
        let open = top.join("open").join("mdkg").join("affected");
        let places = [
            (Place::Temp, Some(locked.clone())),
            (Place::UserCache, None),
            (Place::CargoTargetDir, Some(open.clone())),
        ];
        let mut skipped = Vec::new();
        let (root, named, place) = place_dir(&places, &mut skipped).unwrap();
        assert_eq!(place, Place::CargoTargetDir);
        assert_eq!(named.parent(), Some(open.as_path()));
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(
            skipped.iter().map(|s| s.place).collect::<Vec<_>>(),
            [Place::Temp, Place::UserCache]
        );
        assert!(
            skipped[0].detail.starts_with("can't create "),
            "{skipped:?}"
        );
        assert_eq!(skipped[1].detail, "not set");

        // Nowhere at all: an error naming every place.
        let mut skipped = Vec::new();
        let e = place_dir(&places[..2], &mut skipped).unwrap_err();
        assert!(
            e.starts_with("no folder could hold the head's snapshot"),
            "{e}"
        );
        assert!(e.contains("the temporary folder: can't create"), "{e}");
        assert!(e.contains("the user cache: not set"), "{e}");
        std::fs::remove_dir_all(&top).unwrap();
    }
}
