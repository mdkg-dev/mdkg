//! Observe graph checkout damage before config parsing or graph mutation.

use std::path::Path;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_run::install::worktree::{self, RepairStatus};

use crate::coord::Repo;
use crate::output::{self, Report};

fn topology(root: &Path) -> Result<Option<Repo>, GraphError> {
    // Init may create several missing folders. Inspect the nearest existing
    // ancestor without treating a present, damaged entry as an absent folder.
    let mut existing = root;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = existing.parent() else {
                    break;
                };
                existing = parent;
            }
            Err(e) => return Err(crate::coord::git_error(e)),
        }
    }
    match Repo::open(existing) {
        Ok(repo) => Ok(Some(repo)),
        // A present but damaged Git entry must not bypass the freeze gate.
        Err(e)
            if existing
                .ancestors()
                .any(|p| std::fs::symlink_metadata(p.join(".git")).is_ok()) =>
        {
            Err(e)
        }
        Err(_) => Ok(None),
    }
}

/// Non-repositories retain each command's existing missing-graph/git error.
/// In a repository, no graph config or coordinator store is read here.
pub(crate) fn inspect(root: &Path) -> Result<Option<RepairStatus>, GraphError> {
    // Repo::open checks only Git topology and caches that check; it does
    // not parse config. Keep the ordinary command's Git error on failure.
    let Some(repo) = topology(root)? else {
        return Ok(None);
    };
    worktree::repair_status(&repo.top).map_err(crate::run::graph_error)
}

/// Gate the physical shared graph checkout, including a nested graph root.
/// Other checkouts remain writable even when repository diagnostics warn.
pub(crate) fn require_writable(root: &Path) -> Result<(), GraphError> {
    let Some(repo) = topology(root)? else {
        return Ok(());
    };
    worktree::ensure_graph_writable(&repo.top).map_err(crate::run::graph_error)
}

/// Keep the graph stable from the command's first read until its last write.
/// Ordinary checkouts and non-repositories do not acquire a graph lock.
pub(crate) fn lock_writer(root: &Path) -> Result<Option<worktree::GraphWriteGuard>, GraphError> {
    let Some(repo) = topology(root)? else {
        return Ok(None);
    };
    worktree::lock_graph_writer(&repo.top).map_err(crate::run::graph_error)
}

pub(crate) fn error(state: &RepairStatus) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, state.reason.clone())
        .with_hint(state.guidance.join("\n"))
}

/// A frozen status has no inventory: opening its store could both block and
/// trust the damaged config. Still show independently readable build warnings.
/// This is the ordinary status envelope's failure form, with no `data` field.
pub(crate) fn status(root: &Path, command: &str, json: bool, state: &RepairStatus) -> u8 {
    let warnings = mdkg_run::state::run_dir(root)
        .map_err(|e| e.message)
        .and_then(|dir| mdkg_run::builds::live(&dir).map_err(|e| e.message))
        .map(|live| live.warnings(&crate::version::build()))
        .unwrap_or_else(|e| vec![e]);
    let report =
        Report::<()>::failed(error(state)).with(warnings.into_iter().map(|warning| {
            Diagnostic::warning(Code::DoctorCheckWarned, format!("builds: {warning}"))
        }));
    output::emit(command, json, report, |_| {})
}
