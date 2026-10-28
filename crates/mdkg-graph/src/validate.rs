//! `validate`: every node and graph error at once, plus warnings, each a
//! diagnostic with a stable output-contract code.
//!
//! Unlike read commands, validation keeps going after an error so it can
//! report all of them. Outside the phase 1 compatibility scope: subgraph
//! projections, visibility rules, agent workflow contract warnings, event log
//! checks and manifest sibling conflicts.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mdkg_format::node::{FormatVersion, ParseOptions, is_agent_type, is_date, parse_node};
use mdkg_format::{Code, Diagnostic, Uuid7};

use crate::config::Config;
use crate::discovery;
use crate::error::GraphError;
use crate::graph_checks;
use crate::index::{Index, IndexNode, RefTarget, relative_label, relative_path};
use crate::templates;
use mdkg_format::program::BIN;

/// Recommended body headings per node type in a 0.6.0 graph, as 0.6.0
/// lists them. A format 0.7 graph recommends its templates' headings
/// instead ([`template_headings`]).
pub fn recommended_headings(node_type: &str) -> &'static [&'static str] {
    match node_type {
        "task" => &[
            "Overview",
            "Acceptance Criteria",
            "Files Affected",
            "Implementation Notes",
            "Test Plan",
            "Links / Artifacts",
        ],
        "bug" => &[
            "Overview",
            "Reproduction Steps",
            "Expected vs Actual",
            "Suspected Cause",
            "Fix Plan",
            "Test Plan",
            "Links / Artifacts",
        ],
        "feat" => &["Overview", "Acceptance Criteria", "Notes"],
        "spike" => &[
            "Research Question",
            "Context And Constraints",
            "Search Plan",
            "Findings",
            "Options And Tradeoffs",
            "Recommendation",
            "Follow-Up Nodes To Create",
            "Skill Candidates",
            "Evidence And Sources",
        ],
        "epic" => &[
            "Goal",
            "Scope",
            "Milestones",
            "Out of Scope",
            "Risks",
            "Links / Artifacts",
        ],
        "checkpoint" => &[
            "Summary",
            "Scope Covered",
            "Decisions Captured",
            "Implementation Summary",
            "Verification / Testing",
            "Known Issues / Follow-ups",
            "Links / Artifacts",
        ],
        "prd" => &[
            "Problem",
            "Goals",
            "Non-goals",
            "Requirements",
            "Acceptance Criteria",
            "Metrics / Success",
            "Risks",
            "Open Questions",
        ],
        "edd" => &[
            "Overview",
            "Architecture",
            "Data model",
            "APIs / interfaces",
            "Failure modes",
            "Observability",
            "Security / privacy",
            "Testing strategy",
            "Rollout plan",
        ],
        "dec" => &[
            "Context",
            "Decision",
            "Alternatives considered",
            "Consequences",
            "Links / references",
        ],
        "spec" => &["Purpose", "Runtime", "Work Contracts", "Capabilities"],
        "work" => &["Capability", "Inputs", "Outputs", "Receipt"],
        "work_order" => &["Request", "Inputs", "Constraints"],
        "receipt" => &["Outcome", "Artifacts", "Notes"],
        "feedback" => &["Feedback", "Evidence"],
        "dispute" => &["Dispute", "Evidence", "Resolution"],
        "proposal" => &["Summary", "Evidence", "Proposed Change", "Review"],
        _ => &[],
    }
}

fn js_trim_lower(s: &str) -> String {
    mdkg_format::js::trim(s).to_lowercase()
}

/// A format 0.7 type's recommended headings: every heading of the
/// template `new` writes it from (the graph's own, else the built-in one),
/// in order, so a node `new` made never lacks one. None without a template.
pub fn template_headings(root: &Path, config: &Config, node_type: &str) -> Vec<String> {
    let Ok((text, _)) = templates::template_text(root, config, node_type) else {
        return Vec::new();
    };
    // The body as `new` reads it; a template that doesn't parse, `new`
    // refuses, so it recommends nothing.
    let Ok(parsed) = mdkg_format::frontmatter::parse(&text, "<template>") else {
        return Vec::new();
    };
    let body: Vec<&str> = parsed
        .body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for line in body {
        if let Some(h) = heading_text(line)
            && !out.iter().any(|o| js_trim_lower(o) == js_trim_lower(&h))
        {
            out.push(h);
        }
    }
    out
}

/// A line's heading text, for `^#+\s+(.*)$`, trimmed.
fn heading_text(line: &str) -> Option<String> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 {
        return None;
    }
    let rest = &line[hashes..];
    let after_ws = rest.trim_start_matches(mdkg_format::js::is_js_whitespace);
    // `\s+` needs at least one whitespace character.
    (after_ws.len() != rest.len()).then(|| mdkg_format::js::trim(after_ws).to_string())
}

/// Headings in a body: lines matching `^#+\s+(.*)$`, trimmed and lowercased.
fn extract_headings(body: &str) -> HashSet<String> {
    body.split('\n')
        .filter_map(|line| heading_text(line.strip_suffix('\r').unwrap_or(line)))
        .map(|h| h.to_lowercase())
        .collect()
}

/// Case-insensitive `\bWORD\b` for an ASCII word.
fn contains_word(haystack: &str, word: &str) -> bool {
    let hay = haystack.to_ascii_lowercase();
    let word = word.to_ascii_lowercase();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = hay.as_bytes();
    let mut start = 0;
    while let Some(pos) = hay[start..].find(&word) {
        let i = start + pos;
        let end = i + word.len();
        let before = i == 0 || !is_word(bytes[i - 1]);
        let after = end >= bytes.len() || !is_word(bytes[end]);
        if before && after {
            return true;
        }
        start = i + 1;
    }
    false
}

fn has_private_key_marker(body: &str) -> bool {
    // /BEGIN [A-Z ]*PRIVATE KEY/i
    let upper = body.to_ascii_uppercase();
    let mut start = 0;
    while let Some(pos) = upper[start..].find("BEGIN ") {
        let after = &upper[start + pos + 6..];
        let run = after
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || *b == b' ')
            .count();
        // Backtrack: any prefix of the run may end where "PRIVATE KEY" starts.
        if (0..=run).any(|k| after[k..].starts_with("PRIVATE KEY")) {
            return true;
        }
        start += pos + 1;
    }
    false
}

fn has_secret_assignment(body: &str) -> bool {
    // /secret\s*=/i
    let lower = body.to_ascii_lowercase();
    let mut start = 0;
    while let Some(pos) = lower[start..].find("secret") {
        let rest = &lower[start + pos + 6..];
        if rest
            .trim_start_matches(mdkg_format::js::is_js_whitespace)
            .starts_with('=')
        {
            return true;
        }
        start += pos + 1;
    }
    false
}

fn raw_content_warnings(node_ref: &NodeRef<'_>, node: &mdkg_format::Node) -> Vec<Diagnostic> {
    let checks = is_agent_type(&node.node_type)
        || (node.node_type == "checkpoint"
            && matches!(
                node.frontmatter.get("checkpoint_kind"),
                Some(mdkg_format::Value::String(_))
            ));
    if !checks {
        return Vec::new();
    }
    let markers = [
        (
            Code::NodeRawPrompt,
            "raw prompt marker",
            contains_word(&node.body, "RAW_PROMPT_MARKER"),
        ),
        (
            Code::NodeRawPayload,
            "raw payload marker",
            contains_word(&node.body, "RAW_PAYLOAD_MARKER"),
        ),
        (
            Code::NodeRawSecret,
            "raw secret marker",
            contains_word(&node.body, "RAW_SECRET_MARKER")
                || has_private_key_marker(&node.body)
                || has_secret_assignment(&node.body),
        ),
    ];
    markers
        .into_iter()
        .filter(|(_, _, hit)| *hit)
        .map(|(code, description, _)| {
            node_ref
                .warning(code, format!("{description} detected"))
                .with_hint("use refs, hashes, summaries or artifact links instead of raw secrets, prompts or payloads")
        })
        .collect()
}

fn manifest_compat_warnings(
    node_ref: &NodeRef<'_>,
    file: &Path,
    node: &mdkg_format::Node,
) -> Vec<Diagnostic> {
    if node.node_type != "spec" {
        return Vec::new();
    }
    match file.file_name().and_then(|n| n.to_str()) {
        Some("SPEC.md") => vec![
            node_ref
                .warning(
                    Code::NodeLegacySpecFile,
                    "SPEC.md is the legacy manifest file name",
                )
                .with_hint("rename the file to MANIFEST.md"),
        ],
        Some("MANIFEST.md") => vec![
            node_ref
                .warning(
                    Code::NodeLegacySpecType,
                    "MANIFEST.md uses the legacy type: spec",
                )
                .with_hint("change the type to manifest"),
        ],
        _ => Vec::new(),
    }
}

/// Where a node-level diagnostic points.
struct NodeRef<'a> {
    path: &'a str,
    qid: &'a str,
}

impl NodeRef<'_> {
    fn warning(&self, code: Code, message: impl Into<String>) -> Diagnostic {
        Diagnostic::warning(code, message)
            .at(self.path, None)
            .for_node(self.qid)
    }
}

/// Skill slugs under `.mdkg/skills/*/`, plus layout errors and warnings.
fn skills(
    root: &Path,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) -> HashSet<String> {
    let mut slugs = HashSet::new();
    let skills_root = root.join(".mdkg/skills");
    let Ok(entries) = std::fs::read_dir(&skills_root) else {
        return slugs;
    };
    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();
    dirs.sort_by_key(std::fs::DirEntry::file_name);
    for dir in dirs {
        let path = dir.path();
        let rel = relative_path(root, &path);
        let canonical = path.join("SKILL.md").exists();
        let compat = path.join("SKILLS.md").exists();
        if canonical && compat {
            errors.push(
                Diagnostic::error(Code::SkillBothFiles, "both SKILL.md and SKILLS.md exist")
                    .at(rel, None),
            );
            continue;
        }
        if !canonical && !compat {
            errors
                .push(Diagnostic::error(Code::SkillMissingFile, "missing SKILL.md").at(rel, None));
            continue;
        }
        if compat {
            warnings.push(
                Diagnostic::warning(
                    Code::SkillLegacyFile,
                    "using the legacy SKILLS.md file name",
                )
                .at(relative_path(root, &path.join("SKILLS.md")), None)
                .with_hint("rename the file to SKILL.md"),
            );
        }
        // 0.6.0 lowercases the folder name to get the slug.
        slugs.insert(dir.file_name().to_string_lossy().to_lowercase());
    }
    slugs
}

fn skill_mirror_warnings(root: &Path, config: &Config) -> Vec<Diagnostic> {
    let markers = [
        ".mdkg/core/SOUL.md",
        ".mdkg/core/COLLABORATION.md",
        ".mdkg/core/HUMAN.md",
    ];
    let targets: Vec<_> = config
        .skill_mirror_targets
        .iter()
        .map(|t| root.join(t))
        .collect();
    let target_present = targets
        .iter()
        .any(|t| t.exists() || t.parent().is_some_and(Path::exists));
    if targets.is_empty() || !(markers.iter().any(|m| root.join(m).exists()) || target_present) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for target in targets {
        if !target.exists() {
            out.push(
                Diagnostic::warning(Code::SkillMirrorMissing, "skill mirror folder is missing")
                    .at(relative_path(root, &target), None)
                    .with_hint(format!("run `{BIN} skills sync`")),
            );
        } else if !target.join(".mdkg-managed.json").exists() {
            out.push(
                Diagnostic::warning(
                    Code::SkillMirrorManifestMissing,
                    "skill mirror manifest is missing",
                )
                .at(
                    relative_path(root, &target.join(".mdkg-managed.json")),
                    None,
                )
                .with_hint(format!("run `{BIN} skills sync`")),
            );
        }
        // Per-skill mirror drift checks need skill sources; those checks
        // belong to the later skill-migration scope.
    }
    out
}

fn dedupe(values: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// What validation found.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// Node files checked.
    pub checked: usize,
    pub errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
}

impl Receipt {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Errors, then warnings.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.errors.iter().chain(&self.warnings)
    }
}

/// Validates the graph at `root`.
pub fn validate(root: &Path, config: &Config) -> Result<Receipt, GraphError> {
    let tpl = templates::load(root, config)?;
    let documents = discovery::discover(root, config)?;
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut warnings: Vec<Diagnostic> = Vec::new();
    if root.join(".mdkg/graph.json").exists() {
        errors.push(
            Diagnostic::error(
                Code::ConfigGraphV2Unsupported,
                "graph format v2 (.mdkg/graph.json) needs migrating to format 0.7",
            )
            .at(".mdkg/graph.json", None)
            .with_hint(crate::config::identity_v2_hint()),
        );
    }
    // In a format 0.7 graph the built-in templates are the normal case, and
    // `.mdkg/templates/` holds only overrides.
    if !tpl.fallback_types.is_empty() && config.format != FormatVersion::V07 {
        warnings.push(
            Diagnostic::warning(
                Code::TemplateFallback,
                format!(
                    "using bundled templates for missing local type(s): {}",
                    tpl.fallback_types.join(", ")
                ),
            )
            .with_hint("preview the built-in templates with `mdkg upgrade`"),
        );
    }
    let opts = ParseOptions {
        work_status: &config.status_enum,
        priority_min: config.priority_min,
        priority_max: config.priority_max,
        templates: &tpl.schemas,
        allow_extension_keys: true,
        format: config.format,
    };
    let v07 = config.format == FormatVersion::V07;
    // Format 0.7 ids are unique in the whole graph, by UUID.
    let mut first_uuids: HashMap<Uuid7, String> = HashMap::new();

    let mut nodes: Vec<IndexNode> = Vec::new();
    let mut checked = 0;
    // Workspaces in sorted alias order, as the reference iterates them.
    let mut template_recommended: HashMap<String, Vec<String>> = HashMap::new();
    for (ws, files) in &documents.by_workspace {
        let mut first_paths: Vec<(String, String)> = Vec::new();
        let mut first_legacy: HashMap<String, String> = HashMap::new();
        for file in files {
            if file.file_name().is_some_and(|n| n == "core.md")
                && file
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "core")
            {
                continue;
            }
            checked += 1;
            let label = file.display().to_string();
            let rel = relative_path(root, file);
            let content = match std::fs::read(file)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
            {
                Ok(c) => c,
                Err(e) => {
                    errors.push(
                        Diagnostic::error(Code::IoReadFailed, format!("failed to read file: {e}"))
                            .at(rel, None),
                    );
                    continue;
                }
            };
            let node = match parse_node(&content, &label, &opts) {
                Ok(n) => n,
                Err(e) => {
                    errors.push(e.to_diagnostic(|p| relative_label(root, p)));
                    continue;
                }
            };
            let qid = format!("{}:{}", ws.alias, node.id);
            if node.identity.is_some() {
                errors.push(
                    Diagnostic::error(
                        Code::NodeIdentityUnsupported,
                        "identity-bearing node requires an explicit v2 graph manifest; no implicit migration",
                    )
                    .at(rel, None)
                    .for_node(qid),
                );
                continue;
            }
            let uuid = mdkg_format::NodeId::parse(&node.id)
                .ok()
                .map(|id| id.uuid());
            let first = match uuid {
                Some(u) if v07 => first_uuids.get(&u).cloned(),
                _ => first_paths
                    .iter()
                    .find(|(id, _)| *id == node.id)
                    .map(|(_, p)| p.clone()),
            };
            if let Some(first) = first {
                errors.push(
                    Diagnostic::error(
                        Code::NodeDuplicateId,
                        format!("duplicate id {} in workspace {}", node.id, ws.alias),
                    )
                    .at(rel, None)
                    .for_node(qid)
                    .also_at(first.clone(), None)
                    .with_hint("rename one of the ids, or remove the duplicate file"),
                );
                continue;
            }
            if let Some(legacy) = &node.legacy_id {
                match first_legacy.get(legacy) {
                    Some(first) => errors.push(crate::index::duplicate_legacy_id(
                        legacy, &ws.alias, &node.id, &rel, first,
                    )),
                    None => {
                        first_legacy.insert(legacy.clone(), rel.clone());
                    }
                }
            }
            if let Some(u) = uuid {
                first_uuids.insert(u, rel.clone());
            }
            first_paths.push((node.id.clone(), rel.clone()));
            let node_ref = NodeRef {
                path: &rel,
                qid: &qid,
            };
            let headings = extract_headings(&node.body);
            let legacy: Vec<String>;
            let recommended: &[String] = if v07 {
                template_recommended
                    .entry(node.node_type.clone())
                    .or_insert_with(|| template_headings(root, config, &node.node_type))
            } else {
                legacy = recommended_headings(&node.node_type)
                    .iter()
                    .map(|h| (*h).to_string())
                    .collect();
                &legacy
            };
            for heading in recommended {
                if !headings.contains(&js_trim_lower(heading)) {
                    warnings.push(
                        node_ref
                            .warning(
                                Code::NodeHeadingMissing,
                                format!("missing recommended heading \"{heading}\""),
                            )
                            .with_hint(format!("add a \"# {heading}\" section")),
                    );
                }
            }
            warnings.extend(raw_content_warnings(&node_ref, &node));
            warnings.extend(manifest_compat_warnings(&node_ref, file, &node));
            if v07 {
                warnings.extend(v07_node_warnings(&node_ref, &node));
            }
            nodes.push(IndexNode::from_parsed(node, &ws.alias, rel));
        }
    }
    let workspaces = documents
        .by_workspace
        .iter()
        .map(|(w, _)| w.alias.clone())
        .collect();
    let mut index = Index::assemble_in(nodes, workspaces, config.graph_id);
    index.private_work = config.work_committed == crate::config::WorkCommitted::None;
    if v07 {
        v07_index_checks(&index, config.graph_id, &mut errors, &mut warnings);
    }

    let known_skills = skills(root, &mut errors, &mut warnings);
    for node in &index.nodes {
        // A goal's required_skills name skills too (format 0.7); 0.6.0
        // never checked them.
        let required: &[String] = if v07 {
            node.attributes
                .get("required_skills")
                .and_then(|v| v.as_list())
                .unwrap_or(&[])
        } else {
            &[]
        };
        let named = node
            .skills
            .iter()
            .map(|s| ("skills", s))
            .chain(required.iter().map(|s| ("required_skills", s)));
        for (key, slug) in named {
            // node.rs checks a required skill's slug lowercased, as skills'.
            let slug = &slug.to_lowercase();
            if !known_skills.contains(slug) {
                errors.push(
                    Diagnostic::error(
                        Code::SkillMissingSlug,
                        format!("{key} reference missing slug: {slug}"),
                    )
                    .at(node.path.clone(), None)
                    .for_node(node.qid.clone()),
                );
            }
        }
    }
    let external: HashSet<&str> = config.subgraphs.iter().map(String::as_str).collect();
    errors.extend(graph_checks::collect(&index, false, &external));
    errors.extend(graph_checks::private_targets(&index));
    warnings.extend(graph_checks::branch_local_targets(&index, &external));
    warnings.extend(graph_checks::ancestor_blocks(&index));
    for node in &index.nodes {
        warnings.extend(graph_checks::absent_pointers(&index, node));
    }
    warnings.extend(skill_mirror_warnings(root, config));

    Ok(Receipt {
        checked,
        errors: dedupe(errors),
        warnings: dedupe(warnings),
    })
}

/// Format 0.7 warnings about one node: a stale type in its own id, a
/// date-only timestamp, a legacy `x_` key.
fn v07_node_warnings(node_ref: &NodeRef<'_>, node: &mdkg_format::Node) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Ok(id) = mdkg_format::NodeId::parse(&node.id)
        && id.node_type() != node.node_type
    {
        out.push(
            node_ref
                .warning(
                    Code::NodeStaleType,
                    format!(
                        "id names type {} but the node is a {}",
                        id.node_type(),
                        node.node_type
                    ),
                )
                .with_hint(format!(
                    "run `{BIN} retype` to rewrite the id and every ref to it"
                )),
        );
    }
    for (key, value) in [("created", &node.created), ("updated", &node.updated)] {
        if is_date(value) {
            out.push(
                node_ref
                    .warning(
                        Code::NodeDateOnly,
                        format!("{key} is a date, not a timestamp"),
                    )
                    .with_hint("format 0.7 writes YYYY-MM-DDTHH:MM:SS.mmmZ"),
            );
        }
    }
    for key in node.frontmatter.keys() {
        if key.starts_with(mdkg_format::node::EXTENSION_KEY_PREFIX) {
            out.push(
                node_ref
                    .warning(
                        Code::NodeLegacyExtensionKey,
                        format!("{key} is a legacy x_ key"),
                    )
                    .with_hint("rename it x-<namespace>.<field>"),
            );
        }
    }
    out
}

/// The id a node has by its type: `<type>-<uuid>`.
fn current_id(node: &IndexNode) -> String {
    mdkg_format::NodeId::parse(&node.id)
        .ok()
        .and_then(|id| id.with_type(&node.node_type).ok())
        .map_or_else(|| node.id.clone(), |id| id.to_string())
}

fn stale_warning(node: &IndexNode, what: String) -> Diagnostic {
    Diagnostic::warning(Code::NodeStaleType, what)
        .at(node.path.clone(), None)
        .for_node(node.qid.clone())
        .with_hint(format!("run `{BIN} retype` to rewrite refs to the node"))
}

/// Format 0.7 checks over the whole graph: refs with a stale type
/// (warnings), and body links to node ids (an error when no node has the
/// UUID, a warning when the type is stale).
fn v07_index_checks(
    index: &Index,
    _graph_id: Option<Uuid7>,
    errors: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    for stale in &index.stale_refs {
        let (Some(source), Some(target)) = (index.get(&stale.source), index.get(&stale.target))
        else {
            continue;
        };
        warnings.push(stale_warning(
            source,
            format!(
                "{} names {} by a stale type; it is now {}",
                stale.key,
                stale.written,
                current_id(target)
            ),
        ));
    }
    for node in &index.nodes {
        for target in body_links(&node.body) {
            match index.target(target) {
                // A profile C node's: graph_checks::branch_local_targets.
                RefTarget::Missing if crate::store_work::is_store_path(&node.path) => {}
                RefTarget::Missing => errors.push(
                    Diagnostic::error(
                        Code::NodeBadBodyLink,
                        format!(
                            "body links to missing node {}",
                            mdkg_format::NodeRef::parse(target)
                                .map_or_else(|_| target.to_string(), |r| r.id.to_string())
                        ),
                    )
                    .at(node.path.clone(), None)
                    .for_node(node.qid.clone()),
                ),
                RefTarget::Local {
                    node: t,
                    stale: true,
                } => warnings.push(stale_warning(
                    node,
                    format!("a body link names {} by a stale type", current_id(t)),
                )),
                _ => {}
            }
        }
    }
}

/// The length of the run of `c` at the start of `s`.
fn run_of(s: &[u8], c: u8) -> usize {
    s.iter().take_while(|b| **b == c).count()
}

/// The targets of body links (format 0.7, section 5.1), by mdkg's own
/// grammar rather than full CommonMark, so every implementation finds the
/// same links:
/// - a fence opens with 3 or more backticks or tildes, and closes on a line
///   holding only a run of the same character at least as long (and
///   spaces or tabs); nothing inside is a link;
/// - on any other line, a code span opens with a run of backticks and
///   closes with the next run of exactly as many on the same line;
///   nothing inside is a link;
/// - a link is `](`, a target with no whitespace, then `)` on the same
///   line, optionally after spaces or tabs and a closed title (`"…"`,
///   `'…'` or `(…)`).
pub fn body_links(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start().as_bytes();
        let marker = [b'`', b'~']
            .into_iter()
            .map(|c| (c, run_of(trimmed, c)))
            .find(|(_, n)| *n >= 3);
        if let Some((c, n)) = fence {
            let closes = marker.is_some_and(|(m, k)| {
                m == c && k >= n && trimmed[k..].iter().all(|b| *b == b' ' || *b == b'\t')
            });
            if closes {
                fence = None;
            }
            continue;
        }
        if marker.is_some() {
            fence = marker;
            continue;
        }
        links_in(line, &mut out);
    }
    out
}

/// Links in one line, outside its code spans.
fn links_in<'a>(line: &'a str, out: &mut Vec<&'a str>) {
    let bytes = line.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let n = run_of(&bytes[i..], b'`');
        let mut j = i + n;
        let mut closed = None;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let k = run_of(&bytes[j..], b'`');
                if k == n {
                    closed = Some(j + k);
                    break;
                }
                j += k;
            } else {
                j += 1;
            }
        }
        match closed {
            Some(end) => {
                spans.push((i, end));
                i = end;
            }
            None => i += n,
        }
    }
    let in_span = |at: usize| spans.iter().any(|(a, b)| (*a..*b).contains(&at));
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' && !in_span(i) {
            let rest = &line[i + 2..];
            let end = rest
                .find(|c: char| c == ')' || c.is_whitespace())
                .unwrap_or(rest.len());
            if end > 0 && closes_link(&rest[end..]) {
                out.push(&rest[..end]);
            }
            i += 2 + end;
            continue;
        }
        i += 1;
    }
}

/// Whether what follows a link target closes the link: `)`, or spaces or
/// tabs, a closed title, optional spaces or tabs, then `)`.
fn closes_link(tail: &str) -> bool {
    if tail.starts_with(')') {
        return true;
    }
    let after_space = tail.trim_start_matches([' ', '\t']);
    if after_space.len() == tail.len() {
        return false;
    }
    let close = match after_space.chars().next() {
        Some('"') => '"',
        Some('\'') => '\'',
        Some('(') => ')',
        _ => return false,
    };
    let Some(end) = after_space[1..].find(close) else {
        return false;
    };
    after_space[1 + end + 1..]
        .trim_start_matches([' ', '\t'])
        .starts_with(')')
}

#[cfg(test)]
mod v07_tests {
    use super::body_links;

    #[test]
    fn body_links_skip_code() {
        let body = "See [a](task-1) and [b](x \"t\").\n`[c](no)` [d](yes)\n```\n[e](fenced)\n```\n~~~md\n[f](no)\n~~~\n[g](ok)\n[h](unclosed";
        assert_eq!(body_links(body), vec!["task-1", "x", "yes", "ok"]);
        // Multi-backtick spans on a line, an unclosed run, and fences closed
        // only by an equal or longer run of the same character.
        let body = "``[a](no) ` still``\n`` [b](yes)\n````\n```\n[c](no)\n````\n~~~~\n~~~\n[d](no)\n~~~~~\n[é](ü) [e](ok)";
        assert_eq!(body_links(body), vec!["yes", "ü", "ok"]);
        // A fence line with text after it doesn't close; spans never cross
        // lines, so headings, quotes and lists need no rules of their own.
        let body = "```md\n```example\n[w](no)\n```  \n# Literal `\n[x](real) `\n> `q\n> [y](real2)`\nLit `\n===\n[z](real3) `";
        assert_eq!(body_links(body), vec!["real", "real2", "real3"]);
        // A link closes on its line, after an optional closed title.
        let body = "[a](open\n[b](t \"title\")\n[c](t2 'x' )\n[d](t3 (p))\n[e](no \"unfinished )\n[f](no2 x)\n[g]()";
        assert_eq!(body_links(body), vec!["t", "t2", "t3"]);
    }
}
