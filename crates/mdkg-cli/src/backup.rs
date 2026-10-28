//! `store snapshots` and `store restore`: the copies a store upgrade takes
//! of the coordinator store, and putting one back (orchestrator,
//! 2026-10-21, upgrade safety).

use std::path::{Path, PathBuf};

use clap::Subcommand;
use mdkg_coord_sqlite::{StoreError, git};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::BIN;
use crate::coord::{Repo, git_error, store_error};
use crate::output::{self, Report, rfc3339};

#[derive(Debug, Subcommand)]
pub enum StoreCommand {
    /// List the copies of the coordinator store taken before upgrades and
    /// restores.
    #[command(after_help = examples!("store snapshots", "store snapshots --json"))]
    Snapshots {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Put a store copy back.
    ///
    /// Stop every process using the repository's store first: commands, the
    /// MCP server and the launcher.
    #[command(after_help = examples!("store restore <copy>"))]
    Restore {
        /// The copy: a path, or a file name beside the store.
        snapshot: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl StoreCommand {
    pub fn json(&self) -> bool {
        match self {
            StoreCommand::Snapshots { json } | StoreCommand::Restore { json, .. } => *json,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            StoreCommand::Snapshots { .. } => "store.snapshots",
            StoreCommand::Restore { .. } => "store.restore",
        }
    }
}

pub fn run(root: &Path, command: &StoreCommand) -> u8 {
    match command {
        StoreCommand::Snapshots { json } => snapshots(root, *json),
        StoreCommand::Restore { snapshot, json } => restore(root, snapshot, *json),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SnapshotRecord {
    /// The copy's path.
    pub path: String,
    /// Its schema and mail schema, as the store had them when copied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_schema: Option<String>,
    /// Whether `store restore` would take it: a copy of this repository's
    /// store.
    pub restorable: bool,
    pub bytes: u64,
    /// When it was written, RFC 3339 UTC.
    pub written: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SnapshotsData {
    /// The repository's coordinator store.
    pub store: String,
    /// Its copies, oldest first.
    pub snapshots: Vec<SnapshotRecord>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoreData {
    /// The store restored.
    pub store: String,
    /// The copy it was restored from.
    pub restored_from: String,
    /// The store's versions before and after (`2+msg3`: schema 2, mail
    /// schema 3).
    pub replaced: String,
    pub restored: String,
    /// A copy of the store as it was just before, which `store restore`
    /// takes too.
    pub previous: String,
}

fn store_path(root: &Path) -> Result<PathBuf, GraphError> {
    let repo = Repo::open(root)?;
    git::store_path(&repo.top).map_err(git_error)
}

pub fn snapshots(root: &Path, json: bool) -> u8 {
    let report = (|| -> Result<Report<SnapshotsData>, GraphError> {
        let store = store_path(root)?;
        let snapshots = mdkg_coord_sqlite::backups(&store)
            .map_err(store_error)?
            .into_iter()
            .map(|b| SnapshotRecord {
                path: b.path.display().to_string(),
                restorable: mdkg_coord_sqlite::copy_of(
                    &store,
                    &b.path,
                    b.backup_of.as_deref(),
                    b.store_id.as_deref(),
                ),
                schema: b.schema,
                msg_schema: b.msg_schema,
                bytes: b.bytes,
                written: rfc3339(b.modified_ms),
            })
            .collect();
        Ok(Report::ok(SnapshotsData {
            store: store.display().to_string(),
            snapshots,
        }))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("store.snapshots", json, report, |d| {
        if d.snapshots.is_empty() {
            println!("no copies of {}", d.store);
        }
        for s in &d.snapshots {
            println!(
                "{} (schema {}, mail {}, {}{})",
                s.path,
                s.schema.as_deref().unwrap_or("?"),
                s.msg_schema.as_deref().unwrap_or("none"),
                s.written,
                if s.restorable {
                    ""
                } else {
                    ", not this store's"
                }
            );
        }
    })
}

pub fn restore(root: &Path, copy: &str, json: bool) -> u8 {
    let report = (|| -> Result<Report<RestoreData>, GraphError> {
        let store = store_path(root)?;
        // A bare file name is one of the copies beside the store.
        let given = Path::new(copy);
        let copy = if !given.exists() && given.components().count() == 1 {
            store.with_file_name(given)
        } else {
            given.to_path_buf()
        };
        let listed = || format!("`{BIN} store snapshots` lists this store's copies");
        // A copy that isn't there is not found (exit 3), not a conflict;
        // any other trouble reading it is the restore's to refuse.
        if std::fs::symlink_metadata(&copy).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(GraphError::not_found(
                Code::IoReadFailed,
                "no store copy by that name or path",
            )
            .with_hint(listed()));
        }
        let restored = mdkg_coord_sqlite::restore(&store, &copy).map_err(|e| match e {
            StoreError::Restore(_) => {
                GraphError::conflict(Code::CoordStoreFailed, e.to_string()).with_hint(listed())
            }
            other => store_error(other),
        })?;
        let older = (restored.restored != mdkg_coord_sqlite::build_label()).then(|| {
            Diagnostic::warning(
                Code::CoordStoreRestored,
                format!(
                    "the restored store ({}) is older than this build ({}): the next command \
                     this build runs upgrades it again, copying it first",
                    restored.restored,
                    mdkg_coord_sqlite::build_label()
                ),
            )
            .with_hint("reinstall the older build before running anything else here")
        });
        Ok(Report::ok(RestoreData {
            store: store.display().to_string(),
            restored_from: restored.from.display().to_string(),
            replaced: restored.replaced,
            restored: restored.restored,
            previous: restored.previous.display().to_string(),
        })
        .with(older))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("store.restore", json, report, |d| {
        println!(
            "restored {} from {} ({} -> {}); the store as it was is at {}",
            d.store, d.restored_from, d.replaced, d.restored, d.previous
        );
    })
}
