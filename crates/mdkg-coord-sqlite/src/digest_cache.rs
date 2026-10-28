//! Content hashes of blobs, remembered by object id, so reading main hashes
//! only the blobs that changed since the last read.
//!
//! A blob's object id names its content, so the `sha256:` of a given id
//! never changes: a hit gives exactly the hash a fresh read would. The
//! cache is its own file beside the store, never part of it: the
//! coordinator never depends on it. Any failure to open, read or write it
//! (missing, locked, corrupt, read only) only means hashing those blobs
//! again, and deleting the file is always safe. Each row also keeps the
//! blob's size, checked against git's listing, and a checksum over the
//! whole row, so a damaged row (a flipped bit included) is ignored rather
//! than trusted. Rows are pruned to the last listing.
//!
//! A hit is trusted as it stands, so whoever can write the cache can make
//! main read with any hash they like. That is no new power: the cache has
//! the store's folder, owner and mode, and whoever can write the store can
//! already append any observation to it, which `verify` accepts, since
//! records aren't signed yet (envelope.rs) and an anchor pins only the
//! chain up to its head. When records are signed, the cache must be
//! authenticated as strongly, or rehashed.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use mdkg_format::Digest;
use rusqlite::{Connection, OpenFlags, params};

/// The cache's file name, beside `coord.sqlite`.
pub(crate) const FILE: &str = "blob-digests.sqlite";

/// The table layout this build reads and writes (`PRAGMA user_version`).
/// An older layout is rebuilt; a newer one is left alone.
const VERSION: i64 = 2;

/// A reader never waits long for a writer: hashing again is the fallback.
const BUSY_WAIT: Duration = Duration::from_millis(200);

pub(crate) struct DigestCache {
    conn: Connection,
}

impl DigestCache {
    /// Opens or creates the cache at `path`. `None` when it can't be used:
    /// its folder is missing, it isn't a database, or it has a newer layout
    /// than this build's (left untouched).
    pub(crate) fn open(path: &Path) -> Option<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).ok()?;
        conn.busy_timeout(BUSY_WAIT).ok()?;
        // A crash mid-write must leave either the old rows or the new ones.
        conn.pragma_update(None, "synchronous", "FULL").ok()?;
        conn.pragma_update(None, "fullfsync", true).ok()?;
        match version(&conn)? {
            VERSION => return Some(Self { conn }),
            v if v > VERSION => return None,
            _ => {}
        }
        // Created or rebuilt under the write lock, judged again there, so a
        // newer build's layout written in between is never dropped.
        let mut conn = conn;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .ok()?;
        match version(&tx)? {
            VERSION => {}
            v if v > VERSION => return None,
            _ => tx
                .execute_batch(&format!(
                    "DROP TABLE IF EXISTS digests;
                     CREATE TABLE digests (
                         oid TEXT PRIMARY KEY,
                         size INTEGER NOT NULL,
                         digest TEXT NOT NULL,
                         checksum TEXT NOT NULL
                     ) WITHOUT ROWID;
                     PRAGMA user_version = {VERSION};"
                ))
                .ok()?,
        }
        tx.commit().ok()?;
        Some(Self { conn })
    }

    /// The remembered hashes of `wanted` (object id to size), and how many
    /// rows the cache holds in all. A row whose size, hash or checksum
    /// doesn't check out is left out, so its blob is hashed again. Read in
    /// one transaction with the layout, which must still be this build's.
    pub(crate) fn lookup(
        &self,
        wanted: &BTreeMap<&str, u64>,
    ) -> Option<(HashMap<String, String>, usize)> {
        let tx = self.conn.unchecked_transaction().ok()?;
        if version(&tx)? != VERSION {
            return None;
        }
        let mut stmt = tx
            .prepare("SELECT oid, size, digest, checksum FROM digests")
            .ok()?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .ok()?;
        let mut hits = HashMap::new();
        let mut total = 0;
        for row in rows {
            let (oid, size, digest, sum) = row.ok()?;
            total += 1;
            let fits = wanted
                .get(oid.as_str())
                .is_some_and(|w| i64::try_from(*w) == Ok(size));
            if fits && Digest::parse(&digest).is_some() && sum == checksum(&oid, size, &digest) {
                hits.insert(oid, digest);
            }
        }
        drop(stmt);
        tx.finish().ok()?;
        Some((hits, total))
    }

    /// Replaces the rows with exactly `rows` (object id to size and hash),
    /// in one transaction. Best effort: an error leaves the old rows, and
    /// a layout that changed since `open` (a newer build) is left alone.
    pub(crate) fn replace(&mut self, rows: &[(&str, u64, &str)]) {
        let mut write = || -> rusqlite::Result<()> {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if version(&tx) != Some(VERSION) {
                return Ok(());
            }
            tx.execute("DELETE FROM digests", [])?;
            {
                let mut insert = tx.prepare(
                    "INSERT INTO digests (oid, size, digest, checksum) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (oid, size, digest) in rows {
                    let size = i64::try_from(*size).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    insert.execute(params![oid, size, digest, checksum(oid, size, digest)])?;
                }
            }
            tx.commit()
        };
        // Nothing depends on the cache: a failed write only costs a rehash.
        let _ = write();
    }
}

/// The cache's layout (`PRAGMA user_version`).
fn version(c: &Connection) -> Option<i64> {
    c.query_row("PRAGMA user_version", [], |r| r.get(0)).ok()
}

/// A row's checksum: the `sha256:` of its fields, one per line.
fn checksum(oid: &str, size: i64, digest: &str) -> String {
    Digest::of_bytes(format!("{oid}\n{size}\n{digest}\n").as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newer build upgrades the layout between this build's lookup and
    /// its rewrite: the rewrite leaves the newer layout's rows alone.
    #[test]
    fn a_layout_upgraded_after_open_is_left_alone() {
        let dir = std::env::temp_dir().join(format!(
            "mdkg-digest-cache-upgrade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(FILE);
        let digest = Digest::of_bytes(b"a").to_string();
        let mut cache = DigestCache::open(&path).expect("open");
        cache.replace(&[("a1", 1, digest.as_str())]);
        let wanted = BTreeMap::from([("a1", 1u64)]);
        assert_eq!(cache.lookup(&wanted).expect("lookup").0.len(), 1);

        let newer = Connection::open(&path).expect("newer");
        newer
            .execute_batch(
                "ALTER TABLE digests ADD COLUMN note TEXT NOT NULL DEFAULT 'kept';
                 UPDATE digests SET note = 'newer';
                 PRAGMA user_version = 3;",
            )
            .expect("upgrade");
        cache.replace(&[("b2", 2, digest.as_str())]);
        let rows: Vec<(String, String)> = newer
            .prepare("SELECT oid, note FROM digests")
            .expect("select")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(rows, vec![("a1".to_string(), "newer".to_string())]);
        assert!(cache.lookup(&wanted).is_none(), "nor read as this build's");
        drop(newer);
        std::fs::remove_dir_all(&dir).ok();
    }
}
