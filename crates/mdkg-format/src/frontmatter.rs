//! The frontmatter grammar.
//!
//! A node file starts with a `---` line, then `key: value` lines, then a
//! closing `---` line. Keys match `^[a-z][a-z0-9_]*$`, or are extension keys
//! `x-<namespace>.<field>[.<field>…]`, accepted in every graph. A value is a scalar
//! string, `true`/`false`, or a `[a, b]` list split on commas. There is no
//! quoting or escaping. This mirrors mdkg 0.6.0 exactly, including its error
//! messages and limits.

use crate::diag::Code;
use crate::error::FormatError;
use crate::js;

pub const MAX_FRONTMATTER_LINES: usize = 1_000;
pub const MAX_FRONTMATTER_LINE_BYTES: usize = 64 * 1024;
pub const MAX_FRONTMATTER_LIST_ITEMS: usize = 10_000;

/// A frontmatter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

/// Frontmatter keys and values in file order. Unknown keys are kept, so a
/// read-write round trip never drops data, including unknown extensions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    entries: Vec<(String, Value)>,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// Inserts or replaces a value, keeping the original position on replace.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }
}

/// A parsed node file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub frontmatter: Frontmatter,
    /// The body after the closing delimiter, with CRLF normalized to LF.
    pub body: String,
}

/// Byte offsets of the frontmatter block within the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceBounds {
    /// Where the closing `---` line's content ends (before its line break).
    pub header_end: usize,
    /// Where the body starts (after the closing line's line break).
    pub body_start: usize,
    /// The line ending detected on the first line.
    pub crlf: bool,
}

struct Header<'a> {
    lines: Vec<&'a str>,
    end_index: usize,
    bounds: SourceBounds,
}

/// A core key, `[a-z][a-z0-9_]*`.
pub fn is_core_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// A namespaced extension key in format 0.7:
/// `x-<namespace>.<field>[.<field>…]`, where the namespace is
/// `[a-z][a-z0-9-]*` and each field a core key.
pub fn is_extension_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("x-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let ns = parts.next().unwrap_or_default();
    let mut chars = ns.chars();
    let mut fields = 0;
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
        && parts.all(|f| {
            fields += 1;
            is_core_key(f)
        })
        && fields > 0
}

fn is_key(key: &str) -> bool {
    is_core_key(key) || is_extension_key(key)
}

/// How lines and values are trimmed.
///
/// - `Js`: 0.6.0's `String.prototype.trim` (ECMAScript whitespace, U+FEFF
///   included), for reading and migrating 0.6.0 files.
/// - `Ascii`: format 0.7's strict rule, space and tab only. Anything else
///   (a BOM, U+00A0, a lone CR) is part of the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lexical {
    #[default]
    Js,
    Ascii,
}

impl Lexical {
    pub fn trim(self, s: &str) -> &str {
        match self {
            Lexical::Js => js::trim(s),
            Lexical::Ascii => s.trim_matches([' ', '\t']),
        }
    }
}

fn scan_header<'a>(content: &'a str, path: &str, lex: Lexical) -> Result<Header<'a>, FormatError> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut line_index = 0;
    let mut crlf = false;
    loop {
        let newline = content[start..].find('\n').map(|i| start + i);
        if line_index == 0
            && let Some(n) = newline
        {
            crlf = n > 0 && content.as_bytes()[n - 1] == b'\r';
        }
        let raw_end = newline.unwrap_or(content.len());
        let mut line = &content[start..raw_end];
        if newline.is_some() {
            line = line.strip_suffix('\r').unwrap_or(line);
        }
        if line_index == 0 && lex.trim(line) != "---" {
            return Err(FormatError::at_line(
                path,
                1,
                Code::FrontmatterMissingOpen,
                "frontmatter must start with ---",
            ));
        }
        if line_index > 0 && lex.trim(line) == "---" {
            let (header_end, body_start) = match newline {
                Some(n) => {
                    let before_cr = if n > 0 && content.as_bytes()[n - 1] == b'\r' {
                        n - 1
                    } else {
                        n
                    };
                    (before_cr, n + 1)
                }
                None => (content.len(), content.len()),
            };
            if line_index > MAX_FRONTMATTER_LINES {
                return Err(FormatError::at_line(
                    path,
                    1,
                    Code::FrontmatterTooLarge,
                    format!("frontmatter exceeds {MAX_FRONTMATTER_LINES} lines"),
                ));
            }
            return Ok(Header {
                lines,
                end_index: line_index,
                bounds: SourceBounds {
                    header_end,
                    body_start,
                    crlf,
                },
            });
        }
        if line_index <= MAX_FRONTMATTER_LINES {
            lines.push(line);
        }
        match newline {
            Some(n) => {
                start = n + 1;
                line_index += 1;
            }
            None => break,
        }
    }
    Err(FormatError::at_line(
        path,
        1,
        Code::FrontmatterUnclosed,
        "frontmatter closing --- not found",
    ))
}

/// Source spans of the frontmatter block. Body bytes are not normalized.
pub fn source_bounds(content: &str, path: &str) -> Result<SourceBounds, FormatError> {
    scan_header(content, path, Lexical::Js).map(|h| h.bounds)
}

fn parse_list(
    raw: &str,
    path: &str,
    line: usize,
    lex: Lexical,
) -> Result<Vec<String>, FormatError> {
    if !raw.starts_with('[') || !raw.ends_with(']') {
        return Err(FormatError::at_line(
            path,
            line,
            Code::FrontmatterBadList,
            "list must be enclosed in [ ]",
        ));
    }
    let inner = lex.trim(&raw[1..raw.len() - 1]);
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() > MAX_FRONTMATTER_LIST_ITEMS {
        return Err(FormatError::at_line(
            path,
            line,
            Code::FrontmatterTooLarge,
            format!("list exceeds {MAX_FRONTMATTER_LIST_ITEMS} items"),
        ));
    }
    parts
        .into_iter()
        .map(|part| {
            let item = lex.trim(part);
            if item.is_empty() {
                Err(FormatError::at_line(
                    path,
                    line,
                    Code::FrontmatterEmptyListItem,
                    "list items must be non-empty",
                ))
            } else {
                Ok(item.to_string())
            }
        })
        .collect()
}

fn parse_value(raw: &str, path: &str, line: usize, lex: Lexical) -> Result<Value, FormatError> {
    if raw.is_empty() {
        return Err(FormatError::at_line(
            path,
            line,
            Code::FrontmatterEmptyValue,
            "value must not be empty",
        ));
    }
    if raw.starts_with('[') {
        return parse_list(raw, path, line, lex).map(Value::List);
    }
    Ok(match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(raw.to_string()),
    })
}

/// Parses a node file's frontmatter and body.
pub fn parse(content: &str, path: &str) -> Result<Parsed, FormatError> {
    parse_with(content, path, Lexical::Js)
}

/// [`parse`] with a lexical rule: format 0.7 files are read with
/// [`Lexical::Ascii`].
pub fn parse_with(content: &str, path: &str, lex: Lexical) -> Result<Parsed, FormatError> {
    let header = scan_header(content, path, lex)?;
    let mut frontmatter = Frontmatter::default();
    for i in 1..header.end_index {
        let line = header.lines[i];
        let line_number = i + 1;
        if line.len() > MAX_FRONTMATTER_LINE_BYTES {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterTooLarge,
                format!("frontmatter line exceeds {MAX_FRONTMATTER_LINE_BYTES} bytes"),
            ));
        }
        // Format 0.7: a CR only ends a line (CRLF), so every value writes
        // back as it reads.
        if lex == Lexical::Ascii && line.contains('\r') {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterStrayCr,
                "frontmatter lines must not hold a CR except before LF",
            ));
        }
        if lex.trim(line).is_empty() {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterBlankLine,
                "frontmatter lines must not be blank",
            ));
        }
        let Some(colon) = line.find(':') else {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterNotKeyValue,
                "frontmatter lines must be key: value",
            ));
        };
        let key = lex.trim(&line[..colon]);
        let raw_value = lex.trim(&line[colon + 1..]);
        if !is_key(key) {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterInvalidKey,
                format!("invalid key: {key}"),
            ));
        }
        if frontmatter.contains(key) {
            return Err(FormatError::at_line(
                path,
                line_number,
                Code::FrontmatterDuplicateKey,
                format!("duplicate key: {key}"),
            ));
        }
        let value = parse_value(raw_value, path, line_number, lex)?;
        frontmatter.insert(key, value);
    }
    let body = content[header.bounds.body_start..].replace("\r\n", "\n");
    Ok(Parsed { frontmatter, body })
}

/// The canonical key order used when writing frontmatter.
pub const DEFAULT_KEY_ORDER: &[&str] = &[
    "id",
    "legacy_id",
    "graph_id",
    "node_id",
    "type",
    "title",
    "checkpoint_kind",
    "status",
    "priority",
    "goal_state",
    "goal_condition",
    "scope_refs",
    "active_node",
    "last_active_node",
    "required_skills",
    "required_checks",
    "max_iterations",
    "blocked_after_attempts",
    "loop_mode",
    "loop_role",
    "scope_description",
    "template_refs",
    "materialization_mode",
    "child_refs",
    "pre_run_questions",
    "question_answer_refs",
    "pre_approved_actions",
    "approval_gated_actions",
    "required_actions",
    "requested_actions",
    "prohibited_actions",
    "action_approval_refs",
    "evidence_lanes",
    "evidence_lane_refs",
    "lane_waiver_refs",
    "lane_waiver_decision_refs",
    "lane_waiver_approval_refs",
    "run_refs",
    "decision_refs",
    "output_refs",
    "approval_refs",
    "evaluation_refs",
    "definition_of_done",
    "blocker_policy",
    "epic",
    "parent",
    "prev",
    "next",
    "supersedes",
    "version",
    "spec_kind",
    "role",
    "runtime_mode",
    "work_contracts",
    "contract_profile",
    "validation_policy_ref",
    "evidence_policy_ref",
    "requested_capabilities",
    "resource_profile",
    "update_policy",
    "agent_id",
    "kind",
    "receipt_kind",
    "redaction_class",
    "required_capabilities",
    "inputs",
    "outputs",
    "receipt_required",
    "work_id",
    "work_version",
    "requester",
    "order_status",
    "request_ref",
    "trigger_ref",
    "payload_hash",
    "queue_refs",
    "work_order_id",
    "receipt_status",
    "outcome",
    "cost_ref",
    "redaction_policy",
    "target_id",
    "sentiment",
    "feedback_status",
    "source_ref",
    "receipt_id",
    "dispute_status",
    "severity",
    "proposal_status",
    "proposal_kind",
    "archive_kind",
    "source_path",
    "stored_path",
    "compressed_path",
    "mime_type",
    "byte_size",
    "sha256",
    "compressed_sha256",
    "visibility",
    "provenance",
    "ingest_status",
    "input_refs",
    "requested_outputs",
    "constraint_refs",
    "artifact_policy",
    "proof_refs",
    "attestation_refs",
    "evidence_hashes",
    "input_hashes",
    "output_hashes",
    "tags",
    "owners",
    "links",
    "artifacts",
    "relates",
    "blocked_by",
    "blocks",
    "refs",
    "context_refs",
    "evidence_refs",
    "aliases",
    "skills",
    "cases",
    "scope",
    "created",
    "updated",
];

fn format_value(value: &Value) -> String {
    match value {
        Value::List(items) if items.is_empty() => "[]".to_string(),
        Value::List(items) => format!("[{}]", items.join(", ")),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::String(s) => s.clone(),
    }
}

/// Sets `key` in a node file's frontmatter by editing its one line in place,
/// or adding it before the closing `---`. Everything else, including key
/// order and line endings, stays as it was, so diffs stay one line long.
pub fn set_field(
    content: &str,
    path: &str,
    key: &str,
    value: &Value,
) -> Result<String, FormatError> {
    set_field_with(content, path, key, value, Lexical::Js)
}

/// [`set_field`] under a lexical rule: [`Lexical::Ascii`] for format 0.7
/// files, so the edit finds lines and checks the result as 0.7 reads them.
pub fn set_field_with(
    content: &str,
    path: &str,
    key: &str,
    value: &Value,
    lex: Lexical,
) -> Result<String, FormatError> {
    let parsed = parse_with(content, path, lex)?;
    let line = format!("{key}: {}", format_value(value));
    let mut out = String::with_capacity(content.len() + line.len() + 2);
    let mut in_header = false;
    let mut done = false;
    for (i, raw) in content.split_inclusive('\n').enumerate() {
        let body = raw.trim_end_matches(['\n', '\r']);
        let ending = &raw[body.len()..];
        if i == 0 {
            in_header = true;
            out.push_str(raw);
            continue;
        }
        if in_header && lex.trim(body) == "---" {
            if !done {
                out.push_str(&line);
                out.push_str(if ending.is_empty() { "\n" } else { ending });
                done = true;
            }
            in_header = false;
            out.push_str(raw);
            continue;
        }
        if in_header
            && !done
            && body
                .split_once(':')
                .is_some_and(|(k, _)| lex.trim(k) == key)
        {
            out.push_str(&line);
            out.push_str(ending);
            done = true;
            continue;
        }
        out.push_str(raw);
    }
    // The edit must leave a file that parses with the same other keys.
    let edited = parse_with(&out, path, lex)?;
    debug_assert_eq!(edited.body, parsed.body);
    Ok(out)
}

/// Formats frontmatter lines: known keys in `key_order`, then the rest sorted.
pub fn format_lines(frontmatter: &Frontmatter, key_order: &[&str]) -> Vec<String> {
    let mut keys: Vec<&str> = key_order
        .iter()
        .copied()
        .filter(|k| frontmatter.contains(k))
        .collect();
    let mut rest: Vec<&str> = frontmatter
        .keys()
        .filter(|k| !key_order.contains(k))
        .collect();
    rest.sort_unstable();
    keys.extend(rest);
    keys.into_iter()
        .filter_map(|k| {
            frontmatter
                .get(k)
                .map(|v| format!("{k}: {}", format_value(v)))
        })
        .collect()
}

/// Writes a complete node file: delimiters, frontmatter and body.
pub fn write(frontmatter: &Frontmatter, body: &str) -> String {
    let mut out = String::from("---\n");
    for line in format_lines(frontmatter, DEFAULT_KEY_ORDER) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stray_cr_is_refused_in_0_7() {
        for src in [
            "---\ntitle: \r\r\n---\n",
            "---\ntitle: a\rb\n---\n",
            "---\r\ntitle: a\r\n---\r\n",
        ] {
            let strict = parse_with(src, "f.md", Lexical::Ascii);
            if src.contains("\r\r") || src.contains("a\rb") {
                assert_eq!(
                    strict.unwrap_err().code(),
                    Code::FrontmatterStrayCr,
                    "{src:?}"
                );
            } else {
                assert!(strict.is_ok(), "CRLF endings are fine: {src:?}");
            }
        }
        // 0.6.0 reads keep their behaviour.
        assert!(parse("---\ntitle: a\rb\n---\n", "f.md").is_ok());
    }

    #[test]
    fn strict_edits_keep_what_0_7_reads() {
        // A title of one U+00A0 is text in a 0.7 file: an edit keeps it,
        // where 0.6.0's trimming would call it empty.
        let src = "---\nid: x\ntitle: \u{A0}\nstatus: todo\nx-a.b: \u{A0}v\u{A0}\n---\n";
        let out = set_field_with(
            src,
            "f.md",
            "status",
            &Value::String("done".into()),
            Lexical::Ascii,
        )
        .unwrap();
        assert_eq!(out, src.replace("status: todo", "status: done"));
        assert!(set_field(src, "f.md", "status", &Value::String("done".into())).is_err());
        let parsed = parse_with(src, "f.md", Lexical::Ascii).unwrap();
        assert_eq!(
            parsed.frontmatter.get("x-a.b").and_then(Value::as_str),
            Some("\u{A0}v\u{A0}")
        );
        assert!(write(&parsed.frontmatter, &parsed.body).contains("x-a.b: \u{A0}v\u{A0}"));
    }

    #[test]
    fn set_field_edits_one_line_in_place() {
        let src = "---\nid: task-1\nstatus: todo\ntitle: T\n---\n\n# Body\n";
        let out = set_field(src, "f.md", "status", &Value::String("done".into())).unwrap();
        assert_eq!(
            out,
            "---\nid: task-1\nstatus: done\ntitle: T\n---\n\n# Body\n"
        );
        let out = set_field(src, "f.md", "goal_state", &Value::String("active".into())).unwrap();
        assert_eq!(
            out,
            "---\nid: task-1\nstatus: todo\ntitle: T\ngoal_state: active\n---\n\n# Body\n"
        );
        let crlf = "---\r\nid: task-1\r\nstatus: todo\r\n---\r\nbody\r\n";
        let out = set_field(crlf, "f.md", "status", &Value::String("done".into())).unwrap();
        assert_eq!(out, "---\r\nid: task-1\r\nstatus: done\r\n---\r\nbody\r\n");
    }

    fn err(content: &str) -> String {
        parse(content, "f.md").unwrap_err().to_string()
    }

    #[test]
    fn parses_values() {
        let p = parse(
            "---\nid: task-1\nflag: true\ntags: [a ,  b]\nempty: []\ntitle: a: b\n---\nbody\n",
            "f.md",
        )
        .unwrap();
        assert_eq!(
            p.frontmatter.get("id"),
            Some(&Value::String("task-1".into()))
        );
        assert_eq!(p.frontmatter.get("flag"), Some(&Value::Bool(true)));
        assert_eq!(
            p.frontmatter.get("tags"),
            Some(&Value::List(vec!["a".into(), "b".into()]))
        );
        assert_eq!(p.frontmatter.get("empty"), Some(&Value::List(vec![])));
        assert_eq!(
            p.frontmatter.get("title"),
            Some(&Value::String("a: b".into()))
        );
        assert_eq!(p.body, "body\n");
    }

    #[test]
    fn reports_errors_like_the_reference() {
        assert_eq!(err(""), "f.md:1: frontmatter must start with ---");
        assert_eq!(
            err("---\nid: x\n"),
            "f.md:1: frontmatter closing --- not found"
        );
        assert_eq!(
            err("---\nid: x\n\n---\n"),
            "f.md:3: frontmatter lines must not be blank"
        );
        assert_eq!(
            err("---\nid x\n---\n"),
            "f.md:2: frontmatter lines must be key: value"
        );
        assert_eq!(err("---\nType: x\n---\n"), "f.md:2: invalid key: Type");
        assert_eq!(err("---\na: 1\na: 2\n---\n"), "f.md:3: duplicate key: a");
        assert_eq!(err("---\na:\n---\n"), "f.md:2: value must not be empty");
        assert_eq!(
            err("---\na: [x\n---\n"),
            "f.md:2: list must be enclosed in [ ]"
        );
        assert_eq!(
            err("---\na: [x, ,y]\n---\n"),
            "f.md:2: list items must be non-empty"
        );
    }

    #[test]
    fn crlf_body_is_normalized() {
        let p = parse("---\r\nid: x\r\n---\r\nl1\r\nl2\r\n", "f.md").unwrap();
        assert_eq!(p.frontmatter.get("id"), Some(&Value::String("x".into())));
        assert_eq!(p.body, "l1\nl2\n");
        let b = source_bounds("---\r\nid: x\r\n---\r\nbody", "f.md").unwrap();
        assert!(b.crlf);
        assert_eq!((b.header_end, b.body_start), (15, 17));
    }

    #[test]
    fn write_orders_known_keys_then_the_rest() {
        let p = parse(
            "---\nzeta: 1\ntitle: T\nid: task-1\nalpha: [b, a]\n---\nx\n",
            "f.md",
        )
        .unwrap();
        assert_eq!(
            write(&p.frontmatter, &p.body),
            "---\nid: task-1\ntitle: T\nalpha: [b, a]\nzeta: 1\n---\nx\n"
        );
    }
}
