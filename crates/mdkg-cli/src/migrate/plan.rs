//! The migration plan: a pure function from a 0.6.0 graph's files to every
//! file change and log record the migration makes.
//!
//! Nothing here reads the disk. [`super::gather`] collects a [`Source`];
//! [`plan`] turns it into a [`Plan`]; the plan's hash is the sha256 of its
//! canonical JSON, so `--dry-run` and `--apply <hash>` agree exactly when
//! they see the same graph.

use std::collections::{BTreeMap, BTreeSet};

use mdkg_format::Digest;
use mdkg_format::frontmatter;
use mdkg_format::js;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::classify::{Plane, SectionKind, classify};
use super::mint;
use super::records::{LEGACY_ACTOR, Record, RecordKind};
use super::rewrite::{self, Token};

/// The plan format; part of the hash.
pub const PLAN_FORMAT: u32 = 1;

/// Where a graph's work history goes: checkpoints, finished tests, event
/// logs, and progress and review-ledger sections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum History {
    /// It stays in git, for a graph that commits its work (`work.location`
    /// "git", which a 0.6 config with neither storage key is): checkpoints
    /// and finished tests stay nodes with new ids, and sections and event
    /// logs stay as they are. No log records are written.
    Git,
    /// It moves to the coordinator store's log, as 0.7.0 migrated, and
    /// leaves git. The default when a stored plan names neither, since
    /// 0.7.0's plans didn't: a plan 0.7.0 stored in a migration's journal
    /// keeps its hash when this build resumes it. A 0.7.0 dry run's hash
    /// that no apply started doesn't apply here (the config change differs:
    /// `work.location` is said now); plan again.
    #[default]
    Store,
}

impl History {
    pub fn as_str(self) -> &'static str {
        match self {
            History::Git => "git",
            History::Store => "store",
        }
    }

    fn is_store(&self) -> bool {
        *self == History::Store
    }
}

/// A node file as the graph holds it.
#[derive(Debug, Clone, Default)]
pub struct NodeFile {
    /// Relative to the repository's top.
    pub path: String,
    pub content: String,
    /// The alias of the workspace it's in (`root`).
    pub ws: String,
}

/// Any other text file the migration may change.
#[derive(Debug, Clone, Default)]
pub struct TextFile {
    /// Relative to the repository's top.
    pub path: String,
    pub content: String,
    /// For an event log or a core list, its workspace's alias.
    pub ws: String,
}

/// A subgraph's new ids, as planning the child graph computes them.
#[derive(Debug, Clone, Default)]
pub struct ChildGraph {
    pub graph_id: String,
    /// Legacy id to new id.
    pub ids: BTreeMap<String, String>,
}

/// A node another branch holds and this one doesn't, as read from that
/// branch's tip: refs to it resolve to the id its own branch's migration
/// gives it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignNode {
    pub legacy_id: String,
    /// Its id, when its branch is already on 0.7.0 (or keeps an identity
    /// v2 UUID).
    pub id: Option<String>,
    /// Its identity v2 `node_id`, which `mdkg://` refs to it name.
    pub node_id: Option<String>,
    pub node_type: String,
    pub status: Option<String>,
    pub created: String,
    /// The path its id is minted from ([`Source::mint_path`]):
    /// `work/<legacy-id>-<slug>.md` in the root workspace.
    pub path: String,
    /// Its workspace's alias.
    pub ws: String,
}

/// What an identity format v2 graph brings (format 0.7, section 10.2).
#[derive(Debug, Clone, Default)]
pub struct V2Source {
    /// `.mdkg/graph.json`, which the migration removes.
    pub manifest: TextFile,
    /// Each node's `node_id`, which it keeps, by the node's path.
    pub uuids: BTreeMap<String, String>,
}

/// Everything the planner reads.
#[derive(Debug, Clone, Default)]
pub struct Source {
    /// The commit the plan starts from.
    pub base: String,
    /// The graph's id: minted (`graph_id_minted`), or an identity v2
    /// graph's own.
    pub graph_id: String,
    pub graph_id_minted: bool,
    /// Set for an identity format v2 graph.
    pub v2: Option<V2Source>,
    /// `.mdkg/config.json`.
    pub config: TextFile,
    pub nodes: Vec<NodeFile>,
    /// Every `events.jsonl` under the work folders.
    pub events: Vec<TextFile>,
    /// Each workspace's `core/core.md`, the core list: one id per line.
    pub core_lists: Vec<TextFile>,
    /// Instruction files outside the node folders: AGENTS.md, CLAUDE.md,
    /// skills and scripts. Their changes form the `--rewrite-refs` patch.
    pub outside: Vec<TextFile>,
    /// Of those, the ones whose ids are examples, not refs (0.6.0's
    /// generated `.mdkg/*.md` guides): only their commands are rewritten.
    pub examples: Vec<String>,
    /// The graph folder, relative to the repository's top (`.mdkg/`).
    pub mdkg_prefix: String,
    /// The graph's root, relative to the repository's top (`` or `sub/`).
    pub graph_prefix: String,
    /// Each enabled workspace's folder, relative to the repository's top
    /// (`.mdkg/`, `aux/.mdkg/`), by alias.
    pub folders: BTreeMap<String, String>,
    /// The workspace instruction files outside the graph speak for: the
    /// one at the graph's root.
    pub home_ws: String,
    /// Nodes only other branches hold.
    pub foreign: Vec<ForeignNode>,
    /// The graph's workspace aliases (`root`).
    pub workspaces: Vec<String>,
    /// Subgraph aliases, with the child's ids when the child could be
    /// planned.
    pub subgraphs: BTreeMap<String, Option<ChildGraph>>,
    /// The projections the repository already has (the AGENTS.md block,
    /// each MCP config, each skill mirror), which the config lists from
    /// now on.
    pub projections: Vec<&'static str>,
    /// Where the work history goes.
    pub history: History,
    /// Nodes already on 0.7 that the plan doesn't hold but refs may name:
    /// legacy id to id. Restoring history plans only the nodes it brings
    /// back, and refs to the rest resolve to the ids they have.
    pub known_ids: BTreeMap<String, String>,
    /// Why the work history goes where it does, for the plan to say.
    pub history_reason: String,
    /// Against a migrated main ([`super::against`]): main's ids, by
    /// `<workspace>:<legacy id>`, for the nodes it has; and by path, main's
    /// content for each graph file this branch hasn't changed since it
    /// forked (`None`: main has none there).
    pub id_overrides: BTreeMap<String, String>,
    pub adopt: BTreeMap<String, Option<String>>,
    /// By path, the nodes main made after the fork, added as it has them.
    pub added: BTreeMap<String, String>,
    /// By `<workspace>:<legacy id>`, main's ids of the nodes in `added`.
    pub added_ids: BTreeMap<String, String>,
    /// The records main's migration made, planned again from the commit
    /// it migrated, for a store that doesn't have them (a clone that never
    /// ran it): imported with this branch's own, each once.
    pub replayed: Vec<Record>,
    /// By `<workspace>:<legacy id>`, this branch's nodes whose legacy id
    /// main gave a different node after the fork, with main's id for it:
    /// they leave the handle to main's (no `legacy_id`), and never share
    /// its id.
    pub collisions: BTreeMap<String, String>,
    pub against: Option<super::against::Against>,
    /// Warnings found while gathering it, said with the plan's own.
    pub warnings: Vec<PlanWarning>,
}

impl Source {
    /// The path a node's id is minted from (format 0.7, section 10.1): the
    /// file's path relative to the graph's `.mdkg` folder. That's its path
    /// inside it for the workspace there (`work/task-19-x.md`), and starts
    /// `../` for any other (`../aux/.mdkg/work/task-19-x.md`), which no
    /// file inside it can: two workspaces' `task-1` get different ids.
    pub fn mint_path(&self, path: &str, ws: &str) -> String {
        mint_path(
            &self.folders,
            &self.graph_prefix,
            &self.mdkg_prefix,
            path,
            ws,
        )
    }
}

/// [`Source::mint_path`], from its parts.
pub fn mint_path(
    folders: &BTreeMap<String, String>,
    graph_prefix: &str,
    mdkg_prefix: &str,
    path: &str,
    ws: &str,
) -> String {
    let root = format!("{graph_prefix}.mdkg/");
    match folders.get(ws) {
        Some(folder) if *folder == root => path.strip_prefix(folder.as_str()).map(str::to_string),
        Some(_) => path.strip_prefix(graph_prefix).map(|p| format!("../{p}")),
        None => path.strip_prefix(mdkg_prefix).map(str::to_string),
    }
    .unwrap_or_else(|| path.to_string())
}

/// What happens to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Write,
    Delete,
}

/// One file change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileChange {
    /// Relative to the repository's top.
    pub path: String,
    pub action: Action,
    /// sha256 of the file before (empty for a file the change makes) and
    /// after.
    pub before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The new content (in the plan file, not the summary).
    #[serde(skip)]
    pub content: Option<String>,
}

/// One node's fate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeChange {
    pub legacy_id: String,
    /// Its new id, for a node that stays a node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub path: String,
    /// knowledge, checkpoint, check-result or tier-c.
    pub plane: String,
    /// The record it became, for a node that left git.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
}

/// A workspace the migration converts: its config entry stays as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanWorkspace {
    pub alias: String,
    /// Its folder, relative to the repository's top (`aux/.mdkg/`).
    pub folder: String,
    /// How many of its nodes the plan converts.
    pub nodes: usize,
}

/// Something a reviewer should look at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct PlanWarning {
    /// A `migrate.*` code.
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

/// The whole plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub format: u32,
    pub base: String,
    pub graph_id: String,
    /// Where the work history goes. Left out for `store`, so a plan 0.7.0
    /// stored keeps its hash.
    #[serde(default, skip_serializing_if = "History::is_store")]
    pub history: History,
    /// Why the work history goes there.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub history_reason: String,
    /// Every enabled workspace the migration converts, in alias order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<PlanWorkspace>,
    /// What the plan took from a migrated main it was made against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub against: Option<super::against::Against>,
    pub nodes: Vec<NodeChange>,
    /// Changes inside the graph, made by `--apply`.
    pub files: Vec<FileChange>,
    /// Changes outside it, made only with `--rewrite-refs`.
    pub patch: Vec<FileChange>,
    pub records: Vec<Record>,
    pub warnings: Vec<PlanWarning>,
}

/// Why a graph can't be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub path: String,
    pub message: String,
}

impl Plan {
    /// The plan's hash: sha256 of its canonical JSON, with each file's new
    /// content included.
    pub fn hash(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        // The summary omits contents; the hash covers them through `after`.
        if let Some(obj) = value.as_object_mut() {
            obj.insert("hash_of".into(), json!("mdkg-migrate-plan"));
        }
        Digest::of_record(&value)
            .unwrap_or_else(|_| Digest::of_bytes(value.to_string().as_bytes()))
            .to_string()
    }
}

/// Keys whose values are never ids to rewrite.
const FIXED_KEYS: &[&str] = &[
    "tags",
    "owners",
    "aliases",
    "skills",
    "required_skills",
    "id",
    "legacy_id",
    "type",
    "status",
    "priority",
    "created",
    "updated",
    "checkpoint_kind",
    "goal_state",
];

/// An identity v2 node's identity keys, which its 0.7 id replaces.
fn is_v2_identity_key(key: &str) -> bool {
    matches!(key, "graph_id" | "node_id")
}

/// Keys whose values are refs on a node of `node_type`, as format 0.7's
/// parser reads them (section 5): an entry naming a node that left git is
/// dropped (the record lists who pointed at it). `context_refs` and
/// `evidence_refs` are refs on work items only, `supersedes` on decisions,
/// and `scope_refs`, `active_node` and `last_active_node` on goals. Every
/// other `*_refs` key is opaque ([`opaque_refs`]), and so is an extension
/// key, whatever its name (section 8).
fn is_edge_key(key: &str, node_type: &str) -> bool {
    if key.starts_with("x_") || key.starts_with("x-") {
        return false;
    }
    match key {
        "parent" | "epic" | "prev" | "next" | "relates" | "blocked_by" | "blocks" | "refs"
        | "scope" => true,
        "supersedes" => node_type == "dec",
        "context_refs" | "evidence_refs" => mdkg_format::node::WORK_TYPES.contains(&node_type),
        "scope_refs" | "active_node" | "last_active_node" => node_type == "goal",
        _ => false,
    }
}

/// Whether `key` holds strings the format doesn't interpret, of which
/// `migrate` rewrites only exact 0.6.0 ids (section 5): the `*_refs` keys
/// of loops and agent workflow types, and any other `*_refs` key that isn't
/// a ref on `node_type`.
fn is_opaque_refs_key(key: &str, node_type: &str) -> bool {
    key.ends_with("_refs")
        && !key.starts_with("x_")
        && !key.starts_with("x-")
        && !is_edge_key(key, node_type)
}

/// An opaque `*_refs` value with each item that is exactly the 0.6.0 id of
/// a node that stays replaced by its new id; every other item (a qualified
/// id, a URI, any string) stays as written, in place.
fn opaque_refs(value: &str, ids: &Ids<'_>) -> String {
    let one = |item: &str| {
        ids.local
            .get(&ids.here(item))
            .cloned()
            .unwrap_or_else(|| item.to_string())
    };
    match list_items(value) {
        Some(items) => format!(
            "[{}]",
            items.into_iter().map(one).collect::<Vec<_>>().join(", ")
        ),
        None => one(value),
    }
}

/// A frontmatter line, split.
struct Line<'a> {
    raw: &'a str,
    /// The line break it ended with.
    ending: &'a str,
    key: Option<&'a str>,
    value: &'a str,
}

/// A node file split into its frontmatter lines and body.
struct Split<'a> {
    open: &'a str,
    lines: Vec<Line<'a>>,
    close: &'a str,
    body: &'a str,
}

fn split<'a>(content: &'a str, path: &str) -> Result<Split<'a>, PlanError> {
    let bounds = frontmatter::source_bounds(content, path).map_err(|e| PlanError {
        path: path.to_string(),
        message: e.to_string(),
    })?;
    let head = &content[..bounds.body_start];
    let mut pieces: Vec<&str> = head.split_inclusive('\n').collect();
    if pieces.len() < 2 {
        return Err(PlanError {
            path: path.to_string(),
            message: "frontmatter has no closing line".into(),
        });
    }
    let close = pieces.pop().unwrap_or_default();
    let open = pieces.remove(0);
    let lines = pieces
        .into_iter()
        .map(|raw| {
            let bare = raw.trim_end_matches(['\n', '\r']);
            let ending = &raw[bare.len()..];
            match bare.split_once(':') {
                Some((k, v)) => Line {
                    raw,
                    ending,
                    // Trimmed as 0.6.0 trims (ECMAScript whitespace: a BOM and
                    // U+00A0 too).
                    key: Some(js::trim(k)),
                    value: js::trim(v),
                },
                None => Line {
                    raw,
                    ending,
                    key: None,
                    value: "",
                },
            }
        })
        .collect();
    Ok(Split {
        open,
        lines,
        close,
        body: &content[bounds.body_start..],
    })
}

/// A value as 0.7 writes what 0.6.0 read: a list as `[a, b]` with each
/// item trimmed, a `priority` as the integer 0.6.0's `parseInt` gave
/// (`3.7` is 3), anything else as it is (already trimmed).
fn canonical(key: &str, value: &str) -> String {
    if key == "priority"
        && let Some(n) = js::parse_int(value)
    {
        return n.to_string();
    }
    match list_items(value) {
        Some(items) => format!("[{}]", items.join(", ")),
        None => value.to_string(),
    }
}

/// A list value's items, as 0.6.0 splits them.
fn list_items(value: &str) -> Option<Vec<&str>> {
    let inner = js::trim(value.strip_prefix('[')?.strip_suffix(']')?);
    if inner.is_empty() {
        return Some(Vec::new());
    }
    Some(inner.split(',').map(js::trim).collect())
}

/// One node, read.
#[derive(Debug, Clone)]
struct Read {
    file: NodeFile,
    legacy_id: String,
    /// `<workspace>:<legacy id>`, which the id maps are keyed by.
    key: String,
    node_type: String,
    status: Option<String>,
    title: String,
    created: String,
    updated: String,
    owners: Vec<String>,
    /// Edge key to its items.
    edges: Vec<(String, Vec<String>)>,
    /// Every frontmatter key, in file order.
    keys: Vec<String>,
    plane: Plane,
}

fn read(file: &NodeFile) -> Result<Read, PlanError> {
    let parsed = frontmatter::parse(&file.content, &file.path).map_err(|e| PlanError {
        path: file.path.clone(),
        message: e.to_string(),
    })?;
    let fm = &parsed.frontmatter;
    let text = |k: &str| fm.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let list = |k: &str| fm.get(k).and_then(|v| v.as_list()).map(<[String]>::to_vec);
    let legacy_id = text("id").ok_or_else(|| PlanError {
        path: file.path.clone(),
        message: "node has no id".into(),
    })?;
    let node_type = text("type").unwrap_or_default();
    let status = text("status");
    let mut edges = Vec::new();
    for (key, value) in fm.entries() {
        if !is_edge_key(key, &node_type) {
            continue;
        }
        let items = match value {
            frontmatter::Value::String(s) => vec![s.clone()],
            frontmatter::Value::List(l) => l.clone(),
            frontmatter::Value::Bool(_) => continue,
        };
        edges.push((key.clone(), items));
    }
    let plane = classify(&node_type, status.as_deref());
    Ok(Read {
        file: file.clone(),
        key: ws_key(&file.ws, &legacy_id),
        legacy_id,
        title: text("title").unwrap_or_default(),
        created: text("created").unwrap_or_default(),
        updated: text("updated").unwrap_or_default(),
        owners: list("owners").unwrap_or_default(),
        node_type,
        status,
        edges,
        keys: fm.keys().map(str::to_string).collect(),
        plane,
    })
}

fn plane_name(p: Plane) -> &'static str {
    match p {
        Plane::Knowledge => "knowledge",
        Plane::Checkpoint => "checkpoint",
        Plane::CheckResult => "check-result",
        Plane::TierC => "tier-c",
    }
}

/// What a ref names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    /// A node that stays, by its new id (or a cross-graph `mdkg://` ref).
    Node(String),
    /// A node that left git.
    Converted(String),
    /// A node only another branch holds, by the id its branch's migration
    /// gives it (or gave it).
    Elsewhere(String),
    /// A checkpoint or finished test only another branch holds: a log
    /// record once that branch migrates, so there's no id to point at.
    ElsewhereRecord,
    /// Nothing this plan knows.
    Unknown,
}

/// A legacy id in a workspace, as the id maps key it: `<ws>:<id>`.
fn ws_key(ws: &str, id: &str) -> String {
    format!("{ws}:{id}")
}

/// The graph's id map. 0.6.0 ids are unique only in a workspace, so each
/// is keyed by its workspace ([`ws_key`]); an unqualified ref names a node
/// in the workspace of the node it's written in (`ws`), as 0.6.0 reads it.
struct Ids<'a> {
    /// Legacy id to new id, for nodes that stay.
    local: BTreeMap<String, String>,
    /// Legacy ids of nodes that left git.
    converted: BTreeSet<String>,
    /// Nodes only other branches hold: what refs to them become.
    foreign: BTreeMap<String, Target>,
    /// The workspace of the text being rewritten.
    ws: String,
    /// This graph's id, and what each of its UUIDs names (its own nodes',
    /// and other branches' nodes'), for `mdkg://<graph>/<uuid>` refs.
    graph_id: &'a str,
    by_uuid: BTreeMap<String, Target>,
    workspaces: &'a [String],
    subgraphs: &'a BTreeMap<String, Option<ChildGraph>>,
    /// Legacy ids several workspaces hold, which an unqualified mention
    /// outside the graph can't tell apart: [`Ids::text`] leaves those.
    shared: BTreeSet<String>,
}

impl Ids<'_> {
    /// The workspace an id written with `qualifier` is in: the text's own
    /// for none, or the one it names; `None` for any other qualifier (a
    /// subgraph's alias).
    fn workspace<'q>(&'q self, qualifier: Option<&'q str>) -> Option<&'q str> {
        match qualifier {
            None => Some(self.ws.as_str()),
            Some(q) if self.workspaces.iter().any(|w| w == q) => Some(q),
            Some(_) => None,
        }
    }

    /// An unqualified id's key, in the text's own workspace.
    fn here(&self, id: &str) -> String {
        ws_key(&self.ws, id)
    }

    fn resolve(&self, qualifier: Option<&str>, id: &str) -> Target {
        if let Some(ws) = self.workspace(qualifier) {
            let k = ws_key(ws, id);
            if let Some(new) = self.local.get(&k) {
                return Target::Node(new.clone());
            }
            if self.converted.contains(&k) {
                return Target::Converted(k);
            }
            return self.foreign.get(&k).cloned().unwrap_or(Target::Unknown);
        }
        match qualifier.and_then(|q| self.subgraphs.get(q)) {
            Some(Some(child)) => match child.ids.get(id) {
                Some(new) => Target::Node(format!("mdkg://{}/{new}", child.graph_id)),
                None => Target::Unknown,
            },
            _ => Target::Unknown,
        }
    }

    /// An identity v2 ref, `mdkg://<graph>/<uuid>`: a node of this graph by
    /// its UUID, or a subgraph's node by the id its migration gives it.
    fn resolve_identity(&self, graph: &str, uuid: &str) -> Target {
        if graph == self.graph_id {
            return self.by_uuid.get(uuid).cloned().unwrap_or(Target::Unknown);
        }
        let suffix = format!("-{uuid}");
        self.subgraphs
            .values()
            .flatten()
            .filter(|c| c.graph_id == graph)
            .find_map(|c| c.ids.values().find(|id| id.ends_with(&suffix)))
            .map_or(Target::Unknown, |id| {
                Target::Node(format!("mdkg://{graph}/{id}"))
            })
    }

    /// Rewrites each identity v2 ref in `text` (instruction text, read as
    /// code) to the full id it becomes; returns the text and the refs
    /// nothing knew, which stay as written.
    fn identity_refs(&self, text: &str) -> (String, BTreeSet<String>) {
        let mut unknown = BTreeSet::new();
        let targets = rewrite::link_targets(text);
        let out = rewrite::identity_refs(text, &targets, |graph, uuid, written| {
            match self.resolve_identity(graph, uuid) {
                Target::Node(new) | Target::Elsewhere(new) => Some(new),
                Target::Unknown => {
                    unknown.insert(written.to_string());
                    None
                }
                _ => None,
            }
        });
        (out, unknown)
    }

    /// A whole ref value: a bare legacy id, a workspace-qualified `ws:id`,
    /// or an identity v2 `mdkg://<graph>/<uuid>`.
    fn resolve_ref(&self, value: &str) -> Option<Target> {
        if let Some(r) = mdkg_format::ids::parse_identity_ref(value) {
            return Some(self.resolve_identity(&r.graph_id, &r.node_id));
        }
        let (qualifier, id) = match value.split_once(':') {
            Some((q, id)) if !id.starts_with("//") => (Some(q), id),
            Some(_) => return None,
            None => (None, value),
        };
        let known = match self.workspace(qualifier) {
            Some(ws) => {
                let k = ws_key(ws, id);
                self.local.contains_key(&k)
                    || self.converted.contains(&k)
                    || self.foreign.contains_key(&k)
            }
            None => qualifier
                .and_then(|q| self.subgraphs.get(q))
                .and_then(Option::as_ref)
                .is_some_and(|c| c.ids.contains_key(id)),
        };
        if !(known || mdkg_format::ids::is_canonical_id(id)) {
            return None;
        }
        Some(self.resolve(qualifier, id))
    }

    /// Rewrites the ids a node body mentions (links in prose, full ids in
    /// code); returns the body and the tokens nothing knew.
    fn body(&self, text: &str) -> (String, BTreeSet<String>) {
        use rewrite::Mention;
        let mention = |target: Target| match target {
            Target::Node(new) => Mention::Node(new),
            Target::Elsewhere(new) => Mention::CodeOnly(new),
            Target::Converted(_) | Target::ElsewhereRecord => Mention::Leave,
            Target::Unknown => Mention::Unknown,
        };
        let (out, unknown) = rewrite::rewrite_body(
            text,
            |t: &Token<'_>| mention(self.resolve(t.qualifier, t.id)),
            |graph, uuid| mention(self.resolve_identity(graph, uuid)),
        );
        (out, unknown.into_iter().collect())
    }

    /// Rewrites the id tokens in a frontmatter value to full ids; returns
    /// the text and the tokens nothing knew.
    fn text(&self, text: &str) -> (String, BTreeSet<String>) {
        let ambiguous = |t: &Token<'_>| t.qualifier.is_none() && self.shared.contains(t.id);
        let (out, unknown) = rewrite::rewrite_tokens(text, |t: &Token<'_>| {
            if ambiguous(t) {
                return None;
            }
            match self.resolve(t.qualifier, t.id) {
                Target::Node(new) | Target::Elsewhere(new) => Some(new),
                _ => None,
            }
        });
        let unknown = unknown
            .into_iter()
            .filter(|t| {
                !ambiguous(t)
                    && !matches!(
                        self.resolve(t.qualifier, t.id),
                        Target::Converted(_) | Target::ElsewhereRecord
                    )
            })
            .map(|t| t.text.to_string())
            .collect::<BTreeSet<String>>();
        let (out, more) = self.identity_refs(&out);
        (out, unknown.into_iter().chain(more).collect())
    }
}

/// What a ref to a node only another branch holds becomes: its id as
/// written once its branch migrated, else the id that branch's migration
/// gives it (its identity v2 UUID, or a minted one), or a record for a
/// node that leaves git there. `None` when no id can be minted for it.
fn foreign_target(source: &Source, f: &ForeignNode) -> Option<Target> {
    if let Some(id) = &f.id {
        return Some(Target::Elsewhere(id.clone()));
    }
    // That branch's migration keeps its history where this one does: as
    // nodes in git, or as records.
    if source.history == History::Store
        && matches!(
            classify(&f.node_type, f.status.as_deref()),
            Plane::Checkpoint | Plane::CheckResult
        )
    {
        return Some(Target::ElsewhereRecord);
    }
    let uuid = match &f.node_id {
        Some(uuid) => uuid.clone(),
        None => mint::node_uuid(&source.graph_id, &f.legacy_id, &f.created, &f.path)
            .ok()?
            .to_string(),
    };
    Some(Target::Elsewhere(format!("{}-{uuid}", f.node_type)))
}

/// Every node that left git, with what it became.
struct Converted {
    record: Record,
    /// The summary line for the nodes it covered.
    line: String,
    /// The new ids of the nodes it covered.
    targets: Vec<String>,
}

fn warn(warnings: &mut Vec<PlanWarning>, code: &str, path: Option<&str>, message: String) {
    warnings.push(PlanWarning {
        code: code.to_string(),
        path: path.map(str::to_string),
        message,
    });
}

/// What a node's line says of a section the migration moved to the work
/// log (a migration in store mode leaves it).
pub const MOVED_SECTION: &str = "moved to the work log at migration";

/// The node types a checkpoint's or test's summary line goes to first.
const WORK_ITEMS: &[&str] = &["task", "bug", "feat", "spike", "test", "goal", "epic"];

/// Plans the migration of `source`.
pub fn plan(source: &Source) -> Result<Plan, PlanError> {
    let mut warnings = source.warnings.clone();
    let mut reads: Vec<Read> = source.nodes.iter().map(read).collect::<Result<_, _>>()?;
    // A graph that commits its work keeps its history there: checkpoints
    // and finished tests stay nodes, minted and rewritten like any other.
    let keep = source.history == History::Git;
    if keep {
        for r in &mut reads {
            if matches!(r.plane, Plane::Checkpoint | Plane::CheckResult) {
                r.plane = Plane::Knowledge;
            }
        }
    }

    // Pass 1: ids. Every canonical id that stays gets a minted id; ids
    // 0.7.0 keeps (reserved and portable ids) stay as they are.
    let mut ids = Ids {
        local: BTreeMap::new(),
        converted: BTreeSet::new(),
        foreign: BTreeMap::new(),
        ws: source.home_ws.clone(),
        graph_id: &source.graph_id,
        by_uuid: BTreeMap::new(),
        workspaces: &source.workspaces,
        subgraphs: &source.subgraphs,
        shared: BTreeSet::new(),
    };
    ids.local.extend(source.known_ids.clone());
    // Nodes taken from main: refs to them name them where this branch has
    // no node of its own by that legacy id (one it has wins, below).
    ids.local.extend(source.added_ids.clone());
    for r in &reads {
        // A file main no longer has is gone here too: refs to it name the
        // node main moved it to, or go as to a node that left git.
        if source.adopt.get(&r.file.path) == Some(&None) {
            match source.id_overrides.get(&r.key) {
                Some(main) => {
                    ids.local.insert(r.key.clone(), main.clone());
                }
                None => {
                    ids.converted.insert(r.key.clone());
                }
            }
            continue;
        }
        match r.plane {
            Plane::Checkpoint | Plane::CheckResult => {
                ids.converted.insert(r.key.clone());
            }
            Plane::Knowledge | Plane::TierC => {
                if let Some(main) = source.id_overrides.get(&r.key) {
                    ids.local.insert(r.key.clone(), main.clone());
                } else if let Some(mut new) = new_id(source, r, &mut warnings)? {
                    // Made on both sides at the same path on the same day,
                    // it would mint main's id: minted from this commit too.
                    if source.collisions.get(&r.key) == Some(&new) {
                        let path = format!(
                            "{}@{}",
                            source.mint_path(&r.file.path, &r.file.ws),
                            source.base
                        );
                        if let Ok(uuid) =
                            mint::node_uuid(&source.graph_id, &r.legacy_id, &r.created, &path)
                        {
                            let prefix = new.len().saturating_sub(36);
                            new = format!("{}{uuid}", &new[..prefix]);
                        }
                    }
                    ids.local.insert(r.key.clone(), new);
                }
            }
        }
    }

    // An identity v2 graph's refs name its nodes by UUID, other branches'
    // nodes too: by their v2 node_id, or by the UUID in an id their branch
    // already migrated to. A UUID is one node, whatever legacy ids collide.
    if let Some(v2) = &source.v2 {
        // The graph's other nodes, already on 0.7 (restoring history),
        // keep their v2 UUID in their id.
        for id in source.known_ids.values() {
            if let Some(uuid) = id.get(id.len().saturating_sub(36)..)
                && mdkg_format::ids::is_identity_uuid(uuid)
            {
                ids.by_uuid
                    .entry(uuid.to_string())
                    .or_insert_with(|| Target::Node(id.clone()));
            }
        }
        for r in &reads {
            if let Some(uuid) = v2.uuids.get(&r.file.path) {
                ids.ws = r.file.ws.clone();
                let target = ids.resolve(None, &r.legacy_id);
                ids.by_uuid.insert(uuid.clone(), target);
            }
        }
    }
    let mut elsewhere: BTreeMap<String, BTreeSet<Target>> = BTreeMap::new();
    for f in &source.foreign {
        let uuid = f.node_id.clone().or_else(|| {
            let id = f.id.as_deref()?;
            let uuid = id.get(id.len().checked_sub(36)?..)?;
            (id.len() > 37
                && id.as_bytes()[id.len() - 37] == b'-'
                && mdkg_format::ids::is_identity_uuid(uuid))
            .then(|| uuid.to_string())
        });
        let (Some(uuid), Some(target)) = (uuid, foreign_target(source, f)) else {
            continue;
        };
        if !ids.by_uuid.contains_key(&uuid) {
            elsewhere.entry(uuid).or_default().insert(target);
        }
    }
    for (uuid, found) in elsewhere {
        if found.len() == 1 {
            ids.by_uuid
                .extend(found.into_iter().map(|t| (uuid.clone(), t)));
        } else {
            warn(
                &mut warnings,
                "migrate.ambiguous_ref",
                None,
                format!(
                    "other branches name {uuid} as {} different nodes; refs to it stay unresolved",
                    found.len()
                ),
            );
        }
    }

    // Nodes other branches hold get the ids their own migration gives
    // them; two different nodes with one legacy id resolve to neither.
    let mut candidates: BTreeMap<String, BTreeSet<Target>> = BTreeMap::new();
    for f in &source.foreign {
        let k = ws_key(&f.ws, &f.legacy_id);
        if ids.local.contains_key(&k) || ids.converted.contains(&k) {
            continue;
        }
        let Some(candidate) = foreign_target(source, f) else {
            continue;
        };
        candidates.entry(k).or_default().insert(candidate);
    }
    for (k, found) in candidates {
        let legacy = k.split_once(':').map_or(k.as_str(), |(_, id)| id);
        match found.len() {
            1 => {
                ids.foreign.insert(
                    k.clone(),
                    found.into_iter().next().unwrap_or(Target::Unknown),
                );
            }
            n => warn(
                &mut warnings,
                "migrate.ambiguous_ref",
                None,
                format!(
                    "{n} different nodes on other branches are {legacy}; refs to it stay unresolved"
                ),
            ),
        }
    }

    // Who points at each node that leaves git.
    let mut referenced_by: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for r in &reads {
        ids.ws = r.file.ws.clone();
        for (field, items) in &r.edges {
            for item in items {
                if let Some(Target::Converted(k)) = ids.resolve_ref(item)
                    && let Some(target) = reads.iter().find(|x| x.key == k)
                {
                    let by = ids
                        .local
                        .get(&r.key)
                        .cloned()
                        .unwrap_or(r.legacy_id.clone());
                    referenced_by
                        .entry(target.key.as_str())
                        .or_default()
                        .push(json!({ "node": by, "field": field }));
                }
            }
        }
    }
    // A record names its workspace when the graph has several.
    let several = reads
        .iter()
        .map(|r| &r.file.ws)
        .collect::<BTreeSet<_>>()
        .len()
        > 1;

    // Pass 2: the nodes that leave git become records.
    let by_id: BTreeMap<&str, &Read> = reads.iter().map(|r| (r.key.as_str(), r)).collect();
    let mut converted: Vec<Converted> = Vec::new();
    let mut records = Vec::new();
    for r in &reads {
        let kind = match r.plane {
            Plane::Checkpoint => RecordKind::LegacyCheckpoint,
            Plane::CheckResult => RecordKind::CheckResult,
            _ => continue,
        };
        // Main's migration recorded what this branch shares with it.
        if source.adopt.contains_key(&r.file.path) {
            continue;
        }
        ids.ws = r.file.ws.clone();
        let targets = summary_targets(r, &ids, &by_id);
        if targets.is_empty() {
            warn(
                &mut warnings,
                "migrate.unattached_record",
                Some(&r.file.path),
                format!(
                    "{} covers no node that stays, so no summary line is left for it",
                    r.legacy_id
                ),
            );
        }
        let refs: BTreeMap<String, Value> = r
            .edges
            .iter()
            .flat_map(|(_, items)| items.iter())
            .filter_map(|item| match ids.resolve_ref(item)? {
                Target::Node(new) => Some((item.clone(), Value::String(new))),
                _ => None,
            })
            .collect();
        let mut body = json!({
            "legacy_id": r.legacy_id,
            "type": r.node_type,
            "title": r.title,
            "status": r.status,
            "created": r.created,
            "updated": r.updated,
            "path": r.file.path,
            "file": r.file.content,
            "refs": refs,
            "referenced_by": referenced_by.get(r.key.as_str()).cloned().unwrap_or_default(),
        });
        if several {
            body["workspace"] = json!(r.file.ws);
        }
        let actor = r
            .owners
            .first()
            .cloned()
            .unwrap_or_else(|| LEGACY_ACTOR.into());
        // About the nodes it covered, or its own old id when it covered
        // none that stays (lane 2's `legacy:` typed ref).
        let subject = if targets.is_empty() {
            vec![format!("legacy:{}", r.legacy_id)]
        } else {
            targets.clone()
        };
        let record = Record::new(kind, r.created.clone(), actor, subject, body);
        let what = match kind {
            RecordKind::LegacyCheckpoint => "log record",
            _ => "check result",
        };
        let detail = match (kind, r.status.as_deref()) {
            (RecordKind::LegacyCheckpoint, _) => r.created.clone(),
            (_, Some(s)) => format!("{s}, {}", r.created),
            (_, None) => r.created.clone(),
        };
        let line = format!(
            "- {} ({detail}): {} ({what}, migration digest {})",
            r.legacy_id,
            r.title.trim(),
            record.digest
        );
        records.push(record.clone());
        converted.push(Converted {
            record,
            line,
            targets,
        });
    }

    // Format 0.7's node schema decides what a node may hold, not the graph's
    // templates (spec section 3.5): a key a local 0.6.0 template added has
    // no core spelling, so it becomes the extension key `x-local.<key>`,
    // its value unchanged and opaque (orchestrator, 2026-10-25).
    let mut converted_keys: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for r in reads
        .iter()
        .filter(|r| matches!(r.plane, Plane::Knowledge | Plane::TierC))
    {
        let Some(schema) = mdkg_format::schema07::schemas().get(&r.node_type) else {
            continue;
        };
        for k in &r.keys {
            let extension = k.starts_with("x_") || k.starts_with("x-");
            if extension || schema.allows(k) || (source.v2.is_some() && is_v2_identity_key(k)) {
                continue;
            }
            let to = format!("x-local.{k}");
            warn(
                &mut warnings,
                "migrate.key_converted",
                Some(&r.file.path),
                format!(
                    "{}: {k} isn't in format 0.7's node schema; it becomes {to}, value unchanged",
                    r.legacy_id
                ),
            );
            converted_keys.entry(&r.key).or_default().insert(k.clone());
        }
    }

    // Every key's name in the 0.7 file is unique: a converted key, an
    // extension key's new spelling and a key kept as it is may meet
    // (`colour` and `x_local_colour` both become `x-local.colour`), and
    // the plan stops rather than write a node 0.7 refuses after the swap.
    for r in reads
        .iter()
        .filter(|r| matches!(r.plane, Plane::Knowledge | Plane::TierC))
    {
        let converted = converted_keys.get(r.key.as_str());
        let mut names: BTreeMap<String, &str> = BTreeMap::new();
        // The legacy_id line rewrite_node writes beside a new id.
        if ids.local.contains_key(&r.key) {
            names.insert("legacy_id".into(), "id");
        }
        for k in &r.keys {
            if source.v2.is_some() && is_v2_identity_key(k) {
                continue;
            }
            let name = if converted.is_some_and(|c| c.contains(k)) {
                format!("x-local.{k}")
            } else {
                match rewrite::extension_key(k) {
                    Some(Ok((name, _))) => name,
                    _ => k.clone(),
                }
            };
            if let Some(first) = names.insert(name.clone(), k) {
                return Err(PlanError {
                    path: r.file.path.clone(),
                    message: format!(
                        "{} has {first} and {k}, which would both become {name} in format 0.7",
                        r.legacy_id
                    ),
                });
            }
        }
    }

    // Pass 3: rewrite the nodes that stay; delete the ones that left.
    let mut nodes = Vec::new();
    let mut files = Vec::new();
    for r in &reads {
        ids.ws = r.file.ws.clone();
        // A file the branch hasn't changed comes out as main has it
        // (below). Its records are main's migration's own
        // (`Source::replayed`).
        if source.adopt.contains_key(&r.file.path) {
            nodes.push(NodeChange {
                legacy_id: r.legacy_id.clone(),
                id: ids.local.get(&r.key).cloned(),
                path: r.file.path.clone(),
                plane: plane_name(r.plane).into(),
                record: None,
            });
            continue;
        }
        match r.plane {
            Plane::Checkpoint | Plane::CheckResult => {
                let c = converted
                    .iter()
                    .find(|c| c.record.body["path"] == Value::String(r.file.path.clone()));
                nodes.push(NodeChange {
                    legacy_id: r.legacy_id.clone(),
                    id: None,
                    path: r.file.path.clone(),
                    plane: plane_name(r.plane).into(),
                    record: c.map(|c| c.record.digest.clone()),
                });
                files.push(FileChange {
                    path: r.file.path.clone(),
                    action: Action::Delete,
                    before: Digest::of_bytes(r.file.content.as_bytes()).to_string(),
                    after: None,
                    content: None,
                });
                continue;
            }
            Plane::TierC => warn(
                &mut warnings,
                "migrate.tier_c_kept",
                Some(&r.file.path),
                format!(
                    "{} is a {} node, which format 0.7 keeps as a plain node without its 0.6 behaviour",
                    r.legacy_id, r.node_type
                ),
            ),
            Plane::Knowledge => {}
        }
        if r.node_type == "test"
            && classify(&r.node_type, r.status.as_deref()) != Plane::CheckResult
        {
            warn(
                &mut warnings,
                "migrate.open_test_kept",
                Some(&r.file.path),
                format!(
                    "{} isn't done, so it stays a work item rather than a check result",
                    r.legacy_id
                ),
            );
        }
        let new = ids.local.get(&r.key).cloned();
        let lines: Vec<&str> = converted
            .iter()
            .filter(|c| new.as_ref().is_some_and(|n| c.targets.contains(n)))
            .map(|c| c.line.as_str())
            .collect();
        let (content, mut node_records) = rewrite_node(
            r,
            new.as_deref(),
            &ids,
            &lines,
            converted_keys.get(r.key.as_str()),
            source.v2.is_some(),
            keep,
            several,
            &mut warnings,
        )?;
        let content = if source.collisions.contains_key(&r.key) {
            drop_legacy_id(&content)
        } else {
            content
        };
        records.append(&mut node_records);
        nodes.push(NodeChange {
            legacy_id: r.legacy_id.clone(),
            id: new,
            path: r.file.path.clone(),
            // A checkpoint or finished test kept in git says so.
            plane: plane_name(match classify(&r.node_type, r.status.as_deref()) {
                kept @ (Plane::Checkpoint | Plane::CheckResult) => kept,
                _ => r.plane,
            })
            .into(),
            record: None,
        });
        if content != r.file.content {
            files.push(FileChange {
                path: r.file.path.clone(),
                action: Action::Write,
                before: Digest::of_bytes(r.file.content.as_bytes()).to_string(),
                after: Some(Digest::of_bytes(content.as_bytes()).to_string()),
                content: Some(content),
            });
        }
    }

    // The event logs become legacy records, and leave git (or stay, as
    // they are, with the rest of the work history).
    for f in source
        .events
        .iter()
        .filter(|f| !keep && !source.adopt.contains_key(&f.path))
    {
        ids.ws = f.ws.clone();
        let (mut evs, unparsed) =
            super::records::import_events(&f.path, &f.content, |r| match ids.resolve_ref(r)? {
                Target::Node(new) => Some(new),
                _ => None,
            });
        if !unparsed.is_empty() {
            warn(
                &mut warnings,
                "migrate.event_unparsed",
                Some(&f.path),
                format!(
                    "{} line(s) aren't JSON objects; each is kept whole as text",
                    unparsed.len()
                ),
            );
        }
        records.append(&mut evs);
        files.push(FileChange {
            path: f.path.clone(),
            action: Action::Delete,
            before: Digest::of_bytes(f.content.as_bytes()).to_string(),
            after: None,
            content: None,
        });
    }

    // The config records the graph's id; an identity v2 graph's manifest,
    // which held it, goes.
    if let Some(v2) = &source.v2 {
        files.push(FileChange {
            path: v2.manifest.path.clone(),
            action: Action::Delete,
            before: Digest::of_bytes(v2.manifest.content.as_bytes()).to_string(),
            after: None,
            content: None,
        });
    }
    {
        let content = upgrade_config(
            &source.config.content,
            &source.graph_id,
            &source.projections,
        )
        .ok_or_else(|| PlanError {
            path: source.config.path.clone(),
            message: "config isn't a JSON object this migration can add graph_id to".into(),
        })?;
        files.push(FileChange {
            path: source.config.path.clone(),
            action: Action::Write,
            before: Digest::of_bytes(source.config.content.as_bytes()).to_string(),
            after: Some(Digest::of_bytes(content.as_bytes()).to_string()),
            content: Some(content),
        });
    }

    // Each core list names its workspace's nodes by id, reserved ids too.
    for core in &source.core_lists {
        let mut out = String::with_capacity(core.content.len());
        for line in core.content.split_inclusive('\n') {
            let bare = line.trim_end_matches(['\n', '\r']);
            match ids.local.get(&ws_key(&core.ws, bare.trim())) {
                Some(new) if !bare.trim_start().starts_with('#') => {
                    out.push_str(new);
                    out.push_str(&line[bare.len()..]);
                }
                _ => out.push_str(line),
            }
        }
        if out != core.content {
            files.push(FileChange {
                path: core.path.clone(),
                action: Action::Write,
                before: Digest::of_bytes(core.content.as_bytes()).to_string(),
                after: Some(Digest::of_bytes(out.as_bytes()).to_string()),
                content: Some(out),
            });
        }
    }

    // The patch: instruction files outside the node folders, read as the
    // graph's root workspace's.
    ids.ws = source.home_ws.clone();
    // An id several workspaces hold names none of them for sure there: it
    // stays as written (qualified ones still change).
    let mut held: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for k in ids
        .local
        .keys()
        .chain(&ids.converted)
        .chain(ids.foreign.keys())
    {
        if let Some((ws, id)) = k.split_once(':') {
            held.entry(id).or_default().insert(ws);
        }
    }
    let shared: BTreeSet<String> = held
        .into_iter()
        .filter(|(_, ws)| ws.len() > 1)
        .map(|(id, _)| id.to_string())
        .collect();
    ids.shared = shared;
    let mut patch = Vec::new();
    for f in &source.outside {
        let named: Vec<&String> = ids
            .shared
            .iter()
            .filter(|id| {
                f.content.match_indices(id.as_str()).any(|(at, _)| {
                    let before = |c: char| c.is_ascii_alphanumeric() || "-_.:".contains(c);
                    let after = |c: char| c.is_ascii_alphanumeric() || "-_".contains(c);
                    !f.content[..at].ends_with(before)
                        && !f.content[at + id.len()..].starts_with(after)
                })
            })
            .collect();
        if !named.is_empty() {
            let shown: Vec<&str> = named.iter().take(8).map(|s| s.as_str()).collect();
            warn(
                &mut warnings,
                "migrate.ambiguous_ref",
                Some(&f.path),
                format!(
                    "{} id(s) are in several workspaces, so where they're written without one \
                     they stay as written: {}; write them as <workspace>:<id> to have them \
                     rewritten",
                    named.len(),
                    shown.join(", ")
                ),
            );
        }
        let examples = source.examples.contains(&f.path);
        let (content, unknown, kept) = rewrite_instructions(&f.content, &ids, examples);
        if kept > 0 {
            warn(
                &mut warnings,
                "migrate.command_kept",
                Some(&f.path),
                format!(
                    "{kept} 0.6.0 command line(s) have no exact new spelling (an option or \
                     a status the new command lacks); they stay as written (the 0.6.0 \
                     spelling still runs, with a notice)"
                ),
            );
        }
        if !unknown.is_empty() {
            unresolved(&mut warnings, &f.path, &unknown);
        }
        if content != f.content {
            patch.push(FileChange {
                path: f.path.clone(),
                action: Action::Write,
                before: Digest::of_bytes(f.content.as_bytes()).to_string(),
                after: Some(Digest::of_bytes(content.as_bytes()).to_string()),
                content: Some(content),
            });
        }
    }

    // Against a migrated main, each graph file this branch hasn't changed
    // since it forked comes out as main has it, byte for byte.
    if !source.adopt.is_empty() {
        let mut current: BTreeMap<&str, &str> = BTreeMap::new();
        current.insert(&source.config.path, &source.config.content);
        for f in source.events.iter().chain(&source.core_lists) {
            current.insert(&f.path, &f.content);
        }
        for n in &source.nodes {
            current.insert(&n.path, &n.content);
        }
        // Only the files main's content stands in for; any other change
        // the plan makes (an identity v2 manifest that goes) stays.
        files.retain(|f| {
            !(source.adopt.contains_key(&f.path) && current.contains_key(f.path.as_str()))
        });
        for (path, main) in &source.adopt {
            let Some(now) = current.get(path.as_str()) else {
                continue;
            };
            let before = Digest::of_bytes(now.as_bytes()).to_string();
            match main {
                Some(c) if c != now => files.push(FileChange {
                    path: path.clone(),
                    action: Action::Write,
                    before,
                    after: Some(Digest::of_bytes(c.as_bytes()).to_string()),
                    content: Some(c.clone()),
                }),
                Some(_) => {}
                None => files.push(FileChange {
                    path: path.clone(),
                    action: Action::Delete,
                    before,
                    after: None,
                    content: None,
                }),
            }
        }
    }
    for (path, main) in &source.added {
        files.push(FileChange {
            path: path.clone(),
            action: Action::Write,
            before: String::new(),
            after: Some(Digest::of_bytes(main.as_bytes()).to_string()),
            content: Some(main.clone()),
        });
    }

    records.extend(source.replayed.iter().cloned());
    files.sort_by(|a, b| a.path.cmp(&b.path));
    patch.sort_by(|a, b| a.path.cmp(&b.path));
    warnings.sort();
    Ok(Plan {
        format: PLAN_FORMAT,
        base: source.base.clone(),
        graph_id: source.graph_id.clone(),
        history: source.history,
        history_reason: source.history_reason.clone(),
        against: source.against.clone(),
        workspaces: source
            .folders
            .iter()
            .map(|(alias, folder)| PlanWorkspace {
                alias: alias.clone(),
                folder: folder.clone(),
                nodes: source.nodes.iter().filter(|n| &n.ws == alias).count(),
            })
            .collect(),
        nodes,
        files,
        patch,
        records,
        warnings,
    })
}

fn unresolved(warnings: &mut Vec<PlanWarning>, path: &str, unknown: &BTreeSet<String>) {
    let shown: Vec<&str> = unknown.iter().take(8).map(String::as_str).collect();
    let more = unknown.len().saturating_sub(shown.len());
    warn(
        warnings,
        "migrate.unresolved_ref",
        Some(path),
        format!(
            "{} id(s) name no node here and stay as written: {}{}",
            unknown.len(),
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ),
    );
}

/// A remaining node's new id: `<type>-<uuidv7>` (lane 1, 2026-10-24: every
/// node, reserved and portable ids too), minted from its creation date
/// and legacy id.
fn new_id(
    source: &Source,
    r: &Read,
    _warnings: &mut Vec<PlanWarning>,
) -> Result<Option<String>, PlanError> {
    let node_type = if r.node_type.is_empty() {
        r.legacy_id.split('-').next().unwrap_or_default()
    } else {
        r.node_type.as_str()
    };
    let type_ok = node_type.starts_with(|c: char| c.is_ascii_lowercase())
        && node_type.len() <= 32
        && node_type
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !type_ok {
        return Err(PlanError {
            path: r.file.path.clone(),
            message: format!("{}'s type can't prefix a format 0.7 id", r.legacy_id),
        });
    }
    // An identity v2 node keeps its UUID.
    if let Some(uuid) = source.v2.as_ref().and_then(|v2| v2.uuids.get(&r.file.path)) {
        return Ok(Some(format!("{node_type}-{uuid}")));
    }
    let path = source.mint_path(&r.file.path, &r.file.ws);
    match mint::node_uuid(&source.graph_id, &r.legacy_id, &r.created, &path) {
        Ok(uuid) => Ok(Some(format!("{node_type}-{uuid}"))),
        Err(e) => Err(PlanError {
            path: r.file.path.clone(),
            message: match e {
                mint::MintError::BadCreated => {
                    format!("{}'s created isn't a YYYY-MM-DD date", r.legacy_id)
                }
                mint::MintError::NumberTooLarge => {
                    format!("{}'s number is too large to mint from", r.legacy_id)
                }
            },
        }),
    }
}

/// `created` and `updated` as 0.7.0 writes them: `created` is the id's own
/// time, and `updated` is never earlier.
fn timestamps(new: &str, kept: Option<&str>, updated: &str) -> Option<(String, String)> {
    let created_ms = match kept {
        // A kept identity v2 UUID: the old `created` date, at midnight.
        Some(created) => mint::date_ms(created.get(..10).unwrap_or(created))?,
        None => {
            let uuid = new.get(new.len().checked_sub(36)?..)?;
            mdkg_format::uuid7::uuid_v7_timestamp(uuid)?
        }
    };
    let updated_ms = mint::date_ms(updated.get(..10).unwrap_or(updated)).unwrap_or(0);
    Some((
        mint::rfc3339_ms(created_ms),
        mint::rfc3339_ms(created_ms.max(updated_ms)),
    ))
}

/// The new ids of the nodes a checkpoint's or test's summary line goes to:
/// its `scope`, else its `parent`, else what it `relates` to, keeping work
/// items when there are any.
fn summary_targets(r: &Read, ids: &Ids<'_>, by_id: &BTreeMap<&str, &Read>) -> Vec<String> {
    for key in ["scope", "parent", "relates"] {
        let Some((_, items)) = r.edges.iter().find(|(k, _)| k == key) else {
            continue;
        };
        let staying: Vec<(String, &str)> = items
            .iter()
            .filter_map(|item| match ids.resolve_ref(item)? {
                Target::Node(new) if !new.starts_with("mdkg://") => {
                    let k = match item.split_once(':') {
                        Some((q, id)) => ws_key(q, id),
                        None => ids.here(item),
                    };
                    let t = by_id.get(k.as_str()).map_or("", |x| x.node_type.as_str());
                    Some((new, t))
                }
                _ => None,
            })
            .collect();
        if staying.is_empty() {
            continue;
        }
        let work: Vec<String> = staying
            .iter()
            .filter(|(_, t)| WORK_ITEMS.contains(t))
            .map(|(n, _)| n.clone())
            .collect();
        let mut out = if work.is_empty() {
            staying.into_iter().map(|(n, _)| n).collect()
        } else {
            work
        };
        out.dedup();
        return out;
    }
    Vec::new()
}

/// `content` without its frontmatter's `legacy_id` line.
fn drop_legacy_id(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_front = false;
    let mut dropped = false;
    for (i, line) in content.split_inclusive('\n').enumerate() {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == "---" {
            in_front = i == 0;
        } else if in_front && !dropped && bare.starts_with("legacy_id:") {
            dropped = true;
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Rewrites one node that stays.
#[allow(clippy::too_many_arguments)]
fn rewrite_node(
    r: &Read,
    new: Option<&str>,
    ids: &Ids<'_>,
    summary: &[&str],
    to_extension: Option<&BTreeSet<String>>,
    v2: bool,
    keep_sections: bool,
    several: bool,
    warnings: &mut Vec<PlanWarning>,
) -> Result<(String, Vec<Record>), PlanError> {
    let path = &r.file.path;
    let s = split(&r.file.content, path)?;
    let mut out = String::with_capacity(r.file.content.len() + 128);
    let mut unknown = BTreeSet::new();
    let mut removed: Vec<(String, String)> = Vec::new();
    let stamps = new.and_then(|n| timestamps(n, v2.then_some(r.created.as_str()), &r.updated));
    // An identity v2 node keeps its `created`, which 0.6.0 checked only for
    // digits: one naming no real day can't become a 0.7 timestamp.
    if new.is_some() && stamps.is_none() {
        return Err(PlanError {
            path: path.clone(),
            message: format!(
                "{}'s created isn't a YYYY-MM-DD date that exists",
                r.legacy_id
            ),
        });
    }
    let default_ending = if s.open.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    // Every line in its 0.7 form (section 10 of the format spec): the
    // delimiters bare, each key and value as 0.6.0 read them.
    out.push_str("---");
    out.push_str(default_ending);
    for line in &s.lines {
        let Some(key) = line.key else {
            out.push_str(line.raw);
            continue;
        };
        let ending = if line.ending.is_empty() {
            default_ending
        } else {
            line.ending
        };
        // The node's identity is its id now (section 10.2).
        if v2 && is_v2_identity_key(key) {
            continue;
        }
        if key == "id" {
            match new {
                Some(new) => {
                    out.push_str(&format!("id: {new}{ending}"));
                    out.push_str(&format!("legacy_id: {}{ending}", r.legacy_id));
                }
                None => out.push_str(&format!("id: {}{ending}", line.value)),
            }
            continue;
        }
        if let (Some((created, updated)), "created" | "updated") = (&stamps, key) {
            let v = if key == "created" { created } else { updated };
            out.push_str(&format!("{key}: {v}{ending}"));
            continue;
        }
        // A key outside the node schema: an opaque extension key, value
        // as 0.6.0 read it.
        if to_extension.is_some_and(|keys| keys.contains(key)) {
            out.push_str(&format!(
                "x-local.{key}: {}{ending}",
                canonical(key, line.value)
            ));
            continue;
        }
        // Extension keys get their 0.7.0 spelling.
        let key_out = match rewrite::extension_key(key) {
            Some(Ok((k, guessed))) => {
                if guessed {
                    warn(
                        warnings,
                        "migrate.extension_key_split",
                        Some(path),
                        format!("{key} became {k}; check where its namespace ends"),
                    );
                }
                k
            }
            Some(Err(())) => {
                warn(
                    warnings,
                    "migrate.extension_key_kept",
                    Some(path),
                    format!(
                        "{key} has no format 0.7 spelling (x-<namespace>.<field>), so it stays"
                    ),
                );
                key.to_string()
            }
            None => key.to_string(),
        };
        if FIXED_KEYS.contains(&key) {
            out.push_str(&format!(
                "{key_out}: {}{ending}",
                canonical(key, line.value)
            ));
            continue;
        }
        // Only ref fields name nodes by id. Every other value is text: a
        // title, a goal_condition, links, artifacts, required_checks
        // commands, extension values. Each stays as written, its old ids
        // too, which still resolve through legacy_id.
        let value = if is_edge_key(key, &r.node_type) {
            edge_value(key, line.value, ids, &mut unknown, &mut removed)
        } else if is_opaque_refs_key(key, &r.node_type) {
            Some(opaque_refs(line.value, ids))
        } else {
            let mut v = line.value.to_string();
            if key == "required_checks"
                && let Some((gated, dropped)) = rewrite::capability_search(&v)
            {
                v = gated;
                if dropped {
                    warn(
                        warnings,
                        "migrate.gate_option_dropped",
                        Some(path),
                        "a capability search gate lost an option mdkg search lacks".into(),
                    );
                }
            }
            Some(v)
        };
        match value {
            None => {} // A scalar ref to a node that left git: the line goes.
            Some(v) => out.push_str(&format!("{key_out}: {}{ending}", canonical(key, &v))),
        }
    }
    let close_ending = &s.close[s.close.trim_end_matches(['\n', '\r']).len()..];
    out.push_str("---");
    out.push_str(close_ending);

    // The body: sections that move (unless the history stays in git),
    // gates, then ids.
    let mut moved = Vec::new();
    let subject = new.map_or(r.legacy_id.clone(), str::to_string);
    let (body, _) = if keep_sections {
        (s.body.to_string(), Vec::new())
    } else {
        rewrite::split_sections(s.body, |section| {
            let mut body = json!({
                "legacy_id": r.legacy_id,
                "section": section.kind.as_str(),
                "heading": section.heading,
                "text": section.text,
            });
            if several {
                body["workspace"] = json!(r.file.ws);
            }
            let record = Record::new(
                RecordKind::ReviewRecord,
                r.updated.clone(),
                LEGACY_ACTOR,
                vec![subject.clone()],
                body,
            );
            let what = match section.kind {
                SectionKind::Progress => "Progress notes",
                SectionKind::ReviewLedger => "The review ledger",
            };
            let line = format!(
                "{what} {MOVED_SECTION}: review record, migration digest {}.",
                record.digest
            );
            moved.push(record);
            line
        })
    };
    let body = match rewrite::gate_sections(&body) {
        Some((gated, dropped)) => {
            if dropped {
                warn(
                    warnings,
                    "migrate.gate_option_dropped",
                    Some(path),
                    "a capability search gate lost an option mdkg search lacks".into(),
                );
            }
            gated
        }
        None => body,
    };
    let (mut body, body_unknown) = ids.body(&body);
    unknown.extend(body_unknown);
    if !summary.is_empty() || !removed.is_empty() {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("\n# Migrated records\n\n");
        for line in summary {
            body.push_str(line);
            body.push('\n');
        }
        if !removed.is_empty() {
            let mut by_key: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (k, v) in &removed {
                by_key.entry(k.as_str()).or_default().push(v.as_str());
            }
            let listed: Vec<String> = by_key
                .into_iter()
                .map(|(k, v)| format!("{k}: {}", v.join(", ")))
                .collect();
            body.push_str(&format!(
                "- Refs to nodes this graph doesn't have, moved here from the frontmatter: {}\n",
                listed.join("; ")
            ));
        }
    }
    out.push_str(&body);
    if !unknown.is_empty() {
        unresolved(warnings, path, &unknown);
    }
    Ok((out, moved))
}

/// A ref field's new value: known refs rewritten, refs to nodes that left
/// git dropped (their records list who pointed at them), and refs nothing
/// knows moved to `removed` (0.7.0's ref fields hold only ids and URIs; the
/// node's body keeps them). URIs stay. `None` for a scalar whose line goes.
fn edge_value(
    key: &str,
    value: &str,
    ids: &Ids<'_>,
    unknown: &mut BTreeSet<String>,
    removed: &mut Vec<(String, String)>,
) -> Option<String> {
    let mut one = |item: &str| -> Option<String> {
        match ids.resolve_ref(item) {
            Some(Target::Node(new)) => Some(new),
            Some(Target::Converted(_)) => None,
            // 0.7.0 refuses a ref to a node this branch lacks: the body
            // keeps it, by the id it will have, until its branch merges.
            Some(Target::Elsewhere(new)) => {
                removed.push((
                    key.to_string(),
                    format!("{new} ({item}, on another branch)"),
                ));
                None
            }
            // A record there, after that branch migrates: the ref is kept
            // here, as written, since no record on this branch holds it.
            Some(Target::ElsewhereRecord) => {
                removed.push((
                    key.to_string(),
                    format!("{item} (a checkpoint or finished test on another branch)"),
                ));
                None
            }
            // Other URIs stay; an identity v2 ref is never one of them.
            _ if mdkg_format::ids::is_uri_ref(item)
                && mdkg_format::ids::parse_identity_ref(item).is_none() =>
            {
                Some(item.to_string())
            }
            _ => {
                unknown.insert(item.to_string());
                removed.push((key.to_string(), item.to_string()));
                None
            }
        }
    };
    match list_items(value) {
        Some(items) => {
            let kept: Vec<String> = items.into_iter().filter_map(&mut one).collect();
            Some(format!("[{}]", kept.join(", ")))
        }
        None => one(value),
    }
}

/// 0.6.0 command spellings and their 0.7.0 ones, with the options each new
/// command takes. A line whose invocation uses another option keeps its
/// 0.6.0 spelling, which still runs as an alias.
const COMMANDS: &[(&str, &str, &[&str])] = &[
    ("mdkg task start ", "mdkg claim ", &["--json"]),
    ("mdkg task update ", "mdkg update ", &["--status", "--json"]),
    ("mdkg task done ", "mdkg done ", &["--json"]),
    (
        "mdkg checkpoint new ",
        "mdkg new checkpoint ",
        &[
            "--kind",
            "--scope",
            "--relates",
            "--status",
            "--priority",
            "--json",
        ],
    ),
];

/// `task update <id> …`'s new spelling, by its `--status`: 0.7.0's `update`
/// takes only review and blocked, and done becomes `done` without the
/// option, as the alias runs it. `None` for any other status, or none: no
/// command does exactly what that line did. Progress is one: the alias
/// claims the node, but `claim` takes an existing path of that name first,
/// a full id's too, so the line keeps its 0.6.0 spelling.
fn task_update(rest: &str, invocation: &str) -> Option<String> {
    let words: Vec<&str> = invocation.split(' ').collect();
    let (at, status, width) =
        words
            .iter()
            .enumerate()
            .find_map(|(i, w)| match w.strip_prefix("--status") {
                Some("") => Some((i, *words.get(i + 1)?, 2)),
                Some(v) => v.strip_prefix('=').map(|v| (i, v, 1)),
                None => None,
            })?;
    let command = match status.to_lowercase().as_str() {
        "review" | "blocked" => return Some(format!("mdkg update {rest}")),
        "done" => "done",
        _ => return None,
    };
    let kept: Vec<&str> = words
        .iter()
        .enumerate()
        .filter(|(i, _)| !(at..at + width).contains(i))
        .map(|(_, w)| *w)
        .collect();
    let tail = &rest[invocation.len()..];
    Some(format!("mdkg {command} {}{tail}", kept.join(" ")))
}

/// Rewrites an instruction file: 0.6.0 command names, `capability search`
/// gates, and ids. Returns the text, unknown id tokens, and how many
/// command lines kept their 0.6.0 spelling.
fn rewrite_instructions(
    text: &str,
    ids: &Ids<'_>,
    examples: bool,
) -> (String, BTreeSet<String>, usize) {
    let mut out = String::with_capacity(text.len());
    let mut kept = 0;
    for line in text.split_inclusive('\n') {
        let mut line = line.to_string();
        for (old, new, allowed) in COMMANDS {
            let Some(at) = line.find(old) else { continue };
            let rest = &line[at + old.len()..];
            let invocation = rest.split(['`', '\n', '|', ';']).next().unwrap_or("");
            let ok = invocation
                .split_whitespace()
                .filter(|w| w.starts_with("--"))
                .all(|w| allowed.contains(&w.split('=').next().unwrap_or(w)));
            let rewritten = match *old {
                "mdkg task update " if ok => task_update(rest, invocation),
                _ if ok => Some(format!("{new}{rest}")),
                _ => None,
            };
            match rewritten {
                Some(r) => line = format!("{}{r}", &line[..at]),
                None => kept += 1,
            }
        }
        if let Some((gated, _)) = rewrite::capability_search(&line) {
            line = gated;
        }
        out.push_str(&line);
    }
    if examples {
        return (out, BTreeSet::new(), kept);
    }
    let (out, unknown) = ids.text(&out);
    (out, unknown, kept)
}

/// The config as 0.7.0 reads it (lane 1, 2026-10-24): `schema_version` 3,
/// `format_version` "0.7" and the `graph_id`, written right after
/// `schema_version`; every other member keeps its place and value. A
/// config without a `projections` list gets one, last: the `projections`
/// the repository has, so a sync keeps writing exactly those. The file is
/// written back in its own indentation, as JSON.stringify writes it.
/// `None` when the text isn't a JSON object.
fn upgrade_config(config: &str, graph_id: &str, projections: &[&str]) -> Option<String> {
    let v: Value = serde_json::from_str(config).ok()?;
    let old = v.as_object()?;
    let mut out = serde_json::Map::new();
    let mut placed = false;
    let add_ours = |out: &mut serde_json::Map<String, Value>| {
        out.insert("schema_version".into(), json!(3));
        out.insert("format_version".into(), json!("0.7"));
        out.insert("graph_id".into(), json!(graph_id));
    };
    if !old.contains_key("schema_version") {
        add_ours(&mut out);
        placed = true;
    }
    for (k, val) in old {
        match k.as_str() {
            "schema_version" if !placed => {
                add_ours(&mut out);
                placed = true;
            }
            "schema_version" | "format_version" | "graph_id" => {}
            _ => {
                out.insert(k.clone(), val.clone());
            }
        }
    }
    out.entry(mdkg_run::projection::KEY)
        .or_insert_with(|| json!(projections));
    // Where work lives stays as it was, said in 0.7's words when the
    // config left it implicit: in git (format 0.7, section 2.2).
    match out.get_mut("work") {
        Some(Value::Object(work)) => {
            if !work.contains_key("location") && !work.contains_key("committed") {
                work.insert("location".into(), json!("git"));
            }
        }
        Some(_) => {}
        None => {
            out.insert("work".into(), json!({ "location": "git" }));
        }
    }
    // The file's own indentation: its first member's.
    let indent: String = config
        .split('\n')
        .nth(1)
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
        .filter(|i: &String| !i.is_empty())
        .unwrap_or_else(|| "  ".into());
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    serde::Serialize::serialize(&Value::Object(out), &mut ser).ok()?;
    let mut text = String::from_utf8(bytes).ok()?;
    if config.ends_with('\n') {
        text.push('\n');
    }
    if config.contains("\r\n") {
        text = text.replace('\n', "\r\n");
    }
    Some(text)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const G: &str = "0192f4c2-1b2a-7c3d-8e4f-0123456789ab";

    fn node(path: &str, content: &str) -> NodeFile {
        NodeFile {
            path: format!(".mdkg/work/{path}"),
            content: content.into(),
            ws: "root".into(),
        }
    }

    pub(in crate::migrate) fn source(nodes: Vec<NodeFile>) -> Source {
        Source {
            base: "abc".into(),
            graph_id: G.into(),
            graph_id_minted: true,
            config: TextFile {
                path: ".mdkg/config.json".into(),
                content: "{\n  \"schema_version\": 1,\n  \"tool\": \"mdkg\"\n}\n".into(),
                ws: "root".into(),
            },
            nodes,
            mdkg_prefix: ".mdkg/".into(),
            folders: [("root".to_string(), ".mdkg/".to_string())].into(),
            home_ws: "root".into(),
            workspaces: vec!["root".into()],
            ..Source::default()
        }
    }

    pub(in crate::migrate) fn task(n: u32, extra: &str, body: &str) -> NodeFile {
        node(
            &format!("task-{n}-t.md"),
            &format!(
                "---\nid: task-{n}\ntype: task\ntitle: Task {n}\nstatus: todo\npriority: 1\n{extra}tags: []\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n{body}"
            ),
        )
    }

    fn file<'a>(p: &'a Plan, path: &str) -> &'a FileChange {
        p.files
            .iter()
            .find(|f| f.path.ends_with(path))
            .unwrap_or_else(|| panic!("no change to {path}"))
    }

    #[test]
    fn ids_refs_and_legacy_id() {
        let src = source(vec![
            task(
                1,
                "relates: [task-2, root:task-2, gone-1]\n",
                "\n# Overview\n\nSee task-2.\n",
            ),
            task(2, "parent: task-1\n", "\n# Overview\n\nx\n"),
        ]);
        let p = plan(&src).unwrap();
        let new1 = mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap();
        let new2 = mint::node_uuid(G, "task-2", "2026-10-24", "work/task-2-t.md").unwrap();
        let t1 = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t1.starts_with(&format!(
            "---\nid: task-{new1}\nlegacy_id: task-1\ntype: task\n"
        )));
        // A ref to nothing this graph knows leaves the frontmatter for the
        // body, so nothing is lost and the 0.7.0 reader accepts the node.
        assert!(
            t1.contains(&format!("relates: [task-{new2}, task-{new2}]\n")),
            "{t1}"
        );
        assert!(t1.contains("- Refs to nodes this graph doesn't have, moved here from the frontmatter: relates: gone-1\n"), "{t1}");
        assert!(t1.contains(&format!("See [task-2](task-{new2}).\n")));
        let t2 = file(&p, "task-2-t.md").content.clone().unwrap();
        assert!(t2.contains(&format!("parent: task-{new1}\n")));
        assert!(
            p.warnings
                .iter()
                .any(|w| w.code == "migrate.unresolved_ref" && w.message.contains("gone-1"))
        );
        // The config gets the graph id, and the projections there are
        // (none here).
        let cfg = file(&p, "config.json").content.clone().unwrap();
        assert_eq!(
            cfg,
            format!(
                "{{\n  \"schema_version\": 3,\n  \"format_version\": \"0.7\",\n  \"graph_id\": \"{G}\",\n  \"tool\": \"mdkg\",\n  \"projections\": [],\n  \"work\": {{\n    \"location\": \"git\"\n  }}\n}}\n"
            )
        );
        // Deterministic: the same source, the same hash.
        assert_eq!(p.hash(), plan(&src).unwrap().hash());
        let mut other = src.clone();
        other.nodes[1].content.push_str("more\n");
        assert_ne!(p.hash(), plan(&other).unwrap().hash());
    }

    #[test]
    fn checkpoints_and_tests_leave_git() {
        let chk = node(
            "chk-1-c.md",
            "---\nid: chk-1\ntype: checkpoint\ntitle: Did it\nstatus: done\nscope: [task-1]\nrelates: [task-1]\ncreated: 2026-10-23\nupdated: 2026-10-23\n---\nBody of chk.\n",
        );
        let done_test = node(
            "test-1-x.md",
            "---\nid: test-1\ntype: test\ntitle: Proves it\nstatus: done\nrelates: [task-1, edd-1]\ncreated: 2026-10-23\nupdated: 2026-10-23\n---\nEvidence.\n",
        );
        let open_test = node(
            "test-2-x.md",
            "---\nid: test-2\ntype: test\ntitle: Later\nstatus: todo\nrelates: [task-1]\ncreated: 2026-10-23\nupdated: 2026-10-23\n---\n",
        );
        let src = source(vec![
            task(
                1,
                "blocked_by: [test-1, test-2]\nrefs: [chk-1]\n",
                "\n# Overview\n\nchk-1 did it.\n",
            ),
            chk,
            done_test,
            open_test,
        ]);
        let p = plan(&src).unwrap();
        assert_eq!(file(&p, "chk-1-c.md").action, Action::Delete);
        assert_eq!(file(&p, "test-1-x.md").action, Action::Delete);
        assert!(
            p.files
                .iter()
                .all(|f| !f.path.ends_with("test-2-x.md") || f.action == Action::Write)
        );
        let kinds: Vec<RecordKind> = p.records.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&RecordKind::LegacyCheckpoint));
        assert!(kinds.contains(&RecordKind::CheckResult));
        let chk_record = p
            .records
            .iter()
            .find(|r| r.kind == RecordKind::LegacyCheckpoint)
            .unwrap();
        // The record holds the whole file, and who pointed at it.
        assert_eq!(
            chk_record.body["file"].as_str().unwrap(),
            src.nodes[1].content
        );
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        assert_eq!(chk_record.subject, std::slice::from_ref(&new1));
        assert_eq!(chk_record.body["referenced_by"][0]["field"], "refs");
        let t1 = file(&p, "task-1-t.md").content.clone().unwrap();
        let new_t2 = format!(
            "test-{}",
            mint::node_uuid(G, "test-2", "2026-10-23", "work/test-2-x.md").unwrap()
        );
        // Edges to nodes that left are dropped; the open test is rewritten.
        assert!(t1.contains(&format!("blocked_by: [{new_t2}]\n")), "{t1}");
        assert!(t1.contains("refs: []\n"));
        // Prose about the checkpoint stays; summary lines are added.
        assert!(t1.contains("chk-1 did it."));
        assert!(t1.contains(&format!(
            "# Migrated records\n\n- chk-1 (2026-10-23): Did it (log record, migration digest {})\n",
            chk_record.digest
        )));
        assert!(t1.contains(
            "- test-1 (done, 2026-10-23): Proves it (check result, migration digest sha256:"
        ));
        assert!(
            p.warnings
                .iter()
                .any(|w| w.code == "migrate.open_test_kept")
        );
        // Converted ids aren't reported as unresolved.
        assert!(
            !p.warnings
                .iter()
                .any(|w| w.code == "migrate.unresolved_ref")
        );
    }

    #[test]
    fn a_graph_that_commits_its_work_keeps_its_history_in_git() {
        let chk = node(
            "chk-1-c.md",
            "---\nid: chk-1\ntype: checkpoint\ntitle: Did it\nstatus: done\ncheckpoint_kind: implementation\nscope: [task-1]\nrelates: [task-1]\ncreated: 2026-10-23\nupdated: 2026-10-23\n---\nBody of chk-1, about task-1.\n",
        );
        let done_test = node(
            "test-1-x.md",
            "---\nid: test-1\ntype: test\ntitle: Proves it\nstatus: done\nrelates: [task-1]\ncreated: 2026-10-23\nupdated: 2026-10-23\n---\nEvidence.\n",
        );
        let mut src = source(vec![
            task(
                1,
                "blocked_by: [test-1]\nrefs: [chk-1]\n",
                "\n# Overview\n\nchk-1 did it.\n\n# Progress\n\n- step one\n",
            ),
            chk,
            done_test,
        ]);
        src.events = vec![TextFile {
            path: ".mdkg/work/events/events.jsonl".into(),
            content: "{\"ts\":\"2026-10-23\",\"kind\":\"x\",\"refs\":[\"task-1\"]}\n".into(),
            ws: "root".into(),
        }];
        let store = plan(&src).unwrap();
        src.history = History::Git;
        let p = plan(&src).unwrap();
        // The mode is part of the plan, so of its hash.
        assert_eq!(p.history, History::Git);
        assert_ne!(p.hash(), store.hash());
        // Nothing becomes a record, and nothing leaves git.
        assert!(p.records.is_empty(), "{:?}", p.records);
        assert!(
            p.files.iter().all(|f| f.action == Action::Write),
            "{:?}",
            p.files
        );
        let id = |legacy: &str, ty: &str, path: &str| {
            format!(
                "{ty}-{}",
                mint::node_uuid(G, legacy, "2026-10-23", path).unwrap()
            )
        };
        let new_chk = id("chk-1", "checkpoint", "work/chk-1-c.md");
        let new_test = id("test-1", "test", "work/test-1-x.md");
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        // The checkpoint and the finished test are nodes with new ids, their
        // refs and bodies rewritten like any other's.
        let c = file(&p, "chk-1-c.md").content.clone().unwrap();
        assert!(
            c.contains(&format!("\nid: {new_chk}\nlegacy_id: chk-1\n")),
            "{c}"
        );
        assert!(c.contains(&format!("\nscope: [{new1}]\n")), "{c}");
        assert!(c.contains("\ncheckpoint_kind: implementation\n"), "{c}");
        assert!(
            c.contains(&format!(
                "Body of [chk-1]({new_chk}), about [task-1]({new1})."
            )),
            "{c}"
        );
        let t = file(&p, "test-1-x.md").content.clone().unwrap();
        assert!(t.contains(&format!("\nid: {new_test}\n")), "{t}");
        assert!(t.contains("\nstatus: done\n"), "{t}");
        // Refs to them stay; no summary lines; the progress section stays.
        let t1 = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t1.contains(&format!("blocked_by: [{new_test}]\n")), "{t1}");
        assert!(t1.contains(&format!("refs: [{new_chk}]\n")), "{t1}");
        assert!(!t1.contains("# Migrated records"), "{t1}");
        assert!(t1.contains("# Progress\n\n- step one\n"), "{t1}");
        assert!(t1.contains(&format!("[chk-1]({new_chk}) did it.")), "{t1}");
        let planes: Vec<(&str, &str)> = p
            .nodes
            .iter()
            .map(|n| (n.legacy_id.as_str(), n.plane.as_str()))
            .collect();
        assert!(planes.contains(&("chk-1", "checkpoint")), "{planes:?}");
        assert!(planes.contains(&("test-1", "check-result")), "{planes:?}");
        assert!(p.nodes.iter().all(|n| n.id.is_some() && n.record.is_none()));
        // A finished test isn't reported as an open one.
        assert!(
            !p.warnings
                .iter()
                .any(|w| w.code == "migrate.open_test_kept")
        );
    }

    #[test]
    fn two_workspaces_keep_their_ids_apart() {
        let mut src = source(vec![
            task(1, "relates: [aux:task-1]\n", "\n# Overview\n\nSee aux:task-1.\n"),
            NodeFile {
                path: "aux/.mdkg/work/task-1-a.md".into(),
                content: "---\nid: task-1\ntype: task\ntitle: Aux\nstatus: todo\nrelates: [root:task-1, task-2]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n# Progress\n\n- aux step\n".into(),
                ws: "aux".into(),
            },
            NodeFile {
                path: "aux/.mdkg/work/task-2-a.md".into(),
                content: "---\nid: task-2\ntype: task\ntitle: Aux 2\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n".into(),
                ws: "aux".into(),
            },
            NodeFile {
                path: "aux/.mdkg/work/chk-1-a.md".into(),
                content: "---\nid: chk-1\ntype: checkpoint\ntitle: Aux chk\nstatus: done\nscope: [task-1]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n".into(),
                ws: "aux".into(),
            },
        ]);
        src.workspaces.push("aux".into());
        src.folders.insert("aux".into(), "aux/.mdkg/".into());
        let mint = |legacy: &str, path: &str| {
            format!(
                "task-{}",
                mint::node_uuid(G, legacy, "2026-10-24", path).unwrap()
            )
        };
        // The root workspace mints from inside its folder, the other from
        // the graph's root: two task-1s, two ids.
        let root1 = mint("task-1", "work/task-1-t.md");
        let aux1 = mint("task-1", "../aux/.mdkg/work/task-1-a.md");
        let aux2 = mint("task-2", "../aux/.mdkg/work/task-2-a.md");
        assert_ne!(root1, aux1);
        for history in [History::Store, History::Git] {
            src.history = history;
            let p = plan(&src).unwrap();
            let names: Vec<(&str, &str)> = p
                .workspaces
                .iter()
                .map(|w| (w.alias.as_str(), w.folder.as_str()))
                .collect();
            assert_eq!(names, [("aux", "aux/.mdkg/"), ("root", ".mdkg/")]);
            let t1 = file(&p, ".mdkg/work/task-1-t.md").content.clone().unwrap();
            assert!(t1.contains(&format!("\nrelates: [{aux1}]\n")), "{t1}");
            assert!(t1.contains(&format!("See [aux:task-1]({aux1}).")), "{t1}");
            let a1 = file(&p, "aux/.mdkg/work/task-1-a.md")
                .content
                .clone()
                .unwrap();
            assert!(a1.contains(&format!("\nid: {aux1}\n")), "{a1}");
            // Unqualified, task-2 is aux's own.
            assert!(
                a1.contains(&format!("\nrelates: [{root1}, {aux2}]\n")),
                "{a1}"
            );
            if history == History::Store {
                // Records say which workspace's node they came from.
                let chk = p
                    .records
                    .iter()
                    .find(|r| r.kind == RecordKind::LegacyCheckpoint)
                    .expect("record");
                assert_eq!(chk.body["workspace"], "aux");
                assert_eq!(chk.subject, std::slice::from_ref(&aux1));
                let review = p
                    .records
                    .iter()
                    .find(|r| r.kind == RecordKind::ReviewRecord)
                    .expect("review");
                assert_eq!(review.body["workspace"], "aux");
            } else {
                let c = file(&p, "aux/.mdkg/work/chk-1-a.md")
                    .content
                    .clone()
                    .unwrap();
                assert!(c.contains(&format!("\nscope: [{aux1}]\n")), "{c}");
            }
        }
    }

    #[test]
    fn workspace_ids_outside_the_graph_and_mint_paths_stay_apart() {
        let mut src = source(vec![
            task(1, "", "\n# Overview\n\nx\n"),
            NodeFile {
                path: "aux/.mdkg/work/task-1-a.md".into(),
                content: "---\nid: task-1\ntype: task\ntitle: Aux\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n".into(),
                ws: "aux".into(),
            },
        ]);
        src.workspaces.push("aux".into());
        src.folders.insert("aux".into(), "aux/.mdkg/".into());
        src.outside.push(TextFile {
            path: "AGENTS.md".into(),
            content: "Run `mdkg show task-1 --ws aux`, then `mdkg show aux:task-1`.\n".into(),
            ws: String::new(),
        });
        let p = plan(&src).unwrap();
        let aux1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "../aux/.mdkg/work/task-1-a.md").unwrap()
        );
        let agents = p.patch[0].content.clone().unwrap();
        // Unqualified, task-1 could be either workspace's: it stays.
        assert!(agents.contains("`mdkg show task-1 --ws aux`"), "{agents}");
        assert!(agents.contains(&format!("`mdkg show {aux1}`")), "{agents}");
        assert!(
            p.warnings
                .iter()
                .any(|w| w.code == "migrate.ambiguous_ref" && w.message.contains("task-1")),
            "{:?}",
            p.warnings
        );
        // A root file can't mint from another workspace's path: those start
        // `../`, and nothing inside `.mdkg/` does.
        src.folders.insert("team".into(), "work/team/.mdkg/".into());
        let root = src.mint_path(".mdkg/work/team/.mdkg/work/task-1.md", "root");
        let team = src.mint_path("work/team/.mdkg/work/task-1.md", "team");
        assert_eq!(root, "work/team/.mdkg/work/task-1.md");
        assert_eq!(team, "../work/team/.mdkg/work/task-1.md");
    }

    #[test]
    fn sections_gates_extension_keys_and_tier_c() {
        let body = "\n# Overview\n\nx\n\n# Progress (2026-10-19)\n\n- step one\n\n# Test Plan\n\n- `mdkg capability search \"q\" --json`\n";
        let goal = node(
            "goal-1-g.md",
            "---\nid: goal-1\ntype: goal\ntitle: G\nstatus: todo\nrequired_checks: [mdkg validate, mdkg capability search \"q\" --json]\nx_acme_flag: on\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
        );
        let lp = node(
            "loop-1-l.md",
            "---\nid: loop-1\ntype: loop\ntitle: L\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
        );
        let p = plan(&source(vec![task(1, "", body), goal, lp])).unwrap();
        let t1 = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t1.contains("# Progress (2026-10-19)\n\nProgress notes moved to the work log at migration: review record, migration digest sha256:"));
        assert!(!t1.contains("step one"));
        assert!(t1.contains("- `mdkg search \"q\" --json`"));
        let review = p
            .records
            .iter()
            .find(|r| r.kind == RecordKind::ReviewRecord)
            .unwrap();
        assert_eq!(review.body["text"], "\n- step one\n\n");
        let g = file(&p, "goal-1-g.md").content.clone().unwrap();
        assert!(g.contains("required_checks: [mdkg validate, mdkg search \"q\" --json]\n"));
        assert!(g.contains("x-acme.flag: on\n"));
        assert!(p.warnings.iter().any(|w| w.code == "migrate.tier_c_kept"));
    }

    #[test]
    fn events_and_subgraphs() {
        let mut src = source(vec![task(
            1,
            "relates: [child:task-7, nochild:task-8]\n",
            "\nSee child:task-7.\n",
        )]);
        src.subgraphs.insert(
            "child".into(),
            Some(ChildGraph {
                graph_id: "g2".into(),
                ids: [("task-7".to_string(), "task-n7".to_string())].into(),
            }),
        );
        src.subgraphs.insert("nochild".into(), None);
        src.events.push(TextFile {
            path: ".mdkg/work/events/events.jsonl".into(),
            content: "{\"ts\":\"2026-05-15T01:17:46.639Z\",\"kind\":\"NODE_CREATED\",\"refs\":[\"task-1\"]}\n".into(),
                    ws: "root".into(),
});
        let p = plan(&src).unwrap();
        let t1 = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t1.contains("relates: [mdkg://g2/task-n7]\n"), "{t1}");
        assert!(
            t1.contains("frontmatter: relates: nochild:task-8\n"),
            "{t1}"
        );
        assert!(t1.contains("See [child:task-7](mdkg://g2/task-n7)."));
        assert_eq!(file(&p, "events.jsonl").action, Action::Delete);
        let ev = p
            .records
            .iter()
            .find(|r| r.kind == RecordKind::LegacyEvent)
            .unwrap();
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        assert_eq!(ev.subject, [new1]);
    }

    #[test]
    fn instruction_patch() {
        let mut src = source(vec![task(1, "", "")]);
        src.outside.push(TextFile {
            path: "AGENTS.md".into(),
            content: "Run `mdkg task start task-1`, then `mdkg task done task-1 --checkpoint \"x\"`.\nGate: mdkg capability search \"q\" --json\n".into(),
                    ws: "root".into(),
});
        let p = plan(&src).unwrap();
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        assert_eq!(p.patch.len(), 1);
        assert_eq!(
            p.patch[0].content.as_deref().unwrap(),
            format!(
                "Run `mdkg claim {new1}`, then `mdkg task done {new1} --checkpoint \"x\"`.\nGate: mdkg search \"q\" --json\n"
            )
        );
        assert!(p.warnings.iter().any(|w| w.code == "migrate.command_kept"));
    }

    #[test]
    fn opaque_refs_keep_their_values_and_only_exact_ids_change() {
        let fm = |id: &str, ty: &str, extra: &str| {
            node(
                &format!("{id}-x.md"),
                &format!(
                    "---\nid: {id}\ntype: {ty}\ntitle: T\nstatus: todo\n{extra}created: 2026-10-24\nupdated: 2026-10-24\n---\n"
                ),
            )
        };
        let s = source(vec![
            task(1, "", ""),
            fm(
                "loop-1",
                "loop",
                "scope_refs: [not-a-ref, task-1, root:task-1]\n",
            ),
            fm(
                "proposal-1",
                "proposal",
                "evidence_refs: [root:task-19, task-1]\n",
            ),
            fm(
                "goal-1",
                "goal",
                "goal_state: active\ngoal_condition: c\nscope_refs: [task-1]\n",
            ),
        ]);
        let p = plan(&s).expect("plan");
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        let text = |path: &str| file(&p, path).content.clone().expect("content");
        // A loop's scope_refs and a workflow type's evidence_refs are opaque:
        // only an item that is exactly a node's 0.6.0 id changes.
        let l = text(".mdkg/work/loop-1-x.md");
        assert!(
            l.contains(&format!("\nscope_refs: [not-a-ref, {new1}, root:task-1]\n")),
            "{l}"
        );
        assert!(!l.contains("# Migrated records"), "{l}");
        let pr = text(".mdkg/work/proposal-1-x.md");
        assert!(
            pr.contains(&format!("\nevidence_refs: [root:task-19, {new1}]\n")),
            "{pr}"
        );
        // A goal's scope_refs are refs.
        let g = text(".mdkg/work/goal-1-x.md");
        assert!(g.contains(&format!("\nscope_refs: [{new1}]\n")), "{g}");
    }

    #[test]
    fn task_update_becomes_the_command_its_status_means() {
        let mut src = source(vec![task(1, "", "")]);
        src.outside.push(TextFile {
            path: "AGENTS.md".into(),
            content: "`mdkg task update task-1 --status progress`\n\
                      `mdkg task update task-1 --status=done --json`\n\
                      `mdkg task update task-1 --status review`\n\
                      `mdkg task update task-1 --status todo`\n\
                      `mdkg task update task-1`\n\
                      mdkg task update \"$TASK_ID\" --status progress\n"
                .into(),
            ws: "root".into(),
        });
        let p = plan(&src).unwrap();
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        assert_eq!(
            p.patch[0].content.as_deref().unwrap(),
            format!(
                "`mdkg task update {new1} --status progress`\n\
                 `mdkg done {new1} --json`\n\
                 `mdkg update {new1} --status review`\n\
                 `mdkg task update {new1} --status todo`\n\
                 `mdkg task update {new1}`\n\
                 mdkg task update \"$TASK_ID\" --status progress\n"
            )
        );
        let kept = p
            .warnings
            .iter()
            .find(|w| w.code == "migrate.command_kept")
            .expect("warning");
        // Progress (claim would take a path of that name before the node),
        // the statuses no command takes, and none.
        assert!(kept.message.starts_with("4 "), "{kept:?}");
    }

    #[test]
    fn core_list_and_examples() {
        let mut src = source(vec![task(1, "", "")]);
        src.core_lists = vec![TextFile {
            path: ".mdkg/core/core.md".into(),
            content: "# core\ntask-1\ngone-2\n".into(),
            ws: "root".into(),
        }];
        src.outside.push(TextFile {
            path: ".mdkg/README.md".into(),
            content: "mdkg task start task-1\n".into(),
            ws: "root".into(),
        });
        src.examples.push(".mdkg/README.md".into());
        let p = plan(&src).unwrap();
        let new1 = format!(
            "task-{}",
            mint::node_uuid(G, "task-1", "2026-10-24", "work/task-1-t.md").unwrap()
        );
        assert_eq!(
            file(&p, "core.md").content.as_deref().unwrap(),
            format!("# core\n{new1}\ngone-2\n")
        );
        assert_eq!(
            p.patch[0].content.as_deref().unwrap(),
            "mdkg claim task-1\n"
        );
    }

    #[test]
    fn lines_are_written_as_0_6_0_read_them() {
        let n = node(
            "task-1-t.md",
            "\u{feff}---\nid: task-1\ntype: task\ntitle:\u{a0}T\u{a0}\nstatus: todo\npriority: 3.7\ntags: [ a ,b ]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\nbody\n",
        );
        let p = plan(&source(vec![n])).unwrap();
        let t = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t.starts_with("---\nid: task-"), "{t:?}");
        assert!(t.contains("\ntitle: T\n"), "{t:?}");
        assert!(t.contains("\npriority: 3\n"), "{t:?}");
        assert!(t.contains("\ntags: [a, b]\n"), "{t:?}");
    }

    #[test]
    fn keys_outside_the_node_schema_become_x_local() {
        let n = node(
            "task-1-t.md",
            "---\nid: task-1\ntype: task\ntitle: T\nstatus: todo\ncolour: blue\nlinked_refs: [task-1]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
        );
        let p = plan(&source(vec![n])).unwrap();
        let t = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t.contains("\nx-local.colour: blue\n"), "{t}");
        // Opaque: an id inside the value isn't rewritten.
        assert!(t.contains("\nx-local.linked_refs: [task-1]\n"), "{t}");
        let converted: Vec<&str> = p
            .warnings
            .iter()
            .filter(|w| w.code == "migrate.key_converted")
            .map(|w| w.message.as_str())
            .collect();
        assert_eq!(converted.len(), 2, "{converted:?}");
        assert!(converted[0].contains("becomes x-local."), "{converted:?}");
    }

    #[test]
    fn keys_that_would_share_a_0_7_name_stop_the_plan() {
        // A converted key beside an extension key spelled for it, or beside
        // that name as written; two spellings of one extension key.
        for keys in [
            "colour: blue\nx_local_colour: red\n",
            "colour: blue\nx-local.colour: red\n",
            "x_acme_note: a\nx-acme.note: b\n",
            // A legacy_id of its own (spaced, so it isn't read as migrated)
            // beside the one written for its new id.
            "legacy_id : task-9\n",
        ] {
            let n = node(
                "task-1-t.md",
                &format!(
                    "---\nid: task-1\ntype: task\ntitle: T\nstatus: todo\n{keys}created: 2026-10-24\nupdated: 2026-10-24\n---\n"
                ),
            );
            let e = plan(&source(vec![n])).expect_err(keys);
            assert!(e.message.contains("would both become"), "{keys}: {e:?}");
        }
    }

    #[test]
    fn identity_v2_refs_name_nodes_by_uuid() {
        const N1: &str = "1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b";
        const NF: &str = "2c3d4e5f-6071-4829-ab3c-4d5e6f7a8b9c";
        let mut s = source(vec![task(
            1,
            &format!("graph_id: {G}\nnode_id: {N1}\nrelates: [mdkg://{G}/{NF}]\n"),
            &format!("See mdkg://{G}/{N1}.\n"),
        )]);
        s.graph_id_minted = false;
        s.v2 = Some(V2Source {
            manifest: TextFile {
                path: ".mdkg/graph.json".into(),
                content: "{}".into(),
                ws: "root".into(),
            },
            uuids: [(".mdkg/work/task-1-t.md".to_string(), N1.to_string())].into(),
        });
        // A v2 node only another branch holds keeps its UUID there too.
        s.foreign = vec![ForeignNode {
            legacy_id: "task-9".into(),
            id: Some(format!("task-{NF}")),
            node_id: Some(NF.into()),
            node_type: "task".into(),
            status: Some("todo".into()),
            created: "2026-10-24".into(),
            path: "work/task-9-x.md".into(),
            ws: "root".into(),
        }];
        let p = plan(&s).expect("plan");
        let t = file(&p, ".mdkg/work/task-1-t.md")
            .content
            .as_deref()
            .expect("content");
        assert!(
            t.starts_with(&format!("---\nid: task-{N1}\nlegacy_id: task-1\n")),
            "{t}"
        );
        assert!(!t.contains("graph_id:") && !t.contains("node_id:"), "{t}");
        assert!(t.contains("\ncreated: 2026-10-24T00:00:00.000Z\n"), "{t}");
        assert!(t.contains("\nrelates: []\n"), "{t}");
        assert!(
            t.contains(&format!("task-{NF} (mdkg://{G}/{NF}, on another branch)")),
            "{t}"
        );
        assert!(t.contains(&format!("See task-{N1}.")), "{t}");
        assert!(
            p.files
                .iter()
                .any(|f| f.path == ".mdkg/graph.json" && f.action == Action::Delete)
        );
        assert!(p.warnings.iter().all(|w| w.code != "migrate.key_converted"));
    }

    #[test]
    fn identity_v2_refs_to_other_branches_resolve_by_uuid() {
        const N1: &str = "1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b";
        const NA: &str = "2c3d4e5f-6071-4829-ab3c-4d5e6f7a8b9c";
        const NB: &str = "3d4e5f60-7182-4a3b-8c4d-5e6f708192a3";
        const NC: &str = "4e5f6071-8293-4b4c-9d5e-6f708192a3b4";
        let r = |n: &str| format!("mdkg://{G}/{n}");
        let mut s = source(vec![task(
            1,
            &format!(
                "graph_id: {G}\nnode_id: {N1}\nrelates: [{}, {}]\nx_acme_locator: {}\n",
                r(NA),
                r(NC),
                r(N1)
            ),
            &format!("Run `mdkg show {}`; see {}.\n", r(NA), r(NA)),
        )]);
        s.graph_id_minted = false;
        s.v2 = Some(V2Source {
            manifest: TextFile {
                path: ".mdkg/graph.json".into(),
                content: "{}".into(),
                ws: "root".into(),
            },
            uuids: [(".mdkg/work/task-1-t.md".to_string(), N1.to_string())].into(),
        });
        s.outside = vec![TextFile {
            path: "AGENTS.md".into(),
            content: format!("Run `mdkg show {}`.\n", r(NB)),
            ws: "root".into(),
        }];
        let foreign =
            |legacy: &str, id: Option<String>, node_id: Option<&str>, path: &str| ForeignNode {
                legacy_id: legacy.into(),
                id,
                node_id: node_id.map(str::to_string),
                node_type: "task".into(),
                status: Some("todo".into()),
                created: "2026-10-24".into(),
                path: path.into(),
                ws: "root".into(),
            };
        s.foreign = vec![
            // Two branches' task-9s: their legacy id is ambiguous, their
            // UUIDs aren't.
            foreign("task-9", None, Some(NA), "work/task-9-a.md"),
            foreign("task-9", None, Some(NB), "work/task-9-b.md"),
            // A branch already migrated: its UUID is in its id.
            foreign(
                "task-8",
                Some(format!("task-{NC}")),
                None,
                "work/task-8-c.md",
            ),
        ];
        let p = plan(&s).expect("plan");
        let t = file(&p, ".mdkg/work/task-1-t.md")
            .content
            .as_deref()
            .expect("content");
        assert!(t.contains("\nrelates: []\n"), "{t}");
        assert!(
            t.contains(&format!("task-{NA} ({}, on another branch)", r(NA))),
            "{t}"
        );
        assert!(
            t.contains(&format!("task-{NC} ({}, on another branch)", r(NC))),
            "{t}"
        );
        // Code gets the id; prose keeps a branch-only ref as written.
        assert!(
            t.contains(&format!("Run `mdkg show task-{NA}`; see {}.", r(NA))),
            "{t}"
        );
        // An extension value is opaque.
        assert!(t.contains(&format!("\nx-acme.locator: {}\n", r(N1))), "{t}");
        let agents = p
            .patch
            .iter()
            .find(|f| f.path == "AGENTS.md")
            .expect("patch");
        assert_eq!(
            agents.content.as_deref(),
            Some(format!("Run `mdkg show task-{NB}`.\n").as_str())
        );
    }

    #[test]
    fn text_fields_keep_their_ids_as_written() {
        let s = source(vec![
            task(
                1,
                "relates: [task-2]\nartifacts: [task-2/receipt.json]\nx_acme_note_refs: [task-2, not an id]\nx-acme.see_refs: [task-2]\n",
                "",
            ),
            node(
                "task-2-t.md",
                "---\nid: task-2\ntype: task\ntitle: Close task-1 and prepare root:task-1\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
            ),
            node(
                "goal-1-g.md",
                "---\nid: goal-1\ntype: goal\ntitle: Ship task-2\nstatus: todo\ngoal_state: active\ngoal_condition: task-2 is done\nscope_refs: [task-2]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
            ),
        ]);
        let p = plan(&s).expect("plan");
        let new2 = format!(
            "task-{}",
            mint::node_uuid(G, "task-2", "2026-10-24", "work/task-2-t.md").unwrap()
        );
        let t1 = file(&p, ".mdkg/work/task-1-t.md")
            .content
            .as_deref()
            .expect("content");
        // The ref is rewritten; the text fields aren't.
        assert!(t1.contains(&format!("\nrelates: [{new2}]\n")), "{t1}");
        assert!(t1.contains("\nartifacts: [task-2/receipt.json]\n"), "{t1}");
        // Extension values are opaque, `_refs` names too.
        assert!(
            t1.contains("\nx-acme.note_refs: [task-2, not an id]\n"),
            "{t1}"
        );
        assert!(t1.contains("\nx-acme.see_refs: [task-2]\n"), "{t1}");
        let t2 = file(&p, ".mdkg/work/task-2-t.md")
            .content
            .as_deref()
            .expect("content");
        assert!(
            t2.contains("\ntitle: Close task-1 and prepare root:task-1\n"),
            "{t2}"
        );
        let g = file(&p, ".mdkg/work/goal-1-g.md")
            .content
            .as_deref()
            .expect("content");
        assert!(g.contains("\ntitle: Ship task-2\n"), "{g}");
        assert!(g.contains("\ngoal_condition: task-2 is done\n"), "{g}");
        assert!(g.contains(&format!("\nscope_refs: [{new2}]\n")), "{g}");
    }

    #[test]
    fn config_edits() {
        assert_eq!(
            upgrade_config("{}", G, &[]).unwrap(),
            format!(
                "{{\n  \"schema_version\": 3,\n  \"format_version\": \"0.7\",\n  \"graph_id\": \"{G}\",\n  \"projections\": [],\n  \"work\": {{\n    \"location\": \"git\"\n  }}\n}}"
            )
        );
        assert_eq!(
            upgrade_config(
                "{\n    \"tool\": \"mdkg\",\n    \"schema_version\": 1,\n    \"a\": [1]\n}\n",
                G,
                &["agents-md", "mcp:claude"]
            )
            .unwrap(),
            format!(
                "{{\n    \"tool\": \"mdkg\",\n    \"schema_version\": 3,\n    \"format_version\": \"0.7\",\n    \"graph_id\": \"{G}\",\n    \"a\": [\n        1\n    ],\n    \"projections\": [\n        \"agents-md\",\n        \"mcp:claude\"\n    ],\n    \"work\": {{\n        \"location\": \"git\"\n    }}\n}}\n"
            )
        );
        // A list the config has already is kept as it is.
        assert_eq!(
            upgrade_config("{\"projections\": [\"agents-md\"]}", G, &["mcp:claude"]).unwrap(),
            format!(
                "{{\n  \"schema_version\": 3,\n  \"format_version\": \"0.7\",\n  \"graph_id\": \"{G}\",\n  \"projections\": [\n    \"agents-md\"\n  ],\n  \"work\": {{\n    \"location\": \"git\"\n  }}\n}}"
            )
        );
        // A storage setting the config has is kept as it is, either spelling.
        for work in [
            "{\"location\": \"git\"}",
            "{\"committed\": \"definitions\"}",
        ] {
            let out = upgrade_config(&format!("{{\"work\": {work}}}"), G, &[]).unwrap();
            let v: Value = serde_json::from_str(&out).unwrap();
            let want: Value = serde_json::from_str(work).unwrap();
            assert_eq!(v["work"], want, "{out}");
        }
        // An implicit one is said: work stays in git.
        let out = upgrade_config("{\"work\": {\"priority_max\": 9}}", G, &[]).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["work"], json!({"priority_max": 9, "location": "git"}));
        assert!(upgrade_config("[1]", G, &[]).is_none());
    }

    #[test]
    fn crlf_files_keep_their_endings() {
        let n = node(
            "task-1-t.md",
            "---\r\nid: task-1\r\ntype: task\r\ntitle: T\r\nstatus: todo\r\ncreated: 2026-10-24\r\nupdated: 2026-10-24\r\n---\r\nbody\r\n",
        );
        let p = plan(&source(vec![n])).unwrap();
        let t = file(&p, "task-1-t.md").content.clone().unwrap();
        assert!(t.contains("\r\nlegacy_id: task-1\r\ntype: task\r\n"));
        assert!(!t.replace("\r\n", "").contains('\n'));
    }
}
