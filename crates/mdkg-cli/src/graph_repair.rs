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

/// Whether the repository is in ref mode. A repository whose mode can't be
/// read keeps the checkout-mode gates; the command's own probe reports why.
fn ref_mode(root: &Path) -> bool {
    matches!(
        crate::config_cmd::ref_mode_probe(root),
        crate::config_cmd::RefProbe::Ref(_)
    )
}

/// Gate the physical shared graph checkout, including a nested graph root.
/// Other checkouts remain writable even when repository diagnostics warn.
/// Ref mode has no graph checkout: its writes bind the graph ref, from a
/// `.worktrees/graph` view too (which setup leaves unregistered), and the
/// one command that writes its own checkout is held by `refuse_view`.
pub(crate) fn require_writable(root: &Path) -> Result<(), GraphError> {
    let Some(repo) = topology(root)? else {
        return Ok(());
    };
    if ref_mode(&repo.top) {
        return Ok(());
    }
    worktree::ensure_graph_writable(&repo.top).map_err(crate::run::graph_error)
}

/// Keep the graph stable from the command's first read until its last write.
/// Ordinary checkouts and non-repositories do not acquire a graph lock;
/// ref mode's writes take the graph ref's own lock.
pub(crate) fn lock_writer(root: &Path) -> Result<Option<worktree::GraphWriteGuard>, GraphError> {
    let Some(repo) = topology(root)? else {
        return Ok(None);
    };
    if ref_mode(&repo.top) {
        return Ok(None);
    }
    worktree::lock_graph_writer(&repo.top).map_err(crate::run::graph_error)
}

/// Ref mode: `migrate` writes and commits the checkout it runs in, so it
/// refuses in a graph view: its commit would land on the view's detached
/// HEAD, never the graph ref, and be lost at the next refresh. A view is
/// told by git alone (orchestrator #15983): a linked worktree whose HEAD
/// is detached, wherever it lives and whatever state its owner is in. A
/// primary checkout always passes. A view whose mode can't be read is
/// refused too: ref mode may be configured there.
pub(crate) fn refuse_view(root: &Path) -> Result<(), GraphError> {
    let Some(repo) = topology(root)? else {
        return Ok(());
    };
    if !is_view(&repo.top)? {
        return Ok(());
    }
    let why = match crate::config_cmd::ref_mode_probe(&repo.top) {
        crate::config_cmd::RefProbe::Checkout => return Ok(()),
        crate::config_cmd::RefProbe::Ref(_) => "in ref mode".to_owned(),
        crate::config_cmd::RefProbe::Unreadable(e) | crate::config_cmd::RefProbe::Broken(e) => {
            format!("and the mode can't be read: {}", e.diagnostic.message)
        }
    };
    // The rule is git's (#15977): a detached linked worktree, which is
    // what a graph view is, though a code checkout can be one too.
    Err(GraphError::failure(
        Code::GraphView,
        format!(
            "migrate writes the checkout it runs in, and {} is a linked worktree with a \
             detached HEAD {why}, as a view of the graph ref is",
            repo.top.display()
        ),
    )
    .with_hint(VIEW_HINT))
}

/// The view gate's way out: never "attach a branch" for the graph view,
/// whose refresh needs it detached (Terra #16191, #16437).
const VIEW_HINT: &str = "run migrate from the primary checkout or another code checkout on a \
     branch; in the graph view (.worktrees/graph) leave it detached, since `install --view` \
     refreshes only a detached view, and only a detached code worktree should `git switch \
     <branch>`";

/// `top` is a linked worktree (its own git folder sits under the clone's
/// `<common>/worktrees/`) with a detached HEAD. Anything git can't answer
/// is an error, never "not a view".
fn is_view(top: &Path) -> Result<bool, GraphError> {
    let git = |args: &[&str]| {
        mdkg_coord_sqlite::git::command(top)
            .args(args)
            .run(None)
            .map_err(crate::coord::git_error)
    };
    let common = mdkg_coord_sqlite::git::common_dir(top)
        .and_then(std::fs::canonicalize)
        .map_err(crate::coord::git_error)?;
    let out = git(&["rev-parse", "--absolute-git-dir"])?;
    if !out.status.success() {
        return Err(GraphError::failure(
            Code::IoGitFailed,
            format!("can't find the git folder of {}", top.display()),
        ));
    }
    let dir = String::from_utf8_lossy(&out.stdout);
    let dir = std::fs::canonicalize(dir.trim_end_matches(['\n', '\r']))
        .map_err(crate::coord::git_error)?;
    if dir.parent() != Some(common.join("worktrees").as_path()) {
        return Ok(false);
    }
    // `symbolic-ref -q` exits 1 for a detached HEAD.
    let head = git(&["symbolic-ref", "-q", "HEAD"])?;
    match head.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(GraphError::failure(
            Code::IoGitFailed,
            format!("can't read the HEAD of {}", top.display()),
        )),
    }
}

pub(crate) fn error(state: &RepairStatus) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, state.reason.clone()).with_hint(state.hint())
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

#[cfg(test)]
mod view_hint_tests {
    /// Review #16437: in the bin target, which a graph_repair.rs change
    /// selects, the hint keeps the view detached and offers `git switch`
    /// only to a detached code worktree.
    #[test]
    fn the_view_hint_keeps_the_graph_view_detached() {
        let hint = super::VIEW_HINT;
        assert!(hint.contains("in the graph view (.worktrees/graph) leave it detached"));
        assert!(hint.contains("only a detached code worktree should `git switch <branch>`"));
        assert!(!hint.contains("--detach"), "{hint}");
        assert!(hint.contains("primary checkout"));
    }
}
