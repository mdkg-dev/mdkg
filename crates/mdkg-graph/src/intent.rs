//! The intent journal: `<common>/mdkg/graph/intents/<request-id>.json`
//! (edd-01a18867, "Worktree-free writes" steps 5 to 8).
//!
//! A writer records an intent after it prepares a commit and before the
//! store records the request's changesets. Replay, under the exclusive
//! graph lock, asks the store whether it recorded the prepared commit:
//! - recorded: publish it (no lease needed; the store's record is the
//!   reservation), then drop the intent and the pending ref;
//! - not recorded: the op never succeeded, so drop both;
//! - stale (a touched path changed since): keep both, and report it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::refwrite::{self, Change, GraphLock, Prepared, Receipt, RefWriter, Request, Snapshot};

/// The largest intent the journal writes or reads.
pub const MAX_INTENT_BYTES: u64 = 4 * 1024 * 1024;

/// The largest remembered request (its payload hex-encoded, so a file at
/// the cache's 64 MiB limit fits, Terra #9366).
pub const MAX_REQUEST_BYTES: u64 = 256 * 1024 * 1024;

/// One prepared write, as journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub prepared: Prepared,
    /// The op's one-line summary (the commit subject's `<op> <node>`).
    pub message: String,
    pub actor: String,
    /// Every lease of the request, as the store names them.
    pub leases: Vec<String>,
    /// The op (`new`, `update`, `done`, ...).
    pub op: String,
}

impl Intent {
    pub fn id(&self) -> &str {
        &self.prepared.id
    }

    fn to_json(&self) -> Value {
        let p = &self.prepared;
        json!({
            "v": 1,
            "id": p.id,
            "base": p.base,
            "parent": p.parent,
            "prepared": p.commit,
            "tree": p.tree,
            "changes": p.paths,
            "preimages": p.preimages,
            "fingerprint": p.fingerprint,
            "message": self.message,
            "actor": self.actor,
            "leases": self.leases,
            "op": self.op,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let opt = |k: &str| match v.get(k) {
            Some(Value::Null) | None => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            _ => None,
        };
        let list = |k: &str| -> Option<Vec<String>> {
            v.get(k)?
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_owned))
                .collect()
        };
        if v.get("v").and_then(Value::as_u64) != Some(1) {
            return None;
        }
        Some(Self {
            prepared: Prepared {
                id: s("id")?,
                base: opt("base")?,
                parent: opt("parent")?,
                commit: s("prepared")?,
                tree: s("tree")?,
                preimages: match v.get("preimages") {
                    None => list("changes")?,
                    Some(_) => list("preimages")?,
                },
                paths: list("changes")?,
                fingerprint: s("fingerprint")?,
            },
            message: s("message")?,
            actor: s("actor")?,
            leases: list("leases")?,
            op: s("op")?,
        })
    }
}

/// What replay did with one intent.
#[derive(Debug)]
pub enum Replayed {
    /// The store recorded it, and it's published now (or already was).
    Published { id: String, receipt: Receipt },
    /// The store didn't record it: the intent and pending ref are gone.
    Discarded { id: String },
    /// A touched path changed on the graph since: kept, `graph.intent_stale`.
    Stale { id: String, path: String },
    /// A pending ref with no intent was dropped.
    Unpinned { id: String },
    /// Replay failed this time; the intent stays for the next command.
    Failed { id: String, error: String },
}

/// The journal folder of one clone.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
    /// The canonical common folder the journal belongs to.
    common: PathBuf,
}

impl Journal {
    /// `<common>/mdkg/graph/intents`, created private when missing.
    pub fn open(common: &Path) -> io::Result<Self> {
        let graph = refwrite::graph_dir(common)?;
        let dir = graph.join("intents");
        refwrite::private_dir(&dir, &graph)?;
        Ok(Self {
            dir,
            common: common.canonicalize()?,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes an intent durably: a temporary file, fsynced, renamed, and
    /// the folder fsynced.
    pub fn record(&self, intent: &Intent) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&intent.to_json()).map_err(io::Error::other)?;
        // Never write what `load` can't read back.
        if bytes.len() as u64 > MAX_INTENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the intent is over the journal's 4 MiB limit: split the request",
            ));
        }
        write_durably(&self.dir, &file_name(intent.id())?, &bytes)
    }

    /// One intent by request id.
    pub fn load(&self, id: &str) -> io::Result<Option<Intent>> {
        let path = self.dir.join(file_name(id)?);
        let bytes = match mdkg_git::read_capped(&path, MAX_INTENT_BYTES + 1) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if bytes.len() as u64 > MAX_INTENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("intent {id} is over 4 MiB"),
            ));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Intent::from_json(&value)
            .filter(|i| i.id() == id)
            .map(Some)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("intent {id} is malformed"),
                )
            })
    }

    /// Every pending intent, oldest request id first.
    pub fn pending(&self) -> io::Result<Vec<Intent>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_suffix(".json")
                && !name.starts_with('.')
            {
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        let mut out = Vec::new();
        for id in ids {
            if let Some(intent) = self.load(&id)? {
                out.push(intent);
            }
        }
        Ok(out)
    }

    /// Remembers a request whole, base and payload, under its id
    /// (orchestrator #8013), so a retry by id (a CLI `--request-id`, an MCP
    /// call id) resends exactly the original and never refreshes its base.
    /// Remembering the same request again is fine; a different request
    /// under a remembered id is refused (`graph.request_reused`).
    pub fn remember(&self, request: &Request) -> refwrite::Result<()> {
        let dir = self.requests()?;
        let name = file_name(&request.id)?;
        // A request already remembered is compared and made durable
        // without writing anything new (Terra #10216): a full disk never
        // fails a valid retry.
        if let Some(had) = self.request(&request.id)? {
            if had != *request {
                return Err(refwrite::Error::RequestIdReused(request.id.clone()));
            }
            File::open(dir.join(&name))?.sync_all()?;
            sync_dir(&dir)?;
            return Ok(());
        }
        let bytes = serde_json::to_vec(&request_json(request)).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_REQUEST_BYTES {
            return Err(refwrite::Error::Invalid(
                "the request is over the journal's 256 MiB limit: split it".into(),
            ));
        }
        // Created only where none exists (Terra #9366): a synced temporary
        // file hard-linked into place, so two callers can't both win. Only
        // the link's AlreadyExists means another caller won (Claude #10147).
        let (tmp, mut f) = temporary(&dir, &name)?;
        let linked = f
            .write_all(&bytes)
            .and_then(|()| f.sync_all())
            .and_then(|()| fs::hard_link(&tmp, dir.join(&name)));
        drop(f);
        fs::remove_file(&tmp).ok();
        match linked {
            Ok(()) => sync_dir(&dir)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let had = self.request(&request.id)?;
                if had.as_ref() != Some(request) {
                    return Err(refwrite::Error::RequestIdReused(request.id.clone()));
                }
                // Relied on only once durable: an earlier folder sync may
                // have failed (Terra #9366).
                File::open(dir.join(&name))?.sync_all()?;
                sync_dir(&dir)?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// A remembered request, by id.
    pub fn request(&self, id: &str) -> io::Result<Option<Request>> {
        let path = self.requests()?.join(file_name(id)?);
        let bytes = match mdkg_git::read_capped(&path, MAX_REQUEST_BYTES + 1) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        request_from_json(&value)
            .filter(|r| r.id == id)
            .map(Some)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("remembered request {id} is malformed"),
                )
            })
    }

    /// Forgets a remembered request; an absent one is fine.
    pub fn forget(&self, id: &str) -> io::Result<()> {
        let dir = self.requests()?;
        match fs::remove_file(dir.join(file_name(id)?)) {
            Ok(()) => sync_dir(&dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `<common>/mdkg/graph/requests`, created private.
    fn requests(&self) -> io::Result<PathBuf> {
        let graph = refwrite::graph_dir(&self.common)?;
        let dir = graph.join("requests");
        refwrite::private_dir(&dir, &graph)?;
        Ok(dir)
    }

    /// Removes an intent; an absent one is fine.
    pub fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.dir.join(file_name(id)?)) {
            Ok(()) => sync_dir(&self.dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Finishes a write the caller just published: drops the pending ref,
    /// then the intent.
    pub fn finish(
        &self,
        writer: &RefWriter,
        lock: &GraphLock,
        intent: &Intent,
    ) -> refwrite::Result<()> {
        writer.unpin(lock, &intent.prepared)?;
        self.remove(intent.id())?;
        Ok(())
    }

    /// Replays every pending intent under `lock`. `recorded` asks the store
    /// whether it holds changesets whose source is the prepared commit.
    pub fn replay<E: std::fmt::Display>(
        &self,
        writer: &RefWriter,
        lock: &GraphLock,
        mut recorded: impl FnMut(&Intent) -> Result<bool, E>,
    ) -> refwrite::Result<Vec<Replayed>> {
        // The journal, the writer and the lock must all be one clone's, so
        // a replay never asks another clone's store about these intents.
        lock.require_exclusive(&self.common, "intent replay")?;
        if writer.common().canonicalize()? != self.common {
            return Err(refwrite::Error::Invalid(
                "intent replay: the writer belongs to another clone than the journal".into(),
            ));
        }
        let mut out = Vec::new();
        let intents = self.pending()?;
        for intent in &intents {
            out.push(self.replay_one(writer, lock, intent, &mut recorded));
        }
        // A pin with no intent: a crash between the pin and the intent
        // write, or after a lockless publish and before its unpin. The
        // store never recorded it (the intent comes first), so it goes.
        for (id, commit) in writer.pending()? {
            if !intents.iter().any(|i| i.id() == id) {
                if writer.unpin_orphan(lock, &id, &commit)? {
                    out.push(Replayed::Unpinned { id });
                } else {
                    out.push(Replayed::Failed {
                        id,
                        error: "kept: its receipt names a commit no longer on the graph, so the request counts as unpublished".into(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn replay_one<E: std::fmt::Display>(
        &self,
        writer: &RefWriter,
        lock: &GraphLock,
        intent: &Intent,
        recorded: &mut impl FnMut(&Intent) -> Result<bool, E>,
    ) -> Replayed {
        let id = intent.id().to_owned();
        let failed = |error: String| Replayed::Failed {
            id: id.clone(),
            error,
        };
        match recorded(intent) {
            Err(e) => failed(e.to_string()),
            Ok(false) => match self.finish(writer, lock, intent) {
                Ok(()) => Replayed::Discarded { id },
                Err(e) => failed(e.to_string()),
            },
            Ok(true) => match writer.publish(lock, &intent.prepared) {
                Ok(receipt) => match self.finish(writer, lock, intent) {
                    Ok(()) => Replayed::Published { id, receipt },
                    Err(e) => failed(e.to_string()),
                },
                Err(refwrite::Error::Stale { path }) => Replayed::Stale { id, path },
                Err(e) => failed(e.to_string()),
            },
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn request_json(r: &Request) -> Value {
    let changes: Vec<Value> = r
        .changes
        .iter()
        .map(|c| match c {
            Change::Put { path, bytes } => json!({"path": path, "put": hex(bytes)}),
            Change::Delete { path } => json!({"path": path, "delete": true}),
        })
        .collect();
    json!({
        "v": 1,
        "id": r.id,
        "base": r.base.as_ref().map(Snapshot::commit),
        "changes": changes,
        "message": r.message,
        "trailers": r.trailers,
    })
}

fn request_from_json(v: &Value) -> Option<Request> {
    if v.get("v")?.as_u64()? != 1 {
        return None;
    }
    let base = match v.get("base")? {
        Value::Null => None,
        Value::String(c) => Some(Snapshot::recorded(c.clone())),
        _ => return None,
    };
    let changes = v
        .get("changes")?
        .as_array()?
        .iter()
        .map(|c| {
            let path = c.get("path")?.as_str()?.to_owned();
            match (c.get("put"), c.get("delete")) {
                (Some(Value::String(h)), None) => Some(Change::Put {
                    path,
                    bytes: unhex(h)?,
                }),
                (None, Some(Value::Bool(true))) => Some(Change::Delete { path }),
                _ => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let trailers = v
        .get("trailers")?
        .as_array()?
        .iter()
        .map(|t| {
            let pair = t.as_array()?;
            Some((
                pair.first()?.as_str()?.to_owned(),
                pair.get(1)?.as_str()?.to_owned(),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Request {
        id: v.get("id")?.as_str()?.to_owned(),
        base,
        changes,
        message: v.get("message")?.as_str()?.to_owned(),
        trailers,
    })
}

fn file_name(id: &str) -> io::Result<String> {
    if id.is_empty()
        || id.len() > 120
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "request ID must be 1..120 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(format!("{id}.json"))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Replaces `dir/name` whole and durably: a private temporary file,
/// fsynced, renamed over it, then the folder fsynced.
pub(crate) fn write_durably(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    // Only a temporary file this call created is ever removed (Terra
    // #8377).
    let (tmp, mut f) = temporary(dir, name)?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(name))?;
        sync_dir(dir)
    })();
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

/// The next temporary file's number in this process.
pub(crate) static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A new temporary file for `name` in `dir`, `.<name>.<pid>-<n>.tmp`,
/// created exclusively (Claude #10147, Opus #10313): a name a crashed
/// process left (its pid reused) is skipped for the next, up to 1024 tries
/// per call, and this target's own leftovers over an hour old, far longer
/// than any write holds one, are swept first.
fn temporary(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    sweep_temporaries(dir, name);
    for _ in 0..1024 {
        let n = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}-{n}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free temporary name for {name} in {}", dir.display()),
    ))
}

/// Removes `name`'s temporaries in `dir` (exactly `.<name>.<digits>-<digits>.tmp`)
/// over an hour old: what a crashed write left. Best effort; a fresh
/// file, such as a rescue copy, is never one of them.
fn sweep_temporaries(dir: &Path, target: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!(".{target}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".tmp"))
            .and_then(|r| r.split_once('-'))
            .is_some_and(|(pid, n)| {
                !pid.is_empty()
                    && !n.is_empty()
                    && pid.bytes().all(|b| b.is_ascii_digit())
                    && n.bytes().all(|b| b.is_ascii_digit())
            });
        if !ours {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(3600));
        if old {
            fs::remove_file(entry.path()).ok();
        }
    }
}
