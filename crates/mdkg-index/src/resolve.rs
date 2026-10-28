//! Handles: every command that takes a node accepts a
//! full id, a unique id prefix (as with git's short SHAs) or a title
//! fragment. An ambiguous handle lists its matches.
//!
//! [`Handles`] is mdkg-format's [`HandleIndex`] over the graph as read, so
//! resolution follows lane 1's [`handle::resolve`] order and never depends
//! on the derived file being current. `HandleIndex` speaks 0.7 ids only;
//! on a graph not migrated yet, 0.6.0 ids resolve exactly, as before, and
//! title fragments through the same rules.

use mdkg_format::handle::{self, Candidate, HandleIndex, Via};
use mdkg_format::id::{IdPrefix, NodeId, NodeRef, Uuid7};
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::index::RefTarget;
use mdkg_graph::resolve::{Resolution, format_resolve_error, resolve_qid};
use mdkg_graph::{GraphError, Index, IndexNode};

/// The most matches an ambiguity error lists.
pub const MAX_LISTED: usize = 5;

/// A migrated node's 0.6.0 id.
fn legacy_id(node: &IndexNode) -> Option<&str> {
    node.legacy_id.as_deref()
}

/// The graph as read, as a [`HandleIndex`]: its nodes with 0.7 ids, in one
/// workspace or all.
#[derive(Debug)]
pub struct Handles<'a> {
    index: &'a Index,
    /// Each 0.7 node's parsed id, with its place in `index.nodes`.
    ids: Vec<(NodeId, usize)>,
}

impl<'a> Handles<'a> {
    pub fn new(index: &'a Index, ws: Option<&str>) -> Self {
        let ids = index
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| ws.is_none_or(|w| n.ws == w))
            .filter_map(|(i, n)| NodeId::parse(&n.id).ok().map(|id| (id, i)))
            .collect();
        Handles { index, ids }
    }

    fn matching(&self, keep: impl Fn(&NodeId, &IndexNode) -> bool) -> Vec<Candidate> {
        self.ids
            .iter()
            .filter(|(id, i)| keep(id, &self.index.nodes[*i]))
            .map(|(id, i)| Candidate {
                id: id.clone(),
                title: self.index.nodes[*i].title.clone(),
            })
            .collect()
    }

    /// The qid of a node this index handed out.
    fn qid(&self, id: &NodeId) -> Option<String> {
        self.ids
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, n)| self.index.nodes[*n].qid.clone())
    }
}

impl HandleIndex for Handles<'_> {
    fn by_uuid(&self, uuid: Uuid7) -> Option<Candidate> {
        self.matching(|id, _| id.uuid() == uuid).into_iter().next()
    }

    /// Exact and unqualified only: on a 0.7 graph `root:<legacy_id>` names
    /// nothing (format 0.7, section 6).
    fn by_legacy_id(&self, legacy: &str) -> Vec<Candidate> {
        self.matching(|_, n| legacy_id(n).is_some_and(|l| l.to_lowercase() == legacy))
    }

    fn by_prefix(&self, prefix: &IdPrefix) -> Vec<Candidate> {
        self.matching(|id, _| prefix.matches(id))
    }

    fn by_title(&self, fragment: &str) -> Vec<Candidate> {
        let words = title_words(fragment);
        self.matching(|_, n| title_has(&n.title, &words))
    }
}

fn title_words(fragment: &str) -> Vec<String> {
    fragment
        .split(mdkg_format::js::is_js_whitespace)
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn title_has(title: &str, words: &[String]) -> bool {
    let title = title.to_lowercase();
    !words.is_empty() && words.iter().all(|w| title.contains(w.as_str()))
}

/// Resolves a handle to one node's qid:
/// 1. an exact 0.6.0 id, qid or identity ref, as 0.6.0 resolves them;
/// 2. lane 1's order over 0.7 ids: a full id or ref, a `legacy_id`, a
///    unique UUID prefix (4+ hex digits, type optional);
/// 3. a title fragment, over every node.
///
/// A title fragment matches titles holding every word, ignoring case; one
/// title equal to the whole handle wins over the rest. `ws` limits every
/// step to one workspace.
pub fn resolve_handle(index: &Index, handle: &str, ws: Option<&str>) -> Resolution {
    let handle = mdkg_format::js::trim(handle);
    if handle.is_empty() {
        return Resolution::Missing(Vec::new());
    }
    match resolve_qid(index, handle, ws) {
        Resolution::Missing(candidates) if candidates.is_empty() => {}
        found => return found,
    }
    let handles = Handles::new(index, ws);
    let to_qids = |matches: &[Candidate]| -> Vec<String> {
        let mut qids: Vec<String> = matches.iter().filter_map(|c| handles.qid(&c.id)).collect();
        qids.sort();
        qids
    };
    match handle::resolve(handle, index.graph_id, &handles) {
        handle::Resolution::Found { candidate, via, .. } if via != Via::Title => {
            if let Some(qid) = handles.qid(&candidate.id) {
                return Resolution::Ok(qid);
            }
        }
        handle::Resolution::Ambiguous { matches, via } if via != Via::Title => {
            return Resolution::Ambiguous(to_qids(&matches));
        }
        handle::Resolution::OtherGraph(_) => return Resolution::Missing(Vec::new()),
        // A full id or ref that names no node is final (format 0.7,
        // section 6): never a title.
        handle::Resolution::NotFound if NodeRef::parse(handle).is_ok() => {
            return Resolution::Missing(Vec::new());
        }
        // Titles are matched below, once, across 0.7 nodes and nodes still
        // on 0.6.0 ids (which HandleIndex can't name), by the same rules.
        _ => {}
    }
    let words = title_words(handle);
    let lower = handle.to_lowercase();
    let titled: Vec<&IndexNode> = index
        .nodes
        .iter()
        .filter(|n| ws.is_none_or(|w| n.ws == w))
        .filter(|n| title_has(&n.title, &words))
        .collect();
    let exact: Vec<&IndexNode> = titled
        .iter()
        .copied()
        .filter(|n| n.title.to_lowercase() == lower)
        .collect();
    let mut chosen = if exact.len() == 1 { exact } else { titled };
    match chosen.len() {
        0 => Resolution::Missing(Vec::new()),
        1 => Resolution::Ok(chosen[0].qid.clone()),
        _ => {
            chosen.sort_by(|a, b| a.qid.cmp(&b.qid));
            Resolution::Ambiguous(chosen.into_iter().map(|n| n.qid.clone()).collect())
        }
    }
}

/// When a handle resolved as a legacy id (a migrated node's `legacy_id`,
/// or a 0.6.0 id) that is also a prefix of 0.7 ids, a warning naming
/// both: the legacy id wins, and the prefix's nodes need their full id or
/// a longer prefix. A numbered legacy task id can also be a type-qualified
/// UUID prefix for tasks minted in the corresponding stretch of time.
pub fn shadowed(
    index: &Index,
    handle: &str,
    ws: Option<&str>,
    resolved: &Resolution,
) -> Option<Diagnostic> {
    let Resolution::Ok(qid) = resolved else {
        return None;
    };
    let node = index.get(qid)?;
    let lower = mdkg_format::js::trim(handle).to_lowercase();
    let by_legacy = legacy_id(node).is_some_and(|l| l.to_lowercase() == lower)
        || (NodeId::parse(&node.id).is_err() && (node.id == lower || node.qid == lower));
    if !by_legacy {
        return None;
    }
    let prefix = IdPrefix::parse(&lower)?;
    let handles = Handles::new(index, ws);
    let mut others: Vec<String> = handles
        .by_prefix(&prefix)
        .iter()
        .filter_map(|c| handles.qid(&c.id))
        .filter(|q| q != qid)
        .collect();
    if others.is_empty() {
        return None;
    }
    others.sort();
    Some(
        Diagnostic::warning(
            Code::NodeAmbiguous,
            format!(
                "the handle is {}'s legacy id, which wins; it is also a prefix of {} other node id(s)",
                display_id(index, qid),
                others.len()
            ),
        )
        .with_hint(format!(
            "for those, give the full id or a longer prefix: {}",
            describe(index, &others).join("; ")
        )),
    )
}

/// Resolves a handle as [`resolve_handle`] does, with what to tell the
/// actor: a `node.resolved` note naming the node (full id and title) for
/// any handle that isn't the node's full id or qid, and [`shadowed`]'s
/// warning when a legacy id also spells another node's prefix.
pub fn resolve_noted(
    index: &Index,
    handle: &str,
    ws: Option<&str>,
) -> (Resolution, Vec<Diagnostic>) {
    let resolved = resolve_handle(index, handle, ws);
    let mut notes: Vec<Diagnostic> = shadowed(index, handle, ws, &resolved).into_iter().collect();
    if let Resolution::Ok(qid) = &resolved
        && let Some(node) = index.get(qid)
        && !is_full_id(index, node, handle)
    {
        notes.push(Diagnostic::new(
            Code::NodeResolved,
            mdkg_format::Severity::Info,
            format!(
                "the handle names {}: {}",
                display_id(index, qid),
                node.title
            ),
        ));
    }
    (resolved, notes)
}

/// Whether a handle is `node`'s full id or qid, as written or as a ref
/// to it under its current type.
fn is_full_id(index: &Index, node: &IndexNode, handle: &str) -> bool {
    let lower = mdkg_format::js::trim(handle).to_lowercase();
    if lower == node.id || lower == node.qid {
        return true;
    }
    // A stale type still finds the node; it gets the note, which names
    // the node's current id.
    matches!(index.target(&lower), RefTarget::Local { node: t, stale: false } if t.qid == node.qid)
}

/// A migrated graph's ids share their time prefix, so a short prefix
/// rarely picks one node: a legacy id among the matches (else any in the
/// graph) to suggest instead, so the hint offers a usable handle.
fn legacy_example(index: &Index, qids: &[String]) -> Option<String> {
    qids.iter()
        .filter_map(|q| index.get(q))
        .chain(index.nodes.iter())
        .find_map(|n| n.legacy_id.clone())
}

/// How a node is named to a reader: its bare id in a format 0.7 graph,
/// where every id is unique (lane 1 #138), else its qid.
pub fn display_id(index: &Index, qid: &str) -> String {
    match index.get(qid) {
        Some(n) if index.graph_id.is_some() => n.id.clone(),
        _ => qid.to_string(),
    }
}

/// The matches of an ambiguous handle as `id  title` lines (`id (legacy
/// id)` for a migrated node), at most [`MAX_LISTED`], with a count of the
/// rest.
pub fn describe(index: &Index, qids: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = qids
        .iter()
        .take(MAX_LISTED)
        .map(|q| match index.get(q) {
            Some(n) => match &n.legacy_id {
                Some(legacy) => format!("{} ({legacy})  {}", display_id(index, q), n.title),
                None => format!("{}  {}", display_id(index, q), n.title),
            },
            None => q.clone(),
        })
        .collect();
    if qids.len() > MAX_LISTED {
        lines.push(format!("… and {} more", qids.len() - MAX_LISTED));
    }
    lines
}

/// An unresolved handle as an error: `node.not_found`, or `node.ambiguous`
/// (exit 7) listing the matches with their titles in the hint.
pub fn handle_error(
    index: &Index,
    label: &str,
    handle: &str,
    result: &Resolution,
    ws: Option<&str>,
) -> GraphError {
    match result {
        // The count in the message, the matches (with titles) only in the
        // hint: nothing typed is quoted (the orchestrator's E2/E4 ruling).
        Resolution::Ambiguous(qids) => GraphError::ambiguous(format_resolve_error(
            label, handle, result, ws,
        ))
        .with_hint(format!(
            "matches: {}; give more of the id, or the full id{}",
            describe(index, qids).join("; "),
            legacy_example(index, qids)
                .map(|l| format!("; a legacy id like {l} works too"))
                .unwrap_or_default()
        )),
        other => GraphError::not_found(
            Code::NodeNotFound,
            format_resolve_error(label, handle, other, ws),
        )
        .with_hint(mdkg_graph::resolve::resolve_hint(other)),
    }
}
