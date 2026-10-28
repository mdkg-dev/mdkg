//! Finding a graph's documents: `.mdkg/{core,design,work}/**/*.md` for each
//! enabled workspace, within the configured size limits.

use std::path::{Path, PathBuf};

use crate::config::{Config, Workspace};
use mdkg_format::Code;

use crate::error::GraphError;
use crate::fs_walk;

const DOC_FOLDERS: &[&str] = &["core", "design", "work"];

/// Documents grouped by workspace, in workspace and file order.
#[derive(Debug, Clone)]
pub struct Documents {
    pub by_workspace: Vec<(Workspace, Vec<PathBuf>)>,
}

/// Lists every document, enforcing `index.limits`.
pub fn discover(root: &Path, config: &Config) -> Result<Documents, GraphError> {
    let limits = &config.limits;
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut by_workspace = Vec::new();
    for ws in config.workspaces.iter().filter(|w| w.enabled) {
        let base = fs_walk::normalize(&root.join(&ws.path).join(&ws.mdkg_dir));
        let mut docs = Vec::new();
        for folder in DOC_FOLDERS {
            let dir = base.join(folder);
            if !dir.is_dir() {
                continue;
            }
            for file in fs_walk::markdown_files(&dir, limits.max_depth)? {
                let size = std::fs::metadata(&file)
                    .map_err(|e| {
                        GraphError::failure(
                            Code::IoReadFailed,
                            format!("failed to stat {}: {e}", file.display()),
                        )
                    })?
                    .len();
                check_limits(limits, &file, size, files, bytes)?;
                files += 1;
                bytes += size;
                docs.push(file);
            }
        }
        // Profile C (`work.committed: none`): the root workspace's work
        // nodes also live beside the coordinator store.
        if let Some(store_work) = &config.store_work_dir
            && crate::config::is_root_workspace(ws)
            && store_work.is_dir()
        {
            for file in fs_walk::markdown_files(store_work, limits.max_depth)? {
                let size = std::fs::metadata(&file)
                    .map_err(|e| {
                        GraphError::failure(
                            Code::IoReadFailed,
                            format!("failed to stat {}: {e}", file.display()),
                        )
                    })?
                    .len();
                check_limits(limits, &file, size, files, bytes)?;
                files += 1;
                bytes += size;
                docs.push(file);
            }
        }
        // One order for the whole workspace, by full path as 0.6.0 sorts
        // it, so `a-b.md` comes before `a/x.md` and the first-seen file of
        // a duplicate id matches. Use deterministic byte order.
        docs.sort_by(|a, b| {
            a.to_string_lossy()
                .as_bytes()
                .cmp(b.to_string_lossy().as_bytes())
        });
        by_workspace.push((ws.clone(), docs));
    }
    Ok(Documents { by_workspace })
}

/// `index.limits` for one more file of `size` bytes, after `files` files
/// of `bytes` bytes.
fn check_limits(
    limits: &crate::config::Limits,
    file: &Path,
    size: u64,
    files: u64,
    bytes: u64,
) -> Result<(), GraphError> {
    if size > limits.max_file_bytes {
        return Err(GraphError::failure(
            Code::IndexLimitExceeded,
            format!(
                "graph file exceeds index.limits.max_file_bytes ({}): {}",
                limits.max_file_bytes,
                file.display()
            ),
        ));
    }
    if files + 1 > limits.max_files {
        return Err(GraphError::failure(
            Code::IndexLimitExceeded,
            format!(
                "graph file count exceeds index.limits.max_files ({})",
                limits.max_files
            ),
        ));
    }
    if bytes + size > limits.max_total_bytes {
        return Err(GraphError::failure(
            Code::IndexLimitExceeded,
            format!(
                "graph bytes exceed index.limits.max_total_bytes ({})",
                limits.max_total_bytes
            ),
        ));
    }
    Ok(())
}
