//! `DerivedProvider` is an extension interface that computes outputs from
//! nodes, such as embeddings for semantic search or facts for a code
//! graph, and keeps them in the derived index.
//!
//! No provider ships in 0.7.0; a local embedding model comes in 0.7.x.
//! Outputs are keyed by each node's row hash (which covers its file's
//! hash), so [`refresh`] asks a provider only about nodes that changed
//! since it last saw them.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use mdkg_graph::Index;
use rusqlite::params;
use schemars::JsonSchema;
use serde::Serialize;

use crate::store::{Db, IndexError};

/// What a provider is shown of one node.
#[derive(Debug, Clone, Copy)]
pub struct Document<'a> {
    pub qid: &'a str,
    pub node_type: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub path: &'a str,
    /// The hash of the node's file.
    pub file_hash: &'a str,
}

/// One stored output, as a provider gets it back to answer a query.
#[derive(Debug, Clone, Copy)]
pub struct Stored<'a> {
    pub qid: &'a str,
    pub output: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "derived provider: {}", self.0)
    }
}

impl std::error::Error for ProviderError {}

/// A source of derived outputs.
pub trait DerivedProvider {
    /// A stable name, such as `embeddings.local`.
    fn name(&self) -> &str;

    /// The outputs' version: a new one recomputes every node.
    fn version(&self) -> u32;

    /// One output per document it handles, in any order: `(qid, bytes)`.
    /// A document it skips gets no output and is asked about again only
    /// after it changes.
    fn derive(&mut self, docs: &[Document<'_>]) -> Result<Vec<(String, Vec<u8>)>, ProviderError>;

    /// Nodes matching `text`, best first, scored from the stored outputs.
    /// Providers that don't search return nothing.
    fn search(
        &mut self,
        _text: &str,
        _stored: &[Stored<'_>],
        _limit: usize,
    ) -> Result<Vec<(String, f64)>, ProviderError> {
        Ok(Vec::new())
    }
}

/// What [`refresh`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RefreshStats {
    /// Documents the provider was shown.
    pub derived: usize,
    /// Outputs dropped because their node is gone.
    pub dropped: usize,
}

#[derive(Debug)]
pub enum RefreshError {
    Index(IndexError),
    Provider(ProviderError),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefreshError::Index(e) => e.fmt(f),
            RefreshError::Provider(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RefreshError {}

impl From<rusqlite::Error> for RefreshError {
    fn from(e: rusqlite::Error) -> Self {
        RefreshError::Index(IndexError::Sqlite(e))
    }
}

/// Documents a provider is shown per call.
const BATCH: usize = 64;

fn key(provider: &dyn DerivedProvider) -> String {
    format!("{}@{}", provider.name(), provider.version())
}

/// Brings a provider's outputs up to date with the graph `index`, read
/// from `root`: the index is synced, nodes the provider hasn't seen at
/// their current row hash go to [`DerivedProvider::derive`] in batches,
/// and outputs of nodes that are gone are dropped. Each batch is stored in
/// its own transaction, only against the row hash it was derived from, so
/// a failure keeps what was done and a node changed meanwhile is asked
/// about again.
pub fn refresh(
    db: &mut Db,
    root: &Path,
    index: &Index,
    provider: &mut dyn DerivedProvider,
) -> Result<RefreshStats, RefreshError> {
    let name = key(provider);
    let mut stats = RefreshStats::default();
    // Synced from `index` in the same transaction that lists the stale
    // rows, so each captured row hash is of the document `index` holds,
    // whatever another process synced before.
    let (_, stale) = db
        .sync_then(root, index, |db| {
            let mut stmt = db.conn().prepare(
                "SELECT n.qid, n.row_hash, n.file_hash FROM nodes n \
                 LEFT JOIN derived d ON d.provider = ?1 AND d.qid = n.qid \
                 WHERE d.row_hash IS NOT n.row_hash ORDER BY n.qid",
            )?;
            let rows: Vec<(String, String, String)> = stmt
                .query_map([&name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .map_err(RefreshError::Index)?;
    for batch in stale.chunks(BATCH) {
        let docs: Vec<Document> = batch
            .iter()
            .filter_map(|(qid, _, file_hash)| {
                index.get(qid).map(|n| Document {
                    qid: &n.qid,
                    node_type: &n.node_type,
                    title: &n.title,
                    body: &n.body,
                    path: &n.path,
                    file_hash,
                })
            })
            .collect();
        let outputs = provider.derive(&docs).map_err(RefreshError::Provider)?;
        stats.derived += docs.len();
        // Each output is kept against the row hash its document had when
        // it was read: a node another process synced since keeps no
        // output, and is shown again on the next refresh.
        let hashes: HashMap<&str, &str> = batch
            .iter()
            .map(|(q, h, _)| (q.as_str(), h.as_str()))
            .collect();
        db.write_tx(|db| {
            let mut put = db.conn().prepare(
                "INSERT OR REPLACE INTO derived (provider, qid, row_hash, output) \
                 SELECT ?1, qid, row_hash, ?3 FROM nodes WHERE qid = ?2 AND row_hash = ?4",
            )?;
            for (qid, output) in &outputs {
                if let Some(hash) = hashes.get(qid.as_str()) {
                    put.execute(params![name, qid, output, hash])?;
                }
            }
            Ok(())
        })
        .map_err(RefreshError::Index)?;
    }
    stats.dropped = db.conn().execute(
        "DELETE FROM derived WHERE provider = ?1 AND qid NOT IN (SELECT qid FROM nodes)",
        [&name],
    )?;
    Ok(stats)
}

/// Asks a provider to search its stored outputs.
pub fn search(
    db: &Db,
    provider: &mut dyn DerivedProvider,
    text: &str,
    limit: usize,
) -> Result<Vec<(String, f64)>, RefreshError> {
    let name = key(provider);
    let rows: Vec<(String, Vec<u8>)> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT qid, output FROM derived WHERE provider = ?1 ORDER BY qid")?;
        stmt.query_map([&name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let stored: Vec<Stored> = rows
        .iter()
        .map(|(qid, output)| Stored { qid, output })
        .collect();
    provider
        .search(text, &stored, limit)
        .map_err(RefreshError::Provider)
}
