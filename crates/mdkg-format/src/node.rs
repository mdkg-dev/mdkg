//! Node rules: turning parsed frontmatter into a typed node.
//!
//! The checks run in the same order as mdkg 0.6.0, so the first error a file
//! produces, and its exact text, match the reference implementation.
//! Loops, archives and agent workflow types are parsed generically: their
//! type-specific rules are outside the phase 1 core-node compatibility scope.

use std::collections::HashMap;

use crate::diag::Code;
use crate::error::FormatError;
use crate::frontmatter::{self, Frontmatter, Value};
use crate::id::{NodeId, NodeRef};
use crate::ids;
use crate::js;

pub const WORK_TYPES: &[&str] = &[
    "goal",
    "loop",
    "epic",
    "feat",
    "task",
    "bug",
    "spike",
    "checkpoint",
    "test",
];
pub const DEC_TYPES: &[&str] = &["dec"];
pub const AGENT_FILE_TYPES: &[&str] = &[
    "manifest",
    "spec",
    "work",
    "work_order",
    "receipt",
    "feedback",
    "dispute",
    "proposal",
];
/// All accepted node types, in the order the reference lists them.
pub const ALLOWED_TYPES: &[&str] = &[
    "rule",
    "prd",
    "edd",
    "dec",
    "prop",
    "goal",
    "loop",
    "epic",
    "feat",
    "task",
    "bug",
    "spike",
    "checkpoint",
    "test",
    "archive",
    "manifest",
    "spec",
    "work",
    "work_order",
    "receipt",
    "feedback",
    "dispute",
    "proposal",
];
pub const DEC_STATUS: &[&str] = &["proposed", "accepted", "rejected", "superseded"];
pub const GOAL_STATE: &[&str] = &[
    "active",
    "paused",
    "achieved",
    "blocked",
    "budget_limited",
    "archived",
];
const GOAL_ATTRIBUTE_KEYS: &[&str] = &[
    "goal_state",
    "goal_condition",
    "scope_refs",
    "active_node",
    "last_active_node",
    "required_skills",
    "required_checks",
    "max_iterations",
    "blocked_after_attempts",
];
const LOOP_ATTRIBUTE_KEYS: &[&str] = &[
    "loop_mode",
    "loop_role",
    "scope_refs",
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
];

const MANIFEST_ATTRIBUTE_KEYS: &[&str] = &[
    "version",
    "spec_kind",
    "role",
    "runtime_mode",
    "work_contracts",
    "contract_profile",
    "validation_policy_ref",
    "evidence_policy_ref",
    "requested_capabilities",
    "skill_refs",
    "tool_refs",
    "model_refs",
    "wasm_component_refs",
    "runtime_image_refs",
    "subagent_refs",
    "resource_profile",
    "update_policy",
];

/// Attribute keys extracted for each agent workflow type, in output order.
fn agent_attribute_keys(node_type: &str) -> &'static [&'static str] {
    match node_type {
        "manifest" | "spec" => MANIFEST_ATTRIBUTE_KEYS,
        "work" => &[
            "version",
            "agent_id",
            "kind",
            "contract_profile",
            "required_capabilities",
            "skill_refs",
            "tool_refs",
            "model_refs",
            "wasm_component_refs",
            "runtime_image_refs",
            "subagent_refs",
            "inputs",
            "outputs",
            "receipt_required",
        ],
        "work_order" => &[
            "version",
            "work_id",
            "work_version",
            "requester",
            "order_status",
            "request_ref",
            "trigger_ref",
            "payload_hash",
            "contract_profile",
            "validation_policy_ref",
            "evidence_policy_ref",
            "input_refs",
            "queue_refs",
            "requested_outputs",
            "constraint_refs",
            "artifact_policy",
        ],
        "receipt" => &[
            "version",
            "work_order_id",
            "receipt_status",
            "outcome",
            "cost_ref",
            "redaction_policy",
            "contract_profile",
            "receipt_kind",
            "redaction_class",
            "validation_policy_ref",
            "evidence_policy_ref",
            "proof_refs",
            "attestation_refs",
            "evidence_hashes",
            "input_hashes",
            "output_hashes",
        ],
        "feedback" => &[
            "version",
            "target_id",
            "sentiment",
            "feedback_status",
            "source_ref",
        ],
        "dispute" => &[
            "version",
            "work_order_id",
            "receipt_id",
            "dispute_status",
            "severity",
        ],
        "proposal" => &[
            "version",
            "target_id",
            "proposal_status",
            "proposal_kind",
            "evidence_refs",
        ],
        _ => &[],
    }
}

const ARCHIVE_ATTRIBUTE_KEYS: &[&str] = &[
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
];

/// Legacy extension keys, accepted by mdkg even when a template does
/// not list them, preserving extensions that 0.6.0 would reject.
/// Format 0.7's namespaced keys follow [`frontmatter::is_extension_key`].
pub const EXTENSION_KEY_PREFIX: &str = "x_";

/// Whether a key is an extension key, in either form.
pub fn is_any_extension_key(key: &str) -> bool {
    key.starts_with(EXTENSION_KEY_PREFIX) || frontmatter::is_extension_key(key)
}

/// Which format a graph's files are in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FormatVersion {
    /// mdkg 0.6.0's: sequential and portable ids, workspace-qualified refs,
    /// date-only timestamps.
    #[default]
    Legacy,
    /// Format 0.7 (`docs/format-0.7.md`): UUIDv7 ids and refs, RFC 3339
    /// timestamps, `legacy_id`.
    V07,
}

fn is_work_type(t: &str) -> bool {
    WORK_TYPES.contains(&t)
}

fn is_dec_type(t: &str) -> bool {
    DEC_TYPES.contains(&t)
}

pub fn is_agent_type(t: &str) -> bool {
    AGENT_FILE_TYPES.contains(&t)
}

/// The kind of value a template key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Scalar,
    List,
    Boolean,
}

impl KeyKind {
    fn of(value: &Value) -> Self {
        match value {
            Value::String(_) => KeyKind::Scalar,
            Value::List(_) => KeyKind::List,
            Value::Bool(_) => KeyKind::Boolean,
        }
    }
}

/// The keys a node type may carry, read from its template's frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSchema {
    pub node_type: String,
    keys: Vec<(String, KeyKind)>,
}

impl TemplateSchema {
    pub fn from_frontmatter(node_type: impl Into<String>, frontmatter: &Frontmatter) -> Self {
        Self {
            node_type: node_type.into(),
            keys: frontmatter
                .entries()
                .iter()
                .map(|(k, v)| (k.clone(), KeyKind::of(v)))
                .collect(),
        }
    }

    /// A schema from its keys, in order.
    pub fn from_keys(node_type: impl Into<String>, keys: Vec<(String, KeyKind)>) -> Self {
        Self {
            node_type: node_type.into(),
            keys,
        }
    }

    pub fn allows(&self, key: &str) -> bool {
        self.keys.iter().any(|(k, _)| k == key)
    }

    pub fn kind(&self, key: &str) -> Option<KeyKind> {
        self.keys
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, kind)| *kind)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(k, _)| k.as_str())
    }

    /// This schema plus any keys only `other` has.
    pub fn merged_with(&self, other: &TemplateSchema) -> TemplateSchema {
        let mut keys = self.keys.clone();
        for (key, kind) in &other.keys {
            if !keys.iter().any(|(k, _)| k == key) {
                keys.push((key.clone(), *kind));
            }
        }
        TemplateSchema {
            node_type: self.node_type.clone(),
            keys,
        }
    }

    /// The same keys under another type name.
    pub fn renamed(&self, node_type: impl Into<String>) -> TemplateSchema {
        TemplateSchema {
            node_type: node_type.into(),
            keys: self.keys.clone(),
        }
    }
}

/// Keys some agent workflow types accept even when their template omits them.
fn optional_compat_kind(node_type: &str, key: &str) -> Option<KeyKind> {
    let scalar = Some(KeyKind::Scalar);
    match (node_type, key) {
        (
            "manifest",
            "contract_profile" | "validation_policy_ref" | "evidence_policy_ref" | "profile",
        )
        | (
            "spec",
            "spec_kind"
            | "contract_profile"
            | "validation_policy_ref"
            | "evidence_policy_ref"
            | "profile",
        )
        | ("work", "contract_profile" | "profile")
        | (
            "work_order",
            "contract_profile" | "validation_policy_ref" | "evidence_policy_ref" | "profile",
        )
        | (
            "receipt",
            "contract_profile"
            | "receipt_kind"
            | "redaction_class"
            | "validation_policy_ref"
            | "evidence_policy_ref"
            | "profile",
        ) => scalar,
        _ => None,
    }
}

/// Inputs that come from the graph's config and templates.
#[derive(Debug, Clone)]
pub struct ParseOptions<'a> {
    pub work_status: &'a [String],
    pub priority_min: i64,
    pub priority_max: i64,
    pub templates: &'a HashMap<String, TemplateSchema>,
    /// Accept extension keys (`x-<ns>.<field>`, and legacy `x_`) that
    /// templates do not list.
    pub allow_extension_keys: bool,
    pub format: FormatVersion,
}

/// Graph edges as written in the file (not yet qualified with a workspace).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeMap {
    pub epic: Option<String>,
    pub parent: Option<String>,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub relates: Vec<String>,
    pub blocked_by: Vec<String>,
    pub blocks: Vec<String>,
    pub context_refs: Vec<String>,
    pub evidence_refs: Vec<String>,
}

/// A node's v2 identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    pub graph_id: String,
    pub node_id: String,
}

/// A parsed, validated node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    /// Format 0.7: the 0.6.0 id a migrated node had.
    pub legacy_id: Option<String>,
    pub identity: Option<NodeIdentity>,
    pub node_type: String,
    pub title: String,
    pub created: String,
    pub updated: String,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub tags: Vec<String>,
    pub owners: Vec<String>,
    pub links: Vec<String>,
    pub artifacts: Vec<String>,
    pub refs: Vec<String>,
    pub aliases: Vec<String>,
    pub skills: Vec<String>,
    /// `scope`, as written.
    pub scope: Vec<String>,
    pub edges: EdgeMap,
    pub attributes: Frontmatter,
    pub body: String,
    pub frontmatter: Frontmatter,
}

struct Ctx<'a> {
    fm: &'a Frontmatter,
    path: &'a str,
    v07: bool,
    lex: frontmatter::Lexical,
}

impl Ctx<'_> {
    fn err(&self, code: Code, message: impl std::fmt::Display) -> FormatError {
        FormatError::in_file(self.path, code, message)
    }

    fn expect_string(&self, key: &str) -> Result<String, FormatError> {
        match self.fm.get(key) {
            Some(Value::String(s)) if !self.lex.trim(s).is_empty() => Ok(s.clone()),
            Some(Value::String(_)) => {
                Err(self.err(Code::NodeEmptyField, format!("{key} must not be empty")))
            }
            _ => Err(self.err(Code::NodeWrongKind, format!("{key} must be a string"))),
        }
    }

    fn optional_string(&self, key: &str) -> Result<Option<String>, FormatError> {
        match self.fm.get(key) {
            None => Ok(None),
            Some(_) => self.expect_string(key).map(Some),
        }
    }

    fn optional_list(&self, key: &str) -> Result<Vec<String>, FormatError> {
        match self.fm.get(key) {
            None => Ok(Vec::new()),
            Some(Value::List(items)) => Ok(items.clone()),
            Some(_) => Err(self.err(Code::NodeWrongKind, format!("{key} must be a list"))),
        }
    }

    fn require_lowercase(&self, value: String, key: &str) -> Result<String, FormatError> {
        if js::is_lowercase(&value) {
            Ok(value)
        } else {
            Err(self.err(Code::NodeNotLowercase, format!("{key} must be lowercase")))
        }
    }

    fn require_lowercase_list(
        &self,
        values: Vec<String>,
        key: &str,
    ) -> Result<Vec<String>, FormatError> {
        for (index, value) in values.iter().enumerate() {
            if !js::is_lowercase(value) {
                return Err(self.err(
                    Code::NodeNotLowercase,
                    format!("{key}[{index}] must be lowercase"),
                ));
            }
        }
        Ok(values)
    }

    fn require_date(&self, value: String, key: &str) -> Result<String, FormatError> {
        // A format 0.7 date names a day that exists (section 3.4); 0.6.0
        // checks only the digits.
        let date_ok = if self.v07 {
            is_calendar_date(&value)
        } else {
            is_date(&value)
        };
        if date_ok || (self.v07 && is_timestamp(&value)) {
            Ok(value)
        } else if self.v07 {
            Err(self.err(
                Code::NodeBadDate,
                format!("{key} must be YYYY-MM-DDTHH:MM:SS.mmmZ (or YYYY-MM-DD)"),
            ))
        } else {
            Err(self.err(Code::NodeBadDate, format!("{key} must be YYYY-MM-DD")))
        }
    }

    fn normalize_skills(&self, values: &[String]) -> Result<Vec<String>, FormatError> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let normalized = value.to_lowercase();
                if is_kebab(&normalized) {
                    Ok(normalized)
                } else {
                    Err(self.err(
                        Code::NodeBadSkillSlug,
                        format!("skills[{index}] must be kebab-case"),
                    ))
                }
            })
            .collect()
    }
}

/// `-?[0-9]+` within i64: format 0.7's integer.
pub fn plain_int(value: &str) -> Option<i64> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// `YYYY-MM-DD`, digits only, as 0.6.0 checks it.
pub fn is_date(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// `YYYY-MM-DD` naming a day that exists: month 01-12, a day in that month.
pub fn is_calendar_date(value: &str) -> bool {
    if !is_date(value) {
        return false;
    }
    let num = |r: std::ops::Range<usize>| value[r].parse::<u32>().unwrap_or(u32::MAX);
    let (year, month, day) = (num(0..4), num(5..7), num(8..10));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

/// A format 0.7 `created` or `updated` as a comparable instant: a
/// timestamp as written, a date as its first millisecond, UTC.
fn instant(value: &str) -> String {
    if value.len() == 10 {
        format!("{value}T00:00:00.000Z")
    } else {
        value.to_string()
    }
}

/// RFC 3339 in UTC to the millisecond, `YYYY-MM-DDTHH:MM:SS.mmmZ`, with each
/// field in range (format 0.7, section 3.4). Leap seconds (`:60`) and days
/// past a month's end are refused.
pub fn is_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    // ASCII first, so the slices below fall on character boundaries.
    if b.len() != 24 || !b.is_ascii() || !is_calendar_date(&value[..10]) {
        return false;
    }
    let shape = b.iter().enumerate().all(|(i, &c)| match i {
        10 => c == b'T',
        13 | 16 => c == b':',
        19 => c == b'.',
        23 => c == b'Z',
        0..=9 => true,
        _ => c.is_ascii_digit(),
    });
    if !shape {
        return false;
    }
    let num = |r: std::ops::Range<usize>| value[r].parse::<u32>().unwrap_or(u32::MAX);
    num(11..13) < 24 && num(14..16) < 60 && num(17..19) < 60
}

/// `^[a-z0-9]+(?:-[a-z0-9]+)*$`.
fn is_kebab(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty() && part.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9'))
        })
}

/// `^[1-9][0-9]*$`.
fn is_positive_integer(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('1'..='9')) && chars.all(|c| c.is_ascii_digit())
}

fn validate_template_keys(
    ctx: &Ctx<'_>,
    schema: &TemplateSchema,
    allow_extension_keys: bool,
) -> Result<(), FormatError> {
    for key in ctx.fm.keys() {
        let known = schema.allows(key)
            || key == "graph_id"
            || key == "node_id"
            || (ctx.v07 && key == "legacy_id")
            || optional_compat_kind(&schema.node_type, key).is_some()
            || (allow_extension_keys && is_any_extension_key(key));
        if !known {
            return Err(ctx.err(Code::NodeUnknownKey, format!("unknown key: {key}")));
        }
    }
    // An extension can't be both a value and a namespace:
    // `x-acme.billing` can't sit beside `x-acme.billing.plan`.
    for key in ctx.fm.keys().filter(|k| frontmatter::is_extension_key(k)) {
        let prefix = format!("{key}.");
        if ctx.fm.keys().any(|other| other.starts_with(&prefix)) {
            return Err(ctx.err(
                Code::NodeExtensionKeyConflict,
                format!("{key} is both a value and the prefix of another key"),
            ));
        }
    }
    for (key, value) in ctx.fm.entries() {
        let expected = if key == "graph_id" || key == "node_id" || key == "legacy_id" {
            Some(KeyKind::Scalar)
        } else {
            schema
                .kind(key)
                .or_else(|| optional_compat_kind(&schema.node_type, key))
        };
        let Some(expected) = expected else { continue };
        let actual = KeyKind::of(value);
        if expected != actual {
            let message = match expected {
                KeyKind::List => "must be a list",
                KeyKind::Boolean => "must be a boolean",
                KeyKind::Scalar => "must be a string",
            };
            return Err(ctx.err(Code::NodeWrongKind, format!("{key} {message}")));
        }
    }
    Ok(())
}

fn read_identity(ctx: &Ctx<'_>) -> Result<Option<NodeIdentity>, FormatError> {
    let graph_id = ctx.fm.get("graph_id");
    let node_id = ctx.fm.get("node_id");
    if graph_id.is_none() && node_id.is_none() {
        return Ok(None);
    }
    let uuid = |value: Option<&Value>, key: &str| match value.and_then(Value::as_str) {
        Some(s) if ids::is_identity_uuid(s) => Ok(s.to_string()),
        _ => Err(FormatError::in_file(
            ctx.path,
            Code::NodeBadIdentity,
            format!("{key} must be a lowercase UUID"),
        )
        .into_usage()),
    };
    Ok(Some(NodeIdentity {
        graph_id: uuid(graph_id, "graph_id")?,
        node_id: uuid(node_id, "node_id")?,
    }))
}

fn validate_goal(ctx: &Ctx<'_>) -> Result<(), FormatError> {
    let state = ctx.require_lowercase(ctx.expect_string("goal_state")?, "goal_state")?;
    if !GOAL_STATE.contains(&state.as_str()) {
        return Err(ctx.err(
            Code::GoalBadState,
            format!("goal_state must be one of {}", GOAL_STATE.join(", ")),
        ));
    }
    let condition = ctx.expect_string("goal_condition")?;
    if condition.encode_utf16().count() > 4000 {
        return Err(ctx.err(
            Code::GoalConditionTooLong,
            "goal_condition must be 4000 characters or fewer",
        ));
    }
    for key in ["active_node", "last_active_node"] {
        if let Some(value) = ctx.optional_string(key)? {
            let normalized = ctx.require_lowercase(value, key)?;
            if ctx.v07 {
                if !is_unpinned_ref(&normalized) {
                    return Err(ctx.err(
                        Code::NodeBadRef,
                        format!("{key} must be an unpinned node ref"),
                    ));
                }
            } else if !ids::is_portable_id_ref(&normalized) {
                return Err(ctx.err(Code::NodeBadRef, format!("{key} must be a local id or qid")));
            }
        }
    }
    for (index, value) in ctx.optional_list("scope_refs")?.into_iter().enumerate() {
        if ctx.lex.trim(&value).is_empty() {
            return Err(ctx.err(
                Code::GoalBadScopeRef,
                format!("scope_refs[{index}] must not be empty"),
            ));
        }
        let label = format!("scope_refs[{index}]");
        let normalized = ctx.require_lowercase(value, &label)?;
        if ctx.v07 {
            if !is_unpinned_ref(&normalized) {
                return Err(ctx.err(
                    Code::GoalBadScopeRef,
                    format!("scope_refs[{index}] must be an unpinned node ref"),
                ));
            }
        } else if !ids::is_portable_id_ref(&normalized) {
            return Err(ctx.err(
                Code::GoalBadScopeRef,
                format!("scope_refs[{index}] must be a local id or qid"),
            ));
        }
    }
    ctx.normalize_skills(&ctx.optional_list("required_skills")?)?;
    for (index, value) in ctx.optional_list("required_checks")?.iter().enumerate() {
        if ctx.lex.trim(value).is_empty() {
            return Err(ctx.err(
                Code::GoalEmptyCheck,
                format!("required_checks[{index}] must not be empty"),
            ));
        }
    }
    for key in ["max_iterations", "blocked_after_attempts"] {
        if let Some(value) = ctx.optional_string(key)?
            && !is_positive_integer(&value)
        {
            return Err(ctx.err(
                Code::NodeBadInteger,
                format!("{key} must be a positive integer"),
            ));
        }
    }
    Ok(())
}

fn pick(fm: &Frontmatter, keys: &[&str], into: &mut Frontmatter) {
    for key in keys {
        if let Some(value) = fm.get(key) {
            into.insert(*key, value.clone());
        }
    }
}

fn edge_ref(ctx: &Ctx<'_>, value: &str, key: &str) -> Result<String, FormatError> {
    let normalized = value.to_lowercase();
    if normalized != value {
        return Err(ctx.err(Code::NodeNotLowercase, format!("{key} must be lowercase")));
    }
    if ctx.v07 {
        if NodeId::parse(&normalized).is_err() {
            return Err(ctx.err(Code::NodeBadRef, format!("{key} must be a node id")));
        }
    } else if !ids::is_portable_id_ref(&normalized) {
        return Err(ctx.err(Code::NodeBadRef, format!("{key} invalid id reference")));
    }
    Ok(normalized)
}

/// A node id or graph ref with no pin: format 0.7's goal refs.
fn is_unpinned_ref(value: &str) -> bool {
    NodeRef::parse(value).is_ok_and(|r| r.pin.is_none())
}

/// Format 0.7's `refs`, `context_refs` and `evidence_refs`: a node ref, or
/// a URI that isn't `mdkg:`.
pub fn is_v07_semantic_ref(value: &str) -> bool {
    if NodeRef::parse(value).is_ok() {
        return true;
    }
    if ids::has_mdkg_scheme(value) || !ids::is_uri_ref(value) {
        return false;
    }
    !value.starts_with("archive://") || ids::archive_id_from_uri(value).is_some()
}

fn semantic_ref(ctx: &Ctx<'_>, value: &str, key: &str) -> Result<String, FormatError> {
    if ctx.v07 {
        if !is_v07_semantic_ref(value) {
            return Err(ctx.err(
                Code::NodeBadRef,
                format!("{key} invalid semantic reference"),
            ));
        }
        return Ok(value.to_string());
    }
    if !ids::validate_portable_or_uri_ref(value) {
        return Err(ctx.err(
            Code::NodeBadRef,
            format!("{key} invalid semantic reference"),
        ));
    }
    Ok(if value.contains("://") {
        value.to_string()
    } else {
        value.to_lowercase()
    })
}

fn extract_edges(ctx: &Ctx<'_>, include_semantic: bool) -> Result<EdgeMap, FormatError> {
    let string = |key: &str| -> Result<Option<String>, FormatError> {
        match ctx.fm.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(FormatError::bare(
                Code::NodeWrongKind,
                format!("{key} must be a string"),
            )),
        }
    };
    let list = |key: &str| -> Result<Vec<String>, FormatError> {
        match ctx.fm.get(key) {
            None => Ok(Vec::new()),
            Some(Value::List(items)) => Ok(items.clone()),
            Some(_) => Err(FormatError::bare(
                Code::NodeWrongKind,
                format!("{key} must be a list"),
            )),
        }
    };
    let (epic, parent, prev, next) = (
        string("epic")?,
        string("parent")?,
        string("prev")?,
        string("next")?,
    );
    let relates = list("relates")?;
    let blocked_by = list("blocked_by")?;
    let blocks = list("blocks")?;
    let context_refs = if include_semantic {
        list("context_refs")?
    } else {
        Vec::new()
    };
    let evidence_refs = if include_semantic {
        list("evidence_refs")?
    } else {
        Vec::new()
    };

    let id_list = |values: Vec<String>, key: &str| -> Result<Vec<String>, FormatError> {
        values.iter().map(|v| edge_ref(ctx, v, key)).collect()
    };
    let semantic_list = |values: Vec<String>, key: &str| -> Result<Vec<String>, FormatError> {
        values.iter().map(|v| semantic_ref(ctx, v, key)).collect()
    };
    let mut edges = EdgeMap {
        relates: id_list(relates, "relates")?,
        blocked_by: id_list(blocked_by, "blocked_by")?,
        blocks: id_list(blocks, "blocks")?,
        context_refs: semantic_list(context_refs, "context_refs")?,
        evidence_refs: semantic_list(evidence_refs, "evidence_refs")?,
        ..EdgeMap::default()
    };
    // An empty scalar can't come out of the grammar, so presence is enough.
    edges.epic = epic.map(|v| edge_ref(ctx, &v, "epic")).transpose()?;
    edges.parent = parent.map(|v| edge_ref(ctx, &v, "parent")).transpose()?;
    edges.prev = prev.map(|v| edge_ref(ctx, &v, "prev")).transpose()?;
    edges.next = next.map(|v| edge_ref(ctx, &v, "next")).transpose()?;
    Ok(edges)
}

/// Parses and validates one node file.
pub fn parse_node(content: &str, path: &str, opts: &ParseOptions<'_>) -> Result<Node, FormatError> {
    let v07 = opts.format == FormatVersion::V07;
    let lex = if v07 {
        frontmatter::Lexical::Ascii
    } else {
        frontmatter::Lexical::Js
    };
    let parsed = frontmatter::parse_with(content, path, lex)?;
    let fm = &parsed.frontmatter;
    let ctx = Ctx { fm, path, v07, lex };

    let node_type = ctx.require_lowercase(ctx.expect_string("type")?, "type")?;
    if !ALLOWED_TYPES.contains(&node_type.as_str()) {
        return Err(ctx.err(
            Code::NodeUnknownType,
            format!("type must be one of {}", ALLOWED_TYPES.join(", ")),
        ));
    }
    let portable = is_agent_type(&node_type) || node_type == "archive";
    let schema = opts.templates.get(&node_type).ok_or_else(|| {
        ctx.err(
            Code::TemplateMissing,
            format!("template schema missing for type {node_type}"),
        )
    })?;
    validate_template_keys(&ctx, schema, opts.allow_extension_keys)?;
    let identity = read_identity(&ctx)?;
    if node_type == "goal" {
        validate_goal(&ctx)?;
    }

    let id = ctx.require_lowercase(ctx.expect_string("id")?, "id")?;
    if v07 {
        if NodeId::parse(&id).is_err() {
            return Err(ctx.err(Code::NodeBadId, "id must be <type>-<uuidv7>"));
        }
    } else if portable {
        if !ids::is_portable_id(&id) {
            return Err(ctx.err(Code::NodeBadId, "id must be a lowercase portable id"));
        }
    } else if !ids::is_canonical_id(&id) {
        return Err(ctx.err(
            Code::NodeBadId,
            "id must match <prefix>-<number> or reserved id",
        ));
    }

    let legacy_id = match ctx.optional_string("legacy_id")? {
        Some(value) => {
            let value = ctx.require_lowercase(value, "legacy_id")?;
            if !ids::is_portable_id(&value) {
                return Err(ctx.err(Code::NodeBadLegacyId, "legacy_id must be an mdkg 0.6.0 id"));
            }
            Some(value)
        }
        None => None,
    };

    let title = ctx.expect_string("title")?;
    let created = ctx.require_date(ctx.expect_string("created")?, "created")?;
    let updated = ctx.require_date(ctx.expect_string("updated")?, "updated")?;
    if v07 && instant(&updated) < instant(&created) {
        return Err(ctx.err(
            Code::NodeBadDate,
            "updated must not be earlier than created",
        ));
    }

    let status_value = ctx.optional_string("status")?;
    let status = if is_work_type(&node_type) {
        let Some(value) = status_value else {
            return Err(ctx.err(Code::NodeMissingStatus, "status is required for work items"));
        };
        let normalized = ctx.require_lowercase(value, "status")?;
        let mut allowed: Vec<String> = opts.work_status.iter().map(|s| s.to_lowercase()).collect();
        if node_type == "goal" && !allowed.iter().any(|s| s == "archived") {
            allowed.push("archived".to_string());
        }
        if !allowed.contains(&normalized) {
            return Err(ctx.err(
                Code::NodeBadStatus,
                format!("status must be one of {}", allowed.join(", ")),
            ));
        }
        Some(normalized)
    } else if is_dec_type(&node_type) {
        let Some(value) = status_value else {
            return Err(ctx.err(
                Code::NodeMissingStatus,
                "status is required for decision records",
            ));
        };
        let normalized = ctx.require_lowercase(value, "status")?;
        if !DEC_STATUS.contains(&normalized.as_str()) {
            return Err(ctx.err(
                Code::NodeBadStatus,
                format!("status must be one of {}", DEC_STATUS.join(", ")),
            ));
        }
        Some(normalized)
    } else if status_value.is_some() {
        return Err(ctx.err(
            Code::NodeStatusNotAllowed,
            "status is not allowed for this type",
        ));
    } else {
        None
    };

    let priority = match ctx.optional_string("priority")? {
        None => None,
        Some(value) => {
            if !is_work_type(&node_type) {
                return Err(ctx.err(
                    Code::NodePriorityNotAllowed,
                    "priority is only allowed for work items",
                ));
            }
            // 0.6.0 reads a priority as `parseInt` does (`3.7` is 3); a
            // format 0.7 priority is a plain integer.
            let parsed = if v07 {
                plain_int(&value)
            } else {
                js::parse_int(&value)
            }
            .ok_or_else(|| ctx.err(Code::NodeBadPriority, "priority must be an integer"))?;
            if parsed < opts.priority_min || parsed > opts.priority_max {
                return Err(ctx.err(
                    Code::NodeBadPriority,
                    format!(
                        "priority must be between {} and {}",
                        opts.priority_min, opts.priority_max
                    ),
                ));
            }
            Some(parsed)
        }
    };

    let tags = ctx.require_lowercase_list(ctx.optional_list("tags")?, "tags")?;
    let owners = ctx.require_lowercase_list(ctx.optional_list("owners")?, "owners")?;
    let links = ctx.optional_list("links")?;
    let artifacts = ctx.optional_list("artifacts")?;
    let refs = ctx.optional_list("refs")?;
    let valid_ref = |r: &String| {
        if v07 {
            is_v07_semantic_ref(r)
        } else {
            ids::validate_portable_or_uri_ref(r)
        }
    };
    if !refs.iter().all(valid_ref) {
        return Err(ctx.err(
            Code::NodeBadRef,
            "refs entries must be portable ids or URI refs",
        ));
    }
    let aliases = ctx.require_lowercase_list(ctx.optional_list("aliases")?, "aliases")?;
    let skills = ctx.normalize_skills(&ctx.optional_list("skills")?)?;
    if !skills.is_empty() && !is_work_type(&node_type) {
        return Err(ctx.err(
            Code::NodeSkillsNotAllowed,
            "skills is only allowed for work items",
        ));
    }
    let scope = ctx.optional_list("scope")?;
    for value in scope.clone() {
        if !js::is_lowercase(&value) {
            return Err(ctx.err(Code::NodeBadScope, "scope entries must be lowercase"));
        }
        if v07 {
            if NodeId::parse(&value).is_err() {
                return Err(ctx.err(Code::NodeBadScope, "scope entries must be node ids"));
            }
        } else if ids::parse_identity_ref(&value).is_none() && !ids::is_canonical_id(&value) {
            return Err(ctx.err(
                Code::NodeBadScope,
                "scope entries must match <prefix>-<number> or reserved id",
            ));
        }
    }
    let supersedes = ctx.optional_string("supersedes")?;
    if let Some(value) = &supersedes {
        if !is_dec_type(&node_type) {
            return Err(ctx.err(
                Code::NodeSupersedesNotAllowed,
                "supersedes is only allowed for decision records",
            ));
        }
        let normalized = ctx.require_lowercase(value.clone(), "supersedes")?;
        if v07 && !NodeId::parse(&normalized).is_ok_and(|id| id.node_type() == "dec") {
            return Err(ctx.err(Code::NodeBadSupersedes, "supersedes must be a dec id"));
        }
        let is_dec_id = !v07
            && normalized
                .strip_prefix("dec-")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !v07 && !is_dec_id && ids::parse_identity_ref(&normalized).is_none() {
            return Err(ctx.err(Code::NodeBadSupersedes, "supersedes must be a dec-# id"));
        }
    }

    let edges = extract_edges(&ctx, is_work_type(&node_type))?;

    let mut attributes = Frontmatter::default();
    if let Some(value) = supersedes {
        attributes.insert("supersedes", Value::String(value));
    }
    if node_type == "goal" {
        pick(fm, GOAL_ATTRIBUTE_KEYS, &mut attributes);
    }
    if node_type == "loop" {
        pick(fm, LOOP_ATTRIBUTE_KEYS, &mut attributes);
    }
    pick(fm, agent_attribute_keys(&node_type), &mut attributes);
    if node_type == "archive" {
        pick(fm, ARCHIVE_ATTRIBUTE_KEYS, &mut attributes);
    }

    Ok(Node {
        id,
        legacy_id,
        identity,
        node_type,
        title,
        created,
        updated,
        status,
        priority,
        tags,
        owners,
        links,
        artifacts,
        refs,
        aliases,
        skills,
        scope,
        edges,
        attributes,
        body: parsed.body.clone(),
        frontmatter: parsed.frontmatter.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn templates() -> HashMap<String, TemplateSchema> {
        let task = frontmatter::parse(
            "---\nid: x\ntype: task\ntitle: x\nstatus: x\npriority: x\nparent: x\ntags: []\nrefs: []\nskills: []\nrelates: []\ncreated: x\nupdated: x\n---\n",
            "t.md",
        )
        .unwrap();
        HashMap::from([(
            "task".to_string(),
            TemplateSchema::from_frontmatter("task", &task.frontmatter),
        )])
    }

    fn parse_as(content: &str, format: FormatVersion) -> Result<Node, String> {
        let templates = templates();
        let status = vec!["todo".to_string(), "done".to_string()];
        let opts = ParseOptions {
            work_status: &status,
            priority_min: 0,
            priority_max: 9,
            templates: &templates,
            allow_extension_keys: true,
            format,
        };
        parse_node(content, "f.md", &opts).map_err(|e| e.to_string())
    }

    fn parse(content: &str) -> Result<Node, String> {
        parse_as(content, FormatVersion::Legacy)
    }

    fn v07(content: &str) -> Result<Node, String> {
        parse_as(content, FormatVersion::V07)
    }

    const ID7: &str = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    const GOAL7: &str = "goal-0192f3a4-0000-7000-8000-000000000001";

    fn head7(extra: &str) -> String {
        format!(
            "---\nid: {ID7}\ntype: task\ntitle: T\nstatus: todo\ncreated: 2026-10-24T18:03:11.482Z\nupdated: 2026-10-24T18:03:11.482Z\n{extra}---\nbody\n"
        )
    }

    #[test]
    fn parses_a_v07_task() {
        let node = v07(&head7(&format!(
            "legacy_id: task-19\nparent: {GOAL7}\nrelates: [{GOAL7}]\nrefs: [mdkg://0192f3a0-0000-7000-8000-000000000000/{GOAL7}@sha256:{}, https://x.dev/a]\nx-acme.owner: a\nx-my-co.billing.plan: pro\nx_legacy: b\n",
            "a".repeat(64)
        )))
        .unwrap();
        assert_eq!(node.id, ID7);
        assert_eq!(node.legacy_id.as_deref(), Some("task-19"));
        assert_eq!(node.edges.parent.as_deref(), Some(GOAL7));
        assert_eq!(node.created, "2026-10-24T18:03:11.482Z");
        // Date-only timestamps still read (validate warns).
        let dated = head7("").replace("2026-10-24T18:03:11.482Z", "2026-10-24");
        assert!(v07(&dated).is_ok());
    }

    #[test]
    fn v07_rejects_legacy_forms() {
        let legacy_id = head7("").replace(ID7, "task-1");
        assert_eq!(
            v07(&legacy_id).unwrap_err(),
            "f.md: id must be <type>-<uuidv7>"
        );
        assert_eq!(
            v07(&head7("parent: goal-1\n")).unwrap_err(),
            "f.md: parent must be a node id"
        );
        assert_eq!(
            v07(&head7("relates: [root:goal-1]\n")).unwrap_err(),
            "f.md: relates must be a node id"
        );
        assert_eq!(
            v07(&head7("refs: [goal-1]\n")).unwrap_err(),
            "f.md: refs entries must be portable ids or URI refs"
        );
        // A graph ref in an edge key, and mdkg: that isn't a graph ref.
        let graph_ref = format!("mdkg://0192f3a0-0000-7000-8000-000000000000/{GOAL7}");
        assert!(v07(&head7(&format!("parent: {graph_ref}\n"))).is_err());
        assert!(v07(&head7("refs: [mdkg://garbage]\n")).is_err());
        assert!(v07(&head7("refs: [MDKG://x/y]\n")).is_err());
        assert_eq!(
            v07(&head7("legacy_id: Task-19\n")).unwrap_err(),
            "f.md: legacy_id must be lowercase"
        );
        assert_eq!(
            v07(&head7("legacy_id: root:task-19\n")).unwrap_err(),
            "f.md: legacy_id must be an mdkg 0.6.0 id"
        );
        for bad in [
            "2026-10-24T18:03:11Z",
            "2026-10-24T18:03:11.482+00:00",
            "2026-02-30T00:00:00.000Z",
            "2026-10-24T24:00:00.000Z",
            "2026-10-24T23:59:60.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-10-24 18:03:11.482Z",
        ] {
            let text = head7("").replacen("2026-10-24T18:03:11.482Z", bad, 1);
            assert!(v07(&text).unwrap_err().contains("created must be"), "{bad}");
        }
        assert!(is_timestamp("2024-02-29T23:59:59.999Z"));
        // Multi-byte characters never split a slice (review of d7dacec).
        assert!(!is_timestamp("2026-10-2é00:00:00.000Z"));
        let text = head7("").replacen("2026-10-24T18:03:11.482Z", "2026-10-2é00:00:00.000Z", 1);
        assert!(v07(&text).unwrap_err().contains("created must be"));
        // Goal refs take no pins.
        let goal = format!(
            "---\nid: {GOAL7}\ntype: goal\ntitle: G\nstatus: todo\ngoal_state: active\ngoal_condition: c\nscope_refs: [{ID7}@sha256:{}]\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n",
            "a".repeat(64)
        );
        assert!(parse_as_goal(&goal).unwrap_err().contains("unpinned"));
        let active = goal
            .replace("scope_refs: [", "active_node: ")
            .replace("]\ncreated", "\ncreated");
        assert!(parse_as_goal(&active).unwrap_err().contains("unpinned"));
        assert!(!is_timestamp("2100-02-29T00:00:00.000Z"));
    }

    fn parse_as_goal(content: &str) -> Result<Node, String> {
        let goal = frontmatter::parse(
            "---\nid: x\ntype: goal\ntitle: x\nstatus: x\ngoal_state: x\ngoal_condition: x\nscope_refs: []\nactive_node: x\ncreated: x\nupdated: x\n---\n",
            "t.md",
        )
        .unwrap();
        let templates = HashMap::from([(
            "goal".to_string(),
            TemplateSchema::from_frontmatter("goal", &goal.frontmatter),
        )]);
        let status = vec!["todo".to_string()];
        let opts = ParseOptions {
            work_status: &status,
            priority_min: 0,
            priority_max: 9,
            templates: &templates,
            allow_extension_keys: true,
            format: FormatVersion::V07,
        };
        parse_node(content, "f.md", &opts).map_err(|e| e.to_string())
    }

    proptest::proptest! {
        #[test]
        fn timestamps_never_panic(s in "\\PC{0,30}") {
            let _ = is_timestamp(&s);
            let _ = is_date(&s);
        }
    }

    #[test]
    fn v07_files_are_read_strictly() {
        // Space and tab are the only whitespace trimmed.
        let bom = format!("\u{FEFF}{}", head7(""));
        assert_eq!(
            v07(&bom).unwrap_err(),
            "f.md:1: frontmatter must start with ---"
        );
        let nbsp_key = head7("tags\u{A0}: [a]\n");
        assert!(v07(&nbsp_key).unwrap_err().contains("invalid key"));
        let nbsp_title = head7("").replace("title: T", "title: \u{A0}");
        assert_eq!(v07(&nbsp_title).unwrap().title, "\u{A0}");
        let tab = head7("").replace("title: T", "title:\tT\t");
        assert_eq!(v07(&tab).unwrap().title, "T");
        // A priority is a plain integer.
        for bad in ["3.7", "2junk", "+3", " 3x", "1e1", ""] {
            let text = head7(&format!("priority: {bad}\n"));
            assert!(v07(&text).is_err(), "{bad}");
        }
        assert_eq!(v07(&head7("priority: 3\n")).unwrap().priority, Some(3));
        assert_eq!(v07(&head7("priority: 03\n")).unwrap().priority, Some(3));
        // 0.6.0 input keeps its JavaScript reading.
        assert_eq!(
            parse(&format!("{HEAD}priority: 3.7\n---\n"))
                .unwrap()
                .priority,
            Some(3)
        );
        assert!(parse(&format!("\u{FEFF}{HEAD}---\n")).is_ok());
        assert_eq!(plain_int("-4"), Some(-4));
        assert_eq!(plain_int("99999999999999999999"), None);
    }

    #[test]
    fn legacy_graphs_keep_0_6_0_rules() {
        // legacy_id and timestamps are 0.7 only.
        assert_eq!(
            parse(&format!("{HEAD}legacy_id: task-9\n---\n")).unwrap_err(),
            "f.md: unknown key: legacy_id"
        );
        let stamped = HEAD.replacen("2026-10-18", "2026-10-18T00:00:00.000Z", 1);
        assert_eq!(
            parse(&format!("{stamped}---\n")).unwrap_err(),
            "f.md: created must be YYYY-MM-DD"
        );
        assert!(parse(&head7("")).is_err());
    }

    #[test]
    fn extension_keys_follow_dec_2() {
        assert!(v07(&head7("x-acme.a.b: 1\nx-acme.a.c: 2\n")).is_ok());
        assert_eq!(
            v07(&head7("x-acme.billing: 1\nx-acme.billing.plan: 2\n")).unwrap_err(),
            "f.md: x-acme.billing is both a value and the prefix of another key"
        );
        for bad in [
            "x-acme: 1",
            "x-.a: 1",
            "x-1co.a: 1",
            "x-acme.: 1",
            "x-acme.a..b: 1",
            "x-Acme.a: 1",
            "x-acme.A: 1",
        ] {
            assert!(v07(&head7(&format!("{bad}\n"))).is_err(), "{bad}");
        }
    }

    const HEAD: &str = "---\nid: task-1\ntype: task\ntitle: T\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n";

    #[test]
    fn dates_follow_the_files_format() {
        let task = |id: &str, created: &str, updated: &str| {
            format!(
                "---\nid: {id}\ntype: task\ntitle: x\nstatus: todo\ncreated: {created}\nupdated: {updated}\n---\n"
            )
        };
        // 0.6.0 checks only the digits, and never compares the two.
        for (c, u) in [("2026-02-30", "2026-02-30"), ("2026-10-25", "2026-10-24")] {
            assert!(parse(&task("task-1", c, u)).is_ok(), "{c} {u}");
        }
        // Format 0.7: a real day, and updated never earlier than created,
        // a date reading as its first millisecond.
        for (c, u, ok) in [
            ("2026-02-30", "2026-03-01", false),
            ("2026-10-25", "2026-10-24", false),
            ("2026-10-24T10:00:00.000Z", "2026-10-24", false),
            ("2026-10-24T00:00:00.000Z", "2026-10-24", true),
            ("2026-10-24", "2026-10-24T00:00:00.000Z", true),
            ("2024-02-29", "2024-02-29", true),
        ] {
            let got = v07(&task(ID7, c, u));
            assert_eq!(got.is_ok(), ok, "{c} {u}: {got:?}");
            if let Err(e) = got {
                assert!(e.contains("created"), "{c} {u}: {e}");
            }
        }
    }

    #[test]
    fn parses_a_task() {
        let node = parse(&format!(
            "{HEAD}priority: 3\ntags: [a]\nparent: goal-1\nskills: [Do-It]\n---\nbody\n"
        ))
        .unwrap();
        assert_eq!(node.priority, Some(3));
        assert_eq!(node.edges.parent.as_deref(), Some("goal-1"));
        assert_eq!(node.skills, vec!["do-it"]);
        assert_eq!(node.body, "body\n");
    }

    #[test]
    fn rejects_like_the_reference() {
        assert_eq!(
            parse(&format!("{HEAD}colour: blue\n---\n")).unwrap_err(),
            "f.md: unknown key: colour"
        );
        assert_eq!(
            parse(&format!("{HEAD}priority: 12\n---\n")).unwrap_err(),
            "f.md: priority must be between 0 and 9"
        );
        assert_eq!(
            parse(&format!("{HEAD}tags: alpha\n---\n")).unwrap_err(),
            "f.md: tags must be a list"
        );
        assert_eq!(
            parse(&format!("{HEAD}tags: [Alpha]\n---\n")).unwrap_err(),
            "f.md: tags[0] must be lowercase"
        );
        assert_eq!(
            parse("---\nid: task-1\ntype: widget\ntitle: T\n---\n").unwrap_err(),
            format!("f.md: type must be one of {}", ALLOWED_TYPES.join(", "))
        );
    }

    #[test]
    fn accepts_extension_keys() {
        let node = parse(&format!("{HEAD}x_acme_owner: team-a\n---\n")).unwrap();
        assert!(node.frontmatter.contains("x_acme_owner"));
    }
}
