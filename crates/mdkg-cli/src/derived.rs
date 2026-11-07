//! The derived index for commands, and `mdkg index`.
//!
//! The index file sits beside the coordinator store, in
//! `<git common dir>/mdkg/index/`, one per worktree. Outside a git
//! repository, or where that folder can't be written (an agent sandbox),
//! the index is built in memory for the one command, with a warning.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Subcommand;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{GraphError, Index, config, index};
use mdkg_index::{Db, IndexInfo, SyncStats};
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{self, Report};
use mdkg_format::program::BIN;

/// Where the graph at `root` keeps its index: `None` outside a git
/// repository.
pub(crate) fn index_path(root: &Path) -> Option<PathBuf> {
    let common = mdkg_coord_sqlite::git::common_dir(root).ok()?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Some(
        common
            .join("mdkg")
            .join("index")
            .join(mdkg_index::index_file_name(&root)),
    )
}

/// `<common>/mdkg`, the clone's state folder, made 0700 when missing,
/// whatever the umask (Opus #16893); an existing one keeps its mode, as
/// the store leaves it. The search index and `work localize` come
/// through here, and the store and the graph-ref writer make it 0700 too.
/// Other creators (install's `mdkg/graph`, the hooks' `waiting` folder,
/// some dry runs) still use the umask's mode; see RELEASE_NOTES.
pub(crate) fn state_dir(common: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let dir = common.join("mdkg");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        // The umask may have narrowed it further: exactly 0700.
        Ok(()) => std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} must be a folder, not a link or file",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Makes the folder the index file at `path` goes in: `.git/mdkg` through
/// [`state_dir`], then `index/` in it, 0700 as well.
fn index_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    if let Some(common) = dir.parent().and_then(Path::parent) {
        state_dir(common)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// What a command read from the index.
pub(crate) struct Opened<T> {
    pub value: T,
    pub sync: SyncStats,
    /// The index file; `None` when it was built in memory.
    pub path: Option<PathBuf>,
    pub warnings: Vec<Diagnostic>,
}

/// Syncs the index with the graph as just read (before any coordinator
/// state is laid over it) and runs `read` on it in the same transaction,
/// so the answer matches that graph. Problems keeping the file come back
/// as warnings, with an index in memory instead.
pub(crate) fn read<T>(
    root: &Path,
    graph: &Index,
    mut read: impl FnMut(&Db) -> Result<T, mdkg_index::IndexError>,
) -> Result<Opened<T>, GraphError> {
    let mut warnings = Vec::new();
    if let Some(path) = index_path(root) {
        let opened = index_dir(&path)
            .map_err(mdkg_index::IndexError::from)
            .and_then(|()| Db::open_synced_then(&path, root, graph, &mut read));
        match opened {
            Ok((_, sync, value)) => {
                return Ok(Opened {
                    value,
                    sync,
                    path: Some(path),
                    warnings,
                });
            }
            Err(e) => warnings.push(
                Diagnostic::warning(
                    Code::IoWriteFailed,
                    format!("the derived index was built in memory for this command: {e}"),
                )
                .with_hint(format!(
                    "the index is kept at {}; `{BIN} index rebuild` makes it again",
                    path.display()
                )),
            ),
        }
    }
    let failed =
        |e: mdkg_index::IndexError| GraphError::failure(Code::IoWriteFailed, e.to_string());
    let mut db = Db::in_memory().map_err(failed)?;
    let (sync, value) = db.sync_then(root, graph, read).map_err(failed)?;
    Ok(Opened {
        value,
        sync,
        path: None,
        warnings,
    })
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// Drop the derived index and build it again from the graph.
    #[command(after_help = examples!("index rebuild", "index rebuild --verbose"))]
    Rebuild {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Also print how long it took, on stderr.
        #[arg(long)]
        verbose: bool,
    },
    /// Bring the index up to date and say where it is and what it holds.
    #[command(after_help = examples!("index status", "index status --json"))]
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Also print how long it took, on stderr.
        #[arg(long)]
        verbose: bool,
    },
}

impl IndexCommand {
    /// The output's schema name: one per subcommand.
    pub fn name(&self) -> &'static str {
        match self {
            IndexCommand::Rebuild { .. } => "index.rebuild",
            IndexCommand::Status { .. } => "index.status",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            IndexCommand::Rebuild { json, .. } | IndexCommand::Status { json, .. } => *json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexData {
    /// The index file; absent when it's in memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub index: IndexInfo,
    /// What bringing it up to date changed.
    pub sync: SyncStats,
}

pub fn run(root: &Path, command: &IndexCommand) -> u8 {
    let rebuild = matches!(command, IndexCommand::Rebuild { .. });
    let verbose = matches!(
        command,
        IndexCommand::Rebuild { verbose: true, .. } | IndexCommand::Status { verbose: true, .. }
    );
    let started = Instant::now();
    let report = (|| -> Result<Report<IndexData>, GraphError> {
        let config = config::load(root)?;
        let graph = index::build(root, &config)?;
        let failed =
            |e: mdkg_index::IndexError| GraphError::failure(Code::IoWriteFailed, e.to_string());
        let opened = match index_path(root) {
            // Built anew in one transaction; a file that can't be opened
            // is removed first.
            Some(path) if rebuild => {
                index_dir(&path).map_err(|e| failed(mdkg_index::IndexError::from(e)))?;
                let mut db = Db::open(&path).map_err(failed)?;
                let sync = db.rebuild(root, &graph).map_err(failed)?;
                Opened {
                    value: db.info().map_err(failed)?,
                    sync,
                    path: Some(path),
                    warnings: Vec::new(),
                }
            }
            _ => read(root, &graph, Db::info)?,
        };
        Ok(Report::ok(IndexData {
            path: opened.path.map(|p| p.display().to_string()),
            index: opened.value,
            sync: opened.sync,
        })
        .with(opened.warnings))
    })()
    .unwrap_or_else(Report::failed);
    let code = output::emit(command.name(), command.json(), report, |d| {
        match &d.path {
            Some(p) => println!("index: {}", output::shown_path(p)),
            None => println!("index: in memory"),
        }
        println!(
            "nodes: {} ({} added, {} updated, {} removed, {} unchanged){}",
            d.index.nodes,
            d.sync.added,
            d.sync.updated,
            d.sync.removed,
            d.sync.unchanged,
            if d.sync.rebuilt { ", rebuilt" } else { "" }
        );
        println!("edges: {}", d.index.edges);
    });
    // A timing would make the output differ run to run: keep the output
    // deterministic by sending timings to stderr only when asked for.
    if verbose {
        eprintln!("took: {} ms", started.elapsed().as_millis());
    }
    code
}
