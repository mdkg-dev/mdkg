//! Resolving a user-supplied id or qid to one node, as 0.6.0 does.

use mdkg_format::ids;

use crate::index::{Index, RefTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Ok(String),
    Missing(Vec<String>),
    Ambiguous(Vec<String>),
}

/// Resolves `value` (an id, `ws:id` qid, or identity ref) against the index.
///
/// In a format 0.7 graph a full id or a ref to this graph's node resolves by
/// UUID (a stale type too), and an exact `legacy_id` resolves to its node.
/// Prefixes and titles are lane 4's handle resolution
/// (`mdkg_format::handle`).
pub fn resolve_qid(index: &Index, value: &str, ws_hint: Option<&str>) -> Resolution {
    // A full id names the node with its UUID or nothing: it never falls
    // through to a legacy_id that happens to look the same.
    match index.target(value) {
        RefTarget::Local { node, .. } => return Resolution::Ok(node.qid.clone()),
        RefTarget::Missing | RefTarget::Foreign => return Resolution::Missing(Vec::new()),
        RefTarget::NotARef => {}
    }
    let resolved = resolve_legacy(index, value, ws_hint);
    if !matches!(resolved, Resolution::Missing(_)) {
        return resolved;
    }
    let lower = value.to_lowercase();
    let mut matches: Vec<String> = index
        .nodes
        .iter()
        .filter(|n| {
            n.legacy_id.as_deref() == Some(lower.as_str()) && ws_hint.is_none_or(|ws| n.ws == ws)
        })
        .map(|n| n.qid.clone())
        .collect();
    matches.sort();
    match matches.len() {
        0 => resolved,
        1 => Resolution::Ok(matches.remove(0)),
        _ => Resolution::Ambiguous(matches),
    }
}

/// 0.6.0's resolution: ids, qids and identity refs.
fn resolve_legacy(index: &Index, value: &str, ws_hint: Option<&str>) -> Resolution {
    if ids::parse_identity_ref(value).is_some() {
        let mut matches: Vec<String> = index
            .nodes
            .iter()
            .filter(|n| {
                n.identity
                    .as_ref()
                    .is_some_and(|i| format!("mdkg://{}/{}", i.graph_id, i.node_id) == value)
                    && ws_hint.is_none_or(|ws| n.ws == ws)
            })
            .map(|n| n.qid.clone())
            .collect();
        matches.sort();
        return match matches.len() {
            1 => Resolution::Ok(matches.remove(0)),
            0 => Resolution::Missing(matches),
            _ => Resolution::Ambiguous(matches),
        };
    }
    if ids::has_mdkg_scheme(value) {
        return Resolution::Missing(Vec::new());
    }
    let normalized = value.to_lowercase();
    if normalized.contains(':') {
        return if index.contains(&normalized) {
            Resolution::Ok(normalized)
        } else {
            Resolution::Missing(Vec::new())
        };
    }
    let mut matches: Vec<String> = index
        .nodes
        .iter()
        .filter(|n| n.id == normalized)
        .map(|n| n.qid.clone())
        .collect();
    matches.sort();
    if let Some(ws) = ws_hint {
        let mut in_ws: Vec<String> = matches
            .iter()
            .filter(|q| index.get(q).is_some_and(|n| n.ws == ws))
            .cloned()
            .collect();
        return match in_ws.len() {
            1 => Resolution::Ok(in_ws.remove(0)),
            0 => Resolution::Missing(matches),
            _ => Resolution::Ambiguous(in_ws),
        };
    }
    match matches.len() {
        1 => Resolution::Ok(matches.remove(0)),
        0 => Resolution::Missing(Vec::new()),
        _ => Resolution::Ambiguous(matches),
    }
}

/// An unresolved value as an error: `node.not_found` or `node.ambiguous`,
/// with [`resolve_hint`].
pub fn resolve_error(
    label: &str,
    value: &str,
    result: &Resolution,
    ws_hint: Option<&str>,
) -> crate::GraphError {
    let message = format_resolve_error(label, value, result, ws_hint);
    let error = match result {
        // Ambiguity uses exit 7, distinct from a node that isn't there.
        Resolution::Ambiguous(_) => crate::GraphError::ambiguous(message),
        _ => crate::GraphError::not_found(mdkg_format::Code::NodeNotFound, message),
    };
    error.with_hint(resolve_hint(result))
}

/// The error text for an unresolved value. What was typed is never quoted,
/// even for 0.6.0 aliases, because it may be a secret. The stable diagnostic
/// code is the contract, and the hint says what matched.
pub fn format_resolve_error(
    label: &str,
    _value: &str,
    result: &Resolution,
    ws_hint: Option<&str>,
) -> String {
    match result {
        Resolution::Missing(_) if ws_hint.is_some() => {
            "no node matches that handle in that workspace".to_string()
        }
        Resolution::Missing(_) => "no node matches that handle".to_string(),
        Resolution::Ambiguous(candidates) => {
            format!("that handle names {} nodes", candidates.len())
        }
        Resolution::Ok(qid) => format!("{label} resolved: {qid}"),
    }
}

/// What to do about an unresolved value: the near misses, the matches, or
/// how to find the node.
pub fn resolve_hint(result: &Resolution) -> String {
    match result {
        Resolution::Missing(candidates) if !candidates.is_empty() => {
            format!("did you mean {}?", candidates.join(", "))
        }
        Resolution::Ambiguous(candidates) => format!(
            "matches: {}; give more of the id, or the full id",
            candidates.join(", ")
        ),
        _ => "`search <words>` or `list` finds it; a handle is a full id, a unique id prefix or a \
              title fragment"
            .to_string(),
    }
}

#[cfg(test)]
mod quoting {
    use super::*;

    /// Nothing typed reaches the message or the hint, including secrets
    /// and hidden characters.
    #[test]
    fn a_refusal_quotes_nothing_typed() {
        for result in [
            Resolution::Missing(Vec::new()),
            Resolution::Missing(vec!["task-1".into()]),
            Resolution::Ambiguous(vec!["task-1".into(), "task-2".into()]),
        ] {
            for ws in [None, Some("aux")] {
                let message = format_resolve_error("id", "a\n[NOTE]\u{202e}b", &result, ws);
                assert!(!message.contains("[NOTE]"), "{message}");
                assert!(!resolve_hint(&result).contains("[NOTE]"));
            }
        }
        assert_eq!(
            format_resolve_error("id", "x", &Resolution::Missing(Vec::new()), None),
            "no node matches that handle"
        );
        assert_eq!(
            format_resolve_error(
                "id",
                "x",
                &Resolution::Ambiguous(vec!["a".into(), "b".into()]),
                None
            ),
            "that handle names 2 nodes"
        );
    }
}
