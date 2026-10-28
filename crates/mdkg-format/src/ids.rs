//! Identifier and reference grammars.
//!
//! These are hand-written matchers for the reference implementation's
//! patterns:
//!
//! | Name | Pattern |
//! |---|---|
//! | canonical id | `^[a-z]+-[0-9]+$`, or `rule-guide`, `rule-soul`, `rule-human` |
//! | workspace alias | `^[a-z][a-z0-9_]*$` |
//! | portable id | `^[a-z][a-z0-9_]*(?:[._-][a-z0-9_]+)*$` |
//! | URI ref | `^[a-z][a-z0-9+.-]*://\S+$` (case-insensitive scheme) |
//! | identity ref | `mdkg://{uuid}/{uuid}` |
//! | sha256 ref | `^sha256:[a-f0-9]{64}$` |

const SPECIAL_IDS: &[&str] = &["rule-guide", "rule-soul", "rule-human"];

fn is_lower(c: char) -> bool {
    c.is_ascii_lowercase()
}

fn is_word(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '_')
}

/// `^[a-z]+-[0-9]+$` or a reserved special id.
pub fn is_canonical_id(value: &str) -> bool {
    if SPECIAL_IDS.contains(&value) {
        return true;
    }
    match value.split_once('-') {
        Some((prefix, number)) => {
            !prefix.is_empty()
                && prefix.chars().all(is_lower)
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// `^[a-z][a-z0-9_]*$`.
pub fn is_workspace_alias(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if is_lower(c)) && chars.all(is_word)
}

/// A canonical id, or `^[a-z][a-z0-9_]*(?:[._-][a-z0-9_]+)*$`.
pub fn is_portable_id(value: &str) -> bool {
    if is_canonical_id(value) {
        return true;
    }
    let mut chars = value.chars().peekable();
    match chars.next() {
        Some(c) if is_lower(c) => {}
        _ => return false,
    }
    let mut previous_was_separator = false;
    for c in chars {
        if matches!(c, '.' | '-') {
            if previous_was_separator {
                return false;
            }
            previous_was_separator = true;
        } else if is_word(c) {
            previous_was_separator = false;
        } else {
            return false;
        }
    }
    !previous_was_separator
}

fn is_uuid(value: &str) -> bool {
    // [0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    let hex = |s: &str| s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    parts.iter().zip(lens).all(|(p, n)| p.len() == n && hex(p))
        && matches!(parts[2].as_bytes()[0], b'1'..=b'8')
        && matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// A parsed `mdkg://{graph_id}/{node_id}` identity reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRef {
    pub graph_id: String,
    pub node_id: String,
}

/// Parses `mdkg://{uuid}/{uuid}`.
pub fn parse_identity_ref(value: &str) -> Option<IdentityRef> {
    let rest = value.strip_prefix("mdkg://")?;
    let (graph_id, node_id) = rest.split_once('/')?;
    (is_uuid(graph_id) && is_uuid(node_id)).then(|| IdentityRef {
        graph_id: graph_id.to_string(),
        node_id: node_id.to_string(),
    })
}

/// Whether a value is a valid identity UUID.
pub fn is_identity_uuid(value: &str) -> bool {
    is_uuid(value)
}

fn split_ref(value: &str) -> Option<(Option<&str>, &str)> {
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or_default();
    match (parts.next(), parts.next()) {
        (None, _) => Some((None, first)),
        (Some(second), None) => Some((Some(first), second)),
        _ => None,
    }
}

/// An identity ref, a canonical id, or `workspace:canonical-id`.
pub fn is_canonical_id_ref(value: &str) -> bool {
    if parse_identity_ref(value).is_some() {
        return true;
    }
    let normalized = value.to_lowercase();
    match split_ref(&normalized) {
        Some((None, id)) => is_canonical_id(id),
        Some((Some(ws), id)) => is_workspace_alias(ws) && is_canonical_id(id),
        None => false,
    }
}

/// An identity ref, a portable id, or `workspace:portable-id`.
pub fn is_portable_id_ref(value: &str) -> bool {
    if parse_identity_ref(value).is_some() {
        return true;
    }
    let normalized = value.to_lowercase();
    match split_ref(&normalized) {
        Some((None, id)) => is_portable_id(id),
        Some((Some(ws), id)) => is_workspace_alias(ws) && is_portable_id(id),
        None => false,
    }
}

/// `^[a-z][a-z0-9+.-]*://\S+$`, case-insensitive.
pub fn is_uri_ref(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && !rest.is_empty()
        && !rest.chars().any(crate::js::is_js_whitespace)
}

/// `^archive://([a-z][a-z0-9_]*(?:[._-][a-z0-9_]+)*)$`. Every canonical id
/// also matches the portable pattern, so this is `is_portable_id`.
pub fn archive_id_from_uri(value: &str) -> Option<&str> {
    let id = value.strip_prefix("archive://")?;
    is_portable_id(id).then_some(id)
}

/// `^sha256:[a-f0-9]{64}$`.
pub fn is_sha256_ref(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    })
}

/// `/^mdkg:/i`, safe for any UTF-8 input.
pub fn has_mdkg_scheme(value: &str) -> bool {
    value
        .as_bytes()
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"mdkg:"))
}

/// Whether a reference is a valid portable id ref or URI ref, as accepted in
/// `refs`, `context_refs` and `evidence_refs`.
pub fn validate_portable_or_uri_ref(value: &str) -> bool {
    if has_mdkg_scheme(value) {
        return parse_identity_ref(value).is_some();
    }
    if is_uri_ref(value) {
        if value.starts_with("archive://") {
            return archive_id_from_uri(value).is_some();
        }
        return true;
    }
    value == value.to_lowercase() && is_portable_id_ref(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_ids() {
        assert!(is_canonical_id("task-1"));
        assert!(is_canonical_id("rule-guide"));
        assert!(!is_canonical_id("task-1a"));
        assert!(!is_canonical_id("work_order-1"));
        assert!(!is_canonical_id("Task-1"));
        assert!(!is_canonical_id("-1"));
    }

    #[test]
    fn portable_ids() {
        assert!(is_portable_id("agent.repo-worker"));
        assert!(is_portable_id("a_b.c-d"));
        assert!(!is_portable_id("a..b"));
        assert!(!is_portable_id("a-"));
        assert!(!is_portable_id("1a"));
        assert!(!is_portable_id("a-.b"));
    }

    #[test]
    fn refs() {
        assert!(is_canonical_id_ref("root:task-1"));
        assert!(!is_canonical_id_ref("a:b:task-1"));
        assert!(is_portable_id_ref("ws_2:agent.x"));
        assert!(validate_portable_or_uri_ref("https://example.com/x"));
        assert!(!validate_portable_or_uri_ref("Task-1"));
        assert!(is_uri_ref("HTTP://x"));
        assert!(!is_uri_ref("http://"));
        let id = "mdkg://0192f4c2-1b2a-7c3d-8e4f-0123456789ab/0192f4c2-1b2a-7c3d-9e4f-0123456789ab";
        assert!(parse_identity_ref(id).is_some());
        assert!(is_sha256_ref(&format!("sha256:{}", "a".repeat(64))));
        // Non-ASCII input never panics.
        assert!(!validate_portable_or_uri_ref("abcdé"));
        assert!(!has_mdkg_scheme("ééé"));
    }
}
