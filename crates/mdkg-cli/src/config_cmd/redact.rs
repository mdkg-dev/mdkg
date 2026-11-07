//! Display-only precautions. Keep the merged tree's shape and origin pointers;
//! arbitrary secrets and field names still cannot be classified completely.

use serde_json::Value;

fn credential_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "auth" | "pat" | "bearer")
        || [
            "key",
            "token",
            "secret",
            "password",
            "passwd",
            "credential",
            "authorization",
            "cookie",
        ]
        .iter()
        .any(|part| name.contains(part))
}

/// Some(true) is an assignment; Some(false) consumes the next argument.
fn credential_option(word: &str) -> Option<bool> {
    let option = word.trim_matches(['\'', '"']).strip_prefix("--")?;
    let (name, assigned) = option
        .split_once('=')
        .map_or((option, false), |(n, _)| (n, true));
    credential_name(name).then_some(assigned)
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '+' | '/' | '=')
}

fn basic_payload(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    if text.len() - body.len() > 2 {
        return false;
    }
    let digit = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    if !body.bytes().all(|b| digit(b).is_some()) {
        return false;
    }
    // Basic encodes user:password. Checking for the separator also avoids
    // hiding ordinary single words such as "Basic introduction".
    let (mut bits, mut count) = (0u32, 0);
    for byte in body.bytes() {
        bits = (bits << 6) | u32::from(digit(byte).expect("checked alphabet"));
        count += 6;
        if count >= 8 {
            count -= 8;
            if (bits >> count) & 255 == u32::from(b':') {
                return true;
            }
        }
    }
    false
}

fn authorization(text: &str) -> bool {
    // Inspect adjacent words, including compact headers and prose. Payload
    // validation distinguishes Basic credentials from ordinary descriptions.
    let (mut before_previous, mut previous) = ("", "");
    text.split_whitespace()
        .zip(text.split_whitespace().skip(1))
        .any(|(word, payload)| {
            // Header maps allow whitespace on either side of the colon.
            let (header, scheme) = match word.rsplit_once(':') {
                Some(("", scheme)) => (Some(previous), scheme),
                Some((header, scheme)) => (Some(header), scheme),
                None => (
                    previous.strip_suffix(':').map(|header| {
                        if header.is_empty() {
                            before_previous
                        } else {
                            header
                        }
                    }),
                    word,
                ),
            };
            let explicit = header.is_some_and(|header| {
                // A quoted header may be attached to an option, e.g. curl -H'.
                let header = header
                    .trim_end_matches(['\'', '"'])
                    .rsplit(['\'', '"'])
                    .next()
                    .unwrap_or(header);
                let header = header.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                header.eq_ignore_ascii_case("authorization")
                    || header.eq_ignore_ascii_case("proxy-authorization")
            });
            before_previous = previous;
            previous = word;
            let scheme = scheme.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if scheme.eq_ignore_ascii_case("basic") {
                basic_payload(payload.trim_matches(|c: char| {
                    !c.is_ascii_alphanumeric() && !matches!(c, '+' | '/' | '=')
                }))
            } else if scheme.eq_ignore_ascii_case("bearer") && explicit {
                // A compact header map can continue directly after the token.
                // An opening quote may precede the payload; a second quote
                // means it is empty, even when another map entry follows.
                let payload = payload
                    .trim_start_matches(|c: char| !token_char(c) && !matches!(c, '\'' | '"'));
                let payload = payload
                    .strip_prefix('\'')
                    .or_else(|| payload.strip_prefix('"'))
                    .unwrap_or(payload);
                payload.chars().next().is_some_and(token_char)
            } else {
                let payload = payload.trim_matches(|c: char| !token_char(c));
                scheme.eq_ignore_ascii_case("bearer")
                    // Unlabelled prose keeps the existing length heuristic.
                    && payload.len() >= 8
                    && payload.chars().all(token_char)
            }
        })
}

fn additional_token(text: &str) -> bool {
    // The common mail detector supplies the other provider, JWT and exact
    // AWS shapes. These remaining prefixes are display precautions only.
    [
        ("sk-", 20),
        ("xoxa-", 20),
        ("xoxr-", 20),
        ("ghu_", 30),
        ("ghr_", 30),
    ]
    .iter()
    .any(|(prefix, min)| {
        text.match_indices(prefix).any(|(i, _)| {
            text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
                && text[i + prefix.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    .count()
                    >= *min
        })
    })
}

fn query_name(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(a), Some(b)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            decoded.push((a * 16 + b) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn url_credential(text: &str) -> bool {
    // Bound each scan by the next scheme to avoid repeatedly scanning a
    // long suffix when a setting contains many adjacent or nested URLs.
    let mut schemes = text.match_indices("://").peekable();
    while let Some((i, _)) = schemes.next() {
        let end = schemes.peek().map_or(text.len(), |(next, _)| *next);
        let url = text[i + 3..end]
            .split(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>' | '`'))
            .next()
            .unwrap_or("");
        let authority = url.split(['/', '?', '#']).next().unwrap_or("");
        if authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
            userinfo
                .split_once(':')
                .is_some_and(|(_, password)| !password.is_empty())
        }) {
            return true;
        }
        let before_fragment = url.split('#').next().unwrap_or("");
        if before_fragment.split_once('?').is_some_and(|(_, query)| {
            query.split(['&', ';']).any(|pair| {
                pair.split_once('=').is_some_and(|(name, value)| {
                    !value.is_empty() && credential_name(&query_name(name))
                })
            })
        }) {
            return true;
        }
    }
    false
}

fn contents(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if crate::credentials::looks_secret(text)
        || additional_token(text)
        || (lower.contains("-----begin ") && lower.contains("private key"))
        || authorization(text)
        || url_credential(text)
    {
        return true;
    }
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if credential_option(word).is_some_and(|assigned| assigned || words.peek().is_some()) {
            return true;
        }
    }
    false
}

fn public_auth(path: &str, value: &Value) -> bool {
    let policy_parent = path == "/agents/defaults"
        || path
            .strip_prefix("/agents/")
            .and_then(|p| p.split_once('/'))
            .is_some_and(|(_, rest)| rest == "policy");
    policy_parent
        && (value.is_null()
            || value.as_object().is_some_and(|fields| {
                fields.iter().all(|(key, value)| match key.as_str() {
                    "prefer" => value.is_null() || value == "subscription",
                    "api_key_fallback" => value.is_null() || value.is_boolean(),
                    _ => false,
                })
            }))
}

/// Keys renamed because the key itself looked like a credential, as
/// `(old pointer, new pointer)` in the order they were renamed: an outer
/// key before the keys under it, each old pointer under its parent's new
/// one.
#[derive(Debug, Default)]
pub(crate) struct Renames {
    list: Vec<(String, String)>,
    count: usize,
}

impl Renames {
    /// `pointer` as it reads in the redacted tree.
    pub(crate) fn pointer(&self, pointer: &str) -> String {
        let mut pointer = pointer.to_owned();
        for (old, new) in &self.list {
            if pointer == *old {
                pointer.clone_from(new);
            } else if let Some(rest) = pointer
                .strip_prefix(old.as_str())
                .filter(|r| r.starts_with('/'))
            {
                pointer = format!("{new}{rest}");
            }
        }
        pointer
    }
}

/// Whether a key looks like a credential, by the rules values follow.
pub(super) fn key_looks_secret(key: &str) -> bool {
    contents(key)
}

/// Redacts one entry, read at `path`, as it is redacted inside the whole
/// tree; `sensitive` when its own key looks like a credential, so
/// everything under it is hidden as a renamed key's is.
pub(super) fn entry(value: &mut Value, path: &str, sensitive: bool) {
    walk(
        value,
        path,
        sensitive,
        &mut Vec::new(),
        &mut Renames::default(),
    );
}

/// The name a key that looks like a credential is shown under. The
/// number counts these keys in the whole tree, so names never collide.
pub(super) fn key_name(n: usize) -> String {
    format!("[redacted key {n}]")
}

/// Hides credential-like leaves (by their content, or by their own or a
/// parent's key name), and also renames
/// any object key that itself looks like a credential (a token used as a
/// map key, say), hiding everything under it. Pointers into the
/// original tree read through the returned [`Renames`].
pub(super) fn tree(value: &mut Value, path: &str, hidden: &mut Vec<String>) -> Renames {
    let mut renames = Renames::default();
    walk(value, path, false, hidden, &mut renames);
    renames
}

fn walk(
    value: &mut Value,
    path: &str,
    sensitive: bool,
    hidden: &mut Vec<String>,
    renames: &mut Renames,
) {
    match value {
        Value::Object(fields) => {
            // Rename first, keeping the keys' order. A new name never
            // matches a key the object already has.
            let mut renamed = std::collections::BTreeSet::new();
            if fields.keys().any(|k| contents(k)) {
                let old = std::mem::take(fields);
                let taken: std::collections::BTreeSet<String> = old.keys().cloned().collect();
                for (key, mut value) in old {
                    if !contents(&key) {
                        fields.insert(key, value);
                        continue;
                    }
                    let name = loop {
                        renames.count += 1;
                        let name = key_name(renames.count);
                        if !taken.contains(&name) {
                            break name;
                        }
                    };
                    let pointer = child(path, &name);
                    renames.list.push((child(path, &key), pointer.clone()));
                    // Everything under it is hidden, each leaf listed.
                    walk(&mut value, &pointer, true, hidden, renames);
                    renamed.insert(name.clone());
                    fields.insert(name, value);
                }
            }
            for (key, value) in fields {
                if renamed.contains(key) {
                    continue;
                }
                let public_flag =
                    key == "api_key_fallback" && (value.is_boolean() || value.is_null());
                let public_count = key == "context_tokens"
                    && value.as_u64().is_some()
                    && (path == "/agents/defaults/session"
                        || (path.starts_with("/agents/") && path.ends_with("/policy/session")));
                let auth_policy = key == "auth" && public_auth(path, value);
                let structural_name = value.is_object()
                    && matches!(
                        path,
                        "/agents" | "/workspaces" | "/subgraphs" | "/authority/delegates"
                    );
                let pointer = child(path, key);
                walk(
                    value,
                    &pointer,
                    sensitive
                        || (credential_name(key)
                            && !public_flag
                            && !public_count
                            && !auth_policy
                            && !structural_name),
                    hidden,
                    renames,
                );
            }
        }
        Value::Array(items) => {
            let mut previous_option = false;
            for (index, value) in items.iter_mut().enumerate() {
                // Capture option context before replacing any string leaf.
                let option = value.as_str().and_then(credential_option);
                walk(
                    value,
                    &format!("{path}/{index}"),
                    sensitive || previous_option || option == Some(true),
                    hidden,
                    renames,
                );
                previous_option = option == Some(false);
            }
        }
        _ if sensitive || value.as_str().is_some_and(contents) => {
            *value = Value::String("[redacted]".into());
            hidden.push(path.into());
        }
        _ => {}
    }
}

fn child(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

#[cfg(test)]
mod tests {
    /// A key's own redaction: by content for the key, and everything under
    /// a credential-shaped key hidden.
    #[test]
    fn an_entry_is_redacted_on_its_own_as_in_the_tree() {
        assert!(super::key_looks_secret(concat!(
            "Authorization: Bearer SYNTHETIC_",
            "X"
        )));
        assert!(!super::key_looks_secret("worker"));
        let mut plain = serde_json::json!({"role": "worker", "api_token": "SYNTHETIC_T"});
        super::entry(&mut plain, "/agents/worker", false);
        assert_eq!(
            plain,
            serde_json::json!({"role": "worker", "api_token": "[redacted]"})
        );
        let mut hidden = serde_json::json!({"role": "worker"});
        super::entry(&mut hidden, "/agents/x", true);
        assert_eq!(hidden, serde_json::json!({"role": "[redacted]"}));
        assert_eq!(super::key_name(2), "[redacted key 2]");
    }
}
