//! The in-memory index: every node, keyed by qid (`workspace:id`), with
//! workspace-qualified edges and reverse edges. It is derived data, rebuilt
//! on every run and never committed with the authoritative graph files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use mdkg_format::node::{EdgeMap, FormatVersion, NodeIdentity, ParseOptions, parse_node};
use mdkg_format::{Code, Frontmatter, NodeId, NodeRef, Uuid7};

use crate::config::Config;
use crate::discovery;
use crate::error::GraphError;
use crate::graph_checks;
use crate::templates;

/// One indexed node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexNode {
    pub id: String,
    /// Format 0.7: the 0.6.0 id a migrated node had.
    pub legacy_id: Option<String>,
    pub identity: Option<NodeIdentity>,
    pub qid: String,
    pub ws: String,
    pub node_type: String,
    pub title: String,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub created: String,
    pub updated: String,
    pub tags: Vec<String>,
    pub owners: Vec<String>,
    pub links: Vec<String>,
    pub artifacts: Vec<String>,
    pub refs: Vec<String>,
    pub aliases: Vec<String>,
    pub skills: Vec<String>,
    /// `scope`, as written.
    pub scope: Vec<String>,
    pub attributes: Frontmatter,
    /// Path relative to the graph root, with `/` separators.
    pub path: String,
    pub edges: EdgeMap,
    /// The body exactly as parsed (CRLF normalized, not trimmed).
    pub body: String,
}

/// Edge kinds that get reverse edges.
pub const EDGE_KEYS: &[&str] = &[
    "epic",
    "parent",
    "prev",
    "next",
    "relates",
    "blocked_by",
    "blocks",
    "context_refs",
    "evidence_refs",
];

#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Nodes in insertion order (workspace order, then file order).
    pub nodes: Vec<IndexNode>,
    by_qid: HashMap<String, usize>,
    /// edge kind -> target qid -> sorted source qids.
    pub reverse_edges: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Enabled workspace aliases with documents, sorted.
    pub workspaces: Vec<String>,
    /// Format 0.7: the graph's `graph_id`. Only a format 0.7 graph has one,
    /// and only then are nodes found by UUID.
    pub graph_id: Option<Uuid7>,
    /// Format 0.7: each node's UUID.
    by_uuid: HashMap<Uuid7, usize>,
    /// Format 0.7 refs whose id names a type the target no longer has.
    pub stale_refs: Vec<StaleRef>,
    /// Profile C (`work.committed: none`): work-type nodes live in the
    /// store's folder, so a goal's pointer to one may be absent from a
    /// checkout (a fresh clone) without anything being wrong. Set by
    /// [`build`]; false for an index assembled by hand.
    pub private_work: bool,
}

/// What a value names in a format 0.7 graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefTarget<'a> {
    /// A node in this graph; `stale` when the id's type isn't the node's.
    Local { node: &'a IndexNode, stale: bool },
    /// A ref into this graph that no node has.
    Missing,
    /// A graph ref into another graph: not checked (section 5).
    Foreign,
    /// Not a node ref, or not a format 0.7 graph.
    NotARef,
}

/// A ref whose id has a stale type (format 0.7, section 4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRef {
    pub source: String,
    pub key: &'static str,
    /// The id as written.
    pub written: String,
    pub target: String,
}

/// Every ref-bearing value a node wrote outside its edges, by key:
/// `refs`, `scope`, `supersedes` and the goal refs (section 5).
pub fn other_refs(node: &IndexNode) -> Vec<(&'static str, &str)> {
    let mut out: Vec<(&'static str, &str)> = Vec::new();
    out.extend(node.refs.iter().map(|v| ("refs", v.as_str())));
    out.extend(node.scope.iter().map(|v| ("scope", v.as_str())));
    for key in ["supersedes", "active_node", "last_active_node"] {
        if let Some(v) = node.attributes.get(key).and_then(|v| v.as_str()) {
            out.push((key, v));
        }
    }
    if let Some(list) = node.attributes.get("scope_refs").and_then(|v| v.as_list()) {
        out.extend(list.iter().map(|v| ("scope_refs", v.as_str())));
    }
    out
}

impl IndexNode {
    /// Indexes a parsed node under workspace `ws`, qualifying its edges.
    pub fn from_parsed(node: mdkg_format::Node, ws: &str, path: String) -> Self {
        let edges = qualify_edges(&node.edges, ws);
        IndexNode {
            qid: format!("{ws}:{}", node.id),
            id: node.id,
            legacy_id: node.legacy_id,
            identity: node.identity,
            ws: ws.to_string(),
            node_type: node.node_type,
            title: node.title,
            status: node.status,
            priority: node.priority,
            created: node.created,
            updated: node.updated,
            tags: node.tags,
            owners: node.owners,
            links: node.links,
            artifacts: node.artifacts,
            refs: node.refs,
            aliases: node.aliases,
            skills: node.skills,
            scope: node.scope,
            attributes: node.attributes,
            path,
            edges,
            body: node.body,
        }
    }
}

impl Index {
    /// Builds lookup tables and reverse edges for nodes already in order.
    pub fn assemble(nodes: Vec<IndexNode>, workspaces: Vec<String>) -> Self {
        Self::assemble_in(nodes, workspaces, None)
    }

    /// [`Index::assemble`] for a graph with this `graph_id`. Edges naming a
    /// format 0.7 id (or a ref to one in this graph) point at the node with
    /// that UUID, in whichever workspace it is and whatever type the id
    /// says; stale types are listed in `stale_refs`.
    pub fn assemble_in(
        nodes: Vec<IndexNode>,
        mut workspaces: Vec<String>,
        graph_id: Option<Uuid7>,
    ) -> Self {
        let mut index = Index {
            by_qid: nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (n.qid.clone(), i))
                .collect(),
            // A legacy graph's portable ids may look like 0.7 ids
            // (`spec-<uuid>`): they're found by id only.
            by_uuid: if graph_id.is_some() {
                nodes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| Some((NodeId::parse(&n.id).ok()?.uuid(), i)))
                    .collect()
            } else {
                HashMap::new()
            },
            graph_id,
            nodes,
            ..Index::default()
        };
        workspaces.sort();
        index.workspaces = workspaces;
        index.link_by_uuid();
        for node in &index.nodes {
            for (key, target) in edge_targets(&node.edges) {
                index
                    .reverse_edges
                    .entry(key.to_string())
                    .or_default()
                    .entry(target.to_string())
                    .or_default()
                    .push(node.qid.clone());
            }
        }
        for targets in index.reverse_edges.values_mut() {
            for sources in targets.values_mut() {
                sources.sort();
            }
        }
        index
    }

    /// What `value`, as a node wrote it, names in a format 0.7 graph.
    pub fn target(&self, value: &str) -> RefTarget<'_> {
        if self.graph_id.is_none() {
            return RefTarget::NotARef;
        }
        let Ok(r) = NodeRef::parse(value) else {
            return RefTarget::NotARef;
        };
        if r.graph.is_some() && r.graph != self.graph_id {
            return RefTarget::Foreign;
        }
        match self.by_uuid(r.id.uuid()) {
            Some(node) => RefTarget::Local {
                node,
                stale: r.id.node_type() != node.node_type,
            },
            None => RefTarget::Missing,
        }
    }

    /// Points edges that name format 0.7 refs at their nodes' qids, and
    /// lists every ref with a stale type. An edge naming a missing node
    /// becomes `<ws>:<id>`, which the edge checks report.
    fn link_by_uuid(&mut self) {
        if self.graph_id.is_none() {
            return;
        }
        let mut stale = Vec::new();
        let mut relinked: Vec<EdgeMap> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            // Edges were qualified as `<ws>:<id>`; the id is what was written.
            let written = |v: &str| -> String {
                v.strip_prefix(&format!("{}:", node.ws))
                    .filter(|rest| NodeId::parse(rest).is_ok())
                    .unwrap_or(v)
                    .to_string()
            };
            let mut link = |key: &'static str, v: &str| -> Option<String> {
                let w = written(v);
                match self.target(&w) {
                    RefTarget::Local {
                        node: t,
                        stale: is_stale,
                    } => {
                        if is_stale {
                            stale.push(stale_ref(node, key, &w, t));
                        }
                        Some(t.qid.clone())
                    }
                    RefTarget::Missing => NodeRef::parse(&w)
                        .ok()
                        .map(|r| format!("{}:{}", node.ws, r.id)),
                    RefTarget::Foreign | RefTarget::NotARef => None,
                }
            };
            let mut e = node.edges.clone();
            for (key, slot) in [
                ("epic", &mut e.epic),
                ("parent", &mut e.parent),
                ("prev", &mut e.prev),
                ("next", &mut e.next),
            ] {
                if let Some(q) = slot.as_deref().and_then(|v| link(key, v)) {
                    *slot = Some(q);
                }
            }
            for (key, list) in [
                ("relates", &mut e.relates),
                ("blocked_by", &mut e.blocked_by),
                ("blocks", &mut e.blocks),
                ("context_refs", &mut e.context_refs),
                ("evidence_refs", &mut e.evidence_refs),
            ] {
                for v in list.iter_mut() {
                    if let Some(q) = link(key, v) {
                        *v = q;
                    }
                }
            }
            for (key, v) in other_refs(node) {
                if let RefTarget::Local {
                    node: t,
                    stale: true,
                } = self.target(v)
                {
                    stale.push(stale_ref(node, key, v, t));
                }
            }
            relinked.push(e);
        }
        for (node, edges) in self.nodes.iter_mut().zip(relinked) {
            node.edges = edges;
        }
        self.stale_refs = stale;
    }

    /// The node with this UUID (format 0.7).
    pub fn by_uuid(&self, uuid: Uuid7) -> Option<&IndexNode> {
        self.by_uuid.get(&uuid).map(|i| &self.nodes[*i])
    }

    pub fn get(&self, qid: &str) -> Option<&IndexNode> {
        self.by_qid.get(qid).map(|i| &self.nodes[*i])
    }

    pub fn contains(&self, qid: &str) -> bool {
        self.by_qid.contains_key(qid)
    }

    pub fn reverse(&self, edge: &str, target: &str) -> &[String] {
        self.reverse_edges
            .get(edge)
            .and_then(|m| m.get(target))
            .map_or(&[], Vec::as_slice)
    }
}

fn qualify(value: &str, ws: &str) -> String {
    if value.contains(':') {
        value.to_string()
    } else {
        format!("{ws}:{value}")
    }
}

fn qualify_edges(edges: &EdgeMap, ws: &str) -> EdgeMap {
    let one = |v: &Option<String>| v.as_deref().map(|v| qualify(v, ws));
    let many = |v: &[String]| v.iter().map(|v| qualify(v, ws)).collect();
    EdgeMap {
        epic: one(&edges.epic),
        parent: one(&edges.parent),
        prev: one(&edges.prev),
        next: one(&edges.next),
        relates: many(&edges.relates),
        blocked_by: many(&edges.blocked_by),
        blocks: many(&edges.blocks),
        context_refs: many(&edges.context_refs),
        evidence_refs: many(&edges.evidence_refs),
    }
}

/// Every (edge kind, target) pair a node has, in reverse-edge key order.
pub fn edge_targets(edges: &EdgeMap) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    for (key, value) in [
        ("epic", &edges.epic),
        ("parent", &edges.parent),
        ("prev", &edges.prev),
        ("next", &edges.next),
    ] {
        if let Some(v) = value {
            out.push((key, v.as_str()));
        }
    }
    for (key, values) in [
        ("relates", &edges.relates),
        ("blocked_by", &edges.blocked_by),
        ("blocks", &edges.blocks),
        ("context_refs", &edges.context_refs),
        ("evidence_refs", &edges.evidence_refs),
    ] {
        out.extend(values.iter().map(|v| (key, v.as_str())));
    }
    out
}

/// Reads a document as UTF-8, replacing invalid bytes like Node's decoder.
pub fn read_document(file: &Path) -> Result<String, GraphError> {
    let bytes = std::fs::read(file).map_err(|e| {
        GraphError::failure(
            Code::IoReadFailed,
            format!("failed to read {}: {e}", file.display()),
        )
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `path.relative(root, file)` with `/` separators.
pub fn relative_path(root: &Path, file: &Path) -> String {
    // A file outside the graph root (profile C's store work folder) keeps
    // its absolute path, which joins onto the root as itself.
    let Ok(inside) = file.strip_prefix(root) else {
        return file.to_string_lossy().into_owned();
    };
    inside
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A path label made relative to `root` when it lies inside it.
pub fn relative_label(root: &Path, label: &str) -> String {
    let path = Path::new(label);
    if path.is_absolute() && path.starts_with(root) {
        relative_path(root, path)
    } else {
        label.to_string()
    }
}

/// Builds and validates the index. The first error fails the build, as the
/// reference does for every read command.
pub fn build(root: &Path, config: &Config) -> Result<Index, GraphError> {
    if root.join(".mdkg/graph.json").exists() {
        return Err(GraphError::failure(
            Code::ConfigGraphV2Unsupported,
            "graph format v2 (.mdkg/graph.json) needs migrating to format 0.7",
        )
        .at(".mdkg/graph.json", None)
        .with_hint(crate::config::identity_v2_hint()));
    }
    let schemas = templates::load(root, config)?.schemas;
    let opts = ParseOptions {
        work_status: &config.status_enum,
        priority_min: config.priority_min,
        priority_max: config.priority_max,
        templates: &schemas,
        allow_extension_keys: true,
        format: config.format,
    };
    let documents = discovery::discover(root, config)?;
    let mut nodes = Vec::new();
    let mut workspaces = Vec::new();
    // Format 0.7 ids are unique in the whole graph, by UUID.
    let mut seen_uuids: HashMap<Uuid7, String> = HashMap::new();
    for (ws, files) in &documents.by_workspace {
        workspaces.push(ws.alias.clone());
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut seen_legacy: HashMap<String, String> = HashMap::new();
        for file in files {
            if file.file_name().is_some_and(|n| n == "core.md")
                && file
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "core")
            {
                continue;
            }
            let label = file.display().to_string();
            let content = read_document(file)?;
            let node = match parse_node(&content, &label, &opts) {
                Ok(node) => node,
                // Tolerant mode skips bad files, but never usage-class errors.
                Err(e) if config.tolerant && !e.is_usage() => continue,
                Err(e) => return Err(GraphError::from_format(&e, root)),
            };
            let rel = relative_path(root, file);
            if node.identity.is_some() {
                return Err(GraphError::failure(
                    Code::NodeIdentityUnsupported,
                    "identity-bearing node requires an explicit v2 graph manifest; no implicit migration",
                )
                .at(rel, None));
            }
            let uuid = NodeId::parse(&node.id).ok().map(|id| id.uuid());
            let first = match uuid {
                Some(u) if config.format == FormatVersion::V07 => seen_uuids.get(&u),
                _ => seen.get(&node.id),
            };
            if let Some(first) = first {
                if config.tolerant {
                    continue; // The first file with this id wins.
                }
                let d = mdkg_format::Diagnostic::error(
                    Code::NodeDuplicateId,
                    format!("duplicate id {} in workspace {}", node.id, ws.alias),
                )
                .at(rel, None)
                .for_node(format!("{}:{}", ws.alias, node.id))
                .also_at(first.clone(), None)
                .with_hint("rename one of the ids, or remove the duplicate file");
                return Err(GraphError::new(crate::error::ErrorKind::Failure, d));
            }
            if let Some(legacy) = &node.legacy_id {
                if let Some(first) = seen_legacy.get(legacy) {
                    if !config.tolerant {
                        return Err(GraphError::new(
                            crate::error::ErrorKind::Failure,
                            duplicate_legacy_id(legacy, &ws.alias, &node.id, &rel, first),
                        ));
                    }
                } else {
                    seen_legacy.insert(legacy.clone(), rel.clone());
                }
            }
            if let Some(u) = uuid {
                seen_uuids.insert(u, rel.clone());
            }
            seen.insert(node.id.clone(), rel.clone());
            nodes.push(IndexNode::from_parsed(node, &ws.alias, rel));
        }
    }
    let mut index = Index::assemble_in(nodes, workspaces, config.graph_id);
    index.private_work = config.work_committed == crate::config::WorkCommitted::None;

    let external: HashSet<&str> = config.subgraphs.iter().map(String::as_str).collect();
    let errors = graph_checks::collect(&index, config.tolerant, &external);
    if let Some(first) = errors.into_iter().next() {
        return Err(GraphError::new(crate::error::ErrorKind::Failure, first));
    }
    Ok(index)
}

/// Two nodes in one workspace with the same `legacy_id`.
pub fn duplicate_legacy_id(
    legacy: &str,
    ws: &str,
    id: &str,
    rel: &str,
    first: &str,
) -> mdkg_format::Diagnostic {
    mdkg_format::Diagnostic::error(
        Code::NodeDuplicateLegacyId,
        format!("duplicate legacy_id {legacy} in workspace {ws}"),
    )
    .at(rel.to_string(), None)
    .for_node(format!("{ws}:{id}"))
    .also_at(first.to_string(), None)
    .with_hint("remove legacy_id from the node that didn't have that 0.6.0 id")
}

fn stale_ref(source: &IndexNode, key: &'static str, written: &str, target: &IndexNode) -> StaleRef {
    StaleRef {
        source: source.qid.clone(),
        key,
        written: NodeRef::parse(written).map_or_else(|_| written.to_string(), |r| r.id.to_string()),
        target: target.qid.clone(),
    }
}
