//! `node edit` and `node save` (edd-01a18867, "Editing a node body"): in
//! ref mode the graph's files live in the read-only cache, so a node body
//! is edited through a writable copy outside `.git` and published through
//! the write path.
//!
//! - **Where.** `--dir`, else `<worktree>/.mdkg-edit/<graph path>`; never
//!   the launcher's turn scratch, which is under `.git` (Opus #6237).
//! - **Held folders** (Terra #6377, #6455). The copy's folder is reached
//!   by opening each folder relative to the one before, without following
//!   a link, and every read, write, rename and removal of the copy is made
//!   relative to that held folder. A link anywhere is refused
//!   (`graph.edit_path_unsafe`), and a folder swapped for a link after it
//!   was opened changes nothing: the held handle still names the original.
//! - **A trusted record, not a sidecar** (Terra #6455). The preimage lives
//!   in an mdkg-private record under the common git folder
//!   (`mdkg/graph/edits/`), never beside the copy, so nothing in a checkout
//!   can redirect or rewrite it. It also records the blob mdkg wrote to the
//!   copy. The copy is written first and the record second: a crash
//!   between them leaves a copy that no longer matches the record, which is
//!   kept with the old preimage, and its save is refused as stale (it
//!   fails closed).
//! - **Never overwritten.** A rerun refreshes a copy still holding what
//!   mdkg wrote, keeps a modified one (`graph.edit_copy_kept`), and
//!   `--discard` starts over.
//! - **The exclusion.** `mdkg install` writes `/.mdkg-edit/` to the common
//!   `info/exclude`; `node edit` adds it only when missing and writable,
//!   and otherwise warns, naming the exposure.
//! - **Save.** `node save` refuses a copy whose preimage is no longer the
//!   graph's blob (`graph.write_stale`), then publishes under the node's
//!   lease (prepare, record, publish) and removes the copy. Content passed
//!   directly (MCP's `node_save`) carries its expected preimage, or uses
//!   the record of the actor's `node edit`; with neither it's refused.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use clap::Subcommand;
use mdkg_coord::{Changes, Command, Mode, Outcome, Source};
use mdkg_coord_sqlite::Store;
use mdkg_format::{Code, Diagnostic, Digest};
use mdkg_graph::{GraphError, IndexNode};
use rustix::fs::{AtFlags, FileType, OFlags};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::coord::graph_git::{
    self, Publish, RefMode, Wrote, graph_error, graph_warning, prepare_record_publish,
};
use crate::coord::{self, Repo, git_error, store_error};

/// The default copy folder in a worktree.
pub(crate) const EDIT_DIR: &str = ".mdkg-edit";
/// The line the common `info/exclude` holds for it.
pub(crate) const EXCLUDE_LINE: &str = "/.mdkg-edit/";

/// What `node edit` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edited {
    /// The writable copy.
    pub(crate) copy: PathBuf,
    /// The preimage blob the copy is based on (pass it to a content save),
    /// `None` for a kept copy mdkg has no record of.
    pub(crate) preimage: Option<String>,
    /// A modified copy was kept.
    pub(crate) kept: bool,
    /// The node's lease.
    pub(crate) lease: String,
}

/// What `node save` published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Saved {
    /// The published commit, `None` when the copy changed nothing.
    pub(crate) commit: Option<String>,
}

fn unsafe_path(at: &Path) -> GraphError {
    graph_error(
        "graph.edit_path_unsafe",
        format!(
            "{} is a link or not a folder, so the node's copy can't go there",
            at.display()
        ),
    )
    .with_hint("remove the link, or pass `--dir <folder>`")
}

fn io(e: io::Error) -> GraphError {
    GraphError::failure(Code::IoWriteFailed, e.to_string())
}

/// A folder held open: every operation on its entries is relative to the
/// handle, so the path it was reached by is never resolved again.
#[derive(Debug)]
struct Dir {
    fd: OwnedFd,
    shown: PathBuf,
}

const DIR_FLAGS: OFlags = OFlags::DIRECTORY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::RDONLY)
    .union(OFlags::CLOEXEC);

impl Dir {
    /// Opens `path` itself as a folder, refusing a link there.
    fn open(path: &Path) -> Result<Self, GraphError> {
        match rustix::fs::open(path, DIR_FLAGS, rustix::fs::Mode::empty()) {
            Ok(fd) => Ok(Self {
                fd,
                shown: path.to_path_buf(),
            }),
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => Err(unsafe_path(path)),
            Err(e) => Err(io(e.into())),
        }
    }

    /// The child folder `name`, created when missing, never through a link.
    fn child(&self, name: &str) -> Result<Self, GraphError> {
        let shown = self.shown.join(name);
        match rustix::fs::mkdirat(&self.fd, name, rustix::fs::Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(io(e.into())),
        }
        match rustix::fs::openat(&self.fd, name, DIR_FLAGS, rustix::fs::Mode::empty()) {
            Ok(fd) => Ok(Self { fd, shown }),
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => Err(unsafe_path(&shown)),
            Err(e) => Err(io(e.into())),
        }
    }

    /// A regular file's bytes, `None` when absent; a link or anything but
    /// a regular file is refused.
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, GraphError> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(&self.fd, name, flags, rustix::fs::Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(rustix::io::Errno::LOOP) => return Err(unsafe_path(&self.shown.join(name))),
            Err(e) => return Err(io(e.into())),
        };
        let stat = rustix::fs::fstat(&fd).map_err(|e| io(e.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(unsafe_path(&self.shown.join(name)));
        }
        let mut bytes = Vec::new();
        fs::File::from(fd).read_to_end(&mut bytes).map_err(io)?;
        Ok(Some(bytes))
    }

    /// Replaces `name` whole: a new private temporary file in this folder
    /// (exclusive, no-follow), synced, renamed over it, the folder synced.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), GraphError> {
        let tmp = format!(".mdkg-edit-{}.tmp", mdkg_format::Uuid7::now());
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(
            &self.fd,
            &tmp,
            flags,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(|e| io(e.into()))?;
        let result = (|| -> io::Result<()> {
            let mut f = fs::File::from(fd);
            f.write_all(bytes)?;
            f.sync_all()?;
            rustix::fs::renameat(&self.fd, &tmp, &self.fd, name)?;
            rustix::fs::fsync(&self.fd)?;
            Ok(())
        })();
        if result.is_err() {
            rustix::fs::unlinkat(&self.fd, &tmp, AtFlags::empty()).ok();
        }
        result.map_err(io)
    }

    fn remove(&self, name: &str) {
        rustix::fs::unlinkat(&self.fd, name, AtFlags::empty()).ok();
    }
}

fn ref_mode(repo: &Repo) -> Result<&RefMode, GraphError> {
    repo.mode.as_deref().ok_or_else(|| {
        GraphError::usage(
            Code::UsageInvalidArguments,
            "node edit is for a graph on the graph ref; this graph is a checkout's files",
        )
        .with_hint("edit the node's file in place")
    })
}

/// The copy's held folder and file name: under `--dir`, else under the
/// worktree's `.mdkg-edit/`, each folder opened relative to the last.
fn copy_dir(
    repo: &Repo,
    dir: Option<&Path>,
    graph_path: &str,
) -> Result<(Dir, String), GraphError> {
    let parts: Vec<&str> = Path::new(graph_path)
        .components()
        .map(|c| match c {
            Component::Normal(p) => p.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            graph_error(
                "graph.edit_path_unsafe",
                format!("{graph_path} isn't a plain graph path"),
            )
        })?;
    let mut at = match dir {
        Some(d) => Dir::open(d)?,
        None => Dir::open(&repo.top)?.child(EDIT_DIR)?,
    };
    for part in &parts[..parts.len() - 1] {
        at = at.child(part)?;
    }
    Ok((at, parts[parts.len() - 1].to_owned()))
}

/// The blob id of `bytes`, as git would store it.
fn blob_of(mode: &RefMode, bytes: &[u8]) -> Result<String, GraphError> {
    let out = graph_git::git::command(&mode.checkout)
        .args(["hash-object", "--no-filters", "--stdin"])
        .run(Some(bytes))
        .map_err(git_error)?;
    if !out.status.success() {
        return Err(GraphError::failure(
            Code::IoGitFailed,
            "git hash-object failed",
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The graph's blob for `path` at the cache's commit, `None` when absent.
fn graph_blob(mode: &RefMode, path: &str) -> Result<Option<String>, GraphError> {
    let out = graph_git::git::command(&mode.checkout)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{}:{path}", mode.commit()))
        .run(None)
        .map_err(git_error)?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// The trusted record of an actor's `node edit` of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    node: String,
    path: String,
    actor: String,
    /// Where the copy is.
    copy: String,
    /// The graph blob the copy is based on.
    preimage: String,
    /// The blob mdkg last wrote to the copy.
    written: String,
}

fn record_path(mode: &RefMode, node: &str, actor: &str) -> PathBuf {
    let key = Digest::of_bytes(format!("{node}\0{actor}").as_bytes()).to_string();
    let key = key.trim_start_matches("sha256:");
    mode.graph
        .common
        .join("mdkg/graph/edits")
        .join(format!("{}.json", &key[..32]))
}

fn load_record(mode: &RefMode, node: &str, actor: &str) -> Option<Record> {
    let bytes = fs::read(record_path(mode, node, actor)).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let s = |k: &str| v.get(k)?.as_str().map(str::to_owned);
    let r = Record {
        node: s("node")?,
        path: s("path")?,
        actor: s("actor")?,
        copy: s("copy")?,
        preimage: s("preimage")?,
        written: s("written")?,
    };
    (r.node == node && r.actor == actor).then_some(r)
}

fn save_record(mode: &RefMode, r: &Record) -> Result<(), GraphError> {
    let path = record_path(mode, &r.node, &r.actor);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io)?;
    }
    let v = json!({"v": 1, "node": r.node, "path": r.path, "actor": r.actor,
        "copy": r.copy, "preimage": r.preimage, "written": r.written});
    crate::write::atomic_write(&path, &serde_json::to_vec_pretty(&v).unwrap_or_default())
        .map_err(io)
}

fn drop_record(mode: &RefMode, node: &str, actor: &str) {
    fs::remove_file(record_path(mode, node, actor)).ok();
}

/// The commit an earlier `node save` of `node` under `request_id`
/// published, if any: the request's marker in the graph's history, on a
/// save of this node.
fn earlier_save(
    mode: &RefMode,
    request_id: &str,
    node: &str,
) -> Result<Option<String>, GraphError> {
    let Some(commit) = graph_git::published_request(mode, request_id)? else {
        return Ok(None);
    };
    let out = graph_git::git::command(&mode.checkout)
        .args(["log", "-1", "--format=%s"])
        .arg(&commit)
        .run(None)
        .map_err(git_error)?;
    let subject = String::from_utf8_lossy(&out.stdout);
    Ok(subject
        .starts_with(&format!("mdkg: save {node} ["))
        .then_some(commit))
}

/// A replay of `request_id`, which an earlier save published at `commit`
/// (R41): read-only, it returns that receipt and touches no lease, record
/// or copy (orchestrator's closing rule on Terra #16828). The receipt
/// stands only when it published these very bytes; otherwise the id was
/// reused for a different save, refused with nothing applied (Terra
/// #10293).
fn replayed(
    mode: &RefMode,
    request_id: &str,
    node: &IndexNode,
    commit: String,
    bytes: &[u8],
) -> Result<Saved, GraphError> {
    let published = graph_git::git::command(&mode.checkout)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{commit}:{}", node.path))
        .run(None)
        .map_err(git_error)?;
    let same = published.status.success()
        && String::from_utf8_lossy(&published.stdout).trim() == blob_of(mode, bytes)?;
    if !same {
        return Err(graph_error(
            "graph.request_reused",
            format!(
                "request {request_id} already published a different save of {}",
                node.id
            ),
        )
        .with_hint("your copy is kept: save it with a new request id"));
    }
    Ok(Saved {
        commit: Some(commit),
    })
}

/// Adds `/.mdkg-edit/` to the common `info/exclude` when it's missing and
/// the file can be written; otherwise a warning naming the exposure
/// (install provisions it).
fn exclude(mode: &RefMode) -> Option<Diagnostic> {
    // The one writer install and init use: git decides whether the rule
    // is effective, the file keeps its mode, and a read-only file is left
    // alone (with the warning below) rather than replaced (Terra #16445).
    let added =
        crate::run::setup::exclude(&mode.checkout, &mode.graph.common, EXCLUDE_LINE).map(drop);
    added.err().map(|e| {
        graph_warning(
            "graph.edit_unexcluded",
            format!(
                "couldn't add {EXCLUDE_LINE} to the clone's info/exclude ({}): without it, and \
                 without hooks:git's pre-commit guard, `git add -A` can put the copy in a code \
                 commit",
                e.diagnostic.message
            ),
        )
        .with_hint(format!(
            "run `{} install` once outside the sandbox; the copy still works",
            crate::BIN
        ))
    })
}

/// The node lease a command holds, whether this run acquired it
/// (`fresh`) rather than renewing one held before it started, and the
/// broader claim's scope when it is one covering the node rather than the
/// node's own lease.
struct Taken {
    id: String,
    fresh: bool,
    broader: Option<String>,
}

/// Takes (or renews) the actor's exclusive lease on the node's file. A
/// lease taken fresh is released again when the command then fails or a
/// save commits nothing ([`settle_lease`]), so neither leaves one behind
/// (e2e N2, Terra #16757); a lease held before is never released.
fn lease(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    node: &IndexNode,
) -> Result<Taken, GraphError> {
    let leaf = repo.leaf(&node.path)?;
    let key = repo.node_key(&leaf, &node.qid);
    let now = coord::now_ms();
    let state = store.coordinator().map_err(store_error)?;
    if let Some(l) = coord::covering_lease(&state, actor, &leaf).filter(|l| l.deadline > now) {
        let id = l.id.clone();
        let broader = (l.scope != leaf).then(|| l.scope.to_string());
        store
            .execute(&Command::Heartbeat {
                owner: actor.into(),
                lease: id.clone(),
                now,
            })
            .map_err(store_error)?;
        return Ok(Taken {
            id: id.to_string(),
            fresh: false,
            broader,
        });
    }
    match store
        .execute(&Command::Claim {
            owner: actor.into(),
            scope: leaf,
            mode: Mode::Exclusive,
            now,
            request: None,
            node: Some(key),
        })
        .map_err(store_error)?
    {
        Outcome::Lease { lease } => Ok(Taken {
            id: lease.to_string(),
            fresh: true,
            broader: None,
        }),
        Outcome::Refused { error } => Err(coord::refusal(&error)),
        other => Err(GraphError::failure(
            Code::CoordStoreFailed,
            format!("unexpected outcome {other:?}"),
        )),
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: the next lease release fails as a store write would.
    pub(crate) static RELEASE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Releases the actor's lease `id`.
fn release(store: &mut Store, actor: &str, id: &str) -> Result<(), GraphError> {
    #[cfg(test)]
    if RELEASE_FAILS.with(|f| f.replace(false)) {
        return Err(GraphError::failure(
            Code::CoordStoreFailed,
            "a store write failed (injected)",
        ));
    }
    store
        .execute(&Command::Release {
            owner: actor.into(),
            lease: mdkg_coord::LeaseId(id.to_owned()),
            now: coord::now_ms(),
        })
        .map(drop)
        .map_err(store_error)
}

/// `result`, after releasing the lease when this run acquired it and
/// leaves nothing to hold it for: on any failure, and on a success that
/// doesn't keep it (`kept` false: an unchanged copy, or a retry replaying
/// an earlier save's receipt). A lease that existed before the command
/// started (`node edit`'s, a claim, a broader claim) is never released, on
/// any path (orchestrator decision on Terra #16768). A release that fails
/// is reported: added to the failure's hint, or as a warning on success.
fn settle_lease<T>(
    repo: &Repo,
    store: &mut Store,
    actor: &str,
    taken: &Taken,
    result: Result<(T, bool), GraphError>,
) -> Result<T, GraphError> {
    let release_it = taken.fresh && !matches!(result, Ok((_, true)));
    let failed = if release_it {
        release(store, actor, &taken.id).err()
    } else {
        None
    };
    // With an authority configured, `release <id>` goes to it and can't
    // reach this worktree's lease (Terra #16828): that lease ends when it
    // expires.
    let remote = failed.is_some() && matches!(crate::authority::find(repo), Ok(Some(_)));
    let kept = |e: &GraphError| {
        let how = if remote {
            "it ends when it expires, since `release` by id goes to the configured authority"
                .to_owned()
        } else {
            format!(
                "run `{} release {} --as={}`",
                crate::BIN,
                taken.id,
                shell_word(actor)
            )
        };
        format!(
            "its lease {} may still be held ({}): {how}",
            taken.id, e.diagnostic.message
        )
    };
    match (result, failed) {
        (Ok((v, _)), None) => Ok(v),
        (Ok((v, _)), Some(e)) => {
            crate::output::notice(Diagnostic::warning(
                Code::CoordStoreFailed,
                format!("the node is saved, but {}", kept(&e)),
            ));
            Ok(v)
        }
        (Err(err), None) => Err(err),
        (Err(mut err), Some(e)) => {
            let hint = match err.diagnostic.hint.take() {
                Some(h) => format!("{h}; and {}", kept(&e)),
                None => kept(&e),
            };
            Err(err.with_hint(hint))
        }
    }
}

/// `s` as one shell word to paste: bare when it holds only safe
/// characters, otherwise single-quoted (Terra #16828: an actor with a
/// space or a shell metacharacter).
fn shell_word(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "._-@:+/,=".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A local work node (kept under `.git/mdkg/work`, off the graph ref)
/// isn't edited through a copy: refused before any lease, naming its file.
fn not_local_work(node: &IndexNode) -> Result<(), GraphError> {
    if !mdkg_graph::store_work::is_store_path(&node.path) {
        return Ok(());
    }
    Err(GraphError::usage(
        Code::UsageInvalidArguments,
        format!(
            "{} is local work, not on the graph ref: node edit and node save are for nodes \
             on the graph ref",
            node.id
        ),
    )
    .with_hint(format!(
        "edit its file in place: {} (`{} claim {}` first to hold it)",
        node.path,
        crate::BIN,
        node.id
    )))
}

/// `node edit`: the node's lease, and a writable copy of the graph's
/// bytes outside `.git`. Everything that can refuse is checked before the
/// lease is taken.
pub(crate) fn edit(
    repo: &Repo,
    actor: &str,
    node: &IndexNode,
    dir: Option<&Path>,
    discard: bool,
) -> Result<Edited, GraphError> {
    let mode = ref_mode(repo)?;
    not_local_work(node)?;
    let path = node.path.clone();
    let (held, name) = copy_dir(repo, dir, &path)?;
    let bytes = fs::read(repo.root.join(&path)).map_err(io)?;
    let preimage = blob_of(mode, &bytes)?;
    let mut store = repo.store()?;
    repo.observe(&mut store, coord::now_ms())?;
    let taken = lease(repo, &mut store, actor, node)?;
    // Success keeps the lease for the save (`true`).
    let result = edit_copy(
        mode,
        actor,
        node,
        (dir, discard),
        (held, name),
        (bytes, preimage),
        taken.id.clone(),
    )
    .map(|e| (e, true));
    settle_lease(repo, &mut store, actor, &taken, result)
}

/// `node edit`'s copy, under the lease `edit` took.
fn edit_copy(
    mode: &RefMode,
    actor: &str,
    node: &IndexNode,
    (dir, discard): (Option<&Path>, bool),
    (held, name): (Dir, String),
    (bytes, preimage): (Vec<u8>, String),
    lease: String,
) -> Result<Edited, GraphError> {
    let path = node.path.clone();
    let copy = held.shown.join(&name);
    let shown = copy.display().to_string();
    if dir.is_none()
        && let Some(warning) = exclude(mode)
    {
        crate::output::notice(warning);
    }
    let record = load_record(mode, &node.id, actor).filter(|r| r.path == path && r.copy == shown);
    if !discard && let Some(existing) = held.read(&name)? {
        let as_written = match &record {
            Some(r) => blob_of(mode, &existing)? == r.written,
            None => false,
        };
        if !as_written {
            crate::output::notice(
                graph_warning(
                    "graph.edit_copy_kept",
                    format!("kept your unsaved copy of {} at {shown}", node.id),
                )
                .with_hint(format!(
                    "save it with `{} node save`, or start over with `{} node edit --discard`",
                    crate::BIN,
                    crate::BIN
                )),
            );
            return Ok(Edited {
                copy,
                preimage: record.map(|r| r.preimage),
                kept: true,
                lease,
            });
        }
    }
    // The copy first, then the record: a crash between leaves a copy the
    // record doesn't describe, kept with the old preimage (fails closed).
    held.write(&name, &bytes)?;
    save_record(
        mode,
        &Record {
            node: node.id.clone(),
            path,
            actor: actor.to_owned(),
            copy: shown,
            preimage: preimage.clone(),
            written: preimage.clone(),
        },
    )?;
    Ok(Edited {
        copy,
        preimage: Some(preimage),
        kept: false,
        lease,
    })
}

/// `node save`: publishes the copy, or `content` (MCP's `node_save`) with
/// its `expected` preimage, under the node's lease. Refused as stale when
/// the graph's blob isn't the preimage. The copy and record are removed
/// once published.
pub(crate) fn save(
    repo: &Repo,
    actor: &str,
    node: &IndexNode,
    request_id: &str,
    dir: Option<&Path>,
    content: Option<(&[u8], Option<&str>)>,
) -> Result<Saved, GraphError> {
    let mode = ref_mode(repo)?;
    not_local_work(node)?;
    let path = node.path.clone();
    let record = load_record(mode, &node.id, actor).filter(|r| r.path == path);
    let (held, name) = copy_dir(repo, dir, &path)?;
    let shown = held.shown.join(&name).display().to_string();
    // `discard`: the whole `node edit --discard` for this copy (its root,
    // actor, node and `--dir`), for a stale refusal's hint.
    let (bytes, preimage, discard) = match content {
        Some((bytes, expected)) => {
            let preimage = expected
                .map(str::to_owned)
                .or_else(|| record.as_ref().map(|r| r.preimage.clone()))
                .ok_or_else(|| {
                    GraphError::usage(
                        Code::UsageInvalidArguments,
                        "content to save needs the preimage it was based on",
                    )
                    .with_hint("pass the preimage `node edit` printed, or run `node edit` first")
                })?;
            (bytes.to_vec(), preimage, None)
        }
        None => {
            let r = record.as_ref().filter(|r| r.copy == shown);
            let copy = held.read(&name)?;
            // A retry of a save that already published (its copy and
            // record are gone, or only the copy after a crash between the
            // two): its receipt, never an error (Terra #11543). A record
            // for a copy somewhere else (another `--dir`) is another edit:
            // it's left alone, and this save is refused as "no copy" so the
            // caller finds it (Terra #11611).
            let other_copy = record.is_some() && r.is_none();
            if !other_copy
                && (r.is_none() || copy.is_none())
                && let Some(commit) = earlier_save(mode, request_id, &node.id)?
            {
                // Read-only, like every replay: a record left here is
                // `node edit`'s to settle.
                return Ok(Saved {
                    commit: Some(commit),
                });
            }
            // Nothing to act on: exit 3, as for any missing thing (e2e B16).
            // The hint's command runs as shown, from anywhere: this save's
            // graph and actor, the node, and the `--dir` it looked in, each
            // value attached so one starting with `-` stays a value (Terra
            // #15584, #15805).
            let word = crate::agents::shell_word;
            let again = format!(
                "{} --root={} --as={} node edit {}{}",
                crate::BIN,
                // In ref mode the checkout, not the graph cache under
                // .git (e2e N1). `--dir` stays as given: an edit record
                // keeps the folder as typed, so a save retried with the
                // same `--dir` finds the copy (review of e1045284).
                word(
                    &if crate::coord::graph_git::for_root(&repo.root).is_some() {
                        &repo.top
                    } else {
                        &repo.root
                    }
                    .to_string_lossy()
                ),
                word(actor),
                word(&node.id),
                dir.map(|d| format!(" --dir={}", word(&d.to_string_lossy())))
                    .unwrap_or_default()
            );
            let r = r.ok_or_else(|| {
                GraphError::not_found(
                    Code::UsageInvalidArguments,
                    format!(
                        "there's no copy of {} from your `node edit` to save",
                        node.id
                    ),
                )
                .with_hint(format!("run `{again}` first"))
            })?;
            let bytes = copy.ok_or_else(|| {
                GraphError::not_found(
                    Code::UsageInvalidArguments,
                    format!("the copy of {} is gone", node.id),
                )
                .with_hint(format!("run `{again}` again"))
            })?;
            (
                bytes,
                r.preimage.clone(),
                Some(format!("{again} --discard")),
            )
        }
    };
    let from_copy = discard.is_some();
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| GraphError::usage(Code::UsageInvalidArguments, "the node isn't UTF-8"))?;
    let parsed = mdkg_format::frontmatter::parse(text, &path)
        .map_err(|e| GraphError::from_format(&e, &repo.root))?;
    if parsed
        .frontmatter
        .get("id")
        .and_then(mdkg_format::frontmatter::Value::as_str)
        != Some(node.id.as_str())
    {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "the copy's id isn't the node's: an id never changes",
        ));
    }
    // A replay of a save already published takes no lease (R41).
    if let Some(commit) = earlier_save(mode, request_id, &node.id)? {
        return replayed(mode, request_id, node, commit, &bytes);
    }
    let mut store = repo.store()?;
    repo.observe(&mut store, coord::now_ms())?;
    let taken = lease(repo, &mut store, actor, node)?;
    let lease = taken.id.clone();
    // Published or refused under the lease, and whether the publish
    // committed it; [`settle_lease`] releases what's left (e2e N2, Terra
    // #16757).
    let result = (|| -> Result<(Saved, bool), GraphError> {
        let leaf = repo.leaf(&path)?;
        let digest = Digest::of_bytes(&bytes).to_string();
        let graph = mode.graph.name.clone();
        let wrote = prepare_record_publish(
            mode,
            &mut store,
            (request_id, "save", actor),
            |w, _| {
                // Judged at the session's base, under the lock.
                if graph_blob(mode, &path)?.as_deref() != Some(preimage.as_str()) {
                    return Err(graph_error(
                        "graph.write_stale",
                        format!("the graph changed {} since `node edit`", node.id),
                    )
                    // `--discard` overwrites the copy, so the edits are
                    // moved out of it first (e2e UX).
                    .with_hint(if let Some(discard) = &discard {
                        format!(
                            "your copy {} is kept: move your edits out of it first, since \
                             `{discard}` replaces it with the graph's version; then run \
                             that, re-apply them and save again",
                            crate::output::shown_path(&shown)
                        )
                    } else {
                        "re-read the node, merge your change into it, and save it with the \
                         new preimage"
                            .to_string()
                    }));
                }
                w.write(&path, &bytes)?;
                Ok((
                    Publish {
                        message: format!("save {}", node.id),
                        leases: vec![lease.clone()],
                        allowed: vec![leaf.clone()],
                        trailers: Vec::new(),
                    },
                    (),
                ))
            },
            |store, prepared| {
                // Publishing commits the lease, and committing a broader
                // claim would end it for everything under it: a lease this
                // command didn't take is never committed, so the save is
                // refused with nothing published (orchestrator, Terra
                // #16840).
                if let Some(scope) = &taken.broader {
                    return Err(GraphError::usage(
                        Code::UsageInvalidArguments,
                        format!(
                            "{} is covered by your claim on {scope}: publishing it would \
                             commit, and so end, that whole claim",
                            node.id
                        ),
                    )
                    .with_hint(format!(
                        "{}: release {scope} (or let it end), then save again; or claim the \
                         node's file alone instead of its folder",
                        if from_copy {
                            "your copy is kept"
                        } else {
                            "nothing was saved, so keep the content you passed"
                        }
                    )));
                }
                let changes = Changes::from([(leaf.clone(), Some(digest.clone()))]);
                match store
                    .execute(&Command::Commit {
                        owner: actor.into(),
                        lease: mdkg_coord::LeaseId(lease.clone()),
                        changes,
                        source: Some(Source {
                            commit: prepared.commit.clone(),
                            branch: graph.clone(),
                        }),
                        now: coord::now_ms(),
                        landed: false,
                    })
                    .map_err(store_error)?
                {
                    Outcome::Committed { .. } => Ok(()),
                    Outcome::Refused { error } => Err(coord::refusal(&error)
                        .with_hint("the graph changed under your lease: re-read and retry")),
                    other => Err(GraphError::failure(
                        Code::CoordStoreFailed,
                        format!("unexpected outcome {other:?}"),
                    )),
                }
            },
        )?;
        // Published by a save between the check above and the lock: a
        // replay all the same, touching no record or copy; a lease this
        // run took is released.
        let written = match wrote {
            Wrote::Already(w) => {
                return replayed(mode, request_id, node, w.receipt.commit, &bytes)
                    .map(|s| (s, false));
            }
            Wrote::Done(_, written) => written,
        };
        // The copy goes when it's what was saved, or still what mdkg wrote.
        // Best effort: the save is published, so a failure here keeps the copy
        // and still returns the receipt (Terra #11611).
        let unmodified = match (&record, held.read(&name).ok().flatten()) {
            (Some(r), Some(now)) => {
                r.copy == shown && blob_of(mode, &now).is_ok_and(|b| b == r.written)
            }
            _ => false,
        };
        if from_copy || unmodified {
            held.remove(&name);
        }
        drop_record(mode, &node.id, actor);
        // Only a publish of this save commits its lease (`true`): an
        // unchanged copy writes nothing, so a lease this run took for it is
        // released.
        let commit = written.map(|w| w.receipt.commit);
        let committed = commit.is_some();
        Ok((Saved { commit }, committed))
    })();
    settle_lease(repo, &mut store, actor, &taken, result)
}

/// `mdkg node`: edit a node body when the graph lives on the graph ref.
#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Take the node's lease and write an editable copy outside `.git`:
    /// `--dir`, else `<worktree>/.mdkg-edit/<graph path>`. An unsaved copy
    /// is kept, never overwritten.
    Edit {
        /// The node: an id, a legacy id, a unique prefix or a title.
        id: String,
        /// The folder to put the copy under.
        #[arg(long, value_name = "FOLDER")]
        dir: Option<PathBuf>,
        /// Start over from the graph's bytes, replacing an unsaved copy.
        #[arg(long)]
        discard: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Publish the edited copy under the node's lease; refused as stale
    /// when the graph changed the node since `node edit`.
    Save {
        /// The node: an id, a legacy id, a unique prefix or a title.
        id: String,
        /// The folder `node edit --dir` put the copy under.
        #[arg(long, value_name = "FOLDER")]
        dir: Option<PathBuf>,
        /// Read the node's new content from stdin instead of the copy.
        #[arg(long)]
        stdin: bool,
        /// With `--stdin`: the graph blob the content was based on (as
        /// `node edit` printed it); else the record of your `node edit`.
        #[arg(long, requires = "stdin", value_name = "BLOB")]
        preimage: Option<String>,
        /// A retry passes the same id and gets the original receipt (one
        /// is minted otherwise, and printed).
        #[arg(long, value_name = "ID")]
        request_id: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl NodeCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Edit { .. } => "node.edit",
            Self::Save { .. } => "node.save",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            Self::Edit { json, .. } | Self::Save { json, .. } => *json,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EditData {
    /// The node's id.
    pub node: String,
    /// Its graph path.
    pub path: String,
    /// The writable copy.
    pub copy: String,
    /// The graph blob the copy is based on; pass it to `node save --stdin
    /// --preimage`. Absent for a kept copy mdkg has no record of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
    /// An unsaved copy from an earlier `node edit` was kept.
    pub kept: bool,
    /// The node's lease.
    pub lease: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SaveData {
    /// The node's id.
    pub node: String,
    /// Its graph path.
    pub path: String,
    /// The request id; a retry with it returns this receipt.
    pub request_id: String,
    /// The published commit; absent when the content changed nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Stdin's bytes, capped as a node file is.
fn read_stdin() -> Result<Vec<u8>, GraphError> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "the node's content is over 16 MiB",
        ));
    }
    Ok(bytes)
}

/// The node a handle names in this graph.
fn node_for(repo: &Repo, handle: &str) -> Result<IndexNode, GraphError> {
    let config = mdkg_graph::config::load(&repo.root)?;
    let index = mdkg_graph::index::build(&repo.root, &config)?;
    Ok(crate::write::noted_node(&index, handle, "id")?.clone())
}

pub fn run(root: &Path, actor: Option<String>, command: &NodeCommand) -> u8 {
    match command {
        NodeCommand::Edit {
            id,
            dir,
            discard,
            json,
        } => {
            let report = (|| -> Result<crate::output::Report<EditData>, GraphError> {
                let repo = Repo::open(root)?;
                let actor = repo.actor(actor);
                let node = node_for(&repo, id)?;
                let edited = edit(&repo, &actor, &node, dir.as_deref(), *discard)?;
                Ok(crate::output::Report::ok(EditData {
                    node: node.id.clone(),
                    path: node.path.clone(),
                    copy: edited.copy.display().to_string(),
                    preimage: edited.preimage,
                    kept: edited.kept,
                    lease: edited.lease,
                }))
            })()
            .unwrap_or_else(crate::output::Report::failed);
            crate::output::emit(command.name(), *json, report, |d| {
                println!("{}", d.copy);
                if let Some(p) = &d.preimage {
                    println!("preimage {p}");
                }
            })
        }
        NodeCommand::Save {
            id,
            dir,
            stdin,
            preimage,
            request_id,
            json,
        } => {
            let report = (|| -> Result<crate::output::Report<SaveData>, GraphError> {
                let repo = Repo::open(root)?;
                let actor = repo.actor(actor);
                let node = node_for(&repo, id)?;
                let request = request_id
                    .clone()
                    .unwrap_or_else(|| mdkg_format::Uuid7::now().to_string());
                let content = if *stdin { Some(read_stdin()?) } else { None };
                let saved = save(
                    &repo,
                    &actor,
                    &node,
                    &request,
                    dir.as_deref(),
                    content.as_deref().map(|c| (c, preimage.as_deref())),
                )?;
                Ok(crate::output::Report::ok(SaveData {
                    node: node.id.clone(),
                    path: node.path.clone(),
                    request_id: request,
                    commit: saved.commit,
                }))
            })()
            .unwrap_or_else(crate::output::Report::failed);
            crate::output::emit(command.name(), *json, report, |d| {
                match &d.commit {
                    Some(c) => println!("saved {} in {}", d.node, &c[..c.len().min(12)]),
                    None => println!("{} is unchanged", d.node),
                }
                // The id a retry passes to get this receipt again.
                println!("request {}", d.request_id);
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terra #16828: the actor in a recovery hint is one shell word; the
    /// hint's `--as=` keeps a leading `-` a value.
    #[test]
    fn shell_word_quotes_what_the_shell_would_split() {
        assert_eq!(shell_word("f-refmode"), "f-refmode");
        assert_eq!(shell_word("-x"), "-x");
        assert_eq!(shell_word("Pat Work"), "'Pat Work'");
        assert_eq!(shell_word("a'b;c"), "'a'\\''b;c'");
        assert_eq!(shell_word(""), "''");
    }

    fn tmp() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mdkg-node-edit-{}", mdkg_format::Uuid7::now()));
        fs::create_dir_all(&dir).expect("tmp");
        fs::canonicalize(&dir).expect("canonical")
    }

    /// Operations go through the held folder: a parent swapped for a link
    /// after it was opened doesn't redirect the write.
    #[test]
    fn a_parent_swapped_for_a_link_does_not_redirect_a_held_write() {
        let tmp = tmp();
        let elsewhere = tmp.join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("elsewhere");
        let held = Dir::open(&tmp).expect("open").child("a").expect("child");
        fs::rename(tmp.join("a"), tmp.join("a-moved")).expect("move");
        std::os::unix::fs::symlink(&elsewhere, tmp.join("a")).expect("swap");
        held.write("copy.md", b"notes").expect("write");
        assert_eq!(
            fs::read(tmp.join("a-moved/copy.md")).expect("held"),
            b"notes"
        );
        assert!(fs::read_dir(&elsewhere).expect("dir").next().is_none());
        // Reached again by path, the link is refused.
        let e = Dir::open(&tmp).expect("open").child("a").expect_err("link");
        assert!(
            crate::coord::graph_git::has_code(&e, "graph.edit_path_unsafe"),
            "{e:?}"
        );
        fs::remove_dir_all(&tmp).ok();
    }

    /// A copy that's a link is refused, never read or written through.
    #[test]
    fn a_linked_copy_is_refused() {
        let tmp = tmp();
        fs::write(tmp.join("target"), b"secret").expect("target");
        std::os::unix::fs::symlink(tmp.join("target"), tmp.join("copy.md")).expect("link");
        let held = Dir::open(&tmp).expect("open");
        assert!(held.read("copy.md").is_err());
        held.write("copy.md", b"mine")
            .expect("replaces the link itself");
        assert_eq!(fs::read(tmp.join("target")).expect("target"), b"secret");
        fs::remove_dir_all(&tmp).ok();
    }
}
