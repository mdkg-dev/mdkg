//! Graph walks over the graph as read: refs and
//! backrefs, the ready set, and the critical path to a goal.
//!
//! Callers set each node's effective status (the coordinator's temporary
//! states over the file's) before asking, as `list` does.

use std::collections::{BTreeSet, HashMap, VecDeque};

use mdkg_format::id::NodeRef;
use mdkg_graph::graph_checks::{
    GOAL_SCOPE_ACTIONABLE_TYPES, GOAL_SCOPE_CONTAINER_TYPES, ancestors, goal_scope,
};
use mdkg_graph::index::{RefTarget, edge_targets};
use mdkg_graph::validate::body_links;
use mdkg_graph::{Index, IndexNode};
use schemars::JsonSchema;
use serde::Serialize;

/// Statuses that end a node's work: it blocks nothing any more.
pub const FINISHED: &[&str] = &[
    "done",
    "cancelled",
    "canceled",
    "superseded",
    "accepted",
    "rejected",
    "archived",
    "achieved",
];

pub fn is_finished(status: Option<&str>) -> bool {
    status.is_some_and(|s| FINISHED.contains(&s))
}

/// One edge, seen from the node asked about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Edge {
    /// The edge's field: `parent`, `blocked_by`, `refs` and so on.
    pub kind: String,
    /// The node at the other end.
    pub qid: String,
    /// Whether that node is in this graph (a subgraph's isn't).
    pub exists: bool,
    /// The revision a ref pins (`…@sha256:<hex>`), as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

/// Where a reference a node wrote points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointed {
    /// The node's qid, or the reference as written when no node in this
    /// graph has it.
    pub qid: String,
    pub exists: bool,
    /// The pin's 64 hex digits, kept apart (pins are reserved syntax: a
    /// ref resolves to its node whatever it pins).
    pub pin: Option<String>,
}

/// The one conversion for references a node writes, in `refs` and in body
/// links (format 0.7): a ref to a node of this graph (a bare id, a pinned
/// one, or `mdkg://<this graph>/…`) is that node, found by UUID, a stale
/// type too; a ref into another graph, or to a node this graph lacks,
/// stays as written. `None` for a value that isn't a 0.7 ref.
pub fn point(index: &Index, value: &str) -> Option<Pointed> {
    let pin = NodeRef::parse(value).ok().and_then(|r| r.pin);
    match index.target(value) {
        RefTarget::Local { node, .. } => Some(Pointed {
            qid: node.qid.clone(),
            exists: true,
            pin,
        }),
        RefTarget::Missing | RefTarget::Foreign => Some(Pointed {
            qid: value.to_string(),
            exists: false,
            pin,
        }),
        RefTarget::NotARef => None,
    }
}

/// A node's `refs` values: 0.7 refs as [`point`] reads them, anything
/// else (a 0.6.0 id) qualified with the node's workspace.
pub fn ref_targets<'a>(
    index: &'a Index,
    node: &'a IndexNode,
) -> impl Iterator<Item = Pointed> + 'a {
    node.refs.iter().map(move |r| {
        point(index, r).unwrap_or_else(|| {
            let qid = if r.contains(':') {
                r.clone()
            } else {
                format!("{}:{r}", node.ws)
            };
            Pointed {
                exists: index.contains(&qid),
                qid,
                pin: None,
            }
        })
    })
}

/// Each body link of `node` that is a 0.7 ref, found by the format's own
/// grammar (`mdkg_graph::validate::body_links`) and read by [`point`]. A
/// link target that isn't a ref is no link here. In order, each once.
pub fn links(index: &Index, node: &IndexNode) -> Vec<Pointed> {
    let mut out: Vec<Pointed> = Vec::new();
    for target in body_links(&node.body) {
        if let Some(p) = point(index, target)
            && !out.contains(&p)
        {
            out.push(p);
        }
    }
    out
}

/// The nodes `qid` points at: its edge fields, its `refs`, then the nodes
/// its body links to (kind `link`).
pub fn refs(index: &Index, qid: &str) -> Vec<Edge> {
    let Some(node) = index.get(qid) else {
        return Vec::new();
    };
    let mut out: Vec<Edge> = edge_targets(&node.edges)
        .into_iter()
        .map(|(kind, target)| Edge {
            kind: kind.to_string(),
            qid: target.to_string(),
            exists: index.contains(target),
            pin: None,
        })
        .collect();
    let edge = |kind: &str, p: Pointed| Edge {
        kind: kind.into(),
        qid: p.qid,
        exists: p.exists,
        pin: p.pin,
    };
    out.extend(ref_targets(index, node).map(|p| edge("refs", p)));
    out.extend(links(index, node).into_iter().map(|p| edge("link", p)));
    out
}

/// The nodes that point at `qid`, by edge kind then qid.
pub fn backrefs(index: &Index, qid: &str) -> Vec<Edge> {
    let mut out: Vec<Edge> = Vec::new();
    for (kind, targets) in &index.reverse_edges {
        if let Some(sources) = targets.get(qid) {
            out.extend(sources.iter().map(|s| Edge {
                kind: kind.clone(),
                qid: s.clone(),
                exists: true,
                pin: None,
            }));
        }
    }
    for n in &index.nodes {
        let pointing = ref_targets(index, n)
            .map(|p| ("refs", p))
            .chain(links(index, n).into_iter().map(|p| ("link", p)));
        for (kind, p) in pointing {
            if p.qid == qid {
                out.push(Edge {
                    kind: kind.into(),
                    qid: n.qid.clone(),
                    exists: true,
                    pin: p.pin,
                });
            }
        }
    }
    // The whole edge orders it, so dedup sees every repeat side by side.
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.qid.cmp(&b.qid))
            .then(a.pin.cmp(&b.pin))
    });
    out.dedup();
    out
}

/// What still blocks `node`: its `blocked_by` targets and the nodes whose
/// `blocks` names it, that are not finished and not above it (its parent
/// and epic chain). A local `blocked_by` target no node here has counts as
/// open; one in another graph doesn't block. A node in
/// another graph can't be checked, so it doesn't block.
pub fn open_blockers(index: &Index, node: &IndexNode) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let from_blocks = index.reverse("blocks", &node.qid);
    // A parent, epic or goal above the node never blocks it: the two could
    // only wait on each other (validate warns, edge.blocks_ancestor).
    let mut above: Option<std::collections::HashSet<String>> = None;
    for q in node.edges.blocked_by.iter().chain(from_blocks) {
        // A local prerequisite no node here has (its node is on another
        // branch, a profile C graph's warning) is still open: the dependent
        // waits for it (RC1 audit I3). A ref into another graph can't be
        // checked, so it doesn't block.
        if index.get(q).is_none() {
            let written = q
                .strip_prefix(&format!("{}:", node.ws))
                .filter(|rest| mdkg_format::id::NodeId::parse(rest).is_ok())
                .unwrap_or(q);
            if index.target(written) == RefTarget::Missing
                && !above
                    .get_or_insert_with(|| ancestors(index, node))
                    .contains(q)
            {
                out.insert(q.clone());
            }
            continue;
        }
        if let Some(b) = index.get(q)
            && !is_finished(b.status.as_deref())
            && b.qid != node.qid
            && !above
                .get_or_insert_with(|| ancestors(index, node))
                .contains(&b.qid)
        {
            out.insert(b.qid.clone());
        }
    }
    out.into_iter().collect()
}

/// The ready set: work items of `types` with one of `statuses` (a ready
/// status, in preference order) and nothing open blocking them. Sorted by
/// priority (unset last), then status preference, then qid, as `next`
/// picks.
pub fn ready_set<'a>(
    index: &'a Index,
    types: &[&str],
    statuses: &[String],
    priority_max: i64,
) -> Vec<&'a IndexNode> {
    let rank = |s: Option<&str>| s.and_then(|s| statuses.iter().position(|p| p == s));
    let mut ready: Vec<&IndexNode> = index
        .nodes
        .iter()
        .filter(|n| types.contains(&n.node_type.as_str()))
        .filter(|n| rank(n.status.as_deref()).is_some())
        .filter(|n| open_blockers(index, n).is_empty())
        .collect();
    ready.sort_by(|a, b| {
        let p = |n: &IndexNode| n.priority.unwrap_or(priority_max + 1);
        p(a).cmp(&p(b))
            .then(rank(a.status.as_deref()).cmp(&rank(b.status.as_deref())))
            .then(a.qid.cmp(&b.qid))
    });
    ready
}

/// The critical path to a goal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CriticalPath {
    /// The longest chain of unfinished work items, each blocked by the one
    /// before it: the first can start now (or is blocked from outside the
    /// chain only by finished work), the last finishes the chain.
    pub path: Vec<String>,
    /// Unfinished work items in the goal's scope.
    pub remaining: usize,
    /// Work items found in a blocking cycle, which no order can finish.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cycle: Vec<String>,
}

/// Every node in a goal's scope: its `scope_refs` (as `goal next` reads
/// them) and everything under the goal or under those, through `parent`
/// and `epic`.
pub fn goal_members(index: &Index, goal: &IndexNode) -> BTreeSet<String> {
    let scope = goal_scope(index, goal);
    let mut members: BTreeSet<String> = scope.qids.into_iter().collect();
    let mut queue: VecDeque<String> = members.iter().cloned().collect();
    queue.push_back(goal.qid.clone());
    let mut seen: BTreeSet<String> = queue.iter().cloned().collect();
    while let Some(q) = queue.pop_front() {
        let is_container = q == goal.qid
            || index
                .get(&q)
                .is_some_and(|n| GOAL_SCOPE_CONTAINER_TYPES.contains(&n.node_type.as_str()));
        if !is_container {
            continue;
        }
        for child in index
            .reverse("parent", &q)
            .iter()
            .chain(index.reverse("epic", &q))
        {
            if seen.insert(child.clone()) {
                members.insert(child.clone());
                queue.push_back(child.clone());
            }
        }
    }
    members.remove(&goal.qid);
    members
}

/// The longest chain of unfinished work that the goal waits on: from each
/// unfinished work item in its scope, back through what blocks it (inside
/// the scope or not). Ties go to the smaller qid, so the answer is stable.
pub fn critical_path(index: &Index, goal: &IndexNode) -> CriticalPath {
    let open = |q: &str| {
        index.get(q).is_some_and(|n| {
            GOAL_SCOPE_ACTIONABLE_TYPES.contains(&n.node_type.as_str())
                && !is_finished(n.status.as_deref())
        })
    };
    let starts: Vec<String> = goal_members(index, goal)
        .into_iter()
        .filter(|q| open(q))
        .collect();
    let mut walk = Walk {
        index,
        depth: HashMap::new(),
        next: HashMap::new(),
        stack: Vec::new(),
    };
    for q in &starts {
        walk.depth_of(q);
    }
    let end = starts
        .iter()
        .max_by(|a, b| walk.depth[*a].cmp(&walk.depth[*b]).then(b.cmp(a)))
        .cloned();
    // `next` points from a node to the blocker on its longest chain, so
    // following it walks from the last item back to the first.
    let mut path = Vec::new();
    let mut at = end;
    while let Some(q) = at {
        at = walk.next.get(&q).cloned();
        path.push(q);
    }
    path.reverse();
    CriticalPath {
        path,
        remaining: starts.len(),
        cycle: cycle_members(index, &starts),
    }
}

/// Every node on a blocking cycle reachable from `starts` through open
/// blockers: the members of each strongly connected component of more than
/// one node (Tarjan's algorithm), sorted.
fn cycle_members(index: &Index, starts: &[String]) -> Vec<String> {
    #[derive(Default)]
    struct Tarjan {
        next: usize,
        low: HashMap<String, usize>,
        order: HashMap<String, usize>,
        stack: Vec<String>,
        on_stack: BTreeSet<String>,
        members: BTreeSet<String>,
    }
    impl Tarjan {
        fn visit(&mut self, index: &Index, v: &str) {
            self.order.insert(v.to_string(), self.next);
            self.low.insert(v.to_string(), self.next);
            self.next += 1;
            self.stack.push(v.to_string());
            self.on_stack.insert(v.to_string());
            let blockers = index
                .get(v)
                .map_or_else(Vec::new, |n| open_blockers(index, n));
            for w in blockers {
                if !self.order.contains_key(&w) {
                    self.visit(index, &w);
                    let lw = self.low[&w];
                    let lv = self.low.get_mut(v).expect("visited");
                    *lv = (*lv).min(lw);
                } else if self.on_stack.contains(&w) {
                    let ow = self.order[&w];
                    let lv = self.low.get_mut(v).expect("visited");
                    *lv = (*lv).min(ow);
                }
            }
            if self.low[v] == self.order[v] {
                let mut component = Vec::new();
                while let Some(w) = self.stack.pop() {
                    self.on_stack.remove(&w);
                    let done = w == v;
                    component.push(w);
                    if done {
                        break;
                    }
                }
                if component.len() > 1 {
                    self.members.extend(component);
                }
            }
        }
    }
    let mut t = Tarjan::default();
    for s in starts {
        if !t.order.contains_key(s) {
            t.visit(index, s);
        }
    }
    t.members.into_iter().collect()
}

struct Walk<'a> {
    index: &'a Index,
    /// The longest chain ending at a node, the node included.
    depth: HashMap<String, usize>,
    /// The blocker on that chain.
    next: HashMap<String, String>,
    /// The nodes being walked: a blocker already on it closes a cycle,
    /// which the depth skips ([`cycle_members`] names its members).
    stack: Vec<String>,
}

impl Walk<'_> {
    fn depth_of(&mut self, qid: &str) -> usize {
        if let Some(d) = self.depth.get(qid) {
            return *d;
        }
        self.stack.push(qid.to_string());
        let blockers = match self.index.get(qid) {
            Some(n) => open_blockers(self.index, n),
            None => Vec::new(),
        };
        let mut best: Option<(usize, String)> = None;
        for b in blockers {
            if self.stack.contains(&b) {
                continue;
            }
            let d = self.depth_of(&b);
            if best
                .as_ref()
                .is_none_or(|(bd, bq)| d > *bd || (d == *bd && b < *bq))
            {
                best = Some((d, b));
            }
        }
        self.stack.pop();
        let depth = 1 + best.as_ref().map_or(0, |(d, _)| *d);
        if let Some((_, b)) = best {
            self.next.insert(qid.to_string(), b);
        }
        self.depth.insert(qid.to_string(), depth);
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_graph::IndexNode;

    const A: &str = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    const B: &str = "goal-01930000-0000-7000-8000-000000000000";
    const G: &str = "0190aaaa-0000-7000-8000-000000000000";
    const OTHER: &str = "0191bbbb-0000-7000-8000-000000000000";

    fn node(id: &str, body: &str) -> IndexNode {
        IndexNode {
            id: id.into(),
            legacy_id: None,
            identity: None,
            qid: format!("root:{id}"),
            ws: "root".into(),
            node_type: id.split('-').next().unwrap_or_default().into(),
            title: id.into(),
            status: Some("todo".into()),
            priority: None,
            created: String::new(),
            updated: String::new(),
            tags: vec![],
            owners: vec![],
            links: vec![],
            artifacts: vec![],
            refs: vec![],
            aliases: vec![],
            skills: vec![],
            scope: vec![],
            attributes: Default::default(),
            path: format!("{id}.md"),
            edges: Default::default(),
            body: body.into(),
        }
    }

    #[test]
    fn refs_and_links_resolve_local_refs_and_keep_foreign_ones() {
        let gone = "task-01950000-0000-7000-8000-000000000000";
        let pin = "a".repeat(64);
        let body = format!(
            "See [the task]({A}) and [goal]({} \"title\"), [again]({A}), [gone]({gone}).\n\
             Not `[code]({B})`, not [web](https://example.com), not [x](task-19).\n\
             ```\n[fenced]({B})\n```\n\
             [here](mdkg://{G}/{B}) [elsewhere](mdkg://{OTHER}/{B}) [pinned]({B}@sha256:{pin})\n",
            B.replacen("goal", "epic", 1)
        );
        let mut from = node(A, &body);
        from.refs = vec![
            format!("{B}@sha256:{pin}"),
            format!("mdkg://{G}/{B}"),
            format!("mdkg://{OTHER}/{B}"),
            gone.to_string(),
        ];
        let graph = Index::assemble_in(
            vec![from, node(B, "")],
            vec!["root".into()],
            mdkg_format::id::Uuid7::parse(G).ok(),
        );
        let from = graph.get(&format!("root:{A}")).expect("node");
        let p = |qid: &str, exists: bool, pin: Option<&str>| Pointed {
            qid: qid.to_string(),
            exists,
            pin: pin.map(String::from),
        };
        let b = format!("root:{B}");
        let foreign = format!("mdkg://{OTHER}/{B}");
        assert_eq!(
            links(&graph, from),
            [
                p(&format!("root:{A}"), true, None),
                p(&b, true, None),
                p(gone, false, None),
                p(&foreign, false, None),
                p(&b, true, Some(&pin)),
            ]
        );
        assert_eq!(
            ref_targets(&graph, from).collect::<Vec<_>>(),
            [
                p(&b, true, Some(&pin)),
                p(&b, true, None),
                p(&foreign, false, None),
                p(gone, false, None),
            ]
        );
        // Backrefs find the pinned and graph-qualified refs.
        let back = backrefs(&graph, &b);
        // Pins P, Q and P again (bare and graph-qualified): two backrefs.
        let q = "c".repeat(64);
        let mut twice = node(A, "");
        twice.refs = vec![
            format!("{B}@sha256:{pin}"),
            format!("{B}@sha256:{q}"),
            format!("mdkg://{G}/{B}@sha256:{pin}"),
        ];
        let g2 = Index::assemble_in(
            vec![twice, node(B, "")],
            vec!["root".into()],
            mdkg_format::id::Uuid7::parse(G).ok(),
        );
        assert_eq!(backrefs(&g2, &b).len(), 2, "{:?}", backrefs(&g2, &b));
        assert!(
            back.iter()
                .any(|e| e.kind == "refs" && e.pin.as_deref() == Some(pin.as_str()))
        );
        assert!(
            back.iter().filter(|e| e.kind == "refs").count() >= 2,
            "{back:?}"
        );
    }
}
