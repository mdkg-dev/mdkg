//! Graph-level checks that need the whole index: edge targets, prev/next
//! symmetry and cycles, goal scope references and the single active goal.
//!
//! Errors are collected in the reference implementation's order, so the first
//! one is the one mdkg 0.6.0 reports. Agent workflow, archive and loop
//! reference checks are outside the phase 1 core-node compatibility scope.

use std::collections::HashSet;

use mdkg_format::{Code, Diagnostic, ids};

use crate::index::{Index, IndexNode, RefTarget, other_refs};
use crate::resolve::{Resolution, resolve_qid};

pub const GOAL_SCOPE_CONTAINER_TYPES: &[&str] = &["loop", "epic", "feat"];
pub const GOAL_SCOPE_ACTIONABLE_TYPES: &[&str] = &["feat", "task", "bug", "test", "spike"];
/// Container types, then actionable types, as the reference spreads them.
pub const GOAL_SCOPE_ALLOWED_TYPES: &[&str] =
    &["loop", "epic", "feat", "task", "bug", "test", "spike"];

/// An error about one node, located at its file.
fn node_error(node: &IndexNode, code: Code, message: String) -> Diagnostic {
    Diagnostic::error(code, message)
        .at(node.path.clone(), None)
        .for_node(node.qid.clone())
}

fn is_external_uri(value: &str) -> bool {
    ids::is_uri_ref(value) && ids::parse_identity_ref(value).is_none()
}

/// All graph errors, in reference order.
pub fn collect(
    index: &Index,
    allow_missing: bool,
    external_workspaces: &HashSet<&str>,
) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    edge_targets(index, allow_missing, external_workspaces, &mut errors);
    prev_next_symmetry(index, &mut errors);
    goal_refs(index, allow_missing, &mut errors);
    other_ref_targets(index, allow_missing, &mut errors);
    single_active_root_goal(index, &mut errors);
    prev_next_cycles(index, &mut errors);
    errors
}

fn edge_targets(
    index: &Index,
    allow_missing: bool,
    external: &HashSet<&str>,
    errors: &mut Vec<Diagnostic>,
) {
    for node in &index.nodes {
        let e = &node.edges;
        let mut lists: Vec<(&str, Vec<&str>)> = vec![
            ("relates", e.relates.iter().map(String::as_str).collect()),
            (
                "blocked_by",
                e.blocked_by.iter().map(String::as_str).collect(),
            ),
            ("blocks", e.blocks.iter().map(String::as_str).collect()),
            (
                "context_refs",
                e.context_refs.iter().map(String::as_str).collect(),
            ),
            (
                "evidence_refs",
                e.evidence_refs.iter().map(String::as_str).collect(),
            ),
        ];
        for (key, value) in [
            ("epic", &e.epic),
            ("parent", &e.parent),
            ("prev", &e.prev),
            ("next", &e.next),
        ] {
            if let Some(v) = value {
                lists.push((key, vec![v.as_str()]));
            }
        }
        for (key, values) in lists {
            for value in values {
                if matches!(key, "context_refs" | "evidence_refs") && is_external_uri(value) {
                    continue;
                }
                if index.contains(value) {
                    continue;
                }
                let workspace = value.split(':').next().unwrap_or_default();
                if !workspace.is_empty() && external.contains(workspace) {
                    continue;
                }
                if allow_missing {
                    continue;
                }
                if crate::store_work::is_store_path(&node.path) {
                    continue;
                }
                errors.push(node_error(
                    node,
                    Code::EdgeMissingTarget,
                    format!("{key} references missing node {value}"),
                ));
            }
        }
    }
}

fn prev_next_symmetry(index: &Index, errors: &mut Vec<Diagnostic>) {
    for node in &index.nodes {
        if let Some(next) = &node.edges.next
            && let Some(target) = index.get(next)
            && target.edges.prev.as_deref() != Some(node.qid.as_str())
        {
            errors.push(node_error(
                node,
                Code::EdgePrevNextAsymmetric,
                format!("next {next} missing matching prev"),
            ));
        }
        if let Some(prev) = &node.edges.prev
            && let Some(target) = index.get(prev)
            && target.edges.next.as_deref() != Some(node.qid.as_str())
        {
            errors.push(node_error(
                node,
                Code::EdgePrevNextAsymmetric,
                format!("prev {prev} missing matching next"),
            ));
        }
    }
}

fn list_attr<'a>(node: &'a IndexNode, key: &str) -> &'a [String] {
    node.attributes
        .get(key)
        .and_then(|v| v.as_list())
        .unwrap_or(&[])
}

/// The nodes a goal's `scope_refs` cover, following epic/parent descendants
/// of container types (format 0.7, section 5.2, rule 3). A descendant of a
/// type goals can't scope (a checkpoint under an epic) is skipped, with
/// everything below it; only one named in `scope_refs` is invalid. A
/// 0.6.0 graph keeps 0.6.0's walk, where every such descendant is invalid,
/// so the 0.6.0 CLI still reads what mdkg writes there (section 10).
#[derive(Debug)]
pub struct GoalScope {
    pub qids: HashSet<String>,
    pub actionable: HashSet<String>,
    pub missing_refs: Vec<String>,
    pub invalid_refs: Vec<String>,
}

pub fn goal_scope(index: &Index, goal: &IndexNode) -> GoalScope {
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue: std::collections::VecDeque<String> = Default::default();
    let mut missing_refs = Vec::new();
    for value in list_attr(goal, "scope_refs") {
        // A ref into another graph isn't checked (format 0.7, section 5).
        if index.target(value) == RefTarget::Foreign {
            continue;
        }
        match resolve_qid(index, value, Some(&goal.ws)) {
            Resolution::Ok(qid) => {
                if queued.insert(qid.clone()) {
                    queue.push_back(qid);
                }
            }
            _ => missing_refs.push(value.clone()),
        }
    }
    let named = queued.clone();
    let mut qids = HashSet::new();
    let mut actionable = HashSet::new();
    let mut invalid: Vec<String> = Vec::new();
    while let Some(qid) = queue.pop_front() {
        let Some(node) = index.get(&qid) else {
            continue;
        };
        let t = node.node_type.as_str();
        if !GOAL_SCOPE_ALLOWED_TYPES.contains(&t) {
            if named.contains(&qid) || index.graph_id.is_none() {
                invalid.push(qid);
            }
            continue;
        }
        qids.insert(qid.clone());
        if GOAL_SCOPE_ACTIONABLE_TYPES.contains(&t) {
            actionable.insert(qid.clone());
        }
        if GOAL_SCOPE_CONTAINER_TYPES.contains(&t) {
            let mut children: Vec<String> = index
                .reverse("epic", &qid)
                .iter()
                .chain(index.reverse("parent", &qid))
                .cloned()
                .collect();
            children.sort();
            children.dedup();
            for child in children {
                if queued.insert(child.clone()) {
                    queue.push_back(child);
                }
            }
        }
    }
    invalid.sort();
    invalid.dedup();
    GoalScope {
        qids,
        actionable,
        missing_refs,
        invalid_refs: invalid,
    }
}

fn goal_refs(index: &Index, allow_missing: bool, errors: &mut Vec<Diagnostic>) {
    for node in index.nodes.iter().filter(|n| n.node_type == "goal") {
        let scope = goal_scope(index, node);
        for value in &scope.missing_refs {
            if !allow_missing {
                errors.push(node_error(
                    node,
                    Code::GoalScopeRefMissing,
                    format!("scope_refs references missing node {value}"),
                ));
            }
        }
        for qid in &scope.invalid_refs {
            let type_label = index.get(qid).map_or("missing", |t| t.node_type.as_str());
            errors.push(node_error(
                node,
                Code::GoalScopeRefWrongType,
                format!(
                    "scope_refs references {qid} with type {type_label}, expected {}",
                    GOAL_SCOPE_ALLOWED_TYPES.join(", ")
                ),
            ));
        }
        let Some(active) = node.attributes.get("active_node").and_then(|v| v.as_str()) else {
            continue;
        };
        if absent_private_work(index, active) {
            continue; // A warning: absent_pointers.
        }
        let active_qid = match index.target(active) {
            RefTarget::Local { node: t, .. } => t.qid.clone(),
            RefTarget::Foreign => continue,
            RefTarget::Missing => format!("{}:{active}", node.ws),
            RefTarget::NotARef if active.contains(':') => active.to_string(),
            RefTarget::NotARef => format!("{}:{active}", node.ws),
        };
        let Some(target) = index.get(&active_qid) else {
            if !allow_missing {
                errors.push(node_error(
                    node,
                    Code::GoalActiveNodeMissing,
                    format!("active_node references missing node {active}"),
                ));
            }
            continue;
        };
        if !GOAL_SCOPE_ACTIONABLE_TYPES.contains(&target.node_type.as_str()) {
            errors.push(node_error(
                node,
                Code::GoalActiveNodeWrongType,
                format!(
                    "active_node references {active_qid} with type {}, expected {}",
                    target.node_type,
                    GOAL_SCOPE_ACTIONABLE_TYPES.join(", ")
                ),
            ));
            continue;
        }
        if !scope.qids.is_empty() && !scope.actionable.contains(&active_qid) {
            errors.push(node_error(
                node,
                Code::GoalActiveNodeOutsideScope,
                format!("active_node {active_qid} is not inside goal scope_refs"),
            ));
        }
    }
}

/// Format 0.7: `refs`, `scope`, `supersedes` and `last_active_node` name
/// nodes in the graph (section 5). `scope_refs` and `active_node` are
/// checked with the goal's rules above.
fn other_ref_targets(index: &Index, allow_missing: bool, errors: &mut Vec<Diagnostic>) {
    if allow_missing {
        return;
    }
    for node in &index.nodes {
        for (key, value) in other_refs(node) {
            if matches!(key, "scope_refs" | "active_node") {
                continue;
            }
            if index.target(value) == RefTarget::Missing
                && !crate::store_work::is_store_path(&node.path)
                && !(key == "last_active_node" && absent_private_work(index, value))
            {
                errors.push(node_error(
                    node,
                    Code::EdgeMissingTarget,
                    format!("{key} references missing node {value}"),
                ));
            }
        }
    }
}

fn single_active_root_goal(index: &Index, errors: &mut Vec<Diagnostic>) {
    let mut by_ws: Vec<(String, Vec<String>)> = Vec::new();
    for node in &index.nodes {
        let active = node.node_type == "goal"
            && node.status.as_deref() == Some("progress")
            && node.attributes.get("goal_state").and_then(|v| v.as_str()) == Some("active");
        if !active {
            continue;
        }
        match by_ws.iter_mut().find(|(ws, _)| *ws == node.ws) {
            Some((_, qids)) => qids.push(node.qid.clone()),
            None => by_ws.push((node.ws.clone(), vec![node.qid.clone()])),
        }
    }
    for (ws, mut qids) in by_ws {
        if qids.len() <= 1 {
            continue;
        }
        qids.sort();
        errors.push(
            Diagnostic::error(
                Code::GoalMultipleActive,
                format!(
                    "multiple active root goals in workspace {ws}: {}",
                    qids.join(", ")
                ),
            )
            .with_hint("activate exactly one goal with `goal activate <goal-id>`"),
        );
    }
}

fn prev_next_cycles(index: &Index, errors: &mut Vec<Diagnostic>) {
    let mut seen: HashSet<String> = HashSet::new();
    for start in &index.nodes {
        if seen.contains(&start.qid) {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut current = Some(start.qid.clone());
        while let Some(qid) = current {
            if seen.contains(&qid) {
                break;
            }
            if let Some(pos) = path.iter().position(|p| *p == qid) {
                let mut cycle: Vec<String> = path[pos..].to_vec();
                cycle.push(qid);
                let d = Diagnostic::error(
                    Code::EdgeCycle,
                    format!("cycle detected in prev/next chain: {}", cycle.join(" -> ")),
                );
                errors.push(match index.get(&cycle[0]) {
                    Some(first) => d.at(first.path.clone(), None).for_node(first.qid.clone()),
                    None => d,
                });
                break;
            }
            path.push(qid.clone());
            current = index
                .get(&qid)
                .and_then(|n| n.edges.next.clone())
                .filter(|next| index.contains(next));
        }
        seen.extend(path);
    }
}

/// The qids above `node`: its parent and epic, theirs, and so on.
pub fn ancestors(index: &Index, node: &IndexNode) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<&IndexNode> = vec![node];
    while let Some(n) = queue.pop() {
        for up in [&n.edges.parent, &n.edges.epic].into_iter().flatten() {
            if up != &node.qid
                && seen.insert(up.clone())
                && let Some(next) = index.get(up)
            {
                queue.push(next);
            }
        }
    }
    seen
}

/// A blocking edge between a node and one of its own ancestors, such as
/// a goal whose `blocks` lists its child task:
/// a warning on the node that wrote it, since the two could only wait on
/// each other. The ready set ignores such an edge.
pub fn ancestor_blocks(index: &Index) -> Vec<Diagnostic> {
    let mut warnings = Vec::new();
    for node in &index.nodes {
        // Only a node some blocking edge touches walks its ancestry, so a
        // deep hierarchy without such edges stays linear (RC1 audit E9).
        let blocked_from = index.reverse("blocks", &node.qid);
        if node.edges.blocked_by.is_empty() && blocked_from.is_empty() {
            continue;
        }
        let above = ancestors(index, node);
        if above.is_empty() {
            continue;
        }
        for b in &node.edges.blocked_by {
            if above.contains(b) {
                warnings.push(blocks_ancestor(
                    node,
                    format!("blocked_by names {b}, which is above it"),
                ));
            }
        }
        // In qid order (the reverse index keeps it), so validate's output
        // is the same every run.
        let mut blockers: Vec<&String> =
            blocked_from.iter().filter(|q| above.contains(*q)).collect();
        blockers.dedup();
        for a in blockers {
            if let Some(up) = index.get(a) {
                warnings.push(blocks_ancestor(
                    up,
                    format!("blocks names {}, which is below it", node.qid),
                ));
            }
        }
    }
    warnings
}

/// Profile C (RC1 audit E1, decided 2026-10-25): work refers to design,
/// never the other way round. A committed node that names a work node in
/// the store's folder, which git never holds, breaks every fresh clone, so
/// `validate` and `new` refuse it (`edge.private_target`). A goal's
/// `active_node` and `last_active_node` are the goal commands' own
/// pointers, not checked here. Profile B has no store nodes, so nothing
/// changes there.
pub fn private_targets(index: &Index) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    for node in index
        .nodes
        .iter()
        .filter(|n| !crate::store_work::is_store_path(&n.path))
    {
        let edges = crate::index::edge_targets(&node.edges)
            .into_iter()
            .map(|(key, qid)| (key, index.get(qid)));
        let refs = other_refs(node)
            .into_iter()
            .filter(|(key, _)| !matches!(*key, "active_node" | "last_active_node"))
            // Only a goal's scope_refs name nodes; a loop's are text
            // (format section 5).
            .filter(|(key, _)| *key != "scope_refs" || node.node_type == "goal")
            .map(|(key, value)| {
                let target = match index.target(value) {
                    RefTarget::Local { node, .. } => Some(node),
                    _ => None,
                };
                (key, target)
            });
        for (key, target) in edges.chain(refs) {
            let Some(target) = target.filter(|t| crate::store_work::is_store_path(&t.path)) else {
                continue;
            };
            errors.push(
                node_error(
                    node,
                    Code::EdgePrivateTarget,
                    format!(
                        "{key} names {}, a work node git doesn't hold: a fresh clone can't find it",
                        target.id
                    ),
                )
                .with_hint(format!(
                    "point it at a committed goal or epic, and let {} refer to this node \
                     (its epic, parent or refs)",
                    target.id
                )),
            );
        }
    }
    errors
}

/// Whether `value` names a profile C work node (by its id's type) that
/// this checkout doesn't have: one in another clone's store, say.
fn absent_private_work(index: &Index, value: &str) -> bool {
    let id = value
        .split_once(':')
        .filter(|(_, rest)| mdkg_format::NodeId::parse(rest).is_ok())
        .map_or(value, |(_, rest)| rest);
    index.private_work
        && index.target(id) == RefTarget::Missing
        // A bare id or this graph's `mdkg://` ref (format section 5).
        && mdkg_format::id::NodeRef::parse(id)
            .is_ok_and(|r| crate::store_work::STORE_WORK_TYPES.contains(&r.id.node_type()))
}

/// Profile C: a goal's `active_node` or `last_active_node` naming a work
/// node this checkout doesn't have (a fresh clone has no store nodes).
/// They're the goal's runtime pointers, so a warning, never an error (RC1
/// audit E1, orchestrator 2026-10-25; keeping them in the store is 0.7.1's).
pub fn absent_pointers(index: &Index, node: &IndexNode) -> Vec<Diagnostic> {
    ["active_node", "last_active_node"]
        .into_iter()
        .filter_map(|key| {
            let value = node.attributes.get(key)?.as_str()?;
            absent_private_work(index, value).then(|| {
                let mut d = node_error(
                    node,
                    Code::EdgeTargetNotOnBranch,
                    format!(
                        "{key} {value} isn't in this checkout: it's a work node in another \
                         clone's store"
                    ),
                );
                d.severity = mdkg_format::Severity::Warning;
                d.with_hint("nothing to fix here; work from the goal's scope with `goal next`")
            })
        })
        .collect()
}

fn blocks_ancestor(node: &IndexNode, message: String) -> Diagnostic {
    let mut d = node_error(node, Code::EdgeBlocksAncestor, message);
    d.severity = mdkg_format::Severity::Warning;
    d.with_hint("a parent, epic or goal waits on what's under it already: drop the edge")
}

/// In profile C, a work node in the
/// store's folder is shared by every worktree, while knowledge lives per
/// branch, so a target it names may be on another branch. Those are
/// warnings (`edge.target_not_on_branch`), never errors; the same refs
/// from any other node stay errors in [`collect`].
pub fn branch_local_targets(index: &Index, external: &HashSet<&str>) -> Vec<Diagnostic> {
    let mut warnings = Vec::new();
    for node in index
        .nodes
        .iter()
        .filter(|n| crate::store_work::is_store_path(&n.path))
    {
        let e = &node.edges;
        let mut refs: Vec<(&str, &str)> = Vec::new();
        for (key, list) in [
            ("relates", &e.relates),
            ("blocked_by", &e.blocked_by),
            ("blocks", &e.blocks),
            ("context_refs", &e.context_refs),
            ("evidence_refs", &e.evidence_refs),
        ] {
            refs.extend(list.iter().map(|v| (key, v.as_str())));
        }
        for (key, value) in [
            ("epic", &e.epic),
            ("parent", &e.parent),
            ("prev", &e.prev),
            ("next", &e.next),
        ] {
            if let Some(v) = value {
                refs.push((key, v.as_str()));
            }
        }
        for (key, value) in refs {
            if matches!(key, "context_refs" | "evidence_refs") && is_external_uri(value) {
                continue;
            }
            let workspace = value.split(':').next().unwrap_or_default();
            if index.contains(value) || (!workspace.is_empty() && external.contains(workspace)) {
                continue;
            }
            warnings.push(not_on_branch(node, key, value));
        }
        for (key, value) in other_refs(node) {
            if !matches!(key, "scope_refs" | "active_node")
                && index.target(value) == RefTarget::Missing
            {
                warnings.push(not_on_branch(node, key, value));
            }
        }
        for target in crate::validate::body_links(&node.body) {
            if index.target(target) == RefTarget::Missing {
                warnings.push(not_on_branch(node, "a body link", target));
            }
        }
    }
    warnings
}

fn not_on_branch(node: &IndexNode, key: &str, value: &str) -> Diagnostic {
    let mut d = node_error(
        node,
        Code::EdgeTargetNotOnBranch,
        format!("{key} references {value}, which isn't on this branch"),
    );
    d.severity = mdkg_format::Severity::Warning;
    d.with_hint("a shared work node may name knowledge another branch holds; it resolves once that branch merges")
}
