//! `retype`: change a node's type in a format 0.7 graph (format 0.7,
//! section 4.3).
//!
//! The UUID is the identity, so retyping rewrites the type written in the
//! node's `id` and in every ref to it, in every node file of the graph, and
//! sets the node's `type`. File names stay as they are: no ref uses them.
//!
//! The rewrites are checked in memory against the templates first, then
//! written under one lease per file and committed once, as `done` commits.
//! A crash part way leaves some refs with the old type. They still name
//! the node by UUID (validate warns about them), and running `retype` again
//! finishes the job: it rewrites whatever still has another type, the node
//! last.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use mdkg_format::frontmatter::{self, Value as FmValue};
use mdkg_format::id::is_id_type;
use mdkg_format::node::{ALLOWED_TYPES, FormatVersion, ParseOptions, parse_node};
use mdkg_format::{Code, NodeId};
use mdkg_graph::{ErrorKind, GraphError, Index, IndexNode, config, graph_checks, index, templates};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::coord;
use crate::coord::{Repo, now_ms};
use crate::output::{self, NodeRecord, Report};
use crate::write::{Edit, node_value, not_before, record_edits_checked, today};
use mdkg_coord_sqlite::git;
use mdkg_format::program::BIN;

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetypeData {
    #[schemars(with = "NodeRecord")]
    pub node: Value,
    /// The id before the retype.
    pub from: String,
    /// Graph-relative paths of the files rewritten, the node's last.
    pub rewritten: Vec<String>,
    /// Files an earlier, stopped retype already wrote (its plan says so),
    /// committed and recorded by this one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recovered: Vec<String>,
    /// The commit holding the rewrites, when there were any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// A node file with every `<type>-<uuid>` naming `id`'s UUID written as
/// `id`, except in extension keys' values, which are opaque (format 0.7,
/// section 8): their lines are copied as they are.
pub fn rewrite_node(content: &str, id: &NodeId) -> String {
    let Ok(bounds) = frontmatter::source_bounds(content, "") else {
        return rewrite_refs(content, id);
    };
    let (header, body) = content.split_at(bounds.body_start);
    let mut out = String::with_capacity(content.len());
    for line in header.split_inclusive('\n') {
        let key = line
            .split_once(':')
            .map(|(k, _)| k.trim_matches([' ', '\t']));
        if key.is_some_and(mdkg_format::node::is_any_extension_key) {
            out.push_str(line);
        } else {
            out.push_str(&rewrite_refs(line, id));
        }
    }
    out.push_str(&rewrite_refs(body, id));
    out
}

/// `content` with every `<type>-<uuid>` naming `id`'s UUID written as `id`.
/// A type is only rewritten where it's a whole token: preceded by neither a
/// letter, a digit, `_` nor `-`, and followed by no hex digit or `-`.
pub fn rewrite_refs(content: &str, id: &NodeId) -> String {
    let uuid = id.uuid().to_string();
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = content[from..].find(&uuid) {
        let at = from + found;
        let end = at + uuid.len();
        from = end;
        let after_ok = bytes
            .get(end)
            .is_none_or(|c| !(c.is_ascii_hexdigit() || *c == b'-'));
        if at == 0 || bytes[at - 1] != b'-' || !after_ok {
            continue;
        }
        let dash = at - 1;
        // Back over the type's ASCII bytes, so `start` is always a
        // character boundary (whatever precedes the type).
        let start = bytes[..dash]
            .iter()
            .rposition(|c| !matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_'))
            .map_or(0, |i| i + 1);
        let boundary = start == 0 || bytes[start - 1] != b'-';
        let written = &content[start..dash];
        if !boundary || !is_id_type(written) || written == id.node_type() {
            continue;
        }
        out.push_str(&content[copied..start]);
        out.push_str(id.node_type());
        copied = dash;
    }
    out.push_str(&content[copied..]);
    out
}

/// Makes a folder's entries durable (a no-op where folders can't be synced).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn digest(content: &str) -> String {
    mdkg_format::Digest::of_bytes(content.as_bytes()).to_string()
}

/// Where a retype keeps its [`Plan`]: the repository's git folder.
struct Journal;

/// A retype's plan: each graph-relative path and the sha256 of the content
/// it will hold, and whether the change was recorded.
#[derive(Debug, Default, Serialize, serde::Deserialize)]
struct Plan {
    files: BTreeMap<String, String>,
    #[serde(default)]
    recorded: bool,
}

impl Journal {
    /// One per node and worktree: `<uuid>-<16 hex of sha256(worktree top)>`.
    fn path(repo: &Repo, uuid: mdkg_format::Uuid7) -> Result<std::path::PathBuf, GraphError> {
        let common = git::common_dir(&repo.top).map_err(coord::git_error)?;
        let top = digest(&repo.top.to_string_lossy());
        let key = top.strip_prefix("sha256:").unwrap_or(&top);
        Ok(common
            .join("mdkg")
            .join("retype")
            .join(format!("{uuid}-{}.json", &key[..16])))
    }

    /// The files a stopped (or recently recorded) retype planned, or none.
    fn read(path: &Path) -> BTreeMap<String, String> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Plan>(&t).ok())
            .map(|p| p.files)
            .unwrap_or_default()
    }

    fn write(path: &Path, plan: &Plan) -> Result<(), GraphError> {
        let fail = |e: std::io::Error| {
            GraphError::failure(
                Code::IoWriteFailed,
                format!("can't write retype's plan: {e}"),
            )
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let text = serde_json::to_string(plan).unwrap_or_default();
        crate::write::atomic_write(path, text.as_bytes()).map_err(fail)?;
        // The rename and any new folders are durable before a node is
        // written: a system crash must not keep the rewrites and lose the
        // plan.
        // Up to and including the git folder, which may have just gained
        // its `mdkg` folder.
        let common = path
            .ancestors()
            .nth(3)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut dir = path.parent();
        while let Some(d) = dir {
            sync_dir(d).map_err(fail)?;
            if d == common {
                break;
            }
            dir = d.parent();
        }
        Ok(())
    }
}

fn bad(code: Code, message: impl Into<String>) -> GraphError {
    GraphError::usage(code, message)
}

pub fn retype(root: &Path, actor: Option<String>, target: &str, new_type: &str, json: bool) -> u8 {
    let mut released = Vec::new();
    let report = (|| -> Result<Report<RetypeData>, GraphError> {
        let repo = Repo::open(root)?;
        let config = config::load(&repo.root)?;
        if config.format != FormatVersion::V07 {
            return Err(bad(Code::UsageBadValue, "retype needs a format 0.7 graph")
                .with_hint("0.6.0 ids carry their type in the counter; migrate the graph first"));
        }
        let new_type = new_type.trim().to_lowercase();
        if !ALLOWED_TYPES.contains(&new_type.as_str()) {
            // A type typed on the command line (D2): usage.unknown_type.
            return Err(bad(Code::UsageUnknownType, "that isn't a node type")
                .with_hint(format!("types: {}", ALLOWED_TYPES.join(", "))));
        }
        let index = index::build(&repo.root, &config)?;
        let node = crate::write::noted_node(&index, target, "id")?.clone();
        // Profile C: a work node in the store's folder and a node
        // in git live in different planes; a retype never moves one across.
        if config.work_committed == mdkg_graph::config::WorkCommitted::None {
            let in_store = mdkg_graph::store_work::is_store_path(&node.path);
            let to_store = mdkg_graph::store_work::STORE_WORK_TYPES.contains(&new_type.as_str());
            if in_store != to_store {
                return Err(bad(
                    Code::UsageBadValue,
                    format!(
                        "under work.committed \"none\", a {} lives {} and a {new_type} {}",
                        node.node_type,
                        if in_store {
                            "beside the store"
                        } else {
                            "in git"
                        },
                        if to_store {
                            "beside the store"
                        } else {
                            "in git"
                        }
                    ),
                )
                .with_hint("create the node anew as the other type, and link the two"));
            }
        }
        let old = NodeId::parse(&node.id)
            .map_err(|e| GraphError::failure(Code::NodeBadId, e.to_string()))?;
        let new = old
            .with_type(&new_type)
            .map_err(|e| bad(Code::UsageBadValue, e.to_string()))?;

        let schemas = templates::load(&repo.root, &config)?.schemas;
        let opts = ParseOptions {
            work_status: &config.status_enum,
            priority_min: config.priority_min,
            priority_max: config.priority_max,
            templates: &schemas,
            allow_extension_keys: true,
            format: config.format,
        };
        let actor = repo.actor(actor);
        let mut store = repo.store()?;
        let now = now_ms();
        repo.observe(&mut store, now)?;
        let stamp = today(&repo, now);

        // Every rewrite, checked in memory before anything is written; the
        // node itself goes last.
        let mut edits = Vec::new();
        let mut own = None;
        let mut after_nodes: Vec<(String, IndexNode)> = Vec::new();
        // What each edited file held when read, to check it hasn't moved on
        // once the leases are held.
        let mut read: Vec<(std::path::PathBuf, String)> = Vec::new();
        // A stopped retype's rewrites: the files its journal planned, still
        // holding exactly that content, committed or not (a crash may come
        // after the git commit but before the coordinator recorded it).
        let journal = Journal::path(&repo, new.uuid())?;
        let planned = Journal::read(&journal);
        for n in &index.nodes {
            let path = repo.root.join(&n.path);
            let before = std::fs::read_to_string(&path).map_err(|e| {
                GraphError::failure(Code::IoReadFailed, format!("can't read {}: {e}", n.path))
            })?;
            let is_node = n.qid == node.qid;
            let mut after = rewrite_node(&before, &new);
            if is_node && n.node_type != new_type {
                after = frontmatter::set_field_with(
                    &after,
                    &n.path,
                    "type",
                    &FmValue::String(new_type.clone()),
                    frontmatter::Lexical::Ascii,
                )
                .map_err(|e| GraphError::from_format(&e, &repo.root))?;
            }
            if after == before {
                // Already current, but written by a retype that stopped
                // before it was recorded: commit and record it this time.
                // Committing a clean file again changes nothing in git, and
                // records the version the coordinator may lack.
                if planned.get(&n.path) == Some(&digest(&before)) {
                    read.push((path.clone(), before.clone()));
                    let edit = Edit {
                        node: n.clone(),
                        fields: Vec::new(),
                        content: None,
                    };
                    if is_node {
                        own = Some(edit);
                    } else {
                        edits.push(edit);
                    }
                }
                continue;
            }
            after = frontmatter::set_field_with(
                &after,
                &n.path,
                "updated",
                &FmValue::String(not_before(&stamp, &[&n.created, &n.updated])),
                frontmatter::Lexical::Ascii,
            )
            .map_err(|e| GraphError::from_format(&e, &repo.root))?;
            let parsed = parse_node(&after, &n.path, &opts).map_err(|e| {
                let mut e = GraphError::from_format(&e, &repo.root);
                if is_node {
                    e = e.with_hint(format!(
                        "a {new_type} can't keep this node's keys; edit them first"
                    ));
                }
                e
            })?;
            after_nodes.push((
                n.qid.clone(),
                IndexNode::from_parsed(parsed, &n.ws, n.path.clone()),
            ));
            read.push((path.clone(), before.clone()));
            let edit = Edit {
                node: n.clone(),
                fields: Vec::new(),
                content: Some(after),
            };
            if is_node {
                own = Some(edit);
            } else {
                edits.push(edit);
            }
        }
        edits.extend(own);
        // The graph as it would be: graph-level rules (a goal's scope or
        // active node types, for one) are checked before anything is written.
        let nodes: Vec<IndexNode> = index
            .nodes
            .iter()
            .map(|n| {
                after_nodes
                    .iter()
                    .find(|(q, _)| *q == n.qid)
                    .map_or_else(|| n.clone(), |(_, a)| a.clone())
            })
            .collect();
        let mut preview = Index::assemble_in(nodes, index.workspaces.clone(), config.graph_id);
        preview.private_work = index.private_work;
        let external: HashSet<&str> = config.subgraphs.iter().map(String::as_str).collect();
        if let Some(first) = graph_checks::collect(&preview, config.tolerant, &external)
            .into_iter()
            .next()
        {
            return Err(
                GraphError::new(ErrorKind::Validation, first).with_hint(format!(
                    "the graph wouldn't be valid with this node as a {new_type}"
                )),
            );
        }
        let (recovered, rewritten): (Vec<&Edit>, Vec<&Edit>) =
            edits.iter().partition(|e| e.content.is_none());
        let paths = |v: Vec<&Edit>| v.iter().map(|e| e.node.path.clone()).collect::<Vec<_>>();
        let (recovered, rewritten) = (paths(recovered), paths(rewritten));
        let commit = if edits.is_empty() {
            None
        } else {
            let message = format!(
                "{}: retype from {}",
                output::display_ref(&new.to_string()),
                old.node_type()
            );
            // A lease authority must say the actor holds every file
            // exclusively, as for done and goal edits.
            let mut held = Vec::new();
            for e in &edits {
                let leaf = repo.leaf(&e.node.path)?;
                let lease = coord::require_authority_hold(&repo, &actor, &leaf, Some(&e.node.qid))?;
                held.push((leaf, lease));
            }
            // Under the leases: nothing read may have moved on, here or as a
            // version another worktree committed.
            let plan: BTreeMap<String, String> = edits
                .iter()
                .map(|e| {
                    let content = e.content.clone().or_else(|| {
                        read.iter()
                            .find(|(p, _)| *p == repo.root.join(&e.node.path))
                            .map(|(_, b)| b.clone())
                    });
                    (
                        e.node.path.clone(),
                        digest(content.as_deref().unwrap_or_default()),
                    )
                })
                .collect();
            let check = |store: &mut mdkg_coord_sqlite::Store| -> Result<(), GraphError> {
                for (path, before) in &read {
                    if std::fs::read_to_string(path).ok().as_deref() != Some(before.as_str()) {
                        return Err(GraphError::conflict(
                            Code::CoordLeaseHeld,
                            "a file retype rewrites changed while it ran",
                        )
                        .with_hint("run the same retype again"));
                    }
                }
                let state = store.coordinator().map_err(coord::store_error)?;
                for e in &edits {
                    let leaf = repo.leaf(&e.node.path)?;
                    if !coord::missing_versions(&repo, &state, &leaf)?.is_empty() {
                        return Err(GraphError::conflict(
                            Code::CoordSyncNeeded,
                            format!(
                                "{} has an unmerged version this worktree lacks",
                                e.node.path
                            ),
                        )
                        .with_hint(format!(
                            "run `{BIN} sync` on it, then the same retype again"
                        )));
                    }
                }
                // Before any file is written: what this run will write, so a
                // rerun after a crash knows which files are its.
                Journal::write(
                    &journal,
                    &Plan {
                        files: plan.clone(),
                        recorded: false,
                    },
                )
            };
            let recorded = record_edits_checked(
                &repo,
                &mut store,
                &actor,
                &edits,
                &[],
                &message,
                now,
                &check,
                None,
            );
            // Only once recorded is the plan finished. A refused or undone
            // attempt leaves it, since files an earlier stopped run wrote
            // are still waiting for their commit.
            // The plan stays after it's recorded: the coordinator store
            // syncs lazily (WAL, synchronous=NORMAL), so a power loss could
            // drop records it just acknowledged, and a rerun then records
            // them again from the plan. Plans are never swept, so no
            // deletion can race a replacement: one small file per node and
            // worktree, replaced by that node's next retype.
            let recorded = recorded?;
            // Marked recorded. If the mark fails, nothing is lost: a rerun
            // only records the files again.
            let _ = Journal::write(
                &journal,
                &Plan {
                    files: plan,
                    recorded: true,
                },
            );
            // Profile C's shared files were written under `work:` leases,
            // held until now.
            recorded.release_store_leases(&mut store, &actor, now)?;
            // Recorded: each file's authority lease goes too.
            released = held
                .iter()
                .filter_map(|(leaf, lease)| {
                    coord::release_at_authority(&repo, &actor, leaf, lease.as_ref())
                })
                .collect();
            Some(recorded.commit)
        };
        let (value, warnings) = node_value(&repo, &new.to_string())?;
        Ok(Report::ok(RetypeData {
            node: value,
            from: old.to_string(),
            rewritten,
            recovered,
            commit,
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed)
    .with(released);
    output::emit("retype", json, report, |d| {
        println!(
            "retyped {} to {} ({} files)",
            d.from,
            d.node["id"].as_str().unwrap_or("-"),
            d.rewritten.len()
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";

    #[test]
    fn extension_values_are_opaque() {
        let id = NodeId::parse(&format!("bug-{UUID}")).expect("id");
        let text = format!(
            "---\nid: task-{UUID}\ntype: task\nx-acme.snapshot: task-{UUID}\nx_legacy: [task-{UUID}]\nrelates: [task-{UUID}]\n---\nx-acme.snapshot: task-{UUID} in the body is prose.\n"
        );
        assert_eq!(
            rewrite_node(&text, &id),
            format!(
                "---\nid: bug-{UUID}\ntype: task\nx-acme.snapshot: task-{UUID}\nx_legacy: [task-{UUID}]\nrelates: [bug-{UUID}]\n---\nx-acme.snapshot: bug-{UUID} in the body is prose.\n"
            )
        );
    }

    #[test]
    fn rewrites_whole_ids_only() {
        let id = NodeId::parse(&format!("bug-{UUID}")).expect("id");
        let text = format!(
            "id: task-{UUID}\nrelates: [feat-{UUID}, goal-0192f3a4-0000-7000-8000-000000000001]\nrefs: [mdkg://0190aaaa-0000-7000-8000-000000000000/task-{UUID}@sha256:{}]\n[x](task-{UUID}) `task-{UUID}` x-task-{UUID} {UUID} task-{UUID}0 bug-{UUID}\n",
            "a".repeat(64)
        );
        let out = rewrite_refs(&text, &id);
        assert_eq!(
            out,
            format!(
                "id: bug-{UUID}\nrelates: [bug-{UUID}, goal-0192f3a4-0000-7000-8000-000000000001]\nrefs: [mdkg://0190aaaa-0000-7000-8000-000000000000/bug-{UUID}@sha256:{}]\n[x](bug-{UUID}) `bug-{UUID}` x-task-{UUID} {UUID} task-{UUID}0 bug-{UUID}\n",
                "a".repeat(64)
            )
        );
        // Idempotent, and a no-op when nothing names the node.
        assert_eq!(rewrite_refs(&out, &id), out);
        assert_eq!(rewrite_refs("nothing", &id), "nothing");
        assert_eq!(rewrite_refs("é-task-", &id), "é-task-");
        // Unicode punctuation right before an id (review of 9ce31f5).
        let quoted = format!("“task-{UUID}” «task-{UUID}»");
        assert_eq!(
            rewrite_refs(&quoted, &id),
            format!("“bug-{UUID}” «bug-{UUID}»")
        );
    }
}
