//! The SQLite file: its schema, opening it, and syncing it with the graph.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hasher as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mdkg_format::Value as FmValue;
use mdkg_format::hash::Digest;
use mdkg_graph::index::edge_targets;
use mdkg_graph::{Index, IndexNode};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::Serialize;

/// The index's schema. It's also in the file name, so builds with other
/// schemas keep their own files instead of rebuilding each other's.
pub const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for another process's sync to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

const SCHEMA: &str = "
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE nodes (
    rowid INTEGER PRIMARY KEY,
    qid TEXT NOT NULL UNIQUE,
    id TEXT NOT NULL,
    ws TEXT NOT NULL,
    type TEXT NOT NULL,
    title TEXT NOT NULL,
    priority INTEGER,
    path TEXT NOT NULL UNIQUE,
    created TEXT NOT NULL,
    updated TEXT NOT NULL,
    tags TEXT NOT NULL,
    owners TEXT NOT NULL,
    file_hash TEXT NOT NULL,
    file_stamp TEXT NOT NULL,
    row_hash TEXT NOT NULL
);
CREATE TABLE edges (
    src INTEGER NOT NULL REFERENCES nodes(rowid) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    dst TEXT NOT NULL
);
CREATE INDEX edges_src ON edges(src);
CREATE INDEX edges_dst ON edges(dst);
CREATE TABLE derived (
    provider TEXT NOT NULL,
    qid TEXT NOT NULL,
    row_hash TEXT NOT NULL,
    output BLOB NOT NULL,
    PRIMARY KEY (provider, qid)
);
CREATE VIRTUAL TABLE fts USING fts5(
    id, title, fields, body,
    tokenize = 'unicode61 remove_diacritics 2'
);
";

#[derive(Debug)]
pub enum IndexError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Sqlite(e) => write!(f, "derived index: {e}"),
            IndexError::Io(e) => write!(f, "derived index: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError::Sqlite(e)
    }
}

impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e)
    }
}

impl IndexError {
    /// A file SQLite can't read as a database: dropped and rebuilt.
    fn is_damaged(&self) -> bool {
        matches!(
            self,
            IndexError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
        )
    }
}

/// What a sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SyncStats {
    /// Nodes in the index after the sync.
    pub nodes: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// The file was damaged, or had another schema, and was rebuilt.
    pub rebuilt: bool,
}

/// What `index status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct IndexInfo {
    pub schema: i64,
    pub nodes: usize,
    pub edges: usize,
}

/// The derived index: a file beside the coordinator store, or memory when
/// there's no place to keep one.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
    /// Set when opening dropped a damaged file or an old schema.
    reset: bool,
}

/// The index file's name for a graph root: one file per worktree (and per
/// graph inside it), named after the root's folder and a hash of its path,
/// with the schema version.
pub fn index_file_name(root: &Path) -> String {
    let path = root.to_string_lossy();
    let digest = Digest::of_bytes(path.as_bytes());
    let folder: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("{folder}-{}.v{SCHEMA_VERSION}.sqlite", &digest.hex()[..16])
}

/// Removes an index file and its journal files. Only a `.sqlite` file is
/// ever removed, so a wrong path can't take anything else with it.
fn remove_index_files(path: &Path) -> Result<(), IndexError> {
    if path.extension().is_none_or(|e| e != "sqlite") {
        return Err(IndexError::Io(std::io::Error::other(format!(
            "refusing to remove {}: not an index file",
            path.display()
        ))));
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(PathBuf::from(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

impl Db {
    /// Opens (or creates) the index file at `path`. A damaged file is
    /// removed and created again; another schema's tables are dropped.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        match Self::try_open(path) {
            Err(e) if e.is_damaged() => {
                remove_index_files(path)?;
                let mut db = Self::try_open(path)?;
                db.reset = true;
                Ok(db)
            }
            other => other,
        }
    }

    fn try_open(path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // WAL lets readers go on while another process syncs.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut db = Db {
            conn,
            path: Some(path.to_path_buf()),
            reset: false,
        };
        db.ensure_schema()?;
        Ok(db)
    }

    /// An index in memory, for a graph with nowhere to keep a file.
    pub fn in_memory() -> Result<Self, IndexError> {
        let mut db = Db {
            conn: Connection::open_in_memory()?,
            path: None,
            reset: false,
        };
        db.ensure_schema()?;
        Ok(db)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Creates the schema, or replaces one that isn't exactly this build's:
    /// another `user_version`, or tables made from other SQL (a schema
    /// changed without a version bump), lose every table first.
    fn ensure_schema(&mut self) -> Result<(), IndexError> {
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        let fingerprint = Digest::of_bytes(SCHEMA.as_bytes()).to_string();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let stored: Option<String> = if version == SCHEMA_VERSION {
            tx.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap_or(None)
        } else {
            None
        };
        if stored.as_deref() != Some(fingerprint.as_str()) {
            let tables = |virtual_only: bool| -> Result<Vec<String>, rusqlite::Error> {
                let mut stmt = tx.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' AND (?1 = 0 OR sql LIKE 'CREATE VIRTUAL%')",
                )?;
                stmt.query_map([virtual_only], |r| r.get(0))?.collect()
            };
            // Virtual tables first: their shadow tables go with them.
            let found = tables(true)?.into_iter().chain(tables(false)?);
            let mut dropped = false;
            for table in found.collect::<Vec<_>>() {
                let quoted = table.replace('"', "\"\"");
                tx.execute_batch(&format!("DROP TABLE IF EXISTS \"{quoted}\";"))?;
                dropped = true;
            }
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema', ?1)",
                [&fingerprint],
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            self.reset |= dropped;
        }
        tx.commit()?;
        Ok(())
    }

    /// Opens the file at `path`, syncs it with `index` and runs `read` on
    /// the result, as [`Db::sync_then`] does. A file found damaged on the
    /// way is removed and built again, once.
    pub fn open_synced_then<T>(
        path: &Path,
        root: &Path,
        index: &Index,
        mut read: impl FnMut(&Db) -> Result<T, IndexError>,
    ) -> Result<(Self, SyncStats, T), IndexError> {
        let mut attempt = || -> Result<(Self, SyncStats, T), IndexError> {
            let mut db = Self::open(path)?;
            let (stats, value) = db.sync_then(root, index, &mut read)?;
            Ok((db, stats, value))
        };
        match attempt() {
            Err(e) if e.is_damaged() => {
                remove_index_files(path)?;
                let (db, mut stats, value) = attempt()?;
                stats.rebuilt = true;
                Ok((db, stats, value))
            }
            other => other,
        }
    }

    /// [`Db::open_synced_then`] with nothing to read.
    pub fn open_synced(
        path: &Path,
        root: &Path,
        index: &Index,
    ) -> Result<(Self, SyncStats), IndexError> {
        Self::open_synced_then(path, root, index, |_| Ok(())).map(|(db, stats, ())| (db, stats))
    }

    /// Runs `f` in one write transaction: committed when it succeeds,
    /// rolled back when it fails, so a crash or an error leaves the index
    /// as it was. Other processes wait for it (and it for them).
    pub(crate) fn write_tx<T>(
        &self,
        f: impl FnOnce(&Db) -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let done = f(self).and_then(|value| {
            self.conn.execute_batch("COMMIT")?;
            Ok(value)
        });
        if done.is_err() && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        done
    }

    /// Brings the index up to date with `index`, the graph as just read
    /// from `root`, then runs `read` in the same transaction: what `read`
    /// sees is exactly `index`'s graph, whatever another process syncs or
    /// rebuilds meanwhile. A node's row is replaced when its file's hash
    /// or what the index keeps of it changed; rows for nodes that are gone
    /// are removed.
    pub fn sync_then<T>(
        &mut self,
        root: &Path,
        index: &Index,
        read: impl FnOnce(&Db) -> Result<T, IndexError>,
    ) -> Result<(SyncStats, T), IndexError> {
        let reset = self.reset;
        let done = self.write_tx(|db| {
            let mut stats = db.sync_rows(root, index)?;
            stats.rebuilt = reset;
            Ok((stats, read(db)?))
        })?;
        self.reset = false;
        Ok(done)
    }

    /// [`Db::sync_then`] with nothing to read.
    pub fn sync(&mut self, root: &Path, index: &Index) -> Result<SyncStats, IndexError> {
        self.sync_then(root, index, |_| Ok(()))
            .map(|(stats, ())| stats)
    }

    /// Empties the index and adds every node again, in one transaction, so
    /// no reader ever sees it empty.
    pub fn rebuild(&mut self, root: &Path, index: &Index) -> Result<SyncStats, IndexError> {
        let mut stats = self.write_tx(|db| {
            db.conn.execute_batch(
                "DELETE FROM derived; DELETE FROM edges; DELETE FROM fts; DELETE FROM nodes;",
            )?;
            db.sync_rows(root, index)
        })?;
        stats.rebuilt = true;
        self.reset = false;
        Ok(stats)
    }

    /// The sync itself, inside the caller's transaction.
    fn sync_rows(&self, root: &Path, index: &Index) -> Result<SyncStats, IndexError> {
        let tx = &self.conn;
        let mut existing: HashMap<String, Existing> = HashMap::new();
        {
            let mut stmt =
                tx.prepare("SELECT path, rowid, row_hash, file_hash, file_stamp FROM nodes")?;
            let found = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Existing {
                        rowid: r.get(1)?,
                        row_hash: r.get(2)?,
                        file_hash: r.get(3)?,
                        file_stamp: r.get(4)?,
                    },
                ))
            })?;
            for row in found {
                let (path, entry) = row?;
                existing.insert(path, entry);
            }
        }
        let rows: Vec<Row> = index
            .nodes
            .iter()
            .map(|n| Row::of(root, index, n, existing.get(&n.path)))
            .collect();
        let mut stats = SyncStats::default();
        let mut stale: Vec<i64> = Vec::new();
        let mut fresh: Vec<&Row> = Vec::new();
        // Unchanged rows whose file was touched: the new stamp is kept, so
        // the next sync doesn't hash the file again.
        let mut restamp: Vec<(i64, &str)> = Vec::new();
        for row in &rows {
            match existing.remove(&row.node.path) {
                Some(e) if e.row_hash == row.row_hash => {
                    stats.unchanged += 1;
                    if e.file_stamp != row.file_stamp {
                        restamp.push((e.rowid, &row.file_stamp));
                    }
                }
                Some(e) => {
                    stale.push(e.rowid);
                    fresh.push(row);
                    stats.updated += 1;
                }
                None => {
                    fresh.push(row);
                    stats.added += 1;
                }
            }
        }
        stats.removed = existing.len();
        stale.extend(existing.into_values().map(|e| e.rowid));
        {
            let mut stamp = tx.prepare("UPDATE nodes SET file_stamp = ?2 WHERE rowid = ?1")?;
            for (rowid, s) in restamp {
                stamp.execute(params![rowid, s])?;
            }
        }
        {
            let mut del_fts = tx.prepare("DELETE FROM fts WHERE rowid = ?1")?;
            let mut del_node = tx.prepare("DELETE FROM nodes WHERE rowid = ?1")?;
            for rowid in &stale {
                del_fts.execute([rowid])?;
                // Edges go with the node (ON DELETE CASCADE).
                del_node.execute([rowid])?;
            }
        }
        {
            let mut ins_node = tx.prepare(
                "INSERT INTO nodes (qid, id, ws, type, title, priority, path, created, \
                 updated, tags, owners, file_hash, file_stamp, row_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            let mut ins_edge =
                tx.prepare("INSERT INTO edges (src, kind, dst) VALUES (?1, ?2, ?3)")?;
            let mut ins_fts = tx.prepare(
                "INSERT INTO fts (rowid, id, title, fields, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for row in fresh {
                let n = row.node;
                ins_node.execute(params![
                    n.qid,
                    n.id,
                    n.ws,
                    n.node_type,
                    n.title,
                    n.priority,
                    n.path,
                    n.created,
                    n.updated,
                    row.tags,
                    row.owners,
                    row.file_hash,
                    row.file_stamp,
                    row.row_hash,
                ])?;
                let rowid = tx.last_insert_rowid();
                for (kind, dst) in &row.edges {
                    ins_edge.execute(params![rowid, kind, dst])?;
                }
                ins_fts.execute(params![rowid, n.id, n.title, row.fields, n.body])?;
            }
        }
        stats.nodes =
            tx.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))? as usize;
        Ok(stats)
    }

    pub fn info(&self) -> Result<IndexInfo, IndexError> {
        let count = |sql: &str| -> Result<usize, IndexError> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as usize)
        };
        Ok(IndexInfo {
            schema: SCHEMA_VERSION,
            nodes: count("SELECT count(*) FROM nodes")?,
            edges: count("SELECT count(*) FROM edges")?,
        })
    }

    /// The hash of the file a node was indexed from, for providers that
    /// key their outputs by content.
    pub fn file_hash(&self, qid: &str) -> Result<Option<String>, IndexError> {
        Ok(self
            .conn
            .query_row("SELECT file_hash FROM nodes WHERE qid = ?1", [qid], |r| {
                r.get(0)
            })
            .optional()?)
    }
}

/// A node's row as the index holds it.
struct Existing {
    rowid: i64,
    row_hash: String,
    file_hash: String,
    file_stamp: String,
}

/// A file's size and modification time: while they stay the same, its
/// hash isn't computed again.
fn stamp(path: &Path) -> String {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| {
            let mtime = m
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            Some(format!("{}:{}", m.len(), mtime.as_nanos()))
        })
        .unwrap_or_default()
}

/// What the index keeps of one node.
struct Row<'a> {
    node: &'a IndexNode,
    /// Every frontmatter value but the title, as searchable text.
    fields: String,
    tags: String,
    owners: String,
    edges: Vec<(&'static str, String)>,
    /// The file's sha256, for providers that key outputs by content.
    file_hash: String,
    file_stamp: String,
    /// A hash of everything above (but the stamp) and the node's own
    /// values: the row is replaced exactly when it changes.
    row_hash: String,
}

impl<'a> Row<'a> {
    fn of(root: &Path, index: &Index, node: &'a IndexNode, before: Option<&Existing>) -> Self {
        let file = root.join(&node.path);
        let file_stamp = stamp(&file);
        // The build just read this file. One whose stamp can't be read is
        // hashed every time; one that can't be read now gets no hash, so
        // its row is replaced on the next sync that reads it.
        let file_hash = match before {
            Some(e) if !file_stamp.is_empty() && e.file_stamp == file_stamp => e.file_hash.clone(),
            _ => std::fs::read(&file)
                .map(|bytes| Digest::of_bytes(&bytes).to_string())
                .unwrap_or_default(),
        };
        let mut edges: Vec<(&'static str, String)> = edge_targets(&node.edges)
            .into_iter()
            .map(|(k, v)| (k, v.to_string()))
            .collect();
        edges.extend(crate::graph::ref_targets(index, node).map(|p| ("refs", p.qid)));
        edges.extend(
            crate::graph::links(index, node)
                .into_iter()
                .map(|p| ("link", p.qid)),
        );
        let fields = fields_text(node);
        let tags = serde_json::to_string(&node.tags).unwrap_or_default();
        let owners = serde_json::to_string(&node.owners).unwrap_or_default();
        // Change detection only, so a fast hash: SipHash with fixed keys.
        // If a Rust release changes it, every row is replaced once.
        let mut h = std::hash::DefaultHasher::new();
        for part in [
            node.qid.as_str(),
            &node.id,
            &node.ws,
            &node.node_type,
            &node.title,
            &node.priority.map(|p| p.to_string()).unwrap_or_default(),
            &node.path,
            &node.created,
            &node.updated,
            &tags,
            &owners,
            &fields,
            &node.body,
            &file_hash,
        ] {
            h.write(part.as_bytes());
            h.write_u8(0x1f);
        }
        for (kind, dst) in &edges {
            h.write(kind.as_bytes());
            h.write_u8(0x1e);
            h.write(dst.as_bytes());
            h.write_u8(0x1f);
        }
        let row_hash = format!("sip:{:016x}", h.finish());
        Row {
            node,
            fields,
            tags,
            owners,
            edges,
            file_hash,
            file_stamp,
            row_hash,
        }
    }
}

/// The searchable text of a node's fields: its type, lists, edges and
/// other frontmatter values, keys included. Not its path (the file name
/// repeats the title), and not its status: commands lay the coordinator's
/// states over the file's, so the index keeps no status at all, and
/// `--status` filters on the effective one.
fn fields_text(node: &IndexNode) -> String {
    let mut parts: Vec<&str> = vec![&node.node_type];
    for list in [
        &node.tags,
        &node.owners,
        &node.links,
        &node.artifacts,
        &node.refs,
        &node.aliases,
        &node.skills,
        &node.scope,
    ] {
        parts.extend(list.iter().map(String::as_str));
    }
    let mut owned: Vec<String> = Vec::new();
    for (key, value) in node.attributes.entries() {
        owned.push(key.clone());
        match value {
            FmValue::List(items) => owned.extend(items.iter().cloned()),
            FmValue::String(s) => owned.push(s.clone()),
            FmValue::Bool(b) => owned.push(b.to_string()),
        }
    }
    let edges = edge_targets(&node.edges);
    parts.extend(edges.iter().map(|(_, v)| *v));
    parts.extend(owned.iter().map(String::as_str));
    parts.join(" ")
}
