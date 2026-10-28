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
        let opened = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
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
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| failed(mdkg_index::IndexError::from(e)))?;
                }
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
