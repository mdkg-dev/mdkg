//! `anchor`: commits the coordinator store's record chain to git as its
//! head hash. Written to
//! `.mdkg/anchors/<store-id>.json`, by hand, when a goal is done, and
//! before a release. Committing it is the caller's step.

use std::path::{Path, PathBuf};

use mdkg_coord_sqlite::{Store, StoreError};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError};
use mdkg_log::bundle::Anchor;
use mdkg_log::record::Record;
use schemars::JsonSchema;
use serde::Serialize;

use crate::BIN;
use crate::coord::{Repo, store_error};
use crate::output::{self, Report};

/// Where a graph keeps its anchors, relative to the graph root.
pub const ANCHOR_DIR: &str = ".mdkg/anchors";

#[derive(Debug, Serialize, JsonSchema)]
pub struct AnchorData {
    /// The coordinator store's id.
    pub store: String,
    /// The chain's length when anchored.
    pub seq: u64,
    /// The hash of the record at `seq`.
    pub head: String,
    /// The anchor file, relative to the graph root.
    pub path: String,
    /// The anchor this one replaced, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Previous>,
    /// The compaction this anchor wrote first: a `log.checkpoint` record
    /// summarizing the records since the last one. None when nothing came
    /// since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointData>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckpointData {
    /// The `log.checkpoint` record's seq and hash.
    pub seq: u64,
    pub hash: String,
    /// The records it summarizes, by seq.
    pub from: u64,
    pub to: u64,
    /// At most 150 words, citing the first and last records' hashes.
    pub summary: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Previous {
    pub seq: u64,
    pub head: String,
}

/// The anchor file for `store` under the graph `root`.
pub fn anchor_path(root: &Path, store: &str) -> PathBuf {
    root.join(ANCHOR_DIR).join(format!("{store}.json"))
}

/// A chain or anchor that doesn't verify (exit 2).
pub fn invalid(code: Code, message: impl Into<String>) -> GraphError {
    GraphError::new(ErrorKind::Validation, Diagnostic::error(code, message))
}

/// A store error about its chain as `log.broken`; the rest as usual.
pub fn chain_error(e: StoreError) -> GraphError {
    match e {
        StoreError::Log(why) => invalid(Code::LogBroken, why).with_hint(format!(
            "the store's record chain was changed outside mdkg; `{BIN} store snapshots` lists \
             copies taken before upgrades"
        )),
        other => store_error(other),
    }
}

/// The store's verified chain, read once: its id, its records with their
/// hashes, and their canonical texts.
pub type Chain = (String, Vec<(Record, String)>, Vec<String>);

pub fn verified_chain(store: &Store) -> Result<Chain, GraphError> {
    let (report, texts) = store.verified_log().map_err(chain_error)?;
    let chain = texts
        .iter()
        .map(|t| Record::parse(t))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(Code::LogBroken, e.to_string()))?;
    Ok((report.store, chain, texts))
}

/// Reads the anchor at `path`, if there is one.
pub fn read_anchor(path: &Path) -> Result<Option<Anchor>, GraphError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(GraphError::failure(
                Code::IoReadFailed,
                format!("can't read {}: {e}", path.display()),
            ));
        }
    };
    Anchor::parse(&text)
        .map(Some)
        .map_err(|e| invalid(Code::LogBroken, format!("{}: {e}", path.display())))
}

/// Writes `text` to `path` whole or not at all: a temporary file beside
/// it, synced, then renamed over it.
/// Refuses a link, or anything but a folder, on the way to `rel` under
/// `root` (RC1 audit SEC3): a cloned repository can ship `.mdkg/work` or
/// `.mdkg/anchors` as a link out of itself, and nothing mdkg writes goes
/// through one. A folder that isn't there yet is fine: it's made as a real
/// one. A path outside the root (profile C's store folder, in the git
/// folder) isn't checked: no repository ships it.
pub(crate) fn refuse_linked_folders(root: &Path, rel: &str) -> Result<(), GraphError> {
    if Path::new(rel).is_absolute() {
        return Ok(());
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let mut at = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                let shown = mdkg_graph::index::relative_path(root, &at);
                return Err(GraphError::failure(
                    Code::IoWriteFailed,
                    format!("{shown} is a link or a file, so nothing is written through it"),
                )
                .at(shown, None)
                .with_hint("make it a folder in the repository"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(GraphError::failure(
                    Code::IoWriteFailed,
                    format!("can't check {}: {e}", at.display()),
                ));
            }
        }
    }
    Ok(())
}

pub fn write_whole(path: &Path, text: &str) -> Result<(), GraphError> {
    let fail = |e: std::io::Error| {
        GraphError::failure(
            Code::IoWriteFailed,
            format!("can't write {}: {e}", path.display()),
        )
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let partial = path.with_extension(format!("partial-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&partial)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, path)
    })();
    if let Err(e) = written {
        std::fs::remove_file(&partial).ok();
        return Err(fail(e));
    }
    Ok(())
}

/// Compacts `store`'s chain and anchors it under the graph `root`. An
/// anchor already there must name a record the chain still holds, unless
/// `force`: the chain was then rewritten or restored, and the new anchor
/// says so. Nothing is written when that check fails.
pub fn write_anchor(
    root: &Path,
    store: &mut Store,
    force: bool,
) -> Result<Report<AnchorData>, GraphError> {
    let (id, chain, _) = verified_chain(store)?;
    let path = anchor_path(root, &id);
    refuse_linked_folders(root, &format!("{ANCHOR_DIR}/{id}.json"))?;
    let previous = read_anchor(&path)?;
    let mut diagnostics = Vec::new();
    if let Some(p) = &previous
        && let Err(e) = p.check_chain(&id, &chain)
    {
        if !force {
            return Err(invalid(Code::LogAnchorMismatch, e.to_string()).with_hint(format!(
                "if the store was restored on purpose, `{BIN} anchor --force` anchors the chain \
                 as it is now"
            )));
        }
        diagnostics.push(Diagnostic::warning(Code::LogAnchorMismatch, e.to_string()));
    }
    let checkpoint = store.checkpoint().map_err(chain_error)?;
    let (last, head) = store.log_head().map_err(chain_error)?;
    let anchor = Anchor::new(&id, last, &head, crate::coord::now_ms());
    let text = anchor
        .to_text()
        .map_err(|e| GraphError::failure(Code::IoWriteFailed, e.to_string()))?;
    write_whole(&path, &text)?;
    Ok(Report::ok(AnchorData {
        store: id.clone(),
        seq: last,
        head,
        path: format!("{ANCHOR_DIR}/{id}.json"),
        previous: previous.map(|p| Previous {
            seq: p.seq,
            head: p.head,
        }),
        checkpoint: checkpoint.map(|c| CheckpointData {
            seq: c.seq,
            hash: c.hash,
            from: c.from,
            to: c.to,
            summary: c.summary,
        }),
    })
    .with(diagnostics))
}

/// Anchors the chain once a goal is done, as a notice: the goal is done
/// whether or not the anchor could be written.
pub fn on_goal_done(root: &Path, store: &mut Store) -> Diagnostic {
    match write_anchor(root, store, false) {
        Ok(report) => {
            let d = report.data.as_ref();
            Diagnostic::new(
                Code::LogAnchored,
                mdkg_format::Severity::Info,
                format!(
                    "anchored the coordinator's record chain at seq {} in {}",
                    d.map_or(0, |d| d.seq),
                    d.map_or("", |d| d.path.as_str())
                ),
            )
            .with_hint("commit it")
        }
        Err(e) => {
            let mut d = *e.diagnostic;
            d.severity = mdkg_format::Severity::Warning;
            d.message = format!("the goal is done, but it wasn't anchored: {}", d.message);
            d
        }
    }
}

pub fn run(root: &Path, force: bool, json: bool) -> u8 {
    let report = (|| {
        let repo = Repo::open(root)?;
        let mut store = repo.store()?;
        write_anchor(root, &mut store, force)
    })()
    .unwrap_or_else(Report::failed);
    output::emit("anchor", json, report, |d| {
        if let Some(c) = &d.checkpoint {
            println!("checkpoint {}: {}", c.seq, c.summary);
        }
        println!(
            "anchored store {} at seq {} ({}) in {}; commit it",
            d.store, d.seq, d.head, d.path
        );
    })
}
