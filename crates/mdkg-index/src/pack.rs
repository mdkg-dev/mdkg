//! Markdown context packs: the nodes most relevant to a seed, ranked, in a
//! token budget, using graph-based context ranking.
//!
//! Ranking is personalized PageRank over the graph's edges, restarting at
//! the seeds: the node asked for, or a query's search hits weighted by
//! their BM25 scores. Nodes go into the pack best first while they fit.

use std::collections::HashMap;

use mdkg_graph::index::edge_targets;
use mdkg_graph::{Index, IndexNode};

use crate::resolve::display_id;
use schemars::JsonSchema;
use serde::Serialize;

/// The chance a step of the walk jumps back to a seed.
const RESTART: f64 = 0.15;
const ITERATIONS: usize = 40;
const TOLERANCE: f64 = 1e-9;

/// How strongly each edge kind ties two nodes, walked both ways.
fn edge_weight(kind: &str) -> f64 {
    match kind {
        "parent" | "epic" => 1.0,
        "context_refs" => 0.9,
        "blocked_by" | "blocks" => 0.8,
        "refs" | "link" => 0.7,
        "relates" => 0.6,
        "evidence_refs" | "prev" | "next" => 0.5,
        _ => 0.3,
    }
}

/// Personalized PageRank from `seeds` (qid, weight). The seeds come first,
/// heaviest first (a hub next to them can score more, but they're what was
/// asked for); then every other node the walk reaches, best first. Ties go
/// by qid. Seeds not in the graph are ignored.
pub fn rank(index: &Index, seeds: &[(String, f64)]) -> Vec<(String, f64)> {
    let n = index.nodes.len();
    let pos: HashMap<&str, usize> = index
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.qid.as_str(), i))
        .collect();
    let mut restart = vec![0.0; n];
    for (q, w) in seeds {
        if let Some(&i) = pos.get(q.as_str())
            && *w > 0.0
        {
            restart[i] += w;
        }
    }
    let total: f64 = restart.iter().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    restart.iter_mut().for_each(|r| *r /= total);
    // Undirected, weighted adjacency.
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (i, node) in index.nodes.iter().enumerate() {
        let targets = edge_targets(&node.edges)
            .into_iter()
            .map(|(k, t)| (k, t.to_string()))
            .chain(crate::graph::ref_targets(index, node).map(|p| ("refs", p.qid)))
            .chain(
                crate::graph::links(index, node)
                    .into_iter()
                    .map(|p| ("link", p.qid)),
            );
        for (kind, target) in targets {
            if let Some(&j) = pos.get(target.as_str())
                && j != i
            {
                let w = edge_weight(kind);
                adj[i].push((j, w));
                adj[j].push((i, w));
            }
        }
    }
    let out_weight: Vec<f64> = adj.iter().map(|a| a.iter().map(|(_, w)| w).sum()).collect();
    let mut score = restart.clone();
    for _ in 0..ITERATIONS {
        let mut next = vec![0.0; n];
        let mut dangling = 0.0;
        for i in 0..n {
            if out_weight[i] == 0.0 {
                dangling += score[i];
                continue;
            }
            for &(j, w) in &adj[i] {
                next[j] += (1.0 - RESTART) * score[i] * w / out_weight[i];
            }
        }
        for i in 0..n {
            next[i] += (RESTART + (1.0 - RESTART) * dangling) * restart[i];
        }
        let delta: f64 = next.iter().zip(&score).map(|(a, b)| (a - b).abs()).sum();
        score = next;
        if delta < TOLERANCE {
            break;
        }
    }
    let mut ranked: Vec<(usize, f64)> = score
        .into_iter()
        .enumerate()
        .filter(|(_, s)| *s > 0.0)
        .collect();
    ranked.sort_by(|&(i, a), &(j, b)| {
        restart[j]
            .total_cmp(&restart[i])
            .then(b.total_cmp(&a))
            .then(index.nodes[i].qid.cmp(&index.nodes[j].qid))
    });
    ranked
        .into_iter()
        .map(|(i, s)| (index.nodes[i].qid.clone(), s))
        .collect()
}

/// How much of each body a pack keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Full bodies.
    Standard,
    /// The seed's full body; for the rest, the opening of the body, code
    /// blocks left out.
    Concise,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Profile::Standard),
            "concise" => Some(Profile::Concise),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Standard => "standard",
            Profile::Concise => "concise",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub profile: Profile,
    /// The most tokens the pack may hold (estimated, see
    /// [`estimate_tokens`]).
    pub budget: usize,
    pub max_nodes: usize,
}

/// One node in a pack.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Included {
    pub qid: String,
    pub score: f64,
    pub tokens: usize,
    /// Its body was cut to fit the budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Pack {
    pub profile: Profile,
    pub budget: usize,
    pub tokens: usize,
    pub nodes: Vec<Included>,
    /// Ranked nodes left out because the budget or the node cap ran out.
    pub omitted: usize,
    #[serde(skip)]
    pub markdown: String,
}

/// Tokens a text takes, estimated as one per four bytes, rounded up.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// The opening of a body: code blocks left out, up to the second heading
/// or twelve lines of text.
pub fn summary(body: &str) -> String {
    const MAX_LINES: usize = 12;
    let mut out: Vec<&str> = Vec::new();
    let mut in_code = false;
    let mut headings = 0;
    let mut text_lines = 0;
    let mut cut = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if trimmed.starts_with('#') {
            headings += 1;
            if headings > 1 && text_lines > 0 {
                cut = true;
                break;
            }
        } else if !trimmed.is_empty() {
            if text_lines == MAX_LINES {
                cut = true;
                break;
            }
            text_lines += 1;
        }
        out.push(line);
    }
    let mut s = out.join("\n").trim().to_string();
    if cut {
        s.push_str("\n…");
    }
    s
}

fn list_line(lines: &mut Vec<String>, key: &str, values: &[String]) {
    if !values.is_empty() {
        lines.push(format!("{key}: {}", values.join(", ")));
    }
}

/// A node's section: its card and fields, then the body as given. Ids
/// are shown as the graph's other output shows them: bare in format 0.7.
fn section(index: &Index, node: &IndexNode, body: &str) -> String {
    let id = display_id(index, &node.qid);
    // Show a migrated node's 0.6.0 id after its current id for recognition.
    let heading = match &node.legacy_id {
        Some(legacy) => format!("## {id} ({legacy})"),
        None => format!("## {id}"),
    };
    let mut lines = vec![
        heading,
        format!("type: {}", node.node_type),
        format!("title: {}", node.title),
    ];
    if let Some(s) = &node.status {
        lines.push(format!("status: {s}"));
    }
    if let Some(p) = node.priority {
        lines.push(format!("priority: {p}"));
    }
    lines.push(format!("path: {}", node.path));
    list_line(&mut lines, "tags", &node.tags);
    list_line(&mut lines, "owners", &node.owners);
    for (kind, target) in edge_targets(&node.edges) {
        lines.push(format!("{kind}: {}", display_id(index, target)));
    }
    list_line(&mut lines, "refs", &node.refs);
    let body = body.trim();
    if !body.is_empty() {
        lines.push(String::new());
        lines.push(body.to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Cuts text to at most `bytes` bytes, on a line or char boundary, with a
/// marker when there's room for one.
fn cut_to(text: &str, bytes: usize) -> String {
    const MARK: &str = "\n…(cut to fit the budget)";
    if text.len() <= bytes {
        return text.to_string();
    }
    let floor = |at: usize| {
        let mut end = at.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        end
    };
    if bytes < MARK.len() {
        return text[..floor(bytes)].to_string();
    }
    let head = &text[..floor(bytes - MARK.len())];
    let head = head.rfind('\n').map_or(head, |i| &head[..i]);
    format!("{head}{MARK}")
}

/// The smallest budget a pack takes: room for its header and the start of
/// the seed.
pub const MIN_BUDGET: usize = 64;

/// What comes before each section.
const RULE: &str = "\n---\n\n";

fn header(index: &Index, opts: &Options, qids: &[&str], omitted: usize) -> String {
    let mut lines = vec![
        "# mdkg pack".to_string(),
        format!("profile: {}", opts.profile.as_str()),
        format!("budget: {} tokens", opts.budget),
        format!("nodes: {}", qids.len()),
        format!("omitted: {omitted} lower-ranked nodes"),
        String::new(),
    ];
    lines.extend(qids.iter().map(|q| format!("- {}", display_id(index, q))));
    lines.push(String::new());
    lines.join("\n")
}

/// Builds a pack from ranked nodes; the first is the seed. The pack's
/// estimated tokens never exceed the budget (at least [`MIN_BUDGET`]):
/// nodes go in best first while they fit, and the seed is cut to what's
/// left. Only a seed whose qid can't fit in the header is left out.
pub fn build(index: &Index, ranked: &[(String, f64)], opts: Options) -> Pack {
    let limit = opts.budget.max(MIN_BUDGET) * 4;
    let mut qids: Vec<&str> = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    let mut nodes: Vec<Included> = Vec::new();
    let mut omitted = 0;
    // Bytes so far, with the header sized for the most omitted it can say.
    let used = |qids: &[&str], sections: &[String], extra: usize| {
        header(index, &opts, qids, ranked.len()).len()
            + sections.iter().map(|s| RULE.len() + s.len()).sum::<usize>()
            + extra
    };
    for (i, (qid, score)) in ranked.iter().enumerate() {
        let Some(node) = index.get(qid) else { continue };
        if nodes.len() == opts.max_nodes {
            omitted += 1;
            continue;
        }
        let body = match opts.profile {
            Profile::Concise if i > 0 => summary(&node.body),
            _ => node.body.clone(),
        };
        let mut with: Vec<&str> = qids.clone();
        with.push(qid);
        let mut text = section(index, node, &body);
        let mut cut = false;
        if used(&with, &sections, RULE.len() + text.len()) > limit {
            // A node beyond the first, or a first whose qid alone won't
            // fit in the header, is left out.
            if !nodes.is_empty() || used(&with, &sections, RULE.len()) > limit {
                omitted += 1;
                continue;
            }
            // The seed always goes in, cut to what's left: its body, or
            // even its fields.
            let room = limit.saturating_sub(used(&with, &sections, RULE.len()));
            // A body adds a blank line and its own line to the fields.
            let bare = section(index, node, "");
            let with_body = section(
                index,
                node,
                &cut_to(&body, room.saturating_sub(bare.len() + 2)),
            );
            text = if with_body.len() <= room {
                with_body
            } else {
                cut_to(&bare, room)
            };
            cut = true;
        }
        qids.push(qid);
        nodes.push(Included {
            qid: qid.clone(),
            score: (score * 1e6).round() / 1e6,
            tokens: estimate_tokens(&text),
            cut,
        });
        sections.push(text);
    }
    let mut markdown = header(index, &opts, &qids, omitted);
    for s in &sections {
        markdown.push_str(RULE);
        markdown.push_str(s);
    }
    Pack {
        profile: opts.profile,
        budget: opts.budget,
        tokens: estimate_tokens(&markdown),
        nodes,
        omitted,
        markdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_skip_code_and_stop_at_the_second_heading() {
        let body = "# Overview\n\nFirst line.\n```\ncode\n```\nSecond line.\n\n# Details\n\nMore.";
        assert_eq!(summary(body), "# Overview\n\nFirst line.\nSecond line.\n…");
        assert_eq!(summary("Just text."), "Just text.");
    }

    #[test]
    fn cutting_keeps_whole_lines_and_chars() {
        let text = "ééé\nline two\nline three";
        let cut = cut_to(text, 40);
        assert!(cut.starts_with("ééé\n"));
        assert!(cut.len() <= 40, "{cut:?}");
        assert_eq!(cut_to("short", 100), "short");
        for bytes in 0..text.len() {
            assert!(cut_to(text, bytes).len() <= bytes, "{bytes}");
        }
    }
}
