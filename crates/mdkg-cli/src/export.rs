//! `export --work`: the coordinator store's whole record chain as a
//! bundle anyone can verify without the store. Message bodies stay in the
//! store; records carry their digests.

use std::path::{Path, PathBuf};

use mdkg_format::Code;
use mdkg_graph::GraphError;
use schemars::JsonSchema;
use serde::Serialize;

use crate::anchor::{invalid, verified_chain, write_whole};
use crate::coord::Repo;
use crate::output::{self, Report};

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportData {
    /// The bundle written.
    pub path: String,
    /// The coordinator store's id.
    pub store: String,
    /// Records in the bundle.
    pub count: u64,
    /// The hash of its last record.
    pub head: String,
}

pub fn run(root: &Path, work: bool, out: Option<&Path>, force: bool, json: bool) -> u8 {
    let report = (|| -> Result<Report<ExportData>, GraphError> {
        if !work {
            return Err(
                GraphError::usage(Code::UsageInvalidArguments, "nothing to export")
                    .with_hint("pass --work to export the coordinator's record chain"),
            );
        }
        let repo = Repo::open(root)?;
        let store = repo.store()?;
        // The bundle and its receipt come from the one read verified.
        let (id, chain, texts) = verified_chain(&store)?;
        let bundle = mdkg_log::bundle::write_bundle(&id, &texts)
            .map_err(|e| invalid(Code::LogBroken, e.to_string()))?;
        let (count, head) = chain
            .last()
            .map(|(r, h)| (r.seq, h.clone()))
            .unwrap_or_default();
        let path = out.map_or_else(
            || PathBuf::from(format!("mdkg-work-{id}-{count}.jsonl")),
            Path::to_path_buf,
        );
        if !force && std::fs::symlink_metadata(&path).is_ok() {
            return Err(GraphError::failure(
                Code::IoWriteFailed,
                format!("{} already exists", path.display()),
            )
            .with_hint("pass --force to replace it, or --out to name another file"));
        }
        write_whole(&path, &bundle)?;
        Ok(Report::ok(ExportData {
            path: path.display().to_string(),
            store: id,
            count,
            head,
        }))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("export", json, report, |d| {
        println!(
            "exported {} records of store {} to {} (head {})",
            d.count, d.store, d.path, d.head
        );
    })
}
