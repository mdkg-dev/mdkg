//! Handles: what a person or agent types to name a node.
//!
//! Every command that takes a node accepts a full id or ref, a 0.6.0 id on a
//! graph not yet migrated (or a migrated node's `legacy_id`), a unique UUID
//! prefix, or a title fragment. [`resolve`] tries them in that order and
//! stops at the first kind that matches anything:
//!
//! 1. A full id or [`NodeRef`], looked up by UUID. A stale type still
//!    resolves, and says so.
//! 2. An exact 0.6.0 id (`<type>-<number>`, optionally workspace-qualified).
//! 3. A unique [`IdPrefix`].
//! 4. A title fragment: every word appears in the title, ignoring case.
//!    One exact title among several matches wins.
//!
//! An ambiguous handle returns every match, so the caller can list them.
//! The lookups sit behind [`HandleIndex`]: lane 4 backs it with the derived
//! index, and [`MemoryIndex`] backs it with a list.

use crate::id::{IdPrefix, NodeId, NodeRef, Uuid7};
use crate::ids;

/// A node a handle may name, with what to show for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: NodeId,
    pub title: String,
}

/// The lookups [`resolve`] needs.
pub trait HandleIndex {
    /// The node with this UUID, under its current id.
    fn by_uuid(&self, uuid: Uuid7) -> Option<Candidate>;
    /// Nodes whose 0.6.0 id (unmigrated) or `legacy_id` is exactly this,
    /// lowercase and possibly workspace-qualified.
    fn by_legacy_id(&self, legacy: &str) -> Vec<Candidate>;
    /// Nodes the prefix matches.
    fn by_prefix(&self, prefix: &IdPrefix) -> Vec<Candidate>;
    /// Nodes whose title contains every word of the fragment, ignoring
    /// case.
    fn by_title(&self, fragment: &str) -> Vec<Candidate>;
}

/// Which kind of handle matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Id,
    /// A full id whose type is no longer the node's.
    StaleType,
    LegacyId,
    Prefix,
    Title,
}

/// What a handle names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Found {
        candidate: Candidate,
        via: Via,
        /// The pin, when the handle was a pinned ref.
        pin: Option<String>,
    },
    /// Every match of the first kind that matched, in id order.
    Ambiguous {
        via: Via,
        matches: Vec<Candidate>,
    },
    /// A ref into another graph, pin and all.
    OtherGraph(NodeRef),
    NotFound,
}

/// Resolves a handle in the graph `graph_id` (`None` when the graph has no
/// `graph_id` yet, in which case no `mdkg://` ref is local).
pub fn resolve(handle: &str, graph_id: Option<Uuid7>, index: &impl HandleIndex) -> Resolution {
    let handle = crate::js::trim(handle);
    if handle.is_empty() {
        return Resolution::NotFound;
    }
    if let Ok(r) = NodeRef::parse(handle) {
        if r.graph.is_some_and(|graph| Some(graph) != graph_id) {
            return Resolution::OtherGraph(r);
        }
        return match index.by_uuid(r.id.uuid()) {
            Some(candidate) => Resolution::Found {
                via: if candidate.id == r.id {
                    Via::Id
                } else {
                    Via::StaleType
                },
                candidate,
                pin: r.pin,
            },
            None => Resolution::NotFound,
        };
    }
    let lower = handle.to_lowercase();
    if ids::is_portable_id_ref(&lower)
        && let Some(found) = pick(index.by_legacy_id(&lower), Via::LegacyId)
    {
        return found;
    }
    if let Some(prefix) = IdPrefix::parse(handle)
        && let Some(found) = pick(index.by_prefix(&prefix), Via::Prefix)
    {
        return found;
    }
    let mut matches = index.by_title(handle);
    let folded = handle.to_lowercase();
    let exact: Vec<&Candidate> = matches
        .iter()
        .filter(|c| c.title.to_lowercase() == folded)
        .collect();
    if exact.len() == 1 && matches.len() > 1 {
        let candidate = exact[0].clone();
        matches = vec![candidate];
    }
    pick(matches, Via::Title).unwrap_or(Resolution::NotFound)
}

fn pick(mut matches: Vec<Candidate>, via: Via) -> Option<Resolution> {
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    matches.dedup_by(|a, b| a.id == b.id);
    match matches.len() {
        0 => None,
        1 => matches.pop().map(|candidate| Resolution::Found {
            candidate,
            via,
            pin: None,
        }),
        _ => Some(Resolution::Ambiguous { via, matches }),
    }
}

/// One node in a [`MemoryIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: NodeId,
    pub title: String,
    /// The 0.6.0 id: `legacy_id` on a migrated node.
    pub legacy_id: Option<String>,
}

/// A [`HandleIndex`] over a list, for tests and small graphs.
#[derive(Debug, Clone, Default)]
pub struct MemoryIndex {
    pub entries: Vec<Entry>,
}

impl MemoryIndex {
    fn candidates<'a>(&'a self, keep: impl Fn(&Entry) -> bool + 'a) -> Vec<Candidate> {
        self.entries
            .iter()
            .filter(|e| keep(e))
            .map(|e| Candidate {
                id: e.id.clone(),
                title: e.title.clone(),
            })
            .collect()
    }
}

impl HandleIndex for MemoryIndex {
    fn by_uuid(&self, uuid: Uuid7) -> Option<Candidate> {
        self.candidates(|e| e.id.uuid() == uuid).into_iter().next()
    }

    fn by_legacy_id(&self, legacy: &str) -> Vec<Candidate> {
        self.candidates(|e| e.legacy_id.as_deref() == Some(legacy))
    }

    fn by_prefix(&self, prefix: &IdPrefix) -> Vec<Candidate> {
        self.candidates(|e| prefix.matches(&e.id))
    }

    fn by_title(&self, fragment: &str) -> Vec<Candidate> {
        let words: Vec<String> = fragment.split_whitespace().map(str::to_lowercase).collect();
        self.candidates(move |e| {
            let title = e.title.to_lowercase();
            !words.is_empty() && words.iter().all(|w| title.contains(w.as_str()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, legacy: Option<&str>) -> Entry {
        Entry {
            id: NodeId::parse(id).unwrap(),
            title: title.to_string(),
            legacy_id: legacy.map(str::to_string),
        }
    }

    const A: &str = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    const B: &str = "task-0192f3a4-7b2d-7def-8a9b-1c2d3e4f5a6b";
    const C: &str = "goal-0193aaaa-0000-7000-8000-000000000000";
    const GRAPH: &str = "0190aaaa-0000-7000-8000-000000000000";

    fn index() -> MemoryIndex {
        MemoryIndex {
            entries: vec![
                entry(A, "Fix the parser", Some("task-19")),
                entry(B, "Fix the parser tests", None),
                entry(C, "Ship 0192 release", Some("root:goal-4")),
            ],
        }
    }

    fn found(r: &Resolution) -> (String, Via) {
        match r {
            Resolution::Found { candidate, via, .. } => (candidate.id.to_string(), *via),
            other => panic!("not found: {other:?}"),
        }
    }

    #[test]
    fn each_kind_resolves() {
        let ix = index();
        let graph = Uuid7::parse(GRAPH).ok();
        assert_eq!(found(&resolve(A, graph, &ix)), (A.to_string(), Via::Id));
        assert_eq!(
            found(&resolve(&format!("mdkg://{GRAPH}/{A}"), graph, &ix)).1,
            Via::Id
        );
        let stale = A.replacen("task", "bug", 1);
        assert_eq!(
            found(&resolve(&stale, graph, &ix)),
            (A.to_string(), Via::StaleType)
        );
        assert_eq!(
            found(&resolve("TASK-19", graph, &ix)),
            (A.to_string(), Via::LegacyId)
        );
        assert_eq!(found(&resolve("root:goal-4", graph, &ix)).1, Via::LegacyId);
        assert_eq!(
            found(&resolve("0192f3a47b2d", graph, &ix)),
            (B.to_string(), Via::Prefix)
        );
        assert_eq!(
            found(&resolve(" fix the parser ", graph, &ix)),
            (A.to_string(), Via::Title)
        );
        assert_eq!(
            found(&resolve("TESTS", graph, &ix)),
            (B.to_string(), Via::Title)
        );
    }

    #[test]
    fn ambiguity_lists_every_match() {
        let ix = index();
        match resolve("0192f3a4", None, &ix) {
            Resolution::Ambiguous { via, matches } => {
                assert_eq!(via, Via::Prefix);
                assert_eq!(matches.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        match resolve("parser tests", None, &ix) {
            Resolution::Found { .. } => {}
            other => panic!("{other:?}"),
        }
        match resolve("fix", None, &ix) {
            Resolution::Ambiguous {
                via: Via::Title,
                matches,
            } => assert_eq!(matches.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_prefix_that_matches_nothing_falls_back_to_titles() {
        // `0192` is a prefix of A and B, so it never reaches C's title.
        assert!(matches!(
            resolve("0192", None, &index()),
            Resolution::Ambiguous {
                via: Via::Prefix,
                ..
            }
        ));
        // `release` is no prefix, so it's a title.
        assert_eq!(found(&resolve("release", None, &index())).0, C);
        assert_eq!(found(&resolve("ship 0192", None, &index())).0, C);
    }

    #[test]
    fn foreign_and_missing() {
        let ix = index();
        let other = "0190bbbb-0000-7000-8000-000000000000";
        let pin = "b".repeat(64);
        let foreign = format!("mdkg://{other}/{A}@sha256:{pin}");
        assert_eq!(
            resolve(&foreign, Uuid7::parse(GRAPH).ok(), &ix),
            Resolution::OtherGraph(NodeRef::parse(&foreign).unwrap())
        );
        // With no graph_id, no graph ref is local.
        assert!(matches!(
            resolve(&format!("mdkg://{GRAPH}/{A}"), None, &ix),
            Resolution::OtherGraph(_)
        ));
        assert_eq!(resolve("", None, &ix), Resolution::NotFound);
        assert_eq!(resolve("nothing like it", None, &ix), Resolution::NotFound);
        assert_eq!(
            resolve("task-0199ffff-0000-7000-8000-000000000000", None, &ix),
            Resolution::NotFound
        );
        match resolve(&format!("{A}@sha256:{pin}"), None, &ix) {
            Resolution::Found { pin: Some(p), .. } => assert_eq!(p, pin),
            other => panic!("{other:?}"),
        }
    }
}
