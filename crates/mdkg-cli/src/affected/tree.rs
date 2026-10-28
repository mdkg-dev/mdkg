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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::git::{Repo, TreeEntry};

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

fn fresh_dir() -> Result<(PathBuf, PathBuf), String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("mdkg-affected-{}-{nanos}-{n}", std::process::id()));
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

/// Writes `entries` (the head's tree) into a new temporary folder.
pub fn write(repo: &Repo, entries: &[TreeEntry]) -> Result<Tree, String> {
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
    let (base, named) = fresh_dir()?;
    let tree = Tree {
        root: base.join("tree"),
        cache: base.join("cache"),
        base,
        named,
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

/// The contents of `path` at the head, if the tree has it as a file.
pub fn read(repo: &Repo, entries: &[TreeEntry], path: &str) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = entries
        .iter()
        .find(|e| e.path == path.as_bytes() && matches!(e.mode.as_str(), "100644" | "100755"))
    else {
        return Ok(None);
    };
    let mut out = None;
    repo.each_blob(std::slice::from_ref(&entry.oid), |_, blob| {
        out = Some(blob.to_vec());
        Ok(())
    })
    .map_err(|e| format!("can't read {path} at the head: {e}"))?;
    Ok(out)
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
}
