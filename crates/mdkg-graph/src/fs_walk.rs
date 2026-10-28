//! Deterministic directory walking.
//!
//! Entries are visited in byte order of their names. The reference
//! implementation uses `readdirSync` order, which is name order on macOS
//! (APFS) but unspecified elsewhere; sorting makes mdkg deterministic on
//! every platform. Symlinks are skipped, as the reference skips them.

use std::path::{Component, Path, PathBuf};

use mdkg_format::Code;

use crate::error::GraphError;

fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>, GraphError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| {
            GraphError::failure(
                Code::IoReadFailed,
                format!("failed to read {}: {e}", dir.display()),
            )
        })?
        .collect::<Result<_, _>>()
        .map_err(|e| {
            GraphError::failure(
                Code::IoReadFailed,
                format!("failed to read {}: {e}", dir.display()),
            )
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

/// Every regular `.md` file under `dir`, recursively, in name order.
pub fn markdown_files(dir: &Path, max_depth: u64) -> Result<Vec<PathBuf>, GraphError> {
    let mut out = Vec::new();
    walk(dir, 0, max_depth, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, depth: u64, max_depth: u64, out: &mut Vec<PathBuf>) -> Result<(), GraphError> {
    if depth > max_depth {
        return Err(GraphError::failure(
            Code::IndexLimitExceeded,
            format!(
                "graph directory depth exceeds index.limits.max_depth ({max_depth}): {}",
                dir.display()
            ),
        ));
    }
    for entry in sorted_entries(dir)? {
        let file_type = entry.file_type().map_err(|e| {
            GraphError::failure(
                Code::IoReadFailed,
                format!("failed to stat {}: {e}", entry.path().display()),
            )
        })?;
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, depth + 1, max_depth, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|x| x == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Lexically normalizes a path like Node's `path.join`: drops `.` segments
/// and folds `..` into its parent, without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
