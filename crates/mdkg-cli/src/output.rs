//! The output contract: one envelope per command, compact node
//! records, coded diagnostics and exit codes. Only JSON is a contract; text
//! output is for people and may change.

use std::borrow::Cow;
use std::collections::BTreeMap;

use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_graph::{ErrorKind, GraphError, IndexNode};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;
use serde_json::Value;

/// A diagnostic code, serialized as its dotted name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeName(pub Code);

impl Serialize for CodeName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0.as_str())
    }
}

impl JsonSchema for CodeName {
    fn schema_name() -> Cow<'static, str> {
        "Code".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let names: Vec<&str> = Code::ALL.iter().map(|c| c.as_str()).collect();
        schemars::json_schema!({
            "description": "A stable diagnostic code.",
            "type": "string",
            "enum": names,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeverityName {
    Error,
    Warning,
    Info,
}

impl From<Severity> for SeverityName {
    fn from(s: Severity) -> Self {
        match s {
            Severity::Error => SeverityName::Error,
            Severity::Warning => SeverityName::Warning,
            Severity::Info => SeverityName::Info,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LocationOut {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticOut {
    pub code: CodeName,
    pub severity: SeverityName,
    /// Relative to the repository root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1-based.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// The node's id (`ws:id` outside the root workspace).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The actors it's about (a lease's holders): names other actors
    /// chose, so data, never part of the message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub holders: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<LocationOut>,
}

impl From<&Diagnostic> for DiagnosticOut {
    fn from(d: &Diagnostic) -> Self {
        Self {
            code: CodeName(d.code),
            severity: d.severity.into(),
            path: d.path.clone(),
            line: d.line,
            node: d.node.as_deref().map(display_ref),
            message: d.message.clone(),
            hint: d.hint.clone(),
            holders: d.holders.clone(),
            related: d
                .related
                .iter()
                .map(|l| LocationOut {
                    path: l.path.clone(),
                    line: l.line,
                })
                .collect(),
        }
    }
}

/// Every command's JSON output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Envelope<T> {
    /// `mdkg.<command>.v<major>`.
    pub schema: String,
    /// False exactly when there is an error diagnostic; the exit code is
    /// non-zero exactly when this is false.
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    pub diagnostics: Vec<DiagnosticOut>,
}

pub fn schema_id(command: &str) -> String {
    format!("mdkg.{command}.v1")
}

/// A reference as agents should write it: the bare id in the root
/// workspace, `ws:id` elsewhere.
pub fn display_ref(qid: &str) -> String {
    qid.strip_prefix("root:").unwrap_or(qid).to_string()
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum FieldValue {
    String(String),
    Bool(bool),
    List(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct Edges {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relates: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

impl Edges {
    fn is_empty(&self) -> bool {
        self.parent.is_none()
            && self.epic.is_none()
            && self.prev.is_none()
            && self.next.is_none()
            && self.relates.is_empty()
            && self.refs.is_empty()
            && self.blocked_by.is_empty()
            && self.blocks.is_empty()
            && self.context_refs.is_empty()
            && self.evidence_refs.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeaseBrief {
    pub id: String,
    pub owner: String,
    pub scope: String,
    pub until: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PendingBrief {
    pub changeset: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Coordinator state for one node.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CoordinationRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseBrief>,
    /// A version of the node sequenced but not yet on main.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingBrief>,
}

impl CoordinationRecord {
    pub fn is_empty(&self) -> bool {
        self.lease.is_none() && self.pending.is_none()
    }
}

/// One node. Only `id` is always present: commands pick fields.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct NodeRecord {
    pub id: String,
    /// The id a migrated node had in 0.6.0 (`task-4`), still a handle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_id: Option<String>,
    /// Present only outside the root workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The effective status: the coordinator's state if it has one,
    /// otherwise the lasting status in the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges: Option<Edges>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// Relative to the repository root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Type-specific frontmatter (goal, loop and archive keys).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordination: Option<CoordinationRecord>,
    /// The markdown after the frontmatter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

pub const NODE_FIELDS: &[&str] = &[
    "id",
    "ws",
    "type",
    "title",
    "status",
    "priority",
    "tags",
    "owners",
    "links",
    "artifacts",
    "aliases",
    "skills",
    "edges",
    "created",
    "updated",
    "path",
    "fields",
    "coordination",
    "body",
    "legacy_id",
];

/// Default compact node fields for `list` and `search` output.
pub const DEFAULT_LIST_FIELDS: &[&str] = &[
    "id",
    "legacy_id",
    "ws",
    "type",
    "title",
    "status",
    "priority",
    "path",
];

fn refs(values: &[String]) -> Vec<String> {
    values.iter().map(|v| display_ref(v)).collect()
}

impl NodeRecord {
    pub fn from_node(node: &IndexNode) -> Self {
        let e = &node.edges;
        let edges = Edges {
            parent: e.parent.as_deref().map(display_ref),
            epic: e.epic.as_deref().map(display_ref),
            prev: e.prev.as_deref().map(display_ref),
            next: e.next.as_deref().map(display_ref),
            relates: refs(&e.relates),
            refs: refs(&node.refs),
            blocked_by: refs(&e.blocked_by),
            blocks: refs(&e.blocks),
            context_refs: refs(&e.context_refs),
            evidence_refs: refs(&e.evidence_refs),
        };
        let fields = node
            .attributes
            .entries()
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    mdkg_format::Value::String(s) => FieldValue::String(s.clone()),
                    mdkg_format::Value::Bool(b) => FieldValue::Bool(*b),
                    mdkg_format::Value::List(items) => FieldValue::List(items.clone()),
                };
                (k.clone(), value)
            })
            .collect();
        let body = node
            .body
            .trim_end_matches(mdkg_format::js::is_js_whitespace)
            .trim_start_matches(['\n', '\r']);
        Self {
            id: node.id.clone(),
            legacy_id: node.legacy_id.clone(),
            ws: (node.ws != "root").then(|| node.ws.clone()),
            node_type: Some(node.node_type.clone()),
            title: Some(node.title.clone()),
            status: node.status.clone(),
            priority: node.priority,
            tags: node.tags.clone(),
            owners: node.owners.clone(),
            links: node.links.clone(),
            artifacts: node.artifacts.clone(),
            aliases: node.aliases.clone(),
            skills: node.skills.clone(),
            edges: (!edges.is_empty()).then_some(edges),
            created: Some(node.created.clone()),
            updated: Some(node.updated.clone()),
            path: Some(node.path.clone()),
            fields,
            coordination: None,
            body: (!body.is_empty()).then(|| body.to_string()),
        }
    }

    /// The record with only `fields` (empty means all). `id` and `ws` are
    /// always kept, so records from different workspaces stay distinct.
    pub fn select(&self, fields: &[String]) -> Value {
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        if fields.is_empty() {
            return value;
        }
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .filter(|(k, _)| k == "id" || k == "ws" || fields.iter().any(|f| f == k))
                    .collect(),
            ),
            other => other,
        }
    }
}

/// Parses `--fields`. `None` means the command's default; `all` means every
/// field.
pub fn parse_fields(raw: Option<&str>, default: &[&str]) -> Result<Vec<String>, GraphError> {
    let Some(raw) = raw else {
        return Ok(default.iter().map(|f| f.to_string()).collect());
    };
    // `refs` is what `new --refs` writes and `show` prints; its field is
    // `edges`, which holds them with the other edge kinds.
    let mut names: Vec<String> = Vec::new();
    for f in raw.split(',').map(|f| f.trim().to_lowercase()) {
        let f = if f == "refs" { "edges".to_string() } else { f };
        if !f.is_empty() && !names.contains(&f) {
            names.push(f);
        }
    }
    if names.is_empty() {
        return Err(
            GraphError::usage(Code::UsageUnknownField, "--fields names no field")
                .with_hint(format!("valid fields: {}, or all", NODE_FIELDS.join(", "))),
        );
    }
    if names.iter().any(|n| n == "all") {
        return Ok(Vec::new());
    }
    if let Some(bad) = names.iter().find(|n| !NODE_FIELDS.contains(&n.as_str())) {
        return Err(
            GraphError::usage(Code::UsageUnknownField, format!("unknown field {bad}")).with_hint(
                format!(
                    "valid fields: {}, or all (refs is edges)",
                    NODE_FIELDS.join(", ")
                ),
            ),
        );
    }
    Ok(names)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListData {
    pub count: usize,
    #[schemars(with = "Vec<NodeRecord>")]
    pub nodes: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowData {
    #[schemars(with = "NodeRecord")]
    pub node: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ValidateData {
    /// Node files checked.
    pub checked: usize,
    pub errors: usize,
    pub warnings: usize,
    /// With `--changed-only`: files changed since the branch left main.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<usize>,
}

/// How a command ended: data, diagnostics, and the error class if it failed.
#[derive(Debug)]
pub struct Report<T> {
    pub data: Option<T>,
    pub diagnostics: Vec<Diagnostic>,
    pub failure: Option<ErrorKind>,
}

impl<T> Report<T> {
    pub fn ok(data: T) -> Self {
        Self {
            data: Some(data),
            diagnostics: Vec::new(),
            failure: None,
        }
    }

    pub fn failed(e: GraphError) -> Self {
        Self {
            data: None,
            diagnostics: vec![*e.diagnostic],
            failure: Some(e.kind),
        }
    }

    pub fn with(mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) -> Self {
        self.diagnostics.extend(diagnostics);
        self
    }

    pub fn exit_code(&self) -> u8 {
        self.failure.map_or(0, ErrorKind::exit_code)
    }
}

/// Notices raised while a command ran, outside its own report (an upgrade
/// the store made when it opened): every command's output carries them.
static NOTICES: std::sync::Mutex<Vec<Diagnostic>> = std::sync::Mutex::new(Vec::new());

/// Adds a notice to this command's output, once (a command that checks
/// its input twice says so once).
pub fn notice(d: Diagnostic) {
    if let Ok(mut notices) = NOTICES.lock()
        && !notices.contains(&d)
    {
        notices.push(d);
    }
}

/// Prints the envelope (JSON) or the diagnostics (text) and returns the
/// exit code. `text` prints the data in text mode.
pub fn emit<T: Serialize>(
    command: &str,
    json: bool,
    report: Report<T>,
    text: impl FnOnce(&T),
) -> u8 {
    let notices = NOTICES
        .lock()
        .map(|mut n| std::mem::take(&mut *n))
        .unwrap_or_default();
    let report = report.with(notices);
    let code = report.exit_code();
    if json {
        let envelope = Envelope {
            schema: schema_id(command),
            ok: report.failure.is_none(),
            data: report.data,
            diagnostics: report.diagnostics.iter().map(DiagnosticOut::from).collect(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).unwrap_or_default()
        );
    } else {
        if let Some(data) = &report.data {
            text(data);
        }
        for d in &report.diagnostics {
            eprint_diagnostic(d);
        }
    }
    code
}

/// Text another actor may have chosen (a path, a name), safe for a
/// terminal: every control character and everything outside ASCII
/// escaped, so no format, bidirectional or invisible character slips
/// through a hand-kept list. Lease scopes and names are ASCII anyway.
pub fn shown(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c.to_string()
            } else {
                c.escape_unicode().to_string()
            }
        })
        .collect()
}

/// A message body for a terminal: lines and tabs kept, every other
/// control, format or bidirectional character escaped, so a body can't
/// hide, erase or rewrite what the reader sees. Other text is kept.
pub fn shown_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let mut buf = [0u8; 4];
        // A line's `\r\n` ending is kept; a lone `\r` would let the rest
        // of the line overwrite its start.
        let kept = c == '\n' || c == '\t' || (c == '\r' && chars.peek() == Some(&'\n'));
        if !kept && crate::coord::hidden_characters(c.encode_utf8(&mut buf)) {
            out.push_str(&c.escape_unicode().to_string());
        } else {
            out.push(c);
        }
    }
    out
}

/// A repository path for a terminal: control, format and bidirectional
/// characters escaped, other text (accented, CJK names) kept readable.
pub fn shown_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut buf = [0u8; 4];
            if crate::coord::hidden_characters(c.encode_utf8(&mut buf)) {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

pub fn eprint_diagnostic(d: &Diagnostic) {
    // As `Diagnostic` displays itself, with the location escaped: a path
    // can come from another actor's lease.
    // Escape the message and hint too: a file name can reach them.
    // Readable text is kept.
    match d.location() {
        Some(loc) => eprintln!(
            "{}: {} {}: {}",
            shown_path(&loc),
            d.severity.as_str(),
            d.code.as_str(),
            shown_path(&d.message)
        ),
        None => eprintln!(
            "{} {}: {}",
            d.severity.as_str(),
            d.code.as_str(),
            shown_path(&d.message)
        ),
    }
    if let Some(hint) = &d.hint {
        eprintln!("  hint: {}", shown_path(hint));
    }
    // Other actors' names, escaped: no control character reaches the
    // terminal.
    for h in &d.holders {
        eprintln!("  holder: {}", shown(h));
    }
}

/// Milliseconds since the Unix epoch as RFC 3339 UTC.
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_303_505_000), "2026-10-18T06:05:05Z");
    }

    #[test]
    fn unknown_fields_are_usage_errors_with_the_valid_list() {
        let e = parse_fields(Some("id,colour"), DEFAULT_LIST_FIELDS).unwrap_err();
        assert_eq!(e.code(), Code::UsageUnknownField);
        assert!(e.diagnostic.hint.unwrap().contains("title"));
        assert!(
            parse_fields(Some("all"), DEFAULT_LIST_FIELDS)
                .unwrap()
                .is_empty()
        );
    }
}
