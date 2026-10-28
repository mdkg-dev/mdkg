//! Collecting what the planner reads: the graph's node files, its event
//! logs, the instruction files outside it, its git identity, and the new
//! ids of its subgraphs (each child planned read only).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use mdkg_format::Code;
use mdkg_graph::{GraphError, config, identity_v2};
use serde_json::Value;

use super::gitops;
use super::plan::{self, ChildGraph, ForeignNode, NodeFile, Source, TextFile, V2Source};
use crate::coord::{Repo, git_error};

/// Files larger than this outside the graph are skipped by the patch.
const MAX_INSTRUCTION_BYTES: u64 = 1 << 20;

fn io_error(path: &str, e: std::io::Error) -> GraphError {
    GraphError::failure(Code::IoReadFailed, format!("failed to read {path}: {e}"))
}

/// Reads a tracked file as UTF-8, refusing anything else: the migration
/// rewrites text and must never lose a byte it can't read.
fn read_text(top: &Path, path: &str) -> Result<String, GraphError> {
    let bytes = std::fs::read(top.join(path)).map_err(|e| io_error(path, e))?;
    String::from_utf8(bytes).map_err(|_| {
        GraphError::failure(
            Code::IoReadFailed,
            format!("{path} isn't UTF-8, so the migration can't rewrite it"),
        )
        .at(path, None)
    })
}

/// Whether a tracked path is an instruction file the patch covers:
/// AGENTS.md and CLAUDE.md anywhere, the graph folder's own top-level
/// documents, skills (in the graph and its mirrors) and scripts.
fn is_instruction(path: &str, mdkg_dirs: &[String], mirrors: &[String]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(name, "AGENTS.md" | "CLAUDE.md") {
        return true;
    }
    let under = |dir: &str| {
        let dir = dir.trim_end_matches('/');
        !dir.is_empty() && path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
    };
    for d in mdkg_dirs {
        if let Some(rest) = path.strip_prefix(&format!("{d}/")) {
            if rest.starts_with("skills/") {
                return true;
            }
            // `.mdkg/README.md`, `.mdkg/AGENT_START.md`.
            return !rest.contains('/') && rest.ends_with(".md");
        }
    }
    mirrors.iter().any(|m| under(m)) || under("scripts")
}

/// Collects the source for the graph at `root`. `subgraphs` says whether to
/// plan the configured subgraphs too (a child is planned without its own).
pub fn gather(root: &Path, subgraphs: bool) -> Result<(Repo, Source), GraphError> {
    let repo = Repo::open(root)?;
    let top = repo.top.clone();
    let config_rel = format!("{}{}", repo.prefix, config::CONFIG_PATH);
    let config_text = read_text(&top, &config_rel)?;
    let raw: Value = serde_json::from_str(&config_text).map_err(|e| {
        GraphError::failure(
            Code::ConfigUnreadable,
            format!("config is not valid JSON: {e}"),
        )
    })?;
    // A 0.7.0 graph names itself in its config (lane 1, 2026-10-24).
    if raw.get("graph_id").is_some() {
        return Err(GraphError::conflict(
            Code::MigrateAlreadyMigrated,
            "this graph's config has a graph_id: it's already on 0.7.0",
        )
        .with_hint("there's nothing to migrate; `migrate --status` shows the last migration"));
    }
    // 0.6.0's identity format v2 (format 0.7, section 10.2): lane 1's
    // reader checks every node's identity, and its UUIDs are kept.
    let v2 = if identity_v2::is_identity_v2(root) {
        Some(identity_v2::read(root)?)
    } else {
        None
    };
    let cfg = match &v2 {
        Some(_) => config::load_identity_v2(root)?,
        None => config::load(root)?,
    };
    // The graph must be valid before it's migrated, by the whole
    // validator: a graph that fails it now would fail after the swap. The
    // validator refuses a v2 graph whole; lane 1's reader above checked
    // its identity, and the 0.6.0 CLI's validate is the operator's check
    // (orchestrator, Q2). The migrated graph is validated in place either
    // way, before HEAD moves.
    if v2.is_none()
        && let Some(d) = mdkg_graph::validate::validate(root, &cfg)?
            .errors
            .into_iter()
            .next()
    {
        let mut e = GraphError::new(mdkg_graph::ErrorKind::Failure, d);
        e.diagnostic.hint =
            Some("fix what `validate` reports first, then plan the migration".into());
        return Err(e);
    }
    // Every node, by graph-relative path and legacy id (the index refuses a
    // v2 graph, whose reader lists them instead).
    let listed: Vec<(String, String)> = match &v2 {
        Some(g) => g
            .nodes
            .iter()
            .map(|n| (n.path.clone(), n.legacy_id.clone()))
            .collect(),
        None => mdkg_graph::index::build(root, &cfg)?
            .nodes
            .into_iter()
            .map(|n| (n.path, n.id))
            .collect(),
    };
    let base = gitops::head(&top).map_err(git_error)?;

    let (graph_id, minted) = match &v2 {
        Some(g) => (g.graph_id.to_string(), false),
        None => {
            // The graph_id comes from the first commit, which a shallow clone
            // lacks. mdkg's git ignores the shallow file (so it can't
            // mistake the boundary for the root, or report the clone as
            // shallow), and walking history would fail; say why instead.
            let common = mdkg_coord_sqlite::git::common_dir(&top).map_err(git_error)?;
            if fs::symlink_metadata(common.join("shallow")).is_ok() {
                return Err(GraphError::failure(
                    Code::MigrateUnsupported,
                    "this is a shallow clone: the graph_id comes from the repository's first commit, which it lacks",
                )
                .with_hint("fetch the whole history first: git fetch --unshallow"));
            }
            let (sha, ms) = gitops::root_commit(&top).map_err(git_error)?;
            let path = repo.prefix.trim_end_matches('/');
            (super::mint::graph_id(&sha, ms, path), true)
        }
    };
    let v2 = match v2 {
        Some(g) => {
            let manifest = format!("{}{}", repo.prefix, identity_v2::GRAPH_JSON);
            Some(V2Source {
                manifest: TextFile {
                    content: read_text(&top, &manifest)?,
                    path: manifest,
                },
                uuids: g
                    .nodes
                    .iter()
                    .map(|n| (format!("{}{}", repo.prefix, n.path), n.node_id.to_string()))
                    .collect(),
            })
        }
        None => None,
    };

    let mut nodes = Vec::new();
    for (rel, _) in &listed {
        let path = format!("{}{rel}", repo.prefix);
        let content = read_text(&top, &path)?;
        nodes.push(NodeFile { path, content });
    }

    let mdkg_dirs: Vec<String> = cfg
        .workspaces
        .iter()
        .filter(|w| w.enabled)
        .map(|w| {
            let p = Path::new(&w.path).join(&w.mdkg_dir);
            let p = p.to_string_lossy().replace('\\', "/");
            let p = p.trim_start_matches("./").to_string();
            format!("{}{p}", repo.prefix)
        })
        .collect();
    let mirrors: Vec<String> = cfg
        .skill_mirror_targets
        .iter()
        .map(|m| format!("{}{}", repo.prefix, m.trim_start_matches("./")))
        .collect();
    let tracked = gitops::tracked(&top, &[]).map_err(git_error)?;
    let mut events = Vec::new();
    let mut outside = Vec::new();
    let mut examples = Vec::new();
    let mut core_list = None;
    for path in &tracked {
        if mdkg_dirs
            .iter()
            .any(|d| *path == format!("{d}/core/core.md"))
        {
            core_list = Some(TextFile {
                path: path.clone(),
                content: read_text(&top, path)?,
            });
            continue;
        }
        let in_events = mdkg_dirs
            .iter()
            .any(|d| path.starts_with(&format!("{d}/work/events/")) && path.ends_with(".jsonl"));
        if in_events {
            events.push(TextFile {
                path: path.clone(),
                content: read_text(&top, path)?,
            });
            continue;
        }
        if !is_instruction(path, &mdkg_dirs, &mirrors) {
            continue;
        }
        // Only the graph's own repository part: a nested graph's prefix.
        if !path.starts_with(&repo.prefix) {
            continue;
        }
        let full = top.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_INSTRUCTION_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        if let Ok(content) = String::from_utf8(bytes) {
            // 0.6.0's generated guides at the graph folder's top: their ids
            // are examples.
            if mdkg_dirs.iter().any(|d| {
                path.strip_prefix(&format!("{d}/"))
                    .is_some_and(|r| !r.contains('/'))
            }) {
                examples.push(path.clone());
            }
            outside.push(TextFile {
                path: path.clone(),
                content,
            });
        }
    }

    let mut children = BTreeMap::new();
    if let Some(declared) = raw.get("subgraphs").and_then(Value::as_object) {
        for (alias, entry) in declared {
            let child = if subgraphs {
                entry
                    .get("source_path")
                    .and_then(Value::as_str)
                    .and_then(|p| child_ids(&root.join(p)))
            } else {
                None
            };
            children.insert(alias.clone(), child);
        }
    }

    let local: std::collections::BTreeSet<String> =
        listed.iter().map(|(_, id)| id.clone()).collect();
    // The first workspace's folder, in alias order (spec section 10.1).
    let mdkg_prefix = cfg
        .workspaces
        .iter()
        .filter(|w| w.enabled)
        .zip(&mdkg_dirs)
        .min_by(|(a, _), (b, _)| a.alias.cmp(&b.alias))
        .map(|(_, d)| format!("{d}/"))
        .unwrap_or_default();
    let foreign = foreign_nodes(&top, &mdkg_prefix, &local).map_err(git_error)?;
    let source = Source {
        base,
        graph_id,
        graph_id_minted: minted,
        v2,
        config: TextFile {
            path: config_rel,
            content: config_text,
        },
        nodes,
        events,
        core_list,
        outside,
        examples,
        mdkg_prefix,
        foreign,
        workspaces: cfg.workspaces.iter().map(|w| w.alias.clone()).collect(),
        subgraphs: children,
        // The graph's own root, which install and doctor read them from
        // (review of 6af00f9): a nested graph has its own.
        projections: mdkg_run::projection::upgraded(&top.join(&repo.prefix), &raw),
    };
    Ok((repo, source))
}

/// The nodes other local branches hold and this worktree doesn't, read
/// from each branch's tip: so a ref to a node that lives on an unmerged
/// branch resolves to the id that branch's own migration will give it.
fn foreign_nodes(
    top: &Path,
    mdkg_prefix: &str,
    local: &std::collections::BTreeSet<String>,
) -> std::io::Result<Vec<ForeignNode>> {
    // Every (path, blob) a tip holds: the same bytes at two paths are two
    // candidates, since a node's id derives from its path. Each commit is
    // read once, however many branches point at it, and only its graph
    // folders, however large the rest of the repository.
    let folders: Vec<String> = ["core", "design", "work"]
        .iter()
        .map(|d| format!("{mdkg_prefix}{d}"))
        .collect();
    let tips: std::collections::BTreeSet<String> = gitops::branch_tips(top)?
        .into_iter()
        .map(|(_, tip)| tip)
        .collect();
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for tip in tips {
        for (path, entry) in gitops::ls_tree_under(top, &tip, &folders)? {
            let Some(rel) = path.strip_prefix(mdkg_prefix) else {
                continue;
            };
            let in_docs = ["core/", "design/", "work/"]
                .iter()
                .any(|d| rel.starts_with(d));
            if entry.kind == "blob" && in_docs && rel.ends_with(".md") {
                seen.insert((rel.to_string(), entry.oid));
            }
        }
    }
    let oids: Vec<String> = seen
        .iter()
        .map(|(_, oid)| oid.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let blobs: BTreeMap<String, Vec<u8>> = oids
        .iter()
        .cloned()
        .zip(gitops::read_blobs(top, &oids)?)
        .collect();
    let mut out = Vec::new();
    for (path, oid) in seen {
        let Some(text) = blobs.get(&oid).and_then(|b| std::str::from_utf8(b).ok()) else {
            continue;
        };
        // By line, so a node another branch already migrated (with 0.7.0
        // keys the 0.6.0 grammar refuses) is read too.
        let fields = frontmatter_lines(text);
        let field = |k: &str| fields.get(k).map(|v| (*v).to_string());
        let (Some(id), Some(created)) = (field("id"), field("created")) else {
            continue;
        };
        let node_type = field("type").unwrap_or_default();
        // An identity v2 node keeps its UUID there too.
        let node_id = field("node_id").filter(|u| mdkg_format::ids::is_identity_uuid(u));
        let (legacy_id, id) = match field("legacy_id") {
            Some(legacy) => (legacy, Some(id)),
            None => (id, None),
        };
        // A node this worktree has by legacy id is only another node when
        // it has a UUID of its own (a v2 node_id, or a migrated id): refs
        // by UUID still name it.
        if local.contains(&legacy_id) && node_id.is_none() && id.is_none() {
            continue;
        }
        out.push(ForeignNode {
            legacy_id,
            id,
            node_id,
            node_type,
            status: field("status"),
            created,
            path,
        });
    }
    out.sort_by(|a, b| (&a.legacy_id, &a.path).cmp(&(&b.legacy_id, &b.path)));
    out.dedup();
    Ok(out)
}

/// A node file's frontmatter as `key: value` lines (scalars as written),
/// without the 0.6.0 key grammar; empty when it has none.
fn frontmatter_lines(text: &str) -> BTreeMap<&str, &str> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            out.insert(k.trim(), v.trim());
        }
    }
    out
}

/// A child graph's new ids: read from its nodes' `legacy_id`s when it's
/// already on 0.7.0, else by planning it read only. `None` when it isn't
/// there or can't be read.
fn child_ids(dir: &Path) -> Option<ChildGraph> {
    if !config::has_config(dir) {
        return None;
    }
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(config::CONFIG_PATH)).ok()?).ok()?;
    if let Some(graph_id) = raw.get("graph_id").and_then(Value::as_str) {
        return migrated_ids(dir, graph_id);
    }
    let (_, source) = gather(dir, false).ok()?;
    let p = plan::plan(&source).ok()?;
    Some(ChildGraph {
        graph_id: p.graph_id,
        ids: p
            .nodes
            .into_iter()
            .filter_map(|n| Some((n.legacy_id, n.id?)))
            .collect(),
    })
}

/// A migrated graph's `legacy_id` to `id` map, from its node files'
/// frontmatter (`.mdkg/{core,design,work}`).
fn migrated_ids(dir: &Path, graph_id: &str) -> Option<ChildGraph> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), out);
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "md") {
                out.push(entry.path());
            }
        }
    }
    let mut files = Vec::new();
    for folder in ["core", "design", "work"] {
        walk(&dir.join(".mdkg").join(folder), &mut files);
    }
    let mut ids = BTreeMap::new();
    for file in files {
        // Read by line, not with the 0.6.0 grammar, which refuses 0.7.0's
        // `x-<ns>.<field>` keys: only `id` and `legacy_id` matter here.
        let text = std::fs::read_to_string(&file).ok()?;
        let fields = frontmatter_lines(&text);
        let (id, legacy) = (
            fields.get("id").map(|v| (*v).to_string()),
            fields.get("legacy_id").map(|v| (*v).to_string()),
        );
        if let (Some(legacy), Some(id)) = (legacy, id) {
            ids.insert(legacy, id);
        }
    }
    Some(ChildGraph {
        graph_id: graph_id.to_string(),
        ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_files() {
        let dirs = vec![".mdkg".to_string()];
        let mirrors = vec![".claude/skills".to_string()];
        for yes in [
            "AGENTS.md",
            "sub/CLAUDE.md",
            ".mdkg/README.md",
            ".mdkg/skills/x/SKILL.md",
            ".claude/skills/x/SKILL.md",
            "scripts/check.sh",
        ] {
            assert!(is_instruction(yes, &dirs, &mirrors), "{yes}");
        }
        for no in [
            ".mdkg/work/task-1.md",
            ".mdkg/core/rule-1.md",
            ".mdkg/core/core.md",
            "README.md",
            "src/scripts/x.sh",
            "scriptsx/y",
        ] {
            assert!(!is_instruction(no, &dirs, &mirrors), "{no}");
        }
    }
}
