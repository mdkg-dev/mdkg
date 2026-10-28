//! `mdkg graph`: refs, backrefs, the ready set and the critical path to a
//! goal.

use std::path::Path;

use clap::Subcommand;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::resolve::Resolution;
use mdkg_graph::{GraphError, Index, IndexNode, config, index};
use mdkg_index::graph as walk;
use mdkg_index::resolve::{handle_error, resolve_handle, shadowed};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::output::{self, NodeRecord, Report};
use crate::read;

/// The work items `next` hands out, which the ready set and critical path
/// are made of.
pub const WORK_TYPES: &[&str] = &["feat", "task", "bug", "test", "spike"];

/// Statuses that mean someone is on it or waiting, never ready.
const NOT_READY: &[&str] = &["progress", "review", "blocked"];

#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// List the nodes a node points at (its edge fields and refs).
    #[command(after_help = examples!("graph refs <id>", "graph refs <id> --json"))]
    Refs {
        /// A full id, a unique id prefix or a title fragment.
        node: String,
        /// Workspace hint.
        #[arg(long)]
        ws: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the nodes that point at a node.
    #[command(after_help = examples!("graph backrefs <id>"))]
    Backrefs {
        /// A full id, a unique id prefix or a title fragment.
        node: String,
        /// Workspace hint.
        #[arg(long)]
        ws: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List work items ready to start: a ready status and nothing open
    /// blocking them, in the order `next` picks.
    #[command(after_help = examples!("graph ready", "graph ready --limit 5 --json"))]
    Ready {
        /// At most this many (default: all).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the longest chain of unfinished work a goal waits on.
    #[command(after_help = examples!("graph critical-path", "graph critical-path <goal-id>"))]
    CriticalPath {
        /// The goal (default: the active goal).
        goal: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

impl GraphCommand {
    /// The output's schema name: one per subcommand.
    pub fn name(&self) -> &'static str {
        match self {
            GraphCommand::Refs { .. } => "graph.refs",
            GraphCommand::Backrefs { .. } => "graph.backrefs",
            GraphCommand::Ready { .. } => "graph.ready",
            GraphCommand::CriticalPath { .. } => "graph.critical-path",
        }
    }

    pub fn json(&self) -> bool {
        match self {
            GraphCommand::Refs { json, .. }
            | GraphCommand::Backrefs { json, .. }
            | GraphCommand::Ready { json, .. }
            | GraphCommand::CriticalPath { json, .. } => *json,
        }
    }
}

/// One edge from the node asked about.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EdgeRecord {
    pub kind: String,
    /// The other node's id, as agents write it (`ws:id` outside the root
    /// workspace).
    pub id: String,
    /// Absent for a node outside this graph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub exists: bool,
    /// The revision a ref pins (the 64 hex digits after `@sha256:`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

/// `graph refs` and `graph backrefs`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EdgesData {
    /// The node asked about.
    #[schemars(with = "NodeRecord")]
    pub node: Value,
    /// Its edges, out (refs) or in (backrefs).
    pub edges: Vec<EdgeRecord>,
}

/// `graph ready`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadyData {
    /// The ready set, in the order `next` picks, up to --limit.
    #[schemars(with = "Vec<NodeRecord>")]
    pub nodes: Vec<Value>,
    /// Every ready item, before --limit.
    pub count: usize,
}

/// `graph critical-path`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CriticalPathData {
    /// The goal.
    #[schemars(with = "NodeRecord")]
    pub goal: Value,
    /// The longest chain of unfinished work, first to last.
    #[schemars(with = "Vec<NodeRecord>")]
    pub nodes: Vec<Value>,
    /// Unfinished work items in the goal's scope.
    pub remaining: usize,
    /// Work items in a blocking cycle; empty when there's none.
    pub cycle: Vec<String>,
}

const CARD_FIELDS: &[&str] = &[
    "id",
    "legacy_id",
    "type",
    "title",
    "status",
    "priority",
    "path",
];

fn card_fields() -> Vec<String> {
    CARD_FIELDS.iter().map(|f| f.to_string()).collect()
}

/// A handle's node, with a note in `notes` when a legacy id shadowed a
/// prefix.
fn resolve<'a>(
    graph: &'a Index,
    handle: &str,
    ws: Option<&str>,
    label: &str,
    notes: &mut Vec<Diagnostic>,
) -> Result<&'a IndexNode, GraphError> {
    let resolved = resolve_handle(graph, handle, ws);
    notes.extend(shadowed(graph, handle, ws, &resolved));
    match resolved {
        Resolution::Ok(qid) => graph
            .get(&qid)
            .ok_or_else(|| GraphError::not_found(Code::NodeNotFound, format!("{label} not found"))),
        other => Err(handle_error(graph, label, handle, &other, ws)),
    }
}

/// The active goal, when exactly one is active.
fn active_goal(graph: &Index) -> Result<&IndexNode, GraphError> {
    let mut active: Vec<&IndexNode> = graph
        .nodes
        .iter()
        .filter(|n| {
            n.node_type == "goal"
                && n.attributes.get("goal_state").and_then(|v| v.as_str()) == Some("active")
        })
        .collect();
    active.sort_by(|a, b| a.qid.cmp(&b.qid));
    match active.as_slice() {
        [one] => Ok(one),
        [] => Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "no goal is active; name one",
        )),
        many => Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "several goals are active; name one",
        )
        .with_hint(
            // Bare ids in a format 0.7 graph, where they're unique.
            many.iter()
                .map(|g| mdkg_index::resolve::display_id(graph, &g.qid))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

fn edge_records(graph: &Index, edges: Vec<walk::Edge>) -> Vec<EdgeRecord> {
    edges
        .into_iter()
        .map(|e| {
            let node = graph.get(&e.qid);
            EdgeRecord {
                title: node.map(|n| n.title.clone()),
                node_type: node.map(|n| n.node_type.clone()),
                status: node.and_then(|n| n.status.clone()),
                kind: e.kind,
                id: output::display_ref(&e.qid),
                exists: e.exists,
                pin: e.pin,
            }
        })
        .collect()
}

/// The graph with its coordination view, for one query.
struct Loaded {
    config: config::Config,
    graph: Index,
    view: Option<crate::coord::View>,
    warnings: Vec<Diagnostic>,
}

fn load(root: &Path) -> Result<Loaded, GraphError> {
    let config = config::load(root)?;
    let mut graph = index::build(root, &config)?;
    let (view, warnings) = read::effective_or_warn(root, &mut graph.nodes);
    Ok(Loaded {
        config,
        graph,
        view,
        warnings,
    })
}

fn edges(
    root: &Path,
    node: &str,
    ws: Option<&str>,
    out: bool,
) -> Result<Report<EdgesData>, GraphError> {
    let mut l = load(root)?;
    let ws = read::workspace_filter(ws)?;
    read::check_workspace(&l.config, ws.as_deref())?;
    let n = resolve(&l.graph, node, ws.as_deref(), "id", &mut l.warnings)?;
    let edges = if out {
        walk::refs(&l.graph, &n.qid)
    } else {
        walk::backrefs(&l.graph, &n.qid)
    };
    let node = read::records(l.view.as_ref(), &[n], &card_fields())
        .pop()
        .unwrap_or_default();
    Ok(Report::ok(EdgesData {
        node,
        edges: edge_records(&l.graph, edges),
    })
    .with(l.warnings))
}

fn ready(root: &Path, limit: Option<i64>) -> Result<Report<ReadyData>, GraphError> {
    let limit = match limit {
        None => usize::MAX,
        Some(n) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
        Some(_) => {
            return Err(GraphError::usage(
                Code::UsageBadLimit,
                "--limit must be a positive integer",
            )
            .with_hint("pass 1 or more, or leave it out for every ready item"));
        }
    };
    let l = load(root)?;
    let statuses: Vec<String> = l
        .config
        .next_status_preference
        .iter()
        .filter(|s| !NOT_READY.contains(&s.as_str()))
        .cloned()
        .collect();
    let ready = walk::ready_set(&l.graph, WORK_TYPES, &statuses, l.config.priority_max);
    let count = ready.len();
    let shown: Vec<&IndexNode> = ready.into_iter().take(limit).collect();
    Ok(Report::ok(ReadyData {
        nodes: read::records(l.view.as_ref(), &shown, &card_fields()),
        count,
    })
    .with(l.warnings))
}

fn critical_path(root: &Path, goal: Option<&str>) -> Result<Report<CriticalPathData>, GraphError> {
    let mut l = load(root)?;
    let g = match goal {
        Some(h) => resolve(&l.graph, h, None, "goal", &mut l.warnings)?,
        None => active_goal(&l.graph)?,
    };
    // The handle was typed, so the error doesn't echo it as a node name.
    if g.node_type != "goal" {
        return Err(GraphError::usage(
            Code::GoalNotGoal,
            format!("that node is a {}, not a goal", g.node_type),
        )
        .with_hint(format!("`{} list --type goal` lists the goals", crate::BIN)));
    }
    let cp = walk::critical_path(&l.graph, g);
    let path: Vec<&IndexNode> = cp.path.iter().filter_map(|q| l.graph.get(q)).collect();
    let fields = card_fields();
    Ok(Report::ok(CriticalPathData {
        goal: read::records(l.view.as_ref(), &[g], &fields)
            .pop()
            .unwrap_or_default(),
        nodes: read::records(l.view.as_ref(), &path, &fields),
        remaining: cp.remaining,
        cycle: cp.cycle,
    })
    .with(l.warnings))
}

pub fn run(root: &Path, command: &GraphCommand) -> u8 {
    let (name, json) = (command.name(), command.json());
    match command {
        GraphCommand::Refs { node, ws, .. } | GraphCommand::Backrefs { node, ws, .. } => {
            let out = matches!(command, GraphCommand::Refs { .. });
            let report = edges(root, node, ws.as_deref(), out).unwrap_or_else(Report::failed);
            output::emit(name, json, report, |d| {
                println!("{}", read::card(&d.node));
                for e in &d.edges {
                    let what = match (&e.title, &e.status) {
                        (Some(t), Some(s)) => format!("{s} | {t}"),
                        (Some(t), None) => t.clone(),
                        _ => "(not in this graph)".into(),
                    };
                    // A ref target is the node's own text (a URI can
                    // carry controls): escaped with the rest (SEC2).
                    println!(
                        "{}",
                        output::shown_path(&format!("  {} -> {} | {what}", e.kind, e.id))
                    );
                }
            })
        }
        GraphCommand::Ready { limit, .. } => {
            let report = ready(root, *limit).unwrap_or_else(Report::failed);
            output::emit(name, json, report, |d| {
                for (i, n) in d.nodes.iter().enumerate() {
                    println!("{:>3}. {}", i + 1, read::card(n));
                }
                eprintln!("count: {}", d.count);
            })
        }
        GraphCommand::CriticalPath { goal, .. } => {
            let report = critical_path(root, goal.as_deref()).unwrap_or_else(Report::failed);
            output::emit(name, json, report, |d| {
                println!("{}", read::card(&d.goal));
                println!(
                    "critical path: {} of {} unfinished work items",
                    d.nodes.len(),
                    d.remaining
                );
                for (i, n) in d.nodes.iter().enumerate() {
                    println!("{:>3}. {}", i + 1, read::card(n));
                }
                if !d.cycle.is_empty() {
                    eprintln!("warning: blocking cycle through {}", d.cycle.join(", "));
                }
            })
        }
    }
}
