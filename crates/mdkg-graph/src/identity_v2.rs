//! Reading 0.6.0's identity format v2, for `migrate` only.
//!
//! A v2 graph has config `schema_version` 2, a `.mdkg/graph.json` manifest
//! naming the graph's UUID, and in every node `graph_id` and `node_id`
//! keys; refs to its nodes are written `mdkg://<graph uuid>/<node uuid>`.
//! Format 0.7 keeps those UUIDs (format 0.7, section 10.2):
//!
//! - `id: <type>-<number>` with `node_id: <uuid>` becomes `id: <type>-<uuid>`,
//!   with the original numbered id in `legacy_id`;
//! - the graph's UUID becomes the config's `graph_id`;
//! - the per-node `graph_id` and `node_id` keys are dropped;
//! - `mdkg://<graph>/<uuid>` becomes `<type>-<uuid>` for a node of this
//!   graph, and `mdkg://<graph>/<type>-<uuid>` for another graph's.
//!
//! Every other command refuses a v2 graph ([`crate::config::load`]).

use std::collections::HashMap;
use std::path::Path;

use mdkg_format::node::{FormatVersion, ParseOptions, parse_node};
use mdkg_format::{Code, NodeId, Uuid};
use serde_json::Value;

use crate::config::{self, Config};
use crate::error::GraphError;
use crate::index::{read_document, relative_path};
use crate::{discovery, templates};

/// The manifest's path in a v2 graph.
pub const GRAPH_JSON: &str = ".mdkg/graph.json";

/// One node of a v2 graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Node {
    /// Graph-relative path, with `/` separators.
    pub path: String,
    pub ws: String,
    /// Its numbered 0.6.0 id: the 0.7 node's `legacy_id`.
    pub legacy_id: String,
    pub node_type: String,
    pub node_id: Uuid,
}

impl V2Node {
    /// Its format 0.7 id: its type and its v2 UUID.
    pub fn new_id(&self) -> Result<NodeId, GraphError> {
        NodeId::new(&self.node_type, self.node_id).map_err(|e| {
            GraphError::failure(Code::NodeBadIdentity, e.to_string()).at(self.path.clone(), None)
        })
    }
}

/// A v2 graph, as `migrate` needs it.
#[derive(Debug, Clone)]
pub struct V2Graph {
    pub graph_id: Uuid,
    pub config: Config,
    pub nodes: Vec<V2Node>,
    by_uuid: HashMap<Uuid, usize>,
}

/// Whether `root` holds a v2 graph: a `.mdkg/graph.json`, or config
/// `schema_version` 2.
pub fn is_identity_v2(root: &Path) -> bool {
    if root.join(GRAPH_JSON).exists() {
        return true;
    }
    std::fs::read_to_string(root.join(config::CONFIG_PATH))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("schema_version")?.as_f64())
        == Some(2.0)
}

fn bad(code: Code, path: &str, message: impl Into<String>) -> GraphError {
    GraphError::failure(code, message.into()).at(path.to_string(), None)
}

/// The graph UUID `.mdkg/graph.json` names: `format` "mdkg-graph",
/// `format_version` 2 and a `graph_id`.
fn manifest_graph_id(root: &Path) -> Result<Uuid, GraphError> {
    let text = std::fs::read_to_string(root.join(GRAPH_JSON)).map_err(|e| {
        bad(
            Code::ConfigUnreadable,
            GRAPH_JSON,
            format!("failed to read the v2 manifest: {e}"),
        )
    })?;
    let v: Value = serde_json::from_str(&text).map_err(|e| {
        bad(
            Code::ConfigUnreadable,
            GRAPH_JSON,
            format!("the v2 manifest is not valid JSON: {e}"),
        )
    })?;
    if v.get("format").and_then(Value::as_str) != Some("mdkg-graph")
        || v.get("format_version").and_then(Value::as_f64) != Some(2.0)
    {
        return Err(bad(
            Code::ConfigInvalid,
            GRAPH_JSON,
            "the manifest isn't an mdkg-graph format_version 2 manifest",
        ));
    }
    v.get("graph_id")
        .and_then(Value::as_str)
        .and_then(|g| Uuid::parse(g).ok())
        .ok_or_else(|| {
            bad(
                Code::NodeBadIdentity,
                GRAPH_JSON,
                "the manifest's graph_id isn't a lowercase UUID",
            )
        })
}

/// Reads a v2 graph: its manifest, config and every node's identity.
pub fn read(root: &Path) -> Result<V2Graph, GraphError> {
    let graph_id = manifest_graph_id(root)?;
    let config = config::load_identity_v2(root)?;
    let schemas = templates::load(root, &config)?.schemas;
    let opts = ParseOptions {
        work_status: &config.status_enum,
        priority_min: config.priority_min,
        priority_max: config.priority_max,
        templates: &schemas,
        allow_extension_keys: true,
        format: FormatVersion::Legacy,
    };
    let documents = discovery::discover(root, &config)?;
    let mut nodes = Vec::new();
    let mut by_uuid = HashMap::new();
    for (ws, files) in &documents.by_workspace {
        for file in files {
            if file.file_name().is_some_and(|n| n == "core.md")
                && file
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "core")
            {
                continue;
            }
            let rel = relative_path(root, file);
            let node = parse_node(&read_document(file)?, &rel, &opts)
                .map_err(|e| GraphError::from_format(&e, root))?;
            // Every v2 node carries the graph's identity and its own.
            let Some(identity) = node.identity else {
                return Err(bad(
                    Code::NodeBadIdentity,
                    &rel,
                    "a v2 node needs graph_id and node_id",
                ));
            };
            if Uuid::parse(&identity.graph_id).ok() != Some(graph_id) {
                return Err(bad(
                    Code::NodeBadIdentity,
                    &rel,
                    "the node's graph_id isn't the manifest's",
                ));
            }
            let node_id = Uuid::parse(&identity.node_id).map_err(|_| {
                bad(
                    Code::NodeBadIdentity,
                    &rel,
                    "node_id isn't a lowercase UUID",
                )
            })?;
            if let Some(first) = by_uuid.insert(node_id, nodes.len()) {
                let first: &V2Node = &nodes[first];
                return Err(
                    bad(Code::NodeDuplicateId, &rel, "two nodes have one node_id")
                        .with_hint(format!("the other is {}", first.path)),
                );
            }
            nodes.push(V2Node {
                path: rel,
                ws: ws.alias.clone(),
                legacy_id: node.id,
                node_type: node.node_type,
                node_id,
            });
        }
    }
    Ok(V2Graph {
        graph_id,
        config,
        nodes,
        by_uuid,
    })
}

impl V2Graph {
    /// The node with this v2 UUID.
    pub fn node(&self, node_id: Uuid) -> Option<&V2Node> {
        self.by_uuid.get(&node_id).map(|i| &self.nodes[*i])
    }

    /// The format 0.7 form of a v2 identity ref, `mdkg://<graph>/<uuid>`:
    /// `<type>-<uuid>` for this graph's node, `mdkg://<graph>/<type>-<uuid>`
    /// for another graph's, whose type `foreign` gives. `None` when `value`
    /// isn't a v2 identity ref, or names a node nobody knows.
    pub fn map_ref(
        &self,
        value: &str,
        foreign: impl Fn(Uuid, Uuid) -> Option<String>,
    ) -> Option<String> {
        let r = mdkg_format::ids::parse_identity_ref(value)?;
        let (graph, node) = (
            Uuid::parse(&r.graph_id).ok()?,
            Uuid::parse(&r.node_id).ok()?,
        );
        if graph == self.graph_id {
            return Some(self.node(node)?.new_id().ok()?.to_string());
        }
        let node_type = foreign(graph, node)?;
        let id = NodeId::new(&node_type, node).ok()?;
        Some(format!("mdkg://{graph}/{id}"))
    }
}
