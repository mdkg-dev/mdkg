//! `verify`: checks a work bundle with no store, or the repository's
//! store, against an anchor's recorded chain commitment.

use std::path::Path;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::GraphError;
use mdkg_log::bundle::Anchor;
use schemars::JsonSchema;
use serde::Serialize;

use crate::BIN;
use crate::anchor::{anchor_path, invalid, read_anchor, verified_chain};
use crate::coord::Repo;
use crate::output::{self, Report};

#[derive(Debug, Serialize, JsonSchema)]
pub struct VerifyData {
    /// What was checked: `bundle` or `store`.
    pub source: String,
    /// The coordinator store the chain belongs to.
    pub store: String,
    /// Records in the chain.
    pub count: u64,
    /// The hash of its last record.
    pub head: String,
    /// The seq of the anchored record the chain was checked against, or
    /// none when there was no anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchored: Option<u64>,
}

fn anchor_from(path: &Path) -> Result<Anchor, GraphError> {
    read_anchor(path)?.ok_or_else(|| {
        GraphError::not_found(
            Code::IoReadFailed,
            format!("no anchor at {}", path.display()),
        )
        .with_hint("check the --anchor path; `anchor` writes them in .mdkg/anchors")
    })
}

fn missing() -> Diagnostic {
    Diagnostic::warning(
        Code::LogAnchorMissing,
        "no anchor to check against: the chain itself verifies",
    )
    .with_hint(format!("`{BIN} anchor` writes one; commit it"))
}

/// Verifies a bundle file, against `anchor` if given. Needs no repository.
pub fn bundle(bundle: &Path, anchor: Option<&Path>, json: bool) -> u8 {
    let report = (|| -> Result<Report<VerifyData>, GraphError> {
        let text = std::fs::read_to_string(bundle).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GraphError::not_found(
                    Code::IoReadFailed,
                    format!("no bundle at {}", bundle.display()),
                )
                .with_hint(format!("check the path; `{BIN} export --work` writes one"))
            } else {
                GraphError::failure(
                    Code::IoReadFailed,
                    format!("can't read {}: {e}", bundle.display()),
                )
            }
        })?;
        let anchor = anchor.map(anchor_from).transpose()?;
        let v = mdkg_log::bundle::verify_bundle(&text, anchor.as_ref()).map_err(|e| {
            let code = match e {
                mdkg_log::bundle::BundleError::Mismatch(ref m)
                    if anchor.is_some() && m.contains("anchor") =>
                {
                    Code::LogAnchorMismatch
                }
                _ => Code::LogBroken,
            };
            invalid(code, format!("{}: {e}", bundle.display()))
        })?;
        Ok(Report::ok(VerifyData {
            source: "bundle".into(),
            store: v.store,
            count: v.count,
            head: v.head,
            anchored: v.anchored,
        })
        .with(anchor.is_none().then(missing)))
    })()
    .unwrap_or_else(Report::failed);
    emit(report, json)
}

/// Verifies the repository's store: its chain, its journal, and the
/// graph's anchor (or `anchor`).
pub fn store(root: &Path, anchor: Option<&Path>, json: bool) -> u8 {
    let report = (|| -> Result<Report<VerifyData>, GraphError> {
        let repo = Repo::open(root)?;
        let store = repo.store()?;
        let (id, chain, _) = verified_chain(&store)?;
        let anchor = match anchor {
            Some(p) => Some(anchor_from(p)?),
            None => read_anchor(&anchor_path(root, &id))?,
        };
        if let Some(a) = &anchor {
            a.check_chain(&id, &chain)
                .map_err(|e| invalid(Code::LogAnchorMismatch, e.to_string()))?;
        }
        let (count, head) = chain
            .last()
            .map(|(r, h)| (r.seq, h.clone()))
            .unwrap_or_default();
        Ok(Report::ok(VerifyData {
            source: "store".into(),
            store: id,
            count,
            head,
            anchored: anchor.as_ref().map(|a| a.seq),
        })
        .with(anchor.is_none().then(missing)))
    })()
    .unwrap_or_else(Report::failed);
    emit(report, json)
}

fn emit(report: Report<VerifyData>, json: bool) -> u8 {
    output::emit("verify", json, report, |d| {
        let anchored = d
            .anchored
            .map_or_else(String::new, |s| format!(", anchored at seq {s}"));
        // "verified store <id>" for the store itself, "verified bundle of
        // store <id>" for an export of it.
        let what = if d.source == "store" {
            format!("store {}", d.store)
        } else {
            format!("{} of store {}", d.source, d.store)
        };
        println!(
            "verified {what}: {} records, head {}{anchored}",
            d.count, d.head
        );
    })
}
