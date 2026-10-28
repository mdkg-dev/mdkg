//! Filters and search over the index, as `mdkg list` and `mdkg search` do.

use mdkg_format::Value as FmValue;

use crate::index::{Index, IndexNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagsMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub ws: Option<String>,
    /// Nodes of any of these types (empty means every type).
    pub node_types: Vec<String>,
    /// Nodes with any of these statuses (empty means every status).
    pub statuses: Vec<String>,
    pub epic: Option<String>,
    pub priority: Option<i64>,
    pub blocked: bool,
    pub tags: Vec<String>,
    pub tags_mode: TagsMode,
}

fn matches_tags(node_tags: &[String], filter: &[String], mode: TagsMode) -> bool {
    if filter.is_empty() {
        return true;
    }
    if node_tags.is_empty() {
        return false;
    }
    let set: std::collections::HashSet<String> =
        node_tags.iter().map(|t| t.to_lowercase()).collect();
    match mode {
        TagsMode::All => filter.iter().all(|t| set.contains(t)),
        TagsMode::Any => filter.iter().any(|t| set.contains(t)),
    }
}

/// `filterNodes`.
pub fn filter<'a>(nodes: impl Iterator<Item = &'a IndexNode>, f: &Filters) -> Vec<&'a IndexNode> {
    let tags: Vec<String> = f
        .tags
        .iter()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    nodes
        .filter(|n| f.ws.as_ref().is_none_or(|ws| n.ws == *ws))
        .filter(|n| f.node_types.is_empty() || f.node_types.contains(&n.node_type))
        .filter(|n| {
            f.statuses.is_empty() || n.status.as_ref().is_some_and(|s| f.statuses.contains(s))
        })
        .filter(|n| {
            f.epic
                .as_ref()
                .is_none_or(|e| n.edges.epic.as_ref() == Some(e))
        })
        .filter(|n| f.priority.is_none_or(|p| n.priority == Some(p)))
        .filter(|n| !f.blocked || n.status.as_deref() == Some("blocked"))
        .filter(|n| matches_tags(&n.tags, &tags, f.tags_mode))
        .collect()
}

/// Sorts by qid in byte order (the versioned output contract
/// drops 0.6.0's `localeCompare` order).
pub fn sort_by_qid(nodes: &mut [&IndexNode]) {
    nodes.sort_by(|a, b| a.qid.cmp(&b.qid));
}

/// The metadata text `mdkg search` matches against (bodies are excluded).
fn search_text(node: &IndexNode) -> String {
    let mut tokens: Vec<&str> = vec![
        &node.id,
        &node.qid,
        &node.node_type,
        &node.title,
        &node.path,
    ];
    for list in [
        &node.tags,
        &node.owners,
        &node.links,
        &node.artifacts,
        &node.refs,
        &node.aliases,
        &node.skills,
    ] {
        tokens.extend(list.iter().map(String::as_str));
    }
    let mut owned: Vec<String> = Vec::new();
    for (key, value) in node.attributes.entries() {
        owned.push(key.clone());
        match value {
            FmValue::List(items) => owned.extend(items.iter().cloned()),
            FmValue::String(s) => owned.push(s.clone()),
            FmValue::Bool(b) => owned.push(b.to_string()),
        }
    }
    tokens.extend(owned.iter().map(String::as_str));
    tokens.join(" ").to_lowercase()
}

/// Nodes matching every whitespace-separated term, case-insensitively.
pub fn search<'a>(index: &'a Index, query: &str, f: &Filters) -> Vec<&'a IndexNode> {
    let terms: Vec<String> = query
        .to_lowercase()
        .split(mdkg_format::js::is_js_whitespace)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    let mut hits: Vec<&IndexNode> = filter(index.nodes.iter(), f)
        .into_iter()
        .filter(|n| {
            let text = search_text(n);
            terms.iter().all(|t| text.contains(t.as_str()))
        })
        .collect();
    sort_by_qid(&mut hits);
    hits
}
