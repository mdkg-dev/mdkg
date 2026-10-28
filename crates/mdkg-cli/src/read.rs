//! Read commands: `list`, `show`, `search` and `validate`.

use std::path::Path;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::query::{self, Filters, TagsMode};
use mdkg_graph::resolve::Resolution;
use mdkg_graph::{Config, ErrorKind, GraphError, IndexNode, config, index, validate as v};
use mdkg_index::resolve::{handle_error, resolve_handle, shadowed};
use serde_json::Value;

use crate::output::{
    self, DEFAULT_LIST_FIELDS, ListData, NodeRecord, Report, ShowData, ValidateData,
};
use crate::{FilterArgs, TagsModeArg, coord};

/// Flag values are matched case-insensitively, as 0.6.0 does. An empty
/// `--ws` is a usage error, as for the other filters.
pub(crate) fn workspace_filter(value: Option<&str>) -> Result<Option<String>, GraphError> {
    let Some(v) = value.map(|v| mdkg_format::js::trim(v).to_lowercase()) else {
        return Ok(None);
    };
    if v.is_empty() {
        return Err(GraphError::usage(
            Code::UsageInvalidArguments,
            "--ws names no value",
        ));
    }
    Ok(Some(v).filter(|v| v != "all"))
}

/// An unknown workspace isn't quoted: what was typed may be anything.
pub(crate) fn check_workspace(config: &Config, ws: Option<&str>) -> Result<(), GraphError> {
    if let Some(ws) = ws
        && !config.workspaces.iter().any(|w| w.alias == ws)
        && !config.subgraphs.iter().any(|s| s == ws)
    {
        // Only workspaces that are indexed: a disabled one or a subgraph
        // would match nothing.
        let mut known: Vec<&str> = config
            .workspaces
            .iter()
            .filter(|w| w.enabled)
            .map(|w| w.alias.as_str())
            .collect();
        known.push("all");
        return Err(GraphError::not_found(
            Code::ConfigUnknownWorkspace,
            "--ws names a workspace that isn't configured",
        )
        .with_hint(format!("workspaces: {}", known.join(", "))));
    }
    Ok(())
}

/// A comma-separated filter's values, trimmed and lowercased. A flag given
/// with no value at all is a usage error, not "no filter".
fn filter_values(flag: &str, raw: &[String], code: Code) -> Result<Vec<String>, GraphError> {
    let values: Vec<String> = raw
        .iter()
        .map(|v| mdkg_format::js::trim(v).to_lowercase())
        .filter(|v| !v.is_empty())
        .collect();
    if values.is_empty() && !raw.is_empty() {
        return Err(GraphError::usage(code, format!("{flag} names no value")));
    }
    Ok(values)
}

/// Every status a node can show: the config's work statuses, `archived`
/// for goals, decision statuses, and the coordinator's effective ones.
fn known_statuses(config: &Config) -> Vec<String> {
    let mut known: Vec<String> = config
        .status_enum
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let extra = ["archived", "progress", "review", "blocked"];
    for s in extra.iter().chain(mdkg_format::node::DEC_STATUS) {
        if !known.iter().any(|k| k == s) {
            known.push((*s).to_string());
        }
    }
    known
}

/// The filters for `list` and `search`. A value that can't match any node,
/// such as a misspelt status, is a usage error rather than an empty result.
pub(crate) fn filters_from(
    args: &FilterArgs,
    config: &Config,
    ws: Option<String>,
) -> Result<Filters, GraphError> {
    let node_types = filter_values("--type", &args.node_type, Code::UsageUnknownType)?;
    if node_types
        .iter()
        .any(|t| !mdkg_format::node::ALLOWED_TYPES.contains(&t.as_str()))
    {
        return Err(GraphError::usage(
            Code::UsageUnknownType,
            "--type names a value that isn't a node type",
        )
        .with_hint(format!(
            "valid types: {}",
            mdkg_format::node::ALLOWED_TYPES.join(", ")
        )));
    }
    let statuses = filter_values("--status", &args.status, Code::UsageUnknownStatus)?;
    let known = known_statuses(config);
    if statuses.iter().any(|s| !known.contains(s)) {
        return Err(GraphError::usage(
            Code::UsageUnknownStatus,
            "--status names a value that isn't a status",
        )
        .with_hint(format!("valid statuses: {}", known.join(", "))));
    }
    let tags = filter_values("--tags", &args.tags, Code::UsageInvalidArguments)?;
    Ok(Filters {
        ws,
        node_types,
        statuses,
        tags,
        tags_mode: match args.tags_mode {
            Some(TagsModeArg::All) => TagsMode::All,
            _ => TagsMode::Any,
        },
        ..Filters::default()
    })
}

fn set_effective(view: &coord::View, nodes: &mut [IndexNode]) {
    for n in nodes.iter_mut() {
        n.status = view.effective_status(&n.path, Some(&n.qid), n.status.as_deref());
    }
}

/// For read-only commands: sets each node's effective status from
/// coordinator state, if a store exists, and returns the view for later
/// lookups. A store that exists and can't be read (one a newer build
/// upgraded, a damaged file, git refusing the repository) gives a
/// `coord.store_failed` warning that the coordinator was skipped, instead
/// of silently showing no leases (orchestrator, 2026-10-21).
pub(crate) fn effective_or_warn(
    root: &Path,
    nodes: &mut [IndexNode],
) -> (Option<coord::View>, Vec<Diagnostic>) {
    match coord::View::open(root) {
        Ok(view) => {
            if let Some(v) = &view {
                set_effective(v, nodes);
            }
            (view, Vec::new())
        }
        Err(e) => {
            let mut d = *e.diagnostic;
            d.severity = mdkg_format::Severity::Warning;
            d.message = format!(
                "the coordinator was skipped, so leases and states aren't shown: {}",
                d.message
            );
            (None, vec![d])
        }
    }
}

/// For commands that act on leases (hand out work, write files): a store
/// that exists and can't be read fails the command, so it never goes on as
/// if nobody held anything.
pub(crate) fn effective_or_fail(
    root: &Path,
    nodes: &mut [IndexNode],
) -> Result<Option<coord::View>, GraphError> {
    let view = coord::View::open(root)?;
    if let Some(v) = &view {
        set_effective(v, nodes);
    }
    Ok(view)
}

/// Node records with the chosen fields, with coordinator state when a chosen
/// field needs it.
pub(crate) fn records(
    view: Option<&coord::View>,
    nodes: &[&IndexNode],
    fields: &[String],
) -> Vec<Value> {
    let wants_coordination = fields.is_empty() || fields.iter().any(|f| f == "coordination");
    nodes
        .iter()
        .map(|n| {
            let mut record = NodeRecord::from_node(n);
            if wants_coordination && let Some(view) = view {
                record.coordination = view.record(&n.path, Some(&n.qid));
            }
            record.select(fields)
        })
        .collect()
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("-")
}

/// A node's line in `list`, `search` and the graph views: its id (and the
/// 0.6.0 id it had, when migrated), then status/priority and title. A
/// full id is 41 columns, so the line holds only what a reader needs to
/// fit 80 (RC1 U8): no type column when the id already starts with it (a
/// 0.7.0 id always does), no path (`show` has it), and no placeholder for
/// a field `--fields` left out. JSON keeps every field.
///
/// Node text is the repository's own, maybe hostile, so control, format
/// and bidirectional characters are escaped: no title, path or field can
/// move the cursor, set the terminal's title, or plant a link or a
/// clipboard write (OSC 8, OSC 52).
pub(crate) fn card(v: &Value) -> String {
    let id = str_field(v, "id");
    let mut shown_id = match v.get("ws").and_then(Value::as_str) {
        Some(ws) => format!("{ws}:{id}"),
        None => id.to_string(),
    };
    if let Some(legacy) = v.get("legacy_id").and_then(Value::as_str) {
        shown_id = format!("{shown_id} ({legacy})");
    }
    let mut columns = vec![shown_id];
    if let Some(t) = v.get("type").and_then(Value::as_str)
        && !id.starts_with(&format!("{t}-"))
    {
        columns.push(t.to_string());
    }
    let status = v
        .get("status")
        .map(|s| s.as_str().unwrap_or("-").to_string());
    let priority = v
        .get("priority")
        .map(|p| p.as_i64().map_or("-".to_string(), |p| format!("p{p}")));
    match (status, priority) {
        (Some(s), Some(p)) => columns.push(format!("{s}/{p}")),
        (Some(one), None) | (None, Some(one)) => columns.push(one),
        (None, None) => {}
    }
    if let Some(title) = v.get("title") {
        columns.push(title.as_str().unwrap_or("-").to_string());
    }
    output::shown_path(&columns.join(" | "))
}

fn print_cards(count: usize, nodes: &[Value], empty_note: &str) {
    eprintln!("count: {count}");
    if nodes.is_empty() {
        eprintln!("note: {empty_note}");
    }
    for node in nodes {
        println!("{}", card(node));
    }
}

pub fn list(
    root: &Path,
    filters: &FilterArgs,
    epic: Option<String>,
    priority: Option<i64>,
    blocked: bool,
) -> u8 {
    let report = (|| -> Result<Report<ListData>, GraphError> {
        let fields = output::parse_fields(filters.fields.as_deref(), DEFAULT_LIST_FIELDS)?;
        let config = config::load(root)?;
        let ws = workspace_filter(filters.ws.as_deref())?;
        check_workspace(&config, ws.as_deref())?;
        if priority.is_some_and(|p| !(config.priority_min..=config.priority_max).contains(&p)) {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "--priority is outside the configured range",
            )
            .with_hint(format!(
                "priorities run from {} to {}",
                config.priority_min, config.priority_max
            )));
        }
        if filters
            .node_type
            .iter()
            .any(|t| mdkg_format::js::trim(t).eq_ignore_ascii_case("skill"))
        {
            return Err(GraphError::usage(
                Code::UsageUnsupportedType,
                "--type skill is not supported here",
            )
            .with_hint(format!("`{} skills list` lists the skills", crate::BIN)));
        }
        let mut f = filters_from(filters, &config, ws.clone())?;
        let mut index = index::build(root, &config)?;
        let (view, mut warnings) = effective_or_warn(root, &mut index.nodes);
        f.priority = priority;
        f.blocked = blocked;
        if let Some(epic) = epic {
            let epic = mdkg_format::js::trim(&epic).to_string();
            if epic.is_empty() {
                return Err(GraphError::usage(
                    Code::UsageInvalidArguments,
                    "--epic names no value",
                ));
            }
            let resolved = resolve_handle(&index, &epic, ws.as_deref());
            warnings.extend(shadowed(&index, &epic, ws.as_deref(), &resolved));
            match resolved {
                Resolution::Ok(qid) => f.epic = Some(qid),
                other => return Err(handle_error(&index, "epic", &epic, &other, ws.as_deref())),
            }
        }
        let mut nodes = query::filter(index.nodes.iter(), &f);
        query::sort_by_qid(&mut nodes);
        Ok(Report::ok(ListData {
            count: nodes.len(),
            nodes: records(view.as_ref(), &nodes, &fields),
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("list", filters.json, report, |d| {
        print_cards(d.count, &d.nodes, "no nodes matched current filters");
    })
}

/// The text form of `show`: the card, then every other field, then the body,
/// each escaped as [`card`] says, the body keeping its lines and tabs.
pub(crate) fn show_text(v: &Value) -> String {
    let mut lines = vec![card(v)];
    if let Some(path) = v.get("path").and_then(Value::as_str) {
        lines.push(output::shown_path(&format!("path: {path}")));
    }
    let skip = [
        "id",
        "legacy_id",
        "ws",
        "type",
        "title",
        "status",
        "priority",
        "path",
        "body",
    ];
    if let Some(map) = v.as_object() {
        for (key, value) in map {
            if skip.contains(&key.as_str()) {
                continue;
            }
            match value {
                Value::Array(items) => lines.push(output::shown_path(&format!(
                    "{key}: {}",
                    items
                        .iter()
                        .map(|i| i.as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
                Value::Object(inner) => {
                    for (k, val) in inner {
                        let text = match val {
                            Value::Array(items) => items
                                .iter()
                                .map(|i| i.as_str().map_or(i.to_string(), String::from))
                                .collect::<Vec<_>>()
                                .join(", "),
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        // Leases and changesets are other actors' text,
                        // maybe from an older store: escape hidden characters.
                        let text = if key == "coordination" {
                            crate::output::shown(&text)
                        } else {
                            text
                        };
                        lines.push(output::shown_path(&format!("{k}: {text}")));
                    }
                }
                Value::String(s) => lines.push(output::shown_path(&format!("{key}: {s}"))),
                other => lines.push(output::shown_path(&format!("{key}: {other}"))),
            }
        }
    }
    if let Some(body) = v.get("body").and_then(Value::as_str) {
        lines.push(String::new());
        lines.push(output::shown_body(body));
    }
    lines.join("\n")
}

pub fn show(
    root: &Path,
    id: &str,
    ws: Option<&str>,
    meta: bool,
    fields: Option<&str>,
    json: bool,
) -> u8 {
    let report = (|| -> Result<Report<ShowData>, GraphError> {
        let mut fields = output::parse_fields(fields, &[])?;
        if meta {
            if fields.is_empty() {
                fields = output::NODE_FIELDS.iter().map(|f| f.to_string()).collect();
            }
            fields.retain(|f| f != "body");
            if fields.is_empty() {
                return Err(GraphError::usage(
                    Code::UsageInvalidArguments,
                    "--meta leaves out the body, so --fields body selects nothing",
                )
                .with_hint("drop --meta to get the body"));
            }
        }
        let config = config::load(root)?;
        let ws = workspace_filter(ws)?;
        check_workspace(&config, ws.as_deref())?;
        let mut index = index::build(root, &config)?;
        let (view, mut warnings) = effective_or_warn(root, &mut index.nodes);
        let resolved = resolve_handle(&index, id, ws.as_deref());
        warnings.extend(shadowed(&index, id, ws.as_deref(), &resolved));
        let node = match resolved {
            Resolution::Ok(qid) => index.get(&qid).cloned(),
            other => return Err(handle_error(&index, "id", id, &other, ws.as_deref())),
        };
        let Some(node) = node else {
            return Err(crate::coord::no_node_matches());
        };
        // A profile C node's refs to knowledge may target another branch.
        let external = config.subgraphs.iter().map(String::as_str).collect();
        warnings.extend(
            mdkg_graph::graph_checks::branch_local_targets(&index, &external)
                .into_iter()
                .filter(|d| d.node.as_deref() == Some(node.qid.as_str())),
        );
        let node = records(view.as_ref(), &[&node], &fields)
            .pop()
            .unwrap_or(Value::Null);
        Ok(Report::ok(ShowData { node }).with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("show", json, report, |d| println!("{}", show_text(&d.node)))
}

/// How text-mode `validate` prints warnings. JSON always carries them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warnings {
    None,
    /// Each code that occurs once in full; each code that repeats as one
    /// line with its count and first location.
    Summary,
    All,
}

/// The warning lines text-mode `validate` prints, in first-seen order of
/// their codes, and whether any code was summarised.
fn warning_lines(warnings: &[&mdkg_format::Diagnostic]) -> (Vec<String>, bool) {
    let mut by_code: Vec<(Code, Vec<&mdkg_format::Diagnostic>)> = Vec::new();
    for d in warnings {
        match by_code.iter_mut().find(|(c, _)| *c == d.code) {
            Some((_, list)) => list.push(d),
            None => by_code.push((d.code, vec![d])),
        }
    }
    let mut lines = Vec::new();
    let mut summarised = false;
    for (code, list) in by_code {
        let first = list[0];
        if list.len() == 1 {
            // The location escaped, as `eprint_diagnostic` prints it.
            lines.push(match first.location() {
                Some(loc) => format!(
                    "{}: {} {}: {}",
                    crate::output::shown_path(&loc),
                    first.severity.as_str(),
                    first.code.as_str(),
                    first.message
                ),
                None => first.to_string(),
            });
            if let Some(hint) = &first.hint {
                lines.push(format!("  hint: {hint}"));
            }
            continue;
        }
        summarised = true;
        let at = first.location().map_or(String::new(), |loc| {
            format!(", first at {}", crate::output::shown_path(&loc))
        });
        lines.push(format!(
            "warning {}: {} warnings{at}",
            code.as_str(),
            list.len()
        ));
    }
    (lines, summarised)
}

pub fn validate(root: &Path, json: bool, shown: Warnings, changed_only: bool) -> u8 {
    let report = (|| -> Result<Report<ValidateData>, GraphError> {
        let config = config::load(root)?;
        let receipt = v::validate(root, &config)?;
        let (errors, warnings, changed) = if changed_only {
            let changed = crate::write::changed_graph_paths(root)?;
            (
                // As in 0.6.0: every error, and only warnings on changed files.
                receipt.errors.clone(),
                crate::write::keep_changed(receipt.warnings.clone(), &changed),
                Some(changed.len()),
            )
        } else {
            (receipt.errors.clone(), receipt.warnings.clone(), None)
        };
        let data = ValidateData {
            checked: receipt.checked,
            errors: errors.len(),
            warnings: warnings.len(),
            changed,
        };
        let failed = !errors.is_empty();
        let mut report = Report::ok(data).with(errors.into_iter().chain(warnings));
        if failed {
            report.failure = Some(ErrorKind::Validation);
        }
        Ok(report)
    })()
    .unwrap_or_else(Report::failed);
    if json {
        return output::emit("validate", true, report, |_| {});
    }
    let code = report.exit_code();
    let is_warning = |d: &&mdkg_format::Diagnostic| d.severity == mdkg_format::Severity::Warning;
    for d in report.diagnostics.iter().filter(|d| !is_warning(d)) {
        output::eprint_diagnostic(d);
    }
    let warnings: Vec<&mdkg_format::Diagnostic> =
        report.diagnostics.iter().filter(is_warning).collect();
    match shown {
        Warnings::None => {}
        Warnings::All => warnings.iter().for_each(|d| output::eprint_diagnostic(d)),
        Warnings::Summary => {
            let (lines, summarised) = warning_lines(&warnings);
            for line in lines {
                eprintln!("{line}");
            }
            if summarised {
                eprintln!(
                    "note: run `{} validate --warnings` to list every warning",
                    crate::BIN
                );
            }
        }
    }
    match &report.data {
        Some(d) if d.errors == 0 => println!("validation ok"),
        Some(d) => eprintln!("validation failed with {} error(s)", d.errors),
        None => {}
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdkg_format::Diagnostic;

    #[test]
    fn repeated_warning_codes_collapse_to_one_line() {
        let w = |code, path: &str| {
            Diagnostic::warning(code, "m")
                .at(path, Some(2))
                .with_hint("h")
        };
        let list = [
            w(Code::NodeHeadingMissing, "a.md"),
            w(Code::SkillMirrorMissing, "s"),
            w(Code::NodeHeadingMissing, "b.md"),
        ];
        let refs: Vec<&Diagnostic> = list.iter().collect();
        let (lines, summarised) = warning_lines(&refs);
        assert!(summarised);
        assert_eq!(
            lines,
            [
                "warning node.heading_missing: 2 warnings, first at a.md:2",
                "s:2: warning skill.mirror_missing: m",
                "  hint: h",
            ]
        );
        let (lines, summarised) = warning_lines(&refs[1..2]);
        assert!(!summarised);
        assert_eq!(lines.len(), 2);
    }
}
